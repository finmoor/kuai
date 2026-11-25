//! 仓库页里几块的大小，和改它们的分隔条：侧栏的宽度、本地修改里文件列表的宽度、
//! 提交历史下面详情区的高度和它左边一栏的宽度。终端在自己的一页上，占满右边，不用调。
//!
//! 大小存在设置里（所有标签页共用），拖分隔条时改，双击分隔条回到默认值。存的是用户拖出来的数；
//! 用的时候再按窗口现在的大小收一收（见 [`sizes`]），窗口变小了也不会把别的块挤没。
//!
//! 侧栏可以收成图标栏（见 [`super::rail`]）：把侧栏的分隔条往左拖到头，或者点侧栏右上角的按钮、
//! 按 Ctrl+Shift+B。收着时侧栏按图标栏的宽度算，存的宽度不动，展开时回到原来的宽度；
//! 把图标栏的右边往右拖开，或者双击它，也会展开。
//!
//! 窄窗口逐级让位（`docs/design/narrow-and-changes/` 方向 A）：窗口窄于 1100 时侧栏自动收成
//! 图标栏（手动展开的在这次运行里保持展开，见 [`rail_shown`]）、文件列表最多 300；窄于 900 时文件列表
//! 最多 240、提交框收成一行；窄于 760 时文件列表和改动上下排；矮于 640 时提交框也收成一行；窄于
//! 1000 时工具栏只留图标（[`super::compact_toolbar`]）。

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
}

/// 拖分隔条时带着走的东西：哪一条，和开始拖时存的侧栏宽度（拖到收起时还原成它）。
/// GPUI 按这个类型认是哪一种拖动；它自己不用画出来。
#[derive(Clone, Copy)]
pub(super) struct DragSplit(Split, Option<f32>);

impl Render for DragSplit {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

pub(super) const SIDEBAR_DEFAULT: f32 = 240.;
/// 窗口窄于这么多时侧栏自动收成图标栏
pub(super) const AUTO_RAIL_BELOW: f32 = 1100.;
/// 窗口窄于这么多时本地修改的文件列表和改动上下排
pub(super) const STACK_BELOW: f32 = 760.;
/// 窗口窄于这么多（或者矮于 [`COMPACT_COMMIT_LOWER`]）时提交框收成一行
pub(super) const COMPACT_COMMIT_BELOW: f32 = 900.;
pub(super) const COMPACT_COMMIT_LOWER: f32 = 640.;
/// 侧栏收起后图标栏的宽度
pub(super) const RAIL_WIDTH: f32 = 48.;
/// 拖侧栏的分隔条：离左边不到这么远就收成图标栏，收着时拖过这么远就展开
const COLLAPSE_AT: f32 = 100.;
pub(super) const CHANGES_LIST_DEFAULT: f32 = 340.;
/// 标题栏、工具栏、搜索行、列标题、状态行加起来的高度：详情区上面不归提交列表的部分
const HISTORY_CHROME: f32 = 38. + 52. + 40. + 24. + 26.;
/// 提交列表至少留这么高（三行）
const COMMIT_LIST_MIN: f32 = 78.;

/// 侧栏展开着时多宽（窗口宽 `vw`）：右边至少留 560 给提交列表或文件列表加改动。
///
/// 会话页左边的列表也用它，右边的画面才和仓库的终端页一样大。
pub(crate) fn sidebar_open(layout: &Layout, vw: f32) -> f32 {
    let max = (vw - 560.).min(480.);
    layout
        .sidebar
        .unwrap_or(SIDEBAR_DEFAULT)
        .clamp(160., max.max(160.))
}

/// 各块现在实际用的大小（逻辑像素）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Sizes {
    pub sidebar: f32,
    pub changes_list: f32,
    pub details_height: f32,
    pub details_width: f32,
}

/// 侧栏现在是不是图标栏：手动收起的，或者窗口窄于 [`AUTO_RAIL_BELOW`]、又没手动展开的。
pub(super) fn rail_shown(layout: &Layout, vw: f32) -> bool {
    layout.sidebar_collapsed || (vw < AUTO_RAIL_BELOW && !layout.narrow_open)
}

/// 按设置里存的数和窗口的大小算各块的大小。没拖过的用默认值（详情区按窗口的比例）；
/// 每一块都夹在「自己还看得清」和「别的块还看得清」之间。
pub(super) fn sizes(layout: &Layout, viewport: Size<Pixels>) -> Sizes {
    let (vw, vh) = (f32::from(viewport.width), f32::from(viewport.height));
    let fit = |value: f32, min: f32, max: f32| value.clamp(min, max.max(min));
    // 侧栏：收着时是图标栏
    let sidebar = match rail_shown(layout, vw) {
        true => RAIL_WIDTH,
        false => sidebar_open(layout, vw),
    };
    let main = vw - sidebar;
    // 文件列表：右边至少留 300 看改动；窗口窄时再收一收（存的宽度不动）
    let cap = match vw {
        w if w < COMPACT_COMMIT_BELOW => 240.,
        w if w < AUTO_RAIL_BELOW => 300.,
        _ => f32::MAX,
    };
    let changes_list = fit(
        layout.changes_list.unwrap_or(CHANGES_LIST_DEFAULT).min(cap),
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
    }
}

