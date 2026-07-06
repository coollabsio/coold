use std::net::Ipv4Addr;

use super::checksum_verify_snippet;

pub const DEFAULT_COOLD_DNS_ZONE: &str = "coolify.internal";
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
    /// S1 (opt-in flux TLS): path coold reads the pinned flux cert from
    /// (`COOLIFY_COOLD_FLUX_TLS_PIN_PATH`). `Some` when `--enable-flux-tls`
    /// wired a `https://` URL so coold dials the flux gRPC channel over pinned
    /// TLS; `None` keeps coold's default (plaintext-over-WireGuard) path.
    pub tls_pin_path: Option<String>,
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
    let flux_env = flux
        .map(|s| {
            let pin_env = s
                .tls_pin_path
                .as_ref()
                .map(|p| format!("Environment=COOLIFY_COOLD_FLUX_TLS_PIN_PATH={p}\n"))
                .unwrap_or_default();
            format!(
                "Environment=COOLIFY_COOLD_FLUX_URL={}\nEnvironment=COOLIFY_COOLD_HOST_JWT_PATH={}\n{pin_env}",
                s.url, s.jwt_path
            )
        })
        .unwrap_or_default();
    let mesh_dns_units = if namespaces.is_empty() {
        String::new()
    } else {
        format!(" {MESH_DNS_ANCHOR_SERVICE} {MESH_DNS_RESOLVER_SERVICE}")
    };
    format!(
        "[Unit]\nDescription=Coolify host agent\nWants=corrosion.service{mesh_dns_units}\nAfter=corrosion.service network-online.target podman.socket coolify-mesh-fw.service{mesh_dns_units}\n\n[Service]\nEnvironment=COOLIFY_COOLD_HOST_MGMT_IP={mgmt_ip}\n{ns_env}{flux_env}ExecStart=/usr/local/bin/coold\nAmbientCapabilities=CAP_NET_BIND_SERVICE CAP_NET_ADMIN CAP_NET_RAW\n{}Restart=on-failure\nRestartSec=2s\n\n[Install]\nWantedBy=multi-user.target\n",
        hardening_directives()
    )
}

