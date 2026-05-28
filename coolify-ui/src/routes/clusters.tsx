import { useQuery } from "@tanstack/react-query";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { api, queryKeys } from "@/lib/api";
export function ClustersPage() {
  const q = useQuery({ queryKey: queryKeys.clusters, queryFn: api.clusters });
  return <section className="space-y-4"><h1 className="text-3xl font-bold">Clusters</h1><div className="grid gap-4 md:grid-cols-2">{q.data?.map((c) => <Card key={c.id}><CardHeader><CardTitle>{c.name}</CardTitle></CardHeader><CardContent><p className="text-sm text-muted-foreground">{c.description || "No description"}</p></CardContent></Card>)}{q.data?.length === 0 && <Card><CardContent className="pt-6 text-sm text-muted-foreground">No clusters stored yet.</CardContent></Card>}</div></section>;
}
