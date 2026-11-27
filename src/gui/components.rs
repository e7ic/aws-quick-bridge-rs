use super::motion::{self, SlidingTrackStyle, SpringConfig, TrackAxis};
use super::quota_history::QuotaReading;
use super::{
    ACCOUNT_ACTIVE_GLOW_PATH, ACCOUNT_ACTIVE_LABEL_PATH, ACCOUNT_AVATAR_BACKDROP_PATH, BridgeApp,
    numeric_font, ui_font,
};
use crate::qbs::QuotaWindow;
use chrono::{DateTime, Local, Utc};
use gpui::{
    AnyElement, AnyView, App, Bounds, BoxShadow, ContentMask, Context, Corners, Div, FontWeight,
    Hsla, ImageSource, ObjectFit, PathBuilder, Pixels, Resource, SharedString, Svg, Transformation,
    Window, canvas, div, fill, img, linear_color_stop, linear_gradient, pattern_slash, point,
    prelude::*, px, radians, relative, rgb, rgba, size, svg,
};
use std::f32::consts::{FRAC_PI_4, TAU};
use std::time::Duration;

const CARD_RADIUS: f32 = 13.0;
const CARD_PADDING: f32 = 14.0;
const ACCOUNT_AVATAR_SIZE: f32 = 38.0;
const ACCOUNT_AVATAR_RADIUS: f32 = 9.0;
const ACCOUNT_RIBBON_REACH: f32 = 56.0;
const ACCOUNT_RIBBON_INSET: f32 = 24.0;
const ACCOUNT_AVATAR_BACKDROP_SIZE: f32 =
    ACCOUNT_RIBBON_REACH.max(CARD_PADDING + ACCOUNT_AVATAR_SIZE);

// 用斜带遮罩混合模糊与清晰底层，GPUI 按尺寸缓存头像的 Alpha 蒙版。
pub(super) fn account_avatar_backdrop_svg() -> String {
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"
              width="{ACCOUNT_AVATAR_BACKDROP_SIZE}" height="{ACCOUNT_AVATAR_BACKDROP_SIZE}">
            <defs>
                <path id="ribbon" d="M{ACCOUNT_RIBBON_INSET} 0H{ACCOUNT_RIBBON_REACH}L0 {ACCOUNT_RIBBON_REACH}V{ACCOUNT_RIBBON_INSET}Z"/>
                <filter id="soften" filterUnits="userSpaceOnUse" x="0" y="0"
                        width="{ACCOUNT_AVATAR_BACKDROP_SIZE}" height="{ACCOUNT_AVATAR_BACKDROP_SIZE}"
                        color-interpolation-filters="sRGB">
                    <feImage xlink:href="#ribbon" result="mask"/>
                    <feGaussianBlur in="SourceGraphic" stdDeviation="1.6" result="blurred"/>
                    <feComposite in="blurred" in2="mask" operator="in" result="covered"/>
                    <feComposite in="SourceGraphic" in2="mask" operator="out" result="outside"/>
                    <feComposite in="covered" in2="outside" operator="arithmetic" k2="1" k3="1"/>
                </filter>
            </defs>
            <rect x="{CARD_PADDING}" y="{CARD_PADDING}" width="{ACCOUNT_AVATAR_SIZE}"
                  height="{ACCOUNT_AVATAR_SIZE}" rx="{ACCOUNT_AVATAR_RADIUS}" filter="url(#soften)"/>
        </svg>"##
    )
}

/// Loopo 的中性色、细描边和数据强调色。
#[derive(Clone, Copy)]
pub(super) struct Theme {
    pub(super) keyboard_focus: bool,
    pub(super) bg_window: Hsla,
    pub(super) bg_card: Hsla,
    pub(super) bg_well: Hsla,
    pub(super) bg_hover: Hsla,
    pub(super) bg_nav_active: Hsla,
    pub(super) bg_seg_track: Hsla,
    pub(super) bg_seg_active: Hsla,
    pub(super) bg_skeleton: Hsla,
    skeleton_highlight: Hsla,
    pub(super) gateway_idle_glow: [Hsla; 2],
    pub(super) gateway_running_glow: [Hsla; 2],
    pub(super) border_subtle: Hsla,
    pub(super) border_strong: Hsla,
    pub(super) cta_bg: Hsla,
    pub(super) cta_hover: Hsla,
    pub(super) cta_fg: Hsla,
    pub(super) accent: Hsla,
    pub(super) accent_soft: Hsla,
    pub(super) accent_text: Hsla,
    pub(super) accent_border: Hsla,
    account_ribbon: [Hsla; 2],
    account_ribbon_text: Hsla,
    pub(super) series_pink: Hsla,
    pub(super) series_blue: Hsla,
    pub(super) blue_soft: Hsla,
    pub(super) blue_text: Hsla,
    pub(super) green: Hsla,
    pub(super) green_soft: Hsla,
    pub(super) green_text: Hsla,
    pub(super) green_border: Hsla,
    pub(super) amber: Hsla,
    pub(super) amber_soft: Hsla,
    pub(super) amber_text: Hsla,
    pub(super) amber_border: Hsla,
    pub(super) red: Hsla,
    pub(super) red_soft: Hsla,
    pub(super) red_text: Hsla,
    pub(super) red_border: Hsla,
    pub(super) text_primary: Hsla,
    pub(super) text_secondary: Hsla,
    pub(super) text_muted: Hsla,
    pub(super) switch_off: Hsla,
}

