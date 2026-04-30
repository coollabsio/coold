//! Stub-dashboard smoke e2e. Provisions a single Hetzner VM, runs
//! `coolify init bootstrap` (bringing up coold + scheduler + builder), scp's the
//! prebuilt `coolify-stub` binary onto the VM, launches it next to the
//! scheduler UDS, and drives a real static build through its `/api/*` surface.
//!
//! Binary selection (first match wins):
//!   1. `COOLIFY_STUB_BIN=/abs/path/to/coolify-stub` — explicit override.
//!   2. `COOLIFY_STUB_SOURCE=local` — build locally via
//!      `coolify-stub/scripts/build-binary.ts` and use the resulting
//!      `coolify-stub/dist/coolify-stub`.
//!   3. Default: download `coolify-stub-linux-amd64.tar.gz` from the
//!      `coolify-stub-tag` release (env `COOLIFY_STUB_TAG`, default
//!      `nightly`) on `COOLIFY_STUB_REPO` (default `coollabsio/coold`) into
//!      `target/coolify-stub-cache/<tag>/` and use it.
//!
//! Typical run:
//!
//! ```text
//! cargo test -p e2e-tests --test stub -- --ignored --nocapture --test-threads=1
//! ```

use std::time::Duration;

use e2e_tests::hetzner::EphemeralCluster;
use e2e_tests::install::{
    local_coolify, scp_upload, ssh, ssh_http, unit_active, wait_for, wg0_ip, InstallEnv,
};

const MGMT_POOL: &str = "100.64.0.0/16";
const CONTAINER_POOL: &str = "10.210.0.0/16";
const REMOTE_BIN: &str = "/usr/local/bin/coolify-stub";
const REMOTE_LOG: &str = "/var/log/coolify-stub.log";
const STUB_PORT: u16 = 3000;

// Public repo used by the builder suite; kept in sync intentionally so the
// stub path exercises the same build that `builder.rs` exercises via the UDS
// directly.
const SMALL_REPO: &str = "https://github.com/mdn/beginner-html-site";

fn step(msg: &str) {
    e2e_tests::log_line(&format!("─── {msg} ───"));
}

fn ok(msg: &str) {
    e2e_tests::log_line(&format!("  ✓ {msg}"));
}

fn stub_bin_path() -> String {
    e2e_tests::load_dotenv();

    if let Ok(p) = std::env::var("COOLIFY_STUB_BIN") {
        let meta = std::fs::metadata(&p)
            .unwrap_or_else(|e| panic!("COOLIFY_STUB_BIN={p} not readable: {e}"));
        assert!(meta.is_file(), "COOLIFY_STUB_BIN={p} is not a regular file");
        return p;
    }

    if std::env::var("COOLIFY_STUB_SOURCE").as_deref() == Ok("local") {
        return build_local_stub();
    }

    fetch_stub_from_release()
}

fn crate_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR is the e2e-tests crate. Workspace root is one up.
    crate_root().parent().unwrap().to_path_buf()
}

fn build_local_stub() -> String {
    let stub_dir = repo_root().join("coolify-stub");
    assert!(
        stub_dir.is_dir(),
        "COOLIFY_STUB_SOURCE=local but {stub_dir} not found",
        stub_dir = stub_dir.display()
    );
    eprintln!("[stub  ] building coolify-stub locally (linux-x64)");
    let status = std::process::Command::new("bun")
        .args(["scripts/build-binary.ts"])
        .env("BUN_TARGET", "bun-linux-x64")
        .current_dir(&stub_dir)
        .status()
        .unwrap_or_else(|e| panic!("spawn bun (is bun installed?): {e}"));
    assert!(status.success(), "local stub build failed (exit {:?})", status.code());
    let out = stub_dir.join("dist").join("coolify-stub");
    assert!(out.is_file(), "expected build output at {}", out.display());
    out.to_string_lossy().into_owned()
}

