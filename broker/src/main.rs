mod auth;
mod config;
mod envelope;
mod routing;
mod state;
mod unix_bridge;

use anyhow::Result;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load().await?;
    init_tracing(&config.log_level);

    info!(
        grpc_bind = %config.grpc_bind,
        uds_path = %config.unix_socket_path.display(),
        "broker starting",
    );

    let streams = state::Streams::new();
    let pending = state::Pending::new();

    tokio::try_join!(
        grpc_server::run(config.clone(), streams.clone(), pending.clone()),
        unix_bridge::run(config.clone(), streams.clone(), pending.clone()),
        pending_sweeper::run(pending.clone()),
    )?;

    Ok(())
}

fn init_tracing(level: &str) {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

mod grpc_server {
    use std::pin::Pin;

    use anyhow::Result;
    use tokio::sync::mpsc;
    use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
    use tonic::{transport::Server, Request, Response, Status, Streaming};
    use tracing::{info, warn};

    use coolify_proto::agent::v1::{
        agent_server::{Agent, AgentServer},
        client_msg, response, ClientMsg, ServerMsg,
    };

    use crate::{
        auth,
        config::Config,
        envelope::{BuildResponseBody, ResponseBody},
        state::{Pending, PendingKind, ResponseData, StreamHandle, Streams},
    };

    pub async fn run(config: Config, streams: Streams, pending: Pending) -> Result<()> {
        let addr = config.grpc_bind.parse()?;
        let svc = BrokerAgent { config, streams, pending };

        info!(%addr, "gRPC server listening");
        Server::builder()
            .add_service(AgentServer::new(svc))
            .serve(addr)
            .await?;
        Ok(())
    }

    struct BrokerAgent {
        config: Config,
        streams: Streams,
        pending: Pending,
    }

    type ServerMsgStream = Pin<Box<dyn Stream<Item = Result<ServerMsg, Status>> + Send + 'static>>;

    #[tonic::async_trait]
    impl Agent for BrokerAgent {
        type StreamStream = ServerMsgStream;

        async fn stream(
            &self,
            request: Request<Streaming<ClientMsg>>,
        ) -> Result<Response<Self::StreamStream>, Status> {
            let jwt = request
                .metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .ok_or_else(|| Status::unauthenticated("missing Bearer token"))?;

            let verified = auth::verify_jwt(jwt, &self.config.jwt_public_key)
                .map_err(|e| Status::unauthenticated(format!("invalid JWT: {e}")))?;

            let host_id = verified.host_id.clone();
            let jwt_caps = verified.caps;

            info!(%host_id, caps = ?jwt_caps, "coold stream connected");

            let (cmd_tx, cmd_rx) = mpsc::channel::<ServerMsg>(64);
            self.streams.insert(
                host_id.clone(),
                StreamHandle {
                    tx: cmd_tx,
                    caps: jwt_caps.clone(),
                    builder_capacity: 0,
                },
            );

            let streams = self.streams.clone();
            let pending = self.pending.clone();
            let host_id_clone = host_id.clone();
            let jwt_caps_clone = jwt_caps.clone();
            let mut inbound = request.into_inner();

            tokio::spawn(async move {
                while let Some(msg) = inbound.next().await {
                    match msg {
                        Ok(ClientMsg { payload: Some(client_msg::Payload::Response(resp)) }) => {
                            deliver_response(&pending, resp);
                        }
                        Ok(ClientMsg { payload: Some(client_msg::Payload::Hello(h)) }) => {
                            info!(
                                host_id = %host_id_clone,
                                version = %h.coold_version,
                                capabilities = ?h.capabilities,
                                builder_capacity = h.builder_capacity,
                                "Hello received"
                            );

                            // Defense in depth: the host may only advertise a
                            // capability already granted in its JWT.
                            if let Some(missing) = h
                                .capabilities
                                .iter()
                                .find(|c| !jwt_caps_clone.iter().any(|jc| jc == *c))
                            {
                                warn!(
                                    host_id = %host_id_clone,
                                    missing_cap = %missing,
                                    jwt_caps = ?jwt_caps_clone,
                                    "host advertised a capability not granted in JWT; dropping stream",
                                );
                                break;
                            }

                            streams.update_capabilities(
                                &host_id_clone,
                                h.capabilities,
                                h.builder_capacity,
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            warn!(host_id = %host_id_clone, error = %e, "stream recv error");
                            break;
                        }
                    }
                }
                info!(host_id = %host_id_clone, "coold stream disconnected");
                streams.remove(&host_id_clone);
            });

            let outbound = ReceiverStream::new(cmd_rx).map(Ok);
            Ok(Response::new(Box::pin(outbound)))
        }
    }

    /// Map a coold gRPC `Response` to the right lane's `ResponseData` and
    /// hand it to the pending entry. If the dispatch was for a build, an
    /// `Error` body is translated to `BuildResponseBody::Error`; otherwise
    /// to `ResponseBody::Error`. Unknown request_id → drop.
    fn deliver_response(pending: &Pending, resp: coolify_proto::agent::v1::Response) {
        let request_id = resp.request_id.clone();
        let kind = match pending.get(&request_id) {
            Some(e) => e.kind,
            None => {
                warn!(%request_id, "response for unknown request_id; dropping");
                return;
            }
        };

        let data = match (kind, resp.body) {
            (_, Some(response::Body::Build(b))) => {
                ResponseData::Build(BuildResponseBody::from_proto(b))
            }
            (PendingKind::Build, Some(response::Body::Error(e))) => {
                ResponseData::Build(BuildResponseBody::Error {
                    code: e.code,
                    message: e.message,
                    stage: String::new(),
                })
            }
            (PendingKind::Build, _) => {
                warn!(%request_id, "non-build Response for build request; dropping");
                return;
            }
            (PendingKind::Coold, body) => {
                let resp = coolify_proto::agent::v1::Response {
                    request_id: request_id.clone(),
                    body,
                };
                match ResponseBody::try_from_proto(resp) {
                    Some(rb) => ResponseData::Coold(rb),
                    None => {
                        warn!(%request_id, "build body on coold request; dropping");
                        return;
                    }
                }
            }
        };

        pending.deliver(&request_id, data);
    }
}

mod pending_sweeper {
    use anyhow::Result;
    use tracing::warn;

    use crate::state::{Pending, PendingKind, DISPATCH_TIMEOUT_SECS};

    pub async fn run(pending: Pending) -> Result<()> {
        let interval = std::time::Duration::from_secs(1);
        loop {
            tokio::time::sleep(interval).await;
            let expired = pending.drain_expired();
            for (request_id, entry) in expired {
                if matches!(entry.kind, PendingKind::Coold) {
                    warn!(
                        %request_id,
                        timeout_secs = DISPATCH_TIMEOUT_SECS,
                        "coold dispatch timed out; handler will return 504"
                    );
                }
            }
        }
    }
}
