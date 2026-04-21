# coold architecture

> CLI-side counterpart: `coolify-cli/CONTROL_PLANE.md`. Both docs describe the
> same split from different vantage points; keep them in sync when the wire
> surface changes.

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
| Dockerfile / Buildpack / Nixpacks build | **central** (or pinned builder host) | BuildKit / buildpacks / nixpacks → push to registry. coold only `images/pull`s. |
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
`coolify-cli/CONTROL_PLANE.md §2`. If you add or change a verb here, update
there too.

## 4. Transports

coold speaks two transports, **same endpoint set on both**:

- **Outbound stream (primary)**: coold dials
  `grpcs://<central-host>:443/v1/agent` on start, presenting its per-host JWT.
  Central routes commands to it by host id over the open stream. gRPC bidi +
  Protobuf is the alpha decision — typed schemas + native server-streaming for
  logs/exec. WSS over :443 remains the documented fallback if gRPC-through-
  proxy issues surface. Same code path for self-hosted and cloud SaaS.
- **Local REST on wg0 mgmt IP (`100.64.0.X:8443`)**: intra-mesh callers only
  (the `coolify firewall` CLI via SSH-bounce, peer coolds, optional
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
`coolify-cli/CONTROL_PLANE.md §7`.

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

## 17. Central topology: broker + Laravel

### Why a separate broker service

Laravel (Coolify central brain) runs on a request/response PHP worker model.
Workers cycle on deploy and cannot safely hold thousands of long-lived HTTP/2
streams. The `broker` binary fills this gap: it is the gRPC server
that coold dials, and it bridges commands and responses to Laravel via Redis.

```
[coold hosts]
     │  grpcs://central.example.com:6443
     ▼
[broker]  ←→  Redis (coold:cmd stream, coold:resp:{id} lists)
                              │
                         [Laravel]  (brain: scheduler, deploy controller, RBAC, etc.)
```

### Self-hosted topology (default)

Single central VM. No load balancer required.

- `broker` binds `0.0.0.0:6443` (systemd unit, starts before Laravel).
- Laravel runs on `:80` / `:443` via nginx. No port conflict.
- Redis on localhost. Both Laravel and broker connect to it.
- TLS on broker: Let's Encrypt cert (if domain available) or self-signed
  generated at `coolify init`, pinned in `/etc/coolify/broker.pin`.
- Firewall: open port `6443` inbound for coold connections.

### Cloud SaaS topology (fleet)

Multiple broker instances behind an L4 LB (per §15 sizing):

- L4 LB (TCP pass-through) on `:443` → broker fleet.
- Each broker writes `host_routes(host_id, broker_instance_id)` to Corrosion
  on `Hello`. Inter-broker command forwarding resolves which instance owns
  a given `host_id`.
- Laravel fleet behind L7 LB on `:443`; broker fleet on separate VIP.

### Redis protocol (Laravel → broker → coold)

| Key | Type | Producer | Consumer |
|---|---|---|---|
| `coold:cmd` | Stream | Laravel (`XADD`) | broker (`XREADGROUP GROUP broker`) |
| `coold:resp:{request_id}` | List | broker (`LPUSH`) | Laravel (`BLPOP`) |
| `coold:hosts` | Hash | broker (on `Hello`) | Laravel (dashboard) |

**Dispatch flow:**
1. Laravel `XADD coold:cmd * payload <json>` where JSON = `{host_id, request_id, command: {type: ...}}`.
2. Broker consumes, looks up `host_id` in `DashMap`, sends `ServerMsg` over gRPC.
3. coold replies with `Response`; broker `LPUSH coold:resp:{request_id}` with JSON result.
4. Laravel `BLPOP coold:resp:{request_id} 30` (30 s timeout).
5. Unknown host or timeout → broker pushes `{status: "error", code: 404|504}`.

Response keys expire after 300 s (`EXPIRE`) so stale entries self-clean.

### coold config change

`COOLD_CENTRAL_URL` renamed to `COOLD_BROKER_URL`. Update enrollment and
`coolify init` templates when central enrollment is implemented. Semantics
identical; only the name changes to reflect the actual target.

### Repository layout

```
Cargo.toml        # workspace root (members: coold, broker, proto)
proto/
  agent.proto     # shared Protobuf definitions
  Cargo.toml      # coolify-proto crate (runs tonic-build)
  src/lib.rs
coold/
  Cargo.toml      # depends on coolify-proto
  src/
broker/
  Cargo.toml      # broker binary; depends on coolify-proto
  src/
    main.rs       # tonic AgentServer + spawns redis_bridge
    config.rs     # BROKER_GRPC_BIND, BROKER_REDIS_URL, BROKER_JWT_PUBLIC_KEY_PATH
    state.rs      # Streams: DashMap<host_id, mpsc::Sender<ServerMsg>>
    auth.rs       # JWT verify (ES256/RS256, sub = host_id)
    redis_bridge.rs  # XREADGROUP consumer + push_response
```

## 16. Cross-references

- Bootstrap + CLI: `coolify-cli/CLAUDE.md`, `coolify-cli/CONTROL_PLANE.md`.
- `coolify firewall` CLI (alpha, SSH-bounced REST client of local coold):
  `coolify-cli/CLAUDE.md` § "`coolify firewall`".
- Wire surface + transport: mirror of §3, §4 here ↔ `CONTROL_PLANE.md §2`.
