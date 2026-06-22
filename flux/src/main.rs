mod auth;
mod config;
mod envelope;
mod registry;
mod resource_status;
mod routing;
mod state;
mod unix_bridge;

use std::path::Path;

use anyhow::{Context, Result};
use tracing::info;
use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::EnvFilter;

use crate::config::{Config, FLUX_LOG_FILE_PATH};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load().await?;
    let log_file_path = Path::new(FLUX_LOG_FILE_PATH);
    let _log_guard = init_tracing(&config.log_level, log_file_path)?;

    info!(
        grpc_bind = %config.grpc_bind,
        uds_path = %config.unix_socket_path.display(),
        log_file_path = %log_file_path.display(),
        "flux starting",
    );

    let streams = state::Streams::new();
    let pending = state::Pending::new();

    tokio::try_join!(
        grpc_server::run(config.clone(), streams.clone(), pending.clone()),
        unix_bridge::run(config.clone(), streams.clone(), pending.clone()),
        pending_sweeper::run(config.clone(), pending.clone()),
        registry::heartbeat_loop(config.clone(), streams.clone()),
        coold_heartbeat::run(config.clone(), streams.clone()),
    )?;

    Ok(())
}

fn init_tracing(
    level: &str,
    log_file_path: &Path,
) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));

    let log_dir = log_file_path
        .parent()
        .context("flux log file path must include a parent directory")?;
    let log_file_name = log_file_path
        .file_name()
        .context("flux log file path must include a file name")?;

    std::fs::create_dir_all(log_dir)
        .with_context(|| format!("create flux log directory {}", log_dir.display()))?;

    let file_appender = tracing_appender::rolling::never(log_dir, log_file_name);
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let writer = std::io::stdout.and(file_writer);

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_target(false)
        .compact()
        .init();

    Ok(guard)
}

mod grpc_server {
    use std::net::SocketAddr;
    use std::pin::Pin;

    use anyhow::{Context, Result};
    use tokio::sync::mpsc;
    use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
    use tonic::{transport::Server, Request, Response, Status, Streaming};
    use tracing::{info, warn};

    use coolify_proto::agent::v1::{
        agent_server::{Agent, AgentServer},
        client_msg, ClientMsg, ServerMsg,
    };

    use crate::{
        auth,
        config::Config,
        envelope::ResponseBody,
        registry::RegistryClient,
        resource_status::ResourceStatusPublisher,
        state::{Pending, ResponseData, StreamHandle, Streams},
    };

    /// Reject `0.0.0.0` / `::` binds unless explicitly opted in. The gRPC
    /// stream carries a JWT bearer in cleartext (no TLS layer here yet), so
    /// listening on every interface lets anyone on path capture and replay
    /// the token until `exp`. Production must bind a specific interface IP
    /// (typically the WireGuard mgmt IP — `host_id` already equals it).
    pub(super) fn validate_bind(addr: SocketAddr, allow_public: bool) -> Result<()> {
        if addr.ip().is_unspecified() && !allow_public {
            anyhow::bail!(
                "refusing to bind {addr}: COOLIFY_FLUX_GRPC_BIND must be a specific \
                 interface IP (typically the WireGuard mgmt IP). Set \
                 COOLIFY_FLUX_ALLOW_PUBLIC_BIND=1 to override (dev only — JWTs \
                 cross the wire in cleartext)."
            );
        }
        Ok(())
    }

    fn allow_public_bind() -> bool {
        std::env::var("COOLIFY_FLUX_ALLOW_PUBLIC_BIND")
            .ok()
            .as_deref()
            == Some("1")
    }

