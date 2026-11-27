mod panel;

use crate::gui::{BridgeApp, Tab, motion};
use block2::RcBlock;
use gpui::{
    App, AppContext, AsyncApp, Bounds, DisplayId, Global, KeyBinding, Menu, MenuItem, Pixels,
    Timer, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
    WindowOptions, actions, point, px, size,
};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventMask, NSEventType, NSImage,
    NSPanel, NSScreen, NSSquareStatusItemLength, NSStatusBar, NSStatusBarButton, NSStatusItem,
    NSView, NSWindow, NSWindowAnimationBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{NSData, NSNumber, NSPoint, NSRect, NSSize, NSString};
use panel::{MenuPanel, PanelContent};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::ptr::NonNull;
use std::time::Instant;

const APP_ICON: &[u8] = include_bytes!("../../../assets/app-icon.png");
const MENU_BAR_ICON: &[u8] = include_bytes!("../../../assets/menu-bar-icon.png");
const OPEN_PANEL: &str = "打开主窗口";
const QUIT_APP: &str = "退出 Quick Bridge";

actions!(quick_bridge, [ShowWindow, HideWindow, QuitApplication]);

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "QuickBridgeStatusMenuTarget"]
    #[ivars = AsyncApp]
    struct StatusMenuTarget;

    impl StatusMenuTarget {
        #[unsafe(method(togglePanel:))]
        fn toggle_panel(&self, _: &AnyObject) {
            let pointer = NSEvent::mouseLocation();
            self.ivars().spawn(async move |cx| {
                let _ = cx.update(|cx| toggle_drawer(cx, pointer));
            }).detach();
        }
    }
);

struct MenuBar {
    item: Retained<NSStatusItem>,
    // AppKit 的按钮弱持有 target。
    _target: Retained<StatusMenuTarget>,
}

#[derive(Default)]
struct MacDesktop {
    menu_bar: Option<MenuBar>,
    main_window: Option<WindowHandle<BridgeApp>>,
    drawer: Option<Drawer>,
}

impl Global for MacDesktop {}

struct Drawer {
    overview: WindowHandle<MenuPanel>,
    detail: Option<DetailWindow>,
    placement: PanelPlacement,
    restore_main: bool,
    _outside_clicks: OutsideClicks,
}

struct DetailWindow {
    handle: WindowHandle<MenuPanel>,
    account_key: String,
}

impl Drawer {
    fn windows(&self) -> impl Iterator<Item = WindowHandle<MenuPanel>> + '_ {
        std::iter::once(self.overview).chain(self.detail.as_ref().map(|detail| detail.handle))
    }
}

#[derive(Clone, Copy)]
struct PanelPlacement {
    screen_frame: NSRect,
    display_id: Option<DisplayId>,
    overview: NSRect,
    detail: Option<NSRect>,
}

impl PanelPlacement {
    fn within(screen_frame: NSRect, work_area: NSRect, display_id: Option<DisplayId>) -> Self {
        let padding = f64::from(panel::SHADOW_INSET);
        let edge_gap = 2.0;
        let panel_gap = 10.0;
        let top = work_area.max().y - edge_gap;
        let right = work_area.max().x - edge_gap;
        let left_limit = work_area.min().x + edge_gap;
        let max_height = (work_area.size.height - edge_gap * 2.0).max(1.0);
        let width = (panel::MAIN_SIZE.0 + padding * 2.0).min((right - left_limit).max(1.0));
        let height = (panel::MAIN_SIZE.1 + padding * 2.0).min(max_height);
        let overview = NSRect::new(
            NSPoint::new(right - width, top - height),
            NSSize::new(width, height),
        );

        let detail_right = overview.min().x + padding * 2.0 - panel_gap;
        let available = detail_right - left_limit;
        let detail = (available >= 260.0 + padding * 2.0).then(|| {
            let width = (panel::DETAIL_SIZE.0 + padding * 2.0).min(available);
            let height = (panel::DETAIL_SIZE.1 + padding * 2.0).min(max_height);
            NSRect::new(
                NSPoint::new(detail_right - width, top - height),
                NSSize::new(width, height),
            )
        });
        Self {
            screen_frame,
            display_id,
            overview,
            detail,
        }
    }

