# coold

Per-host Coolify v5 agent. Runs on every node in the WireGuard mesh and does two jobs:

1. **Service discovery sync**: watches the local Podman socket and keeps the Corrosion-replicated `service_endpoints` table in sync with the set of containers attached to the `coolify-mesh` network on this host.
2. **Embedded cluster DNS**: binds UDP+TCP `:53` on the Podman bridge gateway IP and resolves `*.coolify.internal` from the replicated `service_endpoints` view, forwarding everything else upstream.

Scope is deliberately narrow. coold does **not** manage WireGuard, iptables/nft, the Podman bridge, TLS, or RBAC. Those are handled by `coolify init` at bootstrap or (for the runtime allow-list) a future control-plane component. coold also does not supervise `corrosion` — they run as independent systemd services.

## Architecture

```
┌────────────────────────────────────────────────────────┐
│ coold                                                  │
│                                                        │
│  ┌─────────────────┐   UDS     ┌────────────────────┐  │
│  │ podman watcher  │──────────▶│ /run/podman/...sock│  │
│  └─────────────────┘           └────────────────────┘  │
│                                                        │
│  ┌─────────────────┐   HTTP    ┌────────────────────┐  │
│  │ corrosion sync  │──────────▶│ corrosion agent    │  │
│  └─────────────────┘           │ 127.0.0.1:8080     │  │
│                                └────────────────────┘  │
│                                         │              │
│                                         │ SWIM gossip  │
│                                         ▼              │
│                                  other hosts           │
│                                                        │
│  ┌─────────────────┐   UDP+TCP :53 on bridge gateway   │
│  │ DNS server      │──────────────────────────────┐    │
│  │ (hickory)       │                              │    │
│  │                 │  in-zone  → CorrosionBackend │    │
│  │                 │  out-zone → upstream (1.1.1.1)    │
│  └─────────────────┘                              │    │
└────────────────────────────────────────────────────────┘
                                                    │
                                  containers on the bridge
                                  query 10.210.X.1:53
```

## What runs inside coold

| Task | File | Purpose |
| --- | --- | --- |
| `podman watcher` | `src/podman/events.rs` | streams lifecycle events from the Podman UDS |
| `event trigger` | `src/sync.rs` | debounces events into reconciles |
| `reconcile loop` | `src/sync.rs` | periodic full reconcile against Podman + Corrosion |
| `dns server` | `src/dns/server.rs` | hickory-server bound to bridge gateway `:53` |

All four are spawned in parallel by `sync::run`. If the bridge gateway IP is unset, the DNS task no-ops (useful for tests and agent-only deployments).

## Required Corrosion schema

coold does **not** create the schema — load this into each host's Corrosion `schemas/` directory. The `coolify init` bootstrap does this automatically.

```sql
CREATE TABLE service_endpoints (
    container_id    TEXT PRIMARY KEY NOT NULL,
    container_name  TEXT NOT NULL,
    host_mgmt_ip    TEXT NOT NULL,
    container_ip    TEXT NOT NULL,
    state           TEXT NOT NULL,   -- running | exited | stopped | restarting | paused | created | dead | configured | removing
    health          TEXT NOT NULL,   -- healthy | unhealthy | starting | unknown
    updated_at      INTEGER NOT NULL
);
```

