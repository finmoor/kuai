//! 设置页里的终端预览：一小段有代表性的输出（提示符、彩色、中文、表格线、光标），
//! 和真的终端走同一套画法（[`super::paint_frame`]），所以字体、字号、行高、光标改了以后这里
//! 是什么样，终端里就是什么样。

use alacritty_terminal::vte::ansi::CursorShape;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;

use super::colors::Palette;
use super::{CursorDraw, Frame, Run, Style, glyphs, paint_frame, prefs, stands_alone};
use crate::settings::TerminalCursor;

/// 颜色：None 是默认的前景色，Some 是 16 色里的第几个
type Segment = (Option<u8>, &'static str);

const GREEN: Option<u8> = Some(2);
const RED: Option<u8> = Some(1);
const BLUE: Option<u8> = Some(4);
const DIM: Option<u8> = Some(8);

const PROMPT: [Segment; 4] = [
    (GREEN, "PS"),
    (None, " "),
    (BLUE, r"C:\work\kuai"),
    (None, "> "),
];

/// 预览里的内容。中文每个字占两格，表格是按这个排的：换什么字体都该对得齐。
fn lines() -> Vec<Vec<Segment>> {
    let prompt = |command: &'static str| {
        let mut line = PROMPT.to_vec();
        line.push((None, command));
        line
    };
    vec![
        prompt("git status -sb"),
        vec![
            (GREEN, "## main"),
            (None, "..."),
            (RED, "origin/main"),
            (None, " [ahead 1]"),
        ],
        vec![
            (None, " "),
            (RED, "M"),
            (None, " crates/fr-app/src/terminal/mod.rs"),
        ],
        vec![(RED, "??"), (None, " docs/终端设置.md")],
        prompt("ls docs"),
        vec![(DIM, "名称              大小    修改时间")],
        vec![(None, "架构.md           48 KB   11-13 21:05")],
        vec![(None, "界面文案规范.md   6 KB    11-13 10:12")],
        vec![(None, "┌──────────┬─────────┐")],
        vec![(None, "│ 构建     │ "), (GREEN, "52.8s"), (None, "   │")],
        vec![(None, "│ 测试     │ "), (GREEN, "146 项"), (None, "  │")],
        vec![(None, "└──────────┴─────────┘")],
        prompt(""),
    ]
}

/// 占两格的字（预览里只有中文和全角标点）。
fn is_wide(ch: char) -> bool {
    matches!(ch, '\u{2e80}'..='\u{9fff}' | '\u{f900}'..='\u{faff}' | '\u{ff00}'..='\u{ff60}')
}

/// 预览。高度跟着行高变，宽度占满。
pub fn preview(cx: &App) -> impl IntoElement + use<> {
    let prefs = prefs::get(cx);
    let rows = lines().len() as f32;
    let (border, background) = (cx.theme().border, cx.theme().background);
    div()
        .w_full()
        .h(px(prefs.cell_height() * rows + 20.))
        .p(px(10.))
        .rounded(px(8.))
        .border_1()
        .border_color(border)
        .bg(background)
        .overflow_hidden()
        .child(
            canvas(
                |bounds, window, cx| frame(bounds, window, cx),
                |bounds, frame, window, cx| paint_frame(&frame, bounds, window, cx),
            )
            .size_full(),
        )
}

fn frame(bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) -> Frame {
    let prefs = prefs::get(cx);
    let font_size = px(prefs.font_size);
    let font = prefs::base_font(&prefs);
    let font_id = window.text_system().resolve_font(&font);
    let cell_width = window
        .text_system()
        .advance(font_id, font_size, 'm')
        .map(|advance| advance.width)
        .unwrap_or(font_size * 0.55);
    let theme = cx.theme();
    let palette = Palette::new(theme.foreground, theme.background, theme.is_dark());
    let mut runs: Vec<Run> = Vec::new();
    let mut shapes = Vec::new();
    let mut end = (0, 0);
    for (row, line) in lines().into_iter().enumerate() {
        let mut col = 0;
        for (color, text) in line {
            let style = Style {
                color: color.map_or(palette.foreground, |index| palette.ansi(index)),
                bold: false,
                italic: false,
                underline: false,
                strike: false,
            };
            // 一段里窄的和宽的分开：各自按一格、两格的宽度画
            for ch in text.chars() {
                // 制表线和真的终端里一样自己画
                if glyphs::is_drawn(ch) {
                    shapes.push((row, col, ch, style.color));
                    col += 1;
                    continue;
                }
                let wide = is_wide(ch);
                let width = if wide { 2 } else { 1 };
                // 标点、符号一个一段，和真的终端里一样
                let lone = stands_alone(ch, wide);
                match runs.last_mut() {
                    Some(run)
                        if !lone
                            && !run.lone
                            && run.row == row
                            && run.next == col
                            && run.wide == wide
                            && run.style == style =>
                    {
                        run.text.push(ch);
                        run.next += width;
                    }
                    _ => runs.push(Run {
                        row,
                        col,
                        next: col + width,
                        text: ch.to_string(),
                        wide,
                        single: false,
                        lone,
                        style,
                    }),
                }
                col += width;
            }
        }
        end = (row, col);
    }
    // 空格不用画
    runs.retain(|run| !run.text.trim().is_empty());
    Frame {
        origin: bounds.origin,
        cell: size(cell_width, px(prefs.cell_height())),
        rects: Vec::new(),
        shapes,
        runs,
        cursor: Some(CursorDraw {
            row: end.0,
            col: end.1,
            wide: false,
            shape: match prefs.cursor {
                TerminalCursor::Block => CursorShape::Block,
                TerminalCursor::Beam => CursorShape::Beam,
                TerminalCursor::Underline => CursorShape::Underline,
            },
            ch: ' ',
        }),
        foreground: palette.foreground,
        background: palette.background,
        focused: true,
        marked: None,
        font,
        font_size,
    }
}
