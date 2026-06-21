//! JSON envelope types exchanged with the central caller (Laravel) over the
//! flux UDS. Serde shape is deliberately stable — these are the public API.
//!
//! Coold side (`/v1/coold/*`): `DispatchEnvelope` / `ResponseEnvelope`.

use serde::{Deserialize, Serialize};

use coolify_proto::agent::v1::Response as ProtoResponse;

// ─── Stream inventory ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StreamInventoryItem {
    pub host_id: String,
    pub caps: Vec<String>,
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
    #[serde(rename = "images.pull")]
    ImagesPull { reference: String },
    #[serde(rename = "images.list")]
    ImagesList,
    #[serde(rename = "images.delete")]
    ImagesDelete {
        reference: String,
        #[serde(default)]
        force: bool,
    },
    #[serde(rename = "containers.create")]
    ContainersCreate {
        name: String,
        image: String,
        #[serde(default)]
        command: Vec<String>,
        #[serde(default)]
        env: Vec<String>,
        #[serde(default)]
        networks: Vec<String>,
        #[serde(default)]
        volumes: Vec<String>,
        #[serde(default)]
        ports: Vec<PortMapping>,
        #[serde(default)]
        dns: Vec<String>,
        #[serde(default)]
        restart_policy: String,
        #[serde(default)]
        privileged: bool,
        #[serde(default)]
        network_mode: String,
        #[serde(default)]
        capabilities: Vec<String>,
    },
    #[serde(rename = "containers.start")]
    ContainersStart { id: String },
    #[serde(rename = "containers.stop")]
    ContainersStop {
        id: String,
        #[serde(default)]
        timeout_seconds: u32,
    },
    #[serde(rename = "containers.restart")]
    ContainersRestart {
        id: String,
        #[serde(default)]
        timeout_seconds: u32,
    },
    #[serde(rename = "containers.delete")]
    ContainersDelete {
        id: String,
        #[serde(default)]
        force: bool,
    },
    #[serde(rename = "containers.inspect")]
    ContainersInspect { id: String },
    #[serde(rename = "containers.list")]
    ContainersList,
    #[serde(rename = "containers.logs")]
    ContainersLogs {
        id: String,
        #[serde(default)]
        tail: u32,
        #[serde(default = "default_logs_stdout")]
        stdout: bool,
        #[serde(default)]
        stderr: bool,
    },
    #[serde(rename = "containers.exec")]
    ContainersExec { id: String, command: Vec<String> },
    #[serde(rename = "containers.healthcheck.run")]
    ContainersHealthcheckRun { id: String },
    #[serde(rename = "apply_caddy_ingress")]
    ApplyCaddyIngress {
        caddyfile: String,
        #[serde(default)]
        apps: Vec<CaddyAppIngressFile>,
        #[serde(default = "default_caddy_mesh_network")]
        mesh_network: String,
    },
    #[serde(rename = "stop_caddy_ingress")]
    StopCaddyIngress,
}

#[derive(Debug, Deserialize)]
pub struct PortMapping {
    pub host_ip: String,
    pub host_port: u32,
    pub container_port: u32,
    pub protocol: String,
}

