# coolify-web

`coolify-web` is the first Rust API + React UI shell for Coolify v5.

It is intentionally separate from `coold`:

- `coold` runs on every host and executes local primitives.
- `scheduler` holds outbound streams from coold.
- `cooldctl` bootstraps and extends clusters over SSH.
- `coolify-web` is the central API/UI binary for operators and the future
  Coolify v5 product surface.

## Commands

```bash
coolify-web serve
coolify-web healthcheck --url http://127.0.0.1:3000/healthz
coolify-web db migrate
coolify-web db info
```

Environment:

```bash
COOLIFY_WEB_BIND=127.0.0.1:3000
COOLIFY_WEB_DB=coolify-web.db
COOLIFY_WEB_AUTO_MIGRATE=1
COOLIFY_SCHEDULER_SOCKET=/run/coolify/scheduler.sock
COOLIFY_SCHEDULER_TIMEOUT_MS=12000
```

Live host reads flow through scheduler, not directly to coold:

```txt
React → coolify-web → scheduler UDS → coold gRPC stream → Podman
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

## Frontend

The React frontend lives in `../frontend` and is embedded into the binary from
`frontend/dist` using `rust-embed`.

Development:

```bash
bun run dev
```

Backend-only iteration:

```bash
SKIP_FRONTEND=1 rtk cargo run -p coolify-web -- serve
```


### Scheduler stream sync

Connected agents become visible through this flow:

```txt
coold connects → scheduler streams → POST /api/v1/servers/sync-streams → SQLite servers → React Servers page → live container endpoint
```

The sync endpoint creates or updates servers by scheduler `host_id`, stores
capabilities, sets `last_seen_at`, marks the server online, and records an
event.
