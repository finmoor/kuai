//! 自己画的字符：方块（U+2580–259F）和制表线（U+2500–257F 里的直线、拐角、丁字、十字、双线、圆角）。
//!
//! 这些字是拿来拼图、画框的，要一格挨一格、一行接一行地连起来。交给字体画有两个问题：
//! 字形是按字体自己的行高设计的，行高调大以后上下两行之间有缝；等宽字体里常常没有其中一部分
//! （Consolas 没有四分之一方块），缺的由别的字体顶上，宽度、位置都对不上，左右也有缝。
//! Claude Code 开头的那个图标就是方块拼的，画出来一道一道。
//!
//! 所以和 Windows Terminal、Alacritty 一样，这些字不用字体，按格子的大小直接画矩形；
//! 边都对齐到屏幕像素，相邻两格共用同一条边，拼起来没有缝。
//! 虚线、斜线还是交给字体。

use gpui_kit::*;

/// 四个方向上各有没有线、什么样的：0 没有，1 细，2 粗，3 双线。顺序是左、右、上、下。
type Arms = [u8; 4];

/// U+2500 起的直线、拐角、丁字、十字各有哪几条线；虚线那几个是 None（交给字体）。
#[rustfmt::skip]
const LINES: [Option<Arms>; 109] = {
    const fn arms(code: u16) -> Option<Arms> {
        Some([
            (code / 1000) as u8,
            (code / 100 % 10) as u8,
            (code / 10 % 10) as u8,
            (code % 10) as u8,
        ])
    }
    [
        // ─ ━ │ ┃，后面八个是虚线
        arms(1100), arms(2200), arms(11), arms(22),
        None, None, None, None, None, None, None, None,
        // ┌ ┍ ┎ ┏ ┐ ┑ ┒ ┓
        arms(101), arms(201), arms(102), arms(202), arms(1001), arms(2001), arms(1002), arms(2002),
        // └ ┕ ┖ ┗ ┘ ┙ ┚ ┛
        arms(110), arms(210), arms(120), arms(220), arms(1010), arms(2010), arms(1020), arms(2020),
        // ├ ┝ ┞ ┟ ┠ ┡ ┢ ┣
        arms(111), arms(211), arms(121), arms(112), arms(122), arms(221), arms(212), arms(222),
        // ┤ ┥ ┦ ┧ ┨ ┩ ┪ ┫
        arms(1011), arms(2011), arms(1021), arms(1012), arms(1022), arms(2021), arms(2012), arms(2022),
        // ┬ ┭ ┮ ┯ ┰ ┱ ┲ ┳
        arms(1101), arms(2101), arms(1201), arms(2201), arms(1102), arms(2102), arms(1202), arms(2202),
        // ┴ ┵ ┶ ┷ ┸ ┹ ┺ ┻
        arms(1110), arms(2110), arms(1210), arms(2210), arms(1120), arms(2120), arms(1220), arms(2220),
        // ┼ ┽ ┾ ┿ ╀ ╁ ╂ ╃ ╄ ╅ ╆ ╇ ╈ ╉ ╊ ╋
        arms(1111), arms(2111), arms(1211), arms(2211), arms(1121), arms(1112), arms(1122), arms(2121),
        arms(1221), arms(2112), arms(1212), arms(2221), arms(2212), arms(2122), arms(1222), arms(2222),
        // ╌ ╍ ╎ ╏ 虚线
        None, None, None, None,
        // ═ ║ ╒ ╓ ╔ ╕ ╖ ╗
        arms(3300), arms(33), arms(301), arms(103), arms(303), arms(3001), arms(1003), arms(3003),
        // ╘ ╙ ╚ ╛ ╜ ╝
        arms(310), arms(130), arms(330), arms(3010), arms(1030), arms(3030),
        // ╞ ╟ ╠ ╡ ╢ ╣
        arms(311), arms(133), arms(333), arms(3011), arms(1033), arms(3033),
        // ╤ ╥ ╦ ╧ ╨ ╩
        arms(3301), arms(1103), arms(3303), arms(3310), arms(1130), arms(3330),
        // ╪ ╫ ╬
        arms(3311), arms(1133), arms(3333),
    ]
};

/// U+2574 起的半截线：╴ ╵ ╶ ╷ ╸ ╹ ╺ ╻ ╼ ╽ ╾ ╿
#[rustfmt::skip]
const HALVES: [Arms; 12] = [
    [1, 0, 0, 0], [0, 0, 1, 0], [0, 1, 0, 0], [0, 0, 0, 1],
    [2, 0, 0, 0], [0, 0, 2, 0], [0, 2, 0, 0], [0, 0, 0, 2],
    [1, 2, 0, 0], [0, 0, 1, 2], [2, 1, 0, 0], [0, 0, 2, 1],
];

fn arms_of(ch: char) -> Option<Arms> {
    match ch {
        '\u{2500}'..='\u{256C}' => LINES[ch as usize - 0x2500],
        '\u{2574}'..='\u{257F}' => Some(HALVES[ch as usize - 0x2574]),
        _ => None,
    }
}

