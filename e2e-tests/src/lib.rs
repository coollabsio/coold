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
//! The harness drives the broker UDS via `ssh + curl --unix-socket` on the
//! central host and asserts remote state via `buildah images` and
//! `systemctl is-active`. No broker/coold code is linked — tests exercise
//! the black-box contract.

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

    /// POST a JSON payload to the broker UDS via `ssh + curl` on the
    /// central host. Returns (status_code, body). Payload must be valid
    /// JSON — double-quoted strings, no single quotes.
    pub fn uds_post(&self, path: &str, payload: &str) -> Result<(u16, String), String> {
        let cmd = format!(
            "curl --unix-socket {sock} -sS -X POST -H 'Content-Type: application/json' --data '{payload}' -w '\\n__CODE__%{{http_code}}__' http://localhost{path}",
            sock = BROKER_SOCKET
        );
        parse_curl_output(self.ssh(&self.central_host, &cmd)?)
    }

    pub fn uds_get(&self, path: &str) -> Result<(u16, String), String> {
        let cmd = format!(
            "curl --unix-socket {sock} -sS -w '\\n__CODE__%{{http_code}}__' http://localhost{path}",
            sock = BROKER_SOCKET
        );
        parse_curl_output(self.ssh(&self.central_host, &cmd)?)
    }

    /// Submit a build dispatch envelope. Returns `Accepted` on 202 with
    /// the assigned `request_id`, or `Rejected` when the broker refused
    /// pre-dispatch (unknown host, no builder, capacity cap) — the
    /// response body is the final error for this request_id.
    pub fn dispatch_build(&self, payload: &str) -> DispatchResult {
        let (code, body) = self
            .uds_post("/v1/build/dispatch", payload)
            .expect("dispatch POST");
        if code == 202 {
            let ack: DispatchAck =
                serde_json::from_str(&body).unwrap_or_else(|e| panic!("parse ack {body:?}: {e}"));
            DispatchResult::Accepted(ack)
        } else {
            let resp: BuildResponse = serde_json::from_str(&body)
                .unwrap_or_else(|e| panic!("parse error body {body:?}: {e}"));
            DispatchResult::Rejected(resp)
        }
    }

    /// Long-poll `GET /v1/build/result/{request_id}` until the response
    /// lands or `timeout` elapses. 404 is tolerated briefly — dispatches
    /// can race the poller.
    pub fn wait_build_result(&self, request_id: &str, timeout: Duration) -> BuildResponse {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let poll_ms = remaining.as_millis().clamp(1_000, 25_000) as u64;
            let (code, body) = self
                .uds_get(&format!(
                    "/v1/build/result/{request_id}?timeout_ms={poll_ms}"
                ))
                .expect("result GET");
            match code {
                200 => {
                    return serde_json::from_str(&body)
                        .unwrap_or_else(|e| panic!("parse result {body:?}: {e}"));
                }
                408 => continue,
                404 => {
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }
                _ => {
                    return serde_json::from_str(&body)
                        .unwrap_or_else(|e| panic!("parse err {body:?}: {e}"));
                }
            }
        }
        panic!("no result for {request_id} within {timeout:?}");
    }

    /// Combined `dispatch + wait`. If dispatch is rejected pre-flight,
    /// returns the error body directly (no polling).
    pub fn dispatch_and_wait(
        &self,
        payload: &str,
        request_id: &str,
        timeout: Duration,
    ) -> BuildResponse {
        match self.dispatch_build(payload) {
            DispatchResult::Rejected(r) => r,
            DispatchResult::Accepted(_) => self.wait_build_result(request_id, timeout),
        }
    }

    pub fn cancel_build(&self, request_id: &str) {
        let _ = self
            .uds_post(&format!("/v1/build/{request_id}/cancel"), "")
            .expect("cancel POST");
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

    /// `stat -c '%F|%a|%U' <path>` → `(kind, mode, owner)`. Returns `Err`
    /// on stat failure (missing file, ssh error). Callers usually assert
    /// on the tuple directly.
    pub fn stat_spec(&self, host: &str, path: &str) -> Result<(String, String, String), String> {
        let out = self.ssh(host, &format!("stat -c '%F|%a|%U' {path}"))?;
        let line = out.trim();
        let mut parts = line.split('|');
        let kind = parts.next().unwrap_or("").to_owned();
        let mode = parts.next().unwrap_or("").to_owned();
        let owner = parts.next().unwrap_or("").to_owned();
        Ok((kind, mode, owner))
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

pub const BROKER_SOCKET: &str = "/run/coolify/broker.sock";

#[derive(Debug, Deserialize)]
pub struct DispatchAck {
    pub request_id: String,
}

#[derive(Debug)]
pub enum DispatchResult {
    Accepted(DispatchAck),
    Rejected(BuildResponse),
}

/// Parse curl -sS output ending in `\n__CODE__<status>__`. Returns
/// (status, body-without-marker).
fn parse_curl_output(out: String) -> Result<(u16, String), String> {
    let (body, code_part) = out
        .rsplit_once("__CODE__")
        .ok_or_else(|| format!("missing __CODE__ marker in curl output: {out:?}"))?;
    let code: u16 = code_part
        .trim_end_matches('_')
        .trim()
        .parse()
        .map_err(|e| format!("parse http code {code_part:?}: {e}"))?;
    Ok((code, body.trim_end().to_owned()))
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
