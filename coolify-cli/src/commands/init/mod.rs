use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use ipnet::Ipv4Net;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::{
    cli::OutputFormat,
    meshnet::{validate_interface, validate_namespaces, validate_version, MeshNetMultiFlags},
    output,
    services::tls,
    ssh::SshMeshFlags,
    wireguard::{
        apply,
        intent::Intent,
        plan::{build_plan, Plan},
        reconstruct::reconstruct,
        state::DesiredMesh,
    },
};

const ALPHA_BANNER: &str = "\n[ALPHA] coolify init targets Coolify v5 and is experimental.\n[ALPHA] WireGuard mesh bootstrap requires root/sudo and modifies network configuration.\n[ALPHA] Test in non-production environments first. Stability is not guaranteed.\n";

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
    /// Per-node WireGuard listen ports, for dev/NAT cases. Format: node=port,node2=port.
    #[arg(long, value_delimiter = ',')]
    pub wg_listen_port_overrides: Vec<String>,
    /// Per-node WireGuard peer endpoints, for dev/NAT cases. Format: node=host:port,node2=host:port.
    #[arg(long, value_delimiter = ',')]
    pub wg_endpoint_overrides: Vec<String>,
    #[arg(long)]
    pub skip_default_deny: bool,
    #[arg(long, default_value = "nightly")]
    pub coold_version: String,
    #[arg(long, default_value = "v1.0.0")]
    pub corrosion_version: String,
    #[arg(long, default_value_t = 8787)]
    pub corrosion_gossip_port: u16,
    #[arg(long, default_value_t = 8080)]
    pub corrosion_api_port: u16,
    /// S1-adjacent: pin the expected SHA-256 of the coold release tarball.
    /// When set, install aborts on mismatch; when unset, a published
    /// `<url>.sha256` sidecar is used if available.
    #[arg(long)]
    pub coold_sha256: Option<String>,
    /// S1-adjacent: pin the expected SHA-256 of the corrosion release tarball.
    #[arg(long)]
    pub corrosion_sha256: Option<String>,
    /// S5 (opt-in, default OFF): run Corrosion gossip over mutual TLS using a
    /// shared self-signed cert provisioned to every node, instead of the
    /// default plaintext gossip (which relies on WireGuard mesh membership
    /// alone). Verify against your Corrosion version before relying on it.
    #[arg(long)]
    pub enable_corrosion_gossip_tls: bool,
    /// S1 (opt-in, default OFF): generate a self-signed cert for the flux↔coold
    /// channel and drop the pin file coold reads (`/etc/coolify/flux.pin`) on
    /// every node. The flux cert/key are written locally for manual install on
    /// the flux host (see --flux-tls-out-dir). Default provisioning stays on
    /// plaintext-over-WireGuard.
    #[arg(long)]
    pub enable_flux_tls: bool,
    /// Subject alternative names (IPs or DNS) for the generated flux cert —
    /// typically the flux WireGuard mgmt IP and/or its hostname. REQUIRED (and
    /// must include a non-localhost entry) when --enable-flux-tls is set.
    #[arg(long, value_delimiter = ',')]
    pub flux_tls_san: Vec<String>,
    /// Port coold dials the flux gRPC channel on. Used to build the `https://`
    /// `COOLIFY_COOLD_FLUX_URL` baked into each node's unit when flux TLS is on.
    #[arg(long, default_value_t = 6443)]
    pub flux_port: u16,
    /// Where to write the generated flux cert/key for manual install on flux.
    #[arg(long, default_value = "./coolify-flux-tls")]
    pub flux_tls_out_dir: PathBuf,
    #[arg(short = 'y', long)]
    pub yes: bool,
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
            "This command will modify network configuration on the listed nodes. Use --yes to skip this prompt in automation."
        );
    }
    let desired = build_desired(&base, intent, new_nodes, allow_replace, allow_nightly)?;
    if let Some(cert) = &desired.flux_tls {
        write_flux_tls_output(
            &base.flux_tls_out_dir,
            cert,
            desired.flux_tls_url.as_deref(),
        )?;
    }
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

