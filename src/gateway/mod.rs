//! 独立多协议 HTTP 网关：在自主多账号池之上提供 OpenAI Chat Completions、
//! Anthropic Messages 与 OpenAI Responses 接口。

pub mod anthropic;
pub mod ccswitch;
pub(crate) mod claude_desktop;
pub mod responses;

use crate::chat::{
    ChatFailure, ChatOutput, NormalizedUsage, TurnEvent, prepare, run_turn_for_account,
};
use crate::config::{
    AccountRecord, account_models, model_display_name, quick_model, resolve_quick_model,
};
use crate::pool::{AccountPool, LogLevel, RequestLogEntry};
use crate::qbs;
use crate::threads::{ThreadHistory, conversation_of};
use anyhow::Result;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_stream::wrappers::UnboundedReceiverStream;

const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
const SIGN_IN_MSG: &str = "Amazon Quick is not signed in; sign in via GUI or run: aws-quick-bridge-rs login";

fn same_key(given: &str, expected: &str) -> bool {
    !given.is_empty()
        && given.len() == expected.len()
        && bool::from(given.as_bytes().ct_eq(expected.as_bytes()))
}

fn extract_key(headers: &HeaderMap) -> String {
    if let Some(k) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        if !k.trim().is_empty() {
            return k.trim().to_string();
        }
    }
    if let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let trimmed = auth.trim();
        if let Some(rest) = trimmed
            .strip_prefix("Bearer ")
            .or_else(|| trimmed.strip_prefix("bearer "))
        {
            return rest.trim().to_string();
        }
        return trimmed.to_string();
    }
    String::new()
}

fn normalize_route(path: &str) -> String {
    let mut p = path.trim_end_matches('/').to_string();
    while let Some(rest) = p.strip_prefix("/v1") {
        if rest.is_empty() || rest.starts_with('/') {
            p = rest.to_string();
        } else {
            break;
        }
    }
    if p.is_empty() { "/".to_string() } else { p }
}

fn openai_error(status: u16, message: &str, code: &str, retry_after: Option<u64>) -> Response {
    let st = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut resp = (
        st,
        axum::Json(json!({
            "error": { "message": message, "type": code, "code": code }
        })),
    )
        .into_response();
    if let Some(secs) = retry_after {
        if let Ok(hv) = header::HeaderValue::from_str(&secs.to_string()) {
            resp.headers_mut().insert(header::RETRY_AFTER, hv);
        }
    }
    resp
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ApiKind {
    Chat,
    Messages,
    Tokens,
    Responses,
}

impl ApiKind {
    fn label(self) -> &'static str {
        match self {
            ApiKind::Chat => "chat",
            ApiKind::Messages => "messages",
            ApiKind::Tokens => "tokens",
            ApiKind::Responses => "responses",
        }
    }

    fn error(self, status: u16, message: &str, code: &str, retry_after: Option<u64>) -> Response {
        let mut resp = match self {
            ApiKind::Messages | ApiKind::Tokens => anthropic::error_response(status, message),
            ApiKind::Responses => responses::error_response(status, message, code),
            ApiKind::Chat => openai_error(status, message, code, retry_after),
        };
        if let Some(secs) = retry_after {
            if let Ok(hv) = header::HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, hv);
            }
        }
        resp
    }
}

