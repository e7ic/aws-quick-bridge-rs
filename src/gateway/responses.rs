//! OpenAI Responses API 适配层（`POST /v1/responses`，供 Codex 使用）。

use crate::chat::{ChatFailure, ChatOutput, NormalizedUsage, TurnEvent};
use crate::toolbridge::ToolCallReply;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use uuid::Uuid;

fn make_id(prefix: &str) -> String {
    format!("{prefix}{}", Uuid::new_v4().simple())
}

pub fn error_response(status: u16, message: &str, code: &str) -> Response {
    let err_type = if status == 401 {
        "authentication_error"
    } else if status < 500 {
        "invalid_request_error"
    } else {
        "server_error"
    };
    let http_status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    (
        http_status,
        axum::Json(json!({
            "error": {
                "message": message,
                "type": err_type,
                "code": if code.is_empty() { Value::Null } else { json!(code) }
            }
        })),
    )
        .into_response()
}

fn is_text_type(t: &str) -> bool {
    matches!(
        t,
        "input_text" | "output_text" | "text" | "summary_text" | "reasoning_text"
    )
}

fn convert_content(value: Option<&Value>) -> Value {
    let Some(value) = value else {
        return json!("");
    };
    if let Some(s) = value.as_str() {
        return json!(s);
    }
    let Some(arr) = value.as_array() else {
        return json!("");
    };
    let mut parts = Vec::new();
    for p in arr {
        let typ = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if is_text_type(typ) {
            if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                parts.push(json!({ "type": "text", "text": t }));
            }
        } else if typ == "input_image" {
            let url = match p.get("image_url") {
                Some(Value::String(s)) => Some(s.as_str()),
                Some(Value::Object(o)) => o.get("url").and_then(|v| v.as_str()),
                _ => None,
            };
            if let Some(u) = url.filter(|s| !s.is_empty()) {
                parts.push(json!({ "type": "image_url", "image_url": { "url": u } }));
            } else {
                parts.push(json!({ "type": "text", "text": "[image]" }));
            }
        } else if typ == "input_file" {
            let fname = p.get("filename").and_then(|v| v.as_str()).unwrap_or("");
            let label = if fname.is_empty() {
                "[file]".to_string()
            } else {
                format!("[file {fname}]")
            };
            parts.push(json!({ "type": "text", "text": label }));
        }
    }
    if parts
        .iter()
        .all(|p| p.get("type").and_then(|v| v.as_str()) == Some("text"))
    {
        let joined = parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        json!(joined)
    } else {
        Value::Array(parts)
    }
}

fn flat_name(ns: Option<&str>, name: &str) -> String {
    match ns.filter(|s| !s.is_empty()) {
        Some(n) if n.ends_with("__") => format!("{n}{name}"),
        Some(n) => format!("{n}__{name}"),
        None => name.to_string(),
    }
}

