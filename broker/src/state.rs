use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::mpsc;

use coolify_proto::agent::v1::ServerMsg;

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

    pub fn host_ids(&self) -> Vec<String> {
        self.0.iter().map(|e| e.key().clone()).collect()
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

/// In-flight request_ids with their owning host and dispatch timestamp.
/// Sweeper removes entries after DISPATCH_TIMEOUT_SECS and emits code=504.
pub const DISPATCH_TIMEOUT_SECS: u64 = 10;

#[derive(Clone)]
pub struct PendingEntry {
    pub host_id: String,
    pub started_at: Instant,
    pub kind: PendingKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PendingKind {
    Coold,
    Build,
}

#[derive(Clone)]
pub struct Pending(Arc<DashMap<String, PendingEntry>>);

impl Pending {
    pub fn new() -> Self {
        Self(Arc::new(DashMap::new()))
    }

    pub fn insert(&self, request_id: String, host_id: String, kind: PendingKind) {
        self.0.insert(
            request_id,
            PendingEntry {
                host_id,
                started_at: Instant::now(),
                kind,
            },
        );
    }

    pub fn remove(&self, request_id: &str) -> Option<PendingEntry> {
        self.0.remove(request_id).map(|(_, v)| v)
    }

    pub fn get(&self, request_id: &str) -> Option<PendingEntry> {
        self.0.get(request_id).map(|e| e.value().clone())
    }

    /// Drain entries whose `started_at` is older than the configured
    /// dispatch timeout. Only `Coold` kind entries are affected — build
    /// requests get their own (longer) timeout from the builder scope's
    /// `RuntimeMaxSec`, so we do not expire them here.
    pub fn drain_expired(&self) -> Vec<(String, PendingEntry)> {
        let timeout = std::time::Duration::from_secs(DISPATCH_TIMEOUT_SECS);
        let expired: Vec<(String, PendingEntry)> = self
            .0
            .iter()
            .filter(|e| {
                matches!(e.value().kind, PendingKind::Coold)
                    && e.value().started_at.elapsed() >= timeout
            })
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();
        for (id, _) in &expired {
            self.0.remove(id);
        }
        expired
    }
}
