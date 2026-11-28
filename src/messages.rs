//! OpenAI 对话消息列表到 Amazon Quick `user_message` 文本与 `custom_prompt` 的转换。

use anyhow::{Result, bail};
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;
use uuid::Uuid;

pub fn uuid_v4() -> String {
    Uuid::new_v4().to_string()
}

pub fn call_id() -> String {
    format!("call_{}", Uuid::new_v4().simple())
}

pub fn argument_string(raw: Option<&Value>) -> String {
    match raw {
        None | Some(Value::Null) => "{}".to_string(),
        Some(Value::String(s)) => {
            if s.trim().is_empty() {
                "{}".to_string()
            } else {
                s.clone()
            }
        }
        Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string()),
    }
}

#[derive(Debug, Clone)]
pub struct InlineImage {
    pub name: String,
    pub mime_type: String,
    pub base64: String,
}

fn image_ext(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

fn image_part_url(part: &Value) -> String {
    match part.get("image_url") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(obj)) => obj
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// 将 OpenAI `content` 字段展平为纯文本；若传入 `images`，自动提取内联 data-URI 图片。
pub fn content_text(content: Option<&Value>, mut images: Option<&mut Vec<InlineImage>>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    match content {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            static DATA_RE: OnceLock<Regex> = OnceLock::new();
            let data_re = DATA_RE.get_or_init(|| {
                Regex::new(r"(?is)^data:(image/[a-z0-9.+-]+);base64,(.+)$").unwrap()
            });

            let mut out = String::new();
            for part in parts {
                if let Some(s) = part.as_str() {
                    out.push_str(s);
                    continue;
                }
                let Some(obj) = part.as_object() else {
                    if let Ok(s) = serde_json::to_string(part) {
                        out.push_str(&s);
                    }
                    continue;
                };
                match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "text" | "input_text" | "output_text" => {
                        if let Some(t) = obj.get("text").and_then(|v| v.as_str()) {
                            out.push_str(t);
                        }
                    }
                    "image_url" => {
                        let url = image_part_url(part);
                        if let Some(caps) = data_re.captures(&url) {
                            let mime = caps[1].to_ascii_lowercase();
                            if let (Some(ext), Some(imgs)) =
                                (image_ext(&mime), images.as_deref_mut())
                            {
                                let name = format!("image_{}.{ext}", imgs.len() + 1);
                                imgs.push(InlineImage {
                                    name: name.clone(),
                                    mime_type: mime,
                                    base64: caps[2].to_string(),
                                });
                                out.push_str(&format!("[image: attached file {name}]"));
                                continue;
                            }
                        }
                        if !url.is_empty() && !url.starts_with("data:") {
                            out.push_str(&format!("[image: {url}]"));
                        } else {
                            out.push_str("[image]");
                        }
                    }
                    "input_audio" => out.push_str("[audio]"),
                    "file" => out.push_str("[file]"),
                    _ => {
                        if let Some(t) = obj.get("text").and_then(|v| v.as_str()) {
                            out.push_str(t);
                        } else if let Ok(s) = serde_json::to_string(part) {
                            out.push_str(&s);
                        }
                    }
                }
            }
            out
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

pub fn role_of(m: &Value) -> String {
    let r = m
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if r.is_empty() { "user".to_string() } else { r }
}

pub fn is_system_role(role: &str) -> bool {
    role == "system" || role == "developer"
}

pub fn is_system_message(m: &Value) -> bool {
    is_system_role(&role_of(m))
}

fn role_label(role: &str) -> String {
    let mut chars = role.chars();
    match chars.next() {
        None => "User".to_string(),
        Some(first) => {
            first.to_uppercase().collect::<String>() + &chars.as_str().to_ascii_lowercase()
        }
    }
}

pub fn prompt_blocks(
    messages: &[Value],
    all: Option<&[Value]>,
    mut images: Option<&mut Vec<InlineImage>>,
) -> Result<Vec<String>> {
    if messages.is_empty() {
        bail!("messages must not be empty");
    }
    let lookup = all.unwrap_or(messages);
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for m in lookup {
        if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tcs {
                if let (Some(id), Some(name)) = (
                    tc.get("id").and_then(|v| v.as_str()),
                    tc.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str()),
                ) && !id.is_empty()
                    && !name.is_empty()
                {
                    tool_names.insert(id.to_string(), name.to_string());
                }
            }
        }
    }

    let mut blocks = Vec::new();
    let mut turns = 0usize;
    let mut only_user_text: Option<String> = None;

    for m in messages {
        let role = role_of(m);
        if is_system_role(&role) {
            continue;
        }
        turns += 1;
        let allow_img = role == "user" || role == "tool";
        let content = content_text(
            m.get("content"),
            if allow_img {
                images.as_deref_mut()
            } else {
                None
            },
        );
        let has_content = !content.trim().is_empty();
        only_user_text = if role == "user" {
            Some(content.clone())
        } else {
            None
        };

        match role.as_str() {
            "assistant" => {
                if has_content {
                    blocks.push(format!("Assistant: {content}"));
                }
                if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let id = tc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty())
                            .unwrap_or("unknown");
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty())
                            .unwrap_or("unknown");
                        let args =
                            argument_string(tc.get("function").and_then(|f| f.get("arguments")));
                        blocks.push(format!(
                            "Assistant tool call (id={id}, name={name}): {}",
                            args.trim()
                        ));
                    }
                }
                if let Some(fc) = m.get("function_call").filter(|v| v.is_object()) {
                    let name = fc
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .unwrap_or("unknown");
                    let args = argument_string(fc.get("arguments"));
                    blocks.push(format!(
                        "Assistant function call (name={name}): {}",
                        args.trim()
                    ));
                }
            }
            "tool" => {
                let id = m
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                let name = m
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        if !id.is_empty() {
                            tool_names.get(id).cloned()
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                let mut meta = Vec::new();
                if !id.is_empty() {
                    meta.push(format!("call_id={id}"));
                }
                if !name.is_empty() {
                    meta.push(format!("name={name}"));
                }
                let meta_str = if meta.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", meta.join(", "))
                };
                let body = if has_content {
                    content.as_str()
                } else {
                    "(empty)"
                };
                blocks.push(format!("Tool result{meta_str}: {body}"));
            }
            "function" => {
                let name = m.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
                let meta = if name.is_empty() {
                    String::new()
                } else {
                    format!(" (name={name})")
                };
                let body = if has_content {
                    content.as_str()
                } else {
                    "(empty)"
                };
                blocks.push(format!("Function result{meta}: {body}"));
            }
            "user" => {
                if has_content {
                    blocks.push(format!("User: {content}"));
                }
            }
            other => {
                if has_content {
                    blocks.push(format!("{}: {content}", role_label(other)));
                }
            }
        }
    }

    if blocks.is_empty() {
        bail!("messages contain no text or tool calls");
    }
    if turns == 1
        && let Some(u) = only_user_text
    {
        return Ok(vec![u]);
    }
    Ok(blocks)
}

