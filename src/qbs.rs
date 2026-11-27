//! Amazon Quick HTTPS 账号接口：额度查询、账号设置、附件上传与 DLP 安全扫描。

use crate::config::{AccountRecord, Settings};
use crate::oauth::build_http_client;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
#[error("{message}")]
pub struct QbsError {
    pub message: String,
    pub status: u16,
}

impl QbsError {
    pub fn new(message: impl Into<String>, status: u16) -> Self {
        Self {
            message: message.into(),
            status,
        }
    }
}

fn base_url(auth: &AccountRecord) -> Result<String, QbsError> {
    let url = auth.tenant_url.trim().trim_end_matches('/');
    if url.is_empty() {
        return Err(QbsError::new(
            "the account has no tenant_url; sign in again",
            401,
        ));
    }
    Ok(url.to_string())
}

pub fn unwrap_response(text: &str) -> (u16, Value) {
    let outer: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return (200, Value::Null),
    };
    let mut inner = outer
        .get("eventResponse")
        .and_then(|e| e.get("invokeAgentResponse"))
        .or_else(|| outer.get("invokeAgentResponse"))
        .cloned()
        .unwrap_or(outer);
    if let Value::String(s) = &inner
        && let Ok(parsed) = serde_json::from_str::<Value>(s)
    {
        inner = parsed;
    }
    let Some(obj) = inner.as_object() else {
        return (200, inner);
    };
    let status = obj
        .get("status")
        .or_else(|| obj.get("statusCode"))
        .and_then(|v| v.as_u64())
        .map(|n| n as u16)
        .filter(|&n| n > 0)
        .unwrap_or(200);
    let mut body = obj.get("body").cloned().unwrap_or(inner);
    if let Value::String(s) = &body
        && let Ok(parsed) = serde_json::from_str::<Value>(s)
    {
        body = parsed;
    }
    (status, body)
}

pub async fn invoke(
    auth: &AccountRecord,
    cfg: &Settings,
    path: &str,
    method: &str,
    body: Value,
    timeout_secs: u64,
) -> Result<Value, QbsError> {
    let base = base_url(auth)?;
    let path = path.trim_start_matches('/');
    let client = build_http_client(cfg);
    let trace_id = Uuid::new_v4().to_string();

    let url = format!("{base}/integration/quick-work/qbs-event");
    let payload = json!({
        "eventType": "invokeAgent",
        "eventBody": {
            "invokeAgent": {
                "path": path,
                "queryParams": { "_method": method },
                "body": body,
            }
        }
    });

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", auth.access))
        .header("x-amzn-qs-internal-trace-id", trace_id)
        .timeout(Duration::from_secs(timeout_secs))
        .json(&payload)
        .send()
        .await
        .map_err(|e| QbsError::new(format!("Amazon Quick request failed: {e}"), 0))?;

    let http_status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if !(200..300).contains(&http_status) {
        let snippet: String = text.chars().take(200).collect();
        let suffix = if snippet.is_empty() {
            String::new()
        } else {
            format!(": {snippet}")
        };
        return Err(QbsError::new(
            format!("Amazon Quick answered {http_status}{suffix}"),
            http_status,
        ));
    }

    let (inner_status, inner_body) = unwrap_response(&text);
    if inner_status >= 400 {
        return Err(QbsError::new(
            format!("Amazon Quick answered {inner_status} for {path}"),
            inner_status,
        ));
    }
    Ok(inner_body)
}

