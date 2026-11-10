//! 主题和窗口位置：按设置应用深色、浅色主题；记住窗口上次的位置和大小。

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::*;

use crate::settings::{ThemeChoice, WindowState};

/// 应用主题。「跟随系统」时按 Windows 当前的深色、浅色设置。
pub fn apply_theme(choice: ThemeChoice, window: &mut Window, cx: &mut App) {
    match choice {
        ThemeChoice::System => Theme::sync_system_appearance(Some(window), cx),
        ThemeChoice::Light => Theme::change(ThemeMode::Light, Some(window), cx),
        ThemeChoice::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
    }
    // 通知放右下角：默认的右上角会盖住工具栏和「继续 / 中止」的横幅
    Theme::global_mut(cx).notification.placement = Anchor::BottomRight;
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

/// 开窗口时用的位置：上次记下的位置还在某块屏幕上就用它，否则在主屏幕居中。
pub fn initial_bounds(saved: Option<WindowState>, cx: &App) -> WindowBounds {
    let default = Bounds::centered(None, size(px(1400.), px(900.)), cx);
    let Some(state) = saved else {
        return WindowBounds::Windowed(default);
    };
    let bounds = Bounds {
        origin: point(px(state.x), px(state.y)),
        size: size(px(state.width.max(640.)), px(state.height.max(420.))),
    };
    // 屏幕换了（比如拔掉了外接显示器）：窗口的标题栏至少要有一块落在某块屏幕里
    let title_bar = Bounds {
        origin: bounds.origin,
        size: size(bounds.size.width, px(40.)),
    };
    let visible = cx
        .displays()
        .iter()
        .any(|display| display.bounds().intersects(&title_bar));
    let bounds = if visible { bounds } else { default };
    if state.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    }
}
