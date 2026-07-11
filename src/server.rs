use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use async_stream::stream;
use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use reqwest::Client;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Notify, Semaphore},
    time::timeout,
};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    auth::{AuthManager, Session},
    config::Config,
    metrics::Metrics,
    protocol::{
        ProtocolError, StreamTransformer, adapt_request, failed_event, sse_data,
        usage_from_completed,
    },
    quota::QuotaManager,
};

pub const BRIDGE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone)]
pub struct AppState {
    config: Arc<Config>,
    client: Client,
    auth: AuthManager,
    permits: Arc<Semaphore>,
    metrics: Metrics,
    quota: QuotaManager,
}

impl AppState {
    pub fn new(config: Config) -> Result<Self, reqwest::Error> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .http2_adaptive_window(true)
            .user_agent(format!("grok-codex-bridge/{BRIDGE_VERSION}"))
            .build()?;
        let auth = AuthManager::new(config.auth_path.clone(), client.clone());
        let quota = QuotaManager::spawn(Duration::from_secs(60));
        Ok(Self {
            permits: Arc::new(Semaphore::new(config.max_streams)),
            config: Arc::new(config),
            client,
            auth,
            metrics: Metrics::default(),
            quota,
        })
    }

    pub fn metrics(&self) -> Metrics {
        self.metrics.clone()
    }

    pub fn quota(&self) -> QuotaManager {
        self.quota.clone()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(health))
        .route("/health", get(health))
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/status", get(status))
        .route("/v1/models", get(models))
        .route("/models", get(models))
        .route("/v1/responses", post(responses))
        .route("/responses", post(responses))
        .with_state(state)
}

pub async fn serve(
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), std::io::Error> {
    let address = SocketAddr::new(state.config.host, state.config.port);
    let listener = TcpListener::bind(address).await?;
    info!(%address, version = BRIDGE_VERSION, "Codex bridge listening");
    let drain_timeout = state.config.drain_timeout;
    let shutdown_started = Arc::new(Notify::new());
    let shutdown_notification = Arc::clone(&shutdown_started);
    let graceful_shutdown = async move {
        shutdown.await;
        shutdown_notification.notify_one();
    };
    let server = axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(graceful_shutdown)
    .into_future();
    tokio::pin!(server);

    tokio::select! {
        result = &mut server => result,
        () = shutdown_started.notified() => {
            match timeout(drain_timeout, &mut server).await {
                Ok(result) => result,
                Err(_) => {
                    warn!(
                        drain_timeout_secs = drain_timeout.as_secs(),
                        "graceful shutdown timed out; closing remaining connections"
                    );
                    Ok(())
                }
            }
        }
    }
}

async fn health(ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Some(response) = loopback_only(peer) {
        return response;
    }
    (
        StatusCode::OK,
        axum::Json(json!({"ok": true, "service": "grok-codex-bridge", "version": BRIDGE_VERSION})),
    )
        .into_response()
}

async fn ready(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if let Some(response) = loopback_only(peer) {
        return response;
    }
    match state.auth.check_ready().await {
        Ok(()) => {
            state.metrics.set_ready(true);
            (
                StatusCode::OK,
                axum::Json(json!({"ok": true, "auth": "ready"})),
            )
                .into_response()
        }
        Err(_) => {
            state.metrics.set_ready(false);
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "codex_auth_unavailable",
                "Codex authentication is not ready",
            )
        }
    }
}

async fn status(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if let Some(response) = loopback_only(peer) {
        return response;
    }
    axum::Json(json!({
        "service": "grok-codex-bridge",
        "version": BRIDGE_VERSION,
        "bridge": state.metrics.snapshot().await,
        "codex_subscription": state.quota.snapshot().await,
    }))
    .into_response()
}

async fn models(ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Some(response) = loopback_only(peer) {
        return response;
    }
    axum::Json(json!({
        "object": "list",
        "data": [
            {"id": "gpt-5.6-sol", "object": "model"},
            {"id": "gpt-5.6-terra", "object": "model"}
        ]
    }))
    .into_response()
}

