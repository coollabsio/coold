<script lang="ts">
  import { onMount } from "svelte";
  import { link } from "svelte-spa-router";
  import { Activity, Server, Rocket, RefreshCw, CheckCircle2, XCircle } from "lucide-svelte";
  import * as Card from "$lib/components/ui/card";
  import { Badge } from "$lib/components/ui/badge";
  import { Button } from "$lib/components/ui/button";
  import { Separator } from "$lib/components/ui/separator";
  import { getHealth, getHosts, ApiError } from "$lib/api";

  let healthy = $state<boolean | null>(null);
  let healthError = $state<string | null>(null);
  let hosts = $state<string[]>([]);
  let hostsError = $state<string | null>(null);
  let loadingHosts = $state(false);

  async function refreshHealth() {
    try {
      const res = await getHealth();
      healthy = !!res.ok;
      healthError = null;
    } catch (err) {
      healthy = false;
      healthError = err instanceof Error ? err.message : "Unknown error";
    }
  }

  async function refreshHosts() {
    loadingHosts = true;
    try {
      const res = await getHosts();
      hosts = res.hosts ?? [];
      hostsError = null;
    } catch (err) {
      hosts = [];
      hostsError =
        err instanceof ApiError ? err.message : err instanceof Error ? err.message : "Unknown error";
    } finally {
      loadingHosts = false;
    }
  }

  onMount(() => {
    refreshHealth();
    refreshHosts();
    const t = setInterval(refreshHealth, 5000);
    return () => clearInterval(t);
  });
</script>

<div class="grid gap-6 md:grid-cols-2 xl:grid-cols-3">
  <Card.Root>
    <Card.Header class="flex-row items-center justify-between space-y-0">
      <div class="space-y-1">
        <Card.Title class="flex items-center gap-2 text-sm">
          <Activity class="h-4 w-4 text-muted-foreground" />
          Broker health
        </Card.Title>
        <Card.Description>Live probe of /api/health every 5s.</Card.Description>
      </div>
      <Button variant="ghost" size="icon" onclick={refreshHealth} aria-label="Refresh health">
        <RefreshCw class="h-4 w-4" />
      </Button>
    </Card.Header>
    <Card.Content class="flex items-center gap-3">
      {#if healthy === null}
        <Badge variant="secondary">Probing…</Badge>
      {:else if healthy}
        <Badge variant="success">
          <CheckCircle2 class="mr-1 h-3 w-3" /> Healthy
        </Badge>
        <span class="text-xs text-muted-foreground">broker reachable via UDS</span>
      {:else}
        <Badge variant="destructive">
          <XCircle class="mr-1 h-3 w-3" /> Offline
        </Badge>
        {#if healthError}
          <span class="text-xs text-muted-foreground">{healthError}</span>
        {/if}
      {/if}
    </Card.Content>
  </Card.Root>

  <Card.Root>
    <Card.Header class="flex-row items-center justify-between space-y-0">
      <div class="space-y-1">
        <Card.Title class="flex items-center gap-2 text-sm">
          <Server class="h-4 w-4 text-muted-foreground" />
          Known hosts
        </Card.Title>
        <Card.Description>Agents registered with the broker.</Card.Description>
      </div>
      <Button
        variant="ghost"
        size="icon"
        onclick={refreshHosts}
        disabled={loadingHosts}
        aria-label="Refresh hosts"
      >
        <RefreshCw class={"h-4 w-4 " + (loadingHosts ? "animate-spin" : "")} />
      </Button>
    </Card.Header>
    <Card.Content>
      {#if hostsError}
        <p class="text-sm text-destructive">{hostsError}</p>
      {:else if hosts.length === 0}
        <p class="text-sm text-muted-foreground">No hosts registered yet.</p>
      {:else}
        <ul class="flex flex-col gap-2">
          {#each hosts as host}
            <li class="flex items-center justify-between gap-2 rounded-md border bg-card/50 px-3 py-2">
              <Badge variant="outline" class="font-mono">{host}</Badge>
              <a
                href={`/containers?host=${encodeURIComponent(host)}`}
                use:link
                class="text-xs font-medium text-primary hover:underline"
              >
                Refresh containers →
              </a>
            </li>
          {/each}
        </ul>
      {/if}
    </Card.Content>
  </Card.Root>

  <Card.Root>
    <Card.Header class="space-y-1">
      <Card.Title class="flex items-center gap-2 text-sm">
        <Rocket class="h-4 w-4 text-muted-foreground" />
        Quick start
      </Card.Title>
      <Card.Description>Dispatch a static site build to warm the pipeline.</Card.Description>
    </Card.Header>
    <Card.Content class="flex flex-col gap-4">
      <p class="text-sm text-muted-foreground">
        The stub ships with sensible defaults for a simple HTML site. Click below to jump to the
        Deploy form pre-filled with an MDN example repo.
      </p>
      <Separator />
      <Button href="#/deploy" class="w-fit">
        <Rocket class="h-4 w-4" />
        Dispatch a static build
      </Button>
    </Card.Content>
  </Card.Root>
</div>
