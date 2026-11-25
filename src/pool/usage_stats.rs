use super::RequestLogEntry;
use super::request_stats::{RequestProtocol, RequestSample};
use crate::config::write_secure_file;
use anyhow::{Context, Result};
use chrono::{DateTime, Days, Local, NaiveDate, Timelike, Utc};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use parking_lot::Mutex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(crate) const USAGE_RETENTION_DAYS: u64 = 30;

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

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct UsageHistory {
    pub(crate) started_on: Option<NaiveDate>,
    accounts: BTreeMap<String, AccountLabel>,
    #[serde(
        serialize_with = "serialize_buckets",
        deserialize_with = "deserialize_buckets"
    )]
    pub(crate) buckets: UsageBuckets,
}

impl UsageHistory {
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

    fn record(&mut self, bucket: UsageBucket, label: &AccountLabel, totals: UsageTotals) {
        self.started_on = Some(
            self.started_on
                .map_or(bucket.date, |date| date.min(bucket.date)),
        );
        self.update_label(&bucket.account, label);
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
    }

    fn compact(&mut self, today: NaiveDate) {
        let oldest = today - Days::new(USAGE_RETENTION_DAYS - 1);
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

fn serialize_buckets<S: Serializer>(
    buckets: &UsageBuckets,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(buckets)
}

fn deserialize_buckets<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<UsageBuckets, D::Error> {
    let rows = Vec::<(UsageBucket, UsageTotals)>::deserialize(deserializer)?;
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

    #[cfg(feature = "gui")]
    pub(super) fn snapshot(&self) -> Arc<UsageHistory> {
        self.state.lock().visible.clone()
    }

    pub(super) fn record(&self, account: &str, sample: RequestSample, entry: &RequestLogEntry) {
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
        let mut state = self.state.lock();
        Arc::make_mut(&mut state.visible).record(bucket.clone(), &label, totals);
        state.pending.record(bucket, &label, totals);
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
    let mut data = Vec::new();
    GzDecoder::new(file).read_to_end(&mut data)?;
    Ok(serde_json::from_slice(&data)?)
}

#[cfg(test)]
mod tests {
    use super::*;

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
                history.record(bucket(date, hour), &label, totals);
            }
        }

        history.compact(today);
        assert_eq!(history.started_on, Some(expired));
        assert_eq!(history.buckets.len(), 4);
        assert!(history.buckets.keys().all(|row| row.date >= oldest));
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
                state.pending.record(key.clone(), &label, totals);
            }
        }
        first.save();
        second.save();
        first.save();

        let restored = UsageStore::load(path.clone());
        let state = restored.state.lock();
        assert_eq!(state.visible.buckets.len(), 1);
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
}