async fn responses(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let started_at = Instant::now();
    let request_id = Uuid::new_v4().to_string();
    state.metrics.record_request_start();

    if let Some(response) = loopback_only(peer) {
        return response;
    }
    if let Some(response) = validate_content_length(request.headers(), state.config.max_body_bytes)
    {
        return response;
    }

    let bytes = match to_bytes(request.into_body(), state.config.max_body_bytes).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return fail_before_stream(
                &state,
                &request_id,
                "unknown".to_owned(),
                "unknown".to_owned(),
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_too_large",
                    "The request exceeds the bridge body limit",
                ),
                started_at,
            )
            .await;
        }
    };
    let raw_request: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => {
            return fail_before_stream(
                &state,
                &request_id,
                "unknown".to_owned(),
                "unknown".to_owned(),
                (
                    StatusCode::BAD_REQUEST,
                    "invalid_json",
                    "The request body must be valid JSON",
                ),
                started_at,
            )
            .await;
        }
    };
    let adapted = match adapt_request(raw_request) {
        Ok(value) => value,
        Err(error) => {
            let (model, effort) = request_metadata(&Value::Null);
            return fail_before_stream(
                &state,
                &request_id,
                model,
                effort,
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "unsupported_request",
                    &protocol_error_message(error),
                ),
                started_at,
            )
            .await;
        }
    };
    let (model, effort) = request_metadata(&adapted);
    let permit = match state.permits.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return fail_before_stream(
                &state,
                &request_id,
                model,
                effort,
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    "bridge_busy",
                    "The local Codex bridge is at its stream limit",
                ),
                started_at,
            )
            .await;
        }
    };

    let session = match state.auth.session().await {
        Ok(session) => session,
        Err(_) => {
            drop(permit);
            return fail_before_stream(
                &state,
                &request_id,
                model,
                effort,
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "codex_auth_unavailable",
                    "Codex authentication is unavailable; run `codex login`",
                ),
                started_at,
            )
            .await;
        }
    };
    state
        .metrics
        .record_auth_refreshes(state.auth.refresh_count());

    let upstream = match request_upstream(&state, &adapted, &session).await {
        Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
            match state
                .auth
                .refresh_if_unchanged(Some(&session.access_token))
                .await
            {
                Ok(refreshed) => {
                    state
                        .metrics
                        .record_auth_refreshes(state.auth.refresh_count());
                    request_upstream(&state, &adapted, &refreshed).await
                }
                Err(_) => Err(UpstreamError::Auth),
            }
        }
        result => result,
    };
    let upstream = match upstream {
        Ok(response) => response,
        Err(error) => {
            drop(permit);
            let (status, code, message) = error.client_error();
            return fail_before_stream(
                &state,
                &request_id,
                model,
                effort,
                (status, code, message),
                started_at,
            )
            .await;
        }
    };

    if !upstream.status().is_success() {
        drop(permit);
        let upstream_status = upstream.status();
        let upstream_error = upstream
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable upstream error body>".to_owned());
        warn!(
            request_id = %request_id,
            %upstream_status,
            upstream_error = %upstream_error.chars().take(2_000).collect::<String>(),
            "Codex rejected an adapted request"
        );
        let error = UpstreamError::Status(upstream_status);
        let (status, code, message) = error.client_error();
        return fail_before_stream(
            &state,
            &request_id,
            model,
            effort,
            (status, code, message),
            started_at,
        )
        .await;
    }
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !content_type.is_empty() && !content_type.starts_with("text/event-stream") {
        warn!(
            request_id = %request_id,
            upstream_status = %upstream.status(),
            content_type,
            "Codex returned a non-SSE response"
        );
        drop(permit);
        return fail_before_stream(
            &state,
            &request_id,
            model,
            effort,
            (
                StatusCode::BAD_GATEWAY,
                "invalid_upstream_stream",
                "Codex returned an unexpected response format",
            ),
            started_at,
        )
        .await;
    }
    if content_type.is_empty() {
        warn!(
            request_id = %request_id,
            upstream_status = %upstream.status(),
            "Codex omitted Content-Type; validating the stream event-by-event"
        );
    }
    state.metrics.record_rate_limits(upstream.headers()).await;

    let idle_timeout = state.config.upstream_idle_timeout;
    let metrics = state.metrics.clone();
    let active_stream = metrics.record_stream_start();
    let response_request_id = request_id.clone();
    let mut upstream_body = upstream.bytes_stream();
    let stream = stream! {
        let _permit = permit;
        let mut active_stream = active_stream;
        let mut decoder = SseDecoder::default();
        let mut transformer = StreamTransformer::default();
        let mut saw_completed = false;
        let mut first_event_at = None;
        let mut outcome = "truncated";

        loop {
            let chunk = match timeout(idle_timeout, upstream_body.next()).await {
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(_))) => {
                    yield Ok::<Bytes, Infallible>(Bytes::from(sse_data(&failed_event(
                        "upstream_stream_error",
                        "Codex closed the stream unexpectedly",
                    ))));
                    outcome = "failed";
                    break;
                }
                Ok(None) => {
                    outcome = if saw_completed { "completed" } else { "truncated" };
                    if !saw_completed {
                        yield Ok::<Bytes, Infallible>(Bytes::from(sse_data(&failed_event(
                            "upstream_stream_truncated",
                            "Codex ended the stream before completing the response",
                        ))));
                    }
                    break;
                }
                Err(_) => {
                    yield Ok::<Bytes, Infallible>(Bytes::from(sse_data(&failed_event(
                        "upstream_stream_idle_timeout",
                        "Codex did not send a stream event before the idle timeout",
                    ))));
                    outcome = "failed";
                    break;
                }
            };

            for frame in decoder.push(&chunk) {
                if first_event_at.is_none() {
                    first_event_at = Some(Instant::now());
                }
                match frame {
                    SseFrame::Comment(comment) => yield Ok::<Bytes, Infallible>(Bytes::from(comment)),
                    SseFrame::Done => {
                        yield Ok::<Bytes, Infallible>(Bytes::from_static(b"data: [DONE]\n\n"));
                    }
                    SseFrame::Data(data) => {
                        let payload: Value = match serde_json::from_str(&data) {
                            Ok(payload) => payload,
                            Err(_) => {
                                yield Ok::<Bytes, Infallible>(Bytes::from(sse_data(&failed_event(
                                    "malformed_upstream_event",
                                    "Codex sent an invalid stream event",
                                ))));
                                outcome = "failed";
                                break;
                            }
                        };
                        if payload.get("type").and_then(Value::as_str) == Some("response.created")
                            && let Some(returned_model) = payload.pointer("/response/model").and_then(Value::as_str)
                            && returned_model != model
                        {
                            yield Ok::<Bytes, Infallible>(Bytes::from(sse_data(&failed_event(
                                "upstream_model_mismatch",
                                "Codex selected a model different from the requested model",
                            ))));
                            outcome = "failed";
                            break;
                        }
                        if let Some(transformed) = transformer.transform(payload) {
                            if transformed.get("type").and_then(Value::as_str) == Some("response.completed") {
                                saw_completed = true;
                                if let Some(usage) = usage_from_completed(&transformed) {
                                    metrics.record_usage(usage).await;
                                }
                            }
                            yield Ok::<Bytes, Infallible>(Bytes::from(sse_data(&transformed)));
                        }
                    }
                }
            }
            if outcome == "failed" {
                break;
            }
        }

        metrics.record_outcome(request_id, model, effort, outcome, started_at, first_event_at).await;
        active_stream.finish();
    };

    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        "text/event-stream; charset=utf-8"
            .parse()
            .expect("valid content type"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        "no-cache, no-transform"
            .parse()
            .expect("valid cache control"),
    );
    headers.insert("x-accel-buffering", "no".parse().expect("valid header"));
    headers.insert(
        "x-request-id",
        response_request_id
            .parse()
            .expect("UUID header value is valid"),
    );
    response
}

