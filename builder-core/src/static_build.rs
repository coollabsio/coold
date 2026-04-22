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

    emit(sink, "git", format!("cloning {} @ {}", req.repo_url, req.git_ref), 0);

    if !run_ok("git", &["clone", "--depth", "1", "--no-tags", &req.repo_url, "repo"], work_dir).await? {
        return Err(err(500, "git", "git clone failed"));
    }

    let repo_dir = work_dir.join("repo");
    let checkout_ok = run_ok("git", &["checkout", &req.git_ref], &repo_dir).await?;
    if !checkout_ok {
        if !run_ok("git", &["fetch", "--depth", "1", "origin", &req.git_ref], &repo_dir).await? {
            return Err(err(500, "git", format!("git fetch ref {} failed", req.git_ref)));
        }
        run_ok("git", &["checkout", "FETCH_HEAD"], &repo_dir).await?;
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

    if !run_ok(
        "buildah",
        &[
            "bud",
            "--storage-driver",
            "overlay",
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
    let inspect_out = Command::new("buildah")
        .args(["inspect", "--format", "{{.FromImageDigest}}", &req.target_image])
        .output()
        .await
        .map_err(|e| err(500, "store", format!("buildah inspect spawn: {e}")))?;
    if !inspect_out.status.success() {
        return Err(err(500, "store", "buildah inspect failed"));
    }
    let digest = String::from_utf8_lossy(&inspect_out.stdout).trim().to_owned();
    if digest.is_empty() || !digest.starts_with("sha256:") {
        return Err(err(500, "store", format!("unexpected digest format: {digest:?}")));
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

    let mut child = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