    fn initial_bounds(&self, frame: NSRect) -> Bounds<Pixels> {
        // AppKit 使用屏幕左下角坐标，GPUI 的初始窗口位置相对于目标屏幕左上角。
        Bounds::new(
            point(
                px((frame.min().x - self.screen_frame.min().x) as f32),
                px((self.screen_frame.max().y - frame.max().y) as f32),
            ),
            size(px(frame.size.width as f32), px(frame.size.height as f32)),
        )
    }
}

pub(in crate::gui) fn configure_application(cx: &mut App) {
    let menu_bar = MainThreadMarker::new().and_then(|main_thread| {
        set_dock_icon(main_thread);
        install_menu_bar(cx, main_thread)
    });
    cx.set_global(MacDesktop {
        menu_bar,
        ..Default::default()
    });
    cx.on_action(|_: &ShowWindow, cx| cx.defer(show_window));
    cx.on_action(|_: &HideWindow, cx| cx.defer(hide_focused_window));
    cx.on_action(|_: &QuitApplication, cx| cx.quit());
    cx.bind_keys([
        KeyBinding::new("cmd-w", HideWindow, None),
        KeyBinding::new("cmd-q", QuitApplication, None),
    ]);
    cx.set_menus(vec![Menu {
        name: "Quick Bridge".into(),
        items: vec![
            MenuItem::action(OPEN_PANEL, ShowWindow),
            MenuItem::action("收起窗口", HideWindow),
            MenuItem::separator(),
            MenuItem::action(QUIT_APP, QuitApplication),
        ],
    }]);
}

pub(in crate::gui) fn set_main_window(cx: &mut App, window: WindowHandle<BridgeApp>) {
    cx.global_mut::<MacDesktop>().main_window = Some(window);
}

fn install_menu_bar(cx: &mut App, main_thread: MainThreadMarker) -> Option<MenuBar> {
    let bar = NSStatusBar::systemStatusBar();
    let item = bar.statusItemWithLength(NSSquareStatusItemLength);
    let Some(button) = item.button(main_thread) else {
        bar.removeStatusItem(&item);
        eprintln!("Quick Bridge could not create its macOS menu bar item");
        return None;
    };

    let app_name = NSString::from_str("Quick Bridge");
    item.setAutosaveName(Some(&app_name));
    button.setToolTip(Some(&app_name));
    if let Some(icon) = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(MENU_BAR_ICON))
    {
        icon.setSize(NSSize::new(18.0, 16.0));
        icon.setTemplate(true);
        button.setImage(Some(&icon));
    } else {
        button.setTitle(&NSString::from_str("QB"));
    }

    let target = StatusMenuTarget::alloc(main_thread).set_ivars(cx.to_async());
    // SAFETY: 主线程上初始化刚分配的 NSObject 子类。选择器签名与按钮的一参数 action 一致。
    let target: Retained<StatusMenuTarget> = unsafe { msg_send![super(target), init] };
    // SAFETY: target 由 MenuBar 持有，直到移除状态栏图标。
    unsafe {
        button.setTarget(Some(&target));
        button.setAction(Some(sel!(togglePanel:)));
    }

    let item_on_quit = item.clone();
    cx.on_app_quit(move |cx| {
        cx.global_mut::<MacDesktop>().drawer.take();
        bar.removeStatusItem(&item_on_quit);
        std::future::ready(())
    })
    .detach();
    Some(MenuBar {
        item,
        _target: target,
    })
}

fn native_window(window: &Window) -> Option<Retained<NSWindow>> {
    let raw = HasWindowHandle::window_handle(window).ok()?.as_raw();
    let RawWindowHandle::AppKit(handle) = raw else {
        return None;
    };
    // SAFETY: GPUI 的窗口在调用期间存活，AppKit 的窗口句柄指向其主线程 NSView。
    unsafe { handle.ns_view.cast::<NSView>().as_ref() }.window()
}

fn drawer_native(cx: &mut App, handle: WindowHandle<MenuPanel>) -> Option<Retained<NSWindow>> {
    handle
        .update(cx, |_, window, _| native_window(window))
        .ok()
        .flatten()
}

fn status_button(cx: &App, mtm: MainThreadMarker) -> Option<Retained<NSStatusBarButton>> {
    cx.try_global::<MacDesktop>()?
        .menu_bar
        .as_ref()?
        .item
        .button(mtm)
}

fn contains_point(rect: NSRect, point: NSPoint) -> bool {
    let (min, max) = (rect.min(), rect.max());
    (min.x..max.x).contains(&point.x) && (min.y..max.y).contains(&point.y)
}

