use crate::gui::activity::{ActivityRange, RequestActivity};
use crate::gui::charts::format_tokens;
use crate::gui::components::{
    BtnVariant, QuotaTrendPoint, Theme, format_quota_credits, quota_dots, quota_percent,
    quota_remaining,
};
use crate::gui::motion::{self, SpringConfig};
use crate::gui::{BridgeApp, MONO_FONT, logo_source, numeric_font, ui_font};
use crate::pool::{AccountSnapshot, PoolSnapshot, RequestSample};
use crate::qbs::QuotaWindow;
use chrono::{Local, Utc};
use gpui::{
    Bounds, Context, Div, Entity, FocusHandle, FontWeight, Hsla, KeyDownEvent, ObjectFit,
    SharedString, Task, Timer, Transformation, Window, canvas, div, fill, img, linear_color_stop,
    linear_gradient, point, prelude::*, px, radians, size,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) const MAIN_SIZE: (f64, f64) = (365.0, 598.0);
pub(super) const DETAIL_SIZE: (f64, f64) = (312.0, 374.0);
pub(super) const SHADOW_INSET: f32 = 10.0;

const RADIUS: f32 = 15.0;
const COPY_ADDRESS: &str = "menu-copy-address";
const STAGE_TIME: f32 = 0.29;

pub(super) enum PanelContent {
    Overview,
    Quota(String),
}

struct Usage {
    samples: Arc<Vec<RequestSample>>,
    minute: i64,
    activity: RequestActivity,
}

impl Usage {
    fn new(bridge: &BridgeApp) -> Self {
        let now = Utc::now();
        let samples = bridge.pool.request_samples();
        Self {
            activity: RequestActivity::from_samples(&samples, now, ActivityRange::Day),
            samples,
            minute: now.timestamp().div_euclid(60),
        }
    }

    fn refresh(&mut self, bridge: &BridgeApp) {
        let samples = bridge.pool.request_samples();
        if !Arc::ptr_eq(&self.samples, &samples)
            || self.minute != Utc::now().timestamp().div_euclid(60)
        {
            *self = Self::new(bridge);
        }
    }
}

pub(super) struct MenuPanel {
    bridge: Entity<BridgeApp>,
    content: PanelContent,
    usage: Option<Usage>,
    opened_at: Option<Instant>,
    closing: Option<Instant>,
    selected_quota: Option<String>,
    focus: FocusHandle,
    keyboard_focus: bool,
    confirm_quit: bool,
    _minute_refresh: Option<Task<()>>,
}

