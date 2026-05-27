use ipnet::Ipv4Net;
use serde::Serialize;
use std::{collections::BTreeMap, net::Ipv4Addr};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Peer {
    pub public_key: String,
    pub endpoint: String,
    pub allowed_ips: Vec<String>,
    pub latest_handshake: i64,
    pub persistent_keepalive: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NamespaceServerState {
    pub namespace: String,
    pub network_exists: bool,
    pub container_subnet: Option<Ipv4Net>,
    pub dns_enabled: bool,
    pub label: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ServerState {
    pub host: String,
    pub installed: bool,
    pub keys_exist: bool,
    pub public_key: String,
    pub wireguard_mgmt_ip: Option<Ipv4Addr>,
    pub listen_port: u16,
    pub interface: String,
    pub active: bool,
    pub peers: Vec<Peer>,
    pub podman_installed: bool,
    pub podman_socket_active: bool,
    pub namespaces: BTreeMap<String, NamespaceServerState>,
    pub ip_forward_enabled: bool,
    pub firewall_active: bool,
    pub default_deny_active: bool,
    pub firewall_unit_sha256: String,
    pub bridge_table_exists: bool,
    pub nft_available: bool,
    pub corrosion_installed: bool,
    pub corrosion_active: bool,
    pub corrosion_config_hash: String,
    pub corrosion_schema_exists: bool,
    pub corrosion_schema_sha256: String,
    pub coold_installed: bool,
    pub coold_active: bool,
    pub coolify_installed: bool,
    pub coolify_active: bool,
    pub corrosion_version: String,
    pub coold_version: String,
    pub coolify_version: String,
    pub coold_unit_sha256: String,
    pub coolify_unit_sha256: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MeshState {
    pub servers: BTreeMap<String, ServerState>,
}

impl MeshState {
    pub fn assigned_mgmt_ips(&self) -> BTreeMap<String, Ipv4Addr> {
        self.servers
            .iter()
            .filter_map(|(h, s)| s.wireguard_mgmt_ip.map(|ip| (h.clone(), ip)))
            .collect()
    }
    pub fn assigned_container_subnets(&self) -> BTreeMap<String, BTreeMap<String, Ipv4Net>> {
        let mut out = BTreeMap::new();
        for (host, s) in &self.servers {
            for (ns, st) in &s.namespaces {
                if let Some(net) = st.container_subnet {
                    out.entry(ns.clone())
                        .or_insert_with(BTreeMap::new)
                        .insert(host.clone(), net);
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct DesiredMesh {
    /// All WireGuard participants. Includes the central control-plane host
    /// when configured, plus all deployment nodes.
    pub hosts: Vec<String>,
    /// Deployment nodes: hosts that run Podman/coold/Corrosion/firewall.
    pub nodes: Vec<String>,
    pub interface: String,
    pub mgmt_pool: Ipv4Net,
    pub container_pool: Ipv4Net,
    pub container_prefix: u8,
    pub listen_port: u16,
    pub install_podman: bool,
    pub namespaces: Vec<String>,
    pub default_deny_containers: bool,
    pub install_coold: bool,
    pub coold_version: String,
    pub corrosion_version: String,
    pub corrosion_gossip_port: u16,
    pub corrosion_api_port: u16,
    pub central_host: String,
    pub coolify_version: String,
    pub scheduler_version: String,
    pub enable_builder: bool,
    pub builder_hosts: Vec<String>,
    pub builder_capacity: u32,
    pub builder_cpu_quota: String,
    pub builder_memory_max: String,
    pub builder_timeout_secs: u32,
    pub intent: crate::wireguard::intent::Intent,
    pub new_nodes: Vec<String>,
    pub allow_replace: bool,
    pub allow_nightly: bool,
}

impl DesiredMesh {
    pub fn is_node(&self, host: &str) -> bool {
        self.nodes.iter().any(|h| h == host)
    }

    pub fn sorted_namespaces(&self) -> Vec<String> {
        let mut v = self.namespaces.clone();
        v.sort();
        v
    }
    pub fn builder_host_set(&self) -> std::collections::BTreeSet<String> {
        if !self.builder_hosts.is_empty() {
            self.builder_hosts
                .iter()
                .filter(|h| self.nodes.contains(*h))
                .cloned()
                .collect()
        } else if self.enable_builder {
            self.nodes.iter().cloned().collect()
        } else {
            Default::default()
        }
    }
    pub fn has_builder_cap(&self, host: &str) -> bool {
        self.builder_host_set().contains(host)
    }
}
