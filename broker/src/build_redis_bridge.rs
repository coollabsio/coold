/// Redis bridge for build dispatch. Consumes `build:cmd` from Laravel,
/// routes `BuildRequest` / `CancelBuild` over the same coold stream a
/// builder-capable host already holds open on :6443, and pushes responses
/// back to `build:resp:{request_id}` for Laravel to BLPOP.
///
/// Routing contract:
///   * For a new build, envelope may specify `host_id` to pin it to a
///     particular builder-capable host; otherwise the broker picks the
///     first connected host that advertises the `builder` capability.
///   * For a cancel, the envelope specifies only `request_id`; the broker
///     looks up the host that owns that pending request and sends
///     `CancelBuild` down its stream.
use anyhow::Result;
use redis::streams::{StreamReadOptions, StreamReadReply};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use coolify_proto::agent::v1::{
    server_msg, BuildRequest, BuildResponseBody as ProtoBuildResponseBody, CancelBuild, ServerMsg,
    StaticConfig,
};

use crate::{
    config::Config,
    state::{Pending, PendingKind, Streams},
};

const BUILD_CMD_STREAM: &str = "build:cmd";
const CONSUMER_GROUP: &str = "broker";
const CONSUMER_NAME: &str = "broker-1";
const BLOCK_MS: usize = 5000;
const RESP_TTL_SECS: i64 = 30;

