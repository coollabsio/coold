mod auth;
mod config;
mod envelope;
mod registry;
mod resource_status;
mod revocation;
mod routing;
mod state;
mod unix_bridge;

use std::path::Path;

use anyhow::{Context, Result};
use tracing::info;
use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::EnvFilter;

use crate::config::{Config, FLUX_LOG_FILE_PATH};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load().await?;
    let log_file_path = Path::new(FLUX_LOG_FILE_PATH);
    let _log_guard = init_tracing(&config.log_level, log_file_path)?;

    info!(
        grpc_bind = %config.grpc_bind,
        uds_path = %config.unix_socket_path.display(),
        log_file_path = %log_file_path.display(),
        "flux starting",
    );

    let streams = state::Streams::new();
    let pending = state::Pending::new();

    // #3: revocation denylist, loaded from disk (expired entries pruned) and
    // shared between the auth layer (reads on connect) and the UDS bridge
    // (Laravel mutates via /v1/tokens/revoke).
    let revocations = revocation::RevocationStore::load(config.revocation_path.clone());

    // S3: verification key set — the default key plus any rotation keys keyed
    // by `kid`.
    let jwt_keys = std::sync::Arc::new(auth::JwtKeys::new(
        config.jwt_public_key.clone(),
        config.jwt_additional_keys.clone(),
    ));

    tokio::try_join!(
        grpc_server::run(
            config.clone(),
            streams.clone(),
            pending.clone(),
            jwt_keys.clone(),
            revocations.clone(),
        ),
        unix_bridge::run(
            config.clone(),
            streams.clone(),
            pending.clone(),
            revocations.clone(),
        ),
        pending_sweeper::run(config.clone(), pending.clone()),
        revocation_sweeper::run(revocations.clone()),
        registry::heartbeat_loop(config.clone(), streams.clone()),
        coold_heartbeat::run(config.clone(), streams.clone()),
    )?;

    Ok(())
}

/// Periodically prune revocation entries whose token `exp` has passed (#3):
/// once a token would fail expiry on its own, its denylist entry is dead weight.
mod revocation_sweeper {
    use anyhow::Result;
    use tracing::warn;

    use crate::{auth::unix_now, revocation::RevocationStore};

    pub async fn run(revocations: RevocationStore) -> Result<()> {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            ticker.tick().await;
            if let Err(e) = revocations.prune(unix_now()) {
                warn!(error = format!("{e:#}"), "revocation prune failed");
            }
        }
    }
}

fn init_tracing(
    level: &str,
    log_file_path: &Path,
) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));

    let log_dir = log_file_path
        .parent()
        .context("flux log file path must include a parent directory")?;
    let log_file_name = log_file_path
        .file_name()
        .context("flux log file path must include a file name")?;

    std::fs::create_dir_all(log_dir)
        .with_context(|| format!("create flux log directory {}", log_dir.display()))?;

    let file_appender = tracing_appender::rolling::never(log_dir, log_file_name);
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let writer = std::io::stdout.and(file_writer);

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_target(false)
        .compact()
        .init();

    Ok(guard)
}

mod grpc_server {
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::Arc;

    use anyhow::{Context, Result};
    use tokio::sync::{mpsc, Semaphore};
    use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
    use tonic::transport::{Identity, ServerTlsConfig};
    use tonic::{transport::Server, Request, Response, Status, Streaming};
    use tracing::{debug, info, warn};

    use coolify_proto::agent::v1::{
        agent_server::{Agent, AgentServer},
        client_msg, response, ClientMsg, ServerMsg,
    };

    use crate::{
        auth::{self, JwtKeys},
        config::Config,
        envelope::ResponseBody,
        registry::RegistryClient,
        resource_status::ResourceStatusPublisher,
        revocation::RevocationStore,
        state::{Pending, ResponseData, StreamHandle, Streams},
    };

    /// R6: wire schema range flux supports. coold advertises its own
    /// `schema_min`/`schema_max` in Hello; the ranges must overlap or the
    /// stream is rejected at Hello rather than failing per-verb later.
    pub(super) const FLUX_SCHEMA_MIN: u32 = 1;
    pub(super) const FLUX_SCHEMA_MAX: u32 = 1;

