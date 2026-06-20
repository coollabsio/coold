//! JSON envelope types exchanged with the central caller (Laravel) over the
//! flux UDS. Serde shape is deliberately stable — these are the public API.
//!
//! Coold side (`/v1/coold/*`): `DispatchEnvelope` / `ResponseEnvelope`.
//! Build side (`/v1/build/*`): `BuildDispatchEnvelope` / `BuildResponseEnvelope`.

use serde::{Deserialize, Serialize};

use coolify_proto::agent::v1::{
    BuildResponseBody as ProtoBuildResponseBody, Response as ProtoResponse,
};

// ─── Stream inventory ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StreamInventoryItem {
    pub host_id: String,
    pub caps: Vec<String>,
    pub builder_capacity: u32,
}

// ─── Coold dispatch ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DispatchEnvelope {
    pub host_id: String,
    pub request_id: String,
    pub command: CommandPayload,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandPayload {
    ListContainers,
    ApplyCaddyIngress {
        caddyfile: String,
        #[serde(default)]
        apps: Vec<CaddyAppIngressFile>,
        #[serde(default = "default_caddy_mesh_network")]
        mesh_network: String,
    },
    StopCaddyIngress,
}

#[derive(Debug, Deserialize)]
pub struct CaddyAppIngressFile {
    pub name: String,
    pub caddyfile: String,
}

fn default_caddy_mesh_network() -> String {
    "coolify-default-mesh".into()
}

#[derive(Debug, Serialize)]
pub struct ResponseEnvelope {
    pub request_id: String,
    #[serde(flatten)]
    pub body: ResponseBody,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseBody {
    Ok { data: serde_json::Value },
    Error { code: u32, message: String },
}

impl ResponseBody {
    /// Convert a coold `Response` into a `ResponseBody`. Returns `None` for
    /// `Build` bodies (those belong on the build lane).
    pub fn try_from_proto(resp: ProtoResponse) -> Option<Self> {
        use coolify_proto::agent::v1::response::Body;
        match resp.body {
            Some(Body::ListContainers(r)) => {
                let data = serde_json::to_value(
                    r.containers
                        .iter()
                        .map(|c| {
                            serde_json::json!({
                                "id": c.id,
                                "name": c.name,
                                "image": c.image,
                                "state": c.state,
                                "networks": c.networks,
                            })
                        })
                        .collect::<Vec<_>>(),
                )
                .unwrap_or(serde_json::Value::Null);
                Some(ResponseBody::Ok { data })
            }
            Some(Body::ApplyCaddyIngress(r)) => Some(ResponseBody::Ok {
                data: serde_json::json!({ "output": r.output }),
            }),
            Some(Body::StopCaddyIngress(r)) => Some(ResponseBody::Ok {
                data: serde_json::json!({ "output": r.output }),
            }),
            Some(Body::Error(e)) => Some(ResponseBody::Error {
                code: e.code,
                message: e.message,
            }),
            Some(Body::Build(_)) => None,
            None => Some(ResponseBody::Error {
                code: 500,
                message: "empty response body".into(),
            }),
        }
    }
}

// ─── Build dispatch ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct BuildDispatchEnvelope {
    /// Pin to a specific host. Absent → flux load-balances across
    /// builder-capable hosts.
    #[serde(default)]
    pub host_id: Option<String>,
    pub request_id: String,
    pub command: BuildCommandPayload,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BuildCommandPayload {
    StaticBuild {
        repo_url: String,
        git_ref: String,
        target_image: String,
        #[serde(default)]
        output_dir: Option<String>,
        #[serde(default)]
        base_image: Option<String>,
    },
    Cancel {},
}

#[derive(Debug, Serialize)]
pub struct BuildResponseEnvelope {
    pub request_id: String,
    #[serde(flatten)]
    pub body: BuildResponseBody,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BuildResponseBody {
    Ok {
        digest: String,
        registry_ref: String,
        duration_ms: u64,
    },
    Error {
        code: u32,
        message: String,
        stage: String,
    },
}

impl BuildResponseBody {
    pub fn from_proto(body: ProtoBuildResponseBody) -> Self {
        use coolify_proto::agent::v1::build_response_body;
        match body.body {
            Some(build_response_body::Body::Ok(r)) => BuildResponseBody::Ok {
                digest: r.digest,
                registry_ref: r.registry_ref,
                duration_ms: r.duration_ms,
            },
            Some(build_response_body::Body::Err(e)) => BuildResponseBody::Error {
                code: e.code,
                message: e.message,
                stage: e.stage,
            },
            None => BuildResponseBody::Error {
                code: 500,
                message: "empty build response body".into(),
                stage: String::new(),
            },
        }
    }
}
