use anyhow::{anyhow, bail, Result};
use ipnet::Ipv4Net;
use serde::Serialize;
use std::net::Ipv4Addr;

use super::{
    config::{write_config_command, PeerConfig},
    firewall::install_firewall_command,
    plan::{build_plan, ActionType, Plan, PlannedAction},
    reconstruct::reconstruct,
    state::{DesiredMesh, MeshState},
    subnet::{allocate_mgmt_ips, allocate_namespaced, machine_ip},
};
use crate::{
    meshnet::podman_network_for,
    services,
    ssh::{first_line, for_each_server, heredoc, Runner},
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
    if let Some(e) = err {
        Err(e)
    } else {
        Ok(all)
    }
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
                    endpoint: desired.endpoint_for(p),
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
            write_config_command(
                &desired.interface,
                mgmt[host],
                desired.listen_port_for(host),
                &peers,
            ),
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
                "rm -f /etc/systemd/resolved.conf.d/coolify-internal.conf && {} && {} && {} && systemctl daemon-reload && systemctl enable --now {} {} coold",
                heredoc(
                    "/etc/systemd/system/coolify-mesh-dns-anchor.service",
                    &services::coold::mesh_dns_anchor_unit(&ns),
                    "0644"
                ),
                heredoc(
                    "/etc/systemd/system/coolify-mesh-dns-resolver.service",
                    &services::coold::mesh_dns_resolver_unit(&ns),
                    "0644"
                ),
                heredoc(
                    "/etc/systemd/system/coold.service",
                    &services::coold::service_unit(mgmt[host], &ns, None),
                    "0644"
                ),
                services::coold::MESH_DNS_ANCHOR_SERVICE,
                services::coold::MESH_DNS_RESOLVER_SERVICE
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
