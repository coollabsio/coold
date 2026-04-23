<script lang="ts">
  import Router, { link, location } from "svelte-spa-router";
  import { Cloud, LayoutDashboard, Rocket, Hammer, Container } from "lucide-svelte";
  import Overview from "./routes/Overview.svelte";
  import Deploy from "./routes/Deploy.svelte";
  import Builds from "./routes/Builds.svelte";
  import Containers from "./routes/Containers.svelte";
  import { cn } from "$lib/utils";

  const routes = {
    "/": Overview,
    "/deploy": Deploy,
    "/builds": Builds,
    "/containers": Containers,
    "*": Overview,
  };

  const nav = [
    { href: "/", label: "Overview", icon: LayoutDashboard },
    { href: "/deploy", label: "Deploy", icon: Rocket },
    { href: "/builds", label: "Builds", icon: Hammer },
    { href: "/containers", label: "Containers", icon: Container },
  ];

  function isActive(path: string, current: string) {
    if (path === "/") return current === "/" || current === "";
    return current === path || current.startsWith(path + "/") || current.startsWith(path + "?");
  }

  const pageTitle = $derived.by(() => {
    const current = $location ?? "/";
    const match = nav.find((n) => isActive(n.href, current));
    return match?.label ?? "Coolify";
  });
</script>

<div class="flex h-full min-h-screen bg-background text-foreground">
  <aside class="flex w-60 flex-col border-r bg-muted/30">
    <div class="flex h-14 items-center gap-2 border-b px-6">
      <Cloud class="h-5 w-5 text-emerald-500" />
      <span class="text-base font-semibold tracking-tight">Coolify</span>
      <span class="ml-auto rounded bg-muted px-1.5 py-0.5 text-[10px] font-medium text-muted-foreground"
        >v5</span
      >
    </div>
    <nav class="flex flex-1 flex-col gap-1 p-3">
      {#each nav as item}
        {@const active = isActive(item.href, $location ?? "/")}
        <a
          href={item.href}
          use:link
          class={cn(
            "flex items-center gap-2 rounded-md px-3 py-2 text-sm font-medium transition-colors",
            active
              ? "bg-accent text-accent-foreground"
              : "text-muted-foreground hover:bg-accent/50 hover:text-foreground",
          )}
        >
          <item.icon class="h-4 w-4" />
          {item.label}
        </a>
      {/each}
    </nav>
    <div class="border-t p-4 text-[11px] text-muted-foreground">
      <div class="font-medium text-foreground/80">Stub dashboard</div>
      <div>Talks to coold broker via /api.</div>
    </div>
  </aside>

  <main class="flex min-w-0 flex-1 flex-col">
    <header class="flex h-14 items-center justify-between border-b px-8">
      <h1 class="text-lg font-semibold tracking-tight">{pageTitle}</h1>
      <div class="text-xs text-muted-foreground">coolify-stub</div>
    </header>
    <div class="flex-1 overflow-auto px-8 py-6">
      <Router {routes} />
    </div>
  </main>
</div>
