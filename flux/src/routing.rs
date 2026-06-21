//! Pure routing: given an envelope + state, decide the next action.
//! No I/O, no stream writes — callers (the UDS bridge) map outcomes to
//! side effects. Separating this keeps routing logic unit-testable.

use coolify_proto::agent::v1::{
    server_msg, ApplyCaddyIngressReq, CaddyAppIngressFile as ProtoCaddyAppIngressFile,
    ContainersCreateReq, ContainersDeleteReq, ContainersExecReq, ContainersHealthcheckRunReq,
    ContainersInspectReq, ContainersListReq, ContainersLogsReq, ContainersRestartReq,
    ContainersStartReq, ContainersStopReq, ImagesDeleteReq, ImagesListReq, ImagesPullReq,
    PortMapping as ProtoPortMapping, ServerMsg, StopCaddyIngressReq,
};

use crate::envelope::{CommandPayload, DispatchEnvelope};
use crate::state::Streams;

/// What the caller should do next. `SendCoold` carries a fully-formed
/// `ServerMsg` ready to push down the host's mpsc tx. `PushError` maps
/// to an HTTP error response.
#[derive(Debug)]
pub enum RouteOutcome {
    SendCoold { host_id: String, msg: ServerMsg },
    PushError { code: u32, message: &'static str },
}

/// Route a coold command envelope to its pinned target host.
pub fn route_coold(streams: &Streams, env: DispatchEnvelope) -> RouteOutcome {
    if streams.get(&env.host_id).is_none() {
        return RouteOutcome::PushError {
            code: 404,
            message: "host not connected",
        };
    }

    let cmd = match env.command {
        CommandPayload::ImagesPull { reference } => {
            server_msg::Command::ImagesPull(ImagesPullReq { reference })
        }
        CommandPayload::ImagesList => server_msg::Command::ImagesList(ImagesListReq {}),
        CommandPayload::ImagesDelete { reference, force } => {
            server_msg::Command::ImagesDelete(ImagesDeleteReq { reference, force })
        }
        CommandPayload::ContainersCreate {
            name,
            image,
            command,
            env,
            networks,
            volumes,
            ports,
            dns,
            restart_policy,
            privileged,
            network_mode,
            capabilities,
        } => server_msg::Command::ContainersCreate(ContainersCreateReq {
            name,
            image,
            command,
            env,
            networks,
            volumes,
            ports: ports
                .into_iter()
                .map(|port| ProtoPortMapping {
                    host_ip: port.host_ip,
                    host_port: port.host_port,
                    container_port: port.container_port,
                    protocol: port.protocol,
                })
                .collect(),
            dns,
            restart_policy,
            privileged,
            network_mode,
            capabilities,
        }),
        CommandPayload::ContainersStart { id } => {
            server_msg::Command::ContainersStart(ContainersStartReq { id })
        }
        CommandPayload::ContainersStop {
            id,
            timeout_seconds,
        } => server_msg::Command::ContainersStop(ContainersStopReq {
            id,
            timeout_seconds,
        }),
        CommandPayload::ContainersRestart {
            id,
            timeout_seconds,
        } => server_msg::Command::ContainersRestart(ContainersRestartReq {
            id,
            timeout_seconds,
        }),
        CommandPayload::ContainersDelete { id, force } => {
            server_msg::Command::ContainersDelete(ContainersDeleteReq { id, force })
        }
        CommandPayload::ContainersInspect { id } => {
            server_msg::Command::ContainersInspect(ContainersInspectReq { id })
        }
        CommandPayload::ContainersList => server_msg::Command::ContainersList(ContainersListReq {}),
        CommandPayload::ContainersLogs {
            id,
            tail,
            stdout,
            stderr,
        } => server_msg::Command::ContainersLogs(ContainersLogsReq {
            id,
            tail,
            stdout,
            stderr,
        }),
        CommandPayload::ContainersExec { id, command } => {
            server_msg::Command::ContainersExec(ContainersExecReq { id, command })
        }
        CommandPayload::ContainersHealthcheckRun { id } => {
            server_msg::Command::ContainersHealthcheckRun(ContainersHealthcheckRunReq { id })
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{StreamHandle, Streams};
    use tokio::sync::mpsc;

    fn insert_host(streams: &Streams, host_id: &str, caps: &[&str]) -> mpsc::Receiver<ServerMsg> {
        let (tx, rx) = mpsc::channel::<ServerMsg>(16);
        streams.insert(
            host_id.to_owned(),
            StreamHandle {
                tx,
                caps: caps.iter().map(|c| c.to_string()).collect(),
            },
        );
        rx
    }

    #[test]
    fn route_coold_rejects_disconnected_host() {
        let streams = Streams::new();
        let out = route_coold(
            &streams,
            DispatchEnvelope {
                host_id: "missing".into(),
                request_id: "r1".into(),
                command: CommandPayload::ContainersList,
            },
        );

        match out {
            RouteOutcome::PushError { code, message } => {
                assert_eq!(code, 404);
                assert_eq!(message, "host not connected");
            }
            other => panic!("expected PushError, got {other:?}"),
        }
    }

    #[test]
    fn route_coold_builds_containers_list_message() {
        let streams = Streams::new();
        let _rx = insert_host(&streams, "H", &["coold"]);
        let out = route_coold(
            &streams,
            DispatchEnvelope {
                host_id: "H".into(),
                request_id: "r1".into(),
                command: CommandPayload::ContainersList,
            },
        );

        match out {
            RouteOutcome::SendCoold { host_id, msg } => {
                assert_eq!(host_id, "H");
                assert!(matches!(
                    msg.command,
                    Some(server_msg::Command::ContainersList(_))
                ));
            }
            other => panic!("expected SendCoold, got {other:?}"),
        }
    }

    #[test]
    fn route_coold_builds_images_list_message() {
        let streams = Streams::new();
        let _rx = insert_host(&streams, "H", &["coold"]);
        let out = route_coold(
            &streams,
            DispatchEnvelope {
                host_id: "H".into(),
                request_id: "r1".into(),
                command: CommandPayload::ImagesList,
            },
        );

        match out {
            RouteOutcome::SendCoold { msg, .. } => {
                assert!(matches!(
                    msg.command,
                    Some(server_msg::Command::ImagesList(_))
                ));
            }
            other => panic!("expected SendCoold, got {other:?}"),
        }
    }

    fn route_command(command: serde_json::Value) -> server_msg::Command {
        let streams = Streams::new();
        let _rx = insert_host(&streams, "H", &["coold"]);
        let env = serde_json::from_value::<DispatchEnvelope>(serde_json::json!({
            "host_id": "H",
            "request_id": "r1",
            "command": command,
        }))
        .expect("valid dispatch envelope");

        match route_coold(&streams, env) {
            RouteOutcome::SendCoold {
                msg:
                    ServerMsg {
                        command: Some(command),
                        ..
                    },
                ..
            } => command,
            other => panic!("expected routed command, got {other:?}"),
        }
    }

    #[test]
    fn routes_all_image_primitives_from_dotted_json_names() {
        assert!(matches!(
            route_command(serde_json::json!({
                "type": "images.pull",
                "reference": "docker.io/library/nginx:alpine"
            })),
            server_msg::Command::ImagesPull(ImagesPullReq { reference }) if reference == "docker.io/library/nginx:alpine"
        ));

        assert!(matches!(
            route_command(serde_json::json!({ "type": "images.list" })),
            server_msg::Command::ImagesList(_)
        ));

        assert!(matches!(
            route_command(serde_json::json!({
                "type": "images.delete",
                "reference": "docker.io/library/nginx:alpine",
                "force": true
            })),
            server_msg::Command::ImagesDelete(ImagesDeleteReq { reference, force })
                if reference == "docker.io/library/nginx:alpine" && force
        ));
    }

    #[test]
    fn routes_all_container_primitives_from_dotted_json_names() {
        assert!(matches!(
            route_command(serde_json::json!({
                "type": "containers.create",
                "name": "web",
                "image": "docker.io/library/nginx:alpine",
                "command": ["nginx", "-g", "daemon off;"],
                "env": ["APP_ENV=production"],
                "networks": ["coolify-default-mesh"],
                "volumes": ["/data/web:/app"],
                "ports": [{
                    "host_ip": "127.0.0.1",
                    "host_port": 8080,
                    "container_port": 80,
                    "protocol": "tcp"
                }],
                "dns": ["10.210.0.1"],
                "restart_policy": "unless-stopped"
            })),
            server_msg::Command::ContainersCreate(ContainersCreateReq { name, image, ports, .. })
                if name == "web"
                    && image == "docker.io/library/nginx:alpine"
                    && ports.len() == 1
                    && ports[0].container_port == 80
        ));

        assert!(matches!(
            route_command(serde_json::json!({ "type": "containers.start", "id": "abc" })),
            server_msg::Command::ContainersStart(ContainersStartReq { id }) if id == "abc"
        ));

        assert!(matches!(
            route_command(serde_json::json!({
                "type": "containers.stop",
                "id": "abc",
                "timeout_seconds": 3
            })),
            server_msg::Command::ContainersStop(ContainersStopReq { id, timeout_seconds })
                if id == "abc" && timeout_seconds == 3
        ));

        assert!(matches!(
            route_command(serde_json::json!({
                "type": "containers.restart",
                "id": "abc",
                "timeout_seconds": 4
            })),
            server_msg::Command::ContainersRestart(ContainersRestartReq { id, timeout_seconds })
                if id == "abc" && timeout_seconds == 4
        ));

        assert!(matches!(
            route_command(serde_json::json!({
                "type": "containers.delete",
                "id": "abc",
                "force": true
            })),
            server_msg::Command::ContainersDelete(ContainersDeleteReq { id, force })
                if id == "abc" && force
        ));

        assert!(matches!(
            route_command(serde_json::json!({ "type": "containers.inspect", "id": "abc" })),
            server_msg::Command::ContainersInspect(ContainersInspectReq { id }) if id == "abc"
        ));

        assert!(matches!(
            route_command(serde_json::json!({ "type": "containers.list" })),
            server_msg::Command::ContainersList(_)
        ));

        assert!(matches!(
            route_command(serde_json::json!({
                "type": "containers.logs",
                "id": "abc",
                "tail": 50,
                "stdout": true,
                "stderr": true
            })),
            server_msg::Command::ContainersLogs(ContainersLogsReq { id, tail, stdout, stderr })
                if id == "abc" && tail == 50 && stdout && stderr
        ));

        assert!(matches!(
            route_command(serde_json::json!({
                "type": "containers.exec",
                "id": "abc",
                "command": ["echo", "ok"]
            })),
            server_msg::Command::ContainersExec(ContainersExecReq { id, command })
                if id == "abc" && command == ["echo", "ok"]
        ));

        assert!(matches!(
            route_command(serde_json::json!({
                "type": "containers.healthcheck.run",
                "id": "abc"
            })),
            server_msg::Command::ContainersHealthcheckRun(ContainersHealthcheckRunReq { id })
                if id == "abc"
        ));
    }
}