/// True for SANs that only a colocated process could reach — rejected as the
/// sole flux SAN because remote coold nodes must dial the flux host by a real
/// mesh address.
fn is_localhost_san(san: &str) -> bool {
    matches!(
        san.trim().to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1" | "0.0.0.0" | ""
    )
}

/// S1 (opt-in): write the generated flux cert/key locally and print the exact
/// remaining MANUAL steps. The CLI has already wired the NODE side: each node's
/// coold unit now dials `flux_url` (`https://…`, `COOLIFY_COOLD_FLUX_URL`) with
/// the pin path env set, and the pin file (`/etc/coolify/flux.pin`) was dropped
/// on every node. What the CLI CANNOT do (it does not manage the central flux
/// process or mint host JWTs) is left for the operator.
fn write_flux_tls_output(
    out_dir: &std::path::Path,
    cert: &tls::SelfSignedCert,
    flux_url: Option<&str>,
) -> Result<()> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("create flux TLS output dir {}", out_dir.display()))?;
    let cert_path = out_dir.join("cert.pem");
    let key_path = out_dir.join("key.pem");
    std::fs::write(&cert_path, &cert.cert_pem).context("write flux cert.pem")?;
    std::fs::write(&key_path, &cert.key_pem).context("write flux key.pem")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .context("chmod flux key.pem")?;
    }
    let url = flux_url.unwrap_or("https://<flux-mesh-ip>:6443");
    eprintln!(
        "flux TLS enabled.\n\
         \x20 Automatic (node side): each coold unit dials {url} over pinned TLS\n\
         \x20   (COOLIFY_COOLD_FLUX_URL + COOLIFY_COOLD_FLUX_TLS_PIN_PATH), and the pin\n\
         \x20   file {pin} was dropped on every node.\n\
         \x20 Wrote {cert} and {key}.\n\
         \x20 Remaining MANUAL steps (CLI does not manage flux or host JWTs):\n\
         \x20   1. Copy cert.pem/key.pem to the flux host.\n\
         \x20   2. Start flux with COOLIFY_FLUX_TLS_CERT_PATH={cert} \\\n\
         \x20        COOLIFY_FLUX_TLS_KEY_PATH={key}\n\
         \x20      (flux must bind its mesh IP — the SAN above — not localhost).\n\
         \x20   3. Install each node's per-host JWT at {jwt} — coold exits at boot\n\
         \x20      if the flux URL is set but the JWT file is missing.",
        url = url,
        pin = crate::wireguard::state::FLUX_PIN_PATH,
        jwt = crate::wireguard::state::HOST_JWT_PATH,
        cert = cert_path.display(),
        key = key_path.display(),
    );
    Ok(())
}

