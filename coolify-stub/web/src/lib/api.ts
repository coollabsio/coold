export interface ApiHostsResponse {
  hosts: string[];
}

export interface ResponseEnvelope {
  request_id: string;
  status: "ok" | "error";
  data?: unknown;
  code?: number;
  message?: string;
}

export interface BuildResponseEnvelope {
  request_id: string;
  status: "ok" | "error";
  digest?: string;
  registry_ref?: string;
  duration_ms?: number;
  code?: number;
  message?: string;
  stage?: string;
}

export interface BuildDispatchAck {
  request_id: string;
}

export interface ContainerSummary {
  id: string;
  name: string;
  image: string;
  state: string;
  networks: string[];
}

export interface DispatchBuildBody {
  host_id?: string;
  repo_url: string;
  git_ref: string;
  target_image: string;
  output_dir?: string;
  base_image?: string;
}

export class ApiError extends Error {
  status: number;
  body: unknown;
  constructor(status: number, message: string, body: unknown) {
    super(message);
    this.status = status;
    this.body = body;
  }
}

async function parseBody(res: Response): Promise<unknown> {
  const text = await res.text();
  if (!text) return undefined;
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

async function request<T>(
  path: string,
  init?: RequestInit,
  { allow4xxBody = false }: { allow4xxBody?: boolean } = {},
): Promise<T> {
  const res = await fetch(path, {
    ...init,
    headers: {
      "Content-Type": "application/json",
      Accept: "application/json",
      ...(init?.headers ?? {}),
    },
  });
  const body = await parseBody(res);
  if (res.ok) {
    return body as T;
  }
  if (allow4xxBody && res.status >= 400 && res.status < 500 && body !== undefined) {
    return body as T;
  }
  const message =
    (body && typeof body === "object" && "message" in (body as Record<string, unknown>)
      ? String((body as Record<string, unknown>).message)
      : undefined) ?? `${res.status} ${res.statusText}`;
  throw new ApiError(res.status, message, body);
}

export async function getHealth(): Promise<{ ok: boolean }> {
  return request<{ ok: boolean }>("/api/health");
}

export async function getHosts(): Promise<ApiHostsResponse> {
  return request<ApiHostsResponse>("/api/hosts");
}

export async function listContainers(hostId: string): Promise<ResponseEnvelope> {
  const qs = new URLSearchParams({ host_id: hostId }).toString();
  return request<ResponseEnvelope>(`/api/containers?${qs}`, undefined, { allow4xxBody: true });
}

export async function dispatchBuild(
  body: DispatchBuildBody,
): Promise<BuildDispatchAck | BuildResponseEnvelope> {
  return request<BuildDispatchAck | BuildResponseEnvelope>(
    "/api/builds",
    {
      method: "POST",
      body: JSON.stringify(body),
    },
    { allow4xxBody: true },
  );
}

export async function getBuildResult(id: string): Promise<BuildResponseEnvelope> {
  return request<BuildResponseEnvelope>(`/api/builds/${encodeURIComponent(id)}`, undefined, {
    allow4xxBody: true,
  });
}

export async function cancelBuild(id: string): Promise<void> {
  await request<unknown>(
    `/api/builds/${encodeURIComponent(id)}`,
    { method: "DELETE" },
    { allow4xxBody: true },
  );
}
