// Entry point: wires the scheduler client, API dispatcher, and embedded SPA
// assets into a single Bun.serve listener. Designed to compile down to a
// static binary via `bun build --compile`.

import { createSchedulerClient } from "./scheduler.ts";
import { embedded } from "./embedded.ts";
import { createApiHandler } from "./routes.ts";

const PORT = Number(process.env.PORT ?? 3000);
const HOST = process.env.HOST ?? "0.0.0.0";
const SCHEDULER_SOCKET_PATH = process.env.SCHEDULER_SOCKET_PATH ?? "/run/coolify/scheduler.sock";
const HOSTS = (process.env.COOLIFY_HOSTS ?? "")
  .split(",")
  .map((h) => h.trim())
  .filter((h) => h.length > 0);

const scheduler = createSchedulerClient(SCHEDULER_SOCKET_PATH);
const api = createApiHandler(scheduler, HOSTS);

function serveEmbedded(pathname: string): Response | null {
  const entry = embedded[pathname];
  if (!entry) return null;
  return new Response(Bun.file(entry.path), {
    headers: { "Content-Type": entry.type },
  });
}

const server = Bun.serve({
  port: PORT,
  hostname: HOST,
  async fetch(req) {
    const apiResponse = await api(req);
    if (apiResponse) return apiResponse;

    const url = new URL(req.url);
    const direct = serveEmbedded(url.pathname);
    if (direct) return direct;

    // SPA fallback: requests without a file extension fall back to index.
    const hasExtension = /\.[a-zA-Z0-9]+$/.test(url.pathname);
    if (!hasExtension) {
      const index = serveEmbedded("/");
      if (index) return index;
    }

    return new Response("Not found", { status: 404 });
  },
});

console.log("coolify-stub listening");
console.log(`  addr      http://${HOST}:${PORT}`);
console.log(`  scheduler ${SCHEDULER_SOCKET_PATH}`);
console.log(`  hosts     ${HOSTS.length === 0 ? "(none)" : HOSTS.join(", ")}`);
console.log(`  assets    ${Object.keys(embedded).length} embedded file(s)`);

function shutdown(signal: string) {
  console.log(`\nreceived ${signal}, shutting down`);
  server.stop();
  process.exit(0);
}

process.on("SIGTERM", () => shutdown("SIGTERM"));
process.on("SIGINT", () => shutdown("SIGINT"));
