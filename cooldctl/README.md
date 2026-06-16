# cooldctl

`cooldctl` is the Rust CLI for **Coolify v5 cluster operations** that live with
`coold`: WireGuard mesh bootstrap, Podman mesh networks, coold/corrosion
installation, builder capability setup, and SSH-bounced firewall control.

It intentionally does **not** migrate Coolify v4 CLI features such as contexts,
projects, resources, deployments, private keys, or v4 API helpers. The binary is
named `cooldctl` for now so it cannot interfere with the existing v4 `coolify`
CLI.

## Scope

Included:

- `init plan` — inspect host state and print the actions needed to converge.
- `init bootstrap` — first-time v5 mesh install.
- `init extend` — add new nodes while only peer-refreshing existing mesh hosts.
- `init upgrade` — bump coold/corrosion/builder binaries without
  changing mesh topology.
- `firewall containers` — discover Podman containers attached to v5 mesh
  networks.
- `firewall list` / `allow` / `revoke` — mutate coold's wg0-local firewall REST
  API via SSH bounce.

Excluded:

- Coolify v4 API/client commands.
- Application/resource/deployment management.
- Host removal. `extend` exists; `remove-host` is still a future lifecycle flow.

## Build and verify

From the workspace root:

```bash
rtk cargo build -p cooldctl
rtk cargo test -p cooldctl
rtk cargo clippy -p cooldctl --all-targets -- -D warnings
```

Compile the ignored live e2e test target without provisioning anything:

```bash
rtk cargo test -p e2e-tests --test cooldctl --no-run
```

## Shared flags

Most commands need SSH access to every target node. Nodes may include an
SSH port with `host:port`, which is useful for local/dev VMs:

```bash
--nodes IP1,IP2          Comma-separated deployment node list.
--nodes 127.0.0.1:51572,127.0.0.1:51593
--ssh-key ~/.ssh/key      SSH private key.
--ssh-user root           Defaults to root.
--ssh-port 22             Default SSH port when a node has no :port suffix.
--concurrency 10          Parallel SSH fanout limit.
--ssh-timeout 30s         Supports ms, s, m, or plain seconds.
```

`--servers` remains a backwards-compatible alias for `--nodes` during the
rename. `--new-hosts` remains an alias for `--new-nodes`.

Mesh defaults:

```bash
--namespace default             Single namespace for firewall commands.
--namespaces default            Comma-separated namespaces for init commands.
--container-pool 10.210.0.0/16  Per-host Podman subnet pool.
--container-prefix 24           Per-host subnet prefix.
--wg-mgmt-pool 100.64.0.0/16    WireGuard management IP pool.
--wg-interface wg0              WireGuard interface.
--wg-listen-port 51820          WireGuard UDP port.
--wg-listen-port-overrides node=51821,node2=51822
--wg-endpoint-overrides node=host.lima.internal:51821,node2=host.lima.internal:51822
```

Output formats:

```bash
--format table   Human tables where available.
--format json    Compact JSON.
--format pretty  Pretty JSON.
```

## Bootstrap a v5 mesh

Two deployment nodes:

```bash
cooldctl init bootstrap \
  --nodes 203.0.113.11,203.0.113.12 \
  --builder-hosts 203.0.113.11 \
  --ssh-key ~/.ssh/coolify-v5 \
  --yes
```

Dev/Lima-style nodes with forwarded SSH ports and host-side WireGuard UDP
endpoints:

```bash
cooldctl init bootstrap \
  --nodes 127.0.0.1:51572,127.0.0.1:51593 \
  --wg-listen-port-overrides 127.0.0.1:51572=51821,127.0.0.1:51593=51822 \
  --wg-endpoint-overrides 127.0.0.1:51572=host.lima.internal:51821,127.0.0.1:51593=host.lima.internal:51822 \
  --ssh-key ~/.lima/_config/user \
  --yes
```

Useful version pins:

```bash
--coold-version vX.Y.Z
--corrosion-version vX.Y.Z
```

`nightly` is the default for bootstrap. `init upgrade` rejects `nightly`
unless `--allow-nightly` is passed because a moving target would reinstall on
every run.

