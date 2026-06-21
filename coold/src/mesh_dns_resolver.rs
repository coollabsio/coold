use anyhow::{anyhow, Context, Result};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::config::{Config, NamespaceConfig};

pub async fn reconcile(config: &Config) {
    for namespace in reconcilable_namespaces(config.namespaces.as_slice()) {
        match reconcile_namespace(namespace, &config.dns_zone).await {
            Ok(()) => debug!(
                namespace = %namespace.name,
                network = %namespace.network,
                gateway_ip = %namespace.gateway_ip,
                "mesh DNS host resolver route reconciled"
            ),
            Err(e) => warn!(
                namespace = %namespace.name,
                network = %namespace.network,
                gateway_ip = %namespace.gateway_ip,
                error = format!("{:#}", e),
                "mesh DNS host resolver route reconcile failed"
            ),
        }
    }
}

async fn reconcile_namespace(namespace: &NamespaceConfig, dns_zone: &str) -> Result<()> {
    let iface = podman_network_interface(namespace).await?;
    if iface.is_empty() {
        return Err(anyhow!(
            "podman network {} has no network interface",
            namespace.network
        ));
    }

    for args in resolvectl_reconcile_args(&iface, &namespace.gateway_ip.to_string(), dns_zone) {
        let output = Command::new("resolvectl")
            .args(&args)
            .output()
            .await
            .with_context(|| format!("run resolvectl {}", args.join(" ")))?;

        if !output.status.success() {
            return Err(anyhow!(
                "resolvectl {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }

    Ok(())
}

async fn podman_network_interface(namespace: &NamespaceConfig) -> Result<String> {
    let output = Command::new("podman")
        .args(podman_network_interface_args(namespace))
        .output()
        .await
        .with_context(|| format!("inspect podman network {}", namespace.network))?;

    if !output.status.success() {
        return Err(anyhow!(
            "podman network inspect failed for {}: {}",
            namespace.network,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn reconcilable_namespaces(
    namespaces: &[NamespaceConfig],
) -> impl Iterator<Item = &NamespaceConfig> {
    namespaces
        .iter()
        .filter(|namespace| !namespace.gateway_ip.is_unspecified())
}

fn podman_network_interface_args(namespace: &NamespaceConfig) -> Vec<String> {
    vec![
        "network".into(),
        "inspect".into(),
        namespace.network.clone(),
        "--format".into(),
        "{{.NetworkInterface}}".into(),
    ]
}

fn resolvectl_reconcile_args(iface: &str, gateway_ip: &str, dns_zone: &str) -> Vec<Vec<String>> {
    vec![
        vec!["dns".into(), iface.into(), gateway_ip.into()],
        vec!["domain".into(), iface.into(), format!("~{dns_zone}")],
        vec!["default-route".into(), iface.into(), "false".into()],
    ]
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use crate::config::NamespaceConfig;

    #[test]
    fn builds_podman_network_interface_lookup_args() {
        let namespace = NamespaceConfig {
            name: "default".into(),
            network: "coolify-default-mesh".into(),
            gateway_ip: "10.210.0.1".parse::<IpAddr>().unwrap(),
        };

        let args = super::podman_network_interface_args(&namespace);

        assert_eq!(
            args,
            [
                "network",
                "inspect",
                "coolify-default-mesh",
                "--format",
                "{{.NetworkInterface}}"
            ]
        );
    }

    #[test]
    fn builds_resolvectl_reconcile_commands_for_link_route_only_dns() {
        let commands =
            super::resolvectl_reconcile_args("podman1", "10.210.0.1", "coolify.internal");

        assert_eq!(
            commands,
            [
                vec!["dns", "podman1", "10.210.0.1"],
                vec!["domain", "podman1", "~coolify.internal"],
                vec!["default-route", "podman1", "false"],
            ]
        );
    }

    #[test]
    fn skips_namespaces_without_gateway_ip() {
        let namespaces = vec![
            NamespaceConfig {
                name: "default".into(),
                network: "coolify-default-mesh".into(),
                gateway_ip: "0.0.0.0".parse::<IpAddr>().unwrap(),
            },
            NamespaceConfig {
                name: "alpha".into(),
                network: "coolify-alpha-mesh".into(),
                gateway_ip: "10.220.0.1".parse::<IpAddr>().unwrap(),
            },
        ];

        let got: Vec<_> = super::reconcilable_namespaces(&namespaces)
            .map(|namespace| namespace.name.as_str())
            .collect();

        assert_eq!(got, ["alpha"]);
    }
}
