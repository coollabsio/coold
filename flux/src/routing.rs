//! Pure routing: given an envelope + state, decide the next action.
//! No I/O, no stream writes — callers (the UDS bridge) map outcomes to
//! side effects. Separating this keeps routing logic unit-testable.

use coolify_proto::agent::v1::{
    server_msg, ApplyIngressReq, ContainersCreateReq, ContainersDeleteReq, ContainersExecReq,
    ContainersHealthcheckRunReq, ContainersInspectReq, ContainersListReq, ContainersLogsReq,
    ContainersRestartReq, ContainersStartReq, ContainersStopReq, CooldLogsReq, FirewallAllowReq,
    FirewallListReq, FirewallReconcileReq, FirewallRevokeReq, FirewallRule, ImagesDeleteReq,
    ImagesListReq, ImagesPullReq, IngressAppConfig as ProtoIngressAppConfig,
    PortMapping as ProtoPortMapping, ServerMsg, StopIngressReq,
};

use crate::envelope::{CommandPayload, DispatchEnvelope};
use crate::state::Streams;

/// What the caller should do next. `SendCoold` carries a fully-formed
/// `ServerMsg` ready to push down the host's mpsc tx. `PushError` maps
/// to an HTTP error response.
#[derive(Debug)]
pub enum RouteOutcome {
    SendCoold { host_id: String, msg: ServerMsg },
    PushError { code: u32, message: String },
}

