//! 记住一个终端会话的屏幕，客户端重新接上时把它还原出来。
//!
//! 程序的输出除了转给接着的客户端，也喂给这里的终端仿真器（和客户端画终端用的是同一个库），
//! 所以随时知道屏幕上是什么、光标在哪、开着哪些模式。客户端接上来时，[`Screen::dump`] 把这些
//! 写成一串普通的终端输出：往回翻的内容、屏幕、光标、模式。客户端从空白的终端开始照着处理一遍，
//! 就和一直连着没断过一样，之后接着收实时的输出。
//!
//! 还原不了的（仿真库不给读）：滚动区域、自定义的制表位、正在用的画线字符集。全屏程序换窗口
//! 大小时会自己重画，一般碰不上。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, CursorStyle, NamedColor, Processor, Rgb};
use fr_core::term_text::{Row, Shade};

/// 往回翻的内容留多少行。客户端接上时全部还原，所以不能太多
const HISTORY: usize = 2000;
/// [`Screen::last_text`] 最多给这么多个字
const LAST_TEXT: usize = 200;
/// 没写完的转义序列最多记这么长（剪贴板序列可以很长），再长就不管了
const TAIL_LIMIT: usize = 256 * 1024;

/// 这一行是不是 AI 助手的回复、调用的工具：Claude Code 以「●」开头，Codex 以「•」开头。
pub fn is_reply(text: &str) -> bool {
    text.starts_with(['\u{25cf}', '\u{2022}'])
}
/// 画样式要看的那几位（其余的是换行、宽字符占位这些排版标记）
const STYLE: Flags = Flags::INVERSE
    .union(Flags::BOLD)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::DIM)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

/// 几行几列。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Size {
    /// 至少两列一行：仿真器不接受更小的。
    pub fn new(cols: u16, rows: u16) -> Size {
        Size {
            cols: cols.clamp(2, 1000),
            rows: rows.clamp(1, 500),
        }
    }
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// 仿真器要告诉外面的事先攒在这里，处理完一批输出再看。
#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<Event>>>);

impl EventListener for Events {
    fn send_event(&self, event: Event) {
        self.0.lock().unwrap().push(event);
    }
}

pub struct Screen {
    term: Term<Events>,
    parser: Processor,
    events: Events,
    tail: Tail,
    size: Size,
    /// 程序自己设的标题（OSC 0、2）
    title: Option<String>,
}

impl Screen {
    pub fn new(size: Size) -> Screen {
        let events = Events::default();
        let config = Config {
            scrolling_history: HISTORY,
            ..Config::default()
        };
        Screen {
            term: Term::new(config, &size, events.clone()),
            parser: Processor::new(),
            events,
            tail: Tail::default(),
            size,
            title: None,
        }
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// 处理程序的一块输出。返回终端该回给程序的话（程序问光标位置、终端型号之类）：
    /// 有客户端接着时由客户端的终端回答，这里的丢掉；没人接着时才写回给程序。
    pub fn advance(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.flush_sync();
        self.parser.advance(&mut self.term, bytes);
        self.tail.feed(bytes);
        self.take_events()
    }

    /// 程序开了「同步刷新」又迟迟不结束时，到时间就把攒着的画出来。隔一会儿调用一次。
    pub fn flush_sync(&mut self) {
        let due = self
            .parser
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|deadline| deadline <= Instant::now());
        if due {
            self.parser.stop_sync(&mut self.term);
        }
    }

    fn take_events(&mut self) -> Vec<u8> {
        let events = std::mem::take(&mut *self.events.0.lock().unwrap());
        let mut reply = Vec::new();
        for event in events {
            match event {
                Event::PtyWrite(text) => reply.extend_from_slice(text.as_bytes()),
                Event::TextAreaSizeRequest(format) => {
                    let size = WindowSize {
                        num_lines: self.size.rows,
                        num_cols: self.size.cols,
                        cell_width: 8,
                        cell_height: 16,
                    };
                    reply.extend_from_slice(format(size).as_bytes());
                }
                Event::Title(title) => self.title = Some(title),
                Event::ResetTitle => self.title = None,
                // 颜色只有客户端知道；剪贴板不在服务器上
                _ => {}
            }
        }
        reply
    }

