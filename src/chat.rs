//! 对话补全核心引擎：消息准备、64 KB 帧预算压缩与附件转存、线程复用、工具桥接流式转发及错误映射。

use crate::config::{AccountRecord, Settings, map_model};
use crate::messages::{
    InlineImage, is_system_message, language_note, last_user_text, prompt_blocks, system_text,
    title_for, upstream_prompt_override, uuid_v4,
};
use crate::qbs::{self, SpentAllowance};
use crate::threads::{ThreadHistory, ThreadMatch, ThreadStore, digest};
use crate::toolbridge::{
    BridgeStream, TOOL_DETAILS, TOOL_REMINDER, ToolCallReply, ToolDetail, build_tool_bridge_prompt,
    parse_client_tool_calls, parse_tool_choice, requested_tool_names,
};
use crate::upstream::{
    ContextUsage, TurnInput, UpstreamError, UpstreamErrorKind, UpstreamEvent, UpstreamSession,
    UpstreamUsage, chat, frame_bytes, user_message, wait_for_cancellation,
};
use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

const REST_MAX_SECS: u64 = 6 * 3600;

pub fn thinking_effort(body: &Value, fallback: &str) -> String {
    let asked = body
        .get("reasoning_effort")
        .or_else(|| body.get("reasoning").and_then(|r| r.get("effort")))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if asked.is_empty() {
        return fallback.to_string();
    }
    match asked.as_str() {
        "none" | "off" => "off".to_string(),
        "minimal" | "low" => "low".to_string(),
        "medium" => "medium".to_string(),
        _ => "high".to_string(),
    }
}

#[derive(Debug, Clone, Default)]
pub struct NormalizedUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cached_tokens: u64,
    pub resolved_model_id: String,
    pub context: Option<ContextUsage>,
}

impl NormalizedUsage {
    pub fn from_upstream(u: &UpstreamUsage) -> Self {
        let completion = u.completion;
        let input = u.prompt.saturating_add(u.cache_read).saturating_add(u.cache_write);
        let total = if u.total > 0 {
            u.total
        } else {
            input.saturating_add(completion)
        };
        let prompt = if total >= completion {
            total - completion
        } else {
            input
        };
        Self {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total,
            cached_tokens: u.cache_read,
            resolved_model_id: u.resolved_model_id.clone(),
            context: u.context,
        }
    }

    pub fn input_non_cached(&self) -> u64 {
        self.prompt_tokens.saturating_sub(self.cached_tokens)
    }

