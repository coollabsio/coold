# coold architecture

> CLI-side counterpart: `coolify` in this workspace. Historical v5 notes lived
> in `coolify-cli/CONTROL_PLANE.md`; new v5 cluster CLI changes belong here and
> in `coolify-cli/`, not in the v4 Coolify CLI.

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
| OCI image build (Dockerfile / Buildpacks / Railpack / Static) | **deferred builder** | Not active in current v5 Flux/coold runtime. Requires a dedicated ADR/API. coold never builds. |
| App model (app → services → deployments → replicas) | **central** | Central DB. coold has no notion of "app". |
| Scheduling (which host runs which container) | **central** | Consumes coold host facts; decides placement. |
| Deploy orchestration (rolling swap, health gate, proxy cutover, rollback) | **central controller** | State machine in central. |
| Ingress config compilation (hostnames → upstreams, TLS certs) | **central** | Coold only runs the resulting proxy container. |
| Secrets store + injection | **central** | Central templates resolved values into `containers/create` env/mounts. |
| Rule metadata (who/when/why, RBAC, audit, tenant scoping) | **central** | Coold applies tuples, not intent. |
| User identity (logins, tokens, tenants) | **central** | coold only sees per-host JWT / bearer. |

## 3. Wire surface (enumerable)

coold exposes a closed gRPC primitive set over the outbound Flux stream. New
verbs require a coold/protobuf release; there is no raw Podman passthrough and
no host-local control surface.

```
# Images
POST   images/pull           {ref, auth?}            -> {digest}
GET    images                                         -> [{ref, digest, size}]
DELETE images/{ref}

# Containers (filtered podman surface)
POST   containers            <create spec>            -> {id}
POST   containers/{id}/start
POST   containers/{id}/stop          {timeout?}
POST   containers/{id}/restart
DELETE containers/{id}                {force?}
GET    containers/{id}                (inspect)
GET    containers/{id}/logs?follow=true               (streamed)
POST   containers/{id}/exec           {cmd, tty?}     (streamed)
POST   containers/{id}/healthcheck/run

# Volumes
POST   volumes               {name, driver, labels}
DELETE volumes/{name}
GET    volumes/{name}

# Networks (bootstrap creates coolify-mesh; per-app nets created here)
POST   networks              {name, driver, options, labels}
DELETE networks/{name}
GET    networks

# Firewall (coold = sole writer; dual-plane: iptables + nft bridge)
firewall.allow            {id, namespace, src, dst, proto?, port?}  -> {id}
firewall.revoke           {id}
firewall.list             {?namespace}
firewall.reconcile        {}  # flush + reload both snapshots

# Service endpoints (Corrosion writer; central registers on deploy)
services.register
services.unregister
services.endpoints

# DNS (diagnostics)
dns.lookup
dns.stats

# Host facts (read-only; central scrapes for observability + scheduling)
host.info             (podman info, kernel, wg state, load)
host.containers       (podman ps -a)
host.stats            (podman stats snapshot)
```

This list must stay aligned with the protobuf wire surface and Coolify docs. If you add or change a verb here, update the proto, Flux routing, coold handlers, and Coolify client code together.

## 4. Transports

coold speaks one control transport:

- **Outbound stream**: coold dials
  `grpcs://<central-host>:443/v1/agent` on start, presenting its per-host JWT.
  Central routes commands to it by host id over the open stream. gRPC bidi +
  Protobuf is the alpha decision — typed schemas + native server-streaming for
  logs/exec. WSS over :443 remains the documented fallback if gRPC-through-
  proxy issues surface. Same code path for self-hosted and cloud SaaS.
