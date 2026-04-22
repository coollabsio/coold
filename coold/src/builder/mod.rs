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
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn ensure_work_root(&self) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&self.work_root).await
    }

    /// Stop any `coolify-build-*.service` transient units left behind by a
    /// prior coold run. A clean coold shutdown lets `systemd-run --pipe
    /// --wait` tear its unit down via SIGTERM propagation, but SIGKILL / OOM
    /// / host crash leaves the unit running under PID 1 until RuntimeMaxSec
    /// hits. This sweeper reclaims them on startup.
    pub async fn reap_orphan_units() {
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
                warn!(error = %e, "systemctl list-units failed; skipping orphan sweep");
                return;
            }
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let names: Vec<String> = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
            .filter(|n| n.starts_with("coolify-build-") && n.ends_with(".service"))
            .collect();
        if names.is_empty() {
            return;
        }
        warn!(count = names.len(), units = ?names,
              "reaping orphaned builder units from a prior coold run");
        let _ = Command::new("systemctl")
            .arg("stop")
            .args(&names)
            .status()
            .await;
        let _ = Command::new("systemctl")
            .arg("reset-failed")
            .args(&names)
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
            .arg("-p")
            .arg("RestrictNamespaces=yes")
            .arg("-p")
            .arg("SystemCallArchitectures=native")
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
