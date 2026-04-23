//! Hetzner Cloud provisioning for install e2e tests.
//!
//! Shells out to `curl` against the Hetzner Cloud API so no new Rust deps are
//! added. Each test run uploads a one-shot SSH public key and creates N
//! servers labeled `coolify-e2e=1` / `coolify-e2e-project=<HETZNER_PROJECT>`.
//! The [`EphemeralCluster`] RAII guard deletes servers + key on drop, so
//! panics during assertions still clean up.
//!
//! Required env vars:
//!
//! - `HETZNER_TOKEN`   — Hetzner Cloud API token (project-scoped).
//! - `HETZNER_PROJECT` — label value for cleanup filtering.
//! - `SSH_KEY`         — local private key path. The matching public key is
//!   read from `$SSH_KEY.pub` when present, else derived via
//!   `ssh-keygen -y -f $SSH_KEY` (works for unencrypted keys). It is
//!   uploaded as an ephemeral Hetzner ssh_key so cloud-init can inject it
//!   into root's `authorized_keys`. Deleted on teardown.
//!
//! Optional:
//!
//! - `HETZNER_LOCATION`    (default `nbg1`)
//! - `HETZNER_IMAGE`       (default `ubuntu-24.04`)
//! - `HETZNER_SERVER_TYPE` (default `cx23`)

use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const API: &str = "https://api.hetzner.cloud/v1";

/// Emit a tagged `[hetzner] …` line through the shared test logger so
/// parallel-test prefixes apply here too.
macro_rules! hlog {
    ($($arg:tt)*) => {
        $crate::log_line(&format!("[hetzner] {}", format!($($arg)*)))
    };
}

pub struct HetznerClient {
    pub token: String,
    pub project: String,
}

#[derive(Clone, Debug)]
pub struct Server {
    pub id: u64,
    pub name: String,
    pub ipv4: String,
}

impl HetznerClient {
    pub fn from_env() -> Self {
        crate::load_dotenv();
        Self {
            token: must_env("HETZNER_TOKEN"),
            project: must_env("HETZNER_PROJECT"),
        }
    }

