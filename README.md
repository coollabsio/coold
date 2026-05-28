## What is coold?

Per-host agent of Coolify v5. Kubelet-analogue for a WireGuard mesh of Podman hosts. One `coold` process per node. Narrow by design: executes primitives locally, never reasons about apps, builds, or deploys.

coold is the only process on a host with access to the Podman socket, the iptables/nft kernel interface, and the Corrosion gossip layer.

---

## System topology

```
┌────────────────────────────────────────────────────────────┐
│ Laravel (Coolify brain — app model, scheduler, deploy ctrl)│
└──────────────────────────┬─────────────────────────────────┘
                           │ HTTP over /run/coolify/scheduler.sock
                           ▼
┌────────────────────────────────────────────────────────────┐
│ scheduler                                                  │
│  • gRPC :6443 (coold dials in; HTTP/2 bidi, JWT bearer)    │
│  • UDS /run/coolify/scheduler.sock (Laravel; fs-perm auth) │
│  • Streams map (host_id) + Pending map (request_id)        │
└──────────────────────────┬─────────────────────────────────┘
                           │ grpcs://scheduler:6443/v1/agent
                           ▼
┌────────────────────────────────────────────────────────────┐
│ coold (per host)                                           │
│  Podman proxy · Firewall dual-writer · DNS · Corrosion sync│
│  Advertises "builder" cap → spawns builder subprocess      │
└────────┬───────────────┬────────────────┬──────────────────┘
         │ UDS           │ HTTP           │ systemd-run --pipe --scope
         ▼               ▼                ▼
   podman.sock     corrosion agent   coolify-build-<request_id>.service
                        │                 │
                        │ SWIM gossip     │ builder binary
                        ▼                 ▼
                   other hosts       buildah → containers-storage
```

---

## Repo layout (Cargo workspace)

```
proto/          Shared Protobuf: Agent.Stream, Hello, ServerMsg, ClientMsg,
                Response, BuildRequest, CancelBuild, capabilities.
coold/          Per-host agent.
scheduler/      gRPC server coold dials + UDS lane for Laravel.
builder/        One-shot OCI build CLI, spawned by coold per build.
builder-core/   Reusable git + buildah pipeline (static_build.rs, …).
cooldctl/       Rust v5 cluster CLI: WireGuard/Podman/coold init + SSH-bounced firewall.
                Does not include v4 Coolify API/context/project commands.
core/  Pure Coolify v5 domain model: servers, clusters, builds, events.
storage/ SQLite storage traits/repositories + embedded migrations.
api/   Coolify API Axum server + embedded Coolify UI binary for the Coolify v5 UI.
coolify-ui/      React 19 + Vite + TanStack Router/Query + Tailwind/shadcn baseline.
e2e-tests/      Live-server harness (Hetzner-provisioned). Excluded from
                default workspace build.
```

---

## Coolify API + Coolify UI

`api` is the initial Rust API/UI shell for Coolify v5. It follows the
Rust + React single-binary architecture: Axum owns `/api/...`, serves an
embedded Vite React SPA for browser routes, and persists local state in SQLite
through `storage`.

Current API surface:

```http
GET /healthz
GET /api/v1/status
GET /api/v1/scheduler/streams
GET /api/v1/servers
GET /api/v1/servers/:id/live-status
GET /api/v1/servers/:id/containers
POST /api/v1/servers/sync-streams
GET /api/v1/clusters
GET /api/v1/events
GET /api/v1/builds
```

Run locally:

```bash
SKIP_UI=1 rtk cargo run -p api -- serve
```

Coolify UI development (one command starts scheduler, fake coold, api, and Vite):

```bash
bun run dev
```

Useful overrides:

```bash
COOLIFY_UI_PORT=5174 \
COOLIFY_API_BIND=127.0.0.1:3001 \
SCHEDULER_GRPC_BIND=127.0.0.1:6444 \
bun run dev
```