/// 这个字是不是自己画（不交给字体）。
pub(super) fn is_drawn(ch: char) -> bool {
    matches!(ch, '\u{2580}'..='\u{259F}' | '\u{256D}'..='\u{2570}') || arms_of(ch).is_some()
}

/// 在 `cell` 这一格里画出 `ch`。只对 [`is_drawn`] 为真的字调用。
pub(super) fn paint(ch: char, cell: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    let scale = window.scale_factor();
    // 对齐到屏幕像素：相邻的格子算出来的是同一条边，不会各自半透明、中间露出一条缝
    let snap = |value: Pixels| px((f32::from(value) * scale).round() / scale);
    let (left, top) = (cell.origin.x, cell.origin.y);
    let (width, height) = (cell.size.width, cell.size.height);
    // 横向、纵向的八等分线
    let x = |eighths: u32| snap(left + width * (eighths as f32 / 8.));
    let y = |eighths: u32| snap(top + height * (eighths as f32 / 8.));
    let mut rect = |x0: Pixels, y0: Pixels, x1: Pixels, y1: Pixels, color: Hsla| {
        if x1 > x0 && y1 > y0 {
            let bounds = Bounds::from_corners(point(x0, y0), point(x1, y1));
            window.paint_quad(fill(bounds, color));
        }
    };
    let code = ch as u32;
    match ch {
        // ▀ 上半块
        '\u{2580}' => rect(x(0), y(0), x(8), y(4), color),
        // ▁ … █ 从下往上八分之几
        '\u{2581}'..='\u{2588}' => rect(x(0), y(0x2588 - code), x(8), y(8), color),
        // ▉ … ▏ 从左往右八分之几
        '\u{2589}'..='\u{258F}' => rect(x(0), y(0), x(0x2590 - code), y(8), color),
        // ▐ 右半块
        '\u{2590}' => rect(x(4), y(0), x(8), y(8), color),
        // ░ ▒ ▓ 三种深浅
        '\u{2591}'..='\u{2593}' => {
            let alpha = (code - 0x2590) as f32 * 0.25;
            rect(x(0), y(0), x(8), y(8), color.opacity(alpha));
        }
        // ▔ 最上面一条，▕ 最右边一条
        '\u{2594}' => rect(x(0), y(0), x(8), y(1), color),
        '\u{2595}' => rect(x(7), y(0), x(8), y(8), color),
        // ▖ … ▟ 四分之一块的各种组合：左上 1、右上 2、左下 4、右下 8
        '\u{2596}'..='\u{259F}' => {
            const QUADRANTS: [u8; 10] = [4, 8, 1, 13, 9, 7, 11, 2, 6, 14];
            let bits = QUADRANTS[(code - 0x2596) as usize];
            for (bit, column, row) in [(1, 0, 0), (2, 4, 0), (4, 0, 4), (8, 4, 4)] {
                if bits & bit != 0 {
                    rect(x(column), y(row), x(column + 4), y(row + 4), color);
                }
            }
        }
        _ => {}
    }
    // 细线的粗细：格子宽度的八分之一，至少一个屏幕像素；粗线是它的两倍
    let pixel = 1. / scale;
    let thin = px(((f32::from(width) * scale / 8.).round().max(1.)) * pixel);
    let thickness = |weight: u8| thin * weight as f32;
    // 粗细为 `weight` 的竖线占哪一段横坐标、横线占哪一段纵坐标（0 是正中间的一个点）
    let column = |weight: u8| {
        let start = snap(left + (width - thickness(weight)) / 2.);
        (start, start + thickness(weight))
    };
    let row = |weight: u8| {
        let start = snap(top + (height - thickness(weight)) / 2.);
        (start, start + thickness(weight))
    };
    if let Some([l, r, u, d]) = arms_of(ch).filter(|arms| arms.contains(&3)) {
        // 双线：横、竖各有三条轨道（中间一条给单线，两边各一条给双线），线宽和轨道间距都是一条细线。
        // 中间是个小方框：哪一边没有双线接过来，那一边就封上（两条线在这里连成拐角或者直通）
        let track = |from: Pixels, extent: Pixels, index: i32| {
            let start = snap(from + (extent - thin) / 2.) + thin * index as f32;
            (start, start + thin)
        };
        let col = |index: i32| track(left, width, index);
        let line = |index: i32| track(top, height, index);
        // 方框横向、纵向各伸出去几条轨道：竖着有双线，方框就有左右两条边
        let reach_x = i32::from(u == 3 || d == 3);
        let reach_y = i32::from(l == 3 || r == 3);
        let tracks = |arm: u8| if arm == 3 { &[-1, 1][..] } else { &[0][..] };
        if l > 0 {
            for &index in tracks(l) {
                rect(x(0), line(index).0, col(-reach_x).1, line(index).1, color);
            }
        }
        if r > 0 {
            for &index in tracks(r) {
                rect(col(reach_x).0, line(index).0, x(8), line(index).1, color);
            }
        }
        if u > 0 {
            for &index in tracks(u) {
                rect(col(index).0, y(0), col(index).1, line(-reach_y).1, color);
            }
        }
        if d > 0 {
            for &index in tracks(d) {
                rect(col(index).0, line(reach_y).0, col(index).1, y(8), color);
            }
        }
        let (x0, x1) = (col(-reach_x).0, col(reach_x).1);
        let (y0, y1) = (line(-reach_y).0, line(reach_y).1);
        if l != 3 {
            rect(x0, y0, col(-reach_x).1, y1, color);
        }
        if r != 3 {
            rect(col(reach_x).0, y0, x1, y1, color);
        }
        if u != 3 {
            rect(x0, y0, x1, line(-reach_y).1, color);
        }
        if d != 3 {
            rect(x0, line(reach_y).0, x1, y1, color);
        }
        // 两头都是单线：从双线中间穿过去
        if l == 1 && r == 1 {
            rect(x0, line(0).0, x1, line(0).1, color);
        }
        if u == 1 && d == 1 {
            rect(col(0).0, y0, col(0).1, y1, color);
        }
        return;
    }
    if let Some([l, r, u, d]) = arms_of(ch) {
        // 每条线画到交叉点的另一侧：拐角处补成方的，不缺一个角
        let (vertical, horizontal) = (column(u.max(d)), row(l.max(r)));
        if l > 0 {
            rect(x(0), row(l).0, vertical.1, row(l).1, color);
        }
        if r > 0 {
            rect(vertical.0, row(r).0, x(8), row(r).1, color);
        }
        if u > 0 {
            rect(column(u).0, y(0), column(u).1, horizontal.1, color);
        }
        if d > 0 {
            rect(column(d).0, horizontal.0, column(d).1, y(8), color);
        }
        return;
    }
    // ╭ ╮ ╯ ╰ 圆角：竖着的半截、一段弧、横着的半截
    let (toward_right, toward_bottom) = match ch {
        '\u{256D}' => (true, true),
        '\u{256E}' => (false, true),
        '\u{256F}' => (false, false),
        '\u{2570}' => (true, false),
        _ => return,
    };
    let center = point((column(1).0 + column(1).1) / 2., (row(1).0 + row(1).1) / 2.);
    let radius = width.min(height) / 2.;
    let (edge_x, toward_x) = match toward_right {
        true => (x(8), center.x + radius),
        false => (x(0), center.x - radius),
    };
    let (edge_y, toward_y) = match toward_bottom {
        true => (y(8), center.y + radius),
        false => (y(0), center.y - radius),
    };
    let mut path = PathBuilder::stroke(thin);
    path.move_to(point(center.x, edge_y));
    path.line_to(point(center.x, toward_y));
    path.curve_to(point(toward_x, center.y), center);
    path.line_to(point(edge_x, center.y));
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

#[cfg(test)]
mod tests {
    use super::{arms_of, is_drawn};

    #[test]
    fn 哪些字自己画() {
        for ch in [
            '█', '▀', '▄', '▌', '▐', '▛', '▜', '▝', '▘', '░', '─', '│', '┌', '┼', '╭', '╯', '╴',
            '═', '║', '╔', '╬',
        ] {
            assert!(is_drawn(ch), "{ch}");
        }
        // 虚线、斜线、普通的字交给字体
        for ch in ['┄', '╌', '╎', '╱', '╳', 'a', '中', ' ', '■'] {
            assert!(!is_drawn(ch), "{ch}");
        }
    }

    #[test]
    fn 制表线各有哪几条线() {
        // 左、右、上、下
        assert_eq!(arms_of('─'), Some([1, 1, 0, 0]));
        assert_eq!(arms_of('┃'), Some([0, 0, 2, 2]));
        assert_eq!(arms_of('┌'), Some([0, 1, 0, 1]));
        assert_eq!(arms_of('┘'), Some([1, 0, 1, 0]));
        assert_eq!(arms_of('├'), Some([0, 1, 1, 1]));
        assert_eq!(arms_of('┬'), Some([1, 1, 0, 1]));
        assert_eq!(arms_of('┿'), Some([2, 2, 1, 1]));
        assert_eq!(arms_of('╋'), Some([2, 2, 2, 2]));
        assert_eq!(arms_of('╾'), Some([2, 1, 0, 0]));
        // 3 是双线
        assert_eq!(arms_of('═'), Some([3, 3, 0, 0]));
        assert_eq!(arms_of('╔'), Some([0, 3, 0, 3]));
        assert_eq!(arms_of('╟'), Some([0, 1, 3, 3]));
        assert_eq!(arms_of('╪'), Some([3, 3, 1, 1]));
        assert_eq!(arms_of('╬'), Some([3, 3, 3, 3]));
    }
}
