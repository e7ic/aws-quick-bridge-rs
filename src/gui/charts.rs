use super::activity::{ACTIVITY_BUCKETS, ActivityBucket, ActivityRange, RequestActivity};
use super::components::{QuotaTrendPoint, Theme};
use super::motion::{self, SpringConfig};
use super::{BridgeApp, numeric_font};
use chrono::{DateTime, Local, TimeDelta, Utc};
use gpui::{
    AnyElement, Bounds, ContentMask, Context, Div, FontWeight, Hsla, PathBuilder, Pixels, Point,
    Window, canvas, div, fill, linear_color_stop, linear_gradient, point, prelude::*, px, relative,
    size,
};
use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::time::Duration;

const PLOT_HEIGHT: f32 = 136.0;
const AXIS_WIDTH: f32 = 54.0;
const SPARKLINE_INSET: f32 = 2.0;
const QUOTA_TREND_HEIGHT: f32 = 24.0;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum TrendMetric {
    #[default]
    Requests,
    Latency,
    Tokens,
}

impl TrendMetric {
    const ALL: [Self; 3] = [Self::Requests, Self::Latency, Self::Tokens];

    fn label(self) -> &'static str {
        match self {
            Self::Requests => "请求量",
            Self::Latency => "耗时",
            Self::Tokens => "Token",
        }
    }

    fn bucket_peak(self, bucket: ActivityBucket) -> u64 {
        match self {
            Self::Requests => bucket.requests as u64,
            Self::Latency => bucket.latency.map_or(0, |latency| latency.p95_ms),
            Self::Tokens => bucket.tokens(),
        }
    }

    fn axis_label(self, value: u64) -> String {
        match self {
            Self::Requests => value.to_string(),
            Self::Latency => format_duration(value),
            Self::Tokens => format_tokens(value),
        }
    }

    fn empty_label(self) -> &'static str {
        match self {
            Self::Requests => "这段时间暂无对话请求",
            Self::Latency => "这段时间暂无耗时记录",
            Self::Tokens => "这段时间暂无 Token 用量",
        }
    }

    fn bucket_label(self, bucket: ActivityBucket) -> String {
        match self {
            Self::Requests => format!("{} 次请求", bucket.requests),
            Self::Latency => bucket.latency.map_or_else(
                || "暂无耗时记录".into(),
                |latency| {
                    format!(
                        "P50 {} · P95 {}",
                        format_duration(latency.p50_ms),
                        format_duration(latency.p95_ms),
                    )
                },
            ),
            Self::Tokens => format!(
                "输入 {} · 输出 {}",
                bucket.input_tokens, bucket.output_tokens,
            ),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum ProtocolChart {
    #[default]
    Donut,
    Bars,
}

pub(super) fn format_tokens(count: u64) -> String {
    for (unit, suffix) in [
        (1_000_000_000_000, "T"),
        (1_000_000_000, "B"),
        (1_000_000, "M"),
        (1_000, "K"),
    ] {
        // 将在低级单位下会四舍五入为 1000 的值提前进位到上一级单位。
        if count < unit - unit / 20_000 {
            continue;
        }
        let tenth = unit / 10;
        let rounded = count / tenth + u64::from(count % tenth >= tenth / 2);
        let whole = rounded / 10;
        let fraction = rounded % 10;
        return if fraction == 0 {
            format!("{whole}{suffix}")
        } else {
            format!("{whole}.{fraction}{suffix}")
        };
    }
    count.to_string()
}

fn format_duration(ms: u64) -> String {
    if ms >= 1_000 {
        format!("{:.1} s", ms as f64 / 1_000.0)
    } else {
        format!("{ms} ms")
    }
}

impl BridgeApp {
    pub(super) fn sparkline(values: impl AsRef<[f32]> + 'static, color: Hsla) -> impl IntoElement {
        canvas(
            |_, _, _| (),
            move |bounds, (), window, _| paint_sparkline(window, bounds, values.as_ref(), color),
        )
        .size_full()
    }

    pub(super) fn quota_sparkline(
        t: Theme,
        color: Hsla,
        points: Vec<QuotaTrendPoint>,
    ) -> impl IntoElement {
        let count = points.len();
        if count < 2 {
            let baseline = t.text_muted.opacity(0.48);
            return div()
                .id("quota-sparkline-empty")
                .h(px(QUOTA_TREND_HEIGHT))
                .tooltip(Self::text_tooltip(t, "暂无历史走势"))
                .child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, (), window, _| {
                            let plot = bounds.inset(px(SPARKLINE_INSET));
                            let y = plot.bottom() - px(1.0);
                            let mut x = plot.left();
                            while x < plot.right() {
                                let width = px(4.0).min(plot.right() - x);
                                let dash = Bounds::new(point(x, y), size(width, px(1.0)));
                                window.paint_quad(fill(dash, baseline));
                                x += px(7.0);
                            }
                        },
                    )
                    .size_full(),
                );
        }

        let revision = points.last().and_then(|point| point.at).map_or(0, |at| {
            at.timestamp_nanos_opt()
                .unwrap_or_else(|| at.timestamp_millis()) as u64
        });
        let (min, max) = points.iter().fold((1.0_f32, 0.0_f32), |(min, max), point| {
            let remaining = point.remaining as f32;
            (min.min(remaining), max.max(remaining))
        });
        let center = (min + max) * 0.5;
        let span = (max - min).max(0.025) * 1.25;
        let values: Vec<_> = points
            .iter()
            .map(|point| 0.5 + (point.remaining as f32 - center) / span)
            .collect();
        let step = 1.0 / (count - 1) as f32;
        let hover =
            div()
                .absolute()
                .inset_0()
                .px(px(SPARKLINE_INSET))
                .flex()
                .children(values.iter().copied().zip(points).enumerate().map(
                    |(index, (y, point))| {
                        let first = index == 0;
                        let last = index + 1 == count;
                        // 每个悬停热区延伸到相邻采样点中点，首尾两点仅占半步宽。
                        let width = relative(if first || last { step * 0.5 } else { step });
                        let x = if last {
                            1.0
                        } else if first {
                            0.0
                        } else {
                            0.5
                        };
                        let marker_radius = 3.0;
                        let bottom =
                            SPARKLINE_INSET + y * (QUOTA_TREND_HEIGHT - 2.0 * SPARKLINE_INSET);
                        div()
                            .id(("quota-sample", index))
                            .group("quota-sample")
                            .relative()
                            .flex_shrink_0()
                            .w(width)
                            .h_full()
                            .tooltip(move |window, cx| {
                                Self::text_tooltip(t, point.tooltip())(window, cx)
                            })
                            .child(
                                div()
                                    .absolute()
                                    .left(relative(x))
                                    .ml(px(-marker_radius))
                                    .bottom(px(bottom - marker_radius))
                                    .size(px(marker_radius * 2.0))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(t.bg_card)
                                    .bg(color)
                                    .opacity(0.0)
                                    .group_hover("quota-sample", |style| style.opacity(1.0)),
                            )
                    },
                ));

        div()
            .id(("quota-sparkline", revision))
            .relative()
            .h(px(QUOTA_TREND_HEIGHT))
            .child(Self::sparkline(values, color))
            .child(hover)
    }

    pub(super) fn token_sparkline(
        t: Theme,
        activity: &RequestActivity,
        range: ActivityRange,
    ) -> Option<AnyElement> {
        let values = activity.buckets.map(ActivityBucket::tokens);
        let peak = values.into_iter().max().unwrap_or(0);
        (peak > 0).then(|| {
            div()
                .id("kpi-token-trend")
                .w(px(54.0))
                .h(px(28.0))
                .flex_shrink_0()
                .tooltip(Self::text_tooltip(
                    t,
                    format!("近 {} 小时 Token 用量趋势", range.hours()),
                ))
                .child(Self::sparkline(
                    values.map(|value| chart_fraction(value, peak)),
                    t.accent,
                ))
                .into_any_element()
        })
    }

    pub(super) fn account_request_sparkline(
        t: Theme,
        index: usize,
        values: [u64; ACTIVITY_BUCKETS],
    ) -> Option<AnyElement> {
        let peak = values.into_iter().max().unwrap_or(0);
        (peak > 0).then(|| {
            div()
                .id(("account-request-trend", index))
                .flex_1()
                .min_w(px(24.0))
                .max_w(px(108.0))
                .h(px(23.0))
                .tooltip(Self::text_tooltip(t, "本次运行 · 最近 24 个小时的请求量"))
                .child(Self::sparkline(
                    values.map(|value| chart_fraction(value, peak)),
                    t.series_blue,
                ))
                .into_any_element()
        })
    }

    pub(super) fn request_analysis(
        &self,
        t: Theme,
        activity: &RequestActivity,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> Div {
        let range = self.activity_range;
        let metric = self.trend_metric;
        let range_control = Self::segmented_wrap(
            t,
            "activity-range-seg",
            ActivityRange::ALL
                .into_iter()
                .map(|option| {
                    Self::seg_item(
                        t,
                        format!("activity-range-{}", option.hours()),
                        format!("{} 小时", option.hours()),
                        range == option,
                        move |this, _, _| this.activity_range = option,
                        cx,
                    )
                })
                .collect(),
        );
        let metric_control = Self::segmented_wrap(
            t,
            "activity-metric-seg",
            TrendMetric::ALL
                .into_iter()
                .enumerate()
                .map(|(index, option)| {
                    Self::seg_item(
                        t,
                        format!("activity-metric-{index}"),
                        option.label(),
                        metric == option,
                        move |this, _, _| this.trend_metric = option,
                        cx,
                    )
                })
                .collect(),
        );

        let peak = activity
            .buckets
            .iter()
            .copied()
            .map(|bucket| metric.bucket_peak(bucket))
            .max()
            .unwrap_or(0);
        let plot = if peak == 0 {
            Self::empty_state(t, "icons/chart-square-bold.svg", metric.empty_label())
                .min_h(px(156.0))
        } else {
            Self::trend_plot(t, &activity.buckets, metric, range, now, peak)
        };
        let interval = range.bucket_minutes();
        let caption = match metric {
            TrendMetric::Requests => {
                format!("每柱 {interval} 分钟 · 按保留的记录统计，重试分别计数")
            }
            TrendMetric::Latency => format!("每点 {interval} 分钟 · 统计有耗时记录的请求"),
            TrendMetric::Tokens => format!("每柱 {interval} 分钟 · 汇总记录中的输入和输出 Token"),
        };
        let chart_id = range as usize * TrendMetric::ALL.len() + metric as usize;
        let chart = div().flex().flex_col().gap(px(10.0)).child(plot).child(
            div()
                .text_size(px(11.0))
                .text_color(t.text_muted)
                .child(caption),
        );

        Self::card(t)
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(Self::section_header(t, "请求分析").child(range_control))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .flex_wrap()
                    .gap(px(8.0))
                    .child(metric_control)
                    .child(motion::layout(div().id("activity-summary").child(
                        motion::fade_in(
                            ("activity-chart-summary", chart_id),
                            motion::CONTENT_FADE_DURATION,
                            Self::trend_summary(t, activity, metric),
                        ),
                    ))),
            )
            .child(motion::fade_in(
                ("activity-chart", chart_id),
                motion::CONTENT_FADE_DURATION,
                chart,
            ))
    }

    fn trend_summary(t: Theme, activity: &RequestActivity, metric: TrendMetric) -> Div {
        match metric {
            TrendMetric::Requests => Self::value_with_unit(
                t,
                motion::rolling_count("trend-requests", activity.requests as u64),
                Some("次对话请求"),
                23.0,
                t.text_primary,
            ),
            TrendMetric::Latency => {
                let latency_value = |id, ms: Option<u64>| {
                    motion::rolling_number(
                        id,
                        ms.map_or_else(|| "—".into(), format_duration),
                        ms.map(|ms| ms as f64),
                    )
                };
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(Self::series_value(
                        t,
                        t.series_blue,
                        "P50",
                        latency_value("trend-p50", activity.latency.map(|latency| latency.p50_ms)),
                    ))
                    .child(Self::series_value(
                        t,
                        t.accent,
                        "P95",
                        latency_value("trend-p95", activity.latency.map(|latency| latency.p95_ms)),
                    ))
            }
            TrendMetric::Tokens => div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(Self::series_value(
                    t,
                    t.accent,
                    "输入",
                    motion::rolling_number(
                        "trend-input-tokens",
                        format_tokens(activity.input_tokens),
                        activity.input_tokens as f64,
                    ),
                ))
                .child(Self::series_value(
                    t,
                    t.series_blue,
                    "输出",
                    motion::rolling_number(
                        "trend-output-tokens",
                        format_tokens(activity.output_tokens),
                        activity.output_tokens as f64,
                    ),
                )),
        }
    }

    fn series_value(t: Theme, color: Hsla, label: &'static str, value: impl IntoElement) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(5.0))
            .text_size(px(11.5))
            .child(div().size(px(7.0)).rounded_full().bg(color))
            .child(div().text_color(t.text_muted).child(label))
            .child(
                div()
                    .font(numeric_font())
                    .font_weight(FontWeight::MEDIUM)
                    .child(value),
            )
    }

    fn trend_plot(
        t: Theme,
        buckets: &[ActivityBucket; ACTIVITY_BUCKETS],
        metric: TrendMetric,
        range: ActivityRange,
        now: DateTime<Utc>,
        peak: u64,
    ) -> Div {
        let step = peak.div_ceil(3).max(1);
        let ceiling = step.saturating_mul(3);
        let start = now - TimeDelta::hours(range.hours());
        let interval = range.bucket_minutes();
        let axis_label = |label: String| {
            div()
                .text_size(px(10.5))
                .text_color(t.text_muted)
                .child(label)
        };

        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .gap(px(9.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .justify_between()
                            .items_end()
                            .w(px(AXIS_WIDTH))
                            .flex_shrink_0()
                            .children((0..=3).rev().map(|index| {
                                axis_label(metric.axis_label(step.saturating_mul(index)))
                                    .font(numeric_font())
                            })),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w(px(0.0))
                            .h(px(PLOT_HEIGHT))
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .flex_col()
                                    .justify_between()
                                    .children((0..4).map(|_| div().h(px(1.0)).bg(t.border_subtle))),
                            )
                            .child(div().size_full().flex().children(
                                buckets.iter().copied().enumerate().map(|(index, bucket)| {
                                    let from = start + TimeDelta::minutes(index as i64 * interval);
                                    let until = from + TimeDelta::minutes(interval);
                                    let tooltip = format!(
                                        "{}–{} · {}",
                                        from.with_timezone(&Local).format("%m-%d %H:%M"),
                                        until.with_timezone(&Local).format("%H:%M"),
                                        metric.bucket_label(bucket),
                                    );
                                    div()
                                        .id(("activity-bucket", index))
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .h_full()
                                        .flex()
                                        .items_end()
                                        .px(px(1.5))
                                        .rounded(px(5.0))
                                        .hover(move |style| style.bg(t.accent_soft))
                                        .tooltip(Self::text_tooltip(t, tooltip))
                                        .when(metric != TrendMetric::Latency, |el| {
                                            el.child(Self::trend_bar(t, bucket, metric, ceiling))
                                        })
                                }),
                            ))
                            .when(metric == TrendMetric::Latency, |el| {
                                el.child(Self::latency_lines(t, buckets, ceiling))
                            }),
                    ),
            )
            .child(
                div()
                    .pl(px(AXIS_WIDTH + 9.0))
                    .flex()
                    .justify_between()
                    .child(axis_label(format!("{} 小时前", range.hours())).font(numeric_font()))
                    .child(axis_label(format!("{} 小时前", range.hours() / 2)).font(numeric_font()))
                    .child(axis_label("现在".into())),
            )
    }

    fn latency_lines(
        t: Theme,
        buckets: &[ActivityBucket; ACTIVITY_BUCKETS],
        ceiling: u64,
    ) -> impl IntoElement {
        let present = buckets.map(|bucket| bucket.latency.is_some());
        let span = present
            .iter()
            .position(|&visible| visible)
            .zip(present.iter().rposition(|&visible| visible))
            .filter(|(first, last)| first < last);
        let targets: [f32; ACTIVITY_BUCKETS * 2] = std::array::from_fn(|index| {
            let value = buckets[index % ACTIVITY_BUCKETS]
                .latency
                .map_or(0, |latency| {
                    if index < ACTIVITY_BUCKETS {
                        latency.p50_ms
                    } else {
                        latency.p95_ms
                    }
                });
            chart_fraction(value, ceiling)
        });
        let reduced = motion::is_reduced_motion();

        motion::spring_values(
            "latency-points",
            targets,
            SpringConfig::GENTLE,
            move |values| {
                let p50 = std::array::from_fn(|index| present[index].then_some(values[index]));
                let p95 = std::array::from_fn(|index| {
                    present[index].then_some(values[ACTIVITY_BUCKETS + index])
                });
                let draw = move |phase| {
                    let progress = motion::ease_out_cubic(phase);
                    div().absolute().inset_0().child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, (), window, _| {
                                let mask = span.filter(|_| progress < 1.0).map(|(first, last)| {
                                    ContentMask {
                                        bounds: latency_reveal_bounds(
                                            bounds, first, last, progress,
                                        ),
                                    }
                                });
                                let opacity = if span.is_none() { progress } else { 1.0 };
                                window.with_content_mask(mask, |window| {
                                    paint_line(window, bounds, p95, t.accent.opacity(opacity), 2.4);
                                    paint_line(
                                        window,
                                        bounds,
                                        p50,
                                        t.series_blue.opacity(opacity),
                                        1.6,
                                    );
                                });
                            },
                        )
                        .size_full(),
                    )
                };
                if reduced {
                    draw(1.0).into_any_element()
                } else {
                    motion::timeline("latency-line-reveal", Duration::from_millis(240), draw)
                        .into_any_element()
                }
            },
        )
        .settle_at_scale(PLOT_HEIGHT - 6.0)
    }

    fn trend_bar(
        t: Theme,
        bucket: ActivityBucket,
        metric: TrendMetric,
        ceiling: u64,
    ) -> AnyElement {
        let peak = metric.bucket_peak(bucket);
        let target_height = if peak > 0 {
            ((PLOT_HEIGHT - 1.0) * chart_fraction(peak, ceiling)).max(2.0)
        } else {
            0.0
        };
        if metric == TrendMetric::Requests {
            return motion::chart_heights(
                "activity-bar-height",
                [target_height],
                move |[height]| {
                    let height = height.max(0.0);
                    div()
                        .w_full()
                        .h(px(height))
                        .rounded_full()
                        .when(height > 0.5, |el| {
                            el.bg(linear_gradient(
                                180.0,
                                linear_color_stop(t.accent, 0.0),
                                linear_color_stop(t.accent_soft, 1.0),
                            ))
                        })
                },
            )
            .into_any_element();
        }

        let total = bucket.input_tokens as f64 + bucket.output_tokens as f64;
        let input_height = if total > 0.0 {
            target_height * (bucket.input_tokens as f64 / total) as f32
        } else {
            0.0
        };
        motion::chart_heights(
            "activity-bar-height",
            [target_height - input_height, input_height],
            move |[output_height, input_height]| {
                let output_height = output_height.max(0.0);
                let input_height = input_height.max(0.0);
                let height = output_height + input_height;
                div()
                    .w_full()
                    .h(px(height))
                    .flex()
                    .flex_col()
                    .rounded(px(4.0))
                    .overflow_hidden()
                    .when(height > 0.5, |el| {
                        el.child(div().w_full().h(px(output_height)).bg(t.series_blue))
                            .child(div().w_full().h(px(input_height)).bg(t.accent))
                    })
            },
        )
        .into_any_element()
    }

    pub(super) fn protocol_distribution(
        &self,
        t: Theme,
        activity: &RequestActivity,
        cx: &mut Context<Self>,
    ) -> Div {
        let chart = self.protocol_chart;
        let chart_control = Self::segmented_wrap(
            t,
            "protocol-chart-seg",
            [
                (ProtocolChart::Donut, "环形"),
                (ProtocolChart::Bars, "条形"),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (option, label))| {
                Self::seg_item(
                    t,
                    format!("protocol-chart-{index}"),
                    label,
                    chart == option,
                    move |this, _, _| this.protocol_chart = option,
                    cx,
                )
            })
            .collect(),
        );
        let protocols = [
            ("Messages", activity.messages, t.accent),
            ("Responses", activity.responses, t.series_pink),
            ("Chat Completions", activity.chat, t.series_blue),
        ];
        let total = activity.requests;
        let is_donut = chart == ProtocolChart::Donut;

        Self::card(t)
            .flex()
            .flex_col()
            .gap(px(11.0))
            .child(Self::section_header(t, "协议分布").child(chart_control))
            .when(is_donut, |el| {
                el.child(Self::protocol_ring(
                    t,
                    total,
                    protocols.map(|(_, count, color)| (count, color)),
                ))
            })
            .children(
                protocols
                    .into_iter()
                    .enumerate()
                    .map(|(index, (label, count, color))| {
                        let share = count as f32 / total.max(1) as f32;
                        let count = motion::rolling_count(("protocol-count", index), count as u64);
                        let percent = motion::rolling_number(
                            ("protocol-percent", index),
                            format!("{:.0}%", share * 100.0),
                            f64::from(share),
                        );
                        motion::layout(
                            div()
                                .id(("protocol-row", index))
                                .flex()
                                .flex_col()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .gap(px(6.0))
                                        .text_size(px(12.0))
                                        .child(
                                            div()
                                                .flex()
                                                .flex_1()
                                                .min_w(px(0.0))
                                                .items_center()
                                                .gap(px(6.0))
                                                .child(
                                                    div()
                                                        .size(px(7.0))
                                                        .flex_shrink_0()
                                                        .rounded_full()
                                                        .bg(color),
                                                )
                                                .child(
                                                    div()
                                                        .min_w(px(0.0))
                                                        .truncate()
                                                        .text_color(t.text_secondary)
                                                        .child(label),
                                                ),
                                        )
                                        .child(motion::layout(
                                            div()
                                                .id("protocol-values")
                                                .flex()
                                                .flex_shrink_0()
                                                .items_center()
                                                .gap(px(6.0))
                                                .font(numeric_font())
                                                .child(
                                                    div()
                                                        .w(px(44.0))
                                                        .flex_shrink_0()
                                                        .flex()
                                                        .justify_end()
                                                        .text_right()
                                                        .font_weight(FontWeight::MEDIUM)
                                                        .child(count),
                                                )
                                                .child(
                                                    div()
                                                        .w(px(32.0))
                                                        .flex_shrink_0()
                                                        .flex()
                                                        .justify_end()
                                                        .text_right()
                                                        .text_size(px(11.0))
                                                        .text_color(t.text_muted)
                                                        .child(percent),
                                                ),
                                        )),
                                )
                                .when(!is_donut, |el| {
                                    el.child(
                                        Self::hatched_track(
                                            t,
                                            format!("protocol-bar-{index}"),
                                            share,
                                            color,
                                        )
                                        .h(px(8.0)),
                                    )
                                }),
                        )
                    }),
            )
    }

    fn protocol_ring(t: Theme, total: usize, slices: [(usize, Hsla); 3]) -> Div {
        let denom = total.max(1) as f32;
        let targets = if total == 0 {
            [0.0; 3]
        } else {
            [
                slices[0].0 as f32 / denom,
                slices[1].0 as f32 / denom,
                slices[2].0 as f32 / denom,
            ]
        };
        let colors = [slices[0].1, slices[1].1, slices[2].1];
        div().flex().justify_center().child(
            div()
                .relative()
                .size(px(106.0))
                .child(
                    motion::spring_values(
                        "protocol-ring-slices",
                        targets,
                        SpringConfig::GENTLE,
                        move |shares| {
                            canvas(
                                |_, _, _| (),
                                move |bounds, (), window, _| {
                                    paint_ring_shares(
                                        window,
                                        bounds,
                                        t,
                                        [
                                            (shares[0], colors[0]),
                                            (shares[1], colors[1]),
                                            (shares[2], colors[2]),
                                        ],
                                    );
                                },
                            )
                            .size_full()
                        },
                    )
                    .settle_at_scale(106.0 * TAU * 0.5),
                )
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_size(px(21.0))
                                .font(numeric_font())
                                .font_weight(FontWeight::MEDIUM)
                                .child(motion::rolling_count("protocol-total", total as u64)),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(t.text_muted)
                                .child("次请求"),
                        ),
                ),
        )
    }
}

