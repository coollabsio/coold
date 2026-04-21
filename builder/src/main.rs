mod config;
mod executor;
mod progress;
mod static_build;
mod stream;

use anyhow::Result;
use tracing::info;
use tracing_subscriber::EnvFilter;

use config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load().await?;
    init_tracing(&config.log_level);

    info!(
        builder_id = %config.builder_id,
        broker_url = %config.broker_url,
        capacity   = config.capacity,
        "builder starting",
    );

    stream::run(config).await
}

fn init_tracing(level: &str) {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
