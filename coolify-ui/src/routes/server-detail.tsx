import { useQuery } from "@tanstack/react-query";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { api, queryKeys } from "@/lib/api";

export function ServerDetailPage({ serverId }: { serverId: string }) {
  const servers = useQuery({ queryKey: queryKeys.servers, queryFn: api.servers });
  const live = useQuery({ queryKey: queryKeys.serverLiveStatus(serverId), queryFn: () => api.serverLiveStatus(serverId) });
  const containers = useQuery({ queryKey: queryKeys.serverContainers(serverId), queryFn: () => api.serverContainers(serverId), retry: false });
  const server = servers.data?.find((s) => s.id === serverId);
  return <section className="space-y-4">
    <div><h1 className="text-3xl font-bold">{server?.name ?? "Server"}</h1><p className="text-muted-foreground">{server?.address ?? serverId}</p></div>
    <div className="grid gap-4 md:grid-cols-3">
      <Card><CardHeader><CardTitle>Status</CardTitle></CardHeader><CardContent>{server?.status ?? "loading"}</CardContent></Card>
      <Card><CardHeader><CardTitle>Scheduler</CardTitle></CardHeader><CardContent>{live.data?.scheduler_configured ? "configured" : "not configured"}</CardContent></Card>
      <Card><CardHeader><CardTitle>Host stream</CardTitle></CardHeader><CardContent>{live.data?.reachable ? "reachable" : "unknown/offline"}</CardContent></Card>
    </div>
    <Card><CardHeader><CardTitle>Containers</CardTitle></CardHeader><CardContent><div className="overflow-x-auto"><table className="w-full text-left text-sm"><thead className="text-muted-foreground"><tr><th className="py-2">Name</th><th>Image</th><th>State</th><th>Networks</th></tr></thead><tbody>{containers.data?.map((c) => <tr key={c.id} className="border-t"><td className="py-2 font-medium">{c.name}</td><td>{c.image}</td><td>{c.state}</td><td>{c.networks.join(", ") || "—"}</td></tr>)}{containers.isLoading && <tr><td className="py-6 text-muted-foreground" colSpan={4}>Loading live containers…</td></tr>}{containers.isError && <tr><td className="py-6 text-muted-foreground" colSpan={4}>Could not load live containers: {(containers.error as Error).message}</td></tr>}{containers.data?.length === 0 && <tr><td className="py-6 text-muted-foreground" colSpan={4}>No containers reported by coold.</td></tr>}</tbody></table></div></CardContent></Card>
  </section>;
}
