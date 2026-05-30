use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("invalid {field}: {reason}")]
    Invalid { field: &'static str, reason: String },
}

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(pub Uuid);
        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }
    };
}

id_type!(ServerId);
id_type!(ClusterId);
id_type!(AppId);
id_type!(BuildId);
id_type!(DeploymentId);
id_type!(EventId);
id_type!(FirewallRuleId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerStatus {
    #[default]
    Unknown,
    Provisioning,
    Online,
    Offline,
    Error,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub id: ServerId,
    pub name: String,
    pub address: String,
    pub mgmt_ip: Option<String>,
    pub status: ServerStatus,
    pub coold_version: Option<String>,
    pub host_id: Option<String>,
    pub capabilities: Vec<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerStream {
    pub host_id: String,
    pub caps: Vec<String>,
    pub builder_capacity: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSyncResult {
    pub created: u32,
    pub updated: u32,
    pub server_ids: Vec<ServerId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerSummary {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub networks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerLiveStatus {
    pub server_id: ServerId,
    pub host_id: Option<String>,
    pub scheduler_configured: bool,
    pub reachable: bool,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cluster {
    pub id: ClusterId,
    pub name: String,
    pub description: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct App {
    pub id: AppId,
    pub name: String,
    pub cluster_id: ClusterId,
    pub git_url: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub id: BuildId,
    pub app_id: Option<AppId>,
    pub server_id: Option<ServerId>,
    pub status: BuildStatus,
    pub image_ref: Option<String>,
    pub message: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
    pub id: DeploymentId,
    pub app_id: AppId,
    pub build_id: Option<BuildId>,
    pub status: DeploymentStatus,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirewallPolicy {
    pub id: FirewallRuleId,
    pub namespace: String,
    pub src: String,
    pub dst: String,
    pub proto: Option<String>,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub severity: EventSeverity,
    pub subject: String,
    pub message: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

pub fn validate_name(field: &'static str, value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() {
        return Err(CoreError::Empty { field });
    }
    if value.len() > 128 {
        return Err(CoreError::Invalid {
            field,
            reason: "must be at most 128 characters".into(),
        });
    }
    Ok(())
}

pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

impl Server {
    pub fn new(name: impl Into<String>, address: impl Into<String>) -> Result<Self> {
        let name = name.into();
        let address = address.into();
        validate_name("server.name", &name)?;
        validate_name("server.address", &address)?;
        let ts = now();
        Ok(Self {
            id: ServerId::new(),
            name,
            address,
            mgmt_ip: None,
            status: ServerStatus::Unknown,
            coold_version: None,
            host_id: None,
            capabilities: vec![],
            last_seen_at: None,
            created_at: ts,
            updated_at: ts,
        })
    }
}

impl Cluster {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_name("cluster.name", &name)?;
        let ts = now();
        Ok(Self {
            id: ClusterId::new(),
            name,
            description: description.into(),
            created_at: ts,
            updated_at: ts,
        })
    }
}

impl Event {
    pub fn info(subject: impl Into<String>, message: impl Into<String>) -> Result<Self> {
        let subject = subject.into();
        validate_name("event.subject", &subject)?;
        Ok(Self {
            id: EventId::new(),
            severity: EventSeverity::Info,
            subject,
            message: message.into(),
            created_at: now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_empty_names() {
        assert!(matches!(
            validate_name("x", " "),
            Err(CoreError::Empty { field: "x" })
        ));
    }
    #[test]
    fn creates_server_defaults() {
        let s = Server::new("hetzner-1", "203.0.113.10").unwrap();
        assert_eq!(s.status, ServerStatus::Unknown);
        assert_eq!(s.name, "hetzner-1");
    }
    #[test]
    fn ids_are_uuid_v7_strings() {
        let id = ServerId::new().to_string();
        assert_eq!(id.len(), 36);
    }
    #[test]
    fn serializes_server_timestamps_as_rfc3339_strings() {
        let mut server = Server::new("node-a", "203.0.113.10").unwrap();
        server.last_seen_at = Some(now());

        let json = serde_json::to_value(&server).unwrap();

        assert!(
            json["created_at"].as_str().is_some(),
            "created_at must be a JSON string"
        );
        assert!(
            json["updated_at"].as_str().is_some(),
            "updated_at must be a JSON string"
        );
        assert!(
            json["last_seen_at"].as_str().is_some(),
            "last_seen_at must be a JSON string when present"
        );

        server.last_seen_at = None;
        let json = serde_json::to_value(&server).unwrap();
        assert!(json["last_seen_at"].is_null());
    }
}
