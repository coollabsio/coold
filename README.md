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

### Provisioning hardening

The following are **always on** and do not change what a default bootstrap
produces beyond the hardening itself:

- **coold systemd sandbox (S7).** The generated `coold.service` runs as root
  (it needs the Podman socket, iptables/nft, and DNS bind via
  `AmbientCapabilities`) but adds `NoNewPrivileges=yes`, `ProtectSystem=strict`,
  `ProtectHome=yes`, `PrivateTmp=yes`, and a tight `ReadWritePaths` allowlist
  (`/etc/coolify`, `/data/coolify`, `/var/lib/containers`, plus lazily-created
  podman runtime dirs). The install step pre-creates `/etc/coolify` and
  `/data/coolify` so `ProtectSystem=strict` can bind-mount them read-write.
- **WireGuard private-key umask (S1).** Key generation runs under `umask 077`
  so `/etc/wireguard/privatekey` is never briefly world-readable.
- **Namespace / interface / version validation (S-cli).** `--namespaces`,
  `--wg-interface`, `--coold-version`, and `--corrosion-version` are validated
  against strict charsets before they are interpolated into remote shell or
  unit files, rejecting shell/unit-injection payloads up front.

Opt-in flags (default OFF — a bare `coolify init bootstrap` is byte-identical
to before except for the always-on items above):

| Flag | Finding | Effect |
| --- | --- | --- |
| `--coold-sha256 <hex>` / `--corrosion-sha256 <hex>` | S1 supply-chain | Pin the release tarball digest; install aborts on mismatch. Unpinned installs still verify a published `<url>.sha256` sidecar when present. |
| `--enable-corrosion-gossip-tls` | S5 | Wrap Corrosion gossip in a second (QUIC/TLS) encryption layer with a shared self-signed cert provisioned to every node (`/etc/corrosion/tls/`) and `plaintext = false`. Encryption-only defense-in-depth — see the trust-boundary note below. Verify it loads on your Corrosion version before relying on it. |
| `--enable-flux-tls` `--flux-tls-san <mesh-ip/host>` [`--flux-port N`] [`--flux-tls-out-dir DIR`] | S1 | Generate a self-signed flux cert, drop the coold pin file (`/etc/coolify/flux.pin`) on every node, **auto-set each node's `COOLIFY_COOLD_FLUX_URL` to `https://<san>:<port>` and the pin-path env** so coold dials TLS, and write `cert.pem`/`key.pem` locally for manual install on the flux host. Requires a non-localhost `--flux-tls-san`. |

**Corrosion gossip trust boundary (S5).** The enforced trust boundary is the
bind address: the gossip listener binds each host's **WireGuard mgmt IP only**
(never `0.0.0.0`), so only mesh members can reach the port and any mesh host is
trusted to write truthful `service_endpoints` rows. `--enable-corrosion-gossip-tls`
adds a QUIC/TLS **encryption** layer on top of the (already encrypted) WireGuard
tunnel, but does **not** add per-node authentication: Corrosion v1.0.0's verifier
only checks CA membership, and every node holds the same shared cert, so TLS
cannot tell mesh members apart. Because a no-SAN shared cert cannot satisfy
Corrosion's server-name verification, the generated config is deliberately
encryption-only (`insecure = true`, no `ca_file`, no `[gossip.tls.client]`) — the
only shared-cert shape v1.0.0 will load and handshake cross-node. Real per-node
gossip auth would need per-node IP-SAN certs signed by a shared CA and a live
cluster to validate; it is out of scope. Do not extend the mesh to untrusted
hosts and expect gossip TLS to contain them.

**Flux↔coold TLS (S1).** `--enable-flux-tls` now wires the **node side
automatically**: each node's coold unit gets `COOLIFY_COOLD_FLUX_URL=https://<san>:<port>`
plus `COOLIFY_COOLD_FLUX_TLS_PIN_PATH`, and the pin file (`/etc/coolify/flux.pin`)
is dropped on every node so coold dials flux over pinned TLS. The remaining
**manual** steps (the CLI does not run flux or mint host JWTs): install
`cert.pem`/`key.pem` on the flux host (`COOLIFY_FLUX_TLS_CERT_PATH` /
`COOLIFY_FLUX_TLS_KEY_PATH`, binding its mesh IP — the SAN — not localhost), and
provision each node's per-host JWT at `/etc/coolify/host-jwt` (coold exits at
boot if the flux URL is set but the JWT is missing).

---

## coold — three core jobs

### 1. Service-discovery sync

Watches Podman lifecycle events (`start` / `die` / `remove`) plus 2s periodic reconcile. Writes own host's rows to Corrosion `service_endpoints` table. Gossip replicates to peers. Retries on next tick if Corrosion down.

