import { useQuery } from "@tanstack/react-query";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { api, queryKeys } from "@/lib/api";

export function Dashboard() {
  const status = useQuery({ queryKey: queryKeys.status, queryFn: api.status });
  const servers = useQuery({ queryKey: queryKeys.servers, queryFn: api.servers });
  const clusters = useQuery({ queryKey: queryKeys.clusters, queryFn: api.clusters });
  const events = useQuery({ queryKey: queryKeys.events, queryFn: api.events });
  return <section className="space-y-6">
    <div><h1 className="text-3xl font-bold">Coolify v5 control plane</h1><p className="text-muted-foreground">Rust API + embedded React view into cluster state.</p></div>
    <div className="grid gap-4 md:grid-cols-4">
      <Metric title="API" value={status.data?.ok ? "online" : "loading"} />
      <Metric title="Servers" value={servers.data?.length ?? "—"} />
      <Metric title="Clusters" value={clusters.data?.length ?? "—"} />
      <Metric title="Events" value={events.data?.length ?? "—"} />
    </div>
    <Card><CardHeader><CardTitle>Scheduler</CardTitle></CardHeader><CardContent><p className="text-sm text-muted-foreground">Connected streams: {status.data?.scheduler.connected_streams ?? 0}. Scheduler integration will be wired after the basic API/storage shell.</p></CardContent></Card>
  </section>;
}
function Metric({ title, value }: { title: string; value: string | number }) { return <Card><CardHeader><CardTitle className="text-sm text-muted-foreground">{title}</CardTitle></CardHeader><CardContent><div className="text-2xl font-bold">{value}</div></CardContent></Card>; }
