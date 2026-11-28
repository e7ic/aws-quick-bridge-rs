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
use serde::{Deserialize, Serialize};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use usage_stats::UsageStore;
#[cfg(feature = "gui")]
pub(crate) use usage_stats::{
    ContextSample, USAGE_RETENTION_DAYS, UsageBucket, UsageHistory, UsageTotals,
};

const MAX_LOG_ENTRIES: usize = 300;
const DEFAULT_COOLDOWN_SECS: u64 = 120;
const MAX_SESSION_BINDINGS: usize = 1_000;
const SESSION_BINDING_TTL: Duration = Duration::from_secs(6 * 3600);
pub(crate) const PREFERRED_SLOT_WAIT: Duration = Duration::from_millis(500);
pub(crate) const BUSY_SLOT_WAIT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountHealth {
    Healthy,
    CoolingDown { until_epoch_ms: i64, reason: String },
    Expired { reason: String },
    Disabled,
}

#[derive(Debug, Clone)]
pub struct AccountRuntime {
    pub record: AccountRecord,
    pub health: AccountHealth,
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

struct AccountsState {
    records: Vec<AccountRuntime>,
    // Keep slots owned by their requests across credential refreshes and account reloads.
    in_flight: HashMap<String, usize>,
    rr_cursor: usize,
}

pub struct AccountPool {
    config: RwLock<BridgeConfig>,
    primary_key: RwLock<String>,
    accounts: RwLock<AccountsState>,
    session_affinity: Mutex<HashMap<String, (String, Instant)>>,
    refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pub threads: Arc<Mutex<ThreadStore>>,
    logs: Mutex<VecDeque<RequestLogEntry>>,
    request_stats: Arc<RequestStatsStore>,
    usage_stats: Arc<UsageStore>,
    last_resolved_model: RwLock<Option<String>>,
    changes_tx: broadcast::Sender<()>,
}

#[derive(Debug)]
pub(crate) enum NoAvailableAccount {
    Busy,
    Unavailable,
}

enum AccountSelectionError {
    PreferredBusy,
    NoAccount(NoAvailableAccount),
}

impl From<AccountSelectionError> for NoAvailableAccount {
    fn from(error: AccountSelectionError) -> Self {
        match error {
            AccountSelectionError::PreferredBusy => Self::Busy,
            AccountSelectionError::NoAccount(reason) => reason,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AccountSelection {
    Lookup,
    Reserve,
    WaitForPreferred,
}

pub(crate) struct ActiveRequest {
    pool: Arc<AccountPool>,
    account_key: Option<String>,
    started: bool,
}

impl ActiveRequest {
    pub(crate) fn start(&mut self) {
        if let Some(key) = self.account_key.as_deref()
            && !self.started
        {
            self.pool.record_request_start(key);
            self.started = true;
        }
    }

    pub(crate) fn finish(mut self, resolved_model: &str) {
        self.complete(Some(resolved_model));
    }

    fn complete(&mut self, resolved_model: Option<&str>) {
        if let Some(key) = self.account_key.take() {
            self.pool.release_request(key, self.started, resolved_model);
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
            accounts: RwLock::new(AccountsState {
                records: runtimes,
                in_flight: HashMap::new(),
                rr_cursor: 0,
            }),
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

    pub(crate) fn enabled_account_count(&self) -> usize {
        self.accounts
            .read()
            .records
            .iter()
            .filter(|account| account.record.enabled)
            .count()
    }

    pub(crate) fn claude_desktop_1m_modes(&self) -> HashSet<&'static str> {
        let accounts = self
            .accounts
            .read()
            .records
            .iter()
            .filter(|account| account.record.enabled)
            .map(|account| account.record.key())
            .collect();
        self.usage_stats.snapshot().confirmed_1m_modes(&accounts)
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
        let accs = &mut self.accounts.write().records;
        let old_map: HashMap<String, AccountRuntime> =
            accs.drain(..).map(|a| (a.record.key(), a)).collect();
        for rec in disk.accounts {
            let key = rec.key();
            if let Some(mut prev) = old_map.get(&key).cloned() {
                if rec.expires > prev.record.expires
                    || rec.access != prev.record.access
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
            if let Some(rt) = accs.records.iter_mut().find(|a| a.record.key() == key) {
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
        let accs = &mut self.accounts.write().records;
        let old: HashMap<String, AccountRuntime> =
            accs.drain(..).map(|a| (a.record.key(), a)).collect();
        for rec in file.accounts {
            let key = rec.key();
            if let Some(mut prev) = old.get(&key).cloned() {
                let token_changed =
                    prev.record.access != rec.access || prev.record.expires != rec.expires;
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

    pub(crate) fn push_gateway_log(&self, account_key: &str, mode: &str, entry: RequestLogEntry) {
        let sample = RequestSample::from_log(&entry);
        if let Some(sample) = sample {
            self.usage_stats.record(account_key, mode, sample, &entry);
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

    #[cfg(all(feature = "gui", any(target_os = "macos", target_os = "windows")))]
    pub(crate) fn requests_idle(&self) -> bool {
        self.accounts.read().in_flight.is_empty()
    }

    #[cfg(all(feature = "gui", any(target_os = "macos", target_os = "windows")))]
    pub(crate) async fn wait_for_request_idle(&self) {
        let mut events = self.subscribe();
        while !self.requests_idle() {
            let _ = events.recv().await;
        }
    }

    #[cfg(feature = "gui")]
    pub(crate) fn usage_history(&self) -> Arc<UsageHistory> {
        self.usage_stats.snapshot()
    }

    /// 查找可处理元数据等短操作的账号，对话使用 reserve_conversation 占用名额。
    pub fn pick_account(
        &self,
        conversation: &str,
        exclude: &HashSet<String>,
    ) -> Option<AccountRecord> {
        self.select_account(conversation, exclude, None, AccountSelection::Lookup)
            .ok()
    }

    pub(crate) fn reserve_request(
        self: &Arc<Self>,
        conversation: &str,
        exclude: &HashSet<String>,
        thread_account: Option<&str>,
    ) -> Result<(AccountRecord, ActiveRequest), NoAvailableAccount> {
        self.try_reserve_request(
            conversation,
            exclude,
            thread_account,
            AccountSelection::Reserve,
        )
        .map_err(NoAvailableAccount::from)
    }

    pub(crate) async fn reserve_conversation(
        self: &Arc<Self>,
        conversation: &str,
        exclude: &HashSet<String>,
        thread_account: Option<&str>,
        preferred_deadline: tokio::time::Instant,
        busy_deadline: tokio::time::Instant,
    ) -> Result<(AccountRecord, ActiveRequest), NoAvailableAccount> {
        // Subscribe before checking capacity so a simultaneous release cannot be missed.
        let mut changes = self.subscribe();
        loop {
            let now = tokio::time::Instant::now();
            let selection = if (thread_account.is_some() || !conversation.is_empty())
                && now < preferred_deadline
            {
                AccountSelection::WaitForPreferred
            } else {
                AccountSelection::Reserve
            };
            let deadline =
                match self.try_reserve_request(conversation, exclude, thread_account, selection) {
                    Err(AccountSelectionError::PreferredBusy) => preferred_deadline,
                    Err(AccountSelectionError::NoAccount(NoAvailableAccount::Busy))
                        if now < busy_deadline =>
                    {
                        busy_deadline
                    }
                    result => return result.map_err(NoAvailableAccount::from),
                };
            if matches!(
                tokio::time::timeout_at(deadline, changes.recv()).await,
                Ok(Err(broadcast::error::RecvError::Closed))
            ) {
                return self.reserve_request(conversation, exclude, thread_account);
            }
        }
    }

    fn try_reserve_request(
        self: &Arc<Self>,
        conversation: &str,
        exclude: &HashSet<String>,
        thread_account: Option<&str>,
        selection: AccountSelection,
    ) -> Result<(AccountRecord, ActiveRequest), AccountSelectionError> {
        let record = self.select_account(conversation, exclude, thread_account, selection)?;
        let request = ActiveRequest {
            pool: self.clone(),
            account_key: Some(record.key()),
            started: false,
        };
        self.notify();
        Ok((record, request))
    }

    fn select_account(
        &self,
        conversation: &str,
        exclude: &HashSet<String>,
        thread_account: Option<&str>,
        selection: AccountSelection,
    ) -> Result<AccountRecord, AccountSelectionError> {
        let now_ms = Utc::now().timestamp_millis();
        let max_conc = self.settings().max_concurrency;
        let primary = self.primary_key.read().clone();
        let bound = self.session_account(conversation, Instant::now());

        let AccountsState {
            records,
            in_flight,
            rr_cursor,
        } = &mut *self.accounts.write();
        for runtime in records.iter_mut() {
            if let AccountHealth::CoolingDown { until_epoch_ms, .. } = runtime.health
                && now_ms >= until_epoch_ms
            {
                runtime.health = if runtime.record.enabled {
                    AccountHealth::Healthy
                } else {
                    AccountHealth::Disabled
                };
            }
        }

        let mut candidates: Vec<_> = records
            .iter()
            .filter(|runtime| runtime.record.enabled && runtime.health == AccountHealth::Healthy)
            .filter_map(|runtime| {
                let key = runtime.record.key();
                if exclude.contains(&key) {
                    return None;
                }
                let active = in_flight.get(&key).copied().unwrap_or_default();
                Some((runtime, key, active))
            })
            .collect();
        if candidates.is_empty() {
            return Err(AccountSelectionError::NoAccount(
                NoAvailableAccount::Unavailable,
            ));
        }

        let matches_thread = |(_, key, _): &(_, String, _)| {
            thread_account.is_some_and(|account| digest(key) == account)
        };
        let matches_bound = |(_, key, _): &(_, String, _)| bound.as_ref() == Some(key);
        if selection == AccountSelection::WaitForPreferred
            && candidates
                .iter()
                .find(|candidate| matches_thread(candidate))
                .or_else(|| candidates.iter().find(|candidate| matches_bound(candidate)))
                .is_some_and(|(_, _, active)| *active >= max_conc)
        {
            return Err(AccountSelectionError::PreferredBusy);
        }

        let reserve = selection != AccountSelection::Lookup;
        let has_capacity = |(_, _, active): &(_, _, usize)| *active < max_conc;
        if reserve || candidates.iter().any(has_capacity) {
            candidates.retain(has_capacity);
        }
        if candidates.is_empty() {
            return Err(AccountSelectionError::NoAccount(NoAvailableAccount::Busy));
        }

        // 优先保留原线程或会话绑定，然后选择空闲主账号或按轮询分配。
        let (runtime, key, _) = candidates
            .iter()
            .find(|candidate| matches_thread(candidate))
            .or_else(|| {
                candidates
                    .iter()
                    .find(|candidate| has_capacity(candidate) && matches_bound(candidate))
            })
            .or_else(|| {
                candidates
                    .iter()
                    .find(|(_, key, active)| *active == 0 && *key == primary)
            })
            .unwrap_or_else(|| {
                if candidates.len() == 1 {
                    return &candidates[0];
                }
                let chosen = &candidates[*rr_cursor % candidates.len()];
                *rr_cursor = rr_cursor.wrapping_add(1);
                chosen
            });

        if reserve {
            // 与选择账号共用一把锁，名额先于异步续期和上游调用保留。
            *in_flight.entry(key.clone()).or_default() += 1;
        }
        Ok(runtime.record.clone())
    }

    pub fn bind_session(&self, conversation: &str, account_key: &str) {
        if conversation.is_empty() {
            return;
        }
        let conversation = digest(conversation);
        let mut affinity = self.session_affinity.lock();
        if affinity.len() >= MAX_SESSION_BINDINGS
            && !affinity.contains_key(&conversation)
            && let Some(oldest) = affinity
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(key, _)| key.clone())
        {
            affinity.remove(&oldest);
        }
        affinity.insert(conversation, (account_key.to_owned(), Instant::now()));
    }

    fn session_account(&self, conversation: &str, now: Instant) -> Option<String> {
        if conversation.is_empty() {
            return None;
        }
        let conversation = digest(conversation);
        self.session_affinity
            .lock()
            .get(&conversation)
            .filter(|(_, at)| now.duration_since(*at) < SESSION_BINDING_TTL)
            .map(|(key, _)| key.clone())
    }

    fn record_request_start(&self, account_key: &str) {
        {
            let mut accs = self.accounts.write();
            if let Some(rt) = accs
                .records
                .iter_mut()
                .find(|a| a.record.key() == account_key)
            {
                rt.total_requests += 1;
                rt.last_used_at = Some(Instant::now());
            }
        }
        self.request_stats.record(StatsUpdate::Started);
        self.request_stats.schedule_save();
        self.notify();
    }

    fn release_request(&self, account_key: String, started: bool, resolved_model: Option<&str>) {
        let resolved_model = resolved_model.filter(|model| !model.is_empty());
        {
            let mut accs = self.accounts.write();
            if (started || resolved_model.is_some())
                && let Some(rt) = accs
                    .records
                    .iter_mut()
                    .find(|a| a.record.key() == account_key)
            {
                if started {
                    rt.last_used_at = Some(Instant::now());
                }
                if let Some(m) = resolved_model {
                    rt.last_resolved_model = Some(m.to_string());
                }
            }
            if let Entry::Occupied(mut slots) = accs.in_flight.entry(account_key) {
                if *slots.get() == 1 {
                    slots.remove();
                } else {
                    *slots.get_mut() -= 1;
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
            let rt = accs
                .records
                .iter_mut()
                .find(|a| a.record.key() == account_key)?;
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
                    .records
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
            .records
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
        self.mark_expired_if_current(account_key, None, reason.into());
    }

    pub(crate) fn mark_credentials_expired(
        &self,
        attempted: &AccountRecord,
        reason: impl Into<String>,
    ) {
        self.mark_expired_if_current(&attempted.key(), Some(attempted), reason.into());
    }

    fn mark_expired_if_current(
        &self,
        account_key: &str,
        attempted: Option<&AccountRecord>,
        reason: String,
    ) {
        let mut accounts = self.accounts.write();
        let Some(account) = accounts
            .records
            .iter_mut()
            .find(|account| account.record.key() == account_key)
        else {
            return;
        };
        // A refresh or a new sign-in may have replaced the rejected credential.
        let current = &account.record;
        if let Some(attempted) = attempted
            && (!current.enabled
                || current.access != attempted.access
                || current.id_token != attempted.id_token
                || current.refresh != attempted.refresh
                || current.expires != attempted.expires)
        {
            return;
        }
        account.health = AccountHealth::Expired { reason };
        drop(accounts);
        self.notify();
    }

    fn refreshable_account(&self, account_key: &str) -> Result<AccountRecord, OAuthError> {
        let accounts = self.accounts.read();
        let account = accounts
            .records
            .iter()
            .find(|account| account.record.key() == account_key)
            .ok_or_else(|| OAuthError::Expired("account not found".into()))?;
        if let AccountHealth::Expired { reason } = &account.health {
            return Err(OAuthError::Expired(reason.clone()));
        }
        Ok(account.record.clone())
    }

    /// 确保指定账号令牌有效（按账号维度加锁去重，避免并发重复刷新）。
    pub async fn ensure_fresh(
        &self,
        account_key: &str,
        force: bool,
    ) -> std::result::Result<AccountRecord, OAuthError> {
        let current = self.refreshable_account(account_key)?;

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

        // 等待期间，其他请求可能已经刷新或确认凭证失效。
        let latest = self.refreshable_account(account_key)?;

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
                    self.mark_credentials_expired(&latest, e.to_string());
                    Err(e)
                } else if !force && latest.expires > Utc::now().timestamp_millis() {
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
                    if let Some(rt) = accs
                        .records
                        .iter_mut()
                        .find(|a| a.record.key() == account_key)
                    {
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
                    if let Some(rt) = accs
                        .records
                        .iter_mut()
                        .find(|a| a.record.key() == account_key)
                    {
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
            .records
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
        let last_used_account_key = accs
            .records
            .iter()
            .filter_map(|account| account.last_used_at.map(|at| (at, &account.record)))
            .max_by_key(|(at, _)| *at)
            .map(|(_, record)| record.key());
        let account_snaps: Vec<AccountSnapshot> = accs
            .records
            .iter()
            .map(|a| {
                let key = a.record.key();
                let active_requests = accs.in_flight.get(&key).copied().unwrap_or_default();
                AccountSnapshot {
                    is_primary: key == primary,
                    key,
                    label: a.record.display_label(),
                    region: a.record.region.clone(),
                    tenant_url: a.record.tenant_url.clone(),
                    enabled: a.record.enabled,
                    health: a.health.clone(),
                    active_requests,
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
            active_requests: account_snaps.iter().map(|a| a.active_requests).sum(),
            accounts: account_snaps,
            logs: self.logs.lock().iter().cloned().collect(),
            total_requests: self.request_stats.total_requests(),
            last_used_account_key,
            last_resolved_model: self.last_resolved_model.read().clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_bindings_expire_and_evict_the_oldest_at_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let pool = AccountPool::new(BridgeConfig::default(), AccountsFile::default(), dir.path());
        pool.bind_session("expired", "account-a");
        let inserted = pool.session_affinity.lock()[&digest("expired")].1;
        assert_eq!(
            pool.session_account("expired", inserted + SESSION_BINDING_TTL),
            None
        );

        for index in 1..MAX_SESSION_BINDINGS {
            pool.bind_session(&format!("session-{index}"), "account-a");
        }
        pool.bind_session("session-1", "account-b");
        assert_eq!(pool.session_affinity.lock().len(), MAX_SESSION_BINDINGS);
        let long_id = "long-session".repeat(1_000);
        pool.bind_session(&long_id, "account-c");

        let bindings = pool.session_affinity.lock();
        assert_eq!(bindings.len(), MAX_SESSION_BINDINGS);
        assert!(!bindings.contains_key(&digest("expired")));
        assert!(bindings.keys().all(|key| key.len() == 24));
        drop(bindings);
        let now = Instant::now();
        assert_eq!(
            pool.session_account("session-1", now).as_deref(),
            Some("account-b")
        );
        assert_eq!(
            pool.session_account(&long_id, now).as_deref(),
            Some("account-c")
        );
    }

    #[test]
    fn a_late_expiry_does_not_overwrite_a_new_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let stale: AccountRecord = serde_json::from_value(json!({
            "accountId":"fixture", "tenantUrl":"https://example.test", "access":"old-fixture"
        }))
        .unwrap();
        let mut current = stale.clone();
        current.access = "new-fixture".into();
        let pool = AccountPool::new(
            BridgeConfig::default(),
            AccountsFile {
                primary_key: current.key(),
                accounts: vec![current.clone()],
            },
            dir.path(),
        );
        pool.mark_credentials_expired(&stale, "old refresh was revoked");
        assert_eq!(pool.snapshot().accounts[0].health, AccountHealth::Healthy);
        pool.mark_credentials_expired(&current, "current refresh was revoked");
        assert!(matches!(
            pool.snapshot().accounts[0].health,
            AccountHealth::Expired { .. }
        ));
    }

    #[cfg(feature = "gui")]
    #[test]
    fn usage_counts_gateway_attempts_and_survives_log_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let make_pool =
            || AccountPool::new(BridgeConfig::default(), AccountsFile::default(), dir.path());
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
        pool.push_gateway_log("account-a", "smart", failure);
        pool.push_gateway_log("account-b", "smart", completed.clone());
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

    #[cfg(all(feature = "gui", any(target_os = "macos", target_os = "windows")))]
    #[tokio::test]
    async fn updater_waits_until_the_last_reserved_request_has_released() {
        let directory = tempfile::tempdir().unwrap();
        let account: AccountRecord = serde_json::from_value(json!({
            "accountId": "fixture",
            "tenantUrl": "https://example.test",
            "access": "fixture-token"
        }))
        .unwrap();
        let pool = Arc::new(AccountPool::new(
            BridgeConfig::default(),
            AccountsFile {
                primary_key: account.key(),
                accounts: vec![account],
            },
            directory.path(),
        ));
        let (_, request) = pool.reserve_request("turn", &HashSet::new(), None).unwrap();
        assert!(!pool.requests_idle());
        let wait = pool.wait_for_request_idle();
        tokio::pin!(wait);
        assert!(futures_util::poll!(&mut wait).is_pending());
        drop(request);
        tokio::time::timeout(Duration::from_secs(1), wait)
            .await
            .unwrap();
        assert!(pool.requests_idle());
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
        let mut config = BridgeConfig::default();
        config.options.insert("max_concurrency".into(), json!(4));
        let pool = Arc::new(AccountPool::new(
            config,
            AccountsFile {
                primary_key: a_key.clone(),
                accounts: vec![a, b],
            },
            dir.path(),
        ));
        assert_eq!(pool.settings().max_concurrency, 4);
        assert_eq!(pool.snapshot().last_used_account_key, None);

        let reserve = |preferred: &str| {
            pool.reserve_request("conversation", &HashSet::new(), Some(&digest(preferred)))
                .unwrap()
        };
        let (_, mut request_a) = reserve(&a_key);
        request_a.start();
        let (_, mut request_b) = reserve(&b_key);
        request_b.start();
        let snapshot = pool.snapshot();
        assert_eq!(snapshot.accounts[0].active_requests, 1);
        assert_eq!(snapshot.accounts[1].active_requests, 1);
        assert_eq!(snapshot.active_requests, 2);

        drop(request_a);
        pool.mark_cooldown(&a_key, None, "fixture quota exhausted");
        let (picked, mut retry) = reserve(&a_key);
        assert_eq!(picked.key(), b_key);
        retry.start();

        let (_, probe) = reserve(&b_key);
        probe.finish("probe-model");
        assert_eq!(pool.snapshot().accounts[1].active_requests, 2);

        request_b.finish("resolved-model");
        let snapshot = pool.snapshot();
        assert_eq!(snapshot.accounts[0].active_requests, 0);
        assert_eq!(snapshot.accounts[1].active_requests, 1);
        assert_eq!(snapshot.active_requests, 1);
        retry.finish("");

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

    #[test]
    fn an_unstarted_reservation_survives_account_reload_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let record: AccountRecord = serde_json::from_value(json!({
            "accountId": "fixture",
            "tenantUrl": "https://example.test",
            "access": "fixture-token",
        }))
        .unwrap();
        let accounts = AccountsFile {
            primary_key: record.key(),
            accounts: vec![record],
        };
        let mut config = BridgeConfig::default();
        config.options.insert("max_concurrency".into(), json!(1));
        let pool = Arc::new(AccountPool::new(config, accounts.clone(), dir.path()));
        let reserve = || pool.reserve_request("", &HashSet::new(), None);

        let (_, before_refresh) = reserve().unwrap();
        assert!(matches!(reserve(), Err(NoAvailableAccount::Busy)));
        assert_eq!(pool.snapshot().total_requests, 0);

        pool.apply_accounts_file(AccountsFile::default());
        pool.apply_accounts_file(accounts);
        assert!(matches!(reserve(), Err(NoAvailableAccount::Busy)));
        assert_eq!(pool.snapshot().active_requests, 1);

        drop(before_refresh);
        assert_eq!(pool.snapshot().active_requests, 0);
        let (_, mut retry) = reserve().unwrap();
        retry.start();
        retry.finish("resolved-model");
        let snapshot = pool.snapshot();
        assert_eq!(snapshot.total_requests, 1);
        assert_eq!(snapshot.active_requests, 0);
    }
}
