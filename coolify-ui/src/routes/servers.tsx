import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { RefreshCw } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { api, queryKeys } from "@/lib/api";

export function ServersPage() {
  const queryClient = useQueryClient();
  const q = useQuery({ queryKey: queryKeys.servers, queryFn: api.servers });
  const sync = useMutation({
    mutationFn: api.syncStreams,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: queryKeys.servers });
      queryClient.invalidateQueries({ queryKey: queryKeys.events });
      queryClient.invalidateQueries({ queryKey: queryKeys.schedulerStreams });
    },
  });
  return <section className="space-y-4">
    <div className="flex items-center justify-between gap-4"><div><h1 className="text-3xl font-bold">Servers</h1><p className="text-muted-foreground">Known hosts synced from scheduler streams and local storage.</p></div><button onClick={() => sync.mutate()} disabled={sync.isPending} className="inline-flex items-center gap-2 rounded-md bg-primary px-3 py-2 text-sm font-medium text-primary-foreground disabled:opacity-60"><RefreshCw className="h-4 w-4" />{sync.isPending ? "Syncing…" : "Sync scheduler streams"}</button></div>
    {sync.isError && <div className="rounded-md border border-destructive p-3 text-sm text-destructive">Sync failed: {(sync.error as Error).message}</div>}
    {sync.data && <div className="rounded-md border p-3 text-sm text-muted-foreground">Synced streams: created {sync.data.created}, updated {sync.data.updated}</div>}
    <Card><CardHeader><CardTitle>Known hosts</CardTitle></CardHeader><CardContent><div className="overflow-x-auto"><table className="w-full text-left text-sm"><thead className="text-muted-foreground"><tr><th className="py-2">Name</th><th>Host ID</th><th>Address</th><th>Status</th><th>Capabilities</th><th>Last seen</th></tr></thead><tbody>{q.data?.map((s) => <tr key={s.id} className="border-t"><td className="py-2 font-medium"><Link to="/servers/$serverId" params={{ serverId: s.id }} className="text-primary hover:underline">{s.name}</Link></td><td>{s.host_id ?? "—"}</td><td>{s.address}</td><td>{s.status}</td><td>{s.capabilities?.join(", ") || "—"}</td><td>{s.last_seen_at ? new Date(s.last_seen_at).toLocaleString() : "—"}</td></tr>)}{q.data?.length === 0 && <tr><td className="py-6 text-muted-foreground" colSpan={6}>No servers yet. Start scheduler/coold, then sync scheduler streams.</td></tr>}</tbody></table></div></CardContent></Card>
  </section>;
}
