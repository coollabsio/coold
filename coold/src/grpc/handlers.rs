use anyhow::Result;
use futures_util::future::join_all;
use tracing::debug;

use crate::grpc::proto::{
    response, server_msg, ContainerSummary, Error, ListContainersResp, Response,
};
use crate::podman::PodmanClient;

pub async fn handle(request_id: String, command: server_msg::Command, podman: &PodmanClient) -> Response {
    let body = match command {
        server_msg::Command::ListContainers(_) => match list_containers(podman).await {
            Ok(resp) => response::Body::ListContainers(resp),
            Err(e) => response::Body::Error(Error {
                code: 500,
                message: format!("{e:#}"),
            }),
        },
    };
    Response {
        request_id,
        body: Some(body),
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