fn screen_placement(cx: &App, pointer: NSPoint) -> Option<PanelPlacement> {
    let mtm = MainThreadMarker::new()?;
    let screen = NSScreen::screens(mtm)
        .iter()
        .find(|screen| contains_point(screen.frame(), pointer))
        .or_else(|| status_button(cx, mtm)?.window()?.screen())
        .or_else(|| NSScreen::mainScreen(mtm))?;
    let screen_number = screen
        .deviceDescription()
        .objectForKey(&NSString::from_str("NSScreenNumber"))?
        .downcast::<NSNumber>()
        .ok()?
        .unsignedIntValue();
    let display_id = cx
        .displays()
        .into_iter()
        .map(|display| display.id())
        .find(|id| u32::from(*id) == screen_number)?;
    Some(PanelPlacement::within(
        screen.frame(),
        screen.visibleFrame(),
        Some(display_id),
    ))
}

fn pointer_over_button(button: &NSStatusBarButton) -> bool {
    button.window().is_some_and(|window| {
        let frame = window.convertRectToScreen(button.convertRect_toView(button.bounds(), None));
        contains_point(frame, NSEvent::mouseLocation())
    })
}

fn set_status_highlight(cx: &App, highlighted: bool) {
    if let Some(button) = MainThreadMarker::new().and_then(|mtm| status_button(cx, mtm)) {
        button.setHighlighted(highlighted);
    }
}

struct OutsideClicks {
    local: Option<Retained<AnyObject>>,
    global: Option<Retained<AnyObject>>,
}

impl OutsideClicks {
    fn new(cx: &App, overview: WindowHandle<MenuPanel>) -> Self {
        let mask =
            NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown | NSEventMask::OtherMouseDown;
        let local_cx = cx.to_async();
        let button = MainThreadMarker::new().and_then(|mtm| status_button(cx, mtm));
        let local_button = button.clone();
        let local = RcBlock::new(move |event: NonNull<NSEvent>| {
            if !local_button.as_deref().is_some_and(pointer_over_button) {
                // SAFETY: AppKit 提供的事件在监视回调期间存活。
                let window_number = unsafe { event.as_ref() }.windowNumber();
                local_cx
                    .spawn(async move |cx| {
                        let _ =
                            cx.update(|cx| dismiss_if_outside(cx, overview, Some(window_number)));
                    })
                    .detach();
            }
            event.as_ptr()
        });
        let global_cx = cx.to_async();
        let global = RcBlock::new(move |_: NonNull<NSEvent>| {
            if !button.as_deref().is_some_and(pointer_over_button) {
                global_cx
                    .spawn(async move |cx| {
                        let _ = cx.update(|cx| dismiss_if_outside(cx, overview, None));
                    })
                    .detach();
            }
        });

        // SAFETY: 本地回调原样返回 AppKit 传入的有效事件，不拦截鼠标输入。
        Self {
            local: unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &local) },
            global: NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &global),
        }
    }
}

