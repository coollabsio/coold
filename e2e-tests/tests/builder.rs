//! Live-server scenarios. All `#[ignore]` — run with:
//!
//! ```text
//! cargo test -p e2e-tests -- --ignored --nocapture
//! ```

use std::time::Duration;

use e2e_tests::{build_envelope, cancel_envelope, uniq_req_id, wait_until, Env};

// Small static-site repo for the happy-path fixture.
const SMALL_REPO: &str = "https://github.com/mdn/beginner-html-site";

// Kernel repo used to keep a build in-flight for cancel / restart tests.
const SLOW_REPO: &str = "https://github.com/torvalds/linux";

#[test]
#[ignore = "requires live cluster; set BUILDER_HOST/COOLD_ONLY_HOST/... and run with --ignored"]
fn pin_to_builder_host() {
    let e = Env::from_env();
    let req = uniq_req_id("e2e-pin-builder");
    let target = format!("localhost/{req}");

    let payload = build_envelope(&req, &e.builder_mgmt, SMALL_REPO, "main", &target, ".");
    e.redis_xadd(&payload).expect("XADD");

    let resp = e.wait_build_resp(&req, Duration::from_secs(180));
    assert_eq!(resp.status, "ok", "unexpected response: {resp:?}");
    assert!(resp.digest.starts_with("sha256:"), "digest={:?}", resp.digest);
    assert!(e.has_image(&e.builder_host, &req), "image missing on builder");
    assert!(
        !e.has_image(&e.cool_only_host, &req),
        "image leaked to coold-only host"
    );

    e.clean_image(&e.builder_host, &req);
}

#[test]
#[ignore = "requires live cluster"]
fn pin_to_coold_only_host_returns_503() {
    let e = Env::from_env();
    let req = uniq_req_id("e2e-pin-coold-only");
    let target = format!("localhost/{req}");

    let payload = build_envelope(&req, &e.cool_only_mgmt, SMALL_REPO, "main", &target, ".");
    e.redis_xadd(&payload).expect("XADD");

    let resp = e.wait_build_resp(&req, Duration::from_secs(30));
    assert_eq!(resp.status, "error");
    assert_eq!(resp.code, 503);
    assert!(
        resp.message.contains("host has no builder capability"),
        "unexpected message: {:?}",
        resp.message
    );
}

#[test]
#[ignore = "requires live cluster"]
fn unknown_host_id_returns_503() {
    let e = Env::from_env();
    let req = uniq_req_id("e2e-unknown-host");
    let target = format!("localhost/{req}");

    let payload = build_envelope(&req, "100.64.99.99", SMALL_REPO, "main", &target, ".");
    e.redis_xadd(&payload).expect("XADD");

    let resp = e.wait_build_resp(&req, Duration::from_secs(30));
    assert_eq!(resp.status, "error");
    assert_eq!(resp.code, 503);
}

#[test]
#[ignore = "requires live cluster"]
fn load_balance_picks_builder_host() {
    let e = Env::from_env();
    let req = uniq_req_id("e2e-lb");
    let target = format!("localhost/{req}");

    // host_id empty → broker picks
    let payload = build_envelope(&req, "", SMALL_REPO, "main", &target, ".");
    e.redis_xadd(&payload).expect("XADD");

    let resp = e.wait_build_resp(&req, Duration::from_secs(180));
    assert_eq!(resp.status, "ok", "unexpected response: {resp:?}");
    assert!(
        e.has_image(&e.builder_host, &req),
        "load-balance failed to pick builder-capable host"
    );

    e.clean_image(&e.builder_host, &req);
}

#[test]
#[ignore = "requires live cluster"]
fn build_cancel_emits_stage_cancel() {
    let e = Env::from_env();
    let req = uniq_req_id("e2e-cancel");
    let target = format!("localhost/{req}");

    // Slow repo so the build is still cloning when we cancel.
    let payload = build_envelope(&req, &e.builder_mgmt, SLOW_REPO, "master", &target, ".");
    e.redis_xadd(&payload).expect("XADD");

    // Wait for the transient unit to appear.
    assert!(
        wait_until(
            || e.unit_active(&e.builder_host, &req),
            Duration::from_secs(20)
        ),
        "transient unit never activated"
    );

    e.redis_xadd(&cancel_envelope(&req)).expect("cancel XADD");

    let resp = e.wait_build_resp(&req, Duration::from_secs(30));
    assert_eq!(resp.status, "error");
    assert_eq!(resp.code, 499);
    assert_eq!(resp.stage, "cancel", "unexpected stage: {:?}", resp.stage);

    // Unit should be gone.
    assert!(
        wait_until(
            || !e.unit_active(&e.builder_host, &req),
            Duration::from_secs(10)
        ),
        "unit still active after cancel"
    );
}

#[test]
#[ignore = "requires live cluster"]
fn coold_restart_adopts_in_flight_build() {
    let e = Env::from_env();
    let req = uniq_req_id("e2e-restart");
    let target = format!("localhost/{req}");

    let payload = build_envelope(&req, &e.builder_mgmt, SLOW_REPO, "master", &target, ".");
    e.redis_xadd(&payload).expect("XADD");

    assert!(
        wait_until(
            || e.unit_active(&e.builder_host, &req),
            Duration::from_secs(20)
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
    e.redis_xadd(&cancel_envelope(&req)).expect("cancel XADD");

    let resp = e.wait_build_resp(&req, Duration::from_secs(60));
    assert_eq!(resp.status, "error");
    assert_eq!(resp.code, 499);

    // Post-cancel cleanup: unit gone, workdir removed.
    assert!(
        wait_until(
            || !e.unit_active(&e.builder_host, &req),
            Duration::from_secs(10)
        ),
        "unit still active after adopted-cancel"
    );
    assert!(
        !e.work_dir_exists(&e.builder_host, &req),
        "workdir not cleaned"
    );
}
