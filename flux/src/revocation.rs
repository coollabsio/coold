//! JWT `jti` revocation denylist (#3).
//!
//! Laravel calls the flux UDS (`/v1/tokens/revoke`) on server destroy / re-home
//! to revoke a host's token. Revocations are held in memory (consulted by the
//! auth layer on every stream connect) and persisted to a small on-disk JSON
//! file so they survive a flux restart. Entries carry an optional `expires_at`
//! (seconds since epoch) matching the token `exp`; once past `expires_at` a
//! denylist entry is useless (the token would fail expiry anyway) and is pruned.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::auth::{unix_now, RevocationCheck};

/// A single revocation record. `expires_at` is when the underlying token
/// `exp`s; `None` means "keep until explicitly unrevoked".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevocationEntry {
    pub jti: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

/// In-memory revocation denylist shared (via `Arc`) between the UDS bridge
/// (which mutates it) and the auth layer (which reads it). Backed by an
/// on-disk JSON file for durability across restarts.
#[derive(Clone)]
pub struct RevocationStore {
    inner: Arc<RwLock<HashMap<String, Option<u64>>>>,
    /// On-disk backing file. `None` = in-memory only (tests), persistence skipped.
    path: Option<Arc<PathBuf>>,
}

impl RevocationStore {
    /// Load the denylist from `path`, pruning entries already past their
    /// `expires_at`. A missing file yields an empty store. A malformed file is
    /// logged and treated as empty (fail-open on load is acceptable: Laravel
    /// re-pushes revocations, and the token's own `exp` remains enforced).
    pub fn load(path: PathBuf) -> Self {
        let map = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Vec<RevocationEntry>>(&bytes) {
                Ok(entries) => {
                    let now = unix_now();
                    entries
                        .into_iter()
                        .filter(|e| e.expires_at.is_none_or(|exp| exp > now))
                        .map(|e| (e.jti, e.expires_at))
                        .collect()
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "malformed revocation file; starting empty");
                    HashMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "reading revocation file failed; starting empty");
                HashMap::new()
            }
        };
        info!(path = %path.display(), count = map.len(), "loaded JWT revocation denylist");
        Self {
            inner: Arc::new(RwLock::new(map)),
            path: Some(Arc::new(path)),
        }
    }

    /// In-memory-only store (tests / when persistence is undesired).
    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            path: None,
        }
    }

    /// Add `jti` to the denylist and persist. `expires_at` (token `exp`) lets
    /// the sweeper prune the entry once it can no longer matter.
    pub fn revoke(&self, jti: String, expires_at: Option<u64>) -> Result<()> {
        {
            let mut guard = self.inner.write().unwrap();
            guard.insert(jti, expires_at);
        }
        self.persist()
    }

    /// Remove `jti` from the denylist and persist. Returns whether it was present.
    pub fn unrevoke(&self, jti: &str) -> Result<bool> {
        let removed = {
            let mut guard = self.inner.write().unwrap();
            guard.remove(jti).is_some()
        };
        self.persist()?;
        Ok(removed)
    }

    /// Snapshot of current entries (sorted by jti for stable output).
    pub fn list(&self) -> Vec<RevocationEntry> {
        let guard = self.inner.read().unwrap();
        let mut entries: Vec<RevocationEntry> = guard
            .iter()
            .map(|(jti, expires_at)| RevocationEntry {
                jti: jti.clone(),
                expires_at: *expires_at,
            })
            .collect();
        entries.sort_by(|a, b| a.jti.cmp(&b.jti));
        entries
    }

    /// Drop entries past `expires_at` at time `now`; persists if anything
    /// changed. Returns the number pruned.
    pub fn prune(&self, now: u64) -> Result<usize> {
        let pruned = {
            let mut guard = self.inner.write().unwrap();
            let before = guard.len();
            guard.retain(|_, expires_at| expires_at.is_none_or(|exp| exp > now));
            before - guard.len()
        };
        if pruned > 0 {
            self.persist()?;
        }
        Ok(pruned)
    }

    /// Write the current denylist to disk atomically (temp file + rename).
    /// No-op for an in-memory store (`path == None`).
    fn persist(&self) -> Result<()> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        let entries = self.list();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create revocation dir {}", parent.display()))?;
        }
        let json = serde_json::to_vec_pretty(&entries).context("serialize revocations")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)
            .with_context(|| format!("write revocation tmp {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename revocation file {}", path.display()))?;
        Ok(())
    }
}

impl RevocationCheck for RevocationStore {
    fn is_revoked(&self, jti: &str) -> bool {
        self.inner.read().unwrap().contains_key(jti)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revoked_jti_is_reported_revoked() {
        let store = RevocationStore::in_memory();
        assert!(!store.is_revoked("a"));
        store.revoke("a".into(), None).unwrap();
        assert!(store.is_revoked("a"));
        assert!(!store.is_revoked("b"));
    }

    #[test]
    fn unrevoke_removes_entry() {
        let store = RevocationStore::in_memory();
        store.revoke("a".into(), None).unwrap();
        assert!(store.unrevoke("a").unwrap());
        assert!(!store.is_revoked("a"));
        assert!(!store.unrevoke("a").unwrap(), "second unrevoke is a no-op");
    }

    #[test]
    fn revocation_survives_reload_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revocations.json");

        let store = RevocationStore::load(path.clone());
        store
            .revoke("token-1".into(), Some(unix_now() + 3600))
            .unwrap();
        drop(store);

        let reloaded = RevocationStore::load(path);
        assert!(
            reloaded.is_revoked("token-1"),
            "revocation lost across reload"
        );
    }

    #[test]
    fn expired_entries_pruned_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revocations.json");
        let now = unix_now();

        let store = RevocationStore::load(path.clone());
        store.revoke("live".into(), Some(now + 3600)).unwrap();
        store.revoke("dead".into(), Some(now - 10)).unwrap();
        drop(store);

        let reloaded = RevocationStore::load(path);
        assert!(reloaded.is_revoked("live"));
        assert!(
            !reloaded.is_revoked("dead"),
            "expired entry should be pruned on load"
        );
    }

    #[test]
    fn prune_drops_expired_entries() {
        let store = RevocationStore::in_memory();
        let now = unix_now();
        store.revoke("live".into(), Some(now + 3600)).unwrap();
        store.revoke("dead".into(), Some(now - 1)).unwrap();
        store.revoke("forever".into(), None).unwrap();

        assert_eq!(store.prune(now).unwrap(), 1);
        assert!(store.is_revoked("live"));
        assert!(store.is_revoked("forever"));
        assert!(!store.is_revoked("dead"));
    }
}
