use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::{mpsc, oneshot};

use coolify_proto::agent::v1::ServerMsg;

use crate::envelope::{ResponseBody, StreamInventoryItem};

/// Connected coold host along with its advertised capability set. One entry
/// per open gRPC stream; keyed by `host_id` (the stable WireGuard mgmt IP
/// Laravel uses when minting the JWT).
#[derive(Clone)]
pub struct StreamHandle {
    pub tx: mpsc::Sender<ServerMsg>,
    pub caps: Vec<String>,
    pub advertised_caps: Vec<String>,
}

struct StreamEntry {
    tx: mpsc::Sender<ServerMsg>,
    caps: Vec<String>,
    advertised_caps: Vec<String>,
    last_seen_at: Instant,
    reachable: bool,
}

/// Shared map: host_id → StreamHandle.
#[derive(Clone)]
pub struct Streams(Arc<DashMap<String, StreamEntry>>);

impl Streams {
    pub fn new() -> Self {
        Self(Arc::new(DashMap::new()))
    }

    pub fn insert(&self, host_id: String, handle: StreamHandle) {
        self.0.insert(
            host_id,
            StreamEntry {
                tx: handle.tx,
                caps: handle.caps,
                advertised_caps: handle.advertised_caps,
                last_seen_at: Instant::now(),
                reachable: true,
            },
        );
    }

    pub fn update_capabilities(&self, host_id: &str, caps: Vec<String>) {
        if let Some(mut entry) = self.0.get_mut(host_id) {
            entry.caps = caps;
        }
    }

    pub fn remove(&self, host_id: &str) {
        self.0.remove(host_id);
    }

    pub fn get(&self, host_id: &str) -> Option<StreamHandle> {
        self.0.get(host_id).map(|e| StreamHandle {
            tx: e.value().tx.clone(),
            caps: e.value().caps.clone(),
            advertised_caps: e.value().advertised_caps.clone(),
        })
    }

    pub fn get_tx(&self, host_id: &str) -> Option<mpsc::Sender<ServerMsg>> {
        self.0.get(host_id).map(|e| e.value().tx.clone())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn snapshot(&self) -> Vec<StreamInventoryItem> {
        let mut items = self
            .0
            .iter()
            .map(|e| {
                let mut caps = e.value().caps.clone();
                caps.sort();
                caps.dedup();
                StreamInventoryItem {
                    host_id: e.key().clone(),
                    caps,
                }
            })
            .collect::<Vec<_>>();
        items.sort_by(|a, b| a.host_id.cmp(&b.host_id));
        items
    }

    pub fn touch(&self, host_id: &str) -> bool {
        let Some(mut entry) = self.0.get_mut(host_id) else {
            return false;
        };
        let restored = !entry.reachable;
        entry.last_seen_at = Instant::now();
        entry.reachable = true;
        restored
    }

    pub fn ping_targets(&self) -> Vec<(String, mpsc::Sender<ServerMsg>)> {
        self.0
            .iter()
            .map(|e| (e.key().clone(), e.value().tx.clone()))
            .collect()
    }

    pub fn mark_stale(&self, timeout: Duration) -> Vec<String> {
        let now = Instant::now();
        let mut host_ids = self
            .0
            .iter_mut()
            .filter_map(|mut entry| {
                if entry.reachable && now.duration_since(entry.last_seen_at) >= timeout {
                    entry.reachable = false;
                    return Some(entry.key().clone());
                }

                None
            })
            .collect::<Vec<_>>();
        host_ids.sort();
        host_ids
    }
}

// ─── Pending dispatches ──────────────────────────────────────────────────────

/// How long a `Landed` response lingers waiting for a late poller.
pub const LANDED_TTL_SECS: u64 = 30;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PendingKind {
    Coold,
}

/// Typed body of a completed response. Kind must match the originating
/// dispatch — the gRPC response handler decides the variant from the
/// pending entry's `PendingKind`.
#[derive(Debug, Clone)]
pub enum ResponseData {
    Coold(ResponseBody),
}

/// State machine for a single request_id.
///
/// Sync coold dispatch and the early/late poll orderings on builds all
/// fall out of this enum. See `unix_bridge` for the full story.
enum PendingState {
    /// Dispatched to a coold stream; no response yet. `sinks` holds any
    /// HTTP handlers currently parked on this request — `deliver()`
    /// fans out to all of them.
    Waiting {
        sinks: Vec<oneshot::Sender<ResponseData>>,
    },
    /// Response arrived but no parker consumed it (yet). Evicted by the
    /// sweeper after `LANDED_TTL_SECS`.
    Landed { body: ResponseData, until: Instant },
}

pub struct PendingEntry {
    pub host_id: String,
    pub kind: PendingKind,
    pub started_at: Instant,
    state: PendingState,
}

/// Read-only view for tests + timeout reporting; omits oneshot senders
/// because they are not `Clone`.
#[derive(Clone, Debug)]
pub struct PendingSnapshot {
    pub host_id: String,
    pub kind: PendingKind,
}

/// Result of parking an HTTP handler on a request_id.
pub enum ParkResult {
    /// Handler is parked; await the receiver.
    Parked(oneshot::Receiver<ResponseData>),
    /// Response was already cached; the body is returned directly and
    /// the entry has been removed.
    AlreadyLanded(ResponseData),
    /// No such request_id — dispatch was never made, or already consumed.
    NotFound,
}

/// Outcome of `insert_waiting` so callers can distinguish a duplicate
/// from a capacity rejection.
#[derive(Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    Duplicate,
    AtCapacity,
}

