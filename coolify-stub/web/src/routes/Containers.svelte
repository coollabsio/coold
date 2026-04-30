<script lang="ts">
  import { onMount } from "svelte";
  import { RefreshCw, Container as ContainerIcon, XCircle } from "lucide-svelte";
  import * as Card from "$lib/components/ui/card";
  import * as Table from "$lib/components/ui/table";
  import { Badge } from "$lib/components/ui/badge";
  import { Button } from "$lib/components/ui/button";
  import { getHosts, listContainers, ApiError, type ContainerSummary } from "$lib/api";

  type Props = { params?: Record<string, string> };
  let { params = {} }: Props = $props();

  let hosts = $state<string[]>([]);
  let hostId = $state<string>("");
  let containers = $state<ContainerSummary[]>([]);
  let loading = $state(false);
  let errorBlock = $state<{ code?: number; message?: string } | null>(null);

  function parseQuery(qs?: string): string | undefined {
    if (!qs) return undefined;
    const p = new URLSearchParams(qs.startsWith("?") ? qs.slice(1) : qs);
    return p.get("host") ?? undefined;
  }

  onMount(async () => {
    try {
      const res = await getHosts();
      hosts = res.hosts ?? [];
    } catch {
      hosts = [];
    }
    const preferred = params.host ?? params.wild ?? parseQuery(params.querystring);
    if (preferred && hosts.includes(preferred)) {
      hostId = preferred;
    } else if (preferred) {
      hostId = preferred;
    } else if (hosts.length > 0) {
      hostId = hosts[0];
    }
    if (hostId) await refresh();
  });

  async function refresh() {
    if (!hostId) return;
    loading = true;
    errorBlock = null;
    try {
      const res = await listContainers(hostId);
      if (res.status === "error") {
        errorBlock = { code: res.code, message: res.message ?? "Agent returned an error" };
        containers = [];
      } else {
        const arr = Array.isArray(res.data) ? (res.data as ContainerSummary[]) : [];
        containers = arr;
      }
    } catch (err) {
      errorBlock = {
        message:
          err instanceof ApiError
            ? err.message
            : err instanceof Error
              ? err.message
              : "Unknown error",
      };
      containers = [];
    } finally {
      loading = false;
    }
  }

  function stateBadgeVariant(state: string) {
    const s = state.toLowerCase();
    if (s === "running") return "success";
    if (s === "exited" || s === "dead") return "destructive";
    if (s === "paused") return "warning";
    return "secondary";
  }
</script>

<div class="flex flex-col gap-6">
  <Card.Root>
    <Card.Header class="flex-row items-end justify-between gap-4 space-y-0">
      <div class="flex-1 space-y-2">
        <Card.Title class="flex items-center gap-2 text-base">
          <ContainerIcon class="h-4 w-4 text-muted-foreground" /> Containers
        </Card.Title>
        <Card.Description>Query docker/podman state on a specific agent host.</Card.Description>
        <div class="mt-3 flex max-w-md items-center gap-2">
          <select
            bind:value={hostId}
            class="flex h-9 w-full rounded-md border border-input bg-transparent px-3 py-1 text-sm shadow-sm focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
          >
            <option value="" disabled>Select host…</option>
            {#each hosts as host}
              <option value={host}>{host}</option>
            {/each}
          </select>
          <Button onclick={refresh} disabled={!hostId || loading}>
            <RefreshCw class={"h-4 w-4 " + (loading ? "animate-spin" : "")} />
            Refresh
          </Button>
        </div>
      </div>
    </Card.Header>
  </Card.Root>

  {#if errorBlock}
    <Card.Root class="border-destructive/40 bg-destructive/5">
      <Card.Header class="flex-row items-center gap-3 space-y-0">
        <XCircle class="h-5 w-5 text-destructive" />
        <div>
          <Card.Title class="text-sm text-destructive">Scheduler returned an error</Card.Title>
          <Card.Description>
            {#if errorBlock.code !== undefined}code {errorBlock.code} —{/if}
            {errorBlock.message}
          </Card.Description>
        </div>
      </Card.Header>
    </Card.Root>
  {/if}

  <Card.Root>
    <Card.Content class="p-0">
      {#if !hostId}
        <div class="flex items-center justify-center px-6 py-16 text-sm text-muted-foreground">
          Select a host to list containers.
        </div>
      {:else if containers.length === 0 && !errorBlock && !loading}
        <div class="flex items-center justify-center px-6 py-16 text-sm text-muted-foreground">
          No containers on this host.
        </div>
      {:else if containers.length > 0}
        <Table.Root>
          <Table.Header>
            <Table.Row>
              <Table.Head class="w-[160px]">ID</Table.Head>
              <Table.Head>Name</Table.Head>
              <Table.Head>Image</Table.Head>
              <Table.Head class="w-[110px]">State</Table.Head>
              <Table.Head>Networks</Table.Head>
            </Table.Row>
          </Table.Header>
          <Table.Body>
            {#each containers as c}
              <Table.Row>
                <Table.Cell class="font-mono text-xs">{c.id.slice(0, 12)}</Table.Cell>
                <Table.Cell class="text-sm">{c.name}</Table.Cell>
                <Table.Cell class="font-mono text-xs">{c.image}</Table.Cell>
                <Table.Cell>
                  <Badge variant={stateBadgeVariant(c.state)}>{c.state}</Badge>
                </Table.Cell>
                <Table.Cell>
                  <div class="flex flex-wrap gap-1">
                    {#each c.networks as n}
                      <Badge variant="outline" class="font-mono text-[10px]">{n}</Badge>
                    {/each}
                  </div>
                </Table.Cell>
              </Table.Row>
            {/each}
          </Table.Body>
        </Table.Root>
      {/if}
    </Card.Content>
  </Card.Root>
</div>
