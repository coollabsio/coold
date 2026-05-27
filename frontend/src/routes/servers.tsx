import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { api, queryKeys } from "@/lib/api";
export function ServersPage() {
  const q = useQuery({ queryKey: queryKeys.servers, queryFn: api.servers });
  return <section className="space-y-4"><h1 className="text-3xl font-bold">Servers</h1><Card><CardHeader><CardTitle>Known hosts</CardTitle></CardHeader><CardContent><div className="overflow-x-auto"><table className="w-full text-left text-sm"><thead className="text-muted-foreground"><tr><th className="py-2">Name</th><th>Address</th><th>Mgmt IP</th><th>Status</th><th>coold</th></tr></thead><tbody>{q.data?.map((s) => <tr key={s.id} className="border-t"><td className="py-2 font-medium"><Link to="/servers/$serverId" params={{ serverId: s.id }} className="text-primary hover:underline">{s.name}</Link></td><td>{s.address}</td><td>{s.mgmt_ip ?? "—"}</td><td>{s.status}</td><td>{s.coold_version ?? "—"}</td></tr>)}{q.data?.length === 0 && <tr><td className="py-6 text-muted-foreground" colSpan={5}>No servers yet. Use cooldctl init bootstrap/extend, then wire discovery into this API.</td></tr>}</tbody></table></div></CardContent></Card></section>;
}
