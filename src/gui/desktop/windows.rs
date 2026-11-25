use gpui::{App, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::{LIM_SMALL, LoadIconMetric},
        Shell::{
            DefSubclassProc, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE,
            NIM_MODIFY, NIM_SETFOCUS, NIM_SETVERSION, NIN_SELECT, NOTIFYICON_VERSION_4,
            NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, RemoveWindowSubclass, SetWindowSubclass,
            Shell_NotifyIconGetRect, Shell_NotifyIconW,
        },
        WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos, IDI_APPLICATION,
            IsIconic, IsWindowVisible, LoadIconW, MF_SEPARATOR, MF_STRING, PostMessageW,
            PostQuitMessage, RegisterWindowMessageW, SPI_GETCLIENTAREAANIMATION, SW_HIDE,
            SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow, SystemParametersInfoW,
            TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx, WM_APP, WM_CLOSE,
            WM_CONTEXTMENU, WM_LBUTTONUP, WM_NCDESTROY, WM_NULL, WM_RBUTTONUP,
        },
    },
};
use windows_sys::w;

const ICON_ID: u32 = 1;
const APP_ICON_RESOURCE: u16 = 1;
const TRAY_SUBCLASS_ID: usize = 1;
const TRAY_CALLBACK: u32 = WM_APP + 0x51;
const OPEN_PANEL: usize = 1;
const QUIT_APP: usize = 2;
const NIN_KEYSELECT: u32 = NIN_SELECT + 1;
const LEGACY_TRAY: usize = 1;
const TRAY_V4: usize = NOTIFYICON_VERSION_4 as usize;
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

pub(in crate::gui) fn prefers_reduced_motion() -> bool {
    let mut enabled: i32 = 1;
    // SAFETY: 传入有效栈指针查询系统“客户端区域动画”开关。
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            (&raw mut enabled).cast(),
            0,
        )
    };
    ok != 0 && enabled == 0
}

pub(in crate::gui) fn keep_window_in_background(window: &Window, cx: &App) {
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = handle.hwnd.get() as HWND;

    // 窗口已由 GPUI 在当前线程创建，托盘事件沿用同一个消息循环。
    TASKBAR_CREATED.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) });
    if !register_tray(hwnd) {
        eprintln!("Quick Bridge could not register its Windows notification icon");
    }
    cx.on_app_quit(move |_| {
        uninstall_tray(hwnd);
        std::future::ready(())
    })
    .detach();
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    }
}

fn register_tray(hwnd: HWND) -> bool {
    let mut info = icon_data(hwnd);
    info.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    info.uCallbackMessage = TRAY_CALLBACK;
    for (slot, unit) in info.szTip.iter_mut().zip("Quick Bridge".encode_utf16()) {
        *slot = unit;
    }

    // SAFETY: Win32 在调用期间读取结构体。LoadIconMetric 返回的图标由本函数释放。
    let version = unsafe {
        let mut icon = null_mut();
        let own_icon = LoadIconMetric(
            GetModuleHandleW(null()),
            usize::from(APP_ICON_RESOURCE) as *const u16,
            LIM_SMALL,
            &mut icon,
        ) >= 0
            && !icon.is_null();
        info.hIcon = if own_icon {
            icon
        } else {
            LoadIconW(null_mut(), IDI_APPLICATION)
        };
        let registered = !info.hIcon.is_null()
            && (Shell_NotifyIconW(NIM_ADD, &info) != 0
                || Shell_NotifyIconW(NIM_MODIFY, &info) != 0);
        if own_icon {
            DestroyIcon(icon);
        }
        if registered {
            let mut version_info = icon_data(hwnd);
            version_info.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            if Shell_NotifyIconW(NIM_SETVERSION, &version_info) != 0 {
                TRAY_V4
            } else {
                LEGACY_TRAY
            }
        } else {
            0
        }
    };

    // SAFETY: 只在窗口所属线程安装或更新子类。ref_data 是整数版本号。
    if unsafe { SetWindowSubclass(hwnd, Some(tray_window_proc), TRAY_SUBCLASS_ID, version) } == 0 {
        unsafe { Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd)) };
        return false;
    }
    version != 0
}

fn uninstall_tray(hwnd: HWND) {
    // SAFETY: 移除托盘处理器后，关闭通知图标。窗口已销毁时移除操作会失败。
    unsafe {
        if RemoveWindowSubclass(hwnd, Some(tray_window_proc), TRAY_SUBCLASS_ID) != 0 {
            Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
        }
    }
}

