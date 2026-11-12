//! 仓库页里几块的大小，和改它们的分隔条：侧栏的宽度、本地修改里文件列表的宽度、
//! 提交历史下面详情区的高度和它左边一栏的宽度、最底下终端的高度。
//!
//! 大小存在设置里（所有标签页共用），拖分隔条时改，双击分隔条回到默认值。存的是用户拖出来的数；
//! 用的时候再按窗口现在的大小收一收（见 [`sizes`]），窗口变小了也不会把别的块挤没。

use gpui_kit::component::InteractiveElementExt as _;
use gpui_kit::component::resizable::{HandleEdge, resize_handle};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use crate::settings::Layout;

/// 哪一条分隔条。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Split {
    /// 侧栏的右边
    Sidebar,
    /// 本地修改里文件列表的右边
    ChangesList,
    /// 提交列表和下面详情区之间
    DetailsHeight,
    /// 详情区里左边一栏的右边
    DetailsWidth,
    /// 终端的上边
    TerminalHeight,
}

/// 拖分隔条时带着走的东西。GPUI 按这个类型认是哪一种拖动；它自己不用画出来。
#[derive(Clone)]
pub(super) struct DragSplit(Split);

impl Render for DragSplit {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

pub(super) const SIDEBAR_DEFAULT: f32 = 240.;
pub(super) const CHANGES_LIST_DEFAULT: f32 = 340.;
/// 标题栏、工具栏、搜索行、列标题、状态行加起来的高度：详情区上面不归提交列表的部分
const HISTORY_CHROME: f32 = 38. + 52. + 40. + 24. + 26.;
/// 提交列表至少留这么高（三行）
const COMMIT_LIST_MIN: f32 = 78.;
/// 终端最矮这么高（标题一行加几行字）
const TERMINAL_MIN: f32 = 120.;
/// 终端开着时，上面（标题栏和工具栏以下）至少留这么高给仓库的内容
const ABOVE_TERMINAL_MIN: f32 = 150.;

/// 各块现在实际用的大小（逻辑像素）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Sizes {
    pub sidebar: f32,
    pub changes_list: f32,
    pub details_height: f32,
    pub details_width: f32,
    /// 终端的高度；没开着是 0
    pub terminal_height: f32,
}

/// 按设置里存的数和窗口的大小算各块的大小。没拖过的用默认值（详情区按窗口的比例）；
/// 每一块都夹在「自己还看得清」和「别的块还看得清」之间。`terminal` 是底下的终端开没开：
/// 开着就先从下面占掉一块，其余各块按剩下的高度算。
pub(super) fn sizes(layout: &Layout, viewport: Size<Pixels>, terminal: bool) -> Sizes {
    let (vw, vh) = (f32::from(viewport.width), f32::from(viewport.height));
    let fit = |value: f32, min: f32, max: f32| value.clamp(min, max.max(min));
    // 终端：默认占窗口高度的四成不到
    let terminal_height = if terminal {
        fit(
            layout
                .terminal_height
                .unwrap_or((vh * 0.38).clamp(200., 460.)),
            TERMINAL_MIN,
            vh - 38. - 52. - ABOVE_TERMINAL_MIN,
        )
    } else {
        0.
    };
    let vh = vh - terminal_height;
    // 侧栏：右边至少留 560 给提交列表或文件列表加改动
    let sidebar = fit(
        layout.sidebar.unwrap_or(SIDEBAR_DEFAULT),
        160.,
        (vw - 560.).min(480.),
    );
    let main = vw - sidebar;
    // 文件列表：右边至少留 300 看改动
    let changes_list = fit(
        layout.changes_list.unwrap_or(CHANGES_LIST_DEFAULT),
        220.,
        main - 300.,
    );
    // 详情区左边一栏：默认占窗口宽度的三成
    let details_width = fit(
        layout.details_width.unwrap_or((vw * 0.3).clamp(250., 380.)),
        200.,
        main - 260.,
    );
    // 详情区：默认占窗口高度的四成
    let details_height = fit(
        layout
            .details_height
            .unwrap_or((vh * 0.4).clamp(150., 460.)),
        100.,
        vh - HISTORY_CHROME - COMMIT_LIST_MIN,
    );
    Sizes {
        sidebar,
        changes_list,
        details_height,
        details_width,
        terminal_height,
    }
}

impl RepoView {
    /// 各块现在的大小。
    pub(super) fn sizes(&self, cx: &App) -> Sizes {
        sizes(
            &self.app.read(cx).settings.layout,
            self.viewport,
            self.terminals.open,
        )
    }

    /// 一条分隔条：贴在它所属那一块的边上（那一块要是 `relative` 的）。拖动改大小，双击回到默认。
    pub(super) fn split_handle(&self, split: Split, cx: &mut Context<Self>) -> impl IntoElement {
        let (axis, edge) = match split {
            Split::DetailsHeight | Split::TerminalHeight => (Axis::Vertical, HandleEdge::Leading),
            _ => (Axis::Horizontal, HandleEdge::Trailing),
        };
        let view = cx.entity().downgrade();
        resize_handle::<DragSplit, DragSplit>(("split", split as usize), axis)
            .inside(edge)
            .on_drag(DragSplit(split), |drag, _, _, cx| cx.new(|_| drag.clone()))
            .on_double_click(move |_, _, cx| {
                view.update(cx, |this, cx| this.reset_split(split, cx)).ok();
            })
    }