#[derive(Debug, Deserialize)]
struct BuildDispatchEnvelope {
    /// Target host_id. Optional for `static_build` (load-balanced among
    /// builder-capable hosts when absent); required for `cancel` only
    /// indirectly via the `request_id` lookup in `Pending`.
    #[serde(default)]
    host_id: Option<String>,
    request_id: String,
    command: BuildCommandPayload,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BuildCommandPayload {
    StaticBuild {
        repo_url: String,
        git_ref: String,
        target_image: String,
        #[serde(default)]
        output_dir: Option<String>,
        #[serde(default)]
        base_image: Option<String>,
    },
    Cancel {},
}

#[derive(Debug, Serialize)]
struct BuildResponseEnvelope {
    request_id: String,
    #[serde(flatten)]
    body: BuildResponseBody,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BuildResponseBody {
    Ok { digest: String, registry_ref: String, duration_ms: u64 },
    Error { code: u32, message: String, stage: String },
}

impl BuildResponseBody {
    pub fn from_proto(body: ProtoBuildResponseBody) -> Self {
        use coolify_proto::agent::v1::build_response_body;
        match body.body {
            Some(build_response_body::Body::Ok(r)) => BuildResponseBody::Ok {
                digest: r.digest,
                registry_ref: r.registry_ref,
                duration_ms: r.duration_ms,
            },
            Some(build_response_body::Body::Err(e)) => BuildResponseBody::Error {
                code: e.code,
                message: e.message,
                stage: e.stage,
            },
            None => BuildResponseBody::Error {
                code: 500,
                message: "empty build response body".into(),
                stage: String::new(),
            },
        }
    }
}

pub async fn run(config: Config, streams: Streams, pending: Pending) -> Result<()> {
    let client = redis::Client::open(config.redis_url.as_str())?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    let _: Result<(), _> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(BUILD_CMD_STREAM)
        .arg(CONSUMER_GROUP)
        .arg("$")
        .arg("MKSTREAM")
        .query_async(&mut conn)
        .await;

    info!("build Redis bridge running, consuming {BUILD_CMD_STREAM}");

    let opts = StreamReadOptions::default()
        .group(CONSUMER_GROUP, CONSUMER_NAME)
        .count(16)
        .block(BLOCK_MS);

    loop {
        let reply: StreamReadReply = match conn
            .xread_options::<_, _, StreamReadReply>(&[BUILD_CMD_STREAM], &[">"], &opts)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "build xreadgroup failed; retrying");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        for key in reply.keys {
            for entry in key.ids {
                let stream_id = entry.id.clone();
                let json = match entry.map.get("payload") {
                    Some(redis::Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
                    Some(redis::Value::SimpleString(s)) => s.clone(),
                    _ => {
                        warn!(%stream_id, "build:cmd missing payload; skipping");
                        ack(&mut conn, &stream_id).await;
                        continue;
                    }
                };

                let envelope: BuildDispatchEnvelope = match serde_json::from_str(&json) {
                    Ok(e) => e,
                    Err(e) => {
                        warn!(%stream_id, error = %e, "malformed build dispatch envelope; skipping");
                        ack(&mut conn, &stream_id).await;
                        continue;
                    }
                };

                dispatch(&streams, &mut conn, &pending, envelope).await;
                ack(&mut conn, &stream_id).await;
            }
        }
    }
}

pub async fn push_build_response(redis_url: &str, request_id: &str, body: BuildResponseBody) -> Result<()> {
    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;
    let envelope = BuildResponseEnvelope { request_id: request_id.to_owned(), body };
    let json = serde_json::to_string(&envelope)?;
    let key = format!("build:resp:{request_id}");
    conn.lpush::<_, _, ()>(&key, &json).await?;
    conn.expire::<_, ()>(&key, RESP_TTL_SECS).await?;
    Ok(())
}

/// Outcome of pure routing — what the caller should do. Separating this from
/// Redis + stream I/O keeps the routing logic unit-testable without mocking
/// either. The async driver (`dispatch`) then turns each outcome into the
/// appropriate side effects.
#[derive(Debug)]
pub(crate) enum RouteOutcome {
    /// Send a build request to the given host.
    SendBuild { host_id: String, msg: ServerMsg },
    /// Send a cancel for an in-flight request to its owning host.
    SendCancel { host_id: String, msg: ServerMsg },
    /// Push an error envelope back to Laravel.
    PushError { code: u32, message: &'static str },
    /// Cancel target is gone (host disconnected mid-flight). Log-only;
    /// Laravel will already have seen the original build's final response.
    DropCancelHostGone { host_id: String },
}

/// Pure routing logic for a `build:cmd` envelope. Consumes the envelope and
/// returns a `RouteOutcome` describing the next action. No I/O; no mutation
/// of `streams`. `Pending` is mutated on build dispatch so timeout sweeping
/// continues to work — that's the one piece of state the router needs to
/// produce.
pub(crate) fn route(
    streams: &Streams,
    pending: &Pending,
    env: BuildDispatchEnvelope,
) -> RouteOutcome {
    match env.command {
        BuildCommandPayload::StaticBuild {
            repo_url,
            git_ref,
            target_image,
            output_dir,
            base_image,
        } => {
            let target_host = match env.host_id.as_deref() {
                Some(id) => {
                    if !streams.has_cap(id, "builder") {
                        return RouteOutcome::PushError {
                            code: 503,
                            message: "host has no builder capability",
                        };
                    }
                    id.to_string()
                }
                None => match streams.pick_host_with_cap("builder") {
                    Some(id) => id,
                    None => {
                        return RouteOutcome::PushError {
                            code: 503,
                            message: "no builder-capable host connected",
                        };
                    }
                },
            };

            let build_req = BuildRequest {
                repo_url,
                git_ref,
                stack: coolify_proto::agent::v1::BuildStack::Static as i32,
                target_image,
                cache_key: String::new(),
                static_cfg: Some(StaticConfig {
                    output_dir: output_dir.unwrap_or_else(|| "dist".into()),
                    base_image: base_image.unwrap_or_else(|| "docker.io/library/nginx:alpine".into()),
                }),
            };
            let msg = ServerMsg {
                request_id: env.request_id.clone(),
                command: Some(server_msg::Command::Build(build_req)),
            };
            pending.insert(env.request_id.clone(), target_host.clone(), PendingKind::Build);
            RouteOutcome::SendBuild { host_id: target_host, msg }
        }
        BuildCommandPayload::Cancel {} => {
            let Some(entry) = pending.get(&env.request_id) else {
                return RouteOutcome::PushError {
                    code: 404,
                    message: "request_id not in flight",
                };
            };
            if !streams.has_cap(&entry.host_id, "builder") {
                return RouteOutcome::DropCancelHostGone { host_id: entry.host_id };
            }
            let msg = ServerMsg {
                request_id: env.request_id.clone(),
                command: Some(server_msg::Command::CancelBuild(CancelBuild {})),
            };
            RouteOutcome::SendCancel { host_id: entry.host_id, msg }
        }
    }
}

async fn dispatch(
    streams: &Streams,
    conn: &mut impl AsyncCommands,
    pending: &Pending,
    env: BuildDispatchEnvelope,
) {
    let request_id = env.request_id.clone();
    match route(streams, pending, env) {
        RouteOutcome::SendBuild { host_id, msg } => {
            let Some(tx) = streams.get_tx(&host_id) else {
                pending.remove(&request_id);
                push_error(conn, &request_id, 503, "builder host disconnected").await;
                return;
            };
            if let Err(e) = tx.send(msg).await {
                pending.remove(&request_id);
                warn!(%request_id, error = %e, "send build to host stream failed");
                push_error(conn, &request_id, 503, "host stream send failed").await;
            }
        }
        RouteOutcome::SendCancel { host_id, msg } => {
            let Some(tx) = streams.get_tx(&host_id) else {
                warn!(%request_id, %host_id, "cancel: owning host disconnected");
                return;
            };
            if let Err(e) = tx.send(msg).await {
                warn!(%request_id, error = %e, "send cancel to host stream failed");
            }
        }
        RouteOutcome::PushError { code, message } => {
            warn!(%request_id, %code, %message, "build dispatch rejected");
            push_error(conn, &request_id, code, message).await;
        }
        RouteOutcome::DropCancelHostGone { host_id } => {
            warn!(%request_id, %host_id, "cancel: owning host lost builder cap");
        }
    }
}

async fn push_error(conn: &mut impl AsyncCommands, request_id: &str, code: u32, msg: &str) {
    let envelope = BuildResponseEnvelope {
        request_id: request_id.to_owned(),
        body: BuildResponseBody::Error {
            code,
            message: msg.to_owned(),
            stage: String::new(),
        },
    };
    let json = serde_json::to_string(&envelope).unwrap_or_default();
    let key = format!("build:resp:{request_id}");
    let _: Result<(), _> = conn.lpush(&key, &json).await;
    let _: Result<(), _> = conn.expire(&key, RESP_TTL_SECS).await;
}

async fn ack(conn: &mut impl AsyncCommands, stream_id: &str) {
    let _: Result<(), _> = redis::cmd("XACK")
        .arg(BUILD_CMD_STREAM)
        .arg(CONSUMER_GROUP)
        .arg(stream_id)
        .query_async(conn)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{PendingKind, StreamHandle};
    use tokio::sync::mpsc;

    fn insert_host(streams: &Streams, host_id: &str, caps: &[&str]) -> mpsc::Receiver<ServerMsg> {
        let (tx, rx) = mpsc::channel::<ServerMsg>(16);
        streams.insert(
            host_id.to_owned(),
            StreamHandle {
                tx,
                caps: caps.iter().map(|c| c.to_string()).collect(),
                builder_capacity: 2,
            },
        );
        rx
    }

    fn static_env(request_id: &str, host_id: Option<&str>) -> BuildDispatchEnvelope {
        BuildDispatchEnvelope {
            host_id: host_id.map(str::to_owned),
            request_id: request_id.to_owned(),
            command: BuildCommandPayload::StaticBuild {
                repo_url: "https://example.com/repo".into(),
                git_ref: "main".into(),
                target_image: "localhost/t".into(),
                output_dir: None,
                base_image: None,
            },
        }
    }

    fn cancel_env(request_id: &str) -> BuildDispatchEnvelope {
        BuildDispatchEnvelope {
            host_id: None,
            request_id: request_id.to_owned(),
            command: BuildCommandPayload::Cancel {},
        }
    }

    #[test]
    fn dispatch_pinned_to_builder_host_routes_to_that_host() {
        let streams = Streams::new();
        let pending = Pending::new();
        let _rx_a = insert_host(&streams, "A", &["coold", "builder"]);
        let _rx_b = insert_host(&streams, "B", &["coold", "builder"]);

        let out = route(&streams, &pending, static_env("r1", Some("A")));
        match out {
            RouteOutcome::SendBuild { host_id, .. } => assert_eq!(host_id, "A"),
            other => panic!("expected SendBuild to A, got {other:?}"),
        }
        // Pending entry must be created for timeout sweeping.
        let entry = pending.get("r1").expect("pending populated");
        assert_eq!(entry.host_id, "A");
        assert_eq!(entry.kind, PendingKind::Build);
    }

    #[test]
    fn dispatch_pinned_to_coold_only_host_returns_503() {
        let streams = Streams::new();
        let pending = Pending::new();
        insert_host(&streams, "A", &["coold"]);

        let out = route(&streams, &pending, static_env("r1", Some("A")));
        match out {
            RouteOutcome::PushError { code, message } => {
                assert_eq!(code, 503);
                assert_eq!(message, "host has no builder capability");
            }
            other => panic!("expected 503, got {other:?}"),
        }
        assert!(pending.get("r1").is_none(), "no pending on reject");
    }

    #[test]
    fn dispatch_pinned_to_unknown_host_returns_503() {
        let streams = Streams::new();
        let pending = Pending::new();
        insert_host(&streams, "A", &["coold", "builder"]);

        let out = route(&streams, &pending, static_env("r1", Some("Z")));
        match out {
            RouteOutcome::PushError { code, message } => {
                assert_eq!(code, 503);
                assert_eq!(message, "host has no builder capability");
            }
            other => panic!("expected 503, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_without_host_id_picks_builder_capable_host() {
        let streams = Streams::new();
        let pending = Pending::new();
        // A has only coold; B is builder-capable. Router must pick B.
        insert_host(&streams, "A", &["coold"]);
        insert_host(&streams, "B", &["coold", "builder"]);

        let out = route(&streams, &pending, static_env("r1", None));
        match out {
            RouteOutcome::SendBuild { host_id, .. } => assert_eq!(host_id, "B"),
            other => panic!("expected SendBuild to B, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_without_host_id_when_no_builder_returns_503() {
        let streams = Streams::new();
        let pending = Pending::new();
        insert_host(&streams, "A", &["coold"]);
        insert_host(&streams, "B", &["coold"]);

        let out = route(&streams, &pending, static_env("r1", None));
        match out {
            RouteOutcome::PushError { code, message } => {
                assert_eq!(code, 503);
                assert_eq!(message, "no builder-capable host connected");
            }
            other => panic!("expected 503, got {other:?}"),
        }
    }

    #[test]
    fn cancel_routes_to_owning_host() {
        let streams = Streams::new();
        let pending = Pending::new();
        insert_host(&streams, "A", &["coold", "builder"]);
        insert_host(&streams, "B", &["coold", "builder"]);
        pending.insert("r1".into(), "B".into(), PendingKind::Build);

        let out = route(&streams, &pending, cancel_env("r1"));
        match out {
            RouteOutcome::SendCancel { host_id, msg } => {
                assert_eq!(host_id, "B");
                assert_eq!(msg.request_id, "r1");
                assert!(matches!(
                    msg.command,
                    Some(server_msg::Command::CancelBuild(_))
                ));
            }
            other => panic!("expected SendCancel to B, got {other:?}"),
        }
    }

    #[test]
    fn cancel_for_unknown_request_returns_404() {
        let streams = Streams::new();
        let pending = Pending::new();
        insert_host(&streams, "A", &["coold", "builder"]);

        let out = route(&streams, &pending, cancel_env("nope"));
        match out {
            RouteOutcome::PushError { code, message } => {
                assert_eq!(code, 404);
                assert_eq!(message, "request_id not in flight");
            }
            other => panic!("expected 404, got {other:?}"),
        }
    }

    #[test]
    fn cancel_when_owning_host_lost_builder_cap_drops_silently() {
        // Host reconnected with fewer caps between build start and cancel.
        let streams = Streams::new();
        let pending = Pending::new();
        pending.insert("r1".into(), "A".into(), PendingKind::Build);
        insert_host(&streams, "A", &["coold"]);

        let out = route(&streams, &pending, cancel_env("r1"));
        match out {
            RouteOutcome::DropCancelHostGone { host_id } => assert_eq!(host_id, "A"),
            other => panic!("expected DropCancelHostGone, got {other:?}"),
        }
    }
}
