# coold architecture

> CLI-side counterpart: `cooldctl` in this workspace. Historical v5 notes lived
> in `coolify-cli/CONTROL_PLANE.md`; new v5 cluster CLI changes belong here and
> in `cooldctl/`, not in the v4 Coolify CLI.

## 1. Role of coold

coold is the **kubelet-analogue** of Coolify v5: a narrow, per-host agent that
proxies a curated set of primitives over the local `/run/podman/podman.sock`
Unix socket, dual-writes allow rules to iptables (`COOLIFY-ALLOW`) and to the
nft bridge chain (`coolify_bridge::coolify_allow`), writes service-endpoint
rows to Corrosion, and answers `<container>.<namespace>.coolify.internal` DNS
on each namespace's bridge gateway IP.

It is **not** the brain. Central Coolify (the apiserver + controllers analogue)
owns all app-aware logic — compose, Dockerfiles, buildpacks, Nixpacks,
scheduling, rollback, ingress templating, RBAC, audit.

**Test for "should this live in coold?"**: could a second orchestrator
(a Nomad-style competitor) reuse this coold with a different app model? If yes
→ coold. If no → central.

## 2. Responsibility split

| Concern | Owner | Notes |
|---|---|---|
| podman API proxy (containers/images/volumes/networks/exec/logs) | **coold** | Thin pass-through; denies dangerous flags. |
| iptables `COOLIFY-ALLOW` writes + `/etc/coolify/allow.rules` snapshot (cross-host plane) | **coold** | Sole kernel writer. |
| nft `coolify_bridge::coolify_allow` writes + `/etc/coolify/allow.nft` snapshot (intra-host same-bridge plane) | **coold** | Sole kernel writer. Dual-written in the same API handler. |
| Corrosion row writes (service endpoints, per namespace) scoped to own host | **coold** | Gossip distributes. |
| Embedded DNS on each namespace's bridge gateway `:53` | **coold** | Reads local Corrosion, answers `<container>.<namespace>.coolify.internal`. |
| Host facts (`podman info`, `wg show`, `/proc/*`, `iptables -nvL`) | **coold** | Read-only endpoints for central to scrape. |
| Bearer-token authn + deny-dangerous-flags filter + ops/debug request log | **coold** | No RBAC, no per-user identity. |
| Compose file parsing (services, networks, depends_on, volumes) | **central** | Emits primitive op sequence. |
| OCI image build (Dockerfile / Buildpacks / Railpack / Static) | **builder** (separate binary in this workspace) | buildah bud → shared containers-storage. coold never builds. See §6. |
| App model (app → services → deployments → replicas) | **central** | Central DB. coold has no notion of "app". |
| Scheduling (which host runs which container) | **central** | Consumes coold host facts; decides placement. |
| Deploy orchestration (rolling swap, health gate, proxy cutover, rollback) | **central controller** | State machine in central. |
| Ingress config compilation (hostnames → upstreams, TLS certs) | **central** | Coold only runs the resulting proxy container. |
| Secrets store + injection | **central** | Central templates resolved values into `containers/create` env/mounts. |
| Rule metadata (who/when/why, RBAC, audit, tenant scoping) | **central** | Coold applies tuples, not intent. |
| User identity (logins, tokens, tenants) | **central** | coold only sees per-host JWT / bearer. |

## 3. Wire surface (enumerable)

coold exposes the **same endpoint set** on both transports: the outbound stream
(from central) and the local REST listener on wg0 mgmt IP (for intra-mesh
callers). The list is closed — new verbs require a coold release, not a
passthrough.

