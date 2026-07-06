use std::{collections::HashMap, convert::Infallible, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::{body::Incoming, Method, Request, Response};
use hyper_util::client::legacy::{Client, ResponseFuture};
use hyperlocal::{UnixClientExt, UnixConnector};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

use super::types::{
    Container, ContainerCreateResponse, ContainerCreateSpec, ContainerInspect, ExecCreateResponse,
    ExecInspect, Image, ImagePullReport, PortMappingSpec,
};

type HttpBody = BoxBody<Bytes, Infallible>;
type HyperClient = Client<UnixConnector, HttpBody>;

#[derive(Clone)]
pub struct PodmanClient {
    inner: Arc<Inner>,
}

struct Inner {
    socket: PathBuf,
    http: HyperClient,
    /// Allow-list of host directory prefixes that may be bind-mounted into
    /// created containers (S4). Empty = deny every host bind mount.
    allowed_mount_sources: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct CreateContainerInput {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub env: Vec<String>,
    pub networks: Vec<String>,
    pub volumes: Vec<String>,
    pub ports: Vec<CreatePortMapping>,
    pub dns: Vec<String>,
    pub dns_search: Vec<String>,
    pub network_aliases: Vec<String>,
    pub restart_policy: String,
    pub privileged: bool,
    pub network_mode: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CreatePortMapping {
    pub host_ip: String,
    pub host_port: u32,
    pub container_port: u32,
    pub protocol: String,
}

impl PodmanClient {
    pub fn new(socket: PathBuf, allowed_mount_sources: Vec<PathBuf>) -> Self {
        let http: HyperClient = Client::unix();
        Self {
            inner: Arc::new(Inner {
                socket,
                http,
                allowed_mount_sources,
            }),
        }
    }

    pub fn socket(&self) -> &std::path::Path {
        &self.inner.socket
    }

    fn request(&self, method: Method, path: &str, body: HttpBody) -> ResponseFuture {
        let uri: hyper::Uri = hyperlocal::Uri::new(&self.inner.socket, path).into();
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("accept", "application/json")
            .body(body)
            .expect("well-formed podman request");
        self.inner.http.request(req)
    }

    async fn send(&self, method: Method, path: &str, body: HttpBody) -> Result<Response<Incoming>> {
        self.request(method, path, body)
            .await
            .with_context(|| format!("podman {path}"))
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let res = self.send(Method::GET, path, empty_body()).await?;
        read_json(res, path).await
    }

    async fn post_empty(&self, path: &str) -> Result<String> {
        let res = self.send(Method::POST, path, empty_body()).await?;
        read_text(res, path).await
    }

    async fn post_json<T: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let res = self.send(Method::POST, path, json_body(body)?).await?;
        read_json(res, path).await
    }

    async fn delete_empty(&self, path: &str) -> Result<String> {
        let res = self.send(Method::DELETE, path, empty_body()).await?;
        read_text(res, path).await
    }

    /// Connect to the Podman events stream. Caller consumes the hyper body.
    pub async fn events(&self, path: &str) -> Result<Response<Incoming>> {
        let res = self.send(Method::GET, path, empty_body()).await?;
        if !res.status().is_success() {
            return Err(anyhow!("podman {path} returned HTTP {}", res.status()));
        }
        Ok(res)
    }

    pub async fn list_images(&self) -> Result<Vec<Image>> {
        self.get_json("/v5.0.0/libpod/images/json").await
    }

    pub async fn pull_image(&self, reference: &str) -> Result<(String, String)> {
        if !is_safe_podman_ref(reference) {
            return Err(anyhow!("images.pull denied invalid image reference"));
        }
        let path = format!(
            "/v5.0.0/libpod/images/pull?reference={}",
            url_escape(reference)
        );
        let text = self.post_empty(&path).await?;
        let digest = text
            .lines()
            .filter_map(|line| serde_json::from_str::<ImagePullReport>(line).ok())
            .find_map(|report| {
                if !report.digest.is_empty() {
                    Some(report.digest)
                } else if !report.id.is_empty() {
                    Some(report.id)
                } else {
                    report.images.into_iter().next()
                }
            })
            .unwrap_or_default();
        Ok((digest, text))
    }

    pub async fn delete_image(&self, reference: &str, force: bool) -> Result<String> {
        if !is_safe_podman_ref(reference) {
            return Err(anyhow!("images.delete denied invalid image reference"));
        }
        self.delete_empty(&format!(
            "/v5.0.0/libpod/images/{}?force={}",
            url_escape(reference),
            force
        ))
        .await
    }

    pub async fn containers_list(&self) -> Result<Vec<Container>> {
        self.get_json("/v5.0.0/libpod/containers/json?all=true")
            .await
    }

    pub async fn inspect_container(&self, id: &str) -> Result<ContainerInspect> {
        self.get_json(&format!(
            "/v5.0.0/libpod/containers/{}/json",
            url_escape(id)
        ))
        .await
    }

    pub async fn inspect_container_json(&self, id: &str) -> Result<String> {
        let res = self
            .send(
                Method::GET,
                &format!("/v5.0.0/libpod/containers/{}/json", url_escape(id)),
                empty_body(),
            )
            .await?;
        read_text(res, "inspect container").await
    }

    pub async fn create_container(&self, input: CreateContainerInput) -> Result<String> {
        if !is_safe_podman_ref(&input.image) || !is_safe_podman_ref(&input.name) {
            return Err(anyhow!("containers.create denied invalid image or name"));
        }
        validate_create_container(&input, &self.inner.allowed_mount_sources)?;
        let spec = create_spec(input);
        let response: ContainerCreateResponse = self
            .post_json(
                &format!(
                    "/v5.0.0/libpod/containers/create?name={}",
                    url_escape(&spec.name)
                ),
                &spec,
            )
            .await?;
        if response.id.is_empty() {
            return Err(anyhow!("podman returned empty container id"));
        }
        Ok(response.id)
    }

    pub async fn start_container(&self, id: &str) -> Result<String> {
        self.post_empty(&format!(
            "/v5.0.0/libpod/containers/{}/start",
            url_escape(id)
        ))
        .await
    }

    pub async fn stop_container(&self, id: &str, timeout_seconds: u32) -> Result<String> {
        self.post_empty(&format!(
            "/v5.0.0/libpod/containers/{}/stop?t={}",
            url_escape(id),
            timeout_seconds
        ))
        .await
    }

    pub async fn restart_container(&self, id: &str, timeout_seconds: u32) -> Result<String> {
        self.post_empty(&format!(
            "/v5.0.0/libpod/containers/{}/restart?t={}",
            url_escape(id),
            timeout_seconds
        ))
        .await
    }

    pub async fn delete_container(&self, id: &str, force: bool) -> Result<String> {
        self.delete_empty(&format!(
            "/v5.0.0/libpod/containers/{}?force={}",
            url_escape(id),
            force
        ))
        .await
    }

    pub async fn container_logs(
        &self,
        id: &str,
        tail: u32,
        stdout: bool,
        stderr: bool,
    ) -> Result<String> {
        let tail = if tail == 0 {
            "all".to_string()
        } else {
            tail.to_string()
        };
        let res = self
            .send(
                Method::GET,
                &format!(
                    "/v5.0.0/libpod/containers/{}/logs?stdout={}&stderr={}&tail={}",
                    url_escape(id),
                    stdout,
                    stderr,
                    tail
                ),
                empty_body(),
            )
            .await?;
        read_text(res, "container logs").await
    }

    pub async fn exec_container(&self, id: &str, command: Vec<String>) -> Result<(i32, String)> {
        if command.is_empty() {
            return Err(anyhow!("exec command cannot be empty"));
        }
        let create: ExecCreateResponse = self
            .post_json(
                &format!("/v5.0.0/libpod/containers/{}/exec", url_escape(id)),
                &json!({ "Cmd": command, "AttachStdout": true, "AttachStderr": true }),
            )
            .await?;
        if create.id.is_empty() {
            return Err(anyhow!("podman returned empty exec id"));
        }
        let output = self
            .post_empty(&format!(
                "/v5.0.0/libpod/exec/{}/start",
                url_escape(&create.id)
            ))
            .await?;
        // R2: the /start endpoint returns the exec output but not the exit
        // status. Poll the exec inspect endpoint until the process is no
        // longer running, then return its real ExitCode instead of a
        // hard-coded 0 (which masked every command failure).
        let exit_code = self.wait_exec_exit_code(&create.id).await?;
        Ok((exit_code, output))
    }

    /// Poll `exec/{id}/json` until the exec session reports `Running=false`,
    /// then return its `ExitCode`. Handles the race where inspect runs before
    /// the process has finished (R2).
    async fn wait_exec_exit_code(&self, exec_id: &str) -> Result<i32> {
        const POLL_INTERVAL: Duration = Duration::from_millis(50);
        const MAX_ATTEMPTS: u32 = 1200; // ~60s ceiling before giving up.

        let path = format!("/v5.0.0/libpod/exec/{}/json", url_escape(exec_id));
        for _ in 0..MAX_ATTEMPTS {
            let inspect: ExecInspect = self.get_json(&path).await?;
            if let Some(code) = finished_exec_exit_code(&inspect) {
                return Ok(code);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Err(anyhow!("exec {exec_id} did not finish within timeout"))
    }

    pub async fn run_healthcheck(&self, id: &str) -> Result<String> {
        self.post_empty(&format!(
            "/v5.0.0/libpod/containers/{}/healthcheck",
            url_escape(id)
        ))
        .await
    }
}

fn validate_create_container(
    input: &CreateContainerInput,
    allowed_mount_sources: &[PathBuf],
) -> Result<()> {
    if input.privileged {
        return Err(anyhow!("containers.create denied privileged mode"));
    }
    // S4: reject host networking and namespace-join forms. `container:<id>`
    // and `ns:<path>` join another container's / an arbitrary network
    // namespace, escaping mesh isolation just like `host` does.
    if is_denied_network_mode(&input.network_mode) || input.networks.iter().any(|n| n == "host") {
        return Err(anyhow!("containers.create denied host networking"));
    }
    if !input.capabilities.is_empty() {
        return Err(anyhow!("containers.create denied custom capabilities"));
    }
    for volume in &input.volumes {
        let Some((source, _target)) = volume.split_once(':') else {
            continue;
        };
        validate_mount_source(source, allowed_mount_sources)?;
    }
    Ok(())
}

/// True for `network_mode` values that break mesh isolation: literal `host`
/// plus the namespace-join forms `container:<id>` and `ns:<path>` (S4).
fn is_denied_network_mode(mode: &str) -> bool {
    let mode = mode.trim().to_ascii_lowercase();
    mode == "host" || mode.starts_with("container:") || mode.starts_with("ns:")
}

/// Validate a bind-mount source against the allow-list (S4).
///
/// Deny-by-default: with an empty allow-list NO host bind mount is permitted.
/// Named volumes (sources that are not absolute host paths) are always allowed
/// — they are managed by Podman and cannot reference arbitrary host state.
/// Absolute sources are canonicalized (symlinks + `..` resolved) before being
/// matched so `/var/run/podman` (via the `/var/run`→`/run` symlink) and `..`
/// traversal cannot bypass the list. A source that cannot be canonicalized
/// (e.g. does not exist) is rejected — we fail closed rather than guess.
fn validate_mount_source(source: &str, allowed_mount_sources: &[PathBuf]) -> Result<()> {
    if !is_host_path_source(source) {
        return Ok(());
    }

    if allowed_mount_sources.is_empty() {
        return Err(anyhow!(
            "containers.create denied host bind mount {source:?}: host bind mounts are disabled \
             (set COOLIFY_COOLD_ALLOWED_MOUNT_SOURCES to permit specific prefixes)"
        ));
    }

    let canonical = std::fs::canonicalize(source).map_err(|e| {
        anyhow!("containers.create denied host bind mount {source:?}: cannot canonicalize ({e})")
    })?;

    let permitted = allowed_mount_sources.iter().any(|prefix| {
        let prefix = std::fs::canonicalize(prefix).unwrap_or_else(|_| prefix.clone());
        canonical.starts_with(&prefix)
    });

    if permitted {
        Ok(())
    } else {
        Err(anyhow!(
            "containers.create denied host bind mount {source:?}: not under an allowed prefix"
        ))
    }
}

/// A bind-mount source is treated as a host path (subject to the allow-list)
/// when it is absolute or contains path separators / `..` traversal. Bare
/// tokens like `web-data` are named volumes and are left to Podman.
fn is_host_path_source(source: &str) -> bool {
    source.starts_with('/') || source.contains('/') || source.contains("..")
}

fn create_spec(input: CreateContainerInput) -> ContainerCreateSpec {
    let env = input
        .env
        .into_iter()
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect::<HashMap<_, _>>();
    let network_aliases = input.network_aliases.clone();
    let networks = input
        .networks
        .into_iter()
        .map(|network| {
            let value = if network_aliases.is_empty() {
                json!({})
            } else {
                json!({ "aliases": network_aliases.clone() })
            };

            (network, value)
        })
        .collect::<HashMap<_, _>>();
    let mounts = input
        .volumes
        .into_iter()
        .filter_map(|volume| {
            let (source, destination) = volume.split_once(':')?;
            Some(json!({
                "type": "bind",
                "source": source,
                "destination": destination,
            }))
        })
        .collect();
    let port_mappings = input
        .ports
        .into_iter()
        .map(|port| PortMappingSpec {
            host_ip: port.host_ip,
            host_port: port.host_port,
            container_port: port.container_port,
            protocol: if port.protocol.is_empty() {
                "tcp".into()
            } else {
                port.protocol
            },
        })
        .collect();

    ContainerCreateSpec {
        name: input.name,
        image: input.image,
        command: input.command,
        env,
        networks,
        mounts,
        port_mappings,
        dns_servers: input.dns,
        dns_search: input.dns_search,
        restart_policy: if input.restart_policy.is_empty() {
            None
        } else {
            Some(input.restart_policy)
        },
    }
}

fn empty_body() -> HttpBody {
    Empty::<Bytes>::new().boxed()
}

fn json_body<T: Serialize>(body: &T) -> Result<HttpBody> {
    let bytes = serde_json::to_vec(body).context("encode podman JSON request")?;
    Ok(Full::new(Bytes::from(bytes)).boxed())
}

async fn read_json<T: DeserializeOwned>(res: Response<Incoming>, path: &str) -> Result<T> {
    let status = res.status();
    let bytes = body_bytes(res, path).await?;
    if !status.is_success() {
        return Err(anyhow!(
            "podman {path} returned HTTP {status}: {}",
            String::from_utf8_lossy(&bytes)
        ));
    }
    serde_json::from_slice(&bytes).with_context(|| format!("podman {path} decode"))
}

async fn read_text(res: Response<Incoming>, path: &str) -> Result<String> {
    let status = res.status();
    let bytes = body_bytes(res, path).await?;
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    if !status.is_success() {
        return Err(anyhow!("podman {path} returned HTTP {status}: {text}"));
    }
    Ok(text)
}

async fn body_bytes(res: Response<Incoming>, path: &str) -> Result<Bytes> {
    res.into_body()
        .collect()
        .await
        .with_context(|| format!("podman {path} read body"))
        .map(|body| body.to_bytes())
}

/// Percent-encode set for values interpolated into libpod request paths and
/// query strings (S6). Encodes everything except the RFC 3986 unreserved set
/// (`A-Za-z0-9-._~`), so `&`, `?`, `#`, spaces, control characters, `/`, `:`,
/// etc. can no longer inject extra path segments or query parameters into the
/// podman API call.
const PODMAN_ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Return true when a reference/id contains only characters that are safe to
/// place in a podman API path/query. Control characters and whitespace are
/// rejected outright; everything else is percent-encoded by [`url_escape`].
fn is_safe_podman_ref(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| !c.is_control() && !c.is_whitespace())
}

fn url_escape(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, PODMAN_ENCODE).to_string()
}

/// When the exec session has stopped running, return its real exit code; while
/// it is still running return `None` so the caller keeps polling (R2).
fn finished_exec_exit_code(inspect: &ExecInspect) -> Option<i32> {
    (!inspect.running).then_some(inspect.exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input() -> CreateContainerInput {
        CreateContainerInput {
            name: "web".into(),
            image: "docker.io/library/nginx:alpine".into(),
            command: vec![],
            env: vec![],
            networks: vec!["coolify-default-mesh".into()],
            volumes: vec![],
            ports: vec![],
            dns: vec![],
            dns_search: vec![],
            network_aliases: vec![],
            restart_policy: String::new(),
            privileged: false,
            network_mode: String::new(),
            capabilities: vec![],
        }
    }

    #[test]
    fn create_spec_includes_network_aliases_and_dns_search_domains() {
        let mut input = base_input();
        input.network_aliases = vec!["coolify-v5-nginx-test".into()];
        input.dns = vec!["10.210.0.1".into()];
        input.dns_search = vec!["default.coolify.internal".into()];

        let spec = create_spec(input);
        let network = spec.networks.get("coolify-default-mesh").unwrap();

        assert_eq!(network["aliases"][0], "coolify-v5-nginx-test");
        assert_eq!(spec.dns_servers, vec!["10.210.0.1"]);
        assert_eq!(spec.dns_search, vec!["default.coolify.internal"]);
    }

    #[test]
    fn create_filter_denies_privileged_containers() {
        let mut input = base_input();
        input.privileged = true;

        let err = validate_create_container(&input, &[]).unwrap_err();

        assert!(format!("{err:#}").contains("privileged"));
    }

    #[test]
    fn create_filter_denies_host_networking() {
        let mut input = base_input();
        input.network_mode = "host".into();

        let err = validate_create_container(&input, &[]).unwrap_err();

        assert!(format!("{err:#}").contains("host networking"));
    }

    #[test]
    fn create_filter_denies_network_namespace_join_forms() {
        for mode in ["container:abc123", "NS:/proc/1/ns/net", "Container:web"] {
            let mut input = base_input();
            input.network_mode = mode.into();
            let err = validate_create_container(&input, &[]).unwrap_err();
            assert!(
                format!("{err:#}").contains("host networking"),
                "mode {mode} should be denied"
            );
        }
    }

    #[test]
    fn create_filter_denies_all_host_mounts_by_default() {
        // Deny-by-default (empty allow-list): /etc, the podman socket via the
        // /var/run symlink, and `..` traversal are all rejected (S4).
        for source in [
            "/etc",
            "/etc/coolify",
            "/var/run/podman/podman.sock",
            "/data/coolify/../../etc",
        ] {
            let mut input = base_input();
            input.volumes = vec![format!("{source}:/target")];
            let err = validate_create_container(&input, &[]).unwrap_err();
            assert!(
                format!("{err:#}").contains("host bind mounts are disabled"),
                "source {source} should be denied"
            );
        }
    }

    #[test]
    fn create_filter_allows_source_under_explicit_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().to_path_buf()];
        let sub = dir.path().join("data");
        std::fs::create_dir(&sub).unwrap();

        let mut input = base_input();
        input.volumes = vec![format!("{}:/data", sub.display())];

        assert!(validate_create_container(&input, &allowed).is_ok());
    }

    #[test]
    fn create_filter_rejects_traversal_escaping_allowed_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().join("safe")];
        std::fs::create_dir(dir.path().join("safe")).unwrap();
        std::fs::create_dir(dir.path().join("secret")).unwrap();

        // Escapes the allowed prefix via `..` — canonicalization resolves it
        // and the prefix check rejects it.
        let escape = dir.path().join("safe").join("..").join("secret");
        let mut input = base_input();
        input.volumes = vec![format!("{}:/data", escape.display())];

        let err = validate_create_container(&input, &allowed).unwrap_err();
        assert!(format!("{err:#}").contains("not under an allowed prefix"));
    }

    #[test]
    fn create_filter_allows_named_volumes() {
        let mut input = base_input();
        input.volumes = vec!["web-data:/data".into()];
        assert!(validate_create_container(&input, &[]).is_ok());
    }

    #[test]
    fn url_escape_neutralizes_injection_chars() {
        let escaped = url_escape("nginx&force=true?x=#frag zone");
        assert!(!escaped.contains('&'));
        assert!(!escaped.contains('?'));
        assert!(!escaped.contains('#'));
        assert!(!escaped.contains(' '));
    }

    #[test]
    fn url_escape_leaves_unreserved_refs_unchanged() {
        assert_eq!(url_escape("nginx-alpine_1.2~x"), "nginx-alpine_1.2~x");
        assert_eq!(
            url_escape("docker.io/library/nginx"),
            "docker.io%2Flibrary%2Fnginx"
        );
    }

    #[test]
    fn rejects_refs_with_control_or_whitespace() {
        assert!(!is_safe_podman_ref("nginx alpine"));
        assert!(!is_safe_podman_ref("nginx\n"));
        assert!(!is_safe_podman_ref(""));
        assert!(is_safe_podman_ref("docker.io/library/nginx:alpine"));
    }

    #[test]
    fn exec_exit_code_reported_when_finished() {
        let failed: ExecInspect =
            serde_json::from_str(r#"{"Running": false, "ExitCode": 1}"#).unwrap();
        assert_eq!(finished_exec_exit_code(&failed), Some(1));

        let ok: ExecInspect = serde_json::from_str(r#"{"Running": false, "ExitCode": 0}"#).unwrap();
        assert_eq!(finished_exec_exit_code(&ok), Some(0));

        let running: ExecInspect =
            serde_json::from_str(r#"{"Running": true, "ExitCode": 0}"#).unwrap();
        assert_eq!(finished_exec_exit_code(&running), None);
    }
}
