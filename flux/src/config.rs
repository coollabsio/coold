use std::collections::HashMap;
use std::path::PathBuf;

use clap::Parser;

pub const VERSION: &str = match option_env!("COOLIFY_FLUX_VERSION") {
    Some(v) => v,
    None => concat!(env!("CARGO_PKG_VERSION"), "-dev"),
};

pub const FLUX_LOG_FILE_PATH: &str = "/var/www/html/storage/logs/flux.log";

#[derive(Debug, Clone, Parser)]
#[command(name = "flux", version = VERSION, about)]
pub struct Config {
    /// Address the gRPC server binds on (coold dials this).
    ///
    /// Required — must be a specific interface IP, typically the WireGuard
    /// mgmt IP (e.g. `10.42.0.1:6443`). Refuses to start on `0.0.0.0` / `::`
    /// unless `COOLIFY_FLUX_ALLOW_PUBLIC_BIND=1` is set (dev/test only — JWTs
    /// cross the wire in cleartext).
    #[arg(long, env = "COOLIFY_FLUX_GRPC_BIND")]
    pub grpc_bind: String,

    /// Path to the Unix domain socket the central-plane caller (Laravel)
    /// connects to. Access control = filesystem perms: the socket is
    /// `0660` if a group is configured, `0600` otherwise.
    #[arg(
        long,
        env = "COOLIFY_FLUX_UNIX_SOCKET_PATH",
        default_value = "/run/coolify/flux.sock"
    )]
    pub unix_socket_path: PathBuf,

    /// POSIX group name granted read/write on the socket. When unset, the
    /// socket stays mode `0600` and only the flux user can read/write —
    /// suitable for dev. Production deploys set this to the PHP-FPM group.
    #[arg(long, env = "COOLIFY_FLUX_UNIX_SOCKET_GROUP")]
    pub unix_socket_group: Option<String>,

    /// Cap on the number of in-flight + recently-landed pending entries.
    /// Bounds memory against a rogue local caller spamming dispatches.
    #[arg(long, env = "COOLIFY_FLUX_PENDING_MAX", default_value = "10000")]
    pub pending_max: usize,

    /// PEM-encoded RSA/EC public key used to verify per-host JWTs issued by Laravel.
    /// Path to the file (read at startup).
    #[arg(
        long,
        env = "COOLIFY_FLUX_JWT_PUBLIC_KEY_PATH",
        default_value = "/etc/coolify/jwt.pub"
    )]
    pub jwt_public_key_path: std::path::PathBuf,

    /// Loaded public key bytes — populated at startup, not from CLI directly.
    /// This is the default key (kid `flux-default`); see `jwt_keys_dir` for
    /// key rotation (S3).
    #[clap(skip)]
    pub jwt_public_key: String,

    /// Optional directory of additional JWT verification keys for rotation
    /// (S3). Each file must be named `<kid>.pub` (PEM). At verify time the
    /// token header `kid` selects the matching key; a token with no `kid`
    /// (or `kid=flux-default`) falls back to `jwt_public_key`. Unknown `kid`
    /// is rejected.
    #[arg(long, env = "COOLIFY_FLUX_JWT_KEYS_DIR")]
    pub jwt_keys_dir: Option<PathBuf>,

    /// Additional verification keys keyed by `kid` — populated at startup from
    /// `jwt_keys_dir`, not from CLI directly.
    #[clap(skip)]
    pub jwt_additional_keys: HashMap<String, String>,

    /// #2: require the transport/host binding to hold at stream connect.
    /// Default TRUE. When a token's `sub` (the host it was minted for) does not
    /// match the connecting host's independently-observed identity (the gRPC
    /// transport peer IP, else the Hello-advertised `host_mgmt_ip`), the stream
    /// is rejected. If the binding signal is genuinely unavailable (e.g. peer
    /// addr can't be determined) the connection degrades gracefully with a
    /// warning rather than breaking the mesh. Set
    /// `COOLIFY_FLUX_REQUIRE_HOST_BINDING=0` to warn-only (dev/rollback).
    /// Populated at startup from the env var, not from CLI directly.
    #[clap(skip)]
    pub require_host_binding: bool,

    /// #2: require a non-empty `team_id` (tenant) claim in the host JWT.
    /// Default TRUE — every stream is scoped to a tenant. Set
    /// `COOLIFY_FLUX_REQUIRE_TEAM_ID=0` to tolerate legacy tokens during dev.
    /// Populated at startup from the env var, not from CLI directly.
    #[clap(skip)]
    pub require_team_id: bool,

    /// S2: gate expansion of wildcard capability profiles
    /// (`*`, `host-agent:dev`, `host-agent:default`) to the full advertised
    /// primitive set. FALSE by default (secure-by-default): a `caps` claim is
    /// authoritative and flux authorizes only the intersection of the JWT
    /// `caps` with the host's advertised primitives — wildcard profile strings
    /// grant nothing. Set to TRUE (`COOLIFY_FLUX_ALLOW_WILDCARD_CAPABILITIES=1`)
    /// as a dev/rollback escape hatch that restores expand-to-all behavior.
    /// Populated at startup from the env var, not from CLI directly.
    #[clap(skip)]
    pub allow_wildcard_capabilities: bool,

    /// #4: maximum accepted remaining lifetime (`exp - now`) of a host JWT at
    /// connect, in seconds. A token whose remaining lifetime exceeds this clamp
    /// is rejected at connect so a misissued long-lived token can't authorize a
    /// stream indefinitely. `0` disables the clamp.
    #[arg(
        long,
        env = "COOLIFY_FLUX_MAX_TOKEN_LIFETIME_SECS",
        default_value = "3600"
    )]
    pub max_token_lifetime_secs: u64,

    /// #3: on-disk JSON file persisting the JWT `jti` revocation denylist so
    /// revocations survive a flux restart. Loaded at startup (expired entries
    /// pruned); written on every revoke/unrevoke.
    ///
    /// Default lives under the Laravel storage dir (same base as the log file),
    /// which is writable by the `www-data` user flux runs as inside the Coolify
    /// container — unlike `/var/lib/coolify`, which is root-owned there and made
    /// revocation persistence silently fail.
    #[arg(
        long,
        env = "COOLIFY_FLUX_REVOCATION_PATH",
        default_value = "/var/www/html/storage/app/flux/revocations.json"
    )]
    pub revocation_path: PathBuf,

    /// S1 (defense-in-depth, default OFF): PEM certificate chain for optional
    /// TLS on the gRPC listener. TLS is enabled only when BOTH this and
    /// `tls_key_path` are set; otherwise the listener stays plaintext (the
    /// WireGuard mesh is the confidentiality boundary today).
    #[arg(long, env = "COOLIFY_FLUX_TLS_CERT_PATH")]
    pub tls_cert_path: Option<PathBuf>,

    /// S1: PEM private key paired with `tls_cert_path`. See `tls_cert_path`.
    #[arg(long, env = "COOLIFY_FLUX_TLS_KEY_PATH")]
    pub tls_key_path: Option<PathBuf>,

    /// Log filter (e.g. `info`, `flux=debug`).
    #[arg(long, env = "COOLIFY_FLUX_LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Seconds before a pending dispatch request times out waiting for coold response.
    #[arg(long, env = "COOLIFY_FLUX_DISPATCH_TIMEOUT_SECS", default_value = "30")]
    pub dispatch_timeout_secs: u64,

    /// Stable flux identity reported to Laravel in cloud mode.
    #[arg(long, env = "COOLIFY_FLUX_ID")]
    pub flux_id: Option<String>,

    /// Public URL returned by Laravel's assignment endpoint for agents to dial.
    #[arg(long, env = "COOLIFY_FLUX_PUBLIC_URL")]
    pub flux_public_url: Option<String>,

    /// Private URL Laravel uses to dispatch to this flux in cloud mode.
    #[arg(long, env = "COOLIFY_FLUX_INTERNAL_URL")]
    pub flux_internal_url: Option<String>,

    /// Optional region label reported to Laravel.
    #[arg(long, env = "COOLIFY_FLUX_REGION")]
    pub flux_region: Option<String>,

    /// Laravel base URL for internal flux registry calls. When unset,
    /// registry reporting is disabled.
    #[arg(long, env = "COOLIFY_FLUX_LARAVEL_API_URL")]
    pub laravel_api_url: Option<String>,

    /// Bearer token for Laravel internal flux registry calls.
    #[arg(long, env = "COOLIFY_FLUX_LARAVEL_API_TOKEN")]
    pub laravel_api_token: Option<String>,

    /// Max long-lived agent streams this flux should be assigned.
    #[arg(long, env = "COOLIFY_FLUX_AGENT_CAPACITY", default_value = "10000")]
    pub agent_capacity: usize,

    /// Heartbeat interval for Laravel flux registry reporting.
    #[arg(
        long,
        env = "COOLIFY_FLUX_LARAVEL_HEARTBEAT_INTERVAL_SECS",
        default_value = "10"
    )]
    pub laravel_heartbeat_interval_secs: u64,

    /// Seconds between Flux ping frames sent to each connected coold stream.
    #[arg(
        long,
        env = "COOLIFY_FLUX_COOLD_PING_INTERVAL_SECS",
        default_value = "10"
    )]
    pub coold_ping_interval_secs: u64,

    /// Seconds without a coold pong before Flux marks that server unreachable.
    #[arg(
        long,
        env = "COOLIFY_FLUX_COOLD_PONG_TIMEOUT_SECS",
        default_value = "45"
    )]
    pub coold_pong_timeout_secs: u64,
}