impl Theme {
    pub(super) fn interpolated(darkness: f32) -> Self {
        if darkness <= 0.0 {
            return Self::light();
        }
        if darkness >= 1.0 {
            return Self::dark();
        }
        let light = Self::light();
        let dark = Self::dark();
        let blend = |a, b| motion::lerp_hsla(a, b, darkness);
        let blend_layer = |light_layer, dark_layer| {
            blend(
                light.bg_card.blend(light_layer),
                dark.bg_card.blend(dark_layer),
            )
        };
        let background = blend(light.bg_card, dark.bg_card);
        let text = |a, b, surface: Hsla| {
            let color = blend(a, b);
            let background_luminance = Self::luminance(surface);
            let color_luminance = Self::luminance(color);
            let contrast = (background_luminance.max(color_luminance) + 0.05)
                / (background_luminance.min(color_luminance) + 0.05);
            if contrast >= 4.5 {
                color
            } else if background_luminance >= 0.179 {
                rgb(0x000000).into()
            } else {
                rgb(0xffffff).into()
            }
        };
        Self {
            keyboard_focus: false,
            bg_window: blend(light.bg_window, dark.bg_window),
            bg_card: blend(light.bg_card, dark.bg_card),
            bg_well: blend(light.bg_well, dark.bg_well),
            bg_hover: blend(light.bg_hover, dark.bg_hover),
            bg_nav_active: blend(light.bg_nav_active, dark.bg_nav_active),
            bg_seg_track: blend(light.bg_seg_track, dark.bg_seg_track),
            bg_seg_active: blend(light.bg_seg_active, dark.bg_seg_active),
            bg_skeleton: blend(light.bg_skeleton, dark.bg_skeleton),
            skeleton_highlight: blend(light.skeleton_highlight, dark.skeleton_highlight),
            gateway_idle_glow: [
                blend(light.gateway_idle_glow[0], dark.gateway_idle_glow[0]),
                blend(light.gateway_idle_glow[1], dark.gateway_idle_glow[1]),
            ],
            gateway_running_glow: [
                blend(light.gateway_running_glow[0], dark.gateway_running_glow[0]),
                blend(light.gateway_running_glow[1], dark.gateway_running_glow[1]),
            ],
            border_subtle: blend_layer(light.border_subtle, dark.border_subtle),
            border_strong: blend_layer(light.border_strong, dark.border_strong),
            cta_bg: blend(light.cta_bg, dark.cta_bg),
            cta_hover: blend(light.cta_hover, dark.cta_hover),
            cta_fg: text(light.cta_fg, dark.cta_fg, blend(light.cta_bg, dark.cta_bg)),
            accent: blend(light.accent, dark.accent),
            accent_soft: blend_layer(light.accent_soft, dark.accent_soft),
            accent_text: text(light.accent_text, dark.accent_text, background),
            accent_border: blend(light.accent_border, dark.accent_border),
            account_ribbon: [
                blend(light.account_ribbon[0], dark.account_ribbon[0]),
                blend(light.account_ribbon[1], dark.account_ribbon[1]),
            ],
            account_ribbon_text: text(
                light.account_ribbon_text,
                dark.account_ribbon_text,
                background,
            ),
            series_pink: blend(light.series_pink, dark.series_pink),
            series_blue: blend(light.series_blue, dark.series_blue),
            blue_soft: blend_layer(light.blue_soft, dark.blue_soft),
            blue_text: text(light.blue_text, dark.blue_text, background),
            green: blend(light.green, dark.green),
            green_soft: blend_layer(light.green_soft, dark.green_soft),
            green_text: text(light.green_text, dark.green_text, background),
            green_border: blend(light.green_border, dark.green_border),
            amber: blend(light.amber, dark.amber),
            amber_soft: blend_layer(light.amber_soft, dark.amber_soft),
            amber_text: text(light.amber_text, dark.amber_text, background),
            amber_border: blend(light.amber_border, dark.amber_border),
            red: blend(light.red, dark.red),
            red_soft: blend_layer(light.red_soft, dark.red_soft),
            red_text: text(light.red_text, dark.red_text, background),
            red_border: blend(light.red_border, dark.red_border),
            text_primary: text(light.text_primary, dark.text_primary, background),
            text_secondary: text(light.text_secondary, dark.text_secondary, background),
            text_muted: text(light.text_muted, dark.text_muted, background),
            switch_off: blend(light.switch_off, dark.switch_off),
        }
    }