Flux is not installed by `cooldctl`. It is installed by Coolify itself or by
the Coolify installation script.

## Plan before changing hosts

```bash
cooldctl init plan \
  --nodes 203.0.113.10,203.0.113.11 \
  --ssh-key ~/.ssh/coolify-v5
```

Preview another intent:

```bash
cooldctl init plan \
  --intent extend \
  --nodes 203.0.113.10,203.0.113.11,203.0.113.12 \
  --new-nodes 203.0.113.12 \
  --ssh-key ~/.ssh/coolify-v5
```

## Extend a mesh

`--nodes` must contain the full desired deployment node list. `--new-nodes` is
the subset that should receive first-time node installation. Existing hosts get only safe
peer-refresh actions unless `--allow-replace` is explicitly passed.

```bash
cooldctl init extend \
  --nodes 203.0.113.10,203.0.113.11,203.0.113.12 \
  --new-nodes 203.0.113.12 \
  --ssh-key ~/.ssh/coolify-v5
```

## Upgrade agents

```bash
cooldctl init upgrade \
  --nodes 203.0.113.10,203.0.113.11 \
  --coold-version v0.2.0 \
  --corrosion-version v0.2.0 \
  --ssh-key ~/.ssh/coolify-v5
```

Upgrade mode keeps version bumps and service-unit rewrites, but skips topology
changes. Use `init extend` for peer or node-list changes.

## Firewall commands

Firewall commands SSH to each target host, discover coold's wg0 management IP,
read `/etc/coolify/api-token` unless a token override is provided, then call
coold's local REST API.

Token and port overrides:

```bash
--coold-token TOKEN       Per-command bearer token override.
COOLIFY_COOLD_TOKEN=...   Environment bearer token override.
--coold-port 8443         Defaults to 8443.
```

List mesh containers:

```bash
cooldctl firewall containers \
  --nodes 203.0.113.10,203.0.113.11 \
  --namespace default \
  --ssh-key ~/.ssh/coolify-v5
```

List allow rules:

```bash
cooldctl firewall list \
  --nodes 203.0.113.10,203.0.113.11 \
  --namespace default \
  --ssh-key ~/.ssh/coolify-v5
```

Allow traffic from one container IP to another:

```bash
cooldctl firewall allow \
  --nodes 203.0.113.11 \
  --namespace default \
  --from 10.210.0.10 \
  --to 10.210.1.20 \
  --proto tcp \
  --port 80 \
  --ssh-key ~/.ssh/coolify-v5
```

Allow all protocols between two container IPs by omitting `--port`:

```bash
cooldctl firewall allow \
  --nodes 203.0.113.11 \
  --from 10.210.0.10 \
  --to 10.210.1.20 \
  --ssh-key ~/.ssh/coolify-v5
```

Revoke by ID:

```bash
cooldctl firewall revoke \
  --nodes 203.0.113.11 \
  --id abc123def456 \
  --ssh-key ~/.ssh/coolify-v5
```

Or revoke by the same tuple used for allow:

```bash
cooldctl firewall revoke \
  --nodes 203.0.113.11 \
  --from 10.210.0.10 \
  --to 10.210.1.20 \
  --proto tcp \
  --port 80 \
  --ssh-key ~/.ssh/coolify-v5
```

Rule IDs are `sha256("namespace|src|dst|proto|port")[:12]`, with an empty
namespace treated as `default`, matching coold and the retired Go v5 cluster CLI
surface.

## Hetzner e2e tests

The live e2e tests are ignored by default because they create paid Hetzner VMs.
Build the binary first:

```bash
rtk cargo build -p cooldctl
```

Run manually only when you want live provisioning:

```bash
HETZNER_TOKEN=... \
SSH_KEY=~/.ssh/coolify-v5 \
COOLDCTL_BIN=target/debug/cooldctl \
rtk cargo test -p e2e-tests --test cooldctl -- --ignored --nocapture --test-threads=1
```

Optional environment:

```bash
HETZNER_PROJECT=...
```

The ignored tests cover single-host bootstrap, two-host bootstrap, extend with a
third host, and firewall allow/list/revoke behavior.
