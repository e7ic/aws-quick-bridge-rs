//! 配置项、网关选项与多账号凭据持久化管理。
//!
//! 配置与账号文件存储在 `~/.aws-quick-bridge`（或 `AWS_QUICK_BRIDGE_HOME`）目录下，
//! 使用目录 `0700` / 文件 `0600` 权限原子写入，并自动兼容原 JS 版桥接器的 `config.json` 与 `account.json`。

use anyhow::{Context, Result};
use rand::Rng;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

pub const DEFAULT_PORT: u16 = 18765;
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const REFRESH_LEAD_MS: i64 = 10 * 60_000;
pub const MODES: &[&str] = &["smart", "fast", "balanced", "auto"];
pub const AGENT_MODES: &[&str] = &["smart", "fast", "balanced", "auto", "advanced"];
pub(crate) const THINKING_EFFORTS: &[&str] = &["off", "low", "medium", "high", "max"];

pub(crate) fn normalize_thinking_effort(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "none" => Some("off"),
        "minimal" => Some("low"),
        "xhigh" => Some("max"),
        value => THINKING_EFFORTS
            .iter()
            .copied()
            .find(|effort| *effort == value),
    }
}

// 客户端协议角色按 Quick Smart、Balanced、Fast 的顺序发布。
pub(crate) const CLAUDE_ROLE_MODES: &[(&str, &str)] =
    &[("opus", "smart"), ("sonnet", "balanced"), ("haiku", "fast")];
pub(crate) const CLAUDE_DESKTOP_AUTO_ID: &str = "claude-auto-bridge";

pub fn model_display_name(id: &str) -> String {
    match id {
        "smart" => "Quick Smart".to_string(),
        "fast" => "Quick Fast".to_string(),
        "balanced" => "Quick Balanced".to_string(),
        "auto" => "Quick Auto".to_string(),
        "advanced" => "Quick Advanced".to_string(),
        other => format!("Quick {other}"),
    }
}

pub fn home_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("AWS_QUICK_BRIDGE_HOME") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".aws-quick-bridge")
}

pub fn config_path() -> PathBuf {
    home_dir().join("config.json")
}

pub fn legacy_account_path() -> PathBuf {
    home_dir().join("account.json")
}

pub fn accounts_path() -> PathBuf {
    home_dir().join("accounts.json")
}

/// 解析时长配置（支持 `"30s"`、`"5m"`、`"1h"`、`"250ms"` 或纯数字秒数）。
pub fn duration_ms(value: Option<&Value>, fallback_ms: u64) -> u64 {
    let Some(v) = value else {
        return fallback_ms;
    };
    if let Some(n) = v.as_f64() {
        if n > 0.0 && n.is_finite() {
            return (n * 1000.0).round() as u64;
        }
        return fallback_ms;
    }
    let Some(s) = v.as_str() else {
        return fallback_ms;
    };
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^\s*(\d+(?:\.\d+)?)\s*(ms|s|m|h)?\s*$").unwrap());
    let Some(caps) = re.captures(s) else {
        return fallback_ms;
    };
    let num: f64 = caps[1].parse().unwrap_or(0.0);
    if !(num > 0.0 && num.is_finite()) {
        return fallback_ms;
    }
    let mult = match caps.get(2).map(|m| m.as_str()).unwrap_or("s") {
        "ms" => 1.0,
        "s" => 1000.0,
        "m" => 60_000.0,
        "h" => 3_600_000.0,
        _ => 1000.0,
    };
    (num * mult).round() as u64
}

fn normalized_model_id(model: &str) -> String {
    static BRACKET_RE: OnceLock<Regex> = OnceLock::new();
    let re = BRACKET_RE.get_or_init(|| Regex::new(r"\[[^\]]*\]").unwrap());
    let cleaned = re.replace_all(model, "");
    cleaned
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn explicit_quick_mode(model: &str) -> Option<&'static str> {
    let model = model.strip_prefix("qw-").unwrap_or(model);
    AGENT_MODES.iter().copied().find(|&mode| mode == model)
}

