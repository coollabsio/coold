//! Builder + scheduler lifecycle e2e. Provisions 2 Hetzner VMs
//! (A = central + builder, B = coold-only), runs `coolify init bootstrap`,
//! then exercises every scheduler dispatch path, cancel, restart-adoption,
//! and on-disk artifact permissions against the live cluster.
//!
//! Single bundled `#[test]` so one 2-VM cluster is reused across every
//! scenario. `#[ignore]` — run with:
//!
//! ```text
//! HETZNER_TOKEN=... HETZNER_PROJECT=... \
//! SSH_KEY=~/.ssh/test \
//! COOLIFY_BIN=$(which coolify) \
//! cargo test -p e2e-tests --test builder -- --ignored --nocapture --test-threads=1
//! ```

use std::time::Duration;

use e2e_tests::hetzner::EphemeralCluster;
use e2e_tests::install::{local_coolify, ssh, unit_active, wg0_ip, InstallEnv};
use e2e_tests::{
    build_envelope, log_line, set_tag, uniq_req_id, wait_until, DispatchResult, Env,
};

// Small static-site repo for happy-path fixtures.
const SMALL_REPO: &str = "https://github.com/mdn/beginner-html-site";

// Kernel repo used to keep a build in-flight for cancel / restart tests.
const SLOW_REPO: &str = "https://github.com/torvalds/linux";

const MGMT_POOL: &str = "100.64.0.0/16";
const CONTAINER_POOL: &str = "10.210.0.0/16";

#[test]
#[ignore = "requires HETZNER_TOKEN; provisions + destroys 2 Hetzner VMs"]
fn builder_lifecycle() {
    set_tag("build ");
    let cfg = InstallEnv::from_env();
    let cluster = EphemeralCluster::provision(2, "build");
    let host_a = cluster.hosts()[0].ipv4.clone();
    let host_b = cluster.hosts()[1].ipv4.clone();

    // 1. Install: A = central + builder, B = coold-only.
    step("1/9  coolify init bootstrap (A=central+builder, B=coold-only)");
    local_coolify(
        &cfg.coolify_bin,
        &[
            "init",
            "bootstrap",
            "--servers",
            &format!("{host_a},{host_b}"),
            "--central",
            &host_a,
            "--ssh-user",
            "root",
            "--ssh-key",
            &cfg.ssh_key,
            "--wg-mgmt-pool",
            MGMT_POOL,
            "--container-pool",
            CONTAINER_POOL,
            "--builder-hosts",
            &host_a,
            "--builder-cpu-quota",
            "150%",
            "--builder-memory-max",
            "1G",
            "--builder-timeout-secs",
            "900",
            "--yes",
        ],
    )
    .expect("coolify init bootstrap");
    ok("init bootstrap returned success");

    // 2. Wait for coold + scheduler to be active. The CLI returns once the
    //    systemd units are enabled, but the services may still be in
    //    `activating`; dispatch tests fail hard if scheduler isn't ready.
    step("2/9  wait systemd units active");
    assert!(
        wait_until(
            || unit_active(&cfg.ssh_key, &host_a, "scheduler"),
            Duration::from_secs(60),
        ),
        "scheduler never active on {host_a}"
    );
    ok(&format!("{host_a}: scheduler active"));
    assert!(
        wait_until(
            || unit_active(&cfg.ssh_key, &host_a, "coold"),
            Duration::from_secs(60),
        ),
        "coold never active on {host_a}"
    );
    ok(&format!("{host_a}: coold active"));
    assert!(
        wait_until(
            || unit_active(&cfg.ssh_key, &host_b, "coold"),
            Duration::from_secs(60),
        ),
        "coold never active on {host_b}"
    );
    ok(&format!("{host_b}: coold active"));

    // 3. Resolve wg0 mgmt IPs post-install. These are the `host_id`
    //    values the scheduler matches against.
    step("3/9  resolve wg0 addresses");
    let builder_mgmt = wg0_ip(&cfg.ssh_key, &host_a);
    let cool_only_mgmt = wg0_ip(&cfg.ssh_key, &host_b);
    ok(&format!("builder_mgmt={builder_mgmt}  cool_only_mgmt={cool_only_mgmt}"));

    let env = Env::from_cluster(&cluster, builder_mgmt, cool_only_mgmt);

    // 4-10. Cheap scenarios first so slow-repo flakes don't mask cheap
    //       coverage.
    step("4/10  scenario: builder env rendered from CLI flags");
    scenario_builder_env_rendered(&cfg, &host_a, &host_b);
    step("5/10  scenario: pin_to_builder_host");
    scenario_pin_to_builder(&env);
    step("6/10  scenario: pin_to_coold_only returns 503");
    scenario_pin_to_coold_only_503(&env);
    step("7/10  scenario: unknown host_id returns 503");
    scenario_unknown_host_503(&env);
    step("8/10  scenario: load balance picks builder");
    scenario_load_balance(&env);
    step("9/10  scenario: cancel + artifact perms");
    scenario_cancel_and_artifact_perms(&env);
    step("10/10 scenario: coold restart adopts in-flight build");
    scenario_restart_adopts(&env);

    log_line("═══ builder_lifecycle PASS ═══");
    // cluster drops → Hetzner teardown (RAII, survives panics).
}

