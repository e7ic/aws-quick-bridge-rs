//! Amazon Quick 双向 WebSocket（AIDP）单轮对话执行器。
//! 支持 HTTP CONNECT 代理隧道、25 秒应用层+WS 双心跳、中断时发送 `stop_generation`、
//! `_chunked_frame` 分片重组以及 `resolved_model_id` 采集。

use crate::config::{AccountRecord, Settings};
use crate::messages::{argument_string, call_id, title_for, uuid_v4};
use crate::oauth::is_device_id;
use crate::qbs::UploadedAttachment;
use crate::wire::{aidp_decode, encode_frame};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;
use url::Url;

const DEFAULT_WS_PATH: &str = "/streaming/quickwork-chat-bidirectional-stream";
const READ_IDLE_SECS: u64 = 120;
const AFTER_COMPLETE_MS: u64 = 5_000;
const HANDSHAKE_SECS: u64 = 20;
const STOP_SETTLE_MS: u64 = 3_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamErrorKind {
    Handshake,
    Upstream,
    InvalidToolCall,
    Quota,
    Cancelled,
    TooLarge,
    Blocked,
    Timeout,
    Transport,
}

#[derive(Debug, Error, Clone)]
#[error("{message}")]
pub struct UpstreamError {
    pub kind: UpstreamErrorKind,
    pub message: String,
    pub status: u16,
    pub(crate) reported_usage: Option<Box<UpstreamUsage>>,
}

impl UpstreamError {
    pub fn new(kind: UpstreamErrorKind, message: impl Into<String>, status: u16) -> Self {
        Self {
            kind,
            message: message.into(),
            status,
            reported_usage: None,
        }
    }

    pub(crate) fn cancelled() -> Self {
        Self::new(
            UpstreamErrorKind::Cancelled,
            "The client cancelled the request",
            499,
        )
    }

    fn quota() -> Self {
        Self::new(
            UpstreamErrorKind::Quota,
            "Amazon Quick's allowance is used up",
            429,
        )
    }

    pub(crate) fn with_reported_usage(mut self, usage: UpstreamUsage) -> Self {
        if usage.context.is_some() {
            self.reported_usage = Some(Box::new(usage));
        }
        self
    }
}

#[derive(Debug, Clone)]
pub struct UpstreamSession {
    pub jwt: String,
    pub ws_host: String,
    pub actor_arn: String,
    pub surface: String,
    pub outpost_id: String,
    pub working_session_id: String,
}

