//! Install + networking e2e. Provisions Hetzner VMs, runs `coolify init
//! bootstrap`, asserts wg0 / podman bridge / firewall default-deny,
//! then destroys VMs (RAII via [`EphemeralCluster`]).
//!
//! All `#[ignore]` — run with:
//!
//! ```text
//! HETZNER_TOKEN=... HETZNER_PROJECT=... \
//! SSH_KEY=~/.ssh/test \
//! COOLIFY_CLI_BIN=$(which coolify) \
//! cargo test -p e2e-tests --test install -- --ignored --nocapture --test-threads=1
//! ```

use std::time::Duration;

use e2e_tests::hetzner::EphemeralCluster;
use e2e_tests::install::{
    local_coolify, podman_ping, podman_pull, run_container, ssh, ssh_ping, unit_active, wait_for,
    wg0_ip, wg_peers_handshaken, InstallEnv, NET, TEST_IMAGE,
};

const MGMT_POOL: &str = "100.64.0.0/16";
const CONTAINER_POOL: &str = "10.210.0.0/16";

// Systemd units that must be `active` on every coold host post-install.
const CORE_UNITS: &[&str] = &["coold", "corrosion", "coolify-mesh-fw", "wg-quick@wg0"];

// Central-only units. Flux runs the UDS command bus — Redis is no
// longer installed by the CLI.
const CENTRAL_UNITS: &[&str] = &["flux"];

fn assert_central_units(ssh_key: &str, host: &str) {
    for unit in CENTRAL_UNITS {
        assert!(
            unit_active(ssh_key, host, unit),
            "central unit {unit} not active on {host}"
        );
        ok(&format!("{host}: systemd unit {unit} active"));
    }
    assert_flux_socket(ssh_key, host);
    assert_core_file_perms(ssh_key, host);
}

/// Verify the flux UDS is present, is a socket, has the expected
/// filesystem perms (0600 when no group is configured, 0660 when one is),
/// and is live on `/v1/health`.
fn assert_flux_socket(ssh_key: &str, host: &str) {
    const SOCK: &str = "/run/coolify/flux.sock";

    // 1. File type: socket.
    let stat_type = ssh(ssh_key, host, &format!("stat -c %F {SOCK}"))
        .unwrap_or_else(|e| panic!("stat -c %F {SOCK} on {host}: {e}"));
    let stat_type = stat_type.trim();
    assert_eq!(
        stat_type, "socket",
        "{SOCK} on {host} is {stat_type:?}, expected 'socket'"
    );
    ok(&format!("{host}: {SOCK} is a unix socket"));

    // 2. Perms: 0600 (dev default) or 0660 (group configured). Owner must
    //    be root under the default systemd unit. When mode is 660 the
    //    group must be non-empty and distinct from root (group is the
    //    point of the relaxed mode).
    let stat_mode = ssh(ssh_key, host, &format!("stat -c '%a %U %G' {SOCK}"))
        .unwrap_or_else(|e| panic!("stat -c mode {SOCK} on {host}: {e}"));
    let stat_mode = stat_mode.trim();
    let mut parts = stat_mode.split_whitespace();
    let mode = parts.next().unwrap_or("");
    let owner = parts.next().unwrap_or("");
    let group = parts.next().unwrap_or("");
    assert!(
        mode == "600" || mode == "660",
        "{SOCK} on {host} mode={mode}, expected 600 or 660 ({stat_mode})"
    );
    assert_eq!(
        owner, "root",
        "{SOCK} on {host} owner={owner}, expected root ({stat_mode})"
    );
    if mode == "660" {
        assert!(
            !group.is_empty() && group != "root",
            "{SOCK} on {host} mode=660 but group={group:?} (expected a non-root group)"
        );
    }
    ok(&format!(
        "{host}: {SOCK} mode={mode} owner={owner} group={group}"
    ));

    // 3. Liveness: /v1/health via curl-over-UDS.
    let ping = ssh(
        ssh_key,
        host,
        &format!("curl -sS --unix-socket {SOCK} http://localhost/v1/health"),
    )
    .unwrap_or_default();
    assert!(
        ping.contains("\"ok\":true"),
        "flux UDS /v1/health did not return ok on {host}: {ping:?}"
    );
    ok(&format!("{host}: flux UDS /v1/health → ok"));
}

