use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::{sleep, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::{
    config::Config,
    corrosion::{CorrosionClient, Statement},
    model::{diff, Delta, Endpoint},
    podman::{
        events::{self, EventMessage},
        PodmanClient,
    },
};

pub async fn run(config: Config) -> Result<()> {
    let podman = PodmanClient::new(config.podman_socket.clone());
    let corrosion = CorrosionClient::new(&config.corrosion_url)?;
    let ctx = Arc::new(SyncContext {
        config,
        podman,
        corrosion,
    });

    let (tx, rx) = mpsc::channel::<EventMessage>(256);

    let events_handle = {
        let ctx = ctx.clone();
        let tx = tx.clone();
        tokio::spawn(async move { events::run(ctx.podman.clone(), tx).await })
    };

    let trigger_handle = {
        let ctx = ctx.clone();
        tokio::spawn(async move { run_event_trigger(ctx, rx).await })
    };

    let reconcile_handle = {
        let ctx = ctx.clone();
        tokio::spawn(async move { run_reconcile_loop(ctx).await })
    };

    drop(tx);

    tokio::select! {
        _ = events_handle => warn!("events task exited"),
        _ = trigger_handle => warn!("event-trigger task exited"),
        _ = reconcile_handle => warn!("reconcile task exited"),
        _ = tokio::signal::ctrl_c() => info!("ctrl-c received, shutting down"),
    }

    Ok(())
}

struct SyncContext {
    config: Config,
    podman: PodmanClient,
    corrosion: CorrosionClient,
}

async fn run_event_trigger(ctx: Arc<SyncContext>, mut rx: mpsc::Receiver<EventMessage>) {
    while let Some(msg) = rx.recv().await {
        debug!(?msg.kind, container_id = %msg.container_id, "podman event");
        if let Err(e) = reconcile_once(&ctx).await {
            warn!(error = %e, "event-driven reconcile failed");
        }
    }
}

async fn run_reconcile_loop(ctx: Arc<SyncContext>) {
    let mut ticker = tokio::time::interval(ctx.config.reconcile_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        match reconcile_once(&ctx).await {
            Ok(n) if n > 0 => info!(deltas = n, "reconciled"),
            Ok(_) => debug!("reconcile tick: no changes"),
            Err(e) => {
                warn!(error = format!("{:#}", e), "reconcile failed; backing off");
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn reconcile_once(ctx: &SyncContext) -> Result<usize> {
    let desired = desired_endpoints(ctx).await.context("fetch podman state")?;
    let current = ctx
        .corrosion
        .snapshot_for_host(&ctx.config.host_mgmt_ip)
        .await
        .context("fetch corrosion snapshot")?;

    let deltas = diff(&desired, &current);
    if deltas.is_empty() {
        return Ok(0);
    }

    let statements = build_statements(&deltas);
    ctx.corrosion
        .transaction(&statements)
        .await
        .context("apply corrosion transaction")?;
    Ok(deltas.len())
}

async fn desired_endpoints(ctx: &SyncContext) -> Result<HashMap<String, Endpoint>> {
    let containers = ctx.podman.list_containers().await?;
    let mut out = HashMap::new();

    for c in containers {
        let inspect = match ctx.podman.inspect_container(&c.id).await {
            Ok(i) => i,
            Err(e) => {
                debug!(container_id = %c.id, error = %e, "inspect failed; skipping");
                continue;
            }
        };
        let Some(net_settings) = inspect.network_settings.as_ref() else {
            continue;
        };
        let Some(entry) = net_settings.networks.get(&ctx.config.mesh_network) else {
            continue;
        };
        if entry.ip_address.is_empty() {
            continue;
        }

        let name = if !inspect.name.is_empty() {
            inspect.name.trim_start_matches('/').to_string()
        } else {
            c.names
                .first()
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_else(|| c.id.clone())
        };

        out.insert(
            c.id.clone(),
            Endpoint {
                container_id: c.id,
                container_name: name,
                host_mgmt_ip: ctx.config.host_mgmt_ip.clone(),
                container_ip: entry.ip_address.clone(),
            },
        );
    }

    Ok(out)
}

fn build_statements(deltas: &[Delta]) -> Vec<Statement> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let mut out = Vec::with_capacity(deltas.len());
    for d in deltas {
        match d {
            Delta::Upsert(ep) => out.push(Statement::new(
                "INSERT INTO service_endpoints \
                 (container_id, container_name, host_mgmt_ip, container_ip, healthy, updated_at) \
                 VALUES (?, ?, ?, ?, 1, ?) \
                 ON CONFLICT(container_id) DO UPDATE SET \
                   container_name = excluded.container_name, \
                   host_mgmt_ip   = excluded.host_mgmt_ip, \
                   container_ip   = excluded.container_ip, \
                   healthy        = 1, \
                   updated_at     = excluded.updated_at",
                vec![
                    json!(ep.container_id),
                    json!(ep.container_name),
                    json!(ep.host_mgmt_ip),
                    json!(ep.container_ip),
                    json!(now_ms),
                ],
            )),
            Delta::Delete { container_id } => out.push(Statement::new(
                "DELETE FROM service_endpoints WHERE container_id = ?",
                vec![json!(container_id)],
            )),
        }
    }
    out
}

