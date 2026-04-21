use std::pin::Pin;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use tonic::{transport::Server, Request, Response, Status, Streaming};
use tracing::{info, warn};

use coolify_proto::builder::v1::{
    build_response, builder_client_msg, builder_server::Builder, builder_server::BuilderServer,
    BuilderClientMsg, BuilderServerMsg,
};

use crate::{
    auth,
    build_redis_bridge::{self, BuildResponseBody},
    config::Config,
    state::{BuilderStreams, Pending},
};

pub async fn run(config: Config, builder_streams: BuilderStreams, pending: Pending) -> Result<()> {
    let addr = config.builder_grpc_bind.parse()?;
    let svc = BrokerBuilder { config, builder_streams, pending };

    info!(%addr, "builder gRPC server listening");
    Server::builder()
        .add_service(BuilderServer::new(svc))
        .serve(addr)
        .await?;
    Ok(())
}

struct BrokerBuilder {
    config: Config,
    builder_streams: BuilderStreams,
    pending: Pending,
}

type BuilderServerMsgStream =
    Pin<Box<dyn Stream<Item = Result<BuilderServerMsg, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl Builder for BrokerBuilder {
    type StreamStream = BuilderServerMsgStream;

    async fn stream(
        &self,
        request: Request<Streaming<BuilderClientMsg>>,
    ) -> Result<Response<BuilderServerMsgStream>, Status> {
        let jwt = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| Status::unauthenticated("missing Bearer token"))?;

        let builder_id = auth::verify_jwt(jwt, &self.config.jwt_public_key, "builder")
            .map_err(|e| Status::unauthenticated(format!("invalid JWT: {e}")))?;

        info!(%builder_id, "builder stream connected");

        let (cmd_tx, cmd_rx) = mpsc::channel::<BuilderServerMsg>(64);
        self.builder_streams.insert(builder_id.clone(), cmd_tx);

        let builder_streams = self.builder_streams.clone();
        let pending = self.pending.clone();
        let redis_url = self.config.redis_url.clone();
        let builder_id_clone = builder_id.clone();
        let mut inbound = request.into_inner();

        tokio::spawn(async move {
            while let Some(msg) = inbound.next().await {
                match msg {
                    Ok(BuilderClientMsg {
                        payload: Some(builder_client_msg::Payload::Response(resp)),
                    }) => {
                        let request_id = resp.request_id.clone();
                        pending.remove(&request_id);

                        let body = match resp.body {
                            Some(build_response::Body::Ok(r)) => BuildResponseBody::Ok {
                                digest: r.digest,
                                registry_ref: r.registry_ref,
                                duration_ms: r.duration_ms,
                            },
                            Some(build_response::Body::Err(e)) => BuildResponseBody::Error {
                                code: e.code,
                                message: e.message,
                                stage: e.stage,
                            },
                            None => BuildResponseBody::Error {
                                code: 500,
                                message: "empty build response body".into(),
                                stage: String::new(),
                            },
                        };

                        if let Err(e) =
                            build_redis_bridge::push_build_response(&redis_url, &request_id, body)
                                .await
                        {
                            warn!(%request_id, error = %e, "failed to push build response to Redis");
                        }
                    }
                    Ok(BuilderClientMsg {
                        payload: Some(builder_client_msg::Payload::Hello(h)),
                    }) => {
                        info!(
                            builder_id = %builder_id_clone,
                            version = %h.builder_version,
                            capacity = h.capacity,
                            "builder Hello"
                        );
                    }
                    Ok(BuilderClientMsg {
                        payload: Some(builder_client_msg::Payload::Progress(p)),
                    }) => {
                        tracing::debug!(
                            builder_id = %builder_id_clone,
                            stage = %p.stage,
                            percent = p.percent,
                            "{}", p.log_line
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(builder_id = %builder_id_clone, error = %e, "stream recv error");
                        break;
                    }
                }
            }
            info!(builder_id = %builder_id_clone, "builder stream disconnected");
            builder_streams.remove(&builder_id_clone);
        });

        let outbound = ReceiverStream::new(cmd_rx).map(Ok);
        Ok(Response::new(Box::pin(outbound)))
    }
}