Consumers (including coold's own DNS resolver) filter on `state = 'running' AND health IN ('healthy', 'unknown')`.

## Configuration

All flags also read from matching env vars.

| Flag | Env | Default | Meaning |
| --- | --- | --- | --- |
| `--host-mgmt-ip` | `COOLD_HOST_MGMT_IP` | **required** | wg0 mgmt IP for this host (e.g. `100.64.0.5`) |
| `--podman-socket` | `COOLD_PODMAN_SOCKET` | `/run/podman/podman.sock` | local Podman UDS path |
| `--corrosion-url` | `COOLD_CORROSION_URL` | `http://127.0.0.1:8080` | local Corrosion HTTP endpoint |
| `--mesh-network` | `COOLD_MESH_NETWORK` | `coolify-mesh` | Podman network to track |
| `--reconcile-interval` | `COOLD_RECONCILE_INTERVAL` | `2s` | periodic full reconcile cadence |
| `--bridge-gateway-ip` | `COOLD_BRIDGE_GATEWAY_IP` | unset | bridge gateway IP (e.g. `10.210.5.1`) to bind DNS on; when unset, DNS is skipped |
| `--dns-zone` | `COOLD_DNS_ZONE` | `coolify.internal` | authoritative zone |
| `--dns-upstream` | `COOLD_DNS_UPSTREAM` | `1.1.1.1:53` | upstream resolver for out-of-zone queries |
| `--log-level` | `COOLD_LOG_LEVEL` | `info` | `tracing_subscriber` env filter |

## Run

```bash
cargo run --release -- \
  --host-mgmt-ip 100.64.0.5 \
  --bridge-gateway-ip 10.210.5.1
```

Or via env:

```bash
export COOLD_HOST_MGMT_IP=100.64.0.5
export COOLD_BRIDGE_GATEWAY_IP=10.210.5.1
cargo run --release
```

## Systemd (suggested unit ordering)

```ini
# /etc/systemd/system/coold.service
[Unit]
Description=Coolify host agent
Requires=corrosion.service
After=corrosion.service network-online.target podman.socket

[Service]
Environment=COOLD_HOST_MGMT_IP=100.64.0.5
Environment=COOLD_BRIDGE_GATEWAY_IP=10.210.5.1
Environment=COOLD_DNS_ZONE=coolify.internal
ExecStart=/usr/local/bin/coold
AmbientCapabilities=CAP_NET_BIND_SERVICE
Restart=on-failure
RestartSec=2s

[Install]
WantedBy=multi-user.target
```

`CAP_NET_BIND_SERVICE` is needed because coold binds privileged port 53.

## Sync semantics

- **Ownership**: coold writes rows only where `host_mgmt_ip = $COOLD_HOST_MGMT_IP`. Never touches other hosts' rows.
- **Event-driven**: Podman `start`/`die`/`remove` events trigger an immediate reconcile.
- **Periodic**: every `--reconcile-interval`, a full reconcile runs to catch missed events.
- **Filter**: only containers attached to `coolify-mesh` are registered. Off-mesh containers are not routable across wg0 anyway.
- **State reporting**: every attached container is reported with its raw Podman status and healthcheck result, including stopped/exited ones — they remain in `inspect` until `podman rm`. Consumers filter on `state`/`health`.
- **Degraded mode**: if Corrosion is down, writes fail and are retried on the next reconcile tick. Podman polling and DNS continue to run.

## DNS semantics

- **Bind target**: `<bridge-gateway-ip>:53` UDP+TCP only. Never `0.0.0.0`, never `wg0`. This lets host DNS daemons (dnsmasq, pihole, unbound) coexist if they're bound to specific interfaces.
- **Port collision**: three layers of defense (see `CONTROL_PLANE.md §5`):
  1. Bootstrap creates the Podman network with `--disable-dns` so netavark/aardvark-dns never squats `:53`.
  2. coold binds only the bridge gateway IP, not a wildcard.
  3. A preflight probe attempts the bind before the handler is registered; on failure it surfaces an actionable error naming the likely cause (aardvark, host dnsmasq, etc.) and systemd's `Restart=on-failure` retries once the operator clears it.
- **Zone resolution**: `foo.coolify.internal` → `CorrosionBackend::lookup("foo")` → all A records with matching `container_name`. TTL 5s (convergence vs. chatter trade-off from `CONTROL_PLANE.md`).
- **Out-of-zone**: forwarded via `hickory-resolver` to `--dns-upstream`.
- **Records**: IPv4-only in v1. AAAA/other types on an in-zone name return NODATA.
- **Missing name**: NXDOMAIN.

## Not (yet) in scope

- REST API, TLS, RBAC.
- iptables/nft rule management (the `COOLIFY-ALLOW` chain installed by `coolify init --default-deny` is still empty at runtime).
- WireGuard peer management.
- AAAA / IPv6 records.

## Test

```bash
cargo test
```