### 2. Embedded cluster DNS

One hickory-server task per namespace, bound to that bridge's gateway IP (e.g. `10.210.0.1:53`) — never `0.0.0.0`. Resolves `<container>.<namespace>.coolify.internal` from Corrosion, filtered `state='running' AND health IN ('healthy','unknown')`. Bare `<container>.coolify.internal` is intentional NXDOMAIN. Out-of-zone forwarded to upstream (`1.1.1.1:53`). Self-healing rebind with exponential backoff when netavark tears down a bridge. IPv4 only (AAAA → NODATA).

Read-side host cross-check (S5): endpoint reads carry the owning `host_mgmt_ip`; `query_ips_by_name_owned` can bind a lookup to an expected owner so a compromised mesh peer's forged row for another host's service is dropped.

- **DNS stays permissive (by design).** A cluster query `<container>.<namespace>.coolify.internal` may legitimately resolve to whichever host runs the container, and a service may have replicas on several hosts — there is no independent expected-owner signal for a DNS name, so a hard owner filter would break normal cross-host discovery. The DNS resolver therefore looks up with `expected_owner = None`: all healthy IPs are returned, but a warning is logged when they span multiple owners so poisoning attempts stay observable.
- **Firewall `dst` is enforced.** `firewall.allow` is always dispatched to the host that runs the rule's `dst` container (coold is the sole kernel writer there — see ARCHITECTURE §8 T8), so a truthful `service_endpoints` row for `dst` must be owned by THIS host. coold resolves the `dst` name with `expected_owner = host_mgmt_ip`, so a row forged by another host is dropped with a `warn!`. The `src` endpoint is a legitimately cross-host source (an ingress proxy or a peer app) and stays permissive. Enforcement is controlled by `COOLIFY_COOLD_STRICT_ENDPOINT_OWNER` (default `true`); set it to `false` as an escape hatch if a real cluster topology ever trips the check.

RESIDUAL TRUST: fully closing the gap requires authenticating Corrosion SWIM gossip (owned by the `coolify-cli` bootstrap agent, not coold); this read-side check only binds rows to a host when the expected owner is known (the firewall `dst` path) and otherwise surfaces multi-owner results without blocking.

### 3. Firewall enforcement scaffold

The v5 bootstrap firewall scaffold installs the cross-host and same-bridge enforcement planes. Future firewall mutations should be exposed as gRPC primitives through Flux. When implemented, every mutation must write two kernel planes atomically:

| Plane | Mechanism | Traffic path |
| --- | --- | --- |
| Cross-host | iptables `COOLIFY-ALLOW` (filter) | wg0 ↔ bridge |
| Intra-host same-bridge | nft `coolify_bridge::coolify_allow` (bridge family) | Same-bridge traffic bypassing FORWARD |

Snapshots: `/etc/coolify/allow.rules` + `/etc/coolify/allow.nft`. Restored on boot by `coolify-mesh-fw.service` + `coolify-mesh-allow.service`. Rule IDs are caller-provided stable handles from Laravel. Tuples only; audit / RBAC / owners live in Laravel.

---

## Transport

**Outbound gRPC stream.** coold can dial `http(s)://flux:6443` at startup with a per-host JWT and open `Agent.Stream`. The stream starts only when `COOLIFY_COOLD_FLUX_URL` or `COOLIFY_COOLD_ASSIGNMENT_URL` is configured and `COOLIFY_COOLD_GRPC_DISABLED=false`. Flux routes command frames down the open stream. Works through NAT and corporate firewalls — flux never opens inbound to a host.

---

## Flux

Central connection-holder. Laravel (PHP-FPM request/response model) can't hold thousands of long-lived HTTP/2 streams; flux does.

