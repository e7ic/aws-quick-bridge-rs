use super::RequestLogEntry;
use super::request_stats::{RequestProtocol, RequestSample};
use crate::config::{CLAUDE_ROLE_MODES, write_secure_file};
use crate::upstream::ContextUsage;
use anyhow::{Context, Result};
use chrono::{DateTime, Days, Local, NaiveDate, Timelike, Utc};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use parking_lot::Mutex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(crate) const USAGE_RETENTION_DAYS: u64 = 30;
const MILLION_TOKENS: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UsageTotals {
    pub(crate) calls: u64,
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
}

impl UsageTotals {
    #[cfg(feature = "gui")]
    pub(crate) fn tokens(self) -> u64 {
        self.input.saturating_add(self.output)
    }

    pub(crate) fn add(&mut self, other: Self) {
        self.calls = self.calls.saturating_add(other.calls);
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct UsageBucket {
    pub(crate) date: NaiveDate,
    pub(crate) hour: Option<u32>,
    pub(crate) account: String,
    pub(crate) model: String,
    pub(crate) protocol: RequestProtocol,
}

#[derive(Clone, Serialize, Deserialize)]
struct AccountLabel {
    name: String,
    seen_at: DateTime<Utc>,
}

type UsageBuckets = BTreeMap<UsageBucket, UsageTotals>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContextSample {
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub(crate) timestamp: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) usage: Option<ContextUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) quick_mode: Option<String>,
}

type ContextBuckets = BTreeMap<UsageBucket, Vec<ContextSample>>;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct UsageHistory {
    pub(crate) started_on: Option<NaiveDate>,
    accounts: BTreeMap<String, AccountLabel>,
    #[serde(
        serialize_with = "serialize_entries",
        deserialize_with = "deserialize_entries"
    )]
    pub(crate) buckets: UsageBuckets,
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        serialize_with = "serialize_entries",
        deserialize_with = "deserialize_entries"
    )]
    pub(crate) contexts: ContextBuckets,
}

impl UsageHistory {
    pub(super) fn confirmed_1m_modes(&self, accounts: &HashSet<String>) -> HashSet<&'static str> {
        if accounts.is_empty() {
            return HashSet::new();
        }
        let oldest = Local::now().date_naive() - Days::new(USAGE_RETENTION_DAYS - 1);
        let mut latest = BTreeMap::new();
        for (bucket, samples) in &self.contexts {
            if bucket.date < oldest || !accounts.contains(&bucket.account) {
                continue;
            }
            for sample in samples {
                if let Some(mode) = sample.quick_mode.as_deref()
                    && let Some(window) = sample.usage.and_then(|usage| usage.window_tokens)
                {
                    // A mode can serve more than one model. Keep each model's newest report.
                    let report = (Reverse(sample.timestamp), window.get());
                    let newest = latest
                        .entry((bucket.account.as_str(), mode, bucket.model.as_str()))
                        .or_insert(report);
                    *newest = (*newest).min(report);
                }
            }
        }

