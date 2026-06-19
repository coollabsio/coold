use anyhow::{Context, Result};
use serde::Serialize;
use tracing::{debug, warn};

use coolify_proto::agent::v1::ResourceStatusUpdate;

#[derive(Clone)]
pub struct ResourceStatusPublisher {
    http: reqwest::Client,
    endpoint_url: Option<String>,
    token: Option<String>,
}

impl ResourceStatusPublisher {
    pub fn new(laravel_api_url: Option<String>, token: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint_url: laravel_api_url
                .map(|url| url.trim().trim_end_matches('/').to_string())
                .filter(|url| !url.is_empty())
                .map(|url| format!("{url}/api/v1/internal/flux/resource-status")),
            token: token
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty()),
        }
    }

    pub async fn publish(&self, update: ResourceStatusUpdate) {
        let Some(endpoint_url) = self.endpoint_url.as_deref() else {
            debug!("Laravel resource status endpoint disabled; dropping status update");
            return;
        };
        let Some(token) = self.token.as_deref() else {
            debug!("Laravel resource status token missing; dropping status update");
            return;
        };

        match self.publish_inner(endpoint_url, token, &update).await {
            Ok(()) => {}
            Err(e) => warn!(
                error = format!("{e:#}"),
                "publish resource status update failed"
            ),
        }
    }

    async fn publish_inner(
        &self,
        endpoint_url: &str,
        token: &str,
        update: &ResourceStatusUpdate,
    ) -> Result<()> {
        let payload = resource_status_payload(update);
        let response = self
            .http
            .post(endpoint_url)
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("POST Laravel resource status {endpoint_url}"))?;
        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Laravel resource status {endpoint_url} returned {status}: {body}");
        }

        Ok(())
    }
}

#[derive(Serialize)]
pub struct ResourceStatusPayload<'a> {
    pub resource_type: &'a str,
    pub host_id: &'a str,
    pub container_id: &'a str,
    pub container_name: &'a str,
    pub status: &'a str,
    pub status_message: &'a str,
}

pub fn resource_status_payload(update: &ResourceStatusUpdate) -> ResourceStatusPayload<'_> {
    ResourceStatusPayload {
        resource_type: &update.resource_type,
        host_id: &update.host_id,
        container_id: &update.container_id,
        container_name: &update.container_name,
        status: &update.status,
        status_message: &update.status_message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn serializes_status_payload_for_laravel_http() {
        let payload = serde_json::to_string(&resource_status_payload(&ResourceStatusUpdate {
            resource_type: "application".into(),
            host_id: "100.64.0.5".into(),
            container_id: "abc".into(),
            container_name: "web".into(),
            status: "running".into(),
            status_message: "Status received from coold through flux.".into(),
        }))
        .unwrap();
        let json: Value = serde_json::from_str(&payload).unwrap();

        assert_eq!(json["resource_type"], "application");
        assert_eq!(json["host_id"], "100.64.0.5");
        assert_eq!(json["container_id"], "abc");
        assert_eq!(json["container_name"], "web");
        assert_eq!(json["status"], "running");
        assert_eq!(
            json["status_message"],
            "Status received from coold through flux."
        );
    }
}