    fn luminance(color: Hsla) -> f32 {
        let color = gpui::Rgba::from(color);
        let linear = |channel: f32| {
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }

    pub(super) fn light() -> Self {
        Self {
            keyboard_focus: false,
            bg_window: rgb(0xf5f5f5).into(),
            bg_card: rgb(0xffffff).into(),
            bg_well: rgb(0xf9f9f9).into(),
            bg_hover: rgb(0xeeeeee).into(),
            bg_nav_active: rgb(0xececed).into(),
            bg_seg_track: rgb(0xf4f4f4).into(),
            bg_seg_active: rgb(0xffffff).into(),
            bg_skeleton: rgb(0xeaeaea).into(),
            skeleton_highlight: rgb(0xfafafa).into(),
            gateway_idle_glow: [rgba(0x96bfff7a).into(), rgba(0xc1acfa6b).into()],
            gateway_running_glow: [rgba(0x8bcafa7a).into(), rgba(0x89d9cf67).into()],
            border_subtle: rgb(0xe8e8e8).into(),
            border_strong: rgb(0xdedede).into(),
            cta_bg: rgb(0x292929).into(),
            cta_hover: rgb(0x404040).into(),
            cta_fg: rgb(0xffffff).into(),
            accent: rgb(0xfd4b23).into(),
            accent_soft: rgb(0xfff1ed).into(),
            accent_text: rgb(0xc2361a).into(),
            accent_border: rgba(0xfd4b2338).into(),
            account_ribbon: [rgba(0xffbc8d9c).into(), rgba(0xffead180).into()],
            account_ribbon_text: rgb(0x552e19).into(),
            series_pink: rgb(0xf66b9b).into(),
            series_blue: rgb(0x4d88f8).into(),
            blue_soft: rgb(0xeaf1ff).into(),
            blue_text: rgb(0x2862c7).into(),
            green: rgb(0x18b170).into(),
            green_soft: rgb(0xebf8f2).into(),
            green_text: rgb(0x0f7d4f).into(),
            green_border: rgba(0x18b1703a).into(),
            amber: rgb(0xd99a06).into(),
            amber_soft: rgb(0xfdf6e3).into(),
            amber_text: rgb(0x8a6000).into(),
            amber_border: rgba(0xd99a063a).into(),
            red: rgb(0xe5484d).into(),
            red_soft: rgb(0xfdeeee).into(),
            red_text: rgb(0xc4282e).into(),
            red_border: rgba(0xe5484d3a).into(),
            text_primary: rgb(0x222222).into(),
            text_secondary: rgb(0x585858).into(),
            text_muted: rgb(0x6b6b70).into(),
            switch_off: rgb(0xd9d9d9).into(),
        }
    }

    pub(super) fn dark() -> Self {
        Self {
            keyboard_focus: false,
            bg_window: rgb(0x171717).into(),
            bg_card: rgb(0x202020).into(),
            bg_well: rgb(0x252525).into(),
            bg_hover: rgb(0x2a2a2a).into(),
            bg_nav_active: rgb(0x303030).into(),
            bg_seg_track: rgb(0x1b1b1b).into(),
            bg_seg_active: rgb(0x343434).into(),
            bg_skeleton: rgb(0x363636).into(),
            skeleton_highlight: rgb(0x505050).into(),
            gateway_idle_glow: [rgba(0x4785e06b).into(), rgba(0x795bce55).into()],
            gateway_running_glow: [rgba(0x439ae26b).into(), rgba(0x41bca45b).into()],
            border_subtle: rgba(0xffffff10).into(),
            border_strong: rgba(0xffffff20).into(),
            cta_bg: rgb(0xe2e2e2).into(),
            cta_hover: rgb(0xf8f8f8).into(),
            cta_fg: rgb(0x1d1d1d).into(),
            accent: rgb(0xff5e3a).into(),
            accent_soft: rgba(0xff5e3a20).into(),
            accent_text: rgb(0xff8466).into(),
            accent_border: rgba(0xff5e3a4d).into(),
            account_ribbon: [rgba(0xd9976c60).into(), rgba(0xf7cba056).into()],
            account_ribbon_text: rgb(0xfff5e9).into(),
            series_pink: rgb(0xf78db0).into(),
            series_blue: rgb(0x75a5ff).into(),
            blue_soft: rgba(0x75a5ff22).into(),
            blue_text: rgb(0x9bbbff).into(),
            green: rgb(0x22c583).into(),
            green_soft: rgba(0x22c5831d).into(),
            green_text: rgb(0x4ade80).into(),
            green_border: rgba(0x22c58345).into(),
            amber: rgb(0xf5b82e).into(),
            amber_soft: rgba(0xf5b82e1d).into(),
            amber_text: rgb(0xfbbf24).into(),
            amber_border: rgba(0xf5b82e45).into(),
            red: rgb(0xf25f64).into(),
            red_soft: rgba(0xf25f641d).into(),
            red_text: rgb(0xfca5a5).into(),
            red_border: rgba(0xf25f6445).into(),
            text_primary: rgb(0xf2f2f2).into(),
            text_secondary: rgb(0xc2c2c2).into(),
            text_muted: rgb(0x9b9b9b).into(),
            switch_off: rgb(0x484848).into(),
        }
    }
}

pub(super) struct SegItem {
    selected: bool,
    element: AnyElement,
}

#[derive(Clone, Copy)]
pub(super) enum BtnVariant {
    Primary,
    Secondary,
    Ghost,
    Text(Hsla),
    SoftSuccess,
    SoftBlue,
    SoftDanger,
    SoftAccent,
    Disabled,
}

struct TextTooltip {
    theme: Theme,
    text: SharedString,
}

#[derive(Clone, Copy)]
pub(super) struct QuotaTrendPoint {
    pub(super) at: Option<DateTime<Utc>>,
    pub(super) remaining: f64,
    credits: Option<f64>,
}

impl QuotaTrendPoint {
    fn from_window(quota: &QuotaWindow, at: Option<DateTime<Utc>>) -> Self {
        Self {
            at,
            remaining: quota_remaining_ratio(quota),
            credits: quota_credits(quota),
        }
    }

    pub(super) fn history(
        quota: &QuotaWindow,
        history: &[QuotaReading],
        updated_at: Option<DateTime<Utc>>,
    ) -> Vec<Self> {
        let mut points: Vec<_> = history
            .iter()
            .rev()
            .map_while(|reading| {
                reading
                    .windows
                    .iter()
                    .find(|item| item.name == quota.name)
                    .map(|item| Self::from_window(item, Some(reading.at)))
            })
            .collect();
        points.reverse();
        if points.is_empty() {
            points.push(Self::from_window(quota, updated_at));
        }
        points
    }

    pub(super) fn tooltip(self) -> String {
        let time = self.at.map_or_else(
            || "—".to_owned(),
            |at| {
                at.with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            },
        );
        let credits = self.credits.map_or_else(
            || "—".to_owned(),
            |credits| format!("{} credits", format_quota_credits(credits)),
        );
        format!(
            "刷新时间 {time}\n剩余额度 {credits} · 剩余比例 {}",
            quota_percent(self.remaining),
        )
    }
}

impl Render for TextTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        BridgeApp::card(self.theme)
            .max_w(px(420.0))
            .px(px(10.0))
            .py(px(7.0))
            .rounded(px(9.0))
            .shadow_md()
            .font(ui_font())
            .text_size(px(11.5))
            .text_color(self.theme.text_primary)
            .child(self.text.clone())
    }
}

struct Shimmer {
    peak: Pixels,
    feather: Pixels,
    base: Hsla,
    highlight: Hsla,
}

impl Shimmer {
    fn paint(&self, window: &mut Window, bounds: Bounds<Pixels>, radius: f32) {
        let left = bounds.left();
        let right = bounds.right();
        if right <= left || bounds.size.height <= px(0.0) {
            return;
        }
        if self.peak + self.feather <= left || self.peak - self.feather >= right {
            if self.base.a > 0.0 {
                window.paint_quad(fill(bounds, self.base).corner_radii(px(radius)));
            }
            return;
        }

        let offset = |x| (x - left) / bounds.size.width;
        let peak = linear_color_stop(self.highlight, offset(self.peak));
        let start = linear_color_stop(self.base, offset(self.peak - self.feather));
        let end = linear_color_stop(self.base, offset(self.peak + self.feather));
        let split = self.peak.clamp(left, right);

        // 保持原始圆角分别绘制左右两半渐变，并在高光峰值处裁剪拼接。
        for (from, to, first, last) in [(left, split, start, peak), (split, right, peak, end)] {
            if to > from {
                let mask = ContentMask {
                    bounds: Bounds::from_corners(
                        point(from, bounds.top()),
                        point(to, bounds.bottom()),
                    ),
                };
                window.with_content_mask(Some(mask), |window| {
                    let gradient = linear_gradient(90.0, first, last);
                    window.paint_quad(fill(bounds, gradient).corner_radii(px(radius)));
                });
            }
        }
    }
}

