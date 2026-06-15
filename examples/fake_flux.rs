/// Minimal fake-flux gRPC server for local development/testing.
///
/// Starts an Agent service on 127.0.0.1:50051, waits for coold to connect,
/// reads its Hello frame, sends one ListContainersReq, prints the response,
/// then exits. Plain h2c (no TLS).
///
/// Usage:
///   Terminal A: cargo run --example fake_flux
///   Terminal B: COOLIFY_COOLD_FLUX_URL=http://127.0.0.1:50051 \
///               COOLIFY_COOLD_HOST_JWT_PATH=/tmp/jwt \
///               cargo run -- --host-mgmt-ip 127.0.0.1 ...
use std::pin::Pin;

use tokio::sync::mpsc;
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use tonic::{transport::Server, Request, Response, Status, Streaming};

pub mod proto {
    tonic::include_proto!("coolify.agent.v1");
}

use proto::{
    agent_server::{Agent, AgentServer},
    client_msg, server_msg, ClientMsg, ListContainersReq, ServerMsg,
};

struct FakeFlux;

type ResponseStream = Pin<Box<dyn Stream<Item = Result<ServerMsg, Status>> + Send>>;

#[tonic::async_trait]
impl Agent for FakeFlux {
    type StreamStream = ResponseStream;

    async fn stream(
        &self,
        request: Request<Streaming<ClientMsg>>,
    ) -> Result<Response<Self::StreamStream>, Status> {
        let mut inbound = request.into_inner();

        let (tx, rx) = mpsc::channel::<Result<ServerMsg, Status>>(16);

        tokio::spawn(async move {
            // Read Hello
            match inbound.next().await {
                Some(Ok(msg)) => {
                    if let Some(client_msg::Payload::Hello(hello)) = msg.payload {
                        println!(
                            "[fake_flux] Hello from host_mgmt_ip={} coold_version={} schema={}-{}",
                            hello.host_mgmt_ip,
                            hello.coold_version,
                            hello.schema_min,
                            hello.schema_max,
                        );
                    }
                }
                Some(Err(e)) => {
                    eprintln!("[fake_flux] stream error: {e}");
                    return;
                }
                None => {
                    eprintln!("[fake_flux] stream closed before Hello");
                    return;
                }
            }

            // Send ListContainersReq
            let req_id = "r1".to_string();
            let _ = tx
                .send(Ok(ServerMsg {
                    request_id: req_id.clone(),
                    command: Some(server_msg::Command::ListContainers(ListContainersReq {})),
                }))
                .await;

            // Await matching Response
            while let Some(item) = inbound.next().await {
                match item {
                    Ok(msg) => {
                        if let Some(client_msg::Payload::Response(resp)) = msg.payload {
                            if resp.request_id == req_id {
                                println!("[fake_flux] ListContainersResp:");
                                if let Some(proto::response::Body::ListContainers(lc)) = resp.body {
                                    for c in &lc.containers {
                                        println!(
                                            "  id={} name={} image={} state={} networks={:?}",
                                            c.id, c.name, c.image, c.state, c.networks
                                        );
                                    }
                                    println!(
                                        "[fake_flux] done — {} container(s) listed",
                                        lc.containers.len()
                                    );
                                } else if let Some(proto::response::Body::Error(e)) = resp.body {
                                    eprintln!("[fake_flux] error from coold: {} {}", e.code, e.message);
                                }
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[fake_flux] stream error: {e}");
                        break;
                    }
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "127.0.0.1:50051".parse()?;
    println!("[fake_flux] listening on {addr} (plain h2c)");
    Server::builder()
        .add_service(AgentServer::new(FakeFlux))
        .serve(addr)
        .await?;
    Ok(())
}
