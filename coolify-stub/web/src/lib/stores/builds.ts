import { writable } from "svelte/store";
import type { BuildResponseEnvelope } from "$lib/api";

export interface TrackedBuild {
  request_id: string;
  dispatched_at: string;
  settled: boolean;
  last_result?: BuildResponseEnvelope;
}

export const builds = writable<TrackedBuild[]>([]);

export function trackBuild(request_id: string) {
  builds.update((list) => {
    if (list.some((b) => b.request_id === request_id)) return list;
    const entry: TrackedBuild = {
      request_id,
      dispatched_at: new Date().toISOString(),
      settled: false,
    };
    return [entry, ...list];
  });
}

export function updateBuild(request_id: string, patch: Partial<TrackedBuild>) {
  builds.update((list) =>
    list.map((b) => (b.request_id === request_id ? { ...b, ...patch } : b)),
  );
}

export function removeSettled() {
  builds.update((list) => list.filter((b) => !b.settled));
}

export function removeBuild(request_id: string) {
  builds.update((list) => list.filter((b) => b.request_id !== request_id));
}
