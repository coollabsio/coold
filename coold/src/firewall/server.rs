//! Firewall API bind/serve loop. Spawned as a sibling of events/reconcile/dns
//! inside `sync::run`. Follows the same retry-on-bind-error pattern as the
//! DNS server so a temporarily unavailable mgmt IP (wg0 still coming up)
//! doesn't crash the daemon.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use secrecy::SecretString;
use tokio::{fs, time::sleep};
use tracing::{info, warn};

use crate::config::Config;

use super::{
    api::{router, ApiState},
    store::{FirewallStore, StoreConfig},
};

const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Entry point. When `api_bind` is `None` the task exits immediately with
/// `Ok(())` — lets test/agent-only deployments run without the API.
///
/// When `api_bind` is set, `api_token_file` becomes mandatory: refusing to
/// start without a token keeps the "no unauthenticated codepath" promise
/// checkable at config time rather than buried in handlers.
pub async fn run(config: Config) -> Result<()> {
    let Some(addr) = config.api_bind else {
        info!("COOLD_API_BIND unset; firewall API disabled");
        std::future::pending::<()>().await;
        return Ok(());
    };

    let token_path = config
        .api_token_file
        .clone()
        .context("api_bind set but api_token_file unset — refusing to start")?;
    let token = load_token(&token_path)
        .await
        .with_context(|| format!("load api token from {}", token_path.display()))?;

    let store = FirewallStore::new(StoreConfig {
        chain_name: config.chain_name.clone(),
        rules_path: config.rules_path.clone(),
        bridge_rules_path: config.bridge_rules_path.clone(),
    });

    // Best-effort chain bootstrap. ensure_chain is idempotent; this lets
    // the very first API call skip the chain-missing fallback path.
    if let Err(e) = store.ensure_chain().await {
        warn!(error = format!("{e:#}"), "initial ensure_chain failed; continuing");
    }
    if let Err(e) = store.ensure_bridge_chain().await {
        warn!(error = format!("{e:#}"), "initial ensure_bridge_chain failed; continuing");
    }

    let state = ApiState {
        store,
        token: Arc::new(SecretString::from(token)),
    };
    let app = router(state);

    let tls = match (&config.tls_cert, &config.tls_key) {
        (Some(cert), Some(key)) => Some((cert.clone(), key.clone())),
        (None, None) => None,
        _ => anyhow::bail!("tls_cert and tls_key must both be set or both unset"),
    };

    let mut backoff = BACKOFF_START;
    let mut attempt: u32 = 0;

    loop {
        match try_serve(addr, app.clone(), tls.clone()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempt = attempt.saturating_add(1);
                if attempt == 1 {
                    warn!(
                        bind = %addr,
                        error = format!("{e:#}"),
                        retry_in = ?backoff,
                        "firewall API bind/serve failed; retrying. \
                         Likely causes: wg0 not up yet (mgmt IP not assigned), \
                         port already bound, TLS material unreadable."
                    );
                }
                sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

async fn load_token(path: &std::path::Path) -> Result<String> {
    let bytes = fs::read(path).await?;
    let s = std::str::from_utf8(&bytes).context("token file is not utf-8")?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        anyhow::bail!("token file is empty");
    }
    Ok(trimmed.to_string())
}

async fn try_serve(
    addr: SocketAddr,
    app: axum::Router,
    tls: Option<(PathBuf, PathBuf)>,
) -> Result<()> {
    match tls {
        Some((cert, key)) => {
            let tls_cfg = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
                .await
                .with_context(|| {
                    format!(
                        "load TLS from cert={} key={}",
                        cert.display(),
                        key.display()
                    )
                })?;
            info!(bind = %addr, "firewall API listening (tls)");
            axum_server::bind_rustls(addr, tls_cfg)
                .serve(app.into_make_service())
                .await
                .context("axum-server serve tls")?;
        }
        None => {
            info!(bind = %addr, "firewall API listening (plain)");
            axum_server::bind(addr)
                .serve(app.into_make_service())
                .await
                .context("axum-server serve plain")?;
        }
    }
    Ok(())
}
