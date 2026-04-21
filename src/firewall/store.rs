//! Side-effecting boundary: exec iptables, read the chain, snapshot.
//!
//! All mutation is serialized behind a single tokio Mutex — iptables isn't
//! safe against concurrent `-A`/`-D` writers on the same chain (the kernel
//! applies atomically but the CLI reads/mutates the active table via
//! getsockopt+setsockopt round-trips). Serializing at the process level
//! gives us simple, predictable semantics and matches the Go CLI's
//! one-SSH-at-a-time pattern.

use std::{path::PathBuf, process::Stdio, sync::Arc};

use anyhow::{bail, Context, Result};
use tokio::{
    fs,
    io::AsyncWriteExt,
    process::Command,
    sync::Mutex,
};
use tracing::{debug, info};

use super::rule::{parse_chain_line, AllowRule};

/// Bundle of config the store needs. Cheap to clone.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub chain_name: String,
    pub rules_path: PathBuf,
}

/// Iptables-backed COOLIFY-ALLOW manager. Clone freely — all shared state
/// sits behind an `Arc<Mutex>`.
#[derive(Clone)]
pub struct FirewallStore {
    inner: Arc<Inner>,
}

struct Inner {
    cfg: StoreConfig,
    /// Held for every mutation. Reads (list) don't need it but take it
    /// anyway so a caller observing "after apply" never races the apply.
    lock: Mutex<()>,
}

