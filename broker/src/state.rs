use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::{mpsc, oneshot};

use coolify_proto::agent::v1::ServerMsg;

use crate::envelope::{BuildResponseBody, ResponseBody};

/// Connected coold host along with its advertised capability set and
/// builder capacity. One entry per open gRPC stream; keyed by `host_id`
/// (the stable WireGuard mgmt IP Laravel uses when minting the JWT).
#[derive(Clone)]
pub struct StreamHandle {
    pub tx: mpsc::Sender<ServerMsg>,
    pub caps: Vec<String>,
    pub builder_capacity: u32,
}

impl StreamHandle {
    pub fn has_cap(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c == cap)
    }
}

/// Shared map: host_id → StreamHandle.
#[derive(Clone)]
pub struct Streams(Arc<DashMap<String, StreamHandle>>);

impl Streams {
    pub fn new() -> Self {
        Self(Arc::new(DashMap::new()))
    }

    pub fn insert(&self, host_id: String, handle: StreamHandle) {
        self.0.insert(host_id, handle);
    }

    pub fn update_capabilities(&self, host_id: &str, caps: Vec<String>, builder_capacity: u32) {
        if let Some(mut entry) = self.0.get_mut(host_id) {
            entry.caps = caps;
            entry.builder_capacity = builder_capacity;
        }
    }

    pub fn remove(&self, host_id: &str) {
        self.0.remove(host_id);
    }

    pub fn get(&self, host_id: &str) -> Option<StreamHandle> {
        self.0.get(host_id).map(|e| e.value().clone())
    }

    pub fn get_tx(&self, host_id: &str) -> Option<mpsc::Sender<ServerMsg>> {
        self.0.get(host_id).map(|e| e.value().tx.clone())
    }

    pub fn has_cap(&self, host_id: &str, cap: &str) -> bool {
        self.0
            .get(host_id)
            .map(|e| e.value().has_cap(cap))
            .unwrap_or(false)
    }

    /// First connected host that advertises `cap`. First-available semantics
    /// mirror the old `BuilderStreams::pick_idle()` — no load-balancing.
    pub fn pick_host_with_cap(&self, cap: &str) -> Option<String> {
        self.0
            .iter()
            .find(|e| e.value().has_cap(cap))
            .map(|e| e.key().clone())
    }
}

// ─── Pending dispatches ──────────────────────────────────────────────────────

/// Coold sync-dispatch timeout: how long a `POST /v1/coold/dispatch` handler
/// parks before the sweeper drops its oneshot → handler returns 504.
pub const DISPATCH_TIMEOUT_SECS: u64 = 10;

/// How long a `Landed` response lingers waiting for a late poller.
pub const LANDED_TTL_SECS: u64 = 30;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PendingKind {
    Coold,
    Build,
}

/// Typed body of a completed response. Kind must match the originating
/// dispatch — the gRPC response handler decides the variant from the
/// pending entry's `PendingKind`.
#[derive(Debug, Clone)]
pub enum ResponseData {
    Coold(ResponseBody),
    Build(BuildResponseBody),
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
    Landed {
        body: ResponseData,
        until: Instant,
    },
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

#[derive(Clone)]
pub struct Pending(Arc<DashMap<String, PendingEntry>>);

impl Pending {
    pub fn new() -> Self {
        Self(Arc::new(DashMap::new()))
    }