- **No inbound control API**: central never dials coold. All mutations arrive
over the coold-initiated stream. Works through NAT / corp firewalls.
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
- No flux (host placement is central's job).
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
- **Builder**: deferred until a dedicated ADR/API defines build scheduling, logs, artifacts, cancellation, and registry flow.
- **Compose translator**: `docker-compose.yml` → N `containers/create` +
  volume creates + service-register + firewall-allow frames.
- **Flux**: round-robin / least-loaded / pin / GPU-affinity. Consumes
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
`coolify` init/apply docs.

```
T0  Central build pipeline clones source, invokes BuildKit / buildpack / nixpacks.
    Output: OCI image @ registry.coolify.io/tenant/web:v2.

T1  Central deploy controller picks target host H (flux).

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
  `COOLIFY_COOLD_NAMESPACES=<name>:<network>:<gateway-ip>,…` and binds one DNS task per
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
  (`10.210.<ns>.1:53`); never `0.0.0.0`. coold exposes no local control listener.
  Separate concerns, separate sockets.

## 10. Volumes (v5 alpha)

- Local podman volumes per host (`/var/lib/containers/storage/volumes`).
- **Stateful services pin to host** for alpha. Cross-host volume movement and
  distributed FS are post-alpha.
- Backup: `podman volume export` + scp, orchestrated by central.

## 11. Builder (deferred)

Builder is a future building block, but it is not part of the active v5
Flux/coold runtime surface right now. The current contract deliberately has no
Flux `/v1/build/*` lane, no protobuf `BuildRequest` / `CancelBuild`, no coold
builder capability, and no CLI `--builder-*` bootstrap flags.

Before re-enabling builder, add an ADR/API covering:

- who schedules builds and how capacity is represented;
- where build logs, artifacts, and final results live;
- cancellation and restart-adoption semantics;
- whether builder runs as a coold child, a separate host agent, or central infra;
- registry push/pull flow for single-node and multi-node clusters.

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

- **Authn**: per-host JWT for the outbound stream, issued at enrollment.
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

## 16. Central topology: flux + Laravel

### Why a separate flux service

Laravel (Coolify central brain) runs on a request/response PHP worker model.
Workers cycle on deploy and cannot safely hold thousands of long-lived HTTP/2
streams. The `flux` binary fills this gap: it is the gRPC server that
coold dials, and it exposes a synchronous HTTP lane over a Unix domain
socket that Laravel calls per-request.

```
[coold hosts]
     │  grpcs://central.example.com:6443  (JWT bearer, HTTP/2 bidi stream)
     ▼
[flux]
     ▲  HTTP over /run/coolify/flux.sock  (0660, group = PHP-FPM)
     │
[Laravel]  (brain: deploy controller, RBAC, etc.)
```

No Redis, no message queue. The flux owns the connection pool; Laravel
treats it as a local HTTP backend.

### Self-hosted topology (default)

Single central VM. No load balancer required.

- `flux` binds the WireGuard mgmt interface IP on port `6443` for coold
  gRPC (systemd unit, starts before Laravel). `COOLIFY_FLUX_GRPC_BIND` is
  required and must be a specific interface IP — `0.0.0.0` / `::` are
  refused at startup unless `COOLIFY_FLUX_ALLOW_PUBLIC_BIND=1` is set
  (dev/test only; JWTs cross the wire in cleartext).
- `flux` also binds the UDS at `/run/coolify/flux.sock`. Mode `0660`
  when `COOLIFY_FLUX_UNIX_SOCKET_GROUP` is set, else `0600`. The PHP-FPM group
  goes in `COOLIFY_FLUX_UNIX_SOCKET_GROUP` so Laravel workers can dial it
  without a TCP hop or auth handshake.
- Laravel runs on `:80` / `:443` via nginx. No port conflict.
- TLS on flux gRPC only: Let's Encrypt cert (if domain available) or
  self-signed generated at `coolify init`, pinned in
  `/etc/coolify/flux.pin`.
- Firewall: open port `6443` inbound for coold connections.

### Assignment mode / multi-flux

There are two supported control-plane shapes:

- **Simple self-hosted**: one local flux, Laravel talks to its UDS, and
  coold uses static `COOLIFY_COOLD_FLUX_URL`.
- **Assignment mode**: Laravel owns a flux registry and assigns each
  coold to one healthy flux. This is required for hosted Coolify Cloud
  and is also the path for self-hosted users who want to run more than one
  flux.

In assignment mode, each `coold` calls Laravel's assignment endpoint with its
host JWT and capabilities, receives a flux URL, then opens the long-lived
gRPC stream to that flux. If the stream drops, coold backs off and asks
assignment again before reconnecting. Hosted Coolify Cloud uses the same flow,
but cloud central **does not join customer WireGuard meshes**; self-hosted
assignment can return private/WireGuard flux URLs instead.

Fluxs in assignment mode report ownership back to Laravel:

```
flux -> POST internal/fluxs/heartbeat
flux -> POST internal/agent-connections/upsert
flux -> POST internal/agent-connections/disconnect
```

Laravel stores `host_id -> flux_id` and dispatches to the owning
flux's private `COOLIFY_FLUX_INTERNAL_URL`. Assignment should use stable
rendezvous hashing over healthy, non-draining fluxs so adding/removing a
flux moves only a subset of hosts. In hosted Cloud, the flux public
listener must sit behind TLS (typically public :443 LB/ingress → private h2c
flux); the `COOLIFY_FLUX_ALLOW_PUBLIC_BIND=1` escape hatch remains dev/test
only. In scaled self-hosted deployments, `COOLIFY_FLUX_PUBLIC_URL` may be a
private WireGuard/LAN URL as long as coold can dial it.

### UDS wire surface (Laravel → flux)

All handlers live in `flux/src/unix_bridge.rs`. Axum routes:

```
GET  /v1/health                              -> {"ok": true}
POST /v1/coold/dispatch                      sync, 10 s timeout
```

Access control = filesystem perms. No TLS, no bearer, no per-request
authn — any local caller with group membership on the socket is trusted.
JWT stays on the coold→flux gRPC stream where it belongs.

### Envelope schema (`flux/src/envelope.rs`)

Coold dispatch — request and response:

```json
POST /v1/coold/dispatch
{ "host_id": "10.64.0.7",
  "request_id": "01HX…",
  "command": { "type": "containers.list" } }

// response (sync)
{ "request_id": "01HX…",
  "status": "ok",
  "data": [ { "id": "...", "name": "...", "image": "...", "state": "...", "networks": [...] } ] }
// or
{ "request_id": "01HX…",
  "status": "error",
  "code": 404, "message": "host not connected" }
```

Image and container primitives are wired today. Every future verb from §3 adds a
variant to `CommandPayload` and a match arm in `route_coold`.

Build dispatch:

```json
{ "host_id": "10.64.0.7",                // optional — absent = flux picks
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
   `COOLIFY_FLUX_PENDING_MAX`, default 10 000; overflow → 503).
4. Handler calls `Pending::park(request_id)` → `oneshot::Receiver`, then
   pushes `ServerMsg` into the host's `mpsc::Sender<ServerMsg>` feeding
   the open `Agent.Stream` response half.
5. coold executes the command against `/run/podman/podman.sock` and
   writes `Response { request_id, body }` back on the same stream.
6. Flux `grpc_server::deliver_response` looks up the pending entry by
   `request_id`, translates the proto body to `ResponseBody` via
   `ResponseBody::try_from_proto`, and `Pending::deliver` fires every
   parked oneshot sink, then transitions the entry to `Landed` with a
   30 s TTL (`LANDED_TTL_SECS`) so a late poller on the build lane can
   still claim results.
7. HTTP handler receives the body and returns `ResponseEnvelope` to
   Laravel. No response within 10 s (`DISPATCH_TIMEOUT_SECS`, hard-coded
   in `state.rs`) → sweeper evicts the entry, handler returns 504.

Unknown `host_id` → 404. Host stream dropped mid-dispatch → 503.

### Flux config (env vars)

All sourced from `flux/src/config.rs`:

| var | default | role |
|---|---|---|
| `COOLIFY_FLUX_GRPC_BIND` | _required_ | coold dials this. Must be a specific interface IP (typically the WireGuard mgmt IP, e.g. `10.42.0.1:6443`); `0.0.0.0` / `::` refused unless `COOLIFY_FLUX_ALLOW_PUBLIC_BIND=1`. |
| `COOLIFY_FLUX_ALLOW_PUBLIC_BIND` | unset | Set to `1` to allow binding `0.0.0.0` / `::`. Dev/test only — JWTs cross the wire unencrypted. |
| `COOLIFY_FLUX_ID` | unset | Stable flux identity reported to Laravel in hosted-cloud mode. |
| `COOLIFY_FLUX_PUBLIC_URL` | unset | Public TLS URL returned to coold by Laravel assignment. |
| `COOLIFY_FLUX_INTERNAL_URL` | unset | Private Laravel→flux dispatch URL. |
| `COOLIFY_FLUX_REGION` | unset | Optional flux region label. |
| `COOLIFY_FLUX_LARAVEL_API_URL` | unset | Laravel base URL for flux heartbeat and agent connection ownership calls. |
| `COOLIFY_FLUX_LARAVEL_API_TOKEN` | unset | Bearer token for Laravel internal flux registry endpoints. |
| `COOLIFY_FLUX_AGENT_CAPACITY` | `10000` | Max long-lived agent streams this flux should be assigned. |
| `COOLIFY_FLUX_LARAVEL_HEARTBEAT_INTERVAL_SECS` | `10` | Heartbeat interval for registry reporting. |
| `COOLIFY_FLUX_UNIX_SOCKET_PATH` | `/run/coolify/flux.sock` | Laravel UDS. |
| `COOLIFY_FLUX_UNIX_SOCKET_GROUP` | unset (mode `0600`) | PHP-FPM group grants `0660`. |
| `COOLIFY_FLUX_PENDING_MAX` | `10000` | cap on in-flight + landed pendings. |
| `COOLIFY_FLUX_DISPATCH_TIMEOUT_SECS` | `30` | **currently unused** — handler uses the 10 s `DISPATCH_TIMEOUT_SECS` const in `state.rs`. TODO: wire the flag through. |
| `COOLIFY_FLUX_JWT_PUBLIC_KEY_PATH` | `/etc/coolify/jwt.pub` | verifies coold stream JWT. |
| `COOLIFY_FLUX_LOG_LEVEL` | `info` | `tracing` EnvFilter. |

### coold config rename

Self-hosted installs use `COOLIFY_COOLD_FLUX_URL` as a static target. Hosted Coolify Cloud sets
`COOLIFY_COOLD_ASSIGNMENT_URL` instead; coold POSTs its host JWT and capabilities to
Laravel before each connect attempt and dials the returned flux URL.


### Repository layout

```
Cargo.toml          # workspace root (members: coold, flux, builder, builder-core, proto, e2e-tests)
proto/
  agent.proto       # shared Protobuf: Agent.Stream, Hello, ServerMsg, ClientMsg, Response,
  Cargo.toml
  src/lib.rs
coold/
  Cargo.toml
  src/              # dials flux gRPC, podman proxy, firewall scaffold, DNS
flux/
  Cargo.toml
  src/
    main.rs         # tonic AgentServer (grpc_server mod) + pending_sweeper
    config.rs       # COOLIFY_FLUX_* env vars (see table above)
    auth.rs         # JWT verify (ES256/RS256, sub = host_id, caps claim)
                    # Pending: DashMap<request_id, PendingEntry{Waiting|Landed}>
    envelope.rs     # Laravel-facing JSON: DispatchEnvelope, ResponseEnvelope,
builder/
  Cargo.toml
  src/main.rs       # one-shot CLI: reads request.json, runs builder-core, writes result.json
builder-core/
  src/              # reusable git + buildah pipeline (static_build.rs etc.)
```

## 17. builder — deferred OCI image build agent

`builder/` and `builder-core/` remain in the workspace as dormant reference
code, but current v5 does not expose builder as a Flux/coold primitive. Keep
new builder work behind a fresh ADR/API before reintroducing protobuf messages,
Flux routes, coold runtime config, CLI flags, or E2E suites.

## 18. Cross-references

- Bootstrap + CLI: `coolify-cli/` in this workspace.
- Runtime firewall changes: future gRPC primitives through Flux.
- Wire surface + transport: §3 and §4 here are the source of truth for `coolify` command behavior.


## Central control plane

The central Coolify v5 brain is a separate **Laravel app**, not part of this
Rust workspace. It owns all app-aware logic (compose, Dockerfiles, buildpacks,
scheduling, rollback, ingress templating, RBAC, audit) and its own persistent
state. This workspace ships only the data-plane pieces: host agent (`coold`),
stream router (`flux`) and cluster bootstrap (`coolify`). The build agent (`builder`) is deferred reference code until a dedicated ADR/API lands.


### Laravel to coold request path

Laravel does not open inbound connections to host agents. For live host
operations it calls flux's local Unix socket (`COOLIFY_FLUX_UNIX_SOCKET_PATH`,
default `/run/coolify/flux.sock`; grant the php-fpm group access via
`COOLIFY_FLUX_UNIX_SOCKET_GROUP`). Flux routes the request down the existing
coold-initiated gRPC stream keyed by `host_id`. Example: `POST /v1/coold/dispatch`
with a `containers.list` command maps host-offline responses to error code,
timeouts, missing `host_id`, and malformed flux responses per `envelope.rs`.


### Flux stream inventory

Flux exposes a local UDS inventory endpoint at `GET /v1/streams`, returning
connected host streams (`host_id`, capabilities). Laravel polls
this to discover online agents and materialize them into its own server registry
(persisting capabilities and last-seen, marking them online).