fn fetch_stub_from_release() -> String {
    let tag = std::env::var("COOLIFY_STUB_TAG").unwrap_or_else(|_| "nightly".into());
    let repo = std::env::var("COOLIFY_STUB_REPO").unwrap_or_else(|_| "coollabsio/coold".into());
    let asset = "coolify-stub-linux-amd64.tar.gz";

    let cache_dir = repo_root()
        .join("target")
        .join("coolify-stub-cache")
        .join(&tag);
    let binary = cache_dir.join("coolify-stub");
    let stamp = cache_dir.join(".stamp");

    // `nightly` is a rolling tag — re-download if older than 10 minutes so
    // the test picks up fresh pushes. Pinned tags (not `nightly`) are
    // immutable; always reuse once cached.
    let fresh = binary.is_file()
        && stamp.is_file()
        && (tag != "nightly"
            || std::fs::metadata(&stamp)
                .and_then(|m| m.modified())
                .map(|t| t.elapsed().unwrap_or(Duration::from_secs(0)).as_secs() < 600)
                .unwrap_or(false));

    if fresh {
        eprintln!("[stub  ] using cached stub binary from {}", binary.display());
        return binary.to_string_lossy().into_owned();
    }

    std::fs::create_dir_all(&cache_dir).expect("create cache dir");
    let tarball = cache_dir.join(asset);
    let url = format!("https://github.com/{repo}/releases/download/{tag}/{asset}");
    eprintln!("[stub  ] downloading {url}");
    let status = std::process::Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(&tarball)
        .arg(&url)
        .status()
        .unwrap_or_else(|e| panic!("spawn curl: {e}"));
    assert!(
        status.success(),
        "download {url} failed (exit {:?}) — set COOLIFY_STUB_BIN or COOLIFY_STUB_SOURCE=local to bypass",
        status.code()
    );

    let status = std::process::Command::new("tar")
        .args(["-xzf"])
        .arg(&tarball)
        .arg("-C")
        .arg(&cache_dir)
        .status()
        .unwrap_or_else(|e| panic!("spawn tar: {e}"));
    assert!(status.success(), "extract {} failed", tarball.display());

    assert!(
        binary.is_file(),
        "expected {} after extract — archive layout changed?",
        binary.display()
    );
    // Leave execute bit alone; scp_upload preserves file mode.
    let _ = std::fs::write(&stamp, "");
    binary.to_string_lossy().into_owned()
}

fn parse_body(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("parse body {body:?}: {e}"))
}

