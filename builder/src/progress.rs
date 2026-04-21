use tokio::sync::mpsc;
use tracing::warn;

use coolify_proto::builder::v1::{builder_client_msg, BuilderClientMsg, BuildProgress};

pub struct ProgressEmitter {
    tx: mpsc::Sender<BuilderClientMsg>,
}

impl ProgressEmitter {
    pub fn new(tx: mpsc::Sender<BuilderClientMsg>) -> Self {
        Self { tx }
    }

    pub async fn emit(&self, stage: &str, log_line: impl Into<String>, percent: u32) {
        let msg = BuilderClientMsg {
            payload: Some(builder_client_msg::Payload::Progress(BuildProgress {
                stage: stage.to_owned(),
                log_line: log_line.into(),
                percent,
            })),
        };
        if let Err(e) = self.tx.send(msg).await {
            warn!(stage, "progress send failed: {e}");
        }
    }
}