fn step(msg: &str) {
    log_line(&format!("─── {msg} ───"));
}

fn ok(msg: &str) {
    log_line(&format!("  ✓ {msg}"));
}

/// Verify the coolify-cli `--builder-*` flags end up as Environment= lines
/// in the rendered coold.service on the builder host, and that the
/// coold-only host has none of them. Proves the CLI→systemd-unit wiring
/// round-trips through a real bootstrap.
fn scenario_builder_env_rendered(cfg: &InstallEnv, host_a: &str, host_b: &str) {
    let unit_a = ssh(
        &cfg.ssh_key,
        host_a,
        "cat /etc/systemd/system/coold.service",
    )
    .unwrap_or_else(|e| panic!("read coold.service on {host_a}: {e}"));

    for want in [
        "Environment=COOLD_BUILDER_ENABLED=true",
        "Environment=COOLD_BUILDER_CPU_QUOTA=150%",
        "Environment=COOLD_BUILDER_MEMORY_MAX=1G",
        "Environment=COOLD_BUILDER_TIMEOUT_SECS=900",
        "Environment=COOLD_BUILDER_CAPACITY=2",
    ] {
        assert!(
            unit_a.contains(want),
            "builder host {host_a} coold.service missing {want:?}:\n{unit_a}"
        );
    }
    ok(&format!(
        "{host_a}: coold.service carries cpu=150% mem=1G timeout=900"
    ));

    let unit_b = ssh(
        &cfg.ssh_key,
        host_b,
        "cat /etc/systemd/system/coold.service",
    )
    .unwrap_or_else(|e| panic!("read coold.service on {host_b}: {e}"));
    assert!(
        !unit_b.contains("COOLD_BUILDER_"),
        "coold-only host {host_b} unexpectedly has builder env:\n{unit_b}"
    );
    ok(&format!("{host_b}: coold.service has no builder env"));
}

fn scenario_pin_to_builder(e: &Env) {
    let req = uniq_req_id("e2e-pin-builder");
    let target = format!("localhost/{req}");

    let payload = build_envelope(&req, &e.builder_mgmt, SMALL_REPO, "main", &target, ".");
    let resp = e.dispatch_and_wait(&payload, &req, Duration::from_secs(180));
    assert_eq!(resp.status, "ok", "unexpected response: {resp:?}");
    assert!(resp.digest.starts_with("sha256:"), "digest={:?}", resp.digest);
    assert!(e.has_image(&e.builder_host, &req), "image missing on builder");
    assert!(
        !e.has_image(&e.cool_only_host, &req),
        "image leaked to coold-only host"
    );

    e.clean_image(&e.builder_host, &req);
}

fn scenario_pin_to_coold_only_503(e: &Env) {
    let req = uniq_req_id("e2e-pin-coold-only");
    let target = format!("localhost/{req}");

    let payload = build_envelope(&req, &e.cool_only_mgmt, SMALL_REPO, "main", &target, ".");
    let resp = match e.dispatch_build(&payload) {
        DispatchResult::Rejected(r) => r,
        DispatchResult::Accepted(a) => panic!("unexpected accept: {a:?}"),
    };
    assert_eq!(resp.status, "error");
    assert_eq!(resp.code, 503);
    assert!(
        resp.message.contains("host has no builder capability"),
        "unexpected message: {:?}",
        resp.message
    );
}

fn scenario_unknown_host_503(e: &Env) {
    let req = uniq_req_id("e2e-unknown-host");
    let target = format!("localhost/{req}");

    let payload = build_envelope(&req, "100.64.99.99", SMALL_REPO, "main", &target, ".");
    let resp = match e.dispatch_build(&payload) {
        DispatchResult::Rejected(r) => r,
        DispatchResult::Accepted(a) => panic!("unexpected accept: {a:?}"),
    };
    assert_eq!(resp.status, "error");
    assert_eq!(resp.code, 503);
}

fn scenario_load_balance(e: &Env) {
    let req = uniq_req_id("e2e-lb");
    let target = format!("localhost/{req}");

    // host_id empty → scheduler picks.
    let payload = build_envelope(&req, "", SMALL_REPO, "main", &target, ".");
    let resp = e.dispatch_and_wait(&payload, &req, Duration::from_secs(180));
    assert_eq!(resp.status, "ok", "unexpected response: {resp:?}");
    assert!(
        e.has_image(&e.builder_host, &req),
        "load-balance failed to pick builder-capable host"
    );

    e.clean_image(&e.builder_host, &req);
}

