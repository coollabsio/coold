//! Stub-dashboard smoke e2e. Provisions a single Hetzner VM, runs
//! `coolify init apply` (bringing up coold + broker + builder), scp's the
//! prebuilt `coolify-stub` binary onto the VM, launches it next to the
//! broker UDS, and drives a real static build through its `/api/*` surface.
//!
//! Requires an env var pointing at a prebuilt linux-x64 binary:
//!
//! ```text
//! cd coolify-stub
//! bun install && (cd web && bun install)
//! BUN_TARGET=bun-linux-x64 bun scripts/build-binary.ts
//! cd ..
//! COOLIFY_STUB_BIN=$PWD/coolify-stub/dist/coolify-stub \
//!   cargo test -p e2e-tests --test stub -- --ignored --nocapture --test-threads=1
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
    let p = std::env::var("COOLIFY_STUB_BIN")
        .expect("env COOLIFY_STUB_BIN required (path to prebuilt linux-x64 binary)");
    let meta = std::fs::metadata(&p).unwrap_or_else(|e| {
        panic!("COOLIFY_STUB_BIN={p} not readable: {e} — build via `bun scripts/build-binary.ts`")
    });
    assert!(meta.is_file(), "COOLIFY_STUB_BIN={p} is not a regular file");
    p
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

    // 1. Install stack (coold + broker + builder colocated on the VM).
    step("1/7  coolify init apply (install stack)");
    local_coolify(
        &cfg.coolify_bin,
        &[
            "init",
            "apply",
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
    .expect("coolify init apply");
    ok("init apply returned success");

    // 1b. Optional override: swap the nightly-provisioned builder/coold/broker
    //     binaries with a locally-built copy so in-flight fixes can be tested
    //     without a GitHub release round-trip. Restart the affected systemd
    //     units for coold/broker; builder has no daemon (coold spawns it per
    //     dispatch) so no restart needed.
    for (var_name, remote_path, unit) in [
        ("E2E_BUILDER_BIN", "/usr/local/bin/builder", None),
        ("E2E_COOLD_BIN", "/usr/local/bin/coold", Some("coold")),
        ("E2E_BROKER_BIN", "/usr/local/bin/broker", Some("broker")),
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

    // 2. Confirm broker came up before we pile the stub on top.
    step("2/7  wait for broker socket + /v1/health");
    assert!(
        wait_for(
            || unit_active(&cfg.ssh_key, &host, "broker"),
            Duration::from_secs(30),
        ),
        "broker unit never became active on {host}"
    );
    let ping = ssh(
        &cfg.ssh_key,
        &host,
        "curl -sS --unix-socket /run/coolify/broker.sock http://localhost/v1/health",
    )
    .unwrap_or_default();
    assert!(ping.contains("\"ok\":true"), "broker health: {ping:?}");
    ok("broker UDS /v1/health → ok");

    // 3. Upload the prebuilt stub binary and kick it off. We start it under
    //    `setsid` so closing the ssh session doesn't SIGHUP the child.
    step("3/7  scp + launch coolify-stub");
    scp_upload(&cfg.ssh_key, &host, &stub_bin, REMOTE_BIN).expect("scp stub binary");
    ssh(&cfg.ssh_key, &host, &format!("chmod +x {REMOTE_BIN}")).expect("chmod stub");
    let wg_ip = wg0_ip(&cfg.ssh_key, &host);
    let launch = format!(
        "rm -f {REMOTE_LOG}; \
         COOLIFY_HOSTS={wg_ip} BROKER_SOCKET_PATH=/run/coolify/broker.sock PORT={STUB_PORT} HOST=127.0.0.1 \
         setsid nohup {REMOTE_BIN} >{REMOTE_LOG} 2>&1 < /dev/null & \
         echo $!",
    );
    let pid = ssh(&cfg.ssh_key, &host, &launch)
        .unwrap_or_else(|e| panic!("launch stub on {host}: {e}"))
        .trim()
        .to_string();
    ok(&format!("stub launched pid={pid}"));

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

    // 4. Wait for /api/health to flip green (broker behind it).
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
        e2e_tests::log_line("Reach the dashboard via SSH port-forward (stub binds 127.0.0.1 + firewall default-deny):");
        e2e_tests::log_line(&format!(
            "  ssh -i {key} -N -L 3000:127.0.0.1:{STUB_PORT} root@{host}",
            key = cfg.ssh_key
        ));
        e2e_tests::log_line("Then open http://localhost:3000 in your browser.");
        e2e_tests::log_line(&format!("Tail stub log:     ssh -i {key} root@{host} 'tail -f {REMOTE_LOG}'", key = cfg.ssh_key));
        e2e_tests::log_line(&format!("Kill stub:         ssh -i {key} root@{host} pkill -f coolify-stub", key = cfg.ssh_key));
        e2e_tests::log_line("Destroy VM later:  CONFIRM_SWEEP=1 cargo test -p e2e-tests --test install cleanup_leaked_hetzner -- --ignored --nocapture");
    }
    // StubGuard kills the stub (unless E2E_KEEP_VMS=1); EphemeralCluster::drop
    // tears down the VM (unless E2E_KEEP_VMS=1).
}