impl Config {
    pub async fn load() -> anyhow::Result<Self> {
        let mut cfg = Self::parse();
        cfg.jwt_public_key = tokio::fs::read_to_string(&cfg.jwt_public_key_path)
            .await
            .map_err(|e| {
                anyhow::anyhow!("read JWT pubkey {}: {e}", cfg.jwt_public_key_path.display())
            })?;
        cfg.jwt_additional_keys = load_jwt_keys_dir(cfg.jwt_keys_dir.as_deref()).await?;
        cfg.allow_wildcard_capabilities = env_flag("COOLIFY_FLUX_ALLOW_WILDCARD_CAPABILITIES");
        cfg.require_host_binding = env_flag_default_true("COOLIFY_FLUX_REQUIRE_HOST_BINDING");
        cfg.require_team_id = env_flag_default_true("COOLIFY_FLUX_REQUIRE_TEAM_ID");
        Ok(cfg)
    }
}

/// Read `<kid>.pub` PEM files from the optional key-rotation directory (S3).
/// The file stem is the `kid`. Non-`.pub` files are ignored.
async fn load_jwt_keys_dir(
    dir: Option<&std::path::Path>,
) -> anyhow::Result<HashMap<String, String>> {
    let mut keys = HashMap::new();
    let Some(dir) = dir else {
        return Ok(keys);
    };
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| anyhow::anyhow!("read JWT keys dir {}: {e}", dir.display()))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| anyhow::anyhow!("iterate JWT keys dir {}: {e}", dir.display()))?
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pub") {
            continue;
        }
        let Some(kid) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        let pem = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| anyhow::anyhow!("read JWT key {}: {e}", path.display()))?;
        keys.insert(kid, pem);
    }
    Ok(keys)
}

