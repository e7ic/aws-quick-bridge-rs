//! 客户端工具文本协议桥接（`<client_tool_call>...</client_tool_call>`）。

use crate::messages::{argument_string, call_id};
use serde_json::{Value, json};
use std::collections::HashSet;

pub const TOOL_OPEN: &str = "<client_tool_call>";
pub const TOOL_CLOSE: &str = "</client_tool_call>";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolDetail {
    Full,
    Desc(usize),
    Names,
}

pub const TOOL_DETAILS: &[ToolDetail] = &[
    ToolDetail::Full,
    ToolDetail::Desc(160),
    ToolDetail::Desc(60),
    ToolDetail::Desc(0),
    ToolDetail::Names,
];

const NAMES_MAX: usize = 12_000;

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ToolChoice {
    pub mode: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallReply {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolCallError {
    #[error("Amazon Quick returned an incomplete tool call. Try again.")]
    Incomplete,
    #[error("Amazon Quick returned invalid tool call data. Try again.")]
    Invalid,
    #[error("Amazon Quick requested a tool outside the client's current tool list. Try again.")]
    Unavailable,
}

pub fn tool_definitions(tools: Option<&[Value]>, functions: Option<&[Value]>) -> Vec<ToolDef> {
    let mut defs = Vec::new();
    let mut add = |f: Option<&Value>| {
        let Some(obj) = f.and_then(|v| v.as_object()) else {
            return;
        };
        let Some(name) = obj.get("name").and_then(|v| v.as_str()) else {
            return;
        };
        if name.trim().is_empty() {
            return;
        }
        let description = obj
            .get("description")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let parameters = obj.get("parameters").filter(|v| !v.is_null()).cloned();
        defs.push(ToolDef {
            name: name.to_string(),
            description,
            parameters,
        });
    };
    if let Some(ts) = tools {
        for t in ts {
            add(t.get("function"));
        }
    }
    if let Some(fs) = functions {
        for f in fs {
            add(Some(f));
        }
    }
    defs
}

pub fn requested_tool_names(body: &Value) -> HashSet<String> {
    let tools = body
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice());
    let funcs = body
        .get("functions")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice());
    tool_definitions(tools, funcs)
        .into_iter()
        .map(|d| d.name.trim().to_string())
        .collect()
}

pub fn parse_tool_choice(value: Option<&Value>) -> ToolChoice {
    match value {
        Some(Value::String(s)) => ToolChoice {
            mode: s.trim().to_ascii_lowercase(),
            name: String::new(),
        },
        Some(Value::Object(obj)) => {
            if let Some(n) = obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
            {
                return ToolChoice {
                    mode: "function".to_string(),
                    name: n.trim().to_string(),
                };
            }
            if let Some(n) = obj.get("name").and_then(|v| v.as_str()) {
                return ToolChoice {
                    mode: "function".to_string(),
                    name: n.trim().to_string(),
                };
            }
            if let Some(t) = obj.get("type").and_then(|v| v.as_str()) {
                return ToolChoice {
                    mode: t.trim().to_ascii_lowercase(),
                    name: String::new(),
                };
            }
            ToolChoice {
                mode: "auto".to_string(),
                name: String::new(),
            }
        }
        _ => ToolChoice {
            mode: "auto".to_string(),
            name: String::new(),
        },
    }
}

fn type_of(schema: Option<&Value>) -> String {
    let Some(obj) = schema.and_then(|v| v.as_object()) else {
        return "any".to_string();
    };
    if let Some(arr) = obj.get("enum").and_then(|v| v.as_array()) {
        return arr
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("|");
    }
    let typ = match obj.get("type") {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .find(|&t| t != "null")
            .unwrap_or(""),
        Some(Value::String(s)) => s.as_str(),
        _ => "",
    };
    if typ == "array" {
        return format!("{}[]", type_of(obj.get("items")));
    }
    if typ == "object" || obj.contains_key("properties") {
        let props = obj.get("properties").and_then(|v| v.as_object());
        let Some(props) = props else {
            return "object".to_string();
        };
        if props.is_empty() {
            return "object".to_string();
        }
        let required: HashSet<&str> = obj
            .get("required")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let fields: Vec<String> = props
            .iter()
            .map(|(k, v)| {
                let star = if required.contains(k.as_str()) {
                    "*"
                } else {
                    ""
                };
                format!("{k}{star}: {}", type_of(Some(v)))
            })
            .collect();
        return format!("{{{}}}", fields.join(", "));
    }
    if typ.is_empty() {
        "any".to_string()
    } else {
        typ.to_string()
    }
}