impl UpstreamSession {
    pub fn from_auth(auth: &AccountRecord, cfg: &Settings) -> Self {
        let device = if cfg.stable_device_id && is_device_id(&auth.device_id) {
            auth.device_id.clone()
        } else {
            String::new()
        };
        let jwt = if !auth.id_token.is_empty() {
            auth.id_token.clone()
        } else {
            auth.access.clone()
        };
        Self {
            jwt,
            ws_host: auth.ws_host.clone(),
            actor_arn: auth.actor_arn.clone(),
            surface: cfg.surface.clone(),
            outpost_id: if !cfg.outpost_id.is_empty() {
                cfg.outpost_id.clone()
            } else {
                device
            },
            working_session_id: cfg.working_session_id.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TurnInput {
    pub prompt: String,
    pub model: String,
    pub thread_id: String,
    pub title: String,
    pub system_prompt: String,
    pub reminder: String,
    pub thinking_effort: String,
    pub web_search: bool,
    pub resume: bool,
    pub attachments: Vec<UploadedAttachment>,
}

#[derive(Debug, Clone, Default)]
pub struct UpstreamUsage {
    pub prompt: u64,
    pub completion: u64,
    pub total: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub resolved_model_id: String,
    pub context: Option<ContextUsage>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextUsage {
    pub used_tokens: Option<u64>,
    pub window_tokens: Option<NonZeroU64>,
}

impl UpstreamUsage {
    fn record(&mut self, event: &Map<String, Value>) {
        let count = |key: &str| event.get(key).and_then(Value::as_u64);
        let input = count("input_tokens");
        let cache_read = count("cache_read_tokens");
        let cache_write = count("cache_write_tokens");
        let output = count("output_tokens");
        let window_tokens = count("context_window")
            .and_then(NonZeroU64::new)
            .or_else(|| count("contextWindow").and_then(NonZeroU64::new));
        let uncached = input.unwrap_or(0);
        let read = cache_read.unwrap_or(0);
        let write = cache_write.unwrap_or(0);
        let known_input = uncached
            .checked_add(read)
            .and_then(|total| total.checked_add(write));
        let used_tokens = if input.is_some() && cache_read.is_some() && cache_write.is_some() {
            known_input
        } else {
            count("total_tokens")
                .zip(output)
                .and_then(|(total, output)| total.checked_sub(output))
                .filter(|&used| known_input.is_some_and(|known| used >= known))
        };

        if let Some(model) = ["resolved_model_id", "served_model_id"]
            .into_iter()
            .filter_map(|key| event.get(key)?.as_str())
            .find(|model| !model.is_empty())
        {
            if !self.resolved_model_id.is_empty() && self.resolved_model_id != model {
                self.context = None;
            }
            self.resolved_model_id = model.to_owned();
        }
        // The input and the window belong to the same reported model step.
        // Output-only updates must not replace the measured input.
        if used_tokens.is_some() || (window_tokens.is_some() && self.context.is_none()) {
            self.context = Some(ContextUsage {
                used_tokens,
                window_tokens,
            });
        }

        if uncached > 0 || read > 0 || write > 0 {
            self.prompt = uncached;
            self.cache_read = read;
            self.cache_write = write;
        }
        self.completion = self.completion.saturating_add(output.unwrap_or(0));
        self.total = self
            .prompt
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.completion);
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;

    #[test]
    fn context_follows_the_latest_measured_input_and_window() {
        let mut usage = UpstreamUsage::default();
        let mut record = |event: Value| {
            usage.record(event.as_object().unwrap());
            usage.context
        };
        assert_eq!(record(json!({"output_tokens": 3})), None);
        let first = record(json!({
            "total_tokens": 120, "output_tokens": 20,
            "context_window": 320_000, "resolved_model_id": "first"
        })).unwrap();
        assert_eq!(first.used_tokens, Some(100));
        assert_eq!(first.window_tokens.map(NonZeroU64::get), Some(320_000));
        assert_eq!(record(json!({"output_tokens": 7})), Some(first));

        let second = record(json!({
            "input_tokens": 90, "cache_read_tokens": 0, "cache_write_tokens": 10,
            "context_window": 0, "served_model_id": "second"
        })).unwrap();
        assert_eq!(second.used_tokens, Some(100));
        assert_eq!(second.window_tokens, None);
    }
}

#[derive(Debug, Clone)]
pub struct QuickToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub result: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub enum UpstreamEvent {
    Reasoning(String),
    Text(String),
    ToolCall(QuickToolCall),
    ToolCallUpdate(QuickToolCall),
}

#[derive(Debug, Clone)]
pub struct UpstreamResult {
    pub text: String,
    pub tool_calls: Vec<QuickToolCall>,
    pub usage: UpstreamUsage,
}

pub fn user_message(session: &UpstreamSession, cfg: &Settings, input: &TurnInput) -> Value {
    let text = if input.reminder.is_empty() {
        input.prompt.clone()
    } else {
        format!("{}\n\n{}", input.prompt, input.reminder)
    };
    let agent_mode = if input.model == "auto" {
        "smart"
    } else {
        input.model.as_str()
    };
    let custom_prompt = if input.system_prompt.trim().is_empty() {
        Value::Null
    } else {
        Value::String(input.system_prompt.clone())
    };
    let mut payload = serde_json::Map::new();
    payload.insert("text".into(), json!(text));
    payload.insert("send_origin".into(), json!("programmatic"));
    payload.insert("had_physical_input".into(), json!(false));
    payload.insert("surfaceType".into(), json!(session.surface));
    payload.insert("outpostId".into(), json!(session.outpost_id));
    payload.insert("appVersion".into(), json!(cfg.app_version));
    payload.insert("traceId".into(), json!(uuid_v4()));
    payload.insert("message_id".into(), json!(uuid_v4()));
    payload.insert("thread_id".into(), json!(input.thread_id));
    payload.insert("agent_mode".into(), json!(agent_mode));
    payload.insert("user_timezone".into(), json!(cfg.timezone));
    payload.insert("user_locale".into(), json!(cfg.locale));
    payload.insert("chat_agent_id".into(), Value::Null);
    payload.insert("auto_routing".into(), json!(input.model == "auto"));
    payload.insert("web_search_enabled".into(), json!(input.web_search));
    payload.insert("pre_approved_tools".into(), json!([]));
    payload.insert("preload_skills".into(), json!([]));
    payload.insert("panel_edits".into(), json!([]));
    payload.insert("custom_prompt".into(), custom_prompt);
    payload.insert("incognito".into(), json!(!cfg.keep_sessions));
    if !input.thinking_effort.is_empty() && input.thinking_effort != "off" {
        payload.insert("thinking_effort".into(), json!(input.thinking_effort));
    }
    if !input.attachments.is_empty() {
        payload.insert(
            "attachments".into(),
            serde_json::to_value(&input.attachments).unwrap_or(json!([])),
        );
    }
    json!({
        "type": "user_message",
        "payload": Value::Object(payload),
    })
}

pub fn frame_bytes(value: &Value) -> usize {
    serde_json::to_string(value).map(|s| s.len()).unwrap_or(0)
}

pub struct DialInfo {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

pub fn dial_info(sess: &UpstreamSession) -> Result<DialInfo, UpstreamError> {
    if sess.jwt.is_empty() {
        return Err(UpstreamError::new(
            UpstreamErrorKind::Handshake,
            "the account has no token; sign in again",
            401,
        ));
    }
    if sess.ws_host.trim().is_empty() {
        return Err(UpstreamError::new(
            UpstreamErrorKind::Handshake,
            "the account has no ws_host; sign in again",
            401,
        ));
    }
    let mut host = sess.ws_host.trim().trim_end_matches('/').to_string();
    if let Some(rest) = host.strip_prefix("https://") {
        host = format!("wss://{rest}");
    } else if let Some(rest) = host.strip_prefix("http://") {
        host = format!("ws://{rest}");
    } else if !host.starts_with("wss://") && !host.starts_with("ws://") {
        host = format!("wss://{host}");
    }

    let target = format!("{host}{DEFAULT_WS_PATH}");

    let mut url = Url::parse(&target).map_err(|_| {
        UpstreamError::new(
            UpstreamErrorKind::Transport,
            format!("invalid ws_url {target:?}"),
            0,
        )
    })?;

    {
        let mut pairs = url.query_pairs_mut();
        if !sess.surface.is_empty() {
            pairs.append_pair("surfaceType", &sess.surface);
        }
        if !sess.outpost_id.is_empty() {
            pairs.append_pair("outpostId", &sess.outpost_id);
        }
        if !sess.working_session_id.is_empty() {
            pairs.append_pair("workingSessionId", &sess.working_session_id);
        }
        pairs.append_pair("token", &sess.jwt);
    }

    let mut headers = vec![
        ("Authorization".to_string(), format!("Bearer {}", sess.jwt)),
        ("Origin".to_string(), "file://".to_string()),
        ("X-Surface-Type".to_string(), sess.surface.clone()),
        ("X-Outpost-Id".to_string(), sess.outpost_id.clone()),
        (
            "X-Agent-Service-Type".to_string(),
            "UNIFIED_QUICK".to_string(),
        ),
    ];
    if !sess.working_session_id.is_empty() {
        headers.push((
            "X-Working-Session-Id".to_string(),
            sess.working_session_id.clone(),
        ));
    }
    Ok(DialInfo {
        url: url.to_string(),
        headers,
    })
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host == "::1"
        || host.starts_with("127.")
}

pub fn proxy_for(ws_url: &str, cfg: &Settings) -> String {
    let own = cfg.proxy.trim();
    if matches!(own.to_ascii_lowercase().as_str(), "direct" | "none" | "off") {
        return String::new();
    }
    if !own.is_empty() {
        return own.to_string();
    }
    let Ok(to) = Url::parse(ws_url) else {
        return String::new();
    };
    let host = to
        .host_str()
        .unwrap_or("")
        .to_ascii_lowercase()
        .trim_matches(|c| c == '[' || c == ']')
        .to_string();
    if is_loopback(&host) {
        return String::new();
    }
    let read_env = |k: &str| -> String {
        for name in [
            format!("MAGPIE_{k}"),
            format!("MAGPIE_{}", k.to_ascii_lowercase()),
            k.to_string(),
            k.to_ascii_lowercase(),
        ] {
            if let Ok(v) = std::env::var(&name) {
                let t = v.trim();
                if !t.is_empty() {
                    return t.to_string();
                }
            }
        }
        String::new()
    };
    let no_proxy = read_env("NO_PROXY");
    for item in no_proxy.split(',') {
        let pat = item
            .trim()
            .to_ascii_lowercase()
            .trim_start_matches('*')
            .trim_start_matches('.')
            .to_string();
        if pat.is_empty() {
            continue;
        }
        if pat == "*" || host == pat || host.ends_with(&format!(".{pat}")) {
            return String::new();
        }
    }
    let secure = to.scheme() == "wss" || to.scheme() == "https";
    for key in [if secure { "HTTPS_PROXY" } else { "HTTP_PROXY" }, "ALL_PROXY"] {
        let val = read_env(key);
        if val.starts_with("http://") || val.starts_with("https://") {
            return val;
        }
    }
    String::new()
}

/// 直接或通过 HTTP `CONNECT` 代理隧道建立上游 TCP 连接。
async fn connect_tcp_maybe_proxy(ws_url: &str, proxy: &str) -> Result<TcpStream, UpstreamError> {
    let target = Url::parse(ws_url).map_err(|e| {
        UpstreamError::new(UpstreamErrorKind::Transport, format!("invalid url: {e}"), 0)
    })?;
    let target_host = target.host_str().unwrap_or("");
    let target_port = target
        .port_or_known_default()
        .unwrap_or(if target.scheme() == "wss" { 443 } else { 80 });

    if proxy.is_empty() {
        return TcpStream::connect((target_host, target_port))
            .await
            .map_err(|e| {
                UpstreamError::new(
                    UpstreamErrorKind::Handshake,
                    format!("TCP connect failed: {e}"),
                    0,
                )
            });
    }

    let pu = Url::parse(proxy).map_err(|e| {
        UpstreamError::new(UpstreamErrorKind::Transport, format!("invalid proxy url: {e}"), 0)
    })?;
    let phost = pu.host_str().unwrap_or("127.0.0.1");
    let pport = pu.port_or_known_default().unwrap_or(8080);
    let mut stream = TcpStream::connect((phost, pport)).await.map_err(|e| {
        UpstreamError::new(
            UpstreamErrorKind::Handshake,
            format!("proxy TCP connect failed: {e}"),
            0,
        )
    })?;

    let mut req = format!(
        "CONNECT {target_host}:{target_port} HTTP/1.1\r\nHost: {target_host}:{target_port}\r\n"
    );
    if !pu.username().is_empty() {
        let creds = format!("{}:{}", pu.username(), pu.password().unwrap_or(""));
        req.push_str(&format!(
            "Proxy-Authorization: Basic {}\r\n",
            B64.encode(creds.as_bytes())
        ));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.map_err(|e| {
        UpstreamError::new(
            UpstreamErrorKind::Handshake,
            format!("proxy CONNECT write failed: {e}"),
            0,
        )
    })?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).await.map_err(|e| {
        UpstreamError::new(
            UpstreamErrorKind::Handshake,
            format!("proxy CONNECT read failed: {e}"),
            0,
        )
    })?;
    if !status_line.contains(" 200") {
        return Err(UpstreamError::new(
            UpstreamErrorKind::Handshake,
            format!("proxy CONNECT refused: {}", status_line.trim()),
            0,
        ));
    }
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.map_err(|e| {
            UpstreamError::new(
                UpstreamErrorKind::Handshake,
                format!("proxy CONNECT header read failed: {e}"),
                0,
            )
        })?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    Ok(reader.into_inner())
}

fn delta_text(payload: Option<&Value>) -> String {
    match payload {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(obj)) => {
            if let Some(d) = obj.get("delta").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                return d.to_string();
            }
            obj.get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        }
        _ => String::new(),
    }
}

fn raw_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn error_text(env: &Value) -> String {
    let p = env.get("payload").and_then(|v| v.as_object());
    let candidates = [
        p.and_then(|o| o.get("message")),
        p.and_then(|o| o.get("error")),
        env.get("message"),
        env.get("error"),
        p.and_then(|o| o.get("code")),
        env.get("code"),
        env.get("error_code"),
    ];
    for c in candidates.into_iter().flatten() {
        if let Some(s) = c.as_str() {
            if !s.trim().is_empty() {
                return s.to_string();
            }
        }
    }
    "unknown upstream error".to_string()
}

fn is_terminal(t: &str) -> bool {
    matches!(
        t,
        "turn_completed" | "turn_aborted" | "turn_ended" | "stream_ended" | "chat_complete"
    )
}

fn is_failure(t: &str) -> bool {
    matches!(
        t,
        "error" | "session_error" | "stream_error" | "turn_failed" | "stream_truncated"
    )
}

const QUOTA_NOTICES: &[&str] = &[
    "you've reached your usage limit. check your profile for details.",
    "you have reached your usage limit. check your profile for details.",
];

fn quota_notice_text(text: &str) -> String {
    text.replace(['‘', '’'], "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn is_quota_notice(text: &str) -> bool {
    let text = quota_notice_text(text);
    QUOTA_NOTICES
        .iter()
        .any(|notice| text == *notice || notice.strip_suffix('.') == Some(text.as_str()))
}

fn is_quota_error(envelope: &Value) -> bool {
    [&envelope["payload"], envelope].iter().any(|value| {
        [
            "error_type",
            "errorType",
            "failure_type",
            "failureType",
            "code",
            "reason",
            "error",
            "type",
        ]
        .iter()
        .any(|key| value[key].as_str() == Some("usage_limit"))
    })
}

fn push_reply_text(
    text: &mut String,
    pending: &mut bool,
    delta: String,
    emit: &impl Fn(UpstreamEvent),
) {
    if delta.is_empty() {
        return;
    }
    text.push_str(&delta);
    if *pending {
        // Quick may return a quota error as an entire assistant message. Hold
        // only its possible prefix, and release it as soon as the reply differs.
        if text.len() <= 256 {
            let prefix = quota_notice_text(text);
            if QUOTA_NOTICES
                .iter()
                .any(|notice| notice.starts_with(&prefix))
            {
                return;
            }
        }
        *pending = false;
        emit(UpstreamEvent::Text(text.clone()));
    } else {
        emit(UpstreamEvent::Text(delta));
    }
}

struct ChunkEntry {
    parts: Vec<Option<String>>,
    have: usize,
}

pub(crate) async fn wait_for_cancellation(cancel: &mut watch::Receiver<bool>) {
    let closed = cancel.wait_for(|&cancelled| cancelled).await.is_err();
    if closed {
        // A dropped sender with a false value is not a cancellation.
        std::future::pending::<()>().await;
    }
}

/// 通过 WebSocket 向 Amazon Quick 发送一轮对话并收集流式事件与用量。
pub async fn chat(
    session: &UpstreamSession,
    cfg: &Settings,
    input: &TurnInput,
    mut cancel_rx: watch::Receiver<bool>,
    events_tx: Option<mpsc::UnboundedSender<UpstreamEvent>>,
) -> Result<UpstreamResult, UpstreamError> {
    let dial = dial_info(session)?;
    let thread_id = if input.thread_id.trim().is_empty() {
        uuid_v4()
    } else {
        input.thread_id.trim().to_string()
    };
    let title = if input.title.trim().is_empty() {
        title_for(&input.prompt)
    } else {
        input.title.trim().to_string()
    };

    let proxy = proxy_for(&dial.url, cfg);
    let mut ws_req = dial.url.as_str().into_client_request().map_err(|e| {
        UpstreamError::new(UpstreamErrorKind::Transport, format!("ws request error: {e}"), 0)
    })?;
    for (k, v) in &dial.headers {
        if let (Ok(name), Ok(val)) = (
            tokio_tungstenite::tungstenite::http::header::HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            ws_req.headers_mut().insert(name, val);
        }
    }

    let connect_fut = async {
        let tcp = connect_tcp_maybe_proxy(&dial.url, &proxy).await?;
        let (ws, _) = tokio_tungstenite::client_async_tls_with_config(ws_req, tcp, None, None)
            .await
            .map_err(|e| match &e {
                tokio_tungstenite::tungstenite::Error::Http(resp) => UpstreamError::new(
                    UpstreamErrorKind::Handshake,
                    format!("upstream WebSocket dial failed HTTP {}", resp.status().as_u16()),
                    resp.status().as_u16(),
                ),
                other => UpstreamError::new(
                    UpstreamErrorKind::Handshake,
                    format!("upstream WebSocket dial failed: {other}"),
                    0,
                ),
            })?;
        Ok::<_, UpstreamError>(ws)
    };

    let connected = tokio::select! {
        biased;
        _ = wait_for_cancellation(&mut cancel_rx) => {
            return Err(UpstreamError::cancelled());
        }
        result = tokio::time::timeout(Duration::from_secs(HANDSHAKE_SECS), connect_fut) => result,
    };
    let mut ws = connected.map_err(|_| {
        UpstreamError::new(
            UpstreamErrorKind::Timeout,
            "upstream WebSocket handshake timed out",
            0,
        )
    })??;

    let send_frame = async |ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<TcpStream>,
    >,
                                val: &Value|
           -> Result<(), UpstreamError> {
        // AIDP 出站帧为 UTF-8 JSON，与原客户端一致以 Binary 帧发送。
        let bytes = encode_frame(val);
        ws.send(Message::Binary(bytes.into())).await.map_err(|e| {
            UpstreamError::new(UpstreamErrorKind::Transport, format!("send failed: {e}"), 0)
        })
    };

    // 1. 发送 session_connect 握手帧
    send_frame(
        &mut ws,
        &json!({
            "type": "session_connect",
            "payload": {
                "actor_id": session.actor_arn,
                "surfaceType": session.surface,
                "outpostId": session.outpost_id,
                "appVersion": cfg.app_version,
                "external_agent_sign_in": 1,
                "recents_promotion": 1,
            }
        }),
    )
    .await?;
    // 2. 发送 federate_status 同步帧
    send_frame(&mut ws, &json!({ "type": "federate_status", "payload": {} })).await?;
    // 3. 若启用了 keep_sessions 且为新会话，创建远端会话记录
    if cfg.keep_sessions && !input.resume {
        send_frame(
            &mut ws,
            &json!({
                "type": "session_create",
                "payload": { "session_id": thread_id, "title": title }
            }),
        )
        .await?;
    }
    // 4. 发送 user_message 对话内容
    if *cancel_rx.borrow() {
        return Err(UpstreamError::cancelled());
    }
    let mut turn_input = input.clone();
    turn_input.thread_id = thread_id.clone();
    let um = user_message(session, cfg, &turn_input);
    send_frame(&mut ws, &um).await?;

    let mut usage = UpstreamUsage::default();
    let mut chunks: HashMap<String, ChunkEntry> = HashMap::new();
    let mut tools: Vec<QuickToolCall> = Vec::new();
    let mut tool_index: HashMap<String, usize> = HashMap::new();
    let mut sent_tools: HashSet<String> = HashSet::new();
    let mut partial_args: HashMap<String, String> = HashMap::new();
    let mut text = String::new();
    let mut pending_text = true;
    let mut thought = String::new();
    let mut live = true;

    let keepalive_dur = Duration::from_millis(if cfg.keepalive_ms > 0 { cfg.keepalive_ms } else { 25_000 });
    let mut ping_interval = tokio::time::interval(keepalive_dur);
    ping_interval.tick().await; // 跳过首次立即触发的 tick

    let req_deadline = tokio::time::Instant::now() + Duration::from_millis(cfg.request_timeout_ms);
    let mut idle_deadline = tokio::time::Instant::now() + Duration::from_secs(READ_IDLE_SECS);
    let mut complete_deadline: Option<tokio::time::Instant> = None;

    let emit = |ev: UpstreamEvent| {
        if let Some(tx) = &events_tx {
            let _ = tx.send(ev);
        }
    };

    let mut finish_result: Option<Result<(), UpstreamError>> = None;

    // 迭代式事件处理队列，便于在 `_chunked_frame` 重组完成后继续入队处理。
    let mut handle_envelope = |root_env: Value,
                               live: &mut bool,
                               complete_deadline: &mut Option<tokio::time::Instant>|
     -> Option<Result<(), UpstreamError>> {
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(root_env);
        while let Some(env) = queue.pop_front() {
            let typ = env.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let p_val = env.get("payload");
            let p = p_val.and_then(|v| v.as_object());

            if (is_failure(typ) || typ == "guardrail_blocked") && is_quota_error(&env) {
                *live = false;
                return Some(Err(UpstreamError::quota()));
            }

            match typ {
                "_chunked_frame" => {
                    let src = p_val.filter(|v| v.is_object()).unwrap_or(&env);
                    let cid = src.get("chunk_id").and_then(|v| v.as_str()).unwrap_or("");
                    let idx = src.get("chunk_index").and_then(|v| v.as_u64());
                    let total = src.get("total_chunks").and_then(|v| v.as_u64());
                    let data = src.get("data").and_then(|v| v.as_str());
                    if let (Some(idx), Some(total), Some(data)) = (idx, total, data) {
                        let (idx, total) = (idx as usize, total as usize);
                        if !cid.is_empty() && (1..=4096).contains(&total) && idx < total {
                            let entry = chunks.entry(cid.to_string()).or_insert_with(|| ChunkEntry {
                                parts: vec![None; total],
                                have: 0,
                            });
                            if entry.parts.len() != total {
                                *entry = ChunkEntry {
                                    parts: vec![None; total],
                                    have: 0,
                                };
                            }
                            if entry.parts[idx].is_none() {
                                entry.have += 1;
                            }
                            entry.parts[idx] = Some(data.to_string());
                            if entry.have >= total {
                                if let Some(done_entry) = chunks.remove(cid) {
                                    let joined: String = done_entry
                                        .parts
                                        .into_iter()
                                        .flatten()
                                        .collect();
                                    if let Ok(parsed) = serde_json::from_str::<Value>(&joined) {
                                        if parsed.is_object() {
                                            queue.push_back(parsed);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                "thinking_delta" => {
                    let delta = delta_text(p_val);
                    if !delta.is_empty() {
                        thought.push_str(&delta);
                        emit(UpstreamEvent::Reasoning(delta));
                    }
                }
                "guardrail_blocked" => {
                    *live = false;
                    let msg = p
                        .and_then(|o| o.get("message"))
                        .or_else(|| env.get("message"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("Amazon Quick's guardrail blocked the reply");
                    return Some(Err(UpstreamError::new(
                        UpstreamErrorKind::Blocked,
                        msg,
                        400,
                    )));
                }
                "text_delta" => {
                    push_reply_text(&mut text, &mut pending_text, delta_text(p_val), &emit);
                }
                "text_complete" => {
                    if text.is_empty() {
                        push_reply_text(&mut text, &mut pending_text, delta_text(p_val), &emit);
                    }
                }
                "turn_usage" => {
                    if let Some(o) = p {
                        usage.record(o);
                    }
                }
                "tool_start" => {
                    if let Some(o) = p {
                        let tid = o
                            .get("tool_id")
                            .or_else(|| o.get("tool_call_id"))
                            .or_else(|| o.get("toolCallId"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim();
                        let known = !tid.is_empty() && tool_index.contains_key(tid);
                        let id = if tid.is_empty() { call_id() } else { tid.to_string() };
                        let idx = *tool_index.entry(id.clone()).or_insert_with(|| {
                            tools.push(QuickToolCall {
                                id: id.clone(),
                                name: String::new(),
                                arguments: String::new(),
                                result: String::new(),
                                status: String::new(),
                            });
                            tools.len() - 1
                        });
                        if let Some(n) = o.get("tool_name").and_then(|v| v.as_str()) {
                            if !n.trim().is_empty() {
                                tools[idx].name = n.trim().to_string();
                            }
                        }
                        let args = argument_string(o.get("arguments"));
                        if args != "{}" {
                            tools[idx].arguments = args;
                        }
                        if !known && cfg.show_quick_tools && !tools[idx].name.is_empty() {
                            let prefix = if !thought.is_empty() && !thought.ends_with('\n') {
                                "\n"
                            } else {
                                ""
                            };
                            let note = format!("{prefix}[Quick: {}]\n", tools[idx].name);
                            thought.push_str(&note);
                            emit(UpstreamEvent::Reasoning(note));
                        }
                    }
                }
                "tool_arguments" | "tool_arguments_edited" | "tool_call" | "tool_call_update" => {
                    if let Some(o) = p {
                        let tid = o
                            .get("tool_id")
                            .or_else(|| o.get("tool_call_id"))
                            .or_else(|| o.get("toolCallId"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim();
                        let id = if tid.is_empty() { call_id() } else { tid.to_string() };
                        let idx = *tool_index.entry(id.clone()).or_insert_with(|| {
                            tools.push(QuickToolCall {
                                id: id.clone(),
                                name: String::new(),
                                arguments: String::new(),
                                result: String::new(),
                                status: String::new(),
                            });
                            tools.len() - 1
                        });
                        if let Some(n) = o
                            .get("tool_name")
                            .or_else(|| o.get("title"))
                            .and_then(|v| v.as_str())
                        {
                            if !n.trim().is_empty() {
                                tools[idx].name = n.trim().to_string();
                            }
                        }
                        if let Some(args) = o
                            .get("rawInput")
                            .or_else(|| o.get("raw_input"))
                            .or_else(|| o.get("arguments"))
                            .filter(|v| !v.is_null())
                        {
                            tools[idx].arguments = argument_string(Some(args));
                        }
                        let tc = tools[idx].clone();
                        if sent_tools.insert(tc.id.clone()) {
                            emit(UpstreamEvent::ToolCall(tc));
                        } else {
                            emit(UpstreamEvent::ToolCallUpdate(tc));
                        }
                    }
                }
                "tool_progress" => {
                    if let Some(o) = p {
                        let tid = o
                            .get("tool_id")
                            .or_else(|| o.get("tool_call_id"))
                            .or_else(|| o.get("toolCallId"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let delta = o
                            .get("input_delta")
                            .or_else(|| o.get("progress").and_then(|pr| pr.get("input_delta")))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if !tid.is_empty() && !delta.is_empty() {
                            partial_args
                                .entry(tid.to_string())
                                .or_default()
                                .push_str(delta);
                        }
                    }
                }
                "tool_result" | "tool_error" | "tool_complete" => {
                    if let Some(o) = p {
                        let tid = o
                            .get("tool_id")
                            .or_else(|| o.get("tool_call_id"))
                            .or_else(|| o.get("toolCallId"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim();
                        let id = if tid.is_empty() { call_id() } else { tid.to_string() };
                        let idx = *tool_index.entry(id.clone()).or_insert_with(|| {
                            tools.push(QuickToolCall {
                                id: id.clone(),
                                name: String::new(),
                                arguments: String::new(),
                                result: String::new(),
                                status: String::new(),
                            });
                            tools.len() - 1
                        });
                        if let Some(n) = o.get("tool_name").and_then(|v| v.as_str()) {
                            if !n.trim().is_empty() {
                                tools[idx].name = n.trim().to_string();
                            }
                        }
                        if typ == "tool_result" {
                            if let Some(args) = o.get("arguments").filter(|v| !v.is_null()) {
                                tools[idx].arguments = argument_string(Some(args));
                            }
                            if let Some(err) = o.get("error").filter(|v| !v.is_null()) {
                                tools[idx].result = raw_text(Some(err));
                                tools[idx].status = "error".to_string();
                            } else {
                                tools[idx].result = raw_text(o.get("result"));
                                tools[idx].status = raw_text(o.get("status"));
                            }
                        } else if typ == "tool_error" {
                            tools[idx].status = "error".to_string();
                            tools[idx].result =
                                raw_text(o.get("error").or_else(|| o.get("message")));
                        } else if tools[idx].status.is_empty() {
                            tools[idx].status = "complete".to_string();
                        }
                        let tc = tools[idx].clone();
                        if sent_tools.insert(tc.id.clone()) {
                            emit(UpstreamEvent::ToolCall(tc));
                        }
                    }
                }
                "message_complete" => {
                    if text.is_empty() {
                        push_reply_text(&mut text, &mut pending_text, delta_text(p_val), &emit);
                    }
                    let cont = p
                        .and_then(|o| o.get("continuation_expected"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if !cont && pending_text && is_quota_notice(&text) {
                        *live = false;
                        return Some(Err(UpstreamError::quota()));
                    }

                    if let Some(items) = p.and_then(|o| o.get("tool_calls")).and_then(|v| v.as_array())
                    {
                        for item in items {
                            let tid = item
                                .get("id")
                                .or_else(|| item.get("tool_call_id"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .trim();
                            let id = if tid.is_empty() { call_id() } else { tid.to_string() };
                            let idx = *tool_index.entry(id.clone()).or_insert_with(|| {
                                tools.push(QuickToolCall {
                                    id: id.clone(),
                                    name: String::new(),
                                    arguments: String::new(),
                                    result: String::new(),
                                    status: String::new(),
                                });
                                tools.len() - 1
                            });
                            if let Some(n) = item
                                .get("name")
                                .or_else(|| item.get("tool_name"))
                                .or_else(|| item.get("function").and_then(|f| f.get("name")))
                                .and_then(|v| v.as_str())
                            {
                                if !n.trim().is_empty() {
                                    tools[idx].name = n.trim().to_string();
                                }
                            }
                            if let Some(args) = item
                                .get("arguments")
                                .or_else(|| item.get("function").and_then(|f| f.get("arguments")))
                                .filter(|v| !v.is_null())
                            {
                                tools[idx].arguments = argument_string(Some(args));
                            }
                            let tc = tools[idx].clone();
                            if sent_tools.insert(tc.id.clone()) {
                                emit(UpstreamEvent::ToolCall(tc));
                            }
                        }
                    }
                    *live = cont;
                    *complete_deadline = if cont {
                        None
                    } else {
                        Some(tokio::time::Instant::now() + Duration::from_millis(AFTER_COMPLETE_MS))
                    };
                }
                _ => {
                    if is_failure(typ) {
                        *live = false;
                        let code = p
                            .and_then(|o| o.get("code"))
                            .or_else(|| env.get("code"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let msg = error_text(&env);
                        if code == "FrameTooLarge" {
                            return Some(Err(UpstreamError::new(
                                UpstreamErrorKind::TooLarge,
                                msg,
                                400,
                            )));
                        }
                        return Some(Err(UpstreamError::new(
                            UpstreamErrorKind::Upstream,
                            format!("upstream {typ}: {msg}"),
                            502,
                        )));
                    }
                    if is_terminal(typ) {
                        *live = false;
                        if typ == "turn_aborted" {
                            return Some(Err(UpstreamError::new(
                                UpstreamErrorKind::Transport,
                                "Amazon Quick aborted the turn",
                                502,
                            )));
                        }
                        return Some(Ok(()));
                    }
                }
            }
        }
        None
    };

    let closed_turn = |completed, message| {
        if completed {
            Ok(())
        } else {
            Err(UpstreamError::new(
                UpstreamErrorKind::Transport,
                message,
                502,
            ))
        }
    };
    loop {
        tokio::select! {
            biased;
            _ = wait_for_cancellation(&mut cancel_rx) => {
                finish_result = Some(Err(UpstreamError::cancelled()));
                break;
            }
            _ = tokio::time::sleep_until(req_deadline) => {
                finish_result = Some(if complete_deadline.is_some() {
                    Ok(())
                } else {
                    Err(UpstreamError::new(UpstreamErrorKind::Timeout, "the upstream request timed out", 504))
                });
                break;
            }
            _ = tokio::time::sleep_until(idle_deadline) => {
                finish_result = Some(Err(UpstreamError::new(UpstreamErrorKind::Timeout, "upstream WebSocket read timed out", 504)));
                break;
            }
            _ = async {
                match complete_deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                finish_result = Some(Ok(()));
                break;
            }
            _ = ping_interval.tick() => {
                let ts = Utc::now().timestamp_millis();
                let _ = send_frame(&mut ws, &json!({ "type": "ping", "payload": { "timestamp": ts } })).await;
                let _ = ws.send(Message::Ping(b"ping".to_vec().into())).await;
            }
            maybe_msg = ws.next() => {
                match maybe_msg {
                    None => {
                        live = false;
                        finish_result = Some(closed_turn(complete_deadline.is_some(), "upstream WebSocket closed".to_owned()));
                        break;
                    }
                    Some(Err(e)) => {
                        live = false;
                        finish_result = Some(closed_turn(complete_deadline.is_some(), format!("upstream WebSocket read: {e}")));
                        break;
                    }
                    Some(Ok(msg)) => {
                        let frames: Vec<String> = match msg {
                            Message::Text(t) => vec![t.to_string()],
                            Message::Binary(b) => {
                                match aidp_decode(&b) {
                                    Ok(fs) => fs,
                                    Err(e) => {
                                        let em = e.to_string();
                                        if em.starts_with("upstream error [") {
                                            live = false;
                                            finish_result = Some(Err(UpstreamError::new(UpstreamErrorKind::Upstream, em, 502)));
                                            break;
                                        }
                                        continue;
                                    }
                                }
                            }
                            Message::Ping(p) => {
                                let _ = ws.send(Message::Pong(p)).await;
                                continue;
                            }
                            Message::Pong(_) => continue,
                            Message::Close(c) => {
                                live = false;
                                let message = c
                                    .map(|cf| format!("upstream WebSocket closed ({}) {}", cf.code, cf.reason))
                                    .unwrap_or_else(|| "upstream WebSocket closed".to_owned());
                                finish_result = Some(closed_turn(complete_deadline.is_some(), message));
                                break;
                            }
                            _ => continue,
                        };

                        for frame in frames {
                            let Ok(env) = serde_json::from_str::<Value>(&frame) else { continue };
                            if !env.is_object() { continue; }
                            let kind = env["type"].as_str().unwrap_or("");
                            if kind.starts_with("tool_") || matches!(
                                kind,
                                "thinking_delta" | "text_delta" | "text_complete" | "message_start"
                                    | "message_complete" | "turn_usage" | "_chunked_frame"
                            ) {
                                idle_deadline = tokio::time::Instant::now() + Duration::from_secs(READ_IDLE_SECS);
                            }
                            if let Some(done) = handle_envelope(env, &mut live, &mut complete_deadline) {
                                finish_result = Some(done);
                                break;
                            }
                        }
                        if finish_result.is_some() {
                            break;
                        }
                    }
                }
            }
        }
    }

    // 若上游仍在生成阶段提前退出，主动发送 `stop_generation` 优雅终止。
    if live {
        let stop_msg = json!({ "type": "stop_generation", "payload": { "thread_id": thread_id } });
        if send_frame(&mut ws, &stop_msg).await.is_ok() {
            let settle = tokio::time::Instant::now() + Duration::from_millis(STOP_SETTLE_MS);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(settle) => break,
                    m = ws.next() => {
                        let Some(Ok(msg)) = m else { break };
                        let frames = match msg {
                            Message::Text(t) => vec![t.to_string()],
                            Message::Binary(b) => aidp_decode(&b).unwrap_or_default(),
                            Message::Close(_) => break,
                            _ => continue,
                        };
                        let mut done = false;
                        for frame in frames {
                            let Ok(event) = serde_json::from_str::<Value>(&frame) else { continue };
                            let kind = event["type"].as_str().unwrap_or("");
                            if kind == "turn_usage" {
                                if let Some(payload) = event["payload"].as_object() {
                                    usage.record(payload);
                                }
                            }
                            done |= kind == "message_complete" || is_terminal(kind) || is_failure(kind);
                        }
                        if done { break; }
                    }
                }
            }
        }
    }
    let _ = ws.close(None).await;

    match finish_result.unwrap_or(Ok(())) {
        Ok(()) => {
            if pending_text && is_quota_notice(&text) {
                return Err(UpstreamError::quota().with_reported_usage(usage));
            }
            if pending_text && !text.is_empty() {
                emit(UpstreamEvent::Text(text.clone()));
            }
            for tc in &mut tools {
                if tc.name.trim().is_empty() {
                    tc.name = "unknown".to_string();
                }
                if tc.arguments.trim().is_empty() {
                    let partial = partial_args
                        .get(&tc.id)
                        .map(|s| s.trim())
                        .unwrap_or("");
                    if !partial.is_empty() && serde_json::from_str::<Value>(partial).is_ok() {
                        tc.arguments = partial.to_string();
                    } else {
                        tc.arguments = "{}".to_string();
                    }
                }
            }
            Ok(UpstreamResult {
                text,
                tool_calls: tools,
                usage,
            })
        }
        Err(e) => Err(e.with_reported_usage(usage)),
    }
}