    fn curl(&self, method: &str, path: &str, body: Option<&str>) -> Result<Value, String> {
        let url = format!("{API}{path}");
        let mut cmd = Command::new("curl");
        cmd.arg("-sS")
            .arg("-X")
            .arg(method)
            .arg("-H")
            .arg(format!("Authorization: Bearer {}", self.token))
            .arg("-H")
            .arg("Content-Type: application/json")
            .arg("-w")
            .arg("\nHTTP_STATUS:%{http_code}");
        if let Some(b) = body {
            cmd.arg("-d").arg(b);
        }
        cmd.arg(&url);

        let out = cmd
            .output()
            .map_err(|e| format!("curl spawn: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "curl {method} {path} exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        let (body_part, status_part) = stdout
            .rsplit_once("\nHTTP_STATUS:")
            .unwrap_or((stdout.as_ref(), "0"));
        let status: u16 = status_part.trim().parse().unwrap_or(0);
        let body_trim = body_part.trim();

        if !(200..300).contains(&status) {
            return Err(format!(
                "hetzner {method} {path} status={status} body={body_trim}"
            ));
        }

        if body_trim.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str::<Value>(body_trim)
            .map_err(|e| format!("parse json {method} {path}: {e}: {body_trim}"))
    }

    pub fn upload_key(&self, pubkey: &str, name: &str) -> Result<u64, String> {
        let body = serde_json::json!({
            "name": name,
            "public_key": pubkey.trim(),
            "labels": {
                "coolify-e2e": "1",
                "coolify-e2e-project": self.project,
            },
        })
        .to_string();
        let resp = self.curl("POST", "/ssh_keys", Some(&body))?;
        resp["ssh_key"]["id"]
            .as_u64()
            .ok_or_else(|| format!("no ssh_key.id in response: {resp}"))
    }

    /// Look up an existing ssh_key in the project by exact public-key match.
    /// Hetzner rejects duplicate uploads with 409 `uniqueness_error`, so the
    /// test reuses the pre-existing key instead of uploading a new one.
    /// Returns `None` when no match is found.
    pub fn find_key_by_pubkey(&self, pubkey: &str) -> Result<Option<u64>, String> {
        let target = normalize_pubkey(pubkey);
        let mut page = 1u32;
        loop {
            let resp = self.curl(
                "GET",
                &format!("/ssh_keys?per_page=50&page={page}"),
                None,
            )?;
            let items = resp["ssh_keys"].as_array().cloned().unwrap_or_default();
            for k in &items {
                if let Some(pk) = k["public_key"].as_str() {
                    if normalize_pubkey(pk) == target {
                        return Ok(k["id"].as_u64());
                    }
                }
            }
            let last_page = resp["meta"]["pagination"]["last_page"]
                .as_u64()
                .unwrap_or(1) as u32;
            if page >= last_page || items.is_empty() {
                return Ok(None);
            }
            page += 1;
        }
    }

    pub fn delete_key(&self, id: u64) -> Result<(), String> {
        self.curl("DELETE", &format!("/ssh_keys/{id}"), None)
            .map(|_| ())
    }

    pub fn create_server(
        &self,
        name: &str,
        ssh_key_id: u64,
        image: &str,
        server_type: &str,
        location: &str,
    ) -> Result<Server, String> {
        let body = serde_json::json!({
            "name": name,
            "server_type": server_type,
            "image": image,
            "location": location,
            "ssh_keys": [ssh_key_id],
            "labels": {
                "coolify-e2e": "1",
                "coolify-e2e-project": self.project,
            },
            "start_after_create": true,
        })
        .to_string();
        let resp = self.curl("POST", "/servers", Some(&body))?;
        let s = &resp["server"];
        let id = s["id"]
            .as_u64()
            .ok_or_else(|| format!("no server.id in response: {resp}"))?;
        let ipv4 = s["public_net"]["ipv4"]["ip"]
            .as_str()
            .ok_or_else(|| format!("no public_net.ipv4.ip in response: {resp}"))?
            .to_string();
        Ok(Server {
            id,
            name: name.to_string(),
            ipv4,
        })
    }

    pub fn delete_server(&self, id: u64) -> Result<(), String> {
        self.curl("DELETE", &format!("/servers/{id}"), None)
            .map(|_| ())
    }

    pub fn list_labeled(&self, resource: &str) -> Result<Vec<Value>, String> {
        // label_selector value `coolify-e2e=1` — `=` must be URL-encoded.
        let resp = self.curl(
            "GET",
            &format!("/{resource}?label_selector=coolify-e2e%3D1"),
            None,
        )?;
        Ok(resp[resource].as_array().cloned().unwrap_or_default())
    }

    pub fn wait_ssh_ready(
        &self,
        ipv4: &str,
        ssh_key: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let ok = Command::new("ssh")
                .args(ephemeral_ssh_args(ssh_key))
                .arg(format!("root@{ipv4}"))
                .arg("true")
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                return Ok(());
            }
            thread::sleep(Duration::from_secs(3));
        }
        Err(format!("ssh never ready for {ipv4}"))
    }
}

/// RAII handle over a set of Hetzner servers and the one-shot ssh key used to
/// reach them. Drop deletes servers first, then the key (Hetzner rejects key
/// delete while still referenced, so ordering matters).
pub struct EphemeralCluster {
    client: Arc<HetznerClient>,
    pub key_id: u64,
    /// True when we uploaded the pubkey ourselves and should delete it on
    /// drop. False when we reused an existing Hetzner ssh_key — deleting
    /// that would nuke a user-owned project asset.
    key_owned: bool,
    pub servers: Vec<Server>,
    pub ssh_key: String,
}