fn openai_from_chat(
    output: ChatOutput,
    on_done: impl FnOnce(Result<NormalizedUsage, ChatFailure>) + Send + 'static,
) -> Response {
    match output {
        ChatOutput::Completed(res) => {
            let mut msg = json!({
                "role": "assistant",
                "content": if res.text.trim().is_empty() { Value::Null } else { json!(res.text) },
            });
            if !res.reasoning.is_empty() {
                msg["reasoning_content"] = json!(res.reasoning);
            }
            if !res.tool_calls.is_empty() {
                msg["tool_calls"] = Value::Array(
                    res.tool_calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": { "name": c.name, "arguments": c.arguments },
                            })
                        })
                        .collect(),
                );
            }
            let finish = if res.tool_calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            };
            let usage_val = res.usage.openai_json();
            on_done(Ok(res.usage));
            axum::Json(json!({
                "id": res.id,
                "object": "chat.completion",
                "created": res.created,
                "model": res.model,
                "choices": [{ "index": 0, "message": msg, "finish_reason": finish }],
                "usage": usage_val,
            }))
            .into_response()
        }
        ChatOutput::Stream(mut started) => {
            let (tx, rx) = mpsc::unbounded_channel::<Result<Bytes, std::io::Error>>();
            tokio::spawn(async move {
                let send_json = |val: &Value| {
                    let s = format!("data: {}\n\n", serde_json::to_string(val).unwrap_or_default());
                    let _ = tx.send(Ok(Bytes::from(s)));
                };
                send_json(&json!({
                    "id": started.id,
                    "object": "chat.completion.chunk",
                    "created": started.created,
                    "model": started.model,
                    "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": Value::Null }]
                }));

                let mut tool_idx = 0usize;
                let outcome = loop {
                    let Some(ev) = started.rx.recv().await else {
                        break Err(ChatFailure::incomplete());
                    };
                    match ev {
                        TurnEvent::Reasoning(r) => {
                            send_json(&json!({
                                "id": started.id,
                                "object": "chat.completion.chunk",
                                "created": started.created,
                                "model": started.model,
                                "choices": [{ "index": 0, "delta": { "reasoning_content": r }, "finish_reason": Value::Null }]
                            }));
                        }
                        TurnEvent::Text(t) => {
                            send_json(&json!({
                                "id": started.id,
                                "object": "chat.completion.chunk",
                                "created": started.created,
                                "model": started.model,
                                "choices": [{ "index": 0, "delta": { "content": t }, "finish_reason": Value::Null }]
                            }));
                        }
                        TurnEvent::Tool(c) => {
                            let idx = tool_idx;
                            tool_idx += 1;
                            send_json(&json!({
                                "id": started.id,
                                "object": "chat.completion.chunk",
                                "created": started.created,
                                "model": started.model,
                                "choices": [{
                                    "index": 0,
                                    "delta": {
                                        "tool_calls": [{
                                            "index": idx,
                                            "id": c.id,
                                            "type": "function",
                                            "function": { "name": c.name, "arguments": c.arguments }
                                        }]
                                    },
                                    "finish_reason": Value::Null
                                }]
                            }));
                        }
                        TurnEvent::Error(failure) => {
                            send_json(&json!({
                                "error": { "message": failure.message, "type": failure.code, "code": failure.code }
                            }));
                            break Err(failure);
                        }
                        TurnEvent::Done(u) => {
                            let finish = if tool_idx > 0 { "tool_calls" } else { "stop" };
                            send_json(&json!({
                                "id": started.id,
                                "object": "chat.completion.chunk",
                                "created": started.created,
                                "model": started.model,
                                "choices": [{ "index": 0, "delta": {}, "finish_reason": finish }]
                            }));
                            if started.include_usage {
                                send_json(&json!({
                                    "id": started.id,
                                    "object": "chat.completion.chunk",
                                    "created": started.created,
                                    "model": started.model,
                                    "choices": [],
                                    "usage": u.openai_json(),
                                }));
                            }
                            break Ok(u);
                        }
                    }
                };
                let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n")));
                on_done(outcome);
            });

            let stream = UnboundedReceiverStream::new(rx).boxed();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
                .header(header::CACHE_CONTROL, "no-cache")
                .header("x-accel-buffering", "no")
                .body(axum::body::Body::from_stream(stream))
                .unwrap()
        }
    }
}

async fn handle_models(pool: &Arc<AccountPool>) -> Response {
    let cfg = pool.settings();
    let mut listed = account_models(None);
    if let Some(acc) = pool.pick_account("", &HashSet::new()) {
        if let Ok(fresh) = pool.ensure_fresh(&acc.key(), false).await {
            if let Ok(settings_val) = qbs::account_settings(&fresh, &cfg).await {
                listed = account_models(settings_val.as_ref());
            }
        }
    }
    let created = Utc::now().timestamp();
    let created_iso = Utc::now().to_rfc3339();
    let data: Vec<Value> = listed
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "object": "model",
                "created": created,
                "owned_by": "amazon-quick",
                "type": "model",
                "display_name": model_display_name(id),
                "created_at": created_iso,
            })
        })
        .collect();
    models_response(data)
}

fn models_response(data: Vec<Value>) -> Response {
    let first_id = data.first().and_then(|model| model.get("id"));
    let last_id = data.last().and_then(|model| model.get("id"));
    axum::Json(json!({
        "object": "list",
        "data": data,
        "has_more": false,
        "first_id": first_id,
        "last_id": last_id,
    }))
    .into_response()
}

