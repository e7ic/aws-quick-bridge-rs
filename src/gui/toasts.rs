//! 右上角通知队列，以及收拢和展开时的叠放布局。

use super::components::Theme;
use super::motion::{self, MultiSpringState, SpringConfig};
use super::{BridgeApp, ui_font};
use gpui::{
    AnyElement, Context, MouseExitEvent, Pixels, SharedString, TextRun, Window, canvas, div,
    prelude::*, px,
};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::time::{Duration, Instant};

const TOAST_LIFETIME: Duration = Duration::from_secs(3);
const MAX_TOASTS: usize = 12;
const STACK_BACK_LAYERS: usize = 2;
const STACK_OFFSET: f32 = 7.0;
const STACK_INSET: f32 = 5.0;
const REFLOW_STAGGER: Duration = Duration::from_millis(42);
const REFLOW_SPRING: SpringConfig = SpringConfig {
    stiffness: 560.0,
    damping: 32.0,
};
const EXPANDED_GAP: Pixels = px(9.0);
const SHADOW_PADDING: Pixels = px(12.0);
const HORIZONTAL_PADDING: Pixels = px(10.0);
const CONTENT_GAP: Pixels = px(7.0);
const ICON_SIZE: Pixels = px(16.0);
const CLOSE_SIZE: Pixels = px(24.0);
const FONT_SIZE: Pixels = px(12.5);
const LINE_HEIGHT: Pixels = px(17.0);
const DETAIL_FONT_SIZE: Pixels = px(11.0);
const DETAIL_LINE_HEIGHT: Pixels = px(15.0);

pub(super) fn gateway_start_failure(port: u16, error: &anyhow::Error) -> (String, String) {
    if error.downcast_ref::<io::Error>().map(io::Error::kind) == Some(io::ErrorKind::AddrInUse) {
        return (
            format!("端口 {port} 已占用"),
            "在设置中更换端口后重试".into(),
        );
    }
    ("网关启动失败".into(), error.to_string())
}

#[derive(Clone)]
struct Toast {
    id: u64,
    message: SharedString,
    detail: Option<SharedString>,
    is_error: bool,
    shown_at: Instant,
    closing: Option<Instant>,
}

#[derive(Clone, Default)]
pub(super) struct ToastQueue {
    items: VecDeque<Toast>,
    next_id: u64,
    hovered_at: Option<Instant>,
}

impl ToastQueue {
    pub(super) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(super) fn push(
        &mut self,
        message: String,
        detail: Option<String>,
        is_error: bool,
        now: Instant,
    ) {
        self.next_id = self.next_id.wrapping_add(1);
        self.items.push_front(Toast {
            id: self.next_id,
            message: message.into(),
            detail: detail.map(Into::into),
            is_error,
            shown_at: now,
            closing: None,
        });
        self.items.truncate(MAX_TOASTS);
    }

    fn set_hovered(&mut self, hovered: bool, now: Instant) -> bool {
        match (hovered && !self.is_empty(), self.hovered_at) {
            (true, None) => self.hovered_at = Some(now),
            (false, Some(at)) => {
                self.hovered_at = None;
                for toast in &mut self.items {
                    // 悬停期间到达的通知只暂停它实际存在的时间。
                    toast.shown_at += now.saturating_duration_since(at.max(toast.shown_at));
                }
            }
            _ => return false,
        }
        true
    }

    fn dismiss(&mut self, id: u64, now: Instant) -> bool {
        let Some(toast) = self.items.iter_mut().find(|toast| toast.id == id) else {
            return false;
        };
        if toast.closing.is_some() {
            return false;
        }
        toast.closing = Some(now);
        true
    }

    pub(super) fn advance(&mut self, now: Instant) -> bool {
        let count = self.items.len();
        self.items.retain(|toast| {
            toast
                .closing
                .is_none_or(|at| now.saturating_duration_since(at) < motion::EXIT_DURATION)
        });
        let mut changed = self.items.len() != count;
        if self.is_empty() {
            self.hovered_at = None;
        } else if self.hovered_at.is_none() {
            for toast in &mut self.items {
                if toast.closing.is_none()
                    && now.saturating_duration_since(toast.shown_at) >= TOAST_LIFETIME
                {
                    toast.closing = Some(now);
                    changed = true;
                }
            }
        }
        changed
    }
}

