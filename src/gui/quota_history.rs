use crate::config::write_secure_json;
use crate::qbs::{QuotaWindow, UsageReport};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_READINGS: usize = 24;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct QuotaReading {
    pub(super) at: DateTime<Utc>,
    pub(super) plan: String,
    pub(super) windows: Vec<QuotaWindow>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct QuotaHistory {
    accounts: BTreeMap<String, Vec<QuotaReading>>,
}

impl QuotaHistory {
    pub(super) fn readings(&self, account_key: &str) -> &[QuotaReading] {
        self.accounts.get(account_key).map_or(&[], Vec::as_slice)
    }

    fn normalize(readings: &mut Vec<QuotaReading>) {
        readings.sort_by_key(|reading| reading.at);
        readings.dedup_by_key(|reading| reading.at);
        let excess = readings.len().saturating_sub(MAX_READINGS);
        readings.drain(..excess);
    }

    fn merge(&mut self, other: Self) {
        for (key, history) in other.accounts {
            let readings = self.accounts.entry(key).or_default();
            readings.extend(history);
            Self::normalize(readings);
        }
    }
}

pub(super) struct QuotaHistoryStore {
    path: PathBuf,
    visible: Mutex<Arc<QuotaHistory>>,
    writer: Mutex<()>,
}

impl QuotaHistoryStore {
    pub(super) fn load(path: PathBuf) -> Self {
        let history = read_history(&path).unwrap_or_else(|error| {
            eprintln!("{error:#}");
            QuotaHistory::default()
        });
        Self {
            path,
            visible: Mutex::new(Arc::new(history)),
            writer: Mutex::new(()),
        }
    }

    pub(super) fn snapshot(&self) -> Arc<QuotaHistory> {
        self.visible.lock().clone()
    }

    pub(super) fn record(&self, key: String, at: DateTime<Utc>, report: UsageReport) -> Result<()> {
        let _writer = self.writer.lock();
        let mut history = self.snapshot().as_ref().clone();
        let readings = history.accounts.entry(key).or_default();
        readings.push(QuotaReading {
            at,
            plan: report.plan,
            windows: report.windows,
        });
        QuotaHistory::normalize(readings);

        let result = self.save(&mut history);
        *self.visible.lock() = Arc::new(history);
        result
    }

    fn save(&self, history: &mut QuotaHistory) -> Result<()> {
        let parent = self.path.parent().context("无法定位额度历史目录")?;
        fs::create_dir_all(parent).context("无法创建额度历史目录")?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))
            .context("无法打开额度历史锁文件")?;
        lock.lock().context("无法锁定额度历史文件")?;

        // 当前窗口打开期间，其他实例可能已写入更新的额度采样，需先合并再落盘。
        history.merge(read_history(&self.path)?);
        write_secure_json(&self.path, history).context("无法写入额度历史文件")
    }
}

fn read_history(path: &Path) -> Result<QuotaHistory> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(QuotaHistory::default()),
        Err(error) => return Err(error).context("无法读取额度历史文件"),
    };
    let mut history: QuotaHistory =
        serde_json::from_slice(&data).context("额度历史文件格式无效")?;
    for readings in history.accounts.values_mut() {
        QuotaHistory::normalize(readings);
    }
    Ok(history)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn report(amount: f64) -> UsageReport {
        UsageReport {
            plan: "Free Trial".into(),
            user: Some("fixture@example.test".into()),
            until: None,
            windows: vec![QuotaWindow {
                name: "Monthly".into(),
                amount,
                limit: 600.0,
                percent: None,
                display: format!("{amount} / 600 credits"),
                resets_at: None,
                reset_secs: None,
                aside: false,
            }],
        }
    }

    #[test]
    fn reopens_history_and_merges_writes_from_another_instance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota-history.json");
        let first = QuotaHistoryStore::load(path.clone());
        let other = QuotaHistoryStore::load(path.clone());
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();

        std::thread::scope(|scope| {
            let first_write = scope.spawn(|| first.record("a".into(), now, report(10.0)));
            other.record("b".into(), now, report(200.0)).unwrap();
            first_write.join().unwrap().unwrap();
        });
        let next = now + TimeDelta::minutes(1);
        other.record("a".into(), next, report(12.0)).unwrap();

        let snapshot = QuotaHistoryStore::load(path.clone()).snapshot();
        let a = snapshot.readings("a");
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].at, now);
        assert_eq!(a[1].at, next);
        assert_eq!(a[1].windows[0].amount, 12.0);
        assert_eq!(snapshot.readings("b")[0].windows[0].amount, 200.0);
        assert!(
            !fs::read_to_string(path)
                .unwrap()
                .contains("fixture@example.test")
        );
    }

    #[test]
    fn limits_readings_and_ignores_duplicate_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota-history.json");
        let store = QuotaHistoryStore::load(path.clone());
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        for index in 0..=MAX_READINGS {
            let at = now + TimeDelta::minutes(index as i64);
            store.record("a".into(), at, report(index as f64)).unwrap();
        }
        let last = now + TimeDelta::minutes(MAX_READINGS as i64);
        store
            .record("a".into(), last, report(MAX_READINGS as f64))
            .unwrap();

        let snapshot = QuotaHistoryStore::load(path).snapshot();
        let readings = snapshot.readings("a");
        assert_eq!(readings.len(), MAX_READINGS);
        assert_eq!(readings[0].at, now + TimeDelta::minutes(1));
        assert_eq!(readings.last().unwrap().at, last);
    }

    #[test]
    fn preserves_an_unreadable_history_and_keeps_new_samples_visible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota-history.json");
        fs::write(&path, b"{bad json").unwrap();
        let store = QuotaHistoryStore::load(path.clone());

        assert!(store.record("a".into(), Utc::now(), report(10.0)).is_err());
        assert_eq!(fs::read(path).unwrap(), b"{bad json");
        assert_eq!(store.snapshot().readings("a").len(), 1);
    }
}
