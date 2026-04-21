use clap::Parser;

pub const VERSION: &str = match option_env!("BROKER_VERSION") {
    Some(v) => v,
    None => concat!(env!("CARGO_PKG_VERSION"), "-dev"),
};

#[derive(Debug, Clone, Parser)]
#[command(name = "broker", version = VERSION, about)]
pub struct Config {
    /// Address the gRPC server binds on (coold dials this).
    #[arg(long, env = "BROKER_GRPC_BIND", default_value = "0.0.0.0:6443")]
    pub grpc_bind: String,

    /// Redis URL for the Laravel bridge.
    #[arg(long, env = "BROKER_REDIS_URL", default_value = "redis://127.0.0.1:6379")]
    pub redis_url: String,

    /// PEM-encoded RSA/EC public key used to verify per-host JWTs issued by Laravel.
    /// Path to the file (read at startup).
    #[arg(long, env = "BROKER_JWT_PUBLIC_KEY_PATH", default_value = "/etc/coolify/jwt.pub")]
    pub jwt_public_key_path: std::path::PathBuf,

    /// Loaded public key bytes — populated at startup, not from CLI directly.
    #[clap(skip)]
    pub jwt_public_key: String,

    /// Log filter (e.g. `info`, `broker=debug`).
    #[arg(long, env = "BROKER_LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Seconds before a pending dispatch request times out waiting for coold response.
    #[arg(long, env = "BROKER_DISPATCH_TIMEOUT_SECS", default_value = "30")]
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
