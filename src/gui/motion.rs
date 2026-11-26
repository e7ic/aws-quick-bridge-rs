//! 基于物理弹簧与阻尼的轻量 GPUI 过渡动效引擎。
//!
//! 直接构建在 `window.with_element_state`、`window.with_element_offset` 与
//! `window.request_animation_frame` 之上，绕过内置 `AnimationElement` 无法中途打断、
//! 不允许弹簧超调（`0.0..=1.0` 断言）以及缺少退场周期的限制；空闲态自动停帧零开销。

use gpui::{
    AlignItems, AnyElement, App, BorderStyle, Bounds, BoxShadow, Corners, DefiniteLength, Display,
    Edges, Element, ElementId, FlexDirection, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, LayoutId, Length, Pixels, Point, Rgba, SharedString, Style, Styled, Svg,
    Transformation, Window, point, px, quad, radians, size,
};
use std::f32::consts::TAU;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

mod feedback;
mod frame;
mod layout;
mod number;
mod timeline;
pub(super) use feedback::control_feedback;
pub(super) use frame::{begin_frame, frame_time, request_frame};
pub(super) use layout::layout;
pub(super) use number::{rolling_count, rolling_number};
pub(super) use timeline::timeline;

static REDUCED_MOTION: AtomicBool = AtomicBool::new(false);

/// 同步系统“减少动态效果”无障碍开关状态。
pub(super) fn set_reduced_motion(reduced: bool) {
    REDUCED_MOTION.store(reduced, Ordering::Relaxed);
}

/// 当前是否处于“减少动态效果”模式。
pub(super) fn is_reduced_motion() -> bool {
    REDUCED_MOTION.load(Ordering::Relaxed)
}

/// 弹窗、下拉菜单与 Toast 关闭退场的卸载延迟。
pub(super) const EXIT_DURATION: Duration = Duration::from_millis(220);
pub(super) const CONTENT_FADE_DURATION: Duration = Duration::from_millis(140);

const VALUE_EPSILON: f32 = 0.0008;
const VELOCITY_EPSILON: f32 = 0.005;

/// 弹簧刚度与阻尼参数。
#[derive(Clone, Copy, Debug)]
pub(super) struct SpringConfig {
    pub(super) stiffness: f32,
    pub(super) damping: f32,
}

impl SpringConfig {
    /// 控件快速就位，收尾只留一点弹性。
    pub(super) const SNAPPY: Self = Self {
        stiffness: 625.0,
        damping: 43.0,
    };
    /// 页面、卡片及弹层重排在同一节奏下收住。
    pub(super) const SMOOTH: Self = Self {
        stiffness: 484.0,
        damping: 44.0,
    };
    /// 临界阻尼快速退场（弹窗、下拉菜单、Toast 关闭前收拢至全透明）。
    pub(super) const EXIT: Self = Self {
        stiffness: 1600.0,
        damping: 80.0,
    };
    /// 舒缓的数据视觉过渡（图表柱高、环形图弧度）。
    pub(super) const GENTLE: Self = Self {
        stiffness: 360.0,
        damping: 38.0,
    };
    pub(super) const FEEDBACK: Self = Self {
        stiffness: 2304.0,
        damping: 96.0,
    };
    pub(super) const DRAWER: Self = Self {
        stiffness: 676.0,
        damping: 40.0,
    };
}

/// 统一的二次缓出曲线（用于柔和透明度渐隐/渐显）。
pub(super) fn ease_out_quad(t: f32) -> f32 {
    let clamped = t.clamp(0.0, 1.0);
    1.0 - (1.0 - clamped).powi(2)
}

pub(super) fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// 一维阻尼弹簧状态。
#[derive(Clone, Copy, Debug)]
pub(super) struct Spring {
    pub(super) value: f32,
    pub(super) velocity: f32,
}

impl Spring {
    pub(super) fn new(value: f32) -> Self {
        Self {
            value,
            velocity: 0.0,
        }
    }

    pub(super) fn snap(&mut self, value: f32) {
        self.value = value;
        self.velocity = 0.0;
    }