// A disconnected client drops either the handler future or its response body.
// The request worker stays alive long enough to stop generation upstream.
struct RequestCancellation(watch::Sender<bool>);

impl Drop for RequestCancellation {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

impl RequestCancellation {
    fn attach(self, response: Response) -> Response {
        let (parts, body) = response.into_parts();
        let stream = body.into_data_stream().map(move |chunk| {
            let _cancellation = &self;
            chunk
        });
        Response::from_parts(parts, Body::from_stream(stream))
    }
}

fn account_failed(pool: &Arc<AccountPool>, auth: &AccountRecord, failure: &ChatFailure) {
    let key = auth.key();
    if failure.status == 401 && failure.sign_in_expired {
        pool.mark_expired(&key, &failure.message);
    } else if failure.status == 429 {
        let marked = pool.mark_cooldown(&key, failure.retry_after_secs, &failure.message);
        if let Some(until) = marked.filter(|_| failure.confirmed_quota()) {
            pool.refine_quota_cooldown(auth.clone(), until);
        }
    }
}

fn failure_level(status: u16) -> LogLevel {
    match status {
        429 => LogLevel::Warn,
        499 => LogLevel::Info,
        _ => LogLevel::Error,
    }
}

async fn execute_with_pool(
    pool: Arc<AccountPool>,
    api: ApiKind,
    headers: HeaderMap,
    raw_body: Bytes,
    desktop: bool,
) -> Response {
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let cancellation = RequestCancellation(cancel_tx);
    let response = tokio::spawn(execute_request(
        pool, api, headers, raw_body, desktop, cancel_rx,
    ))
    .await
    .unwrap_or_else(|_| {
        let failure = ChatFailure::incomplete();
        api.error(failure.status, &failure.message, &failure.code, None)
    });
    if response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes().starts_with(b"text/event-stream"))
    {
        cancellation.attach(response)
    } else {
        response
    }
}