#[derive(Clone)]
pub struct Pending(Arc<DashMap<String, PendingEntry>>);

impl Pending {
    pub fn new() -> Self {
        Self(Arc::new(DashMap::new()))
    }

    /// Insert a fresh `Waiting` entry with no parked sinks. Returns
    /// `InsertOutcome::Duplicate` when `request_id` is already tracked so the
    /// caller can reject with 409, `AtCapacity` when the map is full (→ 503),
    /// and `Inserted` on success.
    pub fn insert_waiting(
        &self,
        request_id: String,
        host_id: String,
        kind: PendingKind,
        max: usize,
    ) -> InsertOutcome {
        use dashmap::mapref::entry::Entry;
        // Capacity is sampled outside `entry()` because `entry()` holds a
        // write lock on a single shard, and `DashMap::len()` walks every
        // shard — calling it while holding a shard lock deadlocks.
        // The cap is therefore best-effort: brief overshoots under
        // contention are fine for a guardrail against unbounded growth.
        if self.0.len() >= max {
            return InsertOutcome::AtCapacity;
        }
        match self.0.entry(request_id) {
            Entry::Occupied(_) => InsertOutcome::Duplicate,
            Entry::Vacant(slot) => {
                slot.insert(PendingEntry {
                    host_id,
                    kind,
                    started_at: Instant::now(),
                    state: PendingState::Waiting { sinks: Vec::new() },
                });
                InsertOutcome::Inserted
            }
        }
    }

    pub fn remove(&self, request_id: &str) -> Option<PendingSnapshot> {
        self.0.remove(request_id).map(|(_, v)| PendingSnapshot {
            host_id: v.host_id,
            kind: v.kind,
        })
    }

    pub fn get(&self, request_id: &str) -> Option<PendingSnapshot> {
        self.0.get(request_id).map(|e| PendingSnapshot {
            host_id: e.host_id.clone(),
            kind: e.kind,
        })
    }

    /// Park an HTTP handler on `request_id`. If the entry is `Landed`, the
    /// cached body is returned and the entry is evicted. Otherwise a
    /// oneshot pair is created, the sender pushed into the `Waiting` set,
    /// and the receiver returned for the handler to await.
    pub fn park(&self, request_id: &str) -> ParkResult {
        // Hold the shard lock across the state transition so a concurrent
        // `deliver` can't observe a missing entry and drop the response.
        enum Outcome {
            Parked(oneshot::Receiver<ResponseData>),
            Landed(ResponseData),
        }

        let outcome = {
            let mut entry = match self.0.get_mut(request_id) {
                Some(e) => e,
                None => return ParkResult::NotFound,
            };
            match &mut entry.state {
                PendingState::Waiting { sinks } => {
                    let (tx, rx) = oneshot::channel();
                    sinks.push(tx);
                    Outcome::Parked(rx)
                }
                PendingState::Landed { body, .. } => Outcome::Landed(body.clone()),
            }
        };

        match outcome {
            Outcome::Parked(rx) => ParkResult::Parked(rx),
            Outcome::Landed(body) => {
                self.0.remove(request_id);
                ParkResult::AlreadyLanded(body)
            }
        }
    }

