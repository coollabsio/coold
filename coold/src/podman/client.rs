use std::{path::PathBuf, sync::Arc};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{body::Incoming, Request, Response};
use hyper_util::client::legacy::{Client, ResponseFuture};
use hyperlocal::{UnixClientExt, UnixConnector};
use serde::de::DeserializeOwned;

use super::types::{Container, ContainerInspect};

type HyperClient = Client<UnixConnector, Empty<Bytes>>;

#[derive(Clone)]
pub struct PodmanClient {
    inner: Arc<Inner>,
}

struct Inner {
    socket: PathBuf,
    http: HyperClient,
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

    fn request(&self, path: &str) -> ResponseFuture {
        let uri: hyper::Uri = hyperlocal::Uri::new(&self.inner.socket, path).into();
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("accept", "application/json")
            .body(Empty::<Bytes>::new())
            .expect("well-formed GET request");
        self.inner.http.request(req)
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let res = self
            .request(path)
            .await
            .with_context(|| format!("podman GET {path}"))?;
        read_json(res, path).await
    }

    /// Connect to the Podman events stream. Caller consumes the hyper body.
    pub async fn events(&self, path: &str) -> Result<Response<Incoming>> {
        let res = self
            .request(path)
            .await
            .with_context(|| format!("podman GET {path}"))?;
        if !res.status().is_success() {
            return Err(anyhow!("podman {path} returned HTTP {}", res.status()));
        }
        Ok(res)
    }

    pub async fn list_containers(&self) -> Result<Vec<Container>> {
        // `all=true` so we also see non-running containers; we later filter by network membership.
        self.get_json("/v5.0.0/libpod/containers/json?all=true")
            .await
    }

    /// Per-container inspect. libpod's list endpoint returns an empty
    /// `Networks` map; inspect is the only way to read per-network IPs.
    pub async fn inspect_container(&self, id: &str) -> Result<ContainerInspect> {
        self.get_json(&format!("/v5.0.0/libpod/containers/{id}/json"))
            .await
    }
}

async fn read_json<T: DeserializeOwned>(res: Response<Incoming>, path: &str) -> Result<T> {
    let status = res.status();
    let bytes = res
        .into_body()
        .collect()
        .await
        .with_context(|| format!("podman {path} read body"))?
        .to_bytes();
    if !status.is_success() {
        return Err(anyhow!(
            "podman {path} returned HTTP {status}: {}",
            String::from_utf8_lossy(&bytes)
        ));
    }
    serde_json::from_slice(&bytes).with_context(|| format!("podman {path} decode"))
}
