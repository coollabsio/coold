use anyhow::Result;
use tempfile::TempDir;
use tracing::{info, warn};

use coolify_proto::builder::v1::{
    build_response, builder_client_msg, BuildError, BuildRequest, BuildResult, BuildResponse,
    BuildStack, BuilderClientMsg,
};

use crate::{config::Config, progress::ProgressEmitter};

pub async fn handle(
    request_id: String,
    req: BuildRequest,
    config: Config,
    tx: tokio::sync::mpsc::Sender<BuilderClientMsg>,
) {
    let progress = ProgressEmitter::new(tx.clone());

    let result: anyhow::Result<BuilderClientMsg> = execute(request_id.clone(), req, &config, &progress).await;

    let response = match result {
        Ok(r) => r,
        Err(e) => {
            warn!(%request_id, error = %e, "build failed");
            BuilderClientMsg {
                payload: Some(builder_client_msg::Payload::Response(BuildResponse {
                    request_id: request_id.clone(),
                    body: Some(build_response::Body::Err(BuildError {
                        code: 500,
                        message: e.to_string(),
                        stage: String::new(),
                    })),
                })),
            }
        }
    };

    if let Err(e) = tx.send(response).await {
        warn!(%request_id, "send BuildResponse failed: {e}");
    }
}

async fn execute(
    request_id: String,
    req: BuildRequest,
    config: &Config,
    progress: &ProgressEmitter,
) -> Result<BuilderClientMsg> {
    let stack = BuildStack::try_from(req.stack).unwrap_or(BuildStack::Unspecified);

    match stack {
        BuildStack::Static => build_static(request_id, req, config, progress).await,
        other => {
            let msg = format!("build stack {:?} not implemented in MVP", other);
            warn!(%msg);
            Ok(BuilderClientMsg {
                payload: Some(builder_client_msg::Payload::Response(BuildResponse {
                    request_id: request_id.clone(),
                    body: Some(build_response::Body::Err(BuildError {
                        code: 501,
                        message: msg,
                        stage: "detect".into(),
                    })),
                })),
            })
        }
    }
}

async fn build_static(
    request_id: String,
    req: BuildRequest,
    config: &Config,
    progress: &ProgressEmitter,
) -> Result<BuilderClientMsg> {
    info!(%request_id, repo_url = %req.repo_url, "starting static build");

    let work_dir = TempDir::new_in(&config.work_dir)?;

    let static_cfg = req.static_cfg.unwrap_or_default();
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

    let start = std::time::Instant::now();

    let out = crate::static_build::run(
        &req.repo_url,
        &req.git_ref,
        &req.target_image,
        &output_dir,
        &base_image,
        work_dir.path(),
        progress,
    )
    .await?;

    let duration_ms = start.elapsed().as_millis() as u64;

    Ok(BuilderClientMsg {
        payload: Some(builder_client_msg::Payload::Response(BuildResponse {
            request_id: request_id.clone(),
            body: Some(build_response::Body::Ok(BuildResult {
                digest: out.digest,
                registry_ref: out.registry_ref,
                duration_ms,
                stack_used: BuildStack::Static as i32,
            })),
        })),
    })
}