fn signature(def: &ToolDef, about: usize) -> String {
    let params = def.parameters.as_ref().and_then(|v| v.as_object());
    let required: HashSet<&str> = params
        .and_then(|p| p.get("required"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let args: Vec<String> = params
        .and_then(|p| p.get("properties"))
        .and_then(|v| v.as_object())
        .map(|props| {
            props
                .iter()
                .map(|(k, v)| {
                    let star = if required.contains(k.as_str()) {
                        "*"
                    } else {
                        ""
                    };
                    format!("{k}{star}: {}", type_of(Some(v)))
                })
                .collect()
        })
        .unwrap_or_default();
    let desc = if about > 0 {
        def.description
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        String::new()
    };
    let suffix = if desc.is_empty() {
        String::new()
    } else {
        let chars: Vec<char> = desc.chars().collect();
        if chars.len() > about {
            format!(": {}…", chars[..about].iter().collect::<String>())
        } else {
            format!(": {desc}")
        }
    };
    format!("- {}({}){suffix}", def.name, args.join(", "))
}

fn names_only(defs: &[ToolDef]) -> String {
    let mut out = String::new();
    let mut listed = 0usize;
    for def in defs {
        if out.len() + def.name.len() > NAMES_MAX {
            break;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(&def.name);
        listed += 1;
    }
    if listed < defs.len() {
        format!("{out}, … and {} more", defs.len() - listed)
    } else {
        out
    }
}

fn listing(defs: &[ToolDef], detail: ToolDetail) -> String {
    match detail {
        ToolDetail::Names => format!(
            "(tool names only. Call a tool only if you know its exact argument schema.)\n{}",
            names_only(defs)
        ),
        ToolDetail::Desc(about) => {
            let desc_part = if about > 0 { ": description" } else { "" };
            let mut lines = vec![format!(
                "(name(arguments){desc_part}; * marks a required argument)"
            )];
            for def in defs {
                lines.push(signature(def, about));
            }
            lines.join("\n")
        }
        ToolDetail::Full => {
            let arr: Vec<Value> = defs
                .iter()
                .map(|d| {
                    let mut m = serde_json::Map::new();
                    m.insert("name".to_string(), json!(d.name));
                    if let Some(desc) = &d.description {
                        m.insert("description".to_string(), json!(desc));
                    }
                    if let Some(params) = &d.parameters {
                        m.insert("parameters".to_string(), params.clone());
                    }
                    Value::Object(m)
                })
                .collect();
            serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".to_string())
        }
    }
}

pub fn build_tool_bridge_prompt(
    tools: Option<&[Value]>,
    functions: Option<&[Value]>,
    mode: &str,
    name: &str,
    parallel: bool,
    detail: ToolDetail,
) -> String {
    if mode == "none" {
        return String::new();
    }
    let defs = tool_definitions(tools, functions);
    if defs.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "<client_tool_protocol>".to_string(),
        "The client provides the following tools.".to_string(),
        listing(&defs, detail),
        String::new(),
        "Use the listed client tools for files and commands in the client's environment. Use Amazon Quick tools for resources in Quick, such as Quick attachments.".to_string(),
        "Emit one standalone plain-text marker per client tool call using the following format.".to_string(),
        r#"<client_tool_call>{"name":"TOOL_NAME","arguments":{...}}</client_tool_call>"#.to_string(),
        r#"Encode arguments as valid JSON. Escape double quotes (\"), backslashes (\\), and line breaks (\n) inside string values."#.to_string(),
    ];
    if defs.iter().any(|def| def.name.eq_ignore_ascii_case("bash"))
        && (mode != "function" || name.eq_ignore_ascii_case("bash"))
    {
        lines.extend([
            "For Python in Bash, use the workspace's configured launcher if known. Otherwise check the runners available to Bash in one call. For a standalone snippet on Windows, check `command -v uv` first. A Microsoft Store alias on PATH does not confirm that an interpreter works.".to_string(),
            r#"Use a quoted here-document for multiline Python, with its closing delimiter alone on an unindented line. Use Python's UTF-8 mode for captured text. For a standalone snippet on Windows when uv is available, the JSON command string "uv run --no-project python -X utf8 - <<'PY'\nprint('示例', \"quoted\", r\"\\t\")\nPY" shows the format. If the shell reports unexpected EOF or an unmatched quote, correct the quoting before retrying."#.to_string(),
        ]);
    }
    lines.push(
        "The client runs the tool and returns the result. Continue from that result. When the user asks for verbatim tool output, return it verbatim and omit routine tool narration.".to_string(),
    );
    if mode == "function" && !name.is_empty() {
        lines.push(format!(
            "You must call the tool {}.",
            serde_json::to_string(name).unwrap_or_default()
        ));
    }
    if mode == "required" {
        lines.push("You must call at least one tool.".to_string());
    }
    if !parallel {
        lines.push("Call at most one tool per turn.".to_string());
    }
    lines.push("</client_tool_protocol>".to_string());
    lines.join("\n")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JsonContainer {
    Object,
    Array,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JsonExpect {
    KeyOrEnd,
    Colon,
    Value,
    CommaOrEnd,
}

fn skip_ws(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    i
}

fn starts_with_keyword(chars: &[char], pos: usize, kw: &str) -> bool {
    let mut i = pos;
    for expected in kw.chars() {
        if i >= chars.len() || chars[i] != expected {
            return false;
        }
        i += 1;
    }
    i >= chars.len() || !(chars[i].is_ascii_alphanumeric() || chars[i] == '_')
}

fn looks_like_value_start(chars: &[char], pos: usize) -> bool {
    if pos >= chars.len() {
        return false;
    }
    match chars[pos] {
        '"' | '“' | '”' | '{' | '[' | '-' | '0'..='9' => true,
        't' => starts_with_keyword(chars, pos, "true"),
        'f' => starts_with_keyword(chars, pos, "false"),
        'n' => starts_with_keyword(chars, pos, "null"),
        _ => false,
    }
}

/// 判断从 `pos` 开始是否为合法的对象字段键定义（`"key": <value>`）。
fn looks_like_object_key_colon(chars: &[char], pos: usize) -> bool {
    let quote = match chars.get(pos) {
        Some('"') => '"',
        Some('“' | '”') => '”',
        _ => return false,
    };
    let mut j = pos + 1;
    let mut key_len = 0usize;
    while j < chars.len() {
        let c = chars[j];
        if c == '\\' {
            if j + 1 >= chars.len() {
                return false;
            }
            j += 2;
            key_len += 1;
            continue;
        }
        if c == quote {
            if key_len == 0 {
                return false;
            }
            let after_key = skip_ws(chars, j + 1);
            if after_key >= chars.len() || chars[after_key] != ':' {
                return false;
            }
            let after_colon = skip_ws(chars, after_key + 1);
            return looks_like_value_start(chars, after_colon);
        }
        if c == '\n' || c == '\r' || key_len > 256 {
            return false;
        }
        j += 1;
        key_len += 1;
    }
    false
}

/// 递归校验关闭 `}` 或 `]` 后，外层容器栈对应的后续 token 是否合法。
fn is_valid_after_closing(chars: &[char], pos: usize, remaining: &[JsonContainer]) -> bool {
    let Some(&parent) = remaining.last() else {
        return pos >= chars.len();
    };
    if pos >= chars.len() {
        return false;
    }
    let outer = &remaining[..remaining.len() - 1];
    match parent {
        JsonContainer::Object => match chars[pos] {
            ',' => {
                let next = skip_ws(chars, pos + 1);
                if looks_like_object_key_colon(chars, next) {
                    return true;
                }
                next < chars.len()
                    && chars[next] == '}'
                    && is_valid_after_closing(chars, skip_ws(chars, next + 1), outer)
            }
            '}' => is_valid_after_closing(chars, skip_ws(chars, pos + 1), outer),
            _ => false,
        },
        JsonContainer::Array => match chars[pos] {
            ',' => {
                let next = skip_ws(chars, pos + 1);
                if looks_like_value_start(chars, next) {
                    return true;
                }
                next < chars.len()
                    && chars[next] == ']'
                    && is_valid_after_closing(chars, skip_ws(chars, next + 1), outer)
            }
            ']' => is_valid_after_closing(chars, skip_ws(chars, pos + 1), outer),
            _ => false,
        },
    }
}

/// 判断字符串值中的 `"` 是否为真正的字符串结束引号，而非未转义的内部引号。
fn is_real_value_string_end(chars: &[char], pos: usize, stack: &[JsonContainer]) -> bool {
    let Some(&top) = stack.last() else {
        return pos >= chars.len();
    };
    if pos >= chars.len() {
        return false;
    }
    let outer = &stack[..stack.len() - 1];
    match top {
        JsonContainer::Object => match chars[pos] {
            ',' => {
                let next = skip_ws(chars, pos + 1);
                if looks_like_object_key_colon(chars, next) {
                    return true;
                }
                next < chars.len()
                    && chars[next] == '}'
                    && is_valid_after_closing(chars, skip_ws(chars, next + 1), outer)
            }
            '}' => is_valid_after_closing(chars, skip_ws(chars, pos + 1), outer),
            _ => false,
        },
        JsonContainer::Array => match chars[pos] {
            ',' => {
                let next = skip_ws(chars, pos + 1);
                if looks_like_value_start(chars, next) {
                    return true;
                }
                next < chars.len()
                    && chars[next] == ']'
                    && is_valid_after_closing(chars, skip_ws(chars, next + 1), outer)
            }
            ']' => is_valid_after_closing(chars, skip_ws(chars, pos + 1), outer),
            _ => false,
        },
    }
}

/// 容错修复模型在 `<client_tool_call>` 内输出的不规范 JSON：
/// 1. 去除误包在标签内的 Markdown 代码块围栏；
/// 2. 修正 JSON 字符串分隔用的弯引号，转义字符串里的双引号、非法反斜杠转义与裸控制字符；
/// 3. 去除对象/数组末尾的多余逗号。
fn repair_json_payload(raw: &str) -> String {
    let mut text = raw.trim();
    if text.starts_with("```")
        && let Some(nl) = text.find('\n')
    {
        text = text[nl + 1..].trim();
        if let Some(stripped) = text.strip_suffix("```") {
            text = stripped.trim();
        }
    }

    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 32);
    let mut stack: Vec<JsonContainer> = Vec::new();
    let mut expect = JsonExpect::Value;
    let mut i = 0usize;

    while i < chars.len() {
        let ch = chars[i];
        if ch.is_whitespace() {
            out.push(ch);
            i += 1;
            continue;
        }
        match ch {
            '{' => {
                stack.push(JsonContainer::Object);
                expect = JsonExpect::KeyOrEnd;
                out.push('{');
                i += 1;
            }
            '[' => {
                stack.push(JsonContainer::Array);
                expect = JsonExpect::Value;
                out.push('[');
                i += 1;
            }
            '}' => {
                if stack.last() == Some(&JsonContainer::Object) {
                    stack.pop();
                }
                expect = JsonExpect::CommaOrEnd;
                out.push('}');
                i += 1;
            }
            ']' => {
                if stack.last() == Some(&JsonContainer::Array) {
                    stack.pop();
                }
                expect = JsonExpect::CommaOrEnd;
                out.push(']');
                i += 1;
            }
            ':' => {
                expect = JsonExpect::Value;
                out.push(':');
                i += 1;
            }
            ',' => {
                let next = skip_ws(&chars, i + 1);
                if next < chars.len() && (chars[next] == '}' || chars[next] == ']') {
                    // 跳过尾随逗号。
                    i += 1;
                    continue;
                }
                out.push(',');
                expect = match stack.last() {
                    Some(JsonContainer::Object) => JsonExpect::KeyOrEnd,
                    _ => JsonExpect::Value,
                };
                i += 1;
            }
            '"' | '“' | '”' => {
                let quote = if ch == '"' { '"' } else { '”' };
                let is_key =
                    expect == JsonExpect::KeyOrEnd && stack.last() == Some(&JsonContainer::Object);
                out.push('"');
                i += 1;
                while i < chars.len() {
                    let c = chars[i];
                    if c == '\\' {
                        if i + 1 < chars.len() {
                            let next = chars[i + 1];
                            if matches!(next, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') {
                                out.push('\\');
                                out.push(next);
                                i += 2;
                                continue;
                            }
                            if next == 'u'
                                && i + 5 < chars.len()
                                && chars[i + 2..i + 6].iter().all(|h| h.is_ascii_hexdigit())
                            {
                                for &h in &chars[i..i + 6] {
                                    out.push(h);
                                }
                                i += 6;
                                continue;
                            }
                        }
                        // 非标准 JSON 转义（如正则 \d、Shell \$），自动补足反斜杠转义。
                        out.push_str("\\\\");
                        i += 1;
                        continue;
                    }
                    if c == quote {
                        let next_pos = skip_ws(&chars, i + 1);
                        let is_end = if is_key {
                            next_pos < chars.len() && chars[next_pos] == ':'
                        } else {
                            is_real_value_string_end(&chars, next_pos, &stack)
                        };
                        if is_end {
                            out.push('"');
                            i += 1;
                            expect = if is_key {
                                JsonExpect::Colon
                            } else {
                                JsonExpect::CommaOrEnd
                            };
                            break;
                        }
                    }
                    match c {
                        '"' => out.push_str("\\\""),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        ctrl if (ctrl as u32) < 0x20 => {
                            out.push_str(&format!("\\u{:04x}", ctrl as u32));
                        }
                        other => out.push(other),
                    }
                    i += 1;
                }
            }
            _ => {
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], '{' | '}' | '[' | ']' | ',' | ':')
                {
                    out.push(chars[i]);
                    i += 1;
                }
                expect = JsonExpect::CommaOrEnd;
            }
        }
    }
    out
}

fn parse_envelopes(payload: &str) -> Vec<Value> {
    let text = payload.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let val = match serde_json::from_str::<Value>(text) {
        Ok(v) => v,
        Err(_) => {
            // 严格解析失败时回退到容错修复解析，防止未转义引号等导致原始标签泄漏为纯文本。
            let repaired = repair_json_payload(text);
            let Ok(v) = serde_json::from_str::<Value>(&repaired) else {
                return Vec::new();
            };
            v
        }
    };
    match val {
        Value::Array(arr) => arr,
        Value::Object(ref obj) => {
            if let Some(Value::Array(tcs)) = obj.get("tool_calls")
                && !tcs.is_empty()
            {
                return tcs.clone();
            }
            vec![val]
        }
        _ => Vec::new(),
    }
}

fn replies_from(
    envelopes: Vec<Value>,
    allowed: &HashSet<String>,
) -> Result<Vec<ToolCallReply>, ToolCallError> {
    if envelopes.is_empty() {
        return Err(ToolCallError::Invalid);
    }
    envelopes
        .into_iter()
        .map(|env| {
            let name = env["name"]
                .as_str()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .ok_or(ToolCallError::Invalid)?;
            if !allowed.contains(name) {
                return Err(ToolCallError::Unavailable);
            }
            let supplied = env.get("arguments");
            let valid_arguments = match supplied {
                None | Some(Value::Null | Value::Object(_)) => true,
                Some(Value::String(s)) => {
                    s.trim().is_empty()
                        || serde_json::from_str::<Value>(s).is_ok_and(|value| value.is_object())
                }
                _ => false,
            };
            if !valid_arguments {
                return Err(ToolCallError::Invalid);
            }
            Ok(ToolCallReply {
                id: call_id(),
                name: name.to_string(),
                arguments: argument_string(supplied),
            })
        })
        .collect()
}

fn extract_tool_calls(
    text: &str,
    allowed: &HashSet<String>,
) -> Result<(String, Vec<ToolCallReply>), ToolCallError> {
    let mut clean = String::new();
    let mut rest = text;
    let mut replies = Vec::new();
    while let Some(start) = rest.find(TOOL_OPEN) {
        let after_open = start + TOOL_OPEN.len();
        let payload = &rest[after_open..];
        let Some(rel_end) = payload.find(TOOL_CLOSE) else {
            let candidate = payload.trim_start();
            if candidate.starts_with(['{', '[']) || candidate.starts_with("```") {
                return Err(ToolCallError::Incomplete);
            }
            break;
        };
        let end = after_open + rel_end;
        clean.push_str(&rest[..start]);
        let body = payload[..rel_end].trim_end();
        let mut envelopes = parse_envelopes(body);
        // 标签已闭合时，只补全缺失的外层对象，不补写参数值。
        if envelopes.is_empty()
            && body.ends_with('}')
            && let Ok(value) = serde_json::from_str::<Value>(&format!("{body}}}"))
            && value["name"].is_string()
            && value["arguments"].is_object()
        {
            envelopes.push(value);
        }
        replies.extend(replies_from(envelopes, allowed)?);
        rest = &rest[end + TOOL_CLOSE.len()..];
    }
    if replies.is_empty() {
        return Ok((text.to_string(), replies));
    }
    clean.push_str(rest);
    Ok((clean.trim().to_string(), replies))
}

fn extract_bare_tool_call(text: &str, allowed: &HashSet<String>) -> Option<Vec<ToolCallReply>> {
    if allowed.is_empty() {
        return None;
    }
    let mut trimmed = text.trim();
    if trimmed.starts_with("```")
        && let Some(nl) = trimmed.find('\n')
    {
        trimmed = trimmed[nl + 1..].trim();
        if let Some(stripped) = trimmed.strip_suffix("```") {
            trimmed = stripped.trim();
        }
    }
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        return None;
    }
    replies_from(parse_envelopes(trimmed), allowed).ok()
}