    /// C3: max concurrent status-update publish tasks to Laravel across all
    /// hosts. Bounds task amplification from a flooding coold (a burst beyond
    /// this is coalesced away — dropped — rather than spawning unbounded work).
    const STATUS_PUBLISH_CONCURRENCY: usize = 256;

    /// R6: do coold's advertised `[min,max]` schema range and flux's
    /// `[FLUX_SCHEMA_MIN, FLUX_SCHEMA_MAX]` overlap?
    pub(super) fn schema_overlaps(
        coold_min: u32,
        coold_max: u32,
        flux_min: u32,
        flux_max: u32,
    ) -> bool {
        coold_min <= flux_max && coold_max >= flux_min
    }

    /// Reject `0.0.0.0` / `::` binds unless explicitly opted in. The gRPC
    /// stream carries a JWT bearer in cleartext (no TLS layer here yet), so
    /// listening on every interface lets anyone on path capture and replay
    /// the token until `exp`. Production must bind a specific interface IP
    /// (typically the WireGuard mgmt IP — `host_id` already equals it).
    pub(super) fn validate_bind(addr: SocketAddr, allow_public: bool) -> Result<()> {
        if addr.ip().is_unspecified() && !allow_public {
            anyhow::bail!(
                "refusing to bind {addr}: COOLIFY_FLUX_GRPC_BIND must be a specific \
                 interface IP (typically the WireGuard mgmt IP). Set \
                 COOLIFY_FLUX_ALLOW_PUBLIC_BIND=1 to override (dev only — JWTs \
                 cross the wire in cleartext)."
            );
        }
        Ok(())
    }

    fn allow_public_bind() -> bool {
        std::env::var("COOLIFY_FLUX_ALLOW_PUBLIC_BIND")
            .ok()
            .as_deref()
            == Some("1")
    }

    /// Wildcard "authorize-all" profile strings historically minted by Laravel.
    /// See [`effective_capabilities`] for how they are (not) honored.
    fn capability_profile_authorizes_all(capability: &str) -> bool {
        matches!(capability, "*" | "host-agent:dev" | "host-agent:default")
    }

    /// Whether a JWT `caps` list contains any wildcard-profile string.
    pub(super) fn contains_wildcard_profile(jwt_caps: &[String]) -> bool {
        jwt_caps
            .iter()
            .any(|capability| capability_profile_authorizes_all(capability))
    }

    /// S2: compute the capabilities a stream is authorized for.
    ///
    /// Secure-by-default (`allow_wildcard = false`): the JWT `caps` claim is
    /// authoritative — flux authorizes ONLY the intersection of the JWT `caps`
    /// with the host's advertised primitives. Wildcard-profile strings
    /// (`*`, `host-agent:dev`, `host-agent:default`) are NOT primitives, so they
    /// match nothing and grant nothing. This makes the per-verb `caps` claim
    /// real instead of a no-op.
    ///
    /// Escape hatch (`allow_wildcard = true`, via
    /// `COOLIFY_FLUX_ALLOW_WILDCARD_CAPABILITIES=1`): a wildcard-profile string
    /// expands to the full advertised set (the legacy dev/rollback behavior).
    fn effective_capabilities(
        advertised: &[String],
        jwt_caps: &[String],
        allow_wildcard: bool,
    ) -> Vec<String> {
        if allow_wildcard && contains_wildcard_profile(jwt_caps) {
            return advertised.to_vec();
        }

        advertised
            .iter()
            .filter(|capability| jwt_caps.iter().any(|jwt_cap| jwt_cap == *capability))
            .cloned()
            .collect()
    }

    fn missing_capabilities(advertised: &[String], effective: &[String]) -> Vec<String> {
        advertised
            .iter()
            .filter(|capability| {
                !effective
                    .iter()
                    .any(|effective_cap| effective_cap == *capability)
            })
            .cloned()
            .collect()
    }

