//! Builder subprocess.
//!
//! Spawned by coold as a one-shot per `BuildRequest`. Reads the request JSON
//! from `<request_path>`, runs the build against `<work_dir>`, and emits
//! NDJSON event frames both on stdout (live) and into
//! `<work_dir>/events.ndjson` (durable log coold can replay on restart):
//!
//!   {"type":"progress","stage":"git","log":"...","percent":10}
//!   {"type":"result","ok":{"digest":"sha256:..","registry_ref":"..","duration_ms":1234,"stack_used":"static"}}
//!   {"type":"error","err":{"code":500,"message":"...","stage":"build"}}
//!
//! A final `result.json` or `error.json` is written into the work dir before
//! exit so coold can deliver a Response even if it restarts mid-build and
//! adopts the running unit, or was entirely absent when the build finished.
//!
//! Exit codes: 0 success, 1 build error, 2 usage/IO error, 130 on SIGTERM.
//!
//! Stdout is best-effort. When coold dies the pipe closes and subsequent
//! writes return `BrokenPipe`; we swallow the error and keep writing to the
//! durable file. `SIGPIPE` is ignored at startup so a dead reader never
//! terminates the build.
//!
//! Isolation and cgroup/FS sandboxing are imposed externally by the parent
//! (coold's `systemd-run --pipe` transient service). The builder owns
//! `work_dir` exclusively.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;
use tokio::signal::unix::{signal, SignalKind};
use tracing_subscriber::EnvFilter;

use builder_core::{BuildError, BuildRequest, BuildResult, ProgressEvent, ProgressSink};

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame<'a> {
    Progress(&'a ProgressEvent),
    Result { ok: &'a BuildResult },
    Error { err: &'a BuildError },
}

/// Emit a frame to both stdout (live) and the durable events file.
fn emit_frame(frame: Frame<'_>, events: &mut File) {
    let Ok(line) = serde_json::to_string(&frame) else {
        tracing::warn!("failed to serialize builder frame");
        return;
    };
    // Best-effort stdout. coold may be dead; we don't care.
    if let Err(e) = writeln!(std::io::stdout().lock(), "{line}") {
        tracing::debug!(error = %e, "stdout write failed");
    }
    // Durable file. Best-effort too but much less likely to fail.
    if let Err(e) = writeln!(events, "{line}") {
        tracing::warn!(error = %e, "events file write failed");
    }
}

struct DualSink<'a> {
    events: &'a mut File,
}

impl<'a> ProgressSink for DualSink<'a> {
    fn emit(&mut self, ev: &ProgressEvent) {
        emit_frame(Frame::Progress(ev), self.events);
    }
}

fn write_json_atomic(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("json.tmp");
    let ok = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes).and_then(|_| f.sync_all()));
    if ok.is_ok() {
        if let Err(e) = std::fs::rename(&tmp, path) {
            tracing::warn!(error = %e, path = %path.display(), "failed to rename atomic write result");
        }
    } else {
        tracing::warn!(path = %path.display(), "failed to write result tmp file");
    }
}

fn install_sigpipe_ignore() {
    // Ignore SIGPIPE so a write to a dead stdout pipe never terminates us.
    // Safe: no handler code runs; we inspect write errors on each write.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    install_sigpipe_ignore();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact()
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: builder <request.json> <work_dir>");
        return ExitCode::from(2);
    }
    let req_path = PathBuf::from(&args[1]);
    let work_dir = PathBuf::from(&args[2]);

    // Open the durable events log first — needed for *any* frame we emit,
    // including startup errors.
    let mut events = match OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(work_dir.join("events.ndjson"))
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("open events.ndjson: {e}");
            return ExitCode::from(2);
        }
    };

    let req: BuildRequest = match std::fs::read(&req_path)
        .map_err(|e| format!("read {}: {e}", req_path.display()))
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| format!("parse request: {e}")))
    {
        Ok(r) => r,
        Err(e) => {
            let err = BuildError { code: 400, message: e, stage: "load".into() };
            write_json_atomic(&work_dir.join("error.json"), &serde_json::to_vec(&err).unwrap_or_default());
            emit_frame(Frame::Error { err: &err }, &mut events);
            return ExitCode::from(2);
        }
    };

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            let err = BuildError { code: 500, message: format!("install SIGTERM: {e}"), stage: "init".into() };
            write_json_atomic(&work_dir.join("error.json"), &serde_json::to_vec(&err).unwrap_or_default());
            emit_frame(Frame::Error { err: &err }, &mut events);
            return ExitCode::from(2);
        }
    };

    let mut sink = DualSink { events: &mut events };
    let build_fut = builder_core::run_build(req, &work_dir, &mut sink);

    tokio::select! {
        res = build_fut => match res {
            Ok(ok) => {
                write_json_atomic(&work_dir.join("result.json"), &serde_json::to_vec(&ok).unwrap_or_default());
                emit_frame(Frame::Result { ok: &ok }, &mut events);
                ExitCode::SUCCESS
            }
            Err(err) => {
                write_json_atomic(&work_dir.join("error.json"), &serde_json::to_vec(&err).unwrap_or_default());
                emit_frame(Frame::Error { err: &err }, &mut events);
                ExitCode::from(1)
            }
        },
        _ = sigterm.recv() => {
            let err = BuildError { code: 499, message: "cancelled".into(), stage: "cancel".into() };
            write_json_atomic(&work_dir.join("error.json"), &serde_json::to_vec(&err).unwrap_or_default());
            emit_frame(Frame::Error { err: &err }, &mut events);
            ExitCode::from(130)
        }
    }
}