struct ToastLayer {
    id: u64,
    closing: bool,
    height: Pixels,
    position: [f32; 3],
    content: AnyElement,
}

#[derive(Default)]
struct ToastPositions {
    slots: HashMap<u64, ToastSlot>,
}

struct ToastSlot {
    spring: MultiSpringState<3>,
    destination: [f32; 3],
    move_at: Instant,
}

impl ToastPositions {
    fn advance(
        &mut self,
        items: &[(&Toast, Pixels)],
        now: Instant,
        pixel_tolerance: f32,
        reduced_motion: bool,
    ) -> (Vec<[f32; 3]>, bool) {
        self.slots
            .retain(|id, _| items.iter().any(|(toast, _)| toast.id == *id));
        let front_height = items
            .iter()
            .find(|(toast, _)| toast.closing.is_none())
            .or_else(|| items.first())
            .map_or(0.0, |(_, height)| f32::from(*height));
        let mut expanded_y = 0.0;
        let mut stacked_y = 0.0;
        let mut next_move_at = now;
        let mut needs_frame = false;

        let positions = items
            .iter()
            .map(|&(toast, height)| {
                let destination = [expanded_y, stacked_y, front_height];
                let slot = self.slots.entry(toast.id).or_insert_with(|| ToastSlot {
                    spring: MultiSpringState::new(destination, destination, now),
                    destination,
                    move_at: now,
                });
                let closing = toast.closing.is_some();
                if !closing {
                    let [_, previous_stacked_y, _] = slot.destination;
                    if stacked_y < previous_stacked_y {
                        // 只延后需要补位的通知，每条保留自己的位移和速度。
                        slot.move_at = next_move_at;
                        next_move_at += REFLOW_STAGGER;
                    } else if stacked_y > previous_stacked_y {
                        slot.move_at = now;
                    }
                    slot.destination = destination;
                    expanded_y += f32::from(height + EXPANDED_GAP);
                    stacked_y += STACK_OFFSET;
                }

                let waiting = !closing && !reduced_motion && now < slot.move_at;
                let target = if closing || waiting {
                    slot.spring.targets()
                } else {
                    slot.destination
                };
                let (position, moving) = slot.spring.advance_with_policy(
                    target,
                    now,
                    REFLOW_SPRING,
                    Some(pixel_tolerance),
                    reduced_motion,
                );
                needs_frame |= waiting || moving;
                position
            })
            .collect();
        (positions, needs_frame)
    }
}

