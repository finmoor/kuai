//! 把 fr-core 算好的提交图布局画到一行里。
//!
//! 竖线直接画成细矩形；只有换泳道的斜线才用路径（贝塞尔曲线），路径要三角化，开销大一些。

use fr_core::graph::{Segment, SegmentKind};
use gpui_kit::*;

/// 每条泳道的宽度
pub const LANE_WIDTH: f32 = 14.;
/// 提交图那一列最多画多少条泳道，再多的往右截掉（截掉的线段 fr-core 不存，见那边的说明）
pub const MAX_LANES: u16 = fr_core::graph::MAX_LANES;
const LINE_WIDTH: f32 = 2.;
const NODE_RADIUS: f32 = 4.5;

const PALETTE: [u32; 10] = [
    0x2f80ed, 0x27ae60, 0xf2994a, 0x9b51e0, 0xeb5757, 0x00a3bf, 0xd4419e, 0xb7950b, 0x56ccf2,
    0x6fcf97,
];

pub fn lane_color(index: u8) -> Hsla {
    rgb(PALETTE[index as usize % PALETTE.len()]).into()
}

/// 提交图那一列的宽度。
pub fn column_width(max_lanes: u16) -> Pixels {
    px(LANE_WIDTH * max_lanes.clamp(1, MAX_LANES) as f32 + 6.)
}

/// 一行提交图。`segments` 是这一行的线段，`lane`、`color` 是节点的位置和颜色，`clipped` 是这一行
/// 有画不下的。
pub fn graph_cell(
    segments: Vec<Segment>,
    lane: u16,
    color: u8,
    clipped: bool,
    width: Pixels,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| paint_row(bounds, &segments, lane, color, clipped, window),
    )
    .w(width)
    .h_full()
}

fn paint_row(
    bounds: Bounds<Pixels>,
    segments: &[Segment],
    lane: u16,
    color: u8,
    clipped: bool,
    window: &mut Window,
) {
    let x = |l: u16| bounds.origin.x + px(LANE_WIDTH * (l as f32 + 0.5) + 3.);
    let top = bounds.origin.y;
    let mid = top + bounds.size.height / 2.;
    let bottom = top + bounds.size.height;

    // 泳道太多、右边画不下时，在这一行的右边点三个点，免得看着像历史断了
    let mut clipped = clipped || lane >= MAX_LANES;
    for s in segments {
        if s.from >= MAX_LANES || s.to >= MAX_LANES {
            clipped = true;
            continue;
        }
        let c = lane_color(s.color);
        match s.kind {
            SegmentKind::Through => vline(window, x(s.from), top, bottom, c),
            SegmentKind::ToNode if s.from == s.to => vline(window, x(s.from), top, mid, c),
            // 从上方别的泳道弯进节点：竖着出发，横着进入
            SegmentKind::ToNode => curve(
                window,
                point(x(s.from), top),
                point(x(s.to), mid),
                point(x(s.from), mid),
                c,
            ),
            SegmentKind::FromNode if s.from == s.to => vline(window, x(s.from), mid, bottom, c),
            // 从节点弯到下方别的泳道：横着出发，竖着到达
            SegmentKind::FromNode => curve(
                window,
                point(x(s.from), mid),
                point(x(s.to), bottom),
                point(x(s.to), mid),
                c,
            ),
        }
    }

    if lane < MAX_LANES {
        let r = px(NODE_RADIUS);
        let center = point(x(lane), mid);
        window.paint_quad(quad(
            Bounds {
                origin: point(center.x - r, center.y - r),
                size: size(r * 2., r * 2.),
            },
            r,
            lane_color(color),
            px(1.5),
            gpui_kit::white(),
            BorderStyle::Solid,
        ));
    }
    if clipped {
        overflow_mark(window, bounds, mid);
    }
}

fn overflow_mark(window: &mut Window, bounds: Bounds<Pixels>, mid: Pixels) {
    let right = bounds.origin.x + bounds.size.width;
    for i in 0..3 {
        let r = px(1.5);
        let x = right - px(3. + 5. * i as f32);
        window.paint_quad(fill(
            Bounds {
                origin: point(x - r, mid - r),
                size: size(r * 2., r * 2.),
            },
            gpui_kit::hsla(0., 0., 0.5, 1.),
        ));
    }
}

fn vline(window: &mut Window, x: Pixels, y0: Pixels, y1: Pixels, color: Hsla) {
    window.paint_quad(fill(
        Bounds {
            origin: point(x - px(LINE_WIDTH / 2.), y0),
            size: size(px(LINE_WIDTH), y1 - y0),
        },
        color,
    ));
}

fn curve(
    window: &mut Window,
    from: Point<Pixels>,
    to: Point<Pixels>,
    ctrl: Point<Pixels>,
    color: Hsla,
) {
    let mut builder = PathBuilder::stroke(px(LINE_WIDTH));
    builder.move_to(from);
    builder.curve_to(to, ctrl);
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}
