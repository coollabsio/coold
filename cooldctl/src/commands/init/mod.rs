use anyhow::{Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use ipnet::Ipv4Net;
use serde::Serialize;
use std::collections::BTreeSet;

use crate::{
    cli::OutputFormat,
    meshnet::{MeshNetMultiFlags, validate_namespaces},
    output,
    ssh::SshMeshFlags,
    wireguard::{
        apply,
        intent::Intent,
        plan::{Plan, build_plan},
        reconstruct::reconstruct,
        state::DesiredMesh,
    },
};

const ALPHA_BANNER: &str = "\n[ALPHA] cooldctl init targets Coolify v5 and is experimental.\n[ALPHA] WireGuard mesh bootstrap requires root/sudo and modifies network configuration.\n[ALPHA] Test in non-production environments first. Stability is not guaranteed.\n";

#[derive(Debug, Subcommand)]
pub enum InitCommand {
    Plan(PlanCommand),
    Bootstrap(ApplyCommand),
    Extend(ExtendCommand),
    Upgrade(UpgradeCommand),
}

#[derive(Debug, Args)]
pub struct BaseInitFlags {
    #[command(flatten)]
    pub ssh: SshMeshFlags,
    #[command(flatten)]
    pub mesh: MeshNetMultiFlags,
    #[arg(long, default_value = "100.64.0.0/16")]
    pub wg_mgmt_pool: String,
    #[arg(long, default_value = "wg0")]
    pub wg_interface: String,
    #[arg(long, default_value_t = 51820)]
    pub wg_listen_port: u16,
    #[arg(long)]
    pub skip_default_deny: bool,
    #[arg(long, default_value = "nightly")]
    pub coold_version: String,
    #[arg(long, default_value = "nightly")]
    pub corrosion_version: String,
    #[arg(long, default_value_t = 8787)]
    pub corrosion_gossip_port: u16,
    #[arg(long, default_value_t = 8080)]
    pub corrosion_api_port: u16,
    #[arg(short = 'y', long)]
    pub yes: bool,
    #[arg(long, default_value = "")]
    pub central: String,
    #[arg(long, default_value = "nightly")]
    pub coolify_version: String,
    #[arg(long, default_value = "nightly")]
    pub scheduler_version: String,
    #[arg(long, default_value_t = true)]
    pub enable_builder: bool,
    #[arg(long, value_delimiter = ',')]
    pub builder_hosts: Vec<String>,
    #[arg(long, default_value_t = 2)]
    pub builder_capacity: u32,
    #[arg(long, default_value = "200%")]
    pub builder_cpu_quota: String,
    #[arg(long, default_value = "2G")]
    pub builder_memory_max: String,
    #[arg(long, default_value_t = 1800)]
    pub builder_timeout_secs: u32,
}

#[derive(Debug, Args)]
pub struct PlanCommand {
    #[command(flatten)]
    pub base: BaseInitFlags,
    #[arg(long, value_enum, default_value_t=PlanIntent::Bootstrap)]
    pub intent: PlanIntent,
    #[arg(long = "new-nodes", alias = "new-hosts", value_delimiter = ',')]
    pub new_nodes: Vec<String>,
    #[arg(long)]
    pub allow_replace: bool,
    #[arg(long)]
    pub allow_nightly: bool,
}
#[derive(Debug, Args)]
pub struct ApplyCommand {
    #[command(flatten)]
    pub base: BaseInitFlags,
}
#[derive(Debug, Args)]
pub struct ExtendCommand {
    #[command(flatten)]
    pub base: BaseInitFlags,
    #[arg(long = "new-nodes", alias = "new-hosts", value_delimiter = ',')]
    pub new_nodes: Vec<String>,
    #[arg(long)]
    pub allow_replace: bool,
}
#[derive(Debug, Args)]
pub struct UpgradeCommand {
    #[command(flatten)]
    pub base: BaseInitFlags,
    #[arg(long)]
    pub allow_nightly: bool,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PlanIntent {
    Bootstrap,
    Extend,
    Upgrade,
}
impl From<PlanIntent> for Intent {
    fn from(v: PlanIntent) -> Self {
        match v {
            PlanIntent::Bootstrap => Intent::Bootstrap,
            PlanIntent::Extend => Intent::Extend,
            PlanIntent::Upgrade => Intent::Upgrade,
        }
    }
}

pub async fn run(cmd: InitCommand, format: OutputFormat) -> Result<()> {
    match cmd {
        InitCommand::Plan(c) => run_plan(c, format).await,
        InitCommand::Bootstrap(c) => {
            run_apply(
                c.base,
                Intent::Bootstrap,
                vec![],
                false,
                false,
                false,
                "Bootstrapping mesh...",
                format,
            )
            .await
        }
        InitCommand::Extend(c) => {
            if c.new_nodes.is_empty() {
                bail!("--new-nodes is required: list the subset of --nodes that is brand-new");
            }
            run_apply(
                c.base,
                Intent::Extend,
                c.new_nodes,
                c.allow_replace,
                false,
                true,
                "Extending mesh...",
                format,
            )
            .await
        }
        InitCommand::Upgrade(c) => {
            run_apply(
                c.base,
                Intent::Upgrade,
                vec![],
                false,
                c.allow_nightly,
                true,
                "Upgrading agent binaries...",
                format,
            )
            .await
        }
    }
}

async fn run_plan(c: PlanCommand, format: OutputFormat) -> Result<()> {
    eprint!("{ALPHA_BANNER}");
    let desired = build_desired(
        &c.base,
        c.intent.into(),
        c.new_nodes,
        c.allow_replace,
        c.allow_nightly,
    )?;
    let client = c.base.ssh.client();
    eprintln!("Probing {} mesh host(s)...", desired.hosts.len());
    let current = reconstruct(
        &client,
        &desired.hosts,
        &c.base.ssh.ssh_user,
        c.base.ssh.ssh_port,
        &desired.interface,
        &desired.namespaces,
        c.base.ssh.concurrency,
    )
    .await?;
    let plan = build_plan(&desired, &current)?;
    render_plan(format, &desired, &plan)
}

#[allow(clippy::too_many_arguments)]
async fn run_apply(
    base: BaseInitFlags,
    intent: Intent,
    new_nodes: Vec<String>,
    allow_replace: bool,
    allow_nightly: bool,
    skip_gate: bool,
    header: &str,
    format: OutputFormat,
) -> Result<()> {
    eprint!("{ALPHA_BANNER}");
    if !skip_gate
        && !base.yes
        && std::env::var("COOLIFY_NON_INTERACTIVE").unwrap_or_default() != "1"
    {
        eprintln!(
            "This command will modify network configuration on the listed nodes/central host. Use --yes to skip this prompt in automation."
        );
    }
    let desired = build_desired(&base, intent, new_nodes, allow_replace, allow_nightly)?;
    let client = base.ssh.client();
    eprintln!("{header}");
    eprintln!("Probing {} mesh host(s)...", desired.hosts.len());
    let current = reconstruct(
        &client,
        &desired.hosts,
        &base.ssh.ssh_user,
        base.ssh.ssh_port,
        &desired.interface,
        &desired.namespaces,
        base.ssh.concurrency,
    )
    .await?;
    let plan = build_plan(&desired, &current)?;
    if plan.is_empty() {
        eprintln!("No changes needed. Mesh is already converged.");
    } else {
        eprintln!("Plan:");
        for a in &plan.actions {
            eprintln!("  [{}] {}  {}", a.host, a.action_type, a.detail);
        }
    }
    let results = if plan.is_empty() {
        vec![]
    } else {
        apply::apply_mesh(
            &client,
            &base.ssh.ssh_user,
            base.ssh.ssh_port,
            &desired,
            &current,
            base.ssh.concurrency,
        )
        .await?
    };
    let verified = apply::verify(
        &client,
        &desired.hosts,
        &base.ssh.ssh_user,
        base.ssh.ssh_port,
        &desired.interface,
        base.ssh.concurrency,
    )
    .await;
    if matches!(format, OutputFormat::Json | OutputFormat::Pretty) {
        output::print(format, &ApplyOutput { results, verified })
    } else {
        let rows = results
            .iter()
            .map(|r| {
                vec![
                    r.action.host.clone(),
                    r.action.action_type.to_string(),
                    r.status.clone(),
                    r.detail.clone(),
                ]
            })
            .collect::<Vec<_>>();
        if !rows.is_empty() {
            output::table(&["HOST", "ACTION", "STATUS", "DETAIL"], &rows)?;
        }
        let vrows = verified
            .iter()
            .map(|v| {
                vec![
                    v.host.clone(),
                    v.wireguard_ip.clone(),
                    v.peer_count.to_string(),
                    v.status.clone(),
                ]
            })
            .collect::<Vec<_>>();
        output::table(&["HOST", "WIREGUARD IP", "PEERS", "STATUS"], &vrows)
    }
}

fn build_desired(
    base: &BaseInitFlags,
    intent: Intent,
    new_nodes: Vec<String>,
    allow_replace: bool,
    allow_nightly: bool,
) -> Result<DesiredMesh> {
    base.ssh.validate_ssh_key()?;
    validate_namespaces(&base.mesh.namespaces)?;
    let nodes = clean_hosts(&base.ssh.nodes);
    let new_nodes = clean_hosts(&new_nodes);
    if base.central.is_empty() && nodes.is_empty() {
        bail!("at least one of --central or --nodes is required");
    }
    for h in &base.builder_hosts {
        if !nodes.contains(h) {
            bail!("--builder-hosts entry {h:?} is not in --nodes");
        }
    }
    let hosts = mesh_hosts(&base.central, &nodes)?;
    Ok(DesiredMesh {
        hosts,
        nodes,
        interface: base.wg_interface.clone(),
        mgmt_pool: base.wg_mgmt_pool.parse::<Ipv4Net>()?,
        container_pool: base.mesh.container_pool.parse::<Ipv4Net>()?,
        container_prefix: base.mesh.container_prefix,
        listen_port: base.wg_listen_port,
        install_podman: true,
        namespaces: base.mesh.namespaces.clone(),
        default_deny_containers: !base.skip_default_deny,
        install_coold: true,
        coold_version: base.coold_version.clone(),
        corrosion_version: base.corrosion_version.clone(),
        corrosion_gossip_port: base.corrosion_gossip_port,
        corrosion_api_port: base.corrosion_api_port,
        central_host: base.central.clone(),
        coolify_version: base.coolify_version.clone(),
        scheduler_version: base.scheduler_version.clone(),
        enable_builder: base.enable_builder,
        builder_hosts: base.builder_hosts.clone(),
        builder_capacity: base.builder_capacity,
        builder_cpu_quota: base.builder_cpu_quota.clone(),
        builder_memory_max: base.builder_memory_max.clone(),
        builder_timeout_secs: base.builder_timeout_secs,
        intent,
        new_nodes,
        allow_replace,
        allow_nightly,
    })
}

fn mesh_hosts(central: &str, nodes: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut seen_nodes = BTreeSet::new();
    if !central.is_empty() {
        out.push(central.to_string());
        seen.insert(central.to_string());
    }
    for node in nodes {
        if node.is_empty() {
            continue;
        }
        if !seen_nodes.insert(node.clone()) {
            bail!("duplicate node in --nodes: {node}");
        }
        if !seen.insert(node.clone()) {
            continue;
        }
        out.push(node.clone());
    }
    Ok(out)
}

fn clean_hosts(hosts: &[String]) -> Vec<String> {
    hosts
        .iter()
        .map(|h| h.trim())
        .filter(|h| !h.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn render_plan(format: OutputFormat, desired: &DesiredMesh, plan: &Plan) -> Result<()> {
    #[derive(Serialize)]
    struct PlanOutput<'a> {
        nodes: &'a [String],
        mesh_hosts: &'a [String],
        actions: &'a [crate::wireguard::plan::PlannedAction],
        skipped: &'a [crate::wireguard::plan::SkippedAction],
        warnings: &'a [crate::wireguard::subnet::Warning],
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Pretty) {
        return output::print(
            format,
            &PlanOutput {
                nodes: &desired.nodes,
                mesh_hosts: &desired.hosts,
                actions: &plan.actions,
                skipped: &plan.skipped,
                warnings: &plan.warnings,
            },
        );
    }
    if plan.actions.is_empty() && plan.skipped.is_empty() {
        println!("No changes needed. Mesh is already converged.");
        return Ok(());
    }
    let rows = plan
        .actions
        .iter()
        .map(|a| vec![a.host.clone(), a.action_type.to_string(), a.detail.clone()])
        .collect::<Vec<_>>();
    if !rows.is_empty() {
        output::table(&["HOST", "ACTION", "DETAIL"], &rows)?;
    }
    if !plan.skipped.is_empty() {
        eprintln!("Skipped by intent filter:");
        for s in &plan.skipped {
            eprintln!(
                "  [{}] {} — {}",
                s.action.host, s.action.action_type, s.reason
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_hosts_allows_all_in_one_central_node() {
        let got = mesh_hosts("1.2.3.4", &["1.2.3.4".into()]).unwrap();
        assert_eq!(got, vec!["1.2.3.4"]);
    }

    #[test]
    fn mesh_hosts_keeps_central_before_nodes() {
        let got = mesh_hosts("1.2.3.4", &["5.6.7.8".into(), "9.9.9.9".into()]).unwrap();
        assert_eq!(got, vec!["1.2.3.4", "5.6.7.8", "9.9.9.9"]);
    }
}

#[derive(Serialize)]
struct ApplyOutput {
    results: Vec<apply::ActionResult>,
    verified: Vec<apply::VerifyResult>,
}
