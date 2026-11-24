//! 终端的滚动条、往回翻时固定住的输入区、左边的消息刻度（设计见 `docs/design/terminal-scroll/`）。
//!
//! - 滚动条画在右边的留白里，不占列：拖滑块，点滑块上下的空处翻一页。往回翻着时画面底下中间有一个
//!   「回到底部」的圆按钮，这期间来了新输出，按钮右上角多一个蓝点。
//! - 往回翻时屏幕最下面的输入区（[`term_text::input_area`]）留在原处，上面的照常滚。这时打字画面不动，
//!   按回车才回到底部（见 `TerminalView::type_bytes`）。
//! - 发出去的消息（kuaid 在按回车时记下的，协议 16；开在 Kuai 进程里的终端由 Kuai 自己记）在左边
//!   一列短横线，一条一道。鼠标停上去出卡片，点一道跳到那条消息，排在最上面一行、闪一下。消息在
//!   哪一行按字到往回翻的记录里找（[`term_text::locate_all`]），不记行号：Claude Code 改窗口大小、
//!   压缩上下文时会把整段对话重画一遍。找不到的（早的已经不在记录里、最新的还排着队没显示出来）不画，
//!   位置、悬停、点击都按画出来的那些排（[`shown`]）。
//!
//! 全屏程序（备用屏）没有往回翻的记录，三样都不出现。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use chrono::{Local, TimeZone as _};
use fr_channel::TermMark;
use fr_core::i18n::tr;
use fr_core::term_text::{self, Row, Shade};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Icon, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{PADDING, Proxy, TerminalView};

/// 终端左边的留白：消息刻度画在这里（原来是 8，加宽到 16，终端少一列）
pub(super) const GUTTER: f32 = 16.;
/// 画面上面空出来的那一点（和 `TerminalView::render` 里的一样）
pub(super) const TOP: f32 = 4.;
/// 跳到一条消息以后那一行亮多久
const FLASH: Duration = Duration::from_millis(900);
/// 往回翻的记录在长（有新输出）时，消息在哪一行最多隔这么久重新找一次，中间按多出来的行数挪
const RELOCATE: Duration = Duration::from_millis(300);
/// 同上，在最底下、鼠标没停在刻度上时：这时只要知道哪几条找得到（画哪几道），不用那么勤
const RELOCATE_IDLE: Duration = Duration::from_secs(3);
/// 刻度之间最宽、最窄（消息多了挤紧，再多只显示最近的那些）
const TICK_GAP: (f32, f32) = (9., 4.);
/// 回到底部的圆按钮多大、离输入区多远
const BUTTON: f32 = 30.;
const BUTTON_GAP: f32 = 10.;
/// 悬停卡片多宽、离刻度多远
const CARD_WIDTH: f32 = 320.;
/// 往回翻着时输入区上面的字变了，等输出停这么久再看一次，还是变了才算新输出（见 `settle_outside`）
const SETTLE: Duration = Duration::from_millis(120);

/// 终端视图里这几样的状态。
#[derive(Default)]
pub(super) struct ScrollUi {
    /// 上一帧固定住的输入区：打字时要不要回到底部、鼠标点在哪一行按它算
    pub fixed: Rc<Cell<Fixed>>,
    /// 往回翻着时来了新输出（回到底部按钮上的蓝点）
    pub unseen: bool,
    /// 上一次有输出时输入区上面的字的摘要（见 [`outside_digest`]）
    pub outside: Option<u64>,
    /// 摘要变了，过一会儿再看一次（见 `TerminalView::settle_outside`）
    settling: Option<Task<()>>,
    /// 正拖着滑块：按下时鼠标的 y、那时往回翻了几行
    pub drag: Option<(Pixels, usize)>,
    /// 鼠标在滚动条上（滑块变宽、变深）
    pub bar_hover: bool,
    /// 鼠标停在第几条消息的刻度上
    pub hovered: Option<usize>,
    /// 刚跳到的那一行（网格上的行号）、跳的时刻：亮一下
    pub flash: Option<(i32, Instant)>,
    /// 消息在哪一行（见 [`ScrollUi::mark_lines`]）
    lines: RefCell<Option<Lines>>,
}

/// 算好的消息位置。
struct Lines {
    marks: usize,
    history: usize,
    at: Instant,
    rows: Vec<Option<i32>>,
}

