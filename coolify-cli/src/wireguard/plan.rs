use anyhow::{bail, Result};
use ipnet::Ipv4Net;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

use super::{
    firewall,
    intent::{filter_by_intent, validate_intent},
    state::{DesiredMesh, MeshState},
    subnet::{allocate_mgmt_ips, allocate_namespaced, machine_ip, Warning},
};
use crate::{meshnet::podman_network_for, services};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub enum ActionType {
    InstallWg,
    GenKeyPair,
    AllocateMgmtIp,
    AllocateContainerSubnet,
    WriteConfig,
    EnableService,
    ReloadService,
    AddPeer,
    RemovePeer,
    InstallPodman,
    EnablePodmanSocket,
    EnableIpForward,
    CreatePodmanNetwork,
    RecreatePodmanNetwork,
    InstallFirewall,
    InstallCorrosion,
    InstallCoold,
    WriteCorrosionConfig,
    WriteCorrosionSchema,
    InstallCorrosionService,
    InstallCooldService,
}
impl std::fmt::Display for ActionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            serde_json::to_value(self).unwrap().as_str().unwrap()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedAction {
    pub host: String,
    pub namespace: String,
    #[serde(rename = "action")]
    pub action_type: ActionType,
    pub detail: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedAction {
    pub action: PlannedAction,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub actions: Vec<PlannedAction>,
    pub mgmt_assignments: BTreeMap<String, std::net::Ipv4Addr>,
    pub subnet_assignments: BTreeMap<String, BTreeMap<String, Ipv4Net>>,
    pub warnings: Vec<Warning>,
    pub skipped: Vec<SkippedAction>,
}
impl Plan {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

fn sha256_hex(s: &[u8]) -> String {
    hex::encode(Sha256::digest(s))
}
fn binary_version_drift(want: &str, installed: bool, have: &str) -> bool {
    !installed || have.is_empty() || matches!(want, "nightly" | "latest") || have != want
}

pub fn build_plan(desired: &DesiredMesh, current: &MeshState) -> Result<Plan> {
    if !desired.nodes.is_empty() && desired.default_deny_containers && !desired.install_podman {
        bail!("--default-deny requires --podman");
    }
    if !desired.nodes.is_empty() && desired.install_coold && !desired.install_podman {
        bail!("--install-coold requires --podman");
    }
    if !desired.nodes.is_empty() && desired.install_podman && desired.namespaces.is_empty() {
        bail!("at least one namespace is required");
    }
    validate_intent(desired)?;
    for host in &desired.nodes {
        if let Some(s) = current.servers.get(host) {
            if desired.default_deny_containers && !s.nft_available {
                bail!(
                    "host {host}: nft binary not available; install nftables or pass --skip-default-deny"
                );
            }
        }
    }
    let (mgmt, mut warnings) = allocate_mgmt_ips(
        desired.mgmt_pool,
        &current.assigned_mgmt_ips(),
        &desired.hosts,
    )?;
    let (subnets, mut cont_warnings) = allocate_namespaced(
        desired.container_pool,
        desired.container_prefix,
        &current.assigned_container_subnets(),
        &desired.namespaces,
        &desired.nodes,
    )?;
    warnings.append(&mut cont_warnings);
    let mut plan = Plan {
        actions: vec![],
        mgmt_assignments: mgmt.clone(),
        subnet_assignments: subnets.clone(),
        warnings,
        skipped: vec![],
    };
    let ns_sorted = desired.sorted_namespaces();
    for host in &desired.hosts {
        let state = current.servers.get(host).cloned().unwrap_or_default();
        if !state.installed {
            push(
                &mut plan,
                host,
                "",
                ActionType::InstallWg,
                "wireguard not installed",
            );
        }
        if !state.keys_exist {
            push(
                &mut plan,
                host,
                "",
                ActionType::GenKeyPair,
                "missing /etc/wireguard/privatekey",
            );
        }
        let mgmt_ip = mgmt[host];
        let current_keys: BTreeSet<_> = state.peers.iter().map(|p| p.public_key.clone()).collect();
        let desired_keys: BTreeSet<_> = desired
            .hosts
            .iter()
            .filter(|p| *p != host)
            .filter_map(|p| current.servers.get(p).map(|s| s.public_key.clone()))
            .filter(|k| !k.is_empty())
            .collect();
        let peer_key_pending = desired.hosts.iter().any(|p| {
            p != host
                && current
                    .servers
                    .get(p)
                    .map(|s| s.public_key.is_empty())
                    .unwrap_or(true)
        });
        for key in desired_keys.difference(&current_keys) {
            push(&mut plan, host, "", ActionType::AddPeer, &truncate_key(key));
        }
        for key in current_keys.difference(&desired_keys) {
            push(
                &mut plan,
                host,
                "",
                ActionType::RemovePeer,
                &truncate_key(key),
            );
        }
        let mgmt_mismatch = state.wireguard_mgmt_ip != Some(mgmt_ip);
        let peer_drift = !desired_keys.is_subset(&current_keys);
        let needs_config = mgmt_mismatch
            || peer_drift
            || peer_key_pending
            || !state.keys_exist
            || !state.installed
            || (desired.hosts.len() > 1 && state.listen_port != desired.listen_port_for(host));
        if needs_config {
            push(
                &mut plan,
                host,
                "",
                ActionType::WriteConfig,
                &format!(
                    "{}.conf ({} peer(s))",
                    desired.interface,
                    desired.hosts.len().saturating_sub(1)
                ),
            );
        }
        if !state.active {
            push(
                &mut plan,
                host,
                "",
                ActionType::EnableService,
                &format!("systemctl enable --now wg-quick@{}", desired.interface),
            );
        } else if needs_config {
            push(
                &mut plan,
                host,
                "",
                ActionType::ReloadService,
                &format!(
                    "systemctl reload wg-quick@{} (config changed)",
                    desired.interface
                ),
            );
        }
        if desired.is_node(host) && desired.install_podman {
            if !state.podman_installed {
                push(
                    &mut plan,
                    host,
                    "",
                    ActionType::InstallPodman,
                    "podman not installed",
                );
            }
            if !state.podman_socket_active {
                push(
                    &mut plan,
                    host,
                    "",
                    ActionType::EnablePodmanSocket,
                    "systemctl enable --now podman.socket",
                );
            }
            if !state.ip_forward_enabled {
                push(
                    &mut plan,
                    host,
                    "",
                    ActionType::EnableIpForward,
                    "net.ipv4.ip_forward=1",
                );
            }
            for ns in &ns_sorted {
                let want = subnets[ns][host];
                let nss = state.namespaces.get(ns);
                let net_name = podman_network_for(ns);
                if nss.map(|s| !s.network_exists).unwrap_or(true) {
                    push(
                        &mut plan,
                        host,
                        ns,
                        ActionType::CreatePodmanNetwork,
                        &format!("{net_name} subnet={want} gateway={}", machine_ip(want)),
                    );
                } else if let Some(nss) = nss {
                    if nss.dns_enabled || nss.container_subnet != Some(want) || nss.label != *ns {
                        push(
                            &mut plan,
                            host,
                            ns,
                            ActionType::RecreatePodmanNetwork,
                            &format!("{net_name} — drift"),
                        );
                    }
                }
            }
            let host_subnets = ns_sorted
                .iter()
                .map(|ns| subnets[ns][host])
                .collect::<Vec<_>>();
            let expected = firewall::firewall_service_unit(
                &desired.interface,
                &desired.sorted_namespaces(),
                &host_subnets,
                desired.default_deny_containers,
            );
            if !state.firewall_active
                || state.default_deny_active != desired.default_deny_containers
                || state.firewall_unit_sha256 != sha256_hex(expected.as_bytes())
            {
                push(
                    &mut plan,
                    host,
                    "",
                    ActionType::InstallFirewall,
                    &format!(
                        "coolify-mesh-fw.service ({}, {} namespace(s), default-deny={})",
                        desired.interface,
                        host_subnets.len(),
                        desired.default_deny_containers
                    ),
                );
            }
        }
        if desired.is_node(host) && desired.install_coold {
            let corrosion_drift = binary_version_drift(
                &desired.corrosion_version,
                state.corrosion_installed,
                &state.corrosion_version,
            );
            let coold_drift = binary_version_drift(
                &desired.coold_version,
                state.coold_installed,
                &state.coold_version,
            );
            if corrosion_drift {
                push(
                    &mut plan,
                    host,
                    "",
                    ActionType::InstallCorrosion,
                    &format!(
                        "corrosion {} → /usr/local/bin/corrosion",
                        desired.corrosion_version
                    ),
                );
            }
            if coold_drift {
                push(
                    &mut plan,
                    host,
                    "",
                    ActionType::InstallCoold,
                    &format!("coold {} → /usr/local/bin/coold", desired.coold_version),
                );
            }
            let peers = desired
                .nodes
                .iter()
                .filter(|h| *h != host)
                .filter_map(|h| mgmt.get(h).copied())
                .collect::<Vec<_>>();
            let cfg = services::corrosion::config_bytes(
                mgmt_ip,
                desired.corrosion_gossip_port,
                desired.corrosion_api_port,
                &peers,
                desired.corrosion_gossip_tls.is_some(),
            );
            let cfg_drift = state.corrosion_config_hash != sha256_hex(&cfg);
            if cfg_drift {
                push(
                    &mut plan,
                    host,
                    "",
                    ActionType::WriteCorrosionConfig,
                    &format!("/etc/corrosion/config.toml (peers={})", peers.len()),
                );
            }
            let schema_hash = sha256_hex(services::corrosion::COOLIFY_SCHEMA_SQL.as_bytes());
            let schema_drift = state.corrosion_schema_sha256 != schema_hash;
            if !state.corrosion_schema_exists || schema_drift {
                push(
                    &mut plan,
                    host,
                    "",
                    ActionType::WriteCorrosionSchema,
                    if schema_drift && !state.corrosion_schema_sha256.is_empty() {
                        "/etc/corrosion/schemas/coolify.sql [schema drift — DB will be reset]"
                    } else {
                        "/etc/corrosion/schemas/coolify.sql"
                    },
                );
            }
            let ns_configs = ns_sorted
                .iter()
                .map(|ns| services::coold::CooldNamespace {
                    name: ns.clone(),
                    network: podman_network_for(ns),
                    bridge_gateway: machine_ip(subnets[ns][host]),
                })
                .collect::<Vec<_>>();
            let coold_unit = services::coold::service_unit(
                mgmt_ip,
                &ns_configs,
                desired.coold_flux_config().as_ref(),
            );
            let coold_unit_drift = state.coold_unit_sha256 != sha256_hex(coold_unit.as_bytes());
            if !state.corrosion_active || cfg_drift || corrosion_drift || schema_drift {
                push(
                    &mut plan,
                    host,
                    "",
                    ActionType::InstallCorrosionService,
                    "systemctl enable --now corrosion",
                );
            }
            if !state.coold_active || cfg_drift || coold_drift || coold_unit_drift {
                push(
                    &mut plan,
                    host,
                    "",
                    ActionType::InstallCooldService,
                    &format!(
                        "systemctl enable --now coold (mgmt={mgmt_ip}, namespaces={})",
                        ns_configs.len()
                    ),
                );
            }
        }
    }
    filter_by_intent(&mut plan, desired);
    Ok(plan)
}

fn push(plan: &mut Plan, host: &str, ns: &str, action_type: ActionType, detail: &str) {
    plan.actions.push(PlannedAction {
        host: host.into(),
        namespace: ns.into(),
        action_type,
        detail: detail.into(),
    });
}
fn truncate_key(k: &str) -> String {
    if k.len() <= 8 {
        k.into()
    } else {
        format!("{}...", &k[..8])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wireguard::{intent::Intent, state::DesiredMesh};
    #[test]
    fn upgrade_rejects_nightly() {
        let d = DesiredMesh {
            hosts: vec!["h".into()],
            nodes: vec!["h".into()],
            interface: "wg0".into(),
            mgmt_pool: "100.64.0.0/16".parse().unwrap(),
            container_pool: "10.210.0.0/16".parse().unwrap(),
            container_prefix: 24,
            listen_port: 51820,
            listen_port_overrides: Default::default(),
            endpoint_overrides: Default::default(),
            install_podman: true,
            namespaces: vec!["default".into()],
            default_deny_containers: false,
            install_coold: true,
            coold_version: "nightly".into(),
            corrosion_version: "v1".into(),
            corrosion_gossip_port: 8787,
            corrosion_api_port: 8080,
            coold_sha256: None,
            corrosion_sha256: None,
            corrosion_gossip_tls: None,
            flux_tls: None,
            flux_tls_url: None,
            intent: Intent::Upgrade,
            new_nodes: vec![],
            allow_replace: false,
            allow_nightly: false,
        };
        assert!(build_plan(&d, &MeshState::default()).is_err());
    }

    #[test]
    fn latest_is_treated_as_moving_release_version() {
        assert!(binary_version_drift("latest", true, "latest"));
        assert!(binary_version_drift("nightly", true, "nightly"));
        assert!(!binary_version_drift("v1.2.3", true, "v1.2.3"));
    }
}
