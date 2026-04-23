<script lang="ts">
  import { onDestroy } from "svelte";
  import { CheckCircle2, XCircle, LoaderCircle, X, Hammer } from "lucide-svelte";
  import * as Card from "$lib/components/ui/card";
  import * as Table from "$lib/components/ui/table";
  import { Badge } from "$lib/components/ui/badge";
  import { Button } from "$lib/components/ui/button";
  import {
    builds,
    updateBuild,
    removeSettled,
    removeBuild,
    type TrackedBuild,
  } from "$lib/stores/builds";
  import { getBuildResult, cancelBuild, ApiError } from "$lib/api";

  let polling = $state<Record<string, boolean>>({});
  let cancelling = $state<Record<string, boolean>>({});
  const timers = new Map<string, number>();

  const trackedBuilds = $derived($builds);

  $effect(() => {
    const active = trackedBuilds.filter((b) => !b.settled);
    for (const b of active) {
      if (!timers.has(b.request_id)) {
        startPolling(b);
      }
    }
    for (const [id, handle] of timers) {
      if (!active.find((b) => b.request_id === id)) {
        clearInterval(handle);
        timers.delete(id);
      }
    }
  });

  onDestroy(() => {
    for (const handle of timers.values()) clearInterval(handle);
    timers.clear();
  });

  function startPolling(build: TrackedBuild) {
    const tick = async () => {
      if (polling[build.request_id]) return;
      polling[build.request_id] = true;
      try {
        const res = await getBuildResult(build.request_id);
        if (res.status === "ok" || res.status === "error") {
          updateBuild(build.request_id, { settled: true, last_result: res });
          const handle = timers.get(build.request_id);
          if (handle !== undefined) {
            clearInterval(handle);
            timers.delete(build.request_id);
          }
        } else {
          updateBuild(build.request_id, { last_result: res });
        }
      } catch (err) {
        const message =
          err instanceof ApiError
            ? err.message
            : err instanceof Error
              ? err.message
              : "poll failed";
        updateBuild(build.request_id, {
          last_result: { request_id: build.request_id, status: "error", message },
        });
      } finally {
        polling[build.request_id] = false;
      }
    };
    tick();
    const handle = window.setInterval(tick, 2000);
    timers.set(build.request_id, handle);
  }

  async function onCancel(id: string) {
    cancelling[id] = true;
    try {
      await cancelBuild(id);
      updateBuild(id, {
        settled: true,
        last_result: {
          request_id: id,
          status: "error",
          message: "Cancelled by user",
          stage: "cancelled",
        },
      });
    } finally {
      cancelling[id] = false;
    }
  }

  function shortDigest(d?: string) {
    if (!d) return "";
    return d.length > 19 ? d.slice(0, 19) + "…" : d;
  }

  function formatDuration(ms?: number) {
    if (ms === undefined || ms === null) return "—";
    if (ms < 1000) return `${ms}ms`;
    return `${(ms / 1000).toFixed(1)}s`;
  }

  function statusOf(b: TrackedBuild): "pending" | "ok" | "error" {
    if (!b.settled) return "pending";
    return b.last_result?.status ?? "error";
  }
</script>

<div class="flex flex-col gap-6">
  <div class="flex items-center justify-between">
    <div>
      <h2 class="flex items-center gap-2 text-sm font-medium text-muted-foreground">
        <Hammer class="h-4 w-4" /> Tracked builds
      </h2>
      <p class="text-xs text-muted-foreground">
        Polls each pending build every 2s. Persists in-memory for the session only.
      </p>
    </div>
    <Button variant="outline" size="sm" onclick={removeSettled} disabled={trackedBuilds.every((b) => !b.settled)}>
      Clear finished
    </Button>
  </div>

  <Card.Root>
    <Card.Content class="p-0">
      {#if trackedBuilds.length === 0}
        <div class="flex flex-col items-center justify-center gap-2 px-6 py-16 text-center">
          <Hammer class="h-8 w-8 text-muted-foreground/50" />
          <p class="text-sm font-medium">No builds tracked yet</p>
          <p class="text-xs text-muted-foreground">Dispatch one from the Deploy tab.</p>
        </div>
      {:else}
        <Table.Root>
          <Table.Header>
            <Table.Row>
              <Table.Head class="w-[220px]">Request</Table.Head>
              <Table.Head class="w-[110px]">Status</Table.Head>
              <Table.Head class="w-[90px]">Duration</Table.Head>
              <Table.Head>Digest / Stage</Table.Head>
              <Table.Head class="w-[120px] text-right pr-4">Actions</Table.Head>
            </Table.Row>
          </Table.Header>
          <Table.Body>
            {#each trackedBuilds as build (build.request_id)}
              {@const s = statusOf(build)}
              {@const r = build.last_result}
              <Table.Row>
                <Table.Cell>
                  <code class="block truncate font-mono text-xs">{build.request_id}</code>
                  <span class="text-[10px] text-muted-foreground"
                    >dispatched {new Date(build.dispatched_at).toLocaleTimeString()}</span
                  >
                </Table.Cell>
                <Table.Cell>
                  {#if s === "pending"}
                    <Badge variant="secondary">
                      <LoaderCircle class="mr-1 h-3 w-3 animate-spin" /> Pending
                    </Badge>
                  {:else if s === "ok"}
                    <Badge variant="success">
                      <CheckCircle2 class="mr-1 h-3 w-3" /> Success
                    </Badge>
                  {:else}
                    <Badge variant="destructive">
                      <XCircle class="mr-1 h-3 w-3" /> Error
                    </Badge>
                  {/if}
                </Table.Cell>
                <Table.Cell class="font-mono text-xs">{formatDuration(r?.duration_ms)}</Table.Cell>
                <Table.Cell>
                  {#if s === "ok" && r?.digest}
                    <code class="font-mono text-xs">{shortDigest(r.digest)}</code>
                    {#if r.registry_ref}
                      <div class="text-[10px] text-muted-foreground">{r.registry_ref}</div>
                    {/if}
                  {:else if s === "error"}
                    <div class="space-y-0.5">
                      {#if r?.stage}
                        <span class="font-mono text-xs">stage: {r.stage}</span>
                      {/if}
                      {#if r?.message}
                        <div class="text-[11px] text-muted-foreground">{r.message}</div>
                      {/if}
                    </div>
                  {:else}
                    <span class="text-xs text-muted-foreground">—</span>
                  {/if}
                </Table.Cell>
                <Table.Cell class="text-right pr-4">
                  {#if !build.settled}
                    <Button
                      variant="ghost"
                      size="sm"
                      onclick={() => onCancel(build.request_id)}
                      disabled={cancelling[build.request_id]}
                    >
                      <X class="h-3.5 w-3.5" /> Cancel
                    </Button>
                  {:else}
                    <Button variant="ghost" size="sm" onclick={() => removeBuild(build.request_id)}>
                      Dismiss
                    </Button>
                  {/if}
                </Table.Cell>
              </Table.Row>
            {/each}
          </Table.Body>
        </Table.Root>
      {/if}
    </Card.Content>
  </Card.Root>
</div>