fn build_desired(
    base: &BaseInitFlags,
    intent: Intent,
    new_nodes: Vec<String>,
    allow_replace: bool,
    allow_nightly: bool,
) -> Result<DesiredMesh> {
    base.ssh.validate_ssh_access()?;
    validate_namespaces(&base.mesh.namespaces)?;
    // S-cli: reject shell/systemd-injection payloads in operator-supplied
    // interface and version strings before they are interpolated into remote
    // commands and unit files.
    validate_interface(&base.wg_interface)?;
    validate_version("--coold-version", &base.coold_version)?;
    validate_version("--corrosion-version", &base.corrosion_version)?;
    // S5 / S1: generate opt-in TLS material once so the same shared cert is
    // provisioned to every node in this run. Default (flags off) => None, and
    // provisioning is byte-identical to plaintext-over-WireGuard.
    let corrosion_gossip_tls = if base.enable_corrosion_gossip_tls {
        Some(
            tls::generate_self_signed("coolify-corrosion-gossip", &[])
                .context("generate corrosion gossip TLS cert")?,
        )
    } else {
        None
    };
    // S1 (opt-in): when flux TLS is on, the operator MUST name the flux host's
    // reachable mesh IP/hostname so (a) the cert carries a matching SAN and
    // (b) coold can dial a real `https://` URL. A localhost-only SAN would only
    // work for a flux colocated on one node — remote coold nodes could never
    // reach it — so reject it with a clear error rather than silently wiring an
    // unreachable URL.
    let (flux_tls, flux_tls_url) = if base.enable_flux_tls {
        let sans = clean_hosts(&base.flux_tls_san);
        let Some(flux_host) = sans.iter().find(|s| !is_localhost_san(s)).cloned() else {
            bail!(
                "--enable-flux-tls requires --flux-tls-san to include the flux host's mesh IP or \
                 hostname (got {:?}); a localhost-only SAN cannot be reached by remote coold \
                 nodes over TLS",
                base.flux_tls_san
            );
        };
        let cert =
            tls::generate_self_signed("coolify-flux", &sans).context("generate flux TLS cert")?;
        let url = format!("https://{flux_host}:{}", base.flux_port);
        (Some(cert), Some(url))
    } else {
        (None, None)
    };
    let nodes = clean_hosts(&base.ssh.nodes);
    let new_nodes = clean_hosts(&new_nodes);
    if nodes.is_empty() {
        bail!("--nodes is required");
    }
    let hosts = mesh_hosts(&nodes)?;
    let listen_port_overrides = parse_port_overrides(&base.wg_listen_port_overrides)?;
    let endpoint_overrides = parse_endpoint_overrides(&base.wg_endpoint_overrides)?;
    Ok(DesiredMesh {
        hosts,
        nodes,
        interface: base.wg_interface.clone(),
        mgmt_pool: base.wg_mgmt_pool.parse::<Ipv4Net>()?,
        container_pool: base.mesh.container_pool.parse::<Ipv4Net>()?,
        container_prefix: base.mesh.container_prefix,
        listen_port: base.wg_listen_port,
        listen_port_overrides,
        endpoint_overrides,
        install_podman: true,
        namespaces: base.mesh.namespaces.clone(),
        default_deny_containers: !base.skip_default_deny,
        install_coold: true,
        coold_version: base.coold_version.clone(),
        corrosion_version: base.corrosion_version.clone(),
        corrosion_gossip_port: base.corrosion_gossip_port,
        corrosion_api_port: base.corrosion_api_port,
        coold_sha256: base.coold_sha256.clone(),
        corrosion_sha256: base.corrosion_sha256.clone(),
        corrosion_gossip_tls,
        flux_tls,
        flux_tls_url,
        intent,
        new_nodes,
        allow_replace,
        allow_nightly,
    })
}

fn mesh_hosts(nodes: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for node in nodes {
        if node.is_empty() {
            continue;
        }
        if !seen.insert(node.clone()) {
            bail!("duplicate node in --nodes: {node}");
        }
        out.push(node.clone());
    }
    Ok(out)
}

fn parse_port_overrides(values: &[String]) -> Result<BTreeMap<String, u16>> {
    let mut out = BTreeMap::new();
    for value in values {
        let Some((host, raw_port)) = value.split_once('=') else {
            bail!("invalid --wg-listen-port-overrides entry {value:?}; expected node=port");
        };
        let port = raw_port.parse::<u16>()?;
        out.insert(host.trim().to_string(), port);
    }
    Ok(out)
}

