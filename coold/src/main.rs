mod config;
mod corrosion;
mod dns;
mod grpc;
mod host_infra;
mod mesh_dns_anchor;
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

    let namespaces: Vec<String> = config
        .namespaces
        .iter()
        .map(|n| format!("{}={}", n.name, n.network))
        .collect();
    info!(
        host_mgmt_ip = %config.host_mgmt_ip,
        namespaces = %namespaces.join(","),
        podman_socket = %config.podman_socket.display(),
        corrosion_url = %config.corrosion_url,
        reconcile_interval = ?config.reconcile_interval,
        host_infra_reconcile_interval = ?config.host_infra_reconcile_interval,
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
