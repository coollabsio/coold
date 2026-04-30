use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::Request;
use tracing::{info, warn};

use crate::builder::{BuilderCtx, BuilderSettings};
use crate::config::{Config, VERSION};
use crate::grpc::handlers::handle;
use crate::grpc::proto::{
    agent_client::AgentClient, client_msg, ClientMsg, Hello,
};
use crate::podman::PodmanClient;

pub async fn run(config: Config, podman: PodmanClient) -> Result<()> {
    if config.grpc_disabled || config.scheduler_url.is_none() {
        info!("grpc transport disabled; skipping");
        std::future::pending::<()>().await;
        return Ok(());
    }

    let url = config.scheduler_url.clone().unwrap();

    let jwt = tokio::fs::read_to_string(&config.host_jwt_path)
        .await
        .with_context(|| format!("read host JWT from {}", config.host_jwt_path.display()))?;
    let jwt = jwt.trim().to_string();
    if jwt.is_empty() {
        return Err(anyhow!(
            "host JWT file {} is empty",
            config.host_jwt_path.display()
        ));
    }

    let builder_ctx = if config.builder_enabled {
        let ctx = Arc::new(BuilderCtx::new(BuilderSettings {
            work_root: config.builder_work_dir.clone(),
            builder_bin: config.builder_bin.clone(),
            capacity: config.builder_capacity,
            timeout_secs: config.builder_timeout_secs,
            memory_max: config.builder_memory_max.clone(),
            cpu_quota: config.builder_cpu_quota.clone(),
            deny_nets: config.builder_deny_nets.clone(),
        }));
        ctx.ensure_work_root()
            .await
            .with_context(|| format!("mkdir -p {}", config.builder_work_dir.display()))?;
        info!(
            work_dir = %config.builder_work_dir.display(),
            capacity = config.builder_capacity,
            "builder capability enabled",
        );
        Some(ctx)
    } else {
        None
    };

    let mut backoff = Duration::from_secs(1);
    loop {
        match connect_and_serve(&url, &jwt, &config, &podman, builder_ctx.clone()).await {
            Ok(()) => {
                warn!("grpc stream closed cleanly; reconnecting");
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                warn!(
                    error = format!("{e:#}"),
                    backoff_ms = backoff.as_millis(),
                    "grpc stream failed"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

async fn connect_and_serve(
    url: &str,
    jwt: &str,
    config: &Config,
    podman: &PodmanClient,
    builder_ctx: Option<Arc<BuilderCtx>>,
) -> Result<()> {
    let channel = Channel::from_shared(url.to_string())
        .context("invalid scheduler URL")?
        .connect()
        .await
        .context("connect to scheduler")?;

    let bearer: MetadataValue<_> = format!("Bearer {jwt}")
        .parse()
        .context("build bearer metadata")?;

    let mut client = AgentClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut()
            .insert("authorization", bearer.clone());
        Ok(req)
    });

    let (tx, rx) = mpsc::channel::<ClientMsg>(64);

    // Resume any in-flight builder units left by a prior coold invocation.
    // Must happen after the mpsc channel is live so adopted builds can enqueue
    // their final Response onto the stream (it will drain once the stream
    // binds below). Safe when builder_ctx is None — nothing to resume.
    if let Some(ctx) = builder_ctx.as_ref().cloned() {
        ctx.resume_or_reap(tx.clone()).await;
    }

    let mut capabilities = vec!["coold".to_string()];
    let mut builder_capacity = 0u32;
    if config.builder_enabled {
        capabilities.push("builder".to_string());
        builder_capacity = config.builder_capacity;
    }

    tx.send(ClientMsg {
        payload: Some(client_msg::Payload::Hello(Hello {
            host_mgmt_ip: config.host_mgmt_ip.clone(),
            coold_version: VERSION.to_string(),
            schema_min: 1,
            schema_max: 1,
            capabilities,
            builder_capacity,
        })),
    })
    .await
    .context("send Hello")?;

    let outbound = ReceiverStream::new(rx);
    let mut inbound = client
        .stream(outbound)
        .await
        .context("open stream")?
        .into_inner();

    info!(scheduler_url = url, "grpc stream established");

    while let Some(msg) = inbound.message().await.context("receive ServerMsg")? {
        let request_id = msg.request_id.clone();
        let Some(command) = msg.command else {
            warn!(%request_id, "ServerMsg has no command; ignoring");
            continue;
        };

        let tx = tx.clone();
        let podman = podman.clone();
        let builder_ctx = builder_ctx.clone();
        tokio::spawn(async move {
            handle(request_id, command, &podman, builder_ctx, tx).await;
        });
    }

    Ok(())
}
