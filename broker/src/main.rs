mod auth;
mod build_redis_bridge;
mod builder_grpc_server;
mod config;
mod redis_bridge;
mod state;

use anyhow::Result;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load().await?;
    init_tracing(&config.log_level);

    info!(
        grpc_bind         = %config.grpc_bind,
        builder_grpc_bind = %config.builder_grpc_bind,
        redis_url         = %config.redis_url,
        "broker starting",
    );

    let streams = state::Streams::new();
    let builder_streams = state::BuilderStreams::new();
    let pending = state::Pending::new();

    tokio::try_join!(
        grpc_server::run(config.clone(), streams.clone(), pending.clone()),
        redis_bridge::run(config.clone(), streams.clone(), pending.clone()),
        builder_grpc_server::run(config.clone(), builder_streams.clone(), pending.clone()),
        build_redis_bridge::run(config.clone(), builder_streams.clone(), pending.clone()),
        pending_sweeper::run(config.clone(), pending.clone()),
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
        client_msg, ClientMsg, ServerMsg,
    };

    use crate::{auth, config::Config, state::{Pending, Streams}};

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

            let host_id = auth::verify_jwt(jwt, &self.config.jwt_public_key)
                .map_err(|e| Status::unauthenticated(format!("invalid JWT: {e}")))?;

            info!(%host_id, "coold stream connected");

            let (cmd_tx, cmd_rx) = mpsc::channel::<ServerMsg>(64);
            self.streams.insert(host_id.clone(), cmd_tx);

            let streams = self.streams.clone();
            let pending = self.pending.clone();
            let mut inbound = request.into_inner();

            let redis_url = self.config.redis_url.clone();
            let host_id_clone = host_id.clone();
            tokio::spawn(async move {
                while let Some(msg) = inbound.next().await {
                    match msg {
                        Ok(ClientMsg { payload: Some(client_msg::Payload::Response(resp)) }) => {
                            pending.remove(&resp.request_id);
                            if let Err(e) = crate::redis_bridge::push_response(&redis_url, resp).await {
                                warn!(host_id = %host_id_clone, error = %e, "failed to push response to Redis");
                            }
                        }
                        Ok(ClientMsg { payload: Some(client_msg::Payload::Hello(h)) }) => {
                            info!(host_id = %host_id_clone, version = %h.coold_version, "Hello received");
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
}

mod pending_sweeper {
    use anyhow::Result;
    use tracing::warn;

    use crate::{config::Config, state::{Pending, DISPATCH_TIMEOUT_SECS}};

    pub async fn run(config: Config, pending: Pending) -> Result<()> {
        let interval = std::time::Duration::from_secs(1);
        loop {
            tokio::time::sleep(interval).await;
            let expired = pending.drain_expired();
            for request_id in expired {
                warn!(%request_id, timeout_secs = DISPATCH_TIMEOUT_SECS, "dispatch timed out; pushing 504");
                // Push timeout to both coold and build resp keys; only one will exist.
                if let Err(e) = crate::redis_bridge::push_timeout_error(&config.redis_url, &request_id).await {
                    warn!(%request_id, error = %e, "failed to push coold timeout error");
                }
                if let Err(e) = crate::build_redis_bridge::push_build_timeout_error(&config.redis_url, &request_id).await {
                    warn!(%request_id, error = %e, "failed to push build timeout error");
                }
            }
        }
    }
}