    /// 光标停在主屏一个空行的行首：shell 收了命令、新的提示符还没出来（命令在跑，没在输出）。shell
    /// 等输入时光标在提示符后面，提示符不是空的（两行的提示符第二行也有 `❯`、`>`）。
    pub fn at_blank_line(&self) -> bool {
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return false;
        }
        let grid = self.term.grid();
        let at = grid.cursor.point;
        let row = &grid[at.line];
        at.column.0 == 0 && (0..grid.columns()).all(|col| row[Column(col)].c == ' ')
    }

    /// 光标在第几列（0 起）。用的是备用屏（全屏程序）时是 None。定时任务拿它看提示符出来没有。
    pub fn cursor_column(&self) -> Option<usize> {
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return None;
        }
        Some(self.term.grid().cursor.point.column.0)
    }

    /// 屏幕上最后一行有字的内容，终端列表里给人看一眼用：从下往上找，跳过空行和只有框线的行，
    /// 去掉两头的空白和框线（全屏程序爱画框），最多 [`LAST_TEXT`] 个字。
    pub fn last_text(&self) -> String {
        self.texts(self.term.grid().screen_lines())
            .next()
            .unwrap_or_default()
    }

    /// 定时任务的运行记录里「最后一行」写什么。看光标上面的行：AI 助手的光标在输入框里，下面是状态栏；
    /// 程序结束了，光标停在 shell 的提示符那一行，上面是它最后的输出。
    /// - `replies`（运行的是 Claude Code、Codex 这类 AI 助手）：有它的回复（[`is_reply`]）时取最下面
    ///   那一条。光标藏着（Windows 上的 Claude Code）、用的是备用屏（Linux 上的 Claude Code）也找；
    /// - 不是上面那种、在主屏上、光标看得见：取光标上面最近一行有字的；
    /// - 别的（光标藏着，比如弹出了要选的框；全屏程序；光标在第一行），同 [`Self::last_text`]。
    pub fn summary_text(&self, replies: bool) -> String {
        let mode = self.term.mode();
        let cursor = self.term.grid().cursor.point.line.0;
        if cursor <= 0 {
            return self.last_text();
        }
        let above: Vec<String> = self.texts(cursor as usize).collect();
        let reply = above.iter().find(|text| is_reply(text)).filter(|_| replies);
        let near = above.first().filter(|_| {
            mode.contains(TermMode::SHOW_CURSOR) && !mode.contains(TermMode::ALT_SCREEN)
        });
        match reply.or(near) {
            Some(text) => text.clone(),
            None => self.last_text(),
        }
    }

    /// 屏幕上第 `end` 行以上有字的行，从下往上：太长折到下一行的连起来算一行，跳过空行和只有框线
    /// 的行，去掉两头的空白和框线（全屏程序爱画框），每行最多 [`LAST_TEXT`] 个字。
    fn texts(&self, end: usize) -> impl Iterator<Item = String> + '_ {
        let grid = self.term.grid();
        let columns = grid.columns();
        // 制表线和方块字符
        let edge = |c: char| c.is_whitespace() || ('\u{2500}'..='\u{259f}').contains(&c);
        let row_text = move |line: usize| -> String {
            let row = &grid[Line(line as i32)];
            (0..columns)
                .map(|col| &row[Column(col)])
                // 宽字的后半格；行尾放不下宽字时空出来的那一格
                .filter(|cell| {
                    !cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                })
                .map(|cell| cell.c)
                .collect()
        };
        // 这一行写满了、接着折到了下一行
        let wraps = move |line: usize| {
            grid[Line(line as i32)][Column(columns - 1)]
                .flags
                .contains(Flags::WRAPLINE)
        };
        let mut line = end.min(grid.screen_lines());
        std::iter::from_fn(move || {
            while line > 0 {
                line -= 1;
                let mut text = row_text(line);
                while line > 0 && wraps(line - 1) {
                    line -= 1;
                    text.insert_str(0, &row_text(line));
                }
                let text = text.trim_matches(edge);
                if !text.is_empty() {
                    return Some(text.chars().take(LAST_TEXT).collect());
                }
            }
            None
        })
    }

    /// 按回车的那一刻，屏幕上的输入区里写着的消息（见 [`fr_core::term_text::message`]）。全屏程序
    /// （备用屏）里不认。
    #[cfg(test)]
    pub fn message(&self) -> Option<String> {
        self.entered().map(|(text, _)| text)
    }

    /// 按回车的那一刻，屏幕上的输入区里写着的消息，连同光标是不是在 PowerShell 多行语句的续行上
    /// （见 [`fr_core::term_text::record`]）。全屏程序（备用屏）里不认。
    pub fn entered(&self) -> Option<(String, bool)> {
        let (rows, cursor, shown) = self.input_rows()?;
        let text = fr_core::term_text::message(&rows, cursor, shown)?;
        Some((text, fr_core::term_text::continued(&rows, cursor, shown)))
    }

    /// 屏幕上的一行行字、光标在第几行、光标显示着没有：认输入区用（备用屏里是 `None`）。
    fn input_rows(&self) -> Option<(Vec<Row>, usize, bool)> {
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return None;
        }
        let grid = self.term.grid();
        let at = grid.cursor.point;
        let shown = self.term.mode().contains(TermMode::SHOW_CURSOR);
        // 光标所在的那一行（连同折下去的）光标后面的部分：灰色的字也算淡色字（zsh、fish 的建议）
        let mut tail = false;
        let (cols, bottom) = (grid.columns(), grid.screen_lines() as i32);
        let rows: Vec<Row> = (0..bottom)
            .map(|line| {
                let row = &grid[Line(line)];
                let wraps = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
                let in_tail = shown && (line == at.line.0 || (tail && line > at.line.0));
                tail = in_tail && wraps;
                let padded = wraps
                    && line + 1 < bottom
                    && fr_core::term_text::padded(
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
                        (cell.c, shade(cell, after))
                    });
                Row::from_cells(cells, wraps)
            })
            .collect();
        Some((rows, at.line.0.max(0) as usize, shown))
    }

    /// 屏幕上的字（连同往回翻的记录有多少行、在不在备用屏）的摘要：看程序的一块输出改没改屏幕上的
    /// 字（见 `Activity`）。光改颜色、反色，挪光标，改标题的不算。
    pub fn digest(&self) -> u64 {
        use std::hash::{DefaultHasher, Hash as _, Hasher as _};
        let mut hasher = DefaultHasher::new();
        let grid = self.term.grid();
        grid.history_size().hash(&mut hasher);
        self.term
            .mode()
            .contains(TermMode::ALT_SCREEN)
            .hash(&mut hasher);
        for line in 0..grid.screen_lines() as i32 {
            let row = &grid[Line(line)];
            for col in 0..grid.columns() {
                row[Column(col)].c.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    /// 屏幕上某一行的字（测试用）。
    #[cfg(test)]
    pub(crate) fn line_text(&self, line: usize) -> String {
        let grid = self.term.grid();
        let text: String = (0..grid.columns())
            .map(|col| grid[Line(line as i32)][Column(col)].c)
            .collect();
        text.trim_end().to_string()
    }

    pub fn resize(&mut self, size: Size) {
        if size != self.size {
            self.size = size;
            self.term.resize(size);
        }
    }

    /// 把现在的屏幕写成一串终端输出，给已经画着别的东西的客户端（它收得太慢，中间的输出没发给它，
    /// 见 `Session::output`）：先把它的终端整个重置成空白的（[`RESET`]），再是 [`Self::dump`]。
    pub fn redump(&mut self) -> Vec<u8> {
        let mut out = RESET.to_vec();
        out.extend_from_slice(&self.dump());
        out
    }

    /// 把现在的屏幕写成一串终端输出，给刚接上来的客户端（它的终端是空白的）。
    pub fn dump(&mut self) -> Vec<u8> {
        let (mut out, _) = self.picture(true);
        // 最后是没写完的转义序列的前半截：客户端的解析器要停在同一个地方，后半截来了才接得上
        out.extend_from_slice(self.tail.pending());
        out
    }

    /// 给刚接上来的客户端还原屏幕用的几段输出（客户端一段收齐了才一起处理，见
    /// `TermOpened::restore`）。往回翻的记录有好几千行时整份要传一会儿（远程的线路慢），客户端
    /// 照着一块块画，看到的是从最早的记录一路滚下来。所以有往回翻的记录时分两段：先是只有屏幕的
    /// 一段，几十 KB，马上画得出来；再是 [`RESET`] 开头、从头重画、带上往回翻的记录的整份。
    pub fn restore(&mut self) -> Vec<Vec<u8>> {
        let (mut screen, history) = self.picture(false);
        if history == 0 {
            // 没有往回翻的记录：只有屏幕的这一段就是整份
            screen.extend_from_slice(self.tail.pending());
            return vec![screen];
        }
        let mut full = RESET.to_vec();
        full.extend_from_slice(&self.dump());
        vec![screen, full]
    }

    /// [`Self::dump`] 除了没写完的转义序列以外的部分：`history` 为假时不写往回翻的记录。另外返回
    /// 主屏往回翻的记录有多少行（在备用屏里时 [`Term::grid`] 是备用屏，没有往回翻的）。
    fn picture(&mut self, history: bool) -> (Vec<u8>, usize) {
        // 同步刷新攒着没画的先画上，不然拍下来的是旧画面
        self.parser.stop_sync(&mut self.term);
        self.take_events();
        let mut out = Vec::with_capacity(64 * 1024);
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        // 全屏程序用的备用屏先拍下来：下面换到主屏再换回来，仿真器会把它清掉
        let alt_part = alt.then(|| {
            let mut part = b"\x1b[?1049h\x1b[m\x1b[2J\x1b[H".to_vec();
            write_grid(&mut part, self.term.grid(), false);
            write_cursor(&mut part, self.term.grid());
            part
        });
        if alt {
            self.term.swap_alt();
        }
        let lines = self.term.grid().history_size();
        write_grid(&mut out, self.term.grid(), history);
        write_cursor(&mut out, self.term.grid());
        if let Some(part) = alt_part {
            out.extend_from_slice(&part);
            // 自己也照这一段把备用屏重画一遍，和客户端那边一模一样。另起一个解析器：
            // 原来那个可能正停在一条没写完的转义序列中间
            let mut parser: Processor = Processor::new();
            parser.advance(&mut self.term, &part);
            self.take_events();
        }
        if let Some(title) = &self.title {
            let clean: String = title.chars().filter(|c| !c.is_control()).collect();
            out.extend_from_slice(format!("\x1b]2;{clean}\x1b\\").as_bytes());
        }
        write_colors(&mut out, &self.term);
        write_modes(&mut out, &self.term);
        (out, lines)
    }
}

/// 把一个画着别的东西、解析停在哪都不知道的终端重置成空白的（见 [`Screen::redump`]）：
///
/// - CAN：没写完的转义序列作废；
/// - `CSI ? 2026 l`：结束「同步刷新」，攒着的照常处理（不然后面的都攒着，等超时才画）；
/// - RIS（`ESC c`）：内容、往回翻的、模式、光标的样子都回到初始；
/// - OSC 104、110、111、112：改过的调色板和前景、背景、光标的颜色还原（RIS 不管颜色）。
const RESET: &[u8] =
    b"\x18\x1b[?2026l\x1bc\x1b]104\x1b\\\x1b]110\x1b\\\x1b]111\x1b\\\x1b]112\x1b\\";

/// 一格的样式。
#[derive(Clone, PartialEq)]
struct Style {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
    /// 超链接的编号和地址
    link: Option<(String, String)>,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
            underline: None,
            link: None,
        }
    }
}