impl MenuPanel {
    pub(super) fn new(
        content: PanelContent,
        bridge: Entity<BridgeApp>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let usage = matches!(content, PanelContent::Overview).then(|| Usage::new(bridge.read(cx)));
        cx.observe(&bridge, |panel, bridge, cx| {
            if let Some(usage) = &mut panel.usage {
                usage.refresh(bridge.read(cx));
            }
            cx.notify();
        })
        .detach();

        let minute_refresh = usage.as_ref().map(|_| {
            cx.spawn(async move |this, cx| {
                loop {
                    Timer::after(Duration::from_secs(60)).await;
                    if this
                        .update(cx, |panel, cx| {
                            if let Some(usage) = &mut panel.usage {
                                usage.refresh(panel.bridge.read(cx));
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
        });
        cx.observe_window_activation(window, |_, window, cx| {
            if !window.is_window_active()
                && let Some(handle) = window.window_handle().downcast::<MenuPanel>()
            {
                cx.defer(move |cx| super::dismiss_if_unfocused(cx, handle));
            }
        })
        .detach();

        let focus = cx.focus_handle();
        window.focus(&focus);
        Self {
            bridge,
            content,
            usage,
            opened_at: None,
            closing: None,
            selected_quota: None,
            focus,
            keyboard_focus: false,
            confirm_quit: false,
            _minute_refresh: minute_refresh,
        }
    }

    pub(super) fn closing(&self) -> Option<Instant> {
        self.closing
    }

    pub(super) fn begin_close(&mut self, at: Instant, cx: &mut Context<Self>) -> bool {
        if self.closing.is_some() {
            return false;
        }
        self.closing = Some(at);
        self.confirm_quit = false;
        cx.notify();
        true
    }

    pub(super) fn reopen(&mut self, cx: &mut Context<Self>) -> bool {
        let reopening = self.closing.take().is_some();
        if reopening {
            cx.notify();
        }
        reopening
    }

    pub(super) fn select_tile(&mut self, key: Option<String>, cx: &mut Context<Self>) {
        self.selected_quota = key;
        cx.notify();
    }

    pub(super) fn select_quota(&mut self, account_key: String, cx: &mut Context<Self>) {
        self.content = PanelContent::Quota(account_key);
        cx.notify();
    }

    fn render_overview(
        &self,
        t: Theme,
        darkness: f32,
        elapsed: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        let status = {
            let shared = self.bridge.read(cx).shared.lock();
            BridgeApp::gateway_status(t, &shared, "menu-gateway-status", false)
        };
        let heading = header(t)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .child(
                        div()
                            .relative()
                            .w(px(53.0))
                            .h(px(27.0))
                            .flex_shrink_0()
                            .children(
                                [(false, 1.0 - darkness), (true, darkness)]
                                    .into_iter()
                                    .filter(|(_, alpha)| *alpha > 0.0)
                                    .map(|(dark, alpha)| {
                                        img(logo_source(dark))
                                            .absolute()
                                            .inset_0()
                                            .size_full()
                                            .object_fit(ObjectFit::Contain)
                                            .opacity(alpha)
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(15.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Quick Bridge"),
                    ),
            )
            .child(status);
        let gateway = self.gateway(t, cx);
        let accounts = self.accounts(t, cx);
        let footer = self.overview_footer(t, cx);
        let mut body = div().flex().flex_col();
        body = body
            .child(staged(t, elapsed, 0.08, gateway, true))
            .child(staged(t, elapsed, 0.12, accounts, true));
        if let Some(usage) = &self.usage {
            body = body
                .child(staged(
                    t,
                    elapsed,
                    0.16,
                    request_usage(t, &usage.activity),
                    true,
                ))
                .child(staged(
                    t,
                    elapsed,
                    0.20,
                    protocol_mix(t, &usage.activity),
                    false,
                ));
        }
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(heading)
            .child(scroller(body))
            .child(footer)
    }

    fn gateway(&self, t: Theme, cx: &mut Context<Self>) -> Div {
        let (address, action, copied, running, busy, failure) = {
            let bridge = self.bridge.read(cx);
            let shared = bridge.shared.lock();
            (
                bridge.base_url(false),
                shared.gateway_button_content(),
                BridgeApp::is_copied(&shared, COPY_ADDRESS),
                shared.server_running,
                shared.gateway_busy,
                shared.gateway_start_error.clone(),
            )
        };
        let copy = BridgeApp::button(
            t,
            COPY_ADDRESS,
            Some(if copied {
                "icons/check-circle-bold.svg"
            } else {
                "icons/copy-bold.svg"
            }),
            false,
            if copied { "已复制" } else { "复制" },
            BtnVariant::Text(if copied {
                t.green_text
            } else {
                t.text_secondary
            }),
            |panel: &mut Self, _, cx| {
                panel.bridge.update(cx, |bridge, cx| {
                    bridge.copy_with_feedback(COPY_ADDRESS, bridge.base_url(false), cx);
                })
            },
            cx,
        );
        let toggle = BridgeApp::button(
            t,
            "menu-gateway-toggle",
            Some(action.0),
            busy,
            action.1,
            if running {
                BtnVariant::Text(t.red_text)
            } else {
                BtnVariant::SoftBlue
            },
            move |panel: &mut Self, _, cx| {
                let bridge = panel.bridge.read(cx);
                if running {
                    bridge.stop_gateway();
                } else {
                    bridge.start_gateway();
                }
            },
            cx,
        );
        div()
            .flex()
            .flex_col()
            .py(px(4.0))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex()
                            .items_center()
                            .gap(px(9.0))
                            .child(
                                div()
                                    .size(px(27.0))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(7.0))
                                    .bg(t.blue_soft)
                                    .child(BridgeApp::icon(
                                        "icons/server-bold.svg",
                                        14.0,
                                        t.blue_text,
                                    )),
                            )
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .flex()
                                    .flex_col()
                                    .gap(px(3.0))
                                    .child(label(t, "本机地址"))
                                    .child(
                                        div()
                                            .font_family(MONO_FONT)
                                            .text_size(px(12.0))
                                            .truncate()
                                            .child(
                                                address.trim_start_matches("http://").to_owned(),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .flex_col()
                            .items_end()
                            .child(copy)
                            .child(toggle),
                    ),
            )
            .when_some(failure, |el, (title, detail)| {
                el.child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(6.0))
                        .pb(px(6.0))
                        .child(BridgeApp::icon(
                            "icons/danger-circle-bold.svg",
                            13.0,
                            t.red_text,
                        ))
                        .child(
                            div()
                                .min_w(px(0.0))
                                .text_size(px(11.5))
                                .whitespace_normal()
                                .child(div().text_color(t.red_text).child(title))
                                .child(div().text_color(t.text_secondary).child(detail)),
                        ),
                )
            })
    }

    fn accounts(&self, t: Theme, cx: &mut Context<Self>) -> Div {
        let bridge = self.bridge.read(cx);
        let account = visible_account(&bridge.snapshot);
        let heading = row().child(section_title("账号与额度")).child(label(
            t,
            format!(
                "{} / {} 可用",
                bridge.available_account_count(),
                bridge.snapshot.accounts.len()
            ),
        ));
        let section = div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .py(px(10.0))
            .child(heading);
        let Some((account, usage)) = account else {
            return section.child(label(t, "暂无可用账号"));
        };
        let windows = bridge.account_quota_windows(account);
        let loading = bridge
            .visible_state
            .quota_loading_keys
            .contains(&account.key);
        let email = identity(t, account, usage, None);
        let month = compact_quota(
            t,
            "月度额度",
            named_quota(windows, "Monthly"),
            loading,
            t.accent_text,
        );
        let session = compact_quota(
            t,
            "会话额度",
            named_quota(windows, "Session"),
            loading,
            t.blue_text,
        );
        let has_stale_quota = !windows.is_empty()
            && !loading
            && (account.quota.is_none() || account.quota_error.is_some());
        let failure = account.quota_error.clone();
        let key = account.key.clone();
        let selected = self.selected_quota.as_ref() == Some(&key);
        let month = month
            .id("menu-monthly-quota")
            .relative()
            .rounded(px(8.0))
            .cursor_pointer()
            .focusable()
            .tab_stop(true)
            .tooltip(BridgeApp::text_tooltip(
                t,
                if selected {
                    "收起额度详情"
                } else {
                    "查看额度详情"
                },
            ))
            .focus(move |style| style.bg(t.blue_soft))
            .hover(move |style| style.bg(if selected { t.blue_soft } else { t.bg_hover }))
            .when(selected, |el| el.bg(t.blue_soft))
            .child(
                div().absolute().right(px(8.0)).top(px(9.0)).child(
                    BridgeApp::raw_icon(
                        "icons/alt-arrow-down-bold.svg",
                        10.0,
                        if selected { t.blue_text } else { t.text_muted },
                    )
                    .with_transformation(Transformation::rotate(radians(
                        if selected {
                            std::f32::consts::FRAC_PI_2
                        } else {
                            -std::f32::consts::FRAC_PI_2
                        },
                    ))),
                ),
            )
            .on_click(cx.listener(move |_, _, _, cx| {
                let key = key.clone();
                cx.defer(move |cx| super::toggle_quota(cx, key));
            }));
        section
            .child(email)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(month.flex_1())
                    .child(
                        div()
                            .h(px(39.0))
                            .w(px(1.0))
                            .flex_shrink_0()
                            .bg(t.border_subtle),
                    )
                    .child(session.flex_1()),
            )
            .when(has_stale_quota, |el| {
                el.child(label(t, "显示上次成功读取的额度"))
            })
            .when(!loading && !has_stale_quota, |el| {
                el.when_some(failure, |el, error| {
                    el.child(
                        label(t, "额度读取失败")
                            .id("menu-quota-failure")
                            .tooltip(BridgeApp::text_tooltip(t, error)),
                    )
                })
            })
    }

    fn overview_footer(&self, t: Theme, cx: &mut Context<Self>) -> Div {
        if self.confirm_quit {
            return footer(t).child(label(t, "退出会中断客户端连接")).child(
                div()
                    .flex()
                    .gap(px(4.0))
                    .child(BridgeApp::button(
                        t,
                        "menu-cancel-quit",
                        None,
                        false,
                        "取消",
                        BtnVariant::Ghost,
                        |panel: &mut Self, _, _| panel.confirm_quit = false,
                        cx,
                    ))
                    .child(BridgeApp::button(
                        t,
                        "menu-confirm-quit",
                        None,
                        false,
                        "确认退出",
                        BtnVariant::SoftDanger,
                        |panel: &mut Self, _, cx| {
                            panel
                                .bridge
                                .update(cx, |bridge, cx| bridge.request_quit(cx));
                            panel.confirm_quit = false;
                        },
                        cx,
                    )),
            );
        }
        footer(t)
            .child(BridgeApp::button(
                t,
                "menu-quit",
                None,
                false,
                "退出",
                BtnVariant::Text(t.text_muted),
                |panel: &mut Self, _, cx| {
                    if panel.bridge.read(cx).shared.lock().listener_editable() {
                        panel
                            .bridge
                            .update(cx, |bridge, cx| bridge.request_quit(cx));
                    } else {
                        panel.confirm_quit = true;
                    }
                },
                cx,
            ))
            .child(BridgeApp::button(
                t,
                "menu-open-main",
                Some("icons/arrow-right-up-bold.svg"),
                false,
                "打开主窗口",
                BtnVariant::Primary,
                |_, _, cx| cx.defer(super::show_window),
                cx,
            ))
    }

    fn render_quota(&self, t: Theme, key: &str, cx: &mut Context<Self>) -> Div {
        let (details, last_updated) = {
            let bridge = self.bridge.read(cx);
            let account = bridge
                .snapshot
                .accounts
                .iter()
                .find(|account| account.key == key);
            if let Some(account) = account {
                quota_detail(t, bridge, account)
            } else {
                (div().py(px(12.0)).child(label(t, "该账号已移除")), None)
            }
        };
        let close = BridgeApp::button(
            t,
            "menu-close-quota",
            None,
            false,
            "×",
            BtnVariant::Ghost,
            |_, _, cx| {
                cx.defer(|cx| {
                    super::dismiss_quota(cx);
                })
            },
            cx,
        );
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(header(t).child(section_title("账号额度")).child(close))
            .child(scroller(details))
            .child(
                footer(t)
                    .children(last_updated.map(|text| label(t, text)))
                    .child(BridgeApp::button(
                        t,
                        "menu-open-accounts",
                        None,
                        false,
                        "打开账号池",
                        BtnVariant::Text(t.blue_text),
                        |_, _, cx| cx.defer(super::open_account_pool),
                        cx,
                    )),
            )
    }
}

impl Render for MenuPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = motion::begin_frame(window, cx.entity_id());
        let began = *self.opened_at.get_or_insert(now);
        let bridge = self.bridge.read(cx);
        let darkness = bridge.theme_amount(now);
        if (darkness - f32::from(bridge.dark_mode)).abs() > f32::EPSILON {
            motion::request_frame(window);
        }
        let mut t = Theme::interpolated(darkness);
        t.keyboard_focus = self.keyboard_focus;
        let running = bridge.shared.lock().server_running;
        let reduced = motion::is_reduced_motion();
        let elapsed = if reduced {
            1.0
        } else {
            self.closing
                .unwrap_or(now)
                .saturating_duration_since(began)
                .as_secs_f32()
        };
        let overview = matches!(self.content, PanelContent::Overview);
        if overview && self.closing.is_none() && elapsed < 0.20 + STAGE_TIME {
            motion::request_frame(window);
        }
        let content = match &self.content {
            PanelContent::Overview => self.render_overview(t, darkness, elapsed, cx),
            PanelContent::Quota(key) => self.render_quota(t, key, cx),
        };
        let card = div()
            .id("menu-panel-surface")
            .absolute()
            .inset(px(SHADOW_INSET))
            .rounded(px(RADIUS))
            .bg(t.bg_card)
            .border_1()
            .border_color(t.border_strong)
            .shadow_md()
            .overflow_hidden()
            .on_mouse_down_out(cx.listener(|panel, _, _, cx| {
                let quota = matches!(panel.content, PanelContent::Quota(_));
                cx.defer(move |cx| {
                    if quota {
                        super::dismiss_quota(cx);
                    } else {
                        super::dismiss_drawer(cx);
                    }
                });
            }))
            .child(glow(t, running))
            .child(content.relative());
        let exiting = self.closing.is_some();
        let animated = if overview {
            motion::menu_panel_slide("menu-overview-slide", exiting, card).into_any_element()
        } else {
            motion::slide_fade(
                "menu-detail-slide",
                0.0,
                if exiting { 0.0 } else { 1.0 },
                7.0,
                0.0,
                if exiting {
                    SpringConfig::EXIT
                } else {
                    SpringConfig::SMOOTH
                },
                card,
            )
            .into_any_element()
        };

        div()
            .id("menu-panel")
            .relative()
            .size_full()
            .overflow_hidden()
            .font(ui_font())
            .text_size(px(12.0))
            .line_height(px(16.0))
            .text_color(t.text_primary)
            .track_focus(&self.focus)
            .capture_any_mouse_down(cx.listener(|panel, _, _, cx| {
                if std::mem::take(&mut panel.keyboard_focus) {
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(|panel, event: &KeyDownEvent, window, cx| {
                let mods = event.keystroke.modifiers;
                match event.keystroke.key.as_str() {
                    "escape" if panel.confirm_quit => {
                        panel.confirm_quit = false;
                        cx.notify();
                    }
                    "escape" => {
                        cx.defer(|cx| {
                            if !super::dismiss_quota(cx) {
                                super::dismiss_drawer(cx);
                            }
                        });
                    }
                    "tab" if !mods.control && !mods.alt && !mods.platform => {
                        panel.keyboard_focus = true;
                        if mods.shift {
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
            .child(animated)
    }
}

fn row() -> Div {
    div().flex().justify_between().items_center().gap(px(6.0))
}

fn header(t: Theme) -> Div {
    row()
        .h(px(52.0))
        .px(px(16.0))
        .flex_shrink_0()
        .border_b_1()
        .border_color(t.border_subtle)
}

fn footer(t: Theme) -> Div {
    row()
        .h(px(45.0))
        .px(px(12.0))
        .flex_shrink_0()
        .border_t_1()
        .border_color(t.border_subtle)
}

fn scroller(content: Div) -> impl IntoElement {
    div()
        .id("menu-panel-scroll")
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .child(content.px(px(16.0)).pb(px(6.0)))
}

fn section_title(text: &'static str) -> Div {
    div()
        .text_size(px(13.0))
        .line_height(px(18.0))
        .font_weight(FontWeight::SEMIBOLD)
        .child(text)
}

fn label(t: Theme, text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(11.0))
        .line_height(px(15.0))
        .text_color(t.text_muted)
        .child(text.into())
}

fn staged(t: Theme, elapsed: f32, delay: f32, body: Div, border: bool) -> Div {
    let progress = motion::ease_out_cubic((elapsed - delay) / STAGE_TIME);
    div()
        .flex_shrink_0()
        .relative()
        .left(px(24.0 * (1.0 - progress)))
        .opacity(progress)
        .when(border, |el| el.border_b_1().border_color(t.border_subtle))
        .child(body)
}

fn glow(t: Theme, running: bool) -> impl IntoElement {
    let [blue, second] = if running {
        t.gateway_running_glow
    } else {
        t.gateway_idle_glow
    };
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            for (color, angle) in [(blue, 224.0), (second, 46.0)] {
                let gradient = linear_gradient(
                    angle,
                    linear_color_stop(color.opacity(0.43), 0.0),
                    linear_color_stop(color.opacity(0.0), 0.50),
                );
                window.paint_quad(fill(bounds, gradient).corner_radii(px(RADIUS - 1.0)));
            }
        },
    )
    .absolute()
    .inset_0()
}

fn visible_account(snapshot: &PoolSnapshot) -> Option<(&AccountSnapshot, &'static str)> {
    let last = snapshot.last_used_account_key.as_deref().and_then(|key| {
        snapshot
            .accounts
            .iter()
            .find(|account| account.enabled && account.key == key)
    });
    last.filter(|account| account.active_requests > 0)
        .or_else(|| {
            snapshot
                .accounts
                .iter()
                .find(|account| account.active_requests > 0)
        })
        .map(|account| (account, "当前使用"))
        .or_else(|| last.map(|account| (account, "最近使用")))
        .or_else(|| {
            snapshot
                .accounts
                .iter()
                .filter(|account| account.enabled)
                .find(|account| account.is_primary)
                .map(|account| (account, "主账号"))
        })
        .or_else(|| {
            snapshot
                .accounts
                .iter()
                .find(|account| account.enabled)
                .map(|account| (account, ""))
        })
}

fn identity(t: Theme, account: &AccountSnapshot, badge: &str, plan: Option<&str>) -> Div {
    let initial = account
        .label
        .chars()
        .next()
        .unwrap_or('A')
        .to_uppercase()
        .to_string();
    let accent = badge == "当前使用";
    row()
        .child(
            div()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_shrink_0()
                        .size(px(27.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(t.blue_soft)
                        .text_color(t.blue_text)
                        .child(initial),
                )
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .child(
                            div()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .child(account.label.clone()),
                        )
                        .children(
                            plan.filter(|plan| !plan.is_empty())
                                .map(|plan| label(t, plan.to_owned())),
                        ),
                ),
        )
        .when(!badge.is_empty(), |el| {
            el.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .text_size(px(11.0))
                    .text_color(if accent { t.blue_text } else { t.text_muted })
                    .when(accent, |el| {
                        el.child(div().size(px(5.0)).rounded_full().bg(t.series_blue))
                    })
                    .child(badge.to_owned()),
            )
        })
}

fn named_quota<'a>(windows: &'a [QuotaWindow], name: &str) -> Option<&'a QuotaWindow> {
    windows.iter().find(|quota| quota.name == name)
}

fn compact_quota(
    t: Theme,
    title: &'static str,
    quota: Option<&QuotaWindow>,
    loading: bool,
    color: Hsla,
) -> Div {
    let box_ = div()
        .min_w(px(0.0))
        .h(px(61.0))
        .px(px(8.0))
        .py(px(6.0))
        .flex()
        .flex_col()
        .justify_between()
        .child(label(t, title));
    if loading {
        return box_.child(BridgeApp::skeleton_bar(t, title, Some(55.0), 17.0, 4.0));
    }
    let Some(quota) = quota else {
        return box_.child(label(t, "—"));
    };
    let (ratio, credits) = quota_remaining(quota);
    box_.child(
        row()
            .items_end()
            .child(BridgeApp::value_with_unit(
                t,
                motion::rolling_number(
                    title,
                    credits.map_or_else(|| quota_percent(ratio), format_quota_credits),
                    credits.unwrap_or(ratio),
                ),
                credits.map(|_| "credits"),
                22.0,
                t.text_primary,
            ))
            .when(credits.is_some(), |el| {
                el.child(
                    div()
                        .flex_shrink_0()
                        .font(numeric_font())
                        .text_size(px(11.0))
                        .text_color(color)
                        .child(quota_percent(ratio)),
                )
            }),
    )
}

fn request_usage(t: Theme, activity: &RequestActivity) -> Div {
    let (success_rate, ratio) = if activity.requests > 0 {
        let ratio = activity.succeeded as f64 / activity.requests as f64;
        (format!("{:.1}%", ratio * 100.0), ratio)
    } else {
        ("—".into(), 0.0)
    };
    let requests = activity.buckets.map(|bucket| bucket.requests);
    let peak = requests.into_iter().max().unwrap_or(0);
    let section = div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .py(px(10.0))
        .child(
            row()
                .child(section_title("请求用量"))
                .child(label(t, "近 24 小时")),
        )
        .child(
            div().grid().grid_cols(3).gap(px(5.0)).children(
                [
                    (
                        "调用次数",
                        activity.requests.to_string(),
                        activity.requests as f64,
                    ),
                    ("成功率", success_rate, ratio),
                    (
                        "Token 用量",
                        format_tokens(activity.tokens()),
                        activity.tokens() as f64,
                    ),
                ]
                .into_iter()
                .map(|(title, display, value)| {
                    div()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(label(t, title))
                        .child(BridgeApp::value_with_unit(
                            t,
                            motion::rolling_number(title, display, value),
                            None,
                            21.0,
                            t.text_primary,
                        ))
                }),
            ),
        );
    if peak == 0 {
        return section.child(
            div()
                .h(px(63.0))
                .flex()
                .items_center()
                .justify_center()
                .child(label(t, "近 24 小时暂无请求")),
        );
    }
    let plot = div()
        .h(px(57.0))
        .relative()
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    for y in [0.34, 0.67, 1.0] {
                        let origin = point(
                            bounds.left(),
                            bounds.top() + (bounds.size.height - px(1.0)) * y,
                        );
                        window.paint_quad(fill(
                            Bounds::new(origin, size(bounds.size.width, px(1.0))),
                            t.border_subtle,
                        ));
                    }
                },
            )
            .absolute()
            .inset_0(),
        )
        .child(
            motion::spring_values(
                "menu-request-line",
                requests.map(|count| count as f32 / peak as f32),
                SpringConfig::GENTLE,
                move |values| BridgeApp::sparkline(values, t.accent),
            )
            .settle_at_scale(57.0),
        );
    section.child(
        div().flex().flex_col().gap(px(4.0)).child(plot).child(
            row()
                .child(label(t, "24 小时前"))
                .child(label(t, "12 小时前"))
                .child(label(t, "现在")),
        ),
    )
}

fn protocol_mix(t: Theme, a: &RequestActivity) -> Div {
    let section = div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .py(px(10.0))
        .child(section_title("协议分布"));
    if a.requests == 0 {
        return section.child(label(t, "暂无协议数据"));
    }
    let values = [a.messages, a.responses, a.chat];
    let colors = [t.accent, t.series_pink, t.series_blue];
    let shares = values.map(|value| value as f32 / a.requests as f32);
    let bar = motion::spring_values(
        "menu-protocol-shares",
        shares,
        SpringConfig::GENTLE,
        move |shares| {
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    let shares = shares.map(|value| value.max(0.0));
                    let count = shares.iter().filter(|&&share| share > 0.002).count();
                    let total: f32 = shares.iter().copied().filter(|&share| share > 0.002).sum();
                    let gap = px(3.0);
                    let span =
                        (bounds.size.width - gap * count.saturating_sub(1) as f32).max(px(0.0));
                    let mut x = bounds.left();
                    for (share, color) in shares
                        .into_iter()
                        .zip(colors)
                        .filter(|(share, _)| *share > 0.002)
                    {
                        let width = span * (share / total);
                        let segment =
                            Bounds::new(point(x, bounds.top()), size(width, bounds.size.height));
                        window.paint_quad(fill(segment, color).corner_radii(px(2.0)));
                        x += width + gap;
                    }
                },
            )
            .h(px(4.0))
            .w_full()
        },
    )
    .settle_at_scale(MAIN_SIZE.0 as f32);
    section.child(bar).child(
        div().grid().grid_cols(3).gap(px(4.0)).children(
            ["Messages", "Responses", "Chat Completions"]
                .into_iter()
                .zip(values)
                .zip(colors)
                .enumerate()
                .map(|(index, ((name, count), color))| {
                    div()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .child(div().size(px(5.0)).flex_shrink_0().rounded_full().bg(color))
                                .child(label(t, name).min_w(px(0.0)).truncate()),
                        )
                        .child(
                            div()
                                .pl(px(9.0))
                                .font(numeric_font())
                                .text_size(px(13.0))
                                .child(motion::rolling_count(
                                    ("menu-protocol-count", index),
                                    count as u64,
                                )),
                        )
                }),
        ),
    )
}

