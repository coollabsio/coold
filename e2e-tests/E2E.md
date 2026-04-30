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

## Suite 1 — `builder.rs` (Hetzner-provisioned)

Provisions 2 VMs (A = central + builder, B = coold-only), runs
`coolify init bootstrap`, then executes every dispatch / cancel / restart
/ artifact-perm scenario on the shared cluster. VMs destroyed on drop.
Uses the same env vars as the install suite (see below):

```bash
cargo test -p e2e-tests --test builder builder_lifecycle -- --ignored --nocapture --test-threads=1
```

The whole suite is a single `#[test] fn builder_lifecycle` — there are
no longer individual scenario tests (running each separately would
provision its own cluster, which is wasteful).

## Suite 2 — `install.rs` (Hetzner-provisioned)

Provisions VMs via Hetzner Cloud API, runs `coolify init bootstrap`, asserts networking, destroys VMs on drop. Env vars:

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

## Suite 3 — `stub.rs` (coolify-stub dashboard smoke)

Provisions a single Hetzner VM, runs `coolify init bootstrap`, scp's the
`coolify-stub` Bun binary next to the scheduler, and drives a real static build
through the stub's `/api/*` surface.

### Run the suite (default — fetch from nightly release)

```bash
cargo test -p e2e-tests --test stub -- --ignored --nocapture --test-threads=1
```

The harness auto-downloads `coolify-stub-linux-amd64.tar.gz` from the
`nightly` release of `coollabsio/coold` into `target/coolify-stub-cache/`
on first run, then reuses it (re-downloading only after 10 min for the
`nightly` tag). Zero extra setup.

### Binary-selection knobs (first match wins)

| Env                       | Effect                                                                 |
|---------------------------|------------------------------------------------------------------------|
| `COOLIFY_STUB_BIN=<path>` | Use this prebuilt binary as-is.                                        |
| `COOLIFY_STUB_SOURCE=local` | Build locally: `coolify-stub/scripts/build-binary.ts` (needs `bun`). |
| `COOLIFY_STUB_TAG=<tag>`  | Pin a specific release (default `nightly`).                            |
| `COOLIFY_STUB_REPO=<o/r>` | Pull from a fork (default `coollabsio/coold`).                         |

### Build the binary locally (optional)

```bash
cd coolify-stub
bun install && (cd web && bun install)
BUN_TARGET=bun-linux-x64 bun scripts/build-binary.ts
```

Then either set `COOLIFY_STUB_BIN=$PWD/coolify-stub/dist/coolify-stub` or
run the suite with `COOLIFY_STUB_SOURCE=local`.

The test kills the stub process and tails its log on teardown, so a panic
still surfaces the stub's stderr for triage.

### Keep the VM + stub alive to poke the UI

Set `E2E_KEEP_VMS=1` to skip both the stub `pkill` and the Hetzner VM
teardown. The test prints an SSH port-forward command on success — open
`http://localhost:3000` after running it.

```bash
E2E_KEEP_VMS=1 \
COOLIFY_STUB_BIN=$PWD/coolify-stub/dist/coolify-stub \
  cargo test -p e2e-tests --test stub -- --ignored --nocapture --test-threads=1
```

`E2E_KEEP_VMS` works for every e2e suite, not just stub. Clean up leaked
resources with the sweeper when you're done:

```bash
CONFIRM_SWEEP=1 cargo test -p e2e-tests --test install cleanup_leaked_hetzner \
  -- --ignored --nocapture
```

## Manual spot-checks (post-install failure triage)

```bash
ssh root@<ip> wg show wg0
ssh root@<ip> iptables -S COOLIFY-INTRA
ssh root@<ip> nft list table bridge coolify_bridge
ssh root@<ipA> podman exec e2e-a ping -c1 <ipB>
```
