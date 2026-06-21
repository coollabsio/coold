//! Ignored Hetzner e2e coverage for the Rust `coolify` v5 cluster CLI.
//!
//! These tests mirror the Go CLI's live provisioning coverage but invoke
//! `coolify` only. They are intentionally `#[ignore]`; do not run them in
//! normal CI/local verification because they create paid Hetzner VMs.
//!
//! Run manually with:
//!
//! ```text
//! HETZNER_TOKEN=... HETZNER_PROJECT=... SSH_KEY=~/.ssh/test \
//! COOLIFY_CLI_BIN=target/debug/coolify \
//! cargo test -p e2e-tests --test coolify -- --ignored --nocapture --test-threads=1
//! ```

use std::time::Duration;

use e2e_tests::hetzner::EphemeralCluster;
use e2e_tests::install::{
    local_coolify, podman_ping, podman_pull, run_container, ssh_ping, unit_active, wait_for,
    wg0_ip, wg_peers_handshaken, InstallEnv, NET, TEST_IMAGE,
};

const MGMT_POOL: &str = "100.64.0.0/16";
const CONTAINER_POOL: &str = "10.210.0.0/16";
const CORE_UNITS: &[&str] = &["coold", "corrosion", "coolify-mesh-fw", "wg-quick@wg0"];
const CENTRAL_UNITS: &[&str] = &["flux"];

fn step(msg: &str) {
    e2e_tests::log_line(&format!("─── {msg} ───"));
}

fn ok(msg: &str) {
    e2e_tests::log_line(&format!("  ✓ {msg}"));
}

fn bootstrap_args<'a>(servers: &'a str, central: &'a str, ssh_key: &'a str) -> Vec<&'a str> {
    vec![
        "init",
        "bootstrap",
        "--nodes",
        servers,
        "--central",
        central,
        "--ssh-user",
        "root",
        "--ssh-key",
        ssh_key,
        "--wg-mgmt-pool",
        MGMT_POOL,
        "--container-pool",
        CONTAINER_POOL,
        "--yes",
    ]
}

fn assert_core_units(ssh_key: &str, host: &str) {
    for unit in CORE_UNITS {
        assert!(
            unit_active(ssh_key, host, unit),
            "unit {unit} not active on {host}"
        );
        ok(&format!("{host}: {unit} active"));
    }
}

fn assert_central_units(ssh_key: &str, host: &str) {
    for unit in CENTRAL_UNITS {
        assert!(
            unit_active(ssh_key, host, unit),
            "central unit {unit} not active on {host}"
        );
        ok(&format!("{host}: {unit} active"));
    }
}

fn assert_default_deny_scaffold(ssh_key: &str, host: &str) {
    let intra = e2e_tests::install::ssh(ssh_key, host, "iptables -S COOLIFY-INTRA")
        .unwrap_or_else(|e| panic!("iptables COOLIFY-INTRA on {host}: {e}"));
    assert!(
        intra.contains("-j COOLIFY-ALLOW"),
        "missing COOLIFY-ALLOW jump on {host}: {intra}"
    );
    assert!(intra.contains("-j DROP"), "missing DROP on {host}: {intra}");
    let nft = e2e_tests::install::ssh(ssh_key, host, "nft list table bridge coolify_bridge")
        .unwrap_or_else(|e| panic!("nft coolify_bridge on {host}: {e}"));
    assert!(
        nft.contains("chain coolify_intra") && nft.contains("chain coolify_allow"),
        "missing nft chains on {host}: {nft}"
    );
    ok(&format!("{host}: default-deny scaffold present"));
}

#[test]
#[ignore = "requires HETZNER_TOKEN; provisions + destroys a Hetzner VM"]
fn coolify_bootstrap_single_host() {
    e2e_tests::set_tag("ctl-1 ");
    let cfg = InstallEnv::from_env();
    let cluster = EphemeralCluster::provision(1, "ctl-one");
    let host = cluster.hosts()[0].ipv4.clone();

    step("1/5 coolify init bootstrap single host");
    local_coolify(
        &cfg.coolify_bin,
        &bootstrap_args(&host, &host, &cfg.ssh_key),
    )
    .expect("coolify init bootstrap");

    step("2/5 verify wg0 mgmt IP");
    let mgmt = wg0_ip(&cfg.ssh_key, &host);
    assert!(mgmt.starts_with("100.64."), "unexpected wg0 IP {mgmt}");
    ok(&format!("{host}: wg0={mgmt}"));

    step("3/5 verify units");
    assert_core_units(&cfg.ssh_key, &host);
    assert_central_units(&cfg.ssh_key, &host);

    step("4/5 verify default-deny scaffold");
    assert_default_deny_scaffold(&cfg.ssh_key, &host);

    step("5/5 verify coolify init plan is converged");
    let out = local_coolify(
        &cfg.coolify_bin,
        &[
            "init",
            "plan",
            "--nodes",
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
        ],
    )
    .expect("coolify init plan");
    assert!(
        out.contains("No changes needed") || out.trim().is_empty(),
        "plan not converged: {out}"
    );
}