    #[cfg(test)]
    pub(super) fn step(&mut self, target: f32, dt_secs: f32, config: SpringConfig) -> bool {
        SpringStep::new(dt_secs, config).apply(self, target);
        if self.settled(target, None) {
            self.snap(target);
            false
        } else {
            true
        }
    }

    fn settled(&self, target: f32, tolerance: Option<f32>) -> bool {
        let distance = tolerance.unwrap_or(VALUE_EPSILON);
        let velocity = tolerance.map_or(VELOCITY_EPSILON, |pixels| pixels * 60.0);
        (self.value - target).abs() <= distance && self.velocity.abs() <= velocity
    }
}

/// Exact solution of the damped spring over one frame, shared by all channels.
struct SpringStep {
    position: f32,
    velocity: f32,
    cross: f32,
    stiffness_cross: f32,
}

impl SpringStep {
    fn new(dt: f32, config: SpringConfig) -> Self {
        let damping = config.damping * 0.5;
        let discriminant = config.stiffness - damping * damping;
        let (cosine, sine) = if discriminant > 0.0 {
            let frequency = discriminant.sqrt();
            let decay = (-damping * dt).exp();
            let (sine, cosine) = (frequency * dt).sin_cos();
            (decay * cosine, decay * sine / frequency)
        } else if discriminant < 0.0 {
            let frequency = (-discriminant).sqrt();
            // Evaluate the two decays directly to avoid overflows after a dropped frame.
            let slow = ((-damping + frequency) * dt).exp();
            let fast = ((-damping - frequency) * dt).exp();
            ((slow + fast) * 0.5, (slow - fast) / (2.0 * frequency))
        } else {
            let decay = (-damping * dt).exp();
            (decay, decay * dt)
        };
        Self {
            position: cosine + damping * sine,
            velocity: cosine - damping * sine,
            cross: sine,
            stiffness_cross: -config.stiffness * sine,
        }
    }

    fn apply(&self, spring: &mut Spring, target: f32) {
        let displacement = spring.value - target;
        spring.value = target + self.position * displacement + self.cross * spring.velocity;
        spring.velocity = self.stiffness_cross * displacement + self.velocity * spring.velocity;
    }
}

/// Blend premultiplied channels so transparent endpoints don't tint the fade.
pub(super) fn lerp_hsla(from: Hsla, to: Hsla, t: f32) -> Hsla {
    if t <= 0.0 {
        return from;
    }
    if t >= 1.0 {
        return to;
    }
    let a = Rgba::from(from);
    let b = Rgba::from(to);
    let from_weight = a.a * (1.0 - t);
    let to_weight = b.a * t;
    let alpha = from_weight + to_weight;
    if alpha == 0.0 {
        return from;
    }
    Hsla::from(Rgba {
        r: (a.r * from_weight + b.r * to_weight) / alpha,
        g: (a.g * from_weight + b.g * to_weight) / alpha,
        b: (a.b * from_weight + b.b * to_weight) / alpha,
        a: alpha,
    })
}

pub(super) struct MultiSpringState<const N: usize> {
    springs: [Spring; N],
    targets: [f32; N],
    last_frame: Instant,
}

impl<const N: usize> MultiSpringState<N> {
    pub(super) fn new(initial: [f32; N], targets: [f32; N], now: Instant) -> Self {
        Self {
            springs: initial.map(Spring::new),
            targets,
            last_frame: now,
        }
    }

    fn advance(
        &mut self,
        targets: [f32; N],
        now: Instant,
        config: SpringConfig,
        pixel_tolerance: Option<f32>,
    ) -> ([f32; N], bool) {
        self.advance_with_policy(targets, now, config, pixel_tolerance, false)
    }

    pub(super) fn targets(&self) -> [f32; N] {
        self.targets
    }

