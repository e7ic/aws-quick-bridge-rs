use super::activity::{ActivityRange, RequestActivity};
use super::charts::format_tokens;
use super::components::{BtnVariant, Theme};
use super::motion::{self, SlidingTrackStyle, SpringConfig, TrackAxis};
use super::tables::{NumberSort, TablePage};
use super::{
    AccountFilter, BridgeApp, GuiShared, LogFilter, MONO_FONT, PendingAction, SIDEBAR_LOGO_HEIGHT,
    SIDEBAR_LOGO_WIDTH, SignInState, THEME_TRANSITION, Tab, logo_source, numeric_font, ui_font,
};
use crate::config::{Settings, generate_api_key, model_display_name};
use crate::gateway::ccswitch::{APPS, CLAUDE_DESKTOP_APP, endpoint_for, import_link};
use crate::pool::{AccountHealth, AccountSnapshot, LogLevel, RequestLogEntry};
use chrono::{Local, Utc};
use gpui::{
    AnyElement, ContentMask, Context, Div, FontWeight, Hsla, ImageSource, ObjectFit, Pixels,
    Resource, SharedString, TextRun, TextSystem, Transformation, Window, canvas, div, img,
    prelude::*, px, radians, relative, rgba,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SIDEBAR_WIDTH: f32 = 192.0;
const HEADER_HEIGHT: f32 = 48.0;
const PAGE_RIGHT_PADDING: f32 = 12.0;
const PAGE_CONTENT_MAX_WIDTH: Pixels = px(1280.0);
const LOG_LATENCY_WIDTH: f32 = 72.0;
const LOG_MESSAGE_FONT_SIZE: f32 = 12.0;

impl BridgeApp {
    fn confirm_action(
        t: Theme,
        id: &'static str,
        detail: &'static str,
        label: &'static str,
        confirm: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .px(px(10.0))
            .py(px(6.0))
            .rounded(px(8.0))
            .bg(t.bg_well)
            .child(
                div()
                    .min_w(px(0.0))
                    .text_size(px(12.0))
                    .text_color(t.text_secondary)
                    .child(detail),
            )
            .child(Self::button(
                t,
                id,
                None,
                false,
                label,
                BtnVariant::SoftDanger,
                move |this, window, cx| {
                    this.pending_action = None;
                    confirm(this, window, cx);
                },
                cx,
            ))
    }

    fn toggle_action(&mut self, action: PendingAction) {
        self.pending_action = (self.pending_action != Some(action)).then_some(action);
    }

    pub(super) fn page_heading(t: Theme, title: &'static str) -> Div {
        div()
            .text_size(px(23.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(t.text_primary)
            .child(title)
    }

    fn theme_toggle_icon(darkness: f32, color: Hsla) -> Div {
        const ICON_SIZE: f32 = 14.0;
        const TURN: f32 = 0.55;
        let lightness = 1.0 - darkness;
        div()
            .relative()
            .size(px(ICON_SIZE))
            .flex_shrink_0()
            .children(
                [
                    ("icons/moon-bold.svg", lightness, -darkness * TURN),
                    ("icons/sun-2-bold.svg", darkness, lightness * TURN),
                ]
                .into_iter()
                .filter(|(_, opacity, _)| *opacity > 0.0)
                .map(|(path, opacity, angle)| {
                    Self::raw_icon(path, ICON_SIZE, color.opacity(opacity))
                        .absolute()
                        .inset_0()
                        .with_transformation(Transformation::rotate(radians(angle)))
                }),
            )
    }

    fn button_feedback_label(label: &'static str, feedback: &'static str, active: bool) -> Div {
        let longer = if label.chars().count() > feedback.chars().count() {
            label
        } else {
            feedback
        };
        div()
            .relative()
            .child(div().opacity(0.0).child(longer))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(if active { feedback } else { label }),
            )
    }

    fn copy_button(
        t: Theme,
        shared: &GuiShared,
        id: impl Into<SharedString>,
        label: &'static str,
        value: String,
        variant: BtnVariant,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = id.into();
        let copied = Self::is_copied(shared, &id);
        let anim_id = SharedString::from(format!("copy-state-{id}-{}", u8::from(copied)));
        let variant = match (copied, variant) {
            (true, BtnVariant::Text(_)) => BtnVariant::Text(t.green_text),
            (true, _) => BtnVariant::SoftSuccess,
            (false, variant) => variant,
        };
        Self::button_with_content(
            t,
            id.clone(),
            false,
            !label.is_empty(),
            variant,
            move |color| {
                let icon = if copied {
                    "icons/check-circle-bold.svg"
                } else {
                    "icons/copy-bold.svg"
                };
                let content = div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(Self::icon(icon, 14.0, color))
                    .when(!label.is_empty(), |row| {
                        row.child(Self::button_feedback_label(label, "已复制", copied))
                    });
                Some(
                    motion::fade_in(anim_id, Duration::from_millis(120), content)
                        .into_any_element(),
                )
            },
            move |this, _, cx| this.copy_with_feedback(&id, value.clone(), cx),
            cx,
        )
    }

    fn refresh_quotas_button(
        t: Theme,
        id: &'static str,
        label: &'static str,
        loading: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        Self::button_with_content(
            t,
            id,
            loading,
            true,
            BtnVariant::Secondary,
            move |color| {
                [
                    motion::spin_svg(
                        format!("btn-spin-{id}"),
                        loading,
                        Self::raw_icon("icons/refresh-bold.svg", 14.0, color),
                    ),
                    Self::button_feedback_label(label, "刷新中", loading).into_any_element(),
                ]
            },
            |this, _, _| this.refresh_all_quotas_ui(),
            cx,
        )
    }

    fn gateway_button(
        t: Theme,
        shared: &GuiShared,
        id: &'static str,
        start_variant: BtnVariant,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let running = shared.server_running;
        let (icon, label) = match (shared.gateway_busy, running) {
            (true, false) => ("icons/refresh-bold.svg", "正在启动"),
            (true, true) => ("icons/refresh-bold.svg", "正在停止"),
            (false, true) => ("icons/stop-bold.svg", "停止网关"),
            (false, false) => ("icons/play-bold.svg", "启动网关"),
        };
        Self::button(
            t,
            id,
            Some(icon),
            shared.gateway_busy,
            label,
            if running {
                BtnVariant::SoftDanger
            } else {
                start_variant
            },
            move |this, _, _| {
                if running {
                    this.stop_gateway();
                } else {
                    this.start_gateway();
                }
            },
            cx,
        )
    }

    fn gateway_status(t: Theme, shared: &GuiShared, id: &'static str, compact: bool) -> AnyElement {
        if shared.gateway_busy {
            let (width, height) = if compact { (46.0, 10.0) } else { (56.0, 22.0) };
            return Self::skeleton_bar(t, id, Some(width), height, 5.0);
        }
        let (label, fg, bg, border, dot) = if shared.server_running {
            (
                "运行中",
                t.green_text,
                t.green_soft,
                t.green_border,
                t.green,
            )
        } else {
            (
                "已停止",
                t.text_muted,
                t.bg_well,
                t.border_subtle,
                t.text_muted,
            )
        };
        if compact {
            div()
                .flex()
                .items_center()
                .gap(px(5.0))
                .flex_shrink_0()
                .text_size(px(12.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.text_primary)
                .whitespace_nowrap()
                .child(div().size(px(6.0)).rounded_full().bg(dot))
                .child(label)
                .into_any_element()
        } else {
            Self::pill(label, fg, bg, border, Some(dot), None)
        }
    }

    fn sidebar_gateway_controls(
        &self,
        t: Theme,
        shared: &GuiShared,
        cx: &mut Context<Self>,
    ) -> Div {
        const COPY_ADDRESS_ID: &str = "sidebar-copy-local-address";
        let scope = if self.lan_enabled() {
            "允许局域网"
        } else {
            "本机"
        };
        let endpoint = self
            .base_url(false)
            .trim_start_matches("http://")
            .to_string();
        let copied = Self::is_copied(shared, COPY_ADDRESS_ID);
        let (icon, icon_color) = if copied {
            ("icons/check-circle-bold.svg", t.green_text)
        } else {
            ("icons/copy-bold.svg", t.text_muted)
        };
        let address = div()
            .id(COPY_ADDRESS_ID)
            .flex()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .h(px(24.0))
            .gap(px(6.0))
            .px(px(4.0))
            .text_size(px(12.0))
            .text_color(t.text_primary)
            .whitespace_nowrap()
            .cursor_pointer()
            .focusable()
            .tab_stop(true)
            .focus(move |style| style.text_color(t.blue_text))
            .child(div().font_family(MONO_FONT).child(endpoint))
            .child(Self::icon(icon, 13.0, icon_color))
            .on_click(cx.listener(|this, _, _, cx| {
                this.copy_with_feedback(COPY_ADDRESS_ID, this.base_url(false), cx);
                cx.notify();
            }));

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap(px(5.0))
            .child(
                div()
                    .h(px(20.0))
                    .px(px(4.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(Self::gateway_status(
                        t,
                        shared,
                        "sidebar-gateway-loading",
                        true,
                    ))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(t.text_muted)
                            .child(scope),
                    ),
            )
            .child(address)
            .child(Self::gateway_button(
                t,
                shared,
                "sidebar-gateway",
                BtnVariant::SoftBlue,
                cx,
            ))
    }

    fn nav_item(
        &self,
        t: Theme,
        tab: Tab,
        count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.active_tab == tab;
        let item = div()
            .id(tab.title())
            .flex()
            .items_center()
            .gap(px(10.0))
            .h(px(36.0))
            .px(px(10.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(rgba(0x00000000))
            .focusable()
            .tab_stop(true)
            .when(t.keyboard_focus, |el| {
                el.focus(move |style| style.border_color(t.series_blue))
            })
            .cursor_pointer()
            .text_size(px(13.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(if active {
                t.text_primary
            } else {
                t.text_secondary
            })
            .child(Self::icon(
                tab.icon_path(),
                15.0,
                if active { t.text_primary } else { t.text_muted },
            ))
            // MiSans 的可见字形略高于行框中线，向下补齐图标中线。
            .child(div().flex_1().relative().top(px(1.0)).child(tab.title()))
            .when(count > 0, |el| {
                el.child(div().font(numeric_font()).child(Self::pill(
                    count.to_string(),
                    t.text_secondary,
                    t.bg_well,
                    t.border_subtle,
                    None,
                    None,
                )))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.switch_tab(tab);
                cx.notify();
            }));
        motion::layout(motion::control_feedback(
            item,
            move |style, hover, press| {
                if !active {
                    style.background = Some(t.bg_hover.opacity(hover).into());
                }
                style.opacity = Some(1.0 - 0.08 * press);
            },
        ))
    }

    fn render_sidebar(
        &self,
        t: Theme,
        darkness: f32,
        shared: &GuiShared,
        cx: &mut Context<Self>,
    ) -> Div {
        let nav_heading = |title: &'static str, divider: bool| {
            div()
                .when(divider, |el| {
                    el.mt(px(14.0))
                        .pt(px(12.0))
                        .border_t_1()
                        .border_color(t.border_subtle)
                })
                .px(px(9.0))
                .pb(px(5.0))
                .text_size(px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.text_muted)
                .child(title)
                .into_any_element()
        };
        let mut selected_nav = None;
        let mut nav_items = Vec::new();
        for (index, (title, tabs)) in Tab::SIDEBAR_SECTIONS.into_iter().enumerate() {
            nav_items.push(nav_heading(title, index != 0));
            for &tab in tabs {
                if self.active_tab == tab {
                    selected_nav = Some(nav_items.len());
                }
                let count = match tab {
                    Tab::Accounts => self.snapshot.accounts.len(),
                    Tab::Logs => self.snapshot.logs.len(),
                    _ => 0,
                };
                nav_items.push(self.nav_item(t, tab, count, cx).into_any_element());
            }
        }

        div()
            .relative()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(18.0))
            .pt(px(if cfg!(target_os = "macos") {
                34.0
            } else {
                10.0
            }))
            .pb(px(12.0))
            .px(px(12.0))
            .child(Self::sidebar_gateway_glow(t, shared.server_running))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .h(px(40.0))
                    .px(px(4.0))
                    .child(
                        div()
                            .relative()
                            .w(px(SIDEBAR_LOGO_WIDTH))
                            .h(px(SIDEBAR_LOGO_HEIGHT))
                            .flex_shrink_0()
                            .children(
                                [(false, 1.0 - darkness), (true, darkness)]
                                    .into_iter()
                                    .filter(|(_, opacity)| *opacity > 0.0)
                                    .map(|(dark, opacity)| {
                                        img(logo_source(dark))
                                            .absolute()
                                            .inset_0()
                                            .size_full()
                                            .object_fit(ObjectFit::Contain)
                                            .opacity(opacity)
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(FontWeight::MEDIUM)
                            .whitespace_nowrap()
                            .child("Quick Bridge"),
                    ),
            )
            .child(motion::sliding_track(
                "sidebar-nav-track",
                selected_nav,
                SlidingTrackStyle {
                    axis: TrackAxis::Vertical,
                    gap: 2.0,
                    padding: 0.0,
                    track_bg: None,
                    track_border: None,
                    track_radius: 0.0,
                    pill_bg: t.bg_nav_active,
                    pill_border: t.border_strong,
                    pill_radius: 7.0,
                    pill_shadow: None,
                },
                nav_items,
            ))
            .child(div().flex_1())
            .child(self.sidebar_gateway_controls(t, shared, cx))
    }

    fn account_quota_loading(shared: &GuiShared, account: &AccountSnapshot) -> bool {
        shared.quota_loading_keys.contains(&account.key)
    }

    fn account_quota_plan<'a>(&'a self, account: &'a AccountSnapshot) -> Option<&'a str> {
        self.visible_quota_history
            .readings(&account.key)
            .last()
            .map(|reading| reading.plan.as_str())
            .or_else(|| account.quota.as_ref().map(|quota| quota.plan.as_str()))
    }

    fn account_quota_summary(
        &self,
        t: Theme,
        account: &AccountSnapshot,
        loading: bool,
        title: impl Into<SharedString>,
    ) -> Div {
        let history = self.visible_quota_history.readings(&account.key);
        let latest = history.last();
        let windows = latest
            .map(|latest| latest.windows.as_slice())
            .or_else(|| {
                account
                    .quota
                    .as_ref()
                    .map(|report| report.windows.as_slice())
            })
            .unwrap_or_default();
        let status = if loading {
            Some(if windows.is_empty() {
                "读取中"
            } else {
                "刷新中"
            })
        } else if !windows.is_empty() && (account.quota.is_none() || account.quota_error.is_some())
        {
            Some("上次结果")
        } else {
            None
        };
        let panel = div().flex().flex_col().flex_grow().gap(px(6.0)).child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(6.0))
                .text_size(px(11.5))
                .child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.text_secondary)
                        .child(title.into()),
                )
                .when_some(status, |el, status| {
                    el.child(
                        div()
                            .flex()
                            .items_center()
                            .flex_shrink_0()
                            .text_color(t.text_muted)
                            .child(status),
                    )
                }),
        );
        let grid = |count: usize| {
            div()
                .grid()
                .grid_cols(count.min(3) as u16)
                .flex_grow()
                .gap(px(14.0))
        };
        if windows.is_empty() && loading {
            panel.child(grid(2).children((0..2).map(|index| {
                Self::quota_balance_skeleton(t, format!("quota-loading-{}-{index}", account.key))
            })))
        } else if windows.is_empty() {
            let (label, color) = if account.quota_error.is_some() {
                ("额度读取失败", t.amber_text)
            } else {
                ("暂无额度数据", t.text_muted)
            };
            panel.child(motion::fade_in(
                SharedString::from(format!(
                    "quota-status-{}-{}",
                    account.key,
                    u8::from(account.quota_error.is_some())
                )),
                motion::CONTENT_FADE_DURATION,
                Self::quota_metric_frame()
                    .id(SharedString::from(format!("quota-status-{}", account.key)))
                    .flex_grow()
                    .flex()
                    .items_center()
                    .text_size(px(11.5))
                    .text_color(color)
                    .when_some(account.quota_error.as_ref(), |el, error| {
                        el.tooltip(Self::text_tooltip(t, error.clone()))
                    })
                    .child(label),
            ))
        } else {
            let balances = grid(windows.len()).children(windows.iter().map(|quota| {
                Self::quota_balance_metric(
                    t,
                    quota,
                    history,
                    account.quota_updated_at,
                    loading,
                    format!("quota-{}-{}", account.key, quota.name),
                )
            }));
            panel.child(motion::fade_in(
                SharedString::from(format!("quota-content-{}", account.key)),
                motion::CONTENT_FADE_DURATION,
                balances,
            ))
        }
    }

    pub(super) fn kpi_card(
        t: Theme,
        icon: &'static str,
        label: &'static str,
        value: impl IntoElement,
        detail: impl IntoElement,
        trend: Option<AnyElement>,
    ) -> Div {
        Self::card(t)
            .p(px(6.0))
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(3.0))
                    .text_size(px(12.0))
                    .text_color(t.text_secondary)
                    .child(
                        div()
                            .size(px(22.0))
                            .flex_shrink_0()
                            .rounded(px(6.0))
                            .bg(t.bg_seg_track)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Self::icon(icon, 13.0, t.text_muted)),
                    )
                    .child(label),
            )
            .child(motion::layout(
                Self::card_well(t)
                    .id("metric-surface")
                    .flex_1()
                    .min_h(px(68.0))
                    .flex()
                    .flex_col()
                    .justify_between()
                    .gap(px(3.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .text_size(px(25.0))
                                    .font(numeric_font())
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(t.text_primary)
                                    .truncate()
                                    .child(value),
                            )
                            .children(trend),
                    )
                    .child(
                        div()
                            .min_h(px(20.0))
                            .flex()
                            .items_center()
                            .text_size(px(11.5))
                            .text_color(t.text_muted)
                            .child(detail),
                    ),
            ))
    }

    fn model_probes(&self, t: Theme, shared: &GuiShared, cx: &mut Context<Self>) -> Div {
        let mut card = Self::card(t)
            .flex()
            .flex_col()
            .child(Self::section_header(t, "模型探测"))
            .when_some(self.snapshot.last_resolved_model.as_ref(), |el, model| {
                el.child(
                    div()
                        .pb(px(8.0))
                        .text_size(px(11.5))
                        .text_color(t.text_muted)
                        .truncate()
                        .child(format!("最近使用 {model}")),
                )
            });
        for tier in ["smart", "balanced", "fast"] {
            let busy = shared.probe_busy_model.as_deref() == Some(tier);
            let resolved = shared.probed_models.get(tier);
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .min_h(px(42.0))
                    .border_t_1()
                    .border_color(t.border_subtle)
                    .child(
                        div()
                            .w(px(66.0))
                            .flex_shrink_0()
                            .font_family(MONO_FONT)
                            .text_size(px(12.0))
                            .child(tier),
                    )
                    .child(div().flex_1().min_w(px(0.0)).child(if busy {
                        Self::skeleton_bar(t, format!("probe-skeleton-{tier}"), None, 8.0, 3.0)
                    } else {
                        motion::fade_in(
                            SharedString::from(format!(
                                "probe-result-{tier}-{}",
                                u8::from(resolved.is_some())
                            )),
                            motion::CONTENT_FADE_DURATION,
                            div()
                                .font_family(MONO_FONT)
                                .text_size(px(11.5))
                                .text_color(if resolved.is_some() {
                                    t.text_primary
                                } else {
                                    t.text_muted
                                })
                                .truncate()
                                .child(resolved.map_or_else(|| "未探测".to_string(), Clone::clone)),
                        )
                        .into_any_element()
                    }))
                    .child(Self::button(
                        t,
                        format!("probe-{tier}"),
                        Some(if busy {
                            "icons/refresh-bold.svg"
                        } else {
                            "icons/bolt-bold.svg"
                        }),
                        busy,
                        if busy { "探测中" } else { "探测" },
                        if busy {
                            BtnVariant::SoftAccent
                        } else if shared.probe_busy_model.is_some() {
                            BtnVariant::Disabled
                        } else {
                            BtnVariant::Secondary
                        },
                        move |this, _, _| this.run_probe(tier),
                        cx,
                    )),
            );
        }
        card
    }

    fn lan_access_control(
        &self,
        t: Theme,
        shared: &GuiShared,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let enabled = self.lan_enabled();
        if shared.listener_editable() {
            return Self::toggle_switch(
                t,
                id,
                enabled,
                move |this, _, _| {
                    this.set_lan_bind(!enabled);
                },
                cx,
            );
        }
        div()
            .text_size(px(12.0))
            .text_color(t.text_muted)
            .child(if enabled { "已允许" } else { "仅本机" })
            .into_any_element()
    }

    fn connection_card(&self, t: Theme, shared: &GuiShared, cx: &mut Context<Self>) -> Div {
        let confirming_key = self.pending_action == Some(PendingAction::RotateKey);
        let endpoint_row = |label, id, lan, icon, cx: &mut Context<Self>| {
            let address = self.base_url(lan);
            Self::connection_row(t, label, address.clone(), icon)
                .border_b_1()
                .border_color(t.border_subtle)
                .child(Self::copy_button(
                    t,
                    shared,
                    id,
                    "复制",
                    address,
                    BtnVariant::Ghost,
                    cx,
                ))
        };
        let key_row = Self::connection_row(
            t,
            "API Key",
            self.masked_key(),
            ("icons/key-bold.svg", t.amber_text, t.amber_soft),
        )
        .child(
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .id("key-eye-wrap")
                        .tooltip(Self::text_tooltip(
                            t,
                            if self.show_full_key {
                                "隐藏完整密钥"
                            } else {
                                "显示完整密钥"
                            },
                        ))
                        .child(Self::button(
                            t,
                            "key-eye",
                            Some(if self.show_full_key {
                                "icons/eye-closed-bold.svg"
                            } else {
                                "icons/eye-bold.svg"
                            }),
                            false,
                            "",
                            BtnVariant::Ghost,
                            |this, _, _| this.show_full_key = !this.show_full_key,
                            cx,
                        )),
                )
                .child(Self::copy_button(
                    t,
                    shared,
                    "key-copy",
                    "复制",
                    self.snapshot.config.api_key.clone(),
                    BtnVariant::Ghost,
                    cx,
                )),
        );

        Self::card(t)
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                Self::section_header(t, "网关接入").child(Self::gateway_status(
                    t,
                    shared,
                    "dashboard-gateway-loading",
                    false,
                )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_size(px(12.5))
                    .text_color(t.text_secondary)
                    .child("允许局域网访问")
                    .child(self.lan_access_control(t, shared, "hero-lan-sw", cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(endpoint_row(
                        "本机",
                        "cp-lo",
                        false,
                        ("icons/server-bold.svg", t.blue_text, t.blue_soft),
                        cx,
                    ))
                    .when(self.available_lan_ip().is_some(), |el| {
                        el.child(endpoint_row(
                            "局域网",
                            "cp-lan",
                            true,
                            ("icons/global-bold.svg", t.green_text, t.green_soft),
                            cx,
                        ))
                    })
                    .child(key_row),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .gap(px(8.0))
                    .when(!confirming_key, |el| {
                        el.child(Self::button(
                            t,
                            "dash-to-clients",
                            Some("icons/arrow-right-up-bold.svg"),
                            false,
                            "客户端接入",
                            BtnVariant::Secondary,
                            |this, _, _| this.switch_tab(Tab::Clients),
                            cx,
                        ))
                    })
                    .when(confirming_key, |el| {
                        el.child(
                            div()
                                .min_w(px(0.0))
                                .text_color(t.text_muted)
                                .text_size(px(11.5))
                                .child("旧密钥会立即失效，客户端需要重新接入。"),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .gap(px(4.0))
                            .items_center()
                            .child(Self::button(
                                t,
                                "key-rot",
                                (!confirming_key).then_some("icons/key-bold.svg"),
                                false,
                                if confirming_key {
                                    "取消"
                                } else {
                                    "更换密钥"
                                },
                                BtnVariant::Ghost,
                                |this, _, _| this.toggle_action(PendingAction::RotateKey),
                                cx,
                            ))
                            .when(confirming_key, |el| {
                                el.child(Self::button(
                                    t,
                                    "confirm-key-rotation",
                                    None,
                                    false,
                                    "确认更换",
                                    BtnVariant::SoftDanger,
                                    |this, _, _| {
                                        this.pending_action = None;
                                        let mut config = this.pool.config();
                                        config.api_key = generate_api_key();
                                        match this.pool.update_config(config.clone()) {
                                            Ok(()) => {
                                                this.snapshot.config = config;
                                                this.show_full_key = false;
                                                this.set_toast("已更换密钥", false);
                                            }
                                            Err(error) => this.set_toast(error.to_string(), true),
                                        }
                                    },
                                    cx,
                                ))
                            }),
                    ),
            )
    }

    fn connection_row(
        t: Theme,
        label: &'static str,
        value: String,
        (icon_path, icon_color, icon_bg): (&'static str, Hsla, Hsla),
    ) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .min_w(px(0.0))
            .min_h(px(48.0))
            .py(px(6.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(32.0))
                    .flex_shrink_0()
                    .rounded(px(9.0))
                    .bg(icon_bg)
                    .child(Self::icon(icon_path, 16.0, icon_color)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.0))
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .line_height(px(16.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.text_primary)
                            .child(label),
                    )
                    .child(
                        div()
                            .w_full()
                            .font_family(MONO_FONT)
                            .text_size(px(11.5))
                            .line_height(px(16.0))
                            .text_color(t.text_muted)
                            .truncate()
                            .child(value),
                    ),
            )
    }

    fn code_value(t: Theme, value: String) -> Div {
        div()
            .flex_1()
            .min_w(px(0.0))
            .font_family(MONO_FONT)
            .text_size(px(12.0))
            .text_color(t.text_primary)
            .truncate()
            .child(value)
    }

    fn recent_requests(&self, t: Theme, cx: &mut Context<Self>) -> Div {
        let mut card =
            Self::card(t)
                .flex()
                .flex_col()
                .child(Self::section_header(t, "最近日志").child(Self::button(
                    t,
                    "dash-to-logs",
                    Some("icons/arrow-right-up-bold.svg"),
                    false,
                    "查看日志",
                    BtnVariant::Ghost,
                    |this, _, _| this.switch_tab(Tab::Logs),
                    cx,
                )));
        if self.snapshot.logs.is_empty() {
            return card.child(Self::empty_state(t, Tab::Logs.icon_path(), "暂无请求记录"));
        }
        for (entry, occurrence) in Self::log_rows(&self.snapshot.logs).take(5) {
            let (_, _, _, dot) = Self::log_colors(t, entry.level);
            card = card.child(motion::layout(
                div()
                    .id((SharedString::from(entry.timestamp.to_rfc3339()), occurrence))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .min_h(px(34.0))
                    .border_t_1()
                    .border_color(t.border_subtle)
                    .child(div().size(px(6.0)).flex_shrink_0().rounded_full().bg(dot))
                    .child(
                        div()
                            .font(numeric_font())
                            .text_size(px(11.0))
                            .text_color(t.text_muted)
                            .child(
                                entry
                                    .timestamp
                                    .with_timezone(&Local)
                                    .format("%H:%M:%S")
                                    .to_string(),
                            ),
                    )
                    .child(Self::code_value(t, entry.message.clone()))
                    .when(entry.latency_ms > 0, |el| {
                        el.child(
                            div()
                                .font(numeric_font())
                                .text_size(px(11.0))
                                .text_color(t.text_muted)
                                .child(format!("{} ms", entry.latency_ms)),
                        )
                    }),
            ));
        }
        card
    }

    fn dashboard_account_usage(&self, t: Theme) -> Option<Div> {
        let accounts = &self.snapshot.accounts;
        let in_use = self.snapshot.active_requests > 0;
        let labels = if in_use {
            accounts
                .iter()
                .filter(|account| account.active_requests > 0)
                .map(|account| account.label.as_str())
                .collect::<Vec<_>>()
                .join(" · ")
        } else {
            let key = self.snapshot.last_used_account_key.as_deref()?;
            accounts
                .iter()
                .find(|account| account.key == key)?
                .label
                .clone()
        };

        Some(
            div()
                .flex()
                .items_center()
                .min_w(px(0.0))
                .gap(px(7.0))
                .child(
                    div()
                        .flex_shrink_0()
                        .child(Self::account_usage_badge(t, in_use)),
                )
                .child(
                    div()
                        .id("dashboard-account-usage")
                        .min_w(px(0.0))
                        .text_size(px(12.0))
                        .text_color(t.text_secondary)
                        .truncate()
                        .tooltip(Self::text_tooltip(t, labels.clone()))
                        .child(labels),
                ),
        )
    }

    fn render_dashboard(
        &self,
        t: Theme,
        shared: &GuiShared,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let total = self.snapshot.accounts.len();
        let healthy = self
            .snapshot
            .accounts
            .iter()
            .filter(|account| account.enabled && matches!(account.health, AccountHealth::Healthy))
            .count();
        let primary = self
            .snapshot
            .accounts
            .iter()
            .find(|account| account.is_primary)
            .or_else(|| self.snapshot.accounts.first());
        let now = Utc::now();
        let range = self.activity_range;
        let samples = self.pool.request_samples();
        let activity = RequestActivity::from_samples(&samples, now, range);
        let oldest = now - chrono::TimeDelta::hours(ActivityRange::Day.hours());
        let has_recent_requests = samples
            .iter()
            .any(|sample| (oldest..=now).contains(&sample.timestamp));
        let success_ratio =
            (activity.requests > 0).then(|| activity.succeeded as f32 / activity.requests as f32);
        let success_rate =
            success_ratio.map_or_else(|| "—".into(), |ratio| format!("{:.1}%", ratio * 100.0));
        let active = self.snapshot.active_requests;
        let tokens = activity.tokens();

        let kpi_accounts = motion::layout(
            Self::kpi_card(
                t,
                Tab::Accounts.icon_path(),
                "可用账号",
                motion::rolling_count("dash-healthy-accounts", healthy as u64),
                format!("共 {total} 个账号"),
                None,
            )
            .id("dashboard-accounts"),
        );
        let kpi_requests = motion::layout(
            Self::kpi_card(
                t,
                "icons/chart-square-bold.svg",
                "累计请求",
                motion::rolling_count("dash-total-requests", self.snapshot.total_requests),
                div()
                    .when(active > 0, |el| {
                        el.child(Self::pill(
                            format!("{active} 个进行中"),
                            t.text_secondary,
                            t.bg_seg_track,
                            t.border_subtle,
                            Some(t.series_blue),
                            None,
                        ))
                    })
                    .when(active == 0, |el| el.child("历史请求")),
                None,
            )
            .id("dashboard-requests-total"),
        );
        let kpi_success = motion::layout(
            Self::kpi_card(
                t,
                "icons/check-circle-bold.svg",
                "成功率",
                motion::rolling_number(
                    "dash-success-rate",
                    success_rate,
                    success_ratio.map(f64::from),
                ),
                format!("近 {} 小时 · {} 次成功", range.hours(), activity.succeeded),
                None,
            )
            .id("dashboard-success"),
        );
        let kpi_tokens = motion::layout(
            Self::kpi_card(
                t,
                "icons/cpu-bolt-bold.svg",
                "Token 用量",
                motion::rolling_number("dash-token-usage", format_tokens(tokens), tokens as f64),
                div().min_w(px(0.0)).truncate().child(format!(
                    "输入 {} · 输出 {}",
                    format_tokens(activity.input_tokens),
                    format_tokens(activity.output_tokens)
                )),
                Self::token_sparkline(t, &activity, range),
            )
            .id("dashboard-tokens"),
        );
        let metrics = if compact {
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(kpi_accounts)
                        .child(kpi_requests),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(kpi_success)
                        .child(kpi_tokens),
                )
        } else {
            div()
                .flex()
                .gap(px(8.0))
                .child(kpi_accounts)
                .child(kpi_requests)
                .child(kpi_success)
                .child(kpi_tokens)
        };

        let quota = Self::card(t)
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(Self::section_header(t, "账号剩余额度").when(
                self.snapshot.accounts.iter().any(|account| account.enabled),
                |el| {
                    el.child(Self::refresh_quotas_button(
                        t,
                        "dash-rq",
                        "刷新",
                        shared.quota_loading_all,
                        cx,
                    ))
                },
            ))
            .child(match primary {
                Some(account) => self
                    .account_quota_summary(
                        t,
                        account,
                        Self::account_quota_loading(shared, account),
                        self.account_quota_plan(account).map_or_else(
                            || account.label.clone(),
                            |plan| format!("{} · {plan}", account.label),
                        ),
                    )
                    .into_any_element(),
                None => Self::empty_state(t, "icons/chart-square-bold.svg", "登录账号后可查看额度")
                    .child(Self::button(
                        t,
                        "dash-go-login",
                        Some("icons/user-plus-bold.svg"),
                        false,
                        "前往账号池",
                        BtnVariant::Secondary,
                        |this, _, _| this.switch_tab(Tab::Accounts),
                        cx,
                    ))
                    .into_any_element(),
            });

        let analysis = if has_recent_requests {
            self.request_analysis(t, &activity, now, cx)
        } else {
            Self::card(t)
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(Self::section_header(t, "请求分析"))
                .child(
                    Self::empty_state(t, "icons/chart-square-bold.svg", "近 24 小时暂无对话请求")
                        .child(div().text_size(px(12.0)).text_color(t.text_muted).child(
                            "添加账号、启动网关并连接客户端后，这里会显示请求趋势和协议分布。",
                        )),
                )
        };
        div()
            .flex()
            .flex_col()
            .gap(px(13.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .items_center()
                            .gap(px(12.0))
                            .child(Self::page_heading(t, "网关概览").flex_shrink_0())
                            .children(self.dashboard_account_usage(t)),
                    )
                    .child(Self::gateway_button(
                        t,
                        shared,
                        "hero-gw-btn",
                        BtnVariant::Primary,
                        cx,
                    )),
            )
            .child(metrics)
            .child(
                div()
                    .flex()
                    .gap(px(12.0))
                    .when(compact, |el| el.flex_col())
                    .child(motion::layout(
                        analysis
                            .id("dashboard-analysis")
                            .min_w(px(0.0))
                            .when(!compact, |el| el.flex_1()),
                    ))
                    .when(has_recent_requests && activity.requests > 0, |el| {
                        el.child(motion::layout(
                            self.protocol_distribution(t, &activity, cx)
                                .id("dashboard-protocols")
                                .when(!compact, |el| el.w(px(248.0)).flex_shrink_0()),
                        ))
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap(px(12.0))
                    .when(compact, |el| el.flex_col())
                    .child(
                        div()
                            .when(!compact, |el| el.flex_1())
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(motion::layout(quota.id("dashboard-quota")))
                            .when(healthy > 0, |el| {
                                el.child(motion::layout(
                                    self.model_probes(t, shared, cx).id("dashboard-probes"),
                                ))
                            }),
                    )
                    .child(
                        div()
                            .when(!compact, |el| el.flex_1())
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(motion::layout(
                                self.connection_card(t, shared, cx)
                                    .id("dashboard-connection"),
                            ))
                            .when(!self.snapshot.logs.is_empty(), |el| {
                                el.child(motion::layout(
                                    self.recent_requests(t, cx).id("dashboard-logs"),
                                ))
                            }),
                    ),
            )
    }

    fn pending_sign_in(
        &self,
        t: Theme,
        shared: &GuiShared,
        url: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Div {
        let waiting = url.is_some();
        Self::card(t)
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(9.0))
                            .child(Self::skeleton_bar(
                                t,
                                "sign-in-avatar",
                                Some(30.0),
                                30.0,
                                15.0,
                            ))
                            .child(Self::pill(
                                if waiting {
                                    "等待浏览器授权"
                                } else {
                                    "正在准备授权链接"
                                },
                                t.accent_text,
                                t.accent_soft,
                                t.accent_border,
                                Some(t.accent),
                                None,
                            )),
                    )
                    .child(Self::button(
                        t,
                        "login-cancel",
                        None,
                        false,
                        "取消",
                        BtnVariant::Secondary,
                        |this, _, _| this.cancel_login(),
                        cx,
                    )),
            )
            .child(
                Self::field(t)
                    .when_some(url, |el, url| {
                        el.child(motion::fade_in(
                            "login-url-ready",
                            motion::CONTENT_FADE_DURATION,
                            div()
                                .flex()
                                .flex_1()
                                .min_w(px(0.0))
                                .items_center()
                                .gap(px(8.0))
                                .child(Self::code_value(t, url.to_string()))
                                .child(Self::copy_button(
                                    t,
                                    shared,
                                    "login-url-copy",
                                    "复制授权链接",
                                    url.to_string(),
                                    BtnVariant::Text(t.text_primary),
                                    cx,
                                )),
                        ))
                    })
                    .when(!waiting, |el| {
                        el.child(Self::skeleton_bar(t, "sign-in-link", None, 8.0, 3.0))
                    }),
            )
    }

    fn account_status(t: Theme, health: &AccountHealth) -> AnyElement {
        let (label, fg, bg, border, dot) = match health {
            AccountHealth::Healthy => (
                "正常".into(),
                t.green_text,
                t.green_soft,
                t.green_border,
                t.green,
            ),
            AccountHealth::CoolingDown { until_epoch_ms, .. } => {
                let seconds = (until_epoch_ms - Utc::now().timestamp_millis()).max(0) / 1000;
                (
                    format!("冷却 {seconds}s"),
                    t.amber_text,
                    t.amber_soft,
                    t.amber_border,
                    t.amber,
                )
            }
            AccountHealth::Expired { .. } => {
                ("已过期".into(), t.red_text, t.red_soft, t.red_border, t.red)
            }
            AccountHealth::Disabled => (
                "已禁用".into(),
                t.text_muted,
                t.bg_well,
                t.border_subtle,
                t.text_muted,
            ),
        };
        Self::pill(label, fg, bg, border, Some(dot), None)
    }

    fn account_usage_badge(t: Theme, in_use: bool) -> AnyElement {
        if in_use {
            Self::pill(
                "使用中",
                t.blue_text,
                t.blue_soft,
                t.series_blue.opacity(0.28),
                Some(t.series_blue),
                None,
            )
        } else {
            Self::pill(
                "最近使用",
                t.text_secondary,
                t.bg_well,
                t.border_subtle,
                None,
                None,
            )
        }
    }

    fn render_account_card(
        &self,
        t: Theme,
        shared: &GuiShared,
        account: &AccountSnapshot,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Div {
        let key_toggle = account.key.clone();
        let initial = account
            .label
            .chars()
            .next()
            .unwrap_or('A')
            .to_ascii_uppercase()
            .to_string();
        let enabled = account.enabled;
        let in_use = account.active_requests > 0;
        let show_highlight = self.account_card_highlight.visible(account);
        let highlight_opacity = self.account_card_highlight.opacity(account, Instant::now());
        let recently_used = !show_highlight
            && self.snapshot.active_requests == 0
            && self.snapshot.last_used_account_key.as_ref() == Some(&account.key);
        let loading = Self::account_quota_loading(shared, account);
        let plan = self.account_quota_plan(account);
        let request_trend = self.account_activity.get(&account.key).and_then(|history| {
            Self::account_request_sparkline(t, index, history.hourly_requests(Utc::now()))
        });
        let metrics = [
            (
                "total-requests",
                "累计请求",
                account.total_requests,
                false,
                request_trend,
            ),
            (
                "active-requests",
                "进行中请求",
                account.active_requests as u64,
                in_use,
                None,
            ),
        ];

        Self::card(t)
            .min_w(px(0.0))
            .relative()
            .when(highlight_opacity > 0.0, |card| {
                card.child(Self::active_account_glow().opacity(highlight_opacity))
            })
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(10.0))
                    .child(Self::account_avatar(
                        t,
                        initial,
                        account.is_primary,
                        highlight_opacity > 0.0,
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.0))
                            .gap(px(4.0))
                            .child(
                                div()
                                    .id(("account-name", index))
                                    .text_size(px(14.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .truncate()
                                    .tooltip(Self::text_tooltip(t, account.label.clone()))
                                    .child(account.label.clone()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap(px(7.0))
                                    .min_h(px(24.0))
                                    .text_size(px(11.5))
                                    .text_color(t.text_muted)
                                    .child(
                                        div()
                                            .min_w(px(0.0))
                                            .truncate()
                                            .child(account.region.clone()),
                                    )
                                    .when(account.is_primary, |el| {
                                        el.child(div().flex_shrink_0().child(Self::pill_icon(
                                            "icons/star-bold.svg",
                                            "主账号",
                                            t.accent_text,
                                            t.accent_soft,
                                            t.accent_border,
                                        )))
                                    })
                                    .when(recently_used, |el| {
                                        el.child(
                                            div()
                                                .flex_shrink_0()
                                                .opacity(1.0 - highlight_opacity)
                                                .child(Self::account_usage_badge(t, false)),
                                        )
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap(px(5.0))
                            .child(Self::account_status(t, &account.health))
                            .child(self.account_menu_trigger(t, account, loading, index, cx)),
                    ),
            )
            .child(self.account_quota_summary(t, account, loading, "剩余额度"))
            .child(
                div()
                    .id(SharedString::from(format!(
                        "account-metrics-{}",
                        account.key
                    )))
                    .grid()
                    .grid_cols(2)
                    .gap(px(14.0))
                    .children(metrics.into_iter().map(|(id, label, value, live, trend)| {
                        div()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .text_size(px(11.0))
                                    .text_color(t.text_muted)
                                    .when(live, |el| {
                                        el.child(
                                            div().size(px(5.0)).rounded_full().bg(t.series_blue),
                                        )
                                    })
                                    .child(label),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .min_h(px(32.0))
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .min_w(px(0.0))
                                            .font(numeric_font())
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_size(px(20.0))
                                            .truncate()
                                            .child(motion::rolling_count(id, value)),
                                    )
                                    .when_some(trend, |row, trend| row.child(trend)),
                            )
                    })),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .pt(px(10.0))
                    .border_t_1()
                    .border_color(t.border_subtle)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(px(12.0))
                            .text_color(t.text_secondary)
                            .child(Self::toggle_switch(
                                t,
                                format!("account-enabled-{}", account.key),
                                enabled,
                                move |this, _, _| {
                                    if let Err(error) = this.pool.set_enabled(&key_toggle, !enabled)
                                    {
                                        this.set_toast(error.to_string(), true);
                                    }
                                },
                                cx,
                            ))
                            .child("启用"),
                    )
                    .when_some(plan, |el, plan| {
                        el.child(
                            div()
                                .id(("account-plan", index))
                                .min_w(px(0.0))
                                .rounded(px(6.0))
                                .px(px(7.0))
                                .py(px(2.0))
                                .bg(t.bg_well)
                                .border_1()
                                .border_color(t.border_subtle)
                                .text_size(px(11.5))
                                .text_color(t.text_muted)
                                .truncate()
                                .tooltip(Self::text_tooltip(t, format!("计划 · {plan}")))
                                .child(plan.to_owned()),
                        )
                    }),
            )
            .when(highlight_opacity > 0.0, |card| {
                card.child(Self::active_account_ribbon(t).opacity(highlight_opacity))
            })
    }

    fn select_account_filter(&mut self, filter: AccountFilter) {
        if self.account_filter != filter {
            self.account_filter = filter;
            self.account_page = 0;
            self.account_menu = None;
        }
    }

    fn render_accounts(
        &mut self,
        t: Theme,
        shared: &GuiShared,
        columns: u16,
        cx: &mut Context<Self>,
    ) -> Div {
        let (login_busy, login_url) = match &shared.sign_in {
            SignInState::Idle => (false, None),
            SignInState::Preparing => (true, None),
            SignInState::Waiting(url) => (true, Some(url.as_str())),
        };
        let accounts = &self.snapshot.accounts;
        let enabled_count = accounts.iter().filter(|account| account.enabled).count();
        let filters = [
            (AccountFilter::All, "全部", accounts.len()),
            (AccountFilter::Enabled, "已启用", enabled_count),
            (
                AccountFilter::Disabled,
                "已禁用",
                accounts.len() - enabled_count,
            ),
        ];
        let mut filtered = accounts
            .iter()
            .enumerate()
            .filter(|(_, account)| self.account_filter.matches(account.enabled))
            .collect::<Vec<_>>();
        if self.account_sort != NumberSort::Default {
            filtered.sort_by(|(_, left), (_, right)| {
                self.account_sort
                    .compare(left.total_requests, right.total_requests)
            });
        }
        let page = TablePage::new(filtered.len(), self.account_page);
        self.account_page = page.index;
        let visible_accounts = &filtered[page.range()];
        if self.account_menu.as_ref().is_some_and(|menu| {
            !visible_accounts
                .iter()
                .any(|(_, account)| account.key == menu.key)
        }) {
            self.account_menu = None;
        }

        let heading = div()
            .flex()
            .items_center()
            .justify_between()
            .min_h(px(32.0))
            .child(Self::page_heading(t, "账号池"))
            .when(!accounts.is_empty() && !login_busy, |el| {
                el.child(Self::button(
                    t,
                    "acc-add",
                    Some("icons/user-plus-bold.svg"),
                    false,
                    "添加账号",
                    BtnVariant::Primary,
                    |this, _, _| {
                        this.select_account_filter(AccountFilter::All);
                        this.start_login();
                    },
                    cx,
                ))
            });

        let toolbar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(px(10.0))
            .px(px(14.0))
            .py(px(12.0))
            .child(
                div().flex().items_center().gap(px(5.0)).children(
                    filters
                        .into_iter()
                        .enumerate()
                        .filter(|(_, (filter, _, _))| {
                            !accounts.is_empty()
                                && (self.account_filter != AccountFilter::All
                                    || (enabled_count > 0 && enabled_count < accounts.len())
                                    || *filter == AccountFilter::All)
                        })
                        .map(|(i, (filter, label, count))| {
                            Self::filter_chip(
                                t,
                                format!("account-filter-{i}"),
                                label,
                                count,
                                self.account_filter == filter,
                                move |this| this.select_account_filter(filter),
                                cx,
                            )
                        }),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.0))
                    .when(filtered.len() > 1, |el| {
                        el.child(Self::button(
                            t,
                            "sort-account-requests",
                            None,
                            false,
                            format!("累计请求 {}", self.account_sort.arrow()),
                            BtnVariant::Secondary,
                            |this, _, _| {
                                this.account_sort = this.account_sort.cycle();
                                this.account_page = 0;
                                this.account_menu = None;
                            },
                            cx,
                        ))
                    })
                    .child(Self::button(
                        t,
                        "acc-sync",
                        Some("icons/link-round-bold.svg"),
                        false,
                        "同步本地账号",
                        BtnVariant::Ghost,
                        |this, _, _| {
                            this.pool.reload_accounts_from_disk();
                            this.set_toast("已同步本地账号", false);
                        },
                        cx,
                    ))
                    .when(enabled_count > 0, |el| {
                        el.child(Self::refresh_quotas_button(
                            t,
                            "acc-rq-all",
                            "刷新额度",
                            shared.quota_loading_all,
                            cx,
                        ))
                    }),
            );

        let mut panel = Self::card(t)
            .p(px(0.0))
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(toolbar);
        if filtered.is_empty() {
            if !login_busy || !accounts.is_empty() {
                let empty = if accounts.is_empty() {
                    Self::empty_state(t, Tab::Accounts.icon_path(), "暂无已登录账号")
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(t.text_muted)
                                .child("添加 Amazon Quick 账号后可查看额度并处理客户端请求"),
                        )
                        .child(Self::button(
                            t,
                            "acc-first",
                            Some("icons/user-plus-bold.svg"),
                            false,
                            "添加账号",
                            BtnVariant::Primary,
                            |this, _, _| {
                                this.select_account_filter(AccountFilter::All);
                                this.start_login();
                            },
                            cx,
                        ))
                } else {
                    let label = match self.account_filter {
                        AccountFilter::Enabled => "暂无已启用账号",
                        AccountFilter::Disabled => "暂无已禁用账号",
                        AccountFilter::All => "暂无账号",
                    };
                    Self::empty_state(t, Tab::Accounts.icon_path(), label).child(Self::button(
                        t,
                        "acc-reset-filter",
                        None,
                        false,
                        "查看全部",
                        BtnVariant::Secondary,
                        |this, _, _| this.select_account_filter(AccountFilter::All),
                        cx,
                    ))
                };
                panel = panel.child(div().px(px(14.0)).pb(px(14.0)).child(empty));
            }
        } else {
            let grid = div()
                .grid()
                .grid_cols(columns)
                .gap(px(12.0))
                .px(px(14.0))
                .pb(px(14.0))
                .children(visible_accounts.iter().map(|&(index, account)| {
                    motion::layout(
                        self.render_account_card(t, shared, account, index, cx)
                            .id(SharedString::from(account.key.clone())),
                    )
                }));
            panel = panel.child(grid).when(page.has_multiple_pages(), |el| {
                el.child(Self::table_footer(
                    t,
                    "account-page",
                    page,
                    |this, index| {
                        this.account_page = index;
                        this.account_menu = None;
                    },
                    cx,
                ))
            });
        }

        div()
            .flex()
            .flex_col()
            .gap(px(13.0))
            .child(heading)
            .when(login_busy, |el| {
                el.child(motion::slide_fade(
                    "pending-sign-in-card",
                    0.0,
                    1.0,
                    0.0,
                    6.0,
                    SpringConfig::SMOOTH,
                    motion::layout(self.pending_sign_in(t, shared, login_url, cx).id("sign-in")),
                ))
            })
            .when(!login_busy || !accounts.is_empty(), |el| {
                el.child(motion::layout(panel.id("account-list")))
            })
    }

    fn render_clients(
        &self,
        t: Theme,
        shared: &GuiShared,
        content_width: Pixels,
        cx: &mut Context<Self>,
    ) -> Div {
        let card_gap = px(10.0);
        let min_card_width = px(272.0);
        let cards_that_fit = ((content_width + card_gap) / (min_card_width + card_gap)) as u16;
        let app_count = APPS.len() as u16;
        let columns = if cards_that_fit >= app_count {
            app_count
        } else {
            cards_that_fit.clamp(1, 2)
        };

        let lan = self.available_lan_ip();
        let use_lan = self.ccswitch_use_lan && lan.is_some();
        let base = self.base_url(use_lan);
        let key = &self.snapshot.config.api_key;
        let default_model = self.client_default_model();
        let model = self.ccswitch_model.unwrap_or(default_model);
        let address_choice = lan.map(|lan| {
            Self::segmented_wrap(
                t,
                "client-addr-seg",
                vec![
                    Self::seg_item(
                        t,
                        "cc-lo",
                        "本机",
                        !use_lan,
                        |this, _, _| this.ccswitch_use_lan = false,
                        cx,
                    ),
                    Self::seg_item(
                        t,
                        "cc-lan",
                        format!("局域网 {lan}"),
                        use_lan,
                        |this, _, _| this.ccswitch_use_lan = true,
                        cx,
                    ),
                ],
            )
        });
        let model_choice = Self::segmented_wrap(
            t,
            "client-model-seg",
            ["smart[1m]", "smart", "balanced", "fast", "auto"]
                .into_iter()
                .map(|choice| {
                    Self::seg_item(
                        t,
                        format!("client-model-{choice}"),
                        choice,
                        model == choice,
                        move |this, _, _| this.ccswitch_model = Some(choice),
                        cx,
                    )
                })
                .collect(),
        );

        let brand_image = |path: &'static str| {
            img(ImageSource::Resource(Resource::Embedded(path.into())))
                .size(px(22.0))
                .object_fit(ObjectFit::Contain)
                .into_any_element()
        };

        let cards = APPS.iter().map(|app| {
            let endpoint = endpoint_for(app.key, &base);
            let endpoint_copy_id = format!("client-endpoint-{}", app.key);
            let is_desktop = app.key == CLAUDE_DESKTOP_APP;
            let auto_link = is_desktop
                && super::desktop::claude_link::supported()
                && (!use_lan || self.desktop_link_waiting);
            let can_restore = auto_link && self.desktop_link_restorable;
            let app_model = if app.key == "claude" {
                model
            } else {
                model.trim_end_matches("[1m]")
            };
            let link =
                import_link(app.key, &base, key, app_model, "Amazon Quick").unwrap_or_default();
            let link_to_open = link.clone();
            let model_label = if is_desktop {
                model_display_name(default_model)
            } else {
                app_model.to_owned()
            };
            let import_button = Self::button(
                t,
                format!("client-import-{}", app.key),
                (!auto_link).then_some("icons/arrow-right-up-bold.svg"),
                false,
                if auto_link {
                    "CC Switch"
                } else {
                    "导入 CC Switch"
                },
                if is_desktop {
                    BtnVariant::Ghost
                } else {
                    BtnVariant::Primary
                },
                move |this, _, _| {
                    if open::that(&link_to_open).is_err() {
                        this.set_toast("无法打开 CC Switch，请确认已安装", true);
                    } else {
                        this.set_toast("已发送至 CC Switch", false);
                    }
                },
                cx,
            );
            let (protocol, icon, icon_bg) = match app.key {
                "claude" => (
                    "Anthropic Messages",
                    brand_image("client-logos/claude.svg"),
                    t.accent_soft,
                ),
                CLAUDE_DESKTOP_APP => (
                    "Anthropic Messages · 直连",
                    brand_image("client-logos/claude.svg"),
                    t.accent_soft,
                ),
                "codex" => (
                    "OpenAI Responses",
                    Self::icon("client-logos/openai.svg", 21.0, t.text_primary),
                    t.bg_well,
                ),
                "opencode" => (
                    "Chat Completions",
                    brand_image("client-logos/opencode.svg"),
                    rgba(0xf1f0efff).into(),
                ),
                _ => (
                    "Chat Completions",
                    Self::icon(Tab::Clients.icon_path(), 17.0, t.series_blue),
                    t.bg_well,
                ),
            };

            let body = div()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap(px(11.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .relative()
                                .size(px(33.0))
                                .flex_shrink_0()
                                .rounded(px(8.0))
                                .bg(icon_bg)
                                .border_1()
                                .border_color(t.border_subtle)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(icon)
                                .when(app.key == "claude", |el| {
                                    el.child(
                                        div()
                                            .absolute()
                                            .bottom(px(1.0))
                                            .right(px(1.0))
                                            .size(px(12.0))
                                            .rounded(px(3.0))
                                            .border_1()
                                            .border_color(t.border_subtle)
                                            .bg(t.bg_card)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(Self::icon(
                                                "client-logos/terminal.svg",
                                                8.0,
                                                t.text_secondary,
                                            )),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w(px(0.0))
                                .child(
                                    div()
                                        .text_size(px(13.5))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(app.label),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(t.text_muted)
                                        .child(protocol),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .text_size(px(12.0))
                        .text_color(t.text_secondary)
                        .child(if is_desktop {
                            "直连默认挡位"
                        } else {
                            "导入模型"
                        })
                        .child(Self::pill(
                            model_label,
                            t.accent_text,
                            t.accent_soft,
                            t.accent_border,
                            None,
                            None,
                        )),
                )
                .child(
                    Self::field(t)
                        .bg(t.bg_well)
                        .child(
                            div()
                                .id(SharedString::from(format!("endpoint-label-{}", app.key)))
                                .flex_1()
                                .min_w(px(0.0))
                                .py(px(4.0))
                                .font_family(MONO_FONT)
                                .text_size(px(12.0))
                                .whitespace_normal()
                                .tooltip(Self::text_tooltip(t, endpoint.clone()))
                                .child(endpoint.clone()),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!(
                                    "endpoint-copy-label-{}",
                                    app.key
                                )))
                                .tooltip(Self::text_tooltip(
                                    t,
                                    if Self::is_copied(shared, &endpoint_copy_id) {
                                        "已复制"
                                    } else {
                                        "复制接入地址"
                                    },
                                ))
                                .child(Self::copy_button(
                                    t,
                                    shared,
                                    endpoint_copy_id,
                                    "",
                                    endpoint,
                                    BtnVariant::Ghost,
                                    cx,
                                )),
                        ),
                );

            let card = Self::card(t)
                .id(app.key)
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .justify_between()
                .gap(px(11.0))
                .child(body)
                .child(
                    div()
                        .pt(px(10.0))
                        .border_t_1()
                        .border_color(t.border_subtle)
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .justify_between()
                        .gap(px(4.0))
                        .when(!is_desktop, |el| {
                            el.child(Self::copy_button(
                                t,
                                shared,
                                format!("client-link-{}", app.key),
                                "复制导入链接",
                                link,
                                BtnVariant::Ghost,
                                cx,
                            ))
                        })
                        .when(!can_restore, |el| el.child(import_button))
                        .when(can_restore && !self.desktop_link_waiting, |el| {
                            el.child(Self::button(
                                t,
                                "desktop-restore",
                                None,
                                false,
                                "恢复原接入",
                                BtnVariant::Ghost,
                                |this, _, cx| this.link_claude_desktop(true, cx),
                                cx,
                            ))
                        })
                        .when(is_desktop, |el| {
                            el.child(
                                div()
                                    .id("desktop-export-help")
                                    .tooltip(Self::text_tooltip(
                                        t,
                                        "在 Claude Desktop 的第三方推理设置中导入。",
                                    ))
                                    .child(Self::button(
                                        t,
                                        "desktop-export-config",
                                        None,
                                        self.desktop_export_pending,
                                        if self.desktop_export_pending {
                                            "正在导出"
                                        } else if auto_link {
                                            "导出配置"
                                        } else {
                                            "导出直连配置"
                                        },
                                        if auto_link {
                                            BtnVariant::Ghost
                                        } else {
                                            BtnVariant::Primary
                                        },
                                        move |this, _, cx| this.export_desktop_config(use_lan, cx),
                                        cx,
                                    )),
                            )
                        })
                        .when(auto_link, |el| {
                            el.child(Self::button(
                                t,
                                "desktop-link",
                                None,
                                false,
                                if self.desktop_link_waiting {
                                    "取消等待"
                                } else if can_restore {
                                    "更新接入"
                                } else {
                                    "一键接入"
                                },
                                if self.desktop_link_waiting {
                                    BtnVariant::SoftBlue
                                } else {
                                    BtnVariant::Primary
                                },
                                |this, _, cx| this.link_claude_desktop(false, cx),
                                cx,
                            ))
                        }),
                );
            motion::layout(card)
        });

        div()
            .flex()
            .flex_col()
            .gap(px(13.0))
            .child(Self::page_heading(t, "客户端接入"))
            .child(motion::layout(
                Self::card(t)
                    .id("client-options")
                    .flex()
                    .items_center()
                    .justify_between()
                    .flex_wrap()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(10.0))
                            .child(Self::gateway_status(
                                t,
                                shared,
                                "clients-gateway-loading",
                                false,
                            ))
                            .when_some(address_choice, |el, address_choice| {
                                el.child(Self::labeled_control(t, "接入范围", address_choice))
                            })
                            .when(lan.is_none(), |el| {
                                el.child(div().text_size(px(12.0)).text_color(t.text_muted).child(
                                    if self.lan_enabled() {
                                        "未检测到局域网地址"
                                    } else {
                                        "本机接入"
                                    },
                                ))
                            }),
                    )
                    .child(Self::labeled_control(t, "CC Switch 导入模型", model_choice)),
            ))
            .child(
                div()
                    .grid()
                    .grid_cols(columns)
                    .gap(card_gap)
                    .children(cards),
            )
    }

    fn labeled_control(t: Theme, title: &'static str, control: AnyElement) -> impl IntoElement {
        motion::layout(
            div()
                .id(title)
                .flex()
                .items_center()
                .gap(px(10.0))
                .text_size(px(12.5))
                .text_color(t.text_secondary)
                .child(title)
                .child(control),
        )
    }

    fn setting_row(
        t: Theme,
        title: &'static str,
        detail: &'static str,
        control: AnyElement,
    ) -> impl IntoElement {
        motion::layout(
            div()
                .id(title)
                .flex()
                .items_center()
                .gap(px(18.0))
                .min_h(px(54.0))
                .py(px(9.0))
                .border_t_1()
                .border_color(t.border_subtle)
                .child(
                    div()
                        .w(relative(0.40))
                        .min_w(px(0.0))
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .font_weight(FontWeight::MEDIUM)
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(t.text_muted)
                                .child(detail),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_w(px(0.0))
                        .items_center()
                        .justify_end()
                        .child(control),
                ),
        )
    }

    fn render_settings(&self, t: Theme, shared: &GuiShared, cx: &mut Context<Self>) -> Div {
        let config = &self.snapshot.config;
        let settings = Settings::from_options(&config.options);

        let listener_editable = shared.listener_editable();
        let confirming_quit = self.pending_action == Some(PendingAction::Quit);
        let listen = self.lan_access_control(t, shared, "settings-lan", cx);
        let mut port_choices = vec![18765u16, 18766, 8765, 8080];
        if !port_choices.contains(&config.port) {
            port_choices.push(config.port);
        }
        let ports = Self::segmented_wrap(
            t,
            "settings-port-seg",
            port_choices
                .into_iter()
                .map(|port| {
                    Self::seg_item(
                        t,
                        format!("s-p-{port}"),
                        port.to_string(),
                        config.port == port,
                        move |this, _, _| this.set_port(port),
                        cx,
                    )
                })
                .collect(),
        );
        let concurrency = Self::segmented_wrap(
            t,
            "settings-conc-seg",
            [2usize, 4, 8]
                .into_iter()
                .map(|limit| {
                    Self::seg_item(
                        t,
                        format!("s-c-{limit}"),
                        limit.to_string(),
                        settings.max_concurrency == limit,
                        move |this, _, _| this.set_option("max_concurrency", json!(limit)),
                        cx,
                    )
                })
                .collect(),
        );
        let models = Self::segmented_wrap(
            t,
            "settings-model-seg",
            ["smart", "balanced", "fast", "auto"]
                .into_iter()
                .map(|model| {
                    Self::seg_item(
                        t,
                        format!("s-m-{model}"),
                        model,
                        settings.default_model == model,
                        move |this, _, _| this.set_option("default_model", json!(model)),
                        cx,
                    )
                })
                .collect(),
        );
        let effort = Self::segmented_wrap(
            t,
            "settings-effort-seg",
            [
                ("", "自动"),
                ("off", "off"),
                ("low", "low"),
                ("medium", "medium"),
                ("high", "high"),
            ]
            .into_iter()
            .map(|(value, label)| {
                Self::seg_item(
                    t,
                    format!("s-e-{label}"),
                    label,
                    settings.thinking_effort == value,
                    move |this, _, _| this.set_option("thinking_effort", json!(value)),
                    cx,
                )
            })
            .collect(),
        );
        let toggle_row = |title, detail, key: &'static str, current, cx: &mut Context<Self>| {
            Self::setting_row(
                t,
                title,
                detail,
                Self::toggle_switch(
                    t,
                    key,
                    current,
                    move |this, _, _| this.set_option(key, json!(!current)),
                    cx,
                ),
            )
        };

        div()
            .flex()
            .flex_col()
            .gap(px(13.0))
            .child(Self::page_heading(t, "设置"))
            .child(motion::layout(
                Self::card(t)
                    .id("gateway-settings")
                    .flex()
                    .flex_col()
                    .child(Self::section_header(t, "网关"))
                    .child(Self::setting_row(
                        t,
                        "允许局域网访问",
                        if listener_editable {
                            "开启后，同一局域网中的其他设备可以接入"
                        } else {
                            "停止网关后可修改接入范围和端口"
                        },
                        listen,
                    ))
                    .child(Self::setting_row(
                        t,
                        "端口",
                        "客户端使用此端口连接网关",
                        if listener_editable {
                            ports
                        } else {
                            div()
                                .font(numeric_font())
                                .text_color(t.text_muted)
                                .child(config.port.to_string())
                                .into_any_element()
                        },
                    ))
                    .child(Self::setting_row(
                        t,
                        "单账号并发",
                        "每个账号同时处理的请求上限",
                        concurrency,
                    )),
            ))
            .child(motion::layout(
                Self::card(t)
                    .id("conversation-settings")
                    .flex()
                    .flex_col()
                    .child(Self::section_header(t, "对话"))
                    .child(Self::setting_row(
                        t,
                        "默认挡位",
                        "客户端未指定模型时使用",
                        models,
                    ))
                    .child(Self::setting_row(
                        t,
                        "思考强度",
                        "请求未指定时采用的默认强度",
                        effort,
                    ))
                    .child(toggle_row(
                        "多轮线程复用",
                        "匹配到会话时继续使用原有 Quick 线程",
                        "stateful",
                        settings.stateful,
                        cx,
                    ))
                    .child(toggle_row(
                        "超长与图片转附件",
                        "通过附件承载超出帧预算的文本和内嵌图片",
                        "attachments",
                        settings.attachments,
                        cx,
                    ))
                    .child(toggle_row(
                        "保留 Quick 历史",
                        "在 Amazon Quick 中保存会话记录",
                        "keep_sessions",
                        settings.keep_sessions,
                        cx,
                    ))
                    .child(toggle_row(
                        "允许内置联网搜索",
                        "设置 Quick 自带联网搜索的默认状态",
                        "web_search",
                        settings.web_search,
                        cx,
                    ))
                    .child(toggle_row(
                        "显示 Quick 内置工具",
                        "在推理文本中显示 Quick 自带的工具调用",
                        "show_quick_tools",
                        settings.show_quick_tools,
                        cx,
                    ))
                    .child(toggle_row(
                        "固定设备 ID",
                        "使用当前账号已保存的设备标识",
                        "stable_device_id",
                        settings.stable_device_id,
                        cx,
                    )),
            ))
            .child(motion::layout(
                Self::card(t)
                    .id("application-settings")
                    .flex()
                    .flex_col()
                    .child(Self::section_header(t, "应用"))
                    .child(Self::setting_row(
                        t,
                        "关闭窗口行为",
                        if cfg!(target_os = "macos") {
                            "收起到菜单栏，网关继续运行"
                        } else {
                            "收起到后台，网关继续运行"
                        },
                        Self::pill(
                            "后台常驻",
                            t.text_secondary,
                            t.bg_well,
                            t.border_subtle,
                            None,
                            None,
                        ),
                    ))
                    .child(Self::setting_row(
                        t,
                        "退出 Quick Bridge",
                        if confirming_quit {
                            "退出后客户端连接会中断。"
                        } else {
                            "关闭应用并停止正在运行的网关"
                        },
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(Self::button(
                                t,
                                "quit-application",
                                None,
                                false,
                                if confirming_quit {
                                    "取消"
                                } else {
                                    "退出应用"
                                },
                                if confirming_quit {
                                    BtnVariant::Secondary
                                } else {
                                    BtnVariant::SoftDanger
                                },
                                |this, _, cx| {
                                    if this.pending_action == Some(PendingAction::Quit) {
                                        this.pending_action = None;
                                    } else if this.shared.lock().listener_editable() {
                                        cx.quit();
                                    } else {
                                        this.toggle_action(PendingAction::Quit);
                                    }
                                },
                                cx,
                            ))
                            .when(confirming_quit, |el| {
                                el.child(Self::button(
                                    t,
                                    "confirm-quit",
                                    None,
                                    false,
                                    "确认退出",
                                    BtnVariant::SoftDanger,
                                    |_, _, cx| cx.quit(),
                                    cx,
                                ))
                            })
                            .into_any_element(),
                    )),
            ))
    }

    fn log_colors(t: Theme, level: LogLevel) -> (Hsla, Hsla, Hsla, Hsla) {
        match level {
            LogLevel::Info => (t.green_text, t.green_soft, t.green_border, t.green),
            LogLevel::Warn => (t.amber_text, t.amber_soft, t.amber_border, t.amber),
            LogLevel::Error => (t.red_text, t.red_soft, t.red_border, t.red),
        }
    }

    fn select_log_filter(&mut self, filter: LogFilter) {
        if self.log_filter != filter {
            self.log_filter = filter;
            self.log_page = 0;
        }
    }

    fn fit_log_message(
        text_system: &Arc<TextSystem>,
        message: SharedString,
        width: Pixels,
        color: Hsla,
    ) -> (SharedString, Vec<TextRun>) {
        let mut font = ui_font();
        font.family = MONO_FONT.into();
        let mut runs = vec![TextRun {
            len: message.len(),
            font: font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        }];
        let text = text_system
            .line_wrapper(font, px(LOG_MESSAGE_FONT_SIZE))
            .truncate_line(message, width, "…", &mut runs);
        (text, runs)
    }

    fn log_message(t: Theme, message: String) -> impl IntoElement {
        let message = SharedString::from(if message.contains(['\r', '\n']) {
            message.replace(['\r', '\n'], " ")
        } else {
            message
        });
        canvas(
            move |bounds, window, cx| {
                if message.is_empty() || bounds.size.width <= px(0.0) {
                    return None;
                }
                // GPUI 的省略逻辑会缩短字体区间，因此按最终列宽创建新的区间。
                let (text, runs) = Self::fit_log_message(
                    cx.text_system(),
                    message.clone(),
                    bounds.size.width,
                    t.text_primary,
                );
                Some(
                    window
                        .text_system()
                        .shape_line(text, px(LOG_MESSAGE_FONT_SIZE), &runs, None),
                )
            },
            |bounds, line, window, cx| {
                if let Some(line) = line {
                    window.with_content_mask(Some(ContentMask { bounds }), |window| {
                        let _ = line.paint(bounds.origin, bounds.size.height, window, cx);
                    });
                }
            },
        )
        .w_full()
        .h(px(17.0))
    }

    fn log_rows(logs: &[RequestLogEntry]) -> impl Iterator<Item = (&RequestLogEntry, usize)> {
        let mut occurrences = HashMap::new();
        logs.iter().rev().map(move |entry| {
            let occurrence = occurrences.entry(entry.timestamp).or_insert(0_usize);
            let row = (entry, *occurrence);
            *occurrence += 1;
            row
        })
    }

    fn render_log_entry(t: Theme, entry: &RequestLogEntry, occurrence: usize) -> AnyElement {
        let (fg, bg, border, dot) = Self::log_colors(t, entry.level);
        let (level, icon) = match entry.level {
            LogLevel::Info => ("信息", "icons/document-text-bold.svg"),
            LogLevel::Warn => ("告警", "icons/danger-circle-bold.svg"),
            LogLevel::Error => ("错误", "icons/danger-circle-bold.svg"),
        };
        let protocol = match entry.api.as_str() {
            "messages" => "Anthropic Messages",
            "responses" => "OpenAI Responses",
            "chat" => "Chat Completions",
            "system" => "系统",
            "probe" => "模型探测",
            other => other,
        };
        let status = if entry.status > 0 {
            entry.status.to_string()
        } else {
            "—".into()
        };
        let has_tokens =
            entry.input_tokens > 0 || entry.output_tokens > 0 || entry.cached_tokens > 0;
        let tokens = if has_tokens {
            format!(
                "{}/{} ({})",
                entry.input_tokens, entry.output_tokens, entry.cached_tokens
            )
        } else {
            "—".into()
        };
        let latency = if entry.latency_ms > 0 {
            format!("{} ms", entry.latency_ms)
        } else {
            "—".into()
        };

        let row_id = (SharedString::from(entry.timestamp.to_rfc3339()), occurrence);
        let row = Self::table_row(t)
            .id(row_id)
            .hover(move |style| style.bg(t.bg_well))
            .text_size(px(11.5))
            .text_color(t.text_secondary)
            .child(
                div()
                    .id("log-description")
                    .flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .items_center()
                    .gap(px(9.0))
                    .tooltip(Self::text_tooltip(t, entry.message.clone()))
                    .child(
                        div()
                            .size(px(27.0))
                            .flex_shrink_0()
                            .rounded(px(7.0))
                            .bg(bg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Self::icon(icon, 14.0, fg)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(Self::log_message(t, entry.message.clone()))
                            .child(
                                div()
                                    .font(ui_font())
                                    .text_size(px(10.5))
                                    .text_color(t.text_muted)
                                    .truncate()
                                    .child(protocol.to_string()),
                            ),
                    ),
            )
            .child(
                Self::table_cell(72.0)
                    .font(ui_font())
                    .flex()
                    .child(Self::pill(level, fg, bg, border, Some(dot), None)),
            )
            .child(
                Self::table_cell(48.0)
                    .font(numeric_font())
                    .text_right()
                    .child(status),
            )
            .child(
                Self::table_cell(110.0)
                    .id("log-tokens")
                    .font(numeric_font())
                    .text_right()
                    .when(has_tokens, |el| {
                        el.tooltip(Self::text_tooltip(
                            t,
                            format!(
                                "输入 {} · 输出 {} · 缓存 {}",
                                entry.input_tokens, entry.output_tokens, entry.cached_tokens,
                            ),
                        ))
                    })
                    .child(tokens),
            )
            .child(
                Self::table_cell(LOG_LATENCY_WIDTH)
                    .font(numeric_font())
                    .text_right()
                    .child(latency),
            )
            .child(
                Self::table_cell(68.0)
                    .font(numeric_font())
                    .text_right()
                    .child(
                        entry
                            .timestamp
                            .with_timezone(&Local)
                            .format("%H:%M:%S")
                            .to_string(),
                    ),
            );
        motion::layout(row).into_any_element()
    }

    fn render_logs(&mut self, t: Theme, cx: &mut Context<Self>) -> Div {
        let logs = &self.snapshot.logs;
        let confirming_clear = self.pending_action == Some(PendingAction::ClearLogs);
        let has_warning = logs.iter().any(|entry| entry.level == LogLevel::Warn);
        let has_error = logs.iter().any(|entry| entry.level == LogLevel::Error);
        let filters = [
            (LogFilter::All, "全部"),
            (LogFilter::InfoOnly, "信息"),
            (
                LogFilter::IssuesOnly,
                if has_error {
                    "告警与错误"
                } else {
                    "告警"
                },
            ),
            (LogFilter::ErrorsOnly, "错误"),
        ]
        .map(|(filter, label)| {
            let count = logs
                .iter()
                .filter(|entry| filter.matches(entry.level))
                .count();
            (filter, label, count)
        });
        let show_log_filters = self.log_filter != LogFilter::All
            || filters.iter().any(|(filter, _, count)| {
                *filter != LogFilter::All && *count > 0 && *count < logs.len()
            });
        let mut filtered = Self::log_rows(logs)
            .filter(|(entry, _)| self.log_filter.matches(entry.level))
            .collect::<Vec<_>>();
        if self.log_sort != NumberSort::Default {
            filtered.sort_by(|(left, _), (right, _)| {
                (left.latency_ms == 0)
                    .cmp(&(right.latency_ms == 0))
                    .then_with(|| self.log_sort.compare(left.latency_ms, right.latency_ms))
            });
        }
        let page = TablePage::new(filtered.len(), self.log_page);
        self.log_page = page.index;
        let toolbar = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(10.0))
            .px(px(14.0))
            .py(px(10.0))
            .when(show_log_filters, |el| {
                el.child(
                    div().flex().items_center().gap(px(5.0)).children(
                        filters
                            .into_iter()
                            .enumerate()
                            .filter(|(_, (filter, _, count))| {
                                *filter == LogFilter::All
                                    || *filter == self.log_filter
                                    || (*count > 0
                                        && *count < logs.len()
                                        && (*filter != LogFilter::IssuesOnly || has_warning))
                            })
                            .map(|(index, (filter, label, count))| {
                                Self::filter_chip(
                                    t,
                                    format!("log-filter-{index}"),
                                    label,
                                    count,
                                    self.log_filter == filter,
                                    move |this| this.select_log_filter(filter),
                                    cx,
                                )
                            }),
                    ),
                )
            })
            .when(!show_log_filters, |el| {
                el.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(t.text_muted)
                        .child(format!("共 {} 条日志", logs.len())),
                )
            })
            .child(Self::button(
                t,
                "logs-clear",
                (!confirming_clear).then_some("icons/trash-bin-trash-bold.svg"),
                false,
                if confirming_clear {
                    "取消"
                } else {
                    "清空日志"
                },
                BtnVariant::Secondary,
                |this, _, _| this.toggle_action(PendingAction::ClearLogs),
                cx,
            ));

        let mut table = Self::table_frame(t)
            .when(!logs.is_empty(), |el| el.child(toolbar))
            .when(confirming_clear && !logs.is_empty(), |el| {
                el.child(div().px(px(14.0)).pb(px(10.0)).child(Self::confirm_action(
                    t,
                    "confirm-log-clear",
                    "这会清除本地日志及概览的请求统计。",
                    "确认清空",
                    |this, _, _| {
                        let _runtime = this.rt.enter();
                        this.pool.clear_logs();
                        this.log_filter = LogFilter::All;
                        this.log_page = 0;
                        this.set_toast("已清空日志", false);
                    },
                    cx,
                )))
            });
        if filtered.is_empty() {
            let empty = if logs.is_empty() {
                Self::empty_state(t, Tab::Logs.icon_path(), "暂无日志").child(
                    div()
                        .text_size(px(12.0))
                        .text_color(t.text_muted)
                        .child("网关事件和客户端请求会显示在这里"),
                )
            } else {
                Self::empty_state(t, Tab::Logs.icon_path(), "没有符合条件的日志").child(
                    Self::button(
                        t,
                        "logs-reset-filter",
                        None,
                        false,
                        "查看全部",
                        BtnVariant::Secondary,
                        |this, _, _| this.select_log_filter(LogFilter::All),
                        cx,
                    ),
                )
            };
            table = table.child(
                div()
                    .px(px(14.0))
                    .pb(px(14.0))
                    .when(logs.is_empty(), |el| el.pt(px(14.0)))
                    .child(empty),
            );
        } else {
            let range = page.range();
            table = table
                .child(motion::layout(
                    Self::table_header(t)
                        .id("log-columns")
                        .child(div().flex_1().min_w(px(0.0)).child("请求详情"))
                        .child(Self::table_cell(72.0).child("级别"))
                        .child(Self::table_cell(48.0).text_right().child("HTTP"))
                        .child(Self::table_cell(110.0).text_right().child("Tokens"))
                        .child(Self::sortable_column(
                            t,
                            LOG_LATENCY_WIDTH,
                            "sort-log-latency",
                            "耗时",
                            self.log_sort,
                            |this| {
                                this.log_sort = this.log_sort.cycle();
                                this.log_page = 0;
                            },
                            cx,
                        ))
                        .child(Self::table_cell(68.0).text_right().child("时间")),
                ))
                .children(
                    filtered[range]
                        .iter()
                        .map(|(entry, occurrence)| Self::render_log_entry(t, entry, *occurrence)),
                )
                .when(page.has_multiple_pages(), |el| {
                    el.child(Self::table_footer(
                        t,
                        "log-page",
                        page,
                        |this, index| this.log_page = index,
                        cx,
                    ))
                });
        }

        div()
            .flex()
            .flex_col()
            .gap(px(13.0))
            .child(Self::page_heading(t, "日志"))
            .child(motion::layout(table.id("log-table")))
    }
}