    /// 拖着分隔条走：按鼠标的位置改那一块的大小。`bounds` 是整个仓库页的范围。
    pub(super) fn drag_split(&mut self, event: &DragMoveEvent<DragSplit>, cx: &mut Context<Self>) {
        let split = event.drag(cx).0;
        let (pos, bounds) = (event.event.position, event.bounds);
        let now = self.sizes(cx);
        let sidebar = now.sidebar;
        let from_left = f32::from(pos.x - bounds.left());
        let from_bottom = f32::from(bounds.bottom() - pos.y);
        let value = match split {
            Split::Sidebar => from_left,
            Split::ChangesList | Split::DetailsWidth => from_left - sidebar,
            // 详情区的下面是终端（开着的话）
            Split::DetailsHeight => from_bottom - now.terminal_height,
            Split::TerminalHeight => from_bottom,
        };
        // 存之前先按现在的窗口收一下：拖到头以后再往回拖，马上就跟手
        let mut layout = self.app.read(cx).settings.layout;
        set(&mut layout, split, Some(value.max(0.)));
        let fitted = sizes(&layout, self.viewport, self.terminals.open);
        let value = match split {
            Split::Sidebar => fitted.sidebar,
            Split::ChangesList => fitted.changes_list,
            Split::DetailsHeight => fitted.details_height,
            Split::DetailsWidth => fitted.details_width,
            Split::TerminalHeight => fitted.terminal_height,
        };
        self.app.update(cx, |app, cx| {
            app.edit_layout(cx, |layout| set(layout, split, Some(value.round())))
        });
        cx.notify();
    }

    fn reset_split(&mut self, split: Split, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            app.edit_layout(cx, |layout| set(layout, split, None))
        });
        cx.notify();
    }
}

fn set(layout: &mut Layout, split: Split, value: Option<f32>) {
    match split {
        Split::Sidebar => layout.sidebar = value,
        Split::ChangesList => layout.changes_list = value,
        Split::DetailsHeight => layout.details_height = value,
        Split::DetailsWidth => layout.details_width = value,
        Split::TerminalHeight => layout.terminal_height = value,
    }
}

#[cfg(test)]
mod tests {
    use super::{COMMIT_LIST_MIN, HISTORY_CHROME, Sizes, sizes};
    use crate::settings::Layout;
    use gpui_kit::{px, size};

    fn at(layout: Layout, width: f32, height: f32) -> Sizes {
        sizes(&layout, size(px(width), px(height)), false)
    }

    #[test]
    fn 没拖过时用默认值() {
        let s = at(Layout::default(), 1400., 900.);
        assert_eq!(s.sidebar, 240.);
        assert_eq!(s.changes_list, 340.);
        assert_eq!(s.details_height, 360.);
        assert_eq!(s.details_width, 380.);
        // 最小的窗口
        let s = at(Layout::default(), 900., 560.);
        assert_eq!(s.sidebar, 240.);
        assert_eq!(s.details_height, 224.);
        assert_eq!(s.details_width, 270.);
    }

    #[test]
    fn 拖出来的大小不会把别的块挤没() {
        let wide = Layout {
            sidebar: Some(2000.),
            changes_list: Some(2000.),
            details_height: Some(2000.),
            details_width: Some(2000.),
            terminal_height: Some(2000.),
            ..Layout::default()
        };
        let s = at(wide, 900., 560.);
        // 侧栏右边留 560；文件列表右边留 300；详情区上面留三行提交
        assert_eq!(s.sidebar, 340.);
        assert_eq!(s.changes_list, 900. - 340. - 300.);
        assert_eq!(s.details_width, 900. - 340. - 260.);
        assert_eq!(s.details_height, 560. - HISTORY_CHROME - COMMIT_LIST_MIN);
        // 同样的设置，窗口大了就给得多
        let s = at(wide, 2000., 1200.);
        assert_eq!(s.sidebar, 480.);
        assert_eq!(s.changes_list, 2000. - 480. - 300.);
        // 太小的收到下限
        let narrow = Layout {
            sidebar: Some(10.),
            changes_list: Some(10.),
            details_height: Some(10.),
            details_width: Some(10.),
            terminal_height: Some(10.),
            ..Layout::default()
        };
        let s = at(narrow, 1400., 900.);
        assert_eq!(
            (s.sidebar, s.changes_list, s.details_height, s.details_width),
            (160., 220., 100., 200.)
        );
        // 终端没开：不占地方
        assert_eq!(s.terminal_height, 0.);
    }

    #[test]
    fn 终端开着时从下面占一块_上面的按剩下的高度算() {
        let open = |layout, width, height| sizes(&layout, size(px(width), px(height)), true);
        // 没拖过：窗口高度的 0.38；详情区按剩下的高度的四成
        let s = open(Layout::default(), 1400., 1000.);
        assert_eq!(s.terminal_height, 380.);
        assert_eq!(s.details_height, 248.);
        // 拖得再高，上面也留着 150；拖得再矮，也有 120
        let tall = Layout {
            terminal_height: Some(5000.),
            ..Layout::default()
        };
        assert_eq!(open(tall, 1400., 900.).terminal_height, 900. - 38. - 52. - 150.);
        let short = Layout {
            terminal_height: Some(5.),
            ..Layout::default()
        };
        assert_eq!(open(short, 1400., 900.).terminal_height, 120.);
    }
}
