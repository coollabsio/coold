import { Link, Outlet } from "@tanstack/react-router";
import { Activity, Boxes, Gauge, Server } from "lucide-react";
import { cn } from "@/lib/utils";
const nav = [
  { to: "/", label: "Dashboard", icon: Gauge },
  { to: "/servers", label: "Servers", icon: Server },
  { to: "/clusters", label: "Clusters", icon: Boxes },
  { to: "/events", label: "Events", icon: Activity },
];
export function AppShell() {
  return <div className="min-h-screen bg-background">
    <aside className="fixed inset-y-0 left-0 hidden w-64 border-r bg-card p-4 md:block">
      <div className="mb-8 text-xl font-bold text-primary">Coolify v5</div>
      <nav className="space-y-1">{nav.map((item) => <Link key={item.to} to={item.to} className="block">{({ isActive }) => <span className={cn("flex items-center gap-3 rounded-md px-3 py-2 text-sm", isActive ? "bg-primary text-primary-foreground" : "text-muted-foreground hover:bg-muted hover:text-foreground")}><item.icon className="h-4 w-4" />{item.label}</span>}</Link>)}</nav>
    </aside>
    <main className="md:pl-64"><div className="mx-auto max-w-6xl p-6"><Outlet /></div></main>
  </div>;
}