impl BridgeApp {
    pub(super) fn sidebar_gateway_glow(t: Theme, running: bool) -> impl IntoElement {
        let [left, right] = if running {
            t.gateway_running_glow
        } else {
            t.gateway_idle_glow
        };
        canvas(
            |_, _, _| (),
            move |bounds, (), window, _| {
                let radius = px(65.0);
                // 只绘制模糊层，让色彩从侧栏底部向上散开。
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    for (x, bottom_inset, color) in [(0.12, 16.0, left), (0.94, 36.0, right)] {
                        let origin = point(
                            bounds.left() + bounds.size.width * x - radius,
                            bounds.bottom() - px(bottom_inset) - radius,
                        );
                        window.paint_shadows(
                            Bounds::new(origin, size(radius * 2.0, radius * 2.0)),
                            Corners::all(radius),
                            &[BoxShadow {
                                color,
                                offset: point(px(0.0), px(0.0)),
                                blur_radius: px(50.0),
                                spread_radius: px(0.0),
                            }],
                        );
                    }
                });
            },
        )
        .absolute()
        .bottom_0()
        .left_0()
        .right_0()
        .h(px(232.0))
    }

    pub(super) fn text_tooltip(
        theme: Theme,
        text: impl Into<SharedString>,
    ) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        let text = text.into();
        move |_, cx| {
            cx.new(|_| TextTooltip {
                theme,
                text: text.clone(),
            })
            .into()
        }
    }

    pub(super) fn raw_icon(path: &'static str, size_px: f32, color: Hsla) -> Svg {
        svg()
            .path(path)
            .size(px(size_px))
            .flex_shrink_0()
            .text_color(color)
    }

    pub(super) fn icon(path: &'static str, size_px: f32, color: Hsla) -> AnyElement {
        Self::raw_icon(path, size_px, color).into_any_element()
    }

    fn shimmer_layer(
        id: impl Into<SharedString>,
        base: Hsla,
        highlight: Hsla,
        draw: impl Fn(&mut Window, Bounds<Pixels>, Shimmer) + Copy + 'static,
    ) -> AnyElement {
        let layer = move |phase: Option<f32>| {
            div().absolute().inset_0().child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, _| {
                        let feather = phase.map_or(px(0.0), |_| bounds.size.width * 0.32);
                        let peak = phase.map_or(bounds.left() - px(1.0), |phase| {
                            let sweep = (phase / 0.82).min(1.0);
                            bounds.left() - feather + (bounds.size.width + feather * 2.0) * sweep
                        });
                        draw(
                            window,
                            bounds,
                            Shimmer {
                                peak,
                                feather,
                                base,
                                highlight,
                            },
                        );
                    },
                )
                .size_full(),
            )
        };
        if motion::is_reduced_motion() {
            return layer(None).into_any_element();
        }
        motion::timeline(id.into(), Duration::from_millis(1450), move |phase| {
            layer(Some(phase))
        })
        .repeat()
        .into_any_element()
    }

    pub(super) fn shimmer_overlay(
        id: impl Into<SharedString>,
        highlight: Hsla,
        radius: f32,
    ) -> AnyElement {
        Self::shimmer_layer(
            id,
            highlight.opacity(0.0),
            highlight,
            move |window, bounds, shimmer| {
                shimmer.paint(window, bounds, radius);
            },
        )
    }

    pub(super) fn skeleton_bar(
        t: Theme,
        id: impl Into<SharedString>,
        width: Option<f32>,
        height: f32,
        radius: f32,
    ) -> AnyElement {
        div()
            .relative()
            .w(width.map_or(relative(1.0), |width| px(width).into()))
            .h(px(height))
            .child(Self::shimmer_layer(
                id,
                t.bg_skeleton,
                t.skeleton_highlight,
                move |window, bounds, shimmer| shimmer.paint(window, bounds, radius),
            ))
            .into_any_element()
    }

    /// 带可选 Solar Bold 图标的 Loopo 触感按钮。
    #[expect(
        clippy::too_many_arguments,
        reason = "Shared GPUI button entry point with separate presentation and event options"
    )]
    pub(super) fn button<V: 'static>(
        t: Theme,
        id: impl Into<SharedString>,
        icon_path: Option<&'static str>,
        loading: bool,
        label: impl Into<SharedString>,
        variant: BtnVariant,
        on_click: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
        cx: &mut Context<V>,
    ) -> AnyElement {
        let id: SharedString = id.into();
        let spin_id = format!("btn-spin-{id}");
        let label: SharedString = label.into();
        let has_label = !label.is_empty();
        Self::button_with_content(
            t,
            id,
            loading,
            has_label,
            variant,
            move |color| {
                icon_path
                    .map(|path| {
                        let icon = Self::raw_icon(path, 14.0, color);
                        if loading && path == "icons/refresh-bold.svg" {
                            motion::spin_svg(spin_id, true, icon)
                        } else {
                            icon.into_any_element()
                        }
                    })
                    .into_iter()
                    .chain(has_label.then(|| label.into_any_element()))
            },
            on_click,
            cx,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Custom button content uses the same presentation and event options"
    )]
    pub(super) fn button_with_content<V: 'static, C: IntoIterator<Item = AnyElement>>(
        t: Theme,
        id: impl Into<SharedString>,
        loading: bool,
        has_label: bool,
        variant: BtnVariant,
        content: impl FnOnce(Hsla) -> C,
        on_click: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
        cx: &mut Context<V>,
    ) -> AnyElement {
        let id_ss: SharedString = id.into();
        let primary_start: Hsla = rgb(0x1456d9).into();
        let primary_end: Hsla = rgb(0x3670e0).into();
        let is_primary = matches!(variant, BtnVariant::Primary);
        let radius = match variant {
            BtnVariant::Primary | BtnVariant::SoftBlue | BtnVariant::SoftDanger => 8.0,
            _ => 16.0,
        };
        let interactive = !loading && !matches!(variant, BtnVariant::Disabled);
        let transparent = rgba(0x00000000).into();
        let (bg, hover_bg, fg, border) = match variant {
            BtnVariant::Primary => (
                primary_start,
                primary_start,
                rgb(0xffffff).into(),
                primary_start,
            ),
            BtnVariant::Secondary => (t.bg_card, t.bg_hover, t.text_primary, t.border_strong),
            BtnVariant::Ghost => (transparent, t.bg_hover, t.text_secondary, transparent),
            BtnVariant::Text(color) => (transparent, transparent, color, transparent),
            BtnVariant::SoftSuccess => (t.green_soft, t.green_soft, t.green_text, t.green_border),
            BtnVariant::SoftBlue => (t.blue_soft, t.blue_soft, t.blue_text, transparent),
            BtnVariant::SoftDanger => (t.red_soft, t.red_soft, t.red_text, transparent),
            BtnVariant::SoftAccent => {
                (t.accent_soft, t.accent_soft, t.accent_text, t.accent_border)
            }
            BtnVariant::Disabled => (t.bg_well, t.bg_well, t.text_muted, t.border_subtle),
        };
        let button = div()
            .id(id_ss.clone())
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .flex_shrink_0()
            .h(px(32.0))
            .when(has_label, |el| el.px(px(12.0)))
            .when(!has_label, |el| el.w(px(32.0)))
            .rounded(px(radius))
            .bg(bg)
            .border_1()
            .border_color(border)
            .text_color(fg)
            .text_size(px(12.5))
            .font_weight(FontWeight::NORMAL)
            .whitespace_nowrap()
            .when(matches!(variant, BtnVariant::Text(_)), |el| el.opacity(0.8))
            .when(is_primary, |el| {
                el.bg(linear_gradient(
                    90.0,
                    linear_color_stop(primary_start, 0.0),
                    linear_color_stop(primary_end, 1.0),
                ))
                .shadow_xs()
            })
            .when(interactive, |el| {
                el.cursor_pointer()
                    .focusable()
                    .tab_stop(true)
                    .when(t.keyboard_focus, |el| {
                        el.focus(move |s| s.border_color(t.series_blue))
                    })
            })
            .when(loading, |el| {
                el.relative().child(Self::shimmer_overlay(
                    format!("button-loading-{id_ss}"),
                    fg.opacity(if is_primary { 0.20 } else { 0.08 }),
                    radius - 1.0,
                ))
            })
            .children(content(fg))
            .when(interactive, |el| {
                el.on_click(cx.listener(move |this, _, window, cx| {
                    on_click(this, window, cx);
                    cx.notify();
                }))
            });
        if !interactive {
            return motion::layout(button).into_any_element();
        }
        let button = motion::control_feedback(button, move |style, hover, press| {
            let opacity = match variant {
                BtnVariant::Ghost | BtnVariant::Secondary => {
                    style.background = Some(motion::lerp_hsla(bg, hover_bg, hover).into());
                    1.0
                }
                BtnVariant::Primary => 1.0 - 0.02 * hover,
                BtnVariant::Text(_) => 0.8 + 0.2 * hover,
                _ => 1.0 - 0.045 * hover,
            };
            style.opacity = Some(opacity * (1.0 - 0.10 * press));
        });
        motion::layout(button).into_any_element()
    }

    pub(super) fn toggle_switch(
        t: Theme,
        id: impl Into<SharedString>,
        on: bool,
        on_toggle: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id_ss: SharedString = id.into();
        let on_click = cx.listener(move |this, _, window, cx| {
            on_toggle(this, window, cx);
            cx.notify();
        });
        motion::spring_value(
            SharedString::from(format!("toggle-spring-{id_ss}")),
            if on { 1.0 } else { 0.0 },
            SpringConfig::SNAPPY,
            move |progress| {
                let p = progress.clamp(-0.06, 1.06);
                let travel_mid = (1.0 - (p.clamp(0.0, 1.0) * 2.0 - 1.0).abs()).clamp(0.0, 1.0);
                let stretch = travel_mid * 2.5;
                let knob_w = 16.0 + stretch;
                let knob_x = 2.0 + p * (16.0 - stretch);
                let switch = div()
                    .id(id_ss)
                    .relative()
                    .w(px(38.0))
                    .h(px(22.0))
                    .rounded_full()
                    .bg(motion::lerp_hsla(t.switch_off, t.accent, progress))
                    .flex_shrink_0()
                    .focusable()
                    .tab_stop(true)
                    .border_1()
                    .border_color(rgba(0x00000000))
                    .when(t.keyboard_focus, |el| {
                        el.focus(move |s| s.border_color(t.series_blue))
                    })
                    .cursor_pointer()
                    .child(
                        div()
                            .absolute()
                            .top(px(2.0))
                            .left(px(knob_x))
                            .w(px(knob_w))
                            .h(px(16.0))
                            .rounded_full()
                            .bg(rgb(0xffffff))
                            .shadow_xs(),
                    )
                    .on_click(on_click);
                motion::layout(motion::control_feedback(switch, |style, hover, press| {
                    style.opacity = Some(1.0 - 0.025 * hover - 0.10 * press);
                }))
            },
        )
        .into_any_element()
    }

    pub(super) fn seg_item(
        t: Theme,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        selected: bool,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> SegItem {
        let resting_opacity = if selected { 1.0 } else { 0.72 };
        let el = div()
            .id(id.into())
            .px(px(11.0))
            .py(px(4.0))
            .rounded_full()
            .border_1()
            .border_color(rgba(0x00000000))
            .focusable()
            .tab_stop(true)
            .when(t.keyboard_focus, |el| {
                el.focus(move |style| style.border_color(t.series_blue))
            })
            .text_size(px(12.0))
            // 统一字重，避免切换选中项时文字宽度跳变带动滑块重排。
            .font_weight(FontWeight::MEDIUM)
            .whitespace_nowrap()
            .cursor_pointer()
            .text_color(t.text_primary)
            .child(label.into())
            .on_click(cx.listener(move |this, _, window, cx| {
                on_click(this, window, cx);
                cx.notify();
            }));
        let element = motion::layout(motion::control_feedback(el, move |style, hover, press| {
            let opacity = resting_opacity + (1.0 - resting_opacity) * hover;
            style.opacity = Some(opacity * (1.0 - 0.09 * press));
        }))
        .into_any_element();
        SegItem { selected, element }
    }

    pub(super) fn segmented_wrap(
        t: Theme,
        id: impl Into<SharedString>,
        items: Vec<SegItem>,
    ) -> AnyElement {
        let selected = items.iter().position(|item| item.selected);
        let children = items.into_iter().map(|item| item.element).collect();
        let track = motion::sliding_track(
            id.into(),
            selected,
            SlidingTrackStyle {
                axis: TrackAxis::Horizontal,
                gap: 2.0,
                padding: 2.0,
                track_bg: Some(t.bg_seg_track),
                track_border: Some(t.border_subtle),
                track_radius: 16.0,
                pill_bg: t.bg_seg_active,
                pill_border: t.border_subtle,
                pill_radius: 14.0,
                pill_shadow: Some(BoxShadow {
                    color: rgba(0x00000014).into(),
                    offset: point(px(0.0), px(1.0)),
                    blur_radius: px(2.0),
                    spread_radius: px(0.0),
                }),
            },
            children,
        );
        motion::layout(track).into_any_element()
    }

    pub(super) fn filter_chip(
        t: Theme,
        id: impl Into<SharedString>,
        label: &'static str,
        count: usize,
        selected: bool,
        on_click: impl Fn(&mut Self) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chip = div()
            .id(id.into())
            .flex()
            .items_center()
            .gap(px(5.0))
            .h(px(28.0))
            .px(px(10.0))
            .rounded_full()
            .bg(if selected { t.cta_bg } else { t.bg_well })
            .border_1()
            .border_color(if selected { t.cta_bg } else { t.border_subtle })
            .text_size(px(12.0))
            .text_color(if selected { t.cta_fg } else { t.text_primary })
            .whitespace_nowrap()
            .cursor_pointer()
            .focusable()
            .tab_stop(true)
            .when(t.keyboard_focus, |el| {
                el.focus(move |style| style.border_color(t.series_blue))
            })
            .child(label)
            .child(
                div()
                    .font(numeric_font())
                    .text_size(px(10.5))
                    .text_color(if selected { t.cta_fg } else { t.text_muted })
                    .child(count.to_string()),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                on_click(this);
                cx.notify();
            }));
        motion::layout(motion::control_feedback(
            chip,
            move |style, hover, press| {
                let (bg, hovered) = if selected {
                    (t.cta_bg, t.cta_hover)
                } else {
                    (t.bg_well, t.bg_hover)
                };
                style.background = Some(motion::lerp_hsla(bg, hovered, hover).into());
                style.opacity = Some(1.0 - 0.10 * press);
            },
        ))
        .into_any_element()
    }

    pub(super) fn pill(
        label: impl Into<SharedString>,
        fg: Hsla,
        bg: Hsla,
        border: Hsla,
        dot_color: Option<Hsla>,
        pulse_id: Option<&'static str>,
    ) -> AnyElement {
        let mut el = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(7.0))
            .py(px(2.0))
            .rounded(px(5.5))
            .bg(bg)
            .border_1()
            .border_color(border)
            .text_size(px(11.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(fg)
            .whitespace_nowrap();

        if let Some(dc) = dot_color {
            let dot = div().size(px(5.5)).rounded_full().bg(dc);
            if let Some(pid) = pulse_id.filter(|_| !motion::is_reduced_motion()) {
                el = el.child(
                    motion::timeline(pid, Duration::from_millis(2000), move |phase| {
                        dot.opacity(0.9 + 0.1 * (phase * TAU).cos())
                    })
                    .repeat(),
                );
            } else {
                el = el.child(dot);
            }
        }
        el.child(label.into()).into_any_element()
    }

    pub(super) fn pill_icon(
        icon_path: &'static str,
        label: impl Into<SharedString>,
        fg: Hsla,
        bg: Hsla,
        border: Hsla,
    ) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(5.0))
            .px(px(7.0))
            .py(px(2.0))
            .rounded(px(5.5))
            .bg(bg)
            .border_1()
            .border_color(border)
            .text_size(px(11.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(fg)
            .whitespace_nowrap()
            .child(Self::icon(icon_path, 12.0, fg))
            .child(label.into())
            .into_any_element()
    }

    pub(super) fn card(t: Theme) -> Div {
        div()
            .p(px(CARD_PADDING))
            .rounded(px(CARD_RADIUS))
            .bg(t.bg_card)
            .border_1()
            .border_color(t.border_subtle)
    }

    pub(super) fn account_avatar(t: Theme, initial: String, is_primary: bool, in_use: bool) -> Div {
        let (backdrop, text) = if is_primary {
            (t.accent_soft, t.accent_text)
        } else {
            (t.bg_seg_track, t.text_secondary)
        };
        let avatar = div()
            .size(px(ACCOUNT_AVATAR_SIZE))
            .rounded(px(ACCOUNT_AVATAR_RADIUS))
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .text_size(px(15.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(text);
        let avatar = if in_use {
            avatar.relative().child(
                svg()
                    .path(ACCOUNT_AVATAR_BACKDROP_PATH)
                    .absolute()
                    .left(px(-CARD_PADDING))
                    .top(px(-CARD_PADDING))
                    .size(px(ACCOUNT_AVATAR_BACKDROP_SIZE))
                    .text_color(backdrop),
            )
        } else {
            avatar.bg(backdrop)
        };
        avatar.child(initial)
    }

    pub(super) fn active_account_glow() -> Div {
        div().absolute().inset_0().child(
            img(ImageSource::Resource(Resource::Embedded(
                ACCOUNT_ACTIVE_GLOW_PATH.into(),
            )))
            .size_full()
            .rounded(px(CARD_RADIUS - 1.0))
            .object_fit(ObjectFit::Fill),
        )
    }

    pub(super) fn active_account_ribbon(t: Theme) -> Div {
        const LABEL_WIDTH: f32 = 44.0;
        const LABEL_HEIGHT: f32 = 11.0;

        let [ribbon_start, ribbon_end] = t.account_ribbon;
        let label_center = (ACCOUNT_RIBBON_REACH + ACCOUNT_RIBBON_INSET) / 4.0;
        let ribbon_background = canvas(
            |_, _, _| (),
            move |bounds, (), window, _| {
                let left = bounds.left();
                let top = bounds.top();
                let mut path = PathBuilder::fill();
                path.move_to(point(left + px(ACCOUNT_RIBBON_INSET), top));
                path.line_to(point(bounds.right(), top));
                path.line_to(point(left, bounds.bottom()));
                path.line_to(point(left, top + px(ACCOUNT_RIBBON_INSET)));
                path.close();
                if let Ok(path) = path.build() {
                    window.paint_path(
                        path,
                        linear_gradient(
                            45.0,
                            linear_color_stop(ribbon_start, 0.0),
                            linear_color_stop(ribbon_end, 1.0),
                        ),
                    );
                }

                let mut highlight = PathBuilder::stroke(px(0.75));
                highlight.move_to(point(left + px(ACCOUNT_RIBBON_INSET), top + px(1.0)));
                highlight.line_to(point(left + px(1.0), top + px(ACCOUNT_RIBBON_INSET)));
                if let Ok(highlight) = highlight.build() {
                    window.paint_path(highlight, rgba(0xffffff4d));
                }
            },
        )
        .size_full();

        div()
            .absolute()
            .top_0()
            .left_0()
            .size(px(ACCOUNT_RIBBON_REACH))
            .child(ribbon_background)
            .child(
                svg()
                    .path(ACCOUNT_ACTIVE_LABEL_PATH)
                    .absolute()
                    .left(px(label_center - LABEL_WIDTH / 2.0))
                    .top(px(label_center - LABEL_HEIGHT / 2.0))
                    .w(px(LABEL_WIDTH))
                    .h(px(LABEL_HEIGHT))
                    .text_color(t.account_ribbon_text)
                    .with_transformation(Transformation::rotate(radians(-FRAC_PI_4))),
            )
    }

    pub(super) fn card_well(t: Theme) -> Div {
        div()
            .p(px(9.0))
            .rounded(px(8.0))
            .bg(t.bg_well)
            .border_1()
            .border_color(t.border_subtle)
    }

    pub(super) fn card_title(t: Theme, label: &'static str) -> Div {
        div()
            .text_size(px(14.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(t.text_primary)
            .child(label)
    }

    pub(super) fn section_header(t: Theme, title: &'static str) -> Div {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .min_h(px(32.0))
            .child(Self::card_title(t, title))
    }

    pub(super) fn field(t: Theme) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .min_w(px(0.0))
            .min_h(px(34.0))
            .px(px(10.0))
            .rounded(px(7.0))
            .bg(t.bg_card)
            .border_1()
            .border_color(t.border_subtle)
    }

    pub(super) fn empty_state(t: Theme, icon: &'static str, label: &'static str) -> Div {
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .py(px(28.0))
            .px(px(12.0))
            .rounded(px(10.0))
            .bg(t.bg_well)
            .border_1()
            .border_dashed()
            .border_color(t.border_strong)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(32.0))
                    .rounded(px(9.0))
                    .bg(t.bg_card)
                    .border_1()
                    .border_color(t.border_subtle)
                    .child(Self::icon(icon, 16.0, t.text_muted)),
            )
            .child(
                div()
                    .text_size(px(13.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.text_primary)
                    .child(label),
            )
    }

    pub(super) fn hatched_track(
        t: Theme,
        id: impl Into<SharedString>,
        ratio: f32,
        color: Hsla,
    ) -> Div {
        let target = ratio.clamp(0.0, 1.0);
        div()
            .relative()
            .w_full()
            .h(px(7.0))
            .rounded_full()
            .overflow_hidden()
            .bg(t.bg_well)
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(pattern_slash(t.border_strong, 1.1, 3.6)),
            )
            .child(motion::spring_from(
                id.into(),
                0.0,
                target,
                SpringConfig::GENTLE,
                move |value| {
                    let ratio = value.clamp(0.0, 1.0);
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .h_full()
                        .w(relative(ratio))
                        .rounded_full()
                        .when(ratio > 0.002, |el| el.bg(color))
                },
            ))
    }

    pub(super) fn value_with_unit(
        t: Theme,
        value: impl IntoElement,
        unit: Option<&'static str>,
        font_size: f32,
        color: Hsla,
    ) -> Div {
        div()
            .min_w(px(0.0))
            .flex()
            .items_end()
            .gap(px(5.0))
            .child(
                div()
                    .min_w(px(0.0))
                    .font(numeric_font())
                    .font_weight(FontWeight::MEDIUM)
                    .text_size(px(font_size))
                    .line_height(px(font_size))
                    .text_color(color)
                    .truncate()
                    .child(value),
            )
            .when_some(unit, |el, unit| {
                el.child(motion::layout(
                    div()
                        .id("metric-unit")
                        .flex_shrink_0()
                        .mb(px(1.0))
                        .text_size(px(11.0))
                        .line_height(px(12.0))
                        .text_color(t.text_muted)
                        .child(unit),
                ))
            })
    }

    pub(super) fn quota_metric_frame() -> Div {
        div().min_w(px(0.0)).min_h(px(75.0))
    }

    pub(super) fn quota_balance_skeleton(t: Theme, id: impl Into<SharedString>) -> AnyElement {
        Self::quota_metric_frame()
            .relative()
            .child(Self::shimmer_layer(
                id,
                t.bg_skeleton,
                t.skeleton_highlight,
                move |window, bounds, shimmer| {
                    let width = bounds.size.width;
                    let label_y = bounds.top() + px(5.0);
                    let mut shape = |x, y, width, height, radius| {
                        let bounds = Bounds::new(point(x, y), size(width, px(height)));
                        shimmer.paint(window, bounds, radius);
                    };

                    shape(bounds.left(), label_y, px(32.0).min(width * 0.42), 8.0, 3.0);
                    shape(
                        bounds.left(),
                        bounds.top() + px(24.0),
                        px(58.0).min(width * 0.5),
                        17.0,
                        4.0,
                    );
                    let dots_width = px(33.0).min(width * 0.32);
                    shape(
                        bounds.right() - dots_width,
                        bounds.top() + px(20.0),
                        dots_width,
                        26.0,
                        4.0,
                    );
                    shape(
                        bounds.left(),
                        bounds.top() + px(58.0),
                        width * 0.62,
                        6.0,
                        3.0,
                    );
                    let percent_width = px(22.0).min(width * 0.26);
                    shape(
                        bounds.right() - percent_width,
                        label_y,
                        percent_width,
                        8.0,
                        3.0,
                    );
                },
            ))
            .into_any_element()
    }

    pub(super) fn quota_balance_metric(
        t: Theme,
        quota: &QuotaWindow,
        history: &[QuotaReading],
        updated_at: Option<DateTime<Utc>>,
        loading: bool,
        id: impl Into<SharedString>,
    ) -> AnyElement {
        let (label, accent) = match quota.name.as_str() {
            "Monthly" => ("月度", t.accent),
            "Session" => ("会话", t.series_blue),
            "Monthly overage" => ("月度额外", t.series_pink),
            name => (name, t.accent),
        };
        let (remaining, credits) = quota_remaining(quota);
        let percent = quota_percent(remaining);
        let has_credits = credits.is_some();
        let value = credits.map_or_else(|| percent.clone(), format_quota_credits);
        let (percent_color, dot_color) = if remaining <= 0.10 {
            (t.red_text, t.red)
        } else if remaining <= 0.28 {
            (t.amber_text, t.amber)
        } else {
            (t.text_muted, accent)
        };
        let amount = div()
            .id("current-quota")
            .flex()
            .items_center()
            .justify_between()
            .h(px(32.0))
            .gap(px(6.0))
            .child(Self::value_with_unit(
                t,
                motion::rolling_number(
                    if has_credits { "credits" } else { "percentage" },
                    value,
                    credits.unwrap_or(remaining),
                )
                .pause(loading),
                has_credits.then_some("credits"),
                20.0,
                if remaining == 0.0 {
                    t.red_text
                } else {
                    t.text_primary
                },
            ));
        let values = div().flex().flex_col().child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(4.0))
                .text_size(px(11.5))
                .text_color(t.text_muted)
                .child(div().min_w(px(0.0)).truncate().child(label.to_owned()))
                .when(has_credits, |el| {
                    el.child(
                        div()
                            .flex_shrink_0()
                            .font(numeric_font())
                            .text_color(percent_color)
                            .child(
                                motion::rolling_number("remaining", percent, remaining)
                                    .pause(loading),
                            ),
                    )
                }),
        );
        let frame = Self::quota_metric_frame().id(id.into()).relative();
        if loading {
            // Keep the previous values mounted so the refresh can roll from them.
            return motion::layout(
                frame.child(values.opacity(0.0).child(amount)).child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(Self::quota_balance_skeleton(t, "loading")),
                ),
            )
            .into_any_element();
        }

        let used = if has_credits {
            quota.display.clone()
        } else {
            format!("{:.0}%", quota.percent.unwrap_or(0.0))
        };
        let mut tooltip = format!("已用 {used}");
        if let Some(reset) = quota
            .resets_at
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        {
            tooltip.push_str(&format!(
                " · {} 重置",
                reset.with_timezone(&Local).format("%m-%d %H:%M")
            ));
        }

        let trend = QuotaTrendPoint::history(quota, history, updated_at);
        tooltip.push_str(&format!(" · 剩余比例 · 最近 {} 次成功刷新", trend.len()));
        if let Some(latest) = history.last() {
            tooltip.push_str(&format!(
                " · 最近刷新 {}",
                latest.at.with_timezone(&Local).format("%m-%d %H:%M"),
            ));
        }
        motion::layout(
            frame.child(
                values
                    .child(
                        amount
                            .tooltip(Self::text_tooltip(t, tooltip))
                            .child(quota_dots(t, remaining, dot_color)),
                    )
                    .child(Self::quota_sparkline(t, accent, trend)),
            ),
        )
        .into_any_element()
    }
}