/// Verify the runtime dirs + systemd unit files coold relies on exist,
/// are the right object type, and have the expected mode + owner. Called
/// on every host that runs flux (central). Covers the flux parent
/// dir, the builder work tree, and both systemd unit files.
fn assert_core_file_perms(ssh_key: &str, host: &str) {
    // (path, expected `stat -c %F` kind, allowed mode strings, expected owner)
    let specs: &[(&str, &str, &[&str], &str)] = &[
        ("/run/coolify", "directory", &["700", "750", "755"], "root"),
        (
            "/var/lib/coolify-builder",
            "directory",
            &["700", "750", "755"],
            "root",
        ),
        (
            "/var/lib/coolify-builder/work",
            "directory",
            &["700"],
            "root",
        ),
        (
            "/etc/systemd/system/coold.service",
            "regular file",
            &["644"],
            "root",
        ),
        (
            "/etc/systemd/system/flux.service",
            "regular file",
            &["644"],
            "root",
        ),
    ];

    for (path, want_kind, allowed_modes, want_owner) in specs {
        let out = ssh(ssh_key, host, &format!("stat -c '%F|%a|%U' {path}"))
            .unwrap_or_else(|e| panic!("stat {path} on {host}: {e}"));
        let line = out.trim();
        let mut parts = line.split('|');
        let kind = parts.next().unwrap_or("");
        let mode = parts.next().unwrap_or("");
        let owner = parts.next().unwrap_or("");
        assert_eq!(
            kind, *want_kind,
            "{path} on {host} kind={kind:?}, expected {want_kind:?} ({line})"
        );
        assert!(
            allowed_modes.contains(&mode),
            "{path} on {host} mode={mode}, expected one of {allowed_modes:?} ({line})"
        );
        assert_eq!(
            owner, *want_owner,
            "{path} on {host} owner={owner}, expected {want_owner} ({line})"
        );
        ok(&format!(
            "{host}: {path} kind={kind} mode={mode} owner={owner}"
        ));
    }
}

fn step(msg: &str) {
    e2e_tests::log_line(&format!("─── {msg} ───"));
}

fn ok(msg: &str) {
    e2e_tests::log_line(&format!("  ✓ {msg}"));
}

fn assert_default_deny_scaffold(ssh_key: &str, host: &str) {
    // iptables cross-host scaffold.
    let intra = ssh(ssh_key, host, "iptables -S COOLIFY-INTRA")
        .unwrap_or_else(|e| panic!("iptables -S COOLIFY-INTRA on {host}: {e}"));
    assert!(
        intra.contains("-j COOLIFY-ALLOW"),
        "COOLIFY-INTRA missing jump to COOLIFY-ALLOW on {host}:\n{intra}"
    );
    assert!(
        intra.contains("-j DROP"),
        "COOLIFY-INTRA missing terminal DROP on {host}:\n{intra}"
    );
    ok(&format!(
        "{host}: iptables COOLIFY-INTRA has ALLOW→DROP chain"
    ));

    // nft intra-host bridge scaffold.
    let nft = ssh(ssh_key, host, "nft list table bridge coolify_bridge")
        .unwrap_or_else(|e| panic!("nft list table bridge coolify_bridge on {host}: {e}"));
    assert!(
        nft.contains("chain coolify_intra") && nft.contains("chain coolify_allow"),
        "bridge scaffold missing chains on {host}:\n{nft}"
    );
    ok(&format!(
        "{host}: nft bridge coolify_bridge has coolify_intra + coolify_allow chains"
    ));
}