impl Render for BridgeApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = motion::begin_frame(window, cx.entity_id());
        let darkness = self.theme_amount(now);
        if let Some((started, _)) = self.theme_transition {
            if motion::is_reduced_motion() || now.duration_since(started) >= THEME_TRANSITION {
                self.theme_transition = None;
            } else {
                motion::request_frame(window);
            }
        }
        let mut t = Theme::interpolated(darkness);
        t.keyboard_focus = self.keyboard_focus;
        let shared = self.visible_state.clone();
        let width = window.viewport_size().width;
        let compact = width < px(1020.0);
        let page_padding = if compact { 14.0 } else { 18.0 };
        let section = match self.active_tab {
            Tab::Settings | Tab::Logs => "管理",
            _ => "工作区",
        };
        let content = match self.active_tab {
            Tab::Dashboard => self.render_dashboard(t, &shared, compact, cx),
            Tab::Usage => self.render_usage(t, compact, cx),
            Tab::Accounts => {
                if self.account_card_highlight.is_animating(now) {
                    motion::request_frame(window);
                }
                let columns = if compact {
                    1
                } else if width >= px(1440.0) {
                    3
                } else {
                    2
                };
                self.render_accounts(t, &shared, columns, cx)
            }
            Tab::Clients => {
                // 页面外框左右各占 1 px。
                let insets = SIDEBAR_WIDTH + PAGE_RIGHT_PADDING + 2.0 * (page_padding + 1.0);
                let content_width = (width - px(insets)).min(PAGE_CONTENT_MAX_WIDTH);
                self.render_clients(t, &shared, content_width, cx)
            }
            Tab::Settings => self.render_settings(t, &shared, cx),
            Tab::Logs => self.render_logs(t, cx),
        }
        .w_full()
        .max_w(PAGE_CONTENT_MAX_WIDTH)
        .mx_auto();

        div()
            .id("quick-bridge-app")
            .relative()
            .flex()
            .size_full()
            .bg(t.bg_window)
            .font(ui_font())
            .text_color(t.text_primary)
            .child(self.render_sidebar(t, darkness, &shared, cx))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .pr(px(PAGE_RIGHT_PADDING))
                    .pb(px(12.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .h(px(HEADER_HEIGHT))
                            .flex_shrink_0()
                            .px(px(3.0))
                            .child(
                                div()
                                    .id("window-titlebar")
                                    .flex_1()
                                    .h_full()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .text_size(px(12.0))
                                    .text_color(t.text_muted)
                                    .when(cfg!(target_os = "macos"), |header| {
                                        header.on_click(|event, window, _| {
                                            if event.click_count() == 2 && !window.is_fullscreen() {
                                                window.titlebar_double_click();
                                            }
                                        })
                                    })
                                    .when(cfg!(target_os = "windows"), |header| {
                                        header.window_control_area(gpui::WindowControlArea::Drag)
                                    })
                                    .child(Self::icon(
                                        self.active_tab.icon_path(),
                                        14.0,
                                        t.text_muted,
                                    ))
                                    .child(section)
                                    .child("›")
                                    .child(
                                        div()
                                            .text_color(t.text_primary)
                                            .child(self.active_tab.title()),
                                    ),
                            )
                            .child({
                                let dark_mode = self.dark_mode;
                                Self::button_with_content(
                                    t,
                                    "theme-toggle",
                                    false,
                                    true,
                                    BtnVariant::Secondary,
                                    move |color| {
                                        let label = if dark_mode {
                                            "切换浅色"
                                        } else {
                                            "切换深色"
                                        };
                                        [
                                            Self::theme_toggle_icon(darkness, color)
                                                .into_any_element(),
                                            label.into_any_element(),
                                        ]
                                    },
                                    |this, _, _| this.toggle_theme(),
                                    cx,
                                )
                            }),
                    )
                    .child(
                        Self::card(t)
                            .p(px(0.0))
                            .flex_1()
                            .min_h(px(0.0))
                            .overflow_hidden()
                            .child(
                                div()
                                    .id(("page-scroll", self.active_tab.index()))
                                    .size_full()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.page_scroll[self.active_tab.index()])
                                    .p(px(page_padding))
                                    .child(motion::slide_fade(
                                        ("page-transition", self.tab_transition_seq),
                                        0.80,
                                        1.0,
                                        0.0,
                                        f32::from(self.tab_direction) * 18.0,
                                        SpringConfig::SMOOTH,
                                        content,
                                    )),
                            ),
                    ),
            )
            .track_focus(&self.focus)
            .capture_any_mouse_down(cx.listener(|this, _, _, cx| {
                if std::mem::take(&mut this.keyboard_focus) {
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                match event.keystroke.key.as_str() {
                    "escape" if this.pending_action.take().is_some() => cx.notify(),
                    "tab" if !modifiers.control && !modifiers.alt && !modifiers.platform => {
                        this.keyboard_focus = true;
                        if modifiers.shift {
                            window.focus_prev();
                        } else {
                            window.focus_next();
                        }
                        cx.notify();
                    }
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .when(
                self.account_menu
                    .as_ref()
                    .is_some_and(|menu| menu.is_open()),
                |el| el.child(Self::account_menu_backdrop(cx)),
            )
            .when(
                self.usage
                    .dropdown
                    .as_ref()
                    .is_some_and(|dropdown| dropdown.is_open()),
                |el| el.child(Self::usage_dropdown_backdrop(cx)),
            )
            .when(!shared.toasts.is_empty(), |el| {
                el.child(Self::render_toasts(
                    t,
                    &shared.toasts,
                    px(HEADER_HEIGHT + 12.0),
                    compact,
                    window,
                    cx,
                ))
            })
    }
}