async fn request_upstream(
    state: &AppState,
    payload: &Value,
    session: &Session,
) -> Result<reqwest::Response, UpstreamError> {
    let request = state
        .client
        .post(state.config.upstream_url.clone())
        .bearer_auth(&session.access_token)
        .header("chatgpt-account-id", &session.account_id)
        .header("OpenAI-Beta", "responses=experimental")
        .header("originator", "grok-codex-bridge")
        .header(header::ACCEPT, "text/event-stream");
    request.json(payload).send().await.map_err(|error| {
        if error.is_timeout() {
            UpstreamError::Timeout
        } else {
            UpstreamError::Transport
        }
    })
}

#[derive(Debug)]
enum UpstreamError {
    Auth,
    Status(StatusCode),
    Timeout,
    Transport,
}

impl UpstreamError {
    fn client_error(&self) -> (StatusCode, &'static str, &'static str) {
        match self {
            Self::Auth => (
                StatusCode::BAD_GATEWAY,
                "codex_auth_rejected",
                "Codex rejected the local session",
            ),
            Self::Status(StatusCode::TOO_MANY_REQUESTS) => (
                StatusCode::TOO_MANY_REQUESTS,
                "codex_rate_limited",
                "Codex subscription capacity is temporarily unavailable",
            ),
            Self::Status(status) if status.is_client_error() => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "codex_request_rejected",
                "Codex rejected the adapted request",
            ),
            Self::Status(_) | Self::Transport => (
                StatusCode::BAD_GATEWAY,
                "codex_upstream_unavailable",
                "Codex is temporarily unavailable",
            ),
            Self::Timeout => (
                StatusCode::GATEWAY_TIMEOUT,
                "codex_timeout",
                "Codex did not respond in time",
            ),
        }
    }
}