Then open Servers → Sync scheduler streams → open `host-local`.

The Coolify API/UI stack is intentionally separate from `coold`: `coold` remains the
per-host agent, `scheduler` remains the stream router, and `api` becomes
the central Coolify v5 API/UI binary. Live host reads use the flow
Coolify UI → Coolify API → scheduler UDS → coold outbound gRPC stream → Podman.

---

## cooldctl — v5 cluster CLI

`cooldctl` is the Rust CLI for Coolify v5 cluster operations that belong next
to coold. It intentionally excludes v4 Coolify API commands (contexts, projects,
resources, deployments, private keys, etc.) so it cannot interfere with the
existing v4 `coolify` CLI.

Current command surface:

```bash
cooldctl init plan --central CENTRAL --nodes NODE1,NODE2 --ssh-key KEY
cooldctl init bootstrap --central CENTRAL --nodes NODE1,NODE2 --ssh-key KEY --yes
cooldctl init extend --central CENTRAL --nodes NODE1,NODE2,NODE3 --new-nodes NODE3 --ssh-key KEY
cooldctl init upgrade --central CENTRAL --nodes NODE1,NODE2 --ssh-key KEY --coold-version vX.Y.Z --coolify-version latest

cooldctl firewall containers --nodes IP1,IP2 --ssh-key KEY
cooldctl firewall list --nodes IP1,IP2 --ssh-key KEY
cooldctl firewall allow --from 10.0.0.1 --to 10.0.0.2 --port 80 --nodes IP1 --ssh-key KEY
cooldctl firewall revoke --id <rule-id> --nodes IP1 --ssh-key KEY
```

The CLI shares the v5 mesh model: bootstrap over SSH, central Coolify UI/API
installation on `--central`, deployment nodes via `--nodes`, and day-to-day
firewall mutation through coold's wg0-local REST API via SSH bounce. The central
host joins WireGuard for private scheduler/coold streams but only runs
Podman/coold/Corrosion when also listed in `--nodes`.

---

## coold — three core jobs

### 1. Service-discovery sync

Watches Podman lifecycle events (`start` / `die` / `remove`) plus 2s periodic reconcile. Writes own host's rows to Corrosion `service_endpoints` table. Gossip replicates to peers. Retries on next tick if Corrosion down.

### 2. Embedded cluster DNS

One hickory-server task per namespace, bound to that bridge's gateway IP (e.g. `10.210.0.1:53`) — never `0.0.0.0`. Resolves `<container>.<namespace>.coolify.internal` from Corrosion, filtered `state='running' AND health IN ('healthy','unknown')`. Bare `<container>.coolify.internal` is intentional NXDOMAIN. Out-of-zone forwarded to upstream (`1.1.1.1:53`). Self-healing rebind with exponential backoff when netavark tears down a bridge. IPv4 only (AAAA → NODATA).

### 3. Firewall REST API — dual-plane writer

HTTPS on wg0 mgmt IP (e.g. `100.64.0.5:8443`), bearer-token auth. Every mutation writes two kernel planes atomically:

| Plane | Mechanism | Traffic path |
| --- | --- | --- |
| Cross-host | iptables `COOLIFY-ALLOW` (filter) | wg0 ↔ bridge |
| Intra-host same-bridge | nft `coolify_bridge::coolify_allow` (bridge family) | Same-bridge traffic bypassing FORWARD |

Snapshots: `/etc/coolify/allow.rules` + `/etc/coolify/allow.nft`. Restored on boot by `coolify-mesh-fw.service` + `coolify-mesh-allow.service`. Rule ID = `sha256("namespace|src|dst|proto|port")[:12]` — byte-compatible with `cooldctl firewall` and the retired Go v5 cluster CLI surface. Tuples only; audit / RBAC / owners live in Laravel.

---

## Transport

**Outbound gRPC stream.** coold dials `grpcs://scheduler:6443/v1/agent` at startup with per-host JWT. Scheduler routes command frames down the open stream. Works through NAT and corporate firewalls — scheduler never opens inbound to a host.

