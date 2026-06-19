use std::net::Ipv4Addr;

pub const DEFAULT_COOLD_DNS_ZONE: &str = "coolify.internal";
pub const COOLIFY_COOLD_API_PORT: u16 = 8443;
pub const COOLIFY_COOLD_API_TOKEN_PATH: &str = "/etc/coolify/api-token";
pub const MESH_DNS_ANCHOR_SERVICE: &str = "coolify-mesh-dns-anchor.service";
pub const MESH_DNS_RESOLVER_SERVICE: &str = "coolify-mesh-dns-resolver.service";
pub const MESH_DNS_ANCHOR_IMAGE: &str = "docker.io/library/alpine:3.20";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CooldNamespace {
    pub name: String,
    pub network: String,
    pub bridge_gateway: Ipv4Addr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FluxConfig {
    pub url: String,
    pub jwt_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderConfig {
    pub capacity: u32,
    pub cpu_quota: String,
    pub memory_max: String,
    pub timeout_secs: u32,
    pub deny_nets: Vec<String>,
}

pub fn namespaces_env_value(ns: &[CooldNamespace]) -> String {
    ns.iter()
        .map(|n| format!("{}:{}:{}", n.name, n.network, n.bridge_gateway))
        .collect::<Vec<_>>()
        .join(",")
}

pub fn service_unit(
    mgmt_ip: Ipv4Addr,
    namespaces: &[CooldNamespace],
    flux: Option<&FluxConfig>,
    builder: Option<&BuilderConfig>,
) -> String {
    let ns_env = if namespaces.is_empty() {
        String::new()
    } else {
        format!(
            "Environment=COOLIFY_COOLD_NAMESPACES={}\nEnvironment=COOLIFY_COOLD_DNS_ZONE={}\n",
            namespaces_env_value(namespaces),
            DEFAULT_COOLD_DNS_ZONE
        )
    };
    let api_env = format!(
        "Environment=COOLIFY_COOLD_API_BIND={mgmt_ip}:{COOLIFY_COOLD_API_PORT}\nEnvironment=COOLIFY_COOLD_API_TOKEN_FILE={COOLIFY_COOLD_API_TOKEN_PATH}\n"
    );
    let flux_env = flux
        .map(|s| {
            format!(
                "Environment=COOLIFY_COOLD_FLUX_URL={}\nEnvironment=COOLIFY_COOLD_HOST_JWT_PATH={}\n",
                s.url, s.jwt_path
            )
        })
        .unwrap_or_default();
    let (builder_env, builder_pre) = if let Some(b) = builder {
        let cap = if b.capacity == 0 { 2 } else { b.capacity };
        let cpu = if b.cpu_quota.is_empty() {
            "200%"
        } else {
            &b.cpu_quota
        };
        let mem = if b.memory_max.is_empty() {
            "2G"
        } else {
            &b.memory_max
        };
        let timeout = if b.timeout_secs == 0 {
            1800
        } else {
            b.timeout_secs
        };
        (
            format!(
                "Environment=COOLIFY_COOLD_BUILDER_ENABLED=true\nEnvironment=COOLIFY_COOLD_BUILDER_WORK_DIR={}\nEnvironment=COOLIFY_COOLD_BUILDER_CAPACITY={cap}\nEnvironment=COOLIFY_COOLD_BUILDER_CPU_QUOTA={cpu}\nEnvironment=COOLIFY_COOLD_BUILDER_MEMORY_MAX={mem}\nEnvironment=COOLIFY_COOLD_BUILDER_TIMEOUT_SECS={timeout}\nEnvironment=COOLIFY_COOLD_BUILDER_BIN={}\nEnvironment=COOLIFY_COOLD_BUILDER_DENY_NETS={}\n",
                crate::services::builder::BUILDER_WORK_DIR,
                crate::services::builder::BUILDER_BINARY_PATH,
                b.deny_nets.join(",")
            ),
            format!(
                "ExecStartPre=/bin/mkdir -p {}\n",
                crate::services::builder::BUILDER_WORK_DIR
            ),
        )
    } else {
        (String::new(), String::new())
    };
    let mesh_dns_units = if namespaces.is_empty() {
        String::new()
    } else {
        format!(" {MESH_DNS_ANCHOR_SERVICE} {MESH_DNS_RESOLVER_SERVICE}")
    };
    format!(
        "[Unit]\nDescription=Coolify host agent\nWants=corrosion.service{mesh_dns_units}\nAfter=corrosion.service network-online.target podman.socket coolify-mesh-fw.service{mesh_dns_units}\n\n[Service]\nEnvironment=COOLIFY_COOLD_HOST_MGMT_IP={mgmt_ip}\n{ns_env}{api_env}{flux_env}{builder_env}{builder_pre}ExecStart=/usr/local/bin/coold\nAmbientCapabilities=CAP_NET_BIND_SERVICE CAP_NET_ADMIN CAP_NET_RAW\nRestart=on-failure\nRestartSec=2s\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

pub fn mesh_dns_anchor_unit(namespaces: &[CooldNamespace]) -> String {
    let starts = namespaces
        .iter()
        .map(|namespace| {
            let name = mesh_dns_anchor_container_name(&namespace.name);
            format!(
                "podman run -d --replace --name {name} --network {} --label io.coolify.managed=true --label io.coolify.role=mesh-dns-anchor --label io.coolify.namespace={} {MESH_DNS_ANCHOR_IMAGE} sleep infinity >/dev/null",
                namespace.network, namespace.name
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let stops = namespaces
        .iter()
        .map(|namespace| {
            format!(
                "podman rm -f {} >/dev/null 2>&1 || true",
                mesh_dns_anchor_container_name(&namespace.name)
            )
        })
        .collect::<Vec<_>>()
        .join("; ");

    format!(
        "[Unit]\nDescription=Coolify mesh DNS anchor containers\nAfter=network-online.target podman.socket\nRequires=podman.socket\n\n[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/sh -eu -c '{starts}'\nExecStop=/bin/sh -c '{stops}'\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

pub fn mesh_dns_resolver_unit(namespaces: &[CooldNamespace]) -> String {
    let commands = namespaces
        .iter()
        .map(|namespace| {
            format!(
                "iface=$(podman network inspect {} --format '{{{{.NetworkInterface}}}}' 2>/dev/null || true); if [ -n \"$iface\" ]; then resolvectl dns \"$iface\" {} || true; resolvectl domain \"$iface\" '~{}' || true; resolvectl default-route \"$iface\" false || true; fi",
                namespace.network, namespace.bridge_gateway, DEFAULT_COOLD_DNS_ZONE
            )
        })
        .collect::<Vec<_>>()
        .join("; ");

    format!(
        "[Unit]\nDescription=Configure Coolify mesh DNS resolver\nAfter=systemd-resolved.service {MESH_DNS_ANCHOR_SERVICE}\nWants=systemd-resolved.service {MESH_DNS_ANCHOR_SERVICE}\n\n[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/sh -eu -c 'command -v resolvectl >/dev/null 2>&1 || exit 0; {commands}'\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

fn mesh_dns_anchor_container_name(namespace: &str) -> String {
    format!("coolify-mesh-dns-anchor-{namespace}")
}

pub fn install_command(version: &str) -> String {
    format!(
        r#"set -e
DEBIAN_FRONTEND=noninteractive apt-get update -qq 2>/dev/null
DEBIAN_FRONTEND=noninteractive apt-get install -y -o Dpkg::Options::="--force-confold" ca-certificates curl tar 2>&1 >/dev/null
ARCH_RAW=$(uname -m)
case "$ARCH_RAW" in
  x86_64)  ARCH=amd64 ;;
  aarch64) ARCH=arm64 ;;
  *) echo "unsupported arch: $ARCH_RAW" >&2; exit 1 ;;
esac
URL="https://github.com/coollabsio/coold/releases/download/{version}/coold-linux-${{ARCH}}.tar.gz"
DLDIR=$(mktemp -d)
trap 'rm -rf "$DLDIR"' EXIT
curl -fsSL --retry 3 --max-time 120 -o "$DLDIR/coold.tar.gz" "$URL"
tar -xzf "$DLDIR/coold.tar.gz" -C "$DLDIR"
test -f "$DLDIR/coold" || {{ echo "coold binary not found in tarball" >&2; exit 1; }}
install -m 0755 "$DLDIR/coold" /usr/local/bin/coold.tmp
mv /usr/local/bin/coold.tmp /usr/local/bin/coold
echo '{version}' > /usr/local/bin/coold.version"#
    )
}

pub fn ensure_api_token_command() -> String {
    format!(
        "mkdir -p /etc/coolify && if [ ! -s {COOLIFY_COOLD_API_TOKEN_PATH} ]; then openssl rand -hex 32 > {COOLIFY_COOLD_API_TOKEN_PATH}.tmp && chmod 0600 {COOLIFY_COOLD_API_TOKEN_PATH}.tmp && mv {COOLIFY_COOLD_API_TOKEN_PATH}.tmp {COOLIFY_COOLD_API_TOKEN_PATH}; fi"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_namespaces_env() {
        let got = namespaces_env_value(&[CooldNamespace {
            name: "default".into(),
            network: "coolify-default-mesh".into(),
            bridge_gateway: "10.210.0.1".parse().unwrap(),
        }]);
        assert_eq!(got, "default:coolify-default-mesh:10.210.0.1");
    }

    #[test]
    fn install_command_substitutes_version_and_arch() {
        for version in ["nightly", "v1.2.3"] {
            let cmd = install_command(version);
            assert!(cmd.contains(version));
            assert!(cmd.contains(&format!("coollabsio/coold/releases/download/{version}")));
            assert!(cmd.contains("/usr/local/bin/coold.version"));
        }
        let cmd = install_command("nightly");
        for want in [
            "x86_64)  ARCH=amd64",
            "aarch64) ARCH=arm64",
            "coold-linux-${ARCH}.tar.gz",
            "install -m 0755",
        ] {
            assert!(cmd.contains(want), "missing {want} in:\n{cmd}");
        }
    }

    #[test]
    fn service_unit_embeds_mgmt_ip_namespaces_and_api() {
        let got = service_unit(
            "100.64.0.5".parse().unwrap(),
            &[
                CooldNamespace {
                    name: "default".into(),
                    network: "coolify-default-mesh".into(),
                    bridge_gateway: "10.210.7.1".parse().unwrap(),
                },
                CooldNamespace {
                    name: "alpha".into(),
                    network: "coolify-alpha-mesh".into(),
                    bridge_gateway: "10.210.8.1".parse().unwrap(),
                },
            ],
            None,
            None,
        );
        for want in [
            "Environment=COOLIFY_COOLD_HOST_MGMT_IP=100.64.0.5",
            "Environment=COOLIFY_COOLD_NAMESPACES=default:coolify-default-mesh:10.210.7.1,alpha:coolify-alpha-mesh:10.210.8.1",
            "Environment=COOLIFY_COOLD_DNS_ZONE=coolify.internal",
            "Environment=COOLIFY_COOLD_API_BIND=100.64.0.5:8443",
            "Environment=COOLIFY_COOLD_API_TOKEN_FILE=/etc/coolify/api-token",
            "AmbientCapabilities=CAP_NET_BIND_SERVICE CAP_NET_ADMIN CAP_NET_RAW",
            "Wants=corrosion.service coolify-mesh-dns-anchor.service coolify-mesh-dns-resolver.service",
            "After=corrosion.service network-online.target podman.socket coolify-mesh-fw.service coolify-mesh-dns-anchor.service coolify-mesh-dns-resolver.service",
            "ExecStart=/usr/local/bin/coold",
        ] {
            assert!(got.contains(want), "missing {want} in:\n{got}");
        }
    }

    #[test]
    fn mesh_dns_anchor_unit_keeps_network_gateway_present() {
        let got = mesh_dns_anchor_unit(&[CooldNamespace {
            name: "default".into(),
            network: "coolify-default-mesh".into(),
            bridge_gateway: "10.210.0.1".parse().unwrap(),
        }]);

        for want in [
            "Description=Coolify mesh DNS anchor containers",
            "Requires=podman.socket",
            "podman run -d --replace --name coolify-mesh-dns-anchor-default",
            "--network coolify-default-mesh",
            "--label io.coolify.role=mesh-dns-anchor",
            "docker.io/library/alpine:3.20 sleep infinity",
            "podman rm -f coolify-mesh-dns-anchor-default",
        ] {
            assert!(got.contains(want), "missing {want} in:\n{got}");
        }
    }

    #[test]
    fn mesh_dns_resolver_unit_routes_coolify_internal_to_mesh_gateway() {
        let got = mesh_dns_resolver_unit(&[CooldNamespace {
            name: "default".into(),
            network: "coolify-default-mesh".into(),
            bridge_gateway: "10.210.0.1".parse().unwrap(),
        }]);

        for want in [
            "Description=Configure Coolify mesh DNS resolver",
            "After=systemd-resolved.service coolify-mesh-dns-anchor.service",
            "command -v resolvectl >/dev/null 2>&1 || exit 0",
            "podman network inspect coolify-default-mesh --format '{{.NetworkInterface}}'",
            "resolvectl dns \"$iface\" 10.210.0.1 || true",
            "resolvectl domain \"$iface\" '~coolify.internal' || true",
            "resolvectl default-route \"$iface\" false || true",
        ] {
            assert!(got.contains(want), "missing {want} in:\n{got}");
        }
    }

    #[test]
    fn service_unit_omits_namespace_env_when_empty() {
        let got = service_unit("100.64.0.5".parse().unwrap(), &[], None, None);
        assert!(!got.contains("COOLIFY_COOLD_NAMESPACES"));
        assert!(!got.contains("COOLIFY_COOLD_DNS_ZONE"));
        assert!(got.contains("Environment=COOLIFY_COOLD_HOST_MGMT_IP=100.64.0.5"));
    }

    #[test]
    fn service_unit_emits_flux_and_builder_env_with_defaults() {
        let got = service_unit(
            "100.64.0.5".parse().unwrap(),
            &[],
            Some(&FluxConfig {
                url: "http://100.64.0.1:6443".into(),
                jwt_path: "/etc/coolify/host-jwt".into(),
            }),
            Some(&BuilderConfig {
                capacity: 0,
                cpu_quota: String::new(),
                memory_max: String::new(),
                timeout_secs: 0,
                deny_nets: vec!["100.64.0.0/16".into(), "10.210.0.0/16".into()],
            }),
        );
        for want in [
            "Environment=COOLIFY_COOLD_FLUX_URL=http://100.64.0.1:6443",
            "Environment=COOLIFY_COOLD_HOST_JWT_PATH=/etc/coolify/host-jwt",
            "Environment=COOLIFY_COOLD_BUILDER_ENABLED=true",
            "Environment=COOLIFY_COOLD_BUILDER_CAPACITY=2",
            "Environment=COOLIFY_COOLD_BUILDER_CPU_QUOTA=200%",
            "Environment=COOLIFY_COOLD_BUILDER_MEMORY_MAX=2G",
            "Environment=COOLIFY_COOLD_BUILDER_TIMEOUT_SECS=1800",
            "Environment=COOLIFY_COOLD_BUILDER_DENY_NETS=100.64.0.0/16,10.210.0.0/16",
        ] {
            assert!(got.contains(want), "missing {want} in:\n{got}");
        }
    }
}