impl EphemeralCluster {
    /// Provision `count` servers in the configured project. Uploads the local
    /// pubkey first, then creates servers sequentially (fast — 2 req total for
    /// the 2-host case), then waits SSH-ready in parallel, then waits
    /// `cloud-init status --wait` so subsequent apt-get runs don't collide
    /// with the image's first-boot setup.
    ///
    /// If any single step fails after resources exist, best-effort cleanup
    /// happens before panicking so the caller doesn't leak paid VMs.
    pub fn provision(count: usize, prefix: &str) -> Self {
        assert!(count >= 1, "count must be >= 1");
        let ssh_key = must_env("SSH_KEY");
        let pubkey = derive_pubkey(&ssh_key)
            .unwrap_or_else(|e| panic!("derive pubkey from {ssh_key}: {e}"));

        let client = Arc::new(HetznerClient::from_env());
        let location = std::env::var("HETZNER_LOCATION").unwrap_or_else(|_| "nbg1".into());
        let image = std::env::var("HETZNER_IMAGE").unwrap_or_else(|_| "ubuntu-24.04".into());
        let stype = std::env::var("HETZNER_SERVER_TYPE").unwrap_or_else(|_| "cx23".into());

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let key_name = format!("coold-e2e-{prefix}-{nanos}");

        let (key_id, key_owned) = match client
            .find_key_by_pubkey(&pubkey)
            .unwrap_or_else(|e| panic!("find existing pubkey: {e}"))
        {
            Some(id) => {
                hlog!("reuse existing ssh_key id={id}");
                (id, false)
            }
            None => {
                hlog!("upload pubkey {key_name}");
                let id = client
                    .upload_key(&pubkey, &key_name)
                    .unwrap_or_else(|e| panic!("upload pubkey: {e}"));
                (id, true)
            }
        };

        let mut servers: Vec<Server> = Vec::with_capacity(count);
        for i in 0..count {
            let name = format!("coold-e2e-{prefix}-{nanos}-{i}");
            hlog!("create server {name}");
            match client.create_server(&name, key_id, &image, &stype, &location) {
                Ok(s) => {
                    hlog!("  id={} ipv4={}", s.id, s.ipv4);
                    servers.push(s);
                }
                Err(e) => {
                    best_effort_cleanup(&client, key_id, key_owned, &servers);
                    panic!("create_server {name}: {e}");
                }
            }
        }

        // Parallel SSH-ready wait — dominant wall-clock cost (~30–60s per VM).
        let mut handles = Vec::with_capacity(servers.len());
        for s in servers.iter().cloned() {
            let c = Arc::clone(&client);
            let key = ssh_key.clone();
            let t = crate::tag();
            handles.push(thread::spawn(move || {
                crate::set_tag(t);
                c.wait_ssh_ready(&s.ipv4, &key, Duration::from_secs(180))
                    .map(|_| s)
            }));
        }
        for h in handles {
            if let Err(e) = h.join().expect("ssh-ready thread panic") {
                best_effort_cleanup(&client, key_id, key_owned, &servers);
                panic!("ssh ready: {e}");
            }
        }

        // Wait for cloud-init to finish so apt-get in `coolify init` doesn't
        // race the image's first-boot configuration (dpkg locks, etc.).
        let mut handles = Vec::with_capacity(servers.len());
        for s in servers.iter().cloned() {
            let key = ssh_key.clone();
            let t = crate::tag();
            handles.push(thread::spawn(move || {
                crate::set_tag(t);
                wait_cloud_init(&key, &s.ipv4, Duration::from_secs(300))
            }));
        }
        for h in handles {
            h.join().expect("cloud-init thread panic");
        }

        Self {
            client,
            key_id,
            key_owned,
            servers,
            ssh_key,
        }
    }

    pub fn hosts(&self) -> &[Server] {
        &self.servers
    }
}

