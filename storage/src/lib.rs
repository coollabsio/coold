use async_trait::async_trait;
use coolify_core::{
    Build, BuildId, BuildStatus, Cluster, ClusterId, Event, EventId, EventSeverity, Server,
    ServerId, ServerStatus,
};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    Row, SqlitePool,
};
use std::{path::Path, str::FromStr, time::Duration};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, StorageError>;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("not found")]
    NotFound,
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Time(#[from] time::error::Parse),
    #[error(transparent)]
    Uuid(#[from] uuid::Error),
}

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}
impl Store {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn connect(path: impl AsRef<Path>) -> Result<Self> {
        let opts =
            SqliteConnectOptions::from_str(&format!("sqlite://{}", path.as_ref().display()))?
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal)
                .busy_timeout(Duration::from_secs(5))
                .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;
        Ok(Self::new(pool))
    }

    pub async fn memory() -> Result<Self> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        Ok(Self::new(pool))
    }

    pub async fn migrate(&self) -> Result<()> {
        MIGRATOR.run(&self.pool).await.map_err(sqlx::Error::from)?;
        Ok(())
    }

    pub async fn migration_versions(&self) -> Result<Vec<i64>> {
        let rows = sqlx::query("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<i64, _>("version"))
            .collect())
    }
}

#[async_trait]
pub trait ServerRepository {
    async fn upsert_server(&self, server: &Server) -> Result<()>;
    async fn list_servers(&self) -> Result<Vec<Server>>;
    async fn get_server(&self, id: &ServerId) -> Result<Server>;
    async fn get_server_by_host_id(&self, host_id: &str) -> Result<Option<Server>>;
}
#[async_trait]
pub trait ClusterRepository {
    async fn upsert_cluster(&self, cluster: &Cluster) -> Result<()>;
    async fn list_clusters(&self) -> Result<Vec<Cluster>>;
}
#[async_trait]
pub trait EventRepository {
    async fn append_event(&self, event: &Event) -> Result<()>;
    async fn list_events(&self, limit: u32) -> Result<Vec<Event>>;
}
#[async_trait]
pub trait BuildRepository {
    async fn upsert_build(&self, build: &Build) -> Result<()>;
    async fn list_builds(&self, limit: u32) -> Result<Vec<Build>>;
}

#[async_trait]
impl ServerRepository for Store {
    async fn upsert_server(&self, s: &Server) -> Result<()> {
        sqlx::query("INSERT INTO servers (id,name,address,mgmt_ip,status,coold_version,host_id,capabilities,last_seen_at,created_at,updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name,address=excluded.address,mgmt_ip=excluded.mgmt_ip,status=excluded.status,coold_version=excluded.coold_version,host_id=excluded.host_id,capabilities=excluded.capabilities,last_seen_at=excluded.last_seen_at,updated_at=excluded.updated_at")
            .bind(s.id.to_string()).bind(&s.name).bind(&s.address).bind(&s.mgmt_ip).bind(status_to_str(s.status)).bind(&s.coold_version).bind(&s.host_id).bind(serde_json::to_string(&s.capabilities).unwrap_or_else(|_| "[]".into())).bind(s.last_seen_at.map(fmt_ts)).bind(fmt_ts(s.created_at)).bind(fmt_ts(s.updated_at)).execute(&self.pool).await?;
        Ok(())
    }
    async fn list_servers(&self) -> Result<Vec<Server>> {
        let rows = sqlx::query("SELECT * FROM servers ORDER BY name ASC")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_server).collect()
    }
    async fn get_server(&self, id: &ServerId) -> Result<Server> {
        let row = sqlx::query("SELECT * FROM servers WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_server)
            .transpose()?
            .ok_or(StorageError::NotFound)
    }

    async fn get_server_by_host_id(&self, host_id: &str) -> Result<Option<Server>> {
        let row = sqlx::query("SELECT * FROM servers WHERE host_id=?")
            .bind(host_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_server).transpose()
    }
}