    pub async fn run(
        config: Config,
        streams: Streams,
        pending: Pending,
        jwt_keys: Arc<JwtKeys>,
        revocations: RevocationStore,
    ) -> Result<()> {
        let addr: SocketAddr = config
            .grpc_bind
            .parse()
            .with_context(|| format!("parse COOLIFY_FLUX_GRPC_BIND={}", config.grpc_bind))?;

        let allow_public = allow_public_bind();
        validate_bind(addr, allow_public)?;
        if addr.ip().is_unspecified() {
            warn!(
                %addr,
                "COOLIFY_FLUX_ALLOW_PUBLIC_BIND=1 — binding on every interface; \
                 JWTs cross the wire unencrypted",
            );
        }

        // S1: optional defense-in-depth TLS. The WireGuard mesh is the
        // confidentiality boundary today, so TLS is OFF unless BOTH cert and
        // key paths are provisioned (by coolify-cli). When unset the listener
        // stays plaintext — nothing changes for existing deployments.
        let tls = build_tls_config(&config)?;

        let registry = RegistryClient::from_config(&config);
        let resource_status = ResourceStatusPublisher::new(
            config.laravel_api_url.clone(),
            config.laravel_api_token.clone(),
        );
        let svc = FluxAgent {
            config,
            streams,
            pending,
            jwt_keys,
            revocations,
            registry,
            resource_status,
            status_semaphore: Arc::new(Semaphore::new(STATUS_PUBLISH_CONCURRENCY)),
        };

        let mut builder = Server::builder();
        if let Some(tls) = tls {
            info!(%addr, "gRPC server listening (TLS enabled)");
            builder = builder
                .tls_config(tls)
                .context("configure gRPC TLS from COOLIFY_FLUX_TLS_CERT_PATH/KEY_PATH")?;
        } else {
            info!(%addr, "gRPC server listening (plaintext; WireGuard is the confidentiality boundary)");
        }
        builder
            .add_service(AgentServer::new(svc))
            .serve(addr)
            .await?;
        Ok(())
    }

    /// S1: read the TLS cert+key when both paths are configured. Returns `None`
    /// (plaintext) when either is unset.
    fn build_tls_config(config: &Config) -> Result<Option<ServerTlsConfig>> {
        match (&config.tls_cert_path, &config.tls_key_path) {
            (Some(cert_path), Some(key_path)) => {
                let cert = std::fs::read(cert_path)
                    .with_context(|| format!("read TLS cert {}", cert_path.display()))?;
                let key = std::fs::read(key_path)
                    .with_context(|| format!("read TLS key {}", key_path.display()))?;
                let identity = Identity::from_pem(cert, key);
                Ok(Some(ServerTlsConfig::new().identity(identity)))
            }
            (None, None) => Ok(None),
            _ => anyhow::bail!(
                "TLS misconfigured: set BOTH COOLIFY_FLUX_TLS_CERT_PATH and \
                 COOLIFY_FLUX_TLS_KEY_PATH, or neither"
            ),
        }
    }

    struct FluxAgent {
        config: Config,
        streams: Streams,
        pending: Pending,
        jwt_keys: Arc<JwtKeys>,
        revocations: RevocationStore,
        registry: Option<RegistryClient>,
        resource_status: ResourceStatusPublisher,
        /// C3: bounds concurrent status-update publish tasks (see const).
        status_semaphore: Arc<Semaphore>,
    }

    type ServerMsgStream = Pin<Box<dyn Stream<Item = Result<ServerMsg, Status>> + Send + 'static>>;

    #[tonic::async_trait]
    impl Agent for FluxAgent {
        type StreamStream = ServerMsgStream;

