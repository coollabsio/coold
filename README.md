## What is coold?

Per-host agent of Coolify v5. Kubelet-analogue for a WireGuard mesh of Podman hosts. One `coold` process per node. Narrow by design: executes local runtime primitives, never reasons about apps, builds, or deploys.

Today, coold owns service-discovery sync, embedded DNS, firewall mutations, the outbound flux stream, and optional builder subprocess supervision. It is the only process on a node with access to the Podman socket, the iptables/nft kernel interface, and the local Corrosion agent.

---

## System topology

```
┌────────────────────────────────────────────────────────────┐
│ Laravel (Coolify brain — app model, deploy ctrl, state)    │
└──────────────────────────┬─────────────────────────────────┘
                           │ HTTP over /run/coolify/flux.sock
                           ▼
┌────────────────────────────────────────────────────────────┐
│ flux                                                       │
│  • gRPC :6443 (coold dials in; HTTP/2 bidi, JWT bearer)    │
│  • UDS /run/coolify/flux.sock (Laravel; fs-perm auth)      │
│  • Streams map (host_id) + Pending map (request_id)        │
└──────────────────────────┬─────────────────────────────────┘
                           │ http(s)://flux:6443 Agent.Stream
                           ▼
┌────────────────────────────────────────────────────────────┐
│ coold (per node)                                           │
│  Firewall dual-writer · DNS · Corrosion sync · gRPC client │
│  Optional "builder" cap → spawns builder subprocess        │
└────────┬───────────────┬────────────────┬──────────────────┘
         │ UDS           │ HTTP           │ systemd-run --pipe --scope
         ▼               ▼                ▼
   podman.sock     corrosion agent   coolify-build-<request_id>.service
                        │                 │
                        │ SWIM gossip     │ builder binary
                        ▼                 ▼
                   other nodes       buildah → containers-storage
```

---

## Repo layout (Cargo workspace)

```
proto/          Shared Protobuf: Agent.Stream, Hello, ServerMsg, ClientMsg,
                Response, BuildRequest, CancelBuild, capabilities.
coold/          Per-host agent.
flux/          gRPC server coold dials + UDS lane for Laravel.
builder/       One-shot OCI build CLI, spawned by coold per build.
builder-core/  Reusable git + buildah pipeline (static_build.rs, …).
coolify-cli/   Rust v5 cluster CLI: WireGuard/Podman/coold/Corrosion init
               + SSH-bounced firewall. Does not include v4 Coolify
               API/context/project commands.
e2e-tests/      Live-server harness (Hetzner-provisioned). Excluded from
                default workspace build.
```

The central control plane (the "brain") is a separate Laravel app, not in
this workspace. It owns app-aware logic and persistent state, and talks to
`flux` directly over the flux's Unix-socket HTTP/JSON lane.

---

## Control plane (Laravel)

The Coolify v5 brain is a separate Laravel app. It talks to `flux` over
the flux's Unix-socket HTTP/JSON lane (default `/run/coolify/flux.sock`)
— no Rust API binary sits between them. Grant the Laravel/php-fpm group access
to the socket via `COOLIFY_FLUX_UNIX_SOCKET_GROUP`.

Flux UDS surface (consumed by Laravel):

```http
GET  /v1/health
GET  /v1/streams                  connected coold agents
POST /v1/coold/dispatch           e.g. list_containers
POST /v1/build/dispatch
GET  /v1/build/result/:request_id
POST /v1/build/:request_id/cancel
```

Live host reads flow: Laravel → flux UDS → coold outbound gRPC stream → Podman.

### Local development

One command starts flux + a fake coold; point your Laravel app at the
printed flux socket:

```bash
bun run dev
```

Full-fidelity Lima VM development (Podman, Corrosion, flux, real coold all run inside an isolated Linux VM):

```bash
bun run dev:vm:up   # create/start the VM
bun run dev:vm      # run the real-coold dev stack in the VM
```

The Lima VM keeps runtime state inside Linux (`/run/podman`, `/var/lib/corrosion`, `/etc/coolify`, iptables/nft, DNS binds). The repo is mounted read/write at `/workspace/coold`, so source edits still affect your checkout. Useful VM commands:

```bash
bun run dev:vm:shell
bun run dev:vm:stop
bun run dev:vm:delete
```