#[test]
#[ignore = "requires HETZNER_TOKEN + COOLIFY_STUB_BIN; provisions + destroys a Hetzner VM"]
fn stub_smoke() {
    e2e_tests::set_tag("stub  ");
    let cfg = InstallEnv::from_env();
    let stub_bin = stub_bin_path();
    let cluster = EphemeralCluster::provision(1, "stub");
    let host = cluster.hosts()[0].ipv4.clone();

    // 1. Install stack (coold + scheduler + builder colocated on the VM).
    step("1/7  coolify init bootstrap (install stack)");
    local_coolify(
        &cfg.coolify_bin,
        &[
            "init",
            "bootstrap",
            "--servers",
            &host,
            "--central",
            &host,
            "--ssh-user",
            "root",
            "--ssh-key",
            &cfg.ssh_key,
            "--wg-mgmt-pool",
            MGMT_POOL,
            "--container-pool",
            CONTAINER_POOL,
            "--enable-builder",
            "--yes",
        ],
    )
    .expect("coolify init bootstrap");
    ok("init bootstrap returned success");

    // 1b. Optional override: swap the nightly-provisioned builder/coold/scheduler
    //     binaries with a locally-built copy so in-flight fixes can be tested
    //     without a GitHub release round-trip. Restart the affected systemd
    //     units for coold/scheduler; builder has no daemon (coold spawns it per
    //     dispatch) so no restart needed.
    for (var_name, remote_path, unit) in [
        ("E2E_BUILDER_BIN", "/usr/local/bin/builder", None),
        ("E2E_COOLD_BIN", "/usr/local/bin/coold", Some("coold")),
        ("E2E_SCHEDULER_BIN", "/usr/local/bin/scheduler", Some("scheduler")),
    ] {
        if let Ok(local) = std::env::var(var_name) {
            if local.is_empty() {
                continue;
            }
            let meta = std::fs::metadata(&local).unwrap_or_else(|e| {
                panic!("{var_name}={local} not readable: {e}")
            });
            assert!(meta.is_file(), "{var_name}={local} is not a regular file");
            step(&format!("1b/7 override {remote_path} with {local}"));
            scp_upload(&cfg.ssh_key, &host, &local, remote_path)
                .unwrap_or_else(|e| panic!("scp {local} -> {remote_path}: {e}"));
            ssh(&cfg.ssh_key, &host, &format!("chmod +x {remote_path}"))
                .expect("chmod override");
            if let Some(u) = unit {
                ssh(&cfg.ssh_key, &host, &format!("systemctl restart {u}"))
                    .unwrap_or_else(|e| panic!("systemctl restart {u}: {e}"));
                assert!(
                    wait_for(
                        || unit_active(&cfg.ssh_key, &host, u),
                        Duration::from_secs(30),
                    ),
                    "{u} did not come back up after override"
                );
            }
            ok(&format!("{remote_path} replaced from {var_name}"));
        }
    }

    // 2. Confirm scheduler came up before we pile the stub on top.
    step("2/7  wait for scheduler socket + /v1/health");
    assert!(
        wait_for(
            || unit_active(&cfg.ssh_key, &host, "scheduler"),
            Duration::from_secs(30),
        ),
        "scheduler unit never became active on {host}"
    );
    let ping = ssh(
        &cfg.ssh_key,
        &host,
        "curl -sS --unix-socket /run/coolify/scheduler.sock http://localhost/v1/health",
    )
    .unwrap_or_default();
    assert!(ping.contains("\"ok\":true"), "scheduler health: {ping:?}");
    ok("scheduler UDS /v1/health → ok");

    // 3. Upload the prebuilt stub binary and kick it off. We start it under
    //    `setsid` so closing the ssh session doesn't SIGHUP the child. Stub
    //    binds 0.0.0.0:<STUB_PORT> so the dashboard is reachable directly
    //    from outside the VM as well as via SSH port-forward.
    step("3/7  scp + launch coolify-stub");
    scp_upload(&cfg.ssh_key, &host, &stub_bin, REMOTE_BIN).expect("scp stub binary");
    ssh(&cfg.ssh_key, &host, &format!("chmod +x {REMOTE_BIN}")).expect("chmod stub");
    // coold's INPUT chain is default-deny via the coolify firewall scaffold,
    // so we explicitly accept inbound connections to the stub's port before
    // starting it. Idempotent: -C checks before -I inserts.
    let open_port = format!(
        "iptables -C INPUT -p tcp --dport {STUB_PORT} -j ACCEPT 2>/dev/null || \
         iptables -I INPUT -p tcp --dport {STUB_PORT} -j ACCEPT"
    );
    let _ = ssh(&cfg.ssh_key, &host, &open_port);
    let wg_ip = wg0_ip(&cfg.ssh_key, &host);
    let launch = format!(
        "rm -f {REMOTE_LOG}; \
         COOLIFY_HOSTS={wg_ip} SCHEDULER_SOCKET_PATH=/run/coolify/scheduler.sock PORT={STUB_PORT} HOST=0.0.0.0 \
         setsid nohup {REMOTE_BIN} >{REMOTE_LOG} 2>&1 < /dev/null & \
         echo $!",
    );
    let pid = ssh(&cfg.ssh_key, &host, &launch)
        .unwrap_or_else(|e| panic!("launch stub on {host}: {e}"))
        .trim()
        .to_string();
    ok(&format!("stub launched pid={pid} (0.0.0.0:{STUB_PORT})"));

    // Ensure cleanup even on panic. Drop order: this runs *before*
    // EphemeralCluster::drop so logs are always retrievable for triage.
    // When E2E_KEEP_VMS=1 the guard skips pkill so the stub stays live for
    // manual poking at the UI.
    struct StubGuard<'a> {
        ssh_key: &'a str,
        host: &'a str,
    }
    impl<'a> Drop for StubGuard<'a> {
        fn drop(&mut self) {
            let keep = std::env::var("E2E_KEEP_VMS").as_deref() == Ok("1");
            if !keep {
                let _ = ssh(self.ssh_key, self.host, "pkill -f coolify-stub || true");
            }
            // Best-effort log tail for triage (panic or keep-alive).
            if let Ok(tail) = ssh(
                self.ssh_key,
                self.host,
                &format!("tail -n 50 {REMOTE_LOG} 2>/dev/null || true"),
            ) {
                if !tail.trim().is_empty() {
                    eprintln!("--- coolify-stub log ({REMOTE_LOG}) ---\n{tail}\n--- end log ---");
                }
            }
        }
    }
    let _guard = StubGuard {
        ssh_key: &cfg.ssh_key,
        host: &host,
    };

    // 4. Wait for /api/health to flip green (scheduler behind it).
    step("4/7  poll /api/health from inside the VM");
    let health_url = format!("http://127.0.0.1:{STUB_PORT}/api/health");
    let healthy = wait_for(
        || match ssh_http(&cfg.ssh_key, &host, "GET", &health_url, None) {
            Ok((200, body)) => body.contains("\"ok\":true"),
            _ => false,
        },
        Duration::from_secs(30),
    );
    assert!(healthy, "stub /api/health never returned 200 ok");
    ok("stub /api/health → 200 ok");

    // 5. list_containers path works — proves UDS plumbing + JSON envelope
    //    mirroring on both ends.
    step("5/7  POST /api/hosts/<wg_ip>/containers (list_containers)");
    let containers_url = format!("http://127.0.0.1:{STUB_PORT}/api/hosts/{wg_ip}/containers");
    let (code, body) = ssh_http(&cfg.ssh_key, &host, "POST", &containers_url, Some("{}"))
        .expect("list_containers");
    assert_eq!(code, 200, "list_containers status = {code}, body={body}");
    let v = parse_body(&body);
    assert!(v.get("containers").is_some(), "no containers field in {body}");
    ok(&format!(
        "list_containers returned ok envelope ({} container(s))",
        v["containers"].as_array().map(|a| a.len()).unwrap_or(0)
    ));

    // 6. Dispatch a real static build through the stub and poll for success.
    step("6/7  POST /api/builds (static_build) → poll /api/builds/:id");
    let builds_url = format!("http://127.0.0.1:{STUB_PORT}/api/builds");
    let dispatch_body = format!(
        r#"{{"host_id":"{wg_ip}","repo_url":"{SMALL_REPO}","git_ref":"main","target_image":"localhost/coolify-stub-e2e:v1","output_dir":"."}}"#,
    );
    let (code, body) =
        ssh_http(&cfg.ssh_key, &host, "POST", &builds_url, Some(&dispatch_body)).expect("dispatch");
    assert_eq!(code, 202, "dispatch status = {code}, body={body}");
    let ack = parse_body(&body);
    let request_id = ack["request_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no request_id in dispatch ack: {body}"))
        .to_string();
    ok(&format!("build dispatched request_id={request_id}"));

    let result_url = format!("http://127.0.0.1:{STUB_PORT}/api/builds/{request_id}?timeout_ms=10000");
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    let mut last_body = String::new();
    let mut success = false;
    while std::time::Instant::now() < deadline {
        match ssh_http(&cfg.ssh_key, &host, "GET", &result_url, None) {
            Ok((200, body)) => {
                let v = parse_body(&body);
                last_body = body.clone();
                match v["status"].as_str() {
                    Some("ok") => {
                        assert!(
                            v["digest"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
                            "ok envelope but empty digest: {body}"
                        );
                        success = true;
                        break;
                    }
                    Some("error") => panic!("build errored: {body}"),
                    _ => { /* still pending — rare, keep polling */ }
                }
            }
            Ok((408, _)) | Ok((404, _)) => { /* long-poll timeout / race, retry */ }
            Ok((code, body)) => {
                last_body = body.clone();
                panic!("unexpected status {code} polling build: {body}");
            }
            Err(e) => panic!("ssh_http poll: {e}"),
        }
    }
    assert!(
        success,
        "build {request_id} did not settle in 240s; last body: {last_body}"
    );
    ok(&format!("build {request_id} settled ok"));

    // 7. Sanity: after a successful build the builder unit is cleaned up.
    step("7/7  verify builder systemd unit exited");
    let cmd = format!(
        "systemctl is-active coolify-build-{request_id}.service 2>&1 || true"
    );
    let state = ssh(&cfg.ssh_key, &host, &cmd).unwrap_or_default();
    assert!(
        !state.trim().eq("active"),
        "coolify-build-{request_id}.service still active after success"
    );
    ok(&format!("coolify-build-{request_id}.service not active (state={})", state.trim()));

    e2e_tests::log_line("═══ stub_smoke PASS ═══");

    if std::env::var("E2E_KEEP_VMS").as_deref() == Ok("1") {
        e2e_tests::log_line("");
        e2e_tests::log_line(&format!("E2E_KEEP_VMS=1 — stub left running on {host}"));
        e2e_tests::log_line(&format!(
            "Direct URL:        http://{host}:{STUB_PORT}"
        ));
        e2e_tests::log_line("SSH port-forward:");
        e2e_tests::log_line(&format!(
            "  ssh -i {key} -N -L {STUB_PORT}:127.0.0.1:{STUB_PORT} root@{host}",
            key = cfg.ssh_key
        ));
        e2e_tests::log_line(&format!(
            "  → open http://localhost:{STUB_PORT}"
        ));
        e2e_tests::log_line(&format!(
            "Tail stub log:     ssh -i {key} root@{host} 'tail -f {REMOTE_LOG}'",
            key = cfg.ssh_key
        ));
        e2e_tests::log_line(&format!(
            "Kill stub:         ssh -i {key} root@{host} pkill -f coolify-stub",
            key = cfg.ssh_key
        ));
        e2e_tests::log_line("Destroy VM later:  CONFIRM_SWEEP=1 cargo test -p e2e-tests --test install cleanup_leaked_hetzner -- --ignored --nocapture");
    }
    // StubGuard kills the stub (unless E2E_KEEP_VMS=1); EphemeralCluster::drop
    // tears down the VM (unless E2E_KEEP_VMS=1).
}
