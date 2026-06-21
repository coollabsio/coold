//! Minimal fake coold: dials flux, sends Hello, answers ContainersList with a stub.
//!
//! Usage:
//!   COOLIFY_COOLD_FLUX_URL=http://127.0.0.1:6443 JWT=<token> cargo run -p flux --example fake_coold

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::Request;

use coolify_proto::agent::v1::{
    agent_client::AgentClient, client_msg, response, server_msg, ClientMsg, ContainerSummary,
    ContainersListResp, Error, Hello, Response,
};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let url =
        std::env::var("COOLIFY_COOLD_FLUX_URL").unwrap_or_else(|_| "http://127.0.0.1:6443".into());
    let jwt = std::env::var("JWT").context("JWT env var required")?;

    let channel = Channel::from_shared(url.clone())?
        .connect()
        .await
        .with_context(|| format!("connect to {url}"))?;

    let bearer: MetadataValue<_> = format!("Bearer {jwt}").parse()?;
    let mut client = AgentClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", bearer.clone());
        Ok(req)
    });

    let (tx, rx) = mpsc::channel::<ClientMsg>(64);

    tx.send(ClientMsg {
        payload: Some(client_msg::Payload::Hello(Hello {
            host_mgmt_ip: "127.0.0.1".into(),
            coold_version: "fake-0.1".into(),
            schema_min: 1,
            schema_max: 1,
            capabilities: vec![
                "coold".into(),
                "ingress.apply".into(),
                "ingress.stop".into(),
            ],
        })),
    })
    .await?;

    let outbound = ReceiverStream::new(rx);
    let mut inbound = client.stream(outbound).await?.into_inner();

    eprintln!("fake_coold connected to {url}");

    while let Some(msg) = inbound.message().await? {
        let request_id = msg.request_id.clone();
        let Some(command) = msg.command else { continue };
        match command {
            server_msg::Command::ContainersList(_) => {
                let resp = Response {
                    request_id,
                    body: Some(response::Body::ContainersList(ContainersListResp {
                        containers: vec![ContainerSummary {
                            id: "deadbeef".into(),
                            name: "fake-web".into(),
                            image: "nginx:latest".into(),
                            state: "running".into(),
                            networks: vec!["coolify-default-mesh".into()],
                        }],
                    })),
                };
                tx.send(ClientMsg {
                    payload: Some(client_msg::Payload::Response(resp)),
                })
                .await?;
            }
            server_msg::Command::IngressApply(_) => {
                let resp = Response {
                    request_id,
                    body: Some(response::Body::IngressApply(
                        coolify_proto::agent::v1::ApplyIngressResp {
                            output: "Caddy ingress applied.".into(),
                        },
                    )),
                };
                tx.send(ClientMsg {
                    payload: Some(client_msg::Payload::Response(resp)),
                })
                .await?;
            }
            server_msg::Command::IngressStop(_) => {
                let resp = Response {
                    request_id,
                    body: Some(response::Body::IngressStop(
                        coolify_proto::agent::v1::StopIngressResp {
                            output: "Caddy ingress stopped.".into(),
                        },
                    )),
                };
                tx.send(ClientMsg {
                    payload: Some(client_msg::Payload::Response(resp)),
                })
                .await?;
            }
            _ => {
                let resp = Response {
                    request_id,
                    body: Some(response::Body::Error(Error {
                        code: 501,
                        message: "fake_coold does not implement this primitive".into(),
                    })),
                };
                tx.send(ClientMsg {
                    payload: Some(client_msg::Payload::Response(resp)),
                })
                .await?;
            }
        }
    }

    Ok(())
}
