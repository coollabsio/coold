use std::path::Path;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use futures_util::future::join_all;
use tokio::sync::mpsc;
use tokio::{fs, process::Command};
use tracing::{debug, info, warn};

use crate::corrosion::CorrosionClient;
use crate::grpc::proto::{
    client_msg, response, server_msg, ApplyIngressResp, ClientMsg, ContainerSummary,
    ContainersCreateResp, ContainersDeleteResp, ContainersExecResp, ContainersHealthcheckRunResp,
    ContainersInspectResp, ContainersListResp, ContainersLogsResp, ContainersRestartResp,
    ContainersStartResp, ContainersStopResp, CooldLogsResp, CorrosionTablesResp, Error,
    FirewallAllowResp, FirewallListResp, FirewallReconcileResp, FirewallRevokeResp,
    FirewallRule as ProtoFirewallRule, ImageSummary, ImagesDeleteResp, ImagesListResp,
    ImagesPullResp, IngressAppConfig, Response, StopIngressResp,
};
use crate::podman::client::{CreateContainerInput, CreatePortMapping};
use crate::podman::PodmanClient;

pub async fn handle(
    request_id: String,
    command: server_msg::Command,
    podman: &PodmanClient,
    corrosion: &CorrosionClient,
    tx: mpsc::Sender<ClientMsg>,
) {
    match command {
        server_msg::Command::Ping(_) => {
            debug!(%request_id, "ping command reached handler after fast-path; ignoring");
        }
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
                dns_search: req.dns_search,
                network_aliases: req.network_aliases,
                restart_policy: req.restart_policy,
                privileged: req.privileged,
                network_mode: req.network_mode,
                capabilities: req.capabilities,
            };
            let body = match apply_mesh_dns_defaults(input).await {
                Ok(input) => match podman.create_container(input).await {
                    Ok(id) => response::Body::ContainersCreate(ContainersCreateResp { id }),
                    Err(e) => error_body(e),
                },
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
        server_msg::Command::CooldLogs(req) => {
            let body = match coold_logs(req.tail).await {
                Ok(output) => response::Body::CooldLogs(CooldLogsResp { output }),
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
        server_msg::Command::CorrosionTables(req) => {
            let body = match corrosion.tables_json(req.limit).await {
                Ok(output) => response::Body::CorrosionTables(CorrosionTablesResp { output }),
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
        server_msg::Command::IngressApply(req) => {
            info!(%request_id, kind = %req.kind, "applying ingress");
            let body = match apply_ingress(req.kind, req.config, req.apps, req.mesh_network).await {
                Ok(output) => {
                    info!(%request_id, "ingress applied");
                    response::Body::IngressApply(ApplyIngressResp { output })
                }
                Err(e) => {
                    warn!(%request_id, error = %format!("{e:#}"), "ingress apply failed");
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
        server_msg::Command::IngressStop(req) => {
            info!(%request_id, kind = %req.kind, "stopping ingress");
            let body = match stop_ingress(req.kind).await {
                Ok(output) => {
                    info!(%request_id, "ingress stopped");
                    response::Body::IngressStop(StopIngressResp { output })
                }
                Err(e) => {
                    warn!(%request_id, error = %format!("{e:#}"), "ingress stop failed");
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
        server_msg::Command::FirewallAllow(req) => {
            let body = match req.rule {
                Some(rule) => match firewall_allow(FirewallRule::from(rule), corrosion).await {
                    Ok((id, output)) => {
                        response::Body::FirewallAllow(FirewallAllowResp { id, output })
                    }
                    Err(e) => error_body(e),
                },
                None => error_body(anyhow!("missing firewall rule")),
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
        server_msg::Command::FirewallRevoke(req) => {
            let body = match firewall_revoke(&req.id).await {
                Ok(output) => response::Body::FirewallRevoke(FirewallRevokeResp { output }),
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
        server_msg::Command::FirewallList(req) => {
            let body = match firewall_list(&req.namespace).await {
                Ok(rules) => response::Body::FirewallList(FirewallListResp {
                    rules: rules.into_iter().map(ProtoFirewallRule::from).collect(),
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
        server_msg::Command::FirewallReconcile(_) => {
            let body = match firewall_reconcile().await {
                Ok(output) => response::Body::FirewallReconcile(FirewallReconcileResp { output }),
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

async fn apply_mesh_dns_defaults(mut input: CreateContainerInput) -> Result<CreateContainerInput> {
    let Some(network) = input
        .networks
        .iter()
        .find(|network| network.starts_with("coolify-") && network.ends_with("-mesh"))
        .cloned()
    else {
        return Ok(input);
    };

    if input.dns.is_empty() {
        input.dns = vec![mesh_network_gateway(&network).await?];
    }

    if input.dns_search.is_empty() {
        input.dns_search = vec![mesh_dns_search_domain(&network)?];
    }

    Ok(input)
}

fn mesh_dns_search_domain(mesh_network: &str) -> Result<String> {
    let namespace = mesh_network
        .strip_prefix("coolify-")
        .and_then(|value| value.strip_suffix("-mesh"))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("invalid mesh network name"))?;

    Ok(format!("{namespace}.coolify.internal"))
}

async fn apply_ingress(
    kind: String,
    config: String,
    apps: Vec<IngressAppConfig>,
    mesh_network: String,
) -> Result<String> {
    match kind.as_str() {
        "caddy" => apply_caddy_ingress(config, apps, mesh_network).await,
        unsupported => Err(anyhow!("unsupported ingress kind: {unsupported}")),
    }
}

async fn apply_caddy_ingress(
    caddyfile: String,
    apps: Vec<IngressAppConfig>,
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

    if caddyfile.len() > 256 * 1024 || apps.iter().any(|app| app.config.len() > 256 * 1024) {
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
        fs::write(apps_path.join(file_name), app.config)
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

const FIREWALL_STATE_PATH: &str = "/etc/coolify/firewall-rules.tsv";
const FIREWALL_ALLOW_RULES_PATH: &str = "/etc/coolify/allow.rules";
const FIREWALL_BRIDGE_ALLOW_RULES_PATH: &str = "/etc/coolify/allow.nft";

#[derive(Debug, Clone, PartialEq, Eq)]
struct FirewallRule {
    id: String,
    namespace: String,
    src: String,
    dst: String,
    proto: String,
    port: u32,
}

impl From<ProtoFirewallRule> for FirewallRule {
    fn from(rule: ProtoFirewallRule) -> Self {
        Self {
            id: rule.id,
            namespace: rule.namespace,
            src: rule.src,
            dst: rule.dst,
            proto: if rule.proto.is_empty() {
                "tcp".into()
            } else {
                rule.proto
            },
            port: rule.port,
        }
    }
}

impl From<FirewallRule> for ProtoFirewallRule {
    fn from(rule: FirewallRule) -> Self {
        Self {
            id: rule.id,
            namespace: rule.namespace,
            src: rule.src,
            dst: rule.dst,
            proto: rule.proto,
            port: rule.port,
        }
    }
}

async fn firewall_allow(
    rule: FirewallRule,
    corrosion: &CorrosionClient,
) -> Result<(String, String)> {
    validate_firewall_rule(&rule)?;
    let normalized = resolve_firewall_rule(rule, corrosion).await?;
    let id = normalized.id.clone();
    let mut rules = load_firewall_rules().await?;
    rules.retain(|existing| existing.id != id);
    rules.push(normalized.clone());
    save_firewall_rules(&rules).await?;
    apply_firewall_rule(&normalized).await?;

    Ok((id, "Firewall rule applied.".into()))
}

async fn firewall_revoke(id: &str) -> Result<String> {
    let mut rules = load_firewall_rules().await?;
    let before = rules.len();
    rules.retain(|rule| rule.id != id);

    if rules.len() == before {
        return Err(anyhow!("firewall rule not found: {id}"));
    }

    save_firewall_rules(&rules).await?;
    firewall_reconcile().await?;

    Ok("Firewall rule removed.".into())
}

async fn firewall_list(namespace: &str) -> Result<Vec<FirewallRule>> {
    let mut rules = load_firewall_rules().await?;
    if !namespace.is_empty() {
        rules.retain(|rule| rule.namespace == namespace);
    }

    Ok(rules)
}

async fn firewall_reconcile() -> Result<String> {
    let rules = load_firewall_rules().await?;
    save_firewall_rules(&rules).await?;
    run_logged_command(
        "flush Coolify allow iptables chain",
        Command::new("iptables").args(["-F", "COOLIFY-ALLOW"]),
    )
    .await
    .context("flush Coolify allow iptables chain")?;
    run_logged_command(
        "restore Coolify allow iptables snapshot",
        Command::new("sh").args([
            "-c",
            &format!("iptables-restore --noflush < {FIREWALL_ALLOW_RULES_PATH}"),
        ]),
    )
    .await
    .context("restore Coolify allow iptables snapshot")?;
    run_logged_command(
        "flush Coolify bridge allow chain",
        Command::new("nft").args([
            "flush",
            "chain",
            "bridge",
            "coolify_bridge",
            "coolify_allow",
        ]),
    )
    .await
    .context("flush Coolify bridge allow chain")?;
    run_logged_command(
        "restore Coolify bridge allow snapshot",
        Command::new("nft").args(["-f", FIREWALL_BRIDGE_ALLOW_RULES_PATH]),
    )
    .await
    .context("restore Coolify bridge allow snapshot")?;

    Ok("Firewall rules reconciled.".into())
}

async fn apply_firewall_rule(rule: &FirewallRule) -> Result<()> {
    let iptables_args = firewall_iptables_insert_args(rule);
    run_logged_command(
        "apply Coolify iptables allow rule",
        Command::new("iptables").args(iptables_args.iter().map(String::as_str)),
    )
    .await
    .context("apply Coolify iptables allow rule")?;

    let nft_args = firewall_nft_add_args(rule);
    run_logged_command(
        "apply Coolify bridge allow rule",
        Command::new("nft").args(nft_args.iter().map(String::as_str)),
    )
    .await
    .context("apply Coolify bridge allow rule")?;

    Ok(())
}

async fn load_firewall_rules() -> Result<Vec<FirewallRule>> {
    if !Path::new(FIREWALL_STATE_PATH).exists() {
        return Ok(vec![]);
    }

    let content = fs::read_to_string(FIREWALL_STATE_PATH)
        .await
        .context("read Coolify firewall rules")?;

    Ok(content
        .lines()
        .filter_map(parse_firewall_rule_line)
        .collect())
}

async fn save_firewall_rules(rules: &[FirewallRule]) -> Result<()> {
    fs::create_dir_all("/etc/coolify")
        .await
        .context("create Coolify config directory")?;
    fs::write(FIREWALL_STATE_PATH, render_firewall_state(rules))
        .await
        .context("write Coolify firewall state")?;
    fs::write(FIREWALL_ALLOW_RULES_PATH, render_iptables_snapshot(rules))
        .await
        .context("write Coolify iptables allow snapshot")?;
    fs::write(FIREWALL_BRIDGE_ALLOW_RULES_PATH, render_nft_snapshot(rules))
        .await
        .context("write Coolify bridge allow snapshot")?;

    Ok(())
}

fn firewall_iptables_insert_args(rule: &FirewallRule) -> Vec<String> {
    vec![
        "-I".into(),
        "COOLIFY-ALLOW".into(),
        "1".into(),
        "-s".into(),
        cidr(&rule.src),
        "-d".into(),
        cidr(&rule.dst),
        "-p".into(),
        rule.proto.clone(),
        "--dport".into(),
        rule.port.to_string(),
        "-m".into(),
        "comment".into(),
        "--comment".into(),
        format!("coolify-fw:{}", rule.id),
        "-j".into(),
        "ACCEPT".into(),
    ]
}

fn firewall_nft_add_args(rule: &FirewallRule) -> Vec<String> {
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
        rule.src.clone(),
        "ip".into(),
        "daddr".into(),
        rule.dst.clone(),
        rule.proto.clone(),
        "dport".into(),
        rule.port.to_string(),
        "accept".into(),
    ]
}

fn render_firewall_state(rules: &[FirewallRule]) -> String {
    rules
        .iter()
        .map(|rule| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                rule.id, rule.namespace, rule.src, rule.dst, rule.proto, rule.port
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn render_iptables_snapshot(rules: &[FirewallRule]) -> String {
    let mut lines = vec!["*filter".to_string()];
    for rule in rules {
        lines.push(format!(
            "-A COOLIFY-ALLOW -s {} -d {} -p {} --dport {} -m comment --comment coolify-fw:{} -j ACCEPT",
            cidr(&rule.src),
            cidr(&rule.dst),
            rule.proto,
            rule.port,
            rule.id
        ));
    }
    lines.push("COMMIT".into());
    lines.join("\n") + "\n"
}

fn render_nft_snapshot(rules: &[FirewallRule]) -> String {
    rules
        .iter()
        .map(|rule| {
            format!(
                "add rule bridge coolify_bridge coolify_allow meta protocol ip ip saddr {} ip daddr {} {} dport {} accept",
                rule.src, rule.dst, rule.proto, rule.port
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn parse_firewall_rule_line(line: &str) -> Option<FirewallRule> {
    let parts = line.split('\t').collect::<Vec<_>>();
    if parts.len() == 6 {
        return Some(FirewallRule {
            id: parts[0].to_string(),
            namespace: parts[1].to_string(),
            src: parts[2].to_string(),
            dst: parts[3].to_string(),
            proto: parts[4].to_string(),
            port: parts[5].parse().ok()?,
        });
    }

    if parts.len() != 5 {
        return None;
    }

    Some(FirewallRule {
        id: String::new(),
        namespace: parts[0].to_string(),
        src: parts[1].to_string(),
        dst: parts[2].to_string(),
        proto: parts[3].to_string(),
        port: parts[4].parse().ok()?,
    })
}

fn validate_firewall_rule(rule: &FirewallRule) -> Result<()> {
    if rule.id.is_empty() {
        return Err(anyhow!("firewall rule id is required"));
    }
    if rule.namespace.is_empty() || rule.src.is_empty() || rule.dst.is_empty() {
        return Err(anyhow!(
            "firewall rule namespace, src, and dst are required"
        ));
    }
    if !matches!(rule.proto.as_str(), "tcp" | "udp") {
        return Err(anyhow!("unsupported firewall protocol: {}", rule.proto));
    }
    if rule.port == 0 || rule.port > 65535 {
        return Err(anyhow!("firewall port must be between 1 and 65535"));
    }

    Ok(())
}

fn cidr(value: &str) -> String {
    if value.contains('/') {
        value.to_string()
    } else {
        format!("{value}/32")
    }
}

async fn resolve_firewall_rule(
    rule: FirewallRule,
    corrosion: &CorrosionClient,
) -> Result<FirewallRule> {
    let network = format!("coolify-{}-mesh", rule.namespace);

    Ok(FirewallRule {
        src: resolve_firewall_endpoint(&rule.src, &rule.namespace, &network, corrosion).await?,
        dst: resolve_firewall_endpoint(&rule.dst, &rule.namespace, &network, corrosion).await?,
        ..rule
    })
}

async fn resolve_firewall_endpoint(
    value: &str,
    namespace: &str,
    network: &str,
    corrosion: &CorrosionClient,
) -> Result<String> {
    if is_ip_or_cidr(value) {
        return Ok(value.trim_end_matches("/32").to_string());
    }

    let ips = corrosion
        .query_ips_by_name(value, namespace)
        .await
        .with_context(|| format!("query Corrosion endpoint {value} in namespace {namespace}"))?;

    if let Some(ip) = firewall_endpoint_ip(value, namespace, &ips)? {
        return Ok(ip);
    }

    container_ip(value, network)
        .await
        .with_context(|| format!("resolve firewall endpoint {value} on {network}"))
}

fn firewall_endpoint_ip(value: &str, namespace: &str, ips: &[String]) -> Result<Option<String>> {
    match ips {
        [] => Ok(None),
        [ip] => Ok(Some(ip.clone())),
        _ => Err(anyhow!(
            "firewall endpoint {value} in namespace {namespace} resolved to multiple IPs"
        )),
    }
}

fn is_ip_or_cidr(value: &str) -> bool {
    value
        .split('/')
        .next()
        .is_some_and(|address| address.parse::<std::net::Ipv4Addr>().is_ok())
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

async fn stop_ingress(kind: String) -> Result<String> {
    match kind.as_str() {
        "caddy" => stop_caddy_ingress().await,
        unsupported => Err(anyhow!("unsupported ingress kind: {unsupported}")),
    }
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

async fn coold_logs(tail: u32) -> Result<String> {
    let tail = tail.clamp(1, 1000).to_string();
    let output = Command::new("journalctl")
        .args([
            "--unit",
            "coold",
            "--no-pager",
            "--output",
            "short-iso",
            "--lines",
            &tail,
        ])
        .output()
        .await
        .context("read coold journal logs")?;

    let combined = String::from_utf8_lossy(&output.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&output.stderr).lines())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    if !output.status.success() {
        return Err(anyhow!(
            "{}",
            if combined.is_empty() {
                format!("journalctl exited with {}", output.status)
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
    fn derives_mesh_dns_search_domain_from_network_name() {
        assert_eq!(
            mesh_dns_search_domain("coolify-default-mesh").unwrap(),
            "default.coolify.internal"
        );
        assert!(mesh_dns_search_domain("host").is_err());
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

    #[test]
    fn firewall_endpoint_ip_uses_single_corrosion_result() {
        assert_eq!(
            firewall_endpoint_ip("remote-api", "default", &["10.210.1.12".into()]).unwrap(),
            Some("10.210.1.12".into())
        );
    }

    #[test]
    fn firewall_allow_args_scope_source_destination_and_port() {
        let rule = FirewallRule {
            id: "rule-api-postgres".into(),
            namespace: "default".into(),
            src: "10.210.0.2".into(),
            dst: "10.210.0.3".into(),
            proto: "tcp".into(),
            port: 5432,
        };

        assert_eq!(
            firewall_iptables_insert_args(&rule),
            [
                "-I",
                "COOLIFY-ALLOW",
                "1",
                "-s",
                "10.210.0.2/32",
                "-d",
                "10.210.0.3/32",
                "-p",
                "tcp",
                "--dport",
                "5432",
                "-m",
                "comment",
                "--comment",
                "coolify-fw:rule-api-postgres",
                "-j",
                "ACCEPT",
            ]
        );
    }
}
