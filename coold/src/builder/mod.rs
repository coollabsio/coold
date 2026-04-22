//! Builder dispatch adapter.
//!
//! coold spawns one `builder` subprocess per `BuildRequest` inside a
//! transient systemd service unit via `systemd-run --pipe`. That gives
//! the build a cgroup with memory/CPU caps plus filesystem sandboxing
//! (PrivateTmp, ProtectSystem=strict, ReadWritePaths allowlist) while
//! still piping stdout/stderr back to coold so the NDJSON event frames
//! can be parsed in real time.
//!
//! (A `systemd-run --scope` was the original design but scopes are
//! process-adoption wrappers and reject service-only properties like
//! PrivateTmp — building with them errors `Unknown assignment:
//! PrivateTmp=yes`. Transient services accept the full sandbox set.)
//!
//! Cancellation uses `systemctl kill --signal=SIGTERM` against the
//! transient unit name; the cgroup kill takes `buildah` and `git`
//! children down in the same sweep.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex, Semaphore};
use tracing::{info, warn};

use crate::grpc::proto::{
    build_response_body, response, BuildError, BuildRequest, BuildResponseBody, BuildResult,
    BuildStack, ClientMsg, Response, StaticConfig,
};

pub struct BuilderCtx {
    sem: Arc<Semaphore>,
    work_root: PathBuf,
    builder_bin: PathBuf,
    timeout_secs: u64,
    memory_max: String,
    cpu_quota: String,
    deny_nets: Vec<String>,
    active: Arc<Mutex<HashMap<String, BuildHandle>>>,
}

struct BuildHandle {
    unit_name: String,
}

pub struct BuilderSettings {
    pub work_root: PathBuf,
    pub builder_bin: PathBuf,
    pub capacity: u32,
    pub timeout_secs: u64,
    pub memory_max: String,
    pub cpu_quota: String,
    /// Extra CIDRs the build subprocess is forbidden to reach. Combined with
    /// a fixed localhost/link-local/ULA set below.
    pub deny_nets: Vec<String>,
}

