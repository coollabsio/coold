mod auth;
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
        grpc_bind = %config.grpc_bind,
        redis_url = %config.redis_url,
        "coolify-broker starting",
    );

    let streams = state::Streams::new();

    tokio::try_join!(
        grpc_server::run(config.clone(), streams.clone()),
        redis_bridge::run(config.clone(), streams.clone()),
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

    use crate::{auth, config::Config, state::Streams};

    pub async fn run(config: Config, streams: Streams) -> Result<()> {
        let addr = config.grpc_bind.parse()?;
        let svc = BrokerAgent { config, streams };

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
    }

    type ServerMsgStream = Pin<Box<dyn Stream<Item = Result<ServerMsg, Status>> + Send + 'static>>;

    #[tonic::async_trait]
    impl Agent for BrokerAgent {
        type StreamStream = ServerMsgStream;

        async fn stream(
            &self,
            request: Request<Streaming<ClientMsg>>,
        ) -> Result<Response<Self::StreamStream>, Status> {
            // Validate JWT from Authorization metadata.
            let jwt = request
                .metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .ok_or_else(|| Status::unauthenticated("missing Bearer token"))?;

            let host_id = auth::verify_jwt(jwt, &self.config.jwt_public_key)
                .map_err(|e| Status::unauthenticated(format!("invalid JWT: {e}")))?;

            info!(%host_id, "coold stream connected");

            // Channel through which the redis_bridge sends commands to this stream.
            let (cmd_tx, cmd_rx) = mpsc::channel::<ServerMsg>(64);
            self.streams.insert(host_id.clone(), cmd_tx);

            let streams = self.streams.clone();
            let mut inbound = request.into_inner();

            // Spawn task: read responses from coold, push to Redis.
            let redis_url = self.config.redis_url.clone();
            let host_id_clone = host_id.clone();
            tokio::spawn(async move {
                while let Some(msg) = inbound.next().await {
                    match msg {
                        Ok(ClientMsg { payload: Some(client_msg::Payload::Response(resp)) }) => {
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

            // Outbound: ServerMsg frames from cmd_rx.
            let outbound = ReceiverStream::new(cmd_rx).map(Ok);
            Ok(Response::new(Box::pin(outbound)))
        }
    }
}