To run the real stack from inside the VM shell directly:

```bash
REAL_COOLD=1 bun run dev
```

Useful override:

```bash
COOLIFY_FLUX_GRPC_BIND=127.0.0.1:6444 \
bun run dev
```

---

## coolify — v5 cluster CLI

`coolify` is the Rust CLI for Coolify v5 cluster operations that belong next
to coold. It intentionally excludes v4 Coolify API commands (contexts, projects,
resources, deployments, private keys, etc.); the existing Go `coolify` CLI
continues to own those v4/current API commands during the migration window.

Current command surface:

```bash
coolify init plan --nodes NODE1,NODE2 --ssh-key KEY
coolify init bootstrap --nodes NODE1,NODE2 --ssh-key KEY --yes
coolify init extend --nodes NODE1,NODE2,NODE3 --new-nodes NODE3 --ssh-key KEY
coolify init upgrade --nodes NODE1,NODE2 --ssh-key KEY --coold-version vX.Y.Z --corrosion-version v1.0.0

# Optional builder placement. If --builder-hosts is omitted and
# --enable-builder=true (default), every node receives the builder binary/cap.
coolify init bootstrap --nodes NODE1,NODE2 --builder-hosts NODE1 --ssh-key KEY --yes

# Dev/NAT cases can override per-node WireGuard listen ports and peer endpoints.
coolify init bootstrap --nodes node-a,node-b --ssh-key KEY \
  --wg-listen-port-overrides node-a=51821,node-b=51822 \
  --wg-endpoint-overrides node-a=host.lima.internal:51821,node-b=host.lima.internal:51822

coolify firewall containers --nodes IP1,IP2 --ssh-key KEY
coolify firewall list --nodes IP1,IP2 --ssh-key KEY
coolify firewall allow --from 10.0.0.1 --to 10.0.0.2 --port 80 --nodes IP1 --ssh-key KEY
coolify firewall revoke --id <rule-id> --nodes IP1 --ssh-key KEY
```

The CLI shares the v5 mesh model: bootstrap over SSH, deployment nodes via
`--nodes` (alias `--servers`), and day-to-day firewall mutation through coold's
wg0-local REST API via SSH bounce. `coolify init` converges node runtime only:
WireGuard, Podman, namespace bridges, the firewall scaffold, Corrosion, coold,
and optionally the builder binary/capability. It no longer installs or
configures central `flux`; connect a node to flux separately with
`COOLIFY_COOLD_FLUX_URL` or `COOLIFY_COOLD_ASSIGNMENT_URL` plus a host JWT.

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

Snapshots: `/etc/coolify/allow.rules` + `/etc/coolify/allow.nft`. Restored on boot by `coolify-mesh-fw.service` + `coolify-mesh-allow.service`. Rule ID = `sha256("namespace|src|dst|proto|port")[:12]` — byte-compatible with `coolify firewall` and the retired Go v5 cluster CLI surface. Tuples only; audit / RBAC / owners live in Laravel.

---

## Transport

**Outbound gRPC stream.** coold can dial `http(s)://flux:6443` at startup with a per-host JWT and open `Agent.Stream`. The stream starts only when `COOLIFY_COOLD_FLUX_URL` or `COOLIFY_COOLD_ASSIGNMENT_URL` is configured and `COOLIFY_COOLD_GRPC_DISABLED=false`. Flux routes command frames down the open stream. Works through NAT and corporate firewalls — flux never opens inbound to a host.

**Local REST on wg0 mgmt IP.** `100.64.X.X:8443` — reachable only inside the mesh. Today this REST server is firewall-only and is used by `coolify firewall` via SSH bounce.

---

## Flux

Central connection-holder. Laravel (PHP-FPM request/response model) can't hold thousands of long-lived HTTP/2 streams; flux does.

- `:6443` gRPC — single listener. coold dispatch + build dispatch share it. The bind must be a specific interface IP unless `COOLIFY_FLUX_ALLOW_PUBLIC_BIND=1` is set for dev/test.
- `/run/coolify/flux.sock` UDS — Laravel's sync + async lane. Mode `0660` when `COOLIFY_FLUX_UNIX_SOCKET_GROUP` set, else `0600`. No TLS, no bearer — filesystem perms replace auth.
- `Streams`: DashMap<host_id, StreamHandle{tx, caps, builder_capacity}>.
- `Pending`: DashMap<request_id, Waiting | Landed>. Cap `COOLIFY_FLUX_PENDING_MAX=10_000`. Landed entries hold 30 s TTL so late pollers still claim results.
- Sweeper evicts `Waiting` coold-lane entries after 10 s → 504.
- JWT verify (ES256/RS256) with `sub=host_id` + `caps` claim.

