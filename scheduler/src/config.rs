use std::path::PathBuf;

use clap::Parser;

pub const VERSION: &str = match option_env!("SCHEDULER_VERSION") {
    Some(v) => v,
    None => concat!(env!("CARGO_PKG_VERSION"), "-dev"),
};

#[derive(Debug, Clone, Parser)]
#[command(name = "scheduler", version = VERSION, about)]
pub struct Config {
    /// Address the gRPC server binds on (coold dials this). Build traffic
    /// rides on the same stream; there is no longer a separate builder
    /// listener.
    ///
    /// Required — must be a specific interface IP, typically the WireGuard
    /// mgmt IP (e.g. `10.42.0.1:6443`). Refuses to start on `0.0.0.0` / `::`
    /// unless `SCHEDULER_ALLOW_PUBLIC_BIND=1` is set (dev/test only — JWTs
    /// cross the wire in cleartext).
    #[arg(long, env = "SCHEDULER_GRPC_BIND")]
    pub grpc_bind: String,

    /// Path to the Unix domain socket the central-plane caller (Laravel)
    /// connects to. Access control = filesystem perms: the socket is
    /// `0660` if a group is configured, `0600` otherwise.
    #[arg(
        long,
        env = "SCHEDULER_UNIX_SOCKET_PATH",
        default_value = "/run/coolify/scheduler.sock"
    )]
    pub unix_socket_path: PathBuf,

    /// POSIX group name granted read/write on the socket. When unset, the
    /// socket stays mode `0600` and only the scheduler user can read/write —
    /// suitable for dev. Production deploys set this to the PHP-FPM group.
    #[arg(long, env = "SCHEDULER_UNIX_SOCKET_GROUP")]
    pub unix_socket_group: Option<String>,

    /// Cap on the number of in-flight + recently-landed pending entries.
    /// Bounds memory against a rogue local caller spamming dispatches.
    #[arg(long, env = "SCHEDULER_PENDING_MAX", default_value = "10000")]
    pub pending_max: usize,

    /// PEM-encoded RSA/EC public key used to verify per-host JWTs issued by Laravel.
    /// Path to the file (read at startup).
    #[arg(long, env = "SCHEDULER_JWT_PUBLIC_KEY_PATH", default_value = "/etc/coolify/jwt.pub")]
    pub jwt_public_key_path: std::path::PathBuf,

    /// Loaded public key bytes — populated at startup, not from CLI directly.
    #[clap(skip)]
    pub jwt_public_key: String,

    /// Log filter (e.g. `info`, `scheduler=debug`).
    #[arg(long, env = "SCHEDULER_LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Seconds before a pending dispatch request times out waiting for coold response.
    #[arg(long, env = "SCHEDULER_DISPATCH_TIMEOUT_SECS", default_value = "30")]
    pub dispatch_timeout_secs: u64,

    /// Stable scheduler identity reported to Laravel in cloud mode.
    #[arg(long, env = "SCHEDULER_ID")]
    pub scheduler_id: Option<String>,

    /// Public URL returned by Laravel's assignment endpoint for agents to dial.
    #[arg(long, env = "SCHEDULER_PUBLIC_URL")]
    pub scheduler_public_url: Option<String>,

    /// Private URL Laravel uses to dispatch to this scheduler in cloud mode.
    #[arg(long, env = "SCHEDULER_INTERNAL_URL")]
    pub scheduler_internal_url: Option<String>,

    /// Optional region label reported to Laravel.
    #[arg(long, env = "SCHEDULER_REGION")]
    pub scheduler_region: Option<String>,

    /// Laravel base URL for internal scheduler registry calls. When unset,
    /// registry reporting is disabled.
    #[arg(long, env = "SCHEDULER_LARAVEL_API_URL")]
    pub laravel_api_url: Option<String>,

    /// Bearer token for Laravel internal scheduler registry calls.
    #[arg(long, env = "SCHEDULER_LARAVEL_API_TOKEN")]
    pub laravel_api_token: Option<String>,

    /// Max long-lived agent streams this scheduler should be assigned.
    #[arg(long, env = "SCHEDULER_AGENT_CAPACITY", default_value = "10000")]
    pub agent_capacity: usize,

    /// Heartbeat interval for Laravel scheduler registry reporting.
    #[arg(long, env = "SCHEDULER_LARAVEL_HEARTBEAT_INTERVAL_SECS", default_value = "10")]
    pub laravel_heartbeat_interval_secs: u64,
}

impl Config {
    pub async fn load() -> anyhow::Result<Self> {
        let mut cfg = Self::parse();
        cfg.jwt_public_key = tokio::fs::read_to_string(&cfg.jwt_public_key_path)
            .await
            .map_err(|e| anyhow::anyhow!("read JWT pubkey {}: {e}", cfg.jwt_public_key_path.display()))?;
        Ok(cfg)
    }
}
