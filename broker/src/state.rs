use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::mpsc;

use coolify_proto::agent::v1::ServerMsg;

/// Shared map: host_id → sender into the open gRPC stream.
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
