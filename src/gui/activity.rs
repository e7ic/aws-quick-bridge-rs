use crate::pool::{AccountSnapshot, PoolSnapshot, RequestProtocol, RequestSample};
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::motion::{CONTENT_FADE_DURATION, ease_out_quad, is_reduced_motion};

pub(super) const ACTIVITY_BUCKETS: usize = 24;
const ACCOUNT_HIGHLIGHT_HOLD: Duration = Duration::from_millis(800);

#[derive(Default)]
pub(super) struct AccountCardHighlight {
    held: Option<(String, Instant)>,
    fades: HashMap<String, HighlightFade>,
}

struct HighlightFade {
    started_at: Instant,
    from: f32,
    target: f32,
}

impl HighlightFade {
    fn opacity(&self, now: Instant) -> f32 {
        if is_reduced_motion() {
            return self.target;
        }
        let elapsed = now.saturating_duration_since(self.started_at);
        let progress = (elapsed.as_secs_f32() / CONTENT_FADE_DURATION.as_secs_f32()).min(1.0);
        let eased = ease_out_quad(progress);
        self.from + (self.target - self.from) * eased
    }

    fn is_animating(&self, now: Instant) -> bool {
        !is_reduced_motion() && now.saturating_duration_since(self.started_at) < CONTENT_FADE_DURATION
    }
}

impl AccountCardHighlight {
    pub(super) fn observe(
        &mut self,
        previous: &PoolSnapshot,
        current: &PoolSnapshot,
        now: Instant,
    ) {
        let previous_visible: HashSet<_> = previous
            .accounts
            .iter()
            .filter(|account| self.visible(account))
            .map(|account| account.key.as_str())
            .collect();
        let last_idle_account = current
            .last_used_account_key
            .as_deref()
            .filter(|_| current.active_requests == 0);
        self.held = last_idle_account.and_then(|key| {
            if previous.active_requests > 0
                && previous
                    .accounts
                    .iter()
                    .any(|account| account.key == key && account.active_requests > 0)
            {
                Some((key.to_string(), now + ACCOUNT_HIGHLIGHT_HOLD))
            } else {
                self.held
                    .take()
                    .filter(|(held, until)| held == key && now < *until)
            }
        });
        self.fades.retain(|_, fade| fade.is_animating(now));
        for account in &current.accounts {
            let was_visible = previous_visible.contains(account.key.as_str());
            let visible = self.visible(account);
            if was_visible == visible {
                continue;
            }
            let from = self
                .fades
                .get(&account.key)
                .map_or(u8::from(was_visible) as f32, |fade| fade.opacity(now));
            self.fades.insert(
                account.key.clone(),
                HighlightFade {
                    started_at: now,
                    from,
                    target: u8::from(visible) as f32,
                },
            );
        }
    }

    pub(super) fn visible(&self, account: &AccountSnapshot) -> bool {
        account.active_requests > 0
            || self
                .held
                .as_ref()
                .is_some_and(|(key, _)| key == &account.key)
    }

    pub(super) fn expired(&self, now: Instant) -> bool {
        self.held.as_ref().is_some_and(|(_, until)| now >= *until)
    }

    pub(super) fn opacity(&self, account: &AccountSnapshot, now: Instant) -> f32 {
        self.fades.get(&account.key).map_or_else(
            || u8::from(self.visible(account)) as f32,
            |fade| fade.opacity(now),
        )
    }

