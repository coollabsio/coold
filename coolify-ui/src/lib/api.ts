export type Server = { id: string; name: string; address: string; mgmt_ip?: string | null; status: string; coold_version?: string | null; host_id?: string | null; capabilities: string[]; last_seen_at?: string | null };
export type Container = { id: string; name: string; image: string; state: string; networks: string[] };
export type ServerLiveStatus = { server_id: string; host_id?: string | null; scheduler_configured: boolean; reachable: boolean; capabilities: string[] };
export type Cluster = { id: string; name: string; description: string };
export type Event = { id: string; severity: string; subject: string; message: string; created_at: string };
export type Build = { id: string; status: string; image_ref?: string | null; message: string; created_at: string };
export type Status = { ok: boolean; app: string; version: string; scheduler: { configured: boolean; connected_streams: number } };
export type SchedulerStream = { host_id: string; caps: string[]; builder_capacity: number };
export type ServerSyncResult = { created: number; updated: number; server_ids: string[] };

async function getJson<T>(path: string): Promise<T> {
  const res = await fetch(path, { headers: { Accept: "application/json" } });
  if (!res.ok) throw new Error(`${path}: ${res.status}`);
  return res.json() as Promise<T>;
}
async function postJson<T>(path: string): Promise<T> {
  const res = await fetch(path, { method: "POST", headers: { Accept: "application/json" } });
  if (!res.ok) throw new Error(`${path}: ${res.status}`);
  return res.json() as Promise<T>;
}

export const api = {
  status: () => getJson<Status>("/api/v1/status"),
  schedulerStreams: () => getJson<SchedulerStream[]>("/api/v1/scheduler/streams"),
  servers: () => getJson<Server[]>("/api/v1/servers"),
  syncStreams: () => postJson<ServerSyncResult>("/api/v1/servers/sync-streams"),
  serverLiveStatus: (id: string) => getJson<ServerLiveStatus>(`/api/v1/servers/${id}/live-status`),
  serverContainers: (id: string) => getJson<Container[]>(`/api/v1/servers/${id}/containers`),
  clusters: () => getJson<Cluster[]>("/api/v1/clusters"),
  events: () => getJson<Event[]>("/api/v1/events"),
  builds: () => getJson<Build[]>("/api/v1/builds"),
};

export const queryKeys = {
  status: ["status"] as const,
  schedulerStreams: ["scheduler", "streams"] as const,
  servers: ["servers"] as const,
  serverLiveStatus: (id: string) => ["servers", id, "live-status"] as const,
  serverContainers: (id: string) => ["servers", id, "containers"] as const,
  clusters: ["clusters"] as const,
  events: ["events"] as const,
  builds: ["builds"] as const,
};