impl ScrollUi {
    /// 换了一个会话（新开、重启）：刻度、悬停、闪的都不要了。
    pub fn reset(&mut self) {
        self.unseen = false;
        self.outside = None;
        self.settling = None;
        self.drag = None;
        self.hovered = None;
        self.flash = None;
        self.lines.borrow_mut().take();
    }

    /// 各条消息在网格上的哪一行（往回翻的记录是负的），找不到的是 `None`。按条数、往回翻的行数缓存：
    /// 记录在长时最多隔 `within` 重新找一次，中间按多出来的行数往上挪；`Duration::ZERO` 是现在就要
    /// 准的（点了刻度要跳过去）。
    pub fn mark_lines(
        &self,
        term: &Term<Proxy>,
        marks: &[TermMark],
        within: Duration,
    ) -> Vec<Option<i32>> {
        let grid = term.grid();
        let history = grid.history_size();
        if let Some(lines) = self.lines.borrow().as_ref()
            && lines.marks == marks.len()
        {
            if lines.history == history {
                return lines.rows.clone();
            }
            let grown = history as i32 - lines.history as i32;
            if grown > 0 && lines.at.elapsed() < within {
                return lines
                    .rows
                    .iter()
                    .map(|line| line.map(|line| line - grown))
                    .collect();
            }
        }
        let rows = read_rows(term, -(history as i32), grid.screen_lines() as i32);
        let texts: Vec<&str> = marks.iter().map(|mark| mark.text.as_str()).collect();
        let found: Vec<Option<i32>> = term_text::locate_all(&rows, &texts)
            .into_iter()
            .map(|at| at.map(|at| at as i32 - history as i32))
            .collect();
        *self.lines.borrow_mut() = Some(Lines {
            marks: marks.len(),
            history,
            at: Instant::now(),
            rows: found.clone(),
        });
        found
    }
}

/// 网格上第 `from` 行到第 `to` 行（不含）的字，连同行尾的淡色字有几个（光标所在的那一行光标后面的
/// 灰色字也算，见 [`Shade::of`]；和 kuaid 的 `Screen::message` 一样读）。
pub(super) fn read_rows(term: &Term<Proxy>, from: i32, to: i32) -> Vec<Row> {
    let grid = term.grid();
    let cols = grid.columns();
    let at = grid.cursor.point;
    let shown = term.mode().contains(TermMode::SHOW_CURSOR);
    let bottom = grid.screen_lines() as i32;
    let mut tail = false;
    (from..to)
        .map(|line| {
            let row = &grid[Line(line)];
            let wraps = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
            let in_tail = shown && (line == at.line.0 || (tail && line > at.line.0));
            tail = in_tail && wraps;
            let padded = wraps
                && line + 1 < bottom
                && term_text::padded(
                    row[Column(cols - 1)].c,
                    grid[Line(line + 1)][Column(0)]
                        .flags
                        .contains(Flags::WIDE_CHAR),
                );
            // 宽字的后半格、宽字放不下时留在行尾的那一格都不是字（折下去的行尾空格要留着）
            let cells = (0..cols)
                .map(|col| (col, &row[Column(col)]))
                .filter(|&(col, cell)| {
                    !cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                        && !(padded && col == cols - 1)
                })
                .map(move |(col, cell)| {
                    let after = in_tail && (line > at.line.0 || col >= at.column.0);
                    let gray = matches!(
                        cell.fg,
                        Color::Named(NamedColor::BrightBlack)
                            | Color::Indexed(8)
                            | Color::Spec(Rgb {
                                r: 128,
                                g: 128,
                                b: 128
                            })
                    );
                    let shade = Shade::of(
                        cell.flags.contains(Flags::INVERSE),
                        cell.flags.contains(Flags::DIM),
                        gray,
                        after,
                    );
                    (cell.c, shade)
                });
            Row::from_cells(cells, wraps)
        })
        .collect()
}

/// 往回翻着时固定在画面最下面的输入区：现在的屏幕上从第 `top` 行起的 `len` 行，`len` 是 0 时没固定。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Fixed {
    pub top: usize,
    pub len: usize,
}

impl Fixed {
    /// 画面上从第几行起是固定的输入区。
    pub fn split(self, rows: usize) -> usize {
        rows.saturating_sub(self.len)
    }