#[async_trait]
impl ClusterRepository for Store {
    async fn upsert_cluster(&self, c: &Cluster) -> Result<()> {
        sqlx::query("INSERT INTO clusters (id,name,description,created_at,updated_at) VALUES (?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name,description=excluded.description,updated_at=excluded.updated_at")
            .bind(c.id.to_string()).bind(&c.name).bind(&c.description).bind(fmt_ts(c.created_at)).bind(fmt_ts(c.updated_at)).execute(&self.pool).await?;
        Ok(())
    }
    async fn list_clusters(&self) -> Result<Vec<Cluster>> {
        let rows = sqlx::query("SELECT * FROM clusters ORDER BY name ASC")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_cluster).collect()
    }
}

#[async_trait]
impl EventRepository for Store {
    async fn append_event(&self, e: &Event) -> Result<()> {
        sqlx::query(
            "INSERT INTO events (id,severity,subject,message,created_at) VALUES (?,?,?,?,?)",
        )
        .bind(e.id.to_string())
        .bind(severity_to_str(e.severity))
        .bind(&e.subject)
        .bind(&e.message)
        .bind(fmt_ts(e.created_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    async fn list_events(&self, limit: u32) -> Result<Vec<Event>> {
        let rows = sqlx::query("SELECT * FROM events ORDER BY created_at DESC LIMIT ?")
            .bind(limit.min(500))
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_event).collect()
    }
}

#[async_trait]
impl BuildRepository for Store {
    async fn upsert_build(&self, b: &Build) -> Result<()> {
        sqlx::query("INSERT INTO builds (id,app_id,server_id,status,image_ref,message,created_at,updated_at) VALUES (?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET status=excluded.status,image_ref=excluded.image_ref,message=excluded.message,updated_at=excluded.updated_at")
            .bind(b.id.to_string()).bind(b.app_id.as_ref().map(ToString::to_string)).bind(b.server_id.as_ref().map(ToString::to_string)).bind(build_status_to_str(b.status)).bind(&b.image_ref).bind(&b.message).bind(fmt_ts(b.created_at)).bind(fmt_ts(b.updated_at)).execute(&self.pool).await?;
        Ok(())
    }
    async fn list_builds(&self, limit: u32) -> Result<Vec<Build>> {
        let rows = sqlx::query("SELECT * FROM builds ORDER BY created_at DESC LIMIT ?")
            .bind(limit.min(500))
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_build).collect()
    }
}

fn fmt_ts(ts: OffsetDateTime) -> String {
    ts.format(&time::format_description::well_known::Rfc3339)
        .expect("format rfc3339")
}
fn parse_ts(s: String) -> Result<OffsetDateTime> {
    Ok(OffsetDateTime::parse(
        &s,
        &time::format_description::well_known::Rfc3339,
    )?)
}
fn parse_uuid(s: String) -> Result<Uuid> {
    Ok(Uuid::parse_str(&s)?)
}
fn status_to_str(s: ServerStatus) -> &'static str {
    match s {
        ServerStatus::Unknown => "unknown",
        ServerStatus::Provisioning => "provisioning",
        ServerStatus::Online => "online",
        ServerStatus::Offline => "offline",
        ServerStatus::Error => "error",
    }
}
fn str_to_status(s: &str) -> ServerStatus {
    match s {
        "provisioning" => ServerStatus::Provisioning,
        "online" => ServerStatus::Online,
        "offline" => ServerStatus::Offline,
        "error" => ServerStatus::Error,
        _ => ServerStatus::Unknown,
    }
}
fn severity_to_str(s: EventSeverity) -> &'static str {
    match s {
        EventSeverity::Info => "info",
        EventSeverity::Warning => "warning",
        EventSeverity::Error => "error",
    }
}
fn str_to_severity(s: &str) -> EventSeverity {
    match s {
        "warning" => EventSeverity::Warning,
        "error" => EventSeverity::Error,
        _ => EventSeverity::Info,
    }
}
fn build_status_to_str(s: BuildStatus) -> &'static str {
    match s {
        BuildStatus::Queued => "queued",
        BuildStatus::Running => "running",
        BuildStatus::Succeeded => "succeeded",
        BuildStatus::Failed => "failed",
        BuildStatus::Cancelled => "cancelled",
    }
}
fn str_to_build_status(s: &str) -> BuildStatus {
    match s {
        "running" => BuildStatus::Running,
        "succeeded" => BuildStatus::Succeeded,
        "failed" => BuildStatus::Failed,
        "cancelled" => BuildStatus::Cancelled,
        _ => BuildStatus::Queued,
    }
}

