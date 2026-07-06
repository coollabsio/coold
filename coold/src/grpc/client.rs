use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::transport::{Certificate, Channel, ClientTlsConfig};
use tonic::Request;
use tracing::{info, warn};

use crate::config::{Config, VERSION};
use crate::corrosion::CorrosionClient;
use crate::grpc::handlers::{handle, EndpointOwnerCheck};
use crate::grpc::proto::{
    agent_client::AgentClient, client_msg, server_msg, ClientMsg, Hello, Pong, ResourceStatusUpdate,
};
use crate::podman::PodmanClient;
use crate::sync::ResyncSignal;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AssignmentRequest {
    pub host_id: String,
    pub coold_version: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AssignmentResponse {
    flux_url: String,
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

pub async fn run(
    config: Config,
    podman: PodmanClient,
    resource_status_tx: broadcast::Sender<ResourceStatusUpdate>,
    resync_signal: ResyncSignal,
) -> Result<()> {
    if config.grpc_disabled || (config.flux_url.is_none() && config.assignment_url.is_none()) {
        info!("grpc transport disabled; skipping");
        std::future::pending::<()>().await;
        return Ok(());
    }

    // Verify the JWT file exists and has correct perms before connecting.
    load_host_jwt(&config.host_jwt_path)
        .await
        .context("initial host JWT load")?;

    let corrosion = CorrosionClient::new(&config.corrosion_url)?;
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
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
                backoff = next_backoff_after_delay(backoff);
                continue;
            }
        };

        let assignment_req = assignment_request(&config);
        let url = match resolve_flux_url(
            &http,
            config.assignment_url.as_deref(),
            config.flux_url.as_deref(),
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
                    "resolve flux URL failed"
                );
                tokio::time::sleep(backoff).await;
                backoff = next_backoff_after_delay(backoff);
                continue;
            }
        };

        let connected_at = Instant::now();
        match connect_and_serve(
            &url,
            &jwt,
            &config,
            &podman,
            &corrosion,
            resource_status_tx.subscribe(),
            &resync_signal,
        )
        .await
        {
            Ok(()) => {
                let (delay, next_backoff) =
                    backoff_after_clean_stream_close(connected_at.elapsed(), backoff);
                warn!(
                    connected_ms = connected_at.elapsed().as_millis(),
                    backoff_ms = delay.as_millis(),
                    "grpc stream closed cleanly; reconnecting"
                );
                tokio::time::sleep(delay).await;
                backoff = next_backoff;
            }
            Err(e) => {
                let delay = retry_delay_for_error(&e, backoff);
                warn!(
                    error = format!("{e:#}"),
                    backoff_ms = delay.as_millis(),
                    "grpc stream failed"
                );
                tokio::time::sleep(delay).await;
                backoff = next_backoff_after_delay(delay);
            }
        }
    }
}

fn assignment_request(config: &Config) -> AssignmentRequest {
    AssignmentRequest {
        host_id: config.host_mgmt_ip.clone(),
        coold_version: VERSION.to_string(),
        capabilities: primitive_capabilities(),
    }
}

const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(60);
const STABLE_STREAM_RESET_AFTER: Duration = Duration::from_secs(30);
const PERMANENT_REJECTION_RETRY_DELAY: Duration = Duration::from_secs(15 * 60);

fn next_backoff_after_delay(delay: Duration) -> Duration {
    (delay * 2).min(MAX_RECONNECT_BACKOFF)
}

fn backoff_after_clean_stream_close(
    connected_for: Duration,
    current_backoff: Duration,
) -> (Duration, Duration) {
    if connected_for >= STABLE_STREAM_RESET_AFTER {
        return (INITIAL_RECONNECT_BACKOFF, INITIAL_RECONNECT_BACKOFF);
    }

    (current_backoff, next_backoff_after_delay(current_backoff))
}

fn retry_delay_for_error(error: &anyhow::Error, current_backoff: Duration) -> Duration {
    let is_permanent_rejection = error.chain().any(|cause| {
        cause.downcast_ref::<tonic::Status>().is_some_and(|status| {
            matches!(
                status.code(),
                tonic::Code::Unauthenticated
                    | tonic::Code::PermissionDenied
                    | tonic::Code::InvalidArgument
                    | tonic::Code::FailedPrecondition
            )
        })
    });

    if is_permanent_rejection {
        PERMANENT_REJECTION_RETRY_DELAY
    } else {
        current_backoff
    }
}

