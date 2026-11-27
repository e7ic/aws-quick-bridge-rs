use super::motion::{self, SpringConfig};
use super::{
    BridgeApp,
    components::{BtnVariant, Theme},
};
use crate::pool::AccountSnapshot;
use gpui::{
    AnyElement, Context, Corner, FocusHandle, Window, anchored, deferred, div, prelude::*, px,
    relative,
};
use std::time::Instant;

pub(super) struct AccountMenu {
    pub(super) key: String,
    focus: FocusHandle,
    trigger_focus: Option<FocusHandle>,
    pub(super) closing: Option<Instant>,
    confirming_remove: bool,
}

impl AccountMenu {
    pub(super) fn is_open(&self) -> bool {
        self.closing.is_none()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AccountAction {
    RefreshQuota,
    SetPrimary,
    Remove,
}

impl BridgeApp {
    fn dismiss_account_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(menu) = self.account_menu.as_mut()
            && menu.closing.is_none()
        {
            menu.closing = Some(Instant::now());
            if let Some(trigger) = menu.trigger_focus.take() {
                window.focus(&trigger);
            } else {
                window.blur();
            }
            cx.notify();
        }
    }

    pub(super) fn account_menu_backdrop(cx: &mut Context<Self>) -> AnyElement {
        deferred(
            div()
                .id("account-menu-backdrop")
                .absolute()
                .inset_0()
                .occlude()
                .on_any_mouse_down(cx.listener(|this, _, window, cx| {
                    this.dismiss_account_menu(window, cx);
                    cx.stop_propagation();
                }))
                .on_scroll_wheel(cx.listener(|this, _, window, cx| {
                    this.dismiss_account_menu(window, cx);
                    cx.stop_propagation();
                })),
        )
        .with_priority(1)
        .into_any_element()
    }

    pub(super) fn account_menu_trigger(
        &self,
        t: Theme,
        account: &AccountSnapshot,
        loading: bool,
        index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = account.key.clone();
        let menu = self.account_menu.as_ref().filter(|menu| menu.key == key);
        let is_open = menu.is_some_and(AccountMenu::is_open);
        let trigger =
            div()
                .id(("account-menu-trigger", index))
                .relative()
                .size(px(28.0))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(2.5))
                .rounded(px(6.0))
                .cursor_pointer()
                .focusable()
                .tab_stop(true)
                .focus(move |style| style.bg(t.bg_hover))
                .hover(move |style| style.bg(t.bg_hover))
                .when(!is_open, |el| el.tooltip(Self::text_tooltip(t, "账号操作")))
                .children((0..3).map(|_| div().size(px(2.5)).rounded_full().bg(t.text_secondary)))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if this
                        .account_menu
                        .as_ref()
                        .is_some_and(|menu| menu.key == key && menu.is_open())
                    {
                        this.dismiss_account_menu(window, cx);
                    } else {
                        let menu = AccountMenu {
                            key: key.clone(),
                            focus: cx.focus_handle(),
                            trigger_focus: window.focused(cx),
                            closing: None,
                            confirming_remove: false,
                        };
                        window.focus(&menu.focus);
                        this.account_menu = Some(menu);
                        cx.notify();
                    }
                }))
                .when_some(menu, |el, menu| {
                    el.when(is_open, |el| el.bg(t.bg_hover)).child(
                        div()
                            .absolute()
                            .top(relative(1.0))
                            .right_0()
                            .mt(px(4.0))
                            .size(px(0.0))
                            .child(
                                deferred(anchored().anchor(Corner::TopRight).child(
                                    Self::account_menu_panel(t, menu, account, loading, cx),
                                ))
                                .with_priority(2),
                            ),
                    )
                });
        motion::layout(trigger).into_any_element()
    }

    fn account_menu_panel(
        t: Theme,
        menu: &AccountMenu,
        account: &AccountSnapshot,
        loading: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_open = menu.is_open();
        let items = [
            (
                AccountAction::RefreshQuota,
                "icons/refresh-bold.svg",
                if loading {
                    "额度刷新中"
                } else {
                    "刷新额度"
                },
                !loading,
            ),
            (
                AccountAction::SetPrimary,
                "icons/star-bold.svg",
                "设为主账号",
                true,
            ),
            (
                AccountAction::Remove,
                "icons/trash-bin-trash-bold.svg",
                "移除",
                true,
            ),
        ];
        let panel = Self::card(t)
            .id("account-menu")
            .w(px(if menu.confirming_remove { 224.0 } else { 156.0 }))
            .p(px(4.0))
            .rounded(px(9.0))
            .shadow_md()
            .flex()
            .flex_col()
            .when(is_open, |panel| {
                panel
                    .track_focus(&menu.focus)
                    .tab_group()
                    .tab_stop(false)
                    .occlude()
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                        match event.keystroke.key.as_str() {
                            "escape" => {
                                if let Some(menu) = this
                                    .account_menu
                                    .as_mut()
                                    .filter(|menu| menu.confirming_remove)
                                {
                                    menu.confirming_remove = false;
                                    window.focus(&menu.focus);
                                    cx.notify();
                                } else {
                                    this.dismiss_account_menu(window, cx);
                                }
                            }
                            "up" => window.focus_prev(),
                            "tab" if event.keystroke.modifiers.shift => window.focus_prev(),
                            "down" | "tab" => window.focus_next(),
                            _ => return,
                        }
                        cx.stop_propagation();
                    }))
            })
            .when(!menu.confirming_remove, |panel| {
                panel.children(
                    items
                        .into_iter()
                        .enumerate()
                        .filter(|(_, (action, _, _, _))| {
                            *action != AccountAction::SetPrimary
                                || (account.enabled && !account.is_primary)
                        })
                        .map(|(index, (action, icon, label, enabled))| {
                            let key = account.key.clone();
                            let danger = action == AccountAction::Remove;
                            let color = if !enabled {
                                t.text_muted
                            } else if danger {
                                t.red_text
                            } else {
                                t.text_primary
                            };
                            let background = if danger { t.red_soft } else { t.bg_hover };
                            div()
                                .when(danger, |el| {
                                    el.child(div().my(px(3.0)).h(px(1.0)).bg(t.border_subtle))
                                })
                                .child(
                                    div()
                                        .id(("account-menu-action", index))
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .h(px(30.0))
                                        .px(px(8.0))
                                        .rounded(px(5.0))
                                        .text_size(px(12.5))
                                        .line_height(px(16.0))
                                        .text_color(color)
                                        .when(
                                            is_open
                                                && loading
                                                && action == AccountAction::RefreshQuota,
                                            |el| {
                                                el.relative().child(Self::shimmer_overlay(
                                                    "refresh-quota-menu-loading",
                                                    color.opacity(0.10),
                                                    5.0,
                                                ))
                                            },
                                        )
                                        .child(Self::icon(icon, 14.0, color))
                                        // MiSans 字形的可见中线略高于行框中线。
                                        .child(div().relative().top(px(1.0)).child(label))
                                        .when(is_open && enabled, |el| {
                                            el.tab_index(0)
                                                .cursor_pointer()
                                                .hover(move |style| style.bg(background))
                                                .focus(move |style| style.bg(background))
                                                .active(|style| style.opacity(0.76))
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        if this.account_menu.as_ref().is_none_or(
                                                            |menu| {
                                                                menu.key != key || !menu.is_open()
                                                            },
                                                        ) {
                                                            return;
                                                        }
                                                        if action == AccountAction::Remove {
                                                            if let Some(menu) =
                                                                this.account_menu.as_mut()
                                                            {
                                                                menu.confirming_remove = true;
                                                                window.focus(&menu.focus);
                                                                cx.notify();
                                                            }
                                                            cx.stop_propagation();
                                                            return;
                                                        }
                                                        this.dismiss_account_menu(window, cx);
                                                        let result = match action {
                                                            AccountAction::RefreshQuota => {
                                                                this.refresh_one_quota_ui(
                                                                    key.clone(),
                                                                );
                                                                Ok(())
                                                            }
                                                            AccountAction::SetPrimary => this
                                                                .pool
                                                                .set_primary(&key)
                                                                .inspect(|_| {
                                                                    this.set_toast(
                                                                        "已设为主账号",
                                                                        false,
                                                                    );
                                                                }),
                                                            AccountAction::Remove => unreachable!(),
                                                        };
                                                        if let Err(error) = result {
                                                            this.set_toast(error.to_string(), true);
                                                        }
                                                        cx.stop_propagation();
                                                    },
                                                ))
                                        }),
                                )
                        }),
                )
            })
            .when(menu.confirming_remove, |panel| {
                let key = account.key.clone();
                panel
                    .p(px(10.0))
                    .gap(px(7.0))
                    .child(div().text_size(px(13.0)).child("移除此账号？"))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(t.text_muted)
                            .child("再次使用时需要重新授权。"),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(6.0))
                            .child(Self::button(
                                t,
                                "account-remove-cancel",
                                None,
                                false,
                                "取消",
                                BtnVariant::Secondary,
                                |this, window, cx| {
                                    if let Some(menu) = this.account_menu.as_mut() {
                                        menu.confirming_remove = false;
                                        window.focus(&menu.focus);
                                        cx.notify();
                                    }
                                },
                                cx,
                            ))
                            .child(Self::button(
                                t,
                                "account-remove-confirm",
                                None,
                                false,
                                "确认移除",
                                BtnVariant::SoftDanger,
                                move |this, window, cx| {
                                    if !this.account_menu.as_ref().is_some_and(|menu| {
                                        menu.key == key && menu.is_open() && menu.confirming_remove
                                    }) {
                                        return;
                                    }
                                    match this.pool.delete_account(&key) {
                                        Ok(()) => {
                                            this.dismiss_account_menu(window, cx);
                                            this.set_toast("已移除账号", false);
                                        }
                                        Err(error) => this.set_toast(error.to_string(), true),
                                    }
                                },
                                cx,
                            )),
                    )
            });
        motion::slide_fade(
            gpui::SharedString::from(format!("account-menu-anim-{}", menu.key)),
            0.0,
            if is_open { 1.0 } else { 0.0 },
            0.0,
            -6.0,
            if is_open {
                SpringConfig::SMOOTH
            } else {
                SpringConfig::EXIT
            },
            motion::layout(panel).anchored(),
        )
        .into_any_element()
    }
}
