use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    cli::OutputFormat,
    meshnet::{validate_namespace, MeshNetSingleFlags},
    output, services,
    ssh::{for_each_server, Runner, SshMeshFlags},
};

#[derive(Debug, Subcommand)]
pub enum FirewallCommand {
    Containers(ContainersCommand),
    List(ListCommand),
    Allow(AllowCommand),
    Revoke(RevokeCommand),
}

#[derive(Debug, Args, Clone)]
pub struct FirewallFlags {
    #[command(flatten)]
    pub ssh: SshMeshFlags,
    #[command(flatten)]
    pub mesh: MeshNetSingleFlags,
    #[arg(long, default_value = "wg0")]
    pub wg_interface: String,
    #[arg(long)]
    pub coold_token: Option<String>,
    #[arg(long, default_value_t = services::coold::COOLD_API_PORT)]
    pub coold_port: u16,
}
#[derive(Debug, Args)]
pub struct ContainersCommand {
    #[command(flatten)]
    pub flags: FirewallFlags,
    #[arg(long)]
    pub all_namespaces: bool,
}
#[derive(Debug, Args)]
pub struct ListCommand {
    #[command(flatten)]
    pub flags: FirewallFlags,
    #[arg(long)]
    pub all_namespaces: bool,
}
#[derive(Debug, Args)]
pub struct AllowCommand {
    #[command(flatten)]
    pub flags: FirewallFlags,
    #[arg(long)]
    pub from: String,
    #[arg(long)]
    pub to: String,
    #[arg(long, default_value = "tcp")]
    pub proto: String,
    #[arg(long)]
    pub port: Option<u16>,
}
#[derive(Debug, Args)]
pub struct RevokeCommand {
    #[command(flatten)]
    pub flags: FirewallFlags,
    #[arg(long)]
    pub id: Option<String>,
    #[arg(long)]
    pub from: Option<String>,
    #[arg(long)]
    pub to: Option<String>,
    #[arg(long, default_value = "tcp")]
    pub proto: String,
    #[arg(long)]
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllowRule {
    pub id: String,
    pub namespace: Option<String>,
    pub src: String,
    pub dst: String,
    pub proto: String,
    pub port: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerRow {
    pub server: String,
    pub namespace: String,
    pub id: String,
    pub name: String,
    pub ip: String,
    pub state: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct MutationOutput {
    pub server: String,
    pub id: String,
    pub status: String,
}

pub async fn run(cmd: FirewallCommand, format: OutputFormat) -> Result<()> {
    match cmd {
        FirewallCommand::Containers(c) => containers(c, format).await,
        FirewallCommand::List(c) => list(c, format).await,
        FirewallCommand::Allow(c) => allow(c, format).await,
        FirewallCommand::Revoke(c) => revoke(c, format).await,
    }
}

fn validate(flags: &FirewallFlags) -> Result<()> {
    flags.ssh.validate()?;
    validate_namespace(&flags.mesh.namespace)?;
    Ok(())
}
fn rule_id(
    namespace: &str,
    src: &str,
    dst: &str,
    proto: Option<&str>,
    port: Option<u16>,
) -> String {
    let mut h = Sha256::new();
    let namespace = if namespace.is_empty() {
        "default"
    } else {
        namespace
    };
    h.update(format!(
        "{namespace}|{src}|{dst}|{}|{}",
        proto.unwrap_or("").to_lowercase(),
        port.unwrap_or(0)
    ));
    hex::encode(h.finalize())[..12].to_string()
}

async fn coold_ip<R: Runner>(runner: &R, host: &str, flags: &FirewallFlags) -> Result<String> {
    let out = runner
        .run(
            host,
            &flags.ssh.ssh_user,
            flags.ssh.ssh_port,
            &format!(
                "ip -4 -o addr show dev {} | awk '{{print $4}}' | cut -d/ -f1 | head -n1",
                flags.wg_interface
            ),
        )
        .await?;
    let ip = out.stdout.trim();
    if ip.is_empty() {
        bail!("could not discover IPv4 address on {}", flags.wg_interface)
    };
    Ok(ip.into())
}
async fn token<R: Runner>(runner: &R, host: &str, flags: &FirewallFlags) -> Result<String> {
    if let Some(t) = &flags.coold_token {
        return Ok(t.clone());
    }
    if let Ok(t) = std::env::var("COOLIFY_COOLD_TOKEN") {
        if !t.trim().is_empty() {
            return Ok(t);
        }
    }
    let out = runner
        .run(
            host,
            &flags.ssh.ssh_user,
            flags.ssh.ssh_port,
            &format!("cat {}", services::coold::COOLD_API_TOKEN_PATH),
        )
        .await?;
    let token = out.stdout.trim();
    if token.is_empty() {
        bail!("coold token file is empty on {host}");
    }
    Ok(token.into())
}

fn build_curl_command(
    method: &str,
    token: &str,
    port: u16,
    ip: &str,
    path: &str,
    body: Option<&str>,
) -> String {
    let data = body
        .map(|b| format!("--data '{}'", b.replace("'", "'\"'\"'")))
        .unwrap_or_default();
    format!(
        "curl -fsS -X {method} -H 'Authorization: Bearer {token}' -H 'Content-Type: application/json' {data} 'http://{ip}:{port}{path}'"
    )
}

async fn curl<R: Runner>(
    runner: &R,
    host: &str,
    flags: &FirewallFlags,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<String> {
    let ip = coold_ip(runner, host, flags).await?;
    let tok = token(runner, host, flags).await?;
    let cmd = build_curl_command(method, &tok, flags.coold_port, &ip, path, body);
    Ok(runner
        .run(host, &flags.ssh.ssh_user, flags.ssh.ssh_port, &cmd)
        .await?
        .stdout)
}

async fn containers(c: ContainersCommand, format: OutputFormat) -> Result<()> {
    validate(&c.flags)?;
    let client = c.flags.ssh.client();
    let results = for_each_server(&c.flags.ssh.nodes, c.flags.ssh.concurrency, |host| {
        let flags = c.flags.clone();
        let client = client.clone();
        async move { discover_containers(&client, &host, &flags, c.all_namespaces).await }
    })
    .await;
    let mut rows = vec![];
    for r in results {
        if let Some(mut v) = r.result {
            rows.append(&mut v)
        } else {
            eprintln!("warning [{}]: {}", r.host, r.error.unwrap_or_default())
        }
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Pretty) {
        output::print(format, &rows)
    } else {
        output::table(
            &["SERVER", "NS", "ID", "NAME", "IP", "STATE"],
            &rows
                .iter()
                .map(|r| {
                    vec![
                        r.server.clone(),
                        r.namespace.clone(),
                        r.id.clone(),
                        r.name.clone(),
                        r.ip.clone(),
                        r.state.clone(),
                    ]
                })
                .collect::<Vec<_>>(),
        )
    }
}
async fn discover_containers<R: Runner>(
    runner: &R,
    host: &str,
    flags: &FirewallFlags,
    all: bool,
) -> Result<Vec<ContainerRow>> {
    let filter = if all {
        String::new()
    } else {
        format!("--filter network=coolify-{}-mesh", flags.mesh.namespace)
    };
    let out=runner.run(host,&flags.ssh.ssh_user,flags.ssh.ssh_port,&format!("podman ps --format '{{{{.ID}}}}|{{{{.Names}}}}|{{{{.Networks}}}}|{{{{.Status}}}}' {filter}")).await?;
    Ok(out
        .stdout
        .lines()
        .filter_map(|l| {
            let p = l.split('|').collect::<Vec<_>>();
            if p.len() < 4 {
                return None;
            }
            let ns = if all {
                p[2].split(',')
                    .find_map(|n| {
                        n.strip_prefix("coolify-")
                            .and_then(|x| x.strip_suffix("-mesh"))
                    })
                    .unwrap_or(&flags.mesh.namespace)
                    .to_string()
            } else {
                flags.mesh.namespace.clone()
            };
            Some(ContainerRow {
                server: host.into(),
                namespace: ns,
                id: p[0].into(),
                name: p[1].into(),
                ip: "".into(),
                state: p[3].into(),
            })
        })
        .collect())
}

async fn list(c: ListCommand, format: OutputFormat) -> Result<()> {
    validate(&c.flags)?;
    let client = c.flags.ssh.client();
    let results = for_each_server(&c.flags.ssh.nodes, c.flags.ssh.concurrency, |host| {
        let flags = c.flags.clone();
        let client = client.clone();
        async move {
            let path = if c.all_namespaces {
                "/api/v1/firewall/allow".into()
            } else {
                format!("/api/v1/firewall/allow?namespace={}", flags.mesh.namespace)
            };
            let body = curl(&client, &host, &flags, "GET", &path, None).await?;
            let mut rules: Vec<AllowRule> = serde_json::from_str(&body).unwrap_or_default();
            for r in &mut rules {
                if r.namespace.is_none() {
                    r.namespace = Some(flags.mesh.namespace.clone())
                }
            }
            Ok::<_, anyhow::Error>(rules)
        }
    })
    .await;
    let mut rules = vec![];
    for r in results {
        if let Some(mut v) = r.result {
            rules.append(&mut v)
        } else {
            eprintln!("warning [{}]: {}", r.host, r.error.unwrap_or_default())
        }
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Pretty) {
        output::print(format, &rules)
    } else {
        output::table(
            &["ID", "NS", "SRC", "DST", "PROTO", "PORT"],
            &rules
                .iter()
                .map(|r| {
                    vec![
                        r.id.clone(),
                        r.namespace.clone().unwrap_or_default(),
                        r.src.clone(),
                        r.dst.clone(),
                        r.proto.clone(),
                        r.port.map(|p| p.to_string()).unwrap_or_default(),
                    ]
                })
                .collect::<Vec<_>>(),
        )
    }
}

async fn allow(c: AllowCommand, format: OutputFormat) -> Result<()> {
    validate(&c.flags)?;
    let proto = if c.port.is_some() {
        Some(c.proto.as_str())
    } else {
        None
    };
    let id = rule_id(&c.flags.mesh.namespace, &c.from, &c.to, proto, c.port);
    let mut body = serde_json::json!({
        "id": id,
        "namespace": c.flags.mesh.namespace,
        "src": c.from,
        "dst": c.to,
    });
    if let Some(proto) = proto {
        body["proto"] = serde_json::json!(proto);
    }
    if let Some(port) = c.port {
        body["port"] = serde_json::json!(port);
    }
    mutate(
        c.flags,
        "POST",
        "/api/v1/firewall/allow",
        Some(body.to_string()),
        format,
    )
    .await
}
async fn revoke(c: RevokeCommand, format: OutputFormat) -> Result<()> {
    validate(&c.flags)?;
    let id = match c.id {
        Some(id) => id,
        None => {
            let proto = if c.port.is_some() {
                Some(c.proto.as_str())
            } else {
                None
            };
            rule_id(
                &c.flags.mesh.namespace,
                c.from
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("--from required when --id is omitted"))?,
                c.to.as_deref()
                    .ok_or_else(|| anyhow::anyhow!("--to required when --id is omitted"))?,
                proto,
                c.port,
            )
        }
    };
    mutate(
        c.flags,
        "DELETE",
        &format!("/api/v1/firewall/allow/{id}"),
        None,
        format,
    )
    .await
}
async fn mutate(
    flags: FirewallFlags,
    method: &str,
    path: &str,
    body: Option<String>,
    format: OutputFormat,
) -> Result<()> {
    let client = flags.ssh.client();
    let results = for_each_server(&flags.ssh.nodes, flags.ssh.concurrency, |host| {
        let flags = flags.clone();
        let body = body.clone();
        let client = client.clone();
        async move {
            curl(&client, &host, &flags, method, path, body.as_deref()).await?;
            Ok::<_, anyhow::Error>(MutationOutput {
                server: host,
                id: path.rsplit('/').next().unwrap_or("").into(),
                status: "ok".into(),
            })
        }
    })
    .await;
    let mut out = vec![];
    for r in results {
        if let Some(v) = r.result {
            out.push(v)
        } else {
            out.push(MutationOutput {
                server: r.host,
                id: String::new(),
                status: format!("error: {}", r.error.unwrap_or_default()),
            })
        }
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Pretty) {
        output::print(format, &out)
    } else {
        output::table(
            &["SERVER", "ID", "STATUS"],
            &out.iter()
                .map(|r| vec![r.server.clone(), r.id.clone(), r.status.clone()])
                .collect::<Vec<_>>(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::RunOutput;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex, OnceLock};

    #[derive(Default)]
    struct FakeRunner {
        calls: Arc<Mutex<Vec<String>>>,
        responses: Vec<(&'static str, &'static str)>,
    }

    #[async_trait]
    impl Runner for FakeRunner {
        async fn run(&self, _host: &str, _user: &str, _port: u16, cmd: &str) -> Result<RunOutput> {
            self.calls.lock().unwrap().push(cmd.to_string());
            for (needle, response) in &self.responses {
                if cmd.contains(needle) {
                    return Ok(RunOutput {
                        stdout: (*response).into(),
                        stderr: String::new(),
                        status: 0,
                    });
                }
            }
            Ok(RunOutput {
                stdout: String::new(),
                stderr: String::new(),
                status: 0,
            })
        }
    }

    static ENV_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

    async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    fn flags() -> FirewallFlags {
        FirewallFlags {
            ssh: SshMeshFlags {
                nodes: vec!["h1".into()],
                ssh_key: "test-key".into(),
                ssh_user: "root".into(),
                ssh_port: 22,
                ssh_passphrase_prompt: false,
                concurrency: 1,
                ssh_timeout: "30s".into(),
            },
            mesh: MeshNetSingleFlags {
                namespace: "default".into(),
            },
            wg_interface: "wg0".into(),
            coold_token: None,
            coold_port: 8443,
        }
    }

    #[test]
    fn ids_are_namespace_scoped_and_stable() {
        assert_eq!(
            rule_id("default", "10.0.0.1", "10.0.0.2", Some("tcp"), Some(80)),
            rule_id("default", "10.0.0.1", "10.0.0.2", Some("TCP"), Some(80))
        );
        assert_ne!(
            rule_id("default", "a", "b", None, None),
            rule_id("alpha", "a", "b", None, None)
        );
    }

    #[test]
    fn rule_ids_distinguish_port_and_default_namespace_wire_format() {
        let tcp80 = rule_id("default", "1.1.1.1", "2.2.2.2", Some("tcp"), Some(80));
        let tcp443 = rule_id("default", "1.1.1.1", "2.2.2.2", Some("tcp"), Some(443));
        assert_eq!(tcp80.len(), 12);
        assert_ne!(tcp80, tcp443);
        assert_eq!(
            rule_id("", "10.0.0.1", "10.0.0.2", Some("tcp"), Some(80)),
            rule_id("default", "10.0.0.1", "10.0.0.2", Some("tcp"), Some(80))
        );
    }

    #[test]
    fn build_curl_command_shapes_match_coold_api() {
        let allow = build_curl_command(
            "POST",
            "tok-xyz",
            9443,
            "100.64.0.2",
            "/api/v1/firewall/allow",
            Some(r#"{"src":"10.0.0.1","dst":"10.0.0.2"}"#),
        );
        assert!(allow.contains("curl -fsS"));
        assert!(allow.contains("-X POST"));
        assert!(allow.contains("Authorization: Bearer tok-xyz"));
        assert!(allow.contains("Content-Type: application/json"));
        assert!(allow.contains(r#"{"src":"10.0.0.1","dst":"10.0.0.2"}"#));
        assert!(allow.contains(":9443/api/v1/firewall/allow"));

        let revoke = build_curl_command(
            "DELETE",
            "tok-xyz",
            8443,
            "100.64.0.2",
            "/api/v1/firewall/allow/abc123def456",
            None,
        );
        assert!(revoke.contains("-X DELETE"));
        assert!(revoke.contains(":8443/api/v1/firewall/allow/abc123def456"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn token_prefers_flag_then_env_then_file() {
        let _guard = env_lock().await;
        let mut f = flags();
        f.coold_token = Some("flag-token".into());
        let runner = FakeRunner::default();
        assert_eq!(token(&runner, "h1", &f).await.unwrap(), "flag-token");
        assert!(runner.calls.lock().unwrap().is_empty());

        f.coold_token = None;
        std::env::set_var("COOLIFY_COOLD_TOKEN", "env-token");
        assert_eq!(token(&runner, "h1", &f).await.unwrap(), "env-token");
        std::env::remove_var("COOLIFY_COOLD_TOKEN");

        let runner = FakeRunner {
            responses: vec![(services::coold::COOLD_API_TOKEN_PATH, "file-token\n")],
            ..Default::default()
        };
        assert_eq!(token(&runner, "h1", &f).await.unwrap(), "file-token");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn token_errors_when_file_empty() {
        let _guard = env_lock().await;
        std::env::remove_var("COOLIFY_COOLD_TOKEN");
        let runner = FakeRunner::default();
        let err = token(&runner, "h1", &flags()).await.unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn curl_uses_discovered_ip_token_and_configured_port() {
        let _guard = env_lock().await;
        std::env::remove_var("COOLIFY_COOLD_TOKEN");
        let mut f = flags();
        f.coold_port = 9443;
        let runner = FakeRunner {
            responses: vec![
                ("ip -4 -o addr show dev wg0", "100.64.0.9\n"),
                (services::coold::COOLD_API_TOKEN_PATH, "tok\n"),
                ("curl -fsS", "[]"),
            ],
            ..Default::default()
        };
        let body = curl(
            &runner,
            "h1",
            &f,
            "GET",
            "/api/v1/firewall/allow?namespace=alpha",
            None,
        )
        .await
        .unwrap();
        assert_eq!(body, "[]");
        let calls = runner.calls.lock().unwrap();
        assert!(calls
            .iter()
            .any(|c| c.contains(":9443/api/v1/firewall/allow?namespace=alpha")));
    }
}