    pub(super) fn is_animating(&self, now: Instant) -> bool {
        self.fades.values().any(|fade| fade.is_animating(now))
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum ActivityRange {
    SixHours,
    TwelveHours,
    #[default]
    Day,
}

impl ActivityRange {
    pub(super) const ALL: [Self; 3] = [Self::SixHours, Self::TwelveHours, Self::Day];

    pub(super) fn hours(self) -> i64 {
        match self {
            Self::SixHours => 6,
            Self::TwelveHours => 12,
            Self::Day => 24,
        }
    }

    pub(super) fn bucket_minutes(self) -> i64 {
        self.hours() * 60 / ACTIVITY_BUCKETS as i64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LatencySummary {
    pub(super) p50_ms: u64,
    pub(super) p95_ms: u64,
}

impl LatencySummary {
    fn from_samples(samples: &mut [u64]) -> Option<Self> {
        let count = samples.len();
        if count == 0 {
            return None;
        }
        samples.sort_unstable();
        let lower = samples[(count - 1) / 2];
        let upper = samples[count / 2];
        Some(Self {
            p50_ms: lower + (upper - lower) / 2,
            p95_ms: samples[count - count / 20 - 1],
        })
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct ActivityBucket {
    pub(super) requests: usize,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) latency: Option<LatencySummary>,
}

impl ActivityBucket {
    pub(super) fn tokens(self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

#[derive(Default)]
pub(super) struct RequestActivity {
    pub(super) buckets: [ActivityBucket; ACTIVITY_BUCKETS],
    pub(super) requests: usize,
    pub(super) succeeded: usize,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) latency: Option<LatencySummary>,
    pub(super) messages: usize,
    pub(super) responses: usize,
    pub(super) chat: usize,
}

impl RequestActivity {
    pub(super) fn tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    pub(super) fn from_samples(
        history: &[RequestSample],
        now: DateTime<Utc>,
        range: ActivityRange,
    ) -> Self {
        let start = now - TimeDelta::hours(range.hours());
        let bucket_minutes = range.bucket_minutes();
        let mut activity = Self::default();
        let mut samples: [Vec<u64>; ACTIVITY_BUCKETS] = std::array::from_fn(|_| Vec::new());
        for entry in history {
            if entry.timestamp < start || entry.timestamp > now {
                continue;
            }
            let protocol_count = match entry.protocol {
                RequestProtocol::Messages => &mut activity.messages,
                RequestProtocol::Responses => &mut activity.responses,
                RequestProtocol::Chat => &mut activity.chat,
            };
            *protocol_count += 1;

            let index = ((entry.timestamp - start).num_minutes() / bucket_minutes) as usize;
            let index = index.min(ACTIVITY_BUCKETS - 1);
            let bucket = &mut activity.buckets[index];
            bucket.requests += 1;
            bucket.input_tokens = bucket.input_tokens.saturating_add(entry.input_tokens);
            bucket.output_tokens = bucket.output_tokens.saturating_add(entry.output_tokens);
            if entry.latency_ms > 0 {
                samples[index].push(entry.latency_ms);
            }
            activity.requests += 1;
            activity.succeeded += usize::from(entry.succeeded);
            activity.input_tokens = activity.input_tokens.saturating_add(entry.input_tokens);
            activity.output_tokens = activity.output_tokens.saturating_add(entry.output_tokens);
        }
        let mut all_samples: Vec<_> = samples.iter().flatten().copied().collect();
        activity.latency = LatencySummary::from_samples(&mut all_samples);
        for (bucket, mut samples) in activity.buckets.iter_mut().zip(samples) {
            bucket.latency = LatencySummary::from_samples(&mut samples);
        }
        activity
    }
}

#[derive(Default)]
pub(super) struct AccountActivity {
    request_total: Option<u64>,
    requests_by_hour: [Option<(i64, u64)>; ACTIVITY_BUCKETS],
}

impl AccountActivity {
    fn observe_requests(&mut self, total: u64, now: DateTime<Utc>) {
        let Some(previous) = self.request_total.replace(total) else {
            return;
        };
        let Some(delta) = total.checked_sub(previous) else {
            self.requests_by_hour = [None; ACTIVITY_BUCKETS];
            return;
        };
        if delta == 0 {
            return;
        }

        let hour = now.timestamp().div_euclid(3600);
        let index = request_hour_index(hour);
        let count = request_hour_count(self.requests_by_hour[index], hour);
        self.requests_by_hour[index] = Some((hour, count.saturating_add(delta)));
    }

    pub(super) fn hourly_requests(&self, now: DateTime<Utc>) -> [u64; ACTIVITY_BUCKETS] {
        let last = now.timestamp().div_euclid(3600);
        let first = last - ACTIVITY_BUCKETS as i64 + 1;
        std::array::from_fn(|index| {
            let hour = first + index as i64;
            request_hour_count(self.requests_by_hour[request_hour_index(hour)], hour)
        })
    }
}

pub(super) fn observe_accounts(
    activity: &mut HashMap<String, AccountActivity>,
    accounts: &[AccountSnapshot],
    now: DateTime<Utc>,
) {
    let keys: HashSet<_> = accounts
        .iter()
        .map(|account| account.key.as_str())
        .collect();
    activity.retain(|key, _| keys.contains(key.as_str()));
    for account in accounts {
        activity
            .entry(account.key.clone())
            .or_default()
            .observe_requests(account.total_requests, now);
    }
}

fn request_hour_index(hour: i64) -> usize {
    hour.rem_euclid(ACTIVITY_BUCKETS as i64) as usize
}

fn request_hour_count(bucket: Option<(i64, u64)>, hour: i64) -> u64 {
    bucket
        .filter(|(at, _)| *at == hour)
        .map_or(0, |(_, count)| count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{AccountHealth, LogLevel, RequestLogEntry};

    fn highlight_snapshot(a: usize, b: usize, last: &str) -> PoolSnapshot {
        let accounts = [("a", a), ("b", b)]
            .into_iter()
            .map(|(key, active_requests)| AccountSnapshot {
                key: key.into(),
                label: key.into(),
                region: String::new(),
                tenant_url: String::new(),
                enabled: true,
                is_primary: key == "a",
                health: AccountHealth::Healthy,
                active_requests,
                total_requests: 0,
                expires_ms: 0,
                last_resolved_model: None,
                quota: None,
                quota_error: None,
                quota_updated_at: None,
            })
            .collect();
        PoolSnapshot {
            config: Default::default(),
            accounts,
            logs: Vec::new(),
            total_requests: 0,
            active_requests: a + b,
            last_used_account_key: Some(last.into()),
            last_resolved_model: None,
        }
    }

    #[test]
    fn account_card_highlight_bridges_short_request_gaps_and_expires_when_idle() {
        let now = Instant::now();
        let active = highlight_snapshot(1, 0, "a");
        let idle = highlight_snapshot(0, 0, "a");
        let mut highlight = AccountCardHighlight::default();

        let first = highlight.visible(&active.accounts[0]);
        highlight.observe(&active, &idle, now);
        let gap = highlight.visible(&idle.accounts[0]);
        assert_eq!(highlight.opacity(&idle.accounts[0], now), 1.0);
        assert!(!highlight.is_animating(now));
        assert_eq!(idle.accounts[0].active_requests, 0);
        highlight.observe(&idle, &active, now + Duration::from_millis(200));
        let second = highlight.visible(&active.accounts[0]);
        assert_eq!([first, gap, second], [true, true, true]);

        let finished = now + Duration::from_secs(1);
        highlight.observe(&active, &idle, finished);
        highlight.observe(&idle, &idle, finished + Duration::from_millis(300));
        assert!(highlight.visible(&idle.accounts[0]));
        let deadline = finished + ACCOUNT_HIGHLIGHT_HOLD;
        assert!(!highlight.expired(deadline - Duration::from_millis(1)));
        assert!(highlight.expired(deadline));
        highlight.observe(&idle, &idle, deadline);
        assert!(!highlight.visible(&idle.accounts[0]));
        assert!(!highlight.expired(deadline));
        assert_eq!(highlight.opacity(&idle.accounts[0], deadline), 1.0);
        assert!(highlight.is_animating(deadline));
        let faded = deadline + CONTENT_FADE_DURATION;
        assert_eq!(highlight.opacity(&idle.accounts[0], faded), 0.0);
        assert!(!highlight.is_animating(faded));
    }

    #[test]
    fn account_card_highlight_yields_immediately_on_account_handoff() {
        let now = Instant::now();
        let first = highlight_snapshot(1, 0, "a");
        let gap = highlight_snapshot(0, 0, "a");
        let next = highlight_snapshot(0, 1, "b");
        let mut highlight = AccountCardHighlight::default();

        highlight.observe(&first, &gap, now);
        assert!(highlight.visible(&gap.accounts[0]));
        highlight.observe(&gap, &next, now + Duration::from_millis(100));
        assert!(!highlight.visible(&next.accounts[0]));
        assert!(highlight.visible(&next.accounts[1]));
        let during = now + Duration::from_millis(160);
        let incoming = highlight.opacity(&next.accounts[1], during);
        let outgoing = highlight.opacity(&next.accounts[0], during);
        assert!(incoming > 0.0 && incoming < 1.0);
        assert!(outgoing > 0.0 && outgoing < 1.0);
        highlight.observe(&next, &first, during);
        assert_eq!(highlight.opacity(&first.accounts[0], during), outgoing);
        assert_eq!(highlight.opacity(&first.accounts[1], during), incoming);
    }

    #[test]
    fn account_activity_counts_only_new_requests_in_the_current_window() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let mut activity = AccountActivity::default();
        activity.observe_requests(10, now);
        assert_eq!(activity.hourly_requests(now), [0; ACTIVITY_BUCKETS]);

        activity.observe_requests(14, now);
        activity.observe_requests(14, now);
        let next_hour = now + TimeDelta::hours(1);
        activity.observe_requests(16, next_hour);
        assert_eq!(
            &activity.hourly_requests(next_hour)[ACTIVITY_BUCKETS - 2..],
            &[4, 2]
        );

        let tomorrow = now + TimeDelta::hours(ACTIVITY_BUCKETS as i64);
        let remaining = activity.hourly_requests(tomorrow);
        assert_eq!(remaining[0], 2);
        assert_eq!(remaining.iter().sum::<u64>(), 2);

        activity.observe_requests(0, tomorrow);
        assert_eq!(activity.hourly_requests(tomorrow), [0; ACTIVITY_BUCKETS]);
    }

    #[test]
    fn activity_counts_recent_gateway_attempts_only() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let start = now - TimeDelta::hours(24);
        let entry = |timestamp, api: &str, status, latency_ms| RequestLogEntry {
            timestamp,
            level: LogLevel::Info,
            api: api.into(),
            requested_model: String::new(),
            resolved_model: String::new(),
            account: String::new(),
            status,
            latency_ms,
            input_tokens: 30,
            output_tokens: 10,
            cached_tokens: 15,
            context: None,
            message: String::new(),
        };
        let logs = [
            entry(start - TimeDelta::milliseconds(1), "messages", 200, 6_000),
            entry(start, "chat", 200, 0),
            entry(now - TimeDelta::minutes(30), "responses", 429, 950),
            entry(now, "messages", 200, 350),
            entry(now, "probe", 200, 6_000),
            entry(now, "system", 0, 6_000),
            entry(now + TimeDelta::milliseconds(1), "chat", 200, 6_000),
        ];
        let samples: Vec<_> = logs.iter().filter_map(RequestSample::from_log).collect();
        let activity = RequestActivity::from_samples(&samples, now, ActivityRange::Day);
        assert_eq!(activity.requests, 3);
        assert_eq!(activity.succeeded, 2);
        assert_eq!(activity.tokens(), 120);
        assert_eq!(activity.buckets[0].requests, 1);
        assert_eq!(activity.buckets[0].latency, None);
        assert_eq!(activity.buckets[23].requests, 2);
        assert_eq!(activity.buckets[23].input_tokens, 60);
        assert_eq!(activity.buckets[23].output_tokens, 20);
        assert_eq!(
            activity.latency,
            Some(LatencySummary {
                p50_ms: 650,
                p95_ms: 950
            })
        );
        assert_eq!(activity.buckets[23].latency, activity.latency);
        assert_eq!(
            (activity.messages, activity.responses, activity.chat),
            (1, 1, 1)
        );

        let recent = RequestActivity::from_samples(&samples, now, ActivityRange::SixHours);
        assert_eq!(
            (recent.requests, recent.succeeded, recent.tokens()),
            (2, 1, 80)
        );
        assert_eq!((recent.input_tokens, recent.output_tokens), (60, 20));
        assert_eq!(recent.buckets[0].requests, 0);
        assert_eq!(recent.buckets[22].requests, 1);
        assert_eq!(
            recent.buckets[22].latency.map(|values| values.p95_ms),
            Some(950)
        );
        assert_eq!(recent.buckets[23].requests, 1);
        assert_eq!(
            recent.buckets[23].latency.map(|values| values.p50_ms),
            Some(350)
        );

        let half_day = RequestActivity::from_samples(&samples, now, ActivityRange::TwelveHours);
        assert_eq!(half_day.requests, 2);
        assert_eq!(half_day.buckets[22].requests, 0);
        assert_eq!(half_day.buckets[23].requests, 2);
    }

    #[test]
    fn latency_uses_median_and_nearest_rank_p95() {
        let mut samples: [u64; 20] = std::array::from_fn(|index| (20 - index) as u64 * 10);
        assert_eq!(
            LatencySummary::from_samples(&mut samples),
            Some(LatencySummary {
                p50_ms: 105,
                p95_ms: 190
            }),
        );
        assert_eq!(LatencySummary::from_samples(&mut []), None);
    }
}