    pub(super) fn advance_with_policy(
        &mut self,
        targets: [f32; N],
        now: Instant,
        config: SpringConfig,
        pixel_tolerance: Option<f32>,
        snap_immediately: bool,
    ) -> ([f32; N], bool) {
        if snap_immediately {
            self.last_frame = now;
            self.targets = targets;
            for (spring, target) in self.springs.iter_mut().zip(targets) {
                spring.snap(target);
            }
            return (targets, false);
        }
        let elapsed = now.saturating_duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;
        let mut step = None;
        let mut active = false;
        for ((spring, previous), target) in self.springs.iter_mut().zip(self.targets).zip(targets) {
            // 上一帧与当前帧之间的时间增量仍属于上一帧的目标值，保持中途改向连续性。
            if elapsed > 0.0 && (spring.value != previous || spring.velocity != 0.0) {
                step.get_or_insert_with(|| SpringStep::new(elapsed, config))
                    .apply(spring, previous);
            }
            if spring.settled(target, pixel_tolerance) {
                spring.snap(target);
            } else {
                active = true;
            }
        }
        self.targets = targets;
        (self.springs.map(|spring| spring.value), active)
    }
}

pub(super) fn fade_in<E: Styled + IntoElement + 'static>(
    id: impl Into<ElementId>,
    duration: Duration,
    content: E,
) -> impl IntoElement {
    let reduced = is_reduced_motion();
    let start = if reduced { 0.86 } else { 0.72 };
    let effective = if reduced {
        Duration::from_millis((duration.as_millis() as u64).min(100))
    } else {
        duration
    };
    timeline(id, effective, move |progress| {
        content.opacity(start + ease_out_cubic(progress) * (1.0 - start))
    })
}

/// 为加载或刷新态图标提供平滑的连续旋转，启用“减少动态效果”时保持静止。
pub(super) fn spin_svg(id: impl Into<SharedString>, spinning: bool, svg: Svg) -> AnyElement {
    if !spinning || is_reduced_motion() {
        return svg.into_any_element();
    }
    timeline(id.into(), Duration::from_millis(880), move |phase| {
        svg.with_transformation(Transformation::rotate(radians(phase * TAU)))
    })
    .repeat()
    .into_any_element()
}

/// 多通道可中断弹簧驱动元素。
pub(super) struct SpringElement<const N: usize> {
    id: ElementId,
    initial: [f32; N],
    targets: [f32; N],
    config: SpringConfig,
    pixel_scale: Option<f32>,
    render: Option<Box<dyn FnOnce([f32; N]) -> AnyElement>>,
}

/// 首次挂载直接落位在 `target`，后续 `target` 变更时通过弹簧平滑追踪。
pub(super) fn spring_value<E: IntoElement + 'static>(
    id: impl Into<ElementId>,
    target: f32,
    config: SpringConfig,
    render: impl FnOnce(f32) -> E + 'static,
) -> SpringElement<1> {
    spring_from(id, target, target, config, render)
}

/// 首次挂载从 `initial` 启动并平滑过渡至 `target`，后续变更支持无缝反向打断。
pub(super) fn spring_from<E: IntoElement + 'static>(
    id: impl Into<ElementId>,
    initial: f32,
    target: f32,
    config: SpringConfig,
    render: impl FnOnce(f32) -> E + 'static,
) -> SpringElement<1> {
    SpringElement {
        id: id.into(),
        initial: [initial],
        targets: [target],
        config,
        pixel_scale: None,
        render: Some(Box::new(move |[value]| render(value).into_any_element())),
    }
}

/// 多值弹簧过渡（如环形图多扇区比例同步平滑变化）。
pub(super) fn spring_values<const N: usize, E: IntoElement + 'static>(
    id: impl Into<ElementId>,
    targets: [f32; N],
    config: SpringConfig,
    render: impl FnOnce([f32; N]) -> E + 'static,
) -> SpringElement<N> {
    SpringElement {
        id: id.into(),
        initial: targets,
        targets,
        config,
        pixel_scale: None,
        render: Some(Box::new(move |values| render(values).into_any_element())),
    }
}

/// 柱子首次从零高度升起，随后平滑追踪目标高度，并按屏幕像素精度停帧。
pub(super) fn chart_heights<const N: usize, E: IntoElement + 'static>(
    id: impl Into<ElementId>,
    targets: [f32; N],
    render: impl FnOnce([f32; N]) -> E + 'static,
) -> SpringElement<N> {
    let mut element = spring_values(id, targets, SpringConfig::GENTLE, render);
    element.initial = [0.0; N];
    element.settle_at_scale(1.0)
}

