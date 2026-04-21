use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::mpsc;

use coolify_proto::agent::v1::ServerMsg;
use coolify_proto::builder::v1::BuilderServerMsg;

/// Shared map: host_id → sender into the open coold gRPC stream.
#[derive(Clone)]
pub struct Streams(Arc<DashMap<String, mpsc::Sender<ServerMsg>>>);

impl Streams {
    pub fn new() -> Self {
        Self(Arc::new(DashMap::new()))
    }

    pub fn insert(&self, host_id: String, tx: mpsc::Sender<ServerMsg>) {
        self.0.insert(host_id, tx);
    }

    pub fn remove(&self, host_id: &str) {
        self.0.remove(host_id);
    }

    pub fn get(&self, host_id: &str) -> Option<mpsc::Sender<ServerMsg>> {
        self.0.get(host_id).map(|e| e.value().clone())
    }

    pub fn host_ids(&self) -> Vec<String> {
        self.0.iter().map(|e| e.key().clone()).collect()
    }
}

/// Shared map: builder_id → sender into the open builder gRPC stream.
#[derive(Clone)]
pub struct BuilderStreams(Arc<DashMap<String, mpsc::Sender<BuilderServerMsg>>>);

impl BuilderStreams {
    pub fn new() -> Self {
        Self(Arc::new(DashMap::new()))
    }

    pub fn insert(&self, builder_id: String, tx: mpsc::Sender<BuilderServerMsg>) {
        self.0.insert(builder_id, tx);
    }

    pub fn remove(&self, builder_id: &str) {
        self.0.remove(builder_id);
    }

    pub fn get(&self, builder_id: &str) -> Option<mpsc::Sender<BuilderServerMsg>> {
        self.0.get(builder_id).map(|e| e.value().clone())
    }

    /// Return builder_id with fewest in-flight jobs (for load-balanced dispatch).
    pub fn pick_idle(&self) -> Option<String> {
        self.0.iter().next().map(|e| e.key().clone())
    }
}

/// In-flight request_ids with their dispatch timestamp.
/// Sweeper removes entries after DISPATCH_TIMEOUT_SECS and emits code=504.
pub const DISPATCH_TIMEOUT_SECS: u64 = 10;

#[derive(Clone)]
pub struct Pending(Arc<DashMap<String, Instant>>);

impl Pending {
    pub fn new() -> Self {
        Self(Arc::new(DashMap::new()))
    }

    pub fn insert(&self, request_id: String) {
        self.0.insert(request_id, Instant::now());
    }

    pub fn remove(&self, request_id: &str) {
        self.0.remove(request_id);
    }

    /// Drain entries older than DISPATCH_TIMEOUT_SECS, returning their request_ids.
    pub fn drain_expired(&self) -> Vec<String> {
        let timeout = std::time::Duration::from_secs(DISPATCH_TIMEOUT_SECS);
        let expired: Vec<String> = self
            .0
            .iter()
            .filter(|e| e.value().elapsed() >= timeout)
            .map(|e| e.key().clone())
            .collect();
        for id in &expired {
            self.0.remove(id);
        }
        expired
    }
}
