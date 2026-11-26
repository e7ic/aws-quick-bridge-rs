use super::{frame_time, request_frame};
use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window,
};
use std::time::{Duration, Instant};

pub(in crate::gui) fn timeline<E: IntoElement + 'static>(
    id: impl Into<ElementId>,
    duration: Duration,
    render: impl FnOnce(f32) -> E + 'static,
) -> Timeline {
    Timeline {
        id: id.into(),
        duration,
        loops: false,
        render: Some(Box::new(move |phase| render(phase).into_any_element())),
    }
}

pub(in crate::gui) struct Timeline {
    id: ElementId,
    duration: Duration,
    loops: bool,
    render: Option<Box<dyn FnOnce(f32) -> AnyElement>>,
}

impl Timeline {
    pub(in crate::gui) fn repeat(mut self) -> Self {
        self.loops = true;
        self
    }
}

pub(in crate::gui) struct TimelineLayout {
    content: AnyElement,
    animating: bool,
}

impl IntoElement for Timeline {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Timeline {
    type RequestLayoutState = TimelineLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let now = frame_time(window);
        let started =
            window.with_element_state(id.expect("a timeline has an element ID"), |started, _| {
                let started: Instant = started.unwrap_or(now);
                (started, started)
            });
        let elapsed = now.saturating_duration_since(started);
        let (phase, animating) = timeline_phase(elapsed, self.duration, self.loops);
        let mut content = self.render.take().expect("timeline is rendered once")(phase);
        let id = content.request_layout(window, cx);
        (id, TimelineLayout { content, animating })
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if state.animating && window.content_mask().bounds.intersects(&bounds) {
            request_frame(window);
        }
        state.content.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        state.content.paint(window, cx);
    }
}

fn timeline_phase(elapsed: Duration, duration: Duration, loops: bool) -> (f32, bool) {
    if duration.is_zero() {
        return (1.0, false);
    }
    let phase = elapsed.as_secs_f64() / duration.as_secs_f64();
    if loops {
        (phase.fract() as f32, true)
    } else {
        (phase.min(1.0) as f32, elapsed < duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timelines_finish_and_repeating_phases_keep_the_same_clock() {
        let duration = Duration::from_millis(100);
        assert_eq!(
            timeline_phase(Duration::from_millis(25), duration, false),
            (0.25, true)
        );
        assert_eq!(
            timeline_phase(Duration::from_millis(125), duration, false),
            (1.0, false)
        );
        assert_eq!(
            timeline_phase(Duration::from_millis(125), duration, true),
            (0.25, true)
        );
    }
}
