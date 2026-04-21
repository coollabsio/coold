# coold

Per-host Coolify v5 agent. Runs on every node in the WireGuard mesh and does three jobs:

1. **Service discovery sync**: watches the local Podman socket and keeps the Corrosion-replicated `service_endpoints` table in sync with the set of containers attached to each managed `coolify-<ns>-mesh` network on this host.
2. **Embedded cluster DNS**: binds UDP+TCP `:53` on every namespace's bridge gateway IP and resolves `<container>.<namespace>.coolify.internal` from the replicated `service_endpoints` view (scoped per namespace), forwarding everything else upstream.
3. **Firewall REST API**: serves `/api/v1/firewall/allow` over the wg0 mgmt IP (TLS + bearer token). Dual-writes every rule to both the `COOLIFY-ALLOW` iptables chain (cross-host plane) and the `coolify_bridge::coolify_allow` nft bridge-family chain (intra-host same-bridge plane), snapshotting each to `/etc/coolify/allow.rules` and `/etc/coolify/allow.nft` so the firewall units can restore on boot. Tuples only — metadata (audit, RBAC, owners) lives in the central Coolify DB.

Scope is deliberately narrow. coold does **not** manage WireGuard, the Podman bridges, or the default-deny scaffold (iptables `COOLIFY-INTRA` + nft `coolify_bridge` table). Those are handled by `coolify init` at bootstrap. coold also does not supervise `corrosion` — they run as independent systemd services.

## Architecture

```
┌────────────────────────────────────────────────────────────┐
│ coold                                                      │
│                                                            │
│  ┌─────────────────┐   UDS     ┌────────────────────┐      │
│  │ podman watcher  │──────────▶│ /run/podman/...sock│      │
│  └─────────────────┘           └────────────────────┘      │
│                                                            │
│  ┌─────────────────┐   HTTP    ┌────────────────────┐      │
│  │ corrosion sync  │──────────▶│ corrosion agent    │      │
│  └─────────────────┘           │ 127.0.0.1:8080     │      │
│                                └────────────────────┘      │
│                                         │                  │
│                                         │ SWIM gossip      │
│                                         ▼                  │
│                                  other hosts               │
│                                                            │
│  ┌─────────────────┐   UDP+TCP :53 per namespace gateway   │
│  │ DNS servers     │──────────────────────────────┐        │
│  │ (hickory)       │  one task per namespace      │        │
│  │                 │  in-zone  → CorrosionBackend │        │
│  │                 │  out-zone → upstream (1.1.1.1)        │
│  └─────────────────┘                              │        │
│                                                            │
│  ┌─────────────────┐   HTTPS   ┌────────────────────┐      │
│  │ firewall API    │──────────▶│ wg0 mgmt IP:8443   │      │
│  │ (axum)          │           └────────────────────┘      │
│  │                 │  dual-plane writer:                   │
│  │                 │    iptables COOLIFY-ALLOW (cross-host)│
│  │                 │    nft coolify_bridge::coolify_allow  │
│  │                 │        (intra-host same-bridge)       │
│  │                 │  snapshots: allow.rules + allow.nft   │
│  └─────────────────┘                                       │
└────────────────────────────────────────────────────────────┘
                                                    │
                                  containers on each bridge
                                  query 10.210.<ns>.1:53
```

## What runs inside coold

| Task | File | Purpose |
| --- | --- | --- |
| `podman watcher` | `src/podman/events.rs` | streams lifecycle events from the Podman UDS |
| `event trigger` | `src/sync.rs` | debounces events into reconciles |
| `reconcile loop` | `src/sync.rs` | periodic full reconcile against Podman + Corrosion for every managed namespace |
| `dns servers` | `src/dns/server.rs` | one hickory-server task per namespace, bound to its bridge gateway `:53` |
| `firewall api` | `src/firewall/server.rs` | axum REST on wg0 mgmt IP; dual-writes iptables COOLIFY-ALLOW + nft coolify_bridge::coolify_allow, snapshots both |