fn parse_iso_time(val: Option<&Value>) -> Option<DateTime<Utc>> {
    let val = val?;
    if let Some(n) = val.as_f64()
        && n > 0.0
        && n.is_finite()
    {
        let ms = if n < 1e11 {
            (n * 1000.0) as i64
        } else {
            n as i64
        };
        return DateTime::from_timestamp_millis(ms);
    }
    if let Some(s) = val.as_str() {
        let t = s.trim();
        if t.is_empty() {
            return None;
        }
        if let Ok(n) = t.parse::<f64>()
            && n > 0.0
            && n.is_finite()
        {
            let ms = if n < 1e11 {
                (n * 1000.0) as i64
            } else {
                n as i64
            };
            return DateTime::from_timestamp_millis(ms);
        }
        if let Ok(dt) = DateTime::parse_from_rfc3339(t) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub name: String,
    pub amount: f64,
    pub limit: f64,
    pub percent: Option<f64>,
    pub display: String,
    pub resets_at: Option<String>,
    pub reset_secs: Option<u64>,
    pub aside: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageReport {
    pub plan: String,
    pub user: Option<String>,
    pub until: Option<String>,
    pub windows: Vec<QuotaWindow>,
}

fn quota_window(name: &str, q: Option<&Value>, aside: bool) -> Option<QuotaWindow> {
    let obj = q?.as_object()?;
    let limit = obj.get("limit").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let amount = obj.get("used").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let percent = obj.get("percent").and_then(|v| v.as_f64());
    if limit <= 0.0 && percent.is_none() {
        return None;
    }
    let display = if limit > 0.0 {
        format!("{amount} / {limit} credits")
    } else {
        format!("{:.0}% used", percent.unwrap_or(0.0))
    };
    let resets_at = parse_iso_time(obj.get("resets_at")).map(|dt| dt.to_rfc3339());
    let reset_secs = if resets_at.is_none() {
        obj.get("resume_in_minutes")
            .and_then(|v| v.as_f64())
            .filter(|&m| m > 0.0)
            .map(|m| (m * 60.0).round() as u64)
    } else {
        None
    };
    Some(QuotaWindow {
        name: name.to_string(),
        amount,
        limit,
        percent,
        display,
        resets_at,
        reset_secs,
        aside,
    })
}

pub fn usage_from_profile(raw: &Value, auth: &AccountRecord) -> UsageReport {
    let raw_type = raw
        .get("subscription")
        .and_then(|s| s.get("subscription_type"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let plan = match raw_type.to_ascii_uppercase().as_str() {
        "QUICK_LITE_FREE_TRIAL" | "FREE_TRIAL" => "Free Trial".to_string(),
        "FREE" => "Free".to_string(),
        "PRO" => "Pro".to_string(),
        "TEAM" => "Team".to_string(),
        "ENTERPRISE" => "Enterprise".to_string(),
        "" => "Amazon Quick".to_string(),
        _ => raw_type.to_string(),
    };
    let until = parse_iso_time(
        raw.get("subscription")
            .and_then(|s| s.get("free_trial_metadata"))
            .and_then(|m| m.get("expiry_date")),
    )
    .map(|dt| dt.to_rfc3339());

    let mut windows = Vec::new();
    if let Some(w) = quota_window("Monthly", raw.get("monthly"), false) {
        windows.push(w);
    }
    if let Some(w) = quota_window("Session", raw.get("session"), false) {
        windows.push(w);
    }
    if let Some(w) = quota_window("Monthly overage", raw.get("overage_monthly"), true)
        && (w.limit > 0.0 || w.amount > 0.0 || w.percent.unwrap_or(0.0) > 0.0)
    {
        windows.push(w);
    }

    UsageReport {
        plan,
        user: if auth.email.is_empty() {
            None
        } else {
            Some(auth.email.clone())
        },
        until,
        windows,
    }
}

pub async fn usage(auth: &AccountRecord, cfg: &Settings) -> Result<UsageReport, QbsError> {
    let raw = invoke(auth, cfg, "profile/usage", "GET", json!({}), 15).await?;
    Ok(usage_from_profile(&raw, auth))
}

#[derive(Debug, Clone)]
pub struct SpentAllowance {
    pub name: String,
    pub secs: u64,
}

pub fn spent_allowance(raw: &Value) -> Option<SpentAllowance> {
    let is_spent = |q: Option<&Value>| -> bool {
        let Some(obj) = q.and_then(|v| v.as_object()) else {
            return false;
        };
        let limit = obj.get("limit").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if limit > 0.0 {
            obj.get("used").and_then(|v| v.as_f64()).unwrap_or(0.0) >= limit
        } else {
            obj.get("percent").and_then(|v| v.as_f64()).unwrap_or(0.0) >= 100.0
        }
    };
    let now_ms = Utc::now().timestamp_millis();
    let secs = |q: Option<&Value>| -> u64 {
        let Some(obj) = q.and_then(|v| v.as_object()) else {
            return 0;
        };
        if let Some(dt) = parse_iso_time(obj.get("resets_at")) {
            let diff = dt.timestamp_millis() - now_ms;
            return if diff > 0 {
                ((diff + 999) / 1000) as u64
            } else {
                0
            };
        }
        obj.get("resume_in_minutes")
            .and_then(|v| v.as_f64())
            .filter(|&m| m > 0.0)
            .map(|m| (m * 60.0).round() as u64)
            .unwrap_or(0)
    };

    let mut found = Vec::new();
    if is_spent(raw.get("session")) {
        found.push(SpentAllowance {
            name: "session".to_string(),
            secs: secs(raw.get("session")),
        });
    }
    let overage = raw.get("overage_monthly");
    let has_overage_room = overage
        .and_then(|o| o.get("limit"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
        > 0.0
        && !is_spent(overage);
    if is_spent(raw.get("monthly")) && !has_overage_room {
        found.push(SpentAllowance {
            name: "monthly".to_string(),
            secs: secs(raw.get("monthly")),
        });
    }
    found.sort_by_key(|window| std::cmp::Reverse(window.secs));
    found.into_iter().next()
}

pub async fn spent(auth: &AccountRecord, cfg: &Settings) -> Option<SpentAllowance> {
    let raw = invoke(auth, cfg, "profile/usage", "GET", json!({}), 5)
        .await
        .ok()?;
    spent_allowance(&raw)
}

struct SettingsCacheEntry {
    at: Instant,
    value: Value,
}

fn settings_cache() -> &'static Mutex<HashMap<String, SettingsCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, SettingsCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub async fn account_settings(
    auth: &AccountRecord,
    cfg: &Settings,
) -> Result<Option<Value>, QbsError> {
    let key = auth.key();
    {
        let cache = settings_cache().lock();
        if let Some(hit) = cache.get(&key)
            && hit.at.elapsed() < Duration::from_secs(120)
        {
            return Ok(Some(hit.value.clone()));
        }
    }
    refresh_account_settings(auth, cfg).await
}

pub(crate) async fn refresh_account_settings(
    auth: &AccountRecord,
    cfg: &Settings,
) -> Result<Option<Value>, QbsError> {
    let res = invoke(
        auth,
        cfg,
        "api/ws-rpc",
        "POST",
        json!({ "type": "get_settings", "payload": {} }),
        15,
    )
    .await?;
    let value = res
        .get("frames")
        .and_then(|v| v.as_array())
        .and_then(|frames| {
            frames
                .iter()
                .find(|f| f.get("type").and_then(|v| v.as_str()) == Some("settings"))
                .and_then(|f| f.get("payload"))
        })
        .or_else(|| res.get("payload"))
        .cloned();
    if let Some(ref v) = value
        && v.is_object()
    {
        let mut cache = settings_cache().lock();
        if cache.len() > 32 {
            cache.clear();
        }
        cache.insert(
            auth.key(),
            SettingsCacheEntry {
                at: Instant::now(),
                value: v.clone(),
            },
        );
    }
    Ok(value)
}

pub(crate) fn remember_advanced_unavailable(auth: &AccountRecord) {
    let key = auth.key();
    let mut cache = settings_cache().lock();
    if cache.len() > 32 && !cache.contains_key(&key) {
        cache.clear();
    }
    let entry = cache.entry(key).or_insert_with(|| SettingsCacheEntry {
        at: Instant::now(),
        value: json!({}),
    });
    entry.at = Instant::now();
    entry.value["advanced_mode_enabled"] = json!(false);
}

#[derive(Debug, Clone, Serialize)]
pub struct UploadedAttachment {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub size: usize,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub path: String,
    #[serde(rename = "attachmentId")]
    pub attachment_id: String,
    #[serde(rename = "contentSha256")]
    pub content_sha256: String,
}

pub async fn upload_attachment(
    auth: &AccountRecord,
    cfg: &Settings,
    name: &str,
    mime_type: &str,
    data: &[u8],
    thread_id: &str,
) -> Result<UploadedAttachment, QbsError> {
    if data.is_empty() {
        return Err(QbsError::new("the attachment is empty", 400));
    }
    let mime = mime_type
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .trim()
        .to_ascii_lowercase();
    let mime = if mime.is_empty() {
        "application/octet-stream".to_string()
    } else {
        mime
    };

    let base = base_url(auth)?;
    let client = build_http_client(cfg);
    let upload_url = format!(
        "{base}/integration/quick-work/upload-attachment?name={}&contentType={}&contentLength={}",
        urlencoding::encode(name),
        urlencoding::encode(&mime),
        data.len()
    );
    let resp = client
        .post(&upload_url)
        .header("authorization", format!("Bearer {}", auth.access))
        .header("content-type", "application/octet-stream")
        .timeout(Duration::from_secs(60))
        .body(data.to_vec())
        .send()
        .await
        .map_err(|e| QbsError::new(format!("upload failed: {e}"), 0))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(QbsError::new(
            format!(
                "Amazon Quick answered {status}: {}",
                text.chars().take(200).collect::<String>()
            ),
            status,
        ));
    }
    let up: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let attachment_id = up
        .get("attachmentId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if attachment_id.is_empty() {
        return Err(QbsError::new("Amazon Quick returned no attachment id", 502));
    }

    let dlp_url = format!(
        "{base}/integration/quick-work/attachment-dlp-status?attachmentId={}",
        urlencoding::encode(&attachment_id)
    );
    for _ in 0..20 {
        if let Ok(r) = client
            .get(&dlp_url)
            .header("authorization", format!("Bearer {}", auth.access))
            .timeout(Duration::from_secs(15))
            .send()
            .await
        {
            let st = r.status().as_u16();
            let t = r.text().await.unwrap_or_default();
            if st == 401 || st == 403 {
                return Err(QbsError::new(format!("Amazon Quick answered {st}"), st));
            }
            if let Ok(dlp) = serde_json::from_str::<Value>(&t) {
                let dstatus = dlp
                    .get("dlpJobStatus")
                    .or_else(|| dlp.get("DlpJobStatus"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_uppercase();
                let action = dlp
                    .get("policyAction")
                    .or_else(|| dlp.get("PolicyAction"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_uppercase();
                if matches!(dstatus.as_str(), "COMPLETED" | "SKIPPED" | "NOT_APPLICABLE") {
                    if !action.is_empty() && action != "ALLOW" && action != "WARN" {
                        return Err(QbsError::new(
                            format!(
                                "Amazon Quick's data protection policy blocked the attachment ({action})"
                            ),
                            400,
                        ));
                    }
                    break;
                }
                if matches!(dstatus.as_str(), "FAILED" | "ERROR") {
                    return Err(QbsError::new(
                        format!("Amazon Quick could not scan the attachment ({dstatus})"),
                        502,
                    ));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
    }

    let delivered = invoke(
        auth,
        cfg,
        &format!("files/upload/{}", urlencoding::encode(thread_id)),
        "POST",
        json!({ "attachmentId": attachment_id }),
        30,
    )
    .await?;

    let path = delivered
        .get("path")
        .and_then(|v| v.as_str())
        .or_else(|| {
            delivered
                .get("files")
                .and_then(|f| f.as_array())
                .and_then(|a| a.first())
                .and_then(|x| x.get("path"))
                .and_then(|v| v.as_str())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or(name)
        .to_string();

    let sha = hex::encode(Sha256::digest(data));
    Ok(UploadedAttachment {
        kind: if mime.starts_with("image/") {
            "image".to_string()
        } else {
            "file".to_string()
        },
        name: name.to_string(),
        size: data.len(),
        mime_type: mime,
        path,
        attachment_id,
        content_sha256: sha,
    })
}
