use anyhow::{Result, anyhow, bail};
use ipnet::Ipv4Net;
use serde::Serialize;
use std::net::Ipv4Addr;

use super::{
    config::{PeerConfig, write_config_command},
    firewall::install_firewall_command,
    plan::{ActionType, Plan, PlannedAction, build_plan},
    reconstruct::reconstruct,
    state::{DesiredMesh, MeshState},
    subnet::{allocate_mgmt_ips, allocate_namespaced, machine_ip},
};
use crate::{
    meshnet::podman_network_for,
    services,
    ssh::{Runner, first_line, for_each_server, heredoc},
};

#[derive(Debug, Clone, Serialize)]
pub struct ActionResult {
    pub action: PlannedAction,
    pub status: String,
    pub detail: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct VerifyResult {
    pub host: String,
    pub wireguard_ip: String,
    pub peer_count: usize,
    pub status: String,
}

const APT_WG: &str = "DEBIAN_FRONTEND=noninteractive apt-get update -qq 2>/dev/null && DEBIAN_FRONTEND=noninteractive apt-get install -y -o Dpkg::Options::=\"--force-confold\" wireguard wireguard-tools 2>&1";
const APT_PODMAN: &str = "DEBIAN_FRONTEND=noninteractive apt-get update -qq 2>/dev/null && DEBIAN_FRONTEND=noninteractive apt-get install -y -o Dpkg::Options::=\"--force-confold\" podman nftables 2>&1";
const ENABLE_PODMAN: &str = "systemctl enable --now podman.socket 2>&1";
const ENABLE_IP_FORWARD: &str = "sysctl -w net.ipv4.ip_forward=1 && mkdir -p /etc/sysctl.d && echo 'net.ipv4.ip_forward=1' > /etc/sysctl.d/99-coolify-mesh.conf";

pub async fn apply_mesh<R: Runner>(
    runner: &R,
    user: &str,
    port: u16,
    desired: &DesiredMesh,
    current: &MeshState,
    concurrency: usize,
) -> Result<Vec<ActionResult>> {
    let planned = build_plan(desired, current)?;
    let mut all = vec![];
    let p1 = for_each_server(&desired.hosts, concurrency, |host| {
        let planned = planned.clone();
        async move { phase1(runner, &host, user, port, desired, current, &planned).await }
    })
    .await;
    let mut failed = false;
    for r in p1 {
        if let Some(v) = r.result {
            all.extend(v);
        } else {
            failed = true;
            all.push(err_result(
                &r.host,
                ActionType::InstallWg,
                r.error.unwrap_or_default(),
            ));
        }
    }
    if failed {
        bail!("phase 1 (install/keygen) failed on one or more mesh hosts; aborting");
    }
    let fresh = reconstruct(
        runner,
        &desired.hosts,
        user,
        port,
        &desired.interface,
        &desired.namespaces,
        concurrency,
    )
    .await?;
    let (mgmt, _) = allocate_mgmt_ips(
        desired.mgmt_pool,
        &fresh.assigned_mgmt_ips(),
        &desired.hosts,
    )?;
    let (subnets, _) = allocate_namespaced(
        desired.container_pool,
        desired.container_prefix,
        &fresh.assigned_container_subnets(),
        &desired.namespaces,
        &desired.nodes,
    )?;
    let p2 = for_each_server(&desired.hosts, concurrency, |host| {
        let mgmt = mgmt.clone();
        let subnets = subnets.clone();
        let fresh = fresh.clone();
        let planned = planned.clone();
        async move {
            phase2(
                runner, &host, user, port, desired, &fresh, &mgmt, &subnets, &planned,
            )
            .await
        }
    })
    .await;
    let mut err = None;
    for r in p2 {
        if let Some(v) = r.result {
            all.extend(v);
        } else {
            err = Some(anyhow!("phase 2 failed"));
            all.push(err_result(
                &r.host,
                ActionType::WriteConfig,
                r.error.unwrap_or_default(),
            ));
        }
    }
    if desired.install_coold && err.is_none() {
        let p3 = for_each_server(&desired.nodes, concurrency, |host| {
            let mgmt = mgmt.clone();
            let subnets = subnets.clone();
            let planned = planned.clone();
            async move {
                phase3(
                    runner, &host, user, port, desired, &mgmt, &subnets, &planned,
                )
                .await
            }
        })
        .await;
        for r in p3 {
            if let Some(v) = r.result {
                all.extend(v);
            } else {
                err = Some(anyhow!("phase 3 failed"));
                all.push(err_result(
                    &r.host,
                    ActionType::InstallCoold,
                    r.error.unwrap_or_default(),
                ));
            }
        }
    }
    if !desired.central_host.is_empty() && err.is_none() {
        all.extend(
            phase4(
                runner,
                &desired.central_host,
                user,
                port,
                desired,
                mgmt[&desired.central_host],
                &planned,
            )
            .await?,
        );
        let priv_pem = runner
            .run(
                &desired.central_host,
                user,
                port,
                &format!("cat {}", services::scheduler::SCHEDULER_JWT_PRIV_PATH),
            )
            .await?
            .stdout;
        let scheduler_url = format!(
            "http://{}:{}",
            mgmt[&desired.central_host],
            services::scheduler::SCHEDULER_GRPC_PORT
        );
        let p5 = for_each_server(&desired.nodes, concurrency, |host| {
            let mgmt = mgmt.clone();
            let subnets = subnets.clone();
            let pem = priv_pem.clone();
            let url = scheduler_url.clone();
            let planned = planned.clone();
            async move {
                phase5(
                    runner,
                    &host,
                    user,
                    port,
                    desired,
                    &mgmt,
                    &subnets,
                    pem.as_bytes(),
                    &url,
                    &planned,
                )
                .await
            }
        })
        .await;
        for r in p5 {
            if let Some(v) = r.result {
                all.extend(v);
            } else {
                err = Some(anyhow!("phase 5 failed"));
                all.push(err_result(
                    &r.host,
                    ActionType::WriteHostJwt,
                    r.error.unwrap_or_default(),
                ));
            }
        }
    }
    if let Some(e) = err { Err(e) } else { Ok(all) }
}

#[allow(clippy::too_many_arguments)]
async fn step<R: Runner>(
    runner: &R,
    host: &str,
    user: &str,
    port: u16,
    out: &mut Vec<ActionResult>,
    t: ActionType,
    ns: &str,
    cmd: String,
) -> Result<()> {
    let action = PlannedAction {
        host: host.into(),
        namespace: ns.into(),
        action_type: t,
        detail: String::new(),
    };
    match runner.run(host, user, port, &cmd).await {
        Ok(o) => {
            out.push(ActionResult {
                action,
                status: "ok".into(),
                detail: first_line(&o.stdout).unwrap_or_default(),
            });
            Ok(())
        }
        Err(e) => {
            out.push(ActionResult {
                action,
                status: "error".into(),
                detail: e.to_string(),
            });
            Err(e)
        }
    }
}
fn err_result(host: &str, t: ActionType, e: String) -> ActionResult {
    ActionResult {
        action: PlannedAction {
            host: host.into(),
            namespace: String::new(),
            action_type: t,
            detail: String::new(),
        },
        status: "error".into(),
        detail: e,
    }
}

async fn phase1<R: Runner>(
    runner: &R,
    host: &str,
    user: &str,
    port: u16,
    desired: &DesiredMesh,
    current: &MeshState,
    planned: &Plan,
) -> Result<Vec<ActionResult>> {
    let st = current.servers.get(host).cloned().unwrap_or_default();
    let mut out = vec![];
    if !st.installed && should_run(planned, host, ActionType::InstallWg, "") {
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::InstallWg,
            "",
            APT_WG.into(),
        )
        .await?;
    }
    if !st.keys_exist && should_run(planned, host, ActionType::GenKeyPair, "") {
        step(runner, host, user, port, &mut out, ActionType::GenKeyPair, "", "mkdir -p /etc/wireguard && wg genkey | tee /etc/wireguard/privatekey | wg pubkey | tee /etc/wireguard/publickey && chmod 600 /etc/wireguard/privatekey".into()).await?;
    }
    if desired.is_node(host) && desired.install_podman {
        if !st.podman_installed && should_run(planned, host, ActionType::InstallPodman, "") {
            step(
                runner,
                host,
                user,
                port,
                &mut out,
                ActionType::InstallPodman,
                "",
                APT_PODMAN.into(),
            )
            .await?;
        }
        if !st.podman_socket_active && should_run(planned, host, ActionType::EnablePodmanSocket, "")
        {
            step(
                runner,
                host,
                user,
                port,
                &mut out,
                ActionType::EnablePodmanSocket,
                "",
                ENABLE_PODMAN.into(),
            )
            .await?;
        }
        if !st.ip_forward_enabled && should_run(planned, host, ActionType::EnableIpForward, "") {
            step(
                runner,
                host,
                user,
                port,
                &mut out,
                ActionType::EnableIpForward,
                "",
                ENABLE_IP_FORWARD.into(),
            )
            .await?;
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn phase2<R: Runner>(
    runner: &R,
    host: &str,
    user: &str,
    port: u16,
    desired: &DesiredMesh,
    fresh: &MeshState,
    mgmt: &std::collections::BTreeMap<String, Ipv4Addr>,
    subnets: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, Ipv4Net>>,
    planned: &Plan,
) -> Result<Vec<ActionResult>> {
    let mut out = vec![];
    let peers = desired
        .hosts
        .iter()
        .filter(|p| *p != host)
        .filter_map(|p| {
            fresh
                .servers
                .get(p)
                .filter(|s| !s.public_key.is_empty())
                .map(|s| PeerConfig {
                    endpoint: p.clone(),
                    public_key: s.public_key.clone(),
                    mgmt_ip: mgmt[p],
                    container_subnets: if desired.is_node(p) {
                        desired
                            .sorted_namespaces()
                            .iter()
                            .map(|ns| subnets[ns][p])
                            .collect()
                    } else {
                        vec![]
                    },
                })
        })
        .collect::<Vec<_>>();

    if should_run(planned, host, ActionType::WriteConfig, "") {
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::WriteConfig,
            "",
            write_config_command(&desired.interface, mgmt[host], desired.listen_port, &peers),
        )
        .await?;
    }

    let active = fresh.servers.get(host).map(|s| s.active).unwrap_or(false);
    let svc_action = if active {
        ActionType::ReloadService
    } else {
        ActionType::EnableService
    };
    if should_run(planned, host, svc_action, "") {
        let cmd = if active {
            format!(
                "systemctl restart wg-quick@{} 2>&1 || wg syncconf {} <(wg-quick strip {}) 2>&1",
                desired.interface, desired.interface, desired.interface
            )
        } else {
            format!("systemctl enable --now wg-quick@{} 2>&1", desired.interface)
        };
        step(runner, host, user, port, &mut out, svc_action, "", cmd).await?;
    }

    if desired.is_node(host) && desired.install_podman {
        for ns in desired.sorted_namespaces() {
            let create = should_run(planned, host, ActionType::CreatePodmanNetwork, &ns);
            let recreate = should_run(planned, host, ActionType::RecreatePodmanNetwork, &ns);
            if !create && !recreate {
                continue;
            }
            let net = podman_network_for(&ns);
            let sn = subnets[&ns][host];
            let gw = machine_ip(sn);
            let cmd = if recreate {
                format!(
                    "podman network rm -f {net} 2>&1 && podman network create --driver bridge --disable-dns --label io.coolify.managed=true --label io.coolify.namespace={ns} --subnet={sn} --gateway={gw} {net}"
                )
            } else {
                format!(
                    "podman network exists {net} 2>/dev/null && echo 'network exists, skipping' || podman network create --driver bridge --disable-dns --label io.coolify.managed=true --label io.coolify.namespace={ns} --subnet={sn} --gateway={gw} {net}"
                )
            };
            step(
                runner,
                host,
                user,
                port,
                &mut out,
                if recreate {
                    ActionType::RecreatePodmanNetwork
                } else {
                    ActionType::CreatePodmanNetwork
                },
                &ns,
                cmd,
            )
            .await?;
        }

        if should_run(planned, host, ActionType::InstallFirewall, "") {
            let host_subnets = desired
                .sorted_namespaces()
                .iter()
                .map(|ns| subnets[ns][host])
                .collect::<Vec<_>>();
            step(
                runner,
                host,
                user,
                port,
                &mut out,
                ActionType::InstallFirewall,
                "",
                install_firewall_command(
                    &desired.interface,
                    &desired.sorted_namespaces(),
                    &host_subnets,
                    desired.default_deny_containers,
                ),
            )
            .await?;
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn phase3<R: Runner>(
    runner: &R,
    host: &str,
    user: &str,
    port: u16,
    desired: &DesiredMesh,
    mgmt: &std::collections::BTreeMap<String, Ipv4Addr>,
    subnets: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, Ipv4Net>>,
    planned: &Plan,
) -> Result<Vec<ActionResult>> {
    let mut out = vec![];
    if should_run(planned, host, ActionType::InstallCorrosion, "") {
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::InstallCorrosion,
            "",
            services::corrosion::install_command(&desired.corrosion_version),
        )
        .await?;
    }
    if should_run(planned, host, ActionType::InstallCoold, "") {
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::InstallCoold,
            "",
            services::coold::install_command(&desired.coold_version),
        )
        .await?;
    }
    let peers = desired
        .nodes
        .iter()
        .filter(|h| *h != host)
        .map(|h| mgmt[h])
        .collect::<Vec<_>>();
    if should_run(planned, host, ActionType::WriteCorrosionConfig, "") {
        let cfg = String::from_utf8(services::corrosion::config_bytes(
            mgmt[host],
            desired.corrosion_gossip_port,
            desired.corrosion_api_port,
            &peers,
        ))
        .unwrap();
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::WriteCorrosionConfig,
            "",
            format!(
                "mkdir -p /etc/corrosion && {}",
                heredoc("/etc/corrosion/config.toml", &cfg, "0644")
            ),
        )
        .await?;
    }
    if should_run(planned, host, ActionType::WriteCorrosionSchema, "") {
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::WriteCorrosionSchema,
            "",
            format!(
                "mkdir -p /etc/corrosion/schemas && {}",
                heredoc(
                    "/etc/corrosion/schemas/coolify.sql",
                    services::corrosion::COOLIFY_SCHEMA_SQL,
                    "0644"
                )
            ),
        )
        .await?;
    }
    if should_run(planned, host, ActionType::InstallCorrosionService, "") {
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::InstallCorrosionService,
            "",
            format!(
                "{} && systemctl daemon-reload && systemctl enable --now corrosion",
                heredoc(
                    "/etc/systemd/system/corrosion.service",
                    &services::corrosion::service_unit(&desired.interface),
                    "0644"
                )
            ),
        )
        .await?;
    }
    if should_run(planned, host, ActionType::InstallCooldService, "") {
        let ns = desired
            .sorted_namespaces()
            .iter()
            .map(|n| services::coold::CooldNamespace {
                name: n.clone(),
                network: podman_network_for(n),
                bridge_gateway: machine_ip(subnets[n][host]),
            })
            .collect::<Vec<_>>();
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::InstallCooldService,
            "",
            format!(
                "{} && {} && systemctl daemon-reload && systemctl enable --now coold",
                services::coold::ensure_api_token_command(),
                heredoc(
                    "/etc/systemd/system/coold.service",
                    &services::coold::service_unit(mgmt[host], &ns, None, None),
                    "0644"
                )
            ),
        )
        .await?;
    }
    Ok(out)
}

