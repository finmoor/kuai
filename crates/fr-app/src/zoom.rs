//! 代码字号：改动、合并编辑器、文件页的编辑器里代码的字号，设置里改，也可以在改动上按住
//! Ctrl 滚滚轮调（往上大、往下小）。组件库的改动视图和编辑器按主题里的 `mono_font_size` 排，我们自己
//! 画的改动行（本地修改、合并编辑器的对照）按 [`code_size`]。

use gpui_kit::component::{ActiveTheme as _, Theme};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::settings::{CODE_SIZES, Settings};

/// 现在的代码字号（像素）。
pub fn code_size(cx: &App) -> f32 {
    f32::from(cx.theme().mono_font_size)
}

/// 把设置里的代码字号、Tab 宽度用上（启动时、设置变了、换了主题以后）。
pub fn apply(settings: &Settings, cx: &mut App) {
    let size = settings
        .code_font_size
        .clamp(*CODE_SIZES.start(), *CODE_SIZES.end());
    Theme::update(cx, |theme| theme.mono_font_size = px(f32::from(size)));
    crate::text::set_tab_width(usize::from(settings.tab_width));
}

/// 盖在改动区上面、不挡鼠标的一层：按住 Ctrl 滚滚轮时调代码字号，这一下不再往下传（下面的列表
/// 不跟着滚）。放进一个 `relative()` 的容器里，铺满它。
pub fn wheel_zoom(app: Entity<AppState>) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let app = app.clone();
            window.on_mouse_event(move |event: &ScrollEvent, phase, _, cx| {
                let ctrl = event.modifiers.control || event.modifiers.platform;
                if phase != DispatchPhase::Capture || !ctrl || !bounds.contains(&event.position) {
                    return;
                }
                cx.stop_propagation();
                let dy = match event.delta {
                    ScrollDelta::Lines(delta) => delta.y,
                    ScrollDelta::Pixels(delta) => f32::from(delta.y),
                };
                if dy == 0. {
                    return;
                }
                app.update(cx, |app, cx| {
                    let now = app.settings.code_font_size;
                    let next = match dy > 0. {
                        true => now.saturating_add(1),
                        false => now.saturating_sub(1),
                    }
                    .clamp(*CODE_SIZES.start(), *CODE_SIZES.end());
                    if next != now {
                        app.update_settings(cx, |s| s.code_font_size = next);
                    }
                });
            });
        },
    )
    .absolute()
    .inset_0()
}
