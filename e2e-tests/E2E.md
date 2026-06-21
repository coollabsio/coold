# E2E Tests — Run Book

All tests are `#[ignore]` and require live infrastructure. Always pass `--ignored --nocapture --test-threads=1`.

## `.env` support

Harness auto-loads `e2e-tests/.env` if present (shell-exported vars still win). Copy the template:

```bash
cp e2e-tests/.env.example e2e-tests/.env
$EDITOR e2e-tests/.env
```

`.env` is gitignored + dockerignored.

## Compile only

```bash
cargo test -p e2e-tests --no-run
```

## Deferred builder suite

The previous builder lifecycle suite is parked at `deferred/builder.rs` while
builder is not part of the active v5 Flux/coold API surface. Re-enable it only
after a new builder ADR/API lands.

## Suite 2 — `install.rs` (Hetzner-provisioned)

Provisions VMs via Hetzner Cloud API, runs `coolify init bootstrap`, asserts networking, destroys VMs on drop. Env vars:

```bash
export HETZNER_TOKEN=<project-scoped-token>
export HETZNER_PROJECT=<label-value-for-cleanup-filter>
export SSH_KEY=~/.ssh/<key>              # privkey path; .pub derived or read
export COOLIFY_CLI_BIN=$(which coolify)      # local Rust v5 CLI path (default "coolify")
# optional:
export HETZNER_LOCATION=nbg1
export HETZNER_IMAGE=ubuntu-24.04
export HETZNER_SERVER_TYPE=cx23
```

### Both in parallel (each test provisions its own VMs)

Name filter `install_` selects install_single_host + install_two_hosts and skips `cleanup_leaked_hetzner` (which would otherwise race the sweeper against live VMs).

```bash
cargo test -p e2e-tests --test install install_ -- --ignored --nocapture
```

### Single-host only

```bash
cargo test -p e2e-tests --test install install_single_host -- --ignored --nocapture
```

### Two-host only

```bash
cargo test -p e2e-tests --test install install_two_hosts -- --ignored --nocapture
```

### Keep VMs alive after a run

Set `E2E_KEEP_VMS=1` to skip `EphemeralCluster` teardown for live debugging.
Clean up leaked resources with the sweeper when you are done.

### Leaked-resource sweeper

Deletes every Hetzner server + ssh_key labeled `coolify-e2e=1` in the project. Use after a ctrl-c / crash left VMs behind. Extra `CONFIRM_SWEEP=1` gate — without it the test is a no-op, so it's safe to leave in the suite.

```bash
CONFIRM_SWEEP=1 cargo test -p e2e-tests --test install cleanup_leaked_hetzner -- --ignored --nocapture
```

## Manual spot-checks (post-install failure triage)

```bash
ssh root@<ip> wg show wg0
ssh root@<ip> iptables -S COOLIFY-INTRA
ssh root@<ip> nft list table bridge coolify_bridge
ssh root@<ipA> podman exec e2e-a ping -c1 <ipB>
```