impl BridgeApp {
    fn update_toasts(
        &mut self,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut ToastQueue) -> bool,
    ) {
        let mut shared = self.shared.lock();
        if update(&mut shared.toasts) {
            self.visible_state.toasts = shared.toasts.clone();
            drop(shared);
            cx.notify();
        }
    }

    fn set_toasts_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if hovered != self.visible_state.toasts.hovered_at.is_some() {
            self.update_toasts(cx, |toasts| toasts.set_hovered(hovered, Instant::now()));
        }
    }

    fn toast_content(
        t: Theme,
        toast: &Toast,
        width: Pixels,
        text_width: Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = toast.id;
        let closing = toast.closing.is_some();
        let (icon, color) = if toast.is_error {
            ("icons/danger-circle-bold.svg", t.red_text)
        } else {
            ("icons/check-circle-bold.svg", t.green_text)
        };
        let headline = div()
            .flex()
            .items_center()
            .gap(CONTENT_GAP)
            .child(Self::icon(icon, f32::from(ICON_SIZE), color))
            .child(
                div()
                    .w(text_width)
                    .flex_shrink_0()
                    .text_size(FONT_SIZE)
                    .line_height(LINE_HEIGHT)
                    .whitespace_normal()
                    .text_color(t.text_primary)
                    .child(toast.message.clone()),
            )
            .child(
                div()
                    .id(("toast-dismiss", id))
                    .size(CLOSE_SIZE)
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .text_size(px(17.0))
                    .line_height(px(20.0))
                    .text_color(t.text_muted)
                    .child("×")
                    .when(!closing, |button| {
                        button
                            .cursor_pointer()
                            .focusable()
                            .tab_stop(true)
                            .hover(move |style| style.bg(t.bg_hover).text_color(t.text_primary))
                            .focus(move |style| style.bg(t.bg_hover))
                            .tooltip(Self::text_tooltip(t, "关闭通知"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.update_toasts(cx, |toasts| toasts.dismiss(id, Instant::now()));
                                cx.stop_propagation();
                            }))
                    }),
            );
        div()
            .w(width - px(2.0))
            .h_full()
            .flex()
            .flex_col()
            .justify_center()
            .px(HORIZONTAL_PADDING)
            .child(headline)
            .when_some(toast.detail.clone(), |body, detail| {
                body.child(
                    div()
                        .ml(ICON_SIZE + CONTENT_GAP)
                        .w(text_width)
                        .flex_shrink_0()
                        .text_size(DETAIL_FONT_SIZE)
                        .line_height(DETAIL_LINE_HEIGHT)
                        .whitespace_normal()
                        .text_color(t.text_muted)
                        .child(detail),
                )
            })
            .into_any_element()
    }

    pub(super) fn render_toasts(
        t: Theme,
        toasts: &ToastQueue,
        top: Pixels,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let width = px(if compact { 200.0 } else { 208.0 });
        let right = px(if compact { 16.0 } else { 26.0 });
        let text_width =
            width - px(2.0) - (HORIZONTAL_PADDING + CONTENT_GAP) * 2.0 - ICON_SIZE - CLOSE_SIZE;
        let available_height = (window.viewport_size().height - top).max(px(0.0));
        let font = ui_font();
        let measure_height = |text: &SharedString, font_size, line_height, color| {
            let runs = [TextRun {
                len: text.len(),
                font: font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            window
                .text_system()
                .shape_text(text.clone(), font_size, &runs, Some(text_width), None)
                .map(|lines| {
                    lines.iter().fold(px(0.0), |height, line| {
                        height + line.size(line_height).height
                    })
                })
                .unwrap_or(line_height)
        };
        let items: Vec<_> = toasts
            .items
            .iter()
            .map(|toast| {
                let text_height =
                    measure_height(&toast.message, FONT_SIZE, LINE_HEIGHT, t.text_primary);
                let height = if let Some(detail) = &toast.detail {
                    text_height.max(CLOSE_SIZE)
                        + measure_height(detail, DETAIL_FONT_SIZE, DETAIL_LINE_HEIGHT, t.text_muted)
                        + px(18.0)
                } else {
                    (text_height + px(22.0)).max(px(49.0))
                };
                (toast, height)
            })
            .collect();
        let (positions, moving) = window.with_global_id("toast-positions".into(), |id, window| {
            let pixel_tolerance = 0.5 / window.scale_factor();
            window.with_element_state(id, |state: Option<ToastPositions>, _| {
                let mut state = state.unwrap_or_default();
                let frame = state.advance(
                    &items,
                    Instant::now(),
                    pixel_tolerance,
                    motion::is_reduced_motion(),
                );
                (frame, state)
            })
        });
        if moving {
            window.request_animation_frame();
        }
        let layers: Vec<_> = items
            .into_iter()
            .zip(positions)
            .map(|((toast, height), position)| ToastLayer {
                id: toast.id,
                closing: toast.closing.is_some(),
                height,
                position,
                content: Self::toast_content(t, toast, width, text_width, cx),
            })
            .collect();
        let hovered = toasts.hovered_at.is_some();
        let expanded = hovered && layers.len() > 1;

        div()
            .id("toast-stack")
            .absolute()
            .top(top - SHADOW_PADDING)
            .right(right - SHADOW_PADDING)
            .w(width + SHADOW_PADDING * 2.0)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.set_toasts_hovered(*hovered, cx);
            }))
            .on_mouse_move(cx.listener(|this, _, _, cx| this.set_toasts_hovered(true, cx)))
            .when(hovered, |stack| {
                let exit = cx.listener(|this, _: &MouseExitEvent, _, cx| {
                    this.set_toasts_hovered(false, cx);
                });
                stack.child(
                    canvas(
                        |_, _, _| (),
                        move |_, (), window, _| {
                            window.on_mouse_event(
                                move |event: &MouseExitEvent, phase, window, cx| {
                                    if phase.bubble() {
                                        exit(event, window, cx);
                                    }
                                },
                            );
                        },
                    )
                    .absolute()
                    .size(px(0.0)),
                )
            })
            .child(motion::spring_value(
                "toast-stack-expansion",
                if expanded { 1.0 } else { 0.0 },
                SpringConfig::SNAPPY,
                move |progress| {
                    let progress = progress.clamp(0.0, 1.0);
                    let stacked = 1.0 - progress;
                    let mut content_height = px(0.0);
                    let cards: Vec<_> = layers
                        .into_iter()
                        .rev()
                        .map(|layer| {
                            let [expanded_y, stacked_y, front_height] = layer.position;
                            let rank = stacked_y / STACK_OFFSET;
                            let depth = rank.clamp(0.0, STACK_BACK_LAYERS as f32);
                            let inset = SHADOW_PADDING + px(STACK_INSET * depth * stacked);
                            let y = px(STACK_OFFSET * depth * stacked + expanded_y * progress)
                                .max(-SHADOW_PADDING / 2.0);
                            let card_height = px(front_height) * stacked + layer.height * progress;
                            let card_alpha =
                                1.0 - (rank - STACK_BACK_LAYERS as f32).clamp(0.0, 1.0) * stacked;
                            let body_alpha = 1.0 - rank.clamp(0.0, 1.0) * stacked;
                            content_height = content_height.max(y + card_height);
                            let card = div()
                                .absolute()
                                .top(SHADOW_PADDING + y)
                                .left(inset)
                                .right(inset)
                                .h(card_height)
                                .child(
                                    Self::card(t)
                                        .p(px(0.0))
                                        .size_full()
                                        .overflow_hidden()
                                        .shadow_sm()
                                        .opacity(card_alpha)
                                        .when(body_alpha > 0.0, |card| {
                                            card.child(
                                                div()
                                                    .size_full()
                                                    .opacity(body_alpha)
                                                    .child(layer.content),
                                            )
                                        }),
                                );
                            motion::slide_fade_from_right(("toast", layer.id), layer.closing, card)
                        })
                        .collect();
                    let content_height = content_height + SHADOW_PADDING * 2.0;
                    let height = content_height.min(available_height);
                    div()
                        .id("toast-scroll")
                        .h(height)
                        .w_full()
                        .when(content_height > height, |scroll| scroll.overflow_y_scroll())
                        .child(div().relative().h(content_height).w_full().children(cards))
                },
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occupied_gateway_port_has_a_localized_title_and_recovery_step() {
        let error = anyhow::Error::new(io::Error::new(io::ErrorKind::AddrInUse, "bind error"))
            .context("listener failed");
        let (title, detail) = gateway_start_failure(18765, &error);
        assert_eq!(title, "端口 18765 已占用");
        assert_eq!(detail, "在设置中更换端口后重试");

        let other = anyhow::anyhow!("listener unavailable");
        let (title, detail) = gateway_start_failure(18765, &other);
        assert_eq!(title, "网关启动失败");
        assert_eq!(detail, "listener unavailable");
    }

    #[test]
    fn queued_toasts_keep_the_latest_first_and_bound_bursts() {
        let now = Instant::now();
        let mut toasts = ToastQueue::default();
        for index in 0..=MAX_TOASTS {
            toasts.push(index.to_string(), None, false, now);
        }
        assert_eq!(toasts.items.len(), MAX_TOASTS);
        assert_eq!(toasts.items[0].message.as_ref(), MAX_TOASTS.to_string());
        assert_eq!(toasts.items[MAX_TOASTS - 1].message.as_ref(), "1");

        let latest = toasts.items[0].id;
        assert!(toasts.dismiss(latest, now));
        assert!(!toasts.advance(now));
        assert!(toasts.advance(now + motion::EXIT_DURATION));
        assert_eq!(toasts.items.len(), MAX_TOASTS - 1);
        assert_ne!(toasts.items[0].id, latest);
    }

    #[test]
    fn closing_a_toast_reflows_its_successors_with_individual_delays_and_bounces() {
        let now = Instant::now();
        let mut toasts = ToastQueue::default();
        for label in ["last", "next", "first", "dismissed", "above"] {
            let detail = (label == "dismissed").then(|| "More text".to_string());
            toasts.push(label.into(), detail, false, now);
        }
        let dismissed = toasts.items[1].id;
        let mut positions = ToastPositions::default();
        let mut draw = |queue: &ToastQueue, at: Instant, reduced_motion| {
            let items: Vec<_> = queue
                .items
                .iter()
                .map(|toast| (toast, px(if toast.detail.is_some() { 56.0 } else { 49.0 })))
                .collect();
            positions.advance(&items, at, 0.25, reduced_motion)
        };
        let (initial, active) = draw(&toasts, now, false);
        assert!(!active);

        let closed_at = now + Duration::from_secs(1);
        assert!(toasts.dismiss(dismissed, closed_at));
        assert_eq!(draw(&toasts, closed_at, false), (initial.clone(), true));
        let final_y = [58.0, 116.0, 174.0];
        let mut started_at = [None; 3];
        let mut overshot = [false; 3];
        let mut settled = false;
        for frame in 1..=36 {
            let at = closed_at + Duration::from_secs_f32(frame as f32 / 60.0);
            toasts.advance(at);
            let (layout, moving) = draw(&toasts, at, false);
            assert_eq!(layout[0], initial[0]);
            if layout.len() == initial.len() {
                assert_eq!(layout[1], initial[1]);
            }
            for (index, position) in layout[layout.len() - 3..].iter().enumerate() {
                if position[0] < initial[index + 2][0] {
                    started_at[index].get_or_insert(frame);
                }
                overshot[index] |= position[0] < final_y[index] - 1.0;
                if !moving {
                    assert_eq!(position[0], final_y[index]);
                }
            }
            if !moving {
                settled = true;
                break;
            }
        }
        let [first, second, third] = started_at.map(Option::unwrap);
        assert_eq!(first, 1);
        assert!(second >= first + 2 && third >= second + 2);
        assert_eq!(overshot, [true; 3]);
        assert!(settled);

        let closed_again_at = closed_at + Duration::from_secs(1);
        assert!(toasts.dismiss(toasts.items[0].id, closed_again_at));
        let (layout, moving) = draw(&toasts, closed_again_at, true);
        assert_eq!(
            layout[1..]
                .iter()
                .map(|position| position[0])
                .collect::<Vec<_>>(),
            [0.0, 58.0, 116.0]
        );
        assert!(!moving);
    }

    #[test]
    fn hovering_preserves_each_toasts_remaining_lifetime() {
        let now = Instant::now();
        let mut toasts = ToastQueue::default();
        toasts.push("first".into(), None, false, now);
        assert!(toasts.set_hovered(true, now + Duration::from_secs(2)));
        toasts.push("second".into(), None, true, now + Duration::from_secs(4));
        assert!(!toasts.advance(now + Duration::from_secs(8)));
        assert!(toasts.set_hovered(false, now + Duration::from_secs(8)));

        assert!(!toasts.advance(now + Duration::from_millis(8999)));
        let first_expiry = now + Duration::from_secs(9);
        assert!(toasts.advance(first_expiry));
        assert!(toasts.items[1].closing.is_some());
        assert!(toasts.items[0].closing.is_none());
        assert!(toasts.advance(first_expiry + motion::EXIT_DURATION));
        assert_eq!(toasts.items.len(), 1);
        assert!(toasts.advance(now + Duration::from_secs(11)));
        assert!(toasts.items[0].closing.is_some());
    }
}
