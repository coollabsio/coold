use std::{net::SocketAddr, path::PathBuf, time::Duration};

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub db_path: PathBuf,
    pub auto_migrate: bool,
    pub public_https: bool,
    pub scheduler_socket_path: PathBuf,
    pub scheduler_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Self {
        let bind = std::env::var("COOLIFY_API_BIND")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| "127.0.0.1:3000".parse().unwrap());
        let db_path = std::env::var("COOLIFY_API_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("api.db"));
        let auto_migrate = std::env::var("COOLIFY_API_AUTO_MIGRATE")
            .map(|v| v != "0")
            .unwrap_or(true);
        let public_https = std::env::var("COOLIFY_API_PUBLIC_HTTPS")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false);
        let scheduler_socket_path = std::env::var("COOLIFY_SCHEDULER_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/run/coolify/scheduler.sock"));
        let scheduler_timeout = std::env::var("COOLIFY_SCHEDULER_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_secs(12));
        Self {
            bind,
            db_path,
            auto_migrate,
            public_https,
            scheduler_socket_path,
            scheduler_timeout,
        }
    }
}
