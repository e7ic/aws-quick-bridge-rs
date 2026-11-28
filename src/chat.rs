//! 对话补全核心引擎：消息准备、64 KB 帧预算压缩与附件转存、线程复用、工具桥接流式转发及错误映射。

use crate::config::{
    AccountRecord, Settings, advanced_supported, map_model, normalize_thinking_effort,
};
use crate::messages::{
    InlineImage, is_system_message, language_note, last_user_text, prompt_blocks, system_text,
    title_for, upstream_prompt_override, uuid_v4,
};
use crate::qbs::{self, SpentAllowance, UploadedAttachment};
use crate::threads::{ThreadHistory, ThreadMatch, ThreadPrompt, ThreadStore, ToolAnchor, digest};
use crate::toolbridge::{
    BridgeStream, TOOL_DETAILS, ToolCallError, ToolCallReply, ToolDetail, build_tool_bridge_prompt,
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
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

const REST_MAX_SECS: u64 = 6 * 3600;
const INLINE_ATTACHMENT_TAIL_BYTES: usize = 6_000;
const TOOL_REANCHOR_TURNS: u8 = 6;

pub fn thinking_effort(body: &Value, fallback: &str) -> String {
    body.get("reasoning_effort")
        .or_else(|| {
            body.get("reasoning")
                .and_then(|reasoning| reasoning.get("effort"))
        })
        .and_then(|v| v.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(|value| normalize_thinking_effort(value).unwrap_or("high"))
        .unwrap_or(fallback)
        .to_owned()
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
        let input = u
            .prompt
            .saturating_add(u.cache_read)
            .saturating_add(u.cache_write);
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
    pub(crate) reported_usage: Option<Box<NormalizedUsage>>,
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
    matches!(
        err.kind,
        UpstreamErrorKind::Upstream | UpstreamErrorKind::AdvancedEntitlementUnconfirmed
    ) || (err.kind == UpstreamErrorKind::Handshake && err.status == 429)
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
        .map(|usage| Box::new(NormalizedUsage::from_upstream(usage)));
    if let Some(sp) = spent
        && is_refused(err)
    {
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
            message: format!(
                "Amazon Quick refused the sign-in; sign in again ({})",
                err.message
            ),
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
        UpstreamErrorKind::Handshake if err.status == 504 => ChatFailure {
            status: 504,
            code: "upstream_connect_timeout".into(),
            message: err.message.clone(),
            sign_in_expired: false,
            retry_after_secs: None,
            reported_usage,
        },
        UpstreamErrorKind::AdvancedUnavailable => ChatFailure {
            status: 403,
            code: "advanced_unavailable".into(),
            message: err.message.clone(),
            sign_in_expired: false,
            retry_after_secs: None,
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
        UpstreamErrorKind::Attachment => ChatFailure {
            status: if (400..500).contains(&err.status) {
                err.status
            } else {
                502
            },
            code: "attachment_upload_failed".into(),
            message: err.message.clone(),
            sign_in_expired: err.status == 401,
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
        UpstreamErrorKind::InvalidToolCall => ChatFailure {
            status: 502,
            code: "invalid_tool_call".to_string(),
            message: err.message.clone(),
            sign_in_expired: false,
            retry_after_secs: None,
            reported_usage,
        },
        UpstreamErrorKind::ChunkLimit => ChatFailure {
            status: 502,
            code: "upstream_chunk_limit".into(),
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
    let mut bridge_allowed = requested_tool_names(body);
    let choice = parse_tool_choice(obj.get("tool_choice").or_else(|| obj.get("function_call")));
    match choice.mode.as_str() {
        "none" => bridge_allowed.clear(),
        "function" if !choice.name.is_empty() => {
            if !bridge_allowed.contains(&choice.name) {
                bail!("the selected tool must be available in the current request");
            }
            bridge_allowed.retain(|name| name == &choice.name);
        }
        _ => {}
    }
    let parallel = obj
        .get("parallel_tool_calls")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let language = language_note(&convo);
    let tools_slice = obj
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice());
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
        let mid = (low + high).div_ceil(2);
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
) -> ToolDetail {
    if limit == 0 || input.system_prompt.is_empty() || measure(input) <= limit {
        return ToolDetail::Full;
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
    chosen_detail
}

pub struct ComposedTurn {
    pub input: TurnInput,
    prompt_state: ThreadPrompt,
    context_attachment: Option<UploadedAttachment>,
}

struct ComposeContext<'a> {
    thread_id: &'a str,
    thread: Option<&'a ThreadMatch>,
    previous_attempt: Option<&'a ComposedTurn>,
}

impl<'a> ComposeContext<'a> {
    fn initial(thread_id: &'a str, thread: Option<&'a ThreadMatch>) -> Self {
        Self {
            thread_id,
            thread,
            previous_attempt: None,
        }
    }
}

async fn compose(
    turn: &PreparedTurn,
    model: &str,
    auth: &AccountRecord,
    cfg: &Settings,
    session: &UpstreamSession,
    ComposeContext {
        thread_id,
        thread,
        previous_attempt,
    }: ComposeContext<'_>,
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
    input.model = model.to_owned();
    input.thread_id = thread_id.to_string();
    input.resume = thread.is_some();
    input.prompt = blocks.join("\n\n");

    if let Some(previous) = previous_attempt {
        let first_image = usize::from(previous.context_attachment.is_some());
        input.attachments = previous.input.attachments[first_image..].to_vec();
    } else {
        for img in images.into_iter().take(10) {
            if let Ok(bytes) = B64.decode(img.base64.trim())
                && let Ok(att) =
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

    let custom = digest(&turn.input.system_prompt);
    let previous = thread
        .map(|thread| &thread.prompt)
        .filter(|prompt| prompt.custom == custom);
    let anchor = previous.and_then(|prompt| prompt.tool_anchor);
    let has_tools = !turn.tools_prompt(ToolDetail::Full).is_empty();
    let reanchor =
        has_tools && anchor.is_none_or(|anchor| anchor.reused_turns >= TOOL_REANCHOR_TURNS - 1);
    let can_upgrade = anchor.is_some_and(|anchor| !anchor.full) && fits_fn(&input);
    let send_prompt = previous.is_none() || reanchor || can_upgrade;
    if !send_prompt {
        input.system_prompt.clear();
    }

    let mut tool_detail = fit_system_prompt(turn, &mut input, limit, &measure);

    let previous_attachment =
        previous_attempt.and_then(|previous| previous.context_attachment.as_ref());
    let mut context_attachment = None;
    if cfg.attachments && (previous_attachment.is_some() || !fits_fn(&input)) {
        let last = if blocks.len() > 1
            && blocks
                .last()
                .is_some_and(|s| s.len() <= INLINE_ATTACHMENT_TAIL_BYTES)
        {
            blocks.last().cloned().unwrap_or_default()
        } else {
            String::new()
        };
        let long_text = if !last.is_empty() {
            blocks[..blocks.len() - 1].join("\n\n")
        } else {
            blocks.join("\n\n")
        };
        let bytes = long_text.as_bytes();
        let includes_tool_results = fresh
            .iter()
            .any(|message| matches!(message["role"].as_str(), Some("tool" | "function")));
        let cached = previous_attachment
            .or_else(|| thread.and_then(|t| t.attachment.as_ref()))
            .filter(|att| {
                att.size == bytes.len() && att.content_sha256 == hex::encode(Sha256::digest(bytes))
            });
        let attachment = match cached {
            Some(att) => Some(att.clone()),
            None => {
                let name = format!("context_{}.txt", &uuid_v4()[..8]);
                match qbs::upload_attachment(auth, cfg, &name, "text/plain", bytes, thread_id).await
                {
                    Ok(file) => Some(file),
                    Err(error) if includes_tool_results => {
                        return Err(UpstreamError::new(
                            UpstreamErrorKind::Attachment,
                            "Could not deliver the full tool result to Amazon Quick as an attachment",
                            error.status,
                        ));
                    }
                    Err(_) => None,
                }
            }
        };
        if let Some(att) = attachment {
            let name = att.name.clone();
            context_attachment = Some(att.clone());
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
            let instructions = if includes_tool_results {
                "Use the attachment as the original conversation and tool results. Consult the parts needed for the current task and continue. Read the full result if the task requires it."
            } else {
                "Read the whole file with your file tool before anything else, then carry on from it exactly as if its text had been sent in this message."
            };
            input.prompt = format!(
                "{head} is in the attached file {name} ({} bytes) because it is too long to send here. \
                {instructions} Do not mention the file or that you are reading it.{tail}",
                bytes.len()
            );
            blocks = vec![input.prompt.clone()];
            if send_prompt {
                input.system_prompt.clone_from(&turn.input.system_prompt);
                tool_detail = fit_system_prompt(turn, &mut input, limit, &measure);
            }
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
        context_attachment,
        prompt_state: ThreadPrompt {
            custom,
            tool_anchor: match (has_tools, send_prompt) {
                (false, _) => None,
                (true, true) => Some(ToolAnchor {
                    full: tool_detail == ToolDetail::Full,
                    reused_turns: 0,
                }),
                (true, false) => anchor.map(|mut anchor| {
                    anchor.reused_turns = anchor.reused_turns.saturating_add(1);
                    anchor
                }),
            },
        },
    })
}

fn smaller_frame_budget(cfg: &Settings) -> Option<Settings> {
    let max_frame_bytes = (cfg.max_frame_bytes as u128 * 7 / 10) as usize;
    (max_frame_bytes > 0).then(|| Settings {
        max_frame_bytes,
        ..cfg.clone()
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

/// 使用选定账号执行单轮对话，支持线程复用、帧超限缩容和失效线程重建。
pub(crate) async fn run_turn_for_account(
    turn: Arc<PreparedTurn>,
    model: &str,
    auth: AccountRecord,
    mut cfg: Settings,
    history: Option<Arc<ThreadHistory>>,
    threads: Arc<Mutex<ThreadStore>>,
    cancel_rx: watch::Receiver<bool>,
) -> Result<ChatOutput, ChatFailure> {
    cfg.extend_active_request_timeout &= turn.stream;
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
            composed = compose(
                &turn, model, &auth, &cfg, &session, ComposeContext::initial(&thread_id, thread.as_ref()),
            ) => composed,
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
            let mut res = chat(
                &session,
                &cfg,
                &composed.input,
                cancel_rx.clone(),
                Some(ev_tx),
            )
            .await;
            // 触发 FrameTooLarge 时按 0.7 倍帧预算缩容重试一次。
            if res
                .as_ref()
                .is_err_and(|e| e.kind == UpstreamErrorKind::TooLarge)
                && let Some(smaller) = smaller_frame_budget(&cfg)
            {
                let recomposed = tokio::select! {
                    biased;
                    _ = wait_for_cancellation(&mut prepare_cancel) => Err(UpstreamError::cancelled()),
                    result = compose(&turn, model, &auth, &smaller, &session, ComposeContext {
                        thread_id: &thread_id,
                        thread: thread.as_ref(),
                        previous_attempt: Some(&composed),
                    }) => result,
                };
                match recomposed {
                    Ok(recomposed) => {
                        composed = recomposed;
                        let (retry_tx, retry_rx) = mpsc::unbounded_channel();
                        ev_rx = retry_rx;
                        res = chat(
                            &session,
                            &smaller,
                            &composed.input,
                            cancel_rx.clone(),
                            Some(retry_tx),
                        )
                        .await;
                    }
                    Err(e) if e.kind == UpstreamErrorKind::Cancelled => res = Err(e),
                    Err(_) => {}
                }
            }

            let finish_failure = async |error: &UpstreamError| {
                finish_failed_thread(
                    &threads,
                    thread.as_ref(),
                    error,
                    history.as_deref(),
                    &account,
                    &composed,
                )
                .await;
            };
            let res = match res {
                Ok(r) => r,
                Err(e) => {
                    finish_failure(&e).await;
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
                match parse_client_tool_calls(&res.text, &turn.bridge_allowed) {
                    Ok(reply) => reply,
                    Err(reason) => {
                        let err = invalid_tool_call(reason, res.usage);
                        finish_failure(&err).await;
                        return Err(err);
                    }
                }
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
                finish_failure(&e).await;
                return Err(e);
            }
            if let Some(history) = &history {
                ThreadStore::remember(
                    threads.clone(),
                    history,
                    thread_id,
                    account.clone(),
                    composed.prompt_state,
                    &text,
                    &calls,
                )
                .await;
            }
            return Ok(ChatOutput::Completed(CompletedTurn {
                id: turn.id.clone(),
                created: turn.created,
                model: model.to_owned(),
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
        let turn_model = model.to_owned();
        let include_usage = turn.include_usage;
        let header_wait_ms = cfg.header_wait_ms;

        let sess_owned = session.clone();
        let mut cfg_owned = cfg.clone();
        let threads_owned = threads.clone();
        let account_owned = account.clone();
        let history_owned = history.clone();
        let mut cancel_owned = cancel_rx.clone();
        let turn_owned = turn.clone();
        let auth_owned = auth.clone();

        tokio::spawn(async move {
            let mut first_tx = Some(first_tx);
            let header_wait = async {
                if header_wait_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(header_wait_ms)).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::pin!(header_wait);
            let mut resized = false;

            let (res, mut bridge, mut reply) = loop {
                let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
                let mut bridge = turn_owned
                    .bridge
                    .then(|| BridgeStream::new(turn_owned.bridge_allowed.clone()));
                let mut reply = String::new();
                let relay = async {
                    loop {
                        let event = tokio::select! {
                            _ = &mut header_wait, if first_tx.is_some() => {
                                if let Some(tx) = first_tx.take() {
                                    let _ = tx.send(Ok(()));
                                }
                                continue;
                            }
                            event = ev_rx.recv() => event,
                        };
                        let Some(event) = event else { break };
                        let visible = match event {
                            UpstreamEvent::Reasoning(text) if !text.is_empty() => {
                                TurnEvent::Reasoning(text)
                            }
                            UpstreamEvent::Text(text) if !text.is_empty() => {
                                let ready = match &mut bridge {
                                    Some(bridge) => bridge.push(&text),
                                    None => Some(text),
                                };
                                let Some(ready) = ready else { continue };
                                reply.push_str(&ready);
                                TurnEvent::Text(ready)
                            }
                            _ => continue,
                        };
                        if let Some(tx) = first_tx.take() {
                            let _ = tx.send(Ok(()));
                        }
                        let _ = out_tx.send(visible);
                    }
                };
                let (res, ()) = tokio::join!(
                    chat(
                        &sess_owned,
                        &cfg_owned,
                        &composed.input,
                        cancel_owned.clone(),
                        Some(ev_tx)
                    ),
                    relay,
                );

                // Retry only a rejected frame, before the SSE response is committed.
                if resized
                    || first_tx.is_none()
                    || !res
                        .as_ref()
                        .is_err_and(|e| e.kind == UpstreamErrorKind::TooLarge)
                {
                    break (res, bridge, reply);
                }
                let Some(smaller) = smaller_frame_budget(&cfg_owned) else {
                    break (res, bridge, reply);
                };

                let recomposed = {
                    let prepare = compose(
                        &turn_owned,
                        &composed.input.model,
                        &auth_owned,
                        &smaller,
                        &sess_owned,
                        ComposeContext {
                            thread_id: &thread_id,
                            thread: thread.as_ref(),
                            previous_attempt: Some(&composed),
                        },
                    );
                    tokio::pin!(prepare);
                    loop {
                        tokio::select! {
                            biased;
                            _ = wait_for_cancellation(&mut cancel_owned) => break Err(UpstreamError::cancelled()),
                            result = &mut prepare => break result,
                            _ = &mut header_wait, if first_tx.is_some() => {
                                if let Some(tx) = first_tx.take() {
                                    let _ = tx.send(Ok(()));
                                }
                            }
                        }
                    }
                };
                match recomposed {
                    Ok(retry) => {
                        composed = retry;
                        cfg_owned = smaller;
                        resized = true;
                    }
                    Err(mut e) => {
                        // The rejected turn was never replaced, so the thread remains reusable.
                        e.can_resume_thread = true;
                        break (Err(e), bridge, reply);
                    }
                }
            };

            let result = res.and_then(|up_res| {
                let parsed = bridge
                    .as_mut()
                    .map_or(Ok((None, Vec::new())), BridgeStream::finish);
                match parsed {
                    Err(reason) => Err(invalid_tool_call(reason, up_res.usage)),
                    Ok((None, calls)) if reply.is_empty() && calls.is_empty() => {
                        Err(UpstreamError::new(
                            UpstreamErrorKind::Upstream,
                            "upstream returned no text or tool calls",
                            502,
                        )
                        .with_reported_usage(up_res.usage))
                    }
                    Ok((tail, calls)) => Ok((tail, calls, up_res.usage)),
                }
            });
            match result {
                Ok((tail, mut calls, usage)) => {
                    if let Some(tx) = first_tx.take() {
                        let _ = tx.send(Ok(()));
                    }
                    if let Some(text) = tail {
                        reply.push_str(&text);
                        let _ = out_tx.send(TurnEvent::Text(text));
                    }
                    if !turn_owned.parallel {
                        calls.truncate(1);
                    }
                    if let Some(history) = &history_owned {
                        ThreadStore::remember(
                            threads_owned,
                            history,
                            thread_id,
                            account_owned,
                            composed.prompt_state,
                            &reply,
                            &calls,
                        )
                        .await;
                    }
                    for call in calls {
                        let _ = out_tx.send(TurnEvent::Tool(call));
                    }
                    let _ = out_tx.send(TurnEvent::Done(NormalizedUsage::from_upstream(&usage)));
                }
                Err(e) => {
                    finish_failed_thread(
                        &threads_owned,
                        thread.as_ref(),
                        &e,
                        history_owned.as_deref(),
                        &account_owned,
                        &composed,
                    )
                    .await;
                    if let Some(tx) = first_tx.take() {
                        let _ = tx.send(Err(e));
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
        Err(e)
            if matched_thread.is_some()
                && !*cancel_rx.borrow()
                && e.kind == UpstreamErrorKind::ThreadRejected =>
        {
            attempt_run(None).await
        }
        result => result,
    };

    match final_res {
        Ok(out) => Ok(out),
        Err(mut e) => {
            if e.kind == UpstreamErrorKind::AdvancedEntitlementUnconfirmed {
                let mut cancel = cancel_rx.clone();
                let supported = tokio::select! {
                    biased;
                    _ = wait_for_cancellation(&mut cancel) => {
                        return Err(failure_from_error(&UpstreamError::cancelled(), None));
                    }
                    settings = qbs::refresh_account_settings(&auth, &cfg) => {
                        settings.ok().flatten().as_ref().and_then(advanced_supported)
                    }
                };
                if supported == Some(false) {
                    e.kind = UpstreamErrorKind::AdvancedUnavailable;
                }
            }
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

fn invalid_tool_call(reason: ToolCallError, usage: UpstreamUsage) -> UpstreamError {
    UpstreamError::new(UpstreamErrorKind::InvalidToolCall, reason.to_string(), 502)
        .with_reported_usage(usage)
}

async fn finish_failed_thread(
    threads: &Arc<Mutex<ThreadStore>>,
    thread: Option<&ThreadMatch>,
    err: &UpstreamError,
    history: Option<&ThreadHistory>,
    account: &str,
    composed: &ComposedTurn,
) {
    if err.kind == UpstreamErrorKind::Timeout
        && err.can_resume_thread
        && let Some(history) = history
    {
        ThreadStore::remember_stopped(
            threads.clone(),
            history,
            &composed.input.thread_id,
            account,
            thread,
            composed.context_attachment.clone(),
        )
        .await;
        return;
    }
    let Some(thread) = thread else { return };
    if err.can_resume_thread {
        threads.lock().release(&thread.id);
    } else {
        ThreadStore::discard(threads.clone(), thread.id.clone()).await;
    }
}

#[cfg(test)]
mod tests;
