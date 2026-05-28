pub const COOLIFY_BINARY_PATH: &str = "/usr/local/bin/coolify";
pub const COOLIFY_VERSION_PATH: &str = "/usr/local/bin/coolify.version";
pub const COOLIFY_DB_PATH: &str = "/var/lib/coolify/coolify.db";
pub const COOLIFY_API_BIND: &str = "0.0.0.0:3000";

pub fn release_url(version: &str) -> String {
    if version == "latest" {
        "https://github.com/coollabsio/coold/releases/latest/download/coolify-linux-${ARCH}.tar.gz"
            .into()
    } else {
        format!(
            "https://github.com/coollabsio/coold/releases/download/{version}/coolify-linux-${{ARCH}}.tar.gz"
        )
    }
}

pub fn install_command(version: &str) -> String {
    let url = release_url(version);
    format!(
        r#"set -e
ARCH_RAW=$(uname -m)
case "$ARCH_RAW" in
  x86_64)  ARCH=amd64 ;;
  aarch64) ARCH=arm64 ;;
  *) echo "unsupported arch: $ARCH_RAW" >&2; exit 1 ;;
esac
URL="{url}"
DLDIR=$(mktemp -d)
trap 'rm -rf "$DLDIR"' EXIT
curl -fsSL --retry 3 --max-time 120 -o "$DLDIR/coolify.tar.gz" "$URL"
tar -xzf "$DLDIR/coolify.tar.gz" -C "$DLDIR"
test -f "$DLDIR/coolify" || {{ echo "coolify binary not found in tarball" >&2; exit 1; }}
install -m 0755 "$DLDIR/coolify" {COOLIFY_BINARY_PATH}.tmp
mv {COOLIFY_BINARY_PATH}.tmp {COOLIFY_BINARY_PATH}
echo '{version}' > {COOLIFY_VERSION_PATH}"#
    )
}

pub fn service_unit() -> String {
    format!(
        "[Unit]\nDescription=Coolify UI/API\nAfter=network-online.target scheduler.service\nWants=network-online.target scheduler.service\n\n[Service]\nStateDirectory=coolify\nWorkingDirectory=/var/lib/coolify\nEnvironment=COOLIFY_API_BIND={COOLIFY_API_BIND}\nEnvironment=COOLIFY_API_DB={COOLIFY_DB_PATH}\nEnvironment=COOLIFY_SCHEDULER_SOCKET={}\nExecStart={COOLIFY_BINARY_PATH} serve\nRestart=on-failure\nRestartSec=2s\n\n[Install]\nWantedBy=multi-user.target\n",
        crate::services::scheduler::SCHEDULER_UNIX_SOCKET_PATH
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_command_supports_nightly_pinned_and_latest_release_urls() {
        let nightly = install_command("nightly");
        assert!(nightly.contains("coollabsio/coold/releases/download/nightly"));
        assert!(nightly.contains("coolify-linux-${ARCH}.tar.gz"));
        assert!(nightly.contains("/usr/local/bin/coolify.version"));

        let pinned = install_command("v1.2.3");
        assert!(pinned.contains("coollabsio/coold/releases/download/v1.2.3"));

        let latest = install_command("latest");
        assert!(latest.contains("coollabsio/coold/releases/latest/download"));
    }

    #[test]
    fn service_unit_runs_coolify_binary() {
        let got = service_unit();
        assert!(got.contains("Description=Coolify UI/API"));
        assert!(got.contains("ExecStart=/usr/local/bin/coolify"));
    }
}
