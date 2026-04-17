use std::collections::HashMap;

use serde::Deserialize;

/// Minimal view of `GET /containers/json` entries.
#[derive(Debug, Deserialize)]
pub struct Container {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(default, rename = "Names")]
    pub names: Vec<String>,
    #[serde(default, rename = "NetworkSettings")]
    pub network_settings: Option<ListNetworkSettings>,
}

#[derive(Debug, Deserialize)]
pub struct ListNetworkSettings {
    #[serde(default, rename = "Networks")]
    pub networks: HashMap<String, NetworkEntry>,
}

#[derive(Debug, Deserialize)]
pub struct NetworkEntry {
    #[serde(default, rename = "IPAddress")]
    pub ip_address: String,
}

/// Subset of `GET /containers/{id}/json` — libpod's list endpoint leaves
/// `NetworkSettings.Networks` empty, so we inspect each container to get IPs.
/// Also the only source of truth for container state + HEALTHCHECK result.
#[derive(Debug, Deserialize)]
pub struct ContainerInspect {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(default, rename = "Name")]
    pub name: String,
    #[serde(default, rename = "NetworkSettings")]
    pub network_settings: Option<ListNetworkSettings>,
    #[serde(default, rename = "State")]
    pub state: Option<ContainerState>,
}

/// Podman inspect `State` block. `status` is liveness (running, exited,
/// stopped, restarting, paused, created, dead, configured, removing).
/// `health` is only populated when the container declares a HEALTHCHECK.
#[derive(Debug, Default, Deserialize)]
pub struct ContainerState {
    #[serde(default, rename = "Status")]
    pub status: String,
    #[serde(default, rename = "Health")]
    pub health: Option<ContainerHealth>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ContainerHealth {
    #[serde(default, rename = "Status")]
    pub status: String,
}

/// One line of the `GET /events` NDJSON stream.
#[derive(Debug, Deserialize)]
pub struct Event {
    #[serde(default, rename = "Type")]
    pub kind: String,
    #[serde(default, rename = "Action")]
    pub action: String,
    #[serde(default, rename = "Actor")]
    pub actor: EventActor,
    #[serde(default, rename = "status")]
    pub status: String,
    #[serde(default, rename = "id")]
    pub id: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct EventActor {
    #[serde(default, rename = "ID")]
    pub id: String,
}

impl Event {
    pub fn container_id(&self) -> &str {
        if !self.actor.id.is_empty() {
            &self.actor.id
        } else {
            &self.id
        }
    }

    pub fn action_name(&self) -> &str {
        if !self.action.is_empty() {
            &self.action
        } else {
            &self.status
        }
    }
}
