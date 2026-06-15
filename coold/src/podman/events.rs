use std::time::Duration;

use anyhow::{Context, Result};
use http_body_util::BodyExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::{client::PodmanClient, types::Event};

/// Relevant lifecycle actions we act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Start,
    Stop,
}

impl EventKind {
    fn classify(action: &str) -> Option<Self> {
        match action {
            "start" | "started" | "init" => Some(Self::Start),
            "die" | "died" | "stop" | "stopped" | "remove" | "removed" | "kill" | "killed" => {
                Some(Self::Stop)
            }
            _ => None,
        }
    }
}

pub struct EventMessage {
    pub kind: EventKind,
    pub container_id: String,
}

/// Runs forever: streams Podman events, reconnecting with backoff on failure,
/// forwards lifecycle changes onto `tx`. Returns only if `tx` is dropped.
pub async fn run(client: PodmanClient, tx: mpsc::Sender<EventMessage>) {
    let mut backoff = Duration::from_millis(500);
    let max_backoff = Duration::from_secs(10);

    loop {
        match stream_once(&client, &tx).await {
            Ok(()) => {
                info!("podman event stream closed; reconnecting");
                backoff = Duration::from_millis(500);
            }
            Err(e) => {
                warn!(error = %e, backoff = ?backoff, "podman event stream failed");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
        if tx.is_closed() {
            return;
        }
    }
}

async fn stream_once(client: &PodmanClient, tx: &mpsc::Sender<EventMessage>) -> Result<()> {
    let res = client
        .events("/v5.0.0/libpod/events?stream=true")
        .await
        .context("open events stream")?;

    info!(socket = %client.socket().display(), "podman event stream connected");

    let mut body = res.into_body();
    let mut buf: Vec<u8> = Vec::with_capacity(4096);

    while let Some(frame) = body.frame().await {
        let frame = frame.context("read event frame")?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        buf.extend_from_slice(&data);

        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line = buf.drain(..=pos).collect::<Vec<u8>>();
            let line = &line[..line.len() - 1];
            if line.is_empty() {
                continue;
            }
            dispatch(line, tx).await;
        }
    }

    if !buf.is_empty() {
        dispatch(&buf, tx).await;
    }

    Ok(())
}

async fn dispatch(line: &[u8], tx: &mpsc::Sender<EventMessage>) {
    let event: Event = match serde_json::from_slice(line) {
        Ok(e) => e,
        Err(e) => {
            debug!(error = %e, raw = %String::from_utf8_lossy(line), "skip unparsable event");
            return;
        }
    };

    if event.kind != "container" && !event.kind.is_empty() {
        return;
    }

    let Some(kind) = EventKind::classify(event.action_name()) else {
        return;
    };
    let id = event.container_id().to_string();
    if id.is_empty() {
        return;
    }

    if tx
        .send(EventMessage {
            kind,
            container_id: id,
        })
        .await
        .is_err()
    {
        debug!("event receiver dropped");
    }
}