impl<const N: usize> SpringElement<N> {
    pub(super) fn settle_at_scale(mut self, pixels_per_unit: f32) -> Self {
        self.pixel_scale = Some(pixels_per_unit);
        self
    }
}

impl<const N: usize> IntoElement for SpringElement<N> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<const N: usize> Element for SpringElement<N> {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let initial = self.initial;
        let targets = self.targets;
        let config = self.config;
        let pixel_tolerance = self
            .pixel_scale
            .map(|scale| 0.5 / (window.scale_factor() * scale));
        let snap_immediately = is_reduced_motion();
        let (values, active) = window.with_element_state(
            global_id.unwrap(),
            |state: Option<MultiSpringState<N>>, window| {
                let now = frame_time(window);
                let mut state =
                    state.unwrap_or_else(|| MultiSpringState::new(initial, targets, now));
                (
                    state.advance_with_policy(
                        targets,
                        now,
                        config,
                        pixel_tolerance,
                        snap_immediately,
                    ),
                    state,
                )
            },
        );
        if active {
            request_frame(window);
        }
        let mut element = (self.render.take().expect("render called once"))(values);
        let layout_id = element.request_layout(window, cx);
        (layout_id, element)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.paint(window, cx);
    }
}

/// 无布局重排的滑入/滑出 + 透明度过渡元素（借助 `with_element_offset` 与 `opacity`）。
pub(super) struct SlideFadeElement<E> {
    id: ElementId,
    initial: f32,
    target: f32,
    offset: SlideOffset,
    config: SpringConfig,
    child: Option<E>,
}

enum SlideOffset {
    Fixed(Point<Pixels>),
    WindowRight,
}

impl SlideOffset {
    fn resolve(&self, bounds: Bounds<Pixels>, viewport_width: Pixels) -> Point<Pixels> {
        match self {
            Self::Fixed(offset) => *offset,
            // 按实际卡片宽度离场，并为阴影预留空间。
            Self::WindowRight => point(viewport_width - bounds.left() + px(16.0), px(0.0)),
        }
    }

    fn displacement(
        &self,
        bounds: Bounds<Pixels>,
        viewport_width: Pixels,
        remaining: f32,
    ) -> Point<Pixels> {
        let mut offset = self.resolve(bounds, viewport_width) * remaining;
        if matches!(self, Self::WindowRight) && remaining < 0.0 {
            // The rebound should feel the same even if the drawer is much wider.
            offset.x /= 1.0 + offset.x.abs() / px(6.0);
        }
        offset
    }
}

/// 为任意 `Styled` 元素附加可双向打断的位移 + 淡入淡出过渡。
/// `progress = 0.0` 时位于 `(offset_x, offset_y)` 且透明，`progress = 1.0` 时归位且完全显示。
pub(super) fn slide_fade<E: Styled + IntoElement + 'static>(
    id: impl Into<ElementId>,
    initial: f32,
    target: f32,
    offset_x: f32,
    offset_y: f32,
    config: SpringConfig,
    child: E,
) -> SlideFadeElement<E> {
    SlideFadeElement {
        id: id.into(),
        initial,
        target,
        offset: SlideOffset::Fixed(point(px(offset_x), px(offset_y))),
        config,
        child: Some(child),
    }
}

/// 从窗口右缘弹性滑入，关闭时滑回右侧。
pub(super) fn slide_fade_from_right<E: Styled + IntoElement + 'static>(
    id: impl Into<ElementId>,
    exiting: bool,
    child: E,
) -> SlideFadeElement<E> {
    let (target, config) = if exiting {
        (0.0, SpringConfig::EXIT)
    } else {
        (1.0, SpringConfig::DRAWER)
    };
    SlideFadeElement {
        id: id.into(),
        initial: 0.0,
        target,
        offset: SlideOffset::WindowRight,
        config,
        child: Some(child),
    }
}

impl<E: Styled + IntoElement + 'static> IntoElement for SlideFadeElement<E> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