/// S7 (defense-in-depth for the root-run agent): coold legitimately needs full
/// root — it drives the Podman socket, mutates iptables/nft, and binds DNS on
/// gateway IPs (kept via `AmbientCapabilities`) — so a `User=` drop is not
/// possible. These sandbox directives instead shrink what a compromised coold
/// can reach on the host, mirroring the builder unit's convention:
///
///   * `NoNewPrivileges=yes` — no setuid/setcap escalation from coold or its
///     children (iptables/nft/podman still work via inherited ambient caps).
///   * `ProtectSystem=strict` — the whole filesystem is read-only except the
///     explicit `ReadWritePaths`, so coold cannot tamper with `/usr`, unit
///     files, or other services' config to gain persistence.
///   * `ProtectHome=yes` / `PrivateTmp=yes` — hide `/home` + `/root` and give
///     a private `/tmp`.
///
/// `ReadWritePaths` allowlist (the only paths coold + its podman children
/// write): `/etc/coolify` (firewall snapshots, JWT/pin reads), `/data/coolify`
/// (Caddy ingress config/state), `/var/lib/containers` (Podman image store used
/// by the Caddy ingress `podman pull`/`run`), and the lazily-created podman
/// runtime dirs under `/run` (`-` prefix tolerates their absence at unit
/// start). `/etc/coolify` and `/data/coolify` are pre-created by the install
/// step so `ProtectSystem=strict` can bind-mount them read-write.
fn hardening_directives() -> String {
    concat!(
        "NoNewPrivileges=yes\n",
        "ProtectSystem=strict\n",
        "ProtectHome=yes\n",
        "PrivateTmp=yes\n",
        "ReadWritePaths=/etc/coolify /data/coolify /var/lib/containers\n",
        "ReadWritePaths=-/run/containers -/run/netavark -/run/libpod\n",
    )
    .to_string()
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
        "[Unit]\nDescription=Configure Coolify mesh DNS resolver\nAfter=systemd-resolved.service {MESH_DNS_ANCHOR_SERVICE}\nWants=systemd-resolved.service {MESH_DNS_ANCHOR_SERVICE}\n\n[Service]\nType=oneshot\nExecStart=/bin/sh -eu -c 'command -v resolvectl >/dev/null 2>&1 || exit 0; {commands}'\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

fn mesh_dns_anchor_container_name(namespace: &str) -> String {
    format!("coolify-mesh-dns-anchor-{namespace}")
}

pub fn install_command(version: &str, sha256: Option<&str>) -> String {
    let verify = checksum_verify_snippet("$DLDIR/coold.tar.gz", "$URL", "coold", sha256);
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
{verify}
tar -xzf "$DLDIR/coold.tar.gz" -C "$DLDIR"
test -f "$DLDIR/coold" || {{ echo "coold binary not found in tarball" >&2; exit 1; }}
install -m 0755 "$DLDIR/coold" /usr/local/bin/coold.tmp
mv /usr/local/bin/coold.tmp /usr/local/bin/coold
echo '{version}' > /usr/local/bin/coold.version"#
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
            let cmd = install_command(version, None);
            assert!(cmd.contains(version));
            assert!(cmd.contains(&format!("coollabsio/coold/releases/download/{version}")));
            assert!(cmd.contains("/usr/local/bin/coold.version"));
        }
        let cmd = install_command("nightly", None);
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
    fn install_command_verifies_pinned_checksum_before_install() {
        let cmd = install_command("v1.2.3", Some("cafef00d"));
        assert!(cmd.contains("cafef00d"));
        assert!(cmd.contains("sha256 mismatch"));
        let verify_at = cmd.find("sha256sum -c -").unwrap();
        let install_at = cmd.find("install -m 0755").unwrap();
        assert!(
            verify_at < install_at,
            "checksum verify must precede install"
        );
    }

    #[test]
    fn service_unit_is_hardened() {
        let got = service_unit("100.64.0.5".parse().unwrap(), &[], None);
        for want in [
            "NoNewPrivileges=yes",
            "ProtectSystem=strict",
            "ProtectHome=yes",
            "PrivateTmp=yes",
            "ReadWritePaths=/etc/coolify /data/coolify /var/lib/containers",
            "ReadWritePaths=-/run/containers -/run/netavark -/run/libpod",
        ] {
            assert!(got.contains(want), "missing {want} in:\n{got}");
        }
        // Ambient caps must be preserved so podman/iptables/nft and DNS bind
        // keep working under NoNewPrivileges.
        assert!(got.contains("AmbientCapabilities=CAP_NET_BIND_SERVICE CAP_NET_ADMIN CAP_NET_RAW"));
    }

    #[test]
    fn service_unit_embeds_mgmt_ip_and_namespaces() {
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
        );
        for want in [
            "Environment=COOLIFY_COOLD_HOST_MGMT_IP=100.64.0.5",
            "Environment=COOLIFY_COOLD_NAMESPACES=default:coolify-default-mesh:10.210.7.1,alpha:coolify-alpha-mesh:10.210.8.1",
            "Environment=COOLIFY_COOLD_DNS_ZONE=coolify.internal",
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
    fn mesh_dns_resolver_unit_does_not_mask_resolved_link_drift() {
        let got = mesh_dns_resolver_unit(&[CooldNamespace {
            name: "default".into(),
            network: "coolify-default-mesh".into(),
            bridge_gateway: "10.210.0.1".parse().unwrap(),
        }]);

        assert!(!got.contains("RemainAfterExit=yes"));
    }

    #[test]
    fn service_unit_omits_namespace_env_when_empty() {
        let got = service_unit("100.64.0.5".parse().unwrap(), &[], None);
        assert!(!got.contains("COOLIFY_COOLD_NAMESPACES"));
        assert!(!got.contains("COOLIFY_COOLD_DNS_ZONE"));
        assert!(got.contains("Environment=COOLIFY_COOLD_HOST_MGMT_IP=100.64.0.5"));
    }

    #[test]
    fn service_unit_emits_flux_env() {
        let got = service_unit(
            "100.64.0.5".parse().unwrap(),
            &[],
            Some(&FluxConfig {
                url: "http://100.64.0.1:6443".into(),
                jwt_path: "/etc/coolify/host-jwt".into(),
                tls_pin_path: None,
            }),
        );
        for want in [
            "Environment=COOLIFY_COOLD_FLUX_URL=http://100.64.0.1:6443",
            "Environment=COOLIFY_COOLD_HOST_JWT_PATH=/etc/coolify/host-jwt",
        ] {
            assert!(got.contains(want), "missing {want} in:\n{got}");
        }
        // Plaintext flux (no pin path) must not emit the TLS pin env.
        assert!(!got.contains("COOLIFY_COOLD_FLUX_TLS_PIN_PATH"));
        assert!(!got.contains("COOLIFY_COOLD_BUILDER_"));
    }

    #[test]
    fn service_unit_emits_flux_tls_env_when_pinned() {
        let got = service_unit(
            "100.64.0.5".parse().unwrap(),
            &[],
            Some(&FluxConfig {
                url: "https://100.64.0.1:6443".into(),
                jwt_path: "/etc/coolify/host-jwt".into(),
                tls_pin_path: Some("/etc/coolify/flux.pin".into()),
            }),
        );
        for want in [
            "Environment=COOLIFY_COOLD_FLUX_URL=https://100.64.0.1:6443",
            "Environment=COOLIFY_COOLD_FLUX_TLS_PIN_PATH=/etc/coolify/flux.pin",
        ] {
            assert!(got.contains(want), "missing {want} in:\n{got}");
        }
    }
}
