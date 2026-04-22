//! Builder subprocess.
//!
//! Spawned by coold as a one-shot per `BuildRequest`. Reads the request JSON
//! from `<request_path>`, runs the build against `<work_dir>`, and emits
//! NDJSON event frames on stdout:
//!
//!   {"type":"progress","stage":"git","log":"...","percent":10}
//!   {"type":"result","ok":{"digest":"sha256:..","registry_ref":"..","duration_ms":1234,"stack_used":"static"}}
//!   {"type":"error","err":{"code":500,"message":"...","stage":"build"}}
//!
//! Exit codes: 0 on success, 1 on build error, 2 on usage/IO error, 130 on
//! SIGTERM. On SIGTERM the builder emits a final `{"type":"error","err":
//! {"code":499,"message":"cancelled",...}}` before exiting.
//!
//! Isolation and cgroup/FS sandboxing are imposed externally by the parent
//! (typically `systemd-run --scope` wrapping this invocation). The builder
//! itself assumes it owns `work_dir` exclusively and may write anywhere
//! underneath.

use std::io::Write;
use std::path::PathBuf;
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

struct NdjsonSink;

impl ProgressSink for NdjsonSink {
    fn emit(&mut self, ev: &ProgressEvent) {
        emit_frame(Frame::Progress(ev));
    }
}

fn emit_frame(frame: Frame<'_>) {
    let mut stdout = std::io::stdout().lock();
    if let Ok(line) = serde_json::to_string(&frame) {
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}

#[tokio::main]
async fn main() -> ExitCode {
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

    let req: BuildRequest = match std::fs::read(&req_path)
        .map_err(|e| format!("read {}: {e}", req_path.display()))
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| format!("parse request: {e}")))
    {
        Ok(r) => r,
        Err(e) => {
            emit_frame(Frame::Error {
                err: &BuildError { code: 400, message: e, stage: "load".into() },
            });
            return ExitCode::from(2);
        }
    };

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            emit_frame(Frame::Error {
                err: &BuildError { code: 500, message: format!("install SIGTERM: {e}"), stage: "init".into() },
            });
            return ExitCode::from(2);
        }
    };

    let mut sink = NdjsonSink;
    let build_fut = builder_core::run_build(req, &work_dir, &mut sink);

    tokio::select! {
        res = build_fut => match res {
            Ok(ok)  => { emit_frame(Frame::Result { ok: &ok }); ExitCode::SUCCESS }
            Err(err) => { emit_frame(Frame::Error { err: &err }); ExitCode::from(1) }
        },
        _ = sigterm.recv() => {
            emit_frame(Frame::Error {
                err: &BuildError { code: 499, message: "cancelled".into(), stage: "cancel".into() },
            });
            ExitCode::from(130)
        }
    }
}
