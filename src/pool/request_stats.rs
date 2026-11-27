use super::{MAX_LOG_ENTRIES, RequestLogEntry};
use crate::config::write_secure_file;
use anyhow::{Context, Result, bail, ensure};
use bytes::{Buf, BufMut};
use chrono::{DateTime, TimeDelta, Utc};
use parking_lot::Mutex;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const MAGIC: &[u8; 4] = b"QBS1";
const HEADER_BYTES: usize = MAGIC.len() + 16;
const SAMPLE_BYTES: usize = 33;
const MAX_FILE_BYTES: usize = HEADER_BYTES + MAX_LOG_ENTRIES * SAMPLE_BYTES;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub(crate) enum RequestProtocol {
    Messages = 0,
    Responses = 1,
    Chat = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RequestSample {
    pub(crate) timestamp: DateTime<Utc>,
    pub(crate) protocol: RequestProtocol,
    pub(crate) succeeded: bool,
    pub(crate) latency_ms: u64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
}

impl RequestSample {
    pub(crate) fn from_log(entry: &RequestLogEntry) -> Option<Self> {
        if entry.status == 0 {
            return None;
        }
        let protocol = match entry.api.as_str() {
            "messages" => RequestProtocol::Messages,
            "responses" => RequestProtocol::Responses,
            "chat" => RequestProtocol::Chat,
            _ => return None,
        };
        Some(Self {
            timestamp: entry.timestamp,
            protocol,
            succeeded: (200..300).contains(&entry.status),
            latency_ms: entry.latency_ms,
            input_tokens: entry.input_tokens,
            output_tokens: entry.output_tokens,
        })
    }
}

#[derive(Clone, Copy)]
pub(super) enum StatsUpdate {
    Started,
    Completed(RequestSample),
    Cleared(DateTime<Utc>),
}

#[derive(Default)]
struct RequestStats {
    total_requests: u64,
    cleared_at: Option<DateTime<Utc>>,
    samples: Arc<Vec<RequestSample>>,
}

impl RequestStats {
    fn record(&mut self, update: StatsUpdate) {
        match update {
            StatsUpdate::Started => self.total_requests = self.total_requests.saturating_add(1),
            StatsUpdate::Completed(sample) => {
                if self.cleared_at.is_some_and(|at| sample.timestamp <= at) {
                    return;
                }
                let samples = Arc::make_mut(&mut self.samples);
                let index =
                    samples.partition_point(|existing| existing.timestamp <= sample.timestamp);
                samples.insert(index, sample);
                if samples.len() > MAX_LOG_ENTRIES {
                    samples.remove(0);
                }
            }
            StatsUpdate::Cleared(at) => {
                self.cleared_at = self.cleared_at.max(Some(at));
                self.trim_samples(at);
            }
        }
    }

    fn merge(&mut self, delta: &Self, now: DateTime<Utc>) {
        self.total_requests = self.total_requests.saturating_add(delta.total_requests);
        self.cleared_at = self.cleared_at.max(delta.cleared_at);
        if !delta.samples.is_empty() {
            let samples = Arc::make_mut(&mut self.samples);
            samples.extend(delta.samples.iter().copied());
            samples.sort_by_key(|sample| sample.timestamp);
        }
        self.trim_samples(now);
    }

    fn trim_samples(&mut self, now: DateTime<Utc>) {
        let oldest = now - TimeDelta::hours(24);
        let expired = self.samples.partition_point(|sample| {
            sample.timestamp < oldest || self.cleared_at.is_some_and(|at| sample.timestamp <= at)
        });
        let excess = self.samples.len().saturating_sub(MAX_LOG_ENTRIES);
        if expired.max(excess) > 0 {
            Arc::make_mut(&mut self.samples).drain(..expired.max(excess));
        }
    }

    fn is_empty(&self) -> bool {
        self.total_requests == 0 && self.cleared_at.is_none() && self.samples.is_empty()
    }

    fn encode(&self) -> Vec<u8> {
        // 版本、累计次数、最近清空时间，随后是定长数值记录。
        let mut data = Vec::with_capacity(HEADER_BYTES + self.samples.len() * SAMPLE_BYTES);
        data.extend_from_slice(MAGIC);
        data.put_u64_le(self.total_requests);
        data.put_i64_le(self.cleared_at.map_or(i64::MIN, |at| at.timestamp_micros()));
        for sample in self.samples.iter() {
            data.put_i64_le(sample.timestamp.timestamp_micros());
            data.put_u8(2 * sample.protocol as u8 + u8::from(sample.succeeded));
            data.put_u64_le(sample.latency_ms);
            data.put_u64_le(sample.input_tokens);
            data.put_u64_le(sample.output_tokens);
        }
        data
    }

    fn decode(data: &[u8]) -> Result<Self> {
        ensure!(
            (HEADER_BYTES..=MAX_FILE_BYTES).contains(&data.len())
                && (data.len() - HEADER_BYTES).is_multiple_of(SAMPLE_BYTES),
            "请求统计缓存长度无效"
        );
        let mut data = data.strip_prefix(MAGIC).context("请求统计缓存版本无效")?;
        let total_requests = data.get_u64_le();
        let cleared_at = data.get_i64_le();
        let cleared_at = if cleared_at == i64::MIN {
            None
        } else {
            Some(DateTime::from_timestamp_micros(cleared_at).context("清空时间无效")?)
        };
        let mut samples = Vec::with_capacity(data.len() / SAMPLE_BYTES);
        while data.has_remaining() {
            let timestamp =
                DateTime::from_timestamp_micros(data.get_i64_le()).context("请求时间无效")?;
            let flags = data.get_u8();
            let protocol = match flags / 2 {
                0 => RequestProtocol::Messages,
                1 => RequestProtocol::Responses,
                2 => RequestProtocol::Chat,
                _ => bail!("请求协议无效"),
            };
            samples.push(RequestSample {
                timestamp,
                protocol,
                succeeded: flags % 2 == 1,
                latency_ms: data.get_u64_le(),
                input_tokens: data.get_u64_le(),
                output_tokens: data.get_u64_le(),
            });
        }
        samples.sort_by_key(|sample| sample.timestamp);
        Ok(Self {
            total_requests,
            cleared_at,
            samples: Arc::new(samples),
        })
    }
}

struct StatsState {
    visible: RequestStats,
    pending: RequestStats,
}

pub(super) struct RequestStatsStore {
    path: PathBuf,
    state: Mutex<StatsState>,
    writer: Mutex<()>,
    save_queued: AtomicBool,
}

impl RequestStatsStore {
    pub(super) fn load(path: PathBuf) -> Self {
        let mut visible = read_cache(&path).unwrap_or_else(|error| {
            eprintln!("读取请求统计缓存失败：{error:#}");
            RequestStats::default()
        });
        visible.trim_samples(Utc::now());
        Self {
            path,
            state: Mutex::new(StatsState {
                visible,
                pending: RequestStats::default(),
            }),
            writer: Mutex::new(()),
            save_queued: AtomicBool::new(false),
        }
    }

    pub(super) fn total_requests(&self) -> u64 {
        self.state.lock().visible.total_requests
    }

    #[cfg(feature = "gui")]
    pub(super) fn samples(&self) -> Arc<Vec<RequestSample>> {
        self.state.lock().visible.samples.clone()
    }

    pub(super) fn record(&self, update: StatsUpdate) {
        let mut state = self.state.lock();
        state.visible.record(update);
        state.pending.record(update);
    }

    pub(super) fn schedule_save(self: &Arc<Self>) {
        if self.save_queued.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let store = self.clone();
            runtime.spawn_blocking(move || store.save());
        } else {
            self.save();
        }
    }

    pub(super) fn save(&self) {
        let _writer = self.writer.lock();
        self.save_queued.store(false, Ordering::Release);
        let mut changes = std::mem::take(&mut self.state.lock().pending);
        if changes.is_empty() {
            return;
        }

        let saved = self.write_changes(&changes);
        let mut state = self.state.lock();
        match saved {
            Ok(mut from_disk) => {
                from_disk.merge(&state.pending, Utc::now());
                state.visible = from_disk;
            }
            Err(error) => {
                changes.merge(&state.pending, Utc::now());
                state.pending = changes;
                eprintln!("写入请求统计缓存失败：{error:#}");
            }
        }
    }

    fn write_changes(&self, changes: &RequestStats) -> Result<RequestStats> {
        fs::create_dir_all(self.path.parent().context("无法定位请求统计目录")?)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))?;
        lock.lock()?;

        // 合并各实例本次新增的统计，避免按本地快照覆盖别的实例的写入。
        let mut from_disk = read_cache(&self.path)?;
        from_disk.merge(changes, Utc::now());
        write_secure_file(&self.path, &from_disk.encode())?;
        Ok(from_disk)
    }
}

