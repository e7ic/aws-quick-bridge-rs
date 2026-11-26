use super::{MultiSpringState, SpringConfig, frame_time, is_reduced_motion, request_frame};
use gpui::{
    App, Bounds, ContentMask, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Point, StyleRefinement, Styled, Window, point, px, size,
};
use std::cell::Cell;
use std::time::Instant;

thread_local! {
    // Track local changes when a child and its containing block both move.
    static PARENT_ORIGIN: Cell<Point<Pixels>> = const { Cell::new(Point::new(Pixels::ZERO, Pixels::ZERO)) };
    static SLIDE_OFFSET: Cell<Point<Pixels>> = const { Cell::new(Point::new(Pixels::ZERO, Pixels::ZERO)) };
}

pub(super) fn with_slide_offset<R>(offset: Point<Pixels>, draw: impl FnOnce() -> R) -> R {
    let previous = SLIDE_OFFSET.get();
    SLIDE_OFFSET.set(previous + offset);
    let result = draw();
    SLIDE_OFFSET.set(previous);
    result
}

struct LayoutState(MultiSpringState<4>);

impl LayoutState {
    fn coordinates(bounds: Bounds<Pixels>, parent_origin: Point<Pixels>) -> [f32; 4] {
        let local = bounds.origin - parent_origin;
        [
            local.x.into(),
            local.y.into(),
            bounds.size.width.into(),
            bounds.size.height.into(),
        ]
    }

    fn advance(
        &mut self,
        bounds: Bounds<Pixels>,
        element_offset: Point<Pixels>,
        parent_origin: Point<Pixels>,
        now: Instant,
        tolerance: f32,
        reduced_motion: bool,
    ) -> (Bounds<Pixels>, bool) {
        let logical = Bounds::new(bounds.origin - element_offset, bounds.size);
        let target = Self::coordinates(logical, parent_origin);
        let ([x, y, width, height], moving) = self.0.advance_with_policy(
            target,
            now,
            SpringConfig::SMOOTH,
            Some(tolerance),
            reduced_motion,
        );
        let origin = parent_origin + point(px(x), px(y)) + element_offset;
        (
            Bounds::new(origin, size(px(width.max(0.0)), px(height.max(0.0)))),
            moving,
        )
    }
}

/// Project the bounds of an identified element without changing its layout slot.
/// Apply this before erasing the concrete element type so it can paint at the presented bounds.
pub(in crate::gui) fn layout<E: IntoElement>(child: E) -> LayoutTransition<E::Element> {
    LayoutTransition {
        child: child.into_element(),
        anchored: false,
    }
}

pub(in crate::gui) struct LayoutTransition<E> {
    child: E,
    anchored: bool,
}

impl<E> LayoutTransition<E> {
    /// Account for position changes made by a deferred window anchor.
    pub(in crate::gui) fn anchored(mut self) -> Self {
        self.anchored = true;
        self
    }
}

pub(in crate::gui) struct LayoutPaint<S> {
    bounds: Bounds<Pixels>,
    mask: Option<ContentMask<Pixels>>,
    child: S,
}

impl<E: Element> IntoElement for LayoutTransition<E> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E: Styled> Styled for LayoutTransition<E> {
    fn style(&mut self) -> &mut StyleRefinement {
        self.child.style()
    }
}

impl<E: Element> Element for LayoutTransition<E> {
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = LayoutPaint<E::PrepaintState>;

