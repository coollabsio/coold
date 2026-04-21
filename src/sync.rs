use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinError;
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
        tokio::spawn(async move {
            events::run(ctx.podman.clone(), tx).await;
            Ok::<(), anyhow::Error>(())
        })
    };

    let trigger_handle = {
        let ctx = ctx.clone();
        tokio::spawn(async move { run_event_trigger(ctx, rx).await })
    };

    let reconcile_handle = {
        let ctx = ctx.clone();
        tokio::spawn(async move { run_reconcile_loop(ctx).await })
    };

    let dns_handle = {
        let config = ctx.config.clone();
        let corrosion = ctx.corrosion.clone();
        tokio::spawn(async move { crate::dns::run(config, corrosion).await })
    };

    let firewall_handle = {
        let config = ctx.config.clone();
        tokio::spawn(async move { crate::firewall::run(config).await })
    };

    drop(tx);

    tokio::select! {
        res = events_handle    => propagate("events",    res)?,
        res = trigger_handle   => propagate("trigger",   res)?,
        res = reconcile_handle => propagate("reconcile", res)?,
        res = dns_handle       => propagate("dns",       res)?,
        res = firewall_handle  => propagate("firewall",  res)?,
        _ = tokio::signal::ctrl_c() => info!("ctrl-c received, shutting down"),
    }

    Ok(())
}

/// Convert a `JoinHandle` result for a named task into a top-level `Result`.
/// A normal `Ok(())` return is treated as an unexpected early exit and
/// bubbled up as an error so systemd can restart the daemon instead of
/// silently losing a worker.
fn propagate(
    task: &str,
    res: std::result::Result<Result<()>, JoinError>,
) -> Result<()> {
    match res {
        Ok(Ok(())) => {
            warn!(task, "task exited unexpectedly");
            Err(anyhow!("{task} task exited unexpectedly"))
        }
        Ok(Err(e)) => {
            warn!(task, error = format!("{:#}", e), "task failed");
            Err(e.context(format!("{task} task failed")))
        }
        Err(e) => {
            warn!(task, error = %e, "task panicked");
            Err(anyhow!("{task} task panicked: {e}"))
        }
    }
}

struct SyncContext {
    config: Config,
    podman: PodmanClient,
    corrosion: CorrosionClient,
}

async fn run_event_trigger(
    ctx: Arc<SyncContext>,
    mut rx: mpsc::Receiver<EventMessage>,
) -> Result<()> {
    while let Some(msg) = rx.recv().await {
        debug!(?msg.kind, container_id = %msg.container_id, "podman event");
        if let Err(e) = reconcile_once(&ctx).await {
            warn!(error = %e, "event-driven reconcile failed");
        }
    }
    Ok(())
}

async fn run_reconcile_loop(ctx: Arc<SyncContext>) -> Result<()> {
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

/// Enumerate every container on every managed namespace's bridge network on
/// this host, keyed by `container_id` so the map lines up 1:1 with Corrosion
/// rows (PK is `container_id`). A container attached to two managed bridges
/// yields only one row — the first namespace wins and the second attachment
/// is logged. Alpha does not support dual-attach routing; changing the schema
/// to allow it would require a composite PK.
async fn desired_endpoints(ctx: &SyncContext) -> Result<HashMap<String, Endpoint>> {
    let containers = ctx.podman.list_containers().await?;
    let mut out: HashMap<String, Endpoint> = HashMap::new();

    // Build a lookup from podman network name → namespace so we can stamp
    // the right namespace on each endpoint without another podman call.
    let network_to_ns: HashMap<&str, &str> = ctx
        .config
        .namespaces
        .iter()
        .map(|n| (n.network.as_str(), n.name.as_str()))
        .collect();

    for c in containers {
        let inspect = match ctx.podman.inspect_container(&c.id).await {
            Ok(i) => i,
            Err(e) => {
                debug!(container_id = %c.id, error = %e, "inspect failed; skipping");
                continue;
            }
        };
        // Track every container attached to a managed mesh network regardless
        // of run state — a stopped/exited container's bridge attachment
        // remains in inspect until `podman rm`, so we keep reporting its
        // state until removal. Routing consumers filter on state='running'
        // AND health IN ('healthy','unknown').
        let Some(net_settings) = inspect.network_settings.as_ref() else {
            continue;
        };

        let name = if !inspect.name.is_empty() {
            inspect.name.trim_start_matches('/').to_string()
        } else {
            c.names
                .first()
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_else(|| c.id.clone())
        };

        let state_str = inspect
            .state
            .as_ref()
            .map(|s| s.status.to_lowercase())
            .unwrap_or_default();
        let health_str = inspect
            .state
            .as_ref()
            .and_then(|s| s.health.as_ref())
            .map(|h| h.status.to_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".into());

        for (net_name, entry) in &net_settings.networks {
            let Some(ns) = network_to_ns.get(net_name.as_str()) else {
                continue;
            };
            if let Some(existing) = out.get(&c.id) {
                warn!(
                    container_id = %c.id,
                    existing_namespace = %existing.namespace,
                    additional_namespace = %ns,
                    "container attached to multiple managed namespaces; only first recorded",
                );
                continue;
            }
            out.insert(
                c.id.clone(),
                Endpoint {
                    container_id: c.id.clone(),
                    container_name: name.clone(),
                    namespace: (*ns).to_string(),
                    host_mgmt_ip: ctx.config.host_mgmt_ip.clone(),
                    container_ip: entry.ip_address.clone(),
                    state: state_str.clone(),
                    health: health_str.clone(),
                },
            );
        }
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
                 (container_id, container_name, namespace, host_mgmt_ip, container_ip, state, health, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(container_id) DO UPDATE SET \
                   container_name = excluded.container_name, \
                   namespace      = excluded.namespace, \
                   host_mgmt_ip   = excluded.host_mgmt_ip, \
                   container_ip   = excluded.container_ip, \
                   state          = excluded.state, \
                   health         = excluded.health, \
                   updated_at     = excluded.updated_at",
                vec![
                    json!(ep.container_id),
                    json!(ep.container_name),
                    json!(ep.namespace),
                    json!(ep.host_mgmt_ip),
                    json!(ep.container_ip),
                    json!(ep.state),
                    json!(ep.health),
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
