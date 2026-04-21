use anyhow::Result;

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
    Ok(ListContainersResp {
        containers: containers
            .into_iter()
            .map(|c| {
                let name = c
                    .names
                    .into_iter()
                    .next()
                    .unwrap_or_default()
                    .trim_start_matches('/')
                    .to_string();
                let networks = c
                    .network_settings
                    .map(|ns| ns.networks.into_keys().collect())
                    .unwrap_or_default();
                ContainerSummary {
                    id: c.id,
                    name,
                    image: c.image,
                    state: c.state,
                    networks,
                }
            })
            .collect(),
    })
}
