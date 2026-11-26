//! 并排查看几个终端（会话页、仓库终端页；`docs/design/sessions-status/` 第 5 节方向 C）：
//! 侧栏里勾上 2～4 个，点「并排查看」；或者右键一个终端「与「…」并排」、点工具栏的「并排」挑一个，
//! 直接并排（并排着时用「添加」、右键「加入并排」再加，见 `docs/design/sandbox-1126/` 第 2 题方向 B）。
//! 2 个左右分，3 个左边一大、右边两小，4 个四格；双击格头放大、
//! 再双击还原；格头的 × 把它移出，只剩一个时回到单个。焦点格蓝框，输入只进焦点格（点格子、
//! Ctrl+Alt+方向键换）。
//!
//! 这里只管排版、格头和底下那一条的样子；哪几个、焦点在哪由各页自己记。

use std::rc::Rc;

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::ui;

/// 最多几格
pub const MAX_CELLS: usize = 4;
/// 至少几格（少了就不是并排）
pub const MIN_CELLS: usize = 2;
/// 格子之间的空
const GAP: f32 = 6.;
/// 格头的高度
const HEAD: f32 = 26.;
/// 格子比这小时底下提示放大了再操作（宽、高）
const SMALL: (f32, f32) = (420., 260.);

pub type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

/// 菜单里写的终端名字（「与「…」并排」）：太长的截短。
pub fn menu_name(name: &str) -> SharedString {
    const MAX: usize = 24;
    match name.chars().count() > MAX {
        true => format!("{}…", name.chars().take(MAX).collect::<String>()).into(),
        false => name.to_string().into(),
    }
}

/// 挑一个终端来并排的菜单里的一项：左边名字，右边状态。
pub fn choice(name: SharedString, state: SharedString, color: Hsla) -> PopupMenuItem {
    PopupMenuItem::element(move |_, _| {
        h_flex()
            .w_full()
            .gap(px(16.))
            .child(div().flex_1().min_w_0().truncate().child(name.clone()))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(color)
                    .child(state.clone()),
            )
    })
}

/// 并排时上面那一条里的「添加」：再挑一个终端加进来（点开的菜单由各页接上）。满了（`full`）、没有
/// 别的终端（`more` 为假）时点不了，悬停说为什么。
pub fn add_button(id: &'static str, full: bool, more: bool) -> Button {
    let button = Button::new(id)
        .small()
        .flex_none()
        .icon(ui::icon(IconName::Plus))
        .label(tr("添加"));
    match (full, more) {
        (true, _) => button
            .disabled(true)
            .tooltip(tf!("最多并排查看 {n} 个终端", n = MAX_CELLS.to_string())),
        (_, false) => button.disabled(true).tooltip(tr("没有其他终端可并排查看")),
        _ => button,
    }
}

/// Ctrl+Alt+方向键：换到哪一格（`n` 格，现在是 `at`）。不是这几个键是 None。
pub fn step(key: &str, at: usize, n: usize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let forward = match key {
        "right" | "down" => true,
        "left" | "up" => false,
        _ => return None,
    };
    Some(match forward {
        true => (at + 1) % n,
        false => (at + n - 1) % n,
    })
}

/// 按格数排好：2 个左右分，3 个左边一大、右边两小，4 个四格。每一格要能伸缩（`flex_1`、`min_w_0`、
/// `min_h_0`）。
pub fn grid(mut cells: Vec<AnyElement>) -> Div {
    // 不用 h_flex：它把孩子竖着居中，格子只剩格头那么高；这里要撑满
    let col = || {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .gap(px(GAP))
    };
    let row = || {
        div()
            .flex()
            .flex_row()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .gap(px(GAP))
    };
    match cells.len() {
        0..=2 => row().children(cells),
        3 => {
            let right = cells.split_off(1);
            row()
                .child(col().children(cells))
                .child(col().children(right))
        }
        _ => {
            let bottom = cells.split_off(2);
            col()
                .child(row().children(cells))
                .child(row().children(bottom))
        }
    }
}

/// 一格的格头写什么。
pub struct Head {
    pub name: SharedString,
    /// 仓库（和主机）
    pub place: SharedString,
    /// 状态（静默、待查看的带多久）
    pub state: SharedString,
    pub state_color: Hsla,
    /// 前面那个圆点的颜色
    pub dot: Hsla,
    /// 输入进这一格
    pub focused: bool,
    /// 放大着
    pub zoomed: bool,
}

