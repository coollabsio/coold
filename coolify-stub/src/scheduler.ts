// Thin client around Bun's `fetch({ unix })` option for talking to the coold
// scheduler's HTTP-over-UDS API. Keeps error handling typed so routes can map
// transport failures to 5xx without crashing.

import type {
  BuildDispatchAck,
  BuildDispatchEnvelope,
  BuildResponseEnvelope,
  DispatchEnvelope,
  ResponseEnvelope,
} from "./envelope.ts";

const DISPATCH_TIMEOUT_MS = 5_000;
const DEFAULT_POLL_TIMEOUT_MS = 30_000;

export interface SchedulerHealthOk {
  ok: true;
}

export interface SchedulerHealthErr {
  ok: false;
  error: string;
}

export type SchedulerHealth = SchedulerHealthOk | SchedulerHealthErr;

export interface SchedulerClient {
  health(): Promise<SchedulerHealth>;
  listContainers(hostId: string, requestId: string): Promise<ResponseEnvelope>;
  dispatchBuild(
    env: BuildDispatchEnvelope,
  ): Promise<{ status: number; body: BuildResponseEnvelope | BuildDispatchAck }>;
  buildResult(
    requestId: string,
    timeoutMs?: number,
  ): Promise<{ status: number; body: BuildResponseEnvelope }>;
  cancelBuild(requestId: string): Promise<{ status: number }>;
}

export class SchedulerTransportError extends Error {
  constructor(
    message: string,
    readonly cause?: unknown,
  ) {
    super(message);
    this.name = "SchedulerTransportError";
  }
}

interface FetchInit {
  method: "GET" | "POST" | "DELETE";
  path: string;
  body?: unknown;
  timeoutMs?: number;
}

export function createSchedulerClient(socketPath: string): SchedulerClient {
  async function call({ method, path, body, timeoutMs = DISPATCH_TIMEOUT_MS }: FetchInit) {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), timeoutMs);
    try {
      const res = await fetch(`http://localhost${path}`, {
        unix: socketPath,
        method,
        headers: body === undefined ? undefined : { "Content-Type": "application/json" },
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: ctrl.signal,
      });
      return res;
    } catch (err) {
      throw new SchedulerTransportError(
        `scheduler request failed (${method} ${path}): ${(err as Error).message ?? err}`,
        err,
      );
    } finally {
      clearTimeout(timer);
    }
  }

  async function parseJson<T>(res: Response): Promise<T> {
    const text = await res.text();
    if (text.length === 0) return {} as T;
    try {
      return JSON.parse(text) as T;
    } catch (err) {
      throw new SchedulerTransportError(
        `scheduler returned invalid JSON (status ${res.status}): ${text.slice(0, 200)}`,
        err,
      );
    }
  }

  return {
    async health() {
      try {
        const res = await call({ method: "GET", path: "/v1/health" });
        if (!res.ok) {
          return { ok: false, error: `scheduler /v1/health returned ${res.status}` };
        }
        return { ok: true };
      } catch (err) {
        return { ok: false, error: (err as Error).message };
      }
    },

    async listContainers(hostId, requestId) {
      const env: DispatchEnvelope = {
        host_id: hostId,
        request_id: requestId,
        command: { type: "list_containers" },
      };
      const res = await call({ method: "POST", path: "/v1/coold/dispatch", body: env });
      return parseJson<ResponseEnvelope>(res);
    },

    async dispatchBuild(env) {
      const res = await call({ method: "POST", path: "/v1/build/dispatch", body: env });
      if (res.status === 202) {
        const body = await parseJson<BuildDispatchAck>(res);
        return { status: 202, body };
      }
      const body = await parseJson<BuildResponseEnvelope>(res);
      return { status: res.status, body };
    },

    async buildResult(requestId, timeoutMs = DEFAULT_POLL_TIMEOUT_MS) {
      const res = await call({
        method: "GET",
        path: `/v1/build/result/${encodeURIComponent(requestId)}?timeout_ms=${timeoutMs}`,
        // Allow ~2s slack over the scheduler's long-poll window before aborting.
        timeoutMs: timeoutMs + 2_000,
      });
      const body = await parseJson<BuildResponseEnvelope>(res);
      return { status: res.status, body };
    },

    async cancelBuild(requestId) {
      const res = await call({
        method: "POST",
        path: `/v1/build/${encodeURIComponent(requestId)}/cancel`,
      });
      return { status: res.status };
    },
  };
}
