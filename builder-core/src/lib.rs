//! Coolify v5 build engine.
//!
//! Pure library. Emits structured progress via a [`ProgressSink`] so callers
//! (the `builder` binary, tests) control where events go (stdout NDJSON, test
//! collectors, etc.). No gRPC, no JWT, no network transport — those live in
//! the binary wrapper.

use std::path::Path;

pub mod static_build;

mod types;
pub use types::{BuildError, BuildRequest, BuildResult, BuildStack, ProgressEvent, StaticConfig};

pub trait ProgressSink: Send {
    fn emit(&mut self, event: &ProgressEvent);
}

/// Run a build end-to-end. The caller owns the `work_dir` (a per-request
/// scratch directory that the builder may populate freely). The caller is
/// responsible for creating it beforehand and cleaning it afterwards.
pub async fn run_build(
    req: BuildRequest,
    work_dir: &Path,
    sink: &mut dyn ProgressSink,
) -> Result<BuildResult, BuildError> {
    let stack = req.stack;
    match stack {
        BuildStack::Static => static_build::run(req, work_dir, sink).await,
        other => Err(BuildError {
            code: 501,
            message: format!("build stack {other:?} not implemented in MVP"),
            stage: "detect".into(),
        }),
    }
}
