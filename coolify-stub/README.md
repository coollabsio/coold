# coolify-stub

Stub Coolify "brain" used to drive coold end-to-end in place of the real
Laravel control plane. Ships as a single Bun binary with an embedded SPA and
talks to the coold scheduler over its HTTP-over-UDS lane.

## Dev loop

Backend (auto-reload):

```
bun install
bun run dev
```

Frontend (separate terminal, Vite with `/api` proxy to `http://localhost:3000`):

```
cd web
bun install
bun run dev
```

Requires a scheduler socket at `$SCHEDULER_SOCKET_PATH`. For local work, either run
the scheduler against a scratch path or `ssh -L` the Hetzner socket.

## Env vars

| Var                     | Default                       | Purpose                                  |
| ----------------------- | ----------------------------- | ---------------------------------------- |
| `PORT`                  | `3000`                        | TCP port to listen on                    |
| `HOST`                  | `0.0.0.0`                     | Bind address                             |
| `SCHEDULER_SOCKET_PATH` | `/run/coolify/scheduler.sock` | Coold scheduler UDS path                 |
| `COOLIFY_HOSTS`         | (empty)                       | Comma-separated host IDs served in UI    |

## Build single binary

```
BUN_TARGET=bun-linux-x64 bun run build
```

Frontend is built, `web/dist` is embedded via generated
`src/embedded.ts`, and Bun `--compile` bakes everything into
`dist/coolify-stub`. Caller picks the target per invocation.

## Deploy

`scp dist/coolify-stub root@<hetzner-vm>:/usr/local/bin/coolify-stub` next to
the running scheduler, then run it with the env vars above. No runtime deps.
