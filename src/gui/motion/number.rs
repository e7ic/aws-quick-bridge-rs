use super::{frame_time, is_reduced_motion, request_frame};
use gpui::{
    AnyElement, App, Bounds, Div, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, SharedString, Window, div, prelude::*, px,
};
use std::time::{Duration, Instant};

const ROLL_DURATION: Duration = Duration::from_millis(190);

#[derive(Clone)]
struct Number {
    text: SharedString,
    value: Option<f64>,
}

struct Roll {
    previous: Number,
    started: Instant,
    start_remaining: f32,
    direction: f32,
}

struct RollFrame {
    previous: SharedString,
    height: Pixels,
    outgoing: Pixels,
    incoming: Pixels,
    remaining: f32,
}

struct NumberState {
    shown: Number,
    roll: Option<Roll>,
}

impl NumberState {
    fn update(
        &mut self,
        next: Number,
        now: Instant,
        height: Pixels,
        tolerance: Pixels,
        reduced_motion: bool,
        paused: bool,
    ) -> Option<RollFrame> {
        if paused {
            self.roll = None;
            return None;
        }
        if reduced_motion || next.value.is_none() {
            self.shown = next;
            self.roll = None;
            return None;
        }
        if let Some(roll) = &mut self.roll {
            let remaining = roll.remaining(now);
            if height * remaining > tolerance {
                if next.text == roll.previous.text {
                    // Swapping layers and reversing the offset keeps both glyphs in place.
                    roll.previous = std::mem::replace(&mut self.shown, next);
                    roll.started = now;
                    roll.start_remaining = 1.0 - remaining;
                    roll.direction = -roll.direction;
                    if height * roll.start_remaining <= tolerance {
                        self.roll = None;
                        return None;
                    }
                    return Some(roll.frame(height, roll.start_remaining));
                }
                if height * (1.0 - remaining) <= tolerance
                    && let Some((previous, target)) = roll.previous.value.zip(next.value)
                    && (target - previous) * f64::from(roll.direction) > 0.0
                {
                    self.shown = next;
                }
                // A third value waits only while both current layers are visible.
                return Some(roll.frame(height, remaining));
            }
            self.roll = None;
        }

        let previous = std::mem::replace(&mut self.shown, next);
        if previous.text == self.shown.text {
            return None;
        }
        let (previous_value, value) = previous.value.zip(self.shown.value)?;
        if previous_value == value {
            return None;
        }
        let roll = Roll {
            previous,
            started: now,
            start_remaining: 1.0,
            direction: if value > previous_value { 1.0 } else { -1.0 },
        };
        let frame = roll.frame(height, 1.0);
        self.roll = Some(roll);
        Some(frame)
    }
}

impl Roll {
    fn remaining(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.started);
        let progress = (elapsed.as_secs_f32() / ROLL_DURATION.as_secs_f32()).min(1.0);
        self.start_remaining * (1.0 - progress).powi(3)
    }

    fn frame(&self, height: Pixels, remaining: f32) -> RollFrame {
        let travel = height * self.direction;
        RollFrame {
            previous: self.previous.text.clone(),
            height,
            outgoing: travel * (remaining - 1.0),
            incoming: travel * remaining,
            remaining,
        }
    }
}

pub(in crate::gui) struct RollingNumber {
    id: ElementId,
    number: Number,
    paused: bool,
}

impl RollingNumber {
    pub(in crate::gui) fn pause(mut self, paused: bool) -> Self {
        self.paused = paused;
        self
    }
}

pub(in crate::gui) fn rolling_number(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    value: impl Into<Option<f64>>,
) -> RollingNumber {
    RollingNumber {
        id: id.into(),
        number: Number {
            text: text.into(),
            value: value.into().filter(|value| value.is_finite()),
        },
        paused: false,
    }
}

pub(in crate::gui) fn rolling_count(id: impl Into<ElementId>, count: u64) -> RollingNumber {
    rolling_number(id, count.to_string(), count as f64)
}

fn same_digit_positions(previous: &str, next: &str) -> bool {
    previous.is_ascii()
        && next.is_ascii()
        && previous.len() == next.len()
        && previous
            .bytes()
            .zip(next.bytes())
            .all(|(a, b)| a == b || (a.is_ascii_digit() && b.is_ascii_digit()))
}

fn rolling_slot(previous: SharedString, next: SharedString, frame: &RollFrame) -> Div {
    div()
        .relative()
        .flex()
        .flex_col()
        .h(frame.height)
        .overflow_hidden()
        .child(div().whitespace_nowrap().opacity(0.0).child(next.clone()))
        .child(
            div()
                .whitespace_nowrap()
                .opacity(0.0)
                .child(previous.clone()),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(frame.outgoing)
                .opacity(1.0 - 0.24 * (1.0 - frame.remaining))
                .whitespace_nowrap()
                .child(previous),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(frame.incoming)
                .opacity(1.0 - 0.24 * frame.remaining)
                .whitespace_nowrap()
                .child(next),
        )
}

fn animated_number(text: SharedString, frame: &RollFrame) -> Div {
    if !same_digit_positions(frame.previous.as_ref(), text.as_ref()) {
        return rolling_slot(frame.previous.clone(), text, frame);
    }
    div()
        .flex()
        .overflow_hidden()
        .children(frame.previous.chars().zip(text.chars()).map(|(old, new)| {
            let next = SharedString::from(new.to_string());
            if old == new {
                div().flex_shrink_0().child(next)
            } else {
                rolling_slot(old.to_string().into(), next, frame).flex_shrink_0()
            }
        }))
}

