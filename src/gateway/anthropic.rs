//! Anthropic Messages API 适配层（`POST /v1/messages` 与 `/v1/messages/count_tokens`，供 Claude Code 使用）。

use crate::chat::{ChatFailure, ChatOutput, NormalizedUsage, TurnEvent};
use crate::messages::agent_neutral;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use uuid::Uuid;

fn error_type(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

pub fn error_response(status: u16, message: &str) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    (
        code,
        axum::Json(json!({
            "type": "error",
            "error": { "type": error_type(status), "message": message }
        })),
    )
        .into_response()
}

fn text_of(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn system_from_anthropic(value: Option<&Value>) -> String {
    let raw = match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => text_of(arr),
        _ => String::new(),
    };
    agent_neutral(&raw)
}

fn image_part(block: &Value) -> Option<Value> {
    let s = block.get("source")?.as_object()?;
    let typ = s.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if typ == "base64" {
        let data = s.get("data").and_then(|v| v.as_str())?;
        if !data.is_empty() {
            let media = s
                .get("media_type")
                .and_then(|v| v.as_str())
                .filter(|m| !m.is_empty())
                .unwrap_or("image/png");
            return Some(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{media};base64,{data}") }
            }));
        }
    } else if typ == "url" {
        let url = s.get("url").and_then(|v| v.as_str())?;
        if !url.is_empty() {
            return Some(json!({ "type": "image_url", "image_url": { "url": url } }));
        }
    }
    None
}

fn result_text(block: &Value) -> String {
    let mut parts = Vec::new();
    match block.get("content") {
        Some(Value::String(s)) => parts.push(s.clone()),
        Some(Value::Array(arr)) => {
            for p in arr {
                match p.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "text" => {
                        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                            parts.push(t.to_string());
                        }
                    }
                    "image" => parts.push("[image]".to_string()),
                    "tool_reference" => {
                        if let Some(n) = p.get("tool_name").and_then(|v| v.as_str()) {
                            parts.push(format!("Tool loaded: {n}"));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    let text = parts.join("\n");
    let is_err = block
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_err && !text.to_ascii_lowercase().starts_with("error") {
        format!("Error: {text}").trim().to_string()
    } else {
        text
    }
}

fn convert_messages(list: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in list {
        let blocks: Vec<Value> = match m.get("content") {
            Some(Value::String(s)) => vec![json!({ "type": "text", "text": s })],
            Some(Value::Array(arr)) => arr.clone(),
            _ => Vec::new(),
        };
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role == "assistant" {
            let text = text_of(&blocks);
            let calls: Vec<Value> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
                .map(|b| {
                    let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                    json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".into())
                        }
                    })
                })
                .collect();
            if text.is_empty() && calls.is_empty() {
                continue;
            }
            let mut msg = json!({
                "role": "assistant",
                "content": if text.is_empty() { Value::Null } else { json!(text) },
            });
            if !calls.is_empty() {
                msg["tool_calls"] = json!(calls);
            }
            out.push(msg);
            continue;
        }
        if role == "system" {
            let t = text_of(&blocks).trim().to_string();
            if !t.is_empty() {
                out.push(json!({ "role": "system", "content": t }));
            }
            continue;
        }
        for b in &blocks {
            if b.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or(""),
                    "content": result_text(b),
                }));
            }
        }
        let mut parts = Vec::new();
        for b in &blocks {
            match b.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "text" => {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                        parts.push(json!({ "type": "text", "text": t }));
                    }
                }
                "image" => {
                    parts.push(image_part(b).unwrap_or_else(|| json!({ "type": "text", "text": "[image]" })));
                }
                "document" => {
                    let title = b.get("title").and_then(|v| v.as_str()).unwrap_or("");
                    let label = if title.is_empty() {
                        "[document]".to_string()
                    } else {
                        format!("[document {title}]")
                    };
                    parts.push(json!({ "type": "text", "text": label }));
                }
                _ => {}
            }
        }
        if !parts.is_empty() {
            let all_text = parts
                .iter()
                .all(|p| p.get("type").and_then(|v| v.as_str()) == Some("text"));
            let content = if all_text {
                json!(
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            } else {
                Value::Array(parts)
            };
            out.push(json!({ "role": "user", "content": content }));
        }
    }
    out
}

