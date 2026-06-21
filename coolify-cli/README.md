# coolify

`coolify` is the Rust CLI for **Coolify v5 cluster operations** that live with
`coold`: WireGuard mesh bootstrap, Podman mesh networks, coold/corrosion
installation, and builder capability setup.

It intentionally does **not** migrate Coolify v4 CLI features such as contexts,
projects, resources, deployments, private keys, or v4 API helpers. The shipped binary is named `coolify` because this is the user-facing
Coolify v5 CLI. The existing Go `coolify` CLI remains the v4/current API CLI
until migration/packaging decides which binary is installed for a given channel.

## Scope

Included:

- `init plan` — inspect host state and print the actions needed to converge.
- `init bootstrap` — first-time v5 mesh install.
- `init extend` — add new nodes while only peer-refreshing existing mesh hosts.
- `init upgrade` — bump coold/corrosion/builder binaries without
  changing mesh topology.

Excluded:

- Coolify v4 API/client commands.
- Application/resource/deployment management.
- Host removal. `extend` exists; `remove-host` is still a future lifecycle flow.

## Build and verify

From the workspace root:

```bash
rtk cargo build -p coolify-cli
rtk cargo test -p coolify-cli
rtk cargo clippy -p coolify-cli --all-targets -- -D warnings
```

Compile the ignored live e2e test target without provisioning anything:

```bash
rtk cargo test -p e2e-tests --test coolify --no-run
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
coolify init bootstrap \
  --nodes 203.0.113.11,203.0.113.12 \
  --builder-hosts 203.0.113.11 \
  --ssh-key ~/.ssh/coolify-v5 \
  --yes
```

Dev/Lima-style nodes with forwarded SSH ports and host-side WireGuard UDP
endpoints:

```bash
coolify init bootstrap \
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

Flux is not installed by `coolify`. It is installed by Coolify itself or by
the Coolify installation script.

## Plan before changing hosts

```bash
coolify init plan \
  --nodes 203.0.113.10,203.0.113.11 \
  --ssh-key ~/.ssh/coolify-v5
```

Preview another intent:

```bash
coolify init plan \
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
coolify init extend \
  --nodes 203.0.113.10,203.0.113.11,203.0.113.12 \
  --new-nodes 203.0.113.12 \
  --ssh-key ~/.ssh/coolify-v5
```

## Upgrade agents

```bash
coolify init upgrade \
  --nodes 203.0.113.10,203.0.113.11 \
  --coold-version v0.2.0 \
  --corrosion-version v0.2.0 \
  --ssh-key ~/.ssh/coolify-v5
```

Upgrade mode keeps version bumps and service-unit rewrites, but skips topology
changes. Use `init extend` for peer or node-list changes.

## Runtime mutations

The CLI currently owns cluster bootstrap/extend/upgrade only. Runtime mutations
should flow through Coolify → Flux → coold gRPC primitives, not through a local
host-local API. Firewall CLI commands were removed with that surface.
