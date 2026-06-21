use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Minimal view of `GET /containers/json` entries.
#[derive(Debug, Deserialize)]
pub struct Container {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(default, rename = "Names")]
    pub names: Vec<String>,
    #[serde(default, rename = "Image")]
    pub image: String,
    #[serde(default, rename = "State")]
    pub state: String,
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

#[derive(Debug, Deserialize)]
pub struct Image {
    #[serde(default, rename = "Id")]
    pub id: String,
    #[serde(default, rename = "RepoTags")]
    pub repo_tags: Vec<String>,
    #[serde(default, rename = "RepoDigests")]
    pub repo_digests: Vec<String>,
    #[serde(default, rename = "Size")]
    pub size: i64,
    #[serde(default, rename = "Created")]
    pub created: String,
}

#[derive(Debug, Deserialize)]
pub struct ImagePullReport {
    #[serde(default, rename = "Id")]
    pub id: String,
    #[serde(default, rename = "Images")]
    pub images: Vec<String>,
    #[serde(default, rename = "Digest")]
    pub digest: String,
    #[serde(default, rename = "Stream")]
    pub stream: String,
}

#[derive(Debug, Deserialize)]
pub struct ContainerCreateResponse {
    #[serde(default, rename = "Id")]
    pub id: String,
}

#[derive(Debug, Serialize)]
pub struct ContainerCreateSpec {
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "image")]
    pub image: String,
    #[serde(rename = "command", skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(rename = "env", skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    #[serde(rename = "networks", skip_serializing_if = "HashMap::is_empty")]
    pub networks: HashMap<String, serde_json::Value>,
    #[serde(rename = "mounts", skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<serde_json::Value>,
    #[serde(rename = "portmappings", skip_serializing_if = "Vec::is_empty")]
    pub port_mappings: Vec<PortMappingSpec>,
    #[serde(rename = "dns_server", skip_serializing_if = "Vec::is_empty")]
    pub dns_servers: Vec<String>,
    #[serde(rename = "dns_search", skip_serializing_if = "Vec::is_empty")]
    pub dns_search: Vec<String>,
    #[serde(rename = "restart_policy", skip_serializing_if = "Option::is_none")]
    pub restart_policy: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PortMappingSpec {
    #[serde(rename = "host_ip", skip_serializing_if = "String::is_empty")]
    pub host_ip: String,
    #[serde(rename = "host_port")]
    pub host_port: u32,
    #[serde(rename = "container_port")]
    pub container_port: u32,
    #[serde(rename = "protocol")]
    pub protocol: String,
}

#[derive(Debug, Deserialize)]
pub struct ExecCreateResponse {
    #[serde(default, rename = "Id")]
    pub id: String,
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