fn primitive_capabilities() -> Vec<String> {
    [
        "images.pull",
        "images.list",
        "images.delete",
        "containers.create",
        "containers.start",
        "containers.stop",
        "containers.restart",
        "containers.delete",
        "containers.inspect",
        "containers.list",
        "containers.logs",
        "containers.exec",
        "containers.healthcheck.run",
        "ingress.apply",
        "ingress.stop",
        "firewall.allow",
        "firewall.revoke",
        "firewall.list",
        "firewall.reconcile",
        "coold.logs",
        "corrosion.tables",
        "host.jwt.set",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

async fn resolve_flux_url(
    http: &reqwest::Client,
    assignment_url: Option<&str>,
    flux_url: Option<&str>,
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
            .with_context(|| format!("POST flux assignment {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("flux assignment {url} returned {status}: {body}");
        }

        let body: AssignmentResponse = resp
            .json()
            .await
            .with_context(|| format!("decode flux assignment response from {url}"))?;
        if body.flux_url.trim().is_empty() {
            anyhow::bail!("flux assignment {url} returned empty flux_url");
        }
        return Ok(body.flux_url);
    }

    flux_url.map(str::to_owned).ok_or_else(|| {
        anyhow!("COOLIFY_COOLD_ASSIGNMENT_URL or COOLIFY_COOLD_FLUX_URL must be set")
    })
}

#[allow(clippy::result_large_err)]
async fn connect_and_serve(
    url: &str,
    jwt: &str,
    config: &Config,
    podman: &PodmanClient,
    corrosion: &CorrosionClient,
    mut resource_status_rx: broadcast::Receiver<ResourceStatusUpdate>,
    resync_signal: &ResyncSignal,
) -> Result<()> {
    let mut endpoint = Channel::from_shared(url.to_string()).context("invalid flux URL")?;

    // S1: pin the flux TLS certificate when a pin file is present. Absent →
    // keep plaintext (WireGuard-protected) behavior. https:// without a pin
    // fails closed rather than trusting system roots.
    if let Some(tls) = flux_tls_config(url, &config.flux_tls_pin_path).await? {
        endpoint = endpoint.tls_config(tls).context("configure flux TLS pin")?;
    }

    let channel = endpoint.connect().await.context("connect to flux")?;

    let bearer: MetadataValue<_> = format!("Bearer {jwt}")
        .parse()
        .context("build bearer metadata")?;

    let mut client = AgentClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", bearer.clone());
        Ok(req)
    });

    let (tx, rx) = mpsc::channel::<ClientMsg>(64);

    let capabilities = primitive_capabilities();

    tx.send(ClientMsg {
        payload: Some(client_msg::Payload::Hello(Hello {
            host_mgmt_ip: config.host_mgmt_ip.clone(),
            coold_version: VERSION.to_string(),
            schema_min: 1,
            schema_max: 1,
            capabilities,
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

    info!(flux_url = url, "grpc stream established");

    // R1: a fresh stream means flux/Laravel just (re)attached. Force the next
    // reconcile to re-emit the full container-status snapshot so any state
    // transitions dropped while there were no subscribers are replayed.
    resync_signal.request();

    let status_tx = tx.clone();
    let lag_resync = resync_signal.clone();
    let status_forwarder = tokio::spawn(async move {
        loop {
            match resource_status_rx.recv().await {
                Ok(update) => {
                    if status_tx
                        .send(ClientMsg {
                            payload: Some(client_msg::Payload::ResourceStatusUpdate(update)),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // R1: dropped deltas on lag would leave Laravel stale.
                    // Force a full resync instead of silently losing them.
                    warn!(
                        skipped,
                        "lagged while forwarding resource status updates to flux; forcing resync"
                    );
                    lag_resync.request();
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    while let Some(msg) = inbound.message().await.context("receive ServerMsg")? {
        let request_id = msg.request_id.clone();
        let Some(command) = msg.command else {
            warn!(%request_id, "ServerMsg has no command; ignoring");
            continue;
        };

        if matches!(command, server_msg::Command::Ping(_)) {
            tx.send(pong_for(&request_id)).await.context("send Pong")?;
            continue;
        }

        let tx = tx.clone();
        let podman = podman.clone();
        let corrosion = corrosion.clone();
        let host_jwt_path = config.host_jwt_path.clone();
        let owner_check =
            EndpointOwnerCheck::new(config.host_mgmt_ip.clone(), config.strict_endpoint_owner);
        tokio::spawn(async move {
            handle(
                request_id,
                command,
                &podman,
                &corrosion,
                &host_jwt_path,
                &owner_check,
                tx,
            )
            .await;
        });
    }

    status_forwarder.abort();

    Ok(())
}

/// Build the optional TLS config for the flux channel (S1).
///
/// - Pin file present → dial over TLS pinned to that PEM certificate/CA, with
///   the SNI/verification domain taken from the flux URL host.
/// - Pin file absent + `http(s)` URL is plaintext → `None` (today's behavior).
/// - Pin file absent + `https://` URL → error (fail closed; we refuse to fall
///   back to system roots for a connection that is meant to be pinned).
async fn flux_tls_config(url: &str, pin_path: &Path) -> Result<Option<ClientTlsConfig>> {
    let pem = match tokio::fs::read(pin_path).await {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow!("read flux TLS pin {}: {e}", pin_path.display())),
    };

    match require_flux_tls(url, pem.is_some())? {
        false => Ok(None),
        true => {
            let pem = pem.expect("require_flux_tls only returns true when a pin is present");
            let mut tls = ClientTlsConfig::new().ca_certificate(Certificate::from_pem(pem));
            if let Some(domain) = flux_tls_domain(url) {
                tls = tls.domain_name(domain);
            }
            Ok(Some(tls))
        }
    }
}

/// Decide whether TLS must be configured for `url` given whether a pin file is
/// present. Returns `Ok(true)` to pin, `Ok(false)` for plaintext, or an error
/// when the URL is `https://` but no pin is available (fail closed).
fn require_flux_tls(url: &str, pin_present: bool) -> Result<bool> {
    if pin_present {
        return Ok(true);
    }
    if url
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("https://")
    {
        return Err(anyhow!(
            "flux URL {url} is https but no TLS pin file is present; \
             refusing to connect without a pinned certificate"
        ));
    }
    Ok(false)
}

/// Extract the host portion of the flux URL for TLS SNI / verification.
fn flux_tls_domain(url: &str) -> Option<String> {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    // Strip userinfo and port.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    let host = host.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn pong_for(request_id: &str) -> ClientMsg {
    ClientMsg {
        payload: Some(client_msg::Payload::Pong(Pong {
            request_id: request_id.to_string(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::load_host_jwt;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

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

    #[test]
    fn builds_pong_for_ping_request_id() {
        let msg = super::pong_for("ping-1");

        match msg.payload {
            Some(super::client_msg::Payload::Pong(pong)) => {
                assert_eq!(pong.request_id, "ping-1");
            }
            other => panic!("expected pong payload, got {other:?}"),
        }
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
    async fn reconnect_reads_updated_token_from_disk() {
        // The reconnect loop calls load_host_jwt on every attempt (never caches
        // the first read), so a rotated token is picked up without a restart.
        let dir = tempfile::tempdir().unwrap();
        let p = write_jwt(dir.path(), "host-jwt", "first.token.value", 0o600);
        assert_eq!(load_host_jwt(&p).await.unwrap(), "first.token.value");

        // Simulate an external rotator swapping the file before expiry.
        write_jwt(dir.path(), "host-jwt", "second.token.value", 0o600);
        assert_eq!(load_host_jwt(&p).await.unwrap(), "second.token.value");
    }

    #[test]
    fn tls_required_when_pin_present() {
        assert!(super::require_flux_tls("http://flux:6443", true).unwrap());
        assert!(super::require_flux_tls("https://flux:6443", true).unwrap());
    }

    #[test]
    fn plaintext_when_no_pin_and_http_url() {
        assert!(!super::require_flux_tls("http://10.0.0.1:6443", false).unwrap());
    }

    #[test]
    fn https_without_pin_fails_closed() {
        let err = super::require_flux_tls("https://flux.example.com:6443", false).unwrap_err();
        assert!(format!("{err:#}").contains("no TLS pin"));
    }

    #[test]
    fn extracts_tls_domain_from_url() {
        assert_eq!(
            super::flux_tls_domain("https://flux.example.com:6443/path"),
            Some("flux.example.com".to_string())
        );
        assert_eq!(
            super::flux_tls_domain("http://10.0.0.1:6443"),
            Some("10.0.0.1".to_string())
        );
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

    #[test]
    fn quick_clean_stream_close_keeps_backing_off() {
        let current = Duration::from_secs(8);

        let (delay, next) =
            super::backoff_after_clean_stream_close(Duration::from_secs(2), current);

        assert_eq!(delay, current);
        assert_eq!(next, Duration::from_secs(16));
    }

    #[test]
    fn stable_clean_stream_close_resets_backoff() {
        let (delay, next) = super::backoff_after_clean_stream_close(
            Duration::from_secs(30),
            Duration::from_secs(8),
        );

        assert_eq!(delay, Duration::from_secs(1));
        assert_eq!(next, Duration::from_secs(1));
    }

    #[test]
    fn permanent_stream_rejections_use_long_retry_delay() {
        let status = tonic::Status::permission_denied("invalid capabilities");
        let err = anyhow::Error::new(status).context("open stream");

        assert_eq!(
            super::retry_delay_for_error(&err, Duration::from_secs(4)),
            Duration::from_secs(900),
        );
    }

    #[tokio::test]
    async fn flux_resolution_uses_static_url_without_assignment_url() {
        let req = super::AssignmentRequest {
            host_id: "100.64.0.5".into(),
            coold_version: "test".into(),
            capabilities: super::primitive_capabilities(),
        };

        let got = super::resolve_flux_url(
            &reqwest::Client::new(),
            None,
            Some("https://flux.example.com"),
            "jwt",
            &req,
        )
        .await
        .unwrap();

        assert_eq!(got, "https://flux.example.com");
    }

    #[tokio::test]
    async fn flux_resolution_requires_some_url() {
        let req = super::AssignmentRequest {
            host_id: "100.64.0.5".into(),
            coold_version: "test".into(),
            capabilities: super::primitive_capabilities(),
        };

        let err = super::resolve_flux_url(&reqwest::Client::new(), None, None, "jwt", &req)
            .await
            .unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.contains("COOLIFY_COOLD_ASSIGNMENT_URL or COOLIFY_COOLD_FLUX_URL"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn flux_resolution_posts_assignment_with_bearer_auth() {
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
                "flux_url": "https://assigned.example.com"
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
            capabilities: super::primitive_capabilities(),
        };

        let got = super::resolve_flux_url(
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
