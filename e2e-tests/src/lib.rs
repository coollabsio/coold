//! Live-server test harness for the coold/broker/builder stack.
//!
//! Tests are written as Rust integration tests under `tests/`, marked
//! `#[ignore]` so default `cargo test` skips them. Run with:
//!
//! ```text
//! BUILDER_HOST=<host-a> \
//! COOLD_ONLY_HOST=<host-b> \
//! BUILDER_MGMT=<wg0-ip-of-host-a> \
//! COOLD_ONLY_MGMT=<wg0-ip-of-host-b> \
//! CENTRAL_HOST=<central-host> \
//! SSH_KEY=~/.ssh/<key> \
//! cargo test -p e2e-tests -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` is mandatory: the tests dispatch real builds against
//! a shared cluster, and running them in parallel overwhelms the
//! `COOLD_BUILDER_CAPACITY` semaphore and races on `buildah images` state
//! shared across hosts.
//!
//! The harness drives Redis via `ssh + redis-cli` on the central host and
//! asserts remote state via `buildah images` and `systemctl is-active`.
//! No broker/coold code is linked — tests exercise the black-box contract.

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

pub mod hetzner;
pub mod install;

use std::cell::RefCell;

thread_local! {
    static TAG: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Set a short prefix included by [`log_step`] / [`log_ok`] / hetzner
/// progress lines. Each test calls this at entry so interleaved parallel
/// output can be disambiguated.
pub fn set_tag(t: impl Into<String>) {
    TAG.with(|c| *c.borrow_mut() = t.into());
}

pub fn tag() -> String {
    TAG.with(|c| c.borrow().clone())
}

/// Prefix `msg` with the thread-local tag (if set) and emit to stderr.
pub fn log_line(msg: &str) {
    let t = tag();
    if t.is_empty() {
        eprintln!("{msg}");
    } else {
        eprintln!("[{t}] {msg}");
    }
}

/// Populate `std::env` from `<crate>/.env` if the file exists. Values in the
/// file never override existing env vars — a shell-exported var always wins.
/// Idempotent across calls; safe to invoke from every `from_env()`.
pub fn load_dotenv() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((k, v)) = trimmed.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim().trim_matches('"').trim_matches('\'');
        if std::env::var_os(k).is_none() {
            std::env::set_var(k, v);
        }
    }
}

pub struct Env {
    pub builder_host: String,
    pub cool_only_host: String,
    pub builder_mgmt: String,
    pub cool_only_mgmt: String,
    pub central_host: String,
    pub ssh_key: String,
    pub ssh_user: String,
}

impl Env {
    pub fn from_env() -> Self {
        load_dotenv();
        Self {
            builder_host: must("BUILDER_HOST"),
            cool_only_host: must("COOLD_ONLY_HOST"),
            builder_mgmt: must("BUILDER_MGMT"),
            cool_only_mgmt: must("COOLD_ONLY_MGMT"),
            central_host: must("CENTRAL_HOST"),
            ssh_key: must("SSH_KEY"),
            ssh_user: std::env::var("SSH_USER").unwrap_or_else(|_| "root".into()),
        }
    }

    pub fn ssh(&self, host: &str, cmd: &str) -> Result<String, String> {
        let out = Command::new("ssh")
            .args([
                "-i",
                &self.ssh_key,
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                &format!("{}@{}", self.ssh_user, host),
                cmd,
            ])
            .output()
            .map_err(|e| format!("spawn ssh: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "ssh {host}: exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn redis_xadd(&self, payload: &str) -> Result<(), String> {
        // single-quote the payload in the remote shell; payload is JSON so
        // it already uses double quotes and will survive the single-quote
        // wrapping.
        self.ssh(
            &self.central_host,
            &format!("redis-cli XADD build:cmd '*' payload '{}'", payload),
        )
        .map(|_| ())
    }

    pub fn redis_lpop(&self, request_id: &str) -> Result<Option<String>, String> {
        let out = self.ssh(
            &self.central_host,
            &format!("redis-cli LPOP build:resp:{request_id}"),
        )?;
        let trimmed = out.trim();
        if trimmed.is_empty() || trimmed == "(nil)" {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_owned()))
        }
    }

    pub fn wait_build_resp(&self, request_id: &str, timeout: Duration) -> BuildResponse {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.redis_lpop(request_id) {
                Ok(Some(line)) => {
                    return serde_json::from_str(&line)
                        .unwrap_or_else(|e| panic!("parse response {line:?}: {e}"))
                }
                Ok(None) => thread::sleep(Duration::from_secs(2)),
                Err(e) => panic!("LPOP build:resp:{request_id}: {e}"),
            }
        }
        panic!("no build:resp:{request_id} within {timeout:?}");
    }

    pub fn has_image(&self, host: &str, tag: &str) -> bool {
        let cmd = format!("buildah images 2>/dev/null | grep -q '{tag}' && echo Y || echo N");
        self.ssh(host, &cmd).map(|s| s.contains('Y')).unwrap_or(false)
    }

    pub fn unit_active(&self, host: &str, request_id: &str) -> bool {
        let cmd = format!("systemctl is-active coolify-build-{request_id}.service 2>&1");
        self.ssh(host, &cmd)
            .map(|s| s.trim() == "active")
            .unwrap_or(false)
    }

    pub fn restart_coold(&self, host: &str) -> Result<(), String> {
        self.ssh(host, "systemctl restart coold").map(|_| ())
    }

    pub fn clean_image(&self, host: &str, tag: &str) {
        let _ = self.ssh(
            host,
            &format!("buildah rmi -f localhost/{tag} 2>/dev/null || true"),
        );
    }

    pub fn work_dir_exists(&self, host: &str, request_id: &str) -> bool {
        let cmd = format!(
            "test -d /var/lib/coolify-builder/work/{request_id} && echo Y || echo N"
        );
        self.ssh(host, &cmd).map(|s| s.contains('Y')).unwrap_or(false)
    }
}

fn must(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("env {key} required"))
}

/// Lowercase request_id suitable for use as an OCI image tag (OCI rejects
/// uppercase in repository names).
pub fn uniq_req_id(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{}-{nanos}", prefix.to_lowercase())
}

pub fn build_envelope(
    request_id: &str,
    host_id: &str,
    repo_url: &str,
    git_ref: &str,
    target: &str,
    output_dir: &str,
) -> String {
    let mut obj = serde_json::json!({
        "request_id": request_id,
        "command": {
            "type": "static_build",
            "repo_url": repo_url,
            "git_ref": git_ref,
            "target_image": target,
            "output_dir": output_dir,
        }
    });
    if !host_id.is_empty() {
        obj["host_id"] = serde_json::Value::String(host_id.to_owned());
    }
    obj.to_string()
}

pub fn cancel_envelope(request_id: &str) -> String {
    serde_json::json!({
        "request_id": request_id,
        "command": { "type": "cancel" },
    })
    .to_string()
}

#[derive(Debug, Deserialize)]
pub struct BuildResponse {
    #[serde(default)]
    pub request_id: String,
    pub status: String,
    #[serde(default)]
    pub digest: String,
    #[serde(default)]
    pub registry_ref: String,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub code: u32,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub stage: String,
}

pub fn wait_until<F: FnMut() -> bool>(mut cond: F, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_secs(1));
    }
    false
}