        let mut minimums = BTreeMap::new();
        for ((account, mode, _), (_, window)) in latest {
            let minimum = minimums.entry((account, mode)).or_insert(window);
            *minimum = (*minimum).min(window);
        }
        CLAUDE_ROLE_MODES
            .iter()
            .filter_map(|&(_, mode)| {
                accounts
                    .iter()
                    .all(|account| {
                        minimums
                            .get(&(account.as_str(), mode))
                            .is_some_and(|&window| window >= MILLION_TOKENS)
                    })
                    .then_some(mode)
            })
            .collect()
    }

    #[cfg(feature = "gui")]
    pub(crate) fn account_label(&self, key: &str) -> Option<&str> {
        self.accounts.get(key).map(|label| label.name.as_str())
    }

    fn update_label(&mut self, key: &str, label: &AccountLabel) {
        if self
            .accounts
            .get(key)
            .is_none_or(|old| old.seen_at < label.seen_at)
        {
            self.accounts.insert(key.to_owned(), label.clone());
        }
    }

    fn record(
        &mut self,
        bucket: UsageBucket,
        label: &AccountLabel,
        totals: UsageTotals,
        context: Option<ContextSample>,
    ) {
        self.started_on = Some(
            self.started_on
                .map_or(bucket.date, |date| date.min(bucket.date)),
        );
        self.update_label(&bucket.account, label);
        if let Some(context) = context {
            // Share account, model and protocol across all samples in one day.
            let key = UsageBucket {
                hour: None,
                ..bucket.clone()
            };
            let samples = self.contexts.entry(key).or_default();
            let index = samples.partition_point(|sample| sample.timestamp <= context.timestamp);
            samples.insert(index, context);
        }
        self.buckets.entry(bucket).or_default().add(totals);
    }

    fn merge(&mut self, delta: &Self) {
        if let Some(date) = delta.started_on {
            self.started_on = Some(self.started_on.map_or(date, |old| old.min(date)));
        }
        for (key, label) in &delta.accounts {
            self.update_label(key, label);
        }
        for (bucket, totals) in &delta.buckets {
            self.buckets.entry(bucket.clone()).or_default().add(*totals);
        }
        for (bucket, samples) in &delta.contexts {
            let target = self.contexts.entry(bucket.clone()).or_default();
            target.extend_from_slice(samples);
            target.sort_unstable_by_key(|sample| sample.timestamp);
        }
    }

    fn compact(&mut self, today: NaiveDate) {
        let oldest = today - Days::new(USAGE_RETENTION_DAYS - 1);
        self.contexts.retain(|bucket, _| bucket.date >= oldest);
        let mut daily = Vec::new();
        self.buckets.retain(|bucket, totals| {
            if bucket.date < oldest {
                return false;
            }
            if bucket.date < today && bucket.hour.is_some() {
                let mut bucket = bucket.clone();
                bucket.hour = None;
                daily.push((bucket, *totals));
                return false;
            }
            true
        });
        for (bucket, totals) in daily {
            self.buckets.entry(bucket).or_default().add(totals);
        }
        let active_accounts: BTreeSet<_> =
            self.buckets.keys().map(|bucket| &bucket.account).collect();
        self.accounts.retain(|key, _| active_accounts.contains(key));
    }
}

fn serialize_entries<K: Serialize, V: Serialize, S: Serializer>(
    entries: &BTreeMap<K, V>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(entries)
}

fn deserialize_entries<
    'de,
    K: Ord + Deserialize<'de>,
    V: Deserialize<'de>,
    D: Deserializer<'de>,
>(
    deserializer: D,
) -> Result<BTreeMap<K, V>, D::Error> {
    let rows = Vec::<(K, V)>::deserialize(deserializer)?;
    Ok(rows.into_iter().collect())
}

#[derive(Default)]
struct State {
    visible: Arc<UsageHistory>,
    pending: UsageHistory,
}

pub(super) struct UsageStore {
    path: PathBuf,
    state: Mutex<State>,
    writer: Mutex<()>,
    save_queued: AtomicBool,
}

impl UsageStore {
    pub(super) fn load(path: PathBuf) -> Self {
        let mut visible = read_cache(&path).unwrap_or_else(|error| {
            eprintln!("读取用量历史失败：{error:#}");
            UsageHistory::default()
        });
        visible.compact(Local::now().date_naive());
        Self {
            path,
            state: Mutex::new(State {
                visible: Arc::new(visible),
                ..State::default()
            }),
            writer: Mutex::new(()),
            save_queued: AtomicBool::new(false),
        }
    }

    pub(super) fn snapshot(&self) -> Arc<UsageHistory> {
        self.state.lock().visible.clone()
    }