### UDS wire surface (Laravel → flux)

```
GET  /v1/health
POST /v1/coold/dispatch          sync, 10 s timeout
POST /v1/build/dispatch          202 Accepted + {request_id}
GET  /v1/build/result/:id        long-poll (?timeout_ms=, default 30 000)
POST /v1/build/:id/cancel        204
```

### Coold dispatch flow

Laravel POST → flux checks `Streams::get(host_id)` (miss → 404) → `Pending::insert_waiting` (cap overflow → 503) → parks oneshot → pushes `ServerMsg` onto host's mpsc → coold runs command against podman.sock → writes `Response` on same stream → flux fires parked sinks, transitions to `Landed` with 30 s TTL. 10 s no-response → 504. Stream dropped mid-dispatch → 503.

---

## coold wire surface (implemented)

The implemented control surface is intentionally small. Future Podman lifecycle
verbs should be added explicitly; there is no raw Podman passthrough.

### gRPC via flux (`Agent.Stream`)

```protobuf
ServerMsg.list_containers  -> Response.list_containers
ServerMsg.build            -> Response.build
ServerMsg.cancel_build     -> no immediate response; cancels a running build
```

`list_containers` returns Podman container summaries plus inspected network
attachments. Build messages are capability-routed by flux to a connected host
that advertised `builder` in `Hello.capabilities`.

### Local REST on coold (firewall only)

```http
GET    /healthz
GET    /api/v1/firewall/allow[?namespace=X]
POST   /api/v1/firewall/allow             -> {id}
DELETE /api/v1/firewall/allow/:id
POST   /api/v1/firewall/allow/bulk
POST   /api/v1/firewall/reconcile
```

Not implemented yet in this codebase: image pull/list/delete, container
create/start/stop/restart/logs/exec/healthcheck, volume CRUD, network CRUD,
service endpoint CRUD over REST, DNS diagnostics, host facts, or a deny filter
on `POST /containers`. Those remain architecture goals, not current API.

---

## Builder

Separate binary. coold never builds directly — it spawns the builder per-request.

- Builder rides coold's gRPC stream: one stream per host. coold advertises `"builder"` in Hello `capabilities` when `COOLIFY_COOLD_BUILDER_ENABLED=1`. Flux capability-routes build envelopes to any host carrying it.
- Per build: `systemd-run --pipe --scope coolify-build-<request_id>` transient unit. Sandbox: `PrivateTmp`, `ProtectSystem=strict`, allowlisted `ReadWritePaths`, `MemoryMax`, `CPUQuota`, `RuntimeMaxSec`, `IPAddressDeny` for mgmt + container CIDRs.
- Builder clones repo shallow, runs toolchain, writes OCI image to shared `/var/lib/containers/storage` (same store as podman/coold — no registry hop on single-node).
- Durable output: NDJSON frames appended to `<work_dir>/events.ndjson`. Final outcome atomically written as `result.json` (success) or `error.json` (failure/cancel). Exit codes: 0 ok, 1 build err, 2 usage/IO, 130 SIGTERM.
- Restart adoption (`resume_or_reap`): on coold boot, scans `coolify-build-*.service` units. Active → re-register + poll `systemctl is-active`. Inactive + result/error → emit `Response` immediately. Inactive + neither → emit `500 builder exited without result file`.
- Cancel: `POST /v1/build/:id/cancel` → flux finds owning host in `Pending` → pushes `CancelBuild` → coold runs `systemctl kill --signal=SIGTERM <scope>`. cgroup takes builder + buildah + git together.

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
| gRPC client | `coold/src/grpc/{mod,client,handlers}.rs` | Dials flux, Hello, handles dispatched commands + build lifecycle |
| Builder subprocess driver | `coold/src/builder/mod.rs` | Spawns `systemd-run`, parses `result.json`, restart adoption |

