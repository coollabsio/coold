use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::Request;
use tracing::{info, warn};

use crate::builder::{BuilderCtx, BuilderSettings};
use crate::config::{Config, VERSION};
use crate::grpc::handlers::handle;
use crate::grpc::proto::{agent_client::AgentClient, client_msg, ClientMsg, Hello};
use crate::podman::PodmanClient;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AssignmentRequest {
    pub host_id: String,
    pub coold_version: String,
    pub capabilities: Vec<String>,
    pub builder_capacity: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct AssignmentResponse {
    scheduler_url: String,
}

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
    if config.grpc_disabled || (config.scheduler_url.is_none() && config.assignment_url.is_none()) {
        info!("grpc transport disabled; skipping");
        std::future::pending::<()>().await;
        return Ok(());
    }

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
    let http = reqwest::Client::new();
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

        let assignment_req = assignment_request(&config);
        let url = match resolve_scheduler_url(
            &http,
            config.assignment_url.as_deref(),
            config.scheduler_url.as_deref(),
            &jwt,
            &assignment_req,
        )
        .await
        {
            Ok(url) => url,
            Err(e) => {
                warn!(
                    error = format!("{e:#}"),
                    backoff_ms = backoff.as_millis(),
                    "resolve scheduler URL failed"
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

fn assignment_request(config: &Config) -> AssignmentRequest {
    let mut capabilities = vec!["coold".to_string()];
    let mut builder_capacity = 0u32;
    if config.builder_enabled {
        capabilities.push("builder".to_string());
        builder_capacity = config.builder_capacity;
    }
    AssignmentRequest {
        host_id: config.host_mgmt_ip.clone(),
        coold_version: VERSION.to_string(),
        capabilities,
        builder_capacity,
    }
}

async fn resolve_scheduler_url(
    http: &reqwest::Client,
    assignment_url: Option<&str>,
    scheduler_url: Option<&str>,
    jwt: &str,
    req: &AssignmentRequest,
) -> Result<String> {
    if let Some(url) = assignment_url {
        let resp = http
            .post(url)
            .bearer_auth(jwt)
            .json(req)
            .send()
            .await
            .with_context(|| format!("POST scheduler assignment {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("scheduler assignment {url} returned {status}: {body}");
        }

        let body: AssignmentResponse = resp
            .json()
            .await
            .with_context(|| format!("decode scheduler assignment response from {url}"))?;
        if body.scheduler_url.trim().is_empty() {
            anyhow::bail!("scheduler assignment {url} returned empty scheduler_url");
        }
        return Ok(body.scheduler_url);
    }

    scheduler_url
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("COOLD_ASSIGNMENT_URL or COOLD_SCHEDULER_URL must be set"))
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
        req.metadata_mut().insert("authorization", bearer.clone());
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

    fn write_jwt(
        dir: &std::path::Path,
        name: &str,
        contents: &str,
        mode: u32,
    ) -> std::path::PathBuf {
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

    #[tokio::test]
    async fn scheduler_resolution_uses_static_url_without_assignment_url() {
        let req = super::AssignmentRequest {
            host_id: "100.64.0.5".into(),
            coold_version: "test".into(),
            capabilities: vec!["coold".into()],
            builder_capacity: 0,
        };

        let got = super::resolve_scheduler_url(
            &reqwest::Client::new(),
            None,
            Some("https://scheduler.example.com"),
            "jwt",
            &req,
        )
        .await
        .unwrap();

        assert_eq!(got, "https://scheduler.example.com");
    }

    #[tokio::test]
    async fn scheduler_resolution_requires_some_url() {
        let req = super::AssignmentRequest {
            host_id: "100.64.0.5".into(),
            coold_version: "test".into(),
            capabilities: vec!["coold".into()],
            builder_capacity: 0,
        };

        let err = super::resolve_scheduler_url(&reqwest::Client::new(), None, None, "jwt", &req)
            .await
            .unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.contains("COOLD_ASSIGNMENT_URL or COOLD_SCHEDULER_URL"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn scheduler_resolution_posts_assignment_with_bearer_auth() {
        use axum::{
            extract::State,
            http::{HeaderMap, StatusCode},
            routing::post,
            Json, Router,
        };
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Seen(Arc<Mutex<Option<(String, String)>>>);

        async fn assign(
            State(seen): State<Seen>,
            headers: HeaderMap,
            Json(req): Json<super::AssignmentRequest>,
        ) -> Result<Json<serde_json::Value>, StatusCode> {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .ok_or(StatusCode::UNAUTHORIZED)?
                .to_owned();
            *seen.0.lock().unwrap() = Some((auth, req.host_id));
            Ok(Json(serde_json::json!({
                "scheduler_url": "https://assigned.example.com"
            })))
        }

        let seen = Seen::default();
        let app = Router::new()
            .route("/assign", post(assign))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let req = super::AssignmentRequest {
            host_id: "100.64.0.5".into(),
            coold_version: "test".into(),
            capabilities: vec!["coold".into(), "builder".into()],
            builder_capacity: 2,
        };

        let got = super::resolve_scheduler_url(
            &reqwest::Client::new(),
            Some(&format!("http://{addr}/assign")),
            Some("https://static.example.com"),
            "host.jwt",
            &req,
        )
        .await
        .unwrap();

        assert_eq!(got, "https://assigned.example.com");
        let seen = seen.0.lock().unwrap().clone().unwrap();
        assert_eq!(seen.0, "Bearer host.jwt");
        assert_eq!(seen.1, "100.64.0.5");
    }
}
