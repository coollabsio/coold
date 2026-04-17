use std::{path::PathBuf, time::Duration};

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
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    humantime::parse_duration(s).map_err(|e| e.to_string())
}