impl RepoView {
    /// 各块现在的大小。
    pub(super) fn sizes(&self, cx: &App) -> Sizes {
        sizes(&self.app.read(cx).settings.layout, self.content_viewport())
    }

    /// 窗口内容区扣掉底部的「输出」（开着时）：各块的大小按它算。
    pub(super) fn content_viewport(&self) -> Size<Pixels> {
        let mut viewport = self.viewport;
        viewport.height = (viewport.height - px(self.output_height())).max(px(0.));
        viewport
    }

    /// 一条分隔条：贴在它所属那一块的边上（那一块要是 `relative` 的）。拖动改大小，双击回到默认。
    pub(super) fn split_handle(&self, split: Split, cx: &mut Context<Self>) -> impl IntoElement {
        let (axis, edge) = match split {
            Split::DetailsHeight => (Axis::Vertical, HandleEdge::Leading),
            _ => (Axis::Horizontal, HandleEdge::Trailing),
        };
        let view = cx.entity().downgrade();
        let sidebar = self.app.read(cx).settings.layout.sidebar;
        resize_handle::<DragSplit, DragSplit>(("split", split as usize), axis)
            .inside(edge)
            .on_drag(DragSplit(split, sidebar), |drag, _, _, cx| {
                cx.new(|_| *drag)
            })
            .on_double_click(move |_, _, cx| {
                view.update(cx, |this, cx| this.reset_split(split, cx)).ok();
            })
    }

    /// 拖着分隔条走：按鼠标的位置改那一块的大小。`bounds` 是整个仓库页的范围。
    pub(super) fn drag_split(&mut self, event: &DragMoveEvent<DragSplit>, cx: &mut Context<Self>) {
        let DragSplit(split, sidebar_before) = *event.drag(cx);
        let (pos, bounds) = (event.event.position, event.bounds);
        let now = self.sizes(cx);
        let sidebar = now.sidebar;
        let from_left = f32::from(pos.x - bounds.left());
        // 底部开着「输出」时，详情区的底边在它上面
        let from_bottom = f32::from(bounds.bottom() - pos.y) - self.output_height();
        // 侧栏往左拖到头就收成图标栏；收着时往右拖开就展开
        if split == Split::Sidebar {
            let collapse = from_left < COLLAPSE_AT;
            let narrow = f32::from(self.viewport.width) < AUTO_RAIL_BELOW;
            self.app.update(cx, |app, cx| {
                app.edit_layout(cx, |layout| {
                    // 窗口窄、自动收起时：拖开只在这次运行里记着，拖回去回到自动收起
                    match narrow {
                        true => layout.narrow_open = !collapse,
                        false => layout.sidebar_collapsed = collapse,
                    }
                    if narrow && !collapse {
                        layout.sidebar_collapsed = false;
                    }
                    // 拖到头之前经过的那些宽度不算：展开时回到开始拖之前的宽度
                    if collapse {
                        layout.sidebar = sidebar_before;
                    }
                })
            });
            if collapse {
                cx.notify();
                return;
            }
        }
        let value = match split {
            Split::Sidebar => from_left,
            Split::ChangesList | Split::DetailsWidth => from_left - sidebar,
            Split::DetailsHeight => from_bottom,
        };
        // 存之前先按现在的窗口收一下：拖到头以后再往回拖，马上就跟手
        let mut layout = self.app.read(cx).settings.layout;
        set(&mut layout, split, Some(value.max(0.)));
        let fitted = sizes(&layout, self.content_viewport());
        let value = match split {
            Split::Sidebar => fitted.sidebar,
            Split::ChangesList => fitted.changes_list,
            Split::DetailsHeight => fitted.details_height,
            Split::DetailsWidth => fitted.details_width,
        };
        self.app.update(cx, |app, cx| {
            app.edit_layout(cx, |layout| set(layout, split, Some(value.round())))
        });
        cx.notify();
    }