    /// Deliver a response body. Fans out to all parked sinks.
    ///
    /// C1: when at least one live handler was parked, the entry is dropped
    /// immediately after fan-out instead of lingering as `Landed` for
    /// `LANDED_TTL_SECS` — a delivered-to-a-live-sink response has no late
    /// poller to serve, so keeping it only inflates `pending_max` occupancy.
    /// The `Landed` TTL is retained ONLY for the late-poll race where the
    /// response arrived before any handler parked (`sinks` empty).
    pub fn deliver(&self, request_id: &str, body: ResponseData) {
        // `oneshot::Sender::send` is non-blocking, so fanning out under the
        // shard lock is safe and closes the park/deliver TOCTOU window.
        let fanned_out_to_live_sink = {
            let Some(mut entry) = self.0.get_mut(request_id) else {
                return;
            };
            let new_state = PendingState::Landed {
                body: body.clone(),
                until: Instant::now() + Duration::from_secs(LANDED_TTL_SECS),
            };
            match std::mem::replace(&mut entry.state, new_state) {
                PendingState::Waiting { sinks } => {
                    let had_live_sink = !sinks.is_empty();
                    for sink in sinks {
                        let _ = sink.send(body.clone());
                    }
                    had_live_sink
                }
                PendingState::Landed { .. } => false,
            }
        };
        // Drop the ref (above scope) before removing to avoid a same-shard
        // self-deadlock. If a late poller claimed the Landed body in the
        // meantime the entry is already gone and this is a no-op.
        if fanned_out_to_live_sink {
            self.0.remove(request_id);
        }
    }