impl Drop for OutsideClicks {
    fn drop(&mut self) {
        for monitor in [self.local.take(), self.global.take()]
            .into_iter()
            .flatten()
        {
            // SAFETY: 每个对象都来自 AppKit 的事件监视注册，且只移除一次。
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
    }
}

fn dismiss_if_outside(cx: &mut App, overview: WindowHandle<MenuPanel>, number: Option<isize>) {
    let Some(drawer) = cx.global::<MacDesktop>().drawer.as_ref() else {
        return;
    };
    if drawer.overview != overview {
        return;
    }
    let panels: Vec<_> = drawer.windows().collect();
    let inside = number.is_some_and(|number| {
        panels.into_iter().any(|handle| {
            drawer_native(cx, handle).is_some_and(|window| window.windowNumber() == number)
        })
    });
    if !inside {
        dismiss_drawer(cx);
    }
}

fn dismiss_if_unfocused(cx: &mut App, from: WindowHandle<MenuPanel>) {
    let Some(drawer) = cx.global::<MacDesktop>().drawer.as_ref() else {
        return;
    };
    if !drawer.windows().any(|handle| handle == from) {
        return;
    }
    if let Some(mtm) = MainThreadMarker::new()
        && NSApplication::sharedApplication(mtm)
            .currentEvent()
            .is_some_and(|event| {
                matches!(
                    event.r#type(),
                    NSEventType::LeftMouseDown | NSEventType::LeftMouseUp
                )
            })
        && status_button(cx, mtm)
            .as_deref()
            .is_some_and(pointer_over_button)
    {
        // 图标的 action 负责切换，避免失焦先收起而 action 又把面板打开。
        return;
    }
    let panels: Vec<_> = drawer.windows().collect();
    if !panels
        .into_iter()
        .any(|handle| drawer_native(cx, handle).is_some_and(|window| window.isKeyWindow()))
    {
        dismiss_drawer(cx);
    }
}

fn create_panel(
    cx: &mut App,
    placement: PanelPlacement,
    frame: NSRect,
    content: PanelContent,
) -> anyhow::Result<WindowHandle<MenuPanel>> {
    use anyhow::Context as _;

    let bridge = cx
        .global::<MacDesktop>()
        .main_window
        .context("找不到主窗口")?
        .entity(cx)?;
    let focus = matches!(content, PanelContent::Overview);
    let handle = cx.open_window(
        WindowOptions {
            // GPUI 会在 open_window 返回前绘制首帧，此时窗口就需要在目标屏幕上。
            window_bounds: Some(WindowBounds::Windowed(placement.initial_bounds(frame))),
            display_id: placement.display_id,
            titlebar: None,
            focus: false,
            show: false,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        },
        |window, cx| cx.new(|cx| MenuPanel::new(content, bridge, window, cx)),
    )?;
    let Some(window) = drawer_native(cx, handle) else {
        remove_window(cx, handle);
        anyhow::bail!("找不到菜单栏面板对应的 AppKit 窗口");
    };
    window.setStyleMask(NSWindowStyleMask::NonactivatingPanel);
    window.setAnimationBehavior(NSWindowAnimationBehavior::None);
    window.setHidesOnDeactivate(false);
    window.setHasShadow(false);
    window.setExcludedFromWindowsMenu(true);
    window.setFrame_display(frame, false);
    if !focus && let Some(panel) = window.downcast_ref::<NSPanel>() {
        panel.setBecomesKeyOnlyIfNeeded(true);
    }
    Ok(handle)
}

fn toggle_drawer(cx: &mut App, pointer: NSPoint) {
    if let Some(overview) = cx
        .global::<MacDesktop>()
        .drawer
        .as_ref()
        .map(|drawer| drawer.overview)
    {
        let reopened = overview
            .update(cx, |panel, _, cx| panel.reopen(cx))
            .unwrap_or(false);
        if reopened {
            if let Some(drawer) = cx.global_mut::<MacDesktop>().drawer.as_mut() {
                drawer.restore_main = false;
            }
            if let Some(window) = drawer_native(cx, overview) {
                window.setIgnoresMouseEvents(false);
            }
            let _ = overview.update(cx, |_, window, _| window.activate_window());
            set_status_highlight(cx, true);
        } else {
            dismiss_drawer(cx);
        }
        return;
    }

    let Some(placement) = screen_placement(cx, pointer) else {
        show_window(cx);
        return;
    };
    match create_panel(cx, placement, placement.overview, PanelContent::Overview) {
        Ok(overview) => {
            let outside_clicks = OutsideClicks::new(cx, overview);
            cx.global_mut::<MacDesktop>().drawer = Some(Drawer {
                overview,
                detail: None,
                placement,
                restore_main: false,
                _outside_clicks: outside_clicks,
            });
            let _ = overview.update(cx, |_, window, _| window.activate_window());
            set_status_highlight(cx, true);
        }
        Err(error) => {
            eprintln!("Quick Bridge could not open its macOS menu bar panel: {error:#}");
            show_window(cx);
        }
    }
}

fn toggle_quota(cx: &mut App, account_key: String) {
    let Some(drawer) = cx.global::<MacDesktop>().drawer.as_ref() else {
        return;
    };
    let overview = drawer.overview;
    if !overview
        .read(cx)
        .is_ok_and(|panel| panel.closing().is_none())
    {
        return;
    }

    if let Some(detail) = &drawer.detail {
        let handle = detail.handle;
        let same_account = detail.account_key == account_key;
        let reopened = handle
            .update(cx, |panel, _, cx| panel.reopen(cx))
            .unwrap_or(false);
        if same_account && !reopened {
            dismiss_quota(cx);
            return;
        }
        let _ = handle.update(cx, |panel, _, cx| {
            panel.select_quota(account_key.clone(), cx)
        });
        if let Some(detail) = cx
            .global_mut::<MacDesktop>()
            .drawer
            .as_mut()
            .and_then(|drawer| drawer.detail.as_mut())
        {
            detail.account_key = account_key.clone();
        }
        if let Some(window) = drawer_native(cx, handle) {
            window.setIgnoresMouseEvents(false);
            window.orderFrontRegardless();
        }
        select_quota_tile(cx, overview, Some(account_key));
        return;
    }

    let placement = drawer.placement;
    let Some(frame) = placement.detail else {
        open_account_pool(cx);
        return;
    };
    match create_panel(
        cx,
        placement,
        frame,
        PanelContent::Quota(account_key.clone()),
    ) {
        Ok(handle) => {
            if let Some(drawer) = cx.global_mut::<MacDesktop>().drawer.as_mut() {
                drawer.detail = Some(DetailWindow {
                    handle,
                    account_key: account_key.clone(),
                });
            }
            if let Some(window) = drawer_native(cx, handle) {
                window.orderFrontRegardless();
            }
            select_quota_tile(cx, overview, Some(account_key));
        }
        Err(error) => {
            eprintln!("Quick Bridge could not open its macOS quota panel: {error:#}");
            open_account_pool(cx);
        }
    }
}

fn select_quota_tile(cx: &mut App, overview: WindowHandle<MenuPanel>, key: Option<String>) {
    let _ = overview.update(cx, |panel, _, cx| panel.select_tile(key, cx));
}

fn dismiss_quota(cx: &mut App) -> bool {
    let Some(drawer) = cx.global::<MacDesktop>().drawer.as_ref() else {
        return false;
    };
    let Some(detail) = &drawer.detail else {
        return false;
    };
    let (overview, handle) = (drawer.overview, detail.handle);
    select_quota_tile(cx, overview, None);
    begin_close(cx, handle)
}

fn dismiss_drawer(cx: &mut App) {
    if let Some(overview) = cx
        .global::<MacDesktop>()
        .drawer
        .as_ref()
        .map(|drawer| drawer.overview)
    {
        dismiss_quota(cx);
        begin_close(cx, overview);
        set_status_highlight(cx, false);
    }
}

fn begin_close(cx: &mut App, handle: WindowHandle<MenuPanel>) -> bool {
    let at = Instant::now();
    if !handle
        .update(cx, |panel, _, cx| panel.begin_close(at, cx))
        .unwrap_or(false)
    {
        return false;
    }
    if let Some(window) = drawer_native(cx, handle) {
        window.setIgnoresMouseEvents(true);
    }
    cx.spawn(async move |cx| {
        Timer::after(motion::EXIT_DURATION).await;
        let _ = cx.update(|cx| {
            // 重新打开会使旧计时器失效，退出动画负责的只有原窗口。
            if handle
                .read(cx)
                .is_ok_and(|panel| panel.closing() == Some(at))
            {
                finish_close(cx, handle);
            }
        });
    })
    .detach();
    true
}

fn finish_close(cx: &mut App, handle: WindowHandle<MenuPanel>) {
    let desktop = cx.global_mut::<MacDesktop>();
    let mut restore_main = false;
    if let Some(drawer) = desktop.drawer.as_mut() {
        if drawer.overview == handle {
            if let Some(drawer) = desktop.drawer.take() {
                restore_main = drawer.restore_main;
                if let Some(detail) = drawer.detail {
                    remove_window(cx, detail.handle);
                }
            }
        } else if drawer
            .detail
            .as_ref()
            .is_some_and(|detail| detail.handle == handle)
        {
            drawer.detail = None;
        }
    }
    remove_window(cx, handle);
    if restore_main {
        restore_main_window(cx);
    }
}

fn remove_window(cx: &mut App, handle: WindowHandle<MenuPanel>) {
    if let Some(window) = drawer_native(cx, handle) {
        window.orderOut(None);
    }
    let _ = handle.update(cx, |_, window, _| window.remove_window());
}

fn hide_focused_window(cx: &mut App) {
    let desktop = cx.global::<MacDesktop>();
    let main = desktop.main_window;
    let panels: Vec<_> = desktop.drawer.iter().flat_map(Drawer::windows).collect();
    if panels
        .into_iter()
        .any(|handle| drawer_native(cx, handle).is_some_and(|window| window.isKeyWindow()))
    {
        dismiss_drawer(cx);
    } else if let Some(main) = main {
        let _ = main.update(cx, |_, window, cx| hide_window(window, cx));
    }
}

pub(super) fn hide_window(window: &mut Window, cx: &mut App) {
    dismiss_drawer(cx);
    let has_recovery = cx
        .global::<MacDesktop>()
        .menu_bar
        .as_ref()
        .is_some_and(|bar| bar.item.isVisible());
    if has_recovery
        && let Some(mtm) = MainThreadMarker::new()
        && let Some(window) = native_window(window)
    {
        // 收起的是主窗口。菜单栏仍可独立弹出，网关与根视图继续存活。
        window.orderOut(None);
        NSApplication::sharedApplication(mtm)
            .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    } else {
        cx.hide();
    }
}

pub(in crate::gui) fn show_window(cx: &mut App) {
    if let Some(drawer) = cx
        .try_global::<MacDesktop>()
        .and_then(|desktop| desktop.drawer.as_ref())
    {
        let overview = drawer.overview;
        if overview.read(cx).is_ok() {
            if let Some(drawer) = cx.global_mut::<MacDesktop>().drawer.as_mut() {
                drawer.restore_main = true;
            }
            dismiss_drawer(cx);
            return;
        }
    }
    restore_main_window(cx);
}

fn restore_main_window(cx: &mut App) {
    let Some(handle) = cx
        .try_global::<MacDesktop>()
        .and_then(|desktop| desktop.main_window)
    else {
        return;
    };
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    if app.activationPolicy() == NSApplicationActivationPolicy::Accessory {
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    }
    app.unhideWithoutActivation();
    if let Ok(Some(window)) = handle.update(cx, |_, window, _| native_window(window))
        && window.isMiniaturized()
    {
        window.deminiaturize(None);
    }
    // 非激活面板退场后，先激活应用再由 GPUI 恢复主窗口的键盘焦点。
    cx.activate(true);
    let _ = handle.update(cx, |_, window, _| window.activate_window());
}

fn open_account_pool(cx: &mut App) {
    if let Some(handle) = cx.global::<MacDesktop>().main_window {
        let _ = handle.update(cx, |view, _, cx| {
            view.switch_tab(Tab::Accounts);
            cx.notify();
        });
    }
    show_window(cx);
}

pub(in crate::gui) fn prefers_reduced_motion() -> bool {
    NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

fn set_dock_icon(main_thread: MainThreadMarker) {
    let Some(icon) = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(APP_ICON)) else {
        eprintln!("Quick Bridge could not load the application icon");
        return;
    };
    // SAFETY: 主线程上向 AppKit 传入已校验的非空 NSImage。
    unsafe { NSApplication::sharedApplication(main_thread).setApplicationIconImage(Some(&icon)) }
}

#[cfg(test)]
mod tests {
    use super::{NSPoint, NSRect, NSSize, PanelPlacement, panel, point, px, size};