**Local REST on wg0 mgmt IP.** `100.64.X.X:8443` — reachable only inside the mesh. Used by `cooldctl firewall` (SSH-bounced), peer coolds, optional per-customer gateways.

---

## Scheduler

Central connection-holder. Laravel (PHP-FPM request/response model) can't hold thousands of long-lived HTTP/2 streams; scheduler does.

- `:6443` gRPC — single listener. coold dispatch + build dispatch share it.
- `/run/coolify/scheduler.sock` UDS — Laravel's sync + async lane. Mode `0660` when `SCHEDULER_UNIX_SOCKET_GROUP` set, else `0600`. No TLS, no bearer — filesystem perms replace auth.
- `Streams`: DashMap<host_id, StreamHandle{tx, caps, builder_capacity}>.
- `Pending`: DashMap<request_id, Waiting | Landed>. Cap `SCHEDULER_PENDING_MAX=10_000`. Landed entries hold 30 s TTL so late pollers still claim results.
- Sweeper evicts `Waiting` coold-lane entries after 10 s → 504.
- JWT verify (ES256/RS256) with `sub=host_id` + `caps` claim.

### UDS wire surface (Laravel → scheduler)

```
GET  /v1/health
POST /v1/coold/dispatch          sync, 10 s timeout
POST /v1/build/dispatch          202 Accepted + {request_id}
GET  /v1/build/result/:id        long-poll (?timeout_ms=, default 30 000)
POST /v1/build/:id/cancel        204
```

### Coold dispatch flow

Laravel POST → scheduler checks `Streams::get(host_id)` (miss → 404) → `Pending::insert_waiting` (cap overflow → 503) → parks oneshot → pushes `ServerMsg` onto host's mpsc → coold runs command against podman.sock → writes `Response` on same stream → scheduler fires parked sinks, transitions to `Landed` with 30 s TTL. 10 s no-response → 504. Stream dropped mid-dispatch → 503.

---

## coold wire surface (closed set, same on both transports)

```
# Images
POST   /api/v1/images/pull           {ref, auth?}  -> {digest}
GET    /api/v1/images
DELETE /api/v1/images/{ref}

# Containers (filtered podman surface)
POST   /api/v1/containers
POST   /api/v1/containers/{id}/start
POST   /api/v1/containers/{id}/stop           {timeout?}
POST   /api/v1/containers/{id}/restart
DELETE /api/v1/containers/{id}                {force?}
GET    /api/v1/containers/{id}
GET    /api/v1/containers/{id}/logs?follow=true
POST   /api/v1/containers/{id}/exec           {cmd, tty?}
POST   /api/v1/containers/{id}/healthcheck/run

# Volumes
POST   /api/v1/volumes
DELETE /api/v1/volumes/{name}
GET    /api/v1/volumes/{name}

# Networks
POST   /api/v1/networks
DELETE /api/v1/networks/{name}
GET    /api/v1/networks

# Firewall (sole writer; dual-plane)
POST   /api/v1/firewall/allow            -> {id}
DELETE /api/v1/firewall/allow/{id}
GET    /api/v1/firewall/allow[?namespace=X]
POST   /api/v1/firewall/allow/bulk
POST   /api/v1/firewall/reconcile

# Service endpoints (Corrosion writer)
POST   /api/v1/services/register
DELETE /api/v1/services/{id}/endpoints/{container_id}
GET    /api/v1/services/{id}/endpoints

# DNS (diagnostics)
GET    /api/v1/dns/lookup/{name}
GET    /api/v1/dns/stats

# Host facts
GET    /api/v1/host/info
GET    /api/v1/host/containers
GET    /api/v1/host/stats
```

No raw podman passthrough. New verbs require a coold release.

---

## Builder

Separate binary. coold never builds directly — it spawns the builder per-request.

