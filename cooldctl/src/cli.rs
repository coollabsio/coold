use clap::{Parser, Subcommand, ValueEnum};

use crate::commands::{firewall, init};

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
    Pretty,
}

#[derive(Debug, Parser)]
#[command(author, version, about = "Coolify v5 cluster provisioning CLI")]
pub struct Cli {
    #[arg(long, value_enum, default_value_t = OutputFormat::Table, global = true)]
    pub format: OutputFormat,

    #[arg(long, global = true)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    #[command(subcommand)]
    Init(init::InitCommand),
    #[command(subcommand)]
    Firewall(firewall::FirewallCommand),
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let level = if cli.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| level.into()),
        )
        .with_target(false)
        .init();

    match cli.command {
        Commands::Init(cmd) => init::run(cmd, cli.format).await,
        Commands::Firewall(cmd) => firewall::run(cmd, cli.format).await,
    }
}
