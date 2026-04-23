// JSON envelope types exchanged with the coold broker over its UDS HTTP lane.
// Mirror of broker/src/envelope.rs — keep in sync with the Rust definitions.

// ─── Coold dispatch ──────────────────────────────────────────────────────────

export interface DispatchEnvelope {
  host_id: string;
  request_id: string;
  command: CommandPayload;
}

export type CommandPayload = { type: "list_containers" };

export interface ResponseEnvelope {
  request_id: string;
  status: "ok" | "error";
  data?: unknown;
  code?: number;
  message?: string;
}

// ─── Build dispatch ──────────────────────────────────────────────────────────

export interface BuildDispatchEnvelope {
  host_id?: string;
  request_id: string;
  command: BuildCommandPayload;
}

export type BuildCommandPayload =
  | {
      type: "static_build";
      repo_url: string;
      git_ref: string;
      target_image: string;
      output_dir?: string;
      base_image?: string;
    }
  | { type: "cancel" };

export interface BuildResponseEnvelope {
  request_id: string;
  status: "ok" | "error";
  digest?: string;
  registry_ref?: string;
  duration_ms?: number;
  code?: number;
  message?: string;
  stage?: string;
}

export interface BuildDispatchAck {
  request_id: string;
}

export interface ContainerSummary {
  id: string;
  name: string;
  image: string;
  state: string;
  networks: string[];
}
