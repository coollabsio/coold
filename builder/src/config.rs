use clap::Parser;

pub const VERSION: &str = match option_env!("BUILDER_VERSION") {
    Some(v) => v,
    None => concat!(env!("CARGO_PKG_VERSION"), "-dev"),
};

#[derive(Debug, Clone, Parser)]
#[command(name = "builder", version = VERSION, about)]
pub struct Config {
    /// builder_id issued by central at enrollment. Used as JWT sub + stream identity.
    #[arg(long, env = "BUILDER_ID")]
    pub builder_id: String,

    /// Broker gRPC address to dial.
    #[arg(long, env = "BUILDER_BROKER_URL", default_value = "http://127.0.0.1:6443")]
    pub broker_url: String,

    /// PEM-encoded EC/RSA private key for signing the builder JWT.
    #[arg(long, env = "BUILDER_JWT_PRIVATE_KEY_PATH", default_value = "/etc/coolify/builder.key")]
    pub jwt_private_key_path: std::path::PathBuf,

    /// Loaded private key — populated at startup.
    #[clap(skip)]
    pub jwt_private_key: String,

    /// Directory for temporary build work dirs.
    #[arg(long, env = "BUILDER_WORK_DIR", default_value = "/var/lib/coolify-builder/work")]
    pub work_dir: std::path::PathBuf,

    /// Max concurrent builds.
    #[arg(long, env = "BUILDER_CAPACITY", default_value = "2")]
    pub capacity: u32,

    /// Log filter.
    #[arg(long, env = "BUILDER_LOG_LEVEL", default_value = "info")]
    pub log_level: String,
}

impl Config {
    pub async fn load() -> anyhow::Result<Self> {
        let mut cfg = Self::parse();
        cfg.jwt_private_key = tokio::fs::read_to_string(&cfg.jwt_private_key_path)
            .await
            .map_err(|e| anyhow::anyhow!("read JWT privkey {}: {e}", cfg.jwt_private_key_path.display()))?;
        tokio::fs::create_dir_all(&cfg.work_dir).await?;
        Ok(cfg)
    }
}