Key modules: `coold/src/firewall/store.rs` (Arc<Mutex> serializes iptables), `coold/src/firewall/rule.rs` (SHA256 12-hex ID), `coold/src/corrosion/client.rs` (HTTP to local Corrosion), `coold/src/dns/resolver.rs` (CoolifyResolver, 5 s TTL).

---

## Network model

- **Namespace = tenancy unit.** Each namespace gets a podman bridge `coolify-<ns>-mesh` with its own per-host `/24`. `coolify init --namespaces default,alpha,…` provisions every namespace on every host. coold receives full list via `COOLIFY_COOLD_NAMESPACES=<name>:<network>:<gateway-ip>,…`.
- **Per-app sub-networks.** Additional per-app Podman networks are an architecture goal; the current coold REST server does not expose network CRUD yet.
- **Egress.** Bridge-NAT to host default route. Cross-host container traffic rides wg0 via peer `AllowedIPs`.
- **Two enforcement planes, both coold-written.** iptables FORWARD (cross-host) + nft `coolify_bridge` (intra-host same-bridge, fills a Linux gap where bridge L2 forwarding bypasses iptables FORWARD).
- **Bind discipline.** DNS binds per-namespace bridge gateway only. REST API binds wg0 mgmt IP only. Never `0.0.0.0`.

---

## Responsibility split

| Concern | Owner |
| --- | --- |
| Podman reads used by implemented primitives | **coold** (`list_containers`, inspect for discovery) |
| Podman lifecycle API proxy | **future coold surface** |
| iptables + nft dual-write | **coold** (sole kernel writer) |
| Corrosion row writes (own host only) | **coold** |
| Embedded DNS | **coold** |
| Host facts (`podman info`, load, wg state) | **future coold surface** |
| Deny filter on container create | **future coold surface** |
| Compose parsing, Dockerfile/Buildpacks/Nixpacks | **builder / central** |
| App model, service graph, deployment history | **central** |
| Flux (host placement) | **central** |
| Rolling deploy state machine, health gating, rollback | **central** |
| Ingress config templating, TLS cert mgmt | **central** |
| Secrets (stored encrypted, resolved at deploy time) | **central** |
| RBAC, audit trail, per-user identity | **central** |

**Litmus test**: could a Nomad-based competitor reuse coold with a different app model? yes → coold. no → central.

---

## Deploy flow status

The target architecture is still: central owns the app/deploy state machine and
coold only receives primitive frames. The current implemented primitives support
parts of that path, not the full deploy lifecycle.

Implemented today:

```
T0  Laravel asks flux to dispatch a static build.
T1  Flux picks a connected host with the `builder` capability (or uses host_id).
T2  coold spawns `builder` under a transient systemd scope.
T3  builder writes an OCI image to containers-storage and durable result files.
T4  coold reports the build result over the same gRPC stream.
T5  coold independently syncs running/stopped container endpoints into Corrosion.
T6  Central/Laravel can list live containers through flux → coold → Podman.
T7  Central/Laravel or `coolify firewall` can mutate allow rules through coold.
```

Not implemented in coold yet: image pull, volume creation, container
create/start/stop, service registration REST calls, proxy reload exec, and
retire/delete container primitives.

---

## Security boundary

- **Authn**: static bearer token (local REST, `/etc/coolify/api-token` mode 0600); per-host JWT (outbound stream, issued at enrollment); filesystem perms (flux UDS).
- **Container-create deny filter**: planned for the future Podman lifecycle surface; no `POST /containers` endpoint exists today.
- **No secret storage.** Central resolves secrets at deploy time; coold does not persist them.
- **No business audit.** coold keeps ops/debug request log only (endpoint, status, duration). Who-why lives in central.
- **Privilege boundary**: coold is the only process with podman socket access. No TCP podman API exposed anywhere.

---

## Persistence

coold keeps **no database**. Kernel chain and snapshot files are the local firewall source of truth on restart; central can reconcile drift via `POST /api/v1/firewall/reconcile` or by replaying allow rules.

- `/etc/coolify/allow.rules` — iptables-save fragment for `COOLIFY-ALLOW`.
- `/etc/coolify/allow.nft` — nft fragment for `coolify_bridge::coolify_allow`.
- Both atomically rewritten on every mutation (`.tmp` + rename). Restored on boot by `coolify-mesh-fw.service` + `coolify-mesh-allow.service` (ordered `After=…fw…`).
- Permissive-mode hosts: missing scaffold → bridge-plane write no-ops with one-shot WARN; iptables plane still succeeds; snapshot still written.

