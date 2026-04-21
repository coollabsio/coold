use std::{net::IpAddr, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{anyhow, Result};
use clap::Parser;

/// One managed namespace: a podman bridge network + the bridge gateway IP
/// coold binds DNS on. One entry per namespace per host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceConfig {
    /// DNS-safe label (e.g. "default", "alpha").
    pub name: String,
    /// Podman network name (e.g. "coolify-default-mesh").
    pub network: String,
    /// Bridge gateway IP (e.g. 10.210.0.1). DNS binds here on :53.
    pub gateway_ip: IpAddr,
}

/// Newtype around `Vec<NamespaceConfig>` so clap's `value_parser` can return
/// the whole parsed list from one env var. If the field were `Vec<T>` clap
/// would treat the parser as per-value and collect into `Vec<Vec<T>>`, which
/// panics at parse time with a TypeId downcast mismatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Namespaces(pub Vec<NamespaceConfig>);

impl Namespaces {
    pub fn iter(&self) -> std::slice::Iter<'_, NamespaceConfig> {
        self.0.iter()
    }
    pub fn as_slice(&self) -> &[NamespaceConfig] {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl<'a> IntoIterator for &'a Namespaces {
    type Item = &'a NamespaceConfig;
    type IntoIter = std::slice::Iter<'a, NamespaceConfig>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

#[derive(Debug, Clone, Parser)]
#[command(name = "coold", version, about)]
pub struct Config {
    /// WireGuard management IP for this host (e.g. 100.64.0.5).
    #[arg(long, env = "COOLD_HOST_MGMT_IP")]
    pub host_mgmt_ip: String,

    /// Path to the local Podman Unix socket.
    #[arg(long, env = "COOLD_PODMAN_SOCKET", default_value = "/run/podman/podman.sock")]
    pub podman_socket: PathBuf,

    /// Base URL of the local Corrosion agent's HTTP API.
    #[arg(long, env = "COOLD_CORROSION_URL", default_value = "http://127.0.0.1:8080")]
    pub corrosion_url: String,

    /// Comma-separated list of `<name>:<network>:<gateway-ip>` triples, one
    /// per namespace this host participates in. Example:
    ///
    ///   default:coolify-default-mesh:10.210.0.1,alpha:coolify-alpha-mesh:10.220.0.1
    ///
    /// When unset, coold defaults to a single `default:coolify-default-mesh`
    /// entry with no gateway — DNS is skipped in that mode (agent-only /
    /// test deployments).
    #[arg(
        long,
        env = "COOLD_NAMESPACES",
        value_parser = parse_namespaces,
        default_value = "",
    )]
    pub namespaces: Namespaces,

    /// Periodic full reconcile cadence.
    #[arg(
        long,
        env = "COOLD_RECONCILE_INTERVAL",
        default_value = "2s",
        value_parser = parse_duration,
    )]
    pub reconcile_interval: Duration,

    /// `tracing_subscriber` env filter (e.g. `info`, `coold=debug`).
    #[arg(long, env = "COOLD_LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// DNS zone served authoritatively by coold. Records take the shape
    /// `<container>.<namespace>.<zone>` (e.g. `web.default.coolify.internal`).
    #[arg(long, env = "COOLD_DNS_ZONE", default_value = "coolify.internal")]
    pub dns_zone: String,

    /// Upstream resolver for queries outside `dns_zone`.
    #[arg(long, env = "COOLD_DNS_UPSTREAM", default_value = "1.1.1.1:53")]
    pub dns_upstream: SocketAddr,

    /// Bind address for the firewall REST API (e.g. `100.64.0.5:8443`).
    /// When unset, the API server is disabled. In production set this to
    /// `<host_mgmt_ip>:8443` so the API is reachable only over the wg0
    /// management overlay and never exposed on a public interface.
    #[arg(long, env = "COOLD_API_BIND")]
    pub api_bind: Option<SocketAddr>,

    /// Path to a file containing the API bearer token. When unset, the API
    /// refuses to start (no anonymous access). The file should be root-owned
    /// and mode 0600; contents are trimmed of leading/trailing whitespace.
    #[arg(long, env = "COOLD_API_TOKEN_FILE")]
    pub api_token_file: Option<PathBuf>,

    /// PEM-encoded TLS certificate chain for the API. When both cert and key
    /// are set the API serves HTTPS; otherwise it serves plain HTTP (intended
    /// only for dev/alpha on a trusted overlay).
    #[arg(long, env = "COOLD_TLS_CERT")]
    pub tls_cert: Option<PathBuf>,

    /// PEM-encoded TLS private key for the API.
    #[arg(long, env = "COOLD_TLS_KEY")]
    pub tls_key: Option<PathBuf>,

    /// Path where coold snapshots the COOLIFY-ALLOW chain as an
    /// iptables-restore fragment. `coolify-mesh-allow.service` restores this
    /// on boot via `iptables-restore --noflush`.
    #[arg(long, env = "COOLD_RULES_PATH", default_value = "/etc/coolify/allow.rules")]
    pub rules_path: PathBuf,

    /// Name of the iptables chain coold owns. Must match the chain created
    /// by `coolify init --default-deny` and jumped to from COOLIFY-INTRA.
    #[arg(long, env = "COOLD_CHAIN_NAME", default_value = "COOLIFY-ALLOW")]
    pub chain_name: String,
}

