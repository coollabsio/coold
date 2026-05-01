//! Axum router for the firewall REST surface.
//!
//! Endpoints (all under `/api/v1/firewall`):
//!
//! - `POST   /allow`        create/ensure a rule → `{ id, rule }`
//! - `GET    /allow`        list kernel state → `[{ id, rule }, ...]`
//! - `GET    /allow/:id`    single rule (404 when absent)
//! - `DELETE /allow/:id`    revoke by id (204 even on missing — idempotent)
//! - `POST   /allow/bulk`   `{ add: [...], remove: [id, ...] }` → counts
//! - `POST   /reconcile`    reload kernel chain from the on-disk snapshot
//!
//! Auth: bearer token from `api_token_file`. Anonymous access is refused
//! at server construction (see `server.rs`) — no "dev mode without auth"
//! codepath exists so the surface cannot ship unauthenticated by accident.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tracing::warn;

use super::{
    rule::{AllowRule, DEFAULT_NAMESPACE},
    store::FirewallStore,
};

/// Shared state handed to every handler.
#[derive(Clone)]
pub struct ApiState {
    pub store: FirewallStore,
    pub token: Arc<String>,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/v1/firewall/allow", get(list_allow).post(create_allow))
        .route(
            "/api/v1/firewall/allow/:id",
            get(show_allow).delete(revoke_allow),
        )
        .route("/api/v1/firewall/allow/bulk", post(bulk_allow))
        .route("/api/v1/firewall/reconcile", post(reconcile))
        .route("/healthz", get(healthz))
        .with_state(state)
}

// ---------- handlers ----------

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Debug, Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    namespace: Option<String>,
}

async fn list_allow(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<AllowRule>>, ApiError> {
    authorize(&headers, &s.token)?;
    let raw = s.store.list().await.map_err(ApiError::internal)?;
    // Stamp `default` on legacy rules with an empty namespace so clients see
    // a consistent view regardless of when the rule was installed.
    let rules: Vec<AllowRule> = raw
        .into_iter()
        .map(|mut r| {
            if r.namespace.is_empty() {
                r.namespace = DEFAULT_NAMESPACE.into();
            }
            r
        })
        .filter(|r| match &q.namespace {
            Some(want) if !want.is_empty() => r.namespace == *want,
            _ => true,
        })
        .collect();
    Ok(Json(rules))
}

async fn show_allow(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<AllowRule>, ApiError> {
    authorize(&headers, &s.token)?;
    let rules = s.store.list().await.map_err(ApiError::internal)?;
    rules
        .into_iter()
        .find(|r| r.id.as_deref() == Some(&id))
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("rule {id} not found")))
}

#[derive(Debug, Deserialize)]
struct CreateAllowBody {
    #[serde(flatten)]
    rule: AllowRule,
}

async fn create_allow(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<CreateAllowBody>,
) -> Result<(StatusCode, Json<AllowRule>), ApiError> {
    authorize(&headers, &s.token)?;
    let rule = body.rule.normalize().map_err(ApiError::bad_request)?;
    s.store.apply(&rule).await.map_err(ApiError::internal)?;
    Ok((StatusCode::CREATED, Json(rule)))
}

async fn revoke_allow(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authorize(&headers, &s.token)?;
    s.store
        .revoke_by_id(&id)
        .await
        .map_err(ApiError::internal)?;
    // 204 even when the id didn't exist — DELETE is idempotent, clients
    // shouldn't have to distinguish "never existed" from "already gone".
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct BulkBody {
    #[serde(default)]
    add: Vec<AllowRule>,
    #[serde(default)]
    remove: Vec<String>,
}

async fn bulk_allow(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<BulkBody>,
) -> Result<Json<BulkResponse>, ApiError> {
    authorize(&headers, &s.token)?;
    let mut normalized = Vec::with_capacity(body.add.len());
    for r in body.add {
        normalized.push(r.normalize().map_err(ApiError::bad_request)?);
    }
    let outcome = s
        .store
        .bulk(normalized, body.remove)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(BulkResponse {
        added: outcome.added,
        removed: outcome.removed,
        total: outcome.total,
    }))
}

#[derive(Debug, Serialize)]
struct BulkResponse {
    added: usize,
    removed: usize,
    total: usize,
}

async fn reconcile(
    State(s): State<ApiState>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    authorize(&headers, &s.token)?;
    s.store
        .reconcile_from_file()
        .await
        .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- auth ----------

fn authorize(headers: &HeaderMap, expected: &str) -> Result<(), ApiError> {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::unauthorized("missing Authorization header"))?;

    let token = header
        .strip_prefix("Bearer ")
        .ok_or_else(|| ApiError::unauthorized("Authorization must use Bearer scheme"))?;

    // `subtle::ConstantTimeEq` short-circuits on mismatched lengths but keeps
    // the equal-length comparison constant-time. Cheaper and safer than a
    // hand-rolled loop the optimiser may reshape.
    if !bool::from(token.as_bytes().ct_eq(expected.as_bytes())) {
        return Err(ApiError::unauthorized("invalid token"));
    }
    Ok(())
}

// ---------- error envelope ----------

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn internal(e: anyhow::Error) -> Self {
        // Server-side log keeps the full anyhow chain for ops; the response
        // body is intentionally generic so we don't leak paths, internal
        // identifiers, or shell args to API consumers.
        warn!(error = format!("{e:#}"), "firewall api: internal error");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal error".into(),
        }
    }
    fn bad_request(e: anyhow::Error) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: format!("{e:#}"),
        }
    }
    fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: msg.into(),
        }
    }
    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct Body<'a> {
            error: &'a str,
        }
        (
            self.status,
            Json(Body {
                error: &self.message,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn authorize_accepts_valid_bearer() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        assert!(authorize(&h, "secret").is_ok());
    }

    #[test]
    fn authorize_rejects_bad_scheme() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Basic xyz"),
        );
        assert!(authorize(&h, "secret").is_err());
    }

    #[test]
    fn authorize_rejects_wrong_token() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer nope"),
        );
        assert!(authorize(&h, "secret").is_err());
    }

    #[test]
    fn authorize_rejects_missing_header() {
        let h = HeaderMap::new();
        assert!(authorize(&h, "secret").is_err());
    }
}
