use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use hickory_server::ServerFuture;
use tokio::net::{TcpListener, UdpSocket};
use tracing::info;

use crate::{
    config::Config,
    corrosion::CorrosionClient,
    dns::{
        forwarder,
        preflight,
        resolver::{CoolifyResolver, CorrosionBackend, EndpointLookup},
    },
};

/// Timeout for a single in-flight TCP DNS query.
const TCP_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawned alongside events/reconcile tasks by `sync::run`. Returns `Ok(())`
/// only on clean shutdown (currently: task cancellation from `tokio::select!`).
/// If `config.bridge_gateway_ip` is unset we do nothing — lets the daemon run
/// in test/agent-only modes without touching :53.
pub async fn run(config: Config, corrosion: CorrosionClient) -> Result<()> {
    let Some(gateway) = config.bridge_gateway_ip else {
        info!("COOLD_BRIDGE_GATEWAY_IP unset; DNS server disabled");
        return Ok(());
    };

    preflight::check(gateway).await.context("dns preflight")?;

    let backend: Arc<dyn EndpointLookup> = Arc::new(CorrosionBackend::new(corrosion));
    let upstream = forwarder::build(config.dns_upstream);
    let handler = CoolifyResolver::new(backend, &config.dns_zone, upstream)
        .context("build CoolifyResolver")?;

    let addr = SocketAddr::new(gateway, 53);
    let udp = UdpSocket::bind(addr)
        .await
        .with_context(|| format!("bind udp {addr}"))?;
    let tcp = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind tcp {addr}"))?;

    let mut server = ServerFuture::new(handler);
    server.register_socket(udp);
    server.register_listener(tcp, TCP_TIMEOUT);

    info!(
        bind = %addr,
        zone = %config.dns_zone,
        upstream = %config.dns_upstream,
        "coold DNS listening",
    );

    server.block_until_done().await.context("dns server loop")?;
    Ok(())
}