impl Style {
    fn of(cell: &Cell) -> Style {
        Style {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & STYLE,
            underline: cell.underline_color(),
            link: cell
                .hyperlink()
                .map(|link| (link.id().to_string(), link.uri().to_string())),
        }
    }
}

/// 现在写出去的字会是什么样式；只在变了的时候才发转义序列。
#[derive(Default)]
struct Pen(Style);

impl Pen {
    fn set(&mut self, out: &mut Vec<u8>, style: Style) {
        if style == self.0 {
            return;
        }
        let same_look = style.fg == self.0.fg
            && style.bg == self.0.bg
            && style.flags == self.0.flags
            && style.underline == self.0.underline;
        if !same_look {
            write_sgr(out, &style);
        }
        if style.link != self.0.link {
            match &style.link {
                Some((id, uri)) => {
                    out.extend_from_slice(format!("\x1b]8;id={id};{uri}\x1b\\").as_bytes());
                }
                None => out.extend_from_slice(b"\x1b]8;;\x1b\\"),
            }
        }
        self.0 = style;
    }

    fn reset(&mut self, out: &mut Vec<u8>) {
        self.set(out, Style::default());
    }
}

/// 整个重设一遍样式：先清掉，再把有的都加上。
fn write_sgr(out: &mut Vec<u8>, style: &Style) {
    let mut codes = String::from("\x1b[0");
    let flags = style.flags;
    for (flag, code) in [
        (Flags::BOLD, ";1"),
        (Flags::DIM, ";2"),
        (Flags::ITALIC, ";3"),
        (Flags::UNDERLINE, ";4"),
        (Flags::DOUBLE_UNDERLINE, ";21"),
        (Flags::UNDERCURL, ";4:3"),
        (Flags::DOTTED_UNDERLINE, ";4:4"),
        (Flags::DASHED_UNDERLINE, ";4:5"),
        (Flags::INVERSE, ";7"),
        (Flags::HIDDEN, ";8"),
        (Flags::STRIKEOUT, ";9"),
    ] {
        if flags.contains(flag) {
            codes.push_str(code);
        }
    }
    push_color(&mut codes, style.fg, 30, 90, 38, NamedColor::Foreground);
    push_color(&mut codes, style.bg, 40, 100, 48, NamedColor::Background);
    match style.underline {
        Some(Color::Spec(Rgb { r, g, b })) => codes.push_str(&format!(";58;2;{r};{g};{b}")),
        Some(Color::Indexed(index)) => codes.push_str(&format!(";58;5;{index}")),
        _ => {}
    }
    codes.push('m');
    out.extend_from_slice(codes.as_bytes());
}

/// 前景或背景色的 SGR 参数。默认色不用写（开头的 0 已经清成默认了）。
fn push_color(
    codes: &mut String,
    color: Color,
    normal: u8,
    bright: u8,
    extended: u8,
    default: NamedColor,
) {
    match color {
        Color::Named(name) if name == default => {}
        Color::Named(name) => match name as usize {
            index @ 0..=7 => codes.push_str(&format!(";{}", normal as usize + index)),
            index @ 8..=15 => codes.push_str(&format!(";{}", bright as usize + index - 8)),
            // 别的名字只在画的时候算出来，格子里不会存
            _ => {}
        },
        Color::Indexed(index) => codes.push_str(&format!(";{extended};5;{index}")),
        Color::Spec(Rgb { r, g, b }) => codes.push_str(&format!(";{extended};2;{r};{g};{b}")),
    }
}

/// 这一格是不是什么都没有（没写过字、没有底色）。行尾这样的格子不用写。
/// 认消息时这一格字算什么（见 [`Shade::of`]）。
fn shade(cell: &Cell, after: bool) -> Shade {
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
    Shade::of(
        cell.flags.contains(Flags::INVERSE),
        cell.flags.contains(Flags::DIM),
        gray,
        after,
    )
}

fn is_blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.flags.is_empty()
        && cell.extra.is_none()
        && cell.fg == Color::Named(NamedColor::Foreground)
        && cell.bg == Color::Named(NamedColor::Background)
}

fn write_cell(out: &mut Vec<u8>, pen: &mut Pen, cell: &Cell) {
    pen.set(out, Style::of(cell));
    // 制表符在格子里是原样存着的（复制时用），照写的话光标会多跳一次
    let c = if cell.c == '\t' { ' ' } else { cell.c };
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    for extra in cell.zerowidth().unwrap_or_default() {
        out.extend_from_slice(extra.encode_utf8(&mut buf).as_bytes());
    }
}

