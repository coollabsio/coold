//! Pure routing: given an envelope + state, decide the next action.
//! No I/O, no stream writes — callers (the UDS bridge) map outcomes to
//! side effects. Separating this keeps routing logic unit-testable.

use coolify_proto::agent::v1::{
    server_msg, ApplyCaddyIngressReq, BuildRequest,
    CaddyAppIngressFile as ProtoCaddyAppIngressFile, CancelBuild, ListContainersReq, ServerMsg,
    StaticConfig, StopCaddyIngressReq,
};

use crate::envelope::{
    BuildCommandPayload, BuildDispatchEnvelope, CommandPayload, DispatchEnvelope,
};
use crate::state::{InsertOutcome, Pending, PendingKind, Streams};

/// What the caller should do next. `SendCoold` / `SendBuild` / `SendCancel`
/// all carry a fully-formed `ServerMsg` ready to push down the host's
/// `mpsc` tx. `PushError` maps to an HTTP error response. `DropCancelHostGone`
/// is log-only — the original build's final response will have reported the
/// failure already.
#[derive(Debug)]
pub enum RouteOutcome {
    SendCoold { host_id: String, msg: ServerMsg },
    SendBuild { host_id: String, msg: ServerMsg },
    SendCancel { host_id: String, msg: ServerMsg },
    PushError { code: u32, message: &'static str },
    DropCancelHostGone { host_id: String },
}

/// Route a coold command envelope to its target host. Currently only
/// `list_containers` — expects `host_id` pin to be present.
pub fn route_coold(streams: &Streams, env: DispatchEnvelope) -> RouteOutcome {
    if streams.get(&env.host_id).is_none() {
        return RouteOutcome::PushError {
            code: 404,
            message: "host not connected",
        };
    }

    let cmd = match env.command {
        CommandPayload::ListContainers => server_msg::Command::ListContainers(ListContainersReq {}),
        CommandPayload::ApplyCaddyIngress {
            caddyfile,
            apps,
            mesh_network,
        } => server_msg::Command::ApplyCaddyIngress(ApplyCaddyIngressReq {
            caddyfile,
            mesh_network,
            apps: apps
                .into_iter()
                .map(|app| ProtoCaddyAppIngressFile {
                    name: app.name,
                    caddyfile: app.caddyfile,
                })
                .collect(),
        }),
        CommandPayload::StopCaddyIngress => {
            server_msg::Command::StopCaddyIngress(StopCaddyIngressReq {})
        }
    };

    let msg = ServerMsg {
        request_id: env.request_id.clone(),
        command: Some(cmd),
    };
    RouteOutcome::SendCoold {
        host_id: env.host_id,
        msg,
    }
}

/// Route a build-command envelope. For `static_build`, picks the target host
/// (pinned or load-balanced) and inserts a `Pending` entry keyed by
/// `request_id` so the gRPC response path can deliver to the right waiter.
/// For `cancel`, looks up the owning host via `Pending` and produces a
/// `CancelBuild` message.
pub fn route_build(
    streams: &Streams,
    pending: &Pending,
    pending_max: usize,
    env: BuildDispatchEnvelope,
) -> RouteOutcome {
    match env.command {
        BuildCommandPayload::StaticBuild {
            repo_url,
            git_ref,
            target_image,
            output_dir,
            base_image,
        } => {
            let target_host = match env.host_id.as_deref() {
                Some(id) => {
                    if !streams.has_cap(id, "builder") {
                        return RouteOutcome::PushError {
                            code: 503,
                            message: "host has no builder capability",
                        };
                    }
                    id.to_string()
                }
                None => match streams.pick_host_with_cap("builder") {
                    Some(id) => id,
                    None => {
                        return RouteOutcome::PushError {
                            code: 503,
                            message: "no builder-capable host connected",
                        };
                    }
                },
            };

            match pending.insert_waiting(
                env.request_id.clone(),
                target_host.clone(),
                PendingKind::Build,
                pending_max,
            ) {
                InsertOutcome::Inserted => {}
                InsertOutcome::Duplicate => {
                    return RouteOutcome::PushError {
                        code: 409,
                        message: "request_id already in flight",
                    };
                }
                InsertOutcome::AtCapacity => {
                    return RouteOutcome::PushError {
                        code: 503,
                        message: "flux at pending-dispatch capacity",
                    };
                }
            }

            let build_req = BuildRequest {
                repo_url,
                git_ref,
                stack: coolify_proto::agent::v1::BuildStack::Static as i32,
                target_image,
                cache_key: String::new(),
                static_cfg: Some(StaticConfig {
                    output_dir: output_dir.unwrap_or_else(|| "dist".into()),
                    base_image: base_image
                        .unwrap_or_else(|| "docker.io/library/nginx:alpine".into()),
                }),
            };
            let msg = ServerMsg {
                request_id: env.request_id,
                command: Some(server_msg::Command::Build(build_req)),
            };
            RouteOutcome::SendBuild {
                host_id: target_host,
                msg,
            }
        }
        BuildCommandPayload::Cancel {} => {
            let Some(entry) = pending.get(&env.request_id) else {
                return RouteOutcome::PushError {
                    code: 404,
                    message: "request_id not in flight",
                };
            };
            if !streams.has_cap(&entry.host_id, "builder") {
                return RouteOutcome::DropCancelHostGone {
                    host_id: entry.host_id,
                };
            }
            let msg = ServerMsg {
                request_id: env.request_id,
                command: Some(server_msg::Command::CancelBuild(CancelBuild {})),
            };
            RouteOutcome::SendCancel {
                host_id: entry.host_id,
                msg,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{PendingKind, StreamHandle};
    use tokio::sync::mpsc;

    const CAP: usize = 16;

    fn insert_host(streams: &Streams, host_id: &str, caps: &[&str]) -> mpsc::Receiver<ServerMsg> {
        let (tx, rx) = mpsc::channel::<ServerMsg>(16);
        streams.insert(
            host_id.to_owned(),
            StreamHandle {
                tx,
                caps: caps.iter().map(|c| c.to_string()).collect(),
                builder_capacity: 2,
            },
        );
        rx
    }

    fn static_env(request_id: &str, host_id: Option<&str>) -> BuildDispatchEnvelope {
        BuildDispatchEnvelope {
            host_id: host_id.map(str::to_owned),
            request_id: request_id.to_owned(),
            command: BuildCommandPayload::StaticBuild {
                repo_url: "https://example.com/repo".into(),
                git_ref: "main".into(),
                target_image: "localhost/t".into(),
                output_dir: None,
                base_image: None,
            },
        }
    }

    fn cancel_env(request_id: &str) -> BuildDispatchEnvelope {
        BuildDispatchEnvelope {
            host_id: None,
            request_id: request_id.to_owned(),
            command: BuildCommandPayload::Cancel {},
        }
    }

    #[test]
    fn dispatch_pinned_to_builder_host_routes_to_that_host() {
        let streams = Streams::new();
        let pending = Pending::new();
        let _rx_a = insert_host(&streams, "A", &["coold", "builder"]);
        let _rx_b = insert_host(&streams, "B", &["coold", "builder"]);

        let out = route_build(&streams, &pending, CAP, static_env("r1", Some("A")));
        match out {
            RouteOutcome::SendBuild { host_id, .. } => assert_eq!(host_id, "A"),
            other => panic!("expected SendBuild to A, got {other:?}"),
        }
        // Pending entry must be created for timeout sweeping.
        let entry = pending.get("r1").expect("pending populated");
        assert_eq!(entry.host_id, "A");
        assert_eq!(entry.kind, PendingKind::Build);
    }

    #[test]
    fn dispatch_pinned_to_coold_only_host_returns_503() {
        let streams = Streams::new();
        let pending = Pending::new();
        insert_host(&streams, "A", &["coold"]);

        let out = route_build(&streams, &pending, CAP, static_env("r1", Some("A")));
        match out {
            RouteOutcome::PushError { code, message } => {
                assert_eq!(code, 503);
                assert_eq!(message, "host has no builder capability");
            }
            other => panic!("expected 503, got {other:?}"),
        }
        assert!(pending.get("r1").is_none(), "no pending on reject");
    }

    #[test]
    fn dispatch_pinned_to_unknown_host_returns_503() {
        let streams = Streams::new();
        let pending = Pending::new();
        insert_host(&streams, "A", &["coold", "builder"]);

        let out = route_build(&streams, &pending, CAP, static_env("r1", Some("Z")));
        match out {
            RouteOutcome::PushError { code, message } => {
                assert_eq!(code, 503);
                assert_eq!(message, "host has no builder capability");
            }
            other => panic!("expected 503, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_without_host_id_picks_builder_capable_host() {
        let streams = Streams::new();
        let pending = Pending::new();
        // A has only coold; B is builder-capable. Router must pick B.
        insert_host(&streams, "A", &["coold"]);
        insert_host(&streams, "B", &["coold", "builder"]);

        let out = route_build(&streams, &pending, CAP, static_env("r1", None));
        match out {
            RouteOutcome::SendBuild { host_id, .. } => assert_eq!(host_id, "B"),
            other => panic!("expected SendBuild to B, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_without_host_id_when_no_builder_returns_503() {
        let streams = Streams::new();
        let pending = Pending::new();
        insert_host(&streams, "A", &["coold"]);
        insert_host(&streams, "B", &["coold"]);

        let out = route_build(&streams, &pending, CAP, static_env("r1", None));
        match out {
            RouteOutcome::PushError { code, message } => {
                assert_eq!(code, 503);
                assert_eq!(message, "no builder-capable host connected");
            }
            other => panic!("expected 503, got {other:?}"),
        }
    }

    #[test]
    fn cancel_routes_to_owning_host() {
        let streams = Streams::new();
        let pending = Pending::new();
        insert_host(&streams, "A", &["coold", "builder"]);
        insert_host(&streams, "B", &["coold", "builder"]);
        let _ = pending.insert_waiting("r1".into(), "B".into(), PendingKind::Build, CAP);

        let out = route_build(&streams, &pending, CAP, cancel_env("r1"));
        match out {
            RouteOutcome::SendCancel { host_id, msg } => {
                assert_eq!(host_id, "B");
                assert_eq!(msg.request_id, "r1");
                assert!(matches!(
                    msg.command,
                    Some(server_msg::Command::CancelBuild(_))
                ));
            }
            other => panic!("expected SendCancel to B, got {other:?}"),
        }
    }

    #[test]
    fn cancel_for_unknown_request_returns_404() {
        let streams = Streams::new();
        let pending = Pending::new();
        insert_host(&streams, "A", &["coold", "builder"]);

        let out = route_build(&streams, &pending, CAP, cancel_env("nope"));
        match out {
            RouteOutcome::PushError { code, message } => {
                assert_eq!(code, 404);
                assert_eq!(message, "request_id not in flight");
            }
            other => panic!("expected 404, got {other:?}"),
        }
    }

    #[test]
    fn cancel_when_owning_host_lost_builder_cap_drops_silently() {
        // Host reconnected with fewer caps between build start and cancel.
        let streams = Streams::new();
        let pending = Pending::new();
        let _ = pending.insert_waiting("r1".into(), "A".into(), PendingKind::Build, CAP);
        insert_host(&streams, "A", &["coold"]);

        let out = route_build(&streams, &pending, CAP, cancel_env("r1"));
        match out {
            RouteOutcome::DropCancelHostGone { host_id } => assert_eq!(host_id, "A"),
            other => panic!("expected DropCancelHostGone, got {other:?}"),
        }
    }

    #[test]
    fn coold_dispatch_routes_to_connected_host() {
        let streams = Streams::new();
        let _rx = insert_host(&streams, "A", &["coold"]);
        let env = DispatchEnvelope {
            host_id: "A".into(),
            request_id: "r1".into(),
            command: CommandPayload::ListContainers,
        };
        match route_coold(&streams, env) {
            RouteOutcome::SendCoold { host_id, msg } => {
                assert_eq!(host_id, "A");
                assert_eq!(msg.request_id, "r1");
                assert!(matches!(
                    msg.command,
                    Some(server_msg::Command::ListContainers(_))
                ));
            }
            other => panic!("expected SendCoold, got {other:?}"),
        }
    }

    #[test]
    fn coold_dispatch_routes_caddy_apply_to_connected_host() {
        let streams = Streams::new();
        let _rx = insert_host(&streams, "A", &["coold"]);
        let env = DispatchEnvelope {
            host_id: "A".into(),
            request_id: "r1".into(),
            command: CommandPayload::ApplyCaddyIngress {
                caddyfile: ":80 {\n respond 200\n}".into(),
                apps: vec![crate::envelope::CaddyAppIngressFile {
                    name: "app_1".into(),
                    caddyfile: "example.com {\n reverse_proxy app:80\n}".into(),
                }],
                mesh_network: "coolify-default-mesh".into(),
            },
        };
        match route_coold(&streams, env) {
            RouteOutcome::SendCoold { host_id, msg } => {
                assert_eq!(host_id, "A");
                assert_eq!(msg.request_id, "r1");
                match msg.command {
                    Some(server_msg::Command::ApplyCaddyIngress(req)) => {
                        assert_eq!(req.mesh_network, "coolify-default-mesh");
                        assert!(req.caddyfile.contains("respond 200"));
                        assert_eq!(req.apps.len(), 1);
                        assert_eq!(req.apps[0].name, "app_1");
                    }
                    other => panic!("expected ApplyCaddyIngress, got {other:?}"),
                }
            }
            other => panic!("expected SendCoold, got {other:?}"),
        }
    }

    #[test]
    fn coold_dispatch_unknown_host_returns_404() {
        let streams = Streams::new();
        let env = DispatchEnvelope {
            host_id: "Z".into(),
            request_id: "r1".into(),
            command: CommandPayload::ListContainers,
        };
        match route_coold(&streams, env) {
            RouteOutcome::PushError { code, message } => {
                assert_eq!(code, 404);
                assert_eq!(message, "host not connected");
            }
            other => panic!("expected 404, got {other:?}"),
        }
    }
}