    pub fn openai_json(&self) -> Value {
        let mut out = json!({
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.total_tokens,
        });
        if self.cached_tokens > 0 {
            out["prompt_tokens_details"] = json!({ "cached_tokens": self.cached_tokens });
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct ChatFailure {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub sign_in_expired: bool,
    pub retry_after_secs: Option<u64>,
    pub(crate) reported_usage: Option<NormalizedUsage>,
}

impl ChatFailure {
    pub(crate) fn incomplete() -> Self {
        Self {
            status: 502,
            code: "upstream_error".into(),
            message: "Amazon Quick ended the stream before completing the reply".into(),
            sign_in_expired: false,
            retry_after_secs: None,
            reported_usage: None,
        }
    }

    pub(crate) fn confirmed_quota(&self) -> bool {
        self.code == "insufficient_quota"
    }
}

pub fn is_refused(err: &UpstreamError) -> bool {
    err.kind == UpstreamErrorKind::Upstream
        || (err.kind == UpstreamErrorKind::Handshake && err.status == 429)
}

fn format_span(secs: u64) -> String {
    if secs >= 5400 {
        format!("{} hours", (secs as f64 / 3600.0).round() as u64)
    } else {
        format!("{} minutes", ((secs as f64 / 60.0).round() as u64).max(1))
    }
}

pub fn failure_from_error(err: &UpstreamError, spent: Option<&SpentAllowance>) -> ChatFailure {
    let reported_usage = err
        .reported_usage
        .as_deref()
        .map(NormalizedUsage::from_upstream);
    if let Some(sp) = spent {
        if is_refused(err) {
            let back = if sp.secs > 0 {
                format!("; it is back in {}", format_span(sp.secs))
            } else {
                String::new()
            };
            return ChatFailure {
                status: 429,
                code: "rate_limit_exceeded".to_string(),
                message: format!(
                    "Amazon Quick's {} allowance is used up{back} ({})",
                    sp.name, err.message
                ),
                sign_in_expired: false,
                retry_after_secs: if sp.secs > 0 {
                    Some(sp.secs.min(REST_MAX_SECS))
                } else {
                    None
                },
                reported_usage,
            };
        }
    }
    static THROTTLED_RE: OnceLock<Regex> = OnceLock::new();
    let throttled = THROTTLED_RE.get_or_init(|| {
        Regex::new(r"(?i)thrott|rate.?limit|too many requests|quota|limit exceeded|capacity")
            .unwrap()
    });

    match err.kind {
        UpstreamErrorKind::Quota => ChatFailure {
            status: 429,
            code: "insufficient_quota".to_string(),
            message: err.message.clone(),
            sign_in_expired: false,
            retry_after_secs: Some(300),
            reported_usage,
        },
        UpstreamErrorKind::Cancelled => ChatFailure {
            status: 499,
            code: "request_cancelled".to_string(),
            message: err.message.clone(),
            sign_in_expired: false,
            retry_after_secs: None,
            reported_usage,
        },
        UpstreamErrorKind::Handshake if err.status == 401 => ChatFailure {
            status: 401,
            code: "upstream_session_unavailable".to_string(),
            message: format!("Amazon Quick refused the sign-in; sign in again ({})", err.message),
            sign_in_expired: true,
            retry_after_secs: None,
            reported_usage,
        },
        UpstreamErrorKind::Handshake if err.status == 403 => ChatFailure {
            status: 403,
            code: "upstream_forbidden".to_string(),
            message: err.message.clone(),
            sign_in_expired: false,
            retry_after_secs: None,
            reported_usage,
        },
        UpstreamErrorKind::Handshake if err.status == 429 => ChatFailure {
            status: 429,
            code: "rate_limit_exceeded".to_string(),
            message: err.message.clone(),
            sign_in_expired: false,
            retry_after_secs: Some(60),
            reported_usage,
        },
        UpstreamErrorKind::Timeout => ChatFailure {
            status: 504,
            code: "upstream_timeout".to_string(),
            message: err.message.clone(),
            sign_in_expired: false,
            retry_after_secs: None,
            reported_usage,
        },
        UpstreamErrorKind::TooLarge => ChatFailure {
            status: 400,
            code: "context_length_exceeded".to_string(),
            message: format!(
                "the request does not fit Amazon Quick's 64 KB message limit ({})",
                err.message
            ),
            sign_in_expired: false,
            retry_after_secs: None,
            reported_usage,
        },
        UpstreamErrorKind::Blocked => ChatFailure {
            status: 400,
            code: "content_filter".to_string(),
            message: err.message.clone(),
            sign_in_expired: false,
            retry_after_secs: None,
            reported_usage,
        },
        UpstreamErrorKind::Upstream if throttled.is_match(&err.message) => ChatFailure {
            status: 429,
            code: "rate_limit_exceeded".to_string(),
            message: err.message.clone(),
            sign_in_expired: false,
            retry_after_secs: Some(60),
            reported_usage,
        },
        _ => ChatFailure {
            status: 502,
            code: "upstream_error".to_string(),
            message: err.message.clone(),
            sign_in_expired: false,
            retry_after_secs: None,
            reported_usage,
        },
    }
}

pub struct PreparedTurn {
    pub id: String,
    pub created: i64,
    pub model: String,
    pub parallel: bool,
    pub bridge: bool,
    pub bridge_allowed: HashSet<String>,
    pub stream: bool,
    pub include_usage: bool,
    pub convo: Vec<Value>,
    pub system: String,
    pub tool_prompts: HashMap<ToolDetail, String>,
    pub input: TurnInput,
}

impl PreparedTurn {
    pub fn tools_prompt(&self, detail: ToolDetail) -> &str {
        self.tool_prompts
            .get(&detail)
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    pub fn custom_prompt(&self, system: &str, detail: ToolDetail) -> String {
        let mut parts = Vec::new();
        if !system.is_empty() {
            parts.push(system.to_string());
        }
        parts.push(upstream_prompt_override());
        let tp = self.tools_prompt(detail);
        if !tp.is_empty() {
            parts.push(tp.to_string());
        }
        parts.join("\n\n")
    }
}

pub fn prepare(body: &Value, cfg: &Settings) -> Result<PreparedTurn> {
    let Some(obj) = body.as_object() else {
        bail!("the request body is not a JSON object");
    };
    if obj.get("n").and_then(|v| v.as_u64()).unwrap_or(1) > 1 {
        bail!("n > 1 is not supported");
    }
    let messages = obj
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let convo: Vec<Value> = messages
        .iter()
        .filter(|m| !is_system_message(m))
        .cloned()
        .collect();
    if convo.is_empty() {
        bail!("messages must not be empty");
    }
    let prompt = prompt_blocks(&messages, None, None)?.join("\n\n");
    let mut allowed = requested_tool_names(body);
    let choice = parse_tool_choice(obj.get("tool_choice").or_else(|| obj.get("function_call")));
    if choice.mode == "function" && !choice.name.is_empty() {
        allowed.insert(choice.name.clone());
    }
    let parallel = obj
        .get("parallel_tool_calls")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let language = language_note(&convo);
    let tools_slice = obj.get("tools").and_then(|v| v.as_array()).map(|v| v.as_slice());
    let funcs_slice = obj
        .get("functions")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice());

    let mut tool_prompts = HashMap::new();
    for &detail in TOOL_DETAILS {
        let base = build_tool_bridge_prompt(
            tools_slice,
            funcs_slice,
            &choice.mode,
            &choice.name,
            parallel,
            detail,
        );
        let full = if !base.is_empty() && !language.is_empty() {
            format!("{base}\n\n{language}")
        } else {
            base
        };
        tool_prompts.insert(detail, full);
    }

    let bridge = !tool_prompts
        .get(&ToolDetail::Full)
        .map(|s| s.is_empty())
        .unwrap_or(true);
    let bridge_allowed = if choice.mode == "function" && !choice.name.is_empty() {
        [choice.name.clone()].into_iter().collect()
    } else {
        allowed
    };
    let system = system_text(&messages);
    let model = map_model(
        obj.get("model").and_then(|v| v.as_str()).unwrap_or(""),
        &cfg.default_model,
    );
    let custom_full = {
        let mut parts = Vec::new();
        if !system.is_empty() {
            parts.push(system.clone());
        }
        parts.push(upstream_prompt_override());
        let tp = tool_prompts
            .get(&ToolDetail::Full)
            .map(|s| s.as_str())
            .unwrap_or("");
        if !tp.is_empty() {
            parts.push(tp.to_string());
        }
        parts.join("\n\n")
    };
    let web_search = obj
        .get("web_search_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(cfg.web_search);

    Ok(PreparedTurn {
        id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
        created: chrono::Utc::now().timestamp(),
        model: model.clone(),
        parallel,
        bridge,
        bridge_allowed,
        stream: obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false),
        include_usage: obj
            .get("stream_options")
            .and_then(|s| s.get("include_usage"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        convo,
        system,
        tool_prompts,
        input: TurnInput {
            prompt,
            model,
            thread_id: String::new(),
            title: title_for(&last_user_text(&messages)),
            system_prompt: custom_full,
            reminder: if bridge {
                TOOL_REMINDER.to_string()
            } else {
                String::new()
            },
            thinking_effort: thinking_effort(body, &cfg.thinking_effort),
            web_search,
            resume: false,
            attachments: Vec::new(),
        },
    })
}

fn middle_cut(text: &str, keep_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= keep_chars {
        return text.to_string();
    }
    let note = "\n…[shortened to fit Amazon Quick's 64 KB message limit]…\n";
    let note_len = note.chars().count();
    let half = keep_chars.saturating_sub(note_len) / 2;
    let start: String = chars[..half].iter().collect();
    let end: String = chars[chars.len() - half..].iter().collect();
    format!("{start}{note}{end}")
}

fn longest_fit<F: FnMut(&str) -> bool>(text: &str, mut fits: F) -> String {
    let total = text.chars().count();
    let mut low = 0usize;
    let mut high = total;
    while low < high {
        let mid = (low + high + 1) / 2;
        let cut = middle_cut(text, mid);
        if fits(&cut) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    middle_cut(text, low)
}

fn fit_system_prompt(
    turn: &PreparedTurn,
    input: &mut TurnInput,
    limit: usize,
    measure: &impl Fn(&TurnInput) -> usize,
) {
    if limit == 0 || input.system_prompt.is_empty() || measure(input) <= limit {
        return;
    }

    input.system_prompt.clear();
    let prompt = std::mem::take(&mut input.prompt);
    let fixed_bytes = measure(input);
    // 先扣除附件引用和固定字段，再给长对话留一半可用空间。
    let split = fixed_bytes + limit.saturating_sub(fixed_bytes) / 2;
    input.prompt = prompt;
    let prompt_frame = measure(input);
    let (deferred_prompt, room, bare) = if prompt_frame <= split {
        (None, limit, prompt_frame)
    } else {
        (Some(std::mem::take(&mut input.prompt)), split, fixed_bytes)
    };

    let mut chosen_detail = *TOOL_DETAILS.last().unwrap();
    for &detail in TOOL_DETAILS {
        input.system_prompt = turn.custom_prompt(&turn.system, detail);
        if measure(input) <= room {
            chosen_detail = detail;
            break;
        }
        input.system_prompt = turn.custom_prompt("", detail);
        if measure(input).saturating_sub(bare) <= room.saturating_sub(bare) / 2 {
            chosen_detail = detail;
            break;
        }
    }

    input.system_prompt = turn.custom_prompt(&turn.system, chosen_detail);
    if measure(input) > room {
        let cut = longest_fit(&turn.system, |cut| {
            input.system_prompt = turn.custom_prompt(cut, chosen_detail);
            measure(input) <= room
        });
        input.system_prompt = turn.custom_prompt(&cut, chosen_detail);
    }
    if let Some(prompt) = deferred_prompt {
        input.prompt = prompt;
    }
}

pub struct ComposedTurn {
    pub input: TurnInput,
    pub custom_digest: String,
}

async fn compose(
    turn: &PreparedTurn,
    auth: &AccountRecord,
    cfg: &Settings,
    session: &UpstreamSession,
    thread: Option<&ThreadMatch>,
    thread_id: &str,
) -> Result<ComposedTurn, UpstreamError> {
    let mut images: Vec<InlineImage> = Vec::new();
    let fresh_start = thread.map(|t| t.from).unwrap_or(0);
    let fresh = &turn.convo[fresh_start.min(turn.convo.len())..];
    let mut blocks = prompt_blocks(
        fresh,
        Some(&turn.convo),
        if cfg.attachments {
            Some(&mut images)
        } else {
            None
        },
    )
    .map_err(|e| UpstreamError::new(UpstreamErrorKind::Upstream, e.to_string(), 400))?;

    let mut input = turn.input.clone();
    input.thread_id = thread_id.to_string();
    input.resume = thread.is_some();
    input.prompt = blocks.join("\n\n");

    for img in images.into_iter().take(10) {
        if let Ok(bytes) = B64.decode(img.base64.trim()) {
            if let Ok(att) =
                qbs::upload_attachment(auth, cfg, &img.name, &img.mime_type, &bytes, thread_id)
                    .await
            {
                input.attachments.push(att);
            }
        }
    }

    let limit = cfg.max_frame_bytes;
    let measure = |inp: &TurnInput| frame_bytes(&user_message(session, cfg, inp));
    let fits_fn = |inp: &TurnInput| limit == 0 || measure(inp) <= limit;

    let asked = digest(&turn.input.system_prompt);
    if let Some(t) = thread {
        if t.custom == asked {
            input.system_prompt.clear();
        }
    }
    let instructed = !input.system_prompt.is_empty();

    fit_system_prompt(turn, &mut input, limit, &measure);

    if !fits_fn(&input) && cfg.attachments {
        let last = if blocks.len() > 1 && blocks.last().map(|s| s.len()).unwrap_or(0) <= 6000 {
            blocks.last().cloned().unwrap_or_default()
        } else {
            String::new()
        };
        let long_text = if !last.is_empty() {
            blocks[..blocks.len() - 1].join("\n\n")
        } else {
            blocks.join("\n\n")
        };
        let name = format!("context_{}.txt", &uuid_v4()[..8]);
        let bytes = long_text.as_bytes();
        if let Ok(att) =
            qbs::upload_attachment(auth, cfg, &name, "text/plain", bytes, thread_id).await
        {
            input.attachments.insert(0, att);
            let head = if !last.is_empty() {
                "The conversation up to the last message"
            } else {
                "This message"
            };
            let tail = if !last.is_empty() {
                format!("\n\n{last}")
            } else {
                String::new()
            };
            input.prompt = format!(
                "{head} is in the attached file {name} ({} bytes) because it is too long to send here. \
                Read the whole file with your file tool before anything else, then carry on from it exactly as if its text had been sent in this message. Do not mention the file or that you are reading it.{tail}",
                bytes.len()
            );
            blocks = vec![input.prompt.clone()];
            fit_system_prompt(turn, &mut input, limit, &measure);
        }
    }

    if !fits_fn(&input) {
        for i in 0..blocks.len().saturating_sub(1) {
            input.prompt = blocks.join("\n\n");
            if fits_fn(&input) {
                break;
            }
            blocks[i] = middle_cut(&blocks[i], 1500);
        }
        while blocks.len() > 1 {
            input.prompt = blocks.join("\n\n");
            if fits_fn(&input) {
                break;
            }
            blocks.remove(0);
        }
        input.prompt = blocks.join("\n\n");
        if !fits_fn(&input) && !blocks.is_empty() {
            let cut = longest_fit(&blocks[0], |c| {
                input.prompt = c.to_string();
                fits_fn(&input)
            });
            input.prompt = cut;
        }
    }

    if !fits_fn(&input) {
        return Err(UpstreamError::new(
            UpstreamErrorKind::TooLarge,
            format!("{} bytes after shortening, over {limit}", measure(&input)),
            400,
        ));
    }

    Ok(ComposedTurn {
        input,
        custom_digest: if instructed {
            asked
        } else {
            thread.map(|t| t.custom.clone()).unwrap_or_default()
        },
    })
}

/// 统一结构化对话事件，供 OpenAI / Anthropic / Responses 协议适配器消费。
#[derive(Debug, Clone)]
pub enum TurnEvent {
    Reasoning(String),
    Text(String),
    Tool(ToolCallReply),
    Done(NormalizedUsage),
    Error(ChatFailure),
}

#[derive(Debug, Clone)]
pub struct CompletedTurn {
    pub id: String,
    pub created: i64,
    pub model: String,
    pub reasoning: String,
    pub text: String,
    pub tool_calls: Vec<ToolCallReply>,
    pub usage: NormalizedUsage,
}

pub struct StartedStream {
    pub id: String,
    pub created: i64,
    pub model: String,
    pub include_usage: bool,
    pub rx: mpsc::UnboundedReceiver<TurnEvent>,
}

pub enum ChatOutput {
    Completed(CompletedTurn),
    Stream(StartedStream),
}

/// 使用选定账号执行单轮对话，支持 `threads` 线程复用、`0.7x` 帧超限重试与过期线程回退。
pub(crate) async fn run_turn_for_account(
    turn: Arc<PreparedTurn>,
    auth: AccountRecord,
    cfg: Settings,
    history: Option<Arc<ThreadHistory>>,
    threads: Arc<Mutex<ThreadStore>>,
    cancel_rx: watch::Receiver<bool>,
) -> Result<ChatOutput, ChatFailure> {
    let session = UpstreamSession::from_auth(&auth, &cfg);

    let account = digest(&auth.key());
    let matched_thread = history
        .as_ref()
        .and_then(|history| threads.lock().match_thread(&account, history));

    let attempt_run = async |thread: Option<ThreadMatch>| -> Result<ChatOutput, UpstreamError> {
        let thread_id = thread
            .as_ref()
            .map(|t| t.id.clone())
            .unwrap_or_else(uuid_v4);
        let mut prepare_cancel = cancel_rx.clone();
        let composed = tokio::select! {
            _ = wait_for_cancellation(&mut prepare_cancel) => Err(UpstreamError::cancelled()),
            composed = compose(&turn, &auth, &cfg, &session, thread.as_ref(), &thread_id) => composed,
        };
        let mut composed = match composed {
            Ok(c) => c,
            Err(e) => {
                if let Some(t) = &thread {
                    threads.lock().release(&t.id);
                }
                return Err(e);
            }
        };

        if !turn.stream {
            let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
            let mut res = chat(&session, &cfg, &composed.input, cancel_rx.clone(), Some(ev_tx)).await;
            // 触发 FrameTooLarge 时按 0.7 倍帧预算缩容重试一次。
            if let Err(ref e) = res {
                if e.kind == UpstreamErrorKind::TooLarge && cfg.max_frame_bytes > 0 {
                    let mut smaller = cfg.clone();
                    smaller.max_frame_bytes = (cfg.max_frame_bytes * 7) / 10;
                    if let Ok(recomposed) =
                        compose(&turn, &auth, &smaller, &session, thread.as_ref(), &thread_id).await
                    {
                        composed = recomposed;
                        let (ev_tx2, ev_rx2) = mpsc::unbounded_channel();
                        ev_rx = ev_rx2;
                        res = chat(
                            &session,
                            &smaller,
                            &composed.input,
                            cancel_rx.clone(),
                            Some(ev_tx2),
                        )
                        .await;
                    }
                }
            }

            let res = match res {
                Ok(r) => r,
                Err(e) => {
                    finish_failed_thread(&threads, thread, &e).await;
                    return Err(e);
                }
            };

            let mut reasoning = String::new();
            while let Ok(ev) = ev_rx.try_recv() {
                if let UpstreamEvent::Reasoning(r) = ev {
                    reasoning.push_str(&r);
                }
            }

            let (text, mut calls) = if turn.bridge {
                parse_client_tool_calls(&res.text, &turn.bridge_allowed)
            } else {
                (res.text.clone(), Vec::new())
            };
            if !turn.parallel && calls.len() > 1 {
                calls.truncate(1);
            }
            if text.trim().is_empty() && calls.is_empty() {
                let e = UpstreamError::new(
                    UpstreamErrorKind::Upstream,
                    "upstream returned no text or tool calls",
                    502,
                )
                .with_reported_usage(res.usage);
                finish_failed_thread(&threads, thread, &e).await;
                return Err(e);
            }
            if let Some(history) = &history {
                ThreadStore::remember(
                    threads.clone(),
                    history,
                    thread_id,
                    account.clone(),
                    composed.custom_digest,
                    &text,
                    &calls,
                )
                .await;
            }
            return Ok(ChatOutput::Completed(CompletedTurn {
                id: turn.id.clone(),
                created: turn.created,
                model: turn.model.clone(),
                reasoning,
                text,
                tool_calls: calls,
                usage: NormalizedUsage::from_upstream(&res.usage),
            }));
        }

        // 流式模式：等待首个事件或达到 `header_wait_ms` 后再确认 HTTP 200 响应头，以便前置错误可切换账号。
        let (out_tx, out_rx) = mpsc::unbounded_channel::<TurnEvent>();
        let (first_tx, first_rx) = tokio::sync::oneshot::channel::<Result<(), UpstreamError>>();

        let turn_id = turn.id.clone();
        let turn_created = turn.created;
        let turn_model = turn.model.clone();
        let include_usage = turn.include_usage;
        let parallel = turn.parallel;
        let bridge_enabled = turn.bridge;
        let bridge_allowed = turn.bridge_allowed.clone();
        let header_wait_ms = cfg.header_wait_ms;

        let sess_owned = session.clone();
        let cfg_owned = cfg.clone();
        let threads_owned = threads.clone();
        let account_owned = account.clone();
        let history_owned = history.clone();
        let cancel_owned = cancel_rx.clone();

        tokio::spawn(async move {
            let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<UpstreamEvent>();
            let mut first_tx = Some(first_tx);
            let out_tx_stream = out_tx.clone();

            let bridge_task = tokio::spawn(async move {
                let mut bridge = if bridge_enabled {
                    Some(BridgeStream::new(bridge_allowed))
                } else {
                    None
                };
                let mut reply = String::new();
                let mut signaled = false;

                let wait_fut = async {
                    if header_wait_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(header_wait_ms)).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                };
                tokio::pin!(wait_fut);

                loop {
                    tokio::select! {
                        _ = &mut wait_fut, if !signaled => {
                            signaled = true;
                            if let Some(tx) = first_tx.take() {
                                let _ = tx.send(Ok(()));
                            }
                        }
                        ev = ev_rx.recv() => {
                            let Some(ev) = ev else { break; };
                            match ev {
                                UpstreamEvent::Reasoning(r) => {
                                    if !r.is_empty() {
                                        if !signaled {
                                            signaled = true;
                                            if let Some(tx) = first_tx.take() { let _ = tx.send(Ok(())); }
                                        }
                                        let _ = out_tx_stream.send(TurnEvent::Reasoning(r));
                                    }
                                }
                                UpstreamEvent::Text(t) => {
                                    if t.is_empty() { continue; }
                                    if let Some(b) = bridge.as_mut() {
                                        if let Some(ready) = b.push(&t) {
                                            reply.push_str(&ready);
                                            if !signaled {
                                                signaled = true;
                                                if let Some(tx) = first_tx.take() { let _ = tx.send(Ok(())); }
                                            }
                                            let _ = out_tx_stream.send(TurnEvent::Text(ready));
                                        }
                                    } else {
                                        reply.push_str(&t);
                                        if !signaled {
                                            signaled = true;
                                            if let Some(tx) = first_tx.take() { let _ = tx.send(Ok(())); }
                                        }
                                        let _ = out_tx_stream.send(TurnEvent::Text(t));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                (bridge, reply, signaled, first_tx)
            });

            let res =
                chat(&sess_owned, &cfg_owned, &composed.input, cancel_owned.clone(), Some(ev_tx))
                    .await;

            let Ok((mut bridge, mut reply, signaled, mut first_tx)) = bridge_task.await else {
                if let Some(t) = &thread {
                    ThreadStore::discard(threads_owned, t.id.clone()).await;
                }
                return;
            };

            match res {
                Ok(up_res) => {
                    let mut calls = Vec::new();
                    if let Some(b) = bridge.as_mut() {
                        let (tail, c) = b.finish();
                        if let Some(t) = tail {
                            if !t.is_empty() {
                                reply.push_str(&t);
                                if let Some(tx) = first_tx.take() {
                                    let _ = tx.send(Ok(()));
                                }
                                let _ = out_tx.send(TurnEvent::Text(t));
                            }
                        }
                        calls = c;
                    }
                    if !parallel && calls.len() > 1 {
                        calls.truncate(1);
                    }
                    if reply.is_empty() && calls.is_empty() {
                        let err = UpstreamError::new(
                            UpstreamErrorKind::Upstream,
                            "upstream returned no text or tool calls",
                            502,
                        )
                        .with_reported_usage(up_res.usage);
                        finish_failed_thread(&threads_owned, thread, &err).await;
                        if !signaled {
                            if let Some(tx) = first_tx.take() {
                                let _ = tx.send(Err(err));
                            }
                        } else {
                            let _ = out_tx.send(TurnEvent::Error(failure_from_error(&err, None)));
                        }
                        return;
                    }
                    if let Some(tx) = first_tx.take() {
                        let _ = tx.send(Ok(()));
                    }
                    if let Some(history) = &history_owned {
                        ThreadStore::remember(
                            threads_owned,
                            history,
                            thread_id,
                            account_owned,
                            composed.custom_digest,
                            &reply,
                            &calls,
                        )
                        .await;
                    }
                    for call in calls {
                        let _ = out_tx.send(TurnEvent::Tool(call));
                    }
                    let _ = out_tx.send(TurnEvent::Done(NormalizedUsage::from_upstream(
                        &up_res.usage,
                    )));
                }
                Err(e) => {
                    finish_failed_thread(&threads_owned, thread, &e).await;
                    if !signaled {
                        if let Some(tx) = first_tx.take() {
                            let _ = tx.send(Err(e));
                        }
                    } else {
                        let _ = out_tx.send(TurnEvent::Error(failure_from_error(&e, None)));
                    }
                }
            }
        });

        match first_rx.await {
            Ok(Ok(())) => Ok(ChatOutput::Stream(StartedStream {
                id: turn_id,
                created: turn_created,
                model: turn_model,
                include_usage,
                rx: out_rx,
            })),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(UpstreamError::new(
                UpstreamErrorKind::Transport,
                "stream task closed prematurely",
                502,
            )),
        }
    };

    let first_attempt = attempt_run(matched_thread.clone()).await;
    let final_res = match first_attempt {
        Ok(out) => Ok(out),
        Err(e) => {
            if matched_thread.is_some()
                && !*cancel_rx.borrow()
                && matches!(
                    e.kind,
                    UpstreamErrorKind::Upstream | UpstreamErrorKind::Transport
                )
                && failure_from_error(&e, None).status != 429
            {
                attempt_run(None).await
            } else {
                Err(e)
            }
        }
    };

    match final_res {
        Ok(out) => Ok(out),
        Err(e) => {
            let spent = if is_refused(&e) {
                let mut cancel_rx = cancel_rx;
                tokio::select! {
                    _ = wait_for_cancellation(&mut cancel_rx) => {
                        return Err(failure_from_error(&UpstreamError::cancelled(), None));
                    }
                    spent = qbs::spent(&auth, &cfg) => spent,
                }
            } else {
                None
            };
            Err(failure_from_error(&e, spent.as_ref()))
        }
    }
}

async fn finish_failed_thread(
    threads: &Arc<Mutex<ThreadStore>>,
    thread: Option<ThreadMatch>,
    err: &UpstreamError,
) {
    let Some(thread) = thread else { return };
    if matches!(
        err.kind,
        UpstreamErrorKind::Cancelled
            | UpstreamErrorKind::Quota
            | UpstreamErrorKind::Handshake
            | UpstreamErrorKind::Blocked
            | UpstreamErrorKind::TooLarge
    ) {
        threads.lock().release(&thread.id);
    } else {
        ThreadStore::discard(threads.clone(), thread.id).await;
    }
}

#[cfg(test)]
mod tests;