    pub(super) fn record(
        &self,
        account: &str,
        mode: &str,
        sample: RequestSample,
        entry: &RequestLogEntry,
    ) {
        let at = sample.timestamp.with_timezone(&Local);
        let bucket = UsageBucket {
            date: at.date_naive(),
            hour: Some(at.hour()),
            account: account.to_owned(),
            model: entry.resolved_model.trim().to_owned(),
            protocol: sample.protocol,
        };
        let label = AccountLabel {
            name: entry.account.clone(),
            seen_at: sample.timestamp,
        };
        let totals = UsageTotals {
            calls: 1,
            input: sample.input_tokens,
            output: sample.output_tokens,
            cache_read: entry.cached_tokens,
        };
        // A smaller window can invalidate support even when the turn failed.
        let quick_mode = entry
            .context
            .and_then(|usage| usage.window_tokens)
            .filter(|window| sample.succeeded || window.get() < MILLION_TOKENS)
            .filter(|_| {
                CLAUDE_ROLE_MODES
                    .iter()
                    .any(|&(_, fixed_mode)| fixed_mode == mode)
            })
            .map(|_| mode.to_owned());
        let context = Some(ContextSample {
            timestamp: sample.timestamp,
            usage: entry.context,
            quick_mode,
        });
        let mut state = self.state.lock();
        Arc::make_mut(&mut state.visible).record(bucket.clone(), &label, totals, context.clone());
        state.pending.record(bucket, &label, totals, context);
    }

    pub(super) fn schedule_save(self: &Arc<Self>) {
        if self.save_queued.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let store = self.clone();
            runtime.spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = tokio::task::spawn_blocking(move || store.save()).await;
            });
        } else {
            self.save();
        }
    }

    pub(super) fn save(&self) {
        let _writer = self.writer.lock();
        self.save_queued.store(false, Ordering::Release);
        let mut changes = std::mem::take(&mut self.state.lock().pending);
        if changes.buckets.is_empty() {
            return;
        }
        let today = Local::now().date_naive();
        let saved = self.write_changes(&changes, today);
        let mut state = self.state.lock();
        match saved {
            Ok(mut from_disk) => {
                from_disk.merge(&state.pending);
                state.visible = Arc::new(from_disk);
            }
            Err(error) => {
                changes.merge(&state.pending);
                changes.compact(today);
                state.pending = changes;
                Arc::make_mut(&mut state.visible).compact(today);
                eprintln!("写入用量历史失败：{error:#}");
            }
        }
    }

    fn write_changes(&self, changes: &UsageHistory, today: NaiveDate) -> Result<UsageHistory> {
        fs::create_dir_all(self.path.parent().context("无法定位用量历史目录")?)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))?;
        lock.lock()?;

        // 只合并本次新增的用量，GUI 与命令行可共用同一份本地历史。
        let mut from_disk = read_cache(&self.path)?;
        from_disk.merge(changes);
        from_disk.compact(today);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        serde_json::to_writer(&mut encoder, &from_disk)?;
        write_secure_file(&self.path, &encoder.finish()?)?;
        Ok(from_disk)
    }
}

