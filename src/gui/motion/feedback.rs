use super::{MultiSpringState, SpringConfig, frame_time, is_reduced_motion, request_frame};
use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, Hitbox, InspectorElementId,
    InteractiveElement, IntoElement, LayoutId, Pixels, StatefulInteractiveElement, StyleRefinement,
    Styled, Window,
};

pub(in crate::gui) fn control_feedback<E, F>(child: E, style: F) -> Feedback<E, F>
where
    E: Element<PrepaintState = Option<Hitbox>> + StatefulInteractiveElement + Styled,
    F: Fn(&mut StyleRefinement, f32, f32) + 'static,
{
    // Keep GPUI responsible for dispatch, focus and hitboxes.
    Feedback {
        child: child.hover(|style| style).active(|style| style),
        style,
    }
}

pub(in crate::gui) struct Feedback<E, F> {
    child: E,
    style: F,
}

struct FeedbackState(MultiSpringState<2>);

impl<E, F> IntoElement for Feedback<E, F>
where
    E: Element<PrepaintState = Option<Hitbox>> + InteractiveElement + Styled,
    F: Fn(&mut StyleRefinement, f32, f32) + 'static,
{
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E, F> Element for Feedback<E, F>
where
    E: Element<PrepaintState = Option<Hitbox>> + InteractiveElement + Styled,
    F: Fn(&mut StyleRefinement, f32, f32) + 'static,
{
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        Element::id(&self.child)
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
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.child
            .prepaint(id, inspector, bounds, state, window, cx)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(id) = id {
            let hovered = hitbox
                .as_ref()
                .is_some_and(|hitbox| hitbox.is_hovered(window));
            let pressed = hovered && self.child.interactivity().active.unwrap_or(false);
            let target = [f32::from(hovered), f32::from(pressed)];
            let now = frame_time(window);
            let reduced = is_reduced_motion();
            let ([hover, press], active) =
                window.with_element_state(id, |state: Option<FeedbackState>, _| {
                    let mut state = state.unwrap_or_else(|| {
                        FeedbackState(MultiSpringState::new(target, target, now))
                    });
                    let frame = state.0.advance_with_policy(
                        target,
                        now,
                        SpringConfig::FEEDBACK,
                        Some(0.008),
                        reduced,
                    );
                    (frame, state)
                });
            if active && window.content_mask().bounds.intersects(&bounds) {
                request_frame(window);
            }
            (self.style)(
                self.child.style(),
                hover.clamp(0.0, 1.0),
                press.clamp(0.0, 1.0),
            );
        }
        self.child
            .paint(id, inspector, bounds, layout, hitbox, window, cx);
    }
}