fn parse_endpoint_overrides(values: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for value in values {
        let Some((host, endpoint)) = value.split_once('=') else {
            bail!("invalid --wg-endpoint-overrides entry {value:?}; expected node=host:port");
        };
        out.insert(host.trim().to_string(), endpoint.trim().to_string());
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

    fn base_flags() -> BaseInitFlags {
        BaseInitFlags {
            ssh: SshMeshFlags {
                nodes: vec!["node-a".into()],
                ssh_key: None,
                ssh_config: Some(PathBuf::from("/dev/null")),
                ssh_user: "root".into(),
                ssh_port: 22,
                ssh_passphrase_prompt: false,
                concurrency: 10,
                ssh_timeout: "30s".into(),
            },
            mesh: crate::meshnet::MeshNetMultiFlags {
                namespaces: vec!["default".into()],
                container_pool: "10.210.0.0/16".into(),
                container_prefix: 24,
            },
            wg_mgmt_pool: "100.64.0.0/16".into(),
            wg_interface: "wg0".into(),
            wg_listen_port: 51820,
            wg_listen_port_overrides: vec![],
            wg_endpoint_overrides: vec![],
            skip_default_deny: false,
            coold_version: "nightly".into(),
            corrosion_version: "v1.0.0".into(),
            corrosion_gossip_port: 8787,
            corrosion_api_port: 8080,
            coold_sha256: None,
            corrosion_sha256: None,
            enable_corrosion_gossip_tls: false,
            enable_flux_tls: false,
            flux_tls_san: vec![],
            flux_port: 6443,
            flux_tls_out_dir: PathBuf::from("./coolify-flux-tls"),
            yes: true,
        }
    }

    #[test]
    fn is_localhost_san_matches_local_only_addresses() {
        for s in [
            "localhost",
            "127.0.0.1",
            "::1",
            "0.0.0.0",
            "",
            " LocalHost ",
        ] {
            assert!(is_localhost_san(s), "{s:?} should be localhost");
        }
        for s in ["100.64.0.1", "flux.internal", "10.0.0.5"] {
            assert!(!is_localhost_san(s), "{s:?} should NOT be localhost");
        }
    }

    #[test]
    fn flux_tls_off_leaves_url_unset() {
        let d = build_desired(&base_flags(), Intent::Bootstrap, vec![], false, false).unwrap();
        assert!(d.flux_tls.is_none());
        assert!(d.flux_tls_url.is_none());
        assert!(d.coold_flux_config().is_none());
    }

    #[test]
    fn flux_tls_on_wires_https_url_and_pin_env() {
        let mut base = base_flags();
        base.enable_flux_tls = true;
        base.flux_tls_san = vec!["100.64.0.1".into()];
        let d = build_desired(&base, Intent::Bootstrap, vec![], false, false).unwrap();
        assert!(d.flux_tls.is_some());
        assert_eq!(d.flux_tls_url.as_deref(), Some("https://100.64.0.1:6443"));
        let flux = d.coold_flux_config().expect("flux config present");
        assert_eq!(flux.url, "https://100.64.0.1:6443");
        assert_eq!(flux.tls_pin_path.as_deref(), Some("/etc/coolify/flux.pin"));
        // The generated coold unit must dial TLS and set the pin path env.
        let unit =
            crate::services::coold::service_unit("100.64.0.5".parse().unwrap(), &[], Some(&flux));
        assert!(unit.contains("Environment=COOLIFY_COOLD_FLUX_URL=https://100.64.0.1:6443"));
        assert!(unit.contains("Environment=COOLIFY_COOLD_FLUX_TLS_PIN_PATH=/etc/coolify/flux.pin"));
    }

    #[test]
    fn flux_tls_on_requires_non_localhost_san() {
        for sans in [
            vec![],
            vec!["localhost".to_string()],
            vec!["127.0.0.1".to_string()],
        ] {
            let mut base = base_flags();
            base.enable_flux_tls = true;
            base.flux_tls_san = sans.clone();
            let err = build_desired(&base, Intent::Bootstrap, vec![], false, false)
                .expect_err("localhost-only SAN must be rejected");
            assert!(
                err.to_string().contains("flux-tls-san"),
                "unexpected error for {sans:?}: {err}"
            );
        }
    }

    #[test]
    fn mesh_hosts_keeps_node_order() {
        let got = mesh_hosts(&["1.2.3.4:51572".into(), "5.6.7.8:51593".into()]).unwrap();
        assert_eq!(got, vec!["1.2.3.4:51572", "5.6.7.8:51593"]);
    }

    #[test]
    fn parse_dev_wireguard_overrides() {
        let ports = parse_port_overrides(&["node-a=51821".into(), "node-b=51822".into()]).unwrap();
        assert_eq!(ports["node-a"], 51821);
        let endpoints = parse_endpoint_overrides(&[
            "node-a=host.lima.internal:51821".into(),
            "node-b=host.lima.internal:51822".into(),
        ])
        .unwrap();
        assert_eq!(endpoints["node-b"], "host.lima.internal:51822");
    }
}

#[derive(Serialize)]
struct ApplyOutput {
    results: Vec<apply::ActionResult>,
    verified: Vec<apply::VerifyResult>,
}
