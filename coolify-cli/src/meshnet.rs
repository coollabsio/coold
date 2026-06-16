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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_validation_rejects_uppercase_and_dupes() {
        assert!(validate_namespaces(&["default".into(), "alpha-1".into()]).is_ok());
        assert!(validate_namespaces(&["Default".into()]).is_err());
        assert!(validate_namespaces(&["default".into(), "default".into()]).is_err());
    }
}
