use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures_util::future::join_all;
use tokio::sync::mpsc;
use tokio::{fs, process::Command};
use tracing::{debug, warn};

use crate::builder::BuilderCtx;
use crate::grpc::proto::{
    client_msg, response, server_msg, ApplyCaddyIngressResp, BuildResponseBody,
    CaddyAppIngressFile, ClientMsg, ContainerSummary, Error, ListContainersResp, Response,
    StopCaddyIngressResp,
};
use crate::podman::PodmanClient;

pub async fn handle(
    request_id: String,
    command: server_msg::Command,
    podman: &PodmanClient,
    builder_ctx: Option<Arc<BuilderCtx>>,
    tx: mpsc::Sender<ClientMsg>,
) {
    match command {
        server_msg::Command::ListContainers(_) => {
            let body = match list_containers(podman).await {
                Ok(resp) => response::Body::ListContainers(resp),
                Err(e) => response::Body::Error(Error {
                    code: 500,
                    message: format!("{e:#}"),
                }),
            };
            send_response(
                &tx,
                Response {
                    request_id,
                    body: Some(body),
                },
            )
            .await;
        }
        server_msg::Command::ApplyCaddyIngress(req) => {
            let body = match apply_caddy_ingress(req.caddyfile, req.apps, req.mesh_network).await {
                Ok(output) => response::Body::ApplyCaddyIngress(ApplyCaddyIngressResp { output }),
                Err(e) => response::Body::Error(Error {
                    code: 500,
                    message: format!("{e:#}"),
                }),
            };
            send_response(
                &tx,
                Response {
                    request_id,
                    body: Some(body),
                },
            )
            .await;
        }
        server_msg::Command::StopCaddyIngress(_) => {
            let body = match stop_caddy_ingress().await {
                Ok(output) => response::Body::StopCaddyIngress(StopCaddyIngressResp { output }),
                Err(e) => response::Body::Error(Error {
                    code: 500,
                    message: format!("{e:#}"),
                }),
            };
            send_response(
                &tx,
                Response {
                    request_id,
                    body: Some(body),
                },
            )
            .await;
        }
        server_msg::Command::Build(req) => match builder_ctx {
            Some(ctx) => ctx.dispatch(request_id, req, tx),
            None => {
                let body = response::Body::Build(BuildResponseBody {
                    body: Some(crate::grpc::proto::build_response_body::Body::Err(
                        crate::grpc::proto::BuildError {
                            code: 501,
                            message: "builder capability not enabled on this host".into(),
                            stage: "dispatch".into(),
                        },
                    )),
                });
                send_response(
                    &tx,
                    Response {
                        request_id,
                        body: Some(body),
                    },
                )
                .await;
            }
        },
        server_msg::Command::CancelBuild(_) => match builder_ctx {
            Some(ctx) => {
                if !ctx.cancel(&request_id).await {
                    warn!(%request_id, "cancel for unknown or already-finished request_id");
                }
            }
            None => warn!(%request_id, "received CancelBuild but builder capability disabled"),
        },
    }
}

async fn send_response(tx: &mpsc::Sender<ClientMsg>, response: Response) {
    let request_id = response.request_id.clone();
    let msg = ClientMsg {
        payload: Some(client_msg::Payload::Response(response)),
    };
    if let Err(e) = tx.send(msg).await {
        warn!(%request_id, error = %e, "failed to enqueue response");
    }
}

async fn apply_caddy_ingress(
    caddyfile: String,
    apps: Vec<CaddyAppIngressFile>,
    mesh_network: String,
) -> Result<String> {
    if caddyfile.trim().is_empty() {
        return Err(anyhow!("caddyfile is empty"));
    }

    if caddyfile.len() > 256 * 1024 || apps.iter().any(|app| app.caddyfile.len() > 256 * 1024) {
        return Err(anyhow!("caddyfile is too large"));
    }

    if !is_valid_podman_network_name(&mesh_network) {
        return Err(anyhow!("invalid mesh network name"));
    }

    let base_path = std::path::Path::new("/data/coolify/v5/ingress/caddy");
    let apps_path = base_path.join("apps");
    let caddyfile_path = base_path.join("Caddyfile");
    let temp_caddyfile_path = base_path.join("Caddyfile.tmp");

    fs::create_dir_all(&apps_path)
        .await
        .context("create Caddy app config directory")?;
    fs::create_dir_all(base_path.join("data"))
        .await
        .context("create Caddy data directory")?;
    fs::create_dir_all(base_path.join("config"))
        .await
        .context("create Caddy config directory")?;

    let mut expected_files = std::collections::HashSet::new();
    for app in apps {
        let file_name = caddy_app_file_name(&app.name)?;
        expected_files.insert(file_name.clone());
        fs::write(apps_path.join(file_name), app.caddyfile)
            .await
            .context("write Caddy app config")?;
    }

    let mut entries = fs::read_dir(&apps_path)
        .await
        .context("read Caddy app config directory")?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .context("read Caddy app config")?
    {
        let file_name = entry.file_name().to_string_lossy().to_string();
        if file_name.ends_with(".caddy") && !expected_files.contains(&file_name) {
            fs::remove_file(entry.path())
                .await
                .context("remove stale Caddy app config")?;
        }
    }

    fs::write(&temp_caddyfile_path, caddyfile)
        .await
        .context("write temporary Caddyfile")?;

    run_command(Command::new("podman").args(["pull", "docker.io/library/caddy:2-alpine"]))
        .await
        .context("pull Caddy image")?;

    run_command(Command::new("podman").args([
        "run",
        "--rm",
        "-v",
        &format!("{}:/etc/caddy:ro", base_path.display()),
        "docker.io/library/caddy:2-alpine",
        "caddy",
        "validate",
        "--config",
        "/etc/caddy/Caddyfile.tmp",
    ]))
    .await
    .context("validate Caddyfile")?;

    fs::rename(&temp_caddyfile_path, &caddyfile_path)
        .await
        .context("install Caddyfile")?;

    start_or_reload_caddy(base_path, &mesh_network).await
}