fn row_server(r: sqlx::sqlite::SqliteRow) -> Result<Server> {
    let capabilities_json: String = r.get("capabilities");
    let capabilities = serde_json::from_str(&capabilities_json).unwrap_or_default();
    let last_seen_at: Option<String> = r.get("last_seen_at");
    Ok(Server {
        id: ServerId(parse_uuid(r.get("id"))?),
        name: r.get("name"),
        address: r.get("address"),
        mgmt_ip: r.get("mgmt_ip"),
        status: str_to_status(r.get::<String, _>("status").as_str()),
        coold_version: r.get("coold_version"),
        host_id: r.get("host_id"),
        capabilities,
        last_seen_at: last_seen_at.map(parse_ts).transpose()?,
        created_at: parse_ts(r.get("created_at"))?,
        updated_at: parse_ts(r.get("updated_at"))?,
    })
}
fn row_cluster(r: sqlx::sqlite::SqliteRow) -> Result<Cluster> {
    Ok(Cluster {
        id: ClusterId(parse_uuid(r.get("id"))?),
        name: r.get("name"),
        description: r.get("description"),
        created_at: parse_ts(r.get("created_at"))?,
        updated_at: parse_ts(r.get("updated_at"))?,
    })
}
fn row_event(r: sqlx::sqlite::SqliteRow) -> Result<Event> {
    Ok(Event {
        id: EventId(parse_uuid(r.get("id"))?),
        severity: str_to_severity(r.get::<String, _>("severity").as_str()),
        subject: r.get("subject"),
        message: r.get("message"),
        created_at: parse_ts(r.get("created_at"))?,
    })
}
fn row_build(r: sqlx::sqlite::SqliteRow) -> Result<Build> {
    Ok(Build {
        id: BuildId(parse_uuid(r.get("id"))?),
        app_id: None,
        server_id: None,
        status: str_to_build_status(r.get::<String, _>("status").as_str()),
        image_ref: r.get("image_ref"),
        message: r.get("message"),
        created_at: parse_ts(r.get("created_at"))?,
        updated_at: parse_ts(r.get("updated_at"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use coolify_core::{Cluster, Event, Server};
    #[tokio::test]
    async fn migrates_and_round_trips_records() {
        let store = Store::memory().await.unwrap();
        store.migrate().await.unwrap();
        assert_eq!(
            store.migration_versions().await.unwrap(),
            vec![202605270001, 202605270002]
        );
        let mut server = Server::new("node-a", "203.0.113.10").unwrap();
        server.host_id = Some("host-a".into());
        server.capabilities = vec!["coold".into(), "builder".into()];
        server.last_seen_at = Some(coolify_core::now());
        store.upsert_server(&server).await.unwrap();
        let stored = store.get_server(&server.id).await.unwrap();
        assert_eq!(stored.name, "node-a");
        assert_eq!(stored.host_id.as_deref(), Some("host-a"));
        assert_eq!(stored.capabilities, vec!["coold", "builder"]);
        assert!(stored.last_seen_at.is_some());
        assert_eq!(
            store
                .get_server_by_host_id("host-a")
                .await
                .unwrap()
                .unwrap()
                .id,
            server.id
        );
        assert!(store
            .get_server_by_host_id("missing")
            .await
            .unwrap()
            .is_none());
        let cluster = Cluster::new("prod", "main cluster").unwrap();
        store.upsert_cluster(&cluster).await.unwrap();
        assert_eq!(store.list_clusters().await.unwrap().len(), 1);
        let event = Event::info("bootstrap", "created cluster").unwrap();
        store.append_event(&event).await.unwrap();
        assert_eq!(store.list_events(10).await.unwrap()[0].subject, "bootstrap");
    }
}