pub const RELAYED_NOTE: &str = "Quick Bridge relays this conversation from a client application. Use the client's conversation instructions and any listed client tools for the task.";

pub const PLAIN_TEXT_NOTE: &str = "What you write here is shown outside the Amazon Quick app, as plain text and Markdown only. Please leave out the app's own interface markup, which would reach me as raw tags: no decision or option cards, no artifacts, no sources block and no source markers like [@a1b2c3]. When I have to choose, ask me in plain text with a short numbered list; when you name a source, give its title and address in the text. This is not about code or file contents I ask for.";

pub fn upstream_prompt_override() -> String {
    format!("{RELAYED_NOTE}\n\n{PLAIN_TEXT_NOTE}")
}

const AGENT_PLACE: &str = "This conversation comes from a coding agent running in a terminal or editor on the user's own machine.";

pub fn agent_neutral(text: &str) -> String {
    static RE1: OnceLock<Regex> = OnceLock::new();
    static RE2: OnceLock<Regex> = OnceLock::new();
    let re1 = RE1.get_or_init(|| {
        Regex::new(
            r"You are Claude Code, Anthropic's official CLI for Claude(?:, running within the Claude Agent SDK)?\.",
        )
        .unwrap()
    });
    let re2 = RE2.get_or_init(|| {
        Regex::new(r"You are a Claude agent, built on Anthropic's Claude Agent SDK\.").unwrap()
    });

    let filtered: Vec<&str> = text
        .split('\n')
        .filter(|line| !line.starts_with("x-anthropic-billing-header:"))
        .map(|line| match line.trim() {
            "You are running inside the Claude desktop app (Code tab)." => {
                "The client is using the Code tab in Claude Desktop."
            }
            _ => line,
        })
        .collect();
    let joined = filtered.join("\n");
    let step1 = re1.replace_all(&joined, AGENT_PLACE);
    let step2 = re2.replace_all(&step1, AGENT_PLACE);
    step2.trim().to_string()
}

