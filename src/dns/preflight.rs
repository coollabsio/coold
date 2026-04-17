use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, Result};
use tokio::net::{TcpListener, UdpSocket};

/// Attempt to bind TCP+UDP :53 on `gateway` and immediately drop the sockets.
/// If either bind fails, return an actionable error naming the likely causes.
/// Systemd `Restart=on-failure` re-runs coold once the operator clears the
/// collision.
pub async fn check(gateway: IpAddr) -> Result<()> {
    let addr = SocketAddr::new(gateway, 53);

    TcpListener::bind(addr)
        .await
        .with_context(|| format_collision_msg(gateway, "tcp"))?;

    UdpSocket::bind(addr)
        .await
        .with_context(|| format_collision_msg(gateway, "udp"))?;

    Ok(())
}

fn format_collision_msg(gateway: IpAddr, proto: &str) -> String {
    format!(
        "coold: cannot bind {proto} {gateway}:53. \
         Likely causes: \
         (a) Podman aardvark-dns is running because the `coolify-mesh` network \
         was created without `--disable-dns` — rerun `coolify init apply` to \
         recreate it; \
         (b) a host DNS daemon (dnsmasq, pihole, unbound) is bound to \
         0.0.0.0:53 — bind it to a specific interface (e.g. `interface=eth0`) \
         instead of the wildcard"
    )
}

