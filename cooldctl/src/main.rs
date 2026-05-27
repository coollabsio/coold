mod cli;
mod commands;
mod meshnet;
mod output;
mod services;
mod ssh;
mod wireguard;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::run().await
}