```
# Images
POST   /api/v1/images/pull           {ref, auth?}            -> {digest}
GET    /api/v1/images                                         -> [{ref, digest, size}]
DELETE /api/v1/images/{ref}

# Containers (filtered podman surface)
POST   /api/v1/containers            <create spec>            -> {id}
POST   /api/v1/containers/{id}/start
POST   /api/v1/containers/{id}/stop          {timeout?}
POST   /api/v1/containers/{id}/restart
DELETE /api/v1/containers/{id}                {force?}
GET    /api/v1/containers/{id}                (inspect)
GET    /api/v1/containers/{id}/logs?follow=true               (streamed)
POST   /api/v1/containers/{id}/exec           {cmd, tty?}     (streamed)
POST   /api/v1/containers/{id}/healthcheck/run

# Volumes
POST   /api/v1/volumes               {name, driver, labels}
DELETE /api/v1/volumes/{name}
GET    /api/v1/volumes/{name}

# Networks (bootstrap creates coolify-mesh; per-app nets created here)
POST   /api/v1/networks              {name, driver, options, labels}
DELETE /api/v1/networks/{name}
GET    /api/v1/networks

# Firewall (coold = sole writer; dual-plane: iptables + nft bridge)
POST   /api/v1/firewall/allow            {namespace, src, dst, proto?, port?}  -> {id}
DELETE /api/v1/firewall/allow/{id}
GET    /api/v1/firewall/allow[?namespace=X]
POST   /api/v1/firewall/allow/bulk       {add:[...], remove:[id,...]}          -> {ok}
POST   /api/v1/firewall/reconcile        -> {ok}  # flush + reload both snapshots

# Service endpoints (Corrosion writer; central registers on deploy)
POST   /api/v1/services/register
DELETE /api/v1/services/{id}/endpoints/{container_id}
GET    /api/v1/services/{id}/endpoints

# DNS (diagnostics)
GET    /api/v1/dns/lookup/{name}
GET    /api/v1/dns/stats

# Host facts (read-only; central scrapes for observability + scheduling)
GET    /api/v1/host/info             (podman info, kernel, wg state, load)
GET    /api/v1/host/containers       (podman ps -a)
GET    /api/v1/host/stats            (podman stats snapshot)
```

This list must stay byte-identical to the wire-surface block in
`cooldctl` docs and command handlers. If you add or change a verb here, update
the v5 CLI code in `cooldctl/` too.

## 4. Transports

coold speaks two transports, **same endpoint set on both**:

- **Outbound stream (primary)**: coold dials
  `grpcs://<central-host>:443/v1/agent` on start, presenting its per-host JWT.
  Central routes commands to it by host id over the open stream. gRPC bidi +
  Protobuf is the alpha decision — typed schemas + native server-streaming for
  logs/exec. WSS over :443 remains the documented fallback if gRPC-through-
  proxy issues surface. Same code path for self-hosted and cloud SaaS.
- **Local REST on wg0 mgmt IP (`100.64.0.X:8443`)**: intra-mesh callers only
  (`cooldctl firewall` via SSH-bounce, peer coolds, optional
  per-customer gateway). Bearer-token authn on every request.
- **No inbound from central**: central never dials coold. All mutations from
  central arrive over the coold-initiated stream; no `COOLIFY-ALLOW` rule for
  "central → host:8443" is needed. Works through NAT / corp firewalls.
- **L4 LB + keepalive**: any load balancer between coold and central must be
  L4 (TCP pass-through). gRPC rides HTTP/2 long-lived streams; L7 LBs
  round-robin per-request and break the transport. Both sides send HTTP/2
  PING frames (default interval 30s) so provider idle timeouts (~5 min on
  Hetzner Cloud LB TCP) do not silently drop quiet streams. See §15 for
  capacity planning.

## 5. Deny filter

Defense-in-depth on `POST /containers` — even though central is trusted, coold
refuses the following by default:

- `--privileged`, `--cap-add=SYS_ADMIN/NET_ADMIN` unless the host is marked
  `allow_privileged=true` in coold config.
- Host-path bind mounts outside a configurable allowlist (default: none).
- Host netns (`--net=host`) unless the container is coold itself.

Rejections return `403` with the offending field so central can surface a
readable error.

## 6. What coold does NOT do

Explicit non-goals. If you find yourself wanting any of these in coold, the
answer is "put it in central":