        async fn stream(
            &self,
            request: Request<Streaming<ClientMsg>>,
        ) -> Result<Response<Self::StreamStream>, Status> {
            // Generic message to client; full reason logged server-side.
            // Distinct strings per failure mode (expired vs bad-sig vs wrong-aud)
            // would give a credential-stuffing oracle.
            let jwt = request
                .metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .ok_or_else(|| {
                    warn!("gRPC stream rejected: missing or malformed Authorization");
                    Status::unauthenticated("invalid credentials")
                })?;

            let now = auth::unix_now();
            let verified = auth::verify_jwt(
                jwt,
                &self.jwt_keys,
                &self.revocations,
                now,
                self.config.max_token_lifetime_secs,
            )
            .map_err(|e| {
                warn!(
                    error = format!("{e:#}"),
                    "gRPC stream rejected: JWT verification failed"
                );
                Status::unauthenticated("invalid credentials")
            })?;

            let host_id = verified.host_id.clone();
            let jwt_caps = verified.caps;
            // #4: bound the stream's lifetime to the token's exp.
            let token_exp = verified.exp;
            let team_id = verified.team_id;

            // #2: require a tenant (team_id) claim so every stream is scoped to
            // a tenant. Default-on; toggled by COOLIFY_FLUX_REQUIRE_TEAM_ID.
            if !auth::team_id_satisfied(team_id.as_deref(), self.config.require_team_id) {
                warn!(
                    %host_id,
                    "gRPC stream rejected: missing or blank team_id (tenant) claim"
                );
                return Err(Status::unauthenticated("invalid credentials"));
            }

            // #2: capture the transport peer address before consuming the
            // request — it is the strongest host-binding signal (a stolen token
            // replayed from a different host IP is caught here).
            let peer_ip = request.remote_addr().map(|addr| addr.ip());

            let mut inbound = request.into_inner();
            let hello = match inbound.next().await {
                Some(Ok(ClientMsg {
                    payload: Some(client_msg::Payload::Hello(hello)),
                })) => hello,
                Some(Ok(_)) | None => {
                    warn!(%host_id, "gRPC stream rejected: first message was not Hello");
                    return Err(Status::invalid_argument("hello required"));
                }
                Some(Err(e)) => {
                    warn!(%host_id, error = %e, "gRPC stream rejected: failed to read Hello");
                    return Err(e);
                }
            };

            // R6: reject at Hello when coold's advertised schema range does not
            // overlap flux's supported range, instead of failing per-verb later.
            if !schema_overlaps(
                hello.schema_min,
                hello.schema_max,
                FLUX_SCHEMA_MIN,
                FLUX_SCHEMA_MAX,
            ) {
                warn!(
                    %host_id,
                    coold_schema_min = hello.schema_min,
                    coold_schema_max = hello.schema_max,
                    flux_schema_min = FLUX_SCHEMA_MIN,
                    flux_schema_max = FLUX_SCHEMA_MAX,
                    "gRPC stream rejected: schema range mismatch"
                );
                return Err(Status::failed_precondition(
                    "coold/flux schema versions are incompatible",
                ));
            }

            // #2: bind the token to the host presenting it. Prefer the gRPC
            // transport peer IP (strongest); fall back to the Hello-advertised
            // host_mgmt_ip when the peer addr is unavailable. Mismatch → reject
            // when COOLIFY_FLUX_REQUIRE_HOST_BINDING is on (default).
            match auth::decide_host_binding(
                &host_id,
                &hello.host_mgmt_ip,
                peer_ip,
                self.config.require_host_binding,
            ) {
                Ok(auth::HostBinding::Transport) => {
                    debug!(%host_id, ?peer_ip, "host binding verified against transport peer IP");
                }
                Ok(auth::HostBinding::HelloFallback { reason }) => {
                    warn!(
                        %host_id,
                        reason,
                        advertised_mgmt_ip = %hello.host_mgmt_ip,
                        "host binding verified only against self-asserted Hello host_mgmt_ip; \
                         transport-level binding unavailable"
                    );
                }
                Ok(auth::HostBinding::Unenforced { detail }) => {
                    warn!(
                        %host_id,
                        detail,
                        "host binding failed but COOLIFY_FLUX_REQUIRE_HOST_BINDING is off; \
                         allowing stream"
                    );
                }
                Err(reason) => {
                    warn!(%host_id, reason, "gRPC stream rejected: host binding mismatch");
                    return Err(Status::unauthenticated("invalid credentials"));
                }
            }

            // S2: a wildcard-profile token grants nothing unless the escape
            // hatch is on — make the misconfiguration visible by naming the host.
            if !self.config.allow_wildcard_capabilities && contains_wildcard_profile(&jwt_caps) {
                warn!(
                    %host_id,
                    "JWT carries a wildcard capability profile but \
                     COOLIFY_FLUX_ALLOW_WILDCARD_CAPABILITIES is off; it grants no \
                     capabilities — mint an explicit caps list for this host"
                );
            }

            let effective_caps = effective_capabilities(
                &hello.capabilities,
                &jwt_caps,
                self.config.allow_wildcard_capabilities,
            );
            let missing_caps = missing_capabilities(&hello.capabilities, &effective_caps);

            let team_id_log = team_id.as_deref().unwrap_or("<none>");
            if missing_caps.is_empty() {
                info!(
                    %host_id,
                    team_id = %team_id_log,
                    version = %hello.coold_version,
                    capabilities = ?effective_caps,
                    "coold stream connected"
                );
            } else {
                warn!(
                    %host_id,
                    team_id = %team_id_log,
                    version = %hello.coold_version,
                    effective_capabilities = ?effective_caps,
                    unauthorized_capabilities = ?missing_caps,
                    "coold stream connected with unauthorized advertised capabilities"
                );
            }

            let (cmd_tx, cmd_rx) = mpsc::channel::<ServerMsg>(64);
            self.streams.insert(
                host_id.clone(),
                StreamHandle {
                    tx: cmd_tx,
                    caps: effective_caps.clone(),
                    advertised_caps: hello.capabilities.clone(),
                },
            );

            let streams = self.streams.clone();
            let pending = self.pending.clone();
            let registry = self.registry.clone();
            let resource_status = self.resource_status.clone();
            let status_semaphore = self.status_semaphore.clone();
            let host_id_clone = host_id.clone();
            {
                let publisher = resource_status.clone();
                let host_id = host_id.clone();
                tokio::spawn(async move {
                    publisher
                        .publish(coold_status_update(
                            &host_id,
                            "installed",
                            "coold stream connected.",
                        ))
                        .await;
                });
            }
            if let Some(registry) = registry.clone() {
                let host_id = host_id.clone();
                let capabilities = effective_caps.clone();
                let coold_version = hello.coold_version.clone();
                tokio::spawn(async move {
                    if let Err(e) = registry
                        .upsert_connection(&host_id, capabilities, Some(coold_version))
                        .await
                    {
                        warn!(
                            host_id = %host_id,
                            error = format!("{e:#}"),
                            "Laravel agent connection upsert failed",
                        );
                    }
                });
            }

            tokio::spawn(async move {
                let mut disconnect_reason = "stream_closed";
                // #4: enforce the token's exp as a hard cap on stream lifetime.
                // verify_jwt only runs at connect; without this a token valid at
                // connect would authorize the stream forever. When exp is
                // reached we drop the stream — coold re-reads its JWT file and
                // reconnects with a fresh token.
                let ttl = auth::seconds_until_expiry(token_exp, auth::unix_now());
                let expiry = tokio::time::sleep(std::time::Duration::from_secs(ttl));
                tokio::pin!(expiry);
                loop {
                    let msg = tokio::select! {
                        _ = &mut expiry => {
                            warn!(
                                host_id = %host_id_clone,
                                "host JWT expired, dropping stream; coold will reconnect with a fresh token"
                            );
                            disconnect_reason = "token_expired";
                            break;
                        }
                        msg = inbound.next() => msg,
                    };
                    let Some(msg) = msg else {
                        break;
                    };
                    match msg {
                        Ok(ClientMsg {
                            payload: Some(client_msg::Payload::Response(resp)),
                        }) => {
                            deliver_response(&pending, &host_id_clone, resp);
                        }
                        Ok(ClientMsg {
                            payload: Some(client_msg::Payload::ResourceStatusUpdate(update)),
                        }) => {
                            // C3: bound concurrent publish tasks.
                            spawn_bounded_publish(
                                &status_semaphore,
                                resource_status.clone(),
                                update,
                            );
                        }
                        Ok(ClientMsg {
                            payload: Some(client_msg::Payload::Pong(_)),
                        }) => {
                            if streams.touch(&host_id_clone) {
                                // C3: bound concurrent publish tasks.
                                spawn_bounded_publish(
                                    &status_semaphore,
                                    resource_status.clone(),
                                    coold_status_update(
                                        &host_id_clone,
                                        "installed",
                                        "coold heartbeat restored.",
                                    ),
                                );
                            }
                        }
                        Ok(ClientMsg {
                            payload: Some(client_msg::Payload::Hello(_)),
                        }) => warn!(host_id = %host_id_clone, "duplicate Hello ignored"),
                        Ok(_) => {}
                        Err(e) => {
                            warn!(host_id = %host_id_clone, error = %e, "stream recv error");
                            disconnect_reason = "stream_error";
                            break;
                        }
                    }
                }
                info!(host_id = %host_id_clone, "coold stream disconnected");
                streams.remove(&host_id_clone);
                resource_status
                    .publish(coold_status_update(
                        &host_id_clone,
                        "unreachable",
                        "coold stream disconnected.",
                    ))
                    .await;
                if let Some(registry) = registry {
                    if let Err(e) = registry.disconnect(&host_id_clone, disconnect_reason).await {
                        warn!(
                            host_id = %host_id_clone,
                            error = format!("{e:#}"),
                            "Laravel agent disconnect report failed",
                        );
                    }
                }
            });

            let outbound = ReceiverStream::new(cmd_rx).map(Ok);
            Ok(Response::new(Box::pin(outbound)))
        }
    }