impl Config {
    /// Return the namespace entry with the given name, if any.
    pub fn namespace(&self, name: &str) -> Option<&NamespaceConfig> {
        self.namespaces.iter().find(|ns| ns.name == name)
    }
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    humantime::parse_duration(s).map_err(|e| e.to_string())
}

/// Parse `<name>:<network>:<gateway-ip>,<name>:<network>:<gateway-ip>,...`.
/// Empty input yields a single default-namespace entry without a gateway so
/// tests / agent-only deployments still have a container-network name to
/// iterate. Callers treat `gateway_ip == 0.0.0.0` as "DNS disabled".
fn parse_namespaces(raw: &str) -> Result<Namespaces> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Namespaces(vec![NamespaceConfig {
            name: "default".into(),
            network: "coolify-default-mesh".into(),
            gateway_ip: IpAddr::from([0, 0, 0, 0]),
        }]));
    }
    let mut out = Vec::new();
    for chunk in raw.split(',') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let parts: Vec<&str> = chunk.splitn(3, ':').collect();
        if parts.len() != 3 {
            return Err(anyhow!(
                "COOLD_NAMESPACES entry must be `<name>:<network>:<gateway-ip>`, got {chunk:?}"
            ));
        }
        let name = parts[0].trim().to_string();
        let network = parts[1].trim().to_string();
        let gateway_ip: IpAddr = parts[2]
            .trim()
            .parse()
            .map_err(|e| anyhow!("invalid gateway ip in {chunk:?}: {e}"))?;
        if name.is_empty() || network.is_empty() {
            return Err(anyhow!("empty name or network in COOLD_NAMESPACES entry {chunk:?}"));
        }
        out.push(NamespaceConfig {
            name,
            network,
            gateway_ip,
        });
    }
    if out.is_empty() {
        return Err(anyhow!("COOLD_NAMESPACES parsed to zero entries"));
    }
    Ok(Namespaces(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_triples() {
        let got = parse_namespaces(
            "default:coolify-default-mesh:10.210.0.1,alpha:coolify-alpha-mesh:10.220.0.1",
        )
        .unwrap();
        let items = got.as_slice();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "default");
        assert_eq!(items[0].network, "coolify-default-mesh");
        assert_eq!(items[0].gateway_ip, "10.210.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(items[1].name, "alpha");
    }

    #[test]
    fn empty_yields_default_stub() {
        let got = parse_namespaces("").unwrap();
        let items = got.as_slice();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "default");
        assert_eq!(items[0].network, "coolify-default-mesh");
        assert_eq!(items[0].gateway_ip, "0.0.0.0".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse_namespaces("default:coolify-default-mesh").is_err());
        assert!(parse_namespaces("default::10.0.0.1").is_err());
        assert!(parse_namespaces("default:net:not-an-ip").is_err());
    }
}