impl BuilderCtx {
    pub fn new(settings: BuilderSettings) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(settings.capacity as usize)),
            work_root: settings.work_root,
            builder_bin: settings.builder_bin,
            timeout_secs: settings.timeout_secs,
            memory_max: settings.memory_max,
            cpu_quota: settings.cpu_quota,
            deny_nets: settings
                .deny_nets
                .into_iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn ensure_work_root(&self) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&self.work_root).await
    }

    /// Resume or reap builder transient units left by a prior coold run.
    /// Units still running are adopted: a background task polls until the
    /// unit exits, reads the on-disk `result.json` / `error.json` the
    /// builder wrote, and emits a `Response` on `tx` so Laravel eventually
    /// sees the outcome. Units that finished while coold was dead are
    /// emitted immediately. Units with no on-disk trace (hard kill) get a
    /// fabricated 500.
    pub async fn resume_or_reap(self: &Arc<Self>, tx: mpsc::Sender<ClientMsg>) {
        let units = list_coolify_build_units().await;
        if units.is_empty() {
            return;
        }
        info!(count = units.len(), "resuming builder units from prior coold run");
        for unit in units {
            let Some(request_id) = parse_request_id(&unit) else { continue };
            let work_dir = self.work_root.join(&request_id);
            let active = systemctl_is_active(&unit).await;
            let result_path = work_dir.join("result.json");
            let error_path = work_dir.join("error.json");

            if active {
                // Adopt the running unit. Register in active_builds so an
                // incoming CancelBuild for this request routes correctly,
                // then spawn a waiter task.
                self.active.lock().await.insert(
                    request_id.clone(),
                    BuildHandle { unit_name: unit.clone() },
                );
                let ctx = self.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    info!(%request_id, %unit, "adopting in-flight builder unit");
                    ctx.wait_and_emit(request_id, unit, work_dir, tx).await;
                });
                continue;
            }

            // Unit no longer running — deliver whatever the builder managed
            // to write, then clean up.
            let body = if let Ok(bytes) = tokio::fs::read(&result_path).await {
                body_from_result_bytes(&bytes)
            } else if let Ok(bytes) = tokio::fs::read(&error_path).await {
                body_from_error_bytes(&bytes)
            } else {
                warn!(%request_id, "orphan unit exited without a result/error file");
                body_fabricated_error("builder exited without result file")
            };
            emit_build_response(&tx, &request_id, body).await;
            let _ = tokio::fs::remove_dir_all(&work_dir).await;
            let _ = Command::new("systemctl")
                .arg("reset-failed")
                .arg(&unit)
                .status()
                .await;
        }
    }

    /// Poll an adopted unit until it goes inactive, then emit the Response
    /// from whatever the builder persisted. Used by `resume_or_reap` and
    /// conceptually equivalent to `spawn_and_reap` minus the stdout drain
    /// (which is already gone for an adopted unit).
    async fn wait_and_emit(
        self: Arc<Self>,
        request_id: String,
        unit: String,
        work_dir: std::path::PathBuf,
        tx: mpsc::Sender<ClientMsg>,
    ) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if !systemctl_is_active(&unit).await {
                break;
            }
        }
        let result_path = work_dir.join("result.json");
        let error_path = work_dir.join("error.json");
        let body = if let Ok(bytes) = tokio::fs::read(&result_path).await {
            body_from_result_bytes(&bytes)
        } else if let Ok(bytes) = tokio::fs::read(&error_path).await {
            body_from_error_bytes(&bytes)
        } else {
            warn!(%request_id, %unit, "adopted unit exited without a result/error file");
            body_fabricated_error("adopted unit exited without result file")
        };
        emit_build_response(&tx, &request_id, body).await;
        self.active.lock().await.remove(&request_id);
        let _ = tokio::fs::remove_dir_all(&work_dir).await;
        let _ = Command::new("systemctl")
            .arg("reset-failed")
            .arg(&unit)
            .status()
            .await;
    }

    /// Spawn a build. Returns immediately after the subprocess is started;
    /// the reader task stays alive in a detached tokio task and sends the
    /// final `Response` frame on `tx` when the subprocess exits.
    pub fn dispatch(self: Arc<Self>, request_id: String, req: BuildRequest, tx: mpsc::Sender<ClientMsg>) {
        let ctx = self;
        tokio::spawn(async move {
            let Ok(permit) = ctx.sem.clone().acquire_owned().await else {
                send_err(&tx, &request_id, 500, "build semaphore closed", "dispatch").await;
                return;
            };
            let outcome = ctx.run_build(&request_id, req).await;
            drop(permit);
            let body = match outcome {
                Ok(ok) => BuildResponseBody { body: Some(build_response_body::Body::Ok(ok)) },
                Err(e) => BuildResponseBody { body: Some(build_response_body::Body::Err(e)) },
            };
            let msg = ClientMsg {
                payload: Some(crate::grpc::proto::client_msg::Payload::Response(Response {
                    request_id,
                    body: Some(response::Body::Build(body)),
                })),
            };
            if let Err(e) = tx.send(msg).await {
                warn!(error = %e, "failed to enqueue build response");
            }
        });
    }

    /// Request cancellation. Returns `true` iff an active build was found and
    /// a `systemctl kill` was issued. The build's completion is still
    /// reported via the normal outbound response path once the subprocess
    /// exits.
    pub async fn cancel(&self, request_id: &str) -> bool {
        let unit_name = match self.active.lock().await.get(request_id) {
            Some(h) => h.unit_name.clone(),
            None => return false,
        };
        match Command::new("systemctl")
            .args(["kill", "--signal=SIGTERM", &unit_name])
            .status()
            .await
        {
            Ok(s) if s.success() => {
                info!(%request_id, %unit_name, "cancel SIGTERM sent");
                true
            }
            Ok(s) => {
                warn!(%request_id, %unit_name, status = ?s, "systemctl kill non-zero");
                true
            }
            Err(e) => {
                warn!(%request_id, %unit_name, error = %e, "systemctl kill failed");
                false
            }
        }
    }

    async fn run_build(&self, request_id: &str, req: BuildRequest) -> Result<BuildResult, BuildError> {
        let work_dir = self.work_root.join(request_id);
        tokio::fs::create_dir_all(&work_dir)
            .await
            .map_err(|e| build_err(500, "setup", format!("mkdir work: {e}")))?;

        let req_path = work_dir.join("request.json");
        let req_json = serde_json::to_vec(&SubprocessRequest::from_proto(&req))
            .map_err(|e| build_err(500, "setup", format!("encode request: {e}")))?;
        tokio::fs::write(&req_path, &req_json)
            .await
            .map_err(|e| build_err(500, "setup", format!("write request.json: {e}")))?;

        let unit_name = format!("coolify-build-{request_id}.service");
        let mut cmd = Command::new("systemd-run");
        cmd.arg("--pipe")
            .arg("--quiet")
            .arg("--collect")
            .arg(format!("--unit={unit_name}"))
            .arg("-p")
            .arg(format!("RuntimeMaxSec={}", self.timeout_secs))
            .arg("-p")
            .arg(format!("MemoryMax={}", self.memory_max))
            .arg("-p")
            .arg(format!("CPUQuota={}", self.cpu_quota))
            .arg("-p")
            .arg("PrivateTmp=yes")
            // ProtectSystem=strict mounts / read-only except /dev, /proc, /sys
            // and the explicit ReadWritePaths below. Full allowlist:
            //   * `work_dir`                — git clone + generated Containerfile
            //   * /var/lib/containers       — buildah image store + overlays
            //   * /run/containers           — buildah/libpod runtime state
            //   * /run/netavark             — network-plugin state
            //   * /run/lock                 — netavark.lock
            // /tmp + /var/tmp are ephemeral via PrivateTmp; /home and /root
            // are hidden via ProtectHome.
            .arg("-p")
            .arg("ProtectSystem=strict")
            .arg("-p")
            .arg("ProtectHome=yes")
            .arg("-p")
            .arg(format!("ReadWritePaths={}", work_dir.display()))
            // "-" prefix = tolerate missing path. /run/containers and
            // /run/netavark are created lazily by buildah/netavark on first
            // use; without the prefix systemd refuses to start the unit with
            // "Failed to set up mount namespacing: No such file or directory".
            .arg("-p")
            .arg("ReadWritePaths=/var/lib/containers")
            .arg("-p")
            .arg("ReadWritePaths=-/run/containers")
            .arg("-p")
            .arg("ReadWritePaths=-/run/netavark")
            .arg("-p")
            .arg("ReadWritePaths=-/run/lock")
            // Defense-in-depth. Builder runs as root for buildah's benefit,
            // so lock down everything not strictly required. CAP_* trim and
            // SystemCallFilter are deferred until the ReadWritePaths set is
            // stable (easier to iterate on one dimension at a time).
            .arg("-p")
            .arg("NoNewPrivileges=yes")
            .arg("-p")
            .arg("RestrictSUIDSGID=yes")
            .arg("-p")
            .arg("LockPersonality=yes")
            .arg("-p")
            .arg("RestrictRealtime=yes")
            // buildah creates mount + user namespaces to extract image layers
            // ("creating mount namespace before pivot"). Allow those two but
            // continue denying cgroup/net/uts/pid/ipc that builds never need.
            .arg("-p")
            .arg("RestrictNamespaces=mnt user")
            .arg("-p")
            .arg("SystemCallArchitectures=native");
        // Network deny list via eBPF (IPAddressDeny). Evaluated with
        // longest-prefix match; no Allow entries means the default is
        // allow-all, which we intentionally keep so git clone + registry
        // pulls reach the public internet. We deliberately do NOT block
        // 127.0.0.0/8 wholesale — systemd-resolved's 127.0.0.53:53 stub
        // resolver needs to stay reachable for DNS. Instead we block
        // 127.0.0.1 specifically (Redis, Corrosion API, etc.).
        for net in [
            "127.0.0.1",
            "169.254.0.0/16",
            "::1/128",
            "fc00::/7",
            "fe80::/10",
        ] {
            cmd.arg("-p").arg(format!("IPAddressDeny={net}"));
        }
        for net in &self.deny_nets {
            cmd.arg("-p").arg(format!("IPAddressDeny={net}"));
        }
        cmd
            .arg("--")
            .arg(&self.builder_bin)
            .arg(&req_path)
            .arg(&work_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        self.active
            .lock()
            .await
            .insert(request_id.to_string(), BuildHandle { unit_name: unit_name.clone() });

        let outcome = self.spawn_and_reap(&mut cmd, request_id, &work_dir).await;

        self.active.lock().await.remove(request_id);
        if let Err(e) = tokio::fs::remove_dir_all(&work_dir).await {
            warn!(%request_id, error = %e, "workdir cleanup failed");
        }

        outcome
    }

    async fn spawn_and_reap(
        &self,
        cmd: &mut Command,
        request_id: &str,
        work_dir: &std::path::Path,
    ) -> Result<BuildResult, BuildError> {
        let mut child = cmd
            .spawn()
            .map_err(|e| build_err(500, "spawn", format!("systemd-run: {e}")))?;
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        let rid_err = request_id.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                info!(request_id = %rid_err, builder_stderr = %line);
            }
        });

        let mut final_body: Option<Result<BuildResult, BuildError>> = None;
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match serde_json::from_str::<Frame>(&line) {
                Ok(Frame::Progress { stage, log, percent }) => {
                    info!(%request_id, %stage, %percent, "{log}");
                }
                Ok(Frame::Result { ok }) => {
                    final_body = Some(Ok(ok.into_proto()));
                }
                Ok(Frame::Error { err }) => {
                    final_body = Some(Err(err.into_proto()));
                }
                Err(e) => warn!(%request_id, %e, line = %line, "malformed builder frame"),
            }
        }

        let status = child
            .wait()
            .await
            .map_err(|e| build_err(500, "reap", format!("wait: {e}")))?;

        match final_body {
            Some(body) => body,
            None => Err(build_err(
                500,
                "reap",
                format!(
                    "builder exited ({status}) without a result frame; work_dir={}",
                    work_dir.display()
                ),
            )),
        }
    }
}

