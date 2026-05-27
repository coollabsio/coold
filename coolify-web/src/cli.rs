use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use tokio::time::{timeout, Duration};

#[derive(Debug, Parser)]
#[command(author, version, about = "Coolify v5 Rust API + React web app")]
pub struct Cli {
    #[arg(long, global = true)]
    pub debug: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Serve,
    Healthcheck {
        #[arg(long, default_value = "http://127.0.0.1:3000/healthz")]
        url: String,
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u64,
    },
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum DbCommand {
    Migrate,
    Info,
}

pub async fn healthcheck(url: String, timeout_ms: u64) -> Result<()> {
    let fut = async move {
        let text = reqwest_like_get(&url).await?;
        if !text.contains("ok") {
            bail!("unhealthy response from {url}: {text}");
        }
        Ok(())
    };
    timeout(Duration::from_millis(timeout_ms), fut).await??;
    println!("ok");
    Ok(())
}

async fn reqwest_like_get(url: &str) -> Result<String> {
    // Tiny HTTP-only probe to avoid adding a full client dependency for healthcheck.
    let url = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("only http:// healthcheck URLs are supported for now"))?;
    let (host_port, path) = url
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((url, "/".into()));
    let mut stream = tokio::net::TcpStream::connect(host_port).await?;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).await?;
    Ok(buf)
}
