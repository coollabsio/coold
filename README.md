# coold

Per-host Coolify v5 agent. Watches local Podman and keeps the shared Corrosion
table `service_endpoints` in sync with the real set of containers on the
`coolify-mesh` network for this host.

This is the *basics* scope: sync loop only. No REST API, TLS, RBAC,
firewall management, or embedded DNS yet (see plan).

## Architecture

```
┌───────────────────────┐
│ coold                 │
│  ┌─────────────────┐  │   UDS         ┌──────────────────────┐
│  │ podman watcher  │──┼──────────────▶│ /run/podman/...sock  │
│  └─────────────────┘  │               └──────────────────────┘
│  ┌─────────────────┐  │   HTTP/JSON   ┌──────────────────────┐
│  │ corrosion sync  │──┼──────────────▶│ corrosion agent      │
│  └─────────────────┘  │               │ localhost:8080       │
└───────────────────────┘               └──────────────────────┘
                                                 │ SWIM gossip
                                                 ▼
                                           other hosts' corrosion
```

`coold` and `corrosion` run as **separate** services on each host. `coold`
does not supervise `corrosion`.

## Required Corrosion schema

`coold` does not create the schema. Load this into the Corrosion agent's
schema file on every host:

```sql
CREATE TABLE service_endpoints (
    container_id    TEXT PRIMARY KEY NOT NULL,
    container_name  TEXT NOT NULL,
    host_mgmt_ip    TEXT NOT NULL,
    container_ip    TEXT NOT NULL,
    healthy         INTEGER NOT NULL DEFAULT 1,
    updated_at      INTEGER NOT NULL
);
```

## Configuration

All flags also read from matching env vars:

| Flag | Env | Default | Meaning |
| --- | --- | --- | --- |
| `--host-mgmt-ip` | `COOLD_HOST_MGMT_IP` | **required** | wg0 mgmt IP for this host (e.g. `100.64.0.5`) |
| `--podman-socket` | `COOLD_PODMAN_SOCKET` | `/run/podman/podman.sock` | local Podman UDS path |
| `--corrosion-url` | `COOLD_CORROSION_URL` | `http://127.0.0.1:8080` | local Corrosion HTTP endpoint |
| `--mesh-network` | `COOLD_MESH_NETWORK` | `coolify-mesh` | Podman network to track |
| `--reconcile-interval` | `COOLD_RECONCILE_INTERVAL` | `2s` | periodic full reconcile cadence |
| `--log-level` | `COOLD_LOG_LEVEL` | `info` | `tracing_subscriber` env filter |

## Run

```bash
cargo run --release -- --host-mgmt-ip 100.64.0.5
```

Or via env:

```bash
export COOLD_HOST_MGMT_IP=100.64.0.5
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
ExecStart=/usr/local/bin/coold
Restart=on-failure
RestartSec=2s

[Install]
WantedBy=multi-user.target
```

## Sync semantics

- **Ownership**: coold writes rows only where `host_mgmt_ip = $COOLD_HOST_MGMT_IP`.
  Never touches other hosts' rows.
- **Event-driven**: Podman `start`/`die`/`remove` events trigger an immediate reconcile.
- **Periodic**: every `--reconcile-interval`, run full reconcile to catch missed events.
- **Filter**: only containers attached to `coolify-mesh` are registered. Off-mesh
  containers are not routable across wg0 anyway.
- **Degraded mode**: if Corrosion is down, writes fail and are retried on the next
  reconcile tick. Podman polling continues.

## Test

```bash
cargo test
```
