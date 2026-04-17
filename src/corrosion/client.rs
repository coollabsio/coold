use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::model::Endpoint;

/// One SQL statement with its positional bind parameters, accepted by the
/// Corrosion HTTP API's `/v1/transactions` endpoint as `[sql, [params...]]`.
#[derive(Debug, Clone)]
pub struct Statement {
    pub sql: String,
    pub params: Vec<Value>,
}

impl Statement {
    pub fn new(sql: impl Into<String>, params: Vec<Value>) -> Self {
        Self { sql: sql.into(), params }
    }

    fn to_json(&self) -> Value {
        json!([self.sql, self.params])
    }
}

#[derive(Clone)]
pub struct CorrosionClient {
    base_url: String,
    http: reqwest::Client,
}

impl CorrosionClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .http1_only()
            .build()
            .context("build reqwest client")?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http,
        })
    }

    pub async fn transaction(&self, statements: &[Statement]) -> Result<()> {
        if statements.is_empty() {
            return Ok(());
        }
        let body: Vec<Value> = statements.iter().map(Statement::to_json).collect();
        let url = format!("{}/v1/transactions", self.base_url);
        let res = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = res.status();
        let bytes = res.bytes().await.context("read transaction response")?;
        if !status.is_success() {
            return Err(anyhow!(
                "corrosion transaction failed: HTTP {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        Ok(())
    }

    /// Load the subset of `service_endpoints` owned by the given host.
    pub async fn snapshot_for_host(
        &self,
        host_mgmt_ip: &str,
    ) -> Result<HashMap<String, Endpoint>> {
        let url = format!("{}/v1/queries", self.base_url);
        let body = json!([
            "SELECT container_id, container_name, host_mgmt_ip, container_ip \
             FROM service_endpoints WHERE host_mgmt_ip = ?",
            [host_mgmt_ip]
        ]);
        let res = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = res.status();
        let bytes = res.bytes().await.context("read queries response")?;
        if !status.is_success() {
            return Err(anyhow!(
                "corrosion query failed: HTTP {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        parse_rows(&bytes)
    }
}

/// Corrosion `/v1/queries` returns newline-delimited JSON frames:
/// `{"columns":[...]}`, `{"row":[id, [values...]]}`, `{"eoq":{...}}`, etc.
fn parse_rows(bytes: &[u8]) -> Result<HashMap<String, Endpoint>> {
    let mut out = HashMap::new();
    for line in bytes.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(frame): std::result::Result<Frame, _> = serde_json::from_slice(line) else {
            continue;
        };
        if let Some(row) = frame.row {
            if row.len() != 2 {
                continue;
            }
            let Some(values) = row[1].as_array() else { continue };
            if values.len() < 4 {
                continue;
            }
            let endpoint = Endpoint {
                container_id: string_at(values, 0)?,
                container_name: string_at(values, 1)?,
                host_mgmt_ip: string_at(values, 2)?,
                container_ip: string_at(values, 3)?,
            };
            out.insert(endpoint.container_id.clone(), endpoint);
        }
    }
    Ok(out)
}

fn string_at(values: &[Value], idx: usize) -> Result<String> {
    values
        .get(idx)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("column {idx} missing or not a string"))
}

#[derive(Debug, Deserialize)]
struct Frame {
    #[serde(default)]
    row: Option<Vec<Value>>,
}
