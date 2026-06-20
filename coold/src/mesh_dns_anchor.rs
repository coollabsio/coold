use anyhow::{anyhow, Context, Result};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::{Config, NamespaceConfig};

const MESH_DNS_ANCHOR_IMAGE: &str = "docker.io/library/alpine:3.20";

pub async fn reconcile(config: &Config) {
    for namespace in reconcilable_namespaces(config.namespaces.as_slice()) {
        match anchor_is_running(namespace).await {
            Ok(true) => debug!(
                namespace = %namespace.name,
                network = %namespace.network,
                "mesh DNS anchor container is running"
            ),
            Ok(false) => {
                if let Err(e) = start_anchor(namespace).await {
                    warn!(
                        namespace = %namespace.name,
                        network = %namespace.network,
                        error = format!("{:#}", e),
                        "mesh DNS anchor repair failed"
                    );
                }
            }
            Err(e) => warn!(
                namespace = %namespace.name,
                network = %namespace.network,
                error = format!("{:#}", e),
                "mesh DNS anchor inspect failed"
            ),
        }
    }
}

fn reconcilable_namespaces(
    namespaces: &[NamespaceConfig],
) -> impl Iterator<Item = &NamespaceConfig> {
    namespaces
        .iter()
        .filter(|namespace| !namespace.gateway_ip.is_unspecified())
}

async fn anchor_is_running(namespace: &NamespaceConfig) -> Result<bool> {
    let name = anchor_container_name(&namespace.name);
    let output = Command::new("podman")
        .args(["inspect", "-f", "{{.State.Running}}", &name])
        .output()
        .await
        .with_context(|| format!("inspect mesh DNS anchor container {name}"))?;

    if !output.status.success() {
        return Ok(false);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim() == "true")
}

async fn start_anchor(namespace: &NamespaceConfig) -> Result<()> {
    let name = anchor_container_name(&namespace.name);
    let output = Command::new("podman")
        .args(podman_run_args(namespace))
        .output()
        .await
        .with_context(|| format!("start mesh DNS anchor container {name}"))?;

    if !output.status.success() {
        return Err(anyhow!(
            "podman run failed for {name}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    info!(
        namespace = %namespace.name,
        network = %namespace.network,
        container = %name,
        "repaired mesh DNS anchor container"
    );

    Ok(())
}

fn podman_run_args(namespace: &NamespaceConfig) -> Vec<String> {
    vec![
        "run".into(),
        "-d".into(),
        "--replace".into(),
        "--name".into(),
        anchor_container_name(&namespace.name),
        "--network".into(),
        namespace.network.clone(),
        "--label".into(),
        "io.coolify.managed=true".into(),
        "--label".into(),
        "io.coolify.role=mesh-dns-anchor".into(),
        "--label".into(),
        format!("io.coolify.namespace={}", namespace.name),
        MESH_DNS_ANCHOR_IMAGE.into(),
        "sleep".into(),
        "infinity".into(),
    ]
}

fn anchor_container_name(namespace: &str) -> String {
    format!("coolify-mesh-dns-anchor-{namespace}")
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use crate::config::NamespaceConfig;

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

    #[test]
    fn builds_mesh_dns_anchor_run_args_for_namespace() {
        let namespace = NamespaceConfig {
            name: "default".into(),
            network: "coolify-default-mesh".into(),
            gateway_ip: "10.210.0.1".parse::<IpAddr>().unwrap(),
        };

        let args = super::podman_run_args(&namespace);

        assert_eq!(args[0], "run");
        assert!(args.contains(&"--replace".to_string()));
        assert!(args.contains(&"coolify-mesh-dns-anchor-default".to_string()));
        assert!(args.contains(&"coolify-default-mesh".to_string()));
        assert!(args.contains(&"io.coolify.role=mesh-dns-anchor".to_string()));
        assert!(args.contains(&"io.coolify.namespace=default".to_string()));
        assert_eq!(args[args.len() - 2], "sleep");
        assert_eq!(args[args.len() - 1], "infinity");
    }
}
