use std::path::Path;
use std::time::Instant;

use tokio::process::Command;
use tracing::info;

use crate::{BuildError, BuildRequest, BuildResult, BuildStack, ProgressEvent, ProgressSink};

pub async fn run(
    req: BuildRequest,
    work_dir: &Path,
    sink: &mut dyn ProgressSink,
) -> Result<BuildResult, BuildError> {
    let started = Instant::now();
    let static_cfg = req.static_cfg.clone().unwrap_or_default();
    let output_dir = if static_cfg.output_dir.is_empty() {
        "dist".to_owned()
    } else {
        static_cfg.output_dir.clone()
    };
    let base_image = if static_cfg.base_image.is_empty() {
        "docker.io/library/nginx:alpine".to_owned()
    } else {
        static_cfg.base_image.clone()
    };

    // Containerfile is a templated shell-free format, but the strings still
    // land in `FROM` and `COPY` lines. Newlines forge new directives; `..`
    // in `output_dir` reads outside the work tree. Reject before any I/O.
    if !is_safe_base_image(&base_image) {
        return Err(err(400, "detect", "invalid base_image"));
    }
    if !is_safe_output_dir(&output_dir) {
        return Err(err(400, "detect", "invalid output_dir"));
    }

    // git's `--<flag>` parser accepts options anywhere on the command line
    // unless terminated with `--`. Without it a malicious `repo_url` like
    // `--upload-pack=...` becomes a flag and runs arbitrary commands. Same
    // story for `git_ref` on `checkout` / `fetch`.
    emit(sink, "git", format!("cloning {} @ {}", req.repo_url, req.git_ref), 0);

    if !run_ok(
        "git",
        &["clone", "--depth", "1", "--no-tags", "--", &req.repo_url, "repo"],
        work_dir,
    )
    .await?
    {
        return Err(err(500, "git", "git clone failed"));
    }

    let repo_dir = work_dir.join("repo");
    let checkout_ok = run_ok("git", &["checkout", "--", &req.git_ref], &repo_dir).await?;
    if !checkout_ok {
        if !run_ok(
            "git",
            &["fetch", "--depth", "1", "origin", "--", &req.git_ref],
            &repo_dir,
        )
        .await?
        {
            return Err(err(500, "git", format!("git fetch ref {} failed", req.git_ref)));
        }
        run_ok("git", &["checkout", "--", "FETCH_HEAD"], &repo_dir).await?;
    }

    emit(sink, "git", "clone complete", 10);

    emit(sink, "detect", format!("checking output_dir: {output_dir}"), 15);
    let out_path = repo_dir.join(&output_dir);
    if !out_path.exists() {
        return Err(err(400, "detect", format!("output_dir '{output_dir}' not found in repo")));
    }

    let containerfile = format!("FROM {base_image}\nCOPY ./{output_dir} /usr/share/nginx/html\n");
    tokio::fs::write(repo_dir.join("Containerfile.coolify"), &containerfile)
        .await
        .map_err(|e| err(500, "detect", format!("write Containerfile: {e}")))?;

    emit(sink, "build", format!("buildah bud → {}", req.target_image), 20);
    info!(target_image = %req.target_image, "starting buildah bud");

    // `--iidfile` writes the image ID (sha256:… of the OCI config) during
    // `bud`. Reading it from disk after the build avoids a second buildah
    // process, which on some kernels would trip
    // `remount /var/lib/containers/storage/overlay, flags: 0x40000:
    // invalid argument` inside coold's `systemd-run` sandbox because
    // buildah's overlay driver tries to MS_PRIVATE-remount a bind mount
    // that its predecessor `bud` already placed at that path.
    let iid_path = work_dir.join("image-id");
    let iid_arg = iid_path.to_string_lossy().into_owned();

    if !run_ok(
        "buildah",
        &[
            "bud",
            "--storage-driver",
            "overlay",
            "--iidfile",
            &iid_arg,
            "-t",
            &req.target_image,
            "-f",
            "Containerfile.coolify",
            ".",
        ],
        &repo_dir,
    )
    .await?
    {
        return Err(err(500, "build", format!("buildah bud failed for {}", req.target_image)));
    }

    emit(sink, "build", "build complete", 80);

    emit(sink, "store", "reading image digest", 90);
    let digest = tokio::fs::read_to_string(&iid_path)
        .await
        .map_err(|e| err(500, "store", format!("read iidfile {}: {e}", iid_path.display())))?
        .trim()
        .to_owned();
    if !digest.starts_with("sha256:") {
        return Err(err(500, "store", format!("unexpected iidfile content: {digest:?}")));
    }

    let registry_ref = format!("{}@{}", req.target_image, digest);
    emit(sink, "store", format!("digest: {digest}"), 100);

    info!(%digest, %registry_ref, "static build complete");

    Ok(BuildResult {
        digest,
        registry_ref,
        duration_ms: started.elapsed().as_millis() as u64,
        stack_used: BuildStack::Static,
    })
}