- Builder rides coold's gRPC stream: one stream per host. coold advertises `"builder"` in Hello `capabilities` when `COOLD_BUILDER_ENABLED=1`. Scheduler capability-routes build envelopes to any host carrying it.
- Per build: `systemd-run --pipe --scope coolify-build-<request_id>` transient unit. Sandbox: `PrivateTmp`, `ProtectSystem=strict`, allowlisted `ReadWritePaths`, `MemoryMax`, `CPUQuota`, `RuntimeMaxSec`, `IPAddressDeny` for mgmt + container CIDRs.
- Builder clones repo shallow, runs toolchain, writes OCI image to shared `/var/lib/containers/storage` (same store as podman/coold — no registry hop on single-node).
- Durable output: NDJSON frames appended to `<work_dir>/events.ndjson`. Final outcome atomically written as `result.json` (success) or `error.json` (failure/cancel). Exit codes: 0 ok, 1 build err, 2 usage/IO, 130 SIGTERM.
- Restart adoption (`resume_or_reap`): on coold boot, scans `coolify-build-*.service` units. Active → re-register + poll `systemctl is-active`. Inactive + result/error → emit `Response` immediately. Inactive + neither → emit `500 builder exited without result file`.
- Cancel: `POST /v1/build/:id/cancel` → scheduler finds owning host in `Pending` → pushes `CancelBuild` → coold runs `systemctl kill --signal=SIGTERM <scope>`. cgroup takes builder + buildah + git together.

### Supported stacks (v0.1 MVP)

| Stack | Impl |
| --- | --- |
| `STATIC` | generateContainerfile → `buildah bud` → `nginx:alpine` base |
| `DOCKERFILE` / `BUILDPACKS` / `RAILPACK` | post-MVP |

---

## coold internal tasks

All tasks run concurrently in one `tokio::select!` in `coold/src/sync.rs::run`. Any task exit → whole process exit → systemd `Restart=on-failure` respawns. Fail-fast, never silently lose a worker.

| Task | File | Role |
| --- | --- | --- |
| Podman event stream | `coold/src/podman/events.rs` | Lifecycle events from podman.sock |
| Event trigger + reconcile | `coold/src/sync.rs` | Debounce → immediate reconcile; 2 s periodic |
| DNS servers | `coold/src/dns/server.rs` | hickory-server per namespace |
| Firewall API | `coold/src/firewall/server.rs` | axum REST, dual-plane writer |
| gRPC client | `coold/src/grpc/{mod,client,handlers}.rs` | Dials scheduler, Hello, handles dispatched commands + build lifecycle |
| Builder subprocess driver | `coold/src/builder/mod.rs` | Spawns `systemd-run`, parses `result.json`, restart adoption |

Key modules: `coold/src/firewall/store.rs` (Arc<Mutex> serializes iptables), `coold/src/firewall/rule.rs` (SHA256 12-hex ID), `coold/src/corrosion/client.rs` (HTTP to local Corrosion), `coold/src/dns/resolver.rs` (CoolifyResolver, 5 s TTL).

---

## Network model

- **Namespace = tenancy unit.** Each namespace gets a podman bridge `coolify-<ns>-mesh` with its own per-host `/24`. `coolify init --namespaces default,alpha,…` provisions every namespace on every host. coold receives full list via `COOLD_NAMESPACES=<name>:<network>:<gateway-ip>,…`.
- **Per-app sub-networks.** Inside a namespace, additional podman networks via `POST /networks`.
- **Egress.** Bridge-NAT to host default route. Cross-host container traffic rides wg0 via peer `AllowedIPs`.
- **Two enforcement planes, both coold-written.** iptables FORWARD (cross-host) + nft `coolify_bridge` (intra-host same-bridge, fills a Linux gap where bridge L2 forwarding bypasses iptables FORWARD).
- **Bind discipline.** DNS binds per-namespace bridge gateway only. REST API binds wg0 mgmt IP only. Never `0.0.0.0`.

---

## Responsibility split

