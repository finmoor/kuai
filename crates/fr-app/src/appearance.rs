//! 主题和窗口位置：按设置应用深色、浅色主题；记住窗口上次的位置和大小。

use std::rc::Rc;

use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry};
use gpui_kit::*;

use crate::palette::{self, Palette};
use crate::settings::{Settings, ThemeChoice, WindowState};

/// 现在该用哪套配色：先定深色还是浅色（「跟随系统」看 Windows 的设置），再取这个明暗下选的那套。
pub fn current_palette(settings: &Settings, window: &Window) -> &'static Palette {
    let dark = match settings.theme {
        ThemeChoice::Dark => true,
        ThemeChoice::Light => false,
        ThemeChoice::System => matches!(
            window.appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ),
    };
    let id = if dark {
        &settings.dark_theme
    } else {
        &settings.light_theme
    };
    palette::find(id, dark)
}

/// 应用主题。
pub fn apply_theme(settings: &Settings, window: &mut Window, cx: &mut App) {
    let palette = current_palette(settings, window);
    let mut config = palette.config();
    // 语法高亮沿用组件库自带的深色、浅色两套，只把底色和行号换成这套配色的
    let registry = ThemeRegistry::global(cx);
    let base = if palette.dark {
        registry.default_dark_theme()
    } else {
        registry.default_light_theme()
    };
    let mut highlight = base.highlight.clone().unwrap_or_default();
    let color = |hex: &str| gpui_kit::component::try_parse_color(hex).ok();
    highlight.editor_background = color(palette.bg);
    highlight.editor_foreground = color(palette.fg);
    highlight.editor_line_number = color(&palette.subtle());
    highlight.editor_active_line_number = color(palette.muted);
    highlight.editor_active_line = color(palette.hover);
    highlight.editor_gutter_background = color(palette.bg);
    config.highlight = Some(highlight);
    let config = Rc::new(config);
    let mode = if palette.dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    if !cx.has_global::<Theme>() {
        Theme::change(mode, Some(window), cx);
    }
    Theme::update(cx, |theme| {
        theme.apply_config(&config);
        theme.mode = mode;
        // 通知放右下角：默认的右上角会盖住工具栏和「继续 / 中止」的横幅
        theme.notification.placement = Anchor::BottomRight;
        // 窗口缩到最小宽度（900）时，通知的左边不能压到提交按钮（右边缘在 570 处）
        theme.notification.width = px(312.);
    });
    crate::win::set_dark_frame(window, palette.dark);
    window.refresh();
}

/// 窗口现在的位置和大小，存进设置用。
pub fn window_state(window: &Window) -> WindowState {
    let (bounds, maximized) = match window.window_bounds() {
        WindowBounds::Windowed(bounds) => (bounds, false),
        WindowBounds::Maximized(bounds) | WindowBounds::Fullscreen(bounds) => (bounds, true),
    };
    WindowState {
        x: f32::from(bounds.origin.x),
        y: f32::from(bounds.origin.y),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
        maximized,
    }
}

/// 没有记录时窗口的大小；屏幕放不下就缩到屏幕那么大。
const DEFAULT_SIZE: (f32, f32) = (1400., 900.);
/// 窗口最小能缩到多小。再小的话仓库页的三栏（侧栏、文件列表、改动）排不下：
/// 640×420 时改动区只剩一条缝，提交列表的说明一栏被挤没。
pub const MIN_SIZE: (f32, f32) = (900., 560.);
/// 窗口上下要让出的位置。标题栏是自己画在内容区里的，窗口看得见的部分就是内容区；
/// 但 GPUI 把我们给的矩形换成窗口矩形时，会把下边框的厚度上下各分一半（见 gpui 的
/// `calculate_window_rect`），内容区实际比给的位置高出半个边框：100% 缩放下 4 个逻辑像素，
/// 200% 下 3 个。所以可用区域上下各让出 4，左右不用让（左右的边框是看不见的拉伸区，
/// 伸到屏幕外面没有关系）。
const FRAME: (f32, f32) = (0., 4.);

/// 开窗口时用的位置：上次记下的位置还在某块屏幕上就用它，否则在主屏幕居中。
/// 两种情况都收进那块屏幕的可用区域（去掉任务栏）：窗口比屏幕大就缩小，伸到外面就挪回来，
/// 不然换到小屏幕（笔记本、远程桌面）上标题栏会跑到屏幕外面，拖不动也关不了。
pub fn initial_bounds(saved: Option<WindowState>, cx: &App) -> WindowBounds {
    // 每块屏幕：去掉任务栏的区域，和里面能放内容区的部分
    let areas: Vec<Bounds<Pixels>> = cx
        .displays()
        .iter()
        .map(|display| display.visible_bounds())
        .collect();
    let primary = cx
        .primary_display()
        .map(|display| content_area(display.visible_bounds()))
        .or_else(|| areas.first().map(|area| content_area(*area)));
    let Some(primary) = primary else {
        // 问不到屏幕（不该发生）：交给系统摆
        let default = Bounds::centered(None, size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)), cx);
        return WindowBounds::Windowed(default);
    };
    let default = centered_in(primary, size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)));
    let Some(state) = saved else {
        return WindowBounds::Windowed(default);
    };
    let bounds = Bounds {
        origin: point(px(state.x), px(state.y)),
        size: size(
            px(state.width.max(MIN_SIZE.0)),
            px(state.height.max(MIN_SIZE.1)),
        ),
    };
    let bounds = restore(bounds, &areas, default);
    if state.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    }
}