fn emit(sink: &mut dyn ProgressSink, stage: &str, log: impl Into<String>, percent: u32) {
    sink.emit(&ProgressEvent {
        stage: stage.to_owned(),
        log: log.into(),
        percent,
    });
}

async fn run_ok(bin: &str, args: &[&str], cwd: &Path) -> Result<bool, BuildError> {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    // kill_on_drop ensures cancellation (e.g. SIGTERM to the builder, or the
    // surrounding future being dropped) terminates git/buildah instead of
    // leaving them running detached.
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| err(500, "spawn", format!("{bin} spawn: {e}")))?;

    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let tag = bin.to_owned();
    let tag2 = bin.to_owned();
    // Forward everything the child writes to *our* stderr via tracing so the
    // parent (coold) can keep reading a pure NDJSON stream on our stdout.
    let o = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            tracing::info!(target: "subprocess", "{tag}: {l}");
        }
    });
    let e = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            tracing::info!(target: "subprocess", "{tag2}: {l}");
        }
    });
    let status = child
        .wait()
        .await
        .map_err(|e| err(500, "spawn", format!("{bin} wait: {e}")))?;
    let _ = o.await;
    let _ = e.await;
    Ok(status.success())
}

fn err(code: u32, stage: &str, message: impl Into<String>) -> BuildError {
    BuildError {
        code,
        message: message.into(),
        stage: stage.into(),
    }
}

/// Conservative allowlist for OCI image references. Excludes whitespace and
/// control characters that would corrupt the templated `FROM` line.
fn is_safe_base_image(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '/' | '_' | '-' | '@' | '+')
        })
}

/// `output_dir` is interpolated into a `COPY ./<dir>` line and used as a
/// path join inside the cloned repo. Reject newlines, absolute paths, and
/// any `..` segment to keep the build inside its work tree.
fn is_safe_output_dir(s: &str) -> bool {
    if s.is_empty() || s.len() > 256 {
        return false;
    }
    if s.contains('\n') || s.contains('\r') || s.contains('\0') {
        return false;
    }
    let p = std::path::Path::new(s);
    if p.is_absolute() {
        return false;
    }
    p.components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
}

#[cfg(test)]
mod static_build_validation_tests {
    use super::{is_safe_base_image, is_safe_output_dir};

    #[test]
    fn base_image_accepts_typical_refs() {
        assert!(is_safe_base_image("docker.io/library/nginx:alpine"));
        assert!(is_safe_base_image("ghcr.io/org/img@sha256:abc"));
    }

    #[test]
    fn base_image_rejects_injection() {
        assert!(!is_safe_base_image("alpine\nRUN curl evil"));
        assert!(!is_safe_base_image("alpine RUN x"));
        assert!(!is_safe_base_image(""));
    }

    #[test]
    fn output_dir_accepts_typical_dirs() {
        assert!(is_safe_output_dir("dist"));
        assert!(is_safe_output_dir("build/static"));
    }

    #[test]
    fn output_dir_rejects_traversal_and_newline() {
        assert!(!is_safe_output_dir(".."));
        assert!(!is_safe_output_dir("../etc"));
        assert!(!is_safe_output_dir("/etc"));
        assert!(!is_safe_output_dir("dist\nRUN x"));
        assert!(!is_safe_output_dir(""));
    }
}