fn build_err(code: u32, stage: &str, message: impl Into<String>) -> BuildError {
    BuildError {
        code,
        message: message.into(),
        stage: stage.into(),
    }
}

async fn send_err(tx: &mpsc::Sender<ClientMsg>, request_id: &str, code: u32, message: &str, stage: &str) {
    let body = BuildResponseBody {
        body: Some(build_response_body::Body::Err(build_err(code, stage, message))),
    };
    let msg = ClientMsg {
        payload: Some(crate::grpc::proto::client_msg::Payload::Response(Response {
            request_id: request_id.to_string(),
            body: Some(response::Body::Build(body)),
        })),
    };
    let _ = tx.send(msg).await;
}

// ── wire format with the builder subprocess ───────────────────────────────
//
// coold writes request.json in the shape expected by the builder binary (the
// shape defined by `builder-core::BuildRequest`) and parses NDJSON frames
// matching the shape the binary emits. Keeping these types private to this
// module avoids a compile-time dependency on builder-core.

#[derive(Debug, Serialize)]
struct SubprocessRequest {
    repo_url: String,
    git_ref: String,
    stack: SubprocessStack,
    target_image: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    cache_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    static_cfg: Option<SubprocessStatic>,
}

#[derive(Debug, Serialize)]
struct SubprocessStatic {
    #[serde(skip_serializing_if = "String::is_empty")]
    output_dir: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    base_image: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum SubprocessStack {
    Unspecified,
    Dockerfile,
    Buildpacks,
    Railpack,
    Static,
}

impl From<BuildStack> for SubprocessStack {
    fn from(s: BuildStack) -> Self {
        match s {
            BuildStack::Unspecified => Self::Unspecified,
            BuildStack::Dockerfile => Self::Dockerfile,
            BuildStack::Buildpacks => Self::Buildpacks,
            BuildStack::Railpack => Self::Railpack,
            BuildStack::Static => Self::Static,
        }
    }
}

impl SubprocessStack {
    fn into_proto(self) -> BuildStack {
        match self {
            Self::Unspecified => BuildStack::Unspecified,
            Self::Dockerfile => BuildStack::Dockerfile,
            Self::Buildpacks => BuildStack::Buildpacks,
            Self::Railpack => BuildStack::Railpack,
            Self::Static => BuildStack::Static,
        }
    }
}

impl SubprocessRequest {
    fn from_proto(req: &BuildRequest) -> Self {
        let stack = BuildStack::try_from(req.stack).unwrap_or(BuildStack::Unspecified);
        let static_cfg = req.static_cfg.as_ref().map(|c: &StaticConfig| SubprocessStatic {
            output_dir: c.output_dir.clone(),
            base_image: c.base_image.clone(),
        });
        SubprocessRequest {
            repo_url: req.repo_url.clone(),
            git_ref: req.git_ref.clone(),
            stack: stack.into(),
            target_image: req.target_image.clone(),
            cache_key: req.cache_key.clone(),
            static_cfg,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame {
    Progress { stage: String, log: String, percent: u32 },
    Result { ok: FrameResult },
    Error { err: FrameError },
}

#[derive(Debug, Deserialize)]
struct FrameResult {
    digest: String,
    registry_ref: String,
    duration_ms: u64,
    stack_used: SubprocessStack,
}

impl FrameResult {
    fn into_proto(self) -> BuildResult {
        BuildResult {
            digest: self.digest,
            registry_ref: self.registry_ref,
            duration_ms: self.duration_ms,
            stack_used: self.stack_used.into_proto() as i32,
        }
    }
}

#[derive(Debug, Deserialize)]
struct FrameError {
    code: u32,
    message: String,
    #[serde(default)]
    stage: String,
}

impl FrameError {
    fn into_proto(self) -> BuildError {
        BuildError { code: self.code, message: self.message, stage: self.stage }
    }
}

// ── resume helpers ──────────────────────────────────────────────────────
//
// `list_coolify_build_units` + `parse_request_id` + the JSON parsers below
// let `BuilderCtx::resume_or_reap` reconstruct in-flight builds across a
// coold restart without relying on any in-memory state.

async fn list_coolify_build_units() -> Vec<String> {
    let out = match Command::new("systemctl")
        .args([
            "list-units",
            "--all",
            "--no-legend",
            "--plain",
            "--type=service",
            "coolify-build-*.service",
        ])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            warn!(error = %e, "systemctl list-units failed; skipping resume");
            return vec![];
        }
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
        .filter(|n| n.starts_with("coolify-build-") && n.ends_with(".service"))
        .collect()
}

async fn systemctl_is_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", unit])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

fn parse_request_id(unit_name: &str) -> Option<String> {
    unit_name
        .strip_prefix("coolify-build-")
        .and_then(|rest| rest.strip_suffix(".service"))
        .map(str::to_owned)
}

/// Shape of `result.json` the builder persists on success. Mirrors
/// `builder_core::BuildResult` (which is where the type originates).
#[derive(Debug, Deserialize)]
struct StoredResult {
    digest: String,
    registry_ref: String,
    duration_ms: u64,
    stack_used: SubprocessStack,
}

impl StoredResult {
    fn into_proto(self) -> BuildResult {
        BuildResult {
            digest: self.digest,
            registry_ref: self.registry_ref,
            duration_ms: self.duration_ms,
            stack_used: self.stack_used.into_proto() as i32,
        }
    }
}

/// Shape of `error.json` the builder persists on failure/cancel.
#[derive(Debug, Deserialize)]
struct StoredError {
    code: u32,
    message: String,
    #[serde(default)]
    stage: String,
}

impl StoredError {
    fn into_proto(self) -> BuildError {
        BuildError { code: self.code, message: self.message, stage: self.stage }
    }
}

fn body_from_result_bytes(bytes: &[u8]) -> BuildResponseBody {
    match serde_json::from_slice::<StoredResult>(bytes) {
        Ok(r) => BuildResponseBody { body: Some(build_response_body::Body::Ok(r.into_proto())) },
        Err(e) => body_fabricated_error(&format!("malformed result.json: {e}")),
    }
}

fn body_from_error_bytes(bytes: &[u8]) -> BuildResponseBody {
    match serde_json::from_slice::<StoredError>(bytes) {
        Ok(e) => BuildResponseBody { body: Some(build_response_body::Body::Err(e.into_proto())) },
        Err(e) => body_fabricated_error(&format!("malformed error.json: {e}")),
    }
}

fn body_fabricated_error(message: &str) -> BuildResponseBody {
    BuildResponseBody {
        body: Some(build_response_body::Body::Err(BuildError {
            code: 500,
            message: message.to_owned(),
            stage: "reap".into(),
        })),
    }
}

async fn emit_build_response(
    tx: &mpsc::Sender<ClientMsg>,
    request_id: &str,
    body: BuildResponseBody,
) {
    let msg = ClientMsg {
        payload: Some(crate::grpc::proto::client_msg::Payload::Response(Response {
            request_id: request_id.to_owned(),
            body: Some(response::Body::Build(body)),
        })),
    };
    if let Err(e) = tx.send(msg).await {
        warn!(%request_id, error = %e, "failed to enqueue resumed build response");
    }
}