| Concern | Owner |
| --- | --- |
| Podman API proxy | **coold** |
| iptables + nft dual-write | **coold** (sole kernel writer) |
| Corrosion row writes (own host only) | **coold** |
| Embedded DNS | **coold** |
| Host facts (`podman info`, load, wg state) | **coold** |
| Deny filter on container create | **coold** |
| Compose parsing, Dockerfile/Buildpacks/Nixpacks | **builder / central** |
| App model, service graph, deployment history | **central** |
| Scheduler (host placement) | **central** |
| Rolling deploy state machine, health gating, rollback | **central** |
| Ingress config templating, TLS cert mgmt | **central** |
| Secrets (stored encrypted, resolved at deploy time) | **central** |
| RBAC, audit trail, per-user identity | **central** |

**Litmus test**: could a Nomad-based competitor reuse coold with a different app model? yes → coold. no → central.

---

## Deploy flow (single app, abbreviated)

```
T0  Central builder clones source, invokes buildah / buildpack / nixpacks.
    Output: OCI image in containers-storage (single-node) or registry (multi-node).

T1  Central scheduler picks target host H.

T2  POST /images/pull  {ref: "localhost/tenant/web:v2"}      (skipped on single-node)

T3  POST /volumes      {name: "web-data"}

T4  POST /containers   (central templates from compose + resolved secrets)

T5  POST /containers/{id}/start

T6  Central polls GET /containers/{id} until healthy.

T7  POST /services/register   → Corrosion row → gossip → DNS answers new IP.

T8  POST /firewall/allow  {src: proxy-ip, dst: container-ip, port: 80}

T9  Central regenerates proxy config; POST /containers/{proxy}/exec reload.

T10 Retire old container:
      POST /containers/{old}/stop → DELETE /containers/{old}
      DELETE /services/web/endpoints/{old}
      DELETE /firewall/allow/{old-rule-id}
```

coold never sees "deploy app X". Only primitive frames.

---

## Security boundary

- **Authn**: static bearer token (local REST, `/etc/coolify/api-token` mode 0600); per-host JWT (outbound stream, issued at enrollment); filesystem perms (scheduler UDS).
- **Deny filter on `POST /containers`**: rejects `-privileged`, `-cap-add=SYS_ADMIN/NET_ADMIN`, host-path bind mounts outside an allowlist, `-net=host` (unless coold itself). Returns 403 with offending field.
- **No secret storage.** Central resolves secrets into `POST /containers` env/mounts; coold passes through and forgets.
- **No business audit.** coold keeps ops/debug request log only (endpoint, status, duration). Who-why lives in central.
- **Privilege boundary**: coold is the only process with podman socket access. No TCP podman API exposed anywhere.

---

## Persistence

coold keeps **no database**. Kernel chain is source of truth on restart; central reconciles drift via `POST /reconcile` or replays `POST /allow`.

- `/etc/coolify/allow.rules` — iptables-save fragment for `COOLIFY-ALLOW`.
- `/etc/coolify/allow.nft` — nft fragment for `coolify_bridge::coolify_allow`.
- Both atomically rewritten on every mutation (`.tmp` + rename). Restored on boot by `coolify-mesh-fw.service` + `coolify-mesh-allow.service` (ordered `After=…fw…`).
- Permissive-mode hosts: missing scaffold → bridge-plane write no-ops with one-shot WARN; iptables plane still succeeds; snapshot still written.

Builder-side persistence: `<work_dir>/events.ndjson` + `result.json` / `error.json` on disk, so builds survive coold restart.

---

## Systemd layout (single-node)

```
coold.service    Dials scheduler :6443, advertises "builder" cap when enabled,
                 spawns builder subprocesses in transient units per build.
scheduler.service :6443 (coold gRPC) + /run/coolify/scheduler.sock (Laravel UDS).
```

Builder has no long-lived unit; each build runs under `coolify-build-<request_id>.service` (transient, cleaned by systemd on exit or by `resume_or_reap` on next start).

