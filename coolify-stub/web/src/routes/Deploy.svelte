<script lang="ts">
  import { onMount } from "svelte";
  import { push } from "svelte-spa-router";
  import { Rocket, LoaderCircle, Play, CheckCircle2, XCircle } from "lucide-svelte";
  import * as Card from "$lib/components/ui/card";
  import { Input } from "$lib/components/ui/input";
  import { Label } from "$lib/components/ui/label";
  import { Button } from "$lib/components/ui/button";
  import { Badge } from "$lib/components/ui/badge";
  import { Separator } from "$lib/components/ui/separator";
  import { dispatchBuild, getHosts, ApiError, type DispatchBuildBody } from "$lib/api";
  import { trackBuild, updateBuild } from "$lib/stores/builds";

  let hosts = $state<string[]>([]);
  let hostId = $state("");
  let repoUrl = $state("https://github.com/mdn/beginner-html-site");
  let gitRef = $state("main");
  let targetImage = $state("localhost/coolify-demo:v1");
  // Default to "." so the MDN demo repo (files at repo root) works OOTB.
  // Real SPA projects will change this to "dist" / "build" / etc.
  let outputDir = $state(".");
  let baseImage = $state("");

  let submitting = $state(false);
  let lastRequestId = $state<string | null>(null);
  let formError = $state<string | null>(null);
  let syncResult = $state<{ status: "ok" | "error"; message?: string } | null>(null);

  onMount(async () => {
    try {
      const res = await getHosts();
      hosts = res.hosts ?? [];
    } catch {
      hosts = [];
    }
  });

  async function onSubmit(event: SubmitEvent) {
    event.preventDefault();
    formError = null;
    lastRequestId = null;
    syncResult = null;
    submitting = true;
    try {
      const body: DispatchBuildBody = {
        repo_url: repoUrl.trim(),
        git_ref: gitRef.trim(),
        target_image: targetImage.trim(),
      };
      if (hostId.trim()) body.host_id = hostId.trim();
      if (outputDir.trim()) body.output_dir = outputDir.trim();
      if (baseImage.trim()) body.base_image = baseImage.trim();

      const res = await dispatchBuild(body);
      if (!res || typeof res !== "object" || !("request_id" in res)) {
        throw new Error("Malformed response from scheduler");
      }
      lastRequestId = res.request_id;
      trackBuild(res.request_id);

      if ("status" in res && (res.status === "ok" || res.status === "error")) {
        const settled = res as import("$lib/api").BuildResponseEnvelope;
        updateBuild(res.request_id, { settled: true, last_result: settled });
        syncResult = { status: settled.status, message: settled.message };
      }
    } catch (err) {
      formError =
        err instanceof ApiError ? err.message : err instanceof Error ? err.message : "Unknown error";
    } finally {
      submitting = false;
    }
  }

  function watchInBuilds() {
    push("/builds");
  }
</script>

<div class="grid gap-6 lg:grid-cols-[minmax(0,1fr)_360px]">
  <Card.Root>
    <Card.Header>
      <Card.Title class="flex items-center gap-2 text-base">
        <Rocket class="h-4 w-4 text-muted-foreground" /> Dispatch a build
      </Card.Title>
      <Card.Description>
        Forwards to the coold scheduler, which selects an agent and runs builder-core.
      </Card.Description>
    </Card.Header>
    <Card.Content>
      <form class="grid gap-5" onsubmit={onSubmit}>
        <div class="grid gap-2">
          <Label for="host">Host</Label>
          <select
            id="host"
            bind:value={hostId}
            class="flex h-9 w-full rounded-md border border-input bg-transparent px-3 py-1 text-sm shadow-sm focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
          >
            <option value="">Auto (scheduler load-balances)</option>
            {#each hosts as host}
              <option value={host}>{host}</option>
            {/each}
          </select>
          <p class="text-xs text-muted-foreground">Leave blank to let the scheduler pick.</p>
        </div>

        <div class="grid gap-2">
          <Label for="repo">Repo URL</Label>
          <Input id="repo" bind:value={repoUrl} required placeholder="https://github.com/owner/repo" />
        </div>

        <div class="grid gap-5 sm:grid-cols-2">
          <div class="grid gap-2">
            <Label for="ref">Git ref</Label>
            <Input id="ref" bind:value={gitRef} required placeholder="main" />
          </div>
          <div class="grid gap-2">
            <Label for="target">Target image</Label>
            <Input id="target" bind:value={targetImage} required placeholder="localhost/app:v1" />
          </div>
        </div>

        <div class="grid gap-5 sm:grid-cols-2">
          <div class="grid gap-2">
            <Label for="output">Output dir (optional)</Label>
            <Input id="output" bind:value={outputDir} placeholder="dist" />
          </div>
          <div class="grid gap-2">
            <Label for="base">Base image (optional)</Label>
            <Input id="base" bind:value={baseImage} placeholder="nginx:alpine" />
          </div>
        </div>

        {#if formError}
          <div class="rounded-md border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">
            {formError}
          </div>
        {/if}

        <div class="flex items-center gap-3">
          <Button type="submit" disabled={submitting}>
            {#if submitting}
              <LoaderCircle class="h-4 w-4 animate-spin" />
              Dispatching…
            {:else}
              <Play class="h-4 w-4" />
              Dispatch Build
            {/if}
          </Button>
          {#if lastRequestId}
            <Button type="button" variant="outline" onclick={watchInBuilds}>Watch in Builds</Button>
          {/if}
        </div>
      </form>
    </Card.Content>
  </Card.Root>

  <Card.Root>
    <Card.Header>
      <Card.Title class="text-base">Last dispatch</Card.Title>
      <Card.Description>Result of the most recent request.</Card.Description>
    </Card.Header>
    <Card.Content class="space-y-3">
      {#if !lastRequestId}
        <p class="text-sm text-muted-foreground">Nothing dispatched yet.</p>
      {:else}
        <div class="space-y-1">
          <div class="text-xs font-medium text-muted-foreground">request_id</div>
          <code class="break-all rounded bg-muted px-2 py-1 font-mono text-xs">{lastRequestId}</code>
        </div>
        <Separator />
        {#if syncResult}
          {#if syncResult.status === "ok"}
            <Badge variant="success">
              <CheckCircle2 class="mr-1 h-3 w-3" /> Completed synchronously
            </Badge>
          {:else}
            <Badge variant="destructive">
              <XCircle class="mr-1 h-3 w-3" /> Failed synchronously
            </Badge>
            {#if syncResult.message}
              <p class="text-xs text-muted-foreground">{syncResult.message}</p>
            {/if}
          {/if}
        {:else}
          <Badge variant="secondary">
            <LoaderCircle class="mr-1 h-3 w-3 animate-spin" /> Accepted — poll for result
          </Badge>
        {/if}
      {/if}
    </Card.Content>
  </Card.Root>
</div>
