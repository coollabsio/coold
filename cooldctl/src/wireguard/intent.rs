use anyhow::{Result, bail};
use serde::Serialize;
use std::collections::BTreeSet;

use super::{
    plan::{ActionType, PlannedAction},
    state::DesiredMesh,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum Intent {
    #[default]
    Bootstrap,
    Extend,
    Upgrade,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    SafeAlways,
    PeerRefresh,
    DestructiveReplace,
    VersionBump,
    WipeDb,
    SchemaFirstWrite,
}

fn categorize(a: &PlannedAction) -> Category {
    use ActionType::*;
    match a.action_type {
        InstallWg
        | GenKeyPair
        | AllocateMgmtIp
        | AllocateContainerSubnet
        | EnableService
        | InstallPodman
        | EnablePodmanSocket
        | EnableIpForward
        | CreatePodmanNetwork
        | GenerateJwtKeypair
        | AddPeer
        | RemovePeer => Category::SafeAlways,
        WriteConfig
        | ReloadService
        | InstallFirewall
        | WriteCorrosionConfig
        | InstallCorrosionService
        | InstallCooldService
        | InstallCoolifyService
        | InstallSchedulerService
        | WriteHostJwt
        | UpdateCooldSchedulerEnv => Category::PeerRefresh,
        RecreatePodmanNetwork => Category::DestructiveReplace,
        InstallCorrosion | InstallCoold | InstallCoolify | InstallScheduler | InstallBuilder => {
            Category::VersionBump
        }
        WriteCorrosionSchema if a.detail.contains("DB will be reset") => Category::WipeDb,
        WriteCorrosionSchema => Category::SchemaFirstWrite,
    }
}

pub fn validate_intent(d: &DesiredMesh) -> Result<()> {
    match d.intent {
        Intent::Bootstrap => Ok(()),
        Intent::Extend => {
            if d.new_hosts.is_empty() {
                bail!("extend mode requires at least one host in NewHosts");
            }
            for h in &d.new_hosts {
                if !d.hosts.contains(h) {
                    bail!("extend mode: new host {h:?} not in --servers list");
                }
            }
            Ok(())
        }
        Intent::Upgrade => {
            if !d.allow_nightly {
                for (flag, v) in [
                    ("--coold-version", &d.coold_version),
                    ("--corrosion-version", &d.corrosion_version),
                    ("--coolify-version", &d.coolify_version),
                    ("--scheduler-version", &d.scheduler_version),
                ] {
                    if v == "nightly" {
                        bail!(
                            "upgrade mode rejects {flag}=nightly (moving target forces re-install every run); pin a version or pass --allow-nightly"
                        );
                    }
                }
            }
            Ok(())
        }
    }
}

pub fn filter_by_intent(plan: &mut super::plan::Plan, d: &DesiredMesh) {
    if d.intent == Intent::Bootstrap {
        return;
    }
    let new_hosts: BTreeSet<_> = d.new_hosts.iter().cloned().collect();
    let mut kept = Vec::new();
    for a in std::mem::take(&mut plan.actions) {
        if let Some(reason) = decide(&a, d, &new_hosts) {
            plan.skipped
                .push(super::plan::SkippedAction { action: a, reason });
        } else {
            kept.push(a);
        }
    }
    plan.actions = kept;
}

fn decide(a: &PlannedAction, d: &DesiredMesh, new_hosts: &BTreeSet<String>) -> Option<String> {
    let is_new = new_hosts.contains(&a.host);
    match d.intent {
        Intent::Bootstrap => None,
        Intent::Extend if is_new => None,
        Intent::Extend => match categorize(a) {
            Category::SafeAlways|Category::PeerRefresh|Category::SchemaFirstWrite => None,
            Category::DestructiveReplace if d.allow_replace => None,
            Category::DestructiveReplace => Some("extend: destructive-replace on existing host blocked; pass --allow-replace to override".into()),
            Category::VersionBump => Some("extend: version-bump on existing host skipped; use `cooldctl init upgrade` to bump versions".into()),
            Category::WipeDb => Some("extend: corrosion DB wipe on existing host is never allowed; resolve schema drift with `cooldctl init upgrade` on a fresh schema".into()),
        },
        Intent::Upgrade => match categorize(a) {
            Category::VersionBump => None,
            Category::PeerRefresh if matches!(a.action_type, ActionType::InstallCorrosionService|ActionType::InstallCooldService|ActionType::InstallCoolifyService|ActionType::InstallSchedulerService) => None,
            Category::PeerRefresh => Some("upgrade: peer-refresh skipped; use `cooldctl init extend` for mesh topology changes".into()),
            _ => Some("upgrade: non-version-bump action skipped".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wireguard::plan::Plan;
    use ipnet::Ipv4Net;
    use std::collections::BTreeMap;

    fn desired(intent: Intent) -> DesiredMesh {
        DesiredMesh {
            hosts: vec!["A".into(), "B".into()],
            interface: "wg0".into(),
            mgmt_pool: "100.64.0.0/16".parse::<Ipv4Net>().unwrap(),
            container_pool: "10.210.0.0/16".parse::<Ipv4Net>().unwrap(),
            container_prefix: 24,
            listen_port: 51820,
            install_podman: true,
            namespaces: vec!["default".into()],
            default_deny_containers: true,
            install_coold: true,
            coold_version: "v1".into(),
            corrosion_version: "v1".into(),
            corrosion_gossip_port: 8787,
            corrosion_api_port: 8080,
            central_host: String::new(),
            coolify_version: "v1".into(),
            scheduler_version: "v1".into(),
            enable_builder: true,
            builder_hosts: vec![],
            builder_capacity: 2,
            builder_cpu_quota: "200%".into(),
            builder_memory_max: "2G".into(),
            builder_timeout_secs: 1800,
            intent,
            new_hosts: vec![],
            allow_replace: false,
            allow_nightly: false,
        }
    }

    fn action(host: &str, action_type: ActionType) -> PlannedAction {
        PlannedAction {
            host: host.into(),
            namespace: String::new(),
            action_type,
            detail: String::new(),
        }
    }

    fn plan(actions: Vec<PlannedAction>) -> Plan {
        Plan {
            actions,
            mgmt_assignments: BTreeMap::new(),
            subnet_assignments: BTreeMap::new(),
            warnings: vec![],
            skipped: vec![],
        }
    }

    #[test]
    fn validate_intent_extend_requires_new_hosts_in_servers() {
        let mut d = desired(Intent::Extend);
        let err = validate_intent(&d).unwrap_err();
        assert!(err.to_string().contains("NewHosts"));

        d.new_hosts = vec!["C".into()];
        let err = validate_intent(&d).unwrap_err();
        assert!(err.to_string().contains("\"C\""));
        assert!(err.to_string().contains("--servers"));

        d.hosts.push("C".into());
        assert!(validate_intent(&d).is_ok());
    }

    #[test]
    fn validate_intent_upgrade_rejects_nightly_by_default() {
        for (field, mut d) in [
            ("coold", {
                let mut d = desired(Intent::Upgrade);
                d.coold_version = "nightly".into();
                d
            }),
            ("corrosion", {
                let mut d = desired(Intent::Upgrade);
                d.corrosion_version = "nightly".into();
                d
            }),
            ("scheduler", {
                let mut d = desired(Intent::Upgrade);
                d.scheduler_version = "nightly".into();
                d
            }),
        ] {
            let err = validate_intent(&d).expect_err(field);
            assert!(err.to_string().contains("nightly"));
            d.allow_nightly = true;
            assert!(validate_intent(&d).is_ok());
        }
    }

    #[test]
    fn filter_bootstrap_keeps_everything() {
        let mut p = plan(vec![
            action("A", ActionType::InstallCoold),
            action("B", ActionType::RecreatePodmanNetwork),
        ]);
        filter_by_intent(&mut p, &desired(Intent::Bootstrap));
        assert_eq!(p.actions.len(), 2);
        assert!(p.skipped.is_empty());
    }

    #[test]
    fn filter_extend_existing_hosts_keep_peer_refresh_only() {
        let mut p = plan(vec![
            action("A-old", ActionType::WriteConfig),
            action("A-old", ActionType::ReloadService),
            action("A-old", ActionType::WriteCorrosionConfig),
            action("A-old", ActionType::InstallFirewall),
            action("A-old", ActionType::InstallCoold),
            action("A-old", ActionType::InstallBuilder),
            action("A-new", ActionType::InstallCoold),
        ]);
        let mut d = desired(Intent::Extend);
        d.hosts = vec!["A-old".into(), "A-new".into()];
        d.new_hosts = vec!["A-new".into()];
        filter_by_intent(&mut p, &d);

        let kept = p
            .actions
            .iter()
            .map(|a| (&a.host, a.action_type))
            .collect::<Vec<_>>();
        assert!(kept.contains(&(&"A-old".to_string(), ActionType::WriteConfig)));
        assert!(kept.contains(&(&"A-old".to_string(), ActionType::InstallFirewall)));
        assert!(kept.contains(&(&"A-new".to_string(), ActionType::InstallCoold)));
        assert!(!kept.contains(&(&"A-old".to_string(), ActionType::InstallCoold)));
        assert_eq!(p.skipped.len(), 2);
    }

    #[test]
    fn filter_extend_blocks_destructive_existing_unless_allowed() {
        let mut p = plan(vec![action("A-old", ActionType::RecreatePodmanNetwork)]);
        let mut d = desired(Intent::Extend);
        d.new_hosts = vec!["A-new".into()];
        filter_by_intent(&mut p, &d);
        assert!(p.actions.is_empty());
        assert!(p.skipped[0].reason.contains("destructive-replace"));

        let mut p = plan(vec![action("A-old", ActionType::RecreatePodmanNetwork)]);
        d.allow_replace = true;
        filter_by_intent(&mut p, &d);
        assert_eq!(p.actions.len(), 1);
        assert!(p.skipped.is_empty());
    }

    #[test]
    fn filter_upgrade_keeps_version_bumps_and_service_reinstalls_only() {
        let mut p = plan(vec![
            action("A", ActionType::InstallCoold),
            action("A", ActionType::InstallCorrosion),
            action("A", ActionType::InstallScheduler),
            action("A", ActionType::InstallCooldService),
            action("A", ActionType::WriteConfig),
            action("A", ActionType::CreatePodmanNetwork),
        ]);
        filter_by_intent(&mut p, &desired(Intent::Upgrade));
        let kept = p.actions.iter().map(|a| a.action_type).collect::<Vec<_>>();
        assert_eq!(
            kept,
            vec![
                ActionType::InstallCoold,
                ActionType::InstallCorrosion,
                ActionType::InstallScheduler,
                ActionType::InstallCooldService,
            ]
        );
        assert_eq!(p.skipped.len(), 2);
    }

    #[test]
    fn categorize_schema_wipe_vs_first_write() {
        let first = PlannedAction {
            detail: "/etc/corrosion/schemas/coolify.sql".into(),
            ..action("A", ActionType::WriteCorrosionSchema)
        };
        let wipe = PlannedAction {
            detail: "/etc/corrosion/schemas/coolify.sql [schema drift — DB will be reset]".into(),
            ..action("A", ActionType::WriteCorrosionSchema)
        };
        assert_eq!(categorize(&first), Category::SchemaFirstWrite);
        assert_eq!(categorize(&wipe), Category::WipeDb);
    }
}
