//! 各处共用的小部件和尺寸：图标、头像、计数的小圆角块、分组标题。
//!
//! 界面的层级靠三样东西：文字的三级颜色（正文、次要、最弱）、主色只用在「现在选中的」和
//! 「主要操作」上、同一类东西用同一个行高和圆角。这里把它们集中定下来，各个页面不各写各的。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

/// 行和小块的圆角。
pub const RADIUS: f32 = 6.;

/// 一个图标，按 14 像素画（和 13、14 号字的 x 高度配得上）。
pub fn icon(name: IconName) -> Icon {
    Icon::new(name).size_3p5()
}

/// 小一号的图标：行尾的关闭、展开箭头。
pub fn icon_small(name: IconName) -> Icon {
    Icon::new(name).xsmall()
}

/// 一行放不下就省略的文字，鼠标停上去显示全文。只在真的被截掉时才弹，放得下的不弹。
///
/// 返回的块上可以接着设字号、颜色、`flex_1()`；省略开头、中间的接 `text_ellipsis_start()`、
/// `text_ellipsis_middle()`。不要在它上面加内边距：是拿这一块的宽度和文字的宽度比的。
pub fn clipped(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    let text: SharedString = text.into();
    clipped_with_tip(id, text.clone(), text)
}

/// 同 [`clipped`]，提示里显示的是 `tip`（比如行里是文件名，提示给完整路径）。
pub fn clipped_with_tip(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    tip: impl Into<SharedString>,
) -> Stateful<Div> {
    let (text, tip): (SharedString, SharedString) = (text.into(), tip.into());
    let cut = Rc::new(Cell::new(false));
    // 量一下：这一块有多宽，文字不省略要多宽。画在这一块里面，拿得到它的字体和字号
    let measure = {
        let (text, cut) = (text.clone(), cut.clone());
        canvas(
            move |bounds, window, _| {
                let style = window.text_style();
                let size = style.font_size.to_pixels(window.rem_size());
                let line: SharedString = if text.contains('\n') {
                    text.replace('\n', " ").into()
                } else {
                    text
                };
                let run = style.to_run(line.len());
                let width = window
                    .text_system()
                    .shape_line(line, size, &[run], None)
                    .width;
                cut.set(width > bounds.size.width + px(0.5));
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0()
    };
    div()
        .id(id)
        .relative()
        .min_w_0()
        .truncate()
        .child(text)
        .child(measure)
        .tooltip(move |window, cx| {
            if cut.get() {
                full_text_tip(tip.clone(), window, cx)
            } else {
                // 没被截掉
                no_tip(cx)
            }
        })
}

/// 显示全文的提示：太长就折行，不撑出窗口。
pub fn full_text_tip(text: SharedString, window: &mut Window, cx: &mut App) -> AnyView {
    if menu_is_open(window) {
        return no_tip(cx);
    }
    Tooltip::element(move |_, _| {
        // 文字里的换行各占一行
        v_flex()
            .max_w(px(560.))
            .whitespace_normal()
            .children(text.split('\n').map(|line| div().child(line.to_string())))
    })
    .build(window, cx)
}

/// 两行的提示：上面一行正文，下面一行小一号的次要文字（名字和路径）。
pub fn two_line_tip(
    title: SharedString,
    sub: SharedString,
    window: &mut Window,
    cx: &mut App,
) -> AnyView {
    if menu_is_open(window) {
        return no_tip(cx);
    }
    let muted = cx.theme().muted_foreground;
    Tooltip::element(move |_, _| {
        v_flex()
            .max_w(px(560.))
            .whitespace_normal()
            .child(title.clone())
            .child(div().text_xs().text_color(muted).child(sub.clone()))
    })
    .build(window, cx)
}

/// 主窗口根上放一个：右键菜单弹出来时，不让悬停提示盖在它上面。配合 [`menu_is_open`] 用。
///
/// gpui 的提示排上队以后，半秒到了就弹，那时只看鼠标还在不在元素的矩形里，不管上面是不是
/// 已经盖了菜单（gpui `div.rs` 里 `handle_tooltip_mouse_move` 的 TODO 说的就是这个）。
/// 鼠标挪到标签页上马上点右键，提示正好盖住菜单最上面几项。按下鼠标只收掉已经显示的提示，
/// 排着队的只有「鼠标移走了」才取消，所以按下右键时补发一次移到窗口外的鼠标移动。松开右键时
/// gpui 会把鼠标位置记回真实的，不用再补一次移回去。
///
/// 监听直接挂在窗口上，不看鼠标在哪个元素上：标题栏里的标签页挡着下层的悬停判断（`occlude`，
/// 不然点它会变成拖窗口），根元素的 `capture_any_mouse_down` 在标签页上收不到。
pub fn tooltip_guard() -> impl IntoElement {
    canvas(
        |_, _, _| {},
        |_, _, window, _| {
            window.on_mouse_event(|event: &MouseDownEvent, phase, window, cx| {
                // 正拖着东西的时候不插手
                if phase != DispatchPhase::Capture
                    || event.button != MouseButton::Right
                    || cx.has_active_drag()
                {
                    return;
                }
                let modifiers = window.modifiers();
                // 等这次按下处理完再发，不在事件处理中间插一个事件
                window.defer(cx, move |window, cx| {
                    window.dispatch_event(
                        PlatformInput::MouseMove(MouseMoveEvent {
                            position: point(px(-1.), px(-1.)),
                            pressed_button: None,
                            modifiers,
                        }),
                        cx,
                    );
                });
            });
        },
    )
    .absolute()
    .size_0()
}

/// 有弹出菜单开着（右键菜单、下拉菜单）。这时不弹悬停提示：菜单贴着鼠标弹出来，鼠标稍微一动
/// 又回到下面的元素上，提示就会盖住菜单最上面几项。菜单开着时焦点在它身上，看焦点一路上的
/// 键位上下文里有没有组件库菜单的那个名字。
fn menu_is_open(window: &Window) -> bool {
    window
        .context_stack()
        .iter()
        .any(|context| context.contains("PopupMenu"))
}

/// 什么都不画的提示：该弹的时候发现不用弹（文字没被截掉、有菜单开着）。
fn no_tip(cx: &mut App) -> AnyView {
    cx.new(|_| Empty).into()
}

/// 一行字的提示。自己写 `.tooltip(闭包)` 的地方都用 [`tip`]、[`full_text_tip`]、[`two_line_tip`]，
/// 不直接用 `Tooltip::new`：这三个在有菜单开着时不弹。
pub fn tip(text: impl Into<SharedString>, window: &mut Window, cx: &mut App) -> AnyView {
    if menu_is_open(window) {
        return no_tip(cx);
    }
    Tooltip::new(text.into()).build(window, cx)
}

/// 计数的小块：侧栏「本地修改」后面的数字、分组里有几项。
pub fn count_pill(count: usize, strong: bool, cx: &App) -> Div {
    let theme = cx.theme();
    let (bg, fg) = if strong {
        (theme.primary, theme.primary_foreground)
    } else {
        (theme.muted, theme.muted_foreground)
    };
    div()
        .flex_none()
        .px(px(6.))
        .h(px(16.))
        .min_w(px(18.))
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_size(px(10.5))
        .line_height(px(16.))
        .font_weight(FontWeight::MEDIUM)
        .text_center()
        .child(count.to_string())
}

/// 按名字生成的头像：一个带颜色的圆，里面是名字的首字。同一个名字颜色不变。
pub fn avatar(name: &str, size: f32, cx: &App) -> Div {
    let theme = cx.theme();
    let colors = [
        theme.blue,
        theme.green,
        theme.magenta,
        theme.cyan,
        theme.yellow,
        theme.red,
    ];
    let color = colors[avatar_slot(name, colors.len())];
    div()
        .flex_none()
        .size(px(size))
        .rounded_full()
        .bg(color.opacity(0.22))
        .text_color(color)
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(size * 0.5))
        .line_height(px(size))
        .font_weight(FontWeight::SEMIBOLD)
        .child(initial(name))
}

/// 名字落在第几个颜色上。
fn avatar_slot(name: &str, slots: usize) -> usize {
    // FNV-1a：同一个名字每次、每台电脑上算出来都一样
    let mut hash: u32 = 0x811c9dc5;
    for byte in name.trim().to_lowercase().bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash as usize % slots.max(1)
}

/// 头像里的字：英文名取首字母（大写），中文名取第一个字。
fn initial(name: &str) -> String {
    name.trim()
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".into())
}

#[cfg(test)]
mod tests {
    use super::{avatar_slot, initial};

    #[test]
    fn 头像取名字的首字() {
        assert_eq!(initial("feng"), "F");
        assert_eq!(initial("  张三"), "张");
        assert_eq!(initial("<bot>"), "B");
        assert_eq!(initial(""), "?");
    }

    #[test]
    fn 同一个名字颜色不变() {
        assert_eq!(avatar_slot("Feng", 6), avatar_slot("feng ", 6));
        assert!(avatar_slot("别人", 6) < 6);
        assert_eq!(avatar_slot("x", 0), 0);
    }
}
