//! HTTP-over-Unix-domain-socket bridge.
//!
//! Access control is the filesystem: the socket is bound as
//! `owner:group` = `<flux_user>:<COOLIFY_FLUX_UNIX_SOCKET_GROUP>` with mode
//! `0660`. No TLS, no auth header — any local process with group
//! membership on the socket is a trusted central-plane caller.

use std::ffi::CString;
use std::os::unix::fs::{chown, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use hyper::body::Incoming;
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use serde::{Deserialize, Serialize};
use tokio::net::UnixListener;
use tokio::sync::oneshot;
use tower::Service;
use tracing::{info, warn};

use crate::config::Config;
use crate::envelope::{
    BuildCommandPayload, BuildDispatchEnvelope, BuildResponseBody, BuildResponseEnvelope,
    DispatchEnvelope, ResponseBody, ResponseEnvelope,
};
use crate::routing::{route_build, route_coold, RouteOutcome};
use crate::state::{
    InsertOutcome, ParkResult, Pending, PendingKind, ResponseData, Streams, DISPATCH_TIMEOUT_SECS,
};

#[derive(Clone)]
struct AppState {
    streams: Streams,
    pending: Pending,
    pending_max: usize,
}

pub async fn run(config: Config, streams: Streams, pending: Pending) -> Result<()> {
    let state = AppState {
        streams,
        pending,
        pending_max: config.pending_max,
    };

    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/streams", get(stream_inventory))
        .route("/v1/coold/dispatch", post(coold_dispatch))
        .route("/v1/build/dispatch", post(build_dispatch))
        .route("/v1/build/result/:request_id", get(build_result))
        .route("/v1/build/:request_id/cancel", post(build_cancel))
        .with_state(state);

    let listener = bind(
        &config.unix_socket_path,
        config.unix_socket_group.as_deref(),
    )?;
    info!(path = %config.unix_socket_path.display(), "UDS bridge listening");

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "UDS accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let io = TokioIo::new(stream);
        let app = app.clone();
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                let mut app = app.clone();
                async move {
                    let req = req.map(axum::body::Body::new);
                    app.call(req).await
                }
            });
            if let Err(e) = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                warn!(error = ?e, "UDS connection error");
            }
        });
    }
}

/// Bind a `UnixListener` at `path`, apply `0660` perms, and `chown :<group>`
/// if a group was configured. Parent dir is created if absent. An existing
/// stale socket file at `path` is removed first.
fn bind(path: &Path, group: Option<&str>) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow!("remove stale socket {}: {e}", path.display())),
    }
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;

    let mode = match group {
        Some(_) => 0o660,
        None => 0o600,
    };
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod {}", path.display()))?;

    if let Some(g) = group {
        let gid = resolve_gid(g).with_context(|| format!("resolve group {g}"))?;
        chown(path, None, Some(gid)).with_context(|| format!("chown :{g} {}", path.display()))?;
    }

    Ok(listener)
}

fn resolve_gid(name: &str) -> Result<u32> {
    let cname = CString::new(name).map_err(|e| anyhow!("group name: {e}"))?;
    // Safety: `getgrnam` returns a pointer to a static struct owned by libc;
    // we read `gr_gid` immediately and do not retain the pointer. Called
    // once at startup, so thread-safety of the shared buffer is moot.
    let entry = unsafe { libc::getgrnam(cname.as_ptr()) };
    if entry.is_null() {
        return Err(anyhow!("group {name} not found"));
    }
    Ok(unsafe { (*entry).gr_gid })
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
}

async fn stream_inventory(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.streams.snapshot())
}

async fn coold_dispatch(State(st): State<AppState>, Json(env): Json<DispatchEnvelope>) -> Response {
    let request_id = env.request_id.clone();
    let host_id = env.host_id.clone();

    match route_coold(&st.streams, env) {
        RouteOutcome::SendCoold {
            host_id: target,
            msg,
        } => {
            match st.pending.insert_waiting(
                request_id.clone(),
                target.clone(),
                PendingKind::Coold,
                st.pending_max,
            ) {
                InsertOutcome::Inserted => {}
                InsertOutcome::Duplicate => {
                    return coold_err(&request_id, 409, "request_id already in flight");
                }
                InsertOutcome::AtCapacity => {
                    return coold_err(&request_id, 503, "flux at pending-dispatch capacity");
                }
            }

            let rx = match st.pending.park(&request_id) {
                ParkResult::Parked(rx) => rx,
                ParkResult::AlreadyLanded(_) | ParkResult::NotFound => {
                    return coold_err(&request_id, 500, "pending state lost after insert");
                }
            };

            let Some(tx) = st.streams.get_tx(&target) else {
                st.pending.remove(&request_id);
                return coold_err(&request_id, 503, "host stream gone");
            };
            if tx.send(msg).await.is_err() {
                st.pending.remove(&request_id);
                return coold_err(&request_id, 503, "host stream send failed");
            }

            await_coold(&request_id, rx).await
        }
        RouteOutcome::PushError { code, message } => {
            warn!(%request_id, %host_id, %code, %message, "coold dispatch rejected");
            coold_err(&request_id, code, message)
        }
        _ => coold_err(&request_id, 500, "routing mismatch"),
    }
}

async fn await_coold(request_id: &str, rx: oneshot::Receiver<ResponseData>) -> Response {
    let timeout = Duration::from_secs(DISPATCH_TIMEOUT_SECS);
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(ResponseData::Coold(body))) => Json(ResponseEnvelope {
            request_id: request_id.to_owned(),
            body,
        })
        .into_response(),
        Ok(Ok(ResponseData::Build(_))) => {
            coold_err(request_id, 500, "build response on coold lane")
        }
        // Sink dropped without a value (sweeper evicted on timeout, or the
        // entry was removed by a send-failure path above).
        Ok(Err(_)) => coold_err(request_id, 504, "dispatch timeout"),
        Err(_) => coold_err(request_id, 504, "dispatch timeout"),
    }
}