fn quota_detail(t: Theme, bridge: &BridgeApp, account: &AccountSnapshot) -> (Div, Option<String>) {
    let badge = if account.active_requests > 0 {
        "当前使用"
    } else if bridge.snapshot.last_used_account_key.as_ref() == Some(&account.key) {
        "最近使用"
    } else if account.is_primary {
        "主账号"
    } else {
        ""
    };
    let history = bridge.visible_quota_history.readings(&account.key);
    let windows = bridge.account_quota_windows(account);
    let loading = bridge
        .visible_state
        .quota_loading_keys
        .contains(&account.key);
    let mut details = div().flex().flex_col().child(
        div()
            .py(px(9.0))
            .border_b_1()
            .border_color(t.border_subtle)
            .child(identity(
                t,
                account,
                badge,
                bridge.account_quota_plan(account),
            )),
    );
    if loading {
        details = details.child(
            div()
                .py(px(12.0))
                .flex()
                .flex_col()
                .gap(px(9.0))
                .child(BridgeApp::quota_balance_skeleton(
                    t,
                    "menu-detail-month-loading",
                ))
                .child(BridgeApp::quota_balance_skeleton(
                    t,
                    "menu-detail-session-loading",
                )),
        );
    } else if windows.is_empty() {
        details = details.child(
            div().py(px(12.0)).child(label(
                t,
                account
                    .quota_error
                    .as_deref()
                    .unwrap_or("暂无额度数据")
                    .to_owned(),
            )),
        );
    } else {
        for quota in windows {
            let (name, accent, color) = match quota.name.as_str() {
                "Monthly" => ("月度额度", t.accent, t.accent_text),
                "Session" => ("会话额度", t.series_blue, t.blue_text),
                "Monthly overage" => ("月度额外", t.series_pink, t.text_secondary),
                other => (other, t.accent, t.accent_text),
            };
            let (ratio, credits) = quota_remaining(quota);
            let value = credits.map_or_else(|| quota_percent(ratio), format_quota_credits);
            let balance = BridgeApp::value_with_unit(
                t,
                motion::rolling_number(
                    SharedString::from(format!("menu-detail-{}", quota.name)),
                    value,
                    credits.unwrap_or(ratio),
                ),
                None,
                23.0,
                t.text_primary,
            );
            details = details.child(
                div()
                    .py(px(7.0))
                    .flex()
                    .flex_col()
                    .gap(px(5.0))
                    .border_b_1()
                    .border_color(t.border_subtle)
                    .child(
                        row()
                            .child(label(t, name.to_owned()))
                            .child(label(t, quota_percent(ratio)).text_color(color)),
                    )
                    .child(
                        row()
                            .child(
                                div()
                                    .flex()
                                    .items_end()
                                    .gap(px(4.0))
                                    .child(balance)
                                    .children(credits.map(|_| {
                                        label(
                                            t,
                                            format!(
                                                "/ {} credits",
                                                format_quota_credits(quota.limit)
                                            ),
                                        )
                                    })),
                            )
                            .child(quota_dots(t, ratio, accent)),
                    ),
            );
        }
        if let Some(quota) = named_quota(windows, "Monthly") {
            let points = QuotaTrendPoint::history(quota, history, account.quota_updated_at);
            let start = points.first().and_then(|point| point.at);
            let end = points.last().and_then(|point| point.at);
            details = details.child(
                div()
                    .pt(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(5.0))
                    .child(
                        row()
                            .child(section_title("月度额度走势"))
                            .child(label(t, format!("最近 {} 次刷新", points.len()))),
                    )
                    .child(BridgeApp::quota_sparkline_at_height(
                        t, t.accent, points, 38.0,
                    ))
                    .when_some(
                        start.zip(end).filter(|(start, end)| start != end),
                        |el, (start, end)| {
                            el.child(
                                row()
                                    .child(label(
                                        t,
                                        start
                                            .with_timezone(&Local)
                                            .format("%m-%d %H:%M")
                                            .to_string(),
                                    ))
                                    .child(label(
                                        t,
                                        end.with_timezone(&Local).format("%m-%d %H:%M").to_string(),
                                    )),
                            )
                        },
                    ),
            );
        }
    }
    let updated = history
        .last()
        .map(|entry| entry.at)
        .or(account.quota_updated_at)
        .map(|at| format!("最近刷新 {}", at.with_timezone(&Local).format("%H:%M")));
    (details, updated)
}
