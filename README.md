## What is coold?

Per-host agent of Coolify v5. Kubelet-analogue for a WireGuard mesh of Podman hosts. One `coold` process per node. Narrow by design: executes local runtime primitives, never reasons about apps, builds, or deploys.

Today, coold owns service-discovery sync, embedded DNS, and the outbound flux stream. Firewall runtime mutation and builder orchestration are intentionally deferred until explicit gRPC primitives and ADRs exist. It is the only process on a node with access to the Podman socket, the iptables/nft kernel interface, and the local Corrosion agent.

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
│  Firewall scaffold · DNS · Corrosion sync · gRPC client    │
└────────┬───────────────────────────┬───────────────────────┘
         │ UDS                       │ HTTP
         ▼                           ▼
   podman.sock                 corrosion agent
                                    │
                                    │ SWIM gossip
                                    ▼
                               other nodes
```

---

## Repo layout (Cargo workspace)

```
proto/          Shared Protobuf: Agent.Stream, Hello, ServerMsg, ClientMsg,
                Response, and host capabilities.
coold/          Per-host agent.
flux/          gRPC server coold dials + UDS lane for Laravel.
builder/       Deferred one-shot OCI build CLI reference; not active in v5 runtime.
builder-core/  Deferred reusable git + buildah pipeline reference.
coolify-cli/   Rust v5 cluster CLI: WireGuard/Podman/coold/Corrosion init.
               Does not include v4 Coolify API/context/project commands.
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
POST /v1/coold/dispatch           e.g. containers.list
```

Live host reads flow: Laravel → flux UDS → coold outbound gRPC stream → Podman.

### Local development

Coolify owns local/VM dev orchestration for the full stack. This repo does not
provide dev launcher scripts; use the Coolify repo to start the development
environment, then work on the Rust binaries here as needed.

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


# Dev/NAT cases can override per-node WireGuard listen ports and peer endpoints.
coolify init bootstrap --nodes node-a,node-b --ssh-key KEY \
  --wg-listen-port-overrides node-a=51821,node-b=51822 \
  --wg-endpoint-overrides node-a=host.lima.internal:51821,node-b=host.lima.internal:51822
```

The CLI shares the v5 mesh model: bootstrap over SSH and deployment nodes via
`--nodes` (alias `--servers`). `coolify init` converges node runtime only:
WireGuard, Podman, namespace bridges, the firewall scaffold, Corrosion, and coold. Runtime mutations are expected to
flow through Flux over coold's outbound gRPC stream, not through a host-local
API. The CLI no longer installs or configures central `flux`; connect a node to
flux separately with `COOLIFY_COOLD_FLUX_URL` or
`COOLIFY_COOLD_ASSIGNMENT_URL` plus a host JWT.

---

## coold — three core jobs

### 1. Service-discovery sync

Watches Podman lifecycle events (`start` / `die` / `remove`) plus 2s periodic reconcile. Writes own host's rows to Corrosion `service_endpoints` table. Gossip replicates to peers. Retries on next tick if Corrosion down.

### 2. Embedded cluster DNS

One hickory-server task per namespace, bound to that bridge's gateway IP (e.g. `10.210.0.1:53`) — never `0.0.0.0`. Resolves `<container>.<namespace>.coolify.internal` from Corrosion, filtered `state='running' AND health IN ('healthy','unknown')`. Bare `<container>.coolify.internal` is intentional NXDOMAIN. Out-of-zone forwarded to upstream (`1.1.1.1:53`). Self-healing rebind with exponential backoff when netavark tears down a bridge. IPv4 only (AAAA → NODATA).

### 3. Firewall enforcement scaffold

The v5 bootstrap firewall scaffold installs the cross-host and same-bridge enforcement planes. Future firewall mutations should be exposed as gRPC primitives through Flux. When implemented, every mutation must write two kernel planes atomically:

| Plane | Mechanism | Traffic path |
| --- | --- | --- |
| Cross-host | iptables `COOLIFY-ALLOW` (filter) | wg0 ↔ bridge |
| Intra-host same-bridge | nft `coolify_bridge::coolify_allow` (bridge family) | Same-bridge traffic bypassing FORWARD |

Snapshots: `/etc/coolify/allow.rules` + `/etc/coolify/allow.nft`. Restored on boot by `coolify-mesh-fw.service` + `coolify-mesh-allow.service`. Rule ID = `sha256("namespace|src|dst|proto|port")[:12]`. Tuples only; audit / RBAC / owners live in Laravel.

---

## Transport

