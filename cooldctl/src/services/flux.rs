pub const COOLIFY_FLUX_GRPC_PORT: u16 = 6443;
pub const COOLIFY_FLUX_JWT_PUB_PATH: &str = "/etc/coolify/jwt.pub";
pub const COOLIFY_FLUX_JWT_PRIV_PATH: &str = "/etc/coolify/jwt.priv";
pub const HOST_JWT_PATH: &str = "/etc/coolify/host-jwt";
pub const COOLIFY_FLUX_UNIX_SOCKET_PATH: &str = "/run/coolify/flux.sock";

pub fn service_unit(grpc_bind: &str, jwt_pub_path: &str, iface: &str) -> String {
    format!(
        "[Unit]\nDescription=Coolify flux\nAfter=network-online.target wg-quick@{iface}.service\nRequires=wg-quick@{iface}.service\n\n[Service]\nRuntimeDirectory=coolify\nRuntimeDirectoryMode=0750\nEnvironment=COOLIFY_FLUX_GRPC_BIND={grpc_bind}\nEnvironment=COOLIFY_FLUX_UNIX_SOCKET_PATH={COOLIFY_FLUX_UNIX_SOCKET_PATH}\nEnvironment=COOLIFY_FLUX_JWT_PUBLIC_KEY_PATH={jwt_pub_path}\nExecStart=/usr/local/bin/flux\nRestart=on-failure\nRestartSec=2s\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

pub fn install_command(version: &str) -> String {
    format!(
        r#"set -e
ARCH_RAW=$(uname -m)
case "$ARCH_RAW" in
  x86_64)  ARCH=amd64 ;;
  aarch64) ARCH=arm64 ;;
  *) echo "unsupported arch: $ARCH_RAW" >&2; exit 1 ;;
esac
URL="https://github.com/coollabsio/coold/releases/download/{version}/flux-linux-${{ARCH}}.tar.gz"
DLDIR=$(mktemp -d)
trap 'rm -rf "$DLDIR"' EXIT
curl -fsSL --retry 3 --max-time 120 -o "$DLDIR/flux.tar.gz" "$URL"
tar -xzf "$DLDIR/flux.tar.gz" -C "$DLDIR"
test -f "$DLDIR/flux" || {{ echo "flux binary not found in tarball" >&2; exit 1; }}
install -m 0755 "$DLDIR/flux" /usr/local/bin/flux.tmp
mv /usr/local/bin/flux.tmp /usr/local/bin/flux
echo '{version}' > /usr/local/bin/flux.version"#
    )
}

pub fn ensure_jwt_keypair_command() -> String {
    format!(
        "mkdir -p /etc/coolify && if [ ! -f {COOLIFY_FLUX_JWT_PRIV_PATH} ]; then openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out {COOLIFY_FLUX_JWT_PRIV_PATH}.tmp 2>&1 && chmod 0600 {COOLIFY_FLUX_JWT_PRIV_PATH}.tmp && mv {COOLIFY_FLUX_JWT_PRIV_PATH}.tmp {COOLIFY_FLUX_JWT_PRIV_PATH} && openssl pkey -in {COOLIFY_FLUX_JWT_PRIV_PATH} -pubout -out {COOLIFY_FLUX_JWT_PUB_PATH} 2>&1 && chmod 0644 {COOLIFY_FLUX_JWT_PUB_PATH}; fi"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_command_substitutes_version_and_arch() {
        let cmd = install_command("v1.2.3");
        for want in [
            "coollabsio/coold/releases/download/v1.2.3",
            "flux-linux-${ARCH}.tar.gz",
            "x86_64)  ARCH=amd64",
            "aarch64) ARCH=arm64",
            "/usr/local/bin/flux.version",
        ] {
            assert!(cmd.contains(want), "missing {want} in:\n{cmd}");
        }
    }

    #[test]
    fn service_unit_embeds_bind_socket_and_jwt_path() {
        let got = service_unit("100.64.0.1:6443", "/etc/coolify/jwt.pub", "wg9");
        for want in [
            "Requires=wg-quick@wg9.service",
            "Environment=COOLIFY_FLUX_GRPC_BIND=100.64.0.1:6443",
            "Environment=COOLIFY_FLUX_UNIX_SOCKET_PATH=/run/coolify/flux.sock",
            "Environment=COOLIFY_FLUX_JWT_PUBLIC_KEY_PATH=/etc/coolify/jwt.pub",
            "RuntimeDirectory=coolify",
            "ExecStart=/usr/local/bin/flux",
        ] {
            assert!(got.contains(want), "missing {want} in:\n{got}");
        }
    }

    #[test]
    fn ensure_jwt_keypair_command_uses_expected_paths_and_modes() {
        let cmd = ensure_jwt_keypair_command();
        for want in [
            "openssl genpkey -algorithm EC",
            "ec_paramgen_curve:P-256",
            "/etc/coolify/jwt.priv",
            "/etc/coolify/jwt.pub",
            "chmod 0600",
            "chmod 0644",
        ] {
            assert!(cmd.contains(want), "missing {want} in:\n{cmd}");
        }
    }
}
