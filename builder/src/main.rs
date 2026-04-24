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
use std::io::{self, Write};
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
    let Ok(line) = serde_json::to_string(&frame) else { return };
    // Best-effort stdout. coold may be dead; we don't care.
    let _ = writeln!(std::io::stdout().lock(), "{line}");
    let _ = std::io::stdout().flush();
    // Durable file. Best-effort too but much less likely to fail.
    let _ = writeln!(events, "{line}");
    let _ = events.flush();
}

struct DualSink<'a> {
    events: &'a mut File,
}

impl<'a> ProgressSink for DualSink<'a> {
    fn emit(&mut self, ev: &ProgressEvent) {
        emit_frame(Frame::Progress(ev), self.events);
    }
}

fn write_json_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let ok = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes).and_then(|_| f.sync_all()));
    if let Err(e) = ok {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    Ok(())
}

fn persist_result_json(work_dir: &Path, ok: &BuildResult) -> Result<(), BuildError> {
    let bytes = serde_json::to_vec(ok).unwrap_or_default();
    write_json_atomic(&work_dir.join("result.json"), &bytes).map_err(|e| persist_error("result.json", e))
}

fn persist_error_json(work_dir: &Path, err: &BuildError) -> Result<(), BuildError> {
    let bytes = serde_json::to_vec(err).unwrap_or_default();
    write_json_atomic(&work_dir.join("error.json"), &bytes).map_err(|e| persist_error("error.json", e))
}

fn persist_error(file_name: &str, error: io::Error) -> BuildError {
    BuildError {
        code: 500,
        message: format!("write {file_name}: {error}"),
        stage: "persist".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "coold-builder-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).expect("create test dir");
        dir
    }

    #[test]
    fn write_json_atomic_writes_final_file() {
        let dir = test_dir("json-ok");
        let path = dir.join("result.json");

        write_json_atomic(&path, br#"{"ok":true}"#).expect("write json");

        assert_eq!(
            std::fs::read(&path).expect("read result"),
            br#"{"ok":true}"#
        );
        assert!(
            !path.with_extension("json.tmp").exists(),
            "temp file should not remain after successful rename"
        );

        std::fs::remove_dir_all(dir).expect("cleanup test dir");
    }

    #[test]
    fn write_json_atomic_returns_final_rename_error() {
        let dir = test_dir("json-rename-error");
        let path = dir.join("result.json");
        std::fs::create_dir(&path).expect("create destination directory");

        let err = write_json_atomic(&path, br#"{"ok":true}"#).expect_err("rename should fail");

        assert_ne!(err.kind(), io::ErrorKind::NotFound);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "temp file should be removed after failed rename"
        );

        std::fs::remove_dir_all(dir).expect("cleanup test dir");
    }

    #[test]
    fn result_persist_failure_can_be_saved_as_error_json() {
        let dir = test_dir("json-persist-error");
        std::fs::create_dir(dir.join("result.json")).expect("create result.json directory");
        let ok = BuildResult {
            digest: "sha256:abc".into(),
            registry_ref: "localhost/app@sha256:abc".into(),
            duration_ms: 1,
            stack_used: builder_core::BuildStack::Static,
        };

        let err = persist_result_json(&dir, &ok).expect_err("result persistence should fail");
        persist_error_json(&dir, &err).expect("persist fallback error json");
        let stored: BuildError = serde_json::from_slice(
            &std::fs::read(dir.join("error.json")).expect("read error.json"),
        )
        .expect("parse error.json");

        assert_eq!(stored.stage, "persist");
        assert!(stored.message.contains("write result.json"));

        std::fs::remove_dir_all(dir).expect("cleanup test dir");
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
            match persist_error_json(&work_dir, &err) {
                Ok(()) => emit_frame(Frame::Error { err: &err }, &mut events),
                Err(persist_err) => emit_frame(Frame::Error { err: &persist_err }, &mut events),
            }
            return ExitCode::from(2);
        }
    };

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            let err = BuildError { code: 500, message: format!("install SIGTERM: {e}"), stage: "init".into() };
            match persist_error_json(&work_dir, &err) {
                Ok(()) => emit_frame(Frame::Error { err: &err }, &mut events),
                Err(persist_err) => emit_frame(Frame::Error { err: &persist_err }, &mut events),
            }
            return ExitCode::from(2);
        }
    };

    let mut sink = DualSink { events: &mut events };
    let build_fut = builder_core::run_build(req, &work_dir, &mut sink);

    tokio::select! {
        res = build_fut => match res {
            Ok(ok) => {
                match persist_result_json(&work_dir, &ok) {
                    Ok(()) => {
                        emit_frame(Frame::Result { ok: &ok }, &mut events);
                        ExitCode::SUCCESS
                    }
                    Err(err) => {
                        let _ = persist_error_json(&work_dir, &err);
                        emit_frame(Frame::Error { err: &err }, &mut events);
                        ExitCode::from(2)
                    }
                }
            }
            Err(err) => {
                match persist_error_json(&work_dir, &err) {
                    Ok(()) => {
                        emit_frame(Frame::Error { err: &err }, &mut events);
                        ExitCode::from(1)
                    }
                    Err(persist_err) => {
                        emit_frame(Frame::Error { err: &persist_err }, &mut events);
                        ExitCode::from(2)
                    }
                }
            }
        },
        _ = sigterm.recv() => {
            let err = BuildError { code: 499, message: "cancelled".into(), stage: "cancel".into() };
            match persist_error_json(&work_dir, &err) {
                Ok(()) => {
                    emit_frame(Frame::Error { err: &err }, &mut events);
                    ExitCode::from(130)
                }
                Err(persist_err) => {
                    emit_frame(Frame::Error { err: &persist_err }, &mut events);
                    ExitCode::from(2)
                }
            }
        }
    }
}