**Outbound gRPC stream.** coold can dial `http(s)://flux:6443` at startup with a per-host JWT and open `Agent.Stream`. The stream starts only when `COOLIFY_COOLD_FLUX_URL` or `COOLIFY_COOLD_ASSIGNMENT_URL` is configured and `COOLIFY_COOLD_GRPC_DISABLED=false`. Flux routes command frames down the open stream. Works through NAT and corporate firewalls — flux never opens inbound to a host.

---

## Flux

Central connection-holder. Laravel (PHP-FPM request/response model) can't hold thousands of long-lived HTTP/2 streams; flux does.

- `:6443` gRPC — single listener for coold streams. The bind must be a specific interface IP unless `COOLIFY_FLUX_ALLOW_PUBLIC_BIND=1` is set for dev/test.
- `/run/coolify/flux.sock` UDS — Laravel's sync + async lane. Mode `0660` when `COOLIFY_FLUX_UNIX_SOCKET_GROUP` set, else `0600`. No TLS, no bearer — filesystem perms replace auth.
- `Streams`: DashMap<host_id, StreamHandle{tx, caps}>.
- `Pending`: DashMap<request_id, Waiting | Landed>. Cap `COOLIFY_FLUX_PENDING_MAX=10_000`. Landed entries hold 30 s TTL so late pollers still claim results.
- Sweeper evicts `Waiting` coold-lane entries after 10 s → 504.
- JWT verify (ES256/RS256) with `sub=host_id` + `caps` claim.

### UDS wire surface (Laravel → flux)

```
GET  /v1/health
POST /v1/coold/dispatch          sync, 10 s timeout
```

### Coold dispatch flow

Laravel POST → flux checks `Streams::get(host_id)` (miss → 404) → `Pending::insert_waiting` (cap overflow → 503) → parks oneshot → pushes `ServerMsg` onto host's mpsc → coold runs command against podman.sock → writes `Response` on same stream → flux fires parked sinks, transitions to `Landed` with 30 s TTL. 10 s no-response → 504. Stream dropped mid-dispatch → 503.

---

## coold wire surface (implemented)

The implemented control surface is intentionally small. Future Podman lifecycle
verbs should be added explicitly; there is no raw Podman passthrough.

### gRPC via flux (`Agent.Stream`)

```protobuf
ServerMsg.images_pull                 -> Response.images_pull
ServerMsg.images_list                 -> Response.images_list
ServerMsg.images_delete               -> Response.images_delete
ServerMsg.containers_create           -> Response.containers_create
ServerMsg.containers_start            -> Response.containers_start
ServerMsg.containers_stop             -> Response.containers_stop
ServerMsg.containers_restart          -> Response.containers_restart
ServerMsg.containers_delete           -> Response.containers_delete
ServerMsg.containers_inspect          -> Response.containers_inspect
ServerMsg.containers_list             -> Response.containers_list
ServerMsg.containers_logs             -> Response.containers_logs
ServerMsg.containers_exec             -> Response.containers_exec
ServerMsg.containers_healthcheck_run  -> Response.containers_healthcheck_run
ServerMsg.ingress_apply              -> Response.ingress_apply
ServerMsg.ingress_stop               -> Response.ingress_stop
```

`containers.list` returns Podman container summaries plus inspected network
attachments. `containers.create` applies the coold deny filter for privileged
mode, host networking, custom capabilities, and unsafe host mounts before it
calls Podman. Ingress commands dispatch to the requested ingress kind; Caddy is the first supported kind.

Not implemented yet in this codebase: volume CRUD, network CRUD, firewall
mutation over gRPC, explicit service endpoint CRUD, DNS diagnostics, and host
facts. Those remain architecture goals, not current API.

---

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
| `COOLIFY_COOLD_FLUX_URL` | unset | Static flux URL for self-hosted/private mode |
| `COOLIFY_COOLD_ASSIGNMENT_URL` | unset | Hosted-cloud assignment endpoint; returns current flux URL |
| `COOLIFY_COOLD_HOST_JWT_PATH` | `/etc/coolify/host-jwt` | Per-host JWT for outbound gRPC |
| `COOLIFY_COOLD_GRPC_DISABLED` | `false` | Disable outbound gRPC even when flux URL/assignment is set |

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

- **`install.rs`** — Hetzner-provisioned. Networking assertions after `coolify init bootstrap`. VMs destroyed on drop.

Env: `HETZNER_TOKEN`, `HETZNER_PROJECT`, `SSH_KEY`, `COOLIFY_CLI_BIN`, optional location/image/server-type.

---

## Non-goals

- No Compose parser in coold (Laravel-side).
- No Dockerfile / Buildpacks / Nixpacks in coold. Builder is deferred and will return behind a dedicated ADR/API.
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

Flux reports host `capabilities`, optional `coold_version`,
connection/disconnection reasons, and aggregate stream counts to Laravel.