fn chart_fraction(value: u64, ceiling: u64) -> f32 {
    (value as f64 / ceiling as f64) as f32
}

fn paint_sparkline(window: &mut Window, bounds: Bounds<Pixels>, values: &[f32], color: Hsla) {
    let plot = bounds.inset(px(SPARKLINE_INSET));
    if values.is_empty() || plot.is_empty() {
        return;
    }
    if values.len() == 1 {
        let mark = point(plot.right(), plot.bottom() - plot.size.height * values[0]);
        let radius = px(1.75);
        window.paint_quad(fill(circle_bounds(mark, radius), color).corner_radii(radius));
        return;
    }

    let mut area = PathBuilder::fill();
    let mut line = PathBuilder::stroke(px(1.3));
    area.move_to(point(plot.left(), plot.bottom()));
    for (index, &value) in values.iter().enumerate() {
        let x = index as f32 / (values.len() - 1) as f32;
        let marker = point(
            plot.left() + plot.size.width * x,
            plot.bottom() - plot.size.height * value,
        );
        area.line_to(marker);
        if index == 0 {
            line.move_to(marker);
        } else {
            line.line_to(marker);
        }
    }
    area.line_to(point(plot.right(), plot.bottom()));
    area.close();
    if let Ok(path) = area.build() {
        window.paint_path(path, color.opacity(0.12));
    }
    if let Ok(path) = line.build() {
        window.paint_path(path, color);
    }
}

