# Coolify API

Crate/package: `api`.

`api` is the first Coolify API + Coolify UI shell for Coolify v5.

It is intentionally separate from `coold`:

- `coold` runs on every host and executes local primitives.
- `scheduler` holds outbound streams from coold.
- `cooldctl` bootstraps and extends clusters over SSH.
- `api` is the central API/UI binary for operators and the future
  Coolify v5 product surface.

## Commands

```bash
api serve
api healthcheck --url http://127.0.0.1:3000/healthz
api db migrate
api db info
```

Environment:

```bash
COOLIFY_API_BIND=127.0.0.1:3000
COOLIFY_API_DB=api.db
COOLIFY_API_AUTO_MIGRATE=1
COOLIFY_SCHEDULER_SOCKET=/run/coolify/scheduler.sock
COOLIFY_SCHEDULER_TIMEOUT_MS=12000
```

Live host reads flow through scheduler, not directly to coold:

```txt
Coolify UI → Coolify API → scheduler UDS → coold gRPC stream → Podman
```

## API

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

## Coolify UI

The Coolify UI lives in `../coolify-ui` and is embedded into the binary from
`coolify-ui/dist` using `rust-embed`.

Development:

```bash
bun run dev
```

Backend-only iteration:

```bash
SKIP_UI=1 rtk cargo run -p api -- serve
```


### Scheduler stream sync

Connected agents become visible through this flow:

```txt
coold connects → scheduler streams → POST /api/v1/servers/sync-streams → SQLite servers → Coolify UI Servers page → live container endpoint
```

The sync endpoint creates or updates servers by scheduler `host_id`, stores
capabilities, sets `last_seen_at`, marks the server online, and records an
event.


## One-command full-stack dev

From the workspace root:

```bash
bun run dev
```

This starts scheduler, a fake coold agent, api, and the Coolify UI.
Use env overrides when a port is busy:

```bash
COOLIFY_UI_PORT=5174 COOLIFY_API_BIND=127.0.0.1:3001 SCHEDULER_GRPC_BIND=127.0.0.1:6444 bun run dev
```
