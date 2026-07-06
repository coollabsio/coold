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
    pub corrosion_version: String,
    pub coold_version: String,
    pub coold_unit_sha256: String,
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
    pub listen_port_overrides: BTreeMap<String, u16>,
    pub endpoint_overrides: BTreeMap<String, String>,
    pub install_podman: bool,
    pub namespaces: Vec<String>,
    pub default_deny_containers: bool,
    pub install_coold: bool,
    pub coold_version: String,
    pub corrosion_version: String,
    pub corrosion_gossip_port: u16,
    pub corrosion_api_port: u16,
    /// S1-adjacent: optional pinned SHA-256 for the coold release tarball.
    pub coold_sha256: Option<String>,
    /// S1-adjacent: optional pinned SHA-256 for the corrosion release tarball.
    pub corrosion_sha256: Option<String>,
    /// S5 (opt-in): shared self-signed cert provisioned to every node to run
    /// Corrosion gossip over mutual TLS. `None` = default plaintext gossip.
    pub corrosion_gossip_tls: Option<crate::services::tls::SelfSignedCert>,
    /// S1 (opt-in): self-signed cert for the flux↔coold channel. `None` = keep
    /// default plaintext-over-WireGuard.
    pub flux_tls: Option<crate::services::tls::SelfSignedCert>,
    /// S1 (opt-in): `https://<flux-mesh-ip-or-host>:<port>` gRPC URL wired into
    /// coold's unit when `--enable-flux-tls` is set, so coold dials flux over
    /// pinned TLS. `None` = flux URL not managed by the CLI (coold stays on its
    /// default plaintext path).
    pub flux_tls_url: Option<String>,
    pub intent: crate::wireguard::intent::Intent,
    pub new_nodes: Vec<String>,
    pub allow_replace: bool,
    pub allow_nightly: bool,
}

/// Conventional path coold reads the pinned flux cert from
/// (`COOLIFY_COOLD_FLUX_TLS_PIN_PATH` default). Matches the pin file `apply`
/// drops when `--enable-flux-tls` is set.
pub const FLUX_PIN_PATH: &str = "/etc/coolify/flux.pin";
/// Conventional path to the per-host JWT coold uses to authenticate the
/// outbound flux gRPC stream (`COOLIFY_COOLD_HOST_JWT_PATH` default).
pub const HOST_JWT_PATH: &str = "/etc/coolify/host-jwt";

impl DesiredMesh {
    pub fn is_node(&self, host: &str) -> bool {
        self.nodes.iter().any(|h| h == host)
    }

    /// S1 (opt-in): the flux config to bake into coold's systemd unit. `Some`
    /// only when `--enable-flux-tls` wired an `https://` URL — coold then dials
    /// flux over pinned TLS (`flux.pin`) with the host JWT. Both `plan` (drift
    /// hashing) and `apply` (unit write) MUST use this so the generated unit is
    /// identical; otherwise the coold unit would look permanently drifted.
    pub fn coold_flux_config(&self) -> Option<crate::services::coold::FluxConfig> {
        self.flux_tls_url
            .as_ref()
            .map(|url| crate::services::coold::FluxConfig {
                url: url.clone(),
                jwt_path: HOST_JWT_PATH.to_string(),
                tls_pin_path: Some(FLUX_PIN_PATH.to_string()),
            })
    }

    pub fn sorted_namespaces(&self) -> Vec<String> {
        let mut v = self.namespaces.clone();
        v.sort();
        v
    }

    pub fn listen_port_for(&self, host: &str) -> u16 {
        self.listen_port_overrides
            .get(host)
            .copied()
            .unwrap_or(self.listen_port)
    }

    pub fn endpoint_for(&self, host: &str) -> String {
        self.endpoint_overrides
            .get(host)
            .cloned()
            .unwrap_or_else(|| {
                host.split_once(':')
                    .map(|(h, _)| h)
                    .unwrap_or(host)
                    .to_string()
            })
    }
}
