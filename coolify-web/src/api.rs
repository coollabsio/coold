use axum::{
    extract::{Path, State},
    http::{HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use coolify_core::{
    now, ContainerSummary, SchedulerStream, Server, ServerId, ServerLiveStatus, ServerStatus,
    ServerSyncResult,
};
use coolify_storage::{
    BuildRepository, ClusterRepository, EventRepository, ServerRepository, StorageError,
};
use serde::Serialize;
use tower_http::{
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    set_header::SetResponseHeaderLayer,
    trace::TraceLayer,
};
use uuid::Uuid;

use crate::{scheduler_client::SchedulerError, state::AppState, static_files};

#[derive(Debug, Serialize)]
struct StatusPayload {
    ok: bool,
    app: &'static str,
    version: &'static str,
    scheduler: SchedulerStatus,
}
#[derive(Debug, Serialize)]
struct SchedulerStatus {
    configured: bool,
    connected_streams: u32,
}
#[derive(Debug, Serialize)]
struct ErrorPayload {
    error: String,
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/status", get(status))
        .route("/api/v1/scheduler/streams", get(scheduler_streams))
        .route("/api/v1/servers", get(servers))
        .route("/api/v1/servers/sync-streams", post(sync_streams))
        .route("/api/v1/servers/:id/live-status", get(server_live_status))
        .route("/api/v1/servers/:id/containers", get(server_containers))
        .route("/api/v1/clusters", get(clusters))
        .route("/api/v1/events", get(events))
        .route("/api/v1/builds", get(builds));
    let spa = Router::new()
        .route("/assets/*path", get(static_files::serve))
        .fallback(static_files::fallback);
    api.merge(spa)
        .with_state(state.clone())
        .layer(security_headers(state.config.public_https))
        .layer(RequestBodyLimitLayer::new(50 * 1024 * 1024))
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST, Method::DELETE])
                .allow_origin(Any),
        )
        .layer(TraceLayer::new_for_http())
}

fn security_headers(
    _public_https: bool,
) -> tower_http::set_header::SetResponseHeaderLayer<HeaderValue> {
    let csp = HeaderValue::from_static("default-src 'self'; base-uri 'self'; object-src 'none'; frame-ancestors 'none'; img-src 'self' https: data:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; connect-src 'self'; form-action 'self'");
    SetResponseHeaderLayer::if_not_present(axum::http::header::CONTENT_SECURITY_POLICY, csp)
}

async fn healthz() -> &'static str {
    "ok"
}
async fn status(State(state): State<AppState>) -> Json<StatusPayload> {
    Json(StatusPayload {
        ok: true,
        app: "coolify-web",
        version: env!("CARGO_PKG_VERSION"),
        scheduler: SchedulerStatus {
            configured: state.scheduler.configured(),
            connected_streams: 0,
        },
    })
}
async fn servers(
    State(state): State<AppState>,
) -> Result<Json<Vec<coolify_core::Server>>, ApiError> {
    Ok(Json(state.store.list_servers().await?))
}

async fn scheduler_streams(
    State(state): State<AppState>,
) -> Result<Json<Vec<SchedulerStream>>, ApiError> {
    Ok(Json(state.scheduler.list_streams().await?))
}

async fn sync_streams(State(state): State<AppState>) -> Result<Json<ServerSyncResult>, ApiError> {
    let streams = state.scheduler.list_streams().await?;
    let mut created = 0;
    let mut updated = 0;
    let mut server_ids = Vec::new();
    for stream in streams {
        let mut server = match state.store.get_server_by_host_id(&stream.host_id).await? {
            Some(existing) => {
                updated += 1;
                existing
            }
            None => {
                created += 1;
                let mut s = Server::new(&stream.host_id, &stream.host_id)
                    .map_err(|e| ApiError::BadRequest(e.to_string()))?;
                s.host_id = Some(stream.host_id.clone());
                s
            }
        };
        server.capabilities = stream.caps;
        server.status = ServerStatus::Online;
        server.last_seen_at = Some(now());
        server.updated_at = now();
        state.store.upsert_server(&server).await?;
        server_ids.push(server.id);
    }
    let event = coolify_core::Event::info(
        "scheduler.sync",
        format!("synced scheduler streams: created={created} updated={updated}"),
    )
    .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    state.store.append_event(&event).await?;
    Ok(Json(ServerSyncResult {
        created,
        updated,
        server_ids,
    }))
}
async fn clusters(
    State(state): State<AppState>,
) -> Result<Json<Vec<coolify_core::Cluster>>, ApiError> {
    Ok(Json(state.store.list_clusters().await?))
}
async fn events(State(state): State<AppState>) -> Result<Json<Vec<coolify_core::Event>>, ApiError> {
    Ok(Json(state.store.list_events(100).await?))
}
async fn builds(State(state): State<AppState>) -> Result<Json<Vec<coolify_core::Build>>, ApiError> {
    Ok(Json(state.store.list_builds(100).await?))
}

