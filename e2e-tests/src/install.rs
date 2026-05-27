//! Helpers for the `install` test suite (`tests/install.rs`).
//!
//! Post-install assertions run over SSH against Hetzner VMs provisioned by
//! [`crate::hetzner::EphemeralCluster`]. The coolify init binary is invoked
//! **locally** (from the test runner) via [`local_coolify`], since that is
//! the CLI's native fan-out model.
//!
//! All SSH calls use [`ephemeral_ssh_args`](crate::hetzner::ephemeral_ssh_args)
//! — known_hosts pinning is disabled so reused Hetzner IPs don't collide.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::hetzner::ephemeral_ssh_args;
use crate::wait_until;

pub const TEST_IMAGE: &str = "docker.io/library/alpine:3.19";
pub const NAMESPACE: &str = "default";
pub const NET: &str = "coolify-default-mesh";

pub struct InstallEnv {
    pub ssh_key: String,
    pub coolify_bin: String,
    pub cooldctl_bin: String,
}

impl InstallEnv {
    pub fn from_env() -> Self {
        crate::load_dotenv();
        Self {
            ssh_key: must("SSH_KEY"),
            coolify_bin: std::env::var("COOLIFY_BIN").unwrap_or_else(|_| "coolify".into()),
            cooldctl_bin: std::env::var("COOLDCTL_BIN").unwrap_or_else(|_| "cooldctl".into()),
        }
    }
}

fn must(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("env {k} required"))
}

