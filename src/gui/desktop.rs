#[cfg(not(target_os = "windows"))]
use gpui::{App, Window};

pub(super) mod claude_link;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(super) use macos::{
    configure_application, prefers_reduced_motion, set_main_window, show_window,
};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub(super) use windows::{
    keep_window_in_background, prefers_reduced_motion, sync_window_caption, take_quit_request,
};

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(super) fn prefers_reduced_motion() -> bool {
    false
}

#[cfg(not(target_os = "windows"))]
pub(super) fn keep_window_in_background(window: &Window, cx: &App) {
    window.on_window_should_close(cx, |window, cx| {
        // 拦截关闭请求，保持根视图与运行中的网关继续驻留后台。
        window.defer(cx, background_window);
        false
    });
}

#[cfg(target_os = "macos")]
fn background_window(window: &mut Window, cx: &mut App) {
    macos::hide_window(window, cx);
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn background_window(window: &mut Window, _: &mut App) {
    window.minimize_window();
}