pub(super) struct SlideFadeLayoutState {
    element: AnyElement,
    remaining: f32,
}

impl<E: Styled + IntoElement + 'static> Element for SlideFadeElement<E> {
    type RequestLayoutState = SlideFadeLayoutState;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let reduced = is_reduced_motion();
        let initial = self.initial;
        let target = self.target;
        let config = if reduced {
            SpringConfig::EXIT
        } else {
            self.config
        };
        let distance = match &self.offset {
            SlideOffset::Fixed(offset) => f32::from(offset.x.abs().max(offset.y.abs())),
            SlideOffset::WindowRight => f32::from(window.viewport_size().width),
        };
        let tolerance = 0.5 / (window.scale_factor() * distance.max(128.0));
        let ([progress], active) = window.with_element_state(
            global_id.unwrap(),
            |state: Option<MultiSpringState<1>>, window| {
                let now = frame_time(window);
                let mut state =
                    state.unwrap_or_else(|| MultiSpringState::new([initial], [target], now));
                (state.advance([target], now, config, Some(tolerance)), state)
            },
        );
        if active {
            request_frame(window);
        }
        let child = self.child.take().expect("child taken once");
        let mut element = child.opacity(progress.clamp(0.0, 1.0)).into_any_element();
        let layout_id = element.request_layout(window, cx);
        let remaining = if reduced { 0.0 } else { 1.0 - progress };
        (layout_id, SlideFadeLayoutState { element, remaining })
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
        let offset =
            self.offset
                .displacement(bounds, window.viewport_size().width, state.remaining);
        window.with_element_offset(offset, |window| {
            layout::with_slide_offset(offset, || state.element.prepaint(window, cx));
        });
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
        state.element.paint(window, cx);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TrackAxis {
    Horizontal,
    Vertical,
}

/// 滑动选中指示轨道的外观配置（用于分段控件与侧边栏导航）。
#[derive(Clone)]
pub(super) struct SlidingTrackStyle {
    pub(super) axis: TrackAxis,
    pub(super) gap: f32,
    pub(super) padding: f32,
    pub(super) track_bg: Option<Hsla>,
    pub(super) track_border: Option<Hsla>,
    pub(super) track_radius: f32,
    pub(super) pill_bg: Hsla,
    pub(super) pill_border: Hsla,
    pub(super) pill_radius: f32,
    pub(super) pill_shadow: Option<BoxShadow>,
}

struct SlidingTrackState {
    geometry: Option<MultiSpringState<4>>,
    opacity: MultiSpringState<1>,
}

pub(super) struct SlidingTrackLayout {
    child_layout_ids: Vec<LayoutId>,
}

pub(super) struct SlidingTrackPaint {
    pill_bounds: Option<Bounds<Pixels>>,
    alpha: f32,
}

/// 自适应子项尺寸的弹簧滑动选中指示容器。
pub(super) struct SlidingTrack {
    id: ElementId,
    selected: Option<usize>,
    style: SlidingTrackStyle,
    children: Vec<AnyElement>,
}

pub(super) fn sliding_track(
    id: impl Into<ElementId>,
    selected: Option<usize>,
    style: SlidingTrackStyle,
    children: Vec<AnyElement>,
) -> SlidingTrack {
    SlidingTrack {
        id: id.into(),
        selected,
        style,
        children,
    }
}

impl IntoElement for SlidingTrack {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SlidingTrack {
    type RequestLayoutState = SlidingTrackLayout;
    type PrepaintState = SlidingTrackPaint;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let child_layout_ids: Vec<LayoutId> = self
            .children
            .iter_mut()
            .map(|child| child.request_layout(window, cx))
            .collect();

        let mut style = Style::default();
        style.display = Display::Flex;
        style.flex_direction = match self.style.axis {
            TrackAxis::Horizontal => FlexDirection::Row,
            TrackAxis::Vertical => FlexDirection::Column,
        };
        if self.style.axis == TrackAxis::Horizontal {
            style.align_items = Some(AlignItems::Center);
        }
        style.gap = size(px(self.style.gap), px(self.style.gap)).into();
        let pad = DefiniteLength::Absolute(px(self.style.padding).into());
        style.padding = Edges::all(pad);
        if self.style.track_border.is_some() {
            style.border_widths = Edges::all(px(1.0).into());
        }
        if self.style.axis == TrackAxis::Vertical {
            style.size.width = Length::Definite(DefiniteLength::Fraction(1.0));
        }

