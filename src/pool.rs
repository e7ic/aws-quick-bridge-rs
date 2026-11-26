//! 自主实现的类 Magpie 多账号池、令牌续期、故障切换与实时监控遥测。
//!
//! 在纯 Rust 进程内替代原 Magpie 宿主能力：
//! - 多账号存储、启用/禁用与主账号切换
//! - 按账号去重并发锁定的令牌自动续期与持久化
//! - 会话亲和性路由（将同一会话固定到同一账号以便复用服务端线程）
//! - 并发数跟踪以及 401（过期）/ 429（冷却）自动故障切换
//! - 面向 GPUI 桌面端的请求日志环形缓冲区与实时变更广播

mod request_stats;
mod usage_stats;

use crate::config::{
    AccountRecord, AccountsFile, BridgeConfig, REFRESH_LEAD_MS, Settings, home_dir, load_accounts,
    load_config, remove_account, save_accounts, save_config, upsert_account,
};
use crate::oauth::{OAuthError, refresh_account};
use crate::qbs::{self, UsageReport};
use crate::threads::{ThreadStore, digest};
use crate::upstream::ContextUsage;
use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
#[cfg(feature = "gui")]
pub(crate) use request_stats::RequestProtocol;
pub(crate) use request_stats::RequestSample;
use request_stats::{RequestStatsStore, StatsUpdate};
#[cfg(feature = "gui")]
pub(crate) use usage_stats::{
    ContextSample, USAGE_RETENTION_DAYS, UsageBucket, UsageHistory, UsageTotals,
};
use usage_stats::UsageStore;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

const MAX_LOG_ENTRIES: usize = 300;
const DEFAULT_COOLDOWN_SECS: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountHealth {
    Healthy,
    CoolingDown {
        until_epoch_ms: i64,
        reason: String,
    },
    Expired {
        reason: String,
    },
    Disabled,
}