pub(super) fn quota_dots(t: Theme, remaining: f64, color: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let filled = remaining as f32 * 20.0;
            for index in 0..20 {
                let coverage = (filled - index as f32).clamp(0.0, 1.0);
                let dot = if coverage > 0.0 {
                    color.opacity(coverage.max(0.25))
                } else {
                    t.border_strong
                };
                let origin = point(
                    bounds.left() + px(7.0 * (index % 5) as f32),
                    bounds.top() + px(7.0 * (index / 5) as f32),
                );
                window.paint_quad(
                    fill(Bounds::new(origin, size(px(5.0), px(5.0))), dot).corner_radii(px(1.4)),
                );
            }
        },
    )
    .w(px(33.0))
    .h(px(26.0))
    .flex_shrink_0()
}

fn quota_remaining_ratio(quota: &QuotaWindow) -> f64 {
    let used = if quota.limit > 0.0 {
        quota.amount / quota.limit
    } else {
        quota.percent.unwrap_or(0.0) / 100.0
    };
    (1.0 - used).clamp(0.0, 1.0)
}

pub(super) fn quota_remaining(quota: &QuotaWindow) -> (f64, Option<f64>) {
    (quota_remaining_ratio(quota), quota_credits(quota))
}

fn quota_credits(quota: &QuotaWindow) -> Option<f64> {
    (quota.limit > 0.0).then(|| (quota.limit - quota.amount).clamp(0.0, quota.limit))
}

