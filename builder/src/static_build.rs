use std::path::Path;

use anyhow::{bail, Result};
use tokio::process::Command;
use tracing::info;

use crate::progress::ProgressEmitter;

pub struct StaticBuildOutput {
    pub digest: String,
    pub registry_ref: String,
}

pub async fn run(
    repo_url: &str,
    git_ref: &str,
    target_image: &str,
    output_dir: &str,
    base_image: &str,
    work_dir: &Path,
    progress: &ProgressEmitter,
) -> Result<StaticBuildOutput> {
    // ── git clone ──────────────────────────────────────────────────────────
    progress.emit("git", format!("cloning {repo_url} @ {git_ref}"), 0).await;

    let status = Command::new("git")
        .args(["clone", "--depth", "1", "--no-tags", repo_url, "repo"])
        .current_dir(work_dir)
        .status()
        .await?;
    if !status.success() {
        bail!("git clone failed");
    }

    // Checkout specific ref (depth-1 clone fetches HEAD; fetch the sha if needed)
    let repo_dir = work_dir.join("repo");
    let checkout = Command::new("git")
        .args(["checkout", git_ref])
        .current_dir(&repo_dir)
        .status()
        .await?;
    if !checkout.success() {
        // Shallow clone may not have the ref; fetch it
        let fetch = Command::new("git")
            .args(["fetch", "--depth", "1", "origin", git_ref])
            .current_dir(&repo_dir)
            .status()
            .await?;
        if !fetch.success() {
            bail!("git fetch ref {git_ref} failed");
        }
        Command::new("git")
            .args(["checkout", "FETCH_HEAD"])
            .current_dir(&repo_dir)
            .status()
            .await?;
    }

    progress.emit("git", "clone complete", 10).await;

    // ── detect output_dir ─────────────────────────────────────────────────
    progress.emit("detect", format!("checking output_dir: {output_dir}"), 15).await;
    let out_path = repo_dir.join(output_dir);
    if !out_path.exists() {
        bail!("output_dir '{output_dir}' not found in repo");
    }

    // ── generate Containerfile ────────────────────────────────────────────
    let containerfile = format!(
        "FROM {base_image}\nCOPY ./{output_dir} /usr/share/nginx/html\n"
    );
    tokio::fs::write(repo_dir.join("Containerfile.coolify"), &containerfile).await?;

    // ── buildah bud ───────────────────────────────────────────────────────
    progress.emit("build", format!("buildah bud → {target_image}"), 20).await;
    info!(target_image, "starting buildah bud");

    let build_status = Command::new("buildah")
        .args([
            "bud",
            "--storage-driver", "overlay",
            "-t", target_image,
            "-f", "Containerfile.coolify",
            ".",
        ])
        .current_dir(&repo_dir)
        .status()
        .await?;

    if !build_status.success() {
        bail!("buildah bud failed for {target_image}");
    }

    progress.emit("build", "build complete", 80).await;

    // ── extract digest ────────────────────────────────────────────────────
    progress.emit("store", "reading image digest", 90).await;

    let inspect_out = Command::new("buildah")
        .args(["inspect", "--format", "{{.FromImageDigest}}", target_image])
        .output()
        .await?;

    if !inspect_out.status.success() {
        bail!("buildah inspect failed");
    }

    let digest = String::from_utf8(inspect_out.stdout)?.trim().to_owned();
    if digest.is_empty() || !digest.starts_with("sha256:") {
        // Fallback: use image id as digest placeholder
        let id_out = Command::new("buildah")
            .args(["inspect", "--format", "{{.FromImageID}}", target_image])
            .output()
            .await?;
        let id = String::from_utf8(id_out.stdout)?.trim().to_owned();
        bail!("unexpected digest format: '{digest}', image id: '{id}'");
    }

    let registry_ref = format!("{target_image}@{digest}");
    progress.emit("store", format!("digest: {digest}"), 100).await;

    info!(%digest, %registry_ref, "static build complete");
    Ok(StaticBuildOutput { digest, registry_ref })
}
