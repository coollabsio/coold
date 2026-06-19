use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerStatus {
    pub container_id: String,
    pub container_name: String,
    pub image: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerStatusDelta {
    Upsert(ContainerStatus),
    Delete { container_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub container_id: String,
    pub container_name: String,
    /// Mesh namespace the container lives in. Same shape as the namespace
    /// label stamped on the podman bridge (`io.coolify.namespace=<ns>`).
    pub namespace: String,
    pub host_mgmt_ip: String,
    pub container_ip: String,
    /// Raw podman status: "running", "exited", "stopped", "restarting",
    /// "paused", "created", "dead", "configured", "removing". Liveness signal.
    pub state: String,
    /// HEALTHCHECK result. One of "healthy", "unhealthy", "starting", "unknown".
    /// "unknown" when the container has no HEALTHCHECK declared.
    pub health: String,
}

#[derive(Debug, Clone)]
pub enum Delta {
    Upsert(Endpoint),
    Delete { container_id: String },
}

/// Diff `desired` (from Podman) against `current` (last snapshot in Corrosion for this host).
/// Returns deltas required to make Corrosion match Podman.
pub fn diff(
    desired: &HashMap<String, Endpoint>,
    current: &HashMap<String, Endpoint>,
) -> Vec<Delta> {
    let mut out = Vec::new();

    for (id, ep) in desired {
        match current.get(id) {
            Some(existing) if existing == ep => {}
            _ => out.push(Delta::Upsert(ep.clone())),
        }
    }

    for id in current.keys() {
        if !desired.contains_key(id) {
            out.push(Delta::Delete {
                container_id: id.clone(),
            });
        }
    }

    out
}

/// Diff all Podman containers for host-level status reporting. Unlike
/// `diff`, this intentionally includes containers outside managed mesh
/// networks so Coolify can track ingress and future non-managed containers.
pub fn diff_container_statuses(
    desired: &HashMap<String, ContainerStatus>,
    current: &HashMap<String, ContainerStatus>,
) -> Vec<ContainerStatusDelta> {
    let mut out = Vec::new();

    for (id, status) in desired {
        match current.get(id) {
            Some(existing) if existing == status => {}
            _ => out.push(ContainerStatusDelta::Upsert(status.clone())),
        }
    }

    for id in current.keys() {
        if !desired.contains_key(id) {
            out.push(ContainerStatusDelta::Delete {
                container_id: id.clone(),
            });
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(id: &str, ip: &str) -> Endpoint {
        Endpoint {
            container_id: id.into(),
            container_name: format!("name-{id}"),
            namespace: "default".into(),
            host_mgmt_ip: "100.64.0.5".into(),
            container_ip: ip.into(),
            state: "running".into(),
            health: "unknown".into(),
        }
    }

    #[test]
    fn diff_detects_inserts_updates_deletes() {
        let mut desired = HashMap::new();
        desired.insert("a".into(), ep("a", "10.210.5.2"));
        desired.insert("b".into(), ep("b", "10.210.5.3"));

        let mut current = HashMap::new();
        current.insert("a".into(), ep("a", "10.210.5.99")); // changed IP → upsert
        current.insert("c".into(), ep("c", "10.210.5.4")); // missing from desired → delete

        let deltas = diff(&desired, &current);
        assert_eq!(deltas.len(), 3);

        let upserts: Vec<_> = deltas
            .iter()
            .filter_map(|d| match d {
                Delta::Upsert(e) => Some(e.container_id.as_str()),
                _ => None,
            })
            .collect();
        let deletes: Vec<_> = deltas
            .iter()
            .filter_map(|d| match d {
                Delta::Delete { container_id } => Some(container_id.as_str()),
                _ => None,
            })
            .collect();

        assert!(upserts.contains(&"a"));
        assert!(upserts.contains(&"b"));
        assert_eq!(deletes, vec!["c"]);
    }

    #[test]
    fn diff_noop_when_equal() {
        let mut m = HashMap::new();
        m.insert("a".into(), ep("a", "10.210.5.2"));
        assert!(diff(&m, &m).is_empty());
    }

    #[test]
    fn diff_flags_namespace_change() {
        let mut desired = HashMap::new();
        let mut e = ep("a", "10.210.5.2");
        e.namespace = "alpha".into();
        desired.insert("a".into(), e);

        let mut current = HashMap::new();
        current.insert("a".into(), ep("a", "10.210.5.2")); // namespace="default"

        let deltas = diff(&desired, &current);
        assert_eq!(deltas.len(), 1);
        assert!(matches!(deltas[0], Delta::Upsert(_)));
    }

    fn container_status(id: &str, state: &str) -> ContainerStatus {
        ContainerStatus {
            container_id: id.into(),
            container_name: format!("container-{id}"),
            image: "docker.io/library/nginx:alpine".into(),
            state: state.into(),
        }
    }

    #[test]
    fn diff_container_statuses_reports_all_container_changes() {
        let mut desired = HashMap::new();
        desired.insert("a".into(), container_status("a", "running"));
        desired.insert("b".into(), container_status("b", "exited"));

        let mut current = HashMap::new();
        current.insert("a".into(), container_status("a", "created"));
        current.insert("c".into(), container_status("c", "running"));

        let deltas = diff_container_statuses(&desired, &current);

        assert_eq!(deltas.len(), 3);
        assert!(deltas.iter().any(|delta| matches!(delta, ContainerStatusDelta::Upsert(status) if status.container_id == "a" && status.state == "running")));
        assert!(deltas.iter().any(|delta| matches!(delta, ContainerStatusDelta::Upsert(status) if status.container_id == "b" && status.state == "exited")));
        assert!(deltas.iter().any(|delta| matches!(delta, ContainerStatusDelta::Delete { container_id } if container_id == "c")));
    }
}
