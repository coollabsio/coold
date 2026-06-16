pub const BUILDER_WORK_DIR: &str = "/var/lib/coolify-builder/work";
pub const BUILDER_BINARY_PATH: &str = "/usr/local/bin/builder";

pub fn install_command(version: &str) -> String {
    format!(
        r#"set -e
DEBIAN_FRONTEND=noninteractive apt-get update -qq 2>/dev/null
DEBIAN_FRONTEND=noninteractive apt-get install -y -o Dpkg::Options::="--force-confold" buildah git ca-certificates 2>&1 >/dev/null
mkdir -p {BUILDER_WORK_DIR}
ARCH_RAW=$(uname -m)
case "$ARCH_RAW" in
  x86_64)  ARCH=amd64 ;;
  aarch64) ARCH=arm64 ;;
  *) echo "unsupported arch: $ARCH_RAW" >&2; exit 1 ;;
esac
URL="https://github.com/coollabsio/coold/releases/download/{version}/builder-linux-${{ARCH}}.tar.gz"
DLDIR=$(mktemp -d)
trap 'rm -rf "$DLDIR"' EXIT
curl -fsSL --retry 3 --max-time 120 -o "$DLDIR/builder.tar.gz" "$URL"
tar -xzf "$DLDIR/builder.tar.gz" -C "$DLDIR"
test -f "$DLDIR/builder" || {{ echo "builder binary not found in tarball" >&2; exit 1; }}
install -m 0755 "$DLDIR/builder" {BUILDER_BINARY_PATH}.tmp
mv {BUILDER_BINARY_PATH}.tmp {BUILDER_BINARY_PATH}
echo '{version}' > {BUILDER_BINARY_PATH}.version"#
    )
}