/// 把一个网格从上到下写出来：`history` 为真时从往回翻的最上面一行开始，否则只写屏幕。
/// 写的时候就当对方的光标在屏幕左上角、屏幕是空的；一行行写下去，多出屏幕的自然滚进历史。
fn write_grid(out: &mut Vec<u8>, grid: &Grid<Cell>, history: bool) {
    let cols = grid.columns();
    let bottom = grid.screen_lines() as i32 - 1;
    let top = if history {
        -(grid.history_size() as i32)
    } else {
        0
    };
    out.extend_from_slice(b"\x1b[m");
    let mut pen = Pen::default();
    let mut after_wrap = false;
    for line in top..=bottom {
        let row = &grid[Line(line)];
        // 这一行是写满以后自动折到下一行的：照样写满，让对方也自动折行，不发换行
        let wrapped = line != bottom && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        let end = if wrapped {
            cols
        } else {
            (0..cols)
                .rev()
                .find(|&col| !is_blank(&row[Column(col)]))
                .map_or(0, |col| col + 1)
        };
        for col in 0..end {
            let cell = &row[Column(col)];
            // 宽字符后面（或者行尾放不下它时留的）占位格不写，写宽字符本身时对方会自己留
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            write_cell(out, &mut pen, cell);
        }
        if !wrapped {
            // 先回到默认样式：换行滚屏时新的一行会带上当时的底色
            pen.reset(out);
            // 接在折行后面的这一行是带着上一行的样式滚出来的，没写到的格子清一下
            if after_wrap && end < cols {
                out.extend_from_slice(b"\x1b[K");
            }
            if line != bottom {
                out.extend_from_slice(b"\r\n");
            }
        }
        after_wrap = wrapped;
    }
    pen.reset(out);
}

/// 把光标放到这个网格里它该在的地方，并换成它现在的样式（之后程序写的字用这个样式）。
fn write_cursor(out: &mut Vec<u8>, grid: &Grid<Cell>) {
    let cols = grid.columns();
    let point = grid.cursor.point;
    let line = point.line.0.clamp(0, grid.screen_lines() as i32 - 1);
    let mut col = point.column.0.min(cols - 1);
    let mut pen = Pen::default();
    if grid.cursor.input_needs_wrap {
        // 光标停在最后一列、下一个字要折行：把最后那个字重写一遍，对方的光标才会进到同一个状态
        let row = &grid[Line(line)];
        if row[Column(col)].flags.contains(Flags::WIDE_CHAR_SPACER) && col > 0 {
            col -= 1;
        }
        out.extend_from_slice(format!("\x1b[{};{}H", line + 1, col + 1).as_bytes());
        write_cell(out, &mut pen, &row[Column(col)]);
    } else {
        out.extend_from_slice(format!("\x1b[{};{}H", line + 1, col + 1).as_bytes());
    }
    pen.set(out, Style::of(&grid.cursor.template));
}

/// 程序改过的颜色（OSC 4、10、11、12）。
fn write_colors<T>(out: &mut Vec<u8>, term: &Term<T>) {
    let colors = term.colors();
    let spec = |rgb: Rgb| format!("rgb:{:02x}/{:02x}/{:02x}", rgb.r, rgb.g, rgb.b);
    for index in 0..256 {
        if let Some(rgb) = colors[index] {
            out.extend_from_slice(format!("\x1b]4;{index};{}\x1b\\", spec(rgb)).as_bytes());
        }
    }
    for (name, code) in [
        (NamedColor::Foreground, 10),
        (NamedColor::Background, 11),
        (NamedColor::Cursor, 12),
    ] {
        if let Some(rgb) = colors[name] {
            out.extend_from_slice(format!("\x1b]{code};{}\x1b\\", spec(rgb)).as_bytes());
        }
    }
}

/// 开着的模式和光标的样子。放在内容后面：有的模式（不自动折行、插入）会影响前面内容的写法。
fn write_modes<T>(out: &mut Vec<u8>, term: &Term<T>) {
    let mode = *term.mode();
    let style = term.cursor_style();
    if style != CursorStyle::default() {
        let code = match style.shape {
            CursorShape::Block => Some(1),
            CursorShape::Underline => Some(3),
            CursorShape::Beam => Some(5),
            CursorShape::HollowBlock | CursorShape::Hidden => None,
        };
        if let Some(code) = code {
            let code = if style.blinking { code } else { code + 1 };
            out.extend_from_slice(format!("\x1b[{code} q").as_bytes());
        }
    }
    // 新开的终端里默认开着的：关了才要说
    for (flag, off) in [
        (TermMode::SHOW_CURSOR, "\x1b[?25l"),
        (TermMode::LINE_WRAP, "\x1b[?7l"),
        (TermMode::ALTERNATE_SCROLL, "\x1b[?1007l"),
        (TermMode::URGENCY_HINTS, "\x1b[?1042l"),
    ] {
        if !mode.contains(flag) {
            out.extend_from_slice(off.as_bytes());
        }
    }
    // 默认关着的：开了才要说
    for (flag, on) in [
        (TermMode::APP_CURSOR, "\x1b[?1h"),
        (TermMode::APP_KEYPAD, "\x1b="),
        (TermMode::MOUSE_REPORT_CLICK, "\x1b[?1000h"),
        (TermMode::MOUSE_DRAG, "\x1b[?1002h"),
        (TermMode::MOUSE_MOTION, "\x1b[?1003h"),
        (TermMode::FOCUS_IN_OUT, "\x1b[?1004h"),
        (TermMode::UTF8_MOUSE, "\x1b[?1005h"),
        (TermMode::SGR_MOUSE, "\x1b[?1006h"),
        (TermMode::BRACKETED_PASTE, "\x1b[?2004h"),
        (TermMode::INSERT, "\x1b[4h"),
        (TermMode::LINE_FEED_NEW_LINE, "\x1b[20h"),
    ] {
        if mode.contains(flag) {
            out.extend_from_slice(on.as_bytes());
        }
    }
    let kitty = (mode & TermMode::KITTY_KEYBOARD_PROTOCOL).bits() >> 18;
    if kitty != 0 {
        out.extend_from_slice(format!("\x1b[={kitty};1u").as_bytes());
    }
}

