pub const SCHEDULER_GRPC_PORT: u16 = 6443;
pub const SCHEDULER_JWT_PUB_PATH: &str = "/etc/coolify/jwt.pub";
pub const SCHEDULER_JWT_PRIV_PATH: &str = "/etc/coolify/jwt.priv";
pub const HOST_JWT_PATH: &str = "/etc/coolify/host-jwt";
pub const SCHEDULER_UNIX_SOCKET_PATH: &str = "/run/coolify/scheduler.sock";

pub fn service_unit(grpc_bind: &str, jwt_pub_path: &str, iface: &str) -> String {
    format!(
        "[Unit]\nDescription=Coolify scheduler\nAfter=network-online.target wg-quick@{iface}.service\nRequires=wg-quick@{iface}.service\n\n[Service]\nRuntimeDirectory=coolify\nRuntimeDirectoryMode=0750\nEnvironment=SCHEDULER_GRPC_BIND={grpc_bind}\nEnvironment=SCHEDULER_UNIX_SOCKET_PATH={SCHEDULER_UNIX_SOCKET_PATH}\nEnvironment=SCHEDULER_JWT_PUBLIC_KEY_PATH={jwt_pub_path}\nExecStart=/usr/local/bin/scheduler\nRestart=on-failure\nRestartSec=2s\n\n[Install]\nWantedBy=multi-user.target\n"
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
URL="https://github.com/coollabsio/coold/releases/download/{version}/scheduler-linux-${{ARCH}}.tar.gz"
DLDIR=$(mktemp -d)
trap 'rm -rf "$DLDIR"' EXIT
curl -fsSL --retry 3 --max-time 120 -o "$DLDIR/scheduler.tar.gz" "$URL"
tar -xzf "$DLDIR/scheduler.tar.gz" -C "$DLDIR"
test -f "$DLDIR/scheduler" || {{ echo "scheduler binary not found in tarball" >&2; exit 1; }}
install -m 0755 "$DLDIR/scheduler" /usr/local/bin/scheduler.tmp
mv /usr/local/bin/scheduler.tmp /usr/local/bin/scheduler
echo '{version}' > /usr/local/bin/scheduler.version"#
    )
}

pub fn ensure_jwt_keypair_command() -> String {
    format!(
        "mkdir -p /etc/coolify && if [ ! -f {SCHEDULER_JWT_PRIV_PATH} ]; then openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out {SCHEDULER_JWT_PRIV_PATH}.tmp 2>&1 && chmod 0600 {SCHEDULER_JWT_PRIV_PATH}.tmp && mv {SCHEDULER_JWT_PRIV_PATH}.tmp {SCHEDULER_JWT_PRIV_PATH} && openssl pkey -in {SCHEDULER_JWT_PRIV_PATH} -pubout -out {SCHEDULER_JWT_PUB_PATH} 2>&1 && chmod 0644 {SCHEDULER_JWT_PUB_PATH}; fi"
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
            "scheduler-linux-${ARCH}.tar.gz",
            "x86_64)  ARCH=amd64",
            "aarch64) ARCH=arm64",
            "/usr/local/bin/scheduler.version",
        ] {
            assert!(cmd.contains(want), "missing {want} in:\n{cmd}");
        }
    }

    #[test]
    fn service_unit_embeds_bind_socket_and_jwt_path() {
        let got = service_unit("100.64.0.1:6443", "/etc/coolify/jwt.pub", "wg9");
        for want in [
            "Requires=wg-quick@wg9.service",
            "Environment=SCHEDULER_GRPC_BIND=100.64.0.1:6443",
            "Environment=SCHEDULER_UNIX_SOCKET_PATH=/run/coolify/scheduler.sock",
            "Environment=SCHEDULER_JWT_PUBLIC_KEY_PATH=/etc/coolify/jwt.pub",
            "RuntimeDirectory=coolify",
            "ExecStart=/usr/local/bin/scheduler",
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