pub(super) fn format_quota_credits(balance: f64) -> String {
    if balance > 0.0 && balance < 0.01 {
        "<0.01".to_owned()
    } else {
        format!("{balance:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}

pub(super) fn quota_percent(remaining: f64) -> String {
    if remaining > 0.0 && remaining < 0.01 {
        "<1%".to_owned()
    } else {
        format!("{:.0}%", remaining * 100.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intermediate_themes_keep_borders_subtle_and_text_legible() {
        for step in 0..=20 {
            let theme = Theme::interpolated(step as f32 / 20.0);
            let surface = theme.bg_card;
            let border = surface.blend(theme.border_subtle);
            assert!((surface.l - border.l).abs() < 0.12);

            let a = Theme::luminance(surface);
            let b = Theme::luminance(theme.text_primary);
            assert!((a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5);
        }
    }

    fn quota(amount: f64, limit: f64, percent: Option<f64>) -> QuotaWindow {
        QuotaWindow {
            name: "Monthly".into(),
            amount,
            limit,
            percent,
            display: String::new(),
            resets_at: None,
            reset_secs: None,
            aside: false,
        }
    }

    #[test]
    fn card_quota_shows_remaining_credits_and_percent() {
        assert_eq!(
            quota_remaining(&quota(62.5, 100.0, None)),
            (0.375, Some(37.5))
        );
        assert_eq!(format_quota_credits(37.5), "37.5");
        assert_eq!(quota_remaining(&quota(0.0, 0.0, Some(75.0))), (0.25, None));
    }

    #[test]
    fn quota_sample_tooltip_uses_the_sample_time_and_balance() {
        let at = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let time = at.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S");
        let point = QuotaTrendPoint::from_window(&quota(108.0, 600.0, None), Some(at));
        assert_eq!(
            point.tooltip(),
            format!("刷新时间 {time}\n剩余额度 492 credits · 剩余比例 82%"),
        );

        let percent_only = QuotaTrendPoint::from_window(&quota(0.0, 0.0, Some(75.0)), Some(at));
        assert!(
            percent_only
                .tooltip()
                .ends_with("剩余额度 — · 剩余比例 25%")
        );
    }

    #[test]
    fn card_quota_distinguishes_exhausted_and_small_balances() {
        assert_eq!(quota_percent(0.25), "25%");
        assert_eq!(
            quota_remaining(&quota(105.0, 100.0, None)),
            (0.0, Some(0.0))
        );
        assert_eq!(format_quota_credits(0.0), "0");
        let (remaining, value) = quota_remaining(&quota(99.999, 100.0, None));
        assert!(remaining > 0.0);
        assert_eq!(value.map(format_quota_credits).as_deref(), Some("<0.01"));
        assert_eq!(quota_percent(remaining), "<1%");
    }
}