- `:6443` gRPC — single listener for coold streams. The bind must be a specific interface IP unless `COOLIFY_FLUX_ALLOW_PUBLIC_BIND=1` is set for dev/test. Optional defense-in-depth TLS (S1) turns on when `COOLIFY_FLUX_TLS_CERT_PATH` + `COOLIFY_FLUX_TLS_KEY_PATH` are both set; otherwise plaintext (WireGuard is the confidentiality boundary).
- `/run/coolify/flux.sock` UDS — Laravel's sync + async lane. Mode `0660` when `COOLIFY_FLUX_UNIX_SOCKET_GROUP` set, else `0600`. No TLS, no bearer — filesystem perms replace auth.
- `Streams`: DashMap<host_id, StreamHandle{tx, caps}>.
- `Pending`: DashMap<request_id, Waiting | Landed>. Cap `COOLIFY_FLUX_PENDING_MAX=10_000`. A response delivered to a live parked handler drops its entry immediately; the 30 s `Landed` TTL is kept only for the late-poll race (response beat the parker).
- Sweeper evicts `Waiting` coold-lane entries after `COOLIFY_FLUX_DISPATCH_TIMEOUT_SECS` (default 30 s) → 504.
- **Auth (secure-by-default).** JWT verify pins alg to ES256/RS256/PS* (HMAC/`none`/EdDSA rejected). Header `kid` selects the verification key (`flux-default`, or a `<kid>.pub` from `COOLIFY_FLUX_JWT_KEYS_DIR`); unknown `kid` → reject. The `caps` claim is authoritative: flux authorizes only the intersection of `caps` with the host's advertised primitives. Wildcard profiles (`*`, `host-agent:dev`, `host-agent:default`) grant **nothing** unless `COOLIFY_FLUX_ALLOW_WILDCARD_CAPABILITIES=1` (dev/rollback escape hatch). A token whose remaining lifetime exceeds `COOLIFY_FLUX_MAX_TOKEN_LIFETIME_SECS` (default 3600) is rejected at connect, and the stream is dropped when the token `exp`s (coold reconnects with a fresh token). Revoked `jti`s (denylist) are rejected.
- **Host binding (#2).** At stream connect the token's `sub` (the host id it was minted for) is checked against the host flux independently observes. Preferred signal is the gRPC **transport peer IP** (over the WireGuard mesh this equals the host's mgmt IP, which by design equals `sub`) — a stolen token replayed from a different host IP is rejected. If the transport peer address is unavailable (or `sub` is not an IP), flux degrades to the weaker check that `sub` equals the Hello-advertised `host_mgmt_ip` (self-asserted) and logs a warning. Enforced by `COOLIFY_FLUX_REQUIRE_HOST_BINDING` (default **on**); set `=0` to warn-only. When the signal is genuinely unavailable the stream degrades gracefully rather than breaking the mesh.
- **Tenant binding (#2).** The host JWT must carry a non-empty `team_id` (tenant) claim; a token lacking it is rejected. Enforced by `COOLIFY_FLUX_REQUIRE_TEAM_ID` (default **on**); set `=0` to tolerate legacy tokens during dev. The `team_id` is logged on connect for auditability.
- **Per-cluster signing keys — scaffold only (#2.3).** The `kid` mechanism above already supports per-cluster keys: Laravel would mint with `kid = cluster-<id>`, flux loads `cluster-<id>.pub` from `COOLIFY_FLUX_JWT_KEYS_DIR`, and an unknown `kid` is rejected — isolating a key compromise to a single cluster. **Key distribution is not built**: publishing each cluster's public key into every flux's keys dir is a provisioning/ops task (coolify-cli node provisioning). Until it lands, all clusters share the single `flux-default` key.
- **Schema negotiation (R6).** At Hello, coold's advertised `[schema_min, schema_max]` must overlap flux's supported range (`1..=1`); no overlap → stream rejected.

### UDS wire surface (Laravel → flux)

```
GET    /v1/health
GET    /v1/streams                 connected coold agents
POST   /v1/coold/dispatch          sync, COOLIFY_FLUX_DISPATCH_TIMEOUT_SECS timeout
POST   /v1/tokens/revoke           {jti, expires_at?} — add jti to revocation denylist
GET    /v1/tokens/revoke           list current denylist entries
DELETE /v1/tokens/revoke/:jti      remove jti from denylist
```

The revocation denylist is persisted to `COOLIFY_FLUX_REVOCATION_PATH` (JSON) so it survives restarts; entries past their `expires_at` are pruned on load and by a 60 s sweeper.

### Coold dispatch flow

Laravel POST → flux checks `Streams::get(host_id)` (miss → 404) → `Pending::insert_waiting` (cap overflow → 503) → parks oneshot → `try_send`s `ServerMsg` onto host's bounded mpsc (queue full → fast 503, never blocks the php-fpm worker) → coold runs command against podman.sock → writes `Response` on same stream → flux fires parked sinks and drops the entry. `DISPATCH_TIMEOUT_SECS` no-response → 504. Stream dropped mid-dispatch → 503.

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
ServerMsg.host_jwt_set               -> Response.host_jwt_set
```

`containers.list` returns Podman container summaries plus inspected network
attachments. `containers.create` applies the coold deny filter before it calls
Podman: it rejects privileged mode, host networking (including the
`container:<id>` / `ns:<path>` namespace-join forms), custom capabilities, and
host bind mounts. Host bind mounts are **deny-by-default** — a source directory
is only accepted when it canonicalizes (symlinks + `..` resolved) under a prefix
listed in `COOLIFY_COOLD_ALLOWED_MOUNT_SOURCES` (empty by default = no host bind
mounts). Named volumes are unaffected. Ingress commands dispatch to the
requested ingress kind; Caddy is the first supported kind.

`host.jwt.set` delivers a rotated host JWT to coold **over the existing gRPC
stream** (gated by the `host.jwt.set` capability). coold validates the pushed
token before writing — it rejects an empty or structurally invalid JWT (it must
be three non-empty base64url segments `xxx.yyy.zzz`), and, when it can read its
currently-installed token, requires the new token's `sub` to match its own host
(a token minted for a different host is refused). The signature is not verified
(coold has no verification key; flux already authenticated the dispatch). A
valid token is written atomically (temp file, mode 0600, `rename` over the
target) so a reader never sees a half-written token. See the rotation flow
below.

Not implemented yet in this codebase: volume CRUD, network CRUD, firewall
mutation over gRPC, explicit service endpoint CRUD, DNS diagnostics, and host
facts. Those remain architecture goals, not current API.

### Host JWT rotation

Laravel rotates a host token while the current one is still valid (the stream is
up) and pushes the new token with `{"type":"host.jwt.set","jwt":"<JWT>"}` on the
coold dispatch lane. coold writes it to `COOLIFY_COOLD_HOST_JWT_PATH` but does
**not** reconnect or restart — the current stream keeps running on the old token.
The new token is picked up on the **next reconnect**, which the current token's
`exp` drives: flux drops the stream at `exp`, and coold re-reads the JWT file on
every reconnect attempt. This makes rotation seamless. SSH delivery of the token
remains the Laravel-side fallback for when the stream is down.

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
| `COOLIFY_COOLD_HOST_JWT_PATH` | `/etc/coolify/host-jwt` | Per-host JWT for outbound gRPC. Re-read on every reconnect so a rotated token is picked up without restart. Also the target the `host.jwt.set` RPC atomically writes when Laravel rotates the token over the stream |
| `COOLIFY_COOLD_GRPC_DISABLED` | `false` | Disable outbound gRPC even when flux URL/assignment is set |
| `COOLIFY_COOLD_ALLOWED_MOUNT_SOURCES` | empty | Colon/comma-separated host dir prefixes allowed as `containers.create` bind-mount sources. Empty = deny all host bind mounts (secure default). Sources are canonicalized before matching |
| `COOLIFY_COOLD_FLUX_TLS_PIN_PATH` | `/etc/coolify/flux.pin` | PEM cert/CA pinning the flux gRPC TLS connection. Active only when the file exists; absent = plaintext (WireGuard-protected). `https://` flux URL with no pin fails closed |
| `COOLIFY_COOLD_STRICT_ENDPOINT_OWNER` | `true` | S5 read-side owner cross-check for firewall `dst` name resolution. When `true`, a `dst` service name must resolve to a `service_endpoints` row owned by this host (`host_mgmt_ip`); rows forged by another host are dropped. `src` and cluster DNS stay permissive. Set `false` to disable if a real topology trips the check |

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
| `COOLIFY_FLUX_JWT_PUBLIC_KEY_PATH` | `/etc/coolify/jwt.pub` | Default JWT verification key (kid `flux-default`) |
| `COOLIFY_FLUX_JWT_KEYS_DIR` | unset | Optional dir of `<kid>.pub` rotation keys (S3); token header `kid` selects one, unknown `kid` rejected |
| `COOLIFY_FLUX_ALLOW_WILDCARD_CAPABILITIES` | unset (false) | S2 escape hatch: when true, wildcard-profile tokens expand to all advertised caps. Default false = caps claim is authoritative, wildcards grant nothing |
| `COOLIFY_FLUX_MAX_TOKEN_LIFETIME_SECS` | `3600` | #4 clamp: reject at connect any JWT whose remaining lifetime exceeds this (`0` disables) |
| `COOLIFY_FLUX_REVOCATION_PATH` | `/var/lib/coolify/flux/revocations.json` | #3 on-disk JWT `jti` revocation denylist (persisted across restarts) |
| `COOLIFY_FLUX_TLS_CERT_PATH` | unset | S1 optional gRPC TLS cert (PEM). TLS on only when both cert+key set |
| `COOLIFY_FLUX_TLS_KEY_PATH` | unset | S1 optional gRPC TLS key (PEM) paired with the cert |
| `COOLIFY_FLUX_LOG_LEVEL` | `info` | tracing EnvFilter |
| `COOLIFY_FLUX_DISPATCH_TIMEOUT_SECS` | `30` | Seconds a coold-lane dispatch waits before 504 |

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