fn output_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let joined: Vec<&str> = arr
                .iter()
                .filter(|p| is_text_type(p.get("type").and_then(|v| v.as_str()).unwrap_or("")))
                .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
                .collect();
            if !joined.is_empty() {
                joined.join("\n")
            } else {
                serde_json::to_string(arr).unwrap_or_default()
            }
        }
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn convert_messages(input: Option<&Value>) -> Vec<Value> {
    let Some(input) = input else {
        return Vec::new();
    };
    if let Some(s) = input.as_str() {
        return if s.is_empty() {
            Vec::new()
        } else {
            vec![json!({ "role": "user", "content": s })]
        };
    }
    let Some(items) = input.as_array() else {
        return Vec::new();
    };
    let mut out: Vec<Value> = Vec::new();

    let push_call = |out: &mut Vec<Value>, call_id: &str, name: String, args: String| {
        let entry = json!({
            "id": call_id,
            "type": "function",
            "function": { "name": name, "arguments": args }
        });
        if out.last().and_then(|m| m.get("role")).and_then(|v| v.as_str()) == Some("assistant") {
            let last = out.last_mut().unwrap();
            if let Some(arr) = last.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
                arr.push(entry);
            } else {
                last["tool_calls"] = json!([entry]);
            }
        } else {
            out.push(json!({ "role": "assistant", "content": Value::Null, "tool_calls": [entry] }));
        }
    };

    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let typ = obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or(if obj.contains_key("role") { "message" } else { "" });
        match typ {
            "message" => {
                let raw_role = obj.get("role").and_then(|v| v.as_str()).unwrap_or("");
                let role = if raw_role == "developer" { "system" } else { raw_role };
                if !matches!(role, "user" | "assistant" | "system") {
                    continue;
                }
                let body = convert_content(obj.get("content"));
                let non_empty = match &body {
                    Value::String(s) => !s.is_empty(),
                    Value::Array(a) => !a.is_empty(),
                    _ => false,
                };
                if non_empty {
                    out.push(json!({ "role": role, "content": body }));
                }
            }
            "function_call" => {
                let cid = obj
                    .get("call_id")
                    .or_else(|| obj.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let ns = obj.get("namespace").and_then(|v| v.as_str());
                let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = match obj.get("arguments") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".into()),
                    None => "{}".to_string(),
                };
                push_call(&mut out, cid, flat_name(ns, name), args);
            }
            "custom_tool_call" => {
                let cid = obj
                    .get("call_id")
                    .or_else(|| obj.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let args = serde_json::to_string(&json!({
                    "input": obj.get("input").cloned().unwrap_or(json!(""))
                }))
                .unwrap_or_else(|_| "{}".into());
                push_call(&mut out, cid, name, args);
            }
            "local_shell_call" => {
                let cid = obj
                    .get("call_id")
                    .or_else(|| obj.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let args = serde_json::to_string(obj.get("action").unwrap_or(&json!({})))
                    .unwrap_or_else(|_| "{}".into());
                push_call(&mut out, cid, "local_shell".to_string(), args);
            }
            "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": obj.get("call_id").and_then(|v| v.as_str()).unwrap_or(""),
                    "content": output_text(obj.get("output")),
                }));
            }
            _ => {}
        }
    }
    out
}

pub struct ResponsesRequest {
    pub chat: Value,
    pub custom: HashSet<String>,
    pub spaces: HashMap<String, (String, String)>,
    pub model: String,
    pub conversation: String,
}