fn loopback_only(peer: SocketAddr) -> Option<Response> {
    (!peer.ip().is_loopback()).then(|| {
        error_response(
            StatusCode::FORBIDDEN,
            "loopback_only",
            "The Codex bridge only accepts loopback connections",
        )
    })
}

fn validate_content_length(headers: &HeaderMap, limit: usize) -> Option<Response> {
    let value = headers.get(header::CONTENT_LENGTH)?;
    let Ok(value) = value.to_str() else {
        return Some(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_content_length",
            "Content-Length must be valid",
        ));
    };
    let Ok(length) = value.parse::<usize>() else {
        return Some(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_content_length",
            "Content-Length must be valid",
        ));
    };
    (length > limit).then(|| {
        error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "The request exceeds the bridge body limit",
        )
    })
}

async fn fail_before_stream(
    state: &AppState,
    request_id: &str,
    model: String,
    effort: String,
    failure: (StatusCode, &str, &str),
    started_at: Instant,
) -> Response {
    state
        .metrics
        .record_outcome(
            request_id.to_owned(),
            model,
            effort,
            "failed",
            started_at,
            None,
        )
        .await;
    error_response(failure.0, failure.1, failure.2)
}

fn request_metadata(request: &Value) -> (String, String) {
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let effort = request
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .unwrap_or("medium")
        .to_owned();
    (model, effort)
}

fn protocol_error_message(error: ProtocolError) -> String {
    match error {
        ProtocolError::UnsupportedField(field) => {
            format!("The local bridge does not support request field `{field}`")
        }
        other => other.to_string(),
    }
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        axum::Json(ErrorEnvelope {
            error: ErrorBody { code, message },
        }),
    )
        .into_response()
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
    data_lines: Vec<String>,
}

enum SseFrame {
    Comment(Vec<u8>),
    Data(String),
    Done,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                self.flush(&mut frames);
            } else if line.starts_with(b"data:") {
                let data = &line[5..];
                let data = if data.first() == Some(&b' ') {
                    &data[1..]
                } else {
                    data
                };
                self.data_lines
                    .push(String::from_utf8_lossy(data).into_owned());
            } else if line.starts_with(b":") && self.data_lines.is_empty() {
                let mut comment = line;
                comment.extend_from_slice(b"\n\n");
                frames.push(SseFrame::Comment(comment));
            }
        }
        frames
    }

    fn flush(&mut self, frames: &mut Vec<SseFrame>) {
        if self.data_lines.is_empty() {
            return;
        }
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        if data == "[DONE]" {
            frames.push(SseFrame::Done);
        } else {
            frames.push(SseFrame::Data(data));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_handles_fragmented_crlf_sse() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: {\"type\":\"response.").is_empty());
        let frames = decoder.push(b"completed\"}\r\n\r\n");
        assert!(
            matches!(frames.as_slice(), [SseFrame::Data(value)] if value.contains("response.completed"))
        );
    }

    #[test]
    fn decoder_forwards_done() {
        let mut decoder = SseDecoder::default();
        let frames = decoder.push(b"data: [DONE]\n\n");
        assert!(matches!(frames.as_slice(), [SseFrame::Done]));
    }
}