pub fn map_model(model: &str, fallback: &str) -> String {
    explicit_quick_mode(&normalized_model_id(model))
        .unwrap_or(fallback)
        .to_string()
}

pub(crate) fn resolve_quick_model(requested: &str) -> Option<&'static str> {
    let id = normalized_model_id(requested);
    if let Some(mode) = explicit_quick_mode(&id) {
        return Some(mode);
    }
    if id == CLAUDE_DESKTOP_AUTO_ID {
        return Some("auto");
    }

    // 兼容现代 Claude ID、旧版日期 ID 和 Bedrock 推理配置 ID。
    static CLAUDE_ID: OnceLock<Regex> = OnceLock::new();
    let versioned_id = CLAUDE_ID.get_or_init(|| {
        Regex::new(
            r"(?x)
            \A (?: (?:[a-z]+\.)? anthropic\. )? claude-
            (?:
                (?P<family>opus|sonnet|haiku) (?:-[0-9]+)+
              | [0-9]+ (?:-[0-9]+)* -(?P<legacy_family>opus|sonnet|haiku)
                (?:-[0-9]{8}|-latest)?
            )
            (?:-v[0-9]+(?::[0-9]+)?)? \z",
        )
        .unwrap()
    });
    let family = versioned_id
        .captures(&id)
        .and_then(|caps| caps.name("family").or_else(|| caps.name("legacy_family")))
        .map_or(id.as_str(), |name| name.as_str());
    let alias = id.strip_prefix("claude-").and_then(|id| id.split_once('-'));
    CLAUDE_ROLE_MODES.iter().find_map(|&(role, mode)| {
        let matches_alias = alias.is_some_and(|(family, suffix)| {
            family == role && (suffix == "bridge" || suffix.strip_prefix("quick-") == Some(mode))
        });
        (family == role || matches_alias).then_some(mode)
    })
}

pub fn quick_model(model: &str, fallback: &str) -> String {
    if let Some(mode) = resolve_quick_model(model) {
        return mode.to_string();
    }
    static SMALL_RE: OnceLock<Regex> = OnceLock::new();
    let re = SMALL_RE.get_or_init(|| Regex::new(r"(?i)haiku|mini|nano|small|flash").unwrap());
    if re.is_match(model) {
        "fast".to_string()
    } else {
        fallback.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default)]
    pub options: serde_json::Map<String, Value>,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_host() -> String {
    DEFAULT_HOST.to_string()
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            api_key: generate_api_key(),
            port: DEFAULT_PORT,
            host: DEFAULT_HOST.to_string(),
            options: serde_json::Map::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub app_version: String,
    pub surface: String,
    pub outpost_id: String,
    pub stable_device_id: bool,
    pub working_session_id: String,
    pub timezone: String,
    pub locale: String,
    pub thinking_effort: String,
    pub keep_sessions: bool,
    pub web_search: bool,
    pub stateful: bool,
    pub attachments: bool,
    pub max_frame_bytes: usize,
    pub default_model: String,
    pub request_timeout_ms: u64,
    pub(crate) extend_active_request_timeout: bool,
    pub keepalive_ms: u64,
    pub header_wait_ms: u64,
    pub show_quick_tools: bool,
    pub proxy: String,
    pub stage: String,
    pub callback_port: u16,
    pub login_timeout_ms: u64,
    pub max_concurrency: usize,
    pub oidc_base: String,
    pub discovery_url: String,
    pub data_plane_url: String,
}

