use std::sync::Arc;

use anyhow::Result;
use futures_util::future::join_all;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::builder::BuilderCtx;
use crate::grpc::proto::{
    client_msg, response, server_msg, BuildResponseBody, ClientMsg, ContainerSummary, Error,
    ListContainersResp, Response,
};
use crate::podman::PodmanClient;

pub async fn handle(
    request_id: String,
    command: server_msg::Command,
    podman: &PodmanClient,
    builder_ctx: Option<Arc<BuilderCtx>>,
    tx: mpsc::Sender<ClientMsg>,
) {
    match command {
        server_msg::Command::ListContainers(_) => {
            let body = match list_containers(podman).await {
                Ok(resp) => response::Body::ListContainers(resp),
                Err(e) => response::Body::Error(Error {
                    code: 500,
                    message: format!("{e:#}"),
                }),
            };
            send_response(&tx, Response { request_id, body: Some(body) }).await;
        }
        server_msg::Command::Build(req) => match builder_ctx {
            Some(ctx) => ctx.dispatch(request_id, req, tx),
            None => {
                let body = response::Body::Build(BuildResponseBody {
                    body: Some(crate::grpc::proto::build_response_body::Body::Err(
                        crate::grpc::proto::BuildError {
                            code: 501,
                            message: "builder capability not enabled on this host".into(),
                            stage: "dispatch".into(),
                        },
                    )),
                });
                send_response(&tx, Response { request_id, body: Some(body) }).await;
            }
        },
        server_msg::Command::CancelBuild(_) => match builder_ctx {
            Some(ctx) => {
                if !ctx.cancel(&request_id).await {
                    warn!(%request_id, "cancel for unknown or already-finished request_id");
                }
            }
            None => warn!(%request_id, "received CancelBuild but builder capability disabled"),
        },
    }
}

async fn send_response(tx: &mpsc::Sender<ClientMsg>, response: Response) {
    let request_id = response.request_id.clone();
    let msg = ClientMsg {
        payload: Some(client_msg::Payload::Response(response)),
    };
    if let Err(e) = tx.send(msg).await {
        warn!(%request_id, error = %e, "failed to enqueue response");
    }
}

async fn list_containers(podman: &PodmanClient) -> Result<ListContainersResp> {
    let containers = podman.list_containers().await?;

    // libpod list endpoint returns empty NetworkSettings.Networks; must inspect
    // each container to get actual network attachments. Run inspects concurrently.
    let inspects = join_all(containers.iter().map(|c| podman.inspect_container(&c.id))).await;

    let summaries = containers
        .into_iter()
        .zip(inspects)
        .map(|(c, inspect_result)| {
            let name = c
                .names
                .into_iter()
                .next()
                .unwrap_or_default()
                .trim_start_matches('/')
                .to_string();
            let networks = match inspect_result {
                Ok(inspect) => inspect
                    .network_settings
                    .map(|ns| ns.networks.into_keys().collect())
                    .unwrap_or_default(),
                Err(e) => {
                    debug!(container_id = %c.id, error = %e, "inspect failed; reporting empty networks");
                    vec![]
                }
            };
            ContainerSummary {
                id: c.id,
                name,
                image: c.image,
                state: c.state,
                networks,
            }
        })
        .collect();

    Ok(ListContainersResp { containers: summaries })
}