/// 记下的窗口位置放到现在的这些屏幕（各自的可用区域）上：标题栏落在哪块屏幕里就收进哪块；
/// 哪块都不沾（比如拔掉了外接显示器）就用默认位置。
fn restore(
    saved: Bounds<Pixels>,
    screens: &[Bounds<Pixels>],
    default: Bounds<Pixels>,
) -> Bounds<Pixels> {
    let title_bar = Bounds {
        origin: saved.origin,
        size: size(saved.size.width, px(40.)),
    };
    match screens.iter().find(|area| area.intersects(&title_bar)) {
        Some(area) => fit(saved, content_area(*area)),
        None => default,
    }
}

/// 屏幕的可用区域里能放内容区的部分：上下让出半个边框。
fn content_area(area: Bounds<Pixels>) -> Bounds<Pixels> {
    let (dx, dy) = (
        px(FRAME.0).min(area.size.width / 4.),
        px(FRAME.1).min(area.size.height / 4.),
    );
    Bounds {
        origin: point(area.origin.x + dx, area.origin.y + dy),
        size: size(area.size.width - dx * 2., area.size.height - dy * 2.),
    }
}

/// 在一块区域里居中放一个这么大的窗口；区域放不下就缩到区域那么大。
fn centered_in(area: Bounds<Pixels>, wanted: Size<Pixels>) -> Bounds<Pixels> {
    let width = wanted.width.min(area.size.width);
    let height = wanted.height.min(area.size.height);
    Bounds {
        origin: point(
            area.origin.x + (area.size.width - width) / 2.,
            area.origin.y + (area.size.height - height) / 2.,
        ),
        size: size(width, height),
    }
}

/// 把窗口收进一块区域：比它大就缩到它那么大，伸到外面就挪回来。
fn fit(window: Bounds<Pixels>, area: Bounds<Pixels>) -> Bounds<Pixels> {
    let width = window.size.width.min(area.size.width);
    let height = window.size.height.min(area.size.height);
    let x = window
        .origin
        .x
        .clamp(area.origin.x, area.origin.x + area.size.width - width);
    let y = window
        .origin
        .y
        .clamp(area.origin.y, area.origin.y + area.size.height - height);
    Bounds {
        origin: point(x, y),
        size: size(width, height),
    }
}

#[cfg(test)]
mod tests {
    use super::{centered_in, content_area, fit, restore};
    use gpui_kit::{Bounds, Pixels, point, px, size};

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(w), px(h)),
        }
    }

    #[test]
    fn 默认窗口比屏幕大时缩到屏幕那么大() {
        // 1280×752 的可用区域（2560×1600 的屏幕 200% 缩放，去掉任务栏）放不下 1400×900
        let area = rect(0., 0., 1280., 752.);
        assert_eq!(centered_in(area, size(px(1400.), px(900.))), area);
        // 放得下就居中
        let big = rect(0., 0., 2560., 1392.);
        assert_eq!(
            centered_in(big, size(px(1400.), px(900.))),
            rect(580., 246., 1400., 900.)
        );
    }

    #[test]
    fn 可用区域上下让出半个边框() {
        assert_eq!(
            content_area(rect(0., 0., 1280., 752.)),
            rect(0., 4., 1280., 744.)
        );
    }

    #[test]
    fn 记下的位置收进现在的屏幕() {
        let area = rect(0., 0., 1280., 752.);
        // 在大屏幕上记下的 1400×885，换到小屏幕上
        assert_eq!(fit(rect(46., 0., 1400., 885.), area), area);
        // 大小放得下，只是伸到了右下角外面：挪回来，大小不变
        assert_eq!(
            fit(rect(900., 500., 800., 600.), area),
            rect(480., 152., 800., 600.)
        );
        // 本来就在里面的不动
        assert_eq!(
            fit(rect(100., 50., 800., 600.), area),
            rect(100., 50., 800., 600.)
        );
        // 第二块屏幕在左边（坐标是负的）
        let left = rect(-1920., 0., 1920., 1040.);
        assert_eq!(
            fit(rect(-2000., -30., 1000., 700.), left),
            rect(-1920., 0., 1000., 700.)
        );
    }

    #[test]
    fn 多块屏幕时回到记下的那一块() {
        // 主屏幕 1280×752，右边接了一块 1920×1040
        let screens = [rect(0., 0., 1280., 752.), rect(1280., 0., 1920., 1040.)];
        let default = rect(0., 4., 1280., 744.);
        // 记在右边屏幕上的：还在右边，位置大小不变
        assert_eq!(
            restore(rect(1500., 100., 1000., 700.), &screens, default),
            rect(1500., 100., 1000., 700.)
        );
        // 记在右边、比右边屏幕还大的：缩到右边屏幕放内容区的部分
        assert_eq!(
            restore(rect(1300., 10., 2400., 1300.), &screens, default),
            rect(1280., 4., 1920., 1032.)
        );
        // 外接屏幕拔掉了：记下的位置哪块屏幕都不沾，回到主屏幕的默认位置
        assert_eq!(
            restore(rect(1500., 100., 1000., 700.), &screens[..1], default),
            default
        );
        // 标题栏跑到所有屏幕上面去了（只有窗口下半截还在屏幕里）：也回到默认位置
        assert_eq!(
            restore(rect(100., -400., 800., 600.), &screens, default),
            default
        );
    }
}