/// Invoke `ssh -i <key> [ephemeral opts] root@<host> '<cmd>'` and capture
/// stdout. Returns stderr on non-zero exit.
pub fn ssh(ssh_key: &str, host: &str, cmd: &str) -> Result<String, String> {
    let out = Command::new("ssh")
        .args(ephemeral_ssh_args(ssh_key))
        .arg(format!("root@{host}"))
        .arg(cmd)
        .output()
        .map_err(|e| format!("ssh spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ssh {host} exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run the local `coolify` binary. Inherits stderr so the CLI's progress
/// output is visible with `--nocapture`; captures stdout in case callers
/// want to parse JSON/table output later.
/// Run the local `cooldctl` binary. Used by ignored Hetzner tests for the Rust
/// v5 cluster CLI. Inherits stderr for progress output and forces noninteractive
/// mode so bootstrap never waits for stdin.
pub fn local_cooldctl(cooldctl_bin: &str, args: &[&str]) -> Result<String, String> {
    eprintln!("[local] {cooldctl_bin} {}", args.join(" "));
    let out = Command::new(cooldctl_bin)
        .args(args)
        .env("COOLIFY_NON_INTERACTIVE", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("spawn {cooldctl_bin}: {e}"))?;
    if !out.status.success() {
        return Err(format!("cooldctl {:?} exit {:?}", args, out.status.code()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn local_coolify(coolify_bin: &str, args: &[&str]) -> Result<String, String> {
    eprintln!("[local] {coolify_bin} {}", args.join(" "));
    let out = Command::new(coolify_bin)
        .args(args)
        .env("COOLIFY_NON_INTERACTIVE", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("spawn {coolify_bin}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "coolify {:?} exit {:?}",
            args,
            out.status.code()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// WireGuard IPv4 address assigned to `wg0` on this host.
pub fn wg0_ip(ssh_key: &str, host: &str) -> String {
    let out = ssh(
        ssh_key,
        host,
        "ip -4 -o addr show wg0 | awk '{print $4}' | cut -d/ -f1",
    )
    .unwrap_or_else(|e| panic!("wg0_ip {host}: {e}"));
    out.trim().to_string()
}

/// True when every `wg show wg0 latest-handshakes` row has a non-zero
/// timestamp — i.e. every configured peer has exchanged at least one
/// handshake. Single-host clusters have no peers; callers that need a
/// handshake should only call this on multi-host clusters.
pub fn wg_peers_handshaken(ssh_key: &str, host: &str) -> bool {
    let out = match ssh(ssh_key, host, "wg show wg0 latest-handshakes") {
        Ok(s) => s,
        Err(_) => return false,
    };
    let mut rows = 0;
    for line in out.lines() {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        rows += 1;
        if parts[1] == "0" {
            return false;
        }
    }
    rows > 0
}

pub fn unit_active(ssh_key: &str, host: &str, unit: &str) -> bool {
    match ssh(
        ssh_key,
        host,
        &format!("systemctl is-active {unit} 2>&1 || true"),
    ) {
        Ok(s) => s.trim() == "active",
        Err(_) => false,
    }
}

pub fn podman_pull(ssh_key: &str, host: &str, image: &str) {
    ssh(ssh_key, host, &format!("podman pull {image}"))
        .unwrap_or_else(|e| panic!("podman pull {image} on {host}: {e}"));
}

/// Start a detached container and return its IP on `net`.
pub fn run_container(
    ssh_key: &str,
    host: &str,
    name: &str,
    net: &str,
    image: &str,
) -> String {
    // `sleep infinity` keeps alpine alive without needing an init.
    ssh(
        ssh_key,
        host,
        &format!(
            "podman run -d --name {name} --network {net} {image} sleep infinity >/dev/null"
        ),
    )
    .unwrap_or_else(|e| panic!("run_container {name} on {host}: {e}"));

    // Fetch IP on the given bridge. `index` is required because network
    // names contain hyphens, which Go templates can't dot-access as
    // identifiers (`.Networks.coolify-default-mesh` is a parse error).
    let tpl =
        format!(r#"{{{{(index .NetworkSettings.Networks "{net}").IPAddress}}}}"#);
    let out = ssh(
        ssh_key,
        host,
        &format!("podman inspect -f '{tpl}' {name}"),
    )
    .unwrap_or_else(|e| panic!("inspect {name}: {e}"));
    let ip = out.trim().to_string();
    assert!(!ip.is_empty(), "container {name} has no IP on {net}");
    ip
}

pub fn rm_container(ssh_key: &str, host: &str, name: &str) {
    let _ = ssh(
        ssh_key,
        host,
        &format!("podman rm -f {name} 2>/dev/null || true"),
    );
}

/// `podman exec <from> ping -c1 -W2 <to_ip>`. True only on exit 0.
pub fn podman_ping(ssh_key: &str, host: &str, from: &str, to_ip: &str) -> bool {
    ssh(
        ssh_key,
        host,
        &format!(
            "podman exec {from} ping -c1 -W2 {to_ip} >/dev/null 2>&1 && echo Y || echo N"
        ),
    )
    .map(|s| s.trim() == "Y")
    .unwrap_or(false)
}

/// Raw ICMP ping from a host's root shell (used for wg0 mgmt-to-mgmt checks).
pub fn ssh_ping(ssh_key: &str, host: &str, target_ip: &str) -> bool {
    ssh(
        ssh_key,
        host,
        &format!("ping -c1 -W2 {target_ip} >/dev/null 2>&1 && echo Y || echo N"),
    )
    .map(|s| s.trim() == "Y")
    .unwrap_or(false)
}

pub fn coold_token(ssh_key: &str, host: &str) -> String {
    ssh(ssh_key, host, "cat /etc/coolify/api-token")
        .unwrap_or_else(|e| panic!("read api-token on {host}: {e}"))
        .trim()
        .to_string()
}

/// POST a rule to the coold API (plain HTTP on wg0:8443). Returned rule
/// JSON's `id` is parsed and returned.
pub fn coold_allow(
    ssh_key: &str,
    host: &str,
    mgmt_ip: &str,
    token: &str,
    body_json: &str,
) -> String {
    // curl runs on the host so the :8443 socket bound to wg0 is reachable.
    let cmd = format!(
        "curl -fsS -XPOST \
         -H 'Authorization: Bearer {token}' \
         -H 'Content-Type: application/json' \
         http://{mgmt_ip}:8443/api/v1/firewall/allow \
         -d '{body_json}'"
    );
    let out = ssh(ssh_key, host, &cmd)
        .unwrap_or_else(|e| panic!("coold_allow on {host}: {e}"));
    let v: serde_json::Value = serde_json::from_str(out.trim())
        .unwrap_or_else(|e| panic!("parse allow response {out:?}: {e}"));
    v["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no id in allow response: {out}"))
        .to_string()
}

pub fn coold_revoke(
    ssh_key: &str,
    host: &str,
    mgmt_ip: &str,
    token: &str,
    id: &str,
) {
    let cmd = format!(
        "curl -fsS -XDELETE \
         -H 'Authorization: Bearer {token}' \
         http://{mgmt_ip}:8443/api/v1/firewall/allow/{id}"
    );
    ssh(ssh_key, host, &cmd)
        .unwrap_or_else(|e| panic!("coold_revoke {id} on {host}: {e}"));
}

/// Default path where the CLI writes the api bearer token on central hosts.
pub fn api_token_path() -> PathBuf {
    PathBuf::from("/etc/coolify/api-token")
}

/// Poll `cond` at 1s intervals until it returns true or `timeout` elapses.
/// Thin alias for [`wait_until`] kept close to the install helpers so tests
/// can `use e2e_tests::install::*;` without pulling in the crate root.
pub fn wait_for<F: FnMut() -> bool>(cond: F, timeout: Duration) -> bool {
    wait_until(cond, timeout)
}

/// Upload a local file to `root@<host>:<remote_path>` via scp, reusing the
/// same ephemeral SSH flags as the rest of the suite.
pub fn scp_upload(ssh_key: &str, host: &str, local: &str, remote: &str) -> Result<(), String> {
    let out = Command::new("scp")
        .args(ephemeral_ssh_args(ssh_key))
        .arg(local)
        .arg(format!("root@{host}:{remote}"))
        .output()
        .map_err(|e| format!("scp spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "scp {local} -> {host}:{remote} exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Issue an HTTP request to `url` from `host` via `ssh + curl`, returning
/// `(status_code, body)`. Body reassembly mirrors `Env::uds_*` parsing.
pub fn ssh_http(
    ssh_key: &str,
    host: &str,
    method: &str,
    url: &str,
    json_body: Option<&str>,
) -> Result<(u16, String), String> {
    let body_arg = match json_body {
        Some(b) => format!(" -H 'Content-Type: application/json' --data '{b}'"),
        None => String::new(),
    };
    let cmd = format!(
        "curl -sS -X {method}{body_arg} -w '\\n__CODE__%{{http_code}}__' {url}"
    );
    let out = ssh(ssh_key, host, &cmd)?;
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
