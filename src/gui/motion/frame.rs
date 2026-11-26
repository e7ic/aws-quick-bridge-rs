use gpui::{EntityId, Window, WindowId};
use std::cell::Cell;
use std::time::Instant;

#[derive(Clone, Copy)]
struct Frame {
    window: WindowId,
    view: EntityId,
    time: Instant,
    requested: bool,
}

thread_local! {
    static FRAME: Cell<Option<Frame>> = const { Cell::new(None) };
}

pub(in crate::gui) fn begin_frame(window: &Window, view: EntityId) -> Instant {
    let time = Instant::now();
    let id = window.window_handle().window_id();
    let requested = FRAME
        .get()
        .is_some_and(|frame| frame.window == id && frame.view == view && frame.requested);
    FRAME.set(Some(Frame {
        window: id,
        view,
        time,
        requested,
    }));
    time
}

pub(in crate::gui) fn frame_time(window: &Window) -> Instant {
    let id = window.window_handle().window_id();
    FRAME
        .get()
        .filter(|frame| frame.window == id)
        .map_or_else(Instant::now, |frame| frame.time)
}

pub(in crate::gui) fn request_frame(window: &Window) {
    let id = window.window_handle().window_id();
    let Some(mut frame) = FRAME.get().filter(|frame| frame.window == id) else {
        window.request_animation_frame();
        return;
    };
    if frame.requested {
        return;
    }
    frame.requested = true;
    FRAME.set(Some(frame));
    window.on_next_frame(move |_, cx| {
        if let Some(mut current) = FRAME
            .get()
            .filter(|current| current.window == id && current.view == frame.view)
        {
            current.requested = false;
            FRAME.set(Some(current));
        }
        cx.notify(frame.view);
    });
}
