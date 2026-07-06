use anyhow::{bail, Result};
use clap::Args;
use regex::Regex;

pub const DEFAULT_NAMESPACE: &str = "default";

#[derive(Debug, Clone, Args)]
pub struct MeshNetMultiFlags {
    #[arg(long, value_delimiter = ',', default_value = DEFAULT_NAMESPACE)]
    pub namespaces: Vec<String>,

    #[arg(long, default_value = "10.210.0.0/16")]
    pub container_pool: String,

    #[arg(long, default_value_t = 24)]
    pub container_prefix: u8,
}

#[derive(Debug, Clone, Args)]
pub struct MeshNetSingleFlags {
    #[arg(long, default_value = DEFAULT_NAMESPACE)]
    pub namespace: String,
}

pub fn podman_network_for(ns: &str) -> String {
    format!("coolify-{ns}-mesh")
}

pub fn validate_namespaces(namespaces: &[String]) -> Result<()> {
    if namespaces.is_empty() {
        bail!("--namespaces must list at least one namespace");
    }
    let re = Regex::new(r"^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$").expect("valid regex");
    let mut seen = std::collections::BTreeSet::new();
    for ns in namespaces {
        if !re.is_match(ns) {
            bail!("invalid namespace {ns:?} (must be a DNS label: lowercase alphanumerics + '-', 1-63 chars)");
        }
        if !seen.insert(ns) {
            bail!("duplicate namespace {ns:?} in --namespaces");
        }
    }
    Ok(())
}

pub fn validate_namespace(ns: &str) -> Result<()> {
    let re = Regex::new(r"^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$").expect("valid regex");
    if !re.is_match(ns) {
        bail!("invalid --namespace {ns:?} (must be a DNS label: lowercase alphanumerics + '-', 1-63 chars)");
    }
    Ok(())
}

/// S-cli (shell/systemd injection): the WireGuard interface name is
/// interpolated verbatim into `/bin/sh -c` fragments and into systemd unit
/// names (`wg-quick@{iface}.service`) during provisioning. Without a charset
/// guard an operator-supplied value containing a space, `;`, `$`, `@`, `/`, or
/// other metacharacter could break out of the intended command/unit. Restrict
/// it to the Linux interface charset (alphanumerics plus `-`, `_`, `.`) with a
/// leading alphanumeric, bounded to the kernel's 15-char `IFNAMSIZ` limit.
pub fn validate_interface(iface: &str) -> Result<()> {
    let re = Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9._-]{0,14}$").expect("valid regex");
    if !re.is_match(iface) {
        bail!("invalid --wg-interface {iface:?} (must be 1-15 chars: alphanumerics, '-', '_', '.', leading alphanumeric)");
    }
    Ok(())
}

/// S-cli (shell injection): version strings are interpolated into the binary
/// install commands — into the download URL and shell — so an unvalidated
/// value could inject shell metacharacters. Allow only the charset that release
/// tags/moving-target names actually use (`nightly`, `latest`, `v1.2.3`), i.e.
/// alphanumerics plus `.`, `_`, `-`, with a leading alphanumeric and a bounded
/// length.
pub fn validate_version(label: &str, version: &str) -> Result<()> {
    let re = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$").expect("valid regex");
    if !re.is_match(version) {
        bail!("invalid {label} {version:?} (must be 1-64 chars: alphanumerics, '.', '_', '-', leading alphanumeric)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_validation_rejects_uppercase_and_dupes() {
        assert!(validate_namespaces(&["default".into(), "alpha-1".into()]).is_ok());
        assert!(validate_namespaces(&["Default".into()]).is_err());
        assert!(validate_namespaces(&["default".into(), "default".into()]).is_err());
    }

    #[test]
    fn namespace_validation_rejects_shell_metacharacters() {
        for bad in [
            "a;b", "a b", "a$b", "a`b`", "a$(id)", "a|b", "a&b", "../etc", "a\nb",
        ] {
            assert!(
                validate_namespaces(&[bad.to_string()]).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        assert!(validate_namespaces(&["default".into()]).is_ok());
    }

    #[test]
    fn interface_validation_guards_shell_and_unit_injection() {
        for good in ["wg0", "wg-mesh", "wg_0", "eth0.1"] {
            assert!(validate_interface(good).is_ok(), "{good} should pass");
        }
        for bad in [
            "wg0; rm -rf /",
            "wg0 x",
            "wg$(id)",
            "wg0@evil",
            "wg/0",
            "-wg0",
            "thisnameiswaytoolong",
            "",
        ] {
            assert!(validate_interface(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn version_validation_rejects_injection() {
        for good in ["nightly", "latest", "v1.2.3", "1.0.0-rc.1"] {
            assert!(validate_version("--coold-version", good).is_ok(), "{good}");
        }
        for bad in ["v1;reboot", "v1 2", "$(id)", "v1`id`", "v1|cat", ""] {
            assert!(validate_version("--coold-version", bad).is_err(), "{bad:?}");
        }
    }
}