async fn phase4<R: Runner>(
    runner: &R,
    host: &str,
    user: &str,
    port: u16,
    desired: &DesiredMesh,
    central_ip: Ipv4Addr,
    planned: &Plan,
) -> Result<Vec<ActionResult>> {
    let mut out = vec![];
    if should_run(planned, host, ActionType::InstallCoolify, "") {
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::InstallCoolify,
            "",
            services::coolify::install_command(&desired.coolify_version),
        )
        .await?;
    }
    if should_run(planned, host, ActionType::InstallScheduler, "") {
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::InstallScheduler,
            "",
            services::scheduler::install_command(&desired.scheduler_version),
        )
        .await?;
    }
    if should_run(planned, host, ActionType::GenerateJwtKeypair, "") {
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::GenerateJwtKeypair,
            "",
            services::scheduler::ensure_jwt_keypair_command(),
        )
        .await?;
    }
    if should_run(planned, host, ActionType::InstallSchedulerService, "") {
        let unit = services::scheduler::service_unit(
            &format!("{central_ip}:{}", services::scheduler::SCHEDULER_GRPC_PORT),
            services::scheduler::SCHEDULER_JWT_PUB_PATH,
            &desired.interface,
        );
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::InstallSchedulerService,
            "",
            format!(
                "{} && systemctl daemon-reload && systemctl enable --now scheduler",
                heredoc("/etc/systemd/system/scheduler.service", &unit, "0644")
            ),
        )
        .await?;
    }
    if should_run(planned, host, ActionType::InstallCoolifyService, "") {
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::InstallCoolifyService,
            "",
            format!(
                "{} && systemctl daemon-reload && systemctl enable --now coolify",
                heredoc(
                    "/etc/systemd/system/coolify.service",
                    &services::coolify::service_unit(),
                    "0644"
                )
            ),
        )
        .await?;
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn phase5<R: Runner>(
    runner: &R,
    host: &str,
    user: &str,
    port: u16,
    desired: &DesiredMesh,
    mgmt: &std::collections::BTreeMap<String, Ipv4Addr>,
    subnets: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, Ipv4Net>>,
    priv_pem: &[u8],
    scheduler_url: &str,
    planned: &Plan,
) -> Result<Vec<ActionResult>> {
    let mut out = vec![];
    if should_run(planned, host, ActionType::WriteHostJwt, "") {
        let caps = if desired.has_builder_cap(host) {
            vec!["coold".into(), "builder".into()]
        } else {
            vec!["coold".into()]
        };
        let jwt = services::jwt::mint_host_jwt(priv_pem, host, &caps)?;
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::WriteHostJwt,
            "",
            format!(
                "mkdir -p /etc/coolify && {}",
                heredoc(services::scheduler::HOST_JWT_PATH, &jwt, "0600")
            ),
        )
        .await?;
    }
    if should_run(planned, host, ActionType::InstallBuilder, "") {
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::InstallBuilder,
            "",
            services::builder::install_command(&desired.coold_version),
        )
        .await?;
    }
    if should_run(planned, host, ActionType::UpdateCooldSchedulerEnv, "") {
        let ns = desired
            .sorted_namespaces()
            .iter()
            .map(|n| services::coold::CooldNamespace {
                name: n.clone(),
                network: podman_network_for(n),
                bridge_gateway: machine_ip(subnets[n][host]),
            })
            .collect::<Vec<_>>();
        let builder = if desired.has_builder_cap(host) {
            Some(services::coold::BuilderConfig {
                capacity: desired.builder_capacity,
                cpu_quota: desired.builder_cpu_quota.clone(),
                memory_max: desired.builder_memory_max.clone(),
                timeout_secs: desired.builder_timeout_secs,
                deny_nets: vec![
                    desired.mgmt_pool.to_string(),
                    desired.container_pool.to_string(),
                ],
            })
        } else {
            None
        };
        let sched = services::coold::SchedulerConfig {
            url: scheduler_url.into(),
            jwt_path: services::scheduler::HOST_JWT_PATH.into(),
        };
        step(
            runner,
            host,
            user,
            port,
            &mut out,
            ActionType::UpdateCooldSchedulerEnv,
            "",
            format!(
                "{} && systemctl daemon-reload && systemctl restart coold",
                heredoc(
                    "/etc/systemd/system/coold.service",
                    &services::coold::service_unit(mgmt[host], &ns, Some(&sched), builder.as_ref()),
                    "0644"
                )
            ),
        )
        .await?;
    }
    Ok(out)
}