impl IntoElement for RollingNumber {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RollingNumber {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let height = window.line_height();
        let tolerance = px(0.5 / window.scale_factor());
        let mut element = window.with_element_state(
            global_id.expect("rolling number has a stable element ID"),
            |state: Option<NumberState>, window| {
                let mut state = state.unwrap_or_else(|| NumberState {
                    shown: self.number.clone(),
                    roll: None,
                });
                let frame = state.update(
                    self.number.clone(),
                    frame_time(window),
                    height,
                    tolerance,
                    is_reduced_motion(),
                    self.paused,
                );
                let text = state.shown.text.clone();
                let element = if let Some(frame) = frame {
                    request_frame(window);
                    animated_number(text, &frame).into_any_element()
                } else {
                    text.into_any_element()
                };
                (element, state)
            },
        );
        let id = element.request_layout(window, cx);
        (id, element)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.paint(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn number(text: &'static str, value: impl Into<Option<f64>>) -> Number {
        Number {
            text: text.into(),
            value: value.into(),
        }
    }

    fn state(text: &'static str, value: f64) -> NumberState {
        NumberState {
            shown: number(text, value),
            roll: None,
        }
    }

    #[test]
    fn numbers_roll_in_from_the_direction_of_change_and_stop_at_display_precision() {
        let now = Instant::now();
        let height = px(25.0);
        let tolerance = px(0.25);
        let mut state = state("12", 12.0);
        let up = state
            .update(number("13", 13.0), now, height, tolerance, false, false)
            .unwrap();
        assert_eq!(up.incoming, height);
        assert_eq!(up.outgoing, px(0.0));
        let mid = state
            .update(
                number("13", 13.0),
                now + Duration::from_millis(80),
                height,
                tolerance,
                false,
                false,
            )
            .unwrap();
        assert!(mid.outgoing < px(0.0) && mid.incoming > tolerance);
        let settled_at = now + Duration::from_millis(190);
        assert!(
            state
                .update(
                    number("13", 13.0),
                    settled_at,
                    height,
                    tolerance,
                    false,
                    false
                )
                .is_none()
        );
        assert!(state.roll.is_none());
        let down = state
            .update(
                number("12", 12.0),
                settled_at,
                height,
                tolerance,
                false,
                false,
            )
            .unwrap();
        assert_eq!(down.incoming, -height);
    }

    #[test]
    fn returning_to_the_previous_number_reverses_from_the_current_glyph_locations() {
        let now = Instant::now();
        let height = px(24.0);
        let tolerance = px(0.25);
        let mut state = state("0", 0.0);
        state.update(number("1", 1.0), now, height, tolerance, false, false);
        let time = now + Duration::from_millis(45);
        let before = state
            .update(number("1", 1.0), time, height, tolerance, false, false)
            .unwrap();
        let reversed = state
            .update(number("0", 0.0), time, height, tolerance, false, false)
            .unwrap();
        assert_eq!(reversed.previous.as_ref(), "1");
        assert_eq!(state.shown.text.as_ref(), "0");
        assert!((before.outgoing - reversed.incoming).abs() < px(0.001));
        assert!((before.incoming - reversed.outgoing).abs() < px(0.001));
    }

    #[test]
    fn updates_during_a_roll_coalesce_to_the_latest_value() {
        let now = Instant::now();
        let mut state = state("10", 10.0);
        let mut update = |text, value, millis| {
            state
                .update(
                    number(text, value),
                    now + Duration::from_millis(millis),
                    px(20.0),
                    px(0.25),
                    false,
                    false,
                )
                .unwrap()
        };
        assert_eq!(update("11", 11.0, 0).previous.as_ref(), "10");
        assert_eq!(update("12", 12.0, 40).previous.as_ref(), "10");
        assert_eq!(update("13", 13.0, 190).previous.as_ref(), "11");
        assert_eq!(state.shown.text.as_ref(), "13");
    }

    #[test]
    fn loading_keeps_the_last_balance_and_reduced_motion_applies_the_value_immediately() {
        let now = Instant::now();
        let mut state = state("492", 492.0);
        let height = px(20.0);
        let tolerance = px(0.5);
        assert!(
            state
                .update(number("489", 489.0), now, height, tolerance, false, true)
                .is_none()
        );
        assert_eq!(state.shown.text.as_ref(), "492");
        assert_eq!(
            state
                .update(number("489", 489.0), now, height, tolerance, false, false)
                .unwrap()
                .previous
                .as_ref(),
            "492"
        );
        assert!(
            state
                .update(number("488", 488.0), now, height, tolerance, true, false)
                .is_none()
        );
        assert_eq!(state.shown.text.as_ref(), "488");
    }

    #[test]
    fn unchanged_display_values_and_missing_data_stay_still() {
        let now = Instant::now();
        let mut state = state("1.2K", 1_200.0);
        let height = px(20.0);
        let tolerance = px(0.5);
        for next in [number("1.2K", 1_201.0), number("—", None), number("0", 0.0)] {
            assert!(
                state
                    .update(next, now, height, tolerance, false, false)
                    .is_none()
            );
        }
        assert!(same_digit_positions("12.9%", "13.0%"));
        assert!(!same_digit_positions("9.9K", "10K"));
        assert!(!same_digit_positions("999 ms", "1.0 s"));
    }
}
