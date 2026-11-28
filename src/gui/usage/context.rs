use super::super::tables::TablePage;
use super::{
    BridgeApp, Theme, UsageFilters, format_tokens, model_label, motion, numeric_font,
    protocol_label,
};
use crate::pool::{ContextSample, UsageBucket, UsageHistory};
use crate::upstream::ContextUsage;
use chrono::{Local, NaiveDate};
use gpui::{Context, Div, FontWeight, SharedString, div, prelude::*, px};
use std::collections::BinaryHeap;
use std::ops::RangeInclusive;

fn request_page<'a>(
    history: &'a UsageHistory,
    dates: RangeInclusive<NaiveDate>,
    filters: &UsageFilters,
    index: usize,
) -> (TablePage, Vec<(&'a UsageBucket, &'a ContextSample, usize)>) {
    let groups: Vec<_> = history
        .contexts
        .iter()
        .filter(|(bucket, points)| {
            dates.contains(&bucket.date)
                && filters.matches(&bucket.account, &bucket.model, bucket.protocol)
                && !points.is_empty()
        })
        .collect();
    let page = TablePage::new(groups.iter().map(|(_, points)| points.len()).sum(), index);
    let range = page.range();
    // Merge the newest samples from each ordered series as needed for this page.
    let mut newest: BinaryHeap<_> = groups
        .iter()
        .enumerate()
        .map(|(group, (_, points))| {
            let last = points.len() - 1;
            (points[last].timestamp, group, last)
        })
        .collect();
    let mut rows = Vec::with_capacity(range.len());
    for row in 0..range.end {
        let Some((_, group, index)) = newest.pop() else {
            break;
        };
        let (bucket, points) = groups[group];
        if row >= range.start {
            rows.push((bucket, &points[index], index));
        }
        if let Some(previous) = index.checked_sub(1) {
            newest.push((points[previous].timestamp, group, previous));
        }
    }
    (page, rows)
}

fn context_percent(usage: ContextUsage) -> String {
    let Some((used, window)) = usage.used_tokens.zip(usage.window_tokens) else {
        return "—".into();
    };
    let percent = 100.0 * used as f64 / window.get() as f64;
    if percent > 0.0 && percent < 0.1 {
        "<0.1%".into()
    } else {
        format!("{percent:.1}%")
    }
}

