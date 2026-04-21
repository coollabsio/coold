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

    /// Broker gRPC address to dial (builder-stream listener, default :6444).
    #[arg(long, env = "BUILDER_BROKER_URL", default_value = "http://127.0.0.1:6444")]
    pub broker_url: String,

    /// Path to the pre-minted ES256 bearer JWT signed by central (aud="builder").
    #[arg(long, env = "BUILDER_JWT_PATH", default_value = "/etc/coolify/builder-jwt")]
    pub jwt_path: std::path::PathBuf,

    /// Loaded bearer token — populated at startup.
    #[clap(skip)]
    pub jwt: String,

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
        cfg.jwt = tokio::fs::read_to_string(&cfg.jwt_path)
            .await
            .map_err(|e| anyhow::anyhow!("read JWT from {}: {e}", cfg.jwt_path.display()))?;
        cfg.jwt = cfg.jwt.trim().to_owned();
        tokio::fs::create_dir_all(&cfg.work_dir).await?;
        Ok(cfg)
    }
}
