mod config;
mod corrosion;
mod model;
mod podman;
mod sync;

use anyhow::Result;
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::parse();
    init_tracing(&config.log_level);

    info!(
        host_mgmt_ip = %config.host_mgmt_ip,
        mesh_network = %config.mesh_network,
        podman_socket = %config.podman_socket.display(),
        corrosion_url = %config.corrosion_url,
        reconcile_interval = ?config.reconcile_interval,
        "coold starting",
    );

    sync::run(config).await
}

fn init_tracing(level: &str) {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