All tasks are spawned in parallel by `sync::run`. Namespace entries with a zero gateway (`0.0.0.0`) skip their DNS task — useful for tests and agent-only deployments. If `api_bind` is unset, the firewall API task no-ops.

## Required Corrosion schema

coold does **not** create the schema — load this into each host's Corrosion `schemas/` directory. The `coolify init` bootstrap does this automatically.

```sql
CREATE TABLE service_endpoints (
    container_id    TEXT PRIMARY KEY NOT NULL,
    container_name  TEXT NOT NULL,
    namespace       TEXT NOT NULL,   -- "default", "alpha", ... (DNS-safe label)
    host_mgmt_ip    TEXT NOT NULL,
    container_ip    TEXT NOT NULL,
    state           TEXT NOT NULL,   -- running | exited | stopped | restarting | paused | created | dead | configured | removing
    health          TEXT NOT NULL,   -- healthy | unhealthy | starting | unknown
    updated_at      INTEGER NOT NULL
);
```

Consumers (including coold's own DNS resolver) filter on `state = 'running' AND health IN ('healthy', 'unknown') AND namespace = ?`. DNS records take the shape `<container>.<namespace>.<zone>`; bare `<container>.<zone>` is deliberately NXDOMAIN — callers must fully qualify.

## Configuration

All flags also read from matching env vars.

| Flag | Env | Default | Meaning |
| --- | --- | --- | --- |
| `--host-mgmt-ip` | `COOLD_HOST_MGMT_IP` | **required** | wg0 mgmt IP for this host (e.g. `100.64.0.5`) |
| `--podman-socket` | `COOLD_PODMAN_SOCKET` | `/run/podman/podman.sock` | local Podman UDS path |
| `--corrosion-url` | `COOLD_CORROSION_URL` | `http://127.0.0.1:8080` | local Corrosion HTTP endpoint |
| `--namespaces` | `COOLD_NAMESPACES` | `default:coolify-default-mesh:0.0.0.0` | Comma-separated `<name>:<network>:<gateway-ip>` triples, one per namespace this host participates in. `gateway_ip == 0.0.0.0` disables DNS for that namespace. |
| `--reconcile-interval` | `COOLD_RECONCILE_INTERVAL` | `2s` | periodic full reconcile cadence |
| `--dns-zone` | `COOLD_DNS_ZONE` | `coolify.internal` | authoritative zone. Records shape: `<container>.<namespace>.<zone>` |
| `--dns-upstream` | `COOLD_DNS_UPSTREAM` | `1.1.1.1:53` | upstream resolver for out-of-zone queries |
| `--api-bind` | `COOLD_API_BIND` | unset | bind addr for the firewall REST API (e.g. `100.64.0.5:8443`); unset disables the API |
| `--api-token-file` | `COOLD_API_TOKEN_FILE` | unset | path to bearer-token file; **required** when `--api-bind` is set |
| `--tls-cert` | `COOLD_TLS_CERT` | unset | PEM cert chain for the API; set together with `--tls-key` to enable HTTPS |
| `--tls-key` | `COOLD_TLS_KEY` | unset | PEM private key for the API |
| `--rules-path` | `COOLD_RULES_PATH` | `/etc/coolify/allow.rules` | on-disk snapshot of `COOLIFY-ALLOW` for reboot restore (iptables plane) |
| `--bridge-rules-path` | `COOLD_BRIDGE_RULES_PATH` | `/etc/coolify/allow.nft` | on-disk snapshot of `coolify_bridge::coolify_allow` (nft bridge plane) |
| `--chain-name` | `COOLD_CHAIN_NAME` | `COOLIFY-ALLOW` | iptables chain coold owns |
| `--log-level` | `COOLD_LOG_LEVEL` | `info` | `tracing_subscriber` env filter |

## Run

```bash
cargo run --release -- \
  --host-mgmt-ip 100.64.0.5 \
  --namespaces 'default:coolify-default-mesh:10.210.0.1,alpha:coolify-alpha-mesh:10.220.0.1'
```

Or via env:

```bash
export COOLD_HOST_MGMT_IP=100.64.0.5
export COOLD_NAMESPACES='default:coolify-default-mesh:10.210.0.1,alpha:coolify-alpha-mesh:10.220.0.1'
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
Environment=COOLD_NAMESPACES=default:coolify-default-mesh:10.210.0.1,alpha:coolify-alpha-mesh:10.220.0.1
Environment=COOLD_DNS_ZONE=coolify.internal
# Firewall REST API (optional; omit the block to disable the API)
Environment=COOLD_API_BIND=100.64.0.5:8443
Environment=COOLD_API_TOKEN_FILE=/etc/coolify/api.token
Environment=COOLD_TLS_CERT=/etc/coolify/api.crt
Environment=COOLD_TLS_KEY=/etc/coolify/api.key
ExecStart=/usr/local/bin/coold
AmbientCapabilities=CAP_NET_BIND_SERVICE
Restart=on-failure
RestartSec=2s

[Install]
WantedBy=multi-user.target
```

`CAP_NET_BIND_SERVICE` is needed because coold binds privileged port 53. The token file must be root-owned, mode `0600`; its contents are trimmed of leading/trailing whitespace.

## Sync semantics

- **Ownership**: coold writes rows only where `host_mgmt_ip = $COOLD_HOST_MGMT_IP`. Never touches other hosts' rows.
- **Event-driven**: Podman `start`/`die`/`remove` events trigger an immediate reconcile across every managed namespace.
- **Periodic**: every `--reconcile-interval`, a full reconcile runs to catch missed events.
- **Filter**: only containers attached to one of the managed `coolify-<ns>-mesh` networks are registered, stamped with that namespace. Off-mesh containers are not routable across wg0 anyway.
- **State reporting**: every attached container is reported with its raw Podman status and healthcheck result, including stopped/exited ones — they remain in `inspect` until `podman rm`. Consumers filter on `state`/`health`/`namespace`.
- **Degraded mode**: if Corrosion is down, writes fail and are retried on the next reconcile tick. Podman polling and DNS continue to run.
- **Fail-fast supervision**: all tasks run under one `tokio::select!` in `sync::run`. If any task returns or panics, `run` exits with an error so systemd's `Restart=on-failure` respawns the whole daemon rather than silently losing a worker (`src/sync.rs`).

## DNS semantics

- **Bind target**: one task per namespace, each bound to `<bridge-gateway-ip>:53` UDP+TCP only. Never `0.0.0.0`, never `wg0`. Namespaces with `gateway_ip == 0.0.0.0` skip DNS entirely (agent-only / test mode).
- **Port collision**: three layers of defense (see `CONTROL_PLANE.md §5`):
  1. Bootstrap creates each Podman network with `--disable-dns` so netavark/aardvark-dns never squats `:53`.
  2. coold binds only the per-namespace bridge gateway IP, not a wildcard.
  3. On bind failure each namespace task enters a self-healing retry loop (`src/dns/server.rs`). `EADDRNOTAVAIL` / `EADDRINUSE` / `NetworkUnreachable` are classified as transient and retried with exponential backoff (1s → 30s cap); the first failure logs the three likely causes (bridge torn down because nothing is attached to the mesh network, aardvark-dns squatting `:53`, host DNS daemon on `0.0.0.0`). Zone-parse or resolver-build errors are fatal and bubble up so systemd restarts the daemon.
- **Bridge churn**: netavark tears a Podman bridge down when the last container on that namespace's network detaches, so its gateway IP vanishes at runtime — not only at startup. The retry loop above is what lets coold survive this window without a sentinel container or boot-time hack. During the gap the only potential queriers are containers on that bridge, which are also gone.
- **Zone resolution**: `foo.default.coolify.internal` → `CorrosionBackend::lookup("foo", "default")` → all A records with matching `container_name` AND `namespace`. TTL 5s. Bare `foo.coolify.internal` is NXDOMAIN — callers must fully qualify with the namespace label.
- **Cross-namespace lookups are DNS-only**: coold answers `foo.alpha.coolify.internal` from any bridge that can reach it, but L3 reachability still depends on a firewall allow rule (namespaces are separate podman bridges).
- **Out-of-zone**: forwarded via `hickory-resolver` to `--dns-upstream`.
- **Records**: IPv4-only in v1. AAAA/other types on an in-zone name return NODATA.
- **Missing name**: NXDOMAIN.

## Firewall API

Endpoints (all under `/api/v1/firewall`, Bearer auth, bound to wg0 mgmt IP):

| Method | Path | Body | Purpose |
| --- | --- | --- | --- |
| `POST`   | `/allow`              | `{namespace, src, dst, proto?, port?}` | create-or-ensure one rule, returns `{id, ...}` |
| `GET`    | `/allow[?namespace=X]`| — | list kernel state as JSON array; optional `namespace` filter |
| `GET`    | `/allow/:id`          | — | one rule (404 when absent) |
| `DELETE` | `/allow/:id`          | — | revoke (idempotent, 204 even on missing) |
| `POST`   | `/allow/bulk`         | `{add:[...], remove:[id,...]}` | one kernel transaction per plane (`iptables-restore --noflush` + `nft -f`) |
| `POST`   | `/reconcile`          | — | flush both chains, reload from `/etc/coolify/allow.rules` + `/etc/coolify/allow.nft` |
| `GET`    | `/healthz`            | — | unauthenticated liveness probe, returns `ok` |

**Auth & TLS.** Every `/api/v1/firewall/*` handler requires `Authorization: Bearer <token>`; the token is compared in constant time (`src/firewall/api.rs`) to avoid timing oracles. The server refuses to start without `--api-token-file` — no anonymous-access codepath exists. When both `--tls-cert` and `--tls-key` are set, the API serves HTTPS via rustls; otherwise plain HTTP, intended only for dev on a trusted overlay. Bind the API to the wg0 mgmt IP (never `0.0.0.0`) so it is never reachable off the mesh.

**Dual-plane writer.** Every mutation writes both planes in the same handler:

- **iptables `COOLIFY-ALLOW`** (filter table, cross-host plane): inserted via `iptables` command for immediate effect, snapshotted to `/etc/coolify/allow.rules` in the `*filter` / `:COOLIFY-ALLOW -` / `-A ...` / `COMMIT` shape that `iptables-restore --noflush` expects. Restored on boot by `coolify-mesh-allow.service`.
- **nft `coolify_bridge::coolify_allow`** (bridge family, intra-host same-bridge plane): rules staged via `nft -f` fragment with a top-level `flush chain bridge coolify_bridge coolify_allow` so the chain is atomically replaced each write. Snapshot at `/etc/coolify/allow.nft` restored on boot by `coolify-mesh-fw.service`. When the scaffold table is missing (permissive-mode hosts) the bridge-plane write no-ops with a one-shot WARN — iptables plane still succeeds.

Both snapshots are written atomically (`.tmp` + rename).

Rule identity `id` is `sha256("namespace|src|dst|proto|port")[:12]` — same hash the Go `coolify firewall` CLI computes, so mixed writers share stable IDs. Empty-string namespace on the wire normalizes to `"default"` so legacy CLI clients keep working. Identical src/dst/proto/port tuples in different namespaces produce different IDs and are managed independently on both planes.

coold stores **tuples only**. Audit trail, RBAC, app/owner linkage, and rule intent belong in the central Coolify DB — which issues REST calls to coold(s) after its own commit. On coold restart, the kernel chain is the source of truth; central reconciles any drift via `POST /reconcile` or by replaying `POST /allow`.

## Not (yet) in scope

- WireGuard peer management.
- AAAA / IPv6 records.

## Test

```bash
cargo test
```