fn scenario_cancel_and_artifact_perms(e: &Env) {
    let req = uniq_req_id("e2e-cancel");
    let target = format!("localhost/{req}");

    // Slow repo so the build is still cloning when we cancel.
    let payload = build_envelope(&req, &e.builder_mgmt, SLOW_REPO, "master", &target, ".");
    match e.dispatch_build(&payload) {
        DispatchResult::Accepted(_) => {}
        DispatchResult::Rejected(r) => panic!("dispatch rejected: {r:?}"),
    }

    // Wait for the transient unit to appear.
    assert!(
        wait_until(
            || e.unit_active(&e.builder_host, &req),
            Duration::from_secs(20),
        ),
        "transient unit never activated"
    );

    // In-flight build artifacts must exist with tight perms. coold writes
    // request.json before spawning the unit, so by the time the unit is
    // active the work dir + envelope are guaranteed present. events.ndjson
    // is opened by the builder process on startup; poll briefly to dodge
    // the race with the exec.
    assert_build_artifact_perms(e, &e.builder_host, &req);

    e.cancel_build(&req);

    let resp = e.wait_build_result(&req, Duration::from_secs(30));
    assert_eq!(resp.status, "error");
    assert_eq!(resp.code, 499);
    assert_eq!(resp.stage, "cancel", "unexpected stage: {:?}", resp.stage);

    assert!(
        wait_until(
            || !e.unit_active(&e.builder_host, &req),
            Duration::from_secs(10),
        ),
        "unit still active after cancel"
    );
}

fn scenario_restart_adopts(e: &Env) {
    let req = uniq_req_id("e2e-restart");
    let target = format!("localhost/{req}");

    let payload = build_envelope(&req, &e.builder_mgmt, SLOW_REPO, "master", &target, ".");
    match e.dispatch_build(&payload) {
        DispatchResult::Accepted(_) => {}
        DispatchResult::Rejected(r) => panic!("dispatch rejected: {r:?}"),
    }

    assert!(
        wait_until(
            || e.unit_active(&e.builder_host, &req),
            Duration::from_secs(20),
        ),
        "transient unit never activated"
    );

    // Restart coold. Transient unit lives in system.slice so must survive.
    // New coold's resume_or_reap should adopt it.
    e.restart_coold(&e.builder_host).expect("restart coold");
    std::thread::sleep(Duration::from_secs(3));

    assert!(
        e.unit_active(&e.builder_host, &req),
        "transient unit did not survive coold restart"
    );

    // Cancel so we don't block 30 min on kernel clone.
    e.cancel_build(&req);

    let resp = e.wait_build_result(&req, Duration::from_secs(60));
    assert_eq!(resp.status, "error");
    assert_eq!(resp.code, 499);

    assert!(
        wait_until(
            || !e.unit_active(&e.builder_host, &req),
            Duration::from_secs(10),
        ),
        "unit still active after adopted-cancel"
    );
    assert!(
        !e.work_dir_exists(&e.builder_host, &req),
        "workdir not cleaned"
    );
}

/// Assert the on-disk artifacts for an in-flight build have the expected
/// mode + owner. coold creates the work dir at `0o700` and writes
/// `request.json` at `0o600`; the builder opens `events.ndjson` at
/// `0o600`. Called while a build is running — the work tree is torn down
/// after the request resolves.
fn assert_build_artifact_perms(e: &Env, host: &str, req: &str) {
    let work_dir = format!("/var/lib/coolify-builder/work/{req}");
    let request_json = format!("{work_dir}/request.json");
    let events_ndjson = format!("{work_dir}/events.ndjson");

    let (kind, mode, owner) = e
        .stat_spec(host, &work_dir)
        .unwrap_or_else(|err| panic!("stat {work_dir}: {err}"));
    assert_eq!(kind, "directory", "{work_dir} kind={kind:?}");
    assert_eq!(mode, "700", "{work_dir} mode={mode}, expected 700");
    assert_eq!(owner, "root", "{work_dir} owner={owner}, expected root");

    let (kind, mode, owner) = e
        .stat_spec(host, &request_json)
        .unwrap_or_else(|err| panic!("stat {request_json}: {err}"));
    assert_eq!(kind, "regular file", "{request_json} kind={kind:?}");
    assert_eq!(mode, "600", "{request_json} mode={mode}, expected 600");
    assert_eq!(owner, "root", "{request_json} owner={owner}, expected root");

    // events.ndjson is opened by the builder after exec. Poll briefly so
    // the assertion doesn't race the spawn.
    assert!(
        wait_until(
            || e.stat_spec(host, &events_ndjson).is_ok(),
            Duration::from_secs(10),
        ),
        "{events_ndjson} never appeared"
    );
    let (kind, mode, owner) = e
        .stat_spec(host, &events_ndjson)
        .unwrap_or_else(|err| panic!("stat {events_ndjson}: {err}"));
    assert_eq!(kind, "regular file", "{events_ndjson} kind={kind:?}");
    assert_eq!(mode, "600", "{events_ndjson} mode={mode}, expected 600");
    assert_eq!(owner, "root", "{events_ndjson} owner={owner}, expected root");
}
