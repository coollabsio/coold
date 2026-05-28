use std::{path::PathBuf, time::Duration};

use coolify_core::{ContainerSummary, SchedulerStream};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::timeout,
};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SchedulerClient {
    socket_path: PathBuf,
    timeout: Duration,
}

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("scheduler is not configured")]
    NotConfigured,
    #[error("host not connected")]
    HostNotConnected,
    #[error("scheduler timeout")]
    Timeout,
    #[error("scheduler returned {status}: {message}")]
    Scheduler { status: u16, message: String },
    #[error("malformed scheduler response: {0}")]
    Malformed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Serialize)]
struct DispatchEnvelope<'a> {
    host_id: &'a str,
    request_id: String,
    command: CommandPayload,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CommandPayload {
    ListContainers,
}

#[derive(Debug, Deserialize)]
struct ResponseEnvelope<T> {
    request_id: String,
    #[serde(flatten)]
    body: ResponseBody<T>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ResponseBody<T> {
    Ok { data: T },
    Error { code: u32, message: String },
}

impl SchedulerClient {
    pub fn new(socket_path: PathBuf, timeout: Duration) -> Self {
        Self {
            socket_path,
            timeout,
        }
    }

    pub fn configured(&self) -> bool {
        !self.socket_path.as_os_str().is_empty()
    }

    pub async fn list_streams(&self) -> Result<Vec<SchedulerStream>, SchedulerError> {
        if !self.configured() {
            return Err(SchedulerError::NotConfigured);
        }
        let (status, response_body) = self.request("GET", "/v1/streams", &[]).await?;
        if !(200..300).contains(&status) {
            return Err(SchedulerError::Scheduler {
                status,
                message: String::from_utf8_lossy(&response_body).into_owned(),
            });
        }
        Ok(serde_json::from_slice(&response_body)?)
    }

    pub async fn list_containers(
        &self,
        host_id: &str,
    ) -> Result<Vec<ContainerSummary>, SchedulerError> {
        if !self.configured() {
            return Err(SchedulerError::NotConfigured);
        }
        let request_id = Uuid::now_v7().to_string();
        let env = DispatchEnvelope {
            host_id,
            request_id: request_id.clone(),
            command: CommandPayload::ListContainers,
        };
        let body = serde_json::to_vec(&env)?;
        let (status, response_body) = self.request("POST", "/v1/coold/dispatch", &body).await?;
        let text = String::from_utf8(response_body)
            .map_err(|e| SchedulerError::Malformed(e.to_string()))?;
        let response: ResponseEnvelope<Vec<ContainerSummary>> = serde_json::from_str(&text)?;
        if response.request_id != request_id {
            return Err(SchedulerError::Malformed("request_id mismatch".into()));
        }
        match response.body {
            ResponseBody::Ok { data } if (200..300).contains(&status) => Ok(data),
            ResponseBody::Ok { .. } => Err(SchedulerError::Scheduler {
                status,
                message: "unexpected non-2xx ok response".into(),
            }),
            ResponseBody::Error { code, message } if code == 404 || status == 404 => {
                Err(SchedulerError::HostNotConnected)
            }
            ResponseBody::Error { message, .. } => {
                Err(SchedulerError::Scheduler { status, message })
            }
        }
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), SchedulerError> {
        let fut = async {
            let mut stream = UnixStream::connect(&self.socket_path).await?;
            let req = format!(
                "{method} {path} HTTP/1.1\r\nHost: scheduler\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(req.as_bytes()).await?;
            stream.write_all(body).await?;
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await?;
            parse_http_response(&buf)
        };
        timeout(self.timeout, fut)
            .await
            .map_err(|_| SchedulerError::Timeout)?
    }
}

fn parse_http_response(buf: &[u8]) -> Result<(u16, Vec<u8>), SchedulerError> {
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| SchedulerError::Malformed("missing HTTP header terminator".into()))?;
    let (head, body) = buf.split_at(split + 4);
    let head = std::str::from_utf8(head).map_err(|e| SchedulerError::Malformed(e.to_string()))?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| SchedulerError::Malformed("missing HTTP status".into()))?;
    Ok((status, body.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http_response() {
        let (status, body) =
            parse_http_response(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}").unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"{}");
    }
}