#[test]
#[ignore = "requires HETZNER_TOKEN; provisions + destroys a Hetzner VM"]
fn install_single_host() {
    e2e_tests::set_tag("single");
    let cfg = InstallEnv::from_env();
    let cluster = EphemeralCluster::provision(1, "one");
    let host = cluster.hosts()[0].ipv4.clone();

    // 1. Install full stack colocated on the single VM.
    step("1/8  coolify init bootstrap (install stack)");
    local_coolify(
        &cfg.coolify_bin,
        &[
            "init",
            "bootstrap",
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
            "--enable-builder",
            "--yes",
        ],
    )
    .expect("coolify init bootstrap");
    ok("init bootstrap returned success");

    // 2. wg0 up with expected pool prefix.
    step("2/6  verify wg0 address in mgmt pool");
    let mgmt = wg0_ip(&cfg.ssh_key, &host);
    assert!(
        mgmt.starts_with("100.64."),
        "unexpected wg0 IP {mgmt} (expected pool {MGMT_POOL})"
    );
    ok(&format!("{host}: wg0 = {mgmt} (in {MGMT_POOL})"));

    // 3. All units active (core + central on the same box).
    step("3/8  verify systemd units active");
    for unit in CORE_UNITS {
        assert!(
            unit_active(&cfg.ssh_key, &host, unit),
            "systemd unit {unit} not active on {host}"
        );
        ok(&format!("{host}: systemd unit {unit} active"));
    }
    assert_central_units(&cfg.ssh_key, &host);

    // 4. Default-deny scaffold installed (both families).
    step("4/6  verify default-deny firewall scaffold");
    assert_default_deny_scaffold(&cfg.ssh_key, &host);

    // 5. Two containers on the same namespace bridge.
    step("5/6  start two alpine containers on coolify-default-mesh");
    podman_pull(&cfg.ssh_key, &host, TEST_IMAGE);
    let ip_a = run_container(&cfg.ssh_key, &host, "e2e-a", NET, TEST_IMAGE);
    let ip_b = run_container(&cfg.ssh_key, &host, "e2e-b", NET, TEST_IMAGE);
    ok(&format!("e2e-a={ip_a}  e2e-b={ip_b}"));

    // 6. Intra-host default-deny blocks (nft bridge coolify_intra).
    step("6/6  ping e2e-a → e2e-b should be BLOCKED (default-deny)");
    assert!(
        !podman_ping(&cfg.ssh_key, &host, "e2e-a", &ip_b),
        "intra-host default-deny failed to block e2e-a → {ip_b}"
    );
    ok(&format!("ping blocked as expected ({ip_a} → {ip_b})"));

    e2e_tests::log_line("═══ install_single_host PASS ═══");
    // VM torn down automatically via EphemeralCluster::drop.
}

#[test]
#[ignore = "requires HETZNER_TOKEN; provisions + destroys 2 Hetzner VMs"]
fn install_two_hosts() {
    e2e_tests::set_tag("two   ");
    let cfg = InstallEnv::from_env();
    let cluster = EphemeralCluster::provision(2, "two");
    let host_a = cluster.hosts()[0].ipv4.clone();
    let host_b = cluster.hosts()[1].ipv4.clone();

    // 1. Install: hostA = central + builder, hostB = coold-only.
    step("1/9  coolify init bootstrap (install 2-host stack; A=central+builder, B=coold-only)");
    local_coolify(
        &cfg.coolify_bin,
        &[
            "init",
            "bootstrap",
            "--nodes",
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
            "--yes",
        ],
    )
    .expect("coolify init bootstrap");
    ok("init bootstrap returned success");

    // 2. wg0 addresses on both.
    step("2/7  verify wg0 addresses on both hosts are distinct + in mgmt pool");
    let mgmt_a = wg0_ip(&cfg.ssh_key, &host_a);
    let mgmt_b = wg0_ip(&cfg.ssh_key, &host_b);
    assert!(mgmt_a.starts_with("100.64."), "hostA wg0 = {mgmt_a}");
    assert!(mgmt_b.starts_with("100.64."), "hostB wg0 = {mgmt_b}");
    assert_ne!(mgmt_a, mgmt_b, "both hosts got same wg0 IP");
    ok(&format!("hostA wg0={mgmt_a}  hostB wg0={mgmt_b}"));

    // 3. Peer handshake on both sides. wg-quick starts eagerly but a
    //    handshake requires at least one packet exchange — allow ~60s.
    step("3/9  wait for WireGuard peer handshake (up to 60s)");
    assert!(
        wait_for(
            || wg_peers_handshaken(&cfg.ssh_key, &host_a),
            Duration::from_secs(60),
        ),
        "hostA has no wg0 handshake with hostB"
    );
    ok("hostA has non-zero latest-handshake timestamp");
    assert!(
        wait_for(
            || wg_peers_handshaken(&cfg.ssh_key, &host_b),
            Duration::from_secs(60),
        ),
        "hostB has no wg0 handshake with hostA"
    );
    ok("hostB has non-zero latest-handshake timestamp");

    // 4. Mgmt-to-mgmt ping both directions (wg0 up and routing correct).
    step("4/7  mgmt-to-mgmt ping over wg0, both directions");
    assert!(
        ssh_ping(&cfg.ssh_key, &host_a, &mgmt_b),
        "hostA cannot ping hostB mgmt {mgmt_b}"
    );
    ok(&format!("hostA → {mgmt_b} OK"));
    assert!(
        ssh_ping(&cfg.ssh_key, &host_b, &mgmt_a),
        "hostB cannot ping hostA mgmt {mgmt_a}"
    );
    ok(&format!("hostB → {mgmt_a} OK"));

    // 5. Core coold stack on both, central + builder only on hostA.
    step("5/7  verify systemd units + firewall scaffold on both hosts");
    for h in &[&host_a, &host_b] {
        for unit in CORE_UNITS {
            assert!(
                unit_active(&cfg.ssh_key, h, unit),
                "systemd unit {unit} not active on {h}"
            );
            ok(&format!("{h}: systemd unit {unit} active"));
        }
        assert_default_deny_scaffold(&cfg.ssh_key, h);
    }
    assert_central_units(&cfg.ssh_key, &host_a);

    // 6. Cross-host containers on the shared default namespace.
    step("6/9  start alpine container on each host (same namespace bridge)");
    podman_pull(&cfg.ssh_key, &host_a, TEST_IMAGE);
    podman_pull(&cfg.ssh_key, &host_b, TEST_IMAGE);
    let ip_a = run_container(&cfg.ssh_key, &host_a, "e2e-a", NET, TEST_IMAGE);
    let ip_b = run_container(&cfg.ssh_key, &host_b, "e2e-b", NET, TEST_IMAGE);
    assert_ne!(
        ip_a, ip_b,
        "both containers got same IP — subnet allocator mis-split the pool"
    );
    ok(&format!("hostA/e2e-a={ip_a}  hostB/e2e-b={ip_b}"));

    // 7. Cross-host default-deny blocks (COOLIFY-INTRA on both hosts drops
    //    the forward path).
    step("7/7  cross-host ping e2e-a → e2e-b should be BLOCKED (COOLIFY-INTRA)");
    assert!(
        !podman_ping(&cfg.ssh_key, &host_a, "e2e-a", &ip_b),
        "cross-host default-deny failed to block e2e-a → {ip_b}"
    );
    ok(&format!("ping blocked as expected ({ip_a} → {ip_b})"));

    e2e_tests::log_line("═══ install_two_hosts PASS ═══");
    // Both VMs torn down automatically via EphemeralCluster::drop.
}

