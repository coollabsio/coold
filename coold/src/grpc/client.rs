use std::os::unix::fs::PermissionsExt;
use std::path::Path;
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

/// Read and validate the host JWT from disk. Refuses any file with
/// group/other permission bits set — the bearer must be 0600 or stricter.
/// Re-invoked on every reconnect attempt so an external rotator can swap
/// the file without restarting coold.
async fn load_host_jwt(path: &Path) -> Result<String> {
    let meta = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("stat host JWT {}", path.display()))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(anyhow!(
            "host JWT {} has insecure perms {:#o}; want 0600 (no group/other access)",
            path.display(),
            mode
        ));
    }

    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read host JWT from {}", path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("host JWT file {} is empty", path.display()));
    }
    Ok(trimmed.to_string())
}

pub async fn run(config: Config, podman: PodmanClient) -> Result<()> {
    if config.grpc_disabled || config.scheduler_url.is_none() {
        info!("grpc transport disabled; skipping");
        std::future::pending::<()>().await;
        return Ok(());
    }

    let url = config.scheduler_url.clone().unwrap();

    // Verify the JWT file exists and has correct perms before we set up the
    // builder context — fail-fast on misconfig rather than discovering it
    // halfway through reconnect backoff.
    load_host_jwt(&config.host_jwt_path)
        .await
        .context("initial host JWT load")?;

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
        // Re-read the JWT file each reconnect so an external rotator can swap
        // it (e.g. before exp) without coold needing a restart.
        let jwt = match load_host_jwt(&config.host_jwt_path).await {
            Ok(j) => j,
            Err(e) => {
                warn!(
                    error = format!("{e:#}"),
                    backoff_ms = backoff.as_millis(),
                    "load host JWT failed"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };

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

#[cfg(test)]
mod tests {
    use super::load_host_jwt;
    use std::os::unix::fs::PermissionsExt;

    fn write_jwt(dir: &std::path::Path, name: &str, contents: &str, mode: u32) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[tokio::test]
    async fn rejects_world_readable() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_jwt(dir.path(), "host-jwt", "abc.def.ghi", 0o644);
        let err = load_host_jwt(&p).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("insecure perms"), "got: {msg}");
    }

    #[tokio::test]
    async fn rejects_group_readable() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_jwt(dir.path(), "host-jwt", "abc.def.ghi", 0o640);
        let err = load_host_jwt(&p).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("insecure perms"), "got: {msg}");
    }

    #[tokio::test]
    async fn accepts_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_jwt(dir.path(), "host-jwt", "  abc.def.ghi\n", 0o600);
        let jwt = load_host_jwt(&p).await.unwrap();
        assert_eq!(jwt, "abc.def.ghi");
    }

    #[tokio::test]
    async fn rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_jwt(dir.path(), "host-jwt", "   \n", 0o600);
        let err = load_host_jwt(&p).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("empty"), "got: {msg}");
    }

    #[tokio::test]
    async fn rejects_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nope");
        let err = load_host_jwt(&p).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("stat host JWT"), "got: {msg}");
    }
}