fn effort_from_body(body: &Value) -> Option<String> {
    if let Some(asked) = body
        .get("output_config")
        .and_then(|o| o.get("effort"))
        .and_then(|v| v.as_str())
    {
        let low = asked.to_ascii_lowercase();
        if !low.is_empty() {
            return Some(if low == "max" || low == "xhigh" {
                "high".to_string()
            } else {
                low
            });
        }
    }
    let thinking = body.get("thinking")?;
    if thinking.get("type").and_then(|v| v.as_str()) != Some("enabled") {
        return None;
    }
    let budget = thinking.get("budget_tokens").and_then(|v| v.as_u64())?;
    if budget == 0 {
        return None;
    }
    Some(if budget < 4_000 {
        "low"
    } else if budget < 16_000 {
        "medium"
    } else {
        "high"
    }
    .to_string())
}

pub struct AnthropicRequest {
    pub chat: Value,
    pub conversation: String,
    pub model: String,
    pub thinking: bool,
}

pub fn to_chat(body: &Value) -> AnthropicRequest {
    let sys = system_from_anthropic(body.get("system"));
    let mut msgs = Vec::new();
    if !sys.is_empty() {
        msgs.push(json!({ "role": "system", "content": sys }));
    }
    if let Some(arr) = body.get("messages").and_then(|v| v.as_array()) {
        msgs.extend(convert_messages(arr));
    }

    let mut chat = json!({
        "model": body.get("model").cloned().unwrap_or(json!("")),
        "messages": msgs,
        "stream": body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false),
        "stream_options": { "include_usage": true },
    });

    if let Some(tools_arr) = body.get("tools").and_then(|v| v.as_array()) {
        let converted: Vec<Value> = tools_arr
            .iter()
            .filter(|t| {
                t.get("name").and_then(|v| v.as_str()).is_some()
                    && t.get("input_schema").is_some()
                    && t.get("defer_loading").and_then(|v| v.as_bool()) != Some(true)
            })
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t["name"],
                        "description": t.get("description").cloned().unwrap_or(json!("")),
                        "parameters": t["input_schema"],
                    }
                })
            })
            .collect();
        if !converted.is_empty() {
            chat["tools"] = Value::Array(converted);
        }
    }

    if let Some(tc) = body.get("tool_choice") {
        match tc.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "any" => chat["tool_choice"] = json!("required"),
            "none" => chat["tool_choice"] = json!("none"),
            "tool" => {
                if let Some(name) = tc.get("name").and_then(|v| v.as_str()) {
                    chat["tool_choice"] = json!({ "type": "function", "function": { "name": name } });
                }
            }
            _ => {}
        }
        if tc
            .get("disable_parallel_tool_use")
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            chat["parallel_tool_calls"] = json!(false);
        }
    }

    if let Some(eff) = effort_from_body(body) {
        chat["reasoning_effort"] = json!(eff);
    }

    let conversation = body
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let thinking_type = body
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let thinking_display = body
        .get("thinking")
        .and_then(|t| t.get("display"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let thinking = matches!(thinking_type, "enabled" | "adaptive") && thinking_display != "omitted";

    AnthropicRequest {
        chat,
        conversation,
        model: body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        thinking,
    }
}

fn usage_json(u: &NormalizedUsage) -> Value {
    json!({
        "input_tokens": u.input_non_cached(),
        "output_tokens": u.completion_tokens,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": u.cached_tokens,
    })
}

fn parsed_object(args: &str) -> Value {
    serde_json::from_str::<Value>(args)
        .ok()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}))
}

pub fn count_tokens(body: &Value) -> Response {
    let sample = json!([
        body.get("system").cloned().unwrap_or(json!("")),
        body.get("messages").cloned().unwrap_or(json!([])),
        body.get("tools").cloned().unwrap_or(json!([])),
    ]);
    let chars = serde_json::to_string(&sample).map(|s| s.len()).unwrap_or(0);
    let est = ((chars + 3) / 4).max(1);
    axum::Json(json!({ "input_tokens": est })).into_response()
}