    /// 画面上的第 `row` 行显示的是网格上的哪一行。
    pub fn line(self, row: usize, rows: usize, offset: usize) -> i32 {
        match self.len > 0 && row >= self.split(rows) {
            true => (self.top + row - self.split(rows)) as i32,
            false => row as i32 - offset as i32,
        }
    }
}

/// 往回翻着时固定住哪几行（输入区，见 [`term_text::input_area`]）；在最底下、全屏程序里、认不出
/// 输入区时不固定。输入区下面还空着几行时（屏幕没写满），刚往回翻的那几行输入区跟着往下挪，贴到
/// 画面底边才停住：同一段字不会在画面上出现两次。
pub(super) fn fixed_area(term: &Term<Proxy>) -> Fixed {
    let grid = term.grid();
    let offset = grid.display_offset();
    if offset == 0 || term.mode().contains(TermMode::ALT_SCREEN) {
        return Fixed::default();
    }
    let rows = grid.screen_lines();
    let screen = read_rows(term, 0, rows as i32);
    let cursor = grid.cursor.point.line.0.max(0) as usize;
    let Some(area) =
        term_text::input_area(&screen, cursor, term.mode().contains(TermMode::SHOW_CURSOR))
    else {
        return Fixed::default();
    };
    let below = rows - area.end;
    Fixed {
        top: area.start,
        len: rows - area.start - below.min(offset),
    }
}

impl TerminalView {
    /// 有输出了：往回翻着时输入区上面的字变了就过一会儿再看（见 [`Self::settle_outside`]）；在最底下
    /// 时记下现在的样子。
    pub(super) fn watch_outside(&mut self, cx: &mut Context<Self>) {
        let Some((digest, scrolled)) = self.outside_now() else {
            return;
        };
        match self.scroll.outside {
            Some(old) if scrolled && old != digest => self.settle_outside(cx),
            Some(_) if scrolled => {}
            _ => self.scroll.outside = Some(digest),
        }
    }

    /// 等这一阵输出停下再比一次，还是变了才亮蓝点。程序重画输入区时（PowerShell 补全多行语句后整段
    /// 重画）中间那一下光标藏着、停在半截，认出来的输入区往下挪了，上面一屏的摘要也跟着变；画完就
    /// 回到原样。
    fn settle_outside(&mut self, cx: &mut Context<Self>) {
        if self.scroll.settling.is_some() {
            return;
        }
        self.scroll.settling = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            this.update(cx, |this, cx| {
                this.scroll.settling = None;
                let Some((digest, scrolled)) = this.outside_now() else {
                    return;
                };
                if scrolled && this.scroll.outside.is_some_and(|old| old != digest) {
                    this.scroll.unseen = true;
                    cx.notify();
                }
                this.scroll.outside = Some(digest);
            })
            .ok();
        }));
    }

    /// 现在输入区上面一屏的摘要、是不是往回翻着。
    fn outside_now(&self) -> Option<(u64, bool)> {
        let term = self.session.as_ref()?.term.lock();
        Some((outside_digest(&term), term.grid().display_offset() > 0))
    }
}

/// 输入区上面一屏的字的摘要：往回翻着时来了输出，它变了才算有新内容（回到底部按钮上的蓝点）。只是
/// 输入区里的字变了（往回翻着打字，程序回显）不算，输入区固定在底下看得见；输入区变高把上面的往上
/// 推（推进往回翻的记录里）也不算，从输入区往上数的那几行还是那几行。
pub(super) fn outside_digest(term: &Term<Proxy>) -> u64 {
    use std::hash::{DefaultHasher, Hash as _, Hasher as _};
    let grid = term.grid();
    let rows = grid.screen_lines();
    let top = match term.mode().contains(TermMode::ALT_SCREEN) {
        true => rows,
        false => {
            let screen = read_rows(term, 0, rows as i32);
            let cursor = grid.cursor.point.line.0.max(0) as usize;
            let shown = term.mode().contains(TermMode::SHOW_CURSOR);
            term_text::input_area(&screen, cursor, shown).map_or(rows, |area| area.start)
        }
    };
    let from = (top as i32 - rows as i32).max(-(grid.history_size() as i32));
    let mut hasher = DefaultHasher::new();
    for row in read_rows(term, from, top as i32) {
        row.text.hash(&mut hasher);
    }
    hasher.finish()
}

