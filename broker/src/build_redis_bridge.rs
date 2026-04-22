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

async fn dispatch(
    streams: &Streams,
    conn: &mut impl AsyncCommands,
    pending: &Pending,
    env: BuildDispatchEnvelope,
) {
    match env.command {
        BuildCommandPayload::StaticBuild {
            repo_url,
            git_ref,
            target_image,
            output_dir,
            base_image,
        } => {
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

            let target_host = match env.host_id.as_deref() {
                Some(id) => {
                    if !streams.has_cap(id, "builder") {
                        warn!(host_id = %id, request_id = %env.request_id, "host has no builder capability");
                        push_error(conn, &env.request_id, 503, "host has no builder capability").await;
                        return;
                    }
                    id.to_string()
                }
                None => match streams.pick_host_with_cap("builder") {
                    Some(id) => id,
                    None => {
                        warn!(request_id = %env.request_id, "no builder-capable host connected");
                        push_error(conn, &env.request_id, 503, "no builder-capable host connected").await;
                        return;
                    }
                },
            };

            let Some(tx) = streams.get_tx(&target_host) else {
                push_error(conn, &env.request_id, 503, "builder host disconnected").await;
                return;
            };

            let msg = ServerMsg {
                request_id: env.request_id.clone(),
                command: Some(server_msg::Command::Build(build_req)),
            };

            pending.insert(env.request_id.clone(), target_host.clone(), PendingKind::Build);
            if let Err(e) = tx.send(msg).await {
                pending.remove(&env.request_id);
                warn!(request_id = %env.request_id, error = %e, "send build to host stream failed");
                push_error(conn, &env.request_id, 503, "host stream send failed").await;
            }
        }
        BuildCommandPayload::Cancel {} => {
            let Some(entry) = pending.get(&env.request_id) else {
                warn!(request_id = %env.request_id, "cancel for unknown request_id; already finished or never dispatched");
                push_error(conn, &env.request_id, 404, "request_id not in flight").await;
                return;
            };
            let Some(tx) = streams.get_tx(&entry.host_id) else {
                warn!(request_id = %env.request_id, host_id = %entry.host_id, "cancel: owning host disconnected");
                return;
            };
            let msg = ServerMsg {
                request_id: env.request_id.clone(),
                command: Some(server_msg::Command::CancelBuild(CancelBuild {})),
            };
            if let Err(e) = tx.send(msg).await {
                warn!(request_id = %env.request_id, error = %e, "send cancel to host stream failed");
            }
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