/// Interpret an env var as a boolean flag. `1`/`true`/`yes`/`on`
/// (case-insensitive) are truthy; anything else (incl. unset) is false.
fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Interpret an env var as a boolean flag that defaults to TRUE when unset.
/// Only an explicit falsy value (`0`/`false`/`no`/`off`, case-insensitive)
/// turns it off; anything else — including unset — is true. Used for
/// secure-by-default toggles that must stay on unless deliberately disabled.
fn env_flag_default_true(name: &str) -> bool {
    flag_default_true(std::env::var(name).ok().as_deref())
}

/// Pure core of [`env_flag_default_true`] — testable without touching the
/// process environment.
fn flag_default_true(value: Option<&str>) -> bool {
    match value {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{flag_default_true, Config, FLUX_LOG_FILE_PATH};
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn default_true_flag_is_on_when_unset() {
        assert!(flag_default_true(None));
    }

    #[test]
    fn default_true_flag_only_off_for_explicit_falsy() {
        for off in ["0", "false", "FALSE", "no", "off", " off "] {
            assert!(!flag_default_true(Some(off)), "{off:?} should be off");
        }
        for on in ["1", "true", "yes", "on", "anything"] {
            assert!(flag_default_true(Some(on)), "{on:?} should be on");
        }
    }

    #[test]
    fn defaults_flux_file_log_to_laravel_storage_logs() {
        assert_eq!(
            PathBuf::from(FLUX_LOG_FILE_PATH),
            PathBuf::from("/var/www/html/storage/logs/flux.log")
        );
    }

    #[test]
    fn defaults_coold_heartbeat_timing() {
        let config = Config::parse_from(["flux", "--grpc-bind", "127.0.0.1:6443"]);

        assert_eq!(config.coold_ping_interval_secs, 10);
        assert_eq!(config.coold_pong_timeout_secs, 45);
    }

    #[test]
    fn rejects_flux_file_log_path_override() {
        let result = Config::try_parse_from([
            "flux",
            "--grpc-bind",
            "127.0.0.1:6443",
            "--log-file-path",
            "/tmp/custom-flux.log",
        ]);

        assert!(result.is_err());
    }
}