    /// Evict expired entries.
    ///
    /// - `Waiting` of kind `Coold` past the configured timeout → evicted.
    ///   Dropping the sinks causes each parked receiver to error, which
    ///   the handler maps to a 504 response.
    /// - `Landed` past `until` → evicted.
    ///
    /// Returns `host_id`s of expired waits for logging.
    pub fn drain_expired(&self, coold_timeout: Duration) -> Vec<(String, PendingSnapshot)> {
        let now = Instant::now();

        let expired: Vec<String> = self
            .0
            .iter()
            .filter(|e| match &e.value().state {
                PendingState::Waiting { .. } => {
                    now.duration_since(e.value().started_at) >= coold_timeout
                }
                PendingState::Landed { until, .. } => now >= *until,
            })
            .map(|e| e.key().clone())
            .collect();

        expired
            .into_iter()
            .filter_map(|id| {
                self.0.remove(&id).map(|(_, v)| {
                    (
                        id,
                        PendingSnapshot {
                            host_id: v.host_id,
                            kind: v.kind,
                        },
                    )
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn park_returns_parked_on_waiting() {
        let p = Pending::new();
        assert_eq!(
            p.insert_waiting("r1".into(), "H".into(), PendingKind::Coold, 16),
            InsertOutcome::Inserted
        );

        let rx = match p.park("r1") {
            ParkResult::Parked(rx) => rx,
            _ => panic!("expected Parked"),
        };

        let body = ResponseBody::Ok {
            data: serde_json::json!({"k": "v"}),
        };
        p.deliver("r1", ResponseData::Coold(body));

        let got = rx.await.expect("sink delivered");
        match got {
            ResponseData::Coold(ResponseBody::Ok { data }) => {
                assert_eq!(data, serde_json::json!({"k": "v"}));
            }
            _ => panic!("wrong body"),
        }
    }

    #[tokio::test]
    async fn deliver_to_live_sink_drops_entry_immediately() {
        // C1: a response delivered to a parked handler must not linger as a
        // Landed entry occupying pending_max.
        let p = Pending::new();
        let _ = p.insert_waiting("r1".into(), "H".into(), PendingKind::Coold, 16);
        let rx = match p.park("r1") {
            ParkResult::Parked(rx) => rx,
            _ => panic!("expected Parked"),
        };

        p.deliver(
            "r1",
            ResponseData::Coold(ResponseBody::Ok {
                data: serde_json::json!({"k": "v"}),
            }),
        );

        // Handler still receives the body …
        assert!(rx.await.is_ok());
        // … but the pending entry is gone right away (no 30s Landed lingering).
        assert!(
            p.get("r1").is_none(),
            "entry should be dropped after live delivery"
        );
    }

    #[tokio::test]
    async fn park_returns_landed_when_response_arrived_first() {
        let p = Pending::new();
        let _ = p.insert_waiting("r1".into(), "H".into(), PendingKind::Coold, 16);

        p.deliver(
            "r1",
            ResponseData::Coold(ResponseBody::Ok {
                data: serde_json::json!({"k": "v"}),
            }),
        );

        match p.park("r1") {
            ParkResult::AlreadyLanded(ResponseData::Coold(ResponseBody::Ok { data })) => {
                assert_eq!(data, serde_json::json!({"k": "v"}));
            }
            _ => panic!("expected AlreadyLanded"),
        }
        // Entry consumed by park → next park is NotFound.
        assert!(matches!(p.park("r1"), ParkResult::NotFound));
    }

    #[tokio::test]
    async fn deliver_during_park_does_not_drop_response() {
        use std::sync::Arc;
        let p = Arc::new(Pending::new());
        let _ = p.insert_waiting("r1".into(), "H".into(), PendingKind::Coold, 16);

        let body = ResponseData::Coold(ResponseBody::Ok {
            data: serde_json::json!({"k": "v"}),
        });

        let p1 = p.clone();
        let park_task = tokio::spawn(async move {
            match p1.park("r1") {
                ParkResult::Parked(rx) => rx.await.ok(),
                ParkResult::AlreadyLanded(b) => Some(b),
                ParkResult::NotFound => None,
            }
        });
        tokio::task::yield_now().await;
        p.deliver("r1", body);

        let got = park_task.await.expect("task joined");
        assert!(
            matches!(got, Some(ResponseData::Coold(_))),
            "response dropped"
        );
    }

    #[test]
    fn insert_waiting_respects_cap() {
        let p = Pending::new();
        assert_eq!(
            p.insert_waiting("a".into(), "H".into(), PendingKind::Coold, 2),
            InsertOutcome::Inserted
        );
        assert_eq!(
            p.insert_waiting("b".into(), "H".into(), PendingKind::Coold, 2),
            InsertOutcome::Inserted
        );
        assert_eq!(
            p.insert_waiting("c".into(), "H".into(), PendingKind::Coold, 2),
            InsertOutcome::AtCapacity
        );
    }

    #[test]
    fn drain_expired_uses_the_configured_coold_timeout() {
        let p = Pending::new();
        assert_eq!(
            p.insert_waiting("a".into(), "H".into(), PendingKind::Coold, 16),
            InsertOutcome::Inserted
        );

        std::thread::sleep(Duration::from_millis(25));

        assert!(
            p.drain_expired(Duration::from_secs(30)).is_empty(),
            "entry should not expire before configured timeout"
        );
        assert_eq!(p.drain_expired(Duration::ZERO).len(), 1);
    }

    #[test]
    fn insert_waiting_rejects_duplicate() {
        let p = Pending::new();
        assert_eq!(
            p.insert_waiting("a".into(), "H".into(), PendingKind::Coold, 16),
            InsertOutcome::Inserted
        );
        assert_eq!(
            p.insert_waiting("a".into(), "H2".into(), PendingKind::Coold, 16),
            InsertOutcome::Duplicate
        );
        // Original entry still intact — host_id/kind unchanged.
        let snap = p.get("a").expect("entry present");
        assert_eq!(snap.host_id, "H");
        assert_eq!(snap.kind, PendingKind::Coold);
    }
}

#[cfg(test)]
mod stream_snapshot_tests {
    use super::*;

    #[test]
    fn snapshot_is_sorted_and_caps_are_stable() {
        let streams = Streams::new();
        let (tx, _rx) = mpsc::channel(1);
        streams.insert(
            "b".into(),
            StreamHandle {
                tx: tx.clone(),
                caps: vec!["containers.list".into(), "containers.list".into()],
                advertised_caps: vec!["containers.list".into(), "containers.list".into()],
            },
        );
        streams.insert(
            "a".into(),
            StreamHandle {
                tx,
                caps: vec!["containers.list".into()],
                advertised_caps: vec!["containers.list".into()],
            },
        );
        let got = streams.snapshot();
        assert_eq!(
            got.iter().map(|s| s.host_id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(got[1].caps, vec!["containers.list"]);
    }

    #[test]
    fn stale_streams_are_reported_once_until_touched() {
        let streams = Streams::new();
        let (tx, _rx) = mpsc::channel(1);
        streams.insert(
            "host-1".into(),
            StreamHandle {
                tx,
                caps: vec!["containers.list".into()],
                advertised_caps: vec!["containers.list".into()],
            },
        );

        assert_eq!(
            streams.mark_stale(Duration::ZERO),
            vec!["host-1".to_string()]
        );
        assert!(
            streams.mark_stale(Duration::ZERO).is_empty(),
            "stale host should not be reported again until it recovers"
        );
        assert!(streams.touch("host-1"));
        assert_eq!(
            streams.mark_stale(Duration::ZERO),
            vec!["host-1".to_string()]
        );
    }
}