Builder-side persistence: `<work_dir>/events.ndjson` + `result.json` / `error.json` on disk, so builds survive coold restart.

---

## Systemd layout (single-node)

```
coold.service    Runs discovery/DNS/firewall locally. Dials flux only when
                 COOLIFY_COOLD_FLUX_URL or COOLIFY_COOLD_ASSIGNMENT_URL is set;
                 advertises "builder" cap when enabled and spawns builder
                 subprocesses in transient units per build.
flux.service     :6443 (coold gRPC) + /run/coolify/flux.sock (Laravel UDS).
```

Builder has no long-lived unit; each build runs under `coolify-build-<request_id>.service` (transient, cleaned by systemd on exit or by `resume_or_reap` on next start).

---

## Config surface

### coold env vars

| Var | Default | Role |
| --- | --- | --- |
| `COOLIFY_COOLD_HOST_MGMT_IP` | required | wg0 mgmt IP / host identity used in local state |
| `COOLIFY_COOLD_PODMAN_SOCKET` | `/run/podman/podman.sock` | Local Podman Unix socket |
| `COOLIFY_COOLD_CORROSION_URL` | `http://127.0.0.1:8080` | Local Corrosion HTTP API |
| `COOLIFY_COOLD_NAMESPACES` | empty | `<name>:<network>:<gateway-ip>,…`; empty = no managed namespaces/DNS skipped |
| `COOLIFY_COOLD_RECONCILE_INTERVAL` | `2s` | Service endpoint reconcile cadence |
| `COOLIFY_COOLD_LOG_LEVEL` | `info` | tracing EnvFilter |
| `COOLIFY_COOLD_DNS_ZONE` | `coolify.internal` | Authoritative cluster DNS zone |
| `COOLIFY_COOLD_DNS_UPSTREAM` | `1.1.1.1:53` | Resolver for out-of-zone queries |
| `COOLIFY_COOLD_API_BIND` | unset | Firewall REST bind address; unset disables local REST |
| `COOLIFY_COOLD_API_TOKEN_FILE` | unset | Required when API bind is set |
| `COOLIFY_COOLD_TLS_CERT` / `COOLIFY_COOLD_TLS_KEY` | unset | Enables HTTPS on firewall API when both are set |
| `COOLIFY_COOLD_RULES_PATH` | `/etc/coolify/allow.rules` | iptables allow snapshot |
| `COOLIFY_COOLD_BRIDGE_RULES_PATH` | `/etc/coolify/allow.nft` | nft bridge-family allow snapshot |
| `COOLIFY_COOLD_CHAIN_NAME` | `COOLIFY-ALLOW` | iptables chain coold owns |
| `COOLIFY_COOLD_FLUX_URL` | unset | Static flux URL for self-hosted/private mode |
| `COOLIFY_COOLD_ASSIGNMENT_URL` | unset | Hosted-cloud assignment endpoint; returns current flux URL |
| `COOLIFY_COOLD_HOST_JWT_PATH` | `/etc/coolify/host-jwt` | Per-host JWT for outbound gRPC |
| `COOLIFY_COOLD_GRPC_DISABLED` | `false` | Disable outbound gRPC even when flux URL/assignment is set |
| `COOLIFY_COOLD_BUILDER_ENABLED` | `false` | Advertise `"builder"` cap in Hello |
| `COOLIFY_COOLD_BUILDER_WORK_DIR` | `/var/lib/coolify-builder/work` | Per-build scratch/result root |
| `COOLIFY_COOLD_BUILDER_CAPACITY` | `2` | Concurrent builds accepted by this host |
| `COOLIFY_COOLD_BUILDER_BIN` | `/usr/local/bin/builder` | Builder binary coold spawns |
| `COOLIFY_COOLD_BUILDER_TIMEOUT_SECS` | `1800` | RuntimeMaxSec per build scope |
| `COOLIFY_COOLD_BUILDER_MEMORY_MAX` | `2G` | MemoryMax per build scope |
| `COOLIFY_COOLD_BUILDER_CPU_QUOTA` | `200%` | CPUQuota per build scope |
| `COOLIFY_COOLD_BUILDER_DENY_NETS` | empty | Extra comma-separated CIDRs denied to builder scopes |