/// 一格：格头（圆点、名字、仓库、状态，放大着时有「还原」，× 移出）和下面的画面。点格子换焦点，
/// 双击格头放大、还原。
pub fn cell(
    id: impl Into<ElementId>,
    head: Head,
    body: AnyElement,
    on_focus: Handler,
    on_zoom: Handler,
    on_close: Handler,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    let (border, primary, muted, fg, bg, panel, hover) = (
        theme.border,
        theme.primary,
        theme.muted_foreground,
        theme.foreground,
        theme.background,
        theme.secondary,
        theme.list_hover,
    );
    let id: ElementId = id.into();
    let button = |name: &'static str, icon: IconName, tip: &'static str, run: Handler| {
        div()
            .id(name)
            .flex_none()
            .size(px(20.))
            .rounded(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .text_color(muted)
            .hover(|s| s.bg(hover).text_color(fg))
            .child(ui::icon_small(icon))
            .tooltip(move |window, cx| ui::tip(tip, window, cx))
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                run(window, cx);
            })
    };
    let zoom_tip = match head.zoomed {
        true => tr("还原"),
        false => tr("放大"),
    };
    let zoom_icon = match head.zoomed {
        true => IconName::Minimize2,
        false => IconName::Maximize2,
    };
    let (focus_head, zoom_head) = (on_focus.clone(), on_zoom.clone());
    let header = h_flex()
        .id("cell-head")
        .h(px(HEAD))
        .flex_none()
        .px(px(8.))
        .gap(px(7.))
        .bg(panel)
        .border_b_1()
        .border_color(border)
        .text_size(px(12.))
        .cursor_pointer()
        .child(div().flex_none().size(px(7.)).rounded_full().bg(head.dot))
        .child(
            div()
                .flex_shrink(1.)
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(fg)
                .child(head.name.clone()),
        )
        .child(
            div()
                .flex_shrink(10.)
                .min_w_0()
                .truncate()
                .text_size(px(11.))
                .text_color(muted)
                .child(head.place.clone()),
        )
        .child(div().flex_1())
        .child(
            div()
                .flex_none()
                .text_size(px(11.))
                .text_color(head.state_color)
                .child(head.state.clone()),
        )
        .child(button("cell-zoom", zoom_icon, zoom_tip, on_zoom))
        .child(button(
            "cell-close",
            IconName::X,
            tr("从并排中移除"),
            on_close,
        ))
        .tooltip(|window, cx| ui::tip(tr("单击选择此格，双击放大或还原"), window, cx))
        .on_click(
            move |event: &ClickEvent, window, cx| match event.click_count() >= 2 {
                true => zoom_head(window, cx),
                false => focus_head(window, cx),
            },
        );
    v_flex()
        .id(id)
        .flex_1()
        .min_w_0()
        .min_h_0()
        .rounded(px(ui::RADIUS))
        .overflow_hidden()
        .bg(bg)
        .border_1()
        .border_color(if head.focused { primary } else { border })
        // 焦点格的蓝框粗一档：一眼看得出输入进哪一格
        .when(head.focused, |el| el.border_2())
        .on_any_mouse_down(move |_, window, cx| on_focus(window, cx))
        .child(header)
        .child(div().flex_1().min_h_0().relative().child(body))
}

/// 格子里没有画面时（主机没连着、正在连接）写的一句。
pub fn placeholder(text: SharedString, cx: &App) -> AnyElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .px(px(16.))
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child(div().text_center().child(text))
        .into_any_element()
}

/// 底下那一条：怎么换格子、放大，右边写输入进哪一格。格子小了（`size` 是最小的那格的宽、高）提示
/// 放大了再操作。
pub fn foot(target: Option<SharedString>, size: Option<(f32, f32)>, cx: &App) -> Div {
    let theme = cx.theme();
    let (border, muted, fg, warning) = (
        theme.border,
        theme.muted_foreground,
        theme.foreground,
        theme.warning,
    );
    let small = size.is_some_and(|(w, h)| w < SMALL.0 || h < SMALL.1);
    let keys = |keys: &'static str, text: &'static str| {
        h_flex()
            .flex_none()
            .gap(px(4.))
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(fg)
                    .child(keys),
            )
            .child(text)
    };
    h_flex()
        .h(px(26.))
        .flex_none()
        .px(px(12.))
        .gap(px(14.))
        .border_t_1()
        .border_color(border)
        .overflow_hidden()
        .text_size(px(11.))
        .text_color(muted)
        .child(keys("F6", tr("在侧栏和格子之间切换")))
        .child(keys("Ctrl+Alt+←→", tr("换格子")))
        .child(keys(tr("双击格头"), tr("放大")))
        .when(small, |el| {
            el.child(
                div()
                    .flex_none()
                    .text_color(warning)
                    .child(tr("格子较小，建议放大后操作")),
            )
        })
        .child(div().flex_1())
        .children(target.map(|name| {
            div()
                .flex_shrink(1.)
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .text_color(fg)
                .child(tf!("输入进入：{name}", name))
        }))
}

#[cfg(test)]
mod tests {
    use super::step;

    #[test]
    fn 换格子_到头了绕回来() {
        assert_eq!(step("right", 0, 3), Some(1));
        assert_eq!(step("down", 2, 3), Some(0));
        assert_eq!(step("left", 0, 3), Some(2));
        assert_eq!(step("up", 1, 3), Some(0));
        assert_eq!(step("enter", 0, 3), None);
        assert_eq!(step("right", 0, 0), None);
    }
}
