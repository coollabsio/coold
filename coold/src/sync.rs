use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinError;
use tokio::time::{sleep, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::{
    config::Config,
    corrosion::{CorrosionClient, Statement},
    grpc::proto::ResourceStatusUpdate,
    model::{
        diff, diff_container_statuses, ContainerStatus, ContainerStatusDelta, Delta, Endpoint,
    },
    podman::{
        events::{self, EventMessage},
        PodmanClient,
    },
};

pub async fn run(config: Config) -> Result<()> {
    let podman = PodmanClient::new(
        config.podman_socket.clone(),
        config.allowed_mount_sources.as_slice().to_vec(),
    );
    let corrosion = CorrosionClient::new(&config.corrosion_url)?;
    let (tx, rx) = mpsc::channel::<EventMessage>(256);
    let (resource_status_tx, _) = broadcast::channel(256);
    // R1: shared flag that forces the next reconcile to re-emit a full
    // container-status snapshot. Raised when a flux stream (re)connects or the
    // broadcast lagged, so status truth converges after any flux/Laravel
    // downtime instead of being lost with the advanced snapshot.
    let resync_signal = ResyncSignal::default();
    let ctx = Arc::new(SyncContext {
        config,
        podman,
        corrosion,
        container_status_snapshot: Mutex::new(HashMap::new()),
        resource_status_tx,
        resync_signal: resync_signal.clone(),
    });

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

    let host_infra_handle = {
        let config = ctx.config.clone();
        tokio::spawn(async move { crate::host_infra::run(config).await })
    };

    let dns_handle = {
        let config = ctx.config.clone();
        let corrosion = ctx.corrosion.clone();
        tokio::spawn(async move { crate::dns::run(config, corrosion).await })
    };

    let grpc_handle = {
        let config = ctx.config.clone();
        let podman = ctx.podman.clone();
        let resource_status_tx = ctx.resource_status_tx.clone();
        let resync_signal = resync_signal.clone();
        tokio::spawn(async move {
            crate::grpc::run(config, podman, resource_status_tx, resync_signal).await
        })
    };

    drop(tx);

    tokio::select! {
        res = events_handle    => propagate("events",    res)?,
        res = trigger_handle   => propagate("trigger",   res)?,
        res = reconcile_handle => propagate("reconcile", res)?,
        res = host_infra_handle => propagate("host-infra", res)?,
        res = dns_handle       => propagate("dns",       res)?,
        res = grpc_handle      => propagate("grpc",      res)?,
        _ = tokio::signal::ctrl_c() => info!("ctrl-c received, shutting down"),
    }

    Ok(())
}

/// Convert a `JoinHandle` result for a named task into a top-level `Result`.
/// A normal `Ok(())` return is treated as an unexpected early exit and
/// bubbled up as an error so systemd can restart the daemon instead of
/// silently losing a worker.
fn propagate(task: &str, res: std::result::Result<Result<()>, JoinError>) -> Result<()> {
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
    container_status_snapshot: Mutex<HashMap<String, ContainerStatus>>,
    resource_status_tx: broadcast::Sender<ResourceStatusUpdate>,
    resync_signal: ResyncSignal,
}

/// Cross-task flag (R1) that forces the next reconcile to re-emit the full
/// container-status snapshot. The gRPC client raises it whenever a stream
/// (re)connects or its broadcast receiver lags, guaranteeing Laravel receives
/// current truth for every container after any flux/Laravel outage — instead
/// of a delta that was dropped while there were no subscribers.
#[derive(Clone, Default)]
pub struct ResyncSignal(Arc<AtomicBool>);

impl ResyncSignal {
    /// Request a full status resync on the next reconcile.
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Consume the pending request, returning whether one was set.
    fn take(&self) -> bool {
        self.0.swap(false, Ordering::SeqCst)
    }
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
    let all_containers = desired_container_statuses(ctx)
        .await
        .context("fetch all podman container statuses")?;
    let force_resync = ctx.resync_signal.take();
    let container_status_deltas = {
        // R1: hold the snapshot lock across the broadcast so the snapshot only
        // advances after every delta was actually enqueued. `broadcast::send`
        // is synchronous, so this cannot deadlock. If there are no subscribers
        // (flux down) the send fails and the snapshot stays put, so the same
        // transition is re-emitted on the next reconcile once flux reconnects.
        let mut current = ctx.container_status_snapshot.lock().await;
        reconcile_container_status_snapshot(
            &mut current,
            all_containers,
            force_resync,
            &ctx.config.host_mgmt_ip,
            |update| ctx.resource_status_tx.send(update).is_ok(),
        )
    };

    let desired = desired_endpoints(ctx).await.context("fetch podman state")?;
    let current = ctx
        .corrosion
        .snapshot_for_host(&ctx.config.host_mgmt_ip)
        .await
        .context("fetch corrosion snapshot")?;

    let deltas = diff(&desired, &current);
    if !deltas.is_empty() {
        let statements = build_statements(&deltas);
        ctx.corrosion
            .transaction(&statements)
            .await
            .context("apply corrosion transaction")?;
    }

    Ok(deltas.len() + container_status_deltas.len())
}

/// Enumerate every container on the host, not just mesh-managed containers, so
/// Coolify can receive status updates for ingress and future non-managed views.
async fn desired_container_statuses(ctx: &SyncContext) -> Result<HashMap<String, ContainerStatus>> {
    let containers = ctx.podman.containers_list().await?;
    let mut out = HashMap::new();

    for c in containers {
        let inspect = ctx.podman.inspect_container(&c.id).await.ok();
        let name = inspect
            .as_ref()
            .map(|inspect| inspect.name.trim_start_matches('/').to_string())
            .filter(|name| !name.is_empty())
            .or_else(|| {
                c.names
                    .first()
                    .map(|name| name.trim_start_matches('/').to_string())
                    .filter(|name| !name.is_empty())
            })
            .unwrap_or_else(|| c.id.clone());
        let state = inspect
            .as_ref()
            .and_then(|inspect| inspect.state.as_ref())
            .map(|state| state.status.to_lowercase())
            .filter(|state| !state.is_empty())
            .unwrap_or_else(|| c.state.to_lowercase());

        out.insert(
            c.id.clone(),
            ContainerStatus {
                container_id: c.id,
                container_name: name,
                image: c.image,
                state: if state.is_empty() {
                    "unknown".into()
                } else {
                    state
                },
            },
        );
    }

    Ok(out)
}

/// Enumerate every container on every managed namespace's bridge network on
/// this host, keyed by `container_id` so the map lines up 1:1 with Corrosion
/// rows (PK is `container_id`). A container attached to two managed bridges
/// yields only one row — the first namespace wins and the second attachment
/// is logged. Alpha does not support dual-attach routing; changing the schema
/// to allow it would require a composite PK.
async fn desired_endpoints(ctx: &SyncContext) -> Result<HashMap<String, Endpoint>> {
    let containers = ctx.podman.containers_list().await?;
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

/// Reconcile the container-status snapshot and emit deltas to flux (R1).
///
/// - When `force_resync` is set the snapshot baseline is cleared first, so the
///   full current truth is re-emitted (used on flux (re)connect and on
///   broadcast lag).
/// - The snapshot only advances to `all_containers` when *every* emitted
///   update was accepted (`emit` returned `true`). If flux has no subscribers
///   the emit fails and the baseline is left unchanged, so the same deltas are
///   re-emitted on the next reconcile and Laravel eventually converges.
///
/// `emit` returns `true` when the update was enqueued/broadcast successfully.
fn reconcile_container_status_snapshot(
    snapshot: &mut HashMap<String, ContainerStatus>,
    all_containers: HashMap<String, ContainerStatus>,
    force_resync: bool,
    host_mgmt_ip: &str,
    mut emit: impl FnMut(ResourceStatusUpdate) -> bool,
) -> Vec<ContainerStatusDelta> {
    if force_resync {
        snapshot.clear();
    }

    let deltas = diff_container_statuses(&all_containers, snapshot);
    if deltas.is_empty() {
        return deltas;
    }

    let mut all_emitted = true;
    for update in container_resource_status_updates_from_deltas(host_mgmt_ip, &deltas) {
        if !emit(update) {
            all_emitted = false;
        }
    }

    if all_emitted {
        *snapshot = all_containers;
    } else {
        debug!("no flux status subscribers; snapshot held for re-emit on next reconcile");
    }

    deltas
}

fn container_resource_status_updates_from_deltas(
    host_mgmt_ip: &str,
    deltas: &[ContainerStatusDelta],
) -> Vec<ResourceStatusUpdate> {
    deltas
        .iter()
        .map(|delta| match delta {
            ContainerStatusDelta::Upsert(status) => ResourceStatusUpdate {
                resource_type: "container".into(),
                host_id: host_mgmt_ip.into(),
                container_id: status.container_id.clone(),
                container_name: status.container_name.clone(),
                status: status.state.clone(),
                status_message: "Container state received from coold.".into(),
            },
            ContainerStatusDelta::Delete { container_id } => ResourceStatusUpdate {
                resource_type: "container".into(),
                host_id: host_mgmt_ip.into(),
                container_id: container_id.clone(),
                container_name: String::new(),
                status: "removed".into(),
                status_message: "Container removed from coold host.".into(),
            },
        })
        .collect()
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

#[cfg(test)]
mod resource_status_tests {
    use super::*;
    use crate::grpc::proto::ResourceStatusUpdate;

    fn container_status(id: &str, name: &str, state: &str) -> ContainerStatus {
        ContainerStatus {
            container_id: id.into(),
            container_name: name.into(),
            image: "docker.io/library/nginx:alpine".into(),
            state: state.into(),
        }
    }

    #[test]
    fn container_status_updates_are_generic_container_resources() {
        let updates = container_resource_status_updates_from_deltas(
            "100.64.0.5",
            &[
                ContainerStatusDelta::Upsert(container_status("abc", "coolify-v5-caddy", "exited")),
                ContainerStatusDelta::Delete {
                    container_id: "gone".into(),
                },
            ],
        );

        assert_eq!(updates[0].resource_type, "container");
        assert_eq!(updates[0].container_name, "coolify-v5-caddy");
        assert_eq!(updates[0].status, "exited");
        assert_eq!(updates[1].resource_type, "container");
        assert_eq!(updates[1].status, "removed");
    }

    fn one_container() -> HashMap<String, ContainerStatus> {
        let mut m = HashMap::new();
        m.insert("abc".into(), container_status("abc", "web", "running"));
        m
    }

    #[test]
    fn failed_broadcast_re_emits_same_delta_next_reconcile() {
        let mut snapshot = HashMap::new();

        // First reconcile with NO subscribers (emit fails): delta produced but
        // snapshot must NOT advance.
        let mut emitted = Vec::new();
        let deltas = reconcile_container_status_snapshot(
            &mut snapshot,
            one_container(),
            false,
            "100.64.0.5",
            |u| {
                emitted.push(u);
                false // no subscribers
            },
        );
        assert_eq!(deltas.len(), 1);
        assert!(
            snapshot.is_empty(),
            "snapshot must not advance on failed send"
        );

        // Second reconcile, still same truth: the same delta is re-emitted
        // because the baseline never moved. This time a subscriber accepts it.
        let mut emitted2 = Vec::new();
        let deltas2 = reconcile_container_status_snapshot(
            &mut snapshot,
            one_container(),
            false,
            "100.64.0.5",
            |u| {
                emitted2.push(u);
                true // subscriber present
            },
        );
        assert_eq!(deltas2.len(), 1);
        assert_eq!(emitted2[0].container_id, "abc");
        assert_eq!(snapshot.len(), 1, "snapshot advances after successful send");

        // Third reconcile: nothing changed and snapshot is current → no delta.
        let deltas3 = reconcile_container_status_snapshot(
            &mut snapshot,
            one_container(),
            false,
            "100.64.0.5",
            |_| true,
        );
        assert!(deltas3.is_empty());
    }

    #[test]
    fn reconnect_forces_full_snapshot_re_emit() {
        // Snapshot already reflects current truth (no pending deltas).
        let mut snapshot = one_container();

        // Without a resync, nothing is re-emitted.
        let none = reconcile_container_status_snapshot(
            &mut snapshot,
            one_container(),
            false,
            "100.64.0.5",
            |_| true,
        );
        assert!(none.is_empty());

        // On (re)connect force_resync clears the baseline and the full current
        // truth is re-emitted for every container.
        let mut emitted = Vec::new();
        let deltas = reconcile_container_status_snapshot(
            &mut snapshot,
            one_container(),
            true,
            "100.64.0.5",
            |u| {
                emitted.push(u);
                true
            },
        );
        assert_eq!(deltas.len(), 1);
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].container_id, "abc");
        assert_eq!(emitted[0].status, "running");
    }
}
