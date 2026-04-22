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

## Suite 1 — `builder.rs` (pre-installed cluster)

Requires an already-bootstrapped cluster. Env vars:

```bash
export BUILDER_HOST=<ssh-addr-of-builder-host>
export COOLD_ONLY_HOST=<ssh-addr-of-coold-only-host>
export BUILDER_MGMT=<wg0-ip-of-builder-host>
export COOLD_ONLY_MGMT=<wg0-ip-of-coold-only-host>
export CENTRAL_HOST=<ssh-addr-of-central>
export SSH_KEY=~/.ssh/<key>
# optional:
export SSH_USER=root
```

### Run all in suite

```bash
cargo test -p e2e-tests --test builder -- --ignored --nocapture --test-threads=1
```

### Individual tests

```bash
cargo test -p e2e-tests --test builder pin_to_builder_host             -- --ignored --nocapture
cargo test -p e2e-tests --test builder pin_to_coold_only_host_returns_503 -- --ignored --nocapture
cargo test -p e2e-tests --test builder unknown_host_id_returns_503     -- --ignored --nocapture
cargo test -p e2e-tests --test builder load_balance_picks_builder_host -- --ignored --nocapture
cargo test -p e2e-tests --test builder build_cancel_emits_stage_cancel -- --ignored --nocapture
cargo test -p e2e-tests --test builder coold_restart_adopts_in_flight_build -- --ignored --nocapture
```

## Suite 2 — `install.rs` (Hetzner-provisioned)

Provisions VMs via Hetzner Cloud API, runs `coolify init apply`, asserts networking, destroys VMs on drop. Env vars:

```bash
export HETZNER_TOKEN=<project-scoped-token>
export HETZNER_PROJECT=<label-value-for-cleanup-filter>
export SSH_KEY=~/.ssh/<key>              # privkey path; .pub derived or read
export COOLIFY_BIN=$(which coolify)      # local Go CLI path (default "coolify")
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
