/// Redis bridge: consumes dispatch commands from Laravel via Redis streams,
/// routes them to the appropriate coold gRPC stream, and pushes responses back.
///
/// Redis keys:
///   coold:cmd               — stream; Laravel XADDs, broker XREADGROUP consumes
///   coold:resp:{request_id} — list; broker LPUSHes response, Laravel BLPOPs
///   coold:hosts             — hash; broker writes host_id fields on Hello
use anyhow::Result;
use redis::streams::{StreamReadOptions, StreamReadReply};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use coolify_proto::agent::v1::{server_msg, ListContainersReq, Response, ServerMsg};

use crate::{config::Config, state::{Pending, Streams}};

const CMD_STREAM: &str = "coold:cmd";
const CONSUMER_GROUP: &str = "broker";
const CONSUMER_NAME: &str = "broker-1";
const BLOCK_MS: usize = 5000;

/// JSON envelope Laravel writes to `coold:cmd`.
#[derive(Debug, Deserialize)]
struct DispatchEnvelope {
    host_id: String,
    request_id: String,
    command: CommandPayload,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CommandPayload {
    ListContainers,
}

/// JSON envelope broker writes to `coold:resp:{request_id}`.
#[derive(Debug, Serialize)]
struct ResponseEnvelope {
    request_id: String,
    #[serde(flatten)]
    body: ResponseBody,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ResponseBody {
    Ok { data: serde_json::Value },
    Error { code: u32, message: String },
}

const RESP_TTL_SECS: usize = 10;

pub async fn run(config: Config, streams: Streams, pending: Pending) -> Result<()> {
    let client = redis::Client::open(config.redis_url.as_str())?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    // Create consumer group if it doesn't exist yet.
    let _: Result<(), _> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(CMD_STREAM)
        .arg(CONSUMER_GROUP)
        .arg("$")
        .arg("MKSTREAM")
        .query_async(&mut conn)
        .await;

    info!("Redis bridge running, consuming {CMD_STREAM}");

    let opts = StreamReadOptions::default()
        .group(CONSUMER_GROUP, CONSUMER_NAME)
        .count(16)
        .block(BLOCK_MS);

    loop {
        let reply: StreamReadReply = match conn
            .xread_options::<_, _, StreamReadReply>(&[CMD_STREAM], &[">"], &opts)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "xreadgroup failed; retrying");
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
                        warn!(%stream_id, "missing payload field; skipping");
                        ack(&mut conn, &stream_id).await;
                        continue;
                    }
                };

                let envelope: DispatchEnvelope = match serde_json::from_str(&json) {
                    Ok(e) => e,
                    Err(e) => {
                        warn!(%stream_id, error = %e, "malformed dispatch envelope; skipping");
                        ack(&mut conn, &stream_id).await;
                        continue;
                    }
                };

                dispatch(&streams, &mut conn, &config, &pending, envelope, stream_id.clone()).await;
                ack(&mut conn, &stream_id).await;
            }
        }
    }
}

/// Push a coold Response back to Laravel via `coold:resp:{request_id}`.
pub async fn push_response(redis_url: &str, resp: Response) -> Result<()> {
    let client = redis::Client::open(redis_url)?;
    let conn = client.get_multiplexed_async_connection().await?;

    let body = match resp.body {
        Some(coolify_proto::agent::v1::response::Body::ListContainers(r)) => {
            let data = serde_json::to_value(&r.containers.iter().map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "name": c.name,
                    "image": c.image,
                    "state": c.state,
                    "networks": c.networks,
                })
            }).collect::<Vec<_>>())?;
            ResponseBody::Ok { data }
        }
        Some(coolify_proto::agent::v1::response::Body::Error(e)) => {
            ResponseBody::Error { code: e.code, message: e.message }
        }
        None => ResponseBody::Error { code: 500, message: "empty response body".into() },
    };

    let envelope = ResponseEnvelope { request_id: resp.request_id.clone(), body };
    let json = serde_json::to_string(&envelope)?;
    let key = format!("coold:resp:{}", resp.request_id);

    let mut c = conn;
    c.lpush::<_, _, ()>(&key, &json).await?;
    c.expire::<_, ()>(&key, RESP_TTL_SECS as i64).await?;

    Ok(())
}

/// Push a `code=504` timeout error for `request_id`. Called by the pending sweeper.
pub async fn push_timeout_error(redis_url: &str, request_id: &str) -> Result<()> {
    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;
    let envelope = ResponseEnvelope {
        request_id: request_id.to_string(),
        body: ResponseBody::Error { code: 504, message: "dispatch timeout".into() },
    };
    let json = serde_json::to_string(&envelope)?;
    let key = format!("coold:resp:{request_id}");
    conn.lpush::<_, _, ()>(&key, &json).await?;
    conn.expire::<_, ()>(&key, RESP_TTL_SECS as i64).await?;
    Ok(())
}

async fn dispatch(
    streams: &Streams,
    conn: &mut impl AsyncCommands,
    _config: &Config,
    pending: &Pending,
    env: DispatchEnvelope,
    _stream_id: String,
) {
    let cmd = match env.command {
        CommandPayload::ListContainers => {
            server_msg::Command::ListContainers(ListContainersReq {})
        }
    };

    let msg = ServerMsg {
        request_id: env.request_id.clone(),
        command: Some(cmd),
    };

    match streams.get(&env.host_id) {
        Some(tx) => {
            pending.insert(env.request_id.clone());
            if let Err(e) = tx.send(msg).await {
                pending.remove(&env.request_id);
                warn!(host_id = %env.host_id, error = %e, "failed to send to stream");
                push_error(conn, &env.request_id, 503, "host stream send failed").await;
            }
        }
        None => {
            warn!(host_id = %env.host_id, "unknown host; not connected");
            push_error(conn, &env.request_id, 404, "host not connected").await;
        }
    }
}

async fn push_error(conn: &mut impl AsyncCommands, request_id: &str, code: u32, msg: &str) {
    let envelope = ResponseEnvelope {
        request_id: request_id.to_string(),
        body: ResponseBody::Error { code, message: msg.to_string() },
    };
    let json = serde_json::to_string(&envelope).unwrap_or_default();
    let key = format!("coold:resp:{request_id}");
    let _: Result<(), _> = conn.lpush(&key, &json).await;
    let _: Result<(), _> = conn.expire(&key, RESP_TTL_SECS as i64).await;
}

async fn ack(conn: &mut impl AsyncCommands, stream_id: &str) {
    let _: Result<(), _> = redis::cmd("XACK")
        .arg(CMD_STREAM)
        .arg(CONSUMER_GROUP)
        .arg(stream_id)
        .query_async(conn)
        .await;
}

