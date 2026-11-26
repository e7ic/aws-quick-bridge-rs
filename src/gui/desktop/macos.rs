use gpui::{App, AsyncApp, Global, KeyBinding, Menu, MenuItem, actions};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSImage, NSMenu, NSMenuItem,
    NSSquareStatusItemLength, NSStatusBar, NSStatusItem, NSWorkspace,
};
use objc2_foundation::{NSData, NSSize, NSString};

const APP_ICON: &[u8] = include_bytes!("../../../assets/app-icon.png");
const MENU_BAR_ICON: &[u8] = include_bytes!("../../../assets/menu-bar-icon.png");
const OPEN_PANEL: &str = "打开面板";
const QUIT_APP: &str = "退出 Quick Bridge";

actions!(quick_bridge, [ShowWindow, HideWindow, QuitApplication]);

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "QuickBridgeStatusMenuTarget"]
    #[ivars = AsyncApp]
    struct StatusMenuTarget;

    impl StatusMenuTarget {
        #[unsafe(method(openPanel:))]
        fn open_panel(&self, _: &AnyObject) {
            self.ivars().spawn(async |cx| {
                let _ = cx.update(show_window);
            }).detach();
        }

        #[unsafe(method(quitBridge:))]
        fn quit_bridge(&self, _: &AnyObject) {
            self.ivars().spawn(async |cx| {
                let _ = cx.update(|cx| cx.quit());
            }).detach();
        }
    }
);

struct MenuBar {
    item: Retained<NSStatusItem>,
    // NSMenuItem 的 target 是弱引用，菜单存续期间由应用持有。
    _target: Retained<StatusMenuTarget>,
}

impl Global for MenuBar {}

pub(in crate::gui) fn configure_application(cx: &mut App) {
    if let Some(main_thread) = MainThreadMarker::new() {
        set_dock_icon(main_thread);
        install_menu_bar(cx, main_thread);
    }
    cx.on_action(|_: &ShowWindow, cx| show_window(cx));
    cx.on_action(|_: &HideWindow, cx| hide_window(cx));
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

fn install_menu_bar(cx: &mut App, main_thread: MainThreadMarker) {
    let bar = NSStatusBar::systemStatusBar();
    let item = bar.statusItemWithLength(NSSquareStatusItemLength);
    let Some(button) = item.button(main_thread) else {
        bar.removeStatusItem(&item);
        eprintln!("Quick Bridge could not create its macOS menu bar item");
        return;
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
    // SAFETY: 对已分配的主线程对象调用 NSObject 的 init。
    let target: Retained<StatusMenuTarget> = unsafe { msg_send![super(target), init] };
    let menu = NSMenu::new(main_thread);
    add_menu_action(&menu, &target, OPEN_PANEL, sel!(openPanel:));
    menu.addItem(&NSMenuItem::separatorItem(main_thread));
    add_menu_action(&menu, &target, QUIT_APP, sel!(quitBridge:));
    item.setMenu(Some(&menu));

    let item_on_quit = item.clone();
    cx.on_app_quit(move |_| {
        bar.removeStatusItem(&item_on_quit);
        std::future::ready(())
    })
    .detach();
    cx.set_global(MenuBar {
        item,
        _target: target,
    });
}

fn add_menu_action(menu: &NSMenu, target: &StatusMenuTarget, title: &str, action: Sel) {
    // SAFETY: 选择器对应 StatusMenuTarget 上的一参数方法，target 由 MenuBar 持有。
    unsafe {
        menu.addItemWithTitle_action_keyEquivalent(
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(""),
        )
        .setTarget(Some(target));
    }
}

pub(super) fn hide_window(cx: &mut App) {
    cx.hide();
    // 只有菜单栏可以恢复窗口时，才移除 Dock 的恢复入口。
    if cx
        .try_global::<MenuBar>()
        .is_some_and(|bar| bar.item.isVisible())
        && let Some(main_thread) = MainThreadMarker::new()
    {
        NSApplication::sharedApplication(main_thread)
            .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
}

pub(in crate::gui) fn show_window(cx: &mut App) {
    if let Some(main_thread) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(main_thread);
        if app.activationPolicy() == NSApplicationActivationPolicy::Accessory {
            app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        }
        app.unhideWithoutActivation();
        if let Some(window) = app.windows().iter().find(|window| window.isMiniaturized()) {
            window.deminiaturize(None);
        }
    }
    if let Some(handle) = cx.active_window().or_else(|| cx.windows().first().copied()) {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
    }
    cx.activate(true);
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