impl BridgeApp {
    pub(super) fn usage_contexts(
        &self,
        t: Theme,
        history: &UsageHistory,
        start: NaiveDate,
        today: NaiveDate,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let (page, rows) = request_page(
            history,
            start..=today,
            &self.usage.filters,
            self.usage.context_page,
        );
        let header = Self::section_header(t, "请求上下文")
            .px(px(14.0))
            .py(px(10.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(t.text_muted)
                    .child("输入含缓存读取和写入"),
            );
        let panel = Self::table_frame(t).child(header);
        if rows.is_empty() {
            return panel.child(
                Self::table_row(t)
                    .text_size(px(12.0))
                    .text_color(t.text_muted)
                    .child("此范围暂无上下文记录"),
            );
        }

        let (time_width, tokens_width, ratio_width) = if compact {
            (86.0, 62.0, 52.0)
        } else {
            (104.0, 92.0, 64.0)
        };
        let number_cell = |width, value: String| Self::table_cell(width).text_right().child(value);
        let token_label = |value: Option<u64>| value.map_or_else(|| "—".into(), format_tokens);
        let exact_tokens =
            |value: Option<u64>| value.map_or_else(|| "—".into(), |v| format!("{v} Token"));
        panel
            .child(motion::layout(
                Self::table_header(t)
                    .id("context-columns")
                    .child(Self::table_cell(time_width).child("时间"))
                    .child(div().min_w(px(0.0)).flex_1().child("请求"))
                    .child(number_cell(tokens_width, "已用 Token".into()))
                    .child(number_cell(tokens_width, "窗口上限".into()))
                    .child(number_cell(ratio_width, "占比".into())),
            ))
            .children(rows.into_iter().map(|(bucket, sample, index)| {
                let model = model_label(&bucket.model);
                let account = history
                    .account_label(&bucket.account)
                    .unwrap_or(&bucket.account);
                let protocol = protocol_label(bucket.protocol);
                let usage = sample.usage.unwrap_or_default();
                let window = usage.window_tokens.map(|value| value.get());
                let percent = context_percent(usage);
                let time = sample.timestamp.with_timezone(&Local);
                let tooltip = format!(
                    "{}\n{model}\n{account} · {protocol}\n已用 {} · 上限 {} · 占比 {percent}",
                    time.format("%Y-%m-%d %H:%M:%S"),
                    exact_tokens(usage.used_tokens),
                    exact_tokens(window),
                );
                motion::layout(
                    Self::table_row(t)
                        .id((SharedString::from(format!("{bucket:?}")), index))
                        .text_size(px(12.0))
                        .tooltip(Self::text_tooltip(t, tooltip))
                        .child(
                            Self::table_cell(time_width)
                                .font(numeric_font())
                                .text_color(t.text_muted)
                                .child(time.format("%m-%d %H:%M:%S").to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(div().truncate().child(model.to_owned()))
                                .child(
                                    div()
                                        .truncate()
                                        .text_color(t.text_muted)
                                        .text_size(px(11.0))
                                        .child(format!("{account} · {protocol}")),
                                ),
                        )
                        .child(
                            number_cell(tokens_width, token_label(usage.used_tokens))
                                .font(numeric_font()),
                        )
                        .child(number_cell(tokens_width, token_label(window)).font(numeric_font()))
                        .child(
                            number_cell(ratio_width, percent)
                                .font(numeric_font())
                                .font_weight(FontWeight::MEDIUM),
                        ),
                )
            }))
            .when(page.has_multiple_pages(), |panel| {
                panel.child(Self::table_footer(
                    t,
                    "usage-context-pages",
                    page,
                    |this, index| {
                        this.usage.context_page = index;
                    },
                    cx,
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::RequestProtocol;
    use chrono::Days;
    use std::num::NonZeroU64;

    #[test]
    fn context_page_filters_and_orders_individual_requests() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 26).unwrap();
        let yesterday = today - Days::new(1);
        let make_bucket = |date, account: &str, model: &str, protocol| UsageBucket {
            date,
            account: account.into(),
            model: model.into(),
            hour: None,
            protocol,
        };
        let record = |date: NaiveDate, second| ContextSample {
            timestamp: date.and_hms_opt(1, 0, second).unwrap().and_utc(),
            usage: Some(ContextUsage {
                used_tokens: Some(u64::from(second)),
                window_tokens: NonZeroU64::new(200_000),
            }),
            quick_mode: None,
        };
        let mut history = UsageHistory::default();
        let messages = RequestProtocol::Messages;
        let responses = RequestProtocol::Responses;
        for (date, account, model, protocol, count) in [
            (today, "a", "smart", messages, 11),
            (today, "b", "smart", messages, 30),
            (today, "a", "fast", messages, 30),
            (today, "a", "smart", responses, 30),
            (yesterday, "a", "smart", messages, 1),
        ] {
            history.contexts.insert(
                make_bucket(date, account, model, protocol),
                (1..=count).map(|second| record(date, second)).collect(),
            );
        }
        let filters = UsageFilters {
            account: Some("a".into()),
            model: Some("smart".into()),
            protocol: Some(messages),
        };
        let (first, rows) = request_page(&history, today..=today, &filters, 0);
        assert_eq!(first.range(), 0..10);
        assert_eq!(rows[0].1.usage.unwrap().used_tokens, Some(11));
        assert_eq!(rows[9].1.usage.unwrap().used_tokens, Some(2));
        let (last, rows) = request_page(&history, yesterday..=today, &filters, 5);
        assert_eq!(last.range(), 10..12);
        assert_eq!(rows[0].0.date, today);
        assert_eq!(rows[1].0.date, yesterday);
    }

    #[test]
    fn context_percent_uses_the_reported_window() {
        let usage = ContextUsage {
            used_tokens: Some(50_000),
            window_tokens: NonZeroU64::new(250_000),
        };
        assert_eq!(context_percent(usage), "20.0%");
        assert_eq!(
            context_percent(ContextUsage {
                window_tokens: None,
                ..usage
            }),
            "—"
        );
        assert_eq!(
            context_percent(ContextUsage {
                used_tokens: Some(1),
                ..usage
            }),
            "<0.1%"
        );
        assert_eq!(
            context_percent(ContextUsage {
                used_tokens: Some(0),
                ..usage
            }),
            "0.0%"
        );
    }
}