impl Drop for EphemeralCluster {
    fn drop(&mut self) {
        // Opt-in bypass: set E2E_KEEP_VMS=1 to skip teardown and leave VMs
        // running for manual poking. Clean them up later with the
        // cleanup_leaked_hetzner sweeper (CONFIRM_SWEEP=1) or the Hetzner UI.
        if std::env::var("E2E_KEEP_VMS").as_deref() == Ok("1") {
            hlog!("E2E_KEEP_VMS=1 — keeping {} server(s) alive:", self.servers.len());
            for s in &self.servers {
                hlog!("  kept server {} ({}) {}", s.id, s.name, s.ipv4);
            }
            hlog!(
                "  kept ssh_key id={} (owned={})",
                self.key_id,
                self.key_owned
            );
            hlog!("  tear down later: CONFIRM_SWEEP=1 cargo test -p e2e-tests --test install cleanup_leaked_hetzner -- --ignored --nocapture");
            return;
        }
        for s in &self.servers {
            match self.client.delete_server(s.id) {
                Ok(()) => hlog!("deleted server {} ({})", s.id, s.name),
                Err(e) => hlog!("WARN delete server {} ({}) failed: {e}", s.id, s.name),
            }
        }
        if self.key_owned {
            // Brief settle so the key is no longer referenced.
            thread::sleep(Duration::from_secs(2));
            match self.client.delete_key(self.key_id) {
                Ok(()) => hlog!("deleted key {}", self.key_id),
                Err(e) => hlog!("WARN delete key {} failed: {e}", self.key_id),
            }
        } else {
            hlog!(
                "keeping pre-existing ssh_key id={} (not owned by this run)",
                self.key_id
            );
        }
    }
}

fn best_effort_cleanup(
    client: &HetznerClient,
    key_id: u64,
    key_owned: bool,
    servers: &[Server],
) {
    for s in servers {
        if let Err(e) = client.delete_server(s.id) {
            hlog!("WARN cleanup delete server {}: {e}", s.id);
        }
    }
    if key_owned {
        if let Err(e) = client.delete_key(key_id) {
            hlog!("WARN cleanup delete key {key_id}: {e}");
        }
    }
}

fn wait_cloud_init(ssh_key: &str, ip: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let out = Command::new("ssh")
            .args(ephemeral_ssh_args(ssh_key))
            .arg(format!("root@{ip}"))
            .arg("cloud-init status --wait 2>/dev/null || true")
            .output();
        if let Ok(o) = out {
            let s = String::from_utf8_lossy(&o.stdout);
            if s.contains("status: done") {
                return;
            }
        }
        thread::sleep(Duration::from_secs(3));
    }
    hlog!("WARN cloud-init wait timed out for {ip}; proceeding anyway");
}

/// SSH args safe for short-lived Hetzner IPs that may be reused across runs.
/// Disables known_hosts pinning (otherwise a reused IP collides with the
/// prior run's host key).
pub fn ephemeral_ssh_args(ssh_key: &str) -> Vec<String> {
    vec![
        "-i".into(),
        ssh_key.into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
    ]
}

fn must_env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("env {k} required"))
}

/// Compare-friendly form of an OpenSSH public key: strip the trailing
/// comment field and normalize whitespace so `ssh-ed25519 AAAA... user@host`
/// and `ssh-ed25519 AAAA...` match.
fn normalize_pubkey(pk: &str) -> String {
    let parts: Vec<&str> = pk.split_whitespace().collect();
    match parts.as_slice() {
        [algo, body, ..] => format!("{algo} {body}"),
        _ => pk.trim().to_string(),
    }
}

/// Extract the OpenSSH public key for the given private key path. Prefers
/// the sibling `.pub` file (cheap, no agent interaction). Falls back to
/// `ssh-keygen -y -f <key>`, which works for unencrypted keys without
/// touching the filesystem.
fn derive_pubkey(privkey_path: &str) -> Result<String, String> {
    let sibling = format!("{privkey_path}.pub");
    if std::path::Path::new(&sibling).exists() {
        let pk = std::fs::read_to_string(&sibling)
            .map_err(|e| format!("read {sibling}: {e}"))?;
        return Ok(pk.trim().to_string());
    }
    let out = Command::new("ssh-keygen")
        .args(["-y", "-f", privkey_path])
        .output()
        .map_err(|e| format!("spawn ssh-keygen: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ssh-keygen -y -f {privkey_path} exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let pk = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if pk.is_empty() {
        return Err(format!("ssh-keygen produced empty output for {privkey_path}"));
    }
    Ok(pk)
}
