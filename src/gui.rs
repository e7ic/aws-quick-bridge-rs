//! aws-quick-bridge-rs 原生 GPUI 桌面控制中心。
//! 采用 Ingot UI（Loopo）卡片和控件样式、Solar Bold SVG 矢量图标、
//! 随应用提供的 MiSans 字体与矢量 SVG Logo，支持骨架屏加载动画与跨平台（macOS / Windows）运行。

mod account_menu;
mod activity;
mod charts;
mod components;
mod desktop;
mod motion;
mod quota_history;
mod tables;
mod toasts;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod update;
mod usage;
mod view;

use crate::config::BridgeConfig;
use crate::gateway::ccswitch::detect_lan_ip;
use crate::gateway::{RunningServer, start_server};
use crate::login::start_browser_sign_in;
use crate::pool::{
    AccountHealth, AccountPool, AccountSnapshot, LogLevel, NoAvailableAccount, PoolSnapshot,
};
use crate::qbs::QuotaWindow;
use crate::upstream::{TurnInput, UpstreamSession, chat};
use anyhow::{Context as _, Result};
use chrono::Utc;
use gpui::{
    App, AppContext, Application, AssetSource, Bounds, ClipboardItem, Context, FocusHandle, Font,
    FontFallbacks, FontFeatures, ImageSource, ScrollHandle, SharedString, Timer, TitlebarOptions,
    Window, WindowAppearance, WindowBounds, WindowOptions, font, point, px, size,
};
use parking_lot::Mutex;
use quota_history::{QuotaHistory, QuotaHistoryStore};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;

const UI_FONT: &str = "MiSans";
const THEME_TRANSITION: Duration = Duration::from_millis(240);
static NUMERIC_FONT: OnceLock<Font> = OnceLock::new();

#[cfg(target_os = "macos")]
const MONO_FONT: &str = "Menlo";
#[cfg(target_os = "windows")]
const MONO_FONT: &str = "Consolas";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const MONO_FONT: &str = "monospace";

// 静态嵌入品牌 SVG Logo（浅色与深色双主题适配）。
static LOGO_LIGHT: &[u8] = include_bytes!("../assets/logo-light.svg");
static LOGO_DARK: &[u8] = include_bytes!("../assets/logo-dark.svg");
const LOGO_LIGHT_PATH: &str = "logo-light.svg";
const LOGO_DARK_PATH: &str = "logo-dark.svg";
const ACCOUNT_ACTIVE_GLOW_PATH: &str = "account-active-glow.png";
const ACCOUNT_ACTIVE_LABEL_PATH: &str = "account-active-label.svg";
const ACCOUNT_AVATAR_BACKDROP_PATH: &str = "account-avatar-backdrop.svg";
// 与 SVG 视口尺寸对齐，保证 GPUI 在 2x 高分屏下清晰栅格化侧边栏图标。
const SIDEBAR_LOGO_WIDTH: f32 = 67.0;
const SIDEBAR_LOGO_HEIGHT: f32 = 34.0;

fn logo_source(dark_mode: bool) -> ImageSource {
    let path = if dark_mode {
        LOGO_DARK_PATH
    } else {
        LOGO_LIGHT_PATH
    };
    // 裸文件名在 GPUI 字符串转图片时会被当作 URI 解析，必须显式包装为内嵌资源。
    ImageSource::Resource(gpui::Resource::Embedded(path.into()))
}

// 静态嵌入品牌 Logo、图标和卡片背景纹理。
const EMBEDDED_ASSETS: &[(&str, &[u8])] = &[
    (LOGO_LIGHT_PATH, LOGO_LIGHT),
    (LOGO_DARK_PATH, LOGO_DARK),
    (
        ACCOUNT_ACTIVE_GLOW_PATH,
        include_bytes!("../assets/account-active-glow.png"),
    ),
    (
        ACCOUNT_ACTIVE_LABEL_PATH,
        include_bytes!("../assets/account-active-label.svg"),
    ),
    (
        "client-logos/claude.svg",
        include_bytes!("../assets/client-logos/claude.svg"),
    ),
    (
        "client-logos/openai.svg",
        include_bytes!("../assets/client-logos/openai.svg"),
    ),
    (
        "client-logos/opencode.svg",
        include_bytes!("../assets/client-logos/opencode.svg"),
    ),
    (
        "client-logos/terminal.svg",
        include_bytes!("../assets/client-logos/terminal.svg"),
    ),
    (
        "icons/widget-2-bold.svg",
        include_bytes!("../assets/icons/widget-2-bold.svg"),
    ),
    (
        "icons/users-group-rounded-bold.svg",
        include_bytes!("../assets/icons/users-group-rounded-bold.svg"),
    ),
    (
        "icons/code-square-bold.svg",
        include_bytes!("../assets/icons/code-square-bold.svg"),
    ),
    (
        "icons/settings-bold.svg",
        include_bytes!("../assets/icons/settings-bold.svg"),
    ),
    (
        "icons/document-text-bold.svg",
        include_bytes!("../assets/icons/document-text-bold.svg"),
    ),
    (
        "icons/play-bold.svg",
        include_bytes!("../assets/icons/play-bold.svg"),
    ),
    (
        "icons/stop-bold.svg",
        include_bytes!("../assets/icons/stop-bold.svg"),
    ),
    (
        "icons/refresh-bold.svg",
        include_bytes!("../assets/icons/refresh-bold.svg"),
    ),
    (
        "icons/copy-bold.svg",
        include_bytes!("../assets/icons/copy-bold.svg"),
    ),
    (
        "icons/check-circle-bold.svg",
        include_bytes!("../assets/icons/check-circle-bold.svg"),
    ),
    (
        "icons/eye-bold.svg",
        include_bytes!("../assets/icons/eye-bold.svg"),
    ),
    (
        "icons/eye-closed-bold.svg",
        include_bytes!("../assets/icons/eye-closed-bold.svg"),
    ),
    (
        "icons/key-bold.svg",
        include_bytes!("../assets/icons/key-bold.svg"),
    ),
    (
        "icons/user-plus-bold.svg",
        include_bytes!("../assets/icons/user-plus-bold.svg"),
    ),
    (
        "icons/star-bold.svg",
        include_bytes!("../assets/icons/star-bold.svg"),
    ),
    (
        "icons/trash-bin-trash-bold.svg",
        include_bytes!("../assets/icons/trash-bin-trash-bold.svg"),
    ),
    (
        "icons/bolt-bold.svg",
        include_bytes!("../assets/icons/bolt-bold.svg"),
    ),
    (
        "icons/global-bold.svg",
        include_bytes!("../assets/icons/global-bold.svg"),
    ),
    (
        "icons/server-bold.svg",
        include_bytes!("../assets/icons/server-bold.svg"),
    ),
    (
        "icons/cpu-bolt-bold.svg",
        include_bytes!("../assets/icons/cpu-bolt-bold.svg"),
    ),
    (
        "icons/chart-square-bold.svg",
        include_bytes!("../assets/icons/chart-square-bold.svg"),
    ),
    (
        "icons/sun-2-bold.svg",
        include_bytes!("../assets/icons/sun-2-bold.svg"),
    ),
    (
        "icons/moon-bold.svg",
        include_bytes!("../assets/icons/moon-bold.svg"),
    ),
    (
        "icons/link-round-bold.svg",
        include_bytes!("../assets/icons/link-round-bold.svg"),
    ),
    (
        "icons/arrow-right-up-bold.svg",
        include_bytes!("../assets/icons/arrow-right-up-bold.svg"),
    ),
    (
        "icons/alt-arrow-down-bold.svg",
        include_bytes!("../assets/icons/alt-arrow-down-bold.svg"),
    ),
    (
        "icons/danger-circle-bold.svg",
        include_bytes!("../assets/icons/danger-circle-bold.svg"),
    ),
];

