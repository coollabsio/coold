import { useQuery } from "@tanstack/react-query";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { api, queryKeys } from "@/lib/api";
export function EventsPage() {
  const q = useQuery({ queryKey: queryKeys.events, queryFn: api.events, refetchInterval: 10_000 });
  return <section className="space-y-4"><h1 className="text-3xl font-bold">Events</h1><Card><CardHeader><CardTitle>Recent events</CardTitle></CardHeader><CardContent><ul className="space-y-3">{q.data?.map((e) => <li key={e.id} className="rounded-md border p-3"><div className="flex items-center justify-between"><span className="font-medium">{e.subject}</span><span className="text-xs uppercase text-muted-foreground">{e.severity}</span></div><p className="text-sm text-muted-foreground">{e.message}</p></li>)}{q.data?.length === 0 && <li className="text-sm text-muted-foreground">No events yet.</li>}</ul></CardContent></Card></section>;
}
