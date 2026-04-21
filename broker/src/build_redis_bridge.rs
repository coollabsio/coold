/// Redis bridge for build dispatch: consumes `build:cmd` stream from Laravel,
/// routes BuildRequest to the appropriate builder gRPC stream, pushes responses
/// back to `build:resp:{request_id}` for Laravel to BLPOP.
use anyhow::Result;
use redis::streams::{StreamReadOptions, StreamReadReply};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use coolify_proto::builder::v1::{
    builder_server_msg, BuildRequest, BuilderServerMsg, StaticConfig,
};

use crate::{
    config::Config,
    state::{BuilderStreams, Pending},
};

const BUILD_CMD_STREAM: &str = "build:cmd";
const CONSUMER_GROUP: &str = "broker";
const CONSUMER_NAME: &str = "broker-1";
const BLOCK_MS: usize = 5000;
const RESP_TTL_SECS: i64 = 30;

#[derive(Debug, Deserialize)]
struct BuildDispatchEnvelope {
    builder_id: Option<String>,
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
        output_dir: Option<String>,
        base_image: Option<String>,
    },
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

pub async fn run(config: Config, builder_streams: BuilderStreams, pending: Pending) -> Result<()> {
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

                dispatch(&builder_streams, &mut conn, &pending, envelope).await;
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

pub async fn push_build_timeout_error(redis_url: &str, request_id: &str) -> Result<()> {
    push_build_response(
        redis_url,
        request_id,
        BuildResponseBody::Error {
            code: 504,
            message: "build dispatch timeout".into(),
            stage: String::new(),
        },
    )
    .await
}

async fn dispatch(
    builder_streams: &BuilderStreams,
    conn: &mut impl AsyncCommands,
    pending: &Pending,
    env: BuildDispatchEnvelope,
) {
    let build_req = match env.command {
        BuildCommandPayload::StaticBuild {
            repo_url,
            git_ref,
            target_image,
            output_dir,
            base_image,
        } => BuildRequest {
            repo_url,
            git_ref,
            stack: coolify_proto::builder::v1::BuildStack::Static as i32,
            target_image,
            cache_key: String::new(),
            static_cfg: Some(StaticConfig {
                output_dir: output_dir.unwrap_or_else(|| "dist".into()),
                base_image: base_image.unwrap_or_else(|| "docker.io/library/nginx:alpine".into()),
            }),
        },
    };

    let msg = BuilderServerMsg {
        request_id: env.request_id.clone(),
        command: Some(builder_server_msg::Command::Build(build_req)),
    };

    let tx = if let Some(id) = &env.builder_id {
        builder_streams.get(id)
    } else {
        builder_streams.pick_idle().and_then(|id| builder_streams.get(&id))
    };

    match tx {
        Some(tx) => {
            pending.insert(env.request_id.clone());
            if let Err(e) = tx.send(msg).await {
                pending.remove(&env.request_id);
                warn!(request_id = %env.request_id, error = %e, "send to builder stream failed");
                push_error(conn, &env.request_id, 503, "builder stream send failed").await;
            }
        }
        None => {
            warn!(request_id = %env.request_id, "no builder connected");
            push_error(conn, &env.request_id, 503, "no builder connected").await;
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