impl Settings {
    pub fn from_options(opts: &serde_json::Map<String, Value>) -> Self {
        let text = |key: &str, fallback: &str| -> String {
            opts.get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or(fallback)
                .to_string()
        };
        let bool_opt = |key: &str, default_val: bool| -> bool {
            opts.get(key)
                .and_then(|v| v.as_bool())
                .unwrap_or(default_val)
        };

        let thinking_effort = normalize_thinking_effort(&text("thinking_effort", ""))
            .unwrap_or_default()
            .to_owned();

        let max_frame_bytes = opts
            .get("max_frame_bytes")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(63_000);

        let keepalive_ms = duration_ms(opts.get("keepalive_interval"), 25_000).min(60_000);

        let header_wait_ms = match opts.get("header_wait") {
            Some(Value::String(s))
                if matches!(s.trim().to_ascii_lowercase().as_str(), "off" | "0") =>
            {
                0
            }
            Some(Value::Number(n)) if n.as_u64() == Some(0) => 0,
            other => duration_ms(other, 10_000),
        };

        let callback_port = opts
            .get("callback_port")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            })
            .filter(|&p| p < 65536)
            .map(|p| p as u16)
            .unwrap_or(18080);

        let max_concurrency = opts
            .get("max_concurrency")
            .and_then(|v| v.as_u64())
            .filter(|&n| (1..=64).contains(&n))
            .map(|n| n as usize)
            .unwrap_or(2);

        let stage = normalize_stage(&text("build_stage", "prod"));

        Self {
            app_version: text("app_version", "1.2.4660"),
            surface: text("surface", "QUICK_WORK_DESKTOP_WINDOWS"),
            outpost_id: text("outpost_id", ""),
            stable_device_id: bool_opt("stable_device_id", true),
            working_session_id: text("working_session_id", ""),
            timezone: text("timezone", "UTC"),
            locale: text("locale", "en-US"),
            thinking_effort,
            keep_sessions: bool_opt("keep_sessions", false),
            web_search: bool_opt("web_search", false),
            stateful: bool_opt("stateful", true),
            attachments: bool_opt("attachments", true),
            max_frame_bytes,
            default_model: map_model(&text("default_model", "smart"), "smart"),
            request_timeout_ms: duration_ms(opts.get("request_timeout"), 5 * 60_000),
            extend_active_request_timeout: !opts.contains_key("request_timeout"),
            keepalive_ms,
            header_wait_ms,
            show_quick_tools: bool_opt("show_quick_tools", true),
            proxy: text("proxy", ""),
            stage,
            callback_port,
            login_timeout_ms: duration_ms(opts.get("login_timeout"), 30 * 60_000),
            max_concurrency,
            oidc_base: text("oidc_base", "").trim_end_matches('/').to_string(),
            discovery_url: text("discovery_url", ""),
            data_plane_url: text("data_plane_url", "").trim_end_matches('/').to_string(),
        }
    }
}

pub fn normalize_stage(stage: &str) -> String {
    match stage.trim().to_ascii_lowercase().as_str() {
        "prod" => "prod",
        "beta" => "beta",
        "integ" => "integ",
        "dev" => "dev",
        _ => "prod",
    }
    .to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRecord {
    #[serde(rename = "type", default = "default_oauth_type")]
    pub auth_type: String,
    #[serde(default)]
    pub access: String,
    #[serde(default)]
    pub refresh: String,
    #[serde(default)]
    pub expires: i64,
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub account_name: String,
    #[serde(default)]
    pub actor_arn: String,
    #[serde(default)]
    pub aws_account_id: String,
    #[serde(default = "default_prod")]
    pub stage: String,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default)]
    pub tenant_url: String,
    #[serde(default)]
    pub ws_host: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub issuer_url: String,
    #[serde(default)]
    pub idc_region: String,
    #[serde(default)]
    pub token_endpoint: String,
    #[serde(default)]
    pub redirect_uri: String,
    #[serde(default)]
    pub device_id: String,
    /// 多账号池启用开关（默认 true 以兼容旧版凭据文件）。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_oauth_type() -> String {
    "oauth".to_string()
}
fn default_prod() -> String {
    "prod".to_string()
}
fn default_region() -> String {
    "us-east-1".to_string()
}
fn default_true() -> bool {
    true
}

impl AccountRecord {
    pub fn key(&self) -> String {
        format!("{}|{}", self.tenant_url, self.account_id)
    }

    pub fn display_label(&self) -> String {
        if !self.email.is_empty() {
            self.email.clone()
        } else if !self.account_id.is_empty() {
            self.account_id.clone()
        } else if !self.display_name.is_empty() {
            self.display_name.clone()
        } else {
            self.tenant_url.clone()
        }
    }