#[test]
#[ignore = "requires HETZNER_TOKEN; provisions + destroys 2 Hetzner VMs"]
fn coolify_bootstrap_two_hosts() {
    e2e_tests::set_tag("ctl-2 ");
    let cfg = InstallEnv::from_env();
    let cluster = EphemeralCluster::provision(2, "ctl-two");
    let host_a = cluster.hosts()[0].ipv4.clone();
    let host_b = cluster.hosts()[1].ipv4.clone();
    let servers = format!("{host_a},{host_b}");

    step("1/6 coolify init bootstrap two hosts");
    let mut args = bootstrap_args(&servers, &host_a, &cfg.ssh_key);
    args.extend(["--builder-hosts", &host_a]);
    local_coolify(&cfg.coolify_bin, &args).expect("coolify init bootstrap two hosts");

    step("2/6 verify wg0 IPs distinct");
    let mgmt_a = wg0_ip(&cfg.ssh_key, &host_a);
    let mgmt_b = wg0_ip(&cfg.ssh_key, &host_b);
    assert!(mgmt_a.starts_with("100.64."));
    assert!(mgmt_b.starts_with("100.64."));
    assert_ne!(mgmt_a, mgmt_b);

    step("3/6 wait for peer handshakes");
    assert!(wait_for(
        || wg_peers_handshaken(&cfg.ssh_key, &host_a),
        Duration::from_secs(60)
    ));
    assert!(wait_for(
        || wg_peers_handshaken(&cfg.ssh_key, &host_b),
        Duration::from_secs(60)
    ));

    step("4/6 verify mgmt ping both ways");
    assert!(ssh_ping(&cfg.ssh_key, &host_a, &mgmt_b));
    assert!(ssh_ping(&cfg.ssh_key, &host_b, &mgmt_a));

    step("5/6 verify units and scaffolds");
    for h in [&host_a, &host_b] {
        assert_core_units(&cfg.ssh_key, h);
        assert_default_deny_scaffold(&cfg.ssh_key, h);
    }
    assert_central_units(&cfg.ssh_key, &host_a);

    step("6/6 verify cross-host default deny blocks container ping");
    podman_pull(&cfg.ssh_key, &host_a, TEST_IMAGE);
    podman_pull(&cfg.ssh_key, &host_b, TEST_IMAGE);
    let ip_a = run_container(&cfg.ssh_key, &host_a, "ctl-a", NET, TEST_IMAGE);
    let ip_b = run_container(&cfg.ssh_key, &host_b, "ctl-b", NET, TEST_IMAGE);
    assert_ne!(ip_a, ip_b);
    assert!(!podman_ping(&cfg.ssh_key, &host_a, "ctl-a", &ip_b));
}

#[test]
#[ignore = "requires HETZNER_TOKEN; provisions + destroys 3 Hetzner VMs"]
fn coolify_extend_adds_third_host() {
    e2e_tests::set_tag("ctl-x ");
    let cfg = InstallEnv::from_env();
    let cluster = EphemeralCluster::provision(3, "ctl-ext");
    let host_a = cluster.hosts()[0].ipv4.clone();
    let host_b = cluster.hosts()[1].ipv4.clone();
    let host_c = cluster.hosts()[2].ipv4.clone();
    let initial = format!("{host_a},{host_b}");
    let full = format!("{host_a},{host_b},{host_c}");

    step("1/4 bootstrap initial two-host mesh");
    local_coolify(
        &cfg.coolify_bin,
        &bootstrap_args(&initial, &host_a, &cfg.ssh_key),
    )
    .expect("initial bootstrap");

    step("2/4 extend with third host");
    local_coolify(
        &cfg.coolify_bin,
        &[
            "init",
            "extend",
            "--nodes",
            &full,
            "--new-nodes",
            &host_c,
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
        ],
    )
    .expect("extend third host");

    step("3/4 verify all wg0 IPs and handshakes");
    let mgmt_a = wg0_ip(&cfg.ssh_key, &host_a);
    let mgmt_c = wg0_ip(&cfg.ssh_key, &host_c);
    assert!(mgmt_c.starts_with("100.64."));
    assert!(wait_for(
        || wg_peers_handshaken(&cfg.ssh_key, &host_c),
        Duration::from_secs(60)
    ));
    assert!(ssh_ping(&cfg.ssh_key, &host_c, &mgmt_a));

    step("4/4 verify third host services");
    assert_core_units(&cfg.ssh_key, &host_c);
    assert_default_deny_scaffold(&cfg.ssh_key, &host_c);
}