        let layout_id = window.request_layout(style, child_layout_ids.iter().copied(), cx);
        (layout_id, SlidingTrackLayout { child_layout_ids })
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let target_rect = self
            .selected
            .and_then(|idx| layout.child_layout_ids.get(idx).copied())
            .map(|lid| {
                let b = window.layout_bounds(lid);
                [
                    f32::from(b.origin.x - bounds.origin.x),
                    f32::from(b.origin.y - bounds.origin.y),
                    f32::from(b.size.width),
                    f32::from(b.size.height),
                ]
            });

        let now = frame_time(window);
        let tolerance = 0.5 / window.scale_factor();
        let reduced = is_reduced_motion();
        let (pill_bounds, alpha, active) =
            window.with_element_state(global_id.unwrap(), |state: Option<SlidingTrackState>, _| {
                let opacity_target = [f32::from(target_rect.is_some())];
                let mut state = state.unwrap_or_else(|| SlidingTrackState {
                    geometry: None,
                    opacity: MultiSpringState::new(opacity_target, opacity_target, now),
                });
                let ([alpha], mut active) = state.opacity.advance_with_policy(
                    opacity_target,
                    now,
                    SpringConfig::FEEDBACK,
                    Some(0.006),
                    reduced,
                );
                let alpha = alpha.clamp(0.0, 1.0);
                if let Some(target) = target_rect {
                    state
                        .geometry
                        .get_or_insert_with(|| MultiSpringState::new(target, target, now));
                } else if alpha == 0.0 {
                    state.geometry = None;
                }
                let rect = state.geometry.as_mut().and_then(|geometry| {
                    let target = target_rect.unwrap_or_else(|| geometry.targets());
                    let ([x, y, w, h], moving) = geometry.advance_with_policy(
                        target,
                        now,
                        SpringConfig::SNAPPY,
                        Some(tolerance),
                        reduced,
                    );
                    active |= moving;
                    (alpha > 0.0 && w > 0.0 && h > 0.0).then(|| {
                        Bounds::new(bounds.origin + point(px(x), px(y)), size(px(w), px(h)))
                    })
                });
                ((rect, alpha, active), state)
            });

        if active {
            request_frame(window);
        }
        for child in &mut self.children {
            child.prepaint(window, cx);
        }
        SlidingTrackPaint { pill_bounds, alpha }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        paint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.style.track_bg.is_some() || self.style.track_border.is_some() {
            let bg = self.style.track_bg.unwrap_or_default();
            let border = self.style.track_border.unwrap_or_default();
            let bw = if self.style.track_border.is_some() {
                px(1.0)
            } else {
                px(0.0)
            };
            window.paint_quad(quad(
                bounds,
                Corners::all(px(self.style.track_radius)),
                bg,
                Edges::all(bw),
                border,
                BorderStyle::Solid,
            ));
        }

        if let Some(pill) = paint.pill_bounds {
            let alpha = paint.alpha;
            let radii = Corners::all(px(self.style.pill_radius));
            if let Some(mut shadow) = self.style.pill_shadow.clone() {
                shadow.color = shadow.color.opacity(shadow.color.a * alpha);
                window.paint_shadows(pill, radii, &[shadow]);
            }
            let bg = self.style.pill_bg.opacity(self.style.pill_bg.a * alpha);
            let border = self
                .style
                .pill_border
                .opacity(self.style.pill_border.a * alpha);
            window.paint_quad(quad(
                pill,
                radii,
                bg,
                Edges::all(px(1.0)),
                border,
                BorderStyle::Solid,
            ));
        }