    fn advertised_capability_not_granted<'a>(
        advertised: &'a [String],
        jwt_caps: &[String],
    ) -> Option<&'a str> {
        advertised
            .iter()
            .map(String::as_str)
            .find(|capability| !jwt_caps.iter().any(|jwt_cap| jwt_cap == capability))
    }

    fn validate_advertised_capabilities(
        advertised: &[String],
        jwt_caps: &[String],
    ) -> Result<(), Status> {
        if let Some(missing) = advertised_capability_not_granted(advertised, jwt_caps) {
            warn!(
                missing_cap = %missing,
                jwt_caps = ?jwt_caps,
                "host advertised a capability not granted in JWT; rejecting stream",
            );
            return Err(Status::permission_denied("invalid capabilities"));
        }

        Ok(())
    }

    pub async fn run(config: Config, streams: Streams, pending: Pending) -> Result<()> {
        let addr: SocketAddr = config
            .grpc_bind
            .parse()
            .with_context(|| format!("parse COOLIFY_FLUX_GRPC_BIND={}", config.grpc_bind))?;

        let allow_public = allow_public_bind();
        validate_bind(addr, allow_public)?;
        if addr.ip().is_unspecified() {
            warn!(
                %addr,
                "COOLIFY_FLUX_ALLOW_PUBLIC_BIND=1 — binding on every interface; \
                 JWTs cross the wire unencrypted",
            );
        }

        let registry = RegistryClient::from_config(&config);
        let resource_status = ResourceStatusPublisher::new(
            config.laravel_api_url.clone(),
            config.laravel_api_token.clone(),
        );
        let svc = FluxAgent {
            config,
            streams,
            pending,
            registry,
            resource_status,
        };

        info!(%addr, "gRPC server listening");
        Server::builder()
            .add_service(AgentServer::new(svc))
            .serve(addr)
            .await?;
        Ok(())
    }

    struct FluxAgent {
        config: Config,
        streams: Streams,
        pending: Pending,
        registry: Option<RegistryClient>,
        resource_status: ResourceStatusPublisher,
    }

    type ServerMsgStream = Pin<Box<dyn Stream<Item = Result<ServerMsg, Status>> + Send + 'static>>;

    #[tonic::async_trait]
    impl Agent for FluxAgent {
        type StreamStream = ServerMsgStream;

        async fn stream(
            &self,
            request: Request<Streaming<ClientMsg>>,
        ) -> Result<Response<Self::StreamStream>, Status> {
            // Generic message to client; full reason logged server-side.
            // Distinct strings per failure mode (expired vs bad-sig vs wrong-aud)
            // would give a credential-stuffing oracle.
            let jwt = request
                .metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .ok_or_else(|| {
                    warn!("gRPC stream rejected: missing or malformed Authorization");
                    Status::unauthenticated("invalid credentials")
                })?;

            let verified = auth::verify_jwt(jwt, &self.config.jwt_public_key).map_err(|e| {
                warn!(
                    error = format!("{e:#}"),
                    "gRPC stream rejected: JWT verification failed"
                );
                Status::unauthenticated("invalid credentials")
            })?;

            let host_id = verified.host_id.clone();
            let jwt_caps = verified.caps;

            let mut inbound = request.into_inner();
            let hello = match inbound.next().await {
                Some(Ok(ClientMsg {
                    payload: Some(client_msg::Payload::Hello(hello)),
                })) => hello,
                Some(Ok(_)) | None => {
                    warn!(%host_id, "gRPC stream rejected: first message was not Hello");
                    return Err(Status::invalid_argument("hello required"));
                }
                Some(Err(e)) => {
                    warn!(%host_id, error = %e, "gRPC stream rejected: failed to read Hello");
                    return Err(e);
                }
            };

            info!(
                %host_id,
                version = %hello.coold_version,
                capabilities = ?hello.capabilities,
                "coold stream connected"
            );
            validate_advertised_capabilities(&hello.capabilities, &jwt_caps)?;

            let (cmd_tx, cmd_rx) = mpsc::channel::<ServerMsg>(64);
            self.streams.insert(
                host_id.clone(),
                StreamHandle {
                    tx: cmd_tx,
                    caps: hello.capabilities.clone(),
                },
            );

            let streams = self.streams.clone();
            let pending = self.pending.clone();
            let registry = self.registry.clone();
            let resource_status = self.resource_status.clone();
            let host_id_clone = host_id.clone();
            {
                let publisher = resource_status.clone();
                let host_id = host_id.clone();
                tokio::spawn(async move {
                    publisher
                        .publish(coold_status_update(
                            &host_id,
                            "installed",
                            "coold stream connected.",
                        ))
                        .await;
                });
            }
            if let Some(registry) = registry.clone() {
                let host_id = host_id.clone();
                let capabilities = hello.capabilities.clone();
                let coold_version = hello.coold_version.clone();
                tokio::spawn(async move {
                    if let Err(e) = registry
                        .upsert_connection(&host_id, capabilities, Some(coold_version))
                        .await
                    {
                        warn!(
                            host_id = %host_id,
                            error = format!("{e:#}"),
                            "Laravel agent connection upsert failed",
                        );
                    }
                });
            }

            tokio::spawn(async move {
                let mut disconnect_reason = "stream_closed";
                while let Some(msg) = inbound.next().await {
                    match msg {
                        Ok(ClientMsg {
                            payload: Some(client_msg::Payload::Response(resp)),
                        }) => {
                            deliver_response(&pending, resp);
                        }
                        Ok(ClientMsg {
                            payload: Some(client_msg::Payload::ResourceStatusUpdate(update)),
                        }) => {
                            let publisher = resource_status.clone();
                            tokio::spawn(async move {
                                publisher.publish(update).await;
                            });
                        }
                        Ok(ClientMsg {
                            payload: Some(client_msg::Payload::Pong(_)),
                        }) => {
                            if streams.touch(&host_id_clone) {
                                let publisher = resource_status.clone();
                                let host_id = host_id_clone.clone();
                                tokio::spawn(async move {
                                    publisher
                                        .publish(coold_status_update(
                                            &host_id,
                                            "installed",
                                            "coold heartbeat restored.",
                                        ))
                                        .await;
                                });
                            }
                        }
                        Ok(ClientMsg {
                            payload: Some(client_msg::Payload::Hello(_)),
                        }) => warn!(host_id = %host_id_clone, "duplicate Hello ignored"),
                        Ok(_) => {}
                        Err(e) => {
                            warn!(host_id = %host_id_clone, error = %e, "stream recv error");
                            disconnect_reason = "stream_error";
                            break;
                        }
                    }
                }
                info!(host_id = %host_id_clone, "coold stream disconnected");
                streams.remove(&host_id_clone);
                resource_status
                    .publish(coold_status_update(
                        &host_id_clone,
                        "unreachable",
                        "coold stream disconnected.",
                    ))
                    .await;
                if let Some(registry) = registry {
                    if let Err(e) = registry.disconnect(&host_id_clone, disconnect_reason).await {
                        warn!(
                            host_id = %host_id_clone,
                            error = format!("{e:#}"),
                            "Laravel agent disconnect report failed",
                        );
                    }
                }
            });

            let outbound = ReceiverStream::new(cmd_rx).map(Ok);
            Ok(Response::new(Box::pin(outbound)))
        }
    }

    /// Map a coold gRPC `Response` to `ResponseData` and hand it to the
    /// pending entry. Unknown request_id → drop.
    fn deliver_response(pending: &Pending, resp: coolify_proto::agent::v1::Response) {
        let request_id = resp.request_id.clone();
        let kind = match pending.get(&request_id) {
            Some(e) => e.kind,
            None => {
                warn!(%request_id, "response for unknown request_id; dropping");
                return;
            }
        };

        let resp = coolify_proto::agent::v1::Response {
            request_id: request_id.clone(),
            body: resp.body,
        };
        let Some(body) = ResponseBody::try_from_proto(resp) else {
            warn!(%request_id, ?kind, "unsupported response body; dropping");
            return;
        };
        let data = ResponseData::Coold(body);

        pending.deliver(&request_id, data);
    }

    pub(super) fn coold_status_update(
        host_id: &str,
        status: &str,
        status_message: &str,
    ) -> coolify_proto::agent::v1::ResourceStatusUpdate {
        coolify_proto::agent::v1::ResourceStatusUpdate {
            resource_type: "server".into(),
            host_id: host_id.into(),
            container_id: String::new(),
            container_name: String::new(),
            status: status.into(),
            status_message: status_message.into(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{advertised_capability_not_granted, coold_status_update, validate_bind};
        use std::net::SocketAddr;

        fn parse(s: &str) -> SocketAddr {
            s.parse().unwrap()
        }

        #[test]
        fn rejects_ipv4_unspecified_without_override() {
            let err = validate_bind(parse("0.0.0.0:6443"), false).unwrap_err();
            assert!(err.to_string().contains("refusing to bind"), "got: {err}");
        }

        #[test]
        fn rejects_ipv6_unspecified_without_override() {
            let err = validate_bind(parse("[::]:6443"), false).unwrap_err();
            assert!(err.to_string().contains("refusing to bind"), "got: {err}");
        }

        #[test]
        fn accepts_ipv4_unspecified_with_override() {
            validate_bind(parse("0.0.0.0:6443"), true).unwrap();
        }

        #[test]
        fn accepts_ipv6_unspecified_with_override() {
            validate_bind(parse("[::]:6443"), true).unwrap();
        }

        #[test]
        fn accepts_specific_ipv4_without_override() {
            validate_bind(parse("10.42.0.1:6443"), false).unwrap();
        }

        #[test]
        fn accepts_loopback_without_override() {
            validate_bind(parse("127.0.0.1:6443"), false).unwrap();
        }

        #[test]
        fn invalid_hello_capabilities_are_rejected_before_stream_opens() {
            let advertised = vec!["containers.list".to_string(), "ingress.apply".to_string()];
            let jwt_caps = vec!["containers.list".to_string()];

            let err = super::validate_advertised_capabilities(&advertised, &jwt_caps).unwrap_err();

            assert_eq!(err.code(), tonic::Code::PermissionDenied);
            assert_eq!(err.message(), "invalid capabilities");
        }

        #[test]
        fn explicitly_granted_capabilities_are_allowed() {
            let advertised = vec!["containers.list".to_string(), "ingress.apply".to_string()];
            let jwt_caps = vec!["containers.list".to_string(), "ingress.apply".to_string()];

            assert_eq!(
                advertised_capability_not_granted(&advertised, &jwt_caps),
                None
            );
        }

        #[test]
        fn missing_capability_is_rejected() {
            let advertised = vec!["containers.list".to_string(), "ingress.apply".to_string()];
            let jwt_caps = vec!["containers.list".to_string()];

            assert_eq!(
                advertised_capability_not_granted(&advertised, &jwt_caps),
                Some("ingress.apply")
            );
        }

        #[test]
        fn builds_server_status_update_for_coold_liveness() {
            let update =
                coold_status_update("100.64.0.5", "unreachable", "coold heartbeat timed out.");

            assert_eq!(update.resource_type, "server");
            assert_eq!(update.host_id, "100.64.0.5");
            assert_eq!(update.status, "unreachable");
            assert_eq!(update.status_message, "coold heartbeat timed out.");
        }
    }
}

mod coold_heartbeat {
    use anyhow::Result;
    use coolify_proto::agent::v1::{server_msg, Ping, ServerMsg};
    use tracing::warn;
    use uuid::Uuid;

    use crate::{
        config::Config, grpc_server::coold_status_update, resource_status::ResourceStatusPublisher,
        state::Streams,
    };

    pub async fn run(config: Config, streams: Streams) -> Result<()> {
        let publisher = ResourceStatusPublisher::new(
            config.laravel_api_url.clone(),
            config.laravel_api_token.clone(),
        );
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
            config.coold_ping_interval_secs.max(1),
        ));
        let timeout = std::time::Duration::from_secs(config.coold_pong_timeout_secs.max(1));

        loop {
            ticker.tick().await;

            for host_id in streams.mark_stale(timeout) {
                publisher
                    .publish(coold_status_update(
                        &host_id,
                        "unreachable",
                        "coold heartbeat timed out.",
                    ))
                    .await;
            }

            for (host_id, tx) in streams.ping_targets() {
                let request_id = Uuid::new_v4().to_string();
                if tx
                    .send(ServerMsg {
                        request_id,
                        command: Some(server_msg::Command::Ping(Ping {})),
                    })
                    .await
                    .is_err()
                {
                    warn!(%host_id, "coold heartbeat ping send failed");
                }
            }
        }
    }
}

mod pending_sweeper {
    use anyhow::Result;
    use tracing::warn;

    use crate::{
        config::Config,
        state::{Pending, PendingKind},
    };

    pub async fn run(config: Config, pending: Pending) -> Result<()> {
        let interval = std::time::Duration::from_secs(1);
        let dispatch_timeout = std::time::Duration::from_secs(config.dispatch_timeout_secs);
        loop {
            tokio::time::sleep(interval).await;
            let expired = pending.drain_expired(dispatch_timeout);
            for (request_id, entry) in expired {
                if matches!(entry.kind, PendingKind::Coold) {
                    warn!(
                        %request_id,
                        timeout_secs = config.dispatch_timeout_secs,
                        "coold dispatch timed out; handler will return 504"
                    );
                }
            }
        }
    }
}