pub async fn verify<R: Runner>(
    runner: &R,
    hosts: &[String],
    user: &str,
    port: u16,
    iface: &str,
    concurrency: usize,
) -> Vec<VerifyResult> {
    let res = for_each_server(hosts, concurrency, |host| async move {
        let out = runner
            .run(
                &host,
                user,
                port,
                &format!("wg show {iface} dump 2>/dev/null || true"),
            )
            .await?;
        let mut lines = out.stdout.lines();
        let first = lines.next().unwrap_or("");
        let ip = first
            .split('\t')
            .nth(2)
            .unwrap_or("")
            .split('/')
            .next()
            .unwrap_or("")
            .to_string();
        let peers = lines.count();
        Ok(VerifyResult {
            host,
            wireguard_ip: ip,
            peer_count: peers,
            status: "ok".into(),
        })
    })
    .await;
    res.into_iter()
        .map(|r| {
            r.result.unwrap_or(VerifyResult {
                host: r.host,
                wireguard_ip: String::new(),
                peer_count: 0,
                status: "error".into(),
            })
        })
        .collect()
}

fn should_run(plan: &Plan, host: &str, action_type: ActionType, namespace: &str) -> bool {
    plan.actions.iter().any(|a| {
        a.host == host
            && a.action_type == action_type
            && (namespace.is_empty() || a.namespace == namespace)
    })
}

#[allow(dead_code)]
pub async fn plan_only<R: Runner>(
    runner: &R,
    user: &str,
    port: u16,
    desired: &DesiredMesh,
    concurrency: usize,
) -> Result<super::plan::Plan> {
    let current = reconstruct(
        runner,
        &desired.hosts,
        user,
        port,
        &desired.interface,
        &desired.namespaces,
        concurrency,
    )
    .await?;
    build_plan(desired, &current)
}
