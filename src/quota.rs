use std::{sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::RwLock,
    time::interval,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

const APP_SERVER_COMMAND: &str = "codex";
const QUOTA_READ_METHOD: &str = "account/rateLimits/read";
const QUOTA_UPDATED_METHOD: &str = "account/rateLimits/updated";

#[derive(Clone)]
pub struct QuotaManager {
    snapshot: Arc<RwLock<QuotaSnapshot>>,
    shutdown: CancellationToken,
}

#[derive(Clone, Debug, Serialize)]
pub struct QuotaSnapshot {
    pub source: &'static str,
    pub state: &'static str,
    pub stale: bool,
    pub fetched_at_unix_secs: Option<u64>,
    pub rate_limit: Option<RateLimit>,
    pub error: Option<&'static str>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimit {
    pub limit_id: String,
    pub limit_name: Option<String>,
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub plan_type: Option<String>,
    pub rate_limit_reached_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    pub used_percent: u8,
    pub window_duration_mins: u32,
    pub resets_at: u64,
}

impl Default for QuotaSnapshot {
    fn default() -> Self {
        Self {
            source: "codex-app-server",
            state: "initializing",
            stale: false,
            fetched_at_unix_secs: None,
            rate_limit: None,
            error: None,
        }
    }
}

impl QuotaManager {
    #[must_use]
    pub fn spawn(refresh_interval: Duration) -> Self {
        let manager = Self {
            snapshot: Arc::new(RwLock::new(QuotaSnapshot::default())),
            shutdown: CancellationToken::new(),
        };
        let worker = manager.clone();
        tokio::spawn(async move {
            worker.run(refresh_interval).await;
        });
        manager
    }

    pub async fn snapshot(&self) -> QuotaSnapshot {
        self.snapshot.read().await.clone()
    }

    pub async fn shutdown(&self) {
        self.shutdown.cancel();
    }

    async fn run(&self, refresh_interval: Duration) {
        let mut retry_delay = Duration::from_secs(1);
        loop {
            if self.shutdown.is_cancelled() {
                return;
            }
            match self.run_app_server_session(refresh_interval).await {
                Ok(()) if self.shutdown.is_cancelled() => return,
                Ok(()) | Err(QuotaError::Unavailable) => {
                    self.mark_stale("codex_app_server_unavailable").await;
                }
                Err(QuotaError::Protocol) => {
                    self.mark_stale("codex_app_server_protocol_error").await;
                }
            }
            tokio::select! {
                _ = self.shutdown.cancelled() => return,
                _ = tokio::time::sleep(retry_delay) => {},
            }
            retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
        }
    }

    async fn run_app_server_session(&self, refresh_interval: Duration) -> Result<(), QuotaError> {
        let mut child = Command::new(APP_SERVER_COMMAND)
            .args(["app-server", "--stdio"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|_| QuotaError::Unavailable)?;
        let mut stdin = child.stdin.take().ok_or(QuotaError::Unavailable)?;
        let stdout = child.stdout.take().ok_or(QuotaError::Unavailable)?;
        let mut lines = BufReader::new(stdout).lines();
        let mut interval = interval(refresh_interval);
        interval.tick().await;

        send_rpc(
            &mut stdin,
            1,
            "initialize",
            json!({
                "clientInfo": {
                    "name": "grok-codex-bridge",
                    "title": "Grok Codex Bridge",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )
        .await?;
        send_notification(&mut stdin, "initialized", json!({})).await?;
        send_rpc(&mut stdin, 2, QUOTA_READ_METHOD, json!({})).await?;
        let mut next_request_id = 3_u64;

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    let _ = child.kill().await;
                    return Ok(());
                }
                _ = interval.tick() => {
                    send_rpc(&mut stdin, next_request_id, QUOTA_READ_METHOD, json!({})).await?;
                    next_request_id = next_request_id.saturating_add(1);
                }
                line = lines.next_line() => {
                    let line = line.map_err(|_| QuotaError::Unavailable)?.ok_or(QuotaError::Unavailable)?;
                    self.handle_message(&line).await;
                }
            }
        }
    }

    async fn handle_message(&self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let rate_limit = message
            .pointer("/result/rateLimits")
            .or_else(|| message.pointer("/params/rateLimits"))
            .or_else(|| message.pointer("/params"))
            .and_then(parse_rate_limit);
        if let Some(rate_limit) = rate_limit {
            self.mark_ready(rate_limit).await;
            return;
        }
        if message.get("method").and_then(Value::as_str) == Some(QUOTA_UPDATED_METHOD) {
            warn!("Codex rate-limit update did not match the expected protocol shape");
        }
    }

    async fn mark_ready(&self, rate_limit: RateLimit) {
        *self.snapshot.write().await = QuotaSnapshot {
            source: "codex-app-server",
            state: "ready",
            stale: false,
            fetched_at_unix_secs: Some(unix_seconds()),
            rate_limit: Some(rate_limit),
            error: None,
        };
    }

    async fn mark_stale(&self, error: &'static str) {
        let mut snapshot = self.snapshot.write().await;
        snapshot.state = if snapshot.rate_limit.is_some() {
            "stale"
        } else {
            "unavailable"
        };
        snapshot.stale = snapshot.rate_limit.is_some();
        snapshot.error = Some(error);
    }
}

#[derive(Debug)]
enum QuotaError {
    Unavailable,
    Protocol,
}

async fn send_rpc(
    stdin: &mut tokio::process::ChildStdin,
    id: u64,
    method: &str,
    params: Value,
) -> Result<(), QuotaError> {
    let message = json!({"id": id, "method": method, "params": params});
    send_json_line(stdin, &message).await
}

async fn send_notification(
    stdin: &mut tokio::process::ChildStdin,
    method: &str,
    params: Value,
) -> Result<(), QuotaError> {
    let message = json!({"method": method, "params": params});
    send_json_line(stdin, &message).await
}

async fn send_json_line(
    stdin: &mut tokio::process::ChildStdin,
    message: &Value,
) -> Result<(), QuotaError> {
    let encoded = serde_json::to_vec(message).map_err(|_| QuotaError::Protocol)?;
    stdin
        .write_all(&encoded)
        .await
        .map_err(|_| QuotaError::Unavailable)?;
    stdin
        .write_all(b"\n")
        .await
        .map_err(|_| QuotaError::Unavailable)?;
    stdin.flush().await.map_err(|_| QuotaError::Unavailable)
}

fn parse_rate_limit(value: &Value) -> Option<RateLimit> {
    serde_json::from_value(value.clone()).ok()
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_rate_limit_snapshot() {
        let rate_limit = parse_rate_limit(&json!({
            "limitId": "codex",
            "limitName": null,
            "primary": {"usedPercent": 32, "windowDurationMins": 300, "resetsAt": 1},
            "secondary": {"usedPercent": 5, "windowDurationMins": 10080, "resetsAt": 2},
            "planType": "pro",
            "rateLimitReachedType": null
        }))
        .expect("rate-limit snapshot is valid");
        assert_eq!(rate_limit.limit_id, "codex");
        assert_eq!(
            rate_limit
                .primary
                .as_ref()
                .map(|window| window.used_percent),
            Some(32)
        );
    }
}