### flux env vars

| Var | Default | Role |
| --- | --- | --- |
| `COOLIFY_FLUX_GRPC_BIND` | _required_ | coold dials this. Must be a specific interface IP (typically the WireGuard mgmt IP, e.g. `10.42.0.1:6443`); `0.0.0.0` / `::` refused unless `COOLIFY_FLUX_ALLOW_PUBLIC_BIND=1` (dev only — JWTs cross the wire in cleartext). |
| `COOLIFY_FLUX_ALLOW_PUBLIC_BIND` | unset | Override to allow `0.0.0.0` / `::` bind. Dev/test only. |
| `COOLIFY_FLUX_ID` | unset | Stable flux identity reported to Laravel in hosted-cloud mode (e.g. `flux-eu-1`). |
| `COOLIFY_FLUX_PUBLIC_URL` | unset | Public TLS URL Laravel returns from assignment for coold to dial. |
| `COOLIFY_FLUX_INTERNAL_URL` | unset | Private URL Laravel uses when dispatching to this flux. |
| `COOLIFY_FLUX_REGION` | unset | Optional flux region label reported to Laravel. |
| `COOLIFY_FLUX_LARAVEL_API_URL` | unset | Laravel base URL for flux heartbeat and connection registry calls. Unset disables reporting. |
| `COOLIFY_FLUX_LARAVEL_API_TOKEN` | unset | Bearer token for Laravel internal flux registry endpoints. |
| `COOLIFY_FLUX_AGENT_CAPACITY` | `10000` | Max long-lived coold streams this flux should be assigned. |
| `COOLIFY_FLUX_LARAVEL_HEARTBEAT_INTERVAL_SECS` | `10` | Flux heartbeat interval to Laravel. |
| `COOLIFY_FLUX_UNIX_SOCKET_PATH` | `/run/coolify/flux.sock` | Laravel UDS |
| `COOLIFY_FLUX_UNIX_SOCKET_GROUP` | unset | PHP-FPM group grants `0660` |
| `COOLIFY_FLUX_PENDING_MAX` | `10000` | In-flight + landed cap |
| `COOLIFY_FLUX_JWT_PUBLIC_KEY_PATH` | `/etc/coolify/jwt.pub` | Verifies coold stream JWT |
| `COOLIFY_FLUX_LOG_LEVEL` | `info` | tracing EnvFilter |
| `COOLIFY_FLUX_DISPATCH_TIMEOUT_SECS` | `30` | Config field for dispatch timeout; current coold-lane timeout constant is 10 s |

---

## E2E tests

Live infra, all `#[ignore]`. Run with `--ignored --nocapture --test-threads=1`. `.env` auto-loaded.

- **`builder.rs`** — Hetzner-provisioned. 2 VMs (A = builder-capable, B = coold-only). Exercises dispatch / cancel / restart / artifact-perm on a shared cluster. Single `builder_lifecycle` test.
- **`install.rs`** — Hetzner-provisioned. Networking assertions after `coolify init bootstrap`. VMs destroyed on drop.

Env: `HETZNER_TOKEN`, `HETZNER_PROJECT`, `SSH_KEY`, `COOLIFY_CLI_BIN`, optional location/image/server-type.

---

## Non-goals

- No Compose parser in coold (Laravel-side).
- No Dockerfile / Buildpacks / Nixpacks in coold (builder + builder-core own these).
- No flux, no deploy state machine, no ingress templating, no RBAC, no audit, no secret storage.
- No raw podman passthrough. Enumerated verbs only; current implemented verbs are deliberately minimal.
- No IPv6 (AAAA → NODATA).
- No WireGuard peer management.


### Flux stream registry

When registry reporting is configured, connected agents become visible through
this flow:

```txt
coold connects → flux Streams map → POST /api/v1/internal/agent-connections/upsert
flux heartbeat → POST /api/v1/internal/fluxs/heartbeat
disconnect → POST /api/v1/internal/agent-connections/disconnect
```

Flux reports host `capabilities`, `builder_capacity`, optional `coold_version`,
connection/disconnection reasons, and aggregate stream counts to Laravel.
