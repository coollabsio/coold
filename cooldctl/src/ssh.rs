use std::{path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use clap::Args;
use futures::{stream, StreamExt};
use tokio::{process::Command, time::timeout};

#[derive(Debug, Clone, Args)]
pub struct SshMeshFlags {
    #[arg(long, value_delimiter = ',')]
    pub servers: Vec<String>,

    #[arg(long)]
    pub ssh_key: PathBuf,

    #[arg(long, default_value = "root")]
    pub ssh_user: String,

    #[arg(long, default_value_t = 22)]
    pub ssh_port: u16,

    #[arg(long)]
    pub ssh_passphrase_prompt: bool,

    #[arg(long, default_value_t = 10)]
    pub concurrency: usize,

    #[arg(long, default_value = "30s")]
    pub ssh_timeout: String,
}

impl SshMeshFlags {
    pub fn validate(&self) -> Result<()> {
        if self.servers.is_empty() {
            bail!("--servers is required");
        }
        if self.ssh_key.as_os_str().is_empty() {
            bail!("--ssh-key is required");
        }
        Ok(())
    }

    pub fn timeout(&self) -> Duration {
        parse_duration(&self.ssh_timeout).unwrap_or(Duration::from_secs(30))
    }

    pub fn client(&self) -> SshClient {
        if self.ssh_passphrase_prompt {
            eprintln!("warning: --ssh-passphrase-prompt is delegated to ssh/ssh-agent in cooldctl; ensure your key is unlocked");
        }
        SshClient {
            key: self.ssh_key.clone(),
            timeout: self.timeout(),
        }
    }
}

fn parse_duration(s: &str) -> Option<Duration> {
    if let Some(raw) = s.strip_suffix("ms") {
        raw.parse::<u64>().ok().map(Duration::from_millis)
    } else if let Some(raw) = s.strip_suffix('s') {
        raw.parse::<u64>().ok().map(Duration::from_secs)
    } else if let Some(raw) = s.strip_suffix('m') {
        raw.parse::<u64>().ok().map(|v| Duration::from_secs(v * 60))
    } else {
        s.parse::<u64>().ok().map(Duration::from_secs)
    }
}

#[derive(Debug, Clone)]
pub struct SshClient {
    key: PathBuf,
    timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub stdout: String,
    pub stderr: String,
    #[allow(dead_code)]
    pub status: i32,
}

#[async_trait]
pub trait Runner: Send + Sync {
    async fn run(&self, host: &str, user: &str, port: u16, cmd: &str) -> Result<RunOutput>;
}

#[async_trait]
impl Runner for SshClient {
    async fn run(&self, host: &str, user: &str, port: u16, cmd: &str) -> Result<RunOutput> {
        let dest = format!("{user}@{host}");
        let mut c = Command::new("ssh");
        c.arg("-i")
            .arg(&self.key)
            .arg("-p")
            .arg(port.to_string())
            .arg("-o")
            .arg("StrictHostKeyChecking=no")
            .arg("-o")
            .arg("UserKnownHostsFile=/dev/null")
            .arg("-o")
            .arg(format!("ConnectTimeout={}", self.timeout.as_secs().max(1)))
            .arg(dest)
            .arg(cmd);
        let output = timeout(self.timeout + Duration::from_secs(5), c.output())
            .await
            .context("ssh command timed out")?
            .context("spawn ssh")?;
        let status = output.status.code().unwrap_or(255);
        let out = RunOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            status,
        };
        if !output.status.success() {
            bail!(
                "ssh {host}: exit {status}: {}",
                first_line(&out.stderr)
                    .or_else(|| first_line(&out.stdout))
                    .unwrap_or_default()
            );
        }
        Ok(out)
    }
}

pub async fn for_each_server<T, F, Fut>(
    hosts: &[String],
    concurrency: usize,
    f: F,
) -> Vec<ServerResult<T>>
where
    T: Send + 'static,
    F: Fn(String) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<T>> + Send,
{
    let limit = concurrency.max(1);
    stream::iter(hosts.iter().cloned())
        .map(|host| {
            let fut = f(host.clone());
            async move {
                match fut.await {
                    Ok(result) => ServerResult {
                        host,
                        result: Some(result),
                        error: None,
                    },
                    Err(error) => ServerResult {
                        host,
                        result: None,
                        error: Some(error.to_string()),
                    },
                }
            }
        })
        .buffer_unordered(limit)
        .collect()
        .await
}

#[derive(Debug, Clone)]
pub struct ServerResult<T> {
    pub host: String,
    pub result: Option<T>,
    pub error: Option<String>,
}

pub fn heredoc(path: &str, body: &str, mode: &str) -> String {
    format!("cat > {path}.tmp <<'COOLDCTL_EOF'\n{body}COOLDCTL_EOF\nchmod {mode} {path}.tmp && mv {path}.tmp {path}")
}

pub fn first_line(s: &str) -> Option<String> {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_basic_durations() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
    }
}
