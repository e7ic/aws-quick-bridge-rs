mod context;

use super::charts::format_tokens;
use super::components::{BtnVariant, Theme};
use super::motion::{self, SpringConfig};
use super::{BridgeApp, Tab, numeric_font};
use crate::pool::{RequestProtocol, USAGE_RETENTION_DAYS, UsageHistory, UsageTotals};
use chrono::{Days, Local, NaiveDate, Timelike};
use gpui::{
    AnyElement, Context, Div, FocusHandle, FontWeight, Hsla, KeyDownEvent, ScrollHandle,
    SharedString, Transformation, Window, anchored, deferred, div, prelude::*, px, radians,
    relative, svg,
};
use std::collections::{BTreeMap, BTreeSet};
use std::f32::consts::PI;
use std::time::Instant;

const RANK_LIMIT: usize = 8;
const PLOT_HEIGHT: f32 = 146.0;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum UsageRange {
    #[default]
    Today,
    Week,
    Month,
}

impl UsageRange {
    const ALL: [Self; 3] = [Self::Today, Self::Week, Self::Month];

    fn days(self) -> u64 {
        match self {
            Self::Today => 1,
            Self::Week => 7,
            Self::Month => USAGE_RETENTION_DAYS,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Today => "今天",
            Self::Week => "近 7 天",
            Self::Month => "近 30 天",
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum UsageMetric {
    #[default]
    Tokens,
    Cache,
    Calls,
}

impl UsageMetric {
    const ALL: [Self; 3] = [Self::Tokens, Self::Cache, Self::Calls];

    fn label(self) -> &'static str {
        match self {
            Self::Tokens => "Token 用量",
            Self::Cache => "缓存读取",
            Self::Calls => "调用次数",
        }
    }

    fn value(self, totals: UsageTotals) -> u64 {
        match self {
            Self::Tokens => totals.tokens(),
            Self::Cache => totals.cache_read,
            Self::Calls => totals.calls,
        }
    }

    fn color(self, t: Theme) -> Hsla {
        match self {
            Self::Tokens => t.accent,
            Self::Cache => t.series_blue,
            Self::Calls => t.green_text,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UsageDimension {
    Account,
    Model,
    Protocol,
}

impl UsageDimension {
    fn label(self) -> &'static str {
        match self {
            Self::Account => "账号",
            Self::Model => "实际模型",
            Self::Protocol => "协议",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum UsageChoice {
    Account(Option<String>),
    Model(Option<String>),
    Protocol(Option<RequestProtocol>),
}

#[derive(Default)]
pub(super) struct UsageView {
    pub(super) dropdown: Option<UsageDropdown>,
    range: UsageRange,
    metric: UsageMetric,
    filters: UsageFilters,
    context_page: usize,
}

#[derive(Default)]
struct UsageFilters {
    account: Option<String>,
    model: Option<String>,
    protocol: Option<RequestProtocol>,
}

impl UsageFilters {
    fn selected(&self, choice: &UsageChoice) -> bool {
        match choice {
            UsageChoice::Account(value) => self.account == *value,
            UsageChoice::Model(value) => self.model == *value,
            UsageChoice::Protocol(value) => self.protocol == *value,
        }
    }

    fn select(&mut self, choice: UsageChoice) {
        match choice {
            UsageChoice::Account(value) => self.account = value,
            UsageChoice::Model(value) => self.model = value,
            UsageChoice::Protocol(value) => self.protocol = value,
        }
    }

    fn matches(&self, account: &str, model: &str, protocol: RequestProtocol) -> bool {
        self.account.as_deref().is_none_or(|key| key == account)
            && self.model.as_deref().is_none_or(|id| id == model)
            && self.protocol.is_none_or(|selected| selected == protocol)
    }
}

pub(super) struct UsageDropdown {
    dimension: UsageDimension,
    focus: FocusHandle,
    trigger_focus: Option<FocusHandle>,
    scroll: ScrollHandle,
    pub(super) closing: Option<Instant>,
}

impl UsageDropdown {
    pub(super) fn is_open(&self) -> bool {
        self.closing.is_none()
    }
}

struct UsageSummary<'a> {
    start: NaiveDate,
    timeline: Vec<UsageTotals>,
    total: UsageTotals,
    accounts: BTreeMap<&'a str, UsageTotals>,
    models: BTreeMap<&'a str, UsageTotals>,
    known_accounts: BTreeSet<&'a str>,
    known_models: BTreeSet<&'a str>,
    known_protocols: BTreeSet<RequestProtocol>,
}

impl<'a> UsageSummary<'a> {
    fn query(
        history: &'a UsageHistory,
        today: NaiveDate,
        range: UsageRange,
        filters: &UsageFilters,
    ) -> Self {
        let start = today - Days::new(range.days() - 1);
        let oldest = today - Days::new(USAGE_RETENTION_DAYS - 1);
        let hourly = range == UsageRange::Today;
        let mut summary = Self {
            start,
            timeline: vec![UsageTotals::default(); if hourly { 24 } else { range.days() as usize }],
            total: UsageTotals::default(),
            accounts: BTreeMap::new(),
            models: BTreeMap::new(),
            known_accounts: BTreeSet::new(),
            known_models: BTreeSet::new(),
            known_protocols: BTreeSet::new(),
        };
        for (bucket, totals) in &history.buckets {
            if !(oldest..=today).contains(&bucket.date) {
                continue;
            }
            summary.known_accounts.insert(&bucket.account);
            summary.known_models.insert(&bucket.model);
            summary.known_protocols.insert(bucket.protocol);
            if bucket.date < start
                || !filters.matches(&bucket.account, &bucket.model, bucket.protocol)
            {
                continue;
            }
            summary.total.add(*totals);
            summary
                .accounts
                .entry(&bucket.account)
                .or_default()
                .add(*totals);
            summary
                .models
                .entry(&bucket.model)
                .or_default()
                .add(*totals);
            let index = if hourly {
                bucket.hour.map(|hour| hour as usize)
            } else {
                Some((bucket.date - start).num_days() as usize)
            };
            if let Some(point) = index.and_then(|index| summary.timeline.get_mut(index)) {
                point.add(*totals);
            }
        }
        summary
    }
}

fn protocol_label(protocol: RequestProtocol) -> &'static str {
    match protocol {
        RequestProtocol::Messages => "Messages",
        RequestProtocol::Responses => "Responses",
        RequestProtocol::Chat => "Chat Completions",
    }
}

fn model_label(model: &str) -> &str {
    if model.is_empty() {
        "未上报模型"
    } else {
        model
    }
}

impl BridgeApp {
    pub(super) fn render_usage(&self, t: Theme, compact: bool, cx: &mut Context<Self>) -> Div {
        let usage = &self.usage;
        let history = self.pool.usage_history();
        let now = Local::now();
        let summary = UsageSummary::query(&history, now.date_naive(), usage.range, &usage.filters);
        let mut accounts: BTreeMap<_, _> = summary
            .known_accounts
            .iter()
            .map(|&key| (key, history.account_label(key).unwrap_or(key)))
            .collect();
        accounts.extend(
            self.snapshot
                .accounts
                .iter()
                .filter(|account| summary.known_accounts.contains(account.key.as_str()))
                .map(|a| (a.key.as_str(), a.label.as_str())),
        );
        let range_picker = Self::segmented_wrap(
            t,
            "usage-range-seg",
            UsageRange::ALL
                .into_iter()
                .enumerate()
                .map(|(index, range)| {
                    Self::seg_item(
                        t,
                        format!("usage-range-{index}"),
                        range.label(),
                        usage.range == range,
                        move |this, _, _| {
                            this.usage.range = range;
                            this.usage.context_page = 0;
                        },
                        cx,
                    )
                })
                .collect(),
        );
        let has_history = !summary.known_models.is_empty();
        let total = summary.total;
        let tokens = total.tokens();
        let coverage = history.started_on.map_or_else(
            || "本地保留最近 30 天 · 首次对话后开始记录".to_owned(),
            |date| format!("本地保留最近 30 天 · 记录始于 {}", date.format("%Y-%m-%d")),
        );
        let page = div().flex().flex_col().gap(px(12.0)).child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .child(Self::page_heading(t, Tab::Usage.title()))
                .when(has_history, |el| el.child(range_picker)),
        );
        let page = if has_history {
            page.child(motion::layout(
                self.usage_filters(
                    t,
                    &accounts,
                    &summary.known_models,
                    &summary.known_protocols,
                    compact,
                    cx,
                )
                .id("usage-filters"),
            ))
        } else {
            page
        };
        if total.calls == 0 {
            let (title, detail) = if has_history {
                ("所选范围暂无用量", "可切换时间范围或重置筛选。")
            } else {
                (
                    "暂无用量记录",
                    "连接客户端并发送对话后，这里会显示用量趋势、排行和请求上下文。",
                )
            };
            return page
                .child(
                    Self::empty_state(t, Tab::Usage.icon_path(), title)
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(t.text_muted)
                                .child(detail),
                        )
                        .when(!has_history, |el| {
                            el.child(Self::button(
                                t,
                                "usage-connect-client",
                                Some("icons/arrow-right-up-bold.svg"),
                                false,
                                "客户端接入",
                                BtnVariant::Secondary,
                                |this, _, _| this.switch_tab(Tab::Clients),
                                cx,
                            ))
                        }),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(t.text_muted)
                        .child(coverage),
                );
        }
        page.child(
            div()
                .flex()
                .gap(px(8.0))
                .child(motion::layout(
                    Self::kpi_card(
                        t,
                        "icons/cpu-bolt-bold.svg",
                        "Token 用量",
                        motion::rolling_number(
                            "usage-tokens",
                            format_tokens(tokens),
                            tokens as f64,
                        ),
                        format!(
                            "输入 {} · 输出 {}",
                            format_tokens(total.input),
                            format_tokens(total.output)
                        ),
                        None,
                    )
                    .id("usage-token-total")
                    .tooltip(Self::text_tooltip(
                        t,
                        format!(
                            "输入 {} · 输出 {} · 合计 {}",
                            total.input, total.output, tokens
                        ),
                    )),
                ))
                .child(motion::layout(
                    Self::kpi_card(
                        t,
                        "icons/server-bold.svg",
                        "缓存读取",
                        motion::rolling_number(
                            "usage-cache-read",
                            format_tokens(total.cache_read),
                            total.cache_read as f64,
                        ),
                        "计入输入 Token",
                        None,
                    )
                    .id("usage-cache-total")
                    .tooltip(Self::text_tooltip(t, total.cache_read.to_string())),
                ))
                .child(motion::layout(
                    Self::kpi_card(
                        t,
                        Tab::Usage.icon_path(),
                        "调用次数",
                        motion::rolling_number(
                            "usage-calls",
                            format_tokens(total.calls),
                            total.calls as f64,
                        ),
                        "换号重试分别计次",
                        None,
                    )
                    .id("usage-call-total")
                    .tooltip(Self::text_tooltip(t, format!("{} 次调用", total.calls))),
                )),
        )
        .child(motion::layout(
            self.usage_trend(t, &summary, now.hour(), cx)
                .id("usage-trends"),
        ))
        .child(
            div()
                .flex()
                .when(compact, |el| el.flex_col())
                .gap(px(12.0))
                .child(motion::layout(
                    Self::usage_ranking(
                        t,
                        "账号排行",
                        usage.metric,
                        compact,
                        summary.accounts.into_iter().map(|(key, totals)| {
                            (key, accounts.get(key).copied().unwrap_or(key), totals)
                        }),
                    )
                    .id("usage-accounts"),
                ))
                .child(motion::layout(
                    Self::usage_ranking(
                        t,
                        "实际模型排行",
                        usage.metric,
                        compact,
                        summary
                            .models
                            .into_iter()
                            .map(|(model, totals)| (model, model_label(model), totals)),
                    )
                    .id("usage-models"),
                )),
        )
        .child(motion::layout(
            self.usage_contexts(t, &history, summary.start, now.date_naive(), compact, cx)
                .id("usage-contexts"),
        ))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(t.text_muted)
                .child(coverage),
        )
    }

    fn usage_filters(
        &self,
        t: Theme,
        accounts: &BTreeMap<&str, &str>,
        models: &BTreeSet<&str>,
        protocols: &BTreeSet<RequestProtocol>,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let filters = &self.usage.filters;
        let mut label_counts = BTreeMap::new();
        for &label in accounts.values() {
            *label_counts.entry(label).or_insert(0) += 1;
        }
        let mut account_choices: Vec<_> = accounts
            .iter()
            .map(|(&key, &label)| {
                let display = if label_counts[label] > 1 {
                    format!("{label} · {key}")
                } else {
                    label.to_owned()
                };
                (UsageChoice::Account(Some(key.to_owned())), display)
            })
            .collect();
        account_choices.sort_unstable_by(|a, b| a.1.cmp(&b.1));
        if let Some(key) = filters
            .account
            .as_deref()
            .filter(|key| !accounts.contains_key(key))
        {
            account_choices.push((
                UsageChoice::Account(Some(key.to_owned())),
                "已移除账号".into(),
            ));
        }
        let selected_model = filters
            .model
            .as_deref()
            .filter(|model| !models.contains(model));
        let model_choices = models
            .iter()
            .copied()
            .chain(selected_model)
            .map(|model| {
                (
                    UsageChoice::Model(Some(model.to_owned())),
                    model_label(model).to_owned(),
                )
            })
            .collect();
        let protocol_choices = protocols
            .iter()
            .copied()
            .chain(
                filters
                    .protocol
                    .filter(|protocol| !protocols.contains(protocol)),
            )
            .map(|protocol| {
                (
                    UsageChoice::Protocol(Some(protocol)),
                    protocol_label(protocol).to_owned(),
                )
            })
            .collect();
        let pickers = [
            (
                UsageDimension::Account,
                UsageChoice::Account(None),
                account_choices,
            ),
            (
                UsageDimension::Model,
                UsageChoice::Model(None),
                model_choices,
            ),
            (
                UsageDimension::Protocol,
                UsageChoice::Protocol(None),
                protocol_choices,
            ),
        ];
        let filtered =
            filters.account.is_some() || filters.model.is_some() || filters.protocol.is_some();
        let mut picker_views = pickers.into_iter().map(|(dimension, all, choices)| {
            let choices = std::iter::once((all, "全部".to_owned()))
                .chain(choices)
                .collect();
            self.usage_picker(t, dimension, choices, cx)
        });
        let account_picker = picker_views.next().expect("account picker");
        let model_picker = picker_views.next().expect("model picker");
        let protocol_picker = picker_views.next().expect("protocol picker");
        let clear_button = filtered.then(|| {
            Self::button(
                t,
                "usage-clear-filters",
                None,
                false,
                "重置筛选",
                BtnVariant::Ghost,
                |this, _, _| {
                    this.usage.filters = UsageFilters::default();
                    this.usage.dropdown = None;
                    this.usage.context_page = 0;
                },
                cx,
            )
        });
        let card = Self::card(t).p(px(10.0)).flex().gap(px(8.0));
        if compact {
            card.flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(account_picker)
                        .child(model_picker),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(protocol_picker)
                        .children(clear_button),
                )
        } else {
            card.items_center()
                .child(account_picker)
                .child(model_picker)
                .child(protocol_picker)
                .children(clear_button)
        }
    }

    fn dismiss_usage_dropdown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dropdown) = self.usage.dropdown.as_mut()
            && dropdown.closing.is_none()
        {
            dropdown.closing = Some(Instant::now());
            if let Some(trigger) = dropdown.trigger_focus.take() {
                window.focus(&trigger);
            } else {
                window.blur();
            }
            cx.notify();
        }
    }

    pub(super) fn usage_dropdown_backdrop(cx: &mut Context<Self>) -> AnyElement {
        deferred(
            div()
                .id("usage-picker-backdrop")
                .absolute()
                .inset_0()
                .occlude()
                .on_any_mouse_down(cx.listener(|this, _, window, cx| {
                    this.dismiss_usage_dropdown(window, cx);
                    cx.stop_propagation();
                }))
                .on_scroll_wheel(cx.listener(|this, _, window, cx| {
                    this.dismiss_usage_dropdown(window, cx);
                    cx.stop_propagation();
                })),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn usage_picker(
        &self,
        t: Theme,
        dimension: UsageDimension,
        choices: Vec<(UsageChoice, String)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = choices
            .iter()
            .find(|(choice, _)| self.usage.filters.selected(choice))
            .map_or("全部", |(_, label)| label.as_str())
            .to_owned();
        let open = self
            .usage
            .dropdown
            .as_ref()
            .filter(|d| d.dimension == dimension);
        let is_open = open.is_some_and(UsageDropdown::is_open);
        let show = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            if this
                .usage
                .dropdown
                .as_ref()
                .is_some_and(|d| d.dimension == dimension && d.is_open())
            {
                this.dismiss_usage_dropdown(window, cx);
                return;
            }
            let focus = cx.focus_handle();
            let trigger_focus = window.focused(cx);
            let scroll = this
                .usage
                .dropdown
                .as_ref()
                .filter(|dropdown| dropdown.dimension == dimension)
                .map_or_else(ScrollHandle::new, |dropdown| dropdown.scroll.clone());
            window.focus(&focus);
            this.usage.dropdown = Some(UsageDropdown {
                dimension,
                focus,
                trigger_focus,
                scroll,
                closing: None,
            });
            cx.notify();
        };
        let picker = div()
            .id(("usage-picker-container", dimension as usize))
            .relative()
            .min_w(px(0.0))
            .flex_1()
            .child(motion::layout(
                div()
                    .id(("usage-picker", dimension as usize))
                    .min_w(px(0.0))
                    .h(px(34.0))
                    .px(px(10.0))
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(if is_open {
                        t.border_strong
                    } else {
                        t.border_subtle
                    })
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .focusable()
                    .tab_stop(true)
                    .focus(move |style| style.bg(t.bg_hover))
                    .hover(move |style| style.bg(t.bg_hover))
                    .active(|style| style.opacity(0.84))
                    .text_size(px(12.0))
                    .on_click(cx.listener(move |this, _, window, cx| show(this, window, cx)))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space" | "down") {
                            show(this, window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .when(!is_open && label != "全部", |el| {
                        el.tooltip(Self::text_tooltip(t, label.clone()))
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(t.text_muted)
                            .child(dimension.label()),
                    )
                    .child(div().min_w(px(0.0)).flex_1().truncate().child(label))
                    .child(motion::spring_value(
                        ("usage-arrow", dimension as usize),
                        if is_open { 1.0 } else { 0.0 },
                        SpringConfig::SNAPPY,
                        move |progress| {
                            svg()
                                .path("icons/alt-arrow-down-bold.svg")
                                .size(px(14.0))
                                .flex_shrink_0()
                                .text_color(motion::lerp_hsla(
                                    t.text_muted,
                                    t.text_primary,
                                    progress,
                                ))
                                .with_transformation(Transformation::rotate(radians(PI * progress)))
                        },
                    )),
            ))
            .when_some(open, |el, dropdown| {
                el.child(
                    div()
                        .absolute()
                        .top(relative(1.0))
                        .left_0()
                        .mt(px(4.0))
                        .size(px(0.0))
                        .child(
                            deferred(anchored().snap_to_window().child(Self::usage_options(
                                t,
                                dropdown,
                                choices,
                                &self.usage.filters,
                                cx,
                            )))
                            .with_priority(2),
                        ),
                )
            });
        motion::layout(picker).into_any_element()
    }

    fn usage_options(
        t: Theme,
        dropdown: &UsageDropdown,
        choices: Vec<(UsageChoice, String)>,
        filters: &UsageFilters,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_open = dropdown.is_open();
        let dimension = dropdown.dimension;
        let card = Self::card(t)
            .id("usage-options")
            .w(px(288.0))
            .max_h(px(240.0))
            .track_scroll(&dropdown.scroll)
            .p(px(4.0))
            .rounded(px(9.0))
            .shadow_md()
            .flex()
            .flex_col()
            .when(!is_open, |card| card.overflow_hidden())
            .when(is_open, |card| {
                card.track_focus(&dropdown.focus)
                    .tab_group()
                    .tab_stop(false)
                    .occlude()
                    .overflow_y_scroll()
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        match event.keystroke.key.as_str() {
                            "escape" => this.dismiss_usage_dropdown(window, cx),
                            "up" => window.focus_prev(),
                            "tab" if event.keystroke.modifiers.shift => window.focus_prev(),
                            "down" | "tab" => window.focus_next(),
                            _ => return,
                        }
                        cx.stop_propagation();
                    }))
            })
            .children(
                choices
                    .into_iter()
                    .enumerate()
                    .map(|(index, (choice, name))| {
                        let selected = filters.selected(&choice);
                        let pick =
                            move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                                if !this.usage.dropdown.as_ref().is_some_and(|menu| {
                                    menu.dimension == dimension && menu.is_open()
                                }) {
                                    return;
                                }
                                this.usage.filters.select(choice.clone());
                                this.usage.context_page = 0;
                                this.dismiss_usage_dropdown(window, cx);
                            };
                        let pick_key = pick.clone();
                        div()
                            .id(("usage-option", index))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .flex_shrink_0()
                            .h(px(30.0))
                            .px(px(8.0))
                            .rounded(px(5.0))
                            .text_size(px(12.0))
                            .when(selected, |el| el.bg(t.bg_hover))
                            .when(is_open, |option| {
                                option
                                    .cursor_pointer()
                                    .focusable()
                                    .tab_stop(true)
                                    .hover(move |style| style.bg(t.bg_hover))
                                    .focus(move |style| style.bg(t.bg_hover))
                                    .tooltip(Self::text_tooltip(t, name.clone()))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        pick(this, window, cx)
                                    }))
                                    .on_key_down(cx.listener(
                                        move |this, event: &KeyDownEvent, window, cx| {
                                            if matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space"
                                            ) {
                                                pick_key(this, window, cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    ))
                            })
                            .child(div().min_w(px(0.0)).flex_1().truncate().child(name))
                            .when(selected, |el| {
                                el.child(Self::icon(
                                    "icons/check-circle-bold.svg",
                                    13.0,
                                    t.series_blue,
                                ))
                            })
                    }),
            );
        motion::slide_fade(
            ("usage-options-anim", dropdown.dimension as usize),
            0.0,
            if is_open { 1.0 } else { 0.0 },
            0.0,
            -6.0,
            if is_open {
                SpringConfig::SMOOTH
            } else {
                SpringConfig::EXIT
            },
            motion::layout(card).anchored(),
        )
    }

    fn usage_trend(
        &self,
        t: Theme,
        summary: &UsageSummary<'_>,
        current_hour: u32,
        cx: &mut Context<Self>,
    ) -> Div {
        let metric = self.usage.metric;
        let range = self.usage.range;
        let control = Self::segmented_wrap(
            t,
            "usage-metric-seg",
            UsageMetric::ALL
                .into_iter()
                .enumerate()
                .map(|(index, option)| {
                    Self::seg_item(
                        t,
                        format!("usage-metric-{index}"),
                        option.label(),
                        metric == option,
                        move |this, _, _| this.usage.metric = option,
                        cx,
                    )
                })
                .collect(),
        );
        let plot = if summary.total.calls == 0 {
            Self::empty_state(t, Tab::Usage.icon_path(), "所选范围暂无调用记录").min_h(px(166.0))
        } else {
            Self::usage_plot(t, summary, range, metric, current_hour)
        };
        let chart_id = (
            "usage-chart",
            ((range as usize * UsageMetric::ALL.len() + metric as usize) << 1)
                | usize::from(summary.total.calls > 0),
        );
        let chart = div().flex().flex_col().gap(px(8.0)).child(plot).child(
            div().text_size(px(11.0)).text_color(t.text_muted).child(
                if range == UsageRange::Today {
                    "按小时汇总 · 本地时间"
                } else {
                    "按天汇总 · 本地时间"
                },
            ),
        );
        Self::card(t)
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(Self::section_header(t, "用量趋势").child(control))
            .child(motion::fade_in(
                chart_id,
                motion::CONTENT_FADE_DURATION,
                chart,
            ))
    }

    fn usage_plot(
        t: Theme,
        summary: &UsageSummary<'_>,
        range: UsageRange,
        metric: UsageMetric,
        current_hour: u32,
    ) -> Div {
        let peak = summary
            .timeline
            .iter()
            .map(|&totals| metric.value(totals))
            .max()
            .unwrap_or(0);
        let step = peak.div_ceil(3).max(1);
        let ceiling = step.saturating_mul(3);
        let count = summary.timeline.len();
        let hourly = range == UsageRange::Today;
        let color = metric.color(t);
        let axis_label = |value: String| {
            div()
                .font(numeric_font())
                .text_size(px(10.5))
                .text_color(t.text_muted)
                .whitespace_nowrap()
                .child(value)
        };
        div()
            .flex()
            .gap(px(8.0))
            .child(
                div()
                    .w(px(48.0))
                    .h(px(PLOT_HEIGHT))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .items_end()
                    .justify_between()
                    .children(
                        (0..=3)
                            .rev()
                            .map(|tick| axis_label(format_tokens(step.saturating_mul(tick)))),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top_0()
                            .h(px(PLOT_HEIGHT))
                            .flex()
                            .flex_col()
                            .justify_between()
                            .children((0..=3).map(|_| div().h(px(1.0)).bg(t.border_subtle))),
                    )
                    .child(div().w_full().flex().children(
                        summary.timeline.iter().enumerate().map(|(index, &totals)| {
                            let date = if hourly {
                                summary.start
                            } else {
                                summary.start + Days::new(index as u64)
                            };
                            let date_label = date.format("%m-%d");
                            let time = if hourly {
                                format!("{date_label} {index:02}:00")
                            } else {
                                date_label.to_string()
                            };
                            let tooltip = format!(
                                "{time} · 输入 {} · 输出 {} · 缓存读取 {} · {} 次调用",
                                totals.input, totals.output, totals.cache_read, totals.calls,
                            );
                            let value = metric.value(totals);
                            let target_height = if value > 0 {
                                ((value as f64 / ceiling as f64) as f32 * PLOT_HEIGHT).max(2.0)
                            } else {
                                0.0
                            };
                            let current = !hourly || index <= current_hour as usize;
                            let tick = count <= 7
                                || [0, count / 4, count / 2, count * 3 / 4, count - 1]
                                    .contains(&index);
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex()
                                .flex_col()
                                .items_center()
                                .child(
                                    div()
                                        .id(("usage-time-bucket", index))
                                        .w_full()
                                        .h(px(PLOT_HEIGHT))
                                        .px(px(1.5))
                                        .rounded(px(4.0))
                                        .flex()
                                        .items_end()
                                        .when(current, |el| {
                                            el.hover(move |s| s.bg(t.bg_hover))
                                                .tooltip(Self::text_tooltip(t, tooltip))
                                        })
                                        .child(motion::chart_heights(
                                            "usage-bar-height",
                                            [target_height],
                                            move |[height]| {
                                                let height = height.max(0.0);
                                                div()
                                                    .w_full()
                                                    .h(px(height))
                                                    .rounded_t(px(3.0))
                                                    .when(height > 0.5, |el| el.bg(color))
                                            },
                                        )),
                                )
                                .when(tick, |el| {
                                    el.child(
                                        axis_label(if hourly {
                                            format!("{index:02}:00")
                                        } else {
                                            date_label.to_string()
                                        })
                                        .pt(px(4.0)),
                                    )
                                })
                        }),
                    )),
            )
    }

    fn usage_ranking<'a>(
        t: Theme,
        title: &'static str,
        metric: UsageMetric,
        compact: bool,
        rows: impl IntoIterator<Item = (&'a str, &'a str, UsageTotals)>,
    ) -> Div {
        let mut rows: Vec<_> = rows
            .into_iter()
            .filter(|row| metric.value(row.2) != 0)
            .collect();
        rows.sort_unstable_by(|a, b| {
            metric
                .value(b.2)
                .cmp(&metric.value(a.2))
                .then_with(|| a.0.cmp(b.0))
        });
        let caption = if rows.len() > RANK_LIMIT {
            format!("{} · 前 {RANK_LIMIT} 项", metric.label())
        } else {
            metric.label().to_owned()
        };
        Self::card(t)
            .when(!compact, |el| el.flex_1())
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .child(
                Self::section_header(t, title).child(
                    div()
                        .text_size(px(11.0))
                        .text_color(t.text_muted)
                        .child(caption),
                ),
            )
            .when(rows.is_empty(), |el| {
                el.child(
                    div()
                        .py(px(12.0))
                        .text_size(px(12.0))
                        .text_color(t.text_muted)
                        .child("暂无排行"),
                )
            })
            .child(
                div()
                    .id(title)
                    .children(rows.into_iter().take(RANK_LIMIT).enumerate().map(
                        |(index, (key, label, totals))| {
                            let tooltip = format!(
                                "{label} · Token {} · 缓存读取 {} · {} 次调用",
                                totals.tokens(),
                                totals.cache_read,
                                totals.calls,
                            );
                            let value = metric.value(totals);
                            motion::layout(
                                div()
                                    .id(SharedString::from(key.to_owned()))
                                    .flex()
                                    .items_center()
                                    .gap(px(9.0))
                                    .h(px(34.0))
                                    .border_t_1()
                                    .border_color(t.border_subtle)
                                    .text_size(px(12.0))
                                    .tooltip(Self::text_tooltip(t, tooltip))
                                    .child(
                                        div()
                                            .size(px(20.0))
                                            .flex_shrink_0()
                                            .rounded(px(5.0))
                                            .bg(t.bg_seg_track)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .font(numeric_font())
                                            .text_color(t.text_muted)
                                            .child((index + 1).to_string()),
                                    )
                                    .child(
                                        div()
                                            .min_w(px(0.0))
                                            .flex_1()
                                            .truncate()
                                            .child(SharedString::from(label.to_owned())),
                                    )
                                    .child(
                                        div()
                                            .font(numeric_font())
                                            .font_weight(FontWeight::MEDIUM)
                                            .flex_shrink_0()
                                            .child(motion::rolling_number(
                                                ("value", metric as usize),
                                                format_tokens(value),
                                                value as f64,
                                            )),
                                    ),
                            )
                        },
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::UsageBucket;

    #[test]
    fn date_ranges_and_combined_filters_feed_every_usage_metric() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        let mut history = UsageHistory::default();
        let mut insert = |days_ago, hour, account: &str, model: &str, protocol, input| {
            history.buckets.insert(
                UsageBucket {
                    date: today - Days::new(days_ago),
                    hour,
                    account: account.to_owned(),
                    model: model.to_owned(),
                    protocol,
                },
                UsageTotals {
                    calls: 1,
                    input,
                    output: input / 10,
                    cache_read: input / 2,
                },
            );
        };
        let messages = RequestProtocol::Messages;
        let responses = RequestProtocol::Responses;
        insert(0, Some(8), "a", "resolved", messages, 100);
        insert(0, Some(21), "a", "resolved", messages, 300);
        insert(6, None, "a", "resolved", messages, 600);
        insert(7, None, "a", "resolved", messages, 700);
        insert(29, None, "a", "resolved", messages, 900);
        insert(30, None, "a", "resolved", messages, 1_000);
        insert(0, Some(8), "b", "resolved", messages, 100_000);
        insert(0, Some(8), "a", "other-model", messages, 200_000);
        insert(0, Some(8), "a", "resolved", responses, 400_000);
        insert(0, Some(8), "a", "", messages, 0);

        let mut filters = UsageFilters {
            account: Some("a".into()),
            model: Some("resolved".into()),
            protocol: Some(messages),
        };
        let day = UsageSummary::query(&history, today, UsageRange::Today, &filters);
        assert_eq!(day.timeline.len(), 24);
        assert_eq!(day.total.tokens(), 440);
        assert_eq!(day.timeline[8].tokens(), 110);
        assert_eq!(day.timeline[21].tokens(), 330);
        let week = UsageSummary::query(&history, today, UsageRange::Week, &filters);
        assert_eq!(week.timeline.len(), 7);
        assert_eq!(week.timeline[0].tokens(), 660);
        assert_eq!(
            week.total,
            UsageTotals {
                calls: 3,
                input: 1_000,
                output: 100,
                cache_read: 500
            }
        );
        assert_eq!(week.accounts, BTreeMap::from([("a", week.total)]));
        assert_eq!(week.models, BTreeMap::from([("resolved", week.total)]));
        let month = UsageSummary::query(&history, today, UsageRange::Month, &filters);
        assert_eq!(month.timeline.len(), 30);
        assert_eq!(month.timeline[0].tokens(), 990);
        assert_eq!(month.total.tokens(), 2_860);

        filters.model = Some(String::new());
        let unknown = UsageSummary::query(&history, today, UsageRange::Today, &filters);
        assert_eq!(unknown.total.calls, 1);
        assert_eq!(unknown.total.tokens(), 0);
        assert_eq!(unknown.models.keys().copied().collect::<Vec<_>>(), [""]);
        assert!(unknown.known_models.contains("resolved"));
    }
}