/// 跟着输出走的一个很小的状态机，只管一件事：现在是不是停在一条没写完的转义序列（或者
/// 没写完的 UTF-8 字符）中间，是的话已经来了哪些字节。
#[derive(Default)]
struct Tail {
    state: TailState,
    bytes: Vec<u8>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
enum TailState {
    #[default]
    Ground,
    /// UTF-8 字符还差几个字节
    Utf8(u8),
    /// 刚收到 ESC
    Escape,
    /// ESC 后面跟着中间字节
    EscapeMore,
    /// `ESC [` 开头的控制序列
    Csi,
    /// `ESC ]` 开头的：响铃或者 `ESC \` 结束
    Osc,
    /// `ESC P`、`ESC X`、`ESC ^`、`ESC _` 开头的：`ESC \` 结束
    Text,
}

impl Tail {
    fn feed(&mut self, data: &[u8]) {
        use TailState::*;
        for &byte in data {
            // 字符没写完就来了别的：前面的作废，这个字节当新的看
            if matches!(self.state, Utf8(_)) && !(0x80..=0xbf).contains(&byte) {
                self.state = Ground;
                self.bytes.clear();
            }
            let next = match (self.state, byte) {
                // 取消
                (Escape | EscapeMore | Csi | Osc | Text, 0x18 | 0x1a) => Ground,
                // 来一个 ESC 就是新的开始；在字符串里的话可能是结束符 `ESC \` 的前一半，前面的留着
                (_, 0x1b) => Escape,
                (Ground, 0xc2..=0xdf) => Utf8(1),
                (Ground, 0xe0..=0xef) => Utf8(2),
                (Ground, 0xf0..=0xf4) => Utf8(3),
                (Ground, _) => Ground,
                (Utf8(1), _) => Ground,
                (Utf8(left), _) => Utf8(left - 1),
                (Escape, b'[') => Csi,
                (Escape, b']') => Osc,
                (Escape, b'P' | b'X' | b'^' | b'_') => Text,
                (Escape | EscapeMore, 0x20..=0x2f) => EscapeMore,
                (Escape | EscapeMore, 0x30..=0x7e) => Ground,
                (Escape | EscapeMore, _) => self.state,
                (Csi, 0x40..=0x7e) => Ground,
                (Csi, _) => Csi,
                (Osc, 0x07) => Ground,
                (Osc, _) => Osc,
                (Text, _) => Text,
            };
            if next == Ground || self.bytes.len() >= TAIL_LIMIT {
                self.bytes.clear();
                // 太长的不记了，当它不存在
                self.state = Ground;
            } else {
                self.bytes.push(byte);
                self.state = next;
            }
        }
    }