pub fn to_chat(body: &Value, headers: &HeaderMap) -> ResponsesRequest {
    let mut chat_tools = Vec::new();
    let mut custom = HashSet::new();
    let mut spaces = HashMap::new();

    if let Some(list) = body.get("tools").and_then(|v| v.as_array()) {
        for t in list {
            let typ = t.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match typ {
                "function" => {
                    if let Some(name) = t.get("name").and_then(|v| v.as_str()) {
                        chat_tools.push(json!({
                            "type": "function",
                            "function": {
                                "name": name,
                                "description": t.get("description").cloned().unwrap_or(json!("")),
                                "parameters": t.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                            }
                        }));
                    }
                }
                "namespace" => {
                    if let Some(ns) = t.get("name").and_then(|v| v.as_str()) {
                        if let Some(inners) = t.get("tools").and_then(|v| v.as_array()) {
                            for inner in inners {
                                if inner.get("type").and_then(|v| v.as_str()) != Some("function") {
                                    continue;
                                }
                                if let Some(iname) = inner.get("name").and_then(|v| v.as_str()) {
                                    let flat = flat_name(Some(ns), iname);
                                    spaces.insert(flat.clone(), (ns.to_string(), iname.to_string()));
                                    chat_tools.push(json!({
                                        "type": "function",
                                        "function": {
                                            "name": flat,
                                            "description": inner.get("description").cloned().unwrap_or(json!("")),
                                            "parameters": inner.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                                        }
                                    }));
                                }
                            }
                        }
                    }
                }
                "custom" => {
                    if let Some(name) = t.get("name").and_then(|v| v.as_str()) {
                        custom.insert(name.to_string());
                        let mut desc = t
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if let Some(def) = t
                            .get("format")
                            .and_then(|f| f.get("definition"))
                            .and_then(|v| v.as_str())
                        {
                            let syntax = t
                                .get("format")
                                .and_then(|f| f.get("syntax"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let clipped: String = def.chars().take(3000).collect();
                            desc.push_str(&format!(
                                "\n\nThe input follows this {syntax} grammar:\n{clipped}"
                            ));
                        }
                        chat_tools.push(json!({
                            "type": "function",
                            "function": {
                                "name": name,
                                "description": desc,
                                "parameters": {
                                    "type": "object",
                                    "properties": { "input": { "type": "string", "description": "The tool's whole input, as plain text" } },
                                    "required": ["input"]
                                }
                            }
                        }));
                    }
                }
                "local_shell" => {
                    chat_tools.push(json!({
                        "type": "function",
                        "function": {
                            "name": "local_shell",
                            "description": "Runs a command on the user's computer and returns its output.",
                            "parameters": {
                                "type": "object",
                                "properties": {
                                    "command": { "type": "array", "items": { "type": "string" } },
                                    "workdir": { "type": "string" },
                                    "timeout_ms": { "type": "integer" }
                                },
                                "required": ["command"]
                            }
                        }
                    }));
                }
                _ => {}
            }
        }
    }

    let mut msgs = Vec::new();
    if let Some(inst) = body.get("instructions").and_then(|v| v.as_str()) {
        if !inst.trim().is_empty() {
            msgs.push(json!({ "role": "system", "content": inst }));
        }
    }
    msgs.extend(convert_messages(body.get("input")));

    let mut chat = json!({
        "model": body.get("model").cloned().unwrap_or(json!("")),
        "messages": msgs,
        "stream": body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false),
        "stream_options": { "include_usage": true },
    });
    if !chat_tools.is_empty() {
        chat["tools"] = Value::Array(chat_tools);
    }
    if let Some(tc) = body.get("tool_choice") {
        match tc {
            Value::String(s) if s == "none" || s == "required" => chat["tool_choice"] = json!(s),
            Value::Object(o) => {
                if let Some(name) = o.get("name").and_then(|v| v.as_str()) {
                    chat["tool_choice"] = json!({ "type": "function", "function": { "name": name } });
                }
            }
            _ => {}
        }
    }
    if body.get("parallel_tool_calls").and_then(|v| v.as_bool()) == Some(false) {
        chat["parallel_tool_calls"] = json!(false);
    }
    if let Some(eff) = body
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(|v| v.as_str())
    {
        chat["reasoning_effort"] = json!(eff);
    }

    let conversation = headers
        .get("session-id")
        .or_else(|| headers.get("session_id"))
        .or_else(|| headers.get("conversation_id"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            body.get("prompt_cache_key")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();

    ResponsesRequest {
        chat,
        custom,
        spaces,
        model: body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        conversation,
    }
}

fn usage_json(u: &NormalizedUsage) -> Value {
    let inp = u.input_non_cached() + u.cached_tokens;
    let total = if u.total_tokens > 0 {
        u.total_tokens
    } else {
        inp + u.completion_tokens
    };
    json!({
        "input_tokens": inp,
        "input_tokens_details": { "cached_tokens": u.cached_tokens },
        "output_tokens": u.completion_tokens,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": total,
    })
}

fn call_item(
    call: &ToolCallReply,
    custom: &HashSet<String>,
    spaces: &HashMap<String, (String, String)>,
) -> Value {
    if custom.contains(&call.name) {
        let input = match serde_json::from_str::<Value>(&call.arguments) {
            Ok(Value::Object(m)) => m
                .get("input")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| call.arguments.clone()),
            Ok(Value::String(s)) => s,
            _ => call.arguments.clone(),
        };
        return json!({
            "id": make_id("ctc_"),
            "type": "custom_tool_call",
            "status": "completed",
            "call_id": call.id,
            "name": call.name,
            "input": input,
        });
    }
    if call.name == "local_shell" {
        let mut action = serde_json::from_str::<serde_json::Map<String, Value>>(&call.arguments)
            .unwrap_or_default();
        action.insert("type".to_string(), json!("exec"));
        return json!({
            "id": make_id("lsh_"),
            "type": "local_shell_call",
            "status": "completed",
            "call_id": call.id,
            "action": Value::Object(action),
        });
    }
    let mut obj = json!({
        "id": make_id("fc_"),
        "type": "function_call",
        "status": "completed",
        "call_id": call.id,
        "arguments": call.arguments,
    });
    if let Some((ns, name)) = spaces.get(&call.name) {
        obj["namespace"] = json!(ns);
        obj["name"] = json!(name);
    } else {
        obj["name"] = json!(call.name);
    }
    obj
}

fn reasoning_item(id: String, text: &str) -> Value {
    json!({
        "id": id,
        "type": "reasoning",
        "summary": [{ "type": "summary_text", "text": text }],
    })
}

fn message_item(id: String, text: &str) -> Value {
    json!({
        "id": id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": text, "annotations": [] }],
    })
}

pub fn from_chat_output(
    output: ChatOutput,
    req: ResponsesRequest,
    on_done: impl FnOnce(Result<NormalizedUsage, ChatFailure>) + Send + 'static,
) -> Response {
    let base = json!({
        "id": make_id("resp_"),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "model": req.model,
        "error": Value::Null,
        "incomplete_details": Value::Null,
        "parallel_tool_calls": req.chat.get("parallel_tool_calls").and_then(|v| v.as_bool()).unwrap_or(true),
        "tool_choice": "auto",
        "store": false,
    });

    match output {
        ChatOutput::Completed(res) => {
            let mut items = Vec::new();
            if !res.reasoning.is_empty() {
                items.push(reasoning_item(make_id("rs_"), &res.reasoning));
            }
            if !res.text.is_empty() {
                items.push(message_item(make_id("msg_"), &res.text));
            }
            for c in &res.tool_calls {
                items.push(call_item(c, &req.custom, &req.spaces));
            }
            let mut resp = base;
            resp["status"] = json!("completed");
            resp["output"] = Value::Array(items);
            resp["output_text"] = json!(res.text);
            resp["usage"] = usage_json(&res.usage);
            on_done(Ok(res.usage));
            axum::Json(resp).into_response()
        }
        ChatOutput::Stream(mut started) => {
            let (tx, rx) = mpsc::unbounded_channel::<Result<Bytes, std::io::Error>>();
            tokio::spawn(async move {
                let mut seq: u64 = 0;
                let mut send = |ev_type: &str, mut data: Value| {
                    if let Some(obj) = data.as_object_mut() {
                        obj.insert("type".to_string(), json!(ev_type));
                        obj.insert("sequence_number".to_string(), json!(seq));
                    }
                    seq += 1;
                    let frame = format!(
                        "event: {ev_type}\ndata: {}\n\n",
                        serde_json::to_string(&data).unwrap_or_default()
                    );
                    let _ = tx.send(Ok(Bytes::from(frame)));
                };

                let mut items: Vec<Value> = Vec::new();
                let mut in_prog = base.clone();
                in_prog["status"] = json!("in_progress");
                in_prog["output"] = json!([]);
                in_prog["usage"] = Value::Null;
                send("response.created", json!({ "response": in_prog }));
                send("response.in_progress", json!({ "response": in_prog }));

                struct OpenItem {
                    kind: &'static str,
                    id: String,
                    text: String,
                }
                let mut open: Option<OpenItem> = None;

                let close_item = |open: &mut Option<OpenItem>, items: &mut Vec<Value>, send: &mut dyn FnMut(&str, Value)| {
                    let Some(cur) = open.take() else { return };
                    let output_index = items.len();
                    let finished = if cur.kind == "reasoning" {
                        send(
                            "response.reasoning_summary_text.done",
                            json!({ "item_id": cur.id, "output_index": output_index, "summary_index": 0, "text": cur.text }),
                        );
                        send(
                            "response.reasoning_summary_part.done",
                            json!({ "item_id": cur.id, "output_index": output_index, "summary_index": 0, "part": { "type": "summary_text", "text": cur.text } }),
                        );
                        reasoning_item(cur.id, &cur.text)
                    } else {
                        send(
                            "response.output_text.done",
                            json!({ "item_id": cur.id, "output_index": output_index, "content_index": 0, "text": cur.text, "logprobs": [] }),
                        );
                        send(
                            "response.content_part.done",
                            json!({ "item_id": cur.id, "output_index": output_index, "content_index": 0, "part": { "type": "output_text", "text": cur.text, "annotations": [] } }),
                        );
                        message_item(cur.id, &cur.text)
                    };
                    send(
                        "response.output_item.done",
                        json!({ "output_index": output_index, "item": finished }),
                    );
                    items.push(finished);
                };

                let outcome = loop {
                    let Some(ev) = started.rx.recv().await else {
                        break Err(ChatFailure::incomplete());
                    };
                    match ev {
                        TurnEvent::Reasoning(t) => {
                            let kind = "reasoning";
                            if open.as_ref().map(|o| o.kind) != Some(kind) {
                                close_item(&mut open, &mut items, &mut send);
                                let output_index = items.len();
                                let item_id = make_id("rs_");
                                send(
                                    "response.output_item.added",
                                    json!({ "output_index": output_index, "item": { "id": item_id, "type": "reasoning", "summary": [] } }),
                                );
                                send(
                                    "response.reasoning_summary_part.added",
                                    json!({ "item_id": item_id, "output_index": output_index, "summary_index": 0, "part": { "type": "summary_text", "text": "" } }),
                                );
                                open = Some(OpenItem {
                                    kind,
                                    id: item_id,
                                    text: String::new(),
                                });
                            }
                            if let Some(cur) = open.as_mut() {
                                cur.text.push_str(&t);
                                let output_index = items.len();
                                send(
                                    "response.reasoning_summary_text.delta",
                                    json!({ "item_id": cur.id, "output_index": output_index, "summary_index": 0, "delta": t }),
                                );
                            }
                        }
                        TurnEvent::Text(t) => {
                            let kind = "message";
                            if open.as_ref().map(|o| o.kind) != Some(kind) {
                                close_item(&mut open, &mut items, &mut send);
                                let output_index = items.len();
                                let item_id = make_id("msg_");
                                send(
                                    "response.output_item.added",
                                    json!({ "output_index": output_index, "item": { "id": item_id, "type": "message", "status": "in_progress", "role": "assistant", "content": [] } }),
                                );
                                send(
                                    "response.content_part.added",
                                    json!({ "item_id": item_id, "output_index": output_index, "content_index": 0, "part": { "type": "output_text", "text": "", "annotations": [] } }),
                                );
                                open = Some(OpenItem {
                                    kind,
                                    id: item_id,
                                    text: String::new(),
                                });
                            }
                            if let Some(cur) = open.as_mut() {
                                cur.text.push_str(&t);
                                let output_index = items.len();
                                send(
                                    "response.output_text.delta",
                                    json!({ "item_id": cur.id, "output_index": output_index, "content_index": 0, "delta": t, "logprobs": [] }),
                                );
                            }
                        }
                        TurnEvent::Tool(c) => {
                            close_item(&mut open, &mut items, &mut send);
                            let output_index = items.len();
                            let item = call_item(&c, &req.custom, &req.spaces);
                            let itype = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            let mut pending = item.clone();
                            pending["status"] = json!("in_progress");
                            if itype == "function_call" {
                                pending["arguments"] = json!("");
                            } else if itype == "custom_tool_call" {
                                pending["input"] = json!("");
                            }
                            send(
                                "response.output_item.added",
                                json!({ "output_index": output_index, "item": pending }),
                            );
                            if itype == "function_call" {
                                send(
                                    "response.function_call_arguments.delta",
                                    json!({ "item_id": item["id"], "output_index": output_index, "delta": item["arguments"] }),
                                );
                                send(
                                    "response.function_call_arguments.done",
                                    json!({ "item_id": item["id"], "output_index": output_index, "arguments": item["arguments"] }),
                                );
                            } else if itype == "custom_tool_call" {
                                send(
                                    "response.custom_tool_call_input.delta",
                                    json!({ "item_id": item["id"], "output_index": output_index, "delta": item["input"] }),
                                );
                                send(
                                    "response.custom_tool_call_input.done",
                                    json!({ "item_id": item["id"], "output_index": output_index, "input": item["input"] }),
                                );
                            }
                            send(
                                "response.output_item.done",
                                json!({ "output_index": output_index, "item": item }),
                            );
                            items.push(item);
                        }
                        TurnEvent::Error(failure) => break Err(failure),
                        TurnEvent::Done(u) => {
                            close_item(&mut open, &mut items, &mut send);
                            let mut done_resp = base.clone();
                            done_resp["status"] = json!("completed");
                            done_resp["output"] = Value::Array(items.clone());
                            done_resp["usage"] = usage_json(&u);
                            send("response.completed", json!({ "response": done_resp }));
                            break Ok(u);
                        }
                    }
                };
                if let Err(failure) = &outcome {
                    close_item(&mut open, &mut items, &mut send);
                    let mut failed = base;
                    failed["status"] = json!("failed");
                    failed["output"] = Value::Array(items);
                    failed["usage"] = Value::Null;
                    let code = if failure.status == 429 {
                        "rate_limit_exceeded"
                    } else {
                        "server_error"
                    };
                    failed["error"] = json!({ "code": code, "message": failure.message });
                    send("response.failed", json!({ "response": failed }));
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