#[derive(Debug, Clone)]
pub struct AccountRuntime {
    pub record: AccountRecord,
    pub health: AccountHealth,
    pub active_requests: usize,
    pub total_requests: u64,
    last_used_at: Option<Instant>,
    pub last_resolved_model: Option<String>,
    pub quota: Option<UsageReport>,
    pub quota_error: Option<String>,
    pub quota_updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct RequestLogEntry {
    pub timestamp: DateTime<Utc>,
    pub level: LogLevel,
    pub api: String,
    pub requested_model: String,
    pub resolved_model: String,
    pub account: String,
    pub status: u16,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub context: Option<ContextUsage>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct AccountSnapshot {
    pub key: String,
    pub label: String,
    pub region: String,
    pub tenant_url: String,
    pub enabled: bool,
    pub is_primary: bool,
    pub health: AccountHealth,
    pub active_requests: usize,
    pub total_requests: u64,
    pub expires_ms: i64,
    pub last_resolved_model: Option<String>,
    pub quota: Option<UsageReport>,
    pub quota_error: Option<String>,
    pub quota_updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct PoolSnapshot {
    pub config: BridgeConfig,
    pub accounts: Vec<AccountSnapshot>,
    pub logs: Vec<RequestLogEntry>,
    pub total_requests: u64,
    pub active_requests: usize,
    pub last_used_account_key: Option<String>,
    pub last_resolved_model: Option<String>,
}

pub struct AccountPool {
    config: RwLock<BridgeConfig>,
    primary_key: RwLock<String>,
    accounts: RwLock<Vec<AccountRuntime>>,
    rr_cursor: Mutex<usize>,
    session_affinity: Mutex<HashMap<String, (String, Instant)>>,
    refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pub threads: Arc<Mutex<ThreadStore>>,
    logs: Mutex<VecDeque<RequestLogEntry>>,
    request_stats: Arc<RequestStatsStore>,
    usage_stats: Arc<UsageStore>,
    last_resolved_model: RwLock<Option<String>>,
    changes_tx: broadcast::Sender<()>,
}

pub(crate) struct ActiveRequest {
    pool: Arc<AccountPool>,
    account_key: Option<String>,
}

impl ActiveRequest {
    pub(crate) fn finish(mut self, resolved_model: &str) {
        self.complete(Some(resolved_model));
    }

    fn complete(&mut self, resolved_model: Option<&str>) {
        if let Some(key) = self.account_key.take() {
            self.pool.end_request(&key, resolved_model);
        }
    }
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.complete(None);
    }
}

impl AccountPool {
    pub fn load() -> Result<Arc<Self>> {
        let cfg = load_config()?;
        let accs = load_accounts();
        Ok(Arc::new(Self::new(cfg, accs, &home_dir())))
    }

    fn new(cfg: BridgeConfig, accs: AccountsFile, data_dir: &std::path::Path) -> Self {
        let (changes_tx, _) = broadcast::channel(64);
        let runtimes = accs
            .accounts
            .into_iter()
            .map(|r| {
                let health = if r.enabled {
                    AccountHealth::Healthy
                } else {
                    AccountHealth::Disabled
                };
                AccountRuntime {
                    record: r,
                    health,
                    active_requests: 0,
                    total_requests: 0,
                    last_used_at: None,
                    last_resolved_model: None,
                    quota: None,
                    quota_error: None,
                    quota_updated_at: None,
                }
            })
            .collect();
        Self {
            config: RwLock::new(cfg),
            primary_key: RwLock::new(accs.primary_key),
            accounts: RwLock::new(runtimes),
            rr_cursor: Mutex::new(0),
            session_affinity: Mutex::new(HashMap::new()),
            refresh_locks: Mutex::new(HashMap::new()),
            threads: Arc::new(Mutex::new(ThreadStore::load(
                data_dir.join("threads-v1.json"),
            ))),
            logs: Mutex::new(VecDeque::with_capacity(MAX_LOG_ENTRIES)),
            request_stats: Arc::new(RequestStatsStore::load(data_dir.join("request-stats.bin"))),
            usage_stats: Arc::new(UsageStore::load(data_dir.join("usage-history-v1.json.gz"))),
            last_resolved_model: RwLock::new(None),
            changes_tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changes_tx.subscribe()
    }

    fn notify(&self) {
        let _ = self.changes_tx.send(());
    }

    pub fn config(&self) -> BridgeConfig {
        self.config.read().clone()
    }

    pub fn settings(&self) -> Settings {
        Settings::from_options(&self.config.read().options)
    }

    pub fn update_config(&self, cfg: BridgeConfig) -> Result<()> {
        save_config(&cfg)?;
        *self.config.write() = cfg;
        self.notify();
        Ok(())
    }

    pub fn reload_from_disk(&self) {
        if let Ok(cfg) = load_config() {
            *self.config.write() = cfg;
        }
        self.reload_accounts_from_disk();
    }

    pub(crate) fn reload_accounts_from_disk(&self) {
        let disk = load_accounts();
        let mut accs = self.accounts.write();
        let old_map: HashMap<String, AccountRuntime> = accs
            .drain(..)
            .map(|a| (a.record.key(), a))
            .collect();
        for rec in disk.accounts {
            let key = rec.key();
            if let Some(mut prev) = old_map.get(&key).cloned() {
                if rec.expires > prev.record.expires || rec.access != prev.record.access
                    || rec.enabled != prev.record.enabled
                {
                    prev.health = if rec.enabled {
                        AccountHealth::Healthy
                    } else {
                        AccountHealth::Disabled
                    };
                }
                prev.record = rec;
                if !prev.record.enabled {
                    prev.health = AccountHealth::Disabled;
                }
                accs.push(prev);
            } else {
                let health = if rec.enabled {
                    AccountHealth::Healthy
                } else {
                    AccountHealth::Disabled
                };
                accs.push(AccountRuntime {
                    record: rec,
                    health,
                    active_requests: 0,
                    total_requests: 0,
                    last_used_at: None,
                    last_resolved_model: None,
                    quota: None,
                    quota_error: None,
                    quota_updated_at: None,
                });
            }
        }
        *self.primary_key.write() = disk.primary_key;
        self.notify();
    }

    pub fn add_or_update_account(&self, record: AccountRecord) -> Result<()> {
        let file = upsert_account(record)?;
        self.apply_accounts_file(file);
        Ok(())
    }

    pub fn delete_account(&self, key: &str) -> Result<()> {
        let file = remove_account(key)?;
        self.apply_accounts_file(file);
        Ok(())
    }

    pub fn set_primary(&self, key: &str) -> Result<()> {
        let mut file = load_accounts();
        if file.accounts.iter().any(|a| a.key() == key) {
            file.primary_key = key.to_string();
            save_accounts(&file)?;
            *self.primary_key.write() = key.to_string();
            self.notify();
        }
        Ok(())
    }

    pub fn set_enabled(&self, key: &str, enabled: bool) -> Result<()> {
        let mut file = load_accounts();
        for a in &mut file.accounts {
            if a.key() == key {
                a.enabled = enabled;
            }
        }
        save_accounts(&file)?;
        {
            let mut accs = self.accounts.write();
            if let Some(rt) = accs.iter_mut().find(|a| a.record.key() == key) {
                rt.record.enabled = enabled;
                rt.health = if enabled {
                    AccountHealth::Healthy
                } else {
                    AccountHealth::Disabled
                };
            }
        }
        self.notify();
        Ok(())
    }

    fn apply_accounts_file(&self, file: AccountsFile) {
        let mut accs = self.accounts.write();
        let old: HashMap<String, AccountRuntime> = accs
            .drain(..)
            .map(|a| (a.record.key(), a))
            .collect();
        for rec in file.accounts {
            let key = rec.key();
            if let Some(mut prev) = old.get(&key).cloned() {
                let token_changed = prev.record.access != rec.access || prev.record.expires != rec.expires;
                prev.record = rec;
                if !prev.record.enabled {
                    prev.health = AccountHealth::Disabled;
                } else if token_changed {
                    prev.health = AccountHealth::Healthy;
                }
                accs.push(prev);
            } else {
                let health = if rec.enabled {
                    AccountHealth::Healthy
                } else {
                    AccountHealth::Disabled
                };
                accs.push(AccountRuntime {
                    record: rec,
                    health,
                    active_requests: 0,
                    total_requests: 0,
                    last_used_at: None,
                    last_resolved_model: None,
                    quota: None,
                    quota_error: None,
                    quota_updated_at: None,
                });
            }
        }
        *self.primary_key.write() = file.primary_key;
        self.notify();
    }

    pub fn push_log(&self, entry: RequestLogEntry) {
        let sample = RequestSample::from_log(&entry);
        self.append_log(entry, sample);
    }

    pub(crate) fn push_gateway_log(&self, account_key: &str, entry: RequestLogEntry) {
        let sample = RequestSample::from_log(&entry);
        if let Some(sample) = sample {
            self.usage_stats.record(account_key, sample, &entry);
            self.usage_stats.schedule_save();
        }
        self.append_log(entry, sample);
    }

    fn append_log(&self, entry: RequestLogEntry, sample: Option<RequestSample>) {
        if !entry.resolved_model.is_empty() {
            *self.last_resolved_model.write() = Some(entry.resolved_model.clone());
        }
        let mut logs = self.logs.lock();
        if logs.len() >= MAX_LOG_ENTRIES {
            logs.pop_front();
        }
        logs.push_back(entry);
        if let Some(sample) = sample {
            self.request_stats.record(StatsUpdate::Completed(sample));
        }
        drop(logs);
        if sample.is_some() {
            self.request_stats.schedule_save();
        }
        self.notify();
    }

    pub fn log_info(&self, message: impl Into<String>) {
        self.push_log(RequestLogEntry {
            timestamp: Utc::now(),
            level: LogLevel::Info,
            api: "system".to_string(),
            requested_model: String::new(),
            resolved_model: String::new(),
            account: String::new(),
            status: 0,
            latency_ms: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            context: None,
            message: message.into(),
        });
    }

    pub fn clear_logs(&self) {
        {
            let mut logs = self.logs.lock();
            logs.clear();
            self.request_stats.record(StatsUpdate::Cleared(Utc::now()));
        }
        self.request_stats.schedule_save();
        self.notify();
    }

    #[cfg(feature = "gui")]
    pub(crate) fn request_samples(&self) -> Arc<Vec<RequestSample>> {
        self.request_stats.samples()
    }

    pub(crate) fn flush_request_stats(&self) {
        self.request_stats.save();
        self.usage_stats.save();
    }

    #[cfg(feature = "gui")]
    pub(crate) fn usage_history(&self) -> Arc<UsageHistory> {
        self.usage_stats.snapshot()
    }

    /// 选择下一个可用账号（优先会话亲和，其次空闲主账号或轮询负载均衡）。
    pub fn pick_account(&self, conversation: &str, exclude: &HashSet<String>) -> Option<AccountRecord> {
        self.pick_account_for_turn(conversation, exclude, None)
    }

    pub(crate) fn pick_account_for_turn(
        &self,
        conversation: &str,
        exclude: &HashSet<String>,
        thread_account: Option<&str>,
    ) -> Option<AccountRecord> {
        let now_ms = Utc::now().timestamp_millis();
        let max_conc = self.settings().max_concurrency;
        let primary = self.primary_key.read().clone();

        let mut accs = self.accounts.write();
        // 恢复已过冷却期的账号状态。
        for rt in accs.iter_mut() {
            if let AccountHealth::CoolingDown { until_epoch_ms, .. } = rt.health {
                if now_ms >= until_epoch_ms {
                    rt.health = if rt.record.enabled {
                        AccountHealth::Healthy
                    } else {
                        AccountHealth::Disabled
                    };
                }
            }
        }

        let is_eligible = |rt: &AccountRuntime, allow_over_conc: bool| -> bool {
            rt.record.enabled
                && matches!(rt.health, AccountHealth::Healthy)
                && !exclude.contains(&rt.record.key())
                && (allow_over_conc || rt.active_requests < max_conc)
        };

        if let Some(account) = thread_account {
            if let Some(rt) = accs
                .iter()
                .find(|a| is_eligible(a, false) && digest(&a.record.key()) == account)
            {
                return Some(rt.record.clone());
            }
        }

        // 1. 优先命中会话亲和绑定
        if !conversation.is_empty() {
            let mut aff = self.session_affinity.lock();
            aff.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(6 * 3600));
            if let Some((aff_key, _)) = aff.get(conversation) {
                if let Some(rt) = accs.iter().find(|a| a.record.key() == *aff_key && is_eligible(a, false)) {
                    return Some(rt.record.clone());
                }
            }
        }

        // 2. 筛选未超并发上限的候选账号，全满时退回允许超并发的可用账号
        let eligible_indices: Vec<usize> = accs
            .iter()
            .enumerate()
            .filter(|(_, rt)| is_eligible(rt, false))
            .map(|(i, _)| i)
            .collect();

        let candidates = if !eligible_indices.is_empty() {
            eligible_indices
        } else {
            accs.iter()
                .enumerate()
                .filter(|(_, rt)| is_eligible(rt, true))
                .map(|(i, _)| i)
                .collect()
        };

        if candidates.is_empty() {
            return None;
        }

        // 多账号时若主账号完全空闲则优先主账号，否则按轮询游标分发
        if candidates.len() == 1 {
            return Some(accs[candidates[0]].record.clone());
        }
        if let Some(&pi) = candidates.iter().find(|&&i| accs[i].record.key() == primary && accs[i].active_requests == 0) {
            return Some(accs[pi].record.clone());
        }
        let mut cursor = self.rr_cursor.lock();
        let chosen_idx = candidates[*cursor % candidates.len()];
        *cursor = cursor.wrapping_add(1);
        Some(accs[chosen_idx].record.clone())
    }

    pub fn bind_session(&self, conversation: &str, account_key: &str) {
        if conversation.is_empty() {
            return;
        }
        self.session_affinity
            .lock()
            .insert(conversation.to_string(), (account_key.to_string(), Instant::now()));
    }

    pub fn begin_request(&self, account_key: &str) {
        {
            let mut accs = self.accounts.write();
            if let Some(rt) = accs.iter_mut().find(|a| a.record.key() == account_key) {
                rt.active_requests += 1;
                rt.total_requests += 1;
                rt.last_used_at = Some(Instant::now());
            }
        }
        self.request_stats.record(StatsUpdate::Started);
        self.request_stats.schedule_save();
        self.notify();
    }

    pub(crate) fn track_request(self: &Arc<Self>, account_key: &str) -> ActiveRequest {
        self.begin_request(account_key);
        ActiveRequest {
            pool: self.clone(),
            account_key: Some(account_key.to_owned()),
        }
    }

    pub fn end_request(&self, account_key: &str, resolved_model: Option<&str>) {
        self.record_request_result(account_key, resolved_model, true);
    }

    #[cfg(feature = "gui")]
    pub(crate) fn record_probe_model(&self, account_key: &str, resolved_model: &str) {
        self.record_request_result(account_key, Some(resolved_model), false);
    }

    fn record_request_result(
        &self,
        account_key: &str,
        resolved_model: Option<&str>,
        finished_gateway_request: bool,
    ) {
        let resolved_model = resolved_model.filter(|model| !model.is_empty());
        {
            let mut accs = self.accounts.write();
            if let Some(rt) = accs.iter_mut().find(|a| a.record.key() == account_key) {
                if finished_gateway_request && rt.active_requests > 0 {
                    rt.active_requests -= 1;
                    rt.last_used_at = Some(Instant::now());
                }
                if let Some(m) = resolved_model {
                    rt.last_resolved_model = Some(m.to_string());
                }
            }
        }
        if let Some(m) = resolved_model {
            *self.last_resolved_model.write() = Some(m.to_string());
        }
        self.notify();
    }

    pub fn mark_cooldown(
        &self,
        account_key: &str,
        secs: Option<u64>,
        reason: impl Into<String>,
    ) -> Option<i64> {
        let rest_secs = secs.unwrap_or(DEFAULT_COOLDOWN_SECS).max(30);
        let now = Utc::now().timestamp_millis();
        let until_epoch_ms = now + (rest_secs as i64) * 1000;
        let reason = reason.into();
        {
            let mut accs = self.accounts.write();
            let rt = accs.iter_mut().find(|a| a.record.key() == account_key)?;
            if !rt.record.enabled || matches!(rt.health, AccountHealth::Expired { .. }) {
                return None;
            }
            if let AccountHealth::CoolingDown {
                until_epoch_ms: current_until,
                reason: current_reason,
            } = &rt.health
                && *current_until > now
                && (*current_reason == reason || *current_until >= until_epoch_ms)
            {
                return None;
            }
            rt.health = AccountHealth::CoolingDown {
                until_epoch_ms,
                reason,
            };
        }
        self.notify();
        Some(until_epoch_ms)
    }

    pub(crate) fn refine_quota_cooldown(self: &Arc<Self>, auth: AccountRecord, marked_until: i64) {
        let pool = self.clone();
        tokio::spawn(async move {
            let Some(spent) = qbs::spent(&auth, &pool.settings()).await else {
                return;
            };
            if spent.secs == 0 {
                return;
            }
            let key = auth.key();
            let changed = {
                let mut accounts = pool.accounts.write();
                accounts
                    .iter_mut()
                    .find(|rt| rt.record.enabled && rt.record.key() == key)
                    .and_then(|rt| match &mut rt.health {
                        AccountHealth::CoolingDown { until_epoch_ms, .. }
                            if *until_epoch_ms == marked_until =>
                        {
                            *until_epoch_ms = Utc::now().timestamp_millis()
                                + spent.secs.min(6 * 3600) as i64 * 1000;
                            Some(())
                        }
                        _ => None,
                    })
                    .is_some()
            };
            if changed {
                pool.notify();
            }
        });
    }

    pub(crate) fn next_cooldown_retry(&self) -> Option<u64> {
        let now = Utc::now().timestamp_millis();
        self.accounts
            .read()
            .iter()
            .filter(|rt| rt.record.enabled)
            .filter_map(|rt| match rt.health {
                AccountHealth::CoolingDown { until_epoch_ms, .. } if until_epoch_ms > now => {
                    Some((until_epoch_ms - now) as u64)
                }
                _ => None,
            })
            .min()
            .map(|ms| ms.div_ceil(1000))
    }

    pub fn mark_expired(&self, account_key: &str, reason: impl Into<String>) {
        let reason_str = reason.into();
        {
            let mut accs = self.accounts.write();
            if let Some(rt) = accs.iter_mut().find(|a| a.record.key() == account_key) {
                rt.health = AccountHealth::Expired {
                    reason: reason_str.clone(),
                };
            }
        }
        self.notify();
    }

    /// 确保指定账号令牌有效（按账号维度加锁去重，避免并发重复刷新）。
    pub async fn ensure_fresh(
        &self,
        account_key: &str,
        force: bool,
    ) -> std::result::Result<AccountRecord, OAuthError> {
        let current = {
            let accs = self.accounts.read();
            accs.iter()
                .find(|a| a.record.key() == account_key)
                .map(|a| a.record.clone())
        }
        .ok_or_else(|| OAuthError::Expired("account not found".into()))?;

        let now_ms = Utc::now().timestamp_millis();
        if !force && (current.expires == 0 || current.expires - now_ms > REFRESH_LEAD_MS) {
            return Ok(current);
        }

        let lock = {
            let mut map = self.refresh_locks.lock();
            map.entry(account_key.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        // 获取锁后二次检查，防止等待期间已被其他并发请求刷新。
        let latest = {
            let accs = self.accounts.read();
            accs.iter()
                .find(|a| a.record.key() == account_key)
                .map(|a| a.record.clone())
        }
        .ok_or_else(|| OAuthError::Expired("account not found".into()))?;

        let now_ms = Utc::now().timestamp_millis();
        if !force && (latest.expires == 0 || latest.expires - now_ms > REFRESH_LEAD_MS) {
            return Ok(latest);
        }

        let cfg = self.settings();
        match refresh_account(&latest, &cfg).await {
            Ok(refreshed) => {
                let _ = self.add_or_update_account(refreshed.clone());
                Ok(refreshed)
            }
            Err(e) => {
                if e.is_expired() {
                    self.mark_expired(account_key, e.to_string());
                    Err(e)
                } else if !force && latest.expires > now_ms {
                    // 临时刷新异常但旧令牌尚未过期，继续复用当前令牌。
                    Ok(latest)
                } else {
                    Err(e)
                }
            }
        }
    }

    pub async fn refresh_quota(&self, account_key: &str) -> Result<UsageReport> {
        let auth = self
            .ensure_fresh(account_key, false)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let cfg = self.settings();
        match qbs::usage(&auth, &cfg).await {
            Ok(report) => {
                {
                    let mut accs = self.accounts.write();
                    if let Some(rt) = accs.iter_mut().find(|a| a.record.key() == account_key) {
                        rt.quota = Some(report.clone());
                        rt.quota_error = None;
                        rt.quota_updated_at = Some(Utc::now());
                    }
                }
                self.notify();
                Ok(report)
            }
            Err(e) => {
                {
                    let mut accs = self.accounts.write();
                    if let Some(rt) = accs.iter_mut().find(|a| a.record.key() == account_key) {
                        rt.quota_error = Some(e.message.clone());
                    }
                }
                self.notify();
                Err(anyhow::anyhow!("{}", e.message))
            }
        }
    }

    pub async fn refresh_all_quotas(&self) {
        let keys: Vec<String> = self
            .accounts
            .read()
            .iter()
            .filter(|a| a.record.enabled)
            .map(|a| a.record.key())
            .collect();
        for k in keys {
            let _ = self.refresh_quota(&k).await;
        }
    }

    pub fn snapshot(&self) -> PoolSnapshot {
        let primary = self.primary_key.read().clone();
        let accs = self.accounts.read();
        let active_total: usize = accs.iter().map(|a| a.active_requests).sum();
        let last_used_account_key = accs
            .iter()
            .filter_map(|account| account.last_used_at.map(|at| (at, &account.record)))
            .max_by_key(|(at, _)| *at)
            .map(|(_, record)| record.key());
        let account_snaps: Vec<AccountSnapshot> = accs
            .iter()
            .map(|a| {
                let key = a.record.key();
                AccountSnapshot {
                    is_primary: key == primary,
                    key,
                    label: a.record.display_label(),
                    region: a.record.region.clone(),
                    tenant_url: a.record.tenant_url.clone(),
                    enabled: a.record.enabled,
                    health: a.health.clone(),
                    active_requests: a.active_requests,
                    total_requests: a.total_requests,
                    expires_ms: a.record.expires,
                    last_resolved_model: a.last_resolved_model.clone(),
                    quota: a.quota.clone(),
                    quota_error: a.quota_error.clone(),
                    quota_updated_at: a.quota_updated_at,
                }
            })
            .collect();
        PoolSnapshot {
            config: self.config.read().clone(),
            accounts: account_snaps,
            logs: self.logs.lock().iter().cloned().collect(),
            total_requests: self.request_stats.total_requests(),
            active_requests: active_total,
            last_used_account_key,
            last_resolved_model: self.last_resolved_model.read().clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(feature = "gui")]
    #[test]
    fn usage_counts_gateway_attempts_and_survives_log_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let make_pool = || AccountPool::new(BridgeConfig::default(), AccountsFile::default(), dir.path());
        let pool = make_pool();
        let failure = RequestLogEntry {
            timestamp: Utc::now(),
            level: LogLevel::Warn,
            api: "messages".into(),
            requested_model: "smart".into(),
            resolved_model: String::new(),
            account: "A".into(),
            status: 429,
            latency_ms: 13,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            context: None,
            message: "fixture".into(),
        };
        let completed = RequestLogEntry {
            level: LogLevel::Info,
            resolved_model: "resolved-model".into(),
            account: "B".into(),
            status: 200,
            input_tokens: 150,
            output_tokens: 25,
            cached_tokens: 65,
            context: Some(ContextUsage {
                used_tokens: Some(150),
                window_tokens: std::num::NonZeroU64::new(262_144),
            }),
            ..failure.clone()
        };
        pool.push_gateway_log("account-a", failure);
        pool.push_gateway_log("account-b", completed.clone());
        pool.push_log(completed);
        pool.clear_logs();
        pool.flush_request_stats();
        drop(pool);

        let history = make_pool().usage_history();
        assert_eq!(history.buckets.len(), 2);
        assert_eq!(history.account_label("account-b"), Some("B"));
        assert_eq!(history.contexts.len(), 2);
        let (_, unreported) = history.contexts.first_key_value().unwrap();
        assert_eq!(unreported.len(), 1);
        assert_eq!(unreported[0].usage, None);
        let (bucket, contexts) = history.contexts.last_key_value().unwrap();
        assert_eq!(bucket.account, "account-b");
        assert_eq!(bucket.model, "resolved-model");
        assert_eq!(bucket.protocol, RequestProtocol::Messages);
        assert_eq!(contexts.len(), 1);
        let usage = contexts[0].usage.unwrap();
        assert_eq!(usage.used_tokens, Some(150));
        assert_eq!(usage.window_tokens.map(|value| value.get()), Some(262_144));
        let mut totals = UsageTotals::default();
        for (bucket, usage) in &history.buckets {
            totals.add(*usage);
            if bucket.account == "account-a" {
                assert_eq!(bucket.model, "");
            } else {
                assert_eq!(bucket.model, "resolved-model");
            }
        }
        assert_eq!(totals.calls, 2);
        assert_eq!(totals.tokens(), 175);
        assert_eq!(totals.cache_read, 65);
    }

    #[test]
    fn tracks_live_and_recent_accounts_across_failover_and_parallel_requests() {
        let dir = tempfile::tempdir().unwrap();
        let record = |id| -> AccountRecord {
            serde_json::from_value(json!({
                "accountId": id,
                "tenantUrl": "https://example.test",
                "access": "fixture-token",
            }))
            .unwrap()
        };
        let (a, b) = (record("a"), record("b"));
        let (a_key, b_key) = (a.key(), b.key());
        let pool = Arc::new(AccountPool::new(
            BridgeConfig::default(),
            AccountsFile {
                primary_key: a_key.clone(),
                accounts: vec![a, b],
            },
            dir.path(),
        ));
        assert_eq!(pool.snapshot().last_used_account_key, None);

        let request_a = pool.track_request(&a_key);
        let request_b = pool.track_request(&b_key);
        let snapshot = pool.snapshot();
        assert_eq!(snapshot.accounts[0].active_requests, 1);
        assert_eq!(snapshot.accounts[1].active_requests, 1);
        assert_eq!(snapshot.active_requests, 2);

        drop(request_a);
        pool.mark_cooldown(&a_key, None, "fixture quota exhausted");
        let picked = pool.pick_account("conversation", &HashSet::new()).unwrap();
        assert_eq!(picked.key(), b_key);
        let retry = pool.track_request(&picked.key());

        #[cfg(feature = "gui")]
        pool.record_probe_model(&b_key, "probe-model");
        assert_eq!(pool.snapshot().accounts[1].active_requests, 2);

        request_b.finish("resolved-model");
        let snapshot = pool.snapshot();
        assert_eq!(snapshot.accounts[0].active_requests, 0);
        assert_eq!(snapshot.accounts[1].active_requests, 1);
        assert_eq!(snapshot.active_requests, 1);
        retry.finish("");

        #[cfg(feature = "gui")]
        pool.record_probe_model(&a_key, "another-probe-model");
        let snapshot = pool.snapshot();
        assert_eq!(snapshot.active_requests, 0);
        assert_eq!(
            snapshot.last_used_account_key.as_deref(),
            Some(b_key.as_str())
        );
        assert!(snapshot.accounts[0].is_primary);
        assert_eq!(
            snapshot.accounts[1].last_resolved_model.as_deref(),
            Some("resolved-model")
        );
    }
}