fn circle_bounds(center: Point<Pixels>, radius: Pixels) -> Bounds<Pixels> {
    let diameter = radius * 2.0;
    Bounds::new(
        point(center.x - radius, center.y - radius),
        size(diameter, diameter),
    )
}

fn latency_bucket_x(bounds: Bounds<Pixels>, index: usize) -> Pixels {
    bounds.left() + bounds.size.width * ((index as f32 + 0.5) / ACTIVITY_BUCKETS as f32)
}

fn latency_reveal_bounds(
    bounds: Bounds<Pixels>,
    first: usize,
    last: usize,
    progress: f32,
) -> Bounds<Pixels> {
    let start = latency_bucket_x(bounds, first);
    // Include the last marker's radius so the reveal ends beyond the stroke.
    let end = (latency_bucket_x(bounds, last) + px(3.0)).min(bounds.right());
    let right = start + (end - start) * progress;
    Bounds::new(
        bounds.origin,
        size(right - bounds.left(), bounds.size.height),
    )
}

fn paint_line(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    values: [Option<f32>; ACTIVITY_BUCKETS],
    color: Hsla,
    width: f32,
) {
    let mut path = PathBuilder::stroke(px(width));
    let mut previous = false;
    let mut has_segment = false;
    let marker_radius = px(width + 0.5);
    let mut points = [None; ACTIVITY_BUCKETS];
    for (index, value) in values.into_iter().enumerate() {
        let Some(value) = value else {
            previous = false;
            continue;
        };
        let y = 1.0 - value.clamp(0.0, 1.0);
        let marker = point(
            latency_bucket_x(bounds, index),
            bounds.origin.y + px(3.0) + (bounds.size.height - px(6.0)) * y,
        );
        if previous {
            path.line_to(marker);
            has_segment = true;
        } else {
            path.move_to(marker);
        }
        previous = true;
        points[index] = Some(marker);
    }
    if has_segment && let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
    for marker in points.into_iter().flatten() {
        window.paint_quad(
            fill(circle_bounds(marker, marker_radius), color).corner_radii(marker_radius),
        );
    }
}