pub fn system_text(messages: &[Value]) -> String {
    let mut parts = Vec::new();
    for m in messages {
        if !is_system_message(m) {
            continue;
        }
        let t = agent_neutral(&content_text(m.get("content"), None));
        if !t.is_empty() {
            parts.push(t);
        }
    }
    parts.join("\n\n")
}

fn strip_xml_blocks(input: &str) -> String {
    // 剔除成对的 `<tag>...</tag>` 区块（如 `<system-reminder>...</system-reminder>`），避免干扰用户语言检测。
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let mut j = i + 1;
            if j < bytes.len() && bytes[j].is_ascii_alphabetic() {
                while j < bytes.len()
                    && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'-')
                {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'>' {
                    let tag = &input[i + 1..j];
                    let close = format!("</{tag}>");
                    if let Some(rel) = input[j + 1..].find(&close) {
                        i = j + 1 + rel + close.len();
                        continue;
                    }
                }
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

pub fn user_language(messages: &[Value]) -> &'static str {
    let mut text = String::new();
    let mut seen = 0usize;
    for m in messages.iter().rev() {
        if seen >= 4 {
            break;
        }
        if role_of(m) != "user" {
            continue;
        }
        seen += 1;
        let raw = content_text(m.get("content"), None);
        text.push_str(&strip_xml_blocks(&raw));
        text.push('\n');
    }

    let mut han = 0usize;
    let mut kana = 0usize;
    let mut hangul = 0usize;
    for ch in text.chars() {
        let cp = ch as u32;
        if (0x3400..=0x4dbf).contains(&cp)
            || (0x4e00..=0x9fff).contains(&cp)
            || (0xf900..=0xfaff).contains(&cp)
        {
            han += 1;
        } else if (0x3040..=0x30ff).contains(&cp) {
            kana += 1;
        } else if (0xac00..=0xd7af).contains(&cp) {
            hangul += 1;
        }
    }
    if kana + hangul + han < 4 {
        return "";
    }
    if kana >= 2 && kana >= hangul {
        return "Japanese";
    }
    if hangul >= 2 && hangul > han {
        return "Korean";
    }
    if han > 0 { "Chinese" } else { "" }
}

pub fn language_note(messages: &[Value]) -> String {
    let lang = user_language(messages);
    if lang.is_empty() {
        return String::new();
    }
    format!(
        "I write in {lang}. Please think and plan in {lang} too, and reply in {lang} unless I ask for another language. In tool arguments, copy file names, paths, log lines, error messages and search terms exactly as they appear in my messages, the files and the tool results: never translate them."
    )
}

pub fn last_user_text(messages: &[Value]) -> String {
    for m in messages.iter().rev() {
        if role_of(m) != "user" {
            continue;
        }
        let t = content_text(m.get("content"), None).trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    String::new()
}

pub fn title_for(prompt: &str) -> String {
    let clean: String = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.is_empty() {
        return "New Conversation".to_string();
    }
    let chars: Vec<char> = clean.chars().collect();
    if chars.len() > 60 {
        format!("{}…", chars[..60].iter().collect::<String>())
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_agent_neutral_and_language() {
        let raw = "x-anthropic-billing-header: foo\nYou are Claude Code, Anthropic's official CLI for Claude.\nDo work.";
        let cleaned = agent_neutral(raw);
        assert!(!cleaned.contains("billing-header"));
        assert!(cleaned.contains("coding agent"));

        let msgs = vec![json!({
            "role": "user",
            "content": "<system-reminder>english only reminder</system-reminder>帮我看看这个报错日志"
        })];
        assert_eq!(user_language(&msgs), "Chinese");
    }

    #[test]
    fn test_prompt_blocks_single_and_multi() {
        let single = vec![json!({"role": "user", "content": "hello"})];
        assert_eq!(prompt_blocks(&single, None, None).unwrap(), vec!["hello"]);

        let multi = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "function": {"name": "read", "arguments": "{\"p\":\"a\"}"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "ok"}),
        ];
        let blocks = prompt_blocks(&multi, None, None).unwrap();
        assert_eq!(blocks.len(), 3);
        assert!(blocks[2].contains("call_id=c1, name=read"));
    }
}