/// 按回车的那一刻屏幕上输入区里的消息，连同光标是不是在 PowerShell 多行语句的续行上（开在 Kuai
/// 进程里的终端自己记，见 [`term_text::message`]、[`term_text::record`]）。
pub(super) fn message(term: &Term<Proxy>) -> Option<(String, bool)> {
    if term.mode().contains(TermMode::ALT_SCREEN) {
        return None;
    }
    let grid = term.grid();
    let screen = read_rows(term, 0, grid.screen_lines() as i32);
    let cursor = grid.cursor.point.line.0.max(0) as usize;
    let shown = term.mode().contains(TermMode::SHOW_CURSOR);
    let text = term_text::message(&screen, cursor, shown)?;
    Some((text, term_text::continued(&screen, cursor, shown)))
}

/// 画面上面往回翻到的那一行属于哪一条消息（「所在的那一条」，刻度画得长一点、亮一点）：在最底下是
/// 找得到的最后一条；往回翻着是位置在画面最上面一行或者更上面的最后一条。
pub(super) fn current(lines: &[Option<i32>], offset: usize) -> Option<usize> {
    let top = -(offset as i32);
    lines
        .iter()
        .rposition(|line| line.is_some_and(|line| offset == 0 || line <= top))
}

/// 画出来的刻度是哪几条（从早到晚）：找不到的不画。`lines` 比条数短（还没找）的当作找得到。
pub(super) fn shown(lines: &[Option<i32>], count: usize) -> Vec<usize> {
    (0..count)
        .filter(|&index| lines.get(index).is_none_or(Option::is_some))
        .collect()
}

/// 刻度怎么排：第一道的 y、两道之间多远、从第几条起画（放不下时只画最近的）。整列竖着居中。
#[derive(Debug, PartialEq)]
pub(super) struct Ticks {
    pub start: f32,
    pub step: f32,
    pub first: usize,
}

impl Ticks {
    pub fn new(count: usize, height: f32) -> Ticks {
        let room = (height - 16.).max(0.);
        let step = match count {
            0 | 1 => 0.,
            _ => (room / (count - 1) as f32).clamp(TICK_GAP.1, TICK_GAP.0),
        };
        let fits = match step > 0. {
            true => ((room / step).floor() as usize + 1).min(count),
            false => count,
        };
        let span = step * fits.saturating_sub(1) as f32;
        Ticks {
            start: (height - span) / 2.,
            step,
            first: count - fits,
        }
    }

    /// 第 `index` 条的 y。
    pub fn y(&self, index: usize) -> f32 {
        self.start + self.step * (index - self.first) as f32
    }

    /// 鼠标在 `y`：离它最近的那一道，离得太远是 `None`。
    pub fn hit(&self, y: f32, count: usize) -> Option<usize> {
        if count == 0 {
            return None;
        }
        let index = match self.step > 0. {
            true => ((y - self.start) / self.step).round().max(0.) as usize + self.first,
            false => self.first,
        }
        .min(count - 1);
        let reach = (self.step / 2.).max(5.);
        ((self.y(index) - y).abs() <= reach).then_some(index)
    }
}

/// 滑块的位置和长短：`track` 是滚动条多高，`rows` 是画面几行，`history` 是往回翻的记录有几行，
/// `offset` 是往回翻了几行。
pub(super) fn thumb(track: f32, rows: usize, history: usize, offset: usize) -> (f32, f32) {
    let total = (history + rows).max(1) as f32;
    let len = (track * rows as f32 / total).max(20f32.min(track));
    let room = track - len;
    let top = match history {
        0 => room,
        _ => room * (1. - offset as f32 / history as f32),
    };
    (top, len)
}

/// 从按下时往回翻了 `start` 行、鼠标往下挪了 `dy`：现在该往回翻几行。
pub(super) fn dragged(track: f32, rows: usize, history: usize, start: usize, dy: f32) -> usize {
    let (_, len) = thumb(track, rows, history, start);
    let room = (track - len).max(1.);
    let moved = dy / room * history as f32;
    (start as f32 - moved).round().clamp(0., history as f32) as usize
}

/// 卡片上写的时刻：今天的只写几点几分，别的天加上日期。
fn sent_at(at: u64) -> String {
    let Some(time) = Local.timestamp_millis_opt(at as i64).single() else {
        return String::new();
    };
    match time.date_naive() == Local::now().date_naive() {
        true => time.format("%H:%M").to_string(),
        false => time.format("%m-%d %H:%M").to_string(),
    }
}