/// Safety-net sweeper for interrupted runs. Lists every resource labeled
/// `coolify-e2e=1` in the configured project and deletes it. Run on demand
/// when the Hetzner project accumulates leaked VMs after a ctrl-c / crash.
#[test]
#[ignore = "destructive: deletes ALL Hetzner servers + ssh_keys labeled coolify-e2e=1; also requires CONFIRM_SWEEP=1"]
fn cleanup_leaked_hetzner() {
    // Extra env gate so `cargo test --test install -- --ignored` (which runs
    // every `#[ignore]`d test, including this one, in parallel with the
    // install_* tests) does not race the sweeper against live provisioning.
    // Must opt-in explicitly via CONFIRM_SWEEP=1.
    if std::env::var("CONFIRM_SWEEP").as_deref() != Ok("1") {
        eprintln!(
            "cleanup_leaked_hetzner skipped: set CONFIRM_SWEEP=1 to actually delete resources"
        );
        return;
    }
    use e2e_tests::hetzner::HetznerClient;
    let c = HetznerClient::from_env();

    let servers = c.list_labeled("servers").expect("list servers");
    for s in &servers {
        let id = s["id"].as_u64().unwrap_or(0);
        let name = s["name"].as_str().unwrap_or("?");
        if id == 0 {
            continue;
        }
        match c.delete_server(id) {
            Ok(()) => eprintln!("deleted leaked server {id} ({name})"),
            Err(e) => eprintln!("WARN delete server {id}: {e}"),
        }
    }

    // Servers first, then keys — Hetzner rejects key-delete while referenced.
    std::thread::sleep(Duration::from_secs(3));

    let keys = c.list_labeled("ssh_keys").expect("list ssh_keys");
    for k in &keys {
        let id = k["id"].as_u64().unwrap_or(0);
        let name = k["name"].as_str().unwrap_or("?");
        if id == 0 {
            continue;
        }
        match c.delete_key(id) {
            Ok(()) => eprintln!("deleted leaked key {id} ({name})"),
            Err(e) => eprintln!("WARN delete key {id}: {e}"),
        }
    }

    eprintln!(
        "[sweeper] processed {} servers, {} keys",
        servers.len(),
        keys.len()
    );
}