fn read_cache(path: &Path) -> Result<UsageHistory> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(UsageHistory::default()),
        Err(error) => return Err(error.into()),
    };
    Ok(serde_json::from_reader(BufReader::new(GzDecoder::new(
        file,
    )))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use serde_json::json;
    use std::num::NonZeroU64;

    fn bucket(date: NaiveDate, hour: u32) -> UsageBucket {
        UsageBucket {
            date,
            hour: Some(hour),
            account: "account-a".into(),
            model: "resolved-model".into(),
            protocol: RequestProtocol::Responses,
        }
    }

    fn fixtures() -> (AccountLabel, UsageTotals) {
        (
            AccountLabel {
                name: "A".into(),
                seen_at: Utc::now(),
            },
            UsageTotals {
                calls: 1,
                input: 2_000,
                output: 180,
                cache_read: 800,
            },
        )
    }

    #[test]
    fn compaction_keeps_today_hourly_and_thirty_calendar_days() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        let yesterday = today - Days::new(1);
        let oldest = today - Days::new(29);
        let expired = today - Days::new(30);
        let mut history = UsageHistory::default();
        let (label, totals) = fixtures();
        for date in [expired, oldest, yesterday, today] {
            for hour in [3, 23] {
                let context = ContextSample {
                    timestamp: date.and_hms_opt(hour, 0, 0).unwrap().and_utc(),
                    usage: Some(ContextUsage {
                        used_tokens: Some(2_000),
                        window_tokens: std::num::NonZeroU64::new(200_000),
                    }),
                    quick_mode: None,
                };
                history.record(bucket(date, hour), &label, totals, Some(context));
            }
        }

        history.compact(today);
        assert_eq!(history.started_on, Some(expired));
        assert_eq!(history.buckets.len(), 4);
        assert!(history.buckets.keys().all(|row| row.date >= oldest));
        assert_eq!(history.contexts.len(), 3);
        assert!(
            history
                .contexts
                .keys()
                .all(|row| row.date >= oldest && row.hour.is_none())
        );
        assert_eq!(history.contexts.values().map(Vec::len).sum::<usize>(), 6);
        assert_eq!(history.buckets[&bucket(today, 3)], totals);
        for date in [oldest, yesterday] {
            let daily = UsageBucket {
                hour: None,
                ..bucket(date, 0)
            };
            assert_eq!(
                history.buckets[&daily],
                UsageTotals {
                    calls: 2,
                    input: 4_000,
                    output: 360,
                    cache_read: 1_600,
                }
            );
        }
    }

    #[test]
    fn restart_restores_merged_deltas_without_storing_individual_calls() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("usage-history-v1.json.gz");
        let first = UsageStore::load(path.clone());
        let second = UsageStore::load(path.clone());
        let now = Local::now();
        let key = bucket(now.date_naive(), now.hour());
        let (label, totals) = fixtures();
        for store in [&first, &second] {
            let mut state = store.state.lock();
            for _ in 0..1_000 {
                state.pending.record(key.clone(), &label, totals, None);
            }
        }
        first.save();
        second.save();
        first.save();

        let restored = UsageStore::load(path.clone());
        let state = restored.state.lock();
        assert_eq!(state.visible.buckets.len(), 1);
        assert!(state.visible.contexts.is_empty());
        assert_eq!(
            state.visible.buckets[&key],
            UsageTotals {
                calls: 2_000,
                input: 4_000_000,
                output: 360_000,
                cache_read: 1_600_000,
            }
        );
        assert!(fs::metadata(path).unwrap().len() < 512);
    }

    #[test]
    fn context_support_ignores_legacy_and_expired_reports_and_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("usage-history-v1.json.gz");
        let store = UsageStore::load(path.clone());
        let now = Utc::now();
        let date = now.with_timezone(&Local).date_naive();
        let (label, totals) = fixtures();
        let legacy: ContextSample = serde_json::from_value(json!({
            "timestamp": now.timestamp_millis(),
            "usage": {"used_tokens": 2_000, "window_tokens": MILLION_TOKENS}
        }))
        .unwrap();
        store
            .state
            .lock()
            .pending
            .record(bucket(date, 0), &label, totals, Some(legacy));
        store.save();
        let accounts = HashSet::from(["account-a".into()]);
        let supported = || store.snapshot().confirmed_1m_modes(&accounts);
        assert!(supported().is_empty());

        let record = |timestamp, status, window| {
            let entry = RequestLogEntry {
                timestamp,
                level: crate::pool::LogLevel::Info,
                api: "messages".into(),
                requested_model: "smart".into(),
                resolved_model: "resolved-model".into(),
                account: label.name.clone(),
                status,
                latency_ms: 10,
                input_tokens: 2_000,
                output_tokens: 180,
                cached_tokens: 800,
                context: Some(ContextUsage {
                    used_tokens: Some(2_000),
                    window_tokens: NonZeroU64::new(window),
                }),
                message: String::new(),
            };
            store.record(
                "account-a",
                "smart",
                RequestSample::from_log(&entry).unwrap(),
                &entry,
            );
        };
        record(now - TimeDelta::days(31), 200, MILLION_TOKENS);
        record(now - TimeDelta::seconds(4), 502, MILLION_TOKENS);
        assert!(supported().is_empty());
        record(now - TimeDelta::seconds(3), 200, MILLION_TOKENS);
        assert_eq!(supported(), HashSet::from(["smart"]));
        record(now - TimeDelta::seconds(2), 502, 200_000);
        assert!(supported().is_empty());
        record(now - TimeDelta::seconds(1), 200, 1_048_576);
        store.save();
        let reloaded = UsageStore::load(path);
        assert_eq!(
            reloaded.snapshot().confirmed_1m_modes(&accounts),
            HashSet::from(["smart"])
        );
    }
}
