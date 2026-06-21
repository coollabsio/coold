use std::{collections::HashMap, convert::Infallible, path::PathBuf, sync::Arc};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::{body::Incoming, Method, Request, Response};
use hyper_util::client::legacy::{Client, ResponseFuture};
use hyperlocal::{UnixClientExt, UnixConnector};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

use super::types::{
    Container, ContainerCreateResponse, ContainerCreateSpec, ContainerInspect, ExecCreateResponse,
    Image, ImagePullReport, PortMappingSpec,
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
    pub fn new(socket: PathBuf) -> Self {
        let http: HyperClient = Client::unix();
        Self {
            inner: Arc::new(Inner { socket, http }),
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
        validate_create_container(&input)?;
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
        Ok((0, output))
    }

    pub async fn run_healthcheck(&self, id: &str) -> Result<String> {
        self.post_empty(&format!(
            "/v5.0.0/libpod/containers/{}/healthcheck",
            url_escape(id)
        ))
        .await
    }
}

fn validate_create_container(input: &CreateContainerInput) -> Result<()> {
    if input.privileged {
        return Err(anyhow!("containers.create denied privileged mode"));
    }
    if input.network_mode.eq_ignore_ascii_case("host") || input.networks.iter().any(|n| n == "host")
    {
        return Err(anyhow!("containers.create denied host networking"));
    }
    if !input.capabilities.is_empty() {
        return Err(anyhow!("containers.create denied custom capabilities"));
    }
    for volume in &input.volumes {
        let Some((source, _target)) = volume.split_once(':') else {
            continue;
        };
        if source == "/"
            || source.starts_with("/run/podman")
            || source.starts_with("/var/run/docker")
        {
            return Err(anyhow!("containers.create denied unsafe host mount"));
        }
    }
    Ok(())
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

fn url_escape(value: &str) -> String {
    value.replace('/', "%2F").replace(':', "%3A")
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

        let err = validate_create_container(&input).unwrap_err();

        assert!(format!("{err:#}").contains("privileged"));
    }

    #[test]
    fn create_filter_denies_host_networking() {
        let mut input = base_input();
        input.network_mode = "host".into();

        let err = validate_create_container(&input).unwrap_err();

        assert!(format!("{err:#}").contains("host networking"));
    }

    #[test]
    fn create_filter_denies_unsafe_mounts() {
        let mut input = base_input();
        input.volumes = vec!["/run/podman/podman.sock:/sock".into()];

        let err = validate_create_container(&input).unwrap_err();

        assert!(format!("{err:#}").contains("unsafe host mount"));
    }
}