    pub fn is_valid(&self) -> bool {
        self.auth_type == "oauth" && !self.access.trim().is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AccountsFile {
    #[serde(default)]
    pub primary_key: String,
    #[serde(default)]
    pub accounts: Vec<AccountRecord>,
}

pub fn generate_api_key() -> String {
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    format!("sk-quick-{}", hex::encode(bytes))
}

pub(crate) fn write_secure_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut data = serde_json::to_string_pretty(value)?;
    data.push('\n');
    write_secure_file(path, data.as_bytes())
}

pub(crate) fn write_secure_file(path: &Path, data: &[u8]) -> Result<()> {
    static TMP_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

    let parent = path.parent().context("missing parent dir")?;
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    let seq = TMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}.{}.tmp", std::process::id(), seq));
    let write_res = (|| -> Result<()> {
        fs::write(&tmp, data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
        }
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_res
}

pub fn load_config() -> Result<BridgeConfig> {
    let path = config_path();
    let mut cfg: BridgeConfig = match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => BridgeConfig {
            api_key: String::new(),
            port: DEFAULT_PORT,
            host: DEFAULT_HOST.to_string(),
            options: serde_json::Map::new(),
        },
    };
    let mut dirty = false;
    if cfg.api_key.trim().is_empty() {
        cfg.api_key = generate_api_key();
        dirty = true;
    }
    if cfg.port == 0 {
        cfg.port = DEFAULT_PORT;
        dirty = true;
    }
    if cfg.host.trim().is_empty() {
        cfg.host = DEFAULT_HOST.to_string();
        dirty = true;
    }
    if dirty {
        let _ = save_config(&cfg);
    }
    Ok(cfg)
}

pub fn save_config(cfg: &BridgeConfig) -> Result<()> {
    write_secure_json(&config_path(), cfg)
}

pub fn rotate_api_key() -> Result<String> {
    let mut cfg = load_config()?;
    cfg.api_key = generate_api_key();
    save_config(&cfg)?;
    Ok(cfg.api_key)
}

/// 从 `accounts.json` 加载全部账号，并自动合并旧版 `account.json`（兼容 Node.js CLI）。
pub fn load_accounts() -> AccountsFile {
    let mut file: AccountsFile = fs::read_to_string(accounts_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if let Ok(raw) = fs::read_to_string(legacy_account_path())
        && let Ok(legacy) = serde_json::from_str::<AccountRecord>(&raw)
        && legacy.is_valid()
    {
        let key = legacy.key();
        if let Some(existing) = file.accounts.iter_mut().find(|a| a.key() == key) {
            if legacy.expires > existing.expires {
                let enabled = existing.enabled;
                *existing = legacy;
                existing.enabled = enabled;
            }
        } else {
            file.accounts.push(legacy);
        }
    }

    file.accounts.retain(|a| a.is_valid());
    if file.primary_key.is_empty() || !file.accounts.iter().any(|a| a.key() == file.primary_key) {
        file.primary_key = file.accounts.first().map(|a| a.key()).unwrap_or_default();
    }
    file
}

pub fn save_accounts(file: &AccountsFile) -> Result<()> {
    write_secure_json(&accounts_path(), file)?;
    // 同步写入主账号（或首个账号）到旧版 account.json，便于与原工具互通。
    let primary = file
        .accounts
        .iter()
        .find(|a| a.key() == file.primary_key)
        .or_else(|| file.accounts.first());
    if let Some(acc) = primary {
        let _ = write_secure_json(&legacy_account_path(), acc);
    } else {
        let _ = fs::remove_file(legacy_account_path());
    }
    Ok(())
}

/// 新增或更新已登录账号；重新登录同一账号时保留其固定 `device_id`。
pub fn upsert_account(mut record: AccountRecord) -> Result<AccountsFile> {
    let mut file = load_accounts();
    let key = record.key();
    if let Some(existing) = file.accounts.iter_mut().find(|a| a.key() == key) {
        if !existing.device_id.is_empty() {
            record.device_id = existing.device_id.clone();
        }
        record.enabled = existing.enabled;
        *existing = record;
    } else {
        file.accounts.push(record);
    }
    if file.primary_key.is_empty() {
        file.primary_key = key;
    }
    save_accounts(&file)?;
    Ok(file)
}

pub fn remove_account(key: &str) -> Result<AccountsFile> {
    let mut file = load_accounts();
    file.accounts
        .retain(|a| a.key() != key && a.account_id != key);
    if !file.accounts.iter().any(|a| a.key() == file.primary_key) {
        file.primary_key = file.accounts.first().map(|a| a.key()).unwrap_or_default();
    }
    save_accounts(&file)?;
    Ok(file)
}

pub fn remove_all_accounts() -> Result<()> {
    let empty = AccountsFile::default();
    save_accounts(&empty)?;
    let _ = fs::remove_file(accounts_path());
    let _ = fs::remove_file(legacy_account_path());
    Ok(())
}

fn model_slot_enabled(settings: &Value, mode: &str) -> Option<bool> {
    let slots = settings
        .get("model_slots")
        .or_else(|| settings.get("modelSlots"))?;
    let enabled = |slot: &Value| slot["enabled"].as_bool().unwrap_or(true);
    match slots {
        Value::Array(slots) => slots
            .iter()
            .filter(|slot| slot["id"].as_str() == Some(mode))
            .map(enabled)
            .reduce(|left, right| left && right),
        Value::Object(slots) => slots.get(mode).filter(|slot| slot.is_object()).map(enabled),
        _ => None,
    }
}

pub(crate) fn advanced_supported(settings: &Value) -> Option<bool> {
    let entitlement = settings["advanced_mode_enabled"]
        .as_bool()
        .or_else(|| settings["advancedModeEnabled"].as_bool());
    let slot = model_slot_enabled(settings, "advanced");
    if entitlement == Some(false) || slot == Some(false) {
        return Some(false);
    }
    entitlement.or(slot)
}

/// 根据账号 `get_settings` 返回结果过滤并补充可用模型列表。
pub fn account_models(account_settings: Option<&Value>) -> Vec<String> {
    let mut models: Vec<_> = MODES
        .iter()
        .copied()
        .filter(|mode| {
            account_settings.and_then(|settings| model_slot_enabled(settings, mode)) != Some(false)
        })
        .map(str::to_owned)
        .collect();
    if account_settings.and_then(advanced_supported) == Some(true) {
        models.push("advanced".into());
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_duration_ms() {
        assert_eq!(duration_ms(Some(&json!("30s")), 1), 30_000);
        assert_eq!(duration_ms(Some(&json!("5m")), 1), 300_000);
        assert_eq!(duration_ms(Some(&json!("1h")), 1), 3_600_000);
        assert_eq!(duration_ms(Some(&json!("250ms")), 1), 250);
        assert_eq!(duration_ms(Some(&json!(12)), 1), 12_000);
        assert_eq!(duration_ms(None, 5000), 5000);
    }

    #[test]
    fn test_map_and_quick_model() {
        assert_eq!(map_model("aws-quick/smart[1m]", "fast"), "smart");
        assert_eq!(map_model("qw-balanced", "smart"), "balanced");
        assert_eq!(map_model("unknown", "smart"), "smart");
        assert_eq!(quick_model("claude-3-5-haiku", "smart"), "fast");
        assert_eq!(quick_model("claude-opus-4-6", "smart"), "smart");
    }

    #[test]
    fn claude_model_families_select_their_quick_modes() {
        for (id, mode) in [
            ("claude-opus-5", "smart"),
            ("claude-sonnet-5-5", "balanced"),
            ("claude-haiku-4-5-20251001", "fast"),
            ("claude-3-5-sonnet-20241022", "balanced"),
            ("us.anthropic.claude-opus-5-5", "smart"),
            ("anthropic/claude-opus-5[1m]", "smart"),
            ("opus", "smart"),
            ("claude-sonnet-quick-balanced", "balanced"),
        ] {
            assert_eq!(quick_model(id, "auto"), mode, "{id}");
        }
        assert_eq!(quick_model("qw-fast[1m]", "auto"), "fast");
        assert_eq!(quick_model("not-a-claude-model", "auto"), "auto");
    }
}
