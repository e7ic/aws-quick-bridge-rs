#[cfg(not(target_os = "windows"))]
use gpui::{App, Window};

pub(super) mod claude_link;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub(super) use windows::{keep_window_in_background, prefers_reduced_motion};

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
fn background_window(_: &mut Window, cx: &mut App) {
    cx.hide();
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn background_window(window: &mut Window, _: &mut App) {
    window.minimize_window();
}

#[cfg(target_os = "macos")]
mod macos {
    use super::App;
    use gpui::{KeyBinding, Menu, MenuItem, actions};
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage, NSWorkspace};
    use objc2_foundation::NSData;

    const APP_ICON: &[u8] = include_bytes!("../../assets/app-icon.png");

    actions!(quick_bridge, [ShowWindow, HideWindow, QuitApplication]);

    pub(in crate::gui) fn configure_application(cx: &mut App) {
        set_dock_icon();
        cx.on_action(|_: &ShowWindow, cx| show_window(cx));
        cx.on_action(|_: &HideWindow, cx| cx.hide());
        cx.on_action(|_: &QuitApplication, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("cmd-w", HideWindow, None),
            KeyBinding::new("cmd-q", QuitApplication, None),
        ]);
        cx.set_menus(vec![Menu {
            name: "Quick Bridge".into(),
            items: vec![
                MenuItem::action("打开窗口", ShowWindow),
                MenuItem::action("收起窗口", HideWindow),
                MenuItem::separator(),
                MenuItem::action("退出 Quick Bridge", QuitApplication),
            ],
        }]);
    }

    pub(in crate::gui) fn show_window(cx: &mut App) {
        if let Some(main_thread) = MainThreadMarker::new() {
            NSApplication::sharedApplication(main_thread).unhideWithoutActivation();
        }
        if let Some(handle) = cx.active_window().or_else(|| cx.windows().first().copied()) {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        cx.activate(true);
    }

    pub(in crate::gui) fn prefers_reduced_motion() -> bool {
        NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
    }

    fn set_dock_icon() {
        let Some(main_thread) = MainThreadMarker::new() else {
            return;
        };
        let Some(icon) = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(APP_ICON))
        else {
            eprintln!("Quick Bridge could not load the application icon");
            return;
        };
        // SAFETY：主线程上向 AppKit 传入已校验的非空 NSImage。
        unsafe {
            NSApplication::sharedApplication(main_thread).setApplicationIconImage(Some(&icon))
        }
    }
}

#[cfg(target_os = "macos")]
pub(super) use macos::{configure_application, prefers_reduced_motion, show_window};