impl FirewallStore {
    pub fn new(cfg: StoreConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                cfg,
                lock: Mutex::new(()),
            }),
        }
    }

    pub fn chain(&self) -> &str {
        &self.inner.cfg.chain_name
    }

    /// Ensure the chain exists. Safe to call repeatedly.
    ///
    /// `iptables -N <chain>` errors "Chain already exists" on rerun; we
    /// swallow that specific exit code. Matches the Go CLI's
    /// `iptables -N COOLIFY-ALLOW 2>/dev/null || true`.
    pub async fn ensure_chain(&self) -> Result<()> {
        let out = Command::new("iptables")
            .args(["-N", self.chain()])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("spawn iptables -N")?;
        if out.status.success() {
            return Ok(());
        }
        // Any non-zero is treated as "already exists" — mirrors the Go CLI
        // which `|| true`s this step. A real permission error surfaces on
        // the next -A/-D anyway with a clearer message.
        debug!(
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "iptables -N returned non-zero (likely already exists)"
        );
        Ok(())
    }

    /// Apply one rule: idempotent `-C || -A`, then snapshot.
    pub async fn apply(&self, rule: &AllowRule) -> Result<()> {
        let _guard = self.inner.lock.lock().await;
        self.ensure_chain().await?;

        // Idempotency: -C exits 0 if the rule already exists.
        let check = iptables_exit(&rule.chain_args("-C", self.chain())).await?;
        if check.success() {
            debug!(id = ?rule.id, "rule already present; no-op");
        } else {
            let out = Command::new("iptables")
                .args(rule.chain_args("-A", self.chain()))
                .output()
                .await
                .context("spawn iptables -A")?;
            if !out.status.success() {
                bail!(
                    "iptables -A failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            info!(id = ?rule.id, "rule applied");
        }
        self.snapshot_locked().await?;
        Ok(())
    }

    /// Revoke by id. No-op on missing (same semantics as the Go CLI).
    pub async fn revoke_by_id(&self, id: &str) -> Result<Option<AllowRule>> {
        let _guard = self.inner.lock.lock().await;

        let rules = self.list_locked().await?;
        let Some(rule) = rules.into_iter().find(|r| r.id.as_deref() == Some(id)) else {
            return Ok(None);
        };

        // -C guard avoids log spam when the kernel state already lost it.
        let check = iptables_exit(&rule.chain_args("-C", self.chain())).await?;
        if check.success() {
            let out = Command::new("iptables")
                .args(rule.chain_args("-D", self.chain()))
                .output()
                .await
                .context("spawn iptables -D")?;
            if !out.status.success() {
                bail!(
                    "iptables -D failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            info!(id = %id, "rule revoked");
        }
        self.snapshot_locked().await?;
        Ok(Some(rule))
    }

    /// Snapshot current kernel state of `chain` via `iptables -S`.
    /// Takes the lock so list-after-apply never races.
    pub async fn list(&self) -> Result<Vec<AllowRule>> {
        let _guard = self.inner.lock.lock().await;
        self.list_locked().await
    }

    /// Force-reload the kernel chain from the on-disk snapshot. Used for
    /// explicit recovery/reconcile after manual tampering. Chain is flushed
    /// first to guarantee convergence.
    pub async fn reconcile_from_file(&self) -> Result<()> {
        let _guard = self.inner.lock.lock().await;
        self.ensure_chain().await?;

        let snapshot = match fs::read(&self.inner.cfg.rules_path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!(
                    path = %self.inner.cfg.rules_path.display(),
                    "no snapshot to reconcile from; leaving chain as-is"
                );
                return Ok(());
            }
            Err(e) => {
                return Err(anyhow::Error::from(e)
                    .context(format!("read {}", self.inner.cfg.rules_path.display())));
            }
        };

        // Flush then restore so the kernel mirrors the file exactly.
        let flush = Command::new("iptables")
            .args(["-F", self.chain()])
            .output()
            .await
            .context("spawn iptables -F")?;
        if !flush.status.success() {
            bail!(
                "iptables -F failed: {}",
                String::from_utf8_lossy(&flush.stderr).trim()
            );
        }
        iptables_restore(&snapshot, /*noflush*/ true).await?;
        info!("chain reloaded from snapshot");
        Ok(())
    }

    /// Bulk add/remove in one kernel transaction via iptables-restore.
    /// `adds` are normalized rules to insert; `removes` are ids to drop.
    /// On success the snapshot is rewritten.
    pub async fn bulk(&self, adds: Vec<AllowRule>, removes: Vec<String>) -> Result<BulkOutcome> {
        let _guard = self.inner.lock.lock().await;
        self.ensure_chain().await?;

        let existing = self.list_locked().await?;
        let mut keep: Vec<AllowRule> = existing
            .into_iter()
            .filter(|r| match &r.id {
                Some(id) => !removes.iter().any(|rid| rid == id),
                None => true,
            })
            .collect();

        let mut added = 0usize;
        for rule in adds {
            let already = keep.iter().any(|k| k.id == rule.id);
            if !already {
                keep.push(rule);
                added += 1;
            }
        }

        // Render a full `*filter` fragment with our chain flushed + rebuilt.
        let fragment = render_fragment(self.chain(), &keep);
        iptables_restore(fragment.as_bytes(), /*noflush*/ true).await?;
        self.write_snapshot(fragment.as_bytes()).await?;
        info!(added, removed = removes.len(), "bulk applied");
        Ok(BulkOutcome {
            added,
            removed: removes.len(),
            total: keep.len(),
        })
    }

    async fn list_locked(&self) -> Result<Vec<AllowRule>> {
        let out = Command::new("iptables")
            .args(["-S", self.chain()])
            .output()
            .await
            .context("spawn iptables -S")?;
        if !out.status.success() {
            // Chain missing → treat as empty rather than surface the exit
            // code — easier on callers that poll before init finishes.
            debug!(
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "iptables -S non-zero; treating as empty chain"
            );
            return Ok(vec![]);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let rules: Vec<AllowRule> = text
            .lines()
            .filter_map(|l| parse_chain_line(l, self.chain()))
            .collect();
        Ok(rules)
    }

    /// Snapshot to disk. Rebuilds from the live chain to avoid drifting
    /// if external writers touched the chain between mutations.
    async fn snapshot_locked(&self) -> Result<()> {
        let rules = self.list_locked().await?;
        let fragment = render_fragment(self.chain(), &rules);
        self.write_snapshot(fragment.as_bytes()).await
    }

    async fn write_snapshot(&self, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = self.inner.cfg.rules_path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        // Atomic: .tmp + rename. Matches the Go CLI's SaveRulesCommand.
        let tmp = self.inner.cfg.rules_path.with_extension("rules.tmp");
        let mut f = fs::File::create(&tmp)
            .await
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes)
            .await
            .with_context(|| format!("write {}", tmp.display()))?;
        f.flush().await.ok();
        drop(f);
        fs::rename(&tmp, &self.inner.cfg.rules_path)
            .await
            .with_context(|| {
                format!(
                    "rename {} -> {}",
                    tmp.display(),
                    self.inner.cfg.rules_path.display()
                )
            })?;
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BulkOutcome {
    pub added: usize,
    pub removed: usize,
    pub total: usize,
}

/// `*filter\n:<chain> -\n<rendered rules>\nCOMMIT\n` — the exact shape the
/// Go CLI's `SaveRulesCommand` produces. `iptables-restore --noflush` on
/// this fragment only touches `<chain>`.
///
/// Rules are grouped by namespace with a `# namespace: <ns>` header per
/// group so snapshots are human-readable and operators can see the
/// tenancy split at a glance. The group boundary is cosmetic — iptables
/// treats `#` lines as comments.
fn render_fragment(chain: &str, rules: &[AllowRule]) -> String {
    let mut sorted: Vec<&AllowRule> = rules.iter().collect();
    sorted.sort_by(|a, b| {
        let ns_a = if a.namespace.is_empty() { "default" } else { &a.namespace };
        let ns_b = if b.namespace.is_empty() { "default" } else { &b.namespace };
        ns_a
            .cmp(ns_b)
            .then_with(|| a.id.as_deref().unwrap_or("").cmp(b.id.as_deref().unwrap_or("")))
    });

    let mut out = String::new();
    out.push_str("*filter\n");
    out.push_str(&format!(":{chain} -\n"));

    let mut current_ns: Option<&str> = None;
    for r in &sorted {
        let ns = if r.namespace.is_empty() { "default" } else { &r.namespace };
        if current_ns != Some(ns) {
            out.push_str(&format!("# namespace: {ns}\n"));
            current_ns = Some(ns);
        }
        let args = r.match_args(chain);
        out.push_str(&format!("-A {chain}"));
        for a in args {
            out.push(' ');
            // iptables-restore accepts unquoted single-token comments
            // (our "cid:<hex>:<ns>" is single-token by construction).
            out.push_str(&a);
        }
        out.push('\n');
    }
    out.push_str("COMMIT\n");
    out
}

/// Pipe bytes into `iptables-restore` (with or without --noflush).
async fn iptables_restore(bytes: &[u8], noflush: bool) -> Result<()> {
    let mut cmd = Command::new("iptables-restore");
    if noflush {
        cmd.arg("--noflush");
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn iptables-restore")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(bytes)
            .await
            .context("write iptables-restore stdin")?;
        stdin.flush().await.ok();
    }
    let out = child.wait_with_output().await.context("wait iptables-restore")?;
    if !out.status.success() {
        bail!(
            "iptables-restore failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn iptables_exit(args: &[String]) -> Result<std::process::ExitStatus> {
    let out = Command::new("iptables")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .context("spawn iptables")?;
    Ok(out.status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn ipv4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn rule(src: &str, dst: &str, port: u16, ns: &str) -> AllowRule {
        AllowRule {
            src: ipv4(src),
            dst: ipv4(dst),
            proto: Some("tcp".into()),
            port: Some(port),
            namespace: ns.into(),
            id: None,
        }
        .normalize()
        .unwrap()
    }

    #[test]
    fn render_fragment_golden_shape() {
        let r = rule("10.210.5.2", "10.210.6.3", 80, "default");
        let out = render_fragment("COOLIFY-ALLOW", std::slice::from_ref(&r));
        assert!(out.starts_with("*filter\n:COOLIFY-ALLOW -\n"));
        assert!(out.ends_with("COMMIT\n"));
        assert!(out.contains("# namespace: default"));
        assert!(out.contains("-A COOLIFY-ALLOW -s 10.210.5.2 -d 10.210.6.3 -p tcp --dport 80"));
        assert!(out.contains(&format!("cid:{}:default", r.id.as_ref().unwrap())));
        assert!(out.contains("-j ACCEPT"));
    }

    #[test]
    fn render_fragment_empty_chain() {
        let out = render_fragment("COOLIFY-ALLOW", &[]);
        assert_eq!(out, "*filter\n:COOLIFY-ALLOW -\nCOMMIT\n");
    }

    #[test]
    fn render_fragment_groups_by_namespace() {
        let r1 = rule("10.210.5.2", "10.210.6.3", 80, "alpha");
        let r2 = rule("10.210.5.2", "10.210.6.3", 81, "default");
        let r3 = rule("10.210.5.2", "10.210.6.3", 82, "alpha");
        let out = render_fragment("COOLIFY-ALLOW", &[r1, r2, r3]);
        let alpha_at = out.find("# namespace: alpha").unwrap();
        let default_at = out.find("# namespace: default").unwrap();
        assert!(alpha_at < default_at, "namespaces sorted alphabetically");
        // Only one header per namespace.
        assert_eq!(out.matches("# namespace: alpha").count(), 1);
        assert_eq!(out.matches("# namespace: default").count(), 1);
    }
}