fn paint_ring_shares(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    t: Theme,
    slices: [(f32, Hsla); 3],
) {
    let center = bounds.center();
    let radius = bounds.size.width * 0.5;
    let track = circle_bounds(center, radius);
    window.paint_quad(fill(track, t.border_subtle).corner_radii(radius));

    let active_sum: f32 = slices.iter().map(|(share, _)| share.max(0.0)).sum();
    let scale = if active_sum > 1.0 {
        1.0 / active_sum
    } else {
        1.0
    };
    let mut angle = -FRAC_PI_2;
    for (raw_share, color) in slices {
        let share = raw_share.max(0.0) * scale;
        if share <= 0.001 {
            continue;
        }
        if share >= 0.998 {
            window.paint_quad(fill(track, color).corner_radii(radius));
            break;
        }
        let span = TAU * share;
        let inset = (span * 0.18).min(0.035);
        let on_arc = |angle: f32| {
            point(
                center.x + radius * angle.cos(),
                center.y + radius * angle.sin(),
            )
        };
        let start = angle + inset;
        let end = angle + span - inset;
        let mut sector = PathBuilder::fill();
        sector.move_to(center);
        sector.line_to(on_arc(start));
        sector.arc_to(
            point(radius, radius),
            px(0.0),
            end - start > PI,
            true,
            on_arc(end),
        );
        sector.close();
        if let Ok(path) = sector.build() {
            window.paint_path(path, color);
        }
        angle += span;
    }

    let inner_radius = radius - px(12.0);
    window.paint_quad(
        fill(circle_bounds(center, inner_radius), t.bg_card).corner_radii(inner_radius),
    );
}

