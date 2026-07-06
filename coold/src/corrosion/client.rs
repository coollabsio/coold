use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

use crate::model::Endpoint;

/// One `service_endpoints` row as seen by a read-side resolver (S5): the
/// container IP together with the `host_mgmt_ip` that owns/gossiped it. The
/// owner is carried so callers can cross-check it against an expected host and
/// reject forged rows gossiped by a compromised mesh peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceEndpoint {
    pub container_ip: String,
    pub host_mgmt_ip: String,
}

/// One SQL statement with its positional bind parameters, accepted by the
/// Corrosion HTTP API's `/v1/transactions` endpoint as `[sql, [params...]]`.
#[derive(Debug, Clone)]
pub struct Statement {
    pub sql: String,
    pub params: Vec<Value>,
}

impl Statement {
    pub fn new(sql: impl Into<String>, params: Vec<Value>) -> Self {
        Self {
            sql: sql.into(),
            params,
        }
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
    pub async fn snapshot_for_host(&self, host_mgmt_ip: &str) -> Result<HashMap<String, Endpoint>> {
        let url = format!("{}/v1/queries", self.base_url);
        let body = json!([
            "SELECT container_id, container_name, namespace, host_mgmt_ip, container_ip, state, health \
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

    /// Return container IPs for `container_name` within `namespace` across
    /// the whole mesh (all hosts). Used by the embedded DNS resolver to
    /// answer `<name>.<namespace>.coolify.internal`.
    ///
    /// Filters: `state = 'running'` and `health IN ('healthy', 'unknown')`.
    /// Containers without a declared HEALTHCHECK report `health = 'unknown'`
    /// and must still be resolvable (same convention as k8s readinessProbe
    /// defaulting to ready when absent).

    pub async fn tables_json(&self, limit: u32) -> Result<String> {
        let limit = limit.clamp(1, 1000);
        let table_names = self
            .query_values(
                "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
                vec![],
            )
            .await?
            .into_iter()
            .filter_map(|row| row.first().and_then(|value| value.as_str()).map(str::to_string))
            .collect::<Vec<_>>();

        let mut tables = Vec::new();
        for name in table_names {
            let columns = self
                .query_values(
                    &format!("PRAGMA table_info({})", quote_identifier(&name)),
                    vec![],
                )
                .await?
                .into_iter()
                .filter_map(|row| {
                    row.get(1)
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                })
                .collect::<Vec<_>>();
            let rows = self
                .query_values(
                    &format!("SELECT * FROM {} LIMIT {limit}", quote_identifier(&name)),
                    vec![],
                )
                .await?
                .into_iter()
                .map(|values| values.into_iter().collect())
                .collect();

            tables.push(CorrosionTable {
                name,
                columns,
                rows,
            });
        }

        serde_json::to_string_pretty(&CorrosionTablesDump { limit, tables })
            .context("serialize Corrosion tables")
    }

    async fn query_values(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Vec<Value>>> {
        let url = format!("{}/v1/queries", self.base_url);
        let body = json!([sql, params]);
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

        parse_value_rows(&bytes)
    }

    /// Resolve healthy endpoints for `(container_name, namespace)` across the
    /// whole mesh, each tagged with its owning `host_mgmt_ip` (S5).
    ///
    /// Reads carry the owner so callers can bind a row to an expected host and
    /// reject forged rows. Selecting across all hosts is intentional — mesh
    /// service discovery is cross-host and a service may legitimately have
    /// replicas on several hosts.
    pub async fn query_endpoints_by_name(
        &self,
        container_name: &str,
        namespace: &str,
    ) -> Result<Vec<ServiceEndpoint>> {
        let url = format!("{}/v1/queries", self.base_url);
        let body = json!([
            "SELECT container_ip, host_mgmt_ip FROM service_endpoints \
             WHERE container_name = ? \
               AND namespace = ? \
               AND state = 'running' \
               AND health IN ('healthy', 'unknown')",
            [container_name, namespace]
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
        parse_endpoint_rows(&bytes)
    }

    /// Resolve endpoint IPs for `(container_name, namespace)` (S5).
    ///
    /// When `expected_owner` is `Some`, only rows owned by that host are
    /// returned — this is the cross-check that defeats a compromised peer
    /// gossiping a forged row for another host's service. When `None`
    /// (unrestricted cross-host discovery), all healthy IPs are returned, but a
    /// warning is logged if they span multiple owners so poisoning attempts are
    /// still observable.
    ///
    /// RESIDUAL TRUST: with `expected_owner == None` a forged row from a
    /// compromised mesh peer is still trusted. Closing that gap requires
    /// authenticating Corrosion gossip (owned by the coolify-cli agent); this
    /// read-side check only binds rows to a host when the expected owner is
    /// known and surfaces multi-owner results otherwise.
    pub async fn query_ips_by_name(
        &self,
        container_name: &str,
        namespace: &str,
    ) -> Result<Vec<String>> {
        self.query_ips_by_name_owned(container_name, namespace, None)
            .await
    }

    /// Owner-aware variant of [`Self::query_ips_by_name`].
    pub async fn query_ips_by_name_owned(
        &self,
        container_name: &str,
        namespace: &str,
        expected_owner: Option<&str>,
    ) -> Result<Vec<String>> {
        let rows = self
            .query_endpoints_by_name(container_name, namespace)
            .await?;
        Ok(select_endpoint_ips(
            &rows,
            expected_owner,
            container_name,
            namespace,
        ))
    }
}

/// Choose which endpoint IPs to trust from the gossiped rows (S5).
///
/// See [`CorrosionClient::query_ips_by_name`] for the trust model. Logs a
/// warning when rows owned by an unexpected host are dropped, or when an
/// unrestricted lookup spans multiple owners.
fn select_endpoint_ips(
    rows: &[ServiceEndpoint],
    expected_owner: Option<&str>,
    container_name: &str,
    namespace: &str,
) -> Vec<String> {
    match expected_owner {
        Some(owner) => {
            let (matching, forged): (Vec<_>, Vec<_>) =
                rows.iter().partition(|row| row.host_mgmt_ip == owner);
            if !forged.is_empty() {
                warn!(
                    container_name,
                    namespace,
                    expected_owner = owner,
                    dropped = forged.len(),
                    "dropped service_endpoints rows owned by an unexpected host (possible gossip poisoning)"
                );
            }
            matching
                .into_iter()
                .map(|row| row.container_ip.clone())
                .collect()
        }
        None => {
            let mut owners: Vec<&str> = rows.iter().map(|row| row.host_mgmt_ip.as_str()).collect();
            owners.sort_unstable();
            owners.dedup();
            if owners.len() > 1 {
                warn!(
                    container_name,
                    namespace,
                    owners = owners.len(),
                    "service resolves to endpoints on multiple hosts; \
                     trusting all (unauthenticated gossip — residual trust)"
                );
            }
            rows.iter().map(|row| row.container_ip.clone()).collect()
        }
    }
}

/// Parse `(container_ip, host_mgmt_ip)` rows from `/v1/queries` NDJSON (S5).
fn parse_endpoint_rows(bytes: &[u8]) -> Result<Vec<ServiceEndpoint>> {
    let mut out = Vec::new();
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
            let Some(values) = row[1].as_array() else {
                continue;
            };
            let (Some(ip), Some(owner)) = (
                values.first().and_then(|v| v.as_str()),
                values.get(1).and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            out.push(ServiceEndpoint {
                container_ip: ip.to_string(),
                host_mgmt_ip: owner.to_string(),
            });
        }
    }
    Ok(out)
}

/// Corrosion `/v1/queries` returns newline-delimited JSON frames:
/// `{"columns":[...]}`, `{"row":[id, [values...]]}`, `{"eoq":{...}}`, etc.
fn parse_value_rows(bytes: &[u8]) -> Result<Vec<Vec<Value>>> {
    let mut out = Vec::new();
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
            let Some(values) = row[1].as_array() else {
                continue;
            };
            out.push(values.clone());
        }
    }
    Ok(out)
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('\"', "\"\""))
}

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
            let Some(values) = row[1].as_array() else {
                continue;
            };
            if values.len() < 7 {
                continue;
            }
            let endpoint = Endpoint {
                container_id: string_at(values, 0)?,
                container_name: string_at(values, 1)?,
                namespace: string_at(values, 2)?,
                host_mgmt_ip: string_at(values, 3)?,
                container_ip: string_at(values, 4)?,
                state: string_at(values, 5)?,
                health: string_at(values, 6)?,
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

#[derive(Debug, Serialize)]
struct CorrosionTablesDump {
    limit: u32,
    tables: Vec<CorrosionTable>,
}

#[derive(Debug, Serialize)]
struct CorrosionTable {
    name: String,
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_corrosion_query_rows_as_values() {
        let rows = parse_value_rows(
            br#"{"columns":["container_name","container_ip"]}
{"row":[1,["coolify-v5-nginx","10.210.0.4"]]}
{"eoq":{}}
"#,
        )
        .unwrap();

        assert_eq!(
            rows,
            vec![vec![json!("coolify-v5-nginx"), json!("10.210.0.4")]]
        );
    }

    fn rows() -> Vec<ServiceEndpoint> {
        vec![
            ServiceEndpoint {
                container_ip: "10.210.0.2".into(),
                host_mgmt_ip: "100.64.0.5".into(),
            },
            ServiceEndpoint {
                container_ip: "10.210.0.9".into(),
                host_mgmt_ip: "100.64.0.9".into(), // forged / different owner
            },
        ]
    }

    #[test]
    fn parses_endpoint_rows_with_owner() {
        let parsed = parse_endpoint_rows(
            br#"{"columns":["container_ip","host_mgmt_ip"]}
{"row":[1,["10.210.0.4","100.64.0.5"]]}
{"eoq":{}}
"#,
        )
        .unwrap();
        assert_eq!(
            parsed,
            vec![ServiceEndpoint {
                container_ip: "10.210.0.4".into(),
                host_mgmt_ip: "100.64.0.5".into(),
            }]
        );
    }

    #[test]
    fn expected_owner_cross_check_drops_forged_rows() {
        // Firewall path (S5): the dst host is known, so a row gossiped by
        // another host (100.64.0.9) is dropped as a forgery.
        let ips = select_endpoint_ips(&rows(), Some("100.64.0.5"), "web", "default");
        assert_eq!(ips, vec!["10.210.0.2".to_string()]);
    }

    #[test]
    fn expected_owner_keeps_all_legitimately_owned_rows() {
        // Two healthy replicas, both truthfully owned by the expected host —
        // enforcement must keep every legitimate IP.
        let rows = vec![
            ServiceEndpoint {
                container_ip: "10.210.0.2".into(),
                host_mgmt_ip: "100.64.0.5".into(),
            },
            ServiceEndpoint {
                container_ip: "10.210.0.3".into(),
                host_mgmt_ip: "100.64.0.5".into(),
            },
        ];
        let ips = select_endpoint_ips(&rows, Some("100.64.0.5"), "web", "default");
        assert_eq!(
            ips,
            vec!["10.210.0.2".to_string(), "10.210.0.3".to_string()]
        );
    }

    #[test]
    fn no_expected_owner_keeps_all_for_cross_host_discovery() {
        // DNS path (S5): a cluster query may legitimately resolve to whichever
        // host runs the container, so with no expected owner every healthy IP
        // is returned (multi-owner spans are logged, not blocked).
        let ips = select_endpoint_ips(&rows(), None, "web", "default");
        assert_eq!(
            ips,
            vec!["10.210.0.2".to_string(), "10.210.0.9".to_string()]
        );
    }

    #[test]
    fn quotes_sqlite_identifiers() {
        assert_eq!(
            quote_identifier("service_endpoints"),
            "\"service_endpoints\""
        );
        assert_eq!(quote_identifier("weird\"table"), "\"weird\"\"table\"");
    }
}
