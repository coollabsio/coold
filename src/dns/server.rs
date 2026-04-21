use std::{io, net::IpAddr, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use hickory_server::ServerFuture;
use tokio::net::{TcpListener, UdpSocket};
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::{
    config::Config,
    corrosion::CorrosionClient,
    dns::{
        forwarder,
        resolver::{CoolifyResolver, CorrosionBackend, EndpointLookup},
    },
};

/// Timeout for a single in-flight TCP DNS query.
const TCP_TIMEOUT: Duration = Duration::from_secs(5);

/// Initial backoff when bind/serve fails with a retryable IO error.
const BACKOFF_START: Duration = Duration::from_secs(1);
/// Upper bound for exponential backoff.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Entry point for the DNS subsystem. Spawned by `sync::run`.
///
/// For every namespace with a non-zero bridge gateway IP, spawn a dedicated
/// bind/serve loop on `<gateway>:53`. A namespace with gateway `0.0.0.0`
/// (test / agent-only stub) is skipped. When every namespace is stubbed out,
/// this task returns `Ok(())` immediately.
///
/// Each per-namespace loop retries on transient IO errors (typically
/// `EADDRNOTAVAIL` when the Podman bridge has been torn down because no
/// containers are attached, or `EADDRINUSE` during netavark churn). Fatal
/// errors (zone parse, resolver build) propagate up so systemd restarts
/// the whole daemon.
pub async fn run(config: Config, corrosion: CorrosionClient) -> Result<()> {
    let gateways: Vec<(String, IpAddr)> = config
        .namespaces
        .iter()
        .filter(|n| !n.gateway_ip.is_unspecified())
        .map(|n| (n.name.clone(), n.gateway_ip))
        .collect();

    if gateways.is_empty() {
        info!("no namespace has a bridge gateway IP; DNS server disabled");
        return Ok(());
    }

    let backend: Arc<dyn EndpointLookup> = Arc::new(CorrosionBackend::new(corrosion));
    let upstream = forwarder::build(config.dns_upstream);
    let zone = config.dns_zone.clone();

    let mut handles = Vec::with_capacity(gateways.len());
    for (ns, gateway) in gateways {
        let backend = backend.clone();
        let upstream = upstream.clone();
        let zone = zone.clone();
        handles.push(tokio::spawn(async move {
            run_for_gateway(ns, gateway, zone, backend, upstream).await
        }));
    }

    // First failure takes the whole task down so systemd restarts coold.
    for h in handles {
        match h.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(anyhow::anyhow!("dns bind task panicked: {e}")),
        }
    }
    Ok(())
}

/// Per-namespace bind/serve loop. Retries transient IO errors forever;
/// returns only on fatal errors.
async fn run_for_gateway(
    namespace: String,
    gateway: IpAddr,
    zone: String,
    backend: Arc<dyn EndpointLookup>,
    upstream: hickory_resolver::TokioAsyncResolver,
) -> Result<()> {
    let addr = SocketAddr::new(gateway, 53);
    let mut backoff = BACKOFF_START;
    let mut attempt: u32 = 0;

    loop {
        // Resolver is cheap to build; rebuilding on every retry keeps the
        // fatal-vs-retryable boundary crisp — a zone-parse failure errors
        // out here, not inside try_serve, so it cannot be misclassified.
        let handler = CoolifyResolver::new(backend.clone(), &zone, upstream.clone())
            .context("build CoolifyResolver")?;

        match try_serve(addr, handler).await {
            Ok(()) => return Ok(()),
            Err(e) if is_retryable_io(&e) => {
                attempt = attempt.saturating_add(1);
                log_bind_failure(&namespace, addr, &e, attempt, backoff);
                sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
            Err(e) => {
                return Err(e.context(format!(
                    "dns bind/serve on {addr} (namespace {namespace})"
                )))
            }
        }
    }
}

/// One attempt: bind UDP+TCP, register with hickory, serve until the server
/// future returns. Returns `Ok(())` only on a clean hickory exit. Any bind or
/// serve error is returned so the caller can classify it as retryable or fatal.
async fn try_serve(addr: SocketAddr, handler: CoolifyResolver) -> Result<()> {
    let udp = UdpSocket::bind(addr)
        .await
        .with_context(|| format!("bind udp {addr}"))?;
    let tcp = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind tcp {addr}"))?;

    let mut server = ServerFuture::new(handler);
    server.register_socket(udp);
    server.register_listener(tcp, TCP_TIMEOUT);

    info!(bind = %addr, "coold DNS listening");
    server.block_until_done().await.context("dns server loop")?;
    Ok(())
}

/// True for IO errors that indicate a transient network-config state —
/// typically the Podman bridge being absent because no containers are
/// currently attached to the mesh network. These resolve on their own once
/// the first container attaches (netavark recreates bridge + gateway IP).
fn is_retryable_io(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<io::Error>())
        .any(|io_err| {
            matches!(
                io_err.kind(),
                io::ErrorKind::AddrNotAvailable
                    | io::ErrorKind::AddrInUse
                    | io::ErrorKind::NetworkUnreachable
                    | io::ErrorKind::Other
            )
        })
}

/// Loud on first failure (named causes), quieter on subsequent retries to
/// avoid spamming the journal during prolonged idle windows.
fn log_bind_failure(
    namespace: &str,
    addr: SocketAddr,
    e: &anyhow::Error,
    attempt: u32,
    backoff: Duration,
) {
    if attempt == 1 {
        warn!(
            namespace = %namespace,
            bind = %addr,
            error = format!("{e:#}"),
            retry_in = ?backoff,
            "coold DNS bind failed; retrying. \
             Likely causes: \
             (a) Podman bridge torn down because no containers are attached to the mesh network \
             (netavark recreates it on first container start); \
             (b) aardvark-dns squatting :53 because the network was created without `--disable-dns` \
             — rerun `coolify init apply` to recreate it; \
             (c) a host DNS daemon bound to 0.0.0.0:53 — bind it to a specific interface instead",
        );
    } else {
        debug!(
            namespace = %namespace,
            bind = %addr,
            error = format!("{e:#}"),
            attempt,
            retry_in = ?backoff,
            "coold DNS bind still failing",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn retryable_classifier_addr_not_available() {
        let io_err = io::Error::from(io::ErrorKind::AddrNotAvailable);
        let wrapped: anyhow::Error = anyhow::Error::new(io_err).context("bind udp");
        assert!(is_retryable_io(&wrapped));
    }

    #[test]
    fn retryable_classifier_addr_in_use() {
        let io_err = io::Error::from(io::ErrorKind::AddrInUse);
        let wrapped: anyhow::Error = anyhow::Error::new(io_err).context("bind udp");
        assert!(is_retryable_io(&wrapped));
    }

    #[test]
    fn retryable_classifier_invalid_input_is_fatal() {
        let io_err = io::Error::from(io::ErrorKind::InvalidInput);
        let wrapped: anyhow::Error = anyhow::Error::new(io_err).context("bind udp");
        assert!(!is_retryable_io(&wrapped));
    }

    #[test]
    fn retryable_classifier_non_io_error_is_fatal() {
        let err = anyhow!("zone parse failed");
        assert!(!is_retryable_io(&err));
    }
}