    /// Insert a fresh `Waiting` entry with no parked sinks. Returns `false`
    /// if the map is at-or-above `max` — caller should reject the dispatch
    /// with 503.
    pub fn insert_waiting(
        &self,
        request_id: String,
        host_id: String,
        kind: PendingKind,
        max: usize,
    ) -> bool {
        if self.0.len() >= max {
            return false;
        }
        if self.0.contains_key(&request_id) {
            return false;
        }
        self.0.insert(
            request_id,
            PendingEntry {
                host_id,
                kind,
                started_at: Instant::now(),
                state: PendingState::Waiting { sinks: Vec::new() },
            },
        );
        true
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
        let mut entry = match self.0.get_mut(request_id) {
            Some(e) => e,
            None => return ParkResult::NotFound,
        };

        match &mut entry.state {
            PendingState::Landed { body, .. } => {
                let body = body.clone();
                drop(entry);
                self.0.remove(request_id);
                ParkResult::AlreadyLanded(body)
            }
            PendingState::Waiting { sinks } => {
                let (tx, rx) = oneshot::channel();
                sinks.push(tx);
                ParkResult::Parked(rx)
            }
        }
    }

    /// Deliver a response body. Fans out to all parked sinks and then
    /// transitions the entry to `Landed` with `LANDED_TTL_SECS` TTL so a
    /// late poller can still claim the body.
    pub fn deliver(&self, request_id: &str, body: ResponseData) {
        let mut entry = match self.0.get_mut(request_id) {
            Some(e) => e,
            None => return,
        };

        if let PendingState::Waiting { sinks } = std::mem::replace(
            &mut entry.state,
            PendingState::Landed {
                body: body.clone(),
                until: Instant::now() + Duration::from_secs(LANDED_TTL_SECS),
            },
        ) {
            for sink in sinks {
                let _ = sink.send(body.clone());
            }
        }
    }

    /// Evict expired entries.
    ///
    /// - `Waiting` of kind `Coold` past `DISPATCH_TIMEOUT_SECS` → evicted.
    ///   Dropping the sinks causes each parked receiver to error, which
    ///   the handler maps to a 504 response.
    /// - `Landed` past `until` → evicted.
    ///
    /// Build `Waiting` entries are *not* evicted — builds can take
    /// minutes, and the transient unit's `RuntimeMaxSec` is the real
    /// timeout. Returns `host_id`s of expired coold waits for logging.
    pub fn drain_expired(&self) -> Vec<(String, PendingSnapshot)> {
        let now = Instant::now();
        let coold_timeout = Duration::from_secs(DISPATCH_TIMEOUT_SECS);

        let expired: Vec<String> = self
            .0
            .iter()
            .filter(|e| match &e.value().state {
                PendingState::Waiting { .. } => {
                    matches!(e.value().kind, PendingKind::Coold)
                        && now.duration_since(e.value().started_at) >= coold_timeout
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
        assert!(p.insert_waiting("r1".into(), "H".into(), PendingKind::Coold, 16));

        let rx = match p.park("r1") {
            ParkResult::Parked(rx) => rx,
            _ => panic!("expected Parked"),
        };

        let body = ResponseBody::Ok { data: serde_json::json!({"k": "v"}) };
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
    async fn park_returns_landed_when_response_arrived_first() {
        let p = Pending::new();
        p.insert_waiting("r1".into(), "H".into(), PendingKind::Build, 16);

        p.deliver(
            "r1",
            ResponseData::Build(BuildResponseBody::Ok {
                digest: "sha256:x".into(),
                registry_ref: "ref".into(),
                duration_ms: 1,
            }),
        );

        match p.park("r1") {
            ParkResult::AlreadyLanded(ResponseData::Build(BuildResponseBody::Ok { digest, .. })) => {
                assert_eq!(digest, "sha256:x");
            }
            _ => panic!("expected AlreadyLanded"),
        }
        // Entry consumed by park → next park is NotFound.
        assert!(matches!(p.park("r1"), ParkResult::NotFound));
    }

    #[test]
    fn insert_waiting_respects_cap() {
        let p = Pending::new();
        assert!(p.insert_waiting("a".into(), "H".into(), PendingKind::Coold, 2));
        assert!(p.insert_waiting("b".into(), "H".into(), PendingKind::Coold, 2));
        assert!(!p.insert_waiting("c".into(), "H".into(), PendingKind::Coold, 2));
    }
}
