//! 主机页、工作区页、终端页左边那一栏右边的分隔条。
//!
//! 这三页左边那一栏和仓库页的侧栏一样宽，用的是设置里同一个数（见 [`crate::repo::sidebar_open`]）：
//! 终端页的画面这样才和仓库的终端页一样大，换页时终端不用改行列。所以拖哪一页的分隔条，几页一起变；
//! 双击回到默认宽度。这几页没有图标栏，往左拖到头只是停在最窄，不像仓库页那样收起。
//!
//! 用法：那一栏 `.relative()`，最后一个孩子放 [`handle`]；页面的根上接
//! `.on_drag_move(cx.listener(|this, event: &DragMoveEvent<DragSide>, window, cx| …))`，交给 [`drag`]。

use gpui_kit::component::InteractiveElementExt as _;
use gpui_kit::component::resizable::{HandleEdge, resize_handle};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;

/// 拖这条分隔条。GPUI 按这个类型认是哪一种拖动；它自己不用画出来。
#[derive(Clone, Copy)]
pub struct DragSide;

impl Render for DragSide {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// 分隔条，贴在那一栏右边的里侧。双击回到默认宽度。
pub fn handle(id: impl Into<ElementId>, app: &Entity<AppState>) -> impl IntoElement {
    let app = app.downgrade();
    resize_handle::<DragSide, DragSide>(id, Axis::Horizontal)
        .inside(HandleEdge::Trailing)
        .on_drag(DragSide, |drag, _, _, cx| cx.new(|_| *drag))
        .on_double_click(move |_, _, cx| {
            app.update(cx, |app, cx| {
                app.edit_layout(cx, |layout| layout.sidebar = None)
            })
            .ok();
        })
}

/// 拖着分隔条走：鼠标离页面左边多远，那一栏就多宽。先按窗口收一收再存（和仓库页的算法一样），
/// 拖到头以后再往回拖，马上就跟手。`event.bounds` 是接 `on_drag_move` 的页面根的范围。
pub fn drag(
    app: &Entity<AppState>,
    event: &DragMoveEvent<DragSide>,
    window: &Window,
    cx: &mut App,
) {
    let from_left = f32::from(event.event.position.x - event.bounds.left());
    let viewport = f32::from(window.viewport_size().width);
    let mut layout = app.read(cx).settings.layout;
    layout.sidebar = Some(from_left.max(0.));
    let width = crate::repo::sidebar_open(&layout, viewport).round();
    app.update(cx, |app, cx| {
        app.edit_layout(cx, |layout| layout.sidebar = Some(width))
    });
}