async fn server_live_status(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<ServerLiveStatus>, ApiError> {
    let id = parse_server_id(&id)?;
    let server = state.store.get_server(&id).await?;
    let reachable =
        server.host_id.as_deref().is_some_and(|h| !h.is_empty()) && state.scheduler.configured();
    Ok(Json(ServerLiveStatus {
        server_id: server.id,
        host_id: server.host_id,
        scheduler_configured: state.scheduler.configured(),
        reachable,
        capabilities: server.capabilities,
    }))
}

async fn server_containers(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Vec<ContainerSummary>>, ApiError> {
    let id = parse_server_id(&id)?;
    let server = state.store.get_server(&id).await?;
    let host_id = server
        .host_id
        .as_deref()
        .filter(|h| !h.is_empty())
        .ok_or(ApiError::Conflict("server has no scheduler host_id".into()))?;
    Ok(Json(state.scheduler.list_containers(host_id).await?))
}

fn parse_server_id(id: &str) -> Result<ServerId, ApiError> {
    Ok(ServerId(Uuid::parse_str(id).map_err(|_| {
        ApiError::BadRequest("invalid server id".into())
    })?))
}

#[derive(Debug)]
enum ApiError {
    Storage(StorageError),
    Scheduler(SchedulerError),
    BadRequest(String),
    Conflict(String),
}
impl From<StorageError> for ApiError {
    fn from(value: StorageError) -> Self {
        Self::Storage(value)
    }
}
impl From<SchedulerError> for ApiError {
    fn from(value: SchedulerError) -> Self {
        Self::Scheduler(value)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::Storage(StorageError::NotFound) => {
                (StatusCode::NOT_FOUND, "not found".into())
            }
            ApiError::Storage(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            ApiError::Scheduler(SchedulerError::NotConfigured) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "scheduler is not configured".into(),
            ),
            ApiError::Scheduler(SchedulerError::HostNotConnected) => {
                (StatusCode::NOT_FOUND, "host not connected".into())
            }
            ApiError::Scheduler(SchedulerError::Timeout) => {
                (StatusCode::GATEWAY_TIMEOUT, "scheduler timeout".into())
            }
            ApiError::Scheduler(SchedulerError::Scheduler { status, message }) => (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                message,
            ),
            ApiError::Scheduler(e @ SchedulerError::Malformed(_)) => {
                (StatusCode::BAD_GATEWAY, e.to_string())
            }
            ApiError::Scheduler(e @ SchedulerError::Io(_)) => {
                (StatusCode::BAD_GATEWAY, e.to_string())
            }
            ApiError::Scheduler(e @ SchedulerError::Json(_)) => {
                (StatusCode::BAD_GATEWAY, e.to_string())
            }
            ApiError::BadRequest(e) => (StatusCode::BAD_REQUEST, e),
            ApiError::Conflict(e) => (StatusCode::CONFLICT, e),
        };
        (status, Json(ErrorPayload { error: message })).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use coolify_core::{Event, Server};
    use coolify_storage::{EventRepository, Store};
    use http_body_util::BodyExt;
    use std::{path::PathBuf, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixListener,
    };
    use tower::ServiceExt;
    use uuid::Uuid;

    async fn app_with(socket: PathBuf, timeout_ms: u64, with_host_id: bool) -> (Router, String) {
        let store = Store::memory().await.unwrap();
        store.migrate().await.unwrap();
        let mut server = Server::new("node-a", "203.0.113.10").unwrap();
        if with_host_id {
            server.host_id = Some("host-a".into());
            server.capabilities = vec!["coold".into()];
        }
        let id = server.id.to_string();
        store.upsert_server(&server).await.unwrap();
        store
            .append_event(&Event::info("boot", "ok").unwrap())
            .await
            .unwrap();
        let cfg = crate::config::Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            db_path: ":memory:".into(),
            auto_migrate: true,
            public_https: false,
            scheduler_socket_path: socket,
            scheduler_timeout: Duration::from_millis(timeout_ms),
        };
        (router(AppState::new(store, cfg)), id)
    }

    fn socket_path(name: &str) -> PathBuf {
        PathBuf::from(format!("/tmp/cw-{name}-{}.sock", Uuid::new_v4().simple()))
    }

    async fn mock_scheduler(body: &'static str, status: u16) -> PathBuf {
        let path = socket_path("scheduler");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let path2 = path.clone();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await.unwrap();
            let reason = if status == 200 { "OK" } else { "ERR" };
            let resp = format!("HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len());
            stream.write_all(resp.as_bytes()).await.unwrap();
            let _ = std::fs::remove_file(path2);
        });
        path
    }

    async fn hanging_scheduler() -> PathBuf {
        let path = socket_path("hang");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(250)).await;
        });
        path
    }

    async fn body_text(res: axum::response::Response) -> String {
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn health_and_static_api_work() {
        let (app, _) = app_with(PathBuf::from("/tmp/missing.sock"), 50, true).await;
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(res
            .headers()
            .contains_key(axum::http::header::CONTENT_SECURITY_POLICY));
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/servers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(body_text(res).await.contains("node-a"));
    }

    #[tokio::test]
    async fn scheduler_streams_proxy_returns_inventory() {
        let path = mock_scheduler(
            r#"[{"host_id":"host-a","caps":["builder","coold"],"builder_capacity":2}]"#,
            200,
        )
        .await;
        let (app, _) = app_with(path, 500, true).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/scheduler/streams")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(body_text(res).await.contains("host-a"));
    }

    #[tokio::test]
    async fn sync_streams_creates_and_updates_servers() {
        let path = mock_scheduler(
            r#"[{"host_id":"host-new","caps":["coold","builder"],"builder_capacity":2}]"#,
            200,
        )
        .await;
        let (app, _) = app_with(path, 500, true).await;
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/servers/sync-streams")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let text = body_text(res).await;
        assert!(text.contains("created"));
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/servers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let text = body_text(res).await;
        assert!(text.contains("host-new"));
        assert!(text.contains("online"));
    }

    #[tokio::test]
    async fn containers_success_maps_scheduler_data() {
        let body = r#"{"request_id":"00000000-0000-0000-0000-000000000000","status":"ok","data":[{"id":"c1","name":"web","image":"nginx","state":"running","networks":["coolify-default-mesh"]}]}"#;
        // The client verifies request_id, so respond dynamically with whatever it sent.
        let path = socket_path("success");
        let listener = UnixListener::bind(&path).unwrap();
        let path2 = path.clone();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let n = stream.read(&mut tmp).await.unwrap();
            buf.extend_from_slice(&tmp[..n]);
            let req = String::from_utf8_lossy(&buf);
            let request_id = req
                .split("\r\n\r\n")
                .nth(1)
                .and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok())
                .and_then(|v| v["request_id"].as_str().map(str::to_string))
                .unwrap();
            let resp_body = body.replace("00000000-0000-0000-0000-000000000000", &request_id);
            let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{resp_body}", resp_body.len());
            stream.write_all(resp.as_bytes()).await.unwrap();
            let _ = std::fs::remove_file(path2);
        });
        let (app, id) = app_with(path, 500, true).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/v1/servers/{id}/containers"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(body_text(res).await.contains("web"));
    }

    #[tokio::test]
    async fn containers_maps_scheduler_404_to_host_offline() {
        let path = socket_path("offline");
        let listener = UnixListener::bind(&path).unwrap();
        let path2 = path.clone();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut tmp = [0u8; 4096];
            let n = stream.read(&mut tmp).await.unwrap();
            let req = String::from_utf8_lossy(&tmp[..n]);
            let request_id = req
                .split("\r\n\r\n")
                .nth(1)
                .and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok())
                .and_then(|v| v["request_id"].as_str().map(str::to_string))
                .unwrap();
            let resp_body = format!(
                r#"{{"request_id":"{request_id}","status":"error","code":404,"message":"host not connected"}}"#
            );
            let resp = format!("HTTP/1.1 404 ERR\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{resp_body}", resp_body.len());
            stream.write_all(resp.as_bytes()).await.unwrap();
            let _ = std::fs::remove_file(path2);
        });
        let (app, id) = app_with(path, 500, true).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/v1/servers/{id}/containers"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn containers_missing_host_id_is_conflict() {
        let (app, id) = app_with(PathBuf::from("/tmp/missing.sock"), 50, false).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/v1/servers/{id}/containers"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 409);
    }

    #[tokio::test]
    async fn containers_timeout_is_504() {
        let path = hanging_scheduler().await;
        let (app, id) = app_with(path, 25, true).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/v1/servers/{id}/containers"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 504);
    }

    #[tokio::test]
    async fn malformed_scheduler_response_is_502() {
        let path = mock_scheduler("not-json", 200).await;
        let (app, id) = app_with(path, 500, true).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/v1/servers/{id}/containers"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 502);
    }

    #[tokio::test]
    async fn spa_fallback_serves_index() {
        let (app, _) = app_with(PathBuf::from("/tmp/missing.sock"), 50, true).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/clusters")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(res.headers()[axum::http::header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .contains("text/html"));
    }
}