        for child in &mut self.children {
            child.paint(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::rgb;

    #[test]
    fn spring_converges_smoothly_at_60hz_and_120hz() {
        for (config, dt) in [
            (SpringConfig::SNAPPY, 1.0 / 60.0),
            (SpringConfig::SNAPPY, 1.0 / 120.0),
            (SpringConfig::SMOOTH, 1.0 / 60.0),
            (SpringConfig::GENTLE, 1.0 / 120.0),
        ] {
            let mut spring = Spring::new(0.0);
            let mut steps = 0;
            while spring.step(1.0, dt, config) && steps < 400 {
                steps += 1;
            }
            assert!(steps > 2 && steps < 200, "unexpected step count {steps}");
            assert_eq!(spring.value, 1.0);
            assert_eq!(spring.velocity, 0.0);
        }
    }

    #[test]
    fn spring_interruption_reverses_continuously_without_jumping() {
        let mut spring = Spring::new(0.0);
        for _ in 0..4 {
            spring.step(1.0, 1.0 / 60.0, SpringConfig::SNAPPY);
        }
        let mid = spring.value;
        assert!(mid > 0.1 && mid < 0.95);
        spring.step(0.0, 1.0 / 60.0, SpringConfig::SNAPPY);
        assert!((spring.value - mid).abs() < 0.35);
        let mut steps = 0;
        while spring.step(0.0, 1.0 / 60.0, SpringConfig::SNAPPY) && steps < 200 {
            steps += 1;
        }
        assert_eq!(spring.value, 0.0);
    }

    #[test]
    fn spring_uses_elapsed_time_across_refresh_rates_and_frame_hitches() {
        let mut spring = Spring::new(0.0);
        spring.step(1.0, 5.0, SpringConfig::SNAPPY);
        assert_eq!((spring.value, spring.velocity), (1.0, 0.0));

        for config in [
            SpringConfig::DRAWER,
            SpringConfig::SMOOTH,
            SpringConfig {
                stiffness: 256.0,
                damping: 48.0,
            },
        ] {
            let mut uninterrupted = Spring {
                value: -1.0,
                velocity: 12.0,
            };
            SpringStep::new(0.125, config).apply(&mut uninterrupted, 1.0);
            for frames in [4, 8, 16] {
                let mut sampled = Spring {
                    value: -1.0,
                    velocity: 12.0,
                };
                let step = SpringStep::new(0.125 / frames as f32, config);
                for _ in 0..frames {
                    step.apply(&mut sampled, 1.0);
                }
                assert!((sampled.value - uninterrupted.value).abs() < 0.00001);
                assert!((sampled.velocity - uninterrupted.velocity).abs() < 0.0001);
            }
        }
    }

    #[test]
    fn retargeting_after_idle_starts_from_the_previous_value() {
        let now = Instant::now();
        let clicked_at = now + Duration::from_secs(2);
        let mut state = MultiSpringState::new([0.0], [0.0], now);

        let first = state.advance([1.0], clicked_at, SpringConfig::SNAPPY, None);
        assert_eq!(first, ([0.0], true));
        let ([second], active) = state.advance(
            [1.0],
            clicked_at + Duration::from_secs_f32(1.0 / 60.0),
            SpringConfig::SNAPPY,
            None,
        );
        assert!(active && second > 0.0 && second < 0.1);
    }

    #[test]
    fn chart_height_stops_at_half_a_physical_pixel() {
        let now = Instant::now();
        let mut state = MultiSpringState::new([0.0], [146.0], now);
        let tolerance = 0.5;
        for frame in 1..=40 {
            let ([height], active) = state.advance(
                [146.0],
                now + Duration::from_secs_f32(frame as f32 / 60.0),
                SpringConfig::GENTLE,
                Some(tolerance),
            );
            if !active {
                assert_eq!(height, 146.0);
                assert!((18..=32).contains(&frame));
                return;
            }
        }
        panic!("bar kept requesting frames after its height was visually settled");
    }

    #[test]
    fn chart_bars_grow_when_a_plot_is_first_shown() {
        let chart = chart_heights("test-chart-height", [146.0, 48.0], |_| gpui::div());
        let now = Instant::now();
        let mut state = MultiSpringState::new(chart.initial, chart.targets, now);
        let tolerance = Some(0.5);

        let first_frame = state.advance(chart.targets, now, chart.config, tolerance);
        assert_eq!(first_frame, ([0.0, 0.0], true));
        let (heights, active) = state.advance(
            chart.targets,
            now + Duration::from_millis(16),
            chart.config,
            tolerance,
        );
        assert!(active);
        for (height, target) in heights.into_iter().zip(chart.targets) {
            assert!(height > 0.5 && height < target);
        }
    }

    #[test]
    fn lerp_hsla_blends_linearly_in_rgba() {
        let off: Hsla = rgb(0xd9d9d9).into();
        let on: Hsla = rgb(0xfd4b23).into();
        let mid = Rgba::from(lerp_hsla(off, on, 0.5));
        let expected_r = (0xd9 as f32 / 255.0 + 0xfd as f32 / 255.0) * 0.5;
        assert!((mid.r - expected_r).abs() < 0.01);
        assert_eq!(lerp_hsla(off, on, 0.0), off);
        assert_eq!(lerp_hsla(off, on, 1.0), on);
    }

    #[test]
    fn transparent_fills_keep_their_visible_hue() {
        let solid: Hsla = rgb(0xe8edff).into();
        let mid = Rgba::from(lerp_hsla(Hsla::default().opacity(0.0), solid, 0.5));
        let solid = Rgba::from(solid);
        assert!((mid.a - 0.5).abs() < 0.001);
        assert!((mid.r - solid.r).abs() < 0.001);
        assert!((mid.g - solid.g).abs() < 0.001);
        assert!((mid.b - solid.b).abs() < 0.001);
    }

    #[test]
    fn exit_spring_settles_before_unmount_deadline() {
        let mut spring = Spring::new(1.0);
        let frames = (EXIT_DURATION.as_secs_f32() * 60.0).floor() as usize;
        for _ in 0..frames {
            spring.step(0.0, 1.0 / 60.0, SpringConfig::EXIT);
        }
        assert!(
            spring.value.abs() < 0.012,
            "exit spring still at {} before unmount",
            spring.value
        );
    }

    #[test]
    fn toast_bounces_in_from_right_and_clears_the_window_on_exit() {
        let enter = slide_fade_from_right("toast-enter", false, gpui::div());
        let exit = slide_fade_from_right("toast-exit", true, gpui::div());
        let window_right = px(1120.0);
        for width in [120.0, 420.0] {
            let bounds = Bounds::new(
                point(window_right - px(width + 26.0), px(60.0)),
                size(px(width), px(48.0)),
            );
            let travel = enter.offset.resolve(bounds, window_right);
            assert_eq!(travel.y, px(0.0));
            assert!(bounds.left() + travel.x > window_right);

            for refresh_rate in [30.0, 60.0, 120.0] {
                let mut spring = Spring::new(enter.initial);
                let dt = 1.0 / refresh_rate;
                let mut peak: f32 = 0.0;
                for _ in 0..(0.5 * refresh_rate) as usize {
                    spring.step(enter.target, dt, enter.config);
                    peak = peak.max(spring.value);
                }
                assert!((px(2.0)..px(12.0)).contains(&(travel.x * (peak - 1.0))));
                let rebound = enter.offset.displacement(bounds, window_right, 1.0 - peak);
                assert!((px(-6.0)..px(0.0)).contains(&rebound.x));
                assert_eq!(spring.value, enter.target);

                // 首帧收到关闭目标，余下帧完成位移。
                let frames = (EXIT_DURATION.as_secs_f32() * refresh_rate) as usize;
                for _ in 1..frames {
                    spring.step(exit.target, dt, exit.config);
                }
                assert!(bounds.left() + travel.x * (1.0 - spring.value) >= window_right);
            }
        }
    }

    #[test]
    fn reduced_motion_snaps_spatial_spring_immediately() {
        let now = Instant::now();
        let mut state = MultiSpringState::new([0.0], [0.0], now);
        let (values, active) = state.advance_with_policy(
            [1.0],
            now + Duration::from_millis(16),
            SpringConfig::SNAPPY,
            None,
            true,
        );
        assert_eq!(values, [1.0]);
        assert!(!active);
    }
}
