use super::{high_contrast_enabled, native_window};
use crate::gui::components::Theme;
use gpui::{Hsla, Window};
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::{
    Foundation::{E_INVALIDARG, HWND, LPARAM, LRESULT, WPARAM},
    Graphics::Dwm::{
        DWMWA_CAPTION_COLOR, DWMWA_COLOR_DEFAULT, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
        DWMWINDOWATTRIBUTE, DwmSetWindowAttribute,
    },
    UI::{
        Input::KeyboardAndMouse::GetActiveWindow,
        Shell::{DefSubclassProc, GetWindowSubclass, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            DefWindowProcW, WM_NCACTIVATE, WM_NCDESTROY, WM_SETTINGCHANGE, WM_SYSCOLORCHANGE,
            WM_THEMECHANGED,
        },
    },
};

const CAPTION_SUBCLASS: usize = 1;
const DARK_MODE_BEFORE_20H1: DWMWINDOWATTRIBUTE = 19;
static CUSTOM_CAPTION_COLORS: AtomicBool = AtomicBool::new(true);

pub(in crate::gui) fn sync_window_caption(window: &Window, darkness: f32, theme: &Theme) {
    if let Some(hwnd) = native_window(window) {
        let step = (darkness * 255.0).round() as u8;
        sync_caption(hwnd, step, theme);
    }
}

// 窗口子类只保存 0–255 的过渡进度。窗口隐藏或系统重绘时可以恢复当前配色。
fn caption_step(hwnd: HWND) -> Option<u8> {
    let mut step = 0;
    unsafe { GetWindowSubclass(hwnd, Some(caption_proc), CAPTION_SUBCLASS, &mut step) != 0 }
        .then_some(step as u8)
}

fn sync_caption(hwnd: HWND, step: u8, theme: &Theme) {
    let previous = caption_step(hwnd);
    if previous == Some(step) {
        return;
    }
    unsafe {
        SetWindowSubclass(
            hwnd,
            Some(caption_proc),
            CAPTION_SUBCLASS,
            usize::from(step),
        )
    };
    let dark = step >= 128;
    let mode_changed = previous.is_none_or(|old| (old >= 128) != dark);
    paint_caption(hwnd, theme, dark, mode_changed);
    if mode_changed && !CUSTOM_CAPTION_COLORS.load(Ordering::Relaxed) {
        // Windows 10 会把标题栏颜色缓存到激活态变化。只刷新原生边框，保留真实焦点。
        unsafe {
            let active = GetActiveWindow() == hwnd;
            DefWindowProcW(hwnd, WM_NCACTIVATE, usize::from(!active), 0);
            DefWindowProcW(hwnd, WM_NCACTIVATE, usize::from(active), 0);
        }
    }
}

fn colorref(color: Hsla) -> u32 {
    let rgb = color.to_rgb();
    let channel = |value: f32| (value * 255.0).round() as u8;
    u32::from_le_bytes([channel(rgb.r), channel(rgb.g), channel(rgb.b), 0])
}

fn set_attribute(hwnd: HWND, attribute: DWMWINDOWATTRIBUTE, value: u32) -> i32 {
    // SAFETY: 这些 DWM 属性均接收四字节的 BOOL 或 COLORREF。
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            attribute as u32,
            (&raw const value).cast(),
            size_of::<u32>() as u32,
        )
    }
}