fn read_cache(path: &Path) -> Result<RequestStats> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(RequestStats::default()),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    Ok(RequestStats::decode(&bytes).unwrap_or_else(|error| {
        eprintln!("忽略无效的请求统计缓存：{error:#}");
        RequestStats::default()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(timestamp: DateTime<Utc>) -> RequestSample {
        RequestSample {
            timestamp,
            protocol: RequestProtocol::Responses,
            succeeded: true,
            latency_ms: 1200,
            input_tokens: 28_000,
            output_tokens: 746,
        }
    }

    #[test]
    fn reopens_compact_stats_and_bounds_the_rolling_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("request-stats.bin");
        let store = Arc::new(RequestStatsStore::load(path.clone()));
        let at = DateTime::from_timestamp(Utc::now().timestamp() - 600, 0).unwrap();
        let recent = sample(at);
        store.record(StatsUpdate::Started);
        store.record(StatsUpdate::Completed(sample(at - TimeDelta::hours(24))));
        store.record(StatsUpdate::Completed(recent));
        store.schedule_save();

        let restored = RequestStatsStore::load(path.clone());
        assert_eq!(restored.total_requests(), 1);
        assert_eq!(&*restored.state.lock().visible.samples, &[recent]);
        assert_eq!(
            fs::metadata(&path).unwrap().len() as usize,
            HEADER_BYTES + SAMPLE_BYTES
        );

        for offset in 1..=MAX_LOG_ENTRIES {
            let at = at + TimeDelta::seconds(offset as i64);
            store.record(StatsUpdate::Started);
            store.record(StatsUpdate::Completed(sample(at)));
        }
        store.save();

        let restored = RequestStatsStore::load(path.clone());
        assert_eq!(restored.total_requests(), MAX_LOG_ENTRIES as u64 + 1);
        let state = restored.state.lock();
        assert_eq!(state.visible.samples.len(), MAX_LOG_ENTRIES);
        assert_eq!(
            state.visible.samples[0].timestamp,
            at + TimeDelta::seconds(1)
        );
        assert_eq!(fs::metadata(&path).unwrap().len(), 9920);
    }

    #[test]
    fn merges_parallel_writes_and_keeps_cleared_samples_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("request-stats.bin");
        let first = RequestStatsStore::load(path.clone());
        let second = RequestStatsStore::load(path.clone());
        let at = DateTime::from_timestamp(Utc::now().timestamp() - 60, 0).unwrap();
        for store in [&first, &second] {
            store.record(StatsUpdate::Started);
            store.record(StatsUpdate::Completed(sample(at)));
        }
        std::thread::scope(|scope| {
            scope.spawn(|| first.save());
            second.save();
        });

        let saved = read_cache(&path).unwrap();
        assert_eq!(saved.total_requests, 2);
        assert_eq!(saved.samples.len(), 2);

        second.record(StatsUpdate::Completed(sample(at)));
        let cleared = at + TimeDelta::seconds(1);
        first.record(StatsUpdate::Cleared(cleared));
        first.save();
        let after = sample(cleared + TimeDelta::seconds(1));
        second.record(StatsUpdate::Started);
        second.record(StatsUpdate::Completed(after));
        second.save();

        let saved = read_cache(&path).unwrap();
        assert_eq!(saved.total_requests, 3);
        assert_eq!(&*saved.samples, &[after]);
    }

    #[test]
    fn retries_a_failed_write_without_losing_or_double_counting_changes() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("stats");
        let path = parent.join("request-stats.bin");
        let store = RequestStatsStore::load(path.clone());
        fs::write(&parent, b"blocked directory").unwrap();
        store.record(StatsUpdate::Started);
        store.save();
        assert_eq!(store.total_requests(), 1);

        fs::remove_file(&parent).unwrap();
        store.record(StatsUpdate::Started);
        store.save();
        store.save();
        assert_eq!(RequestStatsStore::load(path).total_requests(), 2);
    }
}