/// 多账号故障切换执行循环。
async fn execute_request(
    pool: Arc<AccountPool>,
    api: ApiKind,
    headers: HeaderMap,
    raw_body: Bytes,
    desktop: bool,
    cancel_rx: watch::Receiver<bool>,
) -> Response {
    let started_at = Instant::now();
    let body: Value = match serde_json::from_slice::<Value>(&raw_body) {
        Ok(v) if v.is_object() => v,
        _ => return api.error(400, "the request body is not a JSON object", "invalid_request_error", None),
    };

    let desktop_mode = if desktop {
        let requested = body["model"].as_str().unwrap_or("").trim();
        if requested.is_empty() {
            return api.error(
                400,
                "a Claude Desktop model is required",
                "invalid_request_error",
                None,
            );
        }
        let Some(mode) = resolve_quick_model(requested) else {
            return api.error(
                404,
                "unknown Claude Desktop model. See /claude-desktop/v1/models",
                "model_not_found",
                None,
            );
        };
        Some(mode)
    } else {
        None
    };

    if api == ApiKind::Tokens {
        return anthropic::count_tokens(&body);
    }
    if api == ApiKind::Responses
        && body
            .get("previous_response_id")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    {
        return api.error(
            400,
            "previous_response_id is not supported: responses are not stored; send the conversation in input",
            "invalid_request_error",
            None,
        );
    }

    let cfg = pool.settings();
    let (mut chat_body, mut conversation, anthropic_req, responses_req) = match api {
        ApiKind::Messages => {
            let req = anthropic::to_chat(&body);
            let c = req.chat.clone();
            let conv = req.conversation.clone();
            (c, conv, Some(req), None)
        }
        ApiKind::Responses => {
            let req = responses::to_chat(&body, &headers);
            let c = req.chat.clone();
            let conv = req.conversation.clone();
            (c, conv, None, Some(req))
        }
        _ => (body, String::new(), None, None),
    };

    if conversation.is_empty() {
        conversation = conversation_of(&headers);
    }
    let raw_model = chat_body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mapped_model = desktop_mode
        .map(str::to_string)
        .unwrap_or_else(|| quick_model(raw_model, &cfg.default_model));
    chat_body["model"] = json!(mapped_model);

    let turn = match prepare(&chat_body, &cfg) {
        Ok(mut turn) => {
            // Desktop 的后续回合也可能收到没有可调用工具的标记。
            turn.bridge |= desktop;
            Arc::new(turn)
        }
        Err(e) => return api.error(400, &e.to_string(), "invalid_request_error", None),
    };
    let history = cfg.stateful.then(|| {
        Arc::new(ThreadHistory::new(
            &conversation,
            &turn.model,
            &turn.system,
            &turn.convo,
        ))
    });
    let preferred_account = history.as_ref().and_then(|history| {
        pool.threads
            .lock()
            .preferred_account(history)
            .map(str::to_owned)
    });
    let run_attempt = |auth| {
        run_turn_for_account(
            turn.clone(),
            auth,
            cfg.clone(),
            history.clone(),
            pool.threads.clone(),
            cancel_rx.clone(),
        )
    };

    let mut tried: HashSet<String> = HashSet::new();
    let mut last_failure: Option<ChatFailure> = None;

    for _attempt in 0..8 {
        if *cancel_rx.borrow() {
            return api.error(
                499,
                "The client cancelled the request",
                "request_cancelled",
                None,
            );
        }
        let Some(picked) =
            pool.pick_account_for_turn(&conversation, &tried, preferred_account.as_deref())
        else {
            break;
        };
        let acc_key = picked.key();
        let acc_label = picked.display_label();
        tried.insert(acc_key.clone());

        let mut auth = match pool.ensure_fresh(&acc_key, false).await {
            Ok(a) => a,
            Err(e) => {
                last_failure = Some(ChatFailure {
                    status: if e.is_expired() { 401 } else { 502 },
                    code: if e.is_expired() {
                        "upstream_session_unavailable".into()
                    } else {
                        "upstream_error".into()
                    },
                    message: e.to_string(),
                    sign_in_expired: e.is_expired(),
                    retry_after_secs: None,
                    reported_usage: None,
                });
                continue;
            }
        };

        let active_request = pool.track_request(&acc_key);
        let mut res = run_attempt(auth.clone()).await;

        // 若返回 401 且判定令牌过期，先对当前账号强制刷新重试一次，失败后再切换其他账号。
        if res
            .as_ref()
            .is_err_and(|f| f.status == 401 && f.sign_in_expired)
            && !*cancel_rx.borrow()
            && let Ok(renewed) = pool.ensure_fresh(&acc_key, true).await
        {
            auth = renewed;
            res = run_attempt(auth.clone()).await;
        }

        match res {
            Ok(output) => {
                pool.bind_session(&conversation, &acc_key);
                let pool_done = pool.clone();
                let acc_label_done = acc_label.clone();
                let req_model_done = mapped_model.clone();
                let api_label = api.label().to_string();

                let on_done = move |outcome: Result<NormalizedUsage, ChatFailure>| {
                    let latency = started_at.elapsed().as_millis() as u64;
                    let (status, level, usage, message) = match outcome {
                        Ok(usage) => {
                            let msg = if usage.resolved_model_id.is_empty() {
                                format!("{api_label} {req_model_done} ok ({latency}ms)")
                            } else {
                                format!(
                                    "{api_label} {req_model_done} -> {} ({latency}ms)",
                                    usage.resolved_model_id
                                )
                            };
                            (200, LogLevel::Info, usage, msg)
                        }
                        Err(failure) => {
                            account_failed(&pool_done, &auth, &failure);
                            let msg = format!("HTTP {}: {}", failure.status, failure.message);
                            (
                                failure.status,
                                failure_level(failure.status),
                                failure.reported_usage.unwrap_or_default(),
                                msg,
                            )
                        }
                    };
                    let resolved = usage.resolved_model_id;
                    active_request.finish(&resolved);
                    pool_done.push_gateway_log(
                        &acc_key,
                        RequestLogEntry {
                            timestamp: Utc::now(),
                            level,
                            api: api_label,
                            requested_model: req_model_done,
                            resolved_model: resolved,
                            account: acc_label_done,
                            status,
                            latency_ms: latency,
                            input_tokens: usage.prompt_tokens,
                            output_tokens: usage.completion_tokens,
                            cached_tokens: usage.cached_tokens,
                            context: usage.context,
                            message,
                        },
                    );
                };

                return match api {
                    ApiKind::Messages => {
                        anthropic::from_chat_output(output, anthropic_req.as_ref().unwrap(), on_done)
                    }
                    ApiKind::Responses => {
                        responses::from_chat_output(output, responses_req.unwrap(), on_done)
                    }
                    _ => openai_from_chat(output, on_done),
                };
            }
            Err(mut f) => {
                drop(active_request);
                let latency = started_at.elapsed().as_millis() as u64;
                account_failed(&pool, &auth, &f);
                let usage = f.reported_usage.take().unwrap_or_default();
                pool.push_gateway_log(
                    &acc_key,
                    RequestLogEntry {
                        timestamp: Utc::now(),
                        level: failure_level(f.status),
                        api: api.label().to_string(),
                        requested_model: mapped_model.clone(),
                        resolved_model: usage.resolved_model_id,
                        account: acc_label,
                        status: f.status,
                        latency_ms: latency,
                        input_tokens: usage.prompt_tokens,
                        output_tokens: usage.completion_tokens,
                        cached_tokens: usage.cached_tokens,
                        context: usage.context,
                        message: format!("HTTP {}: {}", f.status, f.message),
                    },
                );

                let should_failover = f.code != "invalid_tool_call"
                    && matches!(f.status, 401 | 403 | 429 | 502 | 504);
                last_failure = Some(f);
                if !should_failover {
                    break;
                }
            }
        }
    }

    if let Some(f) = last_failure {
        let retry = if f.status == 429 {
            pool.next_cooldown_retry().or(f.retry_after_secs)
        } else {
            f.retry_after_secs
        };
        api.error(f.status, &f.message, &f.code, retry)
    } else if let Some(retry) = pool.next_cooldown_retry() {
        api.error(
            429,
            "Amazon Quick accounts are cooling down",
            "rate_limit_exceeded",
            Some(retry),
        )
    } else {
        api.error(401, SIGN_IN_MSG, "authentication_error", None)
    }
}

