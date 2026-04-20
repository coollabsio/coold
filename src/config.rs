use std::{net::IpAddr, net::SocketAddr, path::PathBuf, time::Duration};

use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(name = "coold", version, about)]
pub struct Config {
    /// WireGuard management IP for this host (e.g. 100.64.0.5).
    #[arg(long, env = "COOLD_HOST_MGMT_IP")]
    pub host_mgmt_ip: String,

    /// Path to the local Podman Unix socket.
    #[arg(long, env = "COOLD_PODMAN_SOCKET", default_value = "/run/podman/podman.sock")]
    pub podman_socket: PathBuf,

    /// Base URL of the local Corrosion agent's HTTP API.
    #[arg(long, env = "COOLD_CORROSION_URL", default_value = "http://127.0.0.1:8080")]
    pub corrosion_url: String,

    /// Podman network whose containers are tracked.
    #[arg(long, env = "COOLD_MESH_NETWORK", default_value = "coolify-mesh")]
    pub mesh_network: String,

    /// Periodic full reconcile cadence.
    #[arg(
        long,
        env = "COOLD_RECONCILE_INTERVAL",
        default_value = "2s",
        value_parser = parse_duration,
    )]
    pub reconcile_interval: Duration,

    /// `tracing_subscriber` env filter (e.g. `info`, `coold=debug`).
    #[arg(long, env = "COOLD_LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Bridge-gateway IP of the Podman mesh network (e.g. 10.210.5.1).
    /// coold's embedded DNS server binds UDP+TCP :53 here.
    /// Set by the init bootstrap via systemd drop-in. Optional — when absent,
    /// the DNS server is skipped (useful for tests / agent-only deployments).
    #[arg(long, env = "COOLD_BRIDGE_GATEWAY_IP")]
    pub bridge_gateway_ip: Option<IpAddr>,

    /// DNS zone served authoritatively by coold.
    #[arg(long, env = "COOLD_DNS_ZONE", default_value = "coolify.internal")]
    pub dns_zone: String,

    /// Upstream resolver for queries outside `dns_zone`.
    #[arg(long, env = "COOLD_DNS_UPSTREAM", default_value = "1.1.1.1:53")]
    pub dns_upstream: SocketAddr,

    /// Bind address for the firewall REST API (e.g. `100.64.0.5:8443`).
    /// When unset, the API server is disabled. In production set this to
    /// `<host_mgmt_ip>:8443` so the API is reachable only over the wg0
    /// management overlay and never exposed on a public interface.
    #[arg(long, env = "COOLD_API_BIND")]
    pub api_bind: Option<SocketAddr>,

    /// Path to a file containing the API bearer token. When unset, the API
    /// refuses to start (no anonymous access). The file should be root-owned
    /// and mode 0600; contents are trimmed of leading/trailing whitespace.
    #[arg(long, env = "COOLD_API_TOKEN_FILE")]
    pub api_token_file: Option<PathBuf>,

    /// PEM-encoded TLS certificate chain for the API. When both cert and key
    /// are set the API serves HTTPS; otherwise it serves plain HTTP (intended
    /// only for dev/alpha on a trusted overlay).
    #[arg(long, env = "COOLD_TLS_CERT")]
    pub tls_cert: Option<PathBuf>,

    /// PEM-encoded TLS private key for the API.
    #[arg(long, env = "COOLD_TLS_KEY")]
    pub tls_key: Option<PathBuf>,

    /// Path where coold snapshots the COOLIFY-ALLOW chain as an
    /// iptables-restore fragment. `coolify-mesh-allow.service` restores this
    /// on boot via `iptables-restore --noflush`.
    #[arg(long, env = "COOLD_RULES_PATH", default_value = "/etc/coolify/allow.rules")]
    pub rules_path: PathBuf,

    /// Name of the iptables chain coold owns. Must match the chain created
    /// by `coolify init --default-deny` and jumped to from COOLIFY-INTRA.
    #[arg(long, env = "COOLD_CHAIN_NAME", default_value = "COOLIFY-ALLOW")]
    pub chain_name: String,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    humantime::parse_duration(s).map_err(|e| e.to_string())
}
