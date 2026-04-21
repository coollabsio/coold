use anyhow::{bail, Result};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tonic::metadata::MetadataValue;
use tracing::{info, warn};

use coolify_proto::builder::v1::{
    builder_client::BuilderClient, builder_client_msg, builder_server_msg, BuilderClientMsg,
    BuilderHello,
};

use crate::config::Config;

const RECONNECT_DELAY_SECS: u64 = 5;

pub async fn run(config: Config) -> Result<()> {
    loop {
        match connect_and_run(&config).await {
            Ok(()) => {
                info!("broker stream closed cleanly; reconnecting");
            }
            Err(e) => {
                warn!(error = %e, "broker stream error; reconnecting in {RECONNECT_DELAY_SECS}s");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(RECONNECT_DELAY_SECS)).await;
    }
}

async fn connect_and_run(config: &Config) -> Result<()> {
    info!(broker_url = %config.broker_url, "dialing broker");
    let channel = tonic::transport::Channel::from_shared(config.broker_url.clone())?
        .connect()
        .await?;

    let bearer: MetadataValue<_> = format!("Bearer {}", config.jwt).parse()?;

    let mut client = BuilderClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
        req.metadata_mut().insert("authorization", bearer.clone());
        Ok(req)
    });

    // outbound channel: executor tasks → broker
    let (tx, rx) = mpsc::channel::<BuilderClientMsg>(64);

    // send Hello first
    let hello = BuilderClientMsg {
        payload: Some(builder_client_msg::Payload::Hello(BuilderHello {
            builder_id: config.builder_id.clone(),
            builder_version: crate::config::VERSION.to_owned(),
            capacity: config.capacity,
        })),
    };
    tx.send(hello).await?;

    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
    let mut inbound = client.stream(outbound).await?.into_inner();

    info!(builder_id = %config.builder_id, "connected to broker");

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(config.capacity as usize));

    while let Some(msg) = inbound.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => bail!("stream recv error: {e}"),
        };

        let request_id = msg.request_id.clone();

        match msg.command {
            Some(builder_server_msg::Command::Build(req)) => {
                let permit = semaphore.clone().acquire_owned().await?;
                let cfg = config.clone();
                let tx2 = tx.clone();
                tokio::spawn(async move {
                    crate::executor::handle(request_id, req, cfg, tx2).await;
                    drop(permit);
                });
            }
            Some(builder_server_msg::Command::Cancel(_)) => {
                warn!(%request_id, "CancelBuild not implemented in MVP; ignoring");
            }
            None => {
                warn!(%request_id, "received BuilderServerMsg with no command");
            }
        }
    }

    Ok(())
}
