use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tokio::sync::RwLock;

use crate::protocol::ObservedUsage;

#[derive(Clone)]
pub struct Metrics {
    requests_total: Arc<AtomicU64>,
    requests_succeeded: Arc<AtomicU64>,
    requests_failed: Arc<AtomicU64>,
    streams_active: Arc<AtomicU64>,
    streams_cancelled: Arc<AtomicU64>,
    streams_truncated: Arc<AtomicU64>,
    auth_refreshes: Arc<AtomicU64>,
    observed_input_tokens: Arc<AtomicU64>,
    observed_output_tokens: Arc<AtomicU64>,
    observed_total_tokens: Arc<AtomicU64>,
    ready: Arc<AtomicBool>,
    latest: Arc<RwLock<Option<RequestObservation>>>,
    latest_usage: Arc<RwLock<Option<UsageObservation>>>,
    rate_limits: Arc<RwLock<RateLimitObservation>>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            requests_total: Arc::default(),
            requests_succeeded: Arc::default(),
            requests_failed: Arc::default(),
            streams_active: Arc::default(),
            streams_cancelled: Arc::default(),
            streams_truncated: Arc::default(),
            auth_refreshes: Arc::default(),
            observed_input_tokens: Arc::default(),
            observed_output_tokens: Arc::default(),
            observed_total_tokens: Arc::default(),
            ready: Arc::new(AtomicBool::new(true)),
            latest: Arc::default(),
            latest_usage: Arc::default(),
            rate_limits: Arc::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RequestObservation {
    pub request_id: String,
    pub model: String,
    pub effort: String,
    pub outcome: String,
    pub duration_ms: u128,
    pub time_to_first_event_ms: Option<u128>,
    pub observed_at_unix_secs: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageObservation {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub observed_at_unix_secs: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RateLimitObservation {
    pub requests_remaining: Option<String>,
    pub tokens_remaining: Option<String>,
    pub requests_reset: Option<String>,
    pub tokens_reset: Option<String>,
}

#[derive(Serialize)]
pub struct StatusSnapshot {
    pub status: &'static str,
    pub bridge_observed_usage: BridgeUsageSnapshot,
    pub rate_limits: RateLimitObservation,
    pub requests: RequestCounters,
    pub latest_request: Option<RequestObservation>,
    pub note: &'static str,
}

#[derive(Serialize)]
pub struct BridgeUsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub latest: Option<UsageObservation>,
}

#[derive(Serialize)]
pub struct RequestCounters {
    pub total: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub active_streams: u64,
    pub cancelled_streams: u64,
    pub truncated_streams: u64,
    pub auth_refreshes: u64,
}

impl Metrics {
    pub fn record_request_start(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_stream_start(&self) -> ActiveStream {
        self.streams_active.fetch_add(1, Ordering::Relaxed);
        ActiveStream {
            metrics: self.clone(),
            finished: false,
        }
    }

    pub fn record_auth_refreshes(&self, count: u64) {
        self.auth_refreshes.store(count, Ordering::Relaxed);
    }

    pub async fn record_outcome(
        &self,
        request_id: String,
        model: String,
        effort: String,
        outcome: &str,
        started_at: Instant,
        time_to_first_event: Option<Instant>,
    ) {
        match outcome {
            "completed" => self.requests_succeeded.fetch_add(1, Ordering::Relaxed),
            "cancelled" => self.streams_cancelled.fetch_add(1, Ordering::Relaxed),
            "truncated" => self.streams_truncated.fetch_add(1, Ordering::Relaxed),
            _ => self.requests_failed.fetch_add(1, Ordering::Relaxed),
        };
        let observation = RequestObservation {
            request_id,
            model,
            effort,
            outcome: outcome.to_owned(),
            duration_ms: started_at.elapsed().as_millis(),
            time_to_first_event_ms: time_to_first_event
                .map(|instant| instant.duration_since(started_at).as_millis()),
            observed_at_unix_secs: unix_seconds(),
        };
        *self.latest.write().await = Some(observation);
    }

    pub async fn record_usage(&self, usage: ObservedUsage) {
        if let Some(value) = usage.input_tokens {
            self.observed_input_tokens
                .fetch_add(value, Ordering::Relaxed);
        }
        if let Some(value) = usage.output_tokens {
            self.observed_output_tokens
                .fetch_add(value, Ordering::Relaxed);
        }
        if let Some(value) = usage.total_tokens {
            self.observed_total_tokens
                .fetch_add(value, Ordering::Relaxed);
        }
        *self.latest_usage.write().await = Some(UsageObservation {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            observed_at_unix_secs: unix_seconds(),
        });
    }

    pub async fn record_rate_limits(&self, headers: &reqwest::header::HeaderMap) {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        *self.rate_limits.write().await = RateLimitObservation {
            requests_remaining: header("x-ratelimit-remaining-requests"),
            tokens_remaining: header("x-ratelimit-remaining-tokens"),
            requests_reset: header("x-ratelimit-reset-requests"),
            tokens_reset: header("x-ratelimit-reset-tokens"),
        };
    }

    pub async fn snapshot(&self) -> StatusSnapshot {
        StatusSnapshot {
            status: if self.ready.load(Ordering::Relaxed) {
                "ready"
            } else {
                "draining"
            },
            bridge_observed_usage: BridgeUsageSnapshot {
                input_tokens: self.observed_input_tokens.load(Ordering::Relaxed),
                output_tokens: self.observed_output_tokens.load(Ordering::Relaxed),
                total_tokens: self.observed_total_tokens.load(Ordering::Relaxed),
                latest: self.latest_usage.read().await.clone(),
            },
            rate_limits: self.rate_limits.read().await.clone(),
            requests: RequestCounters {
                total: self.requests_total.load(Ordering::Relaxed),
                succeeded: self.requests_succeeded.load(Ordering::Relaxed),
                failed: self.requests_failed.load(Ordering::Relaxed),
                active_streams: self.streams_active.load(Ordering::Relaxed),
                cancelled_streams: self.streams_cancelled.load(Ordering::Relaxed),
                truncated_streams: self.streams_truncated.load(Ordering::Relaxed),
                auth_refreshes: self.auth_refreshes.load(Ordering::Relaxed),
            },
            latest_request: self.latest.read().await.clone(),
            note: "Usage is observed from Codex Responses events and headers; it is not a subscription-quota API.",
        }
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Relaxed);
    }
}

pub struct ActiveStream {
    metrics: Metrics,
    finished: bool,
}

impl ActiveStream {
    pub fn finish(&mut self) {
        self.finished = true;
    }
}

impl Drop for ActiveStream {
    fn drop(&mut self) {
        self.metrics.streams_active.fetch_sub(1, Ordering::Relaxed);
        if !self.finished {
            self.metrics
                .streams_cancelled
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}
