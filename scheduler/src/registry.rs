use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use tracing::{debug, warn};

use crate::{config::Config, state::Streams};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HeartbeatPayload {
    pub scheduler_id: String,
    pub public_url: String,
    pub internal_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    pub capacity: usize,
    pub connected_agents_count: usize,
}

impl HeartbeatPayload {
    pub fn new(
        scheduler_id: impl Into<String>,
        public_url: impl Into<String>,
        internal_url: impl Into<String>,
        region: Option<&str>,
        capacity: usize,
        connected_agents_count: usize,
    ) -> Self {
        Self {
            scheduler_id: scheduler_id.into(),
            public_url: public_url.into(),
            internal_url: internal_url.into(),
            region: region.map(str::to_owned),
            capacity,
            connected_agents_count,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentConnectionPayload {
    pub scheduler_id: String,
    pub host_id: String,
    pub capabilities: Vec<String>,
    pub builder_capacity: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coold_version: Option<String>,
}

impl AgentConnectionPayload {
    pub fn connected(
        scheduler_id: impl Into<String>,
        host_id: impl Into<String>,
        capabilities: Vec<String>,
        builder_capacity: u32,
        coold_version: Option<String>,
    ) -> Self {
        Self {
            scheduler_id: scheduler_id.into(),
            host_id: host_id.into(),
            capabilities,
            builder_capacity,
            coold_version,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DisconnectPayload {
    pub scheduler_id: String,
    pub host_id: String,
    pub reason: String,
}

#[derive(Clone)]
pub struct RegistryClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
    scheduler_id: String,
    public_url: String,
    internal_url: String,
    region: Option<String>,
    capacity: usize,
}

impl RegistryClient {
    pub fn from_config(config: &Config) -> Option<Self> {
        let base_url = config
            .laravel_api_url
            .as_ref()?
            .trim()
            .trim_end_matches('/')
            .to_string();
        let token = config.laravel_api_token.as_ref()?.trim().to_string();
        let scheduler_id = config.scheduler_id.as_ref()?.trim().to_string();
        let public_url = config.scheduler_public_url.as_ref()?.trim().to_string();
        let internal_url = config.scheduler_internal_url.as_ref()?.trim().to_string();
        if base_url.is_empty()
            || token.is_empty()
            || scheduler_id.is_empty()
            || public_url.is_empty()
            || internal_url.is_empty()
        {
            return None;
        }
        Some(Self {
            http: reqwest::Client::new(),
            base_url,
            token,
            scheduler_id,
            public_url,
            internal_url,
            region: config.scheduler_region.clone(),
            capacity: config.agent_capacity,
        })
    }

    pub fn heartbeat_payload(&self, connected_agents_count: usize) -> HeartbeatPayload {
        HeartbeatPayload::new(
            &self.scheduler_id,
            &self.public_url,
            &self.internal_url,
            self.region.as_deref(),
            self.capacity,
            connected_agents_count,
        )
    }

    pub async fn heartbeat(&self, connected_agents_count: usize) -> Result<()> {
        self.post(
            "/api/v1/internal/schedulers/heartbeat",
            &self.heartbeat_payload(connected_agents_count),
        )
        .await
    }

    pub async fn upsert_connection(
        &self,
        host_id: &str,
        capabilities: Vec<String>,
        builder_capacity: u32,
        coold_version: Option<String>,
    ) -> Result<()> {
        let payload = AgentConnectionPayload::connected(
            &self.scheduler_id,
            host_id,
            capabilities,
            builder_capacity,
            coold_version,
        );
        self.post("/api/v1/internal/agent-connections/upsert", &payload)
            .await
    }

    pub async fn disconnect(&self, host_id: &str, reason: &str) -> Result<()> {
        let payload = DisconnectPayload {
            scheduler_id: self.scheduler_id.clone(),
            host_id: host_id.to_owned(),
            reason: reason.to_owned(),
        };
        self.post("/api/v1/internal/agent-connections/disconnect", &payload)
            .await
    }

    async fn post<T: Serialize + ?Sized>(&self, path: &str, payload: &T) -> Result<()> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(payload)
            .send()
            .await
            .with_context(|| format!("POST Laravel registry {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Laravel registry {url} returned {status}: {body}");
        }
        Ok(())
    }
}

pub async fn heartbeat_loop(config: Config, streams: Streams) -> Result<()> {
    let Some(client) = RegistryClient::from_config(&config) else {
        debug!("Laravel registry disabled; heartbeat loop sleeping forever");
        std::future::pending::<()>().await;
        return Ok(());
    };

    let mut ticker =
        tokio::time::interval(Duration::from_secs(config.laravel_heartbeat_interval_secs));
    loop {
        ticker.tick().await;
        if let Err(e) = client.heartbeat(streams.len()).await {
            warn!(
                error = format!("{e:#}"),
                "Laravel scheduler heartbeat failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_payload_includes_scheduler_identity_and_capacity() {
        let payload = HeartbeatPayload::new(
            "sched-eu-1",
            "https://sched-eu-1.agent.coolify.io",
            "http://sched-eu-1.internal:6444",
            Some("eu"),
            10_000,
            42,
        );

        assert_eq!(payload.scheduler_id, "sched-eu-1");
        assert_eq!(payload.public_url, "https://sched-eu-1.agent.coolify.io");
        assert_eq!(payload.internal_url, "http://sched-eu-1.internal:6444");
        assert_eq!(payload.region.as_deref(), Some("eu"));
        assert_eq!(payload.capacity, 10_000);
        assert_eq!(payload.connected_agents_count, 42);
    }

    #[test]
    fn connection_payload_carries_stream_ownership() {
        let payload = AgentConnectionPayload::connected(
            "sched-eu-1",
            "100.64.0.5",
            vec!["coold".into(), "builder".into()],
            2,
            Some("0.1.0".into()),
        );

        assert_eq!(payload.scheduler_id, "sched-eu-1");
        assert_eq!(payload.host_id, "100.64.0.5");
        assert_eq!(payload.capabilities, vec!["coold", "builder"]);
        assert_eq!(payload.builder_capacity, 2);
        assert_eq!(payload.coold_version.as_deref(), Some("0.1.0"));
    }
}