    fn id(&self) -> Option<ElementId> {
        self.child.id()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        self.child.source_location()
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.child.request_layout(id, inspector, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        target: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let Some(id) = id else {
            return LayoutPaint {
                bounds: target,
                mask: None,
                child: self
                    .child
                    .prepaint(None, inspector, target, layout, window, cx),
            };
        };
        let element_offset = window.element_offset();
        let (offset, parent) = if self.anchored {
            (SLIDE_OFFSET.get(), Point::default())
        } else {
            (element_offset, PARENT_ORIGIN.get())
        };
        let logical = Bounds::new(target.origin - offset, target.size);
        let now = frame_time(window);
        let tolerance = 0.5 / window.scale_factor();
        let (bounds, moving) = window.with_element_state(id, |state: Option<LayoutState>, _| {
            let mut state = state.unwrap_or_else(|| {
                let initial = LayoutState::coordinates(logical, parent);
                LayoutState(MultiSpringState::new(initial, initial, now))
            });
            (
                state.advance(target, offset, parent, now, tolerance, is_reduced_motion()),
                state,
            )
        });
        if moving {
            request_frame(window);
        }
        let mask = (bounds.size != target.size).then_some(ContentMask { bounds });
        let previous = PARENT_ORIGIN.replace(target.origin - element_offset);
        let child = window.with_element_offset(bounds.origin - target.origin, |window| {
            window.with_content_mask(mask.clone(), |window| {
                self.child
                    .prepaint(Some(id), inspector, bounds, layout, window, cx)
            })
        });
        PARENT_ORIGIN.set(previous);
        LayoutPaint {
            bounds,
            mask,
            child,
        }
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        paint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(paint.mask.clone(), |window| {
            self.child.paint(
                id,
                inspector,
                paint.bounds,
                layout,
                &mut paint.child,
                window,
                cx,
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    fn layout_state(bounds: Bounds<Pixels>, parent: Point<Pixels>, time: Instant) -> LayoutState {
        let values = LayoutState::coordinates(bounds, parent);
        LayoutState(MultiSpringState::new(values, values, time))
    }

    #[test]
    fn position_and_size_continue_from_the_current_rectangle_when_retargeted() {
        let now = Instant::now();
        let origin = Point::default();
        let initial = rect(10.0, 20.0, 120.0, 52.0);
        let next = rect(50.0, 90.0, 200.0, 104.0);
        let last = rect(8.0, 28.0, 180.0, 42.0);
        let mut state = layout_state(initial, origin, now);
        let (start, active) = state.advance(next, origin, origin, now, 0.25, false);
        assert_eq!(start, initial);
        assert!(active);

        let time = now + Duration::from_millis(35);
        let (partway, _) = state.advance(next, origin, origin, time, 0.25, false);
        assert!(partway.origin.y > initial.origin.y && partway.origin.y < next.origin.y);
        assert!(partway.size.width > initial.size.width && partway.size.width < next.size.width);
        let (retargeted, _) = state.advance(last, origin, origin, time, 0.25, false);
        assert_eq!(retargeted, partway);

        for frame in 1..=90 {
            let time = time + Duration::from_millis(8 * frame);
            let (bounds, active) = state.advance(last, origin, origin, time, 0.25, false);
            if !active {
                assert_eq!(bounds, last);
                return;
            }
        }
        panic!("layout kept requesting animation frames after settling");
    }

    #[test]
    fn a_nested_item_inherits_parent_and_scroll_motion_without_animating_twice() {
        let now = Instant::now();
        let mut state = layout_state(
            rect(22.0, 100.0, 160.0, 30.0),
            point(px(10.0), px(80.0)),
            now,
        );
        let parent_target = point(px(100.0), px(200.0));
        let offset = point(px(-50.0), px(-137.0));
        let target = rect(62.0, 83.0, 160.0, 30.0);
        let (bounds, active) = state.advance(target, offset, parent_target, now, 0.25, false);
        assert_eq!(bounds, target);
        assert!(!active);
    }

    #[test]
    fn reduced_motion_removes_the_running_transition() {
        let now = Instant::now();
        let origin = Point::default();
        let mut state = layout_state(rect(10.0, 12.0, 20.0, 30.0), origin, now);
        let target = rect(80.0, 24.0, 150.0, 80.0);
        assert!(state.advance(target, origin, origin, now, 0.25, false).1);
        assert_eq!(
            state.advance(target, origin, origin, now, 0.25, true),
            (target, false)
        );
    }
}