---

## Config surface

### coold env vars

| Var | Default | Role |
| --- | --- | --- |
| `COOLD_HOST_MGMT_IP` | required | wg0 mgmt IP |
| `COOLD_NAMESPACES` | `default:coolify-default-mesh:0.0.0.0` | `<name>:<network>:<gateway-ip>,…` |
| `COOLD_SCHEDULER_URL` | — | grpcs://scheduler:6443/v1/agent |
| `COOLD_BUILDER_ENABLED` | unset | Advertise `"builder"` cap in Hello |
| `COOLD_API_BIND` | unset | wg0:8443 firewall REST (unset = disabled) |
| `COOLD_API_TOKEN_FILE` | unset | Required when API bind set |
| `COOLD_TLS_CERT` / `COOLD_TLS_KEY` | unset | Enables HTTPS on firewall API |
| `COOLD_RULES_PATH` / `COOLD_BRIDGE_RULES_PATH` | `/etc/coolify/allow.rules` / `.nft` | Snapshot paths |
| `COOLD_RECONCILE_INTERVAL` | `2s` | Reconcile cadence |
| `COOLD_DNS_ZONE` / `COOLD_DNS_UPSTREAM` | `coolify.internal` / `1.1.1.1:53` | DNS |

### scheduler env vars

| Var | Default | Role |
| --- | --- | --- |
| `SCHEDULER_GRPC_BIND` | _required_ | coold dials this. Must be a specific interface IP (typically the WireGuard mgmt IP, e.g. `10.42.0.1:6443`); `0.0.0.0` / `::` refused unless `SCHEDULER_ALLOW_PUBLIC_BIND=1` (dev only — JWTs cross the wire in cleartext). |
| `SCHEDULER_ALLOW_PUBLIC_BIND` | unset | Override to allow `0.0.0.0` / `::` bind. Dev/test only. |
| `SCHEDULER_UNIX_SOCKET_PATH` | `/run/coolify/scheduler.sock` | Laravel UDS |
| `SCHEDULER_UNIX_SOCKET_GROUP` | unset | PHP-FPM group grants `0660` |
| `SCHEDULER_PENDING_MAX` | `10000` | In-flight + landed cap |
| `SCHEDULER_JWT_PUBLIC_KEY_PATH` | `/etc/coolify/jwt.pub` | Verifies coold stream JWT |
| `SCHEDULER_LOG_LEVEL` | `info` | tracing EnvFilter |

---

## E2E tests

Live infra, all `#[ignore]`. Run with `--ignored --nocapture --test-threads=1`. `.env` auto-loaded.

- **`builder.rs`** — Hetzner-provisioned. 2 VMs (A = central + builder, B = coold-only). Runs `coolify init apply`, exercises dispatch / cancel / restart / artifact-perm on shared cluster. Single `builder_lifecycle` test.
- **`install.rs`** — Hetzner-provisioned. Networking assertions post `coolify init apply`. VMs destroyed on drop.

Env: `HETZNER_TOKEN`, `HETZNER_PROJECT`, `SSH_KEY`, `COOLIFY_BIN`, optional location/image/server-type.

---

## Non-goals

- No Compose parser in coold (Laravel-side).
- No Dockerfile / Buildpacks / Nixpacks in coold (builder + builder-core own these).
- No scheduler, no deploy state machine, no ingress templating, no RBAC, no audit, no secret storage.
- No raw podman passthrough. Enumerated verbs only.
- No IPv6 (AAAA → NODATA).
- No WireGuard peer management.


### Scheduler stream sync

Connected agents become visible through this flow:

```txt
coold connects → scheduler streams → POST /api/v1/servers/sync-streams → SQLite servers → Coolify UI Servers page → live container endpoint
```

The sync endpoint creates or updates servers by scheduler `host_id`, stores
capabilities, sets `last_seen_at`, marks the server online, and records an
event.