fn coold_err(request_id: &str, code: u32, message: &str) -> Response {
    let env = ResponseEnvelope {
        request_id: request_id.to_owned(),
        body: ResponseBody::Error {
            code,
            message: message.to_owned(),
        },
    };
    let status = code_to_status(code);
    (status, Json(env)).into_response()
}

// ─── Build ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct BuildDispatchAck {
    request_id: String,
}

async fn build_dispatch(
    State(st): State<AppState>,
    Json(env): Json<BuildDispatchEnvelope>,
) -> Response {
    // Cancel is not valid on the dispatch endpoint; route it through the
    // dedicated cancel path for clarity.
    if matches!(env.command, BuildCommandPayload::Cancel {}) {
        return build_err(
            &env.request_id,
            400,
            "use /v1/build/{id}/cancel for cancels",
            "dispatch",
        );
    }

    let request_id = env.request_id.clone();
    match route_build(&st.streams, &st.pending, st.pending_max, env) {
        RouteOutcome::SendBuild { host_id, msg } => {
            let Some(tx) = st.streams.get_tx(&host_id) else {
                st.pending.remove(&request_id);
                return build_err(&request_id, 503, "builder host disconnected", "dispatch");
            };
            if let Err(e) = tx.send(msg).await {
                st.pending.remove(&request_id);
                warn!(%request_id, error = %e, "send build to host stream failed");
                return build_err(&request_id, 503, "host stream send failed", "dispatch");
            }
            (StatusCode::ACCEPTED, Json(BuildDispatchAck { request_id })).into_response()
        }
        RouteOutcome::PushError { code, message } => {
            warn!(%request_id, %code, %message, "build dispatch rejected");
            build_err(&request_id, code, message, "dispatch")
        }
        _ => build_err(&request_id, 500, "routing mismatch", "dispatch"),
    }
}

async fn build_result(
    State(st): State<AppState>,
    AxumPath(request_id): AxumPath<String>,
    axum::extract::Query(q): axum::extract::Query<ResultQuery>,
) -> Response {
    match st.pending.park(&request_id) {
        ParkResult::AlreadyLanded(ResponseData::Build(body)) => {
            Json(BuildResponseEnvelope { request_id, body }).into_response()
        }
        ParkResult::AlreadyLanded(ResponseData::Coold(_)) => {
            build_err(&request_id, 500, "coold response on build lane", "result")
        }
        ParkResult::NotFound => build_err(&request_id, 404, "unknown request_id", "result"),
        ParkResult::Parked(rx) => {
            // Cap caller-supplied poll waits so a malicious or buggy client
            // can't pin a UDS connection forever by passing `u64::MAX`.
            const MAX_POLL_MS: u64 = 300_000;
            let raw_ms = q.timeout_ms.unwrap_or(30_000).min(MAX_POLL_MS);
            let timeout = Duration::from_millis(raw_ms);
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(ResponseData::Build(body))) => {
                    Json(BuildResponseEnvelope { request_id, body }).into_response()
                }
                Ok(Ok(ResponseData::Coold(_))) => {
                    build_err(&request_id, 500, "coold response on build lane", "result")
                }
                Ok(Err(_)) | Err(_) => (
                    StatusCode::REQUEST_TIMEOUT,
                    Json(BuildResponseEnvelope {
                        request_id: request_id.clone(),
                        body: BuildResponseBody::Error {
                            code: 408,
                            message: "result poll timed out".into(),
                            stage: "result".into(),
                        },
                    }),
                )
                    .into_response(),
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResultQuery {
    #[serde(default)]
    timeout_ms: Option<u64>,
}

async fn build_cancel(
    State(st): State<AppState>,
    AxumPath(request_id): AxumPath<String>,
) -> Response {
    let env = BuildDispatchEnvelope {
        host_id: None,
        request_id: request_id.clone(),
        command: BuildCommandPayload::Cancel {},
    };
    match route_build(&st.streams, &st.pending, st.pending_max, env) {
        RouteOutcome::SendCancel { host_id, msg } => {
            let Some(tx) = st.streams.get_tx(&host_id) else {
                warn!(%request_id, %host_id, "cancel: owning host disconnected");
                return StatusCode::NO_CONTENT.into_response();
            };
            if let Err(e) = tx.send(msg).await {
                warn!(%request_id, error = %e, "send cancel to host stream failed");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        RouteOutcome::DropCancelHostGone { host_id } => {
            warn!(%request_id, %host_id, "cancel: owning host lost builder cap");
            StatusCode::NO_CONTENT.into_response()
        }
        RouteOutcome::PushError { code, message } => {
            build_err(&request_id, code, message, "cancel")
        }
        _ => build_err(&request_id, 500, "routing mismatch", "cancel"),
    }
}

fn build_err(request_id: &str, code: u32, message: &str, stage: &str) -> Response {
    let env = BuildResponseEnvelope {
        request_id: request_id.to_owned(),
        body: BuildResponseBody::Error {
            code,
            message: message.to_owned(),
            stage: stage.to_owned(),
        },
    };
    let status = code_to_status(code);
    (status, Json(env)).into_response()
}

fn code_to_status(code: u32) -> StatusCode {
    match code {
        400..=499 => StatusCode::from_u16(code as u16).unwrap_or(StatusCode::BAD_REQUEST),
        500..=599 => StatusCode::from_u16(code as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}