async fn dispatch(
    State(pool): State<Arc<AccountPool>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let desktop_path = uri
        .path()
        .strip_prefix(claude_desktop::BASE_PATH)
        .filter(|rest| rest.is_empty() || rest.starts_with('/'));
    let desktop = desktop_path.is_some();
    let path = normalize_route(desktop_path.unwrap_or(uri.path()));
    let api_hint = if desktop || path.starts_with("/messages") {
        ApiKind::Messages
    } else if path == "/responses" {
        ApiKind::Responses
    } else {
        ApiKind::Chat
    };

    if !desktop && method == Method::GET && (path == "/" || path == "/health") {
        return axum::Json(json!({
            "ok": true,
            "name": "aws-quick-bridge-rs",
            "version": env!("CARGO_PKG_VERSION"),
        }))
        .into_response();
    }

    let api_key = pool.config().api_key;
    if !same_key(&extract_key(&headers), &api_key) {
        return api_hint.error(
            401,
            "the API key is wrong; check the GUI or run `aws-quick-bridge-rs status`",
            "invalid_api_key",
            None,
        );
    }

    match (method.clone(), path.as_str()) {
        (Method::GET, "/models") if desktop => {
            models_response(claude_desktop::list_models(pool.settings().default_model == "auto"))
        }
        (Method::GET, "/models") => handle_models(&pool).await,
        (Method::POST, "/chat/completions") if !desktop => {
            execute_with_pool(pool, ApiKind::Chat, headers, body, false).await
        }
        (Method::POST, "/messages") => {
            execute_with_pool(pool, ApiKind::Messages, headers, body, desktop).await
        }
        (Method::POST, "/messages/count_tokens") => {
            execute_with_pool(pool, ApiKind::Tokens, headers, body, desktop).await
        }
        (Method::POST, "/responses") if !desktop => {
            execute_with_pool(pool, ApiKind::Responses, headers, body, false).await
        }
        _ => api_hint.error(
            404,
            &format!("no such route: {method} {}", uri.path()),
            "not_found",
            None,
        ),
    }
}

pub fn build_router(pool: Arc<AccountPool>) -> Router {
    Router::new()
        .fallback(dispatch)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(pool)
}

pub struct RunningServer {
    pub addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl RunningServer {
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }

    pub async fn stop_and_wait(mut self) {
        self.stop();
        let _ = (&mut self.task).await;
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.stop();
    }
}

pub async fn start_server(
    pool: Arc<AccountPool>,
    host: &str,
    port: u16,
) -> Result<RunningServer> {
    let listener = TcpListener::bind((host, port)).await?;
    let addr = listener.local_addr()?;
    let (tx, rx) = oneshot::channel::<()>();
    let app = build_router(pool.clone());
    pool.log_info(format!("Gateway listening on http://{addr}"));
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    Ok(RunningServer {
        addr,
        shutdown_tx: Some(tx),
        task,
    })
}
