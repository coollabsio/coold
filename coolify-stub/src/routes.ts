// HTTP dispatcher for the stub's `/api/*` surface. Kept as a plain
// `match(req)` function so server.ts can fall through to the embedded SPA
// without fighting Bun's route-map overloads.

import type { SchedulerClient } from "./scheduler.ts";
import { SchedulerTransportError } from "./scheduler.ts";
import type { BuildDispatchEnvelope } from "./envelope.ts";

interface BuildRequestBody {
  host_id?: string;
  repo_url: string;
  git_ref: string;
  target_image: string;
  output_dir?: string;
  base_image?: string;
}

function json(body: unknown, init: ResponseInit = {}): Response {
  return new Response(JSON.stringify(body), {
    ...init,
    headers: {
      "Content-Type": "application/json",
      ...(init.headers ?? {}),
    },
  });
}

function schedulerError(err: unknown): Response {
  if (err instanceof SchedulerTransportError) {
    return json({ error: "scheduler_unreachable", message: err.message }, { status: 502 });
  }
  return json(
    { error: "internal_error", message: (err as Error).message ?? "unknown" },
    { status: 500 },
  );
}

async function readJson<T>(req: Request): Promise<T | null> {
  const text = await req.text();
  if (text.length === 0) return null;
  try {
    return JSON.parse(text) as T;
  } catch {
    return null;
  }
}

const ALLOWED_METHODS = ["GET", "POST", "OPTIONS"] as const;

export function createApiHandler(scheduler: SchedulerClient, hosts: string[]) {
  async function handleContainers(id: string): Promise<Response> {
    const requestId = crypto.randomUUID();
    try {
      const env = await scheduler.listContainers(id, requestId);
      if (env.status === "ok") {
        return json({ request_id: env.request_id, containers: env.data ?? [] });
      }
      return json(
        { request_id: env.request_id, error: env.message ?? "unknown", code: env.code ?? 0 },
        { status: 502 },
      );
    } catch (err) {
      return schedulerError(err);
    }
  }

  async function handleDispatch(req: Request): Promise<Response> {
    const body = await readJson<BuildRequestBody>(req);
    if (
      !body ||
      typeof body.repo_url !== "string" ||
      typeof body.git_ref !== "string" ||
      typeof body.target_image !== "string"
    ) {
      return json(
        { error: "invalid_body", message: "repo_url, git_ref, target_image are required" },
        { status: 400 },
      );
    }
    const requestId = crypto.randomUUID();
    const env: BuildDispatchEnvelope = {
      host_id: body.host_id,
      request_id: requestId,
      command: {
        type: "static_build",
        repo_url: body.repo_url,
        git_ref: body.git_ref,
        target_image: body.target_image,
        output_dir: body.output_dir,
        base_image: body.base_image,
      },
    };
    try {
      const { status, body: respBody } = await scheduler.dispatchBuild(env);
      if (status === 202) {
        return json({ request_id: requestId }, { status: 202 });
      }
      return json(respBody, { status });
    } catch (err) {
      return schedulerError(err);
    }
  }

  async function handleResult(id: string, req: Request): Promise<Response> {
    const url = new URL(req.url);
    const rawTimeout = url.searchParams.get("timeout_ms");
    const timeoutMs = rawTimeout ? Number(rawTimeout) : undefined;
    try {
      const { status, body } = await scheduler.buildResult(id, timeoutMs);
      return json(body, { status });
    } catch (err) {
      return schedulerError(err);
    }
  }

  async function handleCancel(id: string): Promise<Response> {
    try {
      const { status } = await scheduler.cancelBuild(id);
      if (status === 204) return new Response(null, { status: 204 });
      return json({ status }, { status });
    } catch (err) {
      return schedulerError(err);
    }
  }

  async function handleHealth(): Promise<Response> {
    const status = await scheduler.health();
    if (status.ok) return json({ ok: true, scheduler: "ok" });
    return json({ ok: false, scheduler: status.error }, { status: 503 });
  }

  return async function match(req: Request): Promise<Response | null> {
    const url = new URL(req.url);
    const path = url.pathname;
    if (!path.startsWith("/api/")) return null;

    if (!(ALLOWED_METHODS as readonly string[]).includes(req.method)) {
      return json({ error: "method_not_allowed" }, { status: 405 });
    }

    if (path === "/api/health" && req.method === "GET") return handleHealth();
    if (path === "/api/hosts" && req.method === "GET") return json({ hosts });

    // /api/hosts/:id/containers
    const containersMatch = path.match(/^\/api\/hosts\/([^/]+)\/containers$/);
    if (containersMatch && req.method === "POST") {
      return handleContainers(decodeURIComponent(containersMatch[1]!));
    }

    if (path === "/api/builds" && req.method === "POST") return handleDispatch(req);

    // /api/builds/:id/cancel
    const cancelMatch = path.match(/^\/api\/builds\/([^/]+)\/cancel$/);
    if (cancelMatch && req.method === "POST") {
      return handleCancel(decodeURIComponent(cancelMatch[1]!));
    }

    // /api/builds/:id
    const resultMatch = path.match(/^\/api\/builds\/([^/]+)$/);
    if (resultMatch && req.method === "GET") {
      return handleResult(decodeURIComponent(resultMatch[1]!), req);
    }

    return json({ error: "not_found", path }, { status: 404 });
  };
}

export type ApiHandler = ReturnType<typeof createApiHandler>;
