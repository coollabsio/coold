use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use futures_util::future::join_all;
use tokio::sync::mpsc;
use tokio::{fs, process::Command};
use tracing::{debug, info, warn};

use crate::grpc::proto::{
    client_msg, response, server_msg, ApplyCaddyIngressResp, CaddyAppIngressFile, ClientMsg,
    ContainerSummary, ContainersCreateResp, ContainersDeleteResp, ContainersExecResp,
    ContainersHealthcheckRunResp, ContainersInspectResp, ContainersListResp, ContainersLogsResp,
    ContainersRestartResp, ContainersStartResp, ContainersStopResp, Error, ImageSummary,
    ImagesDeleteResp, ImagesListResp, ImagesPullResp, Response, StopCaddyIngressResp,
};
use crate::podman::client::{CreateContainerInput, CreatePortMapping};
use crate::podman::PodmanClient;

pub async fn handle(
    request_id: String,
    command: server_msg::Command,
    podman: &PodmanClient,
    tx: mpsc::Sender<ClientMsg>,
) {
    match command {
        server_msg::Command::ImagesPull(req) => {
            let body = match podman.pull_image(&req.reference).await {
                Ok((digest, output)) => {
                    response::Body::ImagesPull(ImagesPullResp { digest, output })
                }
                Err(e) => error_body(e),
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
        server_msg::Command::ImagesList(_) => {
            let body = match podman.list_images().await {
                Ok(images) => response::Body::ImagesList(ImagesListResp {
                    images: images
                        .into_iter()
                        .map(|image| ImageSummary {
                            id: image.id,
                            repo_tags: image.repo_tags,
                            repo_digests: image.repo_digests,
                            size: image.size,
                            created: image.created,
                        })
                        .collect(),
                }),
                Err(e) => error_body(e),
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
        server_msg::Command::ImagesDelete(req) => {
            let body = match podman.delete_image(&req.reference, req.force).await {
                Ok(output) => response::Body::ImagesDelete(ImagesDeleteResp { output }),
                Err(e) => error_body(e),
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
        server_msg::Command::ContainersCreate(req) => {
            let input = CreateContainerInput {
                name: req.name,
                image: req.image,
                command: req.command,
                env: req.env,
                networks: req.networks,
                volumes: req.volumes,
                ports: req
                    .ports
                    .into_iter()
                    .map(|port| CreatePortMapping {
                        host_ip: port.host_ip,
                        host_port: port.host_port,
                        container_port: port.container_port,
                        protocol: port.protocol,
                    })
                    .collect(),
                dns: req.dns,
                restart_policy: req.restart_policy,
                privileged: req.privileged,
                network_mode: req.network_mode,
                capabilities: req.capabilities,
            };
            let body = match podman.create_container(input).await {
                Ok(id) => response::Body::ContainersCreate(ContainersCreateResp { id }),
                Err(e) => error_body(e),
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
        server_msg::Command::ContainersStart(req) => {
            let body = match podman.start_container(&req.id).await {
                Ok(output) => response::Body::ContainersStart(ContainersStartResp { output }),
                Err(e) => error_body(e),
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
        server_msg::Command::ContainersStop(req) => {
            let body = match podman.stop_container(&req.id, req.timeout_seconds).await {
                Ok(output) => response::Body::ContainersStop(ContainersStopResp { output }),
                Err(e) => error_body(e),
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
        server_msg::Command::ContainersRestart(req) => {
            let body = match podman.restart_container(&req.id, req.timeout_seconds).await {
                Ok(output) => response::Body::ContainersRestart(ContainersRestartResp { output }),
                Err(e) => error_body(e),
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
        server_msg::Command::ContainersDelete(req) => {
            let body = match podman.delete_container(&req.id, req.force).await {
                Ok(output) => response::Body::ContainersDelete(ContainersDeleteResp { output }),
                Err(e) => error_body(e),
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
        server_msg::Command::ContainersInspect(req) => {
            let body = match podman.inspect_container_json(&req.id).await {
                Ok(json) => response::Body::ContainersInspect(ContainersInspectResp { json }),
                Err(e) => error_body(e),
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
        server_msg::Command::ContainersList(_) => {
            let body = match containers_list(podman).await {
                Ok(resp) => response::Body::ContainersList(resp),
                Err(e) => error_body(e),
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
        server_msg::Command::ContainersLogs(req) => {
            let body = match podman
                .container_logs(&req.id, req.tail, req.stdout, req.stderr)
                .await
            {
                Ok(output) => response::Body::ContainersLogs(ContainersLogsResp { output }),
                Err(e) => error_body(e),
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
        server_msg::Command::ContainersExec(req) => {
            let body = match podman.exec_container(&req.id, req.command).await {
                Ok((exit_code, output)) => {
                    response::Body::ContainersExec(ContainersExecResp { exit_code, output })
                }
                Err(e) => error_body(e),
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
        server_msg::Command::ContainersHealthcheckRun(req) => {
            let body = match podman.run_healthcheck(&req.id).await {
                Ok(output) => {
                    response::Body::ContainersHealthcheckRun(ContainersHealthcheckRunResp {
                        output,
                    })
                }
                Err(e) => error_body(e),
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
            info!(%request_id, "applying Caddy ingress");
            let body = match apply_caddy_ingress(req.caddyfile, req.apps, req.mesh_network).await {
                Ok(output) => {
                    info!(%request_id, "Caddy ingress applied");
                    response::Body::ApplyCaddyIngress(ApplyCaddyIngressResp { output })
                }
                Err(e) => {
                    warn!(%request_id, error = %format!("{e:#}"), "Caddy ingress apply failed");
                    error_body(e)
                }
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
            info!(%request_id, "stopping Caddy ingress");
            let body = match stop_caddy_ingress().await {
                Ok(output) => {
                    info!(%request_id, "Caddy ingress stopped");
                    response::Body::StopCaddyIngress(StopCaddyIngressResp { output })
                }
                Err(e) => {
                    warn!(%request_id, error = %format!("{e:#}"), "Caddy ingress stop failed");
                    error_body(e)
                }
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
    }
}

fn error_body(error: anyhow::Error) -> response::Body {
    response::Body::Error(Error {
        code: 500,
        message: format!("{error:#}"),
    })
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
    let started_at = Instant::now();
    info!(
        apps = apps.len(),
        mesh_network,
        caddyfile_bytes = caddyfile.len(),
        "starting Caddy ingress reconciliation"
    );

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

    info!("creating Caddy ingress directories");
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
        info!(file_name, "writing Caddy app config");
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
            info!(file_name, "removing stale Caddy app config");
            fs::remove_file(entry.path())
                .await
                .context("remove stale Caddy app config")?;
        }
    }

    info!("writing temporary Caddyfile");
    fs::write(&temp_caddyfile_path, caddyfile)
        .await
        .context("write temporary Caddyfile")?;

    run_logged_command(
        "pull Caddy image",
        Command::new("podman").args(["pull", "docker.io/library/caddy:2-alpine"]),
    )
    .await
    .context("pull Caddy image")?;

    run_logged_command(
        "validate Caddyfile",
        Command::new("podman").args([
            "run",
            "--rm",
            "-v",
            &format!("{}:/etc/caddy:ro", base_path.display()),
            "docker.io/library/caddy:2-alpine",
            "caddy",
            "validate",
            "--config",
            "/etc/caddy/Caddyfile.tmp",
        ]),
    )
    .await
    .context("validate Caddyfile")?;

    info!("installing Caddyfile");
    fs::rename(&temp_caddyfile_path, &caddyfile_path)
        .await
        .context("install Caddyfile")?;

    let output = start_or_reload_caddy(base_path, &mesh_network).await;
    info!(
        elapsed_ms = started_at.elapsed().as_millis(),
        "finished Caddy ingress reconciliation"
    );

    output
}

async fn start_or_reload_caddy(base_path: &std::path::Path, mesh_network: &str) -> Result<String> {
    let mesh_dns = mesh_network_gateway(mesh_network)
        .await
        .context("get mesh network DNS")?;

    if run_logged_command(
        "check existing Caddy ingress container",
        Command::new("podman").args(["container", "exists", "coolify-v5-caddy"]),
    )
    .await
    .is_ok()
    {
        run_logged_command(
            "recreate Caddy ingress",
            Command::new("podman").args(["rm", "-f", "coolify-v5-caddy"]),
        )
        .await
        .context("recreate Caddy ingress")?;
    }

    let args = start_caddy_args(base_path, mesh_network, &mesh_dns);
    let output = run_logged_command(
        "start Caddy ingress",
        Command::new("podman").args(args.iter().map(String::as_str)),
    )
    .await
    .context("start Caddy ingress")?;

    ensure_caddy_mesh_firewall(mesh_network)
        .await
        .context("allow Caddy ingress to reach mesh containers")?;

    Ok(if output.trim().is_empty() {
        "Caddy ingress applied.".into()
    } else {
        output
    })
}

async fn mesh_network_gateway(mesh_network: &str) -> Result<String> {
    inspect_mesh_network_value(
        mesh_network,
        "{{(index .Subnets 0).Gateway}}",
        "mesh network gateway",
    )
    .await
}

async fn mesh_network_subnet(mesh_network: &str) -> Result<String> {
    inspect_mesh_network_value(
        mesh_network,
        "{{(index .Subnets 0).Subnet}}",
        "mesh network subnet",
    )
    .await
}

async fn inspect_mesh_network_value(
    mesh_network: &str,
    format: &str,
    value_name: &str,
) -> Result<String> {
    let value = run_logged_command(
        value_name,
        Command::new("podman").args(["network", "inspect", mesh_network, "--format", format]),
    )
    .await
    .with_context(|| format!("inspect Caddy {value_name}"))?;

    if value.trim().is_empty() {
        return Err(anyhow!("{value_name} is empty"));
    }

    Ok(value.trim().to_string())
}

async fn container_ip(container: &str, network: &str) -> Result<String> {
    let value = run_logged_command(
        "inspect Caddy ingress container IP",
        Command::new("podman").args([
            "inspect",
            container,
            "--format",
            &format!("{{{{(index .NetworkSettings.Networks \"{network}\").IPAddress}}}}"),
        ]),
    )
    .await
    .context("inspect Caddy ingress container IP")?;

    if value.trim().is_empty() {
        return Err(anyhow!("Caddy ingress container IP is empty"));
    }

    Ok(value.trim().to_string())
}

async fn ensure_caddy_mesh_firewall(mesh_network: &str) -> Result<()> {
    let caddy_ip = container_ip("coolify-v5-caddy", mesh_network).await?;
    let mesh_subnet = mesh_network_subnet(mesh_network).await?;
    let source = format!("{caddy_ip}/32");
    let iptables_args = caddy_iptables_allow_args(&source, &mesh_subnet);

    if run_logged_command(
        "check Caddy ingress iptables allow",
        Command::new("iptables").args(iptables_args.iter().map(String::as_str)),
    )
    .await
    .is_err()
    {
        let insert_args = caddy_iptables_insert_args(&source, &mesh_subnet);
        run_logged_command(
            "allow Caddy ingress through iptables",
            Command::new("iptables").args(insert_args.iter().map(String::as_str)),
        )
        .await
        .context("allow Caddy ingress through iptables")?;
    }

    let nft_rule = caddy_nft_allow_rule(&caddy_ip, &mesh_subnet);
    let existing_rules = run_logged_command(
        "list Caddy ingress bridge firewall rules",
        Command::new("nft").args(["list", "chain", "bridge", "coolify_bridge", "coolify_allow"]),
    )
    .await
    .unwrap_or_default();

    if !existing_rules.contains(&nft_rule) {
        let nft_args = caddy_nft_add_args(&caddy_ip, &mesh_subnet);
        run_logged_command(
            "allow Caddy ingress through bridge firewall",
            Command::new("nft").args(nft_args.iter().map(String::as_str)),
        )
        .await
        .context("allow Caddy ingress through bridge firewall")?;
    }

    Ok(())
}

fn start_caddy_args(
    base_path: &std::path::Path,
    mesh_network: &str,
    mesh_dns: &str,
) -> Vec<String> {
    vec![
        "run".into(),
        "-d".into(),
        "--replace".into(),
        "--name".into(),
        "coolify-v5-caddy".into(),
        "--network".into(),
        mesh_network.into(),
        "--restart".into(),
        "unless-stopped".into(),
        "-p".into(),
        "80:80".into(),
        "--dns".into(),
        mesh_dns.into(),
        "-v".into(),
        format!(
            "{}:/etc/caddy/Caddyfile:ro",
            base_path.join("Caddyfile").display()
        ),
        "-v".into(),
        format!("{}:/etc/caddy/apps:ro", base_path.join("apps").display()),
        "-v".into(),
        format!("{}:/data", base_path.join("data").display()),
        "-v".into(),
        format!("{}:/config", base_path.join("config").display()),
        "docker.io/library/caddy:2-alpine".into(),
    ]
}

fn caddy_iptables_allow_args(source: &str, mesh_subnet: &str) -> Vec<String> {
    vec![
        "-C".into(),
        "COOLIFY-ALLOW".into(),
        "-s".into(),
        source.into(),
        "-d".into(),
        mesh_subnet.into(),
        "-j".into(),
        "ACCEPT".into(),
    ]
}

fn caddy_iptables_insert_args(source: &str, mesh_subnet: &str) -> Vec<String> {
    vec![
        "-I".into(),
        "COOLIFY-ALLOW".into(),
        "1".into(),
        "-s".into(),
        source.into(),
        "-d".into(),
        mesh_subnet.into(),
        "-j".into(),
        "ACCEPT".into(),
    ]
}

fn caddy_nft_allow_rule(caddy_ip: &str, mesh_subnet: &str) -> String {
    format!("ip saddr {caddy_ip} ip daddr {mesh_subnet} accept")
}

fn caddy_nft_add_args(caddy_ip: &str, mesh_subnet: &str) -> Vec<String> {
    vec![
        "add".into(),
        "rule".into(),
        "bridge".into(),
        "coolify_bridge".into(),
        "coolify_allow".into(),
        "meta".into(),
        "protocol".into(),
        "ip".into(),
        "ip".into(),
        "saddr".into(),
        caddy_ip.into(),
        "ip".into(),
        "daddr".into(),
        mesh_subnet.into(),
        "accept".into(),
    ]
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
    let output = run_logged_command(
        "stop Caddy ingress",
        Command::new("podman").args(["rm", "-f", "coolify-v5-caddy"]),
    )
    .await
    .context("stop Caddy ingress")?;

    Ok(if output.trim().is_empty() {
        "Caddy ingress stopped.".into()
    } else {
        output
    })
}

async fn run_logged_command(label: &str, command: &mut Command) -> Result<String> {
    let started_at = Instant::now();
    info!(step = label, "starting command");
    let output = run_command(command).await;

    match &output {
        Ok(_) => info!(
            step = label,
            elapsed_ms = started_at.elapsed().as_millis(),
            "command completed"
        ),
        Err(error) => warn!(
            step = label,
            elapsed_ms = started_at.elapsed().as_millis(),
            %error,
            "command failed"
        ),
    }

    output
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

async fn containers_list(podman: &PodmanClient) -> Result<ContainersListResp> {
    let containers = podman.containers_list().await?;

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

    Ok(ContainersListResp {
        containers: summaries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caddy_start_args_publish_http_only() {
        let args = start_caddy_args(
            std::path::Path::new("/tmp/caddy"),
            "coolify-default-mesh",
            "10.210.0.1",
        );

        assert!(args.windows(2).any(|window| window == ["-p", "80:80"]));
        assert!(!args.windows(2).any(|window| window == ["-p", "443:443"]));
        assert!(!args
            .windows(2)
            .any(|window| window == ["-p", "443:443/udp"]));
    }

    #[test]
    fn caddy_start_args_use_mesh_dns() {
        let args = start_caddy_args(
            std::path::Path::new("/tmp/caddy"),
            "coolify-default-mesh",
            "10.210.0.1",
        );

        assert!(args
            .windows(2)
            .any(|window| window == ["--dns", "10.210.0.1"]));
    }

    #[test]
    fn caddy_firewall_args_allow_ingress_to_mesh_subnet() {
        let iptables_args = caddy_iptables_insert_args("10.210.0.40/32", "10.210.0.0/24");
        let nft_args = caddy_nft_add_args("10.210.0.40", "10.210.0.0/24");

        assert_eq!(
            iptables_args,
            [
                "-I",
                "COOLIFY-ALLOW",
                "1",
                "-s",
                "10.210.0.40/32",
                "-d",
                "10.210.0.0/24",
                "-j",
                "ACCEPT",
            ]
        );
        assert_eq!(
            nft_args,
            [
                "add",
                "rule",
                "bridge",
                "coolify_bridge",
                "coolify_allow",
                "meta",
                "protocol",
                "ip",
                "ip",
                "saddr",
                "10.210.0.40",
                "ip",
                "daddr",
                "10.210.0.0/24",
                "accept",
            ]
        );
    }
}