    /// Map a coold gRPC `Response` to `ResponseData` and hand it to the
    /// pending entry. Unknown request_id → drop.
    fn deliver_response(
        pending: &Pending,
        host_id: &str,
        resp: coolify_proto::agent::v1::Response,
    ) {
        let request_id = resp.request_id.clone();
        let response_type = response_body_type(resp.body.as_ref());
        info!(%request_id, %host_id, %response_type, "coold response received from stream");

        let kind = match pending.get(&request_id) {
            Some(e) => e.kind,
            None => {
                warn!(%request_id, %host_id, %response_type, "response for unknown request_id; dropping");
                return;
            }
        };

        let resp = coolify_proto::agent::v1::Response {
            request_id: request_id.clone(),
            body: resp.body,
        };
        let Some(body) = ResponseBody::try_from_proto(resp) else {
            warn!(%request_id, %host_id, %response_type, ?kind, "unsupported response body; dropping");
            return;
        };
        let data = ResponseData::Coold(body);

        pending.deliver(&request_id, data);
        info!(%request_id, %host_id, %response_type, ?kind, "coold response delivered to pending dispatch");
    }

    fn response_body_type(body: Option<&response::Body>) -> &'static str {
        match body {
            Some(response::Body::ImagesPull(_)) => "images.pull",
            Some(response::Body::ImagesList(_)) => "images.list",
            Some(response::Body::ImagesDelete(_)) => "images.delete",
            Some(response::Body::ContainersCreate(_)) => "containers.create",
            Some(response::Body::ContainersStart(_)) => "containers.start",
            Some(response::Body::ContainersStop(_)) => "containers.stop",
            Some(response::Body::ContainersRestart(_)) => "containers.restart",
            Some(response::Body::ContainersDelete(_)) => "containers.delete",
            Some(response::Body::ContainersInspect(_)) => "containers.inspect",
            Some(response::Body::ContainersList(_)) => "containers.list",
            Some(response::Body::ContainersLogs(_)) => "containers.logs",
            Some(response::Body::ContainersExec(_)) => "containers.exec",
            Some(response::Body::ContainersHealthcheckRun(_)) => "containers.healthcheck.run",
            Some(response::Body::IngressApply(_)) => "ingress.apply",
            Some(response::Body::IngressStop(_)) => "ingress.stop",
            Some(response::Body::FirewallAllow(_)) => "firewall.allow",
            Some(response::Body::FirewallRevoke(_)) => "firewall.revoke",
            Some(response::Body::FirewallList(_)) => "firewall.list",
            Some(response::Body::FirewallReconcile(_)) => "firewall.reconcile",
            Some(response::Body::CooldLogs(_)) => "coold.logs",
            Some(response::Body::CorrosionTables(_)) => "corrosion.tables",
            Some(response::Body::HostJwtSet(_)) => "host.jwt.set",
            Some(response::Body::Error(_)) => "error",
            None => "none",
        }
    }

    /// C3: spawn a status-update publish task only if a concurrency permit is
    /// available; otherwise coalesce (drop) it. A flooding host thus cannot
    /// amplify unbounded background tasks against Laravel — status is
    /// best-effort and the next update supersedes a dropped one.
    fn spawn_bounded_publish(
        semaphore: &Arc<Semaphore>,
        publisher: ResourceStatusPublisher,
        update: coolify_proto::agent::v1::ResourceStatusUpdate,
    ) {
        match Arc::clone(semaphore).try_acquire_owned() {
            Ok(permit) => {
                tokio::spawn(async move {
                    let _permit = permit;
                    publisher.publish(update).await;
                });
            }
            Err(_) => {
                debug!(
                    host_id = %update.host_id,
                    "status publish concurrency cap reached; coalescing (dropping) update"
                );
            }
        }
    }

    pub(super) fn coold_status_update(
        host_id: &str,
        status: &str,
        status_message: &str,
    ) -> coolify_proto::agent::v1::ResourceStatusUpdate {
        coolify_proto::agent::v1::ResourceStatusUpdate {
            resource_type: "server".into(),
            host_id: host_id.into(),
            container_id: String::new(),
            container_name: String::new(),
            status: status.into(),
            status_message: status_message.into(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{
            contains_wildcard_profile, coold_status_update, effective_capabilities,
            missing_capabilities, schema_overlaps, validate_bind, FLUX_SCHEMA_MAX, FLUX_SCHEMA_MIN,
        };
        use std::net::SocketAddr;

        fn parse(s: &str) -> SocketAddr {
            s.parse().unwrap()
        }

        #[test]
        fn rejects_ipv4_unspecified_without_override() {
            let err = validate_bind(parse("0.0.0.0:6443"), false).unwrap_err();
            assert!(err.to_string().contains("refusing to bind"), "got: {err}");
        }

        #[test]
        fn rejects_ipv6_unspecified_without_override() {
            let err = validate_bind(parse("[::]:6443"), false).unwrap_err();
            assert!(err.to_string().contains("refusing to bind"), "got: {err}");
        }

        #[test]
        fn accepts_ipv4_unspecified_with_override() {
            validate_bind(parse("0.0.0.0:6443"), true).unwrap();
        }

        #[test]
        fn accepts_ipv6_unspecified_with_override() {
            validate_bind(parse("[::]:6443"), true).unwrap();
        }

        #[test]
        fn accepts_specific_ipv4_without_override() {
            validate_bind(parse("10.42.0.1:6443"), false).unwrap();
        }

        #[test]
        fn accepts_loopback_without_override() {
            validate_bind(parse("127.0.0.1:6443"), false).unwrap();
        }

        #[test]
        fn missing_hello_capabilities_are_degraded_not_rejected() {
            let advertised = vec!["containers.list".to_string(), "coold.logs".to_string()];
            let jwt_caps = vec!["containers.list".to_string()];

            let effective = effective_capabilities(&advertised, &jwt_caps, false);

            assert_eq!(effective, vec!["containers.list"]);
        }

        // S2: with the escape hatch OFF (production default), a wildcard-profile
        // token grants NOTHING — the caps claim is authoritative.
        #[test]
        fn wildcard_profile_grants_nothing_when_flag_off() {
            let advertised = vec!["containers.list".to_string(), "coold.logs".to_string()];
            let jwt_caps = vec!["*".to_string()];

            let effective = effective_capabilities(&advertised, &jwt_caps, false);

            assert!(effective.is_empty());
        }

        // S2: an explicit caps list still works with the flag OFF — exactly the
        // intersection with advertised is authorized.
        #[test]
        fn wildcard_profile_plus_explicit_caps_authorizes_only_explicit_when_flag_off() {
            let advertised = vec!["containers.list".to_string(), "coold.logs".to_string()];
            let jwt_caps = vec!["*".to_string(), "containers.list".to_string()];

            let effective = effective_capabilities(&advertised, &jwt_caps, false);

            assert_eq!(effective, vec!["containers.list"]);
        }

        // S2: with the escape hatch ON, wildcard expands to all advertised.
        #[test]
        fn wildcard_profile_authorizes_all_advertised_when_flag_on() {
            let advertised = vec!["containers.list".to_string(), "coold.logs".to_string()];
            let jwt_caps = vec!["*".to_string()];

            let effective = effective_capabilities(&advertised, &jwt_caps, true);

            assert_eq!(effective, advertised);
        }

        #[test]
        fn detects_wildcard_profile_strings() {
            assert!(contains_wildcard_profile(&["*".to_string()]));
            assert!(contains_wildcard_profile(&["host-agent:dev".to_string()]));
            assert!(contains_wildcard_profile(&[
                "host-agent:default".to_string()
            ]));
            assert!(!contains_wildcard_profile(&["containers.list".to_string()]));
        }

        #[test]
        fn explicitly_granted_capabilities_are_allowed() {
            let advertised = vec!["containers.list".to_string(), "ingress.apply".to_string()];
            let jwt_caps = vec!["containers.list".to_string(), "ingress.apply".to_string()];

            assert_eq!(
                effective_capabilities(&advertised, &jwt_caps, false),
                advertised
            );
        }

        #[test]
        fn missing_capability_is_left_out_of_effective_capabilities() {
            let advertised = vec!["containers.list".to_string(), "ingress.apply".to_string()];
            let jwt_caps = vec!["containers.list".to_string()];
            let effective = effective_capabilities(&advertised, &jwt_caps, false);

            assert_eq!(effective, vec!["containers.list"]);
            assert_eq!(
                missing_capabilities(&advertised, &effective),
                vec!["ingress.apply"]
            );
        }

        // R6: schema negotiation.
        #[test]
        fn schema_ranges_overlap_is_accepted() {
            assert!(schema_overlaps(1, 1, FLUX_SCHEMA_MIN, FLUX_SCHEMA_MAX));
            assert!(schema_overlaps(1, 3, FLUX_SCHEMA_MIN, FLUX_SCHEMA_MAX));
            assert!(schema_overlaps(0, 1, FLUX_SCHEMA_MIN, FLUX_SCHEMA_MAX));
        }

        #[test]
        fn schema_ranges_without_overlap_are_rejected() {
            // coold only speaks schema 2+, flux caps at 1.
            assert!(!schema_overlaps(2, 3, FLUX_SCHEMA_MIN, FLUX_SCHEMA_MAX));
            // coold unset (0,0) while flux requires >= 1.
            assert!(!schema_overlaps(0, 0, FLUX_SCHEMA_MIN, FLUX_SCHEMA_MAX));
        }

        #[test]
        fn builds_server_status_update_for_coold_liveness() {
            let update =
                coold_status_update("100.64.0.5", "unreachable", "coold heartbeat timed out.");

            assert_eq!(update.resource_type, "server");
            assert_eq!(update.host_id, "100.64.0.5");
            assert_eq!(update.status, "unreachable");
            assert_eq!(update.status_message, "coold heartbeat timed out.");
        }
    }
}

mod coold_heartbeat {
    use anyhow::Result;
    use coolify_proto::agent::v1::{server_msg, Ping, ServerMsg};
    use tracing::warn;
    use uuid::Uuid;

    use crate::{
        config::Config, grpc_server::coold_status_update, resource_status::ResourceStatusPublisher,
        state::Streams,
    };

    pub async fn run(config: Config, streams: Streams) -> Result<()> {
        let publisher = ResourceStatusPublisher::new(
            config.laravel_api_url.clone(),
            config.laravel_api_token.clone(),
        );
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
            config.coold_ping_interval_secs.max(1),
        ));
        let timeout = std::time::Duration::from_secs(config.coold_pong_timeout_secs.max(1));

        loop {
            ticker.tick().await;

            for host_id in streams.mark_stale(timeout) {
                publisher
                    .publish(coold_status_update(
                        &host_id,
                        "unreachable",
                        "coold heartbeat timed out.",
                    ))
                    .await;
            }

            for (host_id, tx) in streams.ping_targets() {
                let request_id = Uuid::new_v4().to_string();
                if tx
                    .send(ServerMsg {
                        request_id,
                        command: Some(server_msg::Command::Ping(Ping {})),
                    })
                    .await
                    .is_err()
                {
                    warn!(%host_id, "coold heartbeat ping send failed");
                }
            }
        }
    }
}

mod pending_sweeper {
    use anyhow::Result;
    use tracing::warn;

    use crate::{
        config::Config,
        state::{Pending, PendingKind},
    };

    pub async fn run(config: Config, pending: Pending) -> Result<()> {
        let interval = std::time::Duration::from_secs(1);
        let dispatch_timeout = std::time::Duration::from_secs(config.dispatch_timeout_secs);
        loop {
            tokio::time::sleep(interval).await;
            let expired = pending.drain_expired(dispatch_timeout);
            for (request_id, entry) in expired {
                if matches!(entry.kind, PendingKind::Coold) {
                    warn!(
                        %request_id,
                        timeout_secs = config.dispatch_timeout_secs,
                        "coold dispatch timed out; handler will return 504"
                    );
                }
            }
        }
    }
}