/// 内置静态资产源，供 GPUI 渲染品牌、图标和卡片纹理。
struct EmbeddedAssets;

impl AssetSource for EmbeddedAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if path == ACCOUNT_AVATAR_BACKDROP_PATH {
            return Ok(Some(Cow::Owned(
                components::account_avatar_backdrop_svg().into_bytes(),
            )));
        }
        Ok(EMBEDDED_ASSETS
            .iter()
            .find(|(k, _)| *k == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(EMBEDDED_ASSETS
            .iter()
            .map(|(name, _)| *name)
            .chain(std::iter::once(ACCOUNT_AVATAR_BACKDROP_PATH))
            .filter(|name| name.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}

#[cfg(macos_bundle_fonts)]
fn load_misans_fonts() -> Result<Vec<Cow<'static, [u8]>>> {
    use flate2::read::GzDecoder;
    use std::fs::File;
    use std::io::Read;

    let executable = std::env::current_exe().context("locate Quick Bridge executable")?;
    let contents = executable
        .parent()
        .and_then(std::path::Path::parent)
        .context("locate Quick Bridge bundle Contents directory")?;
    let fonts_dir = contents.join("Resources").join("fonts");

    ["MiSans-Regular.ttf.gz", "MiSans-Medium.ttf.gz"]
        .into_iter()
        .map(|name| {
            let path = fonts_dir.join(name);
            let file = File::open(&path)
                .with_context(|| format!("open bundled font {}", path.display()))?;
            let mut font = Vec::new();
            GzDecoder::new(file)
                .read_to_end(&mut font)
                .with_context(|| format!("decompress bundled font {}", path.display()))?;
            Ok(Cow::Owned(font))
        })
        .collect()
}

#[cfg(not(macos_bundle_fonts))]
fn load_misans_fonts() -> Result<Vec<Cow<'static, [u8]>>> {
    Ok(vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/MiSans-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/MiSans-Medium.ttf")),
    ])
}

fn configure_fonts(cx: &App, fonts: Vec<Cow<'static, [u8]>>) {
    let text_system = cx.text_system();
    if let Err(e) = text_system.add_fonts(fonts) {
        eprintln!("warning: failed to register MiSans fonts: {e}");
    }
    NUMERIC_FONT.get_or_init(|| {
        let families = text_system.all_font_names();
        let mut numbers = ui_font();
        numbers.features = FontFeatures(Arc::new(vec![("tnum".into(), 1)]));
        let family = ["DIN", "DIN Alternate", "Bahnschrift"]
            .into_iter()
            .find(|family| families.iter().any(|available| available == *family));
        if let Some(family) = family {
            numbers.family = family.into();
            let fallbacks = numbers.fallbacks.get_or_insert_with(FontFallbacks::default);
            Arc::make_mut(&mut fallbacks.0).insert(0, UI_FONT.to_owned());
        }
        numbers
    });
}

fn numeric_font() -> Font {
    NUMERIC_FONT.get().cloned().unwrap_or_else(ui_font)
}

fn ui_font() -> Font {
    let mut f = font(UI_FONT);
    #[cfg(target_os = "macos")]
    {
        f.fallbacks = Some(FontFallbacks::from_fonts(vec![
            "PingFang SC".to_string(),
            "Helvetica Neue".to_string(),
        ]));
    }
    #[cfg(target_os = "windows")]
    {
        f.fallbacks = Some(FontFallbacks::from_fonts(vec![
            "Microsoft YaHei UI".to_string(),
            "Segoe UI".to_string(),
        ]));
    }
    f
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Dashboard,
    Usage,
    Accounts,
    Clients,
    Settings,
    Logs,
}

impl Tab {
    const SIDEBAR_SECTIONS: [(&'static str, &'static [Self]); 2] = [
        (
            "工作区",
            &[Self::Dashboard, Self::Usage, Self::Accounts, Self::Clients],
        ),
        ("管理", &[Self::Settings, Self::Logs]),
    ];

    fn sidebar_position(self) -> usize {
        Self::SIDEBAR_SECTIONS
            .iter()
            .flat_map(|(_, tabs)| tabs.iter().copied())
            .position(|tab| tab == self)
            .expect("tab has a sidebar entry")
    }

    fn slide_direction_to(self, tab: Self) -> i8 {
        match tab.sidebar_position().cmp(&self.sidebar_position()) {
            std::cmp::Ordering::Greater => 1,
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
        }
    }

    fn index(self) -> usize {
        match self {
            Tab::Dashboard => 0,
            Tab::Usage => 5,
            Tab::Accounts => 1,
            Tab::Clients => 2,
            Tab::Settings => 3,
            Tab::Logs => 4,
        }
    }

