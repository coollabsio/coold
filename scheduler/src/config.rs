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
    #[arg(long, env = "SCHEDULER_GRPC_BIND", default_value = "0.0.0.0:6443")]
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