/// Route a coold command envelope to its pinned target host.
pub fn route_coold(streams: &Streams, env: DispatchEnvelope) -> RouteOutcome {
    let Some(stream) = streams.get(&env.host_id) else {
        return RouteOutcome::PushError {
            code: 404,
            message: "host not connected".into(),
        };
    };

    let required_capability = required_capability(&env.command);
    if !stream.caps.iter().any(|cap| cap == required_capability) {
        if stream
            .advertised_caps
            .iter()
            .any(|cap| cap == required_capability)
        {
            return RouteOutcome::PushError {
                code: 403,
                message: format!(
                    "capability {required_capability} is not authorized for this host token"
                ),
            };
        }

        return RouteOutcome::PushError {
            code: 501,
            message: format!("primitive {required_capability} is not supported by host"),
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
            dns_search,
            network_aliases,
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
            dns_search,
            network_aliases,
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
        CommandPayload::ApplyIngress {
            kind,
            config,
            apps,
            mesh_network,
        } => server_msg::Command::IngressApply(ApplyIngressReq {
            kind,
            config,
            mesh_network,
            apps: apps
                .into_iter()
                .map(|app| ProtoIngressAppConfig {
                    name: app.name,
                    config: app.config,
                })
                .collect(),
        }),
        CommandPayload::StopIngress { kind } => {
            server_msg::Command::IngressStop(StopIngressReq { kind })
        }
        CommandPayload::FirewallAllow {
            id,
            namespace,
            src,
            dst,
            proto,
            port,
        } => server_msg::Command::FirewallAllow(FirewallAllowReq {
            rule: Some(FirewallRule {
                id,
                namespace,
                src,
                dst,
                proto,
                port,
            }),
        }),
        CommandPayload::FirewallRevoke { id } => {
            server_msg::Command::FirewallRevoke(FirewallRevokeReq { id })
        }
        CommandPayload::FirewallList { namespace } => {
            server_msg::Command::FirewallList(FirewallListReq { namespace })
        }
        CommandPayload::FirewallReconcile => {
            server_msg::Command::FirewallReconcile(FirewallReconcileReq {})
        }
        CommandPayload::CooldLogs { tail } => server_msg::Command::CooldLogs(CooldLogsReq { tail }),
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

fn required_capability(command: &CommandPayload) -> &'static str {
    match command {
        CommandPayload::ImagesPull { .. } => "images.pull",
        CommandPayload::ImagesList => "images.list",
        CommandPayload::ImagesDelete { .. } => "images.delete",
        CommandPayload::ContainersCreate { .. } => "containers.create",
        CommandPayload::ContainersStart { .. } => "containers.start",
        CommandPayload::ContainersStop { .. } => "containers.stop",
        CommandPayload::ContainersRestart { .. } => "containers.restart",
        CommandPayload::ContainersDelete { .. } => "containers.delete",
        CommandPayload::ContainersInspect { .. } => "containers.inspect",
        CommandPayload::ContainersList => "containers.list",
        CommandPayload::ContainersLogs { .. } => "containers.logs",
        CommandPayload::ContainersExec { .. } => "containers.exec",
        CommandPayload::ContainersHealthcheckRun { .. } => "containers.healthcheck.run",
        CommandPayload::ApplyIngress { .. } => "ingress.apply",
        CommandPayload::StopIngress { .. } => "ingress.stop",
        CommandPayload::FirewallAllow { .. } => "firewall.allow",
        CommandPayload::FirewallRevoke { .. } => "firewall.revoke",
        CommandPayload::FirewallList { .. } => "firewall.list",
        CommandPayload::FirewallReconcile => "firewall.reconcile",
        CommandPayload::CooldLogs { .. } => "coold.logs",
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
                advertised_caps: caps.iter().map(|c| c.to_string()).collect(),
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
    fn route_coold_rejects_host_missing_required_primitive_capability() {
        let streams = Streams::new();
        let _rx = insert_host(&streams, "H", &["images.list"]);

        let out = route_coold(
            &streams,
            DispatchEnvelope {
                host_id: "H".into(),
                request_id: "r1".into(),
                command: CommandPayload::ContainersList,
            },
        );

        match out {
            RouteOutcome::PushError { code, message } => {
                assert_eq!(code, 501);
                assert_eq!(
                    message,
                    "primitive containers.list is not supported by host"
                );
            }
            other => panic!("expected PushError, got {other:?}"),
        }
    }

    #[test]
    fn route_coold_reports_unauthorized_capability_when_host_advertised_it() {
        let streams = Streams::new();
        let (tx, _rx) = mpsc::channel::<ServerMsg>(16);
        streams.insert(
            "H".into(),
            StreamHandle {
                tx,
                caps: vec!["containers.list".into()],
                advertised_caps: vec!["containers.list".into(), "coold.logs".into()],
            },
        );

        let out = route_coold(
            &streams,
            DispatchEnvelope {
                host_id: "H".into(),
                request_id: "r1".into(),
                command: CommandPayload::CooldLogs { tail: 200 },
            },
        );

        match out {
            RouteOutcome::PushError { code, message } => {
                assert_eq!(code, 403);
                assert_eq!(
                    message,
                    "capability coold.logs is not authorized for this host token"
                );
            }
            other => panic!("expected PushError, got {other:?}"),
        }
    }

    #[test]
    fn route_coold_builds_containers_list_message() {
        let streams = Streams::new();
        let _rx = insert_host(&streams, "H", &["containers.list"]);
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
        let _rx = insert_host(&streams, "H", &["images.list"]);
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
        let _rx = insert_host(
            &streams,
            "H",
            &[
                "images.pull",
                "images.list",
                "images.delete",
                "containers.create",
                "containers.start",
                "containers.stop",
                "containers.restart",
                "containers.delete",
                "containers.inspect",
                "containers.list",
                "containers.logs",
                "containers.exec",
                "containers.healthcheck.run",
                "ingress.apply",
                "ingress.stop",
            ],
        );
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
                "dns_search": ["default.coolify.internal"],
                "network_aliases": ["coolify-v5-nginx-test"],
                "restart_policy": "unless-stopped"
            })),
            server_msg::Command::ContainersCreate(ContainersCreateReq { name, image, ports, dns_search, network_aliases, .. })
                if name == "web"
                    && dns_search == vec!["default.coolify.internal"]
                    && network_aliases == vec!["coolify-v5-nginx-test"]
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

    #[test]
    fn routes_ingress_primitives_from_dotted_json_names() {
        assert!(matches!(
            route_command(serde_json::json!({
                "type": "ingress.apply",
                "kind": "caddy",
                "config": "example.com { respond \"ok\" }",
                "mesh_network": "coolify-default-mesh",
                "apps": [{
                    "name": "web",
                    "config": "web.example.com { reverse_proxy web:3000 }"
                }]
            })),
            server_msg::Command::IngressApply(ApplyIngressReq { kind, config, apps, .. })
                if kind == "caddy"
                    && config.contains("example.com")
                    && apps.len() == 1
                    && apps[0].name == "web"
                    && apps[0].config.contains("reverse_proxy")
        ));

        assert!(matches!(
            route_command(serde_json::json!({
                "type": "ingress.stop",
                "kind": "caddy"
            })),
            server_msg::Command::IngressStop(StopIngressReq { kind }) if kind == "caddy"
        ));
    }

    #[test]
    fn every_primitive_requires_its_matching_capability() {
        let cases = [
            (
                "images.pull",
                serde_json::json!({
                    "type": "images.pull",
                    "reference": "docker.io/library/nginx:alpine"
                }),
            ),
            ("images.list", serde_json::json!({ "type": "images.list" })),
            (
                "images.delete",
                serde_json::json!({
                    "type": "images.delete",
                    "reference": "docker.io/library/nginx:alpine"
                }),
            ),
            (
                "containers.create",
                serde_json::json!({
                    "type": "containers.create",
                    "name": "web",
                    "image": "docker.io/library/nginx:alpine"
                }),
            ),
            (
                "containers.start",
                serde_json::json!({ "type": "containers.start", "id": "abc" }),
            ),
            (
                "containers.stop",
                serde_json::json!({ "type": "containers.stop", "id": "abc" }),
            ),
            (
                "containers.restart",
                serde_json::json!({ "type": "containers.restart", "id": "abc" }),
            ),
            (
                "containers.delete",
                serde_json::json!({ "type": "containers.delete", "id": "abc" }),
            ),
            (
                "containers.inspect",
                serde_json::json!({ "type": "containers.inspect", "id": "abc" }),
            ),
            (
                "containers.list",
                serde_json::json!({ "type": "containers.list" }),
            ),
            (
                "containers.logs",
                serde_json::json!({ "type": "containers.logs", "id": "abc" }),
            ),
            (
                "containers.exec",
                serde_json::json!({
                    "type": "containers.exec",
                    "id": "abc",
                    "command": ["echo", "ok"]
                }),
            ),
            (
                "containers.healthcheck.run",
                serde_json::json!({ "type": "containers.healthcheck.run", "id": "abc" }),
            ),
            ("coold.logs", serde_json::json!({ "type": "coold.logs" })),
            (
                "ingress.apply",
                serde_json::json!({
                    "type": "ingress.apply",
                    "kind": "caddy",
                    "config": "example.com { respond \"ok\" }"
                }),
            ),
            (
                "ingress.stop",
                serde_json::json!({ "type": "ingress.stop", "kind": "caddy" }),
            ),
            (
                "firewall.allow",
                serde_json::json!({
                    "type": "firewall.allow",
                    "id": "rule-api-postgres",
                    "namespace": "default",
                    "src": "coolify-v5-nginx-a",
                    "dst": "coolify-v5-nginx-b",
                    "proto": "tcp",
                    "port": 5432
                }),
            ),
            (
                "firewall.revoke",
                serde_json::json!({ "type": "firewall.revoke", "id": "abc123" }),
            ),
            (
                "firewall.list",
                serde_json::json!({ "type": "firewall.list", "namespace": "default" }),
            ),
            (
                "firewall.reconcile",
                serde_json::json!({ "type": "firewall.reconcile" }),
            ),
        ];

        for (capability, command) in cases {
            let streams = Streams::new();
            let _rx = insert_host(&streams, "H", &[capability]);
            let env = serde_json::from_value::<DispatchEnvelope>(serde_json::json!({
                "host_id": "H",
                "request_id": "r1",
                "command": command,
            }))
            .expect("valid dispatch envelope");

            assert!(
                matches!(route_coold(&streams, env), RouteOutcome::SendCoold { .. }),
                "expected {capability} to allow dispatch"
            );
        }
    }
}