#[cfg(test)]
mod tests {
    use super::{Bounds, format_tokens, latency_bucket_x, latency_reveal_bounds, point, px, size};

    #[test]
    fn latency_reveal_starts_at_the_first_sample_in_a_sparse_plot() {
        let bounds = Bounds::new(point(px(20.0), px(10.0)), size(px(240.0), px(136.0)));
        let start = latency_reveal_bounds(bounds, 21, 23, 0.0);
        let middle = latency_reveal_bounds(bounds, 21, 23, 0.5);
        let finish = latency_reveal_bounds(bounds, 21, 23, 1.0);

        assert_eq!(start.right(), latency_bucket_x(bounds, 21));
        assert!(middle.right() > start.right());
        assert!(middle.right() < latency_bucket_x(bounds, 23));
        assert!(finish.right() > latency_bucket_x(bounds, 23));
        assert!(finish.right() <= bounds.right());
    }

    #[test]
    fn token_counts_use_compact_decimal_units() {
        for (count, expected) in [
            (0, "0"),
            (987, "987"),
            (999, "999"),
            (1_000, "1K"),
            (1_250, "1.3K"),
            (530_987, "531K"),
            (999_949, "999.9K"),
            (999_950, "1M"),
            (2_500_000, "2.5M"),
            (1_200_000_000, "1.2B"),
            (1_000_000_000_000, "1T"),
            (u64::MAX, "18446744.1T"),
        ] {
            assert_eq!(format_tokens(count), expected);
        }
    }
}