pub fn parse_client_tool_calls(
    text: &str,
    allowed: &HashSet<String>,
) -> Result<(String, Vec<ToolCallReply>), ToolCallError> {
    let (clean, replies) = extract_tool_calls(text, allowed)?;
    if replies.is_empty()
        && let Some(calls) = extract_bare_tool_call(text, allowed)
    {
        return Ok((String::new(), calls));
    }
    Ok((clean, replies))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BridgeMode {
    Start,
    Text,
    Held,
}

/// 流式转发助手文本，同时暂存可能是 `<client_tool_call>` 工具调用的前缀片段。
pub struct BridgeStream {
    allowed: HashSet<String>,
    mode: BridgeMode,
    pending: String,
    emitted: bool,
}

impl BridgeStream {
    pub fn new(allowed: HashSet<String>) -> Self {
        Self {
            allowed,
            mode: BridgeMode::Start,
            pending: String::new(),
            emitted: false,
        }
    }

    pub fn push(&mut self, delta: &str) -> Option<String> {
        if delta.is_empty() {
            return None;
        }
        self.pending.push_str(delta);
        if self.mode == BridgeMode::Held {
            return None;
        }
        if self.mode == BridgeMode::Start {
            let lead = self.pending.trim_start();
            if lead.is_empty() {
                return None;
            }
            if !self.allowed.is_empty() && lead.starts_with(['{', '[', '`']) {
                self.mode = BridgeMode::Held;
                return None;
            }
            self.mode = BridgeMode::Text;
        }
        if let Some(at) = self.pending.find(TOOL_OPEN) {
            let before = self.pending[..at].to_string();
            self.pending = self.pending[at..].to_string();
            self.mode = BridgeMode::Held;
            return self.emit_chunk(&before);
        }
        // 保留尾部可能匹配 TOOL_OPEN（ASCII）前缀的片段，避免截断起始标签。
        let open_bytes = TOOL_OPEN.as_bytes();
        let pend_bytes = self.pending.as_bytes();
        let max_k = (open_bytes.len() - 1).min(pend_bytes.len());
        let mut keep = 0usize;
        for n in (1..=max_k).rev() {
            if open_bytes[..n] == pend_bytes[pend_bytes.len() - n..] {
                keep = n;
                break;
            }
        }
        let split_at = self.pending.len() - keep;
        if split_at == 0 || !self.pending.is_char_boundary(split_at) {
            return None;
        }
        let ready = self.pending[..split_at].to_string();
        self.pending = self.pending[split_at..].to_string();
        self.emit_chunk(&ready)
    }

    fn emit_chunk(&mut self, text: &str) -> Option<String> {
        if text.is_empty() {
            return None;
        }
        if !self.emitted && text.trim().is_empty() {
            self.pending = format!("{text}{}", self.pending);
            return None;
        }
        self.emitted = true;
        Some(text.to_string())
    }

    pub fn finish(&mut self) -> Result<(Option<String>, Vec<ToolCallReply>), ToolCallError> {
        let rest = std::mem::take(&mut self.pending);
        let (text, replies) = if self.emitted {
            extract_tool_calls(&rest, &self.allowed)
        } else {
            parse_client_tool_calls(&rest, &self.allowed)
        }?;
        let tail = if !text.trim().is_empty() || (self.emitted && replies.is_empty()) {
            self.emit_chunk(&text)
        } else {
            None
        };
        Ok((tail, replies))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_prompt_keeps_required_fields_inside_arrays() {
        let tools = [json!({
            "function": {"name":"update_plan", "parameters": {
                "type":"object", "required":["items"], "properties":{
                    "items":{"type":"array","items":{
                        "type":"object", "required":["content","status"], "properties":{
                            "content":{"type":"string"},
                            "status":{"enum":["pending","in_progress","completed"]}
                        }
                    }}
                }
            }}
        })];
        let prompt =
            build_tool_bridge_prompt(Some(&tools), None, "auto", "", true, ToolDetail::Desc(0));
        assert!(prompt.contains("content*: string"));
        assert!(prompt.contains(r#"status*: "pending"|"in_progress"|"completed""#));
        assert!(!prompt.contains("quoted here-document"));
    }

    #[test]
    fn bridge_stream_repairs_typographic_json_quotes() {
        let mut stream = BridgeStream::new([String::from("Bash")].into());
        assert!(stream.push("<client_tool").is_none());
        let rest = r#"_call>{“name”:“Bash”,“arguments”:{“command”:“cd \"$TEMP\" && printf '%s' '“示例”'”,“description”:“读取示例”}}</client_tool_call>"#;
        assert!(stream.push(rest).is_none());
        let (tail, calls) = stream.finish().unwrap();
        assert!(tail.is_none());
        assert_eq!(calls.len(), 1);
        let arguments: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(
            arguments["command"],
            r#"cd "$TEMP" && printf '%s' '“示例”'"#
        );
    }

    #[test]
    fn tagged_tool_call_can_recover_its_outer_closing_brace() {
        let allowed = [String::from("Bash")].into();
        let payload = r#"{"name":"Bash","arguments":{"command":"python -c \"\nprint('示例')\n\"","description":"读取示例"}"#;
        let tagged = format!("{TOOL_OPEN}{payload}{TOOL_CLOSE}");
        let (text, calls) = parse_client_tool_calls(&tagged, &allowed).unwrap();
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "Bash");
        let arguments: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(arguments["command"], "python -c \"\nprint('示例')\n\"");

        let (_, bare_calls) = parse_client_tool_calls(payload, &allowed).unwrap();
        assert!(bare_calls.is_empty());
        let incomplete_payload = payload.strip_suffix('}').unwrap();
        let incomplete = format!("{TOOL_OPEN}{incomplete_payload}{TOOL_CLOSE}");
        assert!(matches!(
            parse_client_tool_calls(&incomplete, &allowed),
            Err(ToolCallError::Invalid)
        ));
        assert!(matches!(
            parse_client_tool_calls(&tagged, &HashSet::new()),
            Err(ToolCallError::Unavailable)
        ));
    }

    #[test]
    fn bridge_stream_does_not_publish_unusable_tagged_calls() {
        let mut stream = BridgeStream::new([String::from("Read")].into());
        let raw = r#"<client_tool_call>{"name":"Bash","arguments":{"command":"printf demo"}}</client_tool_call>"#;
        assert!(stream.push(raw).is_none());
        assert!(matches!(stream.finish(), Err(ToolCallError::Unavailable)));
    }

    #[test]
    fn bridge_stream_keeps_prose_but_rejects_an_incomplete_tool_call() {
        let mut stream = BridgeStream::new(HashSet::new());
        assert_eq!(
            stream.push("The marker is <client_tool_call>.").as_deref(),
            Some("The marker is ")
        );
        let (tail, calls) = stream.finish().unwrap();
        assert_eq!(tail.as_deref(), Some("<client_tool_call>."));
        assert!(calls.is_empty());

        let mut stream = BridgeStream::new([String::from("Read")].into());
        assert!(stream.push(r#"<client_tool_call>{"name":"Read""#).is_none());
        assert!(matches!(stream.finish(), Err(ToolCallError::Incomplete)));
    }

    #[test]
    fn test_bridge_stream_splits_marker() {
        let allowed: HashSet<String> = ["read_file"].into_iter().map(String::from).collect();
        let mut s = BridgeStream::new(allowed);
        let mut out = String::new();
        for piece in [
            "Hello ",
            "<client_",
            "tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.rs\"}}</client_tool_call>",
        ] {
            if let Some(t) = s.push(piece) {
                out.push_str(&t);
            }
        }
        let (tail, calls) = s.finish().unwrap();
        if let Some(t) = tail {
            out.push_str(&t);
        }
        assert_eq!(out, "Hello ");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
    }

    #[test]
    fn test_bare_json_call() {
        let allowed: HashSet<String> = ["bash"].into_iter().map(String::from).collect();
        let mut s = BridgeStream::new(allowed);
        assert!(
            s.push("```json\n{\"name\":\"bash\",\"arguments\":{\"cmd\":\"ls\"}}\n```")
                .is_none()
        );
        let (tail, calls) = s.finish().unwrap();
        assert!(tail.is_none());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
    }

    #[test]
    fn test_unescaped_quotes_in_shell_command_case1() {
        let allowed: HashSet<String> = ["Bash"].into_iter().map(String::from).collect();
        let leaked = r#"<client_tool_call>{"name":"Bash","arguments":{"command":"for d in /Users/e7ic/.codex/worktrees/v126-1-trip-fixes /Users/e7ic/.codex/worktrees/v126-back-school-qa; do rm "$d/.codex-worktree-name" && rmdir "$d" && echo "已删除 $d"; done; echo '--- worktree 列表:'; git worktree list; echo '--- chore/261124 与 main 差异:'; git rev-parse chore/261124 main","description":"删除两个只剩空标记文件的 codex 残留目录，并复核结果"}}</client_tool_call>"#;
        let (clean, calls) = parse_client_tool_calls(leaked, &allowed).unwrap();
        assert!(clean.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "Bash");
        let parsed: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert!(
            parsed["command"]
                .as_str()
                .unwrap()
                .contains(r#"rm "$d/.codex-worktree-name""#)
        );
        assert!(
            parsed["command"]
                .as_str()
                .unwrap()
                .contains(r#"echo "已删除 $d""#)
        );
        assert_eq!(
            parsed["description"],
            "删除两个只剩空标记文件的 codex 残留目录，并复核结果"
        );
    }

    #[test]
    fn test_unescaped_quotes_with_bracket_and_subshell_case2() {
        let allowed: HashSet<String> = ["Bash"].into_iter().map(String::from).collect();
        let leaked = r#"<client_tool_call>{"name":"Bash","arguments":{"command":"for d in /Users/e7ic/.codex/worktrees/v126-1-trip-fixes /Users/e7ic/.codex/worktrees/v126-back-school-qa; do if [ -e "$d" ]; then echo "仍存在：$d"; ls -la "$d"; else echo "已不存在：$d"; fi; done; echo '--- worktree 列表:'; git worktree list; echo '--- .git/worktrees 元数据:'; ls -la "$(git rev-parse --git-common-dir)/worktrees" 2>&1","description":"核实两个 codex 残留目录和 worktree 元数据是否已删除"}}</client_tool_call>"#;
        let mut stream = BridgeStream::new(allowed);
        assert!(stream.push(&leaked[..40]).is_none());
        assert!(stream.push(&leaked[40..]).is_none());
        let (tail, calls) = stream.finish().unwrap();
        assert!(tail.is_none());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "Bash");
        let parsed: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert!(
            parsed["command"]
                .as_str()
                .unwrap()
                .contains(r#"if [ -e "$d" ];"#)
        );
        assert!(
            parsed["command"]
                .as_str()
                .unwrap()
                .contains(r#"ls -la "$(git rev-parse --git-common-dir)/worktrees" 2>&1"#)
        );
    }

    #[test]
    fn test_repair_invalid_escapes_and_raw_newlines() {
        let allowed: HashSet<String> = ["Grep", "Bash"].into_iter().map(String::from).collect();
        let raw = "<client_tool_call>{\"name\":\"Grep\",\"arguments\":{\"pattern\":\"foo\\d+\\s+\",\"note\":\"line1\nline2\",}}</client_tool_call>";
        let (clean, calls) = parse_client_tool_calls(raw, &allowed).unwrap();
        assert!(clean.is_empty());
        assert_eq!(calls.len(), 1);
        let parsed: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(parsed["pattern"], r"foo\d+\s+");
        assert_eq!(parsed["note"], "line1\nline2");
    }
}
