use std::{path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use clap::Args;
use futures::{stream, StreamExt};
use tokio::{process::Command, time::timeout};

#[derive(Debug, Clone, Args)]
pub struct SshMeshFlags {
    #[arg(long = "nodes", alias = "servers", value_delimiter = ',')]
    pub nodes: Vec<String>,

    #[arg(long)]
    pub ssh_key: Option<PathBuf>,

    #[arg(long)]
    pub ssh_config: Option<PathBuf>,

    #[arg(long, default_value = "root")]
    pub ssh_user: String,

    /// Default SSH port. A node may override it with --nodes host:port.
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
        if self.nodes.is_empty() {
            bail!("--nodes is required");
        }
        self.validate_ssh_access()
    }

    pub fn validate_ssh_access(&self) -> Result<()> {
        if self.ssh_key.is_none() && self.ssh_config.is_none() {
            bail!("--ssh-key or --ssh-config is required");
        }
        Ok(())
    }

    pub fn timeout(&self) -> Duration {
        parse_duration(&self.ssh_timeout).unwrap_or(Duration::from_secs(30))
    }

    pub fn client(&self) -> SshClient {
        if self.ssh_passphrase_prompt {
            eprintln!(
                "warning: --ssh-passphrase-prompt is delegated to ssh/ssh-agent in coolify; ensure your key is unlocked"
            );
        }
        SshClient {
            key: self.ssh_key.clone(),
            config: self.ssh_config.clone(),
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
    key: Option<PathBuf>,
    config: Option<PathBuf>,
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
        let (ssh_host, ssh_port) = split_host_port(host, port);
        let dest = format!("{user}@{ssh_host}");
        let remote_cmd = if user == "root" {
            cmd.to_string()
        } else {
            format!("sudo -n bash -lc {}", shell_escape::escape(cmd.into()))
        };
        let mut c = Command::new("ssh");
        c.args(ssh_command_args(
            self.config.as_ref(),
            self.key.as_ref(),
            self.timeout,
            ssh_port,
            &dest,
            &remote_cmd,
        ));
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

fn ssh_command_args(
    config: Option<&PathBuf>,
    key: Option<&PathBuf>,
    timeout: Duration,
    port: u16,
    dest: &str,
    remote_cmd: &str,
) -> Vec<String> {
    let mut args = Vec::new();

    if let Some(config) = config {
        args.extend(["-F".into(), config.display().to_string()]);
    } else {
        let key = key.expect("ssh key is required when ssh config is not provided");
        args.extend([
            "-i".into(),
            key.display().to_string(),
            "-p".into(),
            port.to_string(),
        ]);
    }

    args.extend([
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        format!("ConnectTimeout={}", timeout.as_secs().max(1)),
    ]);

    if config.is_none() {
        args.extend([
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            "ControlPath=/tmp/coolify-ssh-%C".into(),
            "-o".into(),
            "ControlPersist=60s".into(),
        ]);
    }

    args.extend([dest.into(), remote_cmd.into()]);

    args
}

pub fn split_host_port(host: &str, default_port: u16) -> (String, u16) {
    let Some((name, raw_port)) = host.rsplit_once(':') else {
        return (host.to_string(), default_port);
    };
    if name.is_empty() || name.contains(':') {
        return (host.to_string(), default_port);
    }
    match raw_port.parse::<u16>() {
        Ok(port) => (name.to_string(), port),
        Err(_) => (host.to_string(), default_port),
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
    format!(
        "cat > {path}.tmp <<'COOLIFY_CLI_EOF'\n{body}COOLIFY_CLI_EOF\nchmod {mode} {path}.tmp && mv {path}.tmp {path}"
    )
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

    #[test]
    fn split_host_port_uses_node_port_override() {
        assert_eq!(
            split_host_port("127.0.0.1:51593", 22),
            ("127.0.0.1".into(), 51593)
        );
        assert_eq!(
            split_host_port("example.com", 22),
            ("example.com".into(), 22)
        );
    }

    #[test]
    fn ssh_args_use_short_lived_control_master() {
        let args = ssh_command_args(
            None,
            Some(&PathBuf::from("/tmp/key")),
            Duration::from_secs(30),
            2222,
            "user@example.com",
            "true",
        );

        expect_args_contains(&args, "ControlMaster=auto");
        expect_args_contains(&args, "ControlPath=/tmp/coolify-ssh-%C");
        expect_args_contains(&args, "ControlPersist=60s");
        expect_args_contains(&args, "ConnectTimeout=30");
    }

    #[test]
    fn ssh_args_can_use_config_file() {
        let args = ssh_command_args(
            Some(&PathBuf::from("/tmp/ssh.config")),
            None,
            Duration::from_secs(30),
            2222,
            "lima-coold-dev",
            "true",
        );

        expect_args_contains(&args, "-F");
        expect_args_contains(&args, "/tmp/ssh.config");
        assert!(!args.iter().any(|arg| arg == "-i"));
        assert!(!args.iter().any(|arg| arg == "-p"));
        assert!(!args
            .iter()
            .any(|arg| arg == "ControlPath=/tmp/coolify-ssh-%C"));
    }

    fn expect_args_contains(args: &[String], expected: &str) {
        assert!(
            args.iter().any(|arg| arg == expected),
            "missing {expected} in {args:?}"
        );
    }
}