    fn reset_split(&mut self, split: Split, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            app.edit_layout(cx, |layout| {
                set(layout, split, None);
                // 双击图标栏的右边：展开，回到默认宽度（窗口窄时只在这次运行里展开）
                if split == Split::Sidebar {
                    layout.sidebar_collapsed = false;
                    layout.narrow_open = true;
                }
            })
        });
        cx.notify();
    }

    /// 侧栏是不是收成了图标栏。
    pub(super) fn sidebar_collapsed(&self, cx: &App) -> bool {
        rail_shown(
            &self.app.read(cx).settings.layout,
            f32::from(self.viewport.width),
        )
    }

    /// 收起、展开侧栏（侧栏右上角的按钮、图标栏底下的按钮、Ctrl+Shift+B）。所有标签页一起变。
    pub fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let collapse = !self.sidebar_collapsed(cx);
        // 焦点在侧栏的列表、过滤框里的话，收起以后它们看不见了，键盘就没反应了
        if collapse && self.sidebar_has_focus(window, cx) {
            window.blur(cx);
        }
        // 窗口窄、自动收起时：展开只在这次运行里记着，收起回到自动收起，不改存着的
        let narrow = f32::from(self.viewport.width) < AUTO_RAIL_BELOW;
        self.app.update(cx, |app, cx| {
            app.edit_layout(cx, |layout| match narrow {
                true => {
                    layout.narrow_open = !collapse;
                    if !collapse {
                        layout.sidebar_collapsed = false;
                    }
                }
                false => layout.sidebar_collapsed = collapse,
            })
        });
        cx.notify();
    }

    /// 窗口宽到不用自动收起了：清掉「窄时手动展开」，下次变窄照样自动收起。画之前调。
    pub(super) fn forget_narrow_open(&mut self, cx: &mut Context<Self>) {
        let wide = f32::from(self.viewport.width) >= AUTO_RAIL_BELOW;
        if wide && self.app.read(cx).settings.layout.narrow_open {
            let app = self.app.clone();
            cx.defer(move |cx| {
                app.update(cx, |app, cx| {
                    app.edit_layout(cx, |layout| layout.narrow_open = false)
                })
            });
        }
    }

    /// 本地修改的文件列表和改动上下排（窗口窄于 [`STACK_BELOW`]）。
    pub(super) fn stacked_changes(&self) -> bool {
        f32::from(self.viewport.width) < STACK_BELOW
    }

    /// 提交框收成一行（窗口窄于 [`COMPACT_COMMIT_BELOW`] 或矮于 [`COMPACT_COMMIT_LOWER`]）。
    pub(super) fn compact_commit(&self) -> bool {
        f32::from(self.viewport.width) < COMPACT_COMMIT_BELOW
            || f32::from(self.content_viewport().height) < COMPACT_COMMIT_LOWER
    }
}

fn set(layout: &mut Layout, split: Split, value: Option<f32>) {
    match split {
        Split::Sidebar => layout.sidebar = value,
        Split::ChangesList => layout.changes_list = value,
        Split::DetailsHeight => layout.details_height = value,
        Split::DetailsWidth => layout.details_width = value,
    }
}

#[cfg(test)]
mod tests {
    use super::{COMMIT_LIST_MIN, HISTORY_CHROME, RAIL_WIDTH, Sizes, sizes};
    use crate::settings::Layout;
    use gpui_kit::{px, size};

    fn at(layout: Layout, width: f32, height: f32) -> Sizes {
        sizes(&layout, size(px(width), px(height)))
    }

    #[test]
    fn 没拖过时用默认值() {
        let s = at(Layout::default(), 1400., 900.);
        assert_eq!(s.sidebar, 240.);
        assert_eq!(s.changes_list, 340.);
        assert_eq!(s.details_height, 360.);
        assert_eq!(s.details_width, 380.);
        // 窄的窗口：侧栏自动收成图标栏，手动展开了是原来的宽度
        let s = at(Layout::default(), 900., 560.);
        assert_eq!(s.sidebar, RAIL_WIDTH);
        assert_eq!(s.details_height, 224.);
        assert_eq!(s.details_width, 270.);
        let open = Layout {
            narrow_open: true,
            ..Layout::default()
        };
        assert_eq!(at(open, 900., 560.).sidebar, 240.);
    }

    #[test]
    fn 拖出来的大小不会把别的块挤没() {
        // 窄的窗口里手动展开了侧栏（不然自动收成图标栏）
        let wide = Layout {
            sidebar: Some(2000.),
            changes_list: Some(2000.),
            details_height: Some(2000.),
            details_width: Some(2000.),
            narrow_open: true,
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
            ..Layout::default()
        };
        let s = at(narrow, 1400., 900.);
        assert_eq!(
            (s.sidebar, s.changes_list, s.details_height, s.details_width),
            (160., 220., 100., 200.)
        );
    }

    #[test]
    fn 收起时侧栏是图标栏的宽度_展开回到原来的宽度() {
        let collapsed = Layout {
            sidebar: Some(300.),
            changes_list: Some(2000.),
            sidebar_collapsed: true,
            ..Layout::default()
        };
        let s = at(collapsed, 1400., 900.);
        assert_eq!(s.sidebar, RAIL_WIDTH);
        // 别的块按图标栏的宽度算：文件列表最宽到右边还留 300
        assert_eq!(s.changes_list, 1400. - RAIL_WIDTH - 300.);
        // 展开：还是收起前的宽度
        let open = Layout {
            sidebar_collapsed: false,
            ..collapsed
        };
        assert_eq!(at(open, 1400., 900.).sidebar, 300.);
    }
}