    #[test]
    fn menu_panels_anchor_to_the_display_and_keep_their_content_gap() {
        let work_area = NSRect::new(NSPoint::new(-1280.0, 23.0), NSSize::new(1280.0, 755.0));
        let screen = NSRect::new(NSPoint::new(-1280.0, 0.0), NSSize::new(1280.0, 800.0));
        let placement = PanelPlacement::within(screen, work_area, None);
        let overview = placement.overview;
        let detail = placement.detail.expect("space for the left-hand detail");
        let padding = f64::from(panel::SHADOW_INSET);
        assert_eq!(overview.max().x, work_area.max().x - 2.0);
        assert_eq!(overview.max().y, work_area.max().y - 2.0);
        assert_eq!(detail.max().y, overview.max().y);
        assert_eq!(
            overview.min().x + padding - (detail.max().x - padding),
            10.0
        );
        let overview_bounds = placement.initial_bounds(overview);
        let detail_bounds = placement.initial_bounds(detail);
        assert_eq!(overview_bounds.origin, point(px(893.0), px(24.0)));
        assert_eq!(detail_bounds.origin, point(px(571.0), px(24.0)));
        assert_eq!(detail_bounds.size, size(px(332.0), px(394.0)));
    }

    #[test]
    fn a_small_work_area_limits_height_and_leaves_the_companion_unplaced() {
        let work_area = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(539.0, 480.0));
        let placement = PanelPlacement::within(work_area, work_area, None);
        assert!(placement.detail.is_none());
        assert_eq!(placement.overview.size.height, 476.0);
        assert_eq!(placement.overview.min().y, 2.0);
    }
}