/// 往回翻的记录里有没有字。改窗口大小、程序清屏重画时滚上去的可能只是几行空行，那不算有往回翻的
/// 内容，滚动条不出来（定稿：有往回翻的内容时才出现）。记录比一屏还长就当有，不一行行看。
pub(super) fn history_has_text(term: &Term<Proxy>) -> bool {
    let grid = term.grid();
    let history = grid.history_size();
    if history > grid.screen_lines() {
        return true;
    }
    (1..=history as i32).any(|back| {
        let row = &grid[Line(-back)];
        (0..grid.columns()).any(|col| !row[Column(col)].c.is_whitespace())
    })
}

/// 画这几样要的数，从仿真器上读的。
struct Look {
    offset: usize,
    history: usize,
    /// 往回翻的记录里有字（见 [`history_has_text`]）
    has_text: bool,
    rows: usize,
    cols: usize,
    alt: bool,
    /// 各条消息在哪一行（要用到时才找，见 [`ScrollUi::mark_lines`]）
    lines: Vec<Option<i32>>,
    /// 悬停的那一条：回复的开头
    reply: Vec<String>,
}

impl TerminalView {
    /// 滚动条、回到底部的按钮、消息刻度和悬停的卡片：盖在终端画面上的几块。
    pub(super) fn scroll_overlays(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let look = {
            let term = session.term.lock();
            let grid = term.grid();
            let offset = grid.display_offset();
            // 找不到的刻度不画，所以总要找；往回翻着、鼠标停着时要画「所在的那一条」、卡片，找得勤些
            let busy = offset > 0 || self.scroll.hovered.is_some();
            let lines = match self.marks.is_empty() {
                true => Vec::new(),
                false => {
                    let within = if busy { RELOCATE } else { RELOCATE_IDLE };
                    self.scroll.mark_lines(&term, &self.marks, within)
                }
            };
            let reply = match self.scroll.hovered {
                Some(index) => match lines.get(index).copied().flatten() {
                    Some(line) => {
                        let end = lines
                            .iter()
                            .skip(index + 1)
                            .find_map(|line| *line)
                            .unwrap_or(grid.screen_lines() as i32)
                            .min(line + 40);
                        let rows = read_rows(&term, line, end.max(line + 1));
                        term_text::reply_after(&rows, 0, rows.len(), 2)
                    }
                    None => Vec::new(),
                },
                None => Vec::new(),
            };
            Look {
                offset,
                history: grid.history_size(),
                has_text: history_has_text(&term),
                rows: grid.screen_lines(),
                cols: grid.columns(),
                alt: term.mode().contains(TermMode::ALT_SCREEN),
                lines,
                reply,
            }
        };
        if look.offset == 0 {
            self.scroll.unseen = false;
        }
        if look.alt {
            return Vec::new();
        }
        let geometry = self.geometry.get();
        let cell = geometry.cell;
        if cell.height <= px(0.) {
            return Vec::new();
        }
        let fixed = self.scroll.fixed.get().len.min(look.rows);
        // 滚动条只盖住会滚的那一块；刻度按整块画面排，输入区固定住、放开时不跟着上下挪
        let track = f32::from(cell.height) * (look.rows - fixed) as f32;
        let full = f32::from(cell.height) * look.rows as f32;
        let mut out = Vec::new();
        if !self.marks.is_empty() {
            out.push(self.render_ticks(&look, full, cx));
            if let Some(card) = self.render_card(&look, full, cx) {
                out.push(card);
            }
        }
        // 往回翻着时总要能拖回来：只有空行的记录翻上去了也画
        if look.has_text || look.offset > 0 {
            out.push(self.render_bar(&look, track, cx));
        }
        if look.offset > 0 {
            let width = f32::from(cell.width) * look.cols as f32;
            out.push(self.render_bottom_button(track, width, cx));
        }
        out
    }