pub fn from_chat_output(
    output: ChatOutput,
    req: &AnthropicRequest,
    on_done: impl FnOnce(Result<NormalizedUsage, ChatFailure>) + Send + 'static,
) -> Response {
    let msg_id = format!("msg_{}", Uuid::new_v4().simple());
    let model = req.model.clone();
    let show_thinking = req.thinking;

    match output {
        ChatOutput::Completed(res) => {
            let mut content = Vec::new();
            if show_thinking && !res.reasoning.is_empty() {
                content.push(json!({ "type": "thinking", "thinking": res.reasoning, "signature": "" }));
            }
            if !res.text.is_empty() {
                content.push(json!({ "type": "text", "text": res.text }));
            }
            for c in &res.tool_calls {
                content.push(json!({
                    "type": "tool_use",
                    "id": c.id,
                    "name": c.name,
                    "input": parsed_object(&c.arguments),
                }));
            }
            let stop_reason = if res.tool_calls.is_empty() {
                "end_turn"
            } else {
                "tool_use"
            };
            let usage = usage_json(&res.usage);
            on_done(Ok(res.usage));
            axum::Json(json!({
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": content,
                "stop_reason": stop_reason,
                "stop_sequence": Value::Null,
                "usage": usage,
            }))
            .into_response()
        }
        ChatOutput::Stream(mut started) => {
            let (tx, rx) = mpsc::unbounded_channel::<Result<Bytes, std::io::Error>>();
            tokio::spawn(async move {
                let send = |event: &str, data: &Value| {
                    let frame = format!("event: {event}\ndata: {}\n\n", serde_json::to_string(data).unwrap_or_default());
                    let _ = tx.send(Ok(Bytes::from(frame)));
                };

                send(
                    "message_start",
                    &json!({
                        "type": "message_start",
                        "message": {
                            "id": msg_id,
                            "type": "message",
                            "role": "assistant",
                            "model": model,
                            "content": [],
                            "stop_reason": Value::Null,
                            "stop_sequence": Value::Null,
                            "usage": { "input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0 }
                        }
                    }),
                );
                send("ping", &json!({ "type": "ping" }));

                let mut index: i32 = -1;
                let mut open: &'static str = "";
                let mut calls = 0usize;

                let close_block = |open: &mut &'static str, index: i32| {
                    if open.is_empty() {
                        return;
                    }
                    if *open == "thinking" {
                        send(
                            "content_block_delta",
                            &json!({ "type": "content_block_delta", "index": index, "delta": { "type": "signature_delta", "signature": "" } }),
                        );
                    }
                    send(
                        "content_block_stop",
                        &json!({ "type": "content_block_stop", "index": index }),
                    );
                    *open = "";
                };

                let open_block = |open: &mut &'static str, index: &mut i32, kind: &'static str, content_block: Value| {
                    if *open == kind && kind != "tool_use" {
                        return;
                    }
                    close_block(open, *index);
                    *open = kind;
                    *index += 1;
                    send(
                        "content_block_start",
                        &json!({ "type": "content_block_start", "index": *index, "content_block": content_block }),
                    );
                };

                let outcome = loop {
                    let Some(ev) = started.rx.recv().await else {
                        break Err(ChatFailure::incomplete());
                    };
                    match ev {
                        TurnEvent::Reasoning(r) => {
                            if !show_thinking {
                                continue;
                            }
                            open_block(&mut open, &mut index, "thinking", json!({ "type": "thinking", "thinking": "", "signature": "" }));
                            send(
                                "content_block_delta",
                                &json!({ "type": "content_block_delta", "index": index, "delta": { "type": "thinking_delta", "thinking": r } }),
                            );
                        }
                        TurnEvent::Text(t) => {
                            open_block(&mut open, &mut index, "text", json!({ "type": "text", "text": "" }));
                            send(
                                "content_block_delta",
                                &json!({ "type": "content_block_delta", "index": index, "delta": { "type": "text_delta", "text": t } }),
                            );
                        }
                        TurnEvent::Tool(c) => {
                            calls += 1;
                            open_block(
                                &mut open,
                                &mut index,
                                "tool_use",
                                json!({ "type": "tool_use", "id": c.id, "name": c.name, "input": {} }),
                            );
                            let partial = serde_json::to_string(&parsed_object(&c.arguments))
                                .unwrap_or_else(|_| "{}".into());
                            send(
                                "content_block_delta",
                                &json!({ "type": "content_block_delta", "index": index, "delta": { "type": "input_json_delta", "partial_json": partial } }),
                            );
                        }
                        TurnEvent::Error(failure) => break Err(failure),
                        TurnEvent::Done(u) => {
                            close_block(&mut open, index);
                            let stop_reason = if calls > 0 { "tool_use" } else { "end_turn" };
                            send(
                                "message_delta",
                                &json!({
                                    "type": "message_delta",
                                    "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
                                    "usage": usage_json(&u),
                                }),
                            );
                            send("message_stop", &json!({ "type": "message_stop" }));
                            break Ok(u);
                        }
                    }
                };
                if let Err(failure) = &outcome {
                    close_block(&mut open, index);
                    send(
                        "error",
                        &json!({ "type": "error", "error": { "type": error_type(failure.status), "message": failure.message } }),
                    );
                }
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