fn default_logs_stdout() -> bool {
    true
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
    /// Convert a coold `Response` into a `ResponseBody`.
    pub fn try_from_proto(resp: ProtoResponse) -> Option<Self> {
        use coolify_proto::agent::v1::response::Body;
        match resp.body {
            Some(Body::ContainersList(r)) => {
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
            Some(Body::ImagesPull(r)) => Some(ResponseBody::Ok {
                data: serde_json::json!({ "digest": r.digest, "output": r.output }),
            }),
            Some(Body::ImagesList(r)) => Some(ResponseBody::Ok {
                data: serde_json::to_value(
                    r.images
                        .iter()
                        .map(|image| {
                            serde_json::json!({
                                "id": image.id,
                                "repo_tags": image.repo_tags,
                                "repo_digests": image.repo_digests,
                                "size": image.size,
                                "created": image.created,
                            })
                        })
                        .collect::<Vec<_>>(),
                )
                .unwrap_or(serde_json::Value::Null),
            }),
            Some(Body::ImagesDelete(r)) => Some(ResponseBody::Ok {
                data: serde_json::json!({ "output": r.output }),
            }),
            Some(Body::ContainersCreate(r)) => Some(ResponseBody::Ok {
                data: serde_json::json!({ "id": r.id }),
            }),
            Some(Body::ContainersStart(r)) => Some(ResponseBody::Ok {
                data: serde_json::json!({ "output": r.output }),
            }),
            Some(Body::ContainersStop(r)) => Some(ResponseBody::Ok {
                data: serde_json::json!({ "output": r.output }),
            }),
            Some(Body::ContainersRestart(r)) => Some(ResponseBody::Ok {
                data: serde_json::json!({ "output": r.output }),
            }),
            Some(Body::ContainersDelete(r)) => Some(ResponseBody::Ok {
                data: serde_json::json!({ "output": r.output }),
            }),
            Some(Body::ContainersInspect(r)) => Some(ResponseBody::Ok {
                data: serde_json::from_str(&r.json)
                    .unwrap_or_else(|_| serde_json::json!({ "raw": r.json })),
            }),
            Some(Body::ContainersLogs(r)) => Some(ResponseBody::Ok {
                data: serde_json::json!({ "output": r.output }),
            }),
            Some(Body::ContainersExec(r)) => Some(ResponseBody::Ok {
                data: serde_json::json!({ "exit_code": r.exit_code, "output": r.output }),
            }),
            Some(Body::ContainersHealthcheckRun(r)) => Some(ResponseBody::Ok {
                data: serde_json::json!({ "output": r.output }),
            }),
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
            None => Some(ResponseBody::Error {
                code: 500,
                message: "empty response body".into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coolify_proto::agent::v1::{
        response, ContainerSummary, ContainersCreateResp, ContainersDeleteResp, ContainersExecResp,
        ContainersHealthcheckRunResp, ContainersInspectResp, ContainersListResp,
        ContainersLogsResp, ContainersRestartResp, ContainersStartResp, ContainersStopResp,
        ImageSummary, ImagesDeleteResp, ImagesListResp, ImagesPullResp,
    };

    fn data_for(body: response::Body) -> serde_json::Value {
        match ResponseBody::try_from_proto(ProtoResponse {
            request_id: "r1".into(),
            body: Some(body),
        })
        .expect("response body")
        {
            ResponseBody::Ok { data } => data,
            ResponseBody::Error { code, message } => {
                panic!("expected ok response, got {code}: {message}")
            }
        }
    }

    #[test]
    fn maps_all_image_primitive_responses_to_flux_json() {
        let pull = data_for(response::Body::ImagesPull(ImagesPullResp {
            digest: "sha256:abc".into(),
            output: "pulled".into(),
        }));
        assert_eq!(pull["digest"], "sha256:abc");
        assert_eq!(pull["output"], "pulled");

        let list = data_for(response::Body::ImagesList(ImagesListResp {
            images: vec![ImageSummary {
                id: "image-id".into(),
                repo_tags: vec!["docker.io/library/nginx:alpine".into()],
                repo_digests: vec!["docker.io/library/nginx@sha256:abc".into()],
                size: 123,
                created: "2026-06-21T00:00:00Z".into(),
            }],
        }));
        assert_eq!(list[0]["id"], "image-id");
        assert_eq!(list[0]["repo_tags"][0], "docker.io/library/nginx:alpine");
        assert_eq!(list[0]["size"], 123);

        let delete = data_for(response::Body::ImagesDelete(ImagesDeleteResp {
            output: "deleted".into(),
        }));
        assert_eq!(delete["output"], "deleted");
    }

    #[test]
    fn maps_all_container_primitive_responses_to_flux_json() {
        let create = data_for(response::Body::ContainersCreate(ContainersCreateResp {
            id: "container-id".into(),
        }));
        assert_eq!(create["id"], "container-id");

        let start = data_for(response::Body::ContainersStart(ContainersStartResp {
            output: "started".into(),
        }));
        assert_eq!(start["output"], "started");

        let stop = data_for(response::Body::ContainersStop(ContainersStopResp {
            output: "stopped".into(),
        }));
        assert_eq!(stop["output"], "stopped");

        let restart = data_for(response::Body::ContainersRestart(ContainersRestartResp {
            output: "restarted".into(),
        }));
        assert_eq!(restart["output"], "restarted");

        let delete = data_for(response::Body::ContainersDelete(ContainersDeleteResp {
            output: "deleted".into(),
        }));
        assert_eq!(delete["output"], "deleted");

        let inspect = data_for(response::Body::ContainersInspect(ContainersInspectResp {
            json: r#"{"Id":"container-id","State":{"Status":"running"}}"#.into(),
        }));
        assert_eq!(inspect["Id"], "container-id");
        assert_eq!(inspect["State"]["Status"], "running");

        let list = data_for(response::Body::ContainersList(ContainersListResp {
            containers: vec![ContainerSummary {
                id: "container-id".into(),
                name: "web".into(),
                image: "docker.io/library/nginx:alpine".into(),
                state: "running".into(),
                networks: vec!["coolify-default-mesh".into()],
            }],
        }));
        assert_eq!(list[0]["id"], "container-id");
        assert_eq!(list[0]["name"], "web");
        assert_eq!(list[0]["networks"][0], "coolify-default-mesh");

        let logs = data_for(response::Body::ContainersLogs(ContainersLogsResp {
            output: "hello logs".into(),
        }));
        assert_eq!(logs["output"], "hello logs");

        let exec = data_for(response::Body::ContainersExec(ContainersExecResp {
            exit_code: 0,
            output: "hello exec".into(),
        }));
        assert_eq!(exec["exit_code"], 0);
        assert_eq!(exec["output"], "hello exec");

        let healthcheck = data_for(response::Body::ContainersHealthcheckRun(
            ContainersHealthcheckRunResp {
                output: "healthy".into(),
            },
        ));
        assert_eq!(healthcheck["output"], "healthy");
    }
}
