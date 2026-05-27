mod api;
mod cli;
mod config;
mod scheduler_client;
mod state;
mod static_files;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command, DbCommand};
use config::Config;
use coolify_storage::Store;
use state::AppState;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.debug);
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(Config::from_env()).await,
        Command::Healthcheck { url, timeout_ms } => cli::healthcheck(url, timeout_ms).await,
        Command::Db { command } => match command {
            DbCommand::Migrate => {
                let cfg = Config::from_env();
                let store = Store::connect(&cfg.db_path).await?;
                store.migrate().await?;
                println!("migrations applied: {}", cfg.db_path.display());
                Ok(())
            }
            DbCommand::Info => {
                let cfg = Config::from_env();
                let store = Store::connect(&cfg.db_path).await?;
                store.migrate().await?;
                let versions = store.migration_versions().await?;
                println!(
                    "db_path={} applied_migrations={versions:?}",
                    cfg.db_path.display()
                );
                Ok(())
            }
        },
    }
}

fn init_tracing(debug: bool) {
    let default = if debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| default.into()))
        .with_target(false)
        .init();
}

async fn serve(config: Config) -> Result<()> {
    let store = Store::connect(&config.db_path).await?;
    if config.auto_migrate {
        store.migrate().await?;
    }
    let state = AppState::new(store, config.clone());
    let app = api::router(state);
    let listener = TcpListener::bind(config.bind).await?;
    tracing::info!(addr=%listener.local_addr()?, "coolify-web listening");
    axum::serve(listener, app).await?;
    Ok(())
}
