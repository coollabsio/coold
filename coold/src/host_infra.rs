use anyhow::Result;
use tokio::time::MissedTickBehavior;
use tracing::debug;

use crate::config::Config;

pub async fn run(config: Config) -> Result<()> {
    let mut ticker = tokio::time::interval(config.host_infra_reconcile_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        reconcile_once(&config).await;
    }
}

async fn reconcile_once(config: &Config) {
    debug!("host infrastructure reconcile tick");
    crate::mesh_dns_anchor::reconcile(config).await;
}