    /// 没写完的那一截；不在序列中间时是空的。
    fn pending(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::Point;

    /// 两个仿真器里的东西是不是一样：每一行每一格、光标、模式。
    fn assert_same(a: &Screen, b: &Screen, what: &str) {
        let (ga, gb) = (a.term.grid(), b.term.grid());
        assert_eq!(ga.history_size(), gb.history_size(), "{what}：往回翻的行数");
        assert_eq!(ga.columns(), gb.columns());
        for line in -(ga.history_size() as i32)..ga.screen_lines() as i32 {
            for col in 0..ga.columns() {
                let (x, y) = (&ga[Line(line)][Column(col)], &gb[Line(line)][Column(col)]);
                let text = |cell: &Cell| if cell.c == '\t' { ' ' } else { cell.c };
                let same = text(x) == text(y)
                    && x.flags == y.flags
                    && x.zerowidth().unwrap_or_default() == y.zerowidth().unwrap_or_default()
                    && Style::of(x).link.map(|l| l.1) == Style::of(y).link.map(|l| l.1)
                    && x.underline_color() == y.underline_color()
                    // 空格的前景色看不出来，不比
                    && (x.fg == y.fg || (text(x) == ' ' && (x.flags & STYLE).is_empty()))
                    && x.bg == y.bg;
                assert!(
                    same,
                    "{what}：第 {line} 行第 {col} 列不一样\n原来 {x:?}\n还原 {y:?}"
                );
            }
        }
        assert_eq!(ga.cursor.point, gb.cursor.point, "{what}：光标位置");
        assert_eq!(
            ga.cursor.input_needs_wrap, gb.cursor.input_needs_wrap,
            "{what}：光标是不是等着折行"
        );
        let (ta, tb) = (&ga.cursor.template, &gb.cursor.template);
        assert!(
            Style::of(ta) == Style::of(tb),
            "{what}：之后写字用的样式\n原来 {ta:?}\n还原 {tb:?}"
        );
        assert_eq!(a.term.mode(), b.term.mode(), "{what}：模式");
        assert_eq!(a.title, b.title, "{what}：标题");
    }

    /// 喂一段输出，拍下来，在另一个空白的终端里还原，两边应当一样；之后两边再收同样的输出，还是一样。
    fn roundtrip(cols: u16, rows: u16, input: &[u8], more: &[u8]) -> Screen {
        let size = Size::new(cols, rows);
        let mut original = Screen::new(size);
        original.advance(input);
        let dump = original.dump();
        let mut restored = Screen::new(size);
        restored.advance(&dump);
        assert_same(&original, &restored, "还原后");
        // 拍一下不应该改变自己：再拍一次得到的一样
        assert_eq!(original.dump(), dump, "拍两次不一样");
        assert_same(&original, &restored, "拍第二次后");
        original.advance(more);
        restored.advance(more);
        assert_same(&original, &restored, "接着输出后");
        original
    }

    #[test]
    fn 最后一行有字的内容() {
        let mut screen = Screen::new(Size::new(40, 6));
        assert_eq!(screen.last_text(), "");
        // 全屏程序画的框：跳过只有框线的行，去掉两头的框线；宽字符中间不夹空格
        screen.advance("第一行\r\n\u{256d}\u{2500}\u{2500}\u{256e}\r\n\u{2502} 需要提交吗？ \u{2502}\r\n\u{2570}\u{2500}\u{2500}\u{256f}\r\n".as_bytes());
        assert_eq!(screen.last_text(), "需要提交吗？");
        screen.advance(b"  ? for shortcuts  ");
        assert_eq!(screen.last_text(), "? for shortcuts");
    }

    #[test]
    fn 运行记录的最后一行_看光标上面的() {
        let screen = |text: &str| {
            let mut screen = Screen::new(Size::new(60, 12));
            screen.advance(text.as_bytes());
            screen
        };
        // Claude Code 回复完：光标在输入框里，下面是框线和状态栏
        let claude_text = concat!(
            "\u{276f} Compute 6*7\r\n\r\n\u{25cf} kuai-42\r\n\r\n\u{273b} Churned for 2s \u{b7} done\r\n",
            "\u{2500}\u{2500}\u{2500}\u{2500}\r\n\u{276f} \r\n\u{2500}\u{2500}\u{2500}\u{2500}\r\n",
            "  \u{23f5}\u{23f5} auto mode on (shift+tab to cycle)",
            "\x1b[2A\x1b[3G"
        );
        let claude = screen(claude_text);
        assert_eq!(
            claude.last_text(),
            "\u{23f5}\u{23f5} auto mode on (shift+tab to cycle)"
        );
        assert_eq!(claude.summary_text(true), "\u{25cf} kuai-42");
        // Windows 上的 Claude Code 藏着光标（还是停在输入框里）：照样找它的回复
        let hidden = screen(&format!("{claude_text}\x1b[?25l"));
        assert_eq!(hidden.summary_text(true), "\u{25cf} kuai-42");
        assert_eq!(
            hidden.summary_text(false),
            "\u{23f5}\u{23f5} auto mode on (shift+tab to cycle)"
        );
        // Linux 上的 Claude Code 用备用屏：也照样找
        let alt = screen(&format!("\x1b[?1049h\x1b[H{claude_text}"));
        assert_eq!(alt.summary_text(true), "\u{25cf} kuai-42");
        assert_eq!(
            alt.summary_text(false),
            "\u{23f5}\u{23f5} auto mode on (shift+tab to cycle)"
        );
        // Codex：最后一条回复，不是下面「用了多久」那一行
        let codex = screen(concat!(
            "\u{2022} Ran git status\r\n  \u{2514} ?? kuai-note.txt\r\n\r\n\u{2022} done\r\n\r\n",
            "  Worked for 26s \u{2022} 18:14\r\n\r\n\u{203a} Ask Codex to do anything\r\n\r\n",
            "  GPT-6.2-Sol low \u{b7} ~/repo",
            "\x1b[2A\x1b[3G"
        ));
        assert_eq!(codex.summary_text(true), "\u{2022} done");
        // 程序报错退出、回到提示符：上面那行报错（比屏幕宽、折成了两行的，连起来）
        let failed = screen(concat!(
            "root@dev:~/app# claude --dangerously-skip-permissions \"$KUAI_TASK_PROMPT\"\r\n",
            "--dangerously-skip-permissions cannot be used with root/sudo privileges\r\n",
            "root@dev:~/app# "
        ));
        assert_eq!(failed.last_text(), "root@dev:~/app#");
        assert_eq!(
            failed.summary_text(true),
            "--dangerously-skip-permissions cannot be used with root/sudo privileges"
        );
        let powershell = screen("PS C:\\work> Write-Output $env:P\r\nkuai-ok\r\nPS C:\\work> ");
        assert_eq!(powershell.summary_text(false), "kuai-ok");
        // 不是 AI 助手的程序输出里有「•」开头的行：照样取提示符上面那行
        let script = screen("$ ./build.sh\r\n\u{2022} step 1\r\nall done\r\n$ ");
        assert_eq!(script.summary_text(false), "all done");
        // 行尾放不下的宽字折到下一行：空出来的那一格不算
        let wide = format!("{}中文\r\n$ ", "x".repeat(59));
        assert_eq!(
            screen(&wide).summary_text(false),
            format!("{}中文", "x".repeat(59))
        );
        // 弹出了要选的框（光标藏起来了）、上面没有回复：照最后一行
        let dialog = screen(concat!(
            "Trust this folder?\r\n\u{203a} 1. Trust and continue\r\n  2. Back\r\n\r\n",
            "  enter continue \u{b7} esc back\x1b[?25l\x1b[3A"
        ));
        assert_eq!(dialog.summary_text(true), "enter continue \u{b7} esc back");
        // 不是 AI 助手的全屏程序：照最后一行，不取光标上面那行；AI 助手照样找回复
        let full = screen("\x1b[?1049h\x1b[Hmenu\r\n\u{2022} item\r\nstatus\x1b[3;1H");
        assert_eq!(full.summary_text(false), "status");
        assert_eq!(full.summary_text(true), "\u{2022} item");
        // 光标在第一行：照最后一行
        let top = screen("first\r\nsecond\x1b[1;1H");
        assert_eq!(top.summary_text(false), "second");
        assert_eq!(screen("").summary_text(true), "");
    }

    #[test]
    fn 光标停在空行行首_是命令在跑_提示符后面不是() {
        let mut screen = Screen::new(Size::new(40, 6));
        screen.advance(br"PS C:\> ");
        assert!(!screen.at_blank_line());
        // 按了回车，命令在跑、没在输出
        screen.advance(br"Start-Sleep 15");
        screen.advance(b"\r\n");
        assert!(screen.at_blank_line());
        // 输出了一半（进度没换行）：不算停在空行
        screen.advance(b"copying 45%");
        assert!(!screen.at_blank_line());
        // 跑完，新的提示符出来了：两行的提示符第二行也不是空的
        screen.advance("\r\n\r\n~/repo on main\r\n\u{276f} ".as_bytes());
        assert!(!screen.at_blank_line());
        // 全屏程序里不认
        screen.advance(b"\x1b[?1049h\x1b[H\x1b[2J");
        assert!(!screen.at_blank_line());
    }

    #[test]
    fn 画着别的东西的终端先重置再还原_和空白的终端一样() {
        let size = Size::new(30, 5);
        // kuaid 这边收到的全部
        let mut seen = Vec::new();
        for n in 0..40 {
            seen.extend_from_slice(format!("\x1b[3{}mline {n}\x1b[m\r\n", n % 8).as_bytes());
        }
        seen.extend_from_slice(b"\x1b]2;now\x07\x1b[?2004h$ ");
        let mut original = Screen::new(size);
        original.advance(&seen);
        let dump = original.redump();
        // 客户端只收到了前面一截（后面的因为它收得慢没发给它）：在备用屏里，开着鼠标和同步刷新，
        // 改过调色板和背景色、设过标题，停在一条没写完的转义序列中间
        let mut client = Screen::new(size);
        client.advance(b"old 1\r\nold 2\r\n\x1b]2;old\x07\x1b]4;1;rgb:12/34/56\x1b\\\x1b]11;rgb:01/02/03\x1b\\");
        client.advance(
            b"\x1b[?1049h\x1b[?1000h\x1b[5 q\x1b[2J\x1b[Hfull screen\x1b[?2026hhidden\x1b[38;5",
        );
        client.advance(&dump);
        let mut fresh = Screen::new(size);
        fresh.advance(&original.dump());
        assert_same(&fresh, &client, "重置再还原后");
        assert_same(&original, &client, "和原来比");
        assert_eq!(
            client.term.cursor_style(),
            fresh.term.cursor_style(),
            "光标的样子"
        );
        for index in 0..269 {
            assert_eq!(
                client.term.colors()[index],
                fresh.term.colors()[index],
                "第 {index} 号颜色"
            );
        }
        // 之后的输出两边接着一样
        original.advance(b"git status\r\n");
        client.advance(b"git status\r\n");
        assert_same(&original, &client, "接着输出后");
    }

    #[test]
    fn 往回翻的记录多时分两段还原_先只有屏幕() {
        let size = Size::new(20, 5);
        // 没有往回翻的记录：一段，就是整份
        let mut short = Screen::new(size);
        short.advance(b"$ ls\r\na b\r\n$ ");
        let parts = short.restore();
        assert_eq!(parts, vec![short.dump()]);

        let mut input = Vec::new();
        for n in 0..50 {
            input.extend_from_slice(format!("\x1b[3{}mline {n}\x1b[m\r\n", n % 8).as_bytes());
        }
        // 设了标题、要焦点报告，停在一条没写完的转义序列中间
        input.extend_from_slice(b"\x1b]2;title\x07\x1b[?1004h$ \x1b[38;5");
        let mut original = Screen::new(size);
        original.advance(&input);
        let parts = original.restore();
        assert_eq!(parts.len(), 2);
        assert!(parts[1].starts_with(RESET), "第二段先把终端重置");
        assert!(parts[0].len() * 5 < parts[1].len(), "只有屏幕的一段小得多");
        let mut client = Screen::new(size);
        client.advance(&parts[0]);
        // 第一段：屏幕、光标、模式、标题都对了，还没有往回翻的
        assert_eq!(client.term.grid().history_size(), 0);
        for line in 0..5 {
            assert_eq!(
                client.line_text(line),
                original.line_text(line),
                "第 {line} 行"
            );
        }
        assert_eq!(
            client.term.grid().cursor.point,
            original.term.grid().cursor.point
        );
        assert_eq!(client.term.mode(), original.term.mode());
        assert_eq!(client.title, original.title);
        client.advance(&parts[1]);
        assert_same(&original, &client, "两段都还原后");
        // 拍过以后自己没变；没写完的那条序列的后半截来了两边接得上
        assert_eq!(original.restore(), parts);
        original.advance(b";1mX");
        client.advance(b";1mX");
        assert_same(&original, &client, "接着输出后");
    }

    #[test]
    fn 在备用屏里分两段还原() {
        let size = Size::new(20, 5);
        let mut input = Vec::new();
        for n in 0..30 {
            input.extend_from_slice(format!("line {n}\r\n").as_bytes());
        }
        input.extend_from_slice(b"\x1b[?1049h\x1b[2J\x1b[Hfull screen\r\nmenu");
        let mut original = Screen::new(size);
        original.advance(&input);
        let parts = original.restore();
        assert_eq!(parts.len(), 2, "备用屏里看的是主屏往回翻的记录有没有");
        let mut client = Screen::new(size);
        client.advance(&parts[0]);
        // 第一段：备用屏画出来了
        assert!(client.term.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(client.line_text(0), "full screen");
        assert_eq!(client.line_text(1), "menu");
        client.advance(&parts[1]);
        assert_same(&original, &client, "两段都还原后");
        assert_eq!(original.restore(), parts, "拍过以后自己没变");
        // 回到主屏：主屏和往回翻的记录也一样
        original.advance(b"\x1b[?1049l");
        client.advance(b"\x1b[?1049l");
        assert_same(&original, &client, "回到主屏后");
    }

    #[test]
    fn 普通的几行字() {
        roundtrip(
            40,
            6,
            b"$ ls\r\nCargo.toml  README.md\r\n$ ",
            b"git status\r\n",
        );
    }

    #[test]
    fn 超出屏幕的滚进历史() {
        let mut input = Vec::new();
        for n in 0..50 {
            input.extend_from_slice(format!("line {n}\r\n").as_bytes());
        }
        let screen = roundtrip(20, 5, &input, b"tail\r\n");
        assert_eq!(screen.term.grid().history_size(), 47);
    }

    #[test]
    fn 颜色和样式() {
        let input = b"\x1b[31mred\x1b[0m \x1b[1;4;38;5;200mbold-underline\x1b[0m \
            \x1b[48;2;1;2;3mtrue-bg\x1b[0m \x1b[7minv\x1b[27m \x1b[2mdim\x1b[22m \
            \x1b[3mit\x1b[23m \x1b[9mstrike\x1b[29m \x1b[4:3mcurl\x1b[4:0m \
            \x1b[21mdbl\x1b[24m \x1b[4;58;2;9;8;7mulc\x1b[0m \x1b[92;104mbright\x1b[0m\r\n";
        roundtrip(120, 5, input, b"\x1b[33mmore\x1b[m");
    }

    #[test]
    fn 写字的样式留着() {
        // 样式开着没关就断开了：还原后接着写的字还是这个样式
        roundtrip(30, 4, b"plain \x1b[1;35;44m", b"styled\x1b[m plain");
    }

    #[test]
    fn 带底色的空格和清行() {
        roundtrip(
            30,
            4,
            b"\x1b[44m\x1b[K head\x1b[m\r\n\x1b[41m   \x1b[m|",
            b"x",
        );
    }

    #[test]
    fn 中文是两格宽() {
        roundtrip(
            20,
            4,
            "中文测试 abc 混排\r\n第二行".as_bytes(),
            "接着写".as_bytes(),
        );
    }

    #[test]
    fn 长行自动折行() {
        let line = "0123456789".repeat(7);
        roundtrip(16, 6, line.as_bytes(), b"\r\nnext");
        // 折行的行滚进了历史
        let many = format!("{line}\r\n{line}\r\n{line}\r\nend");
        roundtrip(16, 4, many.as_bytes(), b"!");
    }

    #[test]
    fn 行尾放不下的宽字符整个折到下一行() {
        // 9 列：四个汉字占 8 列，第五个放不下
        roundtrip(9, 4, "一二三四五六".as_bytes(), "七".as_bytes());
    }

    #[test]
    fn 折行时带着底色() {
        let input = format!("\x1b[42m{}\x1b[m tail", "x".repeat(25));
        roundtrip(10, 3, input.as_bytes(), b"\r\nok");
    }

    #[test]
    fn 正好写满一行() {
        // 写满后换行：不算折行
        roundtrip(10, 4, b"0123456789\r\nnext", b"!");
        // 写满后不换行：光标停在最后一列等着折行，下一个字才折
        let screen = roundtrip(10, 4, b"0123456789", b"A");
        assert_eq!(
            screen.term.grid().cursor.point,
            Point::new(Line(1), Column(1))
        );
        // 最后一格是宽字符
        roundtrip(10, 4, "01234567中".as_bytes(), b"A");
    }

    #[test]
    fn 光标不在末尾() {
        roundtrip(20, 6, b"one\r\ntwo\r\nthree\x1b[2;2H", b"X");
        // 屏幕下半截是空的
        roundtrip(20, 6, b"only\x1b[4;7H", b"Y");
    }

    #[test]
    fn 制表符() {
        roundtrip(40, 3, b"a\tb\tc", b"\td");
    }

    #[test]
    fn 全屏程序的备用屏() {
        let mut input = b"$ vim\r\n".to_vec();
        input.extend_from_slice(b"\x1b[?1049h\x1b[2J\x1b[H\x1b[7m status \x1b[m\r\n~\r\n~");
        input.extend_from_slice(b"\x1b[?1h\x1b=\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[2;3H");
        let mut screen = roundtrip(30, 5, &input, b"typed");
        assert!(screen.term.mode().contains(TermMode::ALT_SCREEN));
        // 退出全屏程序后，主屏上原来的东西还在
        screen.advance(b"\x1b[?1049l");
        let dump = screen.dump();
        let mut restored = Screen::new(Size::new(30, 5));
        restored.advance(&dump);
        assert_same(&screen, &restored, "退出备用屏后");
        let first: String = (0..5)
            .map(|col| restored.term.grid()[Line(0)][Column(col)].c)
            .collect();
        assert_eq!(first, "$ vim");
    }

    #[test]
    fn 光标藏起来和别的模式() {
        let screen = roundtrip(20, 3, b"\x1b[?25l\x1b[?1004h\x1b[?7l\x1b[5 qhidden", b"");
        assert!(!screen.term.mode().contains(TermMode::SHOW_CURSOR));
    }

    #[test]
    fn 标题和超链接() {
        let input = b"\x1b]2;user@host: ~/repo\x07see \x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ end";
        let screen = roundtrip(40, 3, input, b"");
        assert_eq!(screen.title(), Some("user@host: ~/repo"));
    }

    #[test]
    fn 组合字符() {
        roundtrip(20, 3, "e\u{301}a\u{308}o".as_bytes(), b"k");
    }

    #[test]
    fn 停在转义序列中间() {
        // 颜色序列只来了前一半就断开重连：后一半来了以后两边还得一样
        roundtrip(20, 3, b"abc\x1b[3", b"1mred\x1b[m");
        // 一个汉字的三个字节只来了两个
        roundtrip(20, 3, &"好".as_bytes()[..2], &"好".as_bytes()[2..]);
        // 标题只来了一半
        roundtrip(20, 3, b"x\x1b]2;half", b" title\x07y");
    }

    #[test]
    fn 同步刷新攒着的也拍得到() {
        // 程序开了同步刷新还没结束：这时拍，画面里要有它已经写的东西
        let size = Size::new(20, 3);
        let mut original = Screen::new(size);
        original.advance(b"before\r\n\x1b[?2026hduring");
        let dump = original.dump();
        let mut restored = Screen::new(size);
        restored.advance(&dump);
        let text: String = (0..6)
            .map(|col| restored.term.grid()[Line(1)][Column(col)].c)
            .collect();
        assert_eq!(text, "during");
    }

    #[test]
    fn 改了大小以后拍() {
        let size = Size::new(30, 6);
        let mut original = Screen::new(size);
        original.advance("一行比较长的字，改窄了以后要折行 abcdefghijklmnop\r\n$ ".as_bytes());
        let narrow = Size::new(12, 4);
        original.resize(narrow);
        let dump = original.dump();
        let mut restored = Screen::new(narrow);
        restored.advance(&dump);
        assert_same(&original, &restored, "改窄后");
    }

    #[test]
    fn 回答程序的提问() {
        let mut screen = Screen::new(Size::new(20, 3));
        // 问光标位置
        let reply = screen.advance(b"ab\x1b[6n");
        assert_eq!(reply, b"\x1b[1;3R");
        // 问终端型号
        assert!(!screen.advance(b"\x1b[c").is_empty());
    }

    #[test]
    fn 认得出没写完的序列() {
        let pending = |data: &[u8]| {
            let mut tail = Tail::default();
            tail.feed(data);
            tail.pending().to_vec()
        };
        assert_eq!(pending(b"plain text\r\n"), b"");
        assert_eq!(pending(b"x\x1b"), b"\x1b");
        assert_eq!(pending(b"x\x1b[38;5"), b"\x1b[38;5");
        assert_eq!(pending(b"x\x1b[38;5;1m"), b"");
        assert_eq!(pending(b"\x1b]0;tit"), b"\x1b]0;tit");
        assert_eq!(pending(b"\x1b]0;title\x07"), b"");
        assert_eq!(pending(b"\x1b]0;title\x1b"), b"\x1b]0;title\x1b");
        assert_eq!(pending(b"\x1b]0;title\x1b\\"), b"");
        assert_eq!(pending(b"\x1bP1$r"), b"\x1bP1$r");
        assert_eq!(pending(b"\x1bP1$r\x1b\\ok"), b"");
        assert_eq!(pending(b"\x1b(B"), b"");
        assert_eq!(pending(b"\x1b("), b"\x1b(");
        assert_eq!(pending(b"\x1b[12\x18"), b"");
        assert_eq!(pending(&"中".as_bytes()[..1]), &"中".as_bytes()[..1]);
        assert_eq!(pending("中".as_bytes()), b"");
        // 一个字节一个字节地喂，结果一样
        let mut tail = Tail::default();
        for byte in b"ok\x1b[1;3" {
            tail.feed(&[*byte]);
        }
        assert_eq!(tail.pending(), b"\x1b[1;3");
    }

    #[test]
    fn 认消息时灰字建议_藏着光标的菜单不算() {
        let message = |bytes: &[u8]| {
            let mut screen = Screen::new(Size::new(60, 12));
            screen.advance(bytes);
            screen.message()
        };
        // PowerShell：打了 claude，后面是淡色斜体的建议（PSReadLine 2.4.5 默认的样式），光标在建议前面
        assert_eq!(
            message(b"PS C:\\> \x1b[93mclaude\x1b[97;2;3m update\x1b[0m\x1b[7D").as_deref(),
            Some("claude")
        );
        // zsh-autosuggestions：光标后面的 8 号灰色字
        assert_eq!(
            message(b"me@box:~$ git\x1b[38;5;8m log --oneline\x1b[0m\x1b[13D").as_deref(),
            Some("git")
        );
        // OpenCode mini：占位提示用 RGB 灰色，光标在它前面。
        assert_eq!(
            message("┃ \x1b[38;2;128;128;128mAsk anything\x1b[0m\x1b[12D".as_bytes()),
            None
        );
        // 光标前面的灰色字是打的（PowerShell 把参数画成灰色）
        assert_eq!(
            message(b"PS C:\\> \x1b[93mGet-Date \x1b[90m-Format\x1b[0m").as_deref(),
            Some("Get-Date -Format")
        );
        // Claude Code 问信任文件夹：光标藏着，指针在第一项上
        let menu = b" \xe2\x9d\xaf No, exit\r\n   Yes, I trust this folder\x1b[1A";
        assert_eq!(message(&[b"\x1b[?25l", &menu[..]].concat()), None);
        // 光标在的照样算（shell 的 ❯ 提示符）
        assert_eq!(message(menu).as_deref(), Some("No, exit"));
    }

    #[test]
    fn 认消息时折行处的空格留着_宽字放不下留的那一格不算() {
        let message = |bytes: &[u8]| {
            let mut screen = Screen::new(Size::new(20, 6));
            screen.advance(bytes);
            screen.message()
        };
        // 第 20 格正好是空格，折到下一行（原来记成 `echo 12| x`）
        assert_eq!(
            message(b"PS C:\\repo> echo 12 | x").as_deref(),
            Some("echo 12 | x")
        );
        // 「二」在第 20 格放不下，整个折到下一行，那一格空着
        assert_eq!(
            message("PS C:\\repo> echo 一二三四".as_bytes()).as_deref(),
            Some("echo 一二三四")
        );
        // 经 ConPTY 来的：它在那一格写一个普通空格再写「二」（测试终端里记成了两个空格）
        assert_eq!(
            message("PS C:\\repo> echo 一 二三四".as_bytes()).as_deref(),
            Some("echo 一二三四")
        );
    }
}