    /// 左边一列刻度。
    fn render_ticks(&self, look: &Look, track: f32, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, fg, accent) = (theme.muted_foreground, theme.foreground, theme.primary);
        let shown = shown(&look.lines, self.marks.len());
        let ticks = Ticks::new(shown.len(), track);
        let current = current(&look.lines, look.offset);
        let hovered = self.scroll.hovered;
        let hits = shown.clone();
        let mut gutter = div()
            .id("terminal-marks")
            .absolute()
            .left_0()
            .top(px(TOP))
            .w(px(GUTTER))
            .h(px(track))
            .when(hovered.is_some(), |el| el.cursor_pointer())
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                let y = f32::from(event.position.y - this.geometry.get().origin.y);
                let hit = Ticks::new(hits.len(), track)
                    .hit(y, hits.len())
                    .map(|at| hits[at]);
                if this.scroll.hovered != hit {
                    if let Some(index) = hit {
                        log::info!("鼠标停在第 {} 条消息的刻度上", index + 1);
                    }
                    this.scroll.hovered = hit;
                    cx.notify();
                }
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered && this.scroll.hovered.take().is_some() {
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.focus.focus(window, cx);
                    if let Some(index) = this.scroll.hovered {
                        this.jump_to_mark(index, cx);
                    }
                }),
            );
        for (at, &index) in shown.iter().enumerate().skip(ticks.first) {
            let (width, color) = if hovered == Some(index) {
                (10., accent)
            } else if current == Some(index) {
                (10., fg.opacity(0.85))
            } else {
                (6., muted.opacity(0.55))
            };
            gutter = gutter.child(
                div()
                    .absolute()
                    .left(px((GUTTER - width) / 2.))
                    .top(px(ticks.y(at) - 1.))
                    .w(px(width))
                    .h(px(2.))
                    .rounded(px(1.))
                    .bg(color),
            );
        }
        gutter.into_any_element()
    }

    /// 鼠标停在刻度上时的卡片：第几条、几点发的、消息（最多四行）、回复的开头（最多两行）。
    fn render_card(&self, look: &Look, track: f32, cx: &mut Context<Self>) -> Option<AnyElement> {
        let index = self.scroll.hovered?;
        let mark = self.marks.get(index)?;
        let theme = cx.theme();
        let (bg, fg, muted, border) = (
            theme.popover,
            theme.popover_foreground,
            theme.muted_foreground,
            theme.border,
        );
        // 只给画出来的那几道出卡片（停着的那条刚变成找不到、挤出了能画的范围时不出）
        let shown = shown(&look.lines, self.marks.len());
        let ticks = Ticks::new(shown.len(), track);
        let at = shown.iter().position(|&shown| shown == index)?;
        if at < ticks.first {
            return None;
        }
        // 卡片的上边对着刻度往上一点，放不下时往上挪
        let top = (ticks.y(at) - 14.).clamp(0., (track - 120.).max(0.)) + TOP;
        let head = tf!(
            "第 {n} 条 · {time}",
            n = (index + 1).to_string(),
            time = sent_at(mark.at)
        );
        let reply = look.reply.join(" ");
        Some(
            v_flex()
                .absolute()
                .left(px(GUTTER + 4.))
                .top(px(top))
                .w(px(CARD_WIDTH))
                .max_w(relative(0.8))
                .p(px(10.))
                .gap(px(4.))
                .rounded(px(8.))
                .border_1()
                .border_color(border)
                .bg(bg)
                .shadow_md()
                .text_color(fg)
                .child(div().text_size(px(11.)).text_color(muted).child(head))
                .child(
                    div()
                        .text_size(px(13.))
                        .line_clamp(4)
                        .text_ellipsis()
                        .whitespace_normal()
                        .child(mark.text.clone()),
                )
                .when(!reply.is_empty(), |card| {
                    card.child(
                        div()
                            .text_size(px(12.))
                            .text_color(muted)
                            .line_clamp(2)
                            .text_ellipsis()
                            .whitespace_normal()
                            .child(reply),
                    )
                })
                .into_any_element(),
        )
    }

    /// 右边的滚动条：只盖住会滚的那一块，不盖住固定住的输入区。
    fn render_bar(&self, look: &Look, track: f32, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let (thumb_top, thumb_len) = thumb(track, look.rows, look.history, look.offset);
        let wide = self.scroll.bar_hover || self.scroll.drag.is_some();
        let offset = look.offset;
        div()
            .id("terminal-scrollbar")
            .absolute()
            .right_0()
            .top(px(TOP))
            .w(px(PADDING))
            .h(px(track))
            .cursor_default()
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if this.scroll.bar_hover != *hovered {
                    this.scroll.bar_hover = *hovered;
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let y = f32::from(event.position.y - this.geometry.get().origin.y);
                    let page = match () {
                        _ if y < thumb_top => Some(Scroll::PageUp),
                        _ if y > thumb_top + thumb_len => Some(Scroll::PageDown),
                        _ => None,
                    };
                    match page {
                        Some(page) => this.scroll_to(page, cx),
                        None => this.scroll.drag = Some((event.position.y, offset)),
                    }
                    cx.notify();
                }),
            )
            .child(
                div()
                    .absolute()
                    .right(px(if wide { 0. } else { 1. }))
                    .top(px(thumb_top))
                    .w(px(if wide { PADDING } else { 6. }))
                    .h(px(thumb_len))
                    .rounded(px(4.))
                    .bg(muted.opacity(if wide { 0.6 } else { 0.3 })),
            )
            .into_any_element()
    }

    /// 往回翻着时画面底下中间的圆按钮：回到底部。这期间来了新输出，右上角一个蓝点。
    fn render_bottom_button(&self, track: f32, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (bg, fg, border, accent) = (
            theme.popover,
            theme.popover_foreground,
            theme.border,
            theme.primary,
        );
        let unseen = self.scroll.unseen;
        div()
            .id("terminal-to-bottom")
            .absolute()
            .left(px(GUTTER + (width - BUTTON) / 2.))
            .top(px(TOP + (track - BUTTON - BUTTON_GAP).max(0.)))
            .size(px(BUTTON))
            .rounded_full()
            .border_1()
            .border_color(border)
            .bg(bg)
            .shadow_md()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .text_color(fg)
            .child(Icon::new(IconName::ArrowDown).size(px(14.)))
            .when(unseen, |button| {
                button.child(
                    div()
                        .absolute()
                        .top(px(-1.))
                        .right(px(-1.))
                        .size(px(9.))
                        .rounded_full()
                        .border_2()
                        .border_color(bg)
                        .bg(accent),
                )
            })
            .tooltip(|window, cx| crate::ui::tip(tr("回到底部"), window, cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.focus.focus(window, cx);
                    this.scroll_to(Scroll::Bottom, cx);
                }),
            )
            .into_any_element()
    }

    /// 翻到某处（滚动条、回到底部按钮）。
    pub(super) fn scroll_to(&mut self, scroll: Scroll, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            let mut term = session.term.lock();
            term.scroll_display(scroll);
            log::info!("终端翻到往回 {} 行", term.grid().display_offset());
        }
        cx.notify();
    }

    /// 拖着滑块挪到了 `y`。
    pub(super) fn drag_bar(&mut self, y: Pixels, cx: &mut Context<Self>) {
        let (Some((start_y, start)), Some(session)) = (self.scroll.drag, &self.session) else {
            return;
        };
        let cell = self.geometry.get().cell.height;
        let mut term = session.term.lock();
        let grid = term.grid();
        let (rows, history, now) = (
            grid.screen_lines(),
            grid.history_size(),
            grid.display_offset(),
        );
        let fixed = self.scroll.fixed.get().len.min(rows);
        let track = f32::from(cell) * (rows - fixed) as f32;
        let target = dragged(track, rows, history, start, f32::from(y - start_y));
        if target != now {
            term.scroll_display(Scroll::Delta(target as i32 - now as i32));
            drop(term);
            cx.notify();
        }
    }

    /// 跳到第 `index` 条消息：排在画面最上面一行，那一行亮一下。已经不在往回翻的记录里的跳不了。
    pub(super) fn jump_to_mark(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let line = {
            let mut term = session.term.lock();
            let lines = self.scroll.mark_lines(&term, &self.marks, Duration::ZERO);
            let Some(Some(line)) = lines.get(index).copied() else {
                return;
            };
            let grid = term.grid();
            let target = (-line).clamp(0, grid.history_size() as i32);
            let now = grid.display_offset() as i32;
            let text = read_rows(&term, line, line + 1).remove(0).text;
            log::info!("跳到第 {} 条消息（网格第 {line} 行：{text}）", index + 1);
            term.scroll_display(Scroll::Delta(target - now));
            line
        };
        self.scroll.flash = Some((line, Instant::now()));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FLASH).await;
            this.update(cx, |this, cx| {
                if this
                    .scroll
                    .flash
                    .is_some_and(|(_, at)| at.elapsed() >= FLASH)
                {
                    this.scroll.flash = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}

/// 刚跳到的那一行现在在画面的第几行（还在亮着、看得见的话）。
pub(super) fn flash_row(
    flash: Option<(i32, Instant)>,
    offset: usize,
    visible: usize,
) -> Option<usize> {
    let (line, at) = flash?;
    if at.elapsed() >= FLASH {
        return None;
    }
    let row = line + offset as i32;
    (0..visible as i32).contains(&row).then_some(row as usize)
}

#[cfg(test)]
mod tests {
    use super::{Fixed, Ticks, current, dragged, shown, thumb};

    #[test]
    fn 固定的输入区画在最下面_画的是现在屏幕上的那几行() {
        // 48 行，输入区是屏幕第 35～39 行（下面空着 8 行），往回翻了 3 行：跟着往下挪了 3 行
        let fixed = Fixed { top: 35, len: 10 };
        assert_eq!(fixed.split(48), 38);
        assert_eq!(fixed.line(37, 48, 3), 34);
        assert_eq!(fixed.line(38, 48, 3), 35);
        assert_eq!(fixed.line(47, 48, 3), 44);
        // 没固定：都按往回翻的行数挪
        assert_eq!(Fixed::default().line(47, 48, 3), 44);
        assert_eq!(Fixed::default().split(48), 48);
    }

    #[test]
    fn 刻度_少的时候间隔最宽_多了挤紧_再多只画最近的() {
        // 3 条，地方很大：间隔 9，整列居中
        let ticks = Ticks::new(3, 400.);
        assert_eq!((ticks.step, ticks.first), (9., 0));
        assert_eq!(ticks.y(1), 200.);
        // 地方不够每道 9：挤紧（这里正好每道 5），全都画得下
        let ticks = Ticks::new(31, 166.);
        assert_eq!(ticks.step, 5.);
        assert_eq!(ticks.first, 0);
        // 再多：只画最近的那些
        let ticks = Ticks::new(200, 216.);
        assert_eq!(ticks.step, 4.);
        assert_eq!(ticks.first, 200 - 51);
        // 鼠标点在一道旁边：认那一道；离得远：不认
        let ticks = Ticks::new(3, 400.);
        assert_eq!(ticks.hit(201., 3), Some(1));
        assert_eq!(ticks.hit(150., 3), None);
        assert_eq!(Ticks::new(0, 400.).hit(200., 0), None);
    }

    #[test]
    fn 所在的那一条() {
        let lines = [Some(-90), Some(-40), None, Some(-5)];
        // 在最底下：最后一条
        assert_eq!(current(&lines, 0), Some(3));
        // 往回翻了 50 行：画面最上面是第 -50 行，属于第 -90 行那条
        assert_eq!(current(&lines, 50), Some(0));
        assert_eq!(current(&lines, 40), Some(1));
        // 比第一条还上面
        assert_eq!(current(&lines, 100), None);
        assert_eq!(current(&[], 0), None);
        // 最后一条找不到（还排着队）：在最底下时是找得到的最后一条
        assert_eq!(current(&[Some(-9), None], 0), Some(0));
        assert_eq!(current(&[None, None], 0), None);
    }

    #[test]
    fn 找不到的刻度不画() {
        assert_eq!(shown(&[Some(-90), None, Some(-5), None], 4), [0, 2]);
        // 还没找（没有位置）的都画
        assert_eq!(shown(&[], 3), [0, 1, 2]);
        assert_eq!(shown(&[None], 0), Vec::<usize>::new());
    }

    #[test]
    fn 滑块的位置和拖动() {
        // 一屏 40 行、记录 360 行：滑块是一成高；在最底下时贴着下面
        let (top, len) = thumb(400., 40, 360, 0);
        assert_eq!((top, len), (360., 40.));
        // 翻到最上面贴着上面
        assert_eq!(thumb(400., 40, 360, 360).0, 0.);
        // 记录很长时滑块最短 20
        assert_eq!(thumb(400., 40, 100_000, 0).1, 20.);
        // 往上拖到头：翻到最上面；往下拖：回到最底下
        assert_eq!(dragged(400., 40, 360, 0, -360.), 360);
        assert_eq!(dragged(400., 40, 360, 360, 1000.), 0);
        assert_eq!(dragged(400., 40, 360, 100, 0.), 100);
    }
}