- No compose parser.
- No Dockerfile handler.
- No Buildpacks / Nixpacks.
- No scheduler (host placement is central's job).
- No deploy state machine (rolling, canary, blue/green — all central).
- No ingress templating or proxy config rendering.
- No RBAC, no per-user identity, no business audit trail.
- No secret storage. Central resolves secrets and templates them into
  `POST /containers` calls; coold sees the final values and forgets them.
- No `/podman/raw` passthrough. Every supported verb is in §3.
- No app model. coold does not know what an "app" is — only containers,
  images, volumes, networks.

## 7. Central's responsibilities (the "brain")

For completeness on the other side of the split:

- **App model**: `App { id, name, source: git|dockerfile|compose|image, services: [...] }`.
- **Builder**: BuildKit / Buildpacks / Nixpacks → registry push. Runs on the
  **first mesh host by default** for self-hosted; central may override per-
  deploy via `target_host_id`. Cloud = central-run. Same binary, config-
  selected role.
- **Compose translator**: `docker-compose.yml` → N `containers/create` +
  volume creates + service-register + firewall-allow frames.
- **Scheduler**: round-robin / least-loaded / pin / GPU-affinity. Consumes
  `GET /host/info`, `/host/stats`.
- **Deploy controller**: state machine
  `Pending → Building → Pulling → Creating → Starting → HealthWaiting →
  Cutover → Running` (happy path). On failure: reverse-apply already-sent
  primitives.
- **Ingress controller**: watches service endpoints, regenerates proxy config,
  restarts proxy container via coold primitives.
- **Secrets**: stored encrypted in central DB; resolved into env/mounts at
  deploy time.
- **RBAC / audit / tenant scoping**: central DB only.

## 8. Deploy flow (T0–T10)

Walkthrough of a single deploy, showing every primitive op. Mirrors
`cooldctl` init/apply docs.

```
T0  Central builder clones source, invokes BuildKit / buildpack / nixpacks.
    Output: OCI image @ registry.coolify.io/tenant/web:v2.

T1  Central deploy controller picks target host H (scheduler).

T2  Frame: POST /images/pull {ref: "registry.coolify.io/tenant/web:v2"}
    coold@H calls podman.sock /images/create, streams progress back.

T3  Frame: POST /volumes {name: "web-data", driver: "local"}
    idempotent; no-op if exists.

T4  Frame: POST /containers  (central templates from compose + resolved secrets)
    body:
      {
        "image": "registry.coolify.io/tenant/web:v2",
        "name": "web-v2-a3f91",
        "network": "coolify-mesh",
        "ip": "10.210.H.42",
        "dns": ["10.210.H.1"],
        "dns_search": ["coolify.internal"],
        "env": {"DATABASE_URL": "postgres://…"},
        "mounts": [{"volume": "web-data", "target": "/data"}],
        "healthcheck": {"test": ["CMD","curl","-f","http://localhost/"], "interval": "5s"},
        "labels": {"coolify.app": "web", "coolify.version": "v2"}
      }
    coold runs deny filter → podman.sock /containers/create → returns id.

T5  Frame: POST /containers/{id}/start

T6  Central polls GET /containers/{id} or subscribes to events.
    Wait for healthy; abort + rollback on timeout.

T7  Frame: POST /services/register
    coold writes Corrosion row. Gossip distributes; DNS now answers new IP.

T8  Frame: POST /firewall/allow  (on dst host — coold = sole kernel writer)
    {src: proxy-ip, dst: 10.210.H.42, proto: "tcp", port: 80}

T9  Central ingress controller regenerates proxy config.
    Frame: POST /containers/{proxy-id}/exec (reload)  or proxy-specific reload.

T10 Cutover complete. Central retires the old container:
      POST /containers/{old-id}/stop {timeout: 10}
      DELETE /containers/{old-id}
      DELETE /services/web/endpoints/{old-container-id}
      DELETE /firewall/allow/{old-rule-id}
```

Every frame = one verb from §3. coold never sees "deploy app X v2".

## 9. Network model

- **Namespaces as the tenancy unit**: every namespace is a separate podman
  bridge named `coolify-<ns>-mesh` with its own per-host `/24` carved from the
  shared container pool. `coolify init --namespaces default,alpha,…` provisions
  every namespace on every host in one pass. coold receives the full list via
  `COOLD_NAMESPACES=<name>:<network>:<gateway-ip>,…` and binds one DNS task per
  entry.
- **Per-app sub-networks**: inside a namespace, users may opt into additional
  podman networks (docker-compose `networks:` style). Central compiles the
  compose block into `POST /networks` + attach-on-create. coold is the only
  writer; bootstrap owns only the top-level `coolify-<ns>-mesh` bridges.
- **Egress from containers**: bridge-NAT to the host's default route. Cross-
  host container traffic rides wg0 via peer `AllowedIPs` (mgmt /32 + every
  peer namespace subnet).
- **Two enforcement planes, both coold-written**:
  - **iptables FORWARD** — cross-host plane. `COOLIFY-INTRA` jumps to
    `COOLIFY-ALLOW`, then `DROP`. Enforces deny/allow on packets crossing
    wg0 ↔ bridge.
  - **nft `coolify_bridge`** — intra-host same-bridge plane. Fills a linux
    gap: bridge L2 forwarding bypasses iptables FORWARD even with
    `bridge-nf-call-iptables=1`. nft bridge-family hooks catch same-subnet
    traffic and jump to the same `coolify_allow` chain.
- **coold DNS binds each namespace bridge gateway IP only**
  (`10.210.<ns>.1:53`); never `0.0.0.0`. REST API binds the wg0 mgmt IP only.
  Separate concerns, separate sockets.

## 10. Volumes (v5 alpha)

- Local podman volumes per host (`/var/lib/containers/storage/volumes`).
- **Stateful services pin to host** for alpha. Cross-host volume movement and
  distributed FS are post-alpha.
- Backup: `podman volume export` + scp, orchestrated by central.

## 11. Builder

- Self-hosted default: **first mesh host** runs the builder. Central may pin
  to a specific host per-deploy via `target_host_id`.
- Cloud: builder runs inside central's infra.
- Builder is never coold. Builder writes to a registry; coold only pulls the
  resulting tag via `POST /images/pull`.

## 12. Persistence

- **Allow-rule snapshots (dual plane)**: on every successful API mutate coold
  rewrites both snapshot files:
  - `/etc/coolify/allow.rules` — flat `iptables-save` fragment
    (`:COOLIFY-ALLOW` + `-A COOLIFY-ALLOW` lines only).
  - `/etc/coolify/allow.nft` — nft fragment opening with
    `flush chain bridge coolify_bridge coolify_allow` then one `add rule`
    per tuple (namespace is preserved in the rule comment as `cid:<id>:<ns>`).
  Both writes are atomic (`.tmp` + `mv`). Empty rule set → just the flush
  line, which the next write naturally replaces.
- **Boot restore**:
  - `coolify-mesh-fw.service` (installed by `coolify init`) runs the nft
    scaffold (`coolify_bridge` table + `forward` + `coolify_intra` chains)
    and then `nft -f /etc/coolify/allow.nft` to restore the coold-owned
    `coolify_allow` chain without touching the scaffold chains.
  - `coolify-mesh-allow.service` is a `Type=oneshot` unit ordered
    `After=coolify-mesh-fw.service`, running
    `iptables-restore --noflush /etc/coolify/allow.rules`. `--noflush`
    preserves everything else in the filter table.
- **Permissive-mode hosts**: when the `coolify_bridge` scaffold is absent
  (`coolify init --skip-default-deny`), the bridge-plane write no-ops with a
  one-shot WARN and the iptables plane still succeeds. Snapshot file is still
  written so a later `coolify init apply` without `--skip-default-deny`
  converges cleanly.
- coold keeps **no other DB**. App-level persistence (rule metadata, audit,
  app model) lives in central.

## 13. Security boundary

- **Authn**: per-host static bearer token (local REST, `/etc/coolify/api-token`
  mode 0600) + per-host JWT (outbound stream, issued at enrollment).
- **No secret storage**: secrets enter via `POST /containers` env/mounts at
  deploy time; coold passes through and forgets.
- **No business audit**: coold keeps an ops/debug request log (request id,
  endpoint, status, duration) only. Who-when-why audit lives in central.
- **Privilege boundary**: coold is the only process with podman socket access.
  No TCP podman API. Deny filter (§5) gates dangerous flags even from trusted
  callers.

## 14. Versioning + compatibility

- Protobuf schema versioned per RPC. Breaking change = major bump.
- Backwards-compatible additions (new optional fields, new RPCs) = minor bump.
- coold binary advertises its supported schema range at stream handshake;
  central pins per-host to what the deployed coold supports until upgraded.
- New verbs require a coold release. No passthrough.

## 15. Scale and routing

Capacity planning for the `coold → central` stream fleet.

### Cost per open stream

- One TCP file descriptor + TLS state + ~50 KB of buffers per agent.
- 10 k streams ≈ 500 MB RSS on central, 10 k fds. Requires `ulimit -n`
  bumped to ≥ 65 k and `net.core.somaxconn` / `tcp_max_syn_backlog` raised.
- Steady-state CPU scales with **command rate**, not stream count. Quiet
  hosts are cheap.

### Fleet sizing tiers

| Fleet | Topology |
|---|---|
| < 1 k | Single central instance. Tune fds. Done. |
| 1 k – 10 k | N × central behind L4 LB. Stateless — any coold lands on any central. |
| 10 k – 100 k | Same, with sharding or consistent hash if per-central stream count exceeds tonic's comfortable ceiling (~30–50 k per 16-core box). |
| > 100 k | Multi-region; regional central clusters; push command routing to edge. |

### Horizontal central (recommended path)

- coold dials the LB VIP; LB distributes new streams across central
  instances. Each central instance serves the streams assigned to it.
- On `Hello`, the receiving central writes a row to Corrosion:
  ```
  host_routes(host_id, central_instance_id, connected_at)
  ```
  Gossip distributes. Controllers doing a deploy look up which instance
  owns `host_id` and forward the command via internal RPC or a small
  inter-central gRPC service.
- Why Corrosion: already in the stack, already gossiping, eventually
  consistent — matches the retry-safe semantics of command dispatch.
  No new infra (Redis / etcd / Kafka).

### Thundering herd / rebalance

- Exponential backoff **with jitter** in the coold dialer prevents lockstep
  reconnects on central restart. Target: `backoff ± random(0, backoff/2)`.
- Optional **periodic reconnect** (e.g. every 60 min ± 15 min jitter) lets
  the fleet rebalance onto newly added central instances without waiting
  for a failure to trigger a redial.
- On central drain/upgrade, the server sends a `ShutdownHint` frame; coold
  closes and redials, landing on a different instance via the LB. Zero
  downtime for deploys already in flight elsewhere.

### Load balancer constraints

- **L4 only** (TCP pass-through). HTTP/2 long-lived streams break L7
  round-robin. Hetzner Cloud Load Balancer supports this in `tcp` service
  mode. AWS NLB, haproxy `mode tcp` equivalent.
- **TLS terminates at central**, not at the LB. JWT check runs on each
  central; LB is transport-only. (mTLS per-host is workable but creates
  handshake thundering on fleet reconnect — JWT + one central-side TLS
  cert scales better.)
- **Idle timeout** on the LB must be tolerated by HTTP/2 PING. Hetzner
  default is ~5 min; enable `http2_keep_alive_interval` on both client and
  server (30 s is fine).
- **Provider connection caps**: Hetzner LB tiers cap concurrent conns
  (LB11 ≈ 10 k, LB21 ≈ 30 k, LB31 ≈ 75 k at time of writing). Pick tier or
  shard across multiple LB VIPs if a single tier is insufficient.

### Bandwidth profile

Bytes flowing through the LB are **control-plane only**. Image bytes
never touch central/LB:

- **Image pulls**: `POST /images/pull {ref}` is a tiny frame. coold calls
  podman, which fetches directly from the registry. Bytes go
  `registry → coold host`, bypassing central entirely.
- **Commands / responses / host-facts scrapes**: negligible.
- **Log follow streams**: the real bandwidth consumer at scale. 10 k
  hosts × 10 KB/s average ≈ 800 Mbps through central. Mitigations: only
  subscribe when a user is actively tailing in the UI, cap per-stream
  throughput, compress non-follow historical fetches.
- **Exec streams**: interactive, normally low bandwidth.

### Capacity rule of thumb

```
N_central = ceil(fleet_size / 30_000) + 1  # +1 for headroom / drain
```

Assumes commodity 16-core / 32 GB nodes running tonic. Increase divisor
on larger hardware; decrease if logs-follow subscriptions are common.

## 16. Central topology: scheduler + Laravel

### Why a separate scheduler service

Laravel (Coolify central brain) runs on a request/response PHP worker model.
Workers cycle on deploy and cannot safely hold thousands of long-lived HTTP/2
streams. The `scheduler` binary fills this gap: it is the gRPC server that
coold dials, and it exposes a synchronous HTTP lane over a Unix domain
socket that Laravel calls per-request.

```
[coold hosts]
     │  grpcs://central.example.com:6443  (JWT bearer, HTTP/2 bidi stream)
     ▼
[scheduler]
     ▲  HTTP over /run/coolify/scheduler.sock  (0660, group = PHP-FPM)
     │
[Laravel]  (brain: deploy controller, RBAC, etc.)
```

No Redis, no message queue. The scheduler owns the connection pool; Laravel
treats it as a local HTTP backend.

### Self-hosted topology (default)

Single central VM. No load balancer required.

- `scheduler` binds the WireGuard mgmt interface IP on port `6443` for coold
  gRPC (systemd unit, starts before Laravel). `SCHEDULER_GRPC_BIND` is
  required and must be a specific interface IP — `0.0.0.0` / `::` are
  refused at startup unless `SCHEDULER_ALLOW_PUBLIC_BIND=1` is set
  (dev/test only; JWTs cross the wire in cleartext).
- `scheduler` also binds the UDS at `/run/coolify/scheduler.sock`. Mode `0660`
  when `SCHEDULER_UNIX_SOCKET_GROUP` is set, else `0600`. The PHP-FPM group
  goes in `SCHEDULER_UNIX_SOCKET_GROUP` so Laravel workers can dial it
  without a TCP hop or auth handshake.
- Laravel runs on `:80` / `:443` via nginx. No port conflict.
- TLS on scheduler gRPC only: Let's Encrypt cert (if domain available) or
  self-signed generated at `coolify init`, pinned in
  `/etc/coolify/scheduler.pin`.
- Firewall: open port `6443` inbound for coold connections.

### Cloud / multi-scheduler

Single scheduler per central VM / pod for the current release. Multi-scheduler
routing (Corrosion `host_routes` fan-out, inter-scheduler command forward)
is deferred until horizontal central is actually needed — the UDS lane
is local-only by design, so scaling requires either sidecar pinning
(Laravel pod → local scheduler) or a new inter-scheduler RPC.

### UDS wire surface (Laravel → scheduler)

All handlers live in `scheduler/src/unix_bridge.rs`. Axum routes:

```
GET  /v1/health                              -> {"ok": true}
POST /v1/coold/dispatch                      sync, 10 s timeout
POST /v1/build/dispatch                      202 Accepted + {request_id}
GET  /v1/build/result/:request_id            long-poll, ?timeout_ms= (default 30 000)
POST /v1/build/:request_id/cancel            204 No Content
```

Access control = filesystem perms. No TLS, no bearer, no per-request
authn — any local caller with group membership on the socket is trusted.
JWT stays on the coold→scheduler gRPC stream where it belongs.

### Envelope schema (`scheduler/src/envelope.rs`)

Coold dispatch — request and response:

```json
POST /v1/coold/dispatch
{ "host_id": "10.64.0.7",
  "request_id": "01HX…",
  "command": { "type": "list_containers" } }

// response (sync)
{ "request_id": "01HX…",
  "status": "ok",
  "data": [ { "id": "...", "name": "...", "image": "...", "state": "...", "networks": [...] } ] }
// or
{ "request_id": "01HX…",
  "status": "error",
  "code": 404, "message": "host not connected" }
```

Only `list_containers` is wired today. Every future verb from §3 adds a
variant to `CommandPayload` and a match arm in `route_coold`.

Build dispatch:

```json
POST /v1/build/dispatch
{ "host_id": "10.64.0.7",                // optional — absent = scheduler picks
  "request_id": "01HY…",
  "command": { "type": "static_build",
               "repo_url": "https://…",
               "git_ref": "main",
               "target_image": "localhost/web:v2",
               "output_dir": "dist",
               "base_image": "docker.io/library/nginx:alpine" } }

// 202 Accepted
{ "request_id": "01HY…" }

// later:
GET /v1/build/result/01HY…?timeout_ms=30000
{ "request_id": "01HY…",
  "status": "ok",
  "digest": "sha256:…",
  "registry_ref": "localhost/web:v2",
  "duration_ms": 12345 }
```

### Coold dispatch flow (step by step)

1. Laravel `POST /v1/coold/dispatch` with `{host_id, request_id, command}`.
2. `route_coold` checks `Streams::get(host_id)`; miss → 404.
3. `Pending::insert_waiting` reserves a slot (capped at
   `SCHEDULER_PENDING_MAX`, default 10 000; overflow → 503).
4. Handler calls `Pending::park(request_id)` → `oneshot::Receiver`, then
   pushes `ServerMsg` into the host's `mpsc::Sender<ServerMsg>` feeding
   the open `Agent.Stream` response half.
5. coold executes the command against `/run/podman/podman.sock` and
   writes `Response { request_id, body }` back on the same stream.
6. Scheduler `grpc_server::deliver_response` looks up the pending entry by
   `request_id`, translates the proto body to `ResponseBody` via
   `ResponseBody::try_from_proto`, and `Pending::deliver` fires every
   parked oneshot sink, then transitions the entry to `Landed` with a
   30 s TTL (`LANDED_TTL_SECS`) so a late poller on the build lane can
   still claim results.
7. HTTP handler receives the body and returns `ResponseEnvelope` to
   Laravel. No response within 10 s (`DISPATCH_TIMEOUT_SECS`, hard-coded
   in `state.rs`) → sweeper evicts the entry, handler returns 504.

Unknown `host_id` → 404. Host stream dropped mid-dispatch → 503.

### Build dispatch flow

1. Laravel `POST /v1/build/dispatch`. `route_build` picks the target:
   - `host_id` present → require `"builder"` cap on that stream, else 503.
   - `host_id` absent → `Streams::pick_host_with_cap("builder")` (first
     match; no load balancing), else 503.
2. `Pending::insert_waiting` with `PendingKind::Build`; handler returns
   202 immediately. Build `Waiting` entries are **not** swept by the
   timeout sweeper — the systemd-run transient unit's `RuntimeMaxSec`
   is the real ceiling.
3. Laravel polls `GET /v1/build/result/:id?timeout_ms=…`. `Pending::park`
   either hands back a cached `Landed` body or returns a receiver that
   awaits the in-flight response.
4. coold streams the final `Response` back the same way coold dispatch
   does; `deliver_response` fans out to parked pollers.
5. Cancel: `POST /v1/build/:request_id/cancel` → `route_build` resolves
   the owning host from `Pending`, emits `CancelBuild` on its stream,
   coold runs `systemctl kill --signal=SIGTERM <scope>`.

### Scheduler config (env vars)

All sourced from `scheduler/src/config.rs`:

| var | default | role |
|---|---|---|
| `SCHEDULER_GRPC_BIND` | _required_ | coold dials this. Build traffic shares this port — no separate builder listener. Must be a specific interface IP (typically the WireGuard mgmt IP, e.g. `10.42.0.1:6443`); `0.0.0.0` / `::` refused unless `SCHEDULER_ALLOW_PUBLIC_BIND=1`. |
| `SCHEDULER_ALLOW_PUBLIC_BIND` | unset | Set to `1` to allow binding `0.0.0.0` / `::`. Dev/test only — JWTs cross the wire unencrypted. |
| `SCHEDULER_UNIX_SOCKET_PATH` | `/run/coolify/scheduler.sock` | Laravel UDS. |
| `SCHEDULER_UNIX_SOCKET_GROUP` | unset (mode `0600`) | PHP-FPM group grants `0660`. |
| `SCHEDULER_PENDING_MAX` | `10000` | cap on in-flight + landed pendings. |
| `SCHEDULER_DISPATCH_TIMEOUT_SECS` | `30` | **currently unused** — handler uses the 10 s `DISPATCH_TIMEOUT_SECS` const in `state.rs`. TODO: wire the flag through. |
| `SCHEDULER_JWT_PUBLIC_KEY_PATH` | `/etc/coolify/jwt.pub` | verifies coold stream JWT. |
| `SCHEDULER_LOG_LEVEL` | `info` | `tracing` EnvFilter. |

### coold config rename

`COOLD_CENTRAL_URL` renamed to `COOLD_SCHEDULER_URL`. Update enrollment and
`coolify init` templates when central enrollment is implemented.
Semantics identical; only the name changes to reflect the actual target.


### Repository layout

```
Cargo.toml          # workspace root (members: coold, scheduler, builder, builder-core, proto, e2e-tests)
proto/
  agent.proto       # shared Protobuf: Agent.Stream, Hello, ServerMsg, ClientMsg, Response,
                    # BuildRequest, CancelBuild, BuildResponseBody, capabilities, builder_capacity
  Cargo.toml
  src/lib.rs
coold/
  Cargo.toml
  src/              # dials scheduler gRPC, podman proxy, firewall writer, DNS, builder subprocess driver
scheduler/
  Cargo.toml
  src/
    main.rs         # tonic AgentServer (grpc_server mod) + pending_sweeper
    config.rs       # SCHEDULER_* env vars (see table above)
    auth.rs         # JWT verify (ES256/RS256, sub = host_id, caps claim)
    state.rs        # Streams: DashMap<host_id, StreamHandle{tx, caps, builder_capacity}>
                    # Pending: DashMap<request_id, PendingEntry{Waiting|Landed}>
    envelope.rs     # Laravel-facing JSON: DispatchEnvelope, ResponseEnvelope,
                    # BuildDispatchEnvelope, BuildResponseEnvelope
    routing.rs      # pure routing (no I/O): route_coold, route_build → RouteOutcome
    unix_bridge.rs  # axum UDS server, handlers for /v1/coold/* and /v1/build/*
builder/
  Cargo.toml
  src/main.rs       # one-shot CLI: reads request.json, runs builder-core, writes result.json
builder-core/
  src/              # reusable git + buildah pipeline (static_build.rs etc.)
```

## 17. builder — OCI image build agent

`builder/` is a separate binary in this Cargo workspace. coold never
runs builds directly; it spawns the builder per-request.

### Role

- Builder no longer holds its own gRPC stream. Commit `1024747`
  collapsed it onto coold's `Agent.Stream`: one stream per host, not
  two. Coold advertises `"builder"` in its Hello `capabilities` (gated
  by `COOLD_BUILDER_ENABLED`); the scheduler capability-routes build
  envelopes to any host that carries it.
- Per build: coold spawns the builder binary inside a
  `systemd-run --pipe --scope coolify-build-<request_id>` transient
  unit for cgroup + FS isolation (`PrivateTmp`, `ProtectSystem=strict`,
  `ReadWritePaths` allowlist, `MemoryMax`, `CPUQuota`, `RuntimeMaxSec`,
  `IPAddressDeny` for mgmt / container CIDRs).
- Builder clones the repo (shallow), runs the toolchain, writes the OCI
  image to shared podman containers-storage
  (`/var/lib/containers/storage`).
- Builder emits NDJSON frames on stdout **and** durably to
  `<work_dir>/events.ndjson` (see persistence below).
- Coold parses the final frame and relays a `Response` over the
  existing stream; scheduler delivers it on the build lane.

### Supported stacks (v0.1 MVP)

| Stack | Detector | Impl |
|---|---|---|
| `STATIC` | explicit in BuildRequest | generateContainerfile → `buildah bud` → nginx:alpine base |
| `DOCKERFILE` | — | post-MVP |
| `BUILDPACKS` | — | post-MVP |
| `RAILPACK` | — | post-MVP |

### Storage model (single-node MVP)

Builder writes to the same `/var/lib/containers/storage` as coold +
podman. No registry, no push over the network. coold calls
`containers/create image=localhost/<app>@sha256:...`; image is already
present. Multi-node requires a registry or `podman save`/`load` —
deferred.

### Persistence (survive coold restart)

Commit `8ac89a1` makes builds durable across a coold upgrade or crash.

- **Durable event log**: every NDJSON frame is appended to
  `<work_dir>/events.ndjson`. Stdout remains best-effort — SIGPIPE is
  ignored at builder startup and write errors are swallowed, so a dead
  reader (coold gone) never terminates the build.
- **Final outcome**: atomically written (`.tmp` + `rename`) as
  `<work_dir>/result.json` on success or `<work_dir>/error.json` on
  error / cancel. Exit codes: 0 success, 1 build error, 2 usage/IO
  error, 130 on SIGTERM.
- **Restart adoption**: coold's `resume_or_reap` runs after the outbound
  gRPC mpsc channel is live but before Hello. For every
  `coolify-build-*.service` found on disk it classifies the unit:
  - **active** → adopt: re-register in `active_builds` (cancel routing
    keeps working), spawn a task that polls `systemctl is-active` every
    2 s and, on exit, reads `result.json` / `error.json` and emits the
    `Response` on the new stream.
  - **inactive + `result.json`** → emit success `Response` immediately.
  - **inactive + `error.json`** → emit error `Response` immediately.
  - **inactive + neither** → emit `500 builder exited without result
    file` so Laravel gets a terminal error instead of hanging.
- Scheduler-side: no change. `Response` envelopes carry `request_id`; the
  scheduler routes by lookup in `Pending` regardless of which coold stream
  (old or restarted) delivered them. If both scheduler and coold restart,
  delivery still works as long as Laravel's poller is still holding
  `GET /v1/build/result/:id`.

### Cancellation

`POST /v1/build/:request_id/cancel` →
`route_build(Cancel)` → scheduler finds the owning host in `Pending`,
pushes `CancelBuild` over the stream → coold runs `systemctl kill
--signal=SIGTERM <scope>`. The cgroup kill takes the builder, buildah,
and git down together.

### Ports

- scheduler coold + build gRPC: `:6443` (single listener).
- No `:6444`. The separate builder listener was removed in `1024747`.

### Single-node systemd layout

```
coold.service     → dials scheduler :6443, advertises "builder" cap when enabled,
                    spawns builder subprocesses in transient units per build
scheduler.service → listens :6443 (coold gRPC) + /run/coolify/scheduler.sock (Laravel UDS)
```

Builder has no long-lived unit; each build runs under
`coolify-build-<request_id>.service` (transient, cleaned by systemd on
exit or by coold's `resume_or_reap` on next start).

## 18. Cross-references

- Bootstrap + CLI: `cooldctl/` in this workspace.
- `cooldctl firewall`: SSH-bounced REST client of local coold.
- Wire surface + transport: §3 and §4 here are the source of truth for `cooldctl` command behavior.


## Coolify v5 Rust API + React UI

The central Coolify v5 application lives in `coolify-web`, not in `coold`.
`coolify-web` is an Axum binary with an embedded React/Vite SPA. It uses
`coolify-core` for pure domain types and `coolify-storage` for SQLite-backed
repositories and migrations. The split keeps host-agent code (`coold`), stream
routing (`scheduler`), cluster bootstrap (`cooldctl`), and the user-facing web
application independently testable while still shipping from one Rust workspace.

Initial operator-visible API routes are `/healthz`, `/api/v1/status`,
`/api/v1/servers`, `/api/v1/servers/:id/live-status`, `/api/v1/servers/:id/containers`, `/api/v1/clusters`, `/api/v1/events`, and `/api/v1/builds`.
The React UI reads those routes through TanStack Query and gives a basic
dashboard for seeing cluster/server/event state while the deeper control-plane
flows are built.


### coolify-web to coold request path

`coolify-web` does not open inbound connections to host agents. For live host
operations it calls scheduler's local Unix socket (`COOLIFY_SCHEDULER_SOCKET`,
default `/run/coolify/scheduler.sock`). Scheduler routes the request down the
existing coold-initiated gRPC stream keyed by `servers.host_id`. The first live
endpoint is `GET /api/v1/servers/:id/containers`, which dispatches
`list_containers` through scheduler and maps host-offline responses to HTTP 404,
timeouts to 504, missing `host_id` to 409, and malformed scheduler responses to
502.