fn show_panel(hwnd: HWND) {
    // SAFETY: 这是由托盘消息交给当前线程的窗口句柄。
    unsafe {
        let show = if IsIconic(hwnd) != 0 {
            SW_RESTORE
        } else {
            SW_SHOW
        };
        ShowWindow(hwnd, show);
        SetForegroundWindow(hwnd);
    }
}

fn tray_menu(hwnd: HWND) -> usize {
    let identifier = NOTIFYICONIDENTIFIER {
        cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    };
    let mut icon_rect = RECT::default();
    let mut position = POINT::default();

    // SAFETY: 菜单由当前线程创建和销毁。Windows 在调用期间读取参数。
    unsafe {
        if Shell_NotifyIconGetRect(&identifier, &mut icon_rect) >= 0 {
            position.x = icon_rect.left;
            position.y = icon_rect.top;
        } else if GetCursorPos(&mut position) == 0 {
            return 0;
        }
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return 0;
        }
        AppendMenuW(menu, MF_STRING, OPEN_PANEL, w!("打开面板"));
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, QUIT_APP, w!("退出 Quick Bridge"));
        SetForegroundWindow(hwnd);
        let command = TrackPopupMenuEx(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            position.x,
            position.y,
            hwnd,
            null(),
        );
        DestroyMenu(menu);
        PostMessageW(hwnd, WM_NULL, 0, 0);
        command as usize
    }
}

unsafe extern "system" fn tray_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    version: usize,
) -> LRESULT {
    match message {
        WM_CLOSE if version != 0 => {
            // SAFETY: 句柄来自窗口消息。隐藏窗口保留 GPUI 根视图和网关。
            unsafe { ShowWindow(hwnd, SW_HIDE) };
            return 0;
        }
        TRAY_CALLBACK if version != 0 => {
            let event = (lparam as u32) & 0xffff;
            match (version == TRAY_V4, event) {
                (true, NIN_SELECT | NIN_KEYSELECT) | (false, WM_LBUTTONUP) => show_panel(hwnd),
                (true, WM_CONTEXTMENU) | (false, WM_RBUTTONUP) => match tray_menu(hwnd) {
                    OPEN_PANEL => show_panel(hwnd),
                    // SAFETY: WM_QUIT 发送到 GPUI 的 Windows 主消息循环，触发应用退出回调。
                    QUIT_APP => unsafe { PostQuitMessage(0) },
                    _ => {
                        unsafe { Shell_NotifyIconW(NIM_SETFOCUS, &icon_data(hwnd)) };
                    }
                },
                _ => {}
            }
            return 0;
        }
        WM_NCDESTROY => uninstall_tray(hwnd),
        _ if TASKBAR_CREATED
            .get()
            .is_some_and(|created| *created != 0 && message == *created) =>
        {
            if !register_tray(hwnd) && unsafe { IsWindowVisible(hwnd) } == 0 {
                show_panel(hwnd);
            }
        }
        _ => {}
    }
    // SAFETY: 将剩余消息转发给当前窗口原有的消息处理链。
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, IsWindow, SW_SHOWNOACTIVATE, SendMessageW,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
    };

    struct TestWindow(HWND);

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // SAFETY: 销毁本测试在当前线程创建的窗口。
            unsafe { DestroyWindow(self.0) };
        }
    }

    #[test]
    fn tray_close_and_reopen_preserve_the_native_window() {
        // SAFETY: 使用系统已有的窗口类，随后在同一线程发送窗口消息。
        unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("Quick Bridge tray test"),
                WS_POPUP,
                0,
                0,
                100,
                100,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!hwnd.is_null());
            let _window = TestWindow(hwnd);
            // 模拟通知区已成功注册，窗口测试可在无 Explorer 的 CI 中执行。
            assert_ne!(
                SetWindowSubclass(hwnd, Some(tray_window_proc), TRAY_SUBCLASS_ID, TRAY_V4),
                0
            );
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            assert_ne!(IsWindowVisible(hwnd), 0);

            SendMessageW(hwnd, WM_CLOSE, 0, 0);
            assert_ne!(IsWindow(hwnd), 0);
            assert_eq!(IsWindowVisible(hwnd), 0);

            SendMessageW(
                hwnd,
                TRAY_CALLBACK,
                0,
                ((ICON_ID << 16) | NIN_SELECT) as LPARAM,
            );
            assert_ne!(IsWindowVisible(hwnd), 0);
        }
    }
}