async fn start_or_reload_caddy(base_path: &std::path::Path, mesh_network: &str) -> Result<String> {
    if run_command(Command::new("podman").args(["container", "exists", "coolify-v5-caddy"]))
        .await
        .is_ok()
    {
        return run_command(Command::new("podman").args([
            "exec",
            "coolify-v5-caddy",
            "caddy",
            "reload",
            "--config",
            "/etc/caddy/Caddyfile",
        ]))
        .await
        .map(|output| {
            if output.trim().is_empty() {
                "Caddy ingress applied.".into()
            } else {
                output
            }
        })
        .context("reload Caddy ingress");
    }

    let output = run_command(Command::new("podman").args([
        "run",
        "-d",
        "--replace",
        "--name",
        "coolify-v5-caddy",
        "--network",
        mesh_network,
        "--restart",
        "unless-stopped",
        "-p",
        "80:80",
        "-p",
        "443:443",
        "-p",
        "443:443/udp",
        "-v",
        &format!(
            "{}:/etc/caddy/Caddyfile:ro",
            base_path.join("Caddyfile").display()
        ),
        "-v",
        &format!("{}:/etc/caddy/apps:ro", base_path.join("apps").display()),
        "-v",
        &format!("{}:/data", base_path.join("data").display()),
        "-v",
        &format!("{}:/config", base_path.join("config").display()),
        "docker.io/library/caddy:2-alpine",
    ]))
    .await
    .context("start Caddy ingress")?;

    Ok(if output.trim().is_empty() {
        "Caddy ingress applied.".into()
    } else {
        output
    })
}

fn caddy_app_file_name(value: &str) -> Result<String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(anyhow!("invalid Caddy app file name"));
    }

    Ok(format!("{value}.caddy"))
}

async fn stop_caddy_ingress() -> Result<String> {
    let output = run_command(Command::new("podman").args(["rm", "-f", "coolify-v5-caddy"]))
        .await
        .context("stop Caddy ingress")?;

    Ok(if output.trim().is_empty() {
        "Caddy ingress stopped.".into()
    } else {
        output
    })
}

fn is_valid_podman_network_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

async fn run_command(command: &mut Command) -> Result<String> {
    let output = command.output().await.context("run command")?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let combined = [stdout.as_str(), stderr.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    if !output.status.success() {
        return Err(anyhow!(
            "{}",
            if combined.is_empty() {
                format!("command exited with {}", output.status)
            } else {
                combined
            }
        ));
    }

    Ok(combined)
}

async fn list_containers(podman: &PodmanClient) -> Result<ListContainersResp> {
    let containers = podman.list_containers().await?;

    // libpod list endpoint returns empty NetworkSettings.Networks; must inspect
    // each container to get actual network attachments. Run inspects concurrently.
    let inspects = join_all(containers.iter().map(|c| podman.inspect_container(&c.id))).await;

    let summaries = containers
        .into_iter()
        .zip(inspects)
        .map(|(c, inspect_result)| {
            let name = c
                .names
                .into_iter()
                .next()
                .unwrap_or_default()
                .trim_start_matches('/')
                .to_string();
            let networks = match inspect_result {
                Ok(inspect) => inspect
                    .network_settings
                    .map(|ns| ns.networks.into_keys().collect())
                    .unwrap_or_default(),
                Err(e) => {
                    debug!(container_id = %c.id, error = %e, "inspect failed; reporting empty networks");
                    vec![]
                }
            };
            ContainerSummary {
                id: c.id,
                name,
                image: c.image,
                state: c.state,
                networks,
            }
        })
        .collect();

    Ok(ListContainersResp {
        containers: summaries,
    })
}
