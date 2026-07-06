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
    extract::{Path as PathParam, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use hyper::body::Incoming;
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use serde::Deserialize;
use tokio::net::UnixListener;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::oneshot;
use tower::Service;
use tracing::{info, warn};

use crate::config::Config;
use crate::envelope::{DispatchEnvelope, ResponseBody, ResponseEnvelope};
use crate::revocation::RevocationStore;
use crate::routing::{route_coold, RouteOutcome};
use crate::state::{InsertOutcome, ParkResult, Pending, PendingKind, ResponseData, Streams};

#[derive(Clone)]
struct AppState {
    streams: Streams,
    pending: Pending,
    pending_max: usize,
    dispatch_timeout: Duration,
    revocations: RevocationStore,
}

pub async fn run(
    config: Config,
    streams: Streams,
    pending: Pending,
    revocations: RevocationStore,
) -> Result<()> {
    let state = AppState {
        streams,
        pending,
        pending_max: config.pending_max,
        dispatch_timeout: Duration::from_secs(config.dispatch_timeout_secs),
        revocations,
    };

    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/streams", get(stream_inventory))
        .route("/v1/coold/dispatch", post(coold_dispatch))
        // #3: JWT revocation denylist management (Laravel calls these on server
        // destroy / re-home). Auth is the same filesystem-perm boundary as the
        // rest of the UDS lane — no extra bearer.
        .route(
            "/v1/tokens/revoke",
            post(revoke_token).get(list_revocations),
        )
        .route("/v1/tokens/revoke/:jti", delete(unrevoke_token))
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
    let command_type = env.command.kind();

    info!(%request_id, %host_id, %command_type, "coold dispatch received");

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
            // C2: never block the calling php-fpm worker on a wedged coold. The
            // per-host command channel is bounded (64); a full queue means that
            // host is not draining, so fail fast with 503 instead of awaiting
            // capacity (which would tie up the UDS request indefinitely).
            match tx.try_send(msg) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    st.pending.remove(&request_id);
                    warn!(%request_id, host_id = %target, %command_type, "coold command queue full; rejecting");
                    return coold_err(&request_id, 503, "host command queue full");
                }
                Err(TrySendError::Closed(_)) => {
                    st.pending.remove(&request_id);
                    warn!(%request_id, host_id = %target, %command_type, "coold dispatch stream send failed");
                    return coold_err(&request_id, 503, "host stream send failed");
                }
            }

            info!(%request_id, host_id = %target, %command_type, "coold dispatch forwarded to stream");

            await_coold(&request_id, &target, command_type, rx, st.dispatch_timeout).await
        }
        RouteOutcome::PushError { code, message } => {
            warn!(%request_id, %host_id, %code, %message, "coold dispatch rejected");
            coold_err(&request_id, code, &message)
        }
    }
}

async fn await_coold(
    request_id: &str,
    host_id: &str,
    command_type: &str,
    rx: oneshot::Receiver<ResponseData>,
    timeout: Duration,
) -> Response {
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(ResponseData::Coold(body))) => {
            info!(%request_id, %host_id, %command_type, "coold dispatch response returned to caller");
            Json(ResponseEnvelope {
                request_id: request_id.to_owned(),
                body,
            })
            .into_response()
        }
        // Sink dropped without a value (sweeper evicted on timeout, or the
        // entry was removed by a send-failure path above).
        Ok(Err(_)) => {
            warn!(%request_id, %host_id, %command_type, "coold dispatch response sink dropped");
            coold_err(request_id, 504, "dispatch timeout")
        }
        Err(_) => {
            warn!(%request_id, %host_id, %command_type, "coold dispatch await timed out");
            coold_err(request_id, 504, "dispatch timeout")
        }
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

fn code_to_status(code: u32) -> StatusCode {
    match code {
        400..=499 => StatusCode::from_u16(code as u16).unwrap_or(StatusCode::BAD_REQUEST),
        500..=599 => StatusCode::from_u16(code as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

// ─── #3: revocation endpoints ────────────────────────────────────────────────

/// Body of `POST /v1/tokens/revoke`. `expires_at` (the token `exp`, seconds
/// since epoch) is optional — when given, the sweeper prunes the entry once it
/// can no longer matter.
#[derive(Debug, Deserialize)]
struct RevokeRequest {
    jti: String,
    #[serde(default)]
    expires_at: Option<u64>,
}

/// `POST /v1/tokens/revoke` — add a `jti` to the denylist (persisted).
async fn revoke_token(State(st): State<AppState>, Json(req): Json<RevokeRequest>) -> Response {
    if req.jti.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "jti is required" })),
        )
            .into_response();
    }
    match st.revocations.revoke(req.jti.clone(), req.expires_at) {
        Ok(()) => {
            info!(jti = %req.jti, "JWT jti revoked");
            (
                StatusCode::OK,
                Json(serde_json::json!({ "revoked": req.jti })),
            )
                .into_response()
        }
        Err(e) => {
            warn!(jti = %req.jti, error = format!("{e:#}"), "persist revocation failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "failed to persist revocation" })),
            )
                .into_response()
        }
    }
}

/// `DELETE /v1/tokens/revoke/:jti` — remove a `jti` from the denylist.
async fn unrevoke_token(State(st): State<AppState>, PathParam(jti): PathParam<String>) -> Response {
    match st.revocations.unrevoke(&jti) {
        Ok(removed) => {
            info!(%jti, removed, "JWT jti unrevoked");
            (
                StatusCode::OK,
                Json(serde_json::json!({ "jti": jti, "removed": removed })),
            )
                .into_response()
        }
        Err(e) => {
            warn!(%jti, error = format!("{e:#}"), "persist unrevocation failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "failed to persist revocation" })),
            )
                .into_response()
        }
    }
}

/// `GET /v1/tokens/revoke` — list current denylist entries.
async fn list_revocations(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.revocations.list())
}
