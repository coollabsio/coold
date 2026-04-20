# coold architecture

> CLI-side counterpart: `coolify-cli/CONTROL_PLANE.md`. Both docs describe the
> same split from different vantage points; keep them in sync when the wire
> surface changes.

## 1. Role of coold

coold is the **kubelet-analogue** of Coolify v5: a narrow, per-host agent that
proxies a curated set of primitives over the local `/run/podman/podman.sock`
Unix socket, programs `COOLIFY-ALLOW` in iptables, writes service-endpoint rows
to Corrosion, and answers `.coolify.internal` DNS on the bridge gateway IP.

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
| iptables COOLIFY-ALLOW writes + `/etc/coolify/allow.rules` snapshot | **coold** | Sole kernel writer. |
| Corrosion row writes (service endpoints) scoped to own host | **coold** | Gossip distributes. |
| Embedded DNS on bridge gateway `:53` | **coold** | Reads local Corrosion, answers `.coolify.internal`. |
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

# Firewall (coold = sole writer)
POST   /api/v1/firewall/allow        {src, dst, proto?, port?}  -> {id}
DELETE /api/v1/firewall/allow/{id}
GET    /api/v1/firewall/allow

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

- **Default**: shared `coolify-mesh` bridge created at bootstrap by
  `coolify init apply --podman`. Containers get `.coolify.internal` DNS + flat
  L3 across the mesh.
- **Per-app namespaces**: users may opt into extra podman networks (docker-
  compose `networks:` style). Central compiles the compose block into
  `POST /networks` + attach-on-create. coold is the only writer; bootstrap
  doesn't own these.
- **Egress from containers**: bridge-NAT to the host's default route. Cross-
  host container traffic rides wg0 via peer `AllowedIPs`.
- **coold DNS binds the bridge gateway IP only** (`10.210.X.1:53`); never
  `0.0.0.0`. REST API binds the wg0 mgmt IP only. Separate concerns, separate
  sockets.

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

- **Allow-rule snapshot**: coold rewrites `/etc/coolify/allow.rules` (flat
  `iptables-save` fragment — `:COOLIFY-ALLOW` + `-A COOLIFY-ALLOW` lines only)
  on every successful API mutate. Atomic write (`.tmp` + `mv`).
- **Boot restore**: `coolify-mesh-allow.service` is a `Type=oneshot` unit
  ordered `After=coolify-mesh-fw.service`, running
  `iptables-restore --noflush /etc/coolify/allow.rules`. `--noflush` preserves
  everything else in the filter table.
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

## 15. Cross-references

- Bootstrap + CLI: `coolify-cli/CLAUDE.md`, `coolify-cli/CONTROL_PLANE.md`.
- `coolify firewall` CLI (alpha, SSH-bounced REST client of local coold):
  `coolify-cli/CLAUDE.md` § "`coolify firewall`".
- Wire surface + transport: mirror of §3, §4 here ↔ `CONTROL_PLANE.md §2`.