fn paint_caption(hwnd: HWND, theme: &Theme, dark: bool, update_mode: bool) {
    let custom_colors = CUSTOM_CAPTION_COLORS.load(Ordering::Relaxed);
    if !update_mode && !custom_colors {
        return;
    }
    let contrast = high_contrast_enabled();
    if update_mode {
        let dark = u32::from(dark && !contrast);
        if set_attribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, dark) == E_INVALIDARG {
            set_attribute(hwnd, DARK_MODE_BEFORE_20H1, dark);
        }
    }
    if custom_colors {
        let (background, text) = if contrast {
            (DWMWA_COLOR_DEFAULT, DWMWA_COLOR_DEFAULT)
        } else {
            (colorref(theme.bg_window), colorref(theme.text_primary))
        };
        let result = set_attribute(hwnd, DWMWA_CAPTION_COLOR, background);
        if result >= 0 {
            set_attribute(hwnd, DWMWA_TEXT_COLOR, text);
            return;
        }
        if result == E_INVALIDARG {
            // Windows 10 仅支持原生深浅标题栏。后续动画帧可跳过自定义颜色调用。
            CUSTOM_CAPTION_COLORS.store(false, Ordering::Relaxed);
        }
    }
}

unsafe extern "system" fn caption_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        unsafe { RemoveWindowSubclass(hwnd, Some(caption_proc), id) };
    }
    // GPUI 会在处理系统通知时写入系统深浅色，在它返回后恢复应用当前选色。
    let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    if matches!(
        message,
        WM_SETTINGCHANGE | WM_THEMECHANGED | WM_SYSCOLORCHANGE
    ) && let Some(step) = caption_step(hwnd)
    {
        let theme = Theme::interpolated(f32::from(step) / 255.0);
        paint_caption(hwnd, &theme, step >= 128, true);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::tests::TestWindow;
    use super::*;
    use windows_sys::Win32::Graphics::Dwm::DwmGetWindowAttribute;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageW, WS_OVERLAPPEDWINDOW};

    const SYSTEM_STYLE: [(DWMWINDOWATTRIBUTE, u32); 3] = [
        (DWMWA_USE_IMMERSIVE_DARK_MODE, 0),
        (DWMWA_CAPTION_COLOR, 0xfafafa),
        (DWMWA_TEXT_COLOR, 0x101010),
    ];

    fn read_attribute(hwnd: HWND, attribute: DWMWINDOWATTRIBUTE) -> Option<u32> {
        let mut value = 0u32;
        let result = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                attribute as u32,
                (&raw mut value).cast(),
                size_of::<u32>() as u32,
            )
        };
        (result >= 0).then_some(value)
    }

    unsafe extern "system" fn framework_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _: usize,
        _: usize,
    ) -> LRESULT {
        let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
        if message == WM_SETTINGCHANGE {
            for (attribute, value) in SYSTEM_STYLE {
                set_attribute(hwnd, attribute, value);
            }
        }
        result
    }

    #[test]
    fn system_notification_preserves_the_current_caption_even_mid_transition() {
        let window = TestWindow::new(WS_OVERLAPPEDWINDOW);
        let hwnd = window.0;
        // 先安装框架的消息处理器，复现它在系统通知返回时覆写标题栏的顺序。
        assert_ne!(
            unsafe { SetWindowSubclass(hwnd, Some(framework_proc), 1, 0) },
            0
        );
        let readable = SYSTEM_STYLE.map(|(attribute, value)| {
            set_attribute(hwnd, attribute, value) >= 0
                && read_attribute(hwnd, attribute) == Some(value)
        });
        let contrast = high_contrast_enabled();
        for step in [0, 255, 96, 192, 0] {
            let theme = Theme::interpolated(f32::from(step) / 255.0);
            sync_caption(hwnd, step, &theme);
            let check = || {
                assert_eq!(caption_step(hwnd), Some(step));
                let expected = [
                    u32::from(step >= 128 && !contrast),
                    colorref(theme.bg_window),
                    colorref(theme.text_primary),
                ];
                for (i, (attribute, _)) in SYSTEM_STYLE.into_iter().enumerate() {
                    if readable[i] && (i == 0 || !contrast) {
                        assert_eq!(read_attribute(hwnd, attribute), Some(expected[i]));
                    }
                }
            };
            check();
            unsafe { SendMessageW(hwnd, WM_SETTINGCHANGE, 0, 0) };
            check();
        }
    }
}