    fn icon_path(self) -> &'static str {
        match self {
            Tab::Dashboard => "icons/widget-2-bold.svg",
            Tab::Usage => "icons/chart-square-bold.svg",
            Tab::Accounts => "icons/users-group-rounded-bold.svg",
            Tab::Clients => "icons/code-square-bold.svg",
            Tab::Settings => "icons/settings-bold.svg",
            Tab::Logs => "icons/document-text-bold.svg",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Tab::Dashboard => "概览",
            Tab::Usage => "用量统计",
            Tab::Accounts => "账号池",
            Tab::Clients => "客户端",
            Tab::Settings => "设置",
            Tab::Logs => "日志",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AccountFilter {
    All,
    Enabled,
    Disabled,
}

impl AccountFilter {
    fn matches(self, enabled: bool) -> bool {
        match self {
            Self::All => true,
            Self::Enabled => enabled,
            Self::Disabled => !enabled,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LogFilter {
    All,
    InfoOnly,
    IssuesOnly,
    ErrorsOnly,
}

impl LogFilter {
    fn matches(self, level: LogLevel) -> bool {
        match self {
            Self::All => true,
            Self::InfoOnly => matches!(level, LogLevel::Info),
            Self::IssuesOnly => matches!(level, LogLevel::Warn | LogLevel::Error),
            Self::ErrorsOnly => matches!(level, LogLevel::Error),
        }
    }
}

#[derive(Clone, Default)]
enum SignInState {
    #[default]
    Idle,
    Preparing,
    Waiting(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingAction {
    RotateKey,
    ClearLogs,
    Quit,
}

#[derive(Clone, Default)]
struct GuiShared {
    server_running: bool,
    gateway_busy: bool,
    #[cfg(target_os = "macos")]
    gateway_start_error: Option<(String, String)>,
    toasts: toasts::ToastQueue,
    sign_in: SignInState,
    sign_in_attempt: u64,
    probe_busy_model: Option<String>,
    probed_models: HashMap<String, String>,
    quota_loading_all: bool,
    quota_loading_keys: HashSet<String>,
    copied_feedback: Option<(String, Instant)>,
}

impl GuiShared {
    fn listener_editable(&self) -> bool {
        !self.server_running && !self.gateway_busy
    }

    fn gateway_button_content(&self) -> (&'static str, &'static str) {
        match (self.gateway_busy, self.server_running) {
            (true, false) => ("icons/refresh-bold.svg", "正在启动"),
            (true, true) => ("icons/refresh-bold.svg", "正在停止"),
            (false, true) => ("icons/stop-bold.svg", "停止网关"),
            (false, false) => ("icons/play-bold.svg", "启动网关"),
        }
    }

    fn queue_quota_batch(&mut self, snapshot: &PoolSnapshot) -> Vec<String> {
        if self.quota_loading_all {
            return Vec::new();
        }
        let keys: Vec<_> = snapshot
            .accounts
            .iter()
            .filter(|account| account.enabled)
            .filter(|account| self.quota_loading_keys.insert(account.key.clone()))
            .map(|account| account.key.clone())
            .collect();
        self.quota_loading_all = !keys.is_empty();
        keys
    }
}

struct BridgeApp {
    focus: FocusHandle,
    keyboard_focus: bool,
    rt: Arc<tokio::runtime::Runtime>,
    pool: Arc<AccountPool>,
    server: Arc<Mutex<Option<RunningServer>>>,
    pending_login: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    shared: Arc<Mutex<GuiShared>>,
    ui_rev: Arc<AtomicU64>,
    visible_state: GuiShared,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    updates: Option<update::Updates>,
    snapshot: PoolSnapshot,
    account_card_highlight: activity::AccountCardHighlight,
    account_activity: HashMap<String, activity::AccountActivity>,
    quota_history: Arc<QuotaHistoryStore>,
    visible_quota_history: Arc<QuotaHistory>,
    lan_ip: Option<String>,
    active_tab: Tab,
    page_scroll: [ScrollHandle; 6],
    tab_transition_seq: u64,
    tab_direction: i8,
    ccswitch_use_lan: bool,
    ccswitch_model: Option<&'static str>,
    desktop_export_pending: bool,
    desktop_link_generation: u64,
    desktop_link_waiting: bool,
    desktop_link_restorable: bool,
    show_full_key: bool,
    pending_action: Option<PendingAction>,
    dark_mode: bool,
    theme_transition: Option<(Instant, f32)>,
    activity_range: activity::ActivityRange,
    trend_metric: charts::TrendMetric,
    usage: usage::UsageView,
    protocol_chart: charts::ProtocolChart,
    account_filter: AccountFilter,
    account_page: usize,
    account_sort: tables::NumberSort,
    account_menu: Option<account_menu::AccountMenu>,
    log_filter: LogFilter,
    log_page: usize,
    log_sort: tables::NumberSort,
}

fn effective_dark_mode(config: &BridgeConfig, appearance: WindowAppearance) -> bool {
    config
        .options
        .get("ui_dark_mode")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(matches!(
            appearance,
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ))
}

impl BridgeApp {
    fn new(
        rt: Arc<tokio::runtime::Runtime>,
        pool: Arc<AccountPool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let snapshot = pool.snapshot();
        let dark_mode = effective_dark_mode(&snapshot.config, window.appearance());
        #[cfg(target_os = "windows")]
        {
            let darkness = f32::from(dark_mode);
            desktop::sync_window_caption(
                window,
                darkness,
                &components::Theme::interpolated(darkness),
            );
            cx.observe_window_appearance(window, |view, window, cx| {
                let dark_mode = effective_dark_mode(&view.snapshot.config, window.appearance());
                if dark_mode != view.dark_mode {
                    view.transition_to_theme(dark_mode);
                    cx.notify();
                }
            })
            .detach();
        }
        let mut account_activity = HashMap::new();
        activity::observe_accounts(&mut account_activity, &snapshot.accounts, Utc::now());
        let quota_history = Arc::new(QuotaHistoryStore::load(
            crate::config::home_dir().join("quota-history.json"),
        ));
        let visible_quota_history = quota_history.snapshot();
        let lan_ip = detect_lan_ip();
        let server = Arc::new(Mutex::new(None));
        let pending_login = Arc::new(Mutex::new(None));
        let ui_rev = Arc::new(AtomicU64::new(1));

        let mut init_shared = GuiShared::default();
        let initial_quota_keys = init_shared.queue_quota_batch(&snapshot);
        let shared = Arc::new(Mutex::new(init_shared.clone()));

        // 订阅多账号池变更事件，实时递增版本号驱动 UI 零延迟刷新。
        {
            let mut rx = pool.subscribe();
            let rev = ui_rev.clone();
            rt.spawn(async move {
                while let Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) =
                    rx.recv().await
                {
                    rev.fetch_add(1, Ordering::Release);
                }
            });
        }

        // 启动异步初始化拉取全部账号额度。
        if !initial_quota_keys.is_empty() {
            let pool_init = pool.clone();
            let history_init = quota_history.clone();
            let shared_init = shared.clone();
            let rev_init = ui_rev.clone();
            rt.spawn(async move {
                if let Err(error) = Self::refresh_quota_batch(
                    &pool_init,
                    &history_init,
                    &shared_init,
                    &rev_init,
                    initial_quota_keys,
                )
                .await
                {
                    Self::push_toast_shared(&shared_init, &rev_init, format!("{error:#}"), true);
                }
            });
        }

        // 高响应事件监测协程：空闲时零开销，变更时 32ms 内响应；仅在存在提示或冷却倒计时时按秒更新。
        motion::set_reduced_motion(desktop::prefers_reduced_motion());
        let rev_watch = ui_rev.clone();
        cx.spawn(async move |this, cx| {
            let mut last_rev = 0;
            let mut last_tick = Instant::now();
            let mut last_activity_minute = Utc::now().timestamp().div_euclid(60);
            loop {
                Timer::after(Duration::from_millis(32)).await;
                let cur_rev = rev_watch.load(Ordering::Acquire);
                let now = Instant::now();
                let second_elapsed = now.duration_since(last_tick) >= Duration::from_secs(1);
                let ok = this.update(cx, |view, cx| {
                    #[cfg(target_os = "windows")]
                    if desktop::take_quit_request() {
                        view.request_quit(cx);
                    }
                    #[cfg(any(target_os = "macos", target_os = "windows"))]
                    view.start_waiting_update(cx);
                    let mut dirty = cur_rev != last_rev || view.account_card_highlight.expired(now);
                    if view
                        .account_menu
                        .as_ref()
                        .and_then(|menu| menu.closing)
                        .is_some_and(|at| {
                            now.saturating_duration_since(at) >= motion::EXIT_DURATION
                        })
                    {
                        view.account_menu = None;
                        dirty = true;
                    }
                    if view
                        .usage
                        .dropdown
                        .as_ref()
                        .and_then(|dropdown| dropdown.closing)
                        .is_some_and(|at| {
                            now.saturating_duration_since(at) >= motion::EXIT_DURATION
                        })
                    {
                        view.usage.dropdown = None;
                        dirty = true;
                    }
                    let mut st = view.shared.lock();
                    dirty |= st.toasts.advance(now);
                    if let Some((_, at)) = &st.copied_feedback
                        && at.elapsed() > Duration::from_millis(1500)
                    {
                        st.copied_feedback = None;
                        dirty = true;
                    }
                    if second_elapsed {
                        let reduced = desktop::prefers_reduced_motion();
                        if reduced != motion::is_reduced_motion() {
                            motion::set_reduced_motion(reduced);
                            dirty = true;
                        }
                        last_tick = now;
                        let activity_minute = Utc::now().timestamp().div_euclid(60);
                        let has_cooldown = view
                            .snapshot
                            .accounts
                            .iter()
                            .any(|a| matches!(a.health, AccountHealth::CoolingDown { .. }));
                        let activity_changed = match view.active_tab {
                            Tab::Dashboard => {
                                activity_minute != last_activity_minute
                                    && !view.pool.request_samples().is_empty()
                            }
                            Tab::Usage => activity_minute != last_activity_minute,
                            Tab::Accounts => {
                                activity_minute.div_euclid(60)
                                    != last_activity_minute.div_euclid(60)
                            }
                            _ => false,
                        };
                        last_activity_minute = activity_minute;
                        dirty |= has_cooldown || activity_changed;
                    }
                    if dirty {
                        last_rev = cur_rev;
                        // 先采样加载状态再读取账号池快照，避免骨架屏消失瞬间闪现旧数据。
                        view.visible_state = st.clone();
                        drop(st);
                        view.visible_quota_history = view.quota_history.snapshot();
                        let snapshot = view.pool.snapshot();
                        view.account_card_highlight
                            .observe(&view.snapshot, &snapshot, now);
                        view.snapshot = snapshot;
                        activity::observe_accounts(
                            &mut view.account_activity,
                            &view.snapshot.accounts,
                            Utc::now(),
                        );
                        cx.notify();
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            focus: cx.focus_handle(),
            keyboard_focus: false,
            rt,
            pool,
            server,
            pending_login,
            shared,
            ui_rev,
            visible_state: init_shared,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            updates: update::Updates::for_current_install(),
            snapshot,
            account_card_highlight: activity::AccountCardHighlight::default(),
            account_activity,
            quota_history,
            visible_quota_history,
            lan_ip,
            active_tab: Tab::Dashboard,
            page_scroll: std::array::from_fn(|_| ScrollHandle::new()),
            tab_transition_seq: 0,
            tab_direction: 0,
            ccswitch_use_lan: false,
            ccswitch_model: None,
            desktop_export_pending: false,
            desktop_link_generation: 0,
            desktop_link_waiting: false,
            desktop_link_restorable: desktop::claude_link::has_restore_point(),
            show_full_key: false,
            pending_action: None,
            dark_mode,
            theme_transition: None,
            activity_range: activity::ActivityRange::default(),
            trend_metric: charts::TrendMetric::default(),
            usage: usage::UsageView::default(),
            protocol_chart: charts::ProtocolChart::default(),
            account_filter: AccountFilter::All,
            account_page: 0,
            account_sort: tables::NumberSort::Default,
            account_menu: None,
            log_filter: LogFilter::All,
            log_page: 0,
            log_sort: tables::NumberSort::Default,
        }
    }

    fn bump_ui(&self) {
        self.ui_rev.fetch_add(1, Ordering::Release);
    }

    fn request_quit(&mut self, cx: &mut Context<Self>) {
        if self.pending_action == Some(PendingAction::Quit) {
            self.pending_action = None;
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if self.install_update_on_quit(cx) {
            return;
        }
        cx.quit();
    }

    fn theme_amount(&self, now: Instant) -> f32 {
        let target = f32::from(self.dark_mode);
        match self.theme_transition {
            Some((start, from)) if !motion::is_reduced_motion() => {
                let progress = (now.saturating_duration_since(start).as_secs_f32()
                    / THEME_TRANSITION.as_secs_f32())
                .min(1.0);
                let eased = progress * progress * (3.0 - 2.0 * progress);
                from + (target - from) * eased
            }
            _ => target,
        }
    }

    fn transition_to_theme(&mut self, dark_mode: bool) {
        let now = Instant::now();
        self.theme_transition = Some((now, self.theme_amount(now)));
        self.dark_mode = dark_mode;
    }

    fn toggle_theme(&mut self) {
        self.transition_to_theme(!self.dark_mode);
        self.set_option("ui_dark_mode", serde_json::Value::Bool(self.dark_mode));
    }

    fn switch_tab(&mut self, tab: Tab) {
        if self.active_tab != tab {
            self.account_menu = None;
            self.usage.dropdown = None;
            self.pending_action = None;
            self.show_full_key = false;
            if tab == Tab::Clients {
                self.desktop_link_restorable = desktop::claude_link::has_restore_point();
            }
            if self.lan_enabled() && matches!(tab, Tab::Dashboard | Tab::Clients) {
                self.lan_ip = detect_lan_ip();
            }
            self.tab_direction = self.active_tab.slide_direction_to(tab);
            self.active_tab = tab;
            self.tab_transition_seq = self.tab_transition_seq.wrapping_add(1);
        }
    }

    fn push_toast_shared(
        shared: &Mutex<GuiShared>,
        rev: &AtomicU64,
        msg: impl Into<String>,
        is_error: bool,
    ) {
        shared
            .lock()
            .toasts
            .push(msg.into(), None, is_error, Instant::now());
        rev.fetch_add(1, Ordering::Release);
    }

    fn set_toast(&self, msg: impl Into<String>, is_error: bool) {
        Self::push_toast_shared(&self.shared, &self.ui_rev, msg, is_error);
    }

    fn copy_with_feedback(&mut self, feedback_id: &str, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        let feedback = (feedback_id.to_string(), Instant::now());
        self.shared.lock().copied_feedback = Some(feedback.clone());
        self.visible_state.copied_feedback = Some(feedback);
        self.bump_ui();
    }

    fn is_copied(shared: &GuiShared, feedback_id: &str) -> bool {
        shared
            .copied_feedback
            .as_ref()
            .map(|(id, at)| id == feedback_id && at.elapsed() <= Duration::from_millis(1500))
            .unwrap_or(false)
    }

    fn start_gateway(&self) {
        {
            let mut st = self.shared.lock();
            if st.server_running || st.gateway_busy {
                return;
            }
            st.gateway_busy = true;
            #[cfg(target_os = "macos")]
            {
                st.gateway_start_error = None;
            }
        }
        self.bump_ui();
        let cfg = self.pool.config();
        let host = cfg.host.clone();
        let port = cfg.port;
        let pool = self.pool.clone();
        let server_slot = self.server.clone();
        let shared = self.shared.clone();
        let rev = self.ui_rev.clone();
        self.rt.spawn(async move {
            if server_slot.lock().is_some() {
                shared.lock().gateway_busy = false;
                rev.fetch_add(1, Ordering::Release);
                return;
            }
            match start_server(pool, &host, port).await {
                Ok(running) => {
                    let addr = if running.addr.ip().is_unspecified() {
                        format!("http://127.0.0.1:{}", running.addr.port())
                    } else {
                        format!("http://{}", running.addr)
                    };
                    *server_slot.lock() = Some(running);
                    {
                        let mut st = shared.lock();
                        st.server_running = true;
                        st.gateway_busy = false;
                        st.toasts
                            .push("网关已启动".into(), Some(addr), false, Instant::now());
                    }
                    rev.fetch_add(1, Ordering::Release);
                }
                Err(e) => {
                    let (title, detail) = toasts::gateway_start_failure(port, &e);
                    {
                        let mut st = shared.lock();
                        st.gateway_busy = false;
                        #[cfg(target_os = "macos")]
                        {
                            st.gateway_start_error = Some((title.clone(), detail.clone()));
                        }
                        st.toasts.push(title, Some(detail), true, Instant::now());
                    }
                    rev.fetch_add(1, Ordering::Release);
                }
            }
        });
    }

    fn stop_gateway(&self) {
        let server = {
            let mut state = self.shared.lock();
            if !state.server_running || state.gateway_busy {
                return;
            }
            let Some(server) = self.server.lock().take() else {
                return;
            };
            state.gateway_busy = true;
            server
        };
        self.bump_ui();
        let shared = self.shared.clone();
        let rev = self.ui_rev.clone();
        let pool = self.pool.clone();
        self.rt.spawn(async move {
            server.stop_and_wait().await;
            {
                let mut state = shared.lock();
                state.server_running = false;
                state.gateway_busy = false;
            }
            Self::push_toast_shared(&shared, &rev, "网关已停止", false);
            pool.log_info("Gateway stopped");
        });
    }

    fn set_lan_bind(&mut self, enable_lan: bool) {
        if !self.shared.lock().listener_editable() {
            return;
        }
        let mut cfg = self.pool.config();
        let next_host = if enable_lan { "0.0.0.0" } else { "127.0.0.1" };
        if cfg.host == next_host {
            return;
        }
        cfg.host = next_host.to_string();
        if let Err(e) = self.pool.update_config(cfg.clone()) {
            self.set_toast(format!("保存失败：{e}"), true);
            return;
        }
        self.snapshot.config = cfg;
        #[cfg(target_os = "macos")]
        {
            self.shared.lock().gateway_start_error = None;
        }
        if enable_lan {
            self.lan_ip = detect_lan_ip();
        } else {
            self.ccswitch_use_lan = false;
        }
    }

    fn set_port(&mut self, port: u16) {
        if !self.shared.lock().listener_editable() {
            return;
        }
        let mut cfg = self.pool.config();
        if cfg.port == port {
            return;
        }
        cfg.port = port;
        if let Err(e) = self.pool.update_config(cfg.clone()) {
            self.set_toast(format!("保存失败：{e}"), true);
            return;
        }
        self.snapshot.config = cfg;
        #[cfg(target_os = "macos")]
        {
            self.shared.lock().gateway_start_error = None;
        }
    }

    fn set_option(&mut self, key: &str, value: serde_json::Value) {
        let mut cfg = self.pool.config();
        cfg.options.insert(key.to_string(), value);
        match self.pool.update_config(cfg.clone()) {
            Ok(()) => self.snapshot.config = cfg,
            Err(e) => self.set_toast(format!("保存失败：{e}"), true),
        }
    }

    async fn refresh_account_quota(
        pool: &AccountPool,
        history: &Arc<QuotaHistoryStore>,
        key: &str,
    ) -> Result<()> {
        let report = pool.refresh_quota(key).await?;
        let at = Utc::now();
        let history = history.clone();
        let key = key.to_owned();
        tokio::task::spawn_blocking(move || history.record(key, at, report))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
            .context("额度已更新，保存历史失败")
    }

    async fn refresh_quota_batch(
        pool: &AccountPool,
        history: &Arc<QuotaHistoryStore>,
        shared: &Mutex<GuiShared>,
        rev: &AtomicU64,
        keys: Vec<String>,
    ) -> Result<()> {
        let mut result = Ok(());
        for key in keys {
            if let Err(error) = Self::refresh_account_quota(pool, history, &key).await {
                result = Err(error);
            }
            shared.lock().quota_loading_keys.remove(&key);
            rev.fetch_add(1, Ordering::Release);
        }
        shared.lock().quota_loading_all = false;
        rev.fetch_add(1, Ordering::Release);
        result
    }

    fn refresh_all_quotas_ui(&self) {
        let keys = self.shared.lock().queue_quota_batch(&self.snapshot);
        if keys.is_empty() {
            return;
        }

        self.bump_ui();
        let pool = self.pool.clone();
        let history = self.quota_history.clone();
        let shared = self.shared.clone();
        let rev = self.ui_rev.clone();
        self.rt.spawn(async move {
            match Self::refresh_quota_batch(&pool, &history, &shared, &rev, keys).await {
                Ok(()) => Self::push_toast_shared(&shared, &rev, "额度已刷新", false),
                Err(error) => Self::push_toast_shared(&shared, &rev, format!("{error:#}"), true),
            }
        });
    }

    fn refresh_one_quota_ui(&self, key: String) {
        {
            let mut st = self.shared.lock();
            if !st.quota_loading_keys.insert(key.clone()) {
                return;
            }
        }
        self.bump_ui();
        let pool = self.pool.clone();
        let history = self.quota_history.clone();
        let shared = self.shared.clone();
        let rev = self.ui_rev.clone();
        self.rt.spawn(async move {
            let res = Self::refresh_account_quota(&pool, &history, &key).await;
            {
                let mut st = shared.lock();
                st.quota_loading_keys.remove(&key);
            }
            match res {
                Ok(_) => Self::push_toast_shared(&shared, &rev, "额度已更新", false),
                Err(error) => Self::push_toast_shared(&shared, &rev, format!("{error:#}"), true),
            }
        });
    }

    fn cancel_login(&self) {
        let shutdown = {
            let mut st = self.shared.lock();
            if matches!(st.sign_in, SignInState::Idle) {
                return;
            }
            st.sign_in = SignInState::Idle;
            self.pending_login.lock().take()
        };
        if let Some(shutdown) = shutdown {
            let _ = shutdown.send(());
        }
        self.set_toast("已取消登录", false);
    }

    fn start_login(&self) {
        let attempt = {
            let mut st = self.shared.lock();
            if !matches!(st.sign_in, SignInState::Idle) {
                return;
            }
            st.sign_in = SignInState::Preparing;
            st.sign_in_attempt = st.sign_in_attempt.wrapping_add(1);
            st.sign_in_attempt
        };
        self.bump_ui();
        let pool = self.pool.clone();
        let history = self.quota_history.clone();
        let pending_slot = self.pending_login.clone();
        let shared = self.shared.clone();
        let rev = self.ui_rev.clone();
        self.rt.spawn(async move {
            let cfg = pool.settings();
            let mut handle = match start_browser_sign_in(&cfg).await {
                Ok(handle) => handle,
                Err(error) => {
                    let mut st = shared.lock();
                    if st.sign_in_attempt != attempt
                        || !matches!(st.sign_in, SignInState::Preparing)
                    {
                        return;
                    }
                    st.sign_in = SignInState::Idle;
                    drop(st);
                    Self::push_toast_shared(&shared, &rev, error.to_string(), true);
                    return;
                }
            };
            let url = handle.start_url.clone();
            {
                let mut st = shared.lock();
                if st.sign_in_attempt != attempt || !matches!(st.sign_in, SignInState::Preparing) {
                    if let Some(shutdown) = handle.take_shutdown() {
                        let _ = shutdown.send(());
                    }
                    return;
                }
                *pending_slot.lock() = handle.take_shutdown();
                st.sign_in = SignInState::Waiting(url.clone());
            }
            rev.fetch_add(1, Ordering::Release);
            if open::that(&url).is_err() {
                Self::push_toast_shared(&shared, &rev, "无法打开浏览器，请复制授权链接", true);
            }
            let outcome = handle.wait().await;
            {
                let mut st = shared.lock();
                if st.sign_in_attempt != attempt || !matches!(st.sign_in, SignInState::Waiting(_)) {
                    return;
                }
                let _ = pending_slot.lock().take();
                st.sign_in = SignInState::Idle;
            }

            let record = match outcome {
                Ok(record) => record,
                Err(error) => {
                    Self::push_toast_shared(&shared, &rev, error.to_string(), true);
                    return;
                }
            };
            let label = record.display_label();
            let key = record.key();
            let quota_reserved = shared.lock().quota_loading_keys.insert(key.clone());
            if let Err(error) = pool.add_or_update_account(record) {
                if quota_reserved {
                    shared.lock().quota_loading_keys.remove(&key);
                }
                Self::push_toast_shared(&shared, &rev, error.to_string(), true);
                return;
            }
            Self::push_toast_shared(&shared, &rev, format!("已登录 {label}"), false);
            pool.log_info(format!("Signed in {label}"));
            if quota_reserved {
                let result = Self::refresh_account_quota(&pool, &history, &key).await;
                shared.lock().quota_loading_keys.remove(&key);
                rev.fetch_add(1, Ordering::Release);
                if let Err(error) = result {
                    Self::push_toast_shared(&shared, &rev, format!("{error:#}"), true);
                }
            }
        });
    }

    fn run_probe(&self, model: &str) {
        {
            let mut st = self.shared.lock();
            if st.probe_busy_model.is_some() {
                return;
            }
            st.probe_busy_model = Some(model.to_string());
        }
        self.bump_ui();
        let model = model.to_string();
        let pool = self.pool.clone();
        let shared = self.shared.clone();
        let rev = self.ui_rev.clone();
        self.rt.spawn(async move {
            let outcome = async {
                let (picked, request) = pool.reserve_request("", &HashSet::new(), None).map_err(
                    |error| match error {
                        NoAvailableAccount::Busy => anyhow::anyhow!("可用账号正忙，请稍后重试"),
                        NoAvailableAccount::Unavailable => anyhow::anyhow!("暂无可用账号"),
                    },
                )?;
                let key = picked.key();
                let label = picked.display_label();
                let auth = pool
                    .ensure_fresh(&key, false)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let cfg = pool.settings();
                let session = UpstreamSession::from_auth(&auth, &cfg);
                let (_tx, cancel_rx) = watch::channel(false);
                let input = TurnInput {
                    prompt: "hi".to_string(),
                    model: model.clone(),
                    thread_id: String::new(),
                    title: "probe".to_string(),
                    system_prompt: String::new(),
                    reminder: String::new(),
                    thinking_effort: String::new(),
                    web_search: false,
                    resume: false,
                    attachments: Vec::new(),
                };
                let t0 = Instant::now();
                let res = chat(&session, &cfg, &input, cancel_rx, None)
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e.message))?;
                let resolved = if res.usage.resolved_model_id.is_empty() {
                    "(unknown)".to_string()
                } else {
                    res.usage.resolved_model_id.clone()
                };
                request.finish(&resolved);
                pool.push_log(crate::pool::RequestLogEntry {
                    timestamp: Utc::now(),
                    level: LogLevel::Info,
                    api: "probe".to_string(),
                    requested_model: model.clone(),
                    resolved_model: resolved.clone(),
                    account: label,
                    status: 200,
                    latency_ms: t0.elapsed().as_millis() as u64,
                    input_tokens: res.usage.prompt,
                    output_tokens: res.usage.completion,
                    cached_tokens: res.usage.cache_read,
                    context: res.usage.context,
                    message: format!("{model} -> {resolved}"),
                });
                Ok::<String, anyhow::Error>(resolved)
            }
            .await;

            {
                let mut st = shared.lock();
                st.probe_busy_model = None;
                if let Ok(ref resolved) = outcome {
                    st.probed_models.insert(model.clone(), resolved.clone());
                }
            }
            match outcome {
                Ok(resolved) => Self::push_toast_shared(&shared, &rev, resolved, false),
                Err(e) => Self::push_toast_shared(&shared, &rev, format!("{e}"), true),
            }
        });
    }

    fn masked_key(&self) -> String {
        let k = &self.snapshot.config.api_key;
        if self.show_full_key {
            return k.clone();
        }
        let chars: Vec<_> = k.chars().collect();
        if chars.len() <= 20 {
            return "••••••••".into();
        }
        let prefix: String = chars[..12].iter().collect();
        let suffix: String = chars[chars.len() - 6..].iter().collect();
        format!("{prefix}••••••{suffix}")
    }

    fn base_url(&self, lan: bool) -> String {
        let port = self.snapshot.config.port;
        let host = if lan { self.available_lan_ip() } else { None };
        format!("http://{}:{port}", host.unwrap_or("127.0.0.1"))
    }

    fn available_account_count(&self) -> usize {
        self.snapshot
            .accounts
            .iter()
            .filter(|account| account.enabled && matches!(account.health, AccountHealth::Healthy))
            .count()
    }

    fn account_quota_windows<'a>(&'a self, account: &'a AccountSnapshot) -> &'a [QuotaWindow] {
        self.visible_quota_history
            .readings(&account.key)
            .last()
            .map(|reading| reading.windows.as_slice())
            .or_else(|| {
                account
                    .quota
                    .as_ref()
                    .map(|report| report.windows.as_slice())
            })
            .unwrap_or_default()
    }

    fn lan_enabled(&self) -> bool {
        self.snapshot.config.host == "0.0.0.0"
    }

    fn available_lan_ip(&self) -> Option<&str> {
        self.lan_enabled()
            .then_some(self.lan_ip.as_deref())
            .flatten()
    }

    fn client_default_model(&self) -> &'static str {
        match self.pool.settings().default_model.as_str() {
            "balanced" => "balanced",
            "fast" => "fast",
            "auto" => "auto",
            _ => "smart",
        }
    }

    fn link_claude_desktop(&mut self, restore: bool, cx: &mut Context<Self>) {
        use desktop::claude_link;

        self.desktop_link_generation = self.desktop_link_generation.wrapping_add(1);
        if self.desktop_link_waiting {
            self.desktop_link_waiting = false;
            self.set_toast("已取消接入变更", false);
            return;
        }
        if let Err(error) = claude_link::check_location() {
            self.set_toast(error.to_string(), true);
            return;
        }
        let generation = self.desktop_link_generation;
        self.desktop_link_waiting = true;
        cx.spawn(async move |this, cx| {
            let mut notified = false;
            let mut gateway_requested = false;
            loop {
                let running = claude_link::is_running();
                let waiting_for_desktop = matches!(running, Ok(true));
                let done = this
                    .update(cx, |view, cx| {
                        if view.desktop_link_generation != generation {
                            return true;
                        }
                        if waiting_for_desktop {
                            if !notified {
                                notified = true;
                                view.set_toast("Claude Desktop 完全退出后会自动应用", false);
                            }
                            return false;
                        }

                        if !restore && running.is_ok() {
                            let (gateway_running, busy) = {
                                let state = view.shared.lock();
                                (state.server_running, state.gateway_busy)
                            };
                            if busy {
                                gateway_requested |= !gateway_running;
                                return false;
                            }
                            if !gateway_running {
                                if gateway_requested {
                                    view.desktop_link_waiting = false;
                                    cx.notify();
                                    return true;
                                }
                                gateway_requested = true;
                                view.start_gateway();
                                return false;
                            }
                        }

                        let result = running.and_then(|_| {
                            if restore {
                                claude_link::restore()
                            } else {
                                let address =
                                    view.server.lock().as_ref().context("网关还未启动")?.addr;
                                let base = if address.ip().is_unspecified() {
                                    let host = if address.is_ipv4() {
                                        "127.0.0.1"
                                    } else {
                                        "[::1]"
                                    };
                                    format!("http://{host}:{}", address.port())
                                } else {
                                    format!("http://{address}")
                                };
                                let endpoint = crate::gateway::claude_desktop::endpoint(&base);
                                let profile = crate::gateway::claude_desktop::native_configuration(
                                    &endpoint,
                                    &view.pool.config().api_key,
                                    view.client_default_model(),
                                );
                                claude_link::apply(&profile)
                            }
                        });

                        view.desktop_link_waiting = false;
                        view.desktop_link_restorable = claude_link::has_restore_point();
                        match result {
                            Ok(()) => {
                                view.shared.lock().toasts.push(
                                    if restore {
                                        "已恢复原接入"
                                    } else {
                                        "已写入直连配置"
                                    }
                                    .into(),
                                    Some("打开 Claude Desktop 后生效。".into()),
                                    false,
                                    Instant::now(),
                                );
                                view.bump_ui();
                            }
                            Err(error) => view.set_toast(format!("接入变更失败：{error}"), true),
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(true);
                if done {
                    break;
                }
                let interval = if waiting_for_desktop { 750 } else { 100 };
                Timer::after(Duration::from_millis(interval)).await;
            }
        })
        .detach();
    }

    fn export_desktop_config(&mut self, lan: bool, cx: &mut Context<Self>) {
        if self.desktop_export_pending {
            return;
        }
        self.desktop_export_pending = true;
        let directory = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(crate::config::home_dir);
        let path = cx.prompt_for_new_path(&directory, Some("Quick-Bridge-Claude-Desktop.json"));
        cx.spawn(async move |this, cx| {
            let selection = path.await;
            let _ = this.update(cx, |view, cx| {
                view.desktop_export_pending = false;
                let result = (|| -> Result<bool> {
                    let Some(path) = selection?? else {
                        return Ok(false);
                    };
                    let endpoint = crate::gateway::claude_desktop::endpoint(&view.base_url(lan));
                    crate::gateway::claude_desktop::save_native_configuration(
                        &path,
                        &endpoint,
                        &view.snapshot.config.api_key,
                        view.client_default_model(),
                    )?;
                    Ok(true)
                })();
                match result {
                    Ok(true) => {
                        view.shared.lock().toasts.push(
                            "已导出直连配置".into(),
                            Some("在 Claude Desktop 的第三方推理设置中导入。".into()),
                            false,
                            Instant::now(),
                        );
                        view.bump_ui();
                    }
                    Ok(false) => {}
                    Err(error) => view.set_toast(format!("导出失败：{error}"), true),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// 启动原生 GPUI 桌面窗口并注册 SVG 资产源与 MiSans 字体。
pub fn run_gui() -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let Some(startup) = update::startup()? else {
        return Ok(());
    };
    let fonts = load_misans_fonts()?;
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?,
    );
    let pool = AccountPool::load()?;

    let application = Application::new().with_assets(EmbeddedAssets);
    #[cfg(target_os = "macos")]
    application.on_reopen(desktop::show_window);
    application.run(move |cx: &mut App| {
        configure_fonts(cx, fonts);
        #[cfg(target_os = "macos")]
        desktop::configure_application(cx);
        let pool_on_quit = pool.clone();
        cx.on_app_quit(move |_| {
            pool_on_quit.flush_request_stats();
            std::future::ready(())
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1120.0), px(730.0)), cx);
        let rt_clone = rt.clone();
        let pool_clone = pool.clone();
        let titlebar = TitlebarOptions {
            title: Some("Quick Bridge".into()),
            appears_transparent: cfg!(target_os = "macos"),
            traffic_light_position: if cfg!(target_os = "macos") {
                Some(point(px(16.0), px(16.0)))
            } else {
                None
            },
        };
        let _main_window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(920.0), px(620.0))),
                    titlebar: Some(titlebar),
                    ..Default::default()
                },
                move |window, cx| {
                    desktop::keep_window_in_background(window, cx);
                    cx.new(|cx| {
                        let view = BridgeApp::new(rt_clone, pool_clone, window, cx);
                        window.focus(&view.focus);
                        view
                    })
                },
            )
            .expect("failed to open GPUI window");
        #[cfg(target_os = "macos")]
        desktop::set_main_window(cx, _main_window);
        cx.activate(true);
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            if let Err(error) = startup.acknowledge() {
                eprintln!("Quick Bridge could not acknowledge the update: {error:#}");
                cx.quit();
                return;
            }
            let _ = _main_window.update(cx, |view, _, cx| view.initialize_updates(&startup, cx));
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_theme_follows_system_until_the_user_chooses_one() {
        let mut config = BridgeConfig::default();
        for appearance in [WindowAppearance::Light, WindowAppearance::VibrantLight] {
            assert!(!effective_dark_mode(&config, appearance));
        }
        for appearance in [WindowAppearance::Dark, WindowAppearance::VibrantDark] {
            assert!(effective_dark_mode(&config, appearance));
        }
        for saved in [true, false] {
            config.options.insert("ui_dark_mode".into(), saved.into());
            let restored = serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
            for appearance in [WindowAppearance::Light, WindowAppearance::Dark] {
                assert_eq!(effective_dark_mode(&restored, appearance), saved);
            }
        }
    }

    #[test]
    fn page_slide_follows_sidebar_order() {
        assert_eq!(Tab::Dashboard.slide_direction_to(Tab::Usage), 1);
        assert_eq!(Tab::Usage.slide_direction_to(Tab::Accounts), 1);
        assert_eq!(Tab::Accounts.slide_direction_to(Tab::Usage), -1);
        assert_eq!(Tab::Clients.slide_direction_to(Tab::Settings), 1);
    }

    #[test]
    fn sidebar_logo_uses_display_sized_embedded_assets_in_both_themes() {
        for (dark_mode, expected) in [(false, "logo-light.svg"), (true, "logo-dark.svg")] {
            let ImageSource::Resource(gpui::Resource::Embedded(path)) = logo_source(dark_mode)
            else {
                panic!("{expected} must load from the embedded asset source");
            };
            assert_eq!(path.as_ref(), expected);
            let bytes = EmbeddedAssets
                .load(&path)
                .unwrap()
                .expect("missing embedded logo");
            let svg = std::str::from_utf8(&bytes).expect("logo must be a UTF-8 SVG");
            let (root, _) = svg.split_once('>').expect("missing SVG root tag");
            assert!(
                root.contains(&format!(r#" width="{SIDEBAR_LOGO_WIDTH}""#))
                    && root.contains(&format!(r#" height="{SIDEBAR_LOGO_HEIGHT}""#)),
                "{path} must declare the sidebar display size",
            );
        }
    }

    #[test]
    fn test_embedded_assets_valid() {
        let store = EmbeddedAssets;
        let paths = store.list("").unwrap();
        assert_eq!(paths.len(), 36);
        for path in paths {
            let bytes = store.load(&path).unwrap().expect("missing embedded asset");
            if path.as_ref() == ACCOUNT_ACTIVE_GLOW_PATH {
                assert!(
                    bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                    "invalid PNG header in {path}"
                );
            } else {
                let text = std::str::from_utf8(&bytes).expect("svg must be utf-8");
                assert!(text.contains("<svg"), "invalid svg header in {path}");
            }
        }
    }

    #[cfg(not(macos_bundle_fonts))]
    #[test]
    fn test_embedded_misans_fonts_weights() {
        fn read_os2_weight(ttf: &[u8]) -> u16 {
            let num_tables = u16::from_be_bytes([ttf[4], ttf[5]]) as usize;
            for idx in 0..num_tables {
                let rec = 12 + idx * 16;
                if &ttf[rec..rec + 4] == b"OS/2" {
                    let off = u32::from_be_bytes([
                        ttf[rec + 8],
                        ttf[rec + 9],
                        ttf[rec + 10],
                        ttf[rec + 11],
                    ]) as usize;
                    return u16::from_be_bytes([ttf[off + 4], ttf[off + 5]]);
                }
            }
            panic!("missing OS/2 table");
        }

        let fonts = load_misans_fonts().expect("load embedded MiSans fonts");
        assert_eq!(fonts.len(), 2);
        assert_eq!(read_os2_weight(&fonts[0]), 400);
        assert_eq!(read_os2_weight(&fonts[1]), 500);
    }
}
