//! 终端里的链接：按住 Ctrl，鼠标底下的网址、文件路径画上下划线、鼠标变成手形，单击打开（和
//! Windows 终端、VS Code 的终端一样）。程序要了鼠标的（opencode、Codex）也是按住 Ctrl 单击。
//!
//! 不按 Ctrl 看不出哪里能点：鼠标在网址、路径上停 [`REST`]，画一条灰的虚线，下面提示「Ctrl+单击…
//! · 右键更多」。右键点在网址、路径上弹它的菜单（在文件页中打开、在浏览器中打开、用默认程序打开、
//! 复制路径……，见 [`fill_menu`]），别处右键照旧。
//!
//! - 网址（http、https）：在默认浏览器里打开。程序用 OSC 8 写的超链接也认。
//! - 仓库里的文件、目录：到这个仓库的文件页打开（带行号的跳到那一行），服务器上的仓库也一样，见
//!   [`TerminalEvent::OpenFile`]。相对路径按那一段输出当时所在的目录算：往上找最近一行写着目录的
//!   提示符（见 [`prompt_dir_above`]），`cd` 过也对得上；找不到（提示符里没写目录）时从终端开在的
//!   目录、仓库根算。
//! - 仓库外的本机文件、目录：用系统给它设的默认程序打开、在资源管理器里打开。服务器上仓库外的不认。
//!
//! 认字的规则在 `fr_core::term_link`。路径要真的在（问一下仓库通道或者本机的文件系统）才算链接，
//! 问过的记一会儿：鼠标挪来挪去不用每次都问。

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column as GridColumn, Line as GridLine, Point as GridPoint};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;
use fr_channel::Channel;
use fr_core::i18n::tr;
use fr_core::term_link::{self, Place, Target};
use fr_core::term_text;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{Proxy, TerminalEvent, TerminalView};
use crate::open_with;
use crate::settings::RepoEntry;

/// 问过的路径在不在，记这么久
const CHECKED_FOR: Duration = Duration::from_secs(5);
/// 没按 Ctrl 时鼠标在网址、路径上停这么久，画虚线、出提示。扫过去不出（停下来才问主机路径在不在）
pub(super) const REST: Duration = Duration::from_millis(500);
/// 菜单顶上那一行的路径最多显示这么多个字，长的把中间省掉
const SHOWN_MAX: usize = 60;
/// 找提示符时往上最多看这么多行
const PROMPT_SCAN: i32 = 5000;
/// 程序主动折行时，只在鼠标所在行附近找括号里的路径。
const PATH_WRAP_ROWS: i32 = 8;

/// 认链接里的路径用的：终端开在哪个仓库、哪个目录，怎么去问文件在不在。
#[derive(Clone)]
pub struct LinkBase {
    /// 仓库根目录（终端里的程序看到的写法：本机是 `C:\…`，服务器上是那边的路径）
    pub root: String,
    /// 终端开在仓库里的哪个子目录（相对仓库根，`/` 分隔；空的是仓库根）
    pub sub: String,
    /// 仓库在这台电脑上：仓库外的文件也能打开
    pub local: bool,
    /// 仓库所在的主机（本机是 None）：在浏览器中打开时按它记网页服务的口令、换连接
    pub server: Option<String>,
    /// 现在的仓库通道（连接断了、换了时跟着换；没有就问不了）
    pub channel: ChannelNow,
}

/// 现拿仓库通道（见 [`LinkBase::channel`]）。
pub type ChannelNow = Rc<dyn Fn(&App) -> Option<Channel>>;

/// 鼠标底下的链接：从哪一格到哪一格（含），单击时打开什么。
#[derive(Clone, PartialEq)]
pub(super) struct Hover {
    pub start: GridPoint,
    pub end: GridPoint,
    /// 没按 Ctrl、停够了显示的：画虚线、出提示，单击不打开
    pub soft: bool,
    open: Open,
}

/// 网格上的一段：从哪一格到哪一格（含）。
pub(super) type Span = (GridPoint, GridPoint);

/// 右键点在网址、路径上：菜单列它的几项（见 [`fill_menu`]）。
pub(super) struct LinkMenu {
    pub start: GridPoint,
    pub end: GridPoint,
    /// 菜单顶上那一行：认出来的写法（路径带着行号）
    shown: String,
    state: MenuState,
}

#[derive(Clone)]
enum MenuState {
    Ready(Open),
    /// 还在问主机这个路径在不在：问的是哪个（见 [`ask_key`]）、行列
    Asking(String, Option<(u32, u32)>),
    /// 问过了，没有这个路径
    Missing,
}

/// 链接菜单里点了哪一项。
#[derive(Clone)]
enum MenuAction {
    /// 在文件页中打开、显示仓库里的路径
    Files {
        path: String,
        at: Option<(u32, u32)>,
        dir: bool,
    },
    /// 仓库里的网页在浏览器中打开
    Browser(String),
    /// 仓库里的文件用默认程序打开
    Default(String),
    Url(String),
    /// 仓库外的本机文件用默认程序打开、目录在资源管理器里打开
    OutsideFile(String),
    OutsideDir(String),
    /// 在资源管理器中显示（本机的完整路径）
    Reveal(String),
    CopyText(String),
    Copy,
    Paste,
}

#[derive(Clone, PartialEq)]
enum Open {
    Url(String),
    /// 仓库里的文件、目录（相对仓库根）
    Repo {
        path: String,
        at: Option<(u32, u32)>,
        dir: bool,
    },
    /// 仓库外的本机文件、目录（绝对路径）
    Outside {
        path: String,
        dir: bool,
    },
}

/// 问过的一个路径。
struct Checked {
    /// 在哪、是不是目录；不在的是 `None`
    found: Option<(Place, bool)>,
    at: Instant,
}

/// 按住 Ctrl 单击时那里的路径还在问：问到了就打开它（服务器上的仓库、慢的线路上会碰到）。
struct Pending {
    /// 问的是哪个（见 [`ask_key`]）
    key: String,
    at: Option<(u32, u32)>,
    window: AnyWindowHandle,
}

/// 链接这一块的状态（[`TerminalView`] 里的 `links`）。
#[derive(Default)]
pub(super) struct LinkUi {
    /// 终端开在哪个仓库：认路径用；没有就只认网址
    base: Option<LinkBase>,
    /// 鼠标底下的链接：按着 Ctrl 的画下划线，单击打开；没按 Ctrl、停够了的画虚线、出提示
    pub hover: Option<Hover>,
    /// 鼠标最后在终端里的哪里（离开了是 None）、那时按着 Ctrl 没有：按下、松开 Ctrl 时重新看
    pub mouse_at: Option<Point<Pixels>>,
    ctrl: bool,
    /// 问过的路径在不在（按 [`ask_key`]：同一个写法在不同的目录下是不同的文件）
    checked: HashMap<String, Checked>,
    /// 正在问的
    asking: HashSet<String>,
    /// 鼠标底下的路径正在问（还不知道算不算链接）：问的是哪个、行列
    waiting: Option<(String, Option<(u32, u32)>)>,
    pending: Option<Pending>,
    /// 鼠标停在哪一格、停够 [`REST`] 了没有；计时的任务
    rest_at: Option<GridPoint>,
    rested: bool,
    rest_task: Option<Task<()>>,
    /// 右键点在了网址、路径上（菜单开着时也在）
    pub menu: Option<LinkMenu>,
    /// 开着的链接菜单：路径问到了以后重画它
    menu_view: Option<(WeakEntity<PopupMenu>, AnyWindowHandle)>,
    /// 提示：文字、做好的提示框（文字变了才重做）
    tip: Option<(SharedString, AnyView)>,
}

impl LinkUi {
    /// 画实线、画虚线的各是哪一段：按着 Ctrl 指着的、右键菜单说的画实线，没按 Ctrl 停着的画虚线。
    pub(super) fn lines(&self) -> (Option<Span>, Option<Span>) {
        let menu = self.menu.as_ref().map(|menu| (menu.start, menu.end));
        match &self.hover {
            Some(hover) if hover.soft => (menu, Some((hover.start, hover.end))),
            Some(hover) => (Some((hover.start, hover.end)), None),
            None => (menu, None),
        }
    }

    /// 鼠标变成手形：按着 Ctrl 指着链接。
    pub(super) fn pointer(&self) -> bool {
        self.hover.as_ref().is_some_and(|hover| !hover.soft)
    }
}

/// 网格上 `point` 所在的那一行，连同写满折下去的前后几行：每个字，和它在网格上的哪一格。宽字的后半格、
/// 宽字放不下时留在行尾的那一格不算（见 [`term_text::padded`]）。
fn line_at(term: &Term<Proxy>, point: GridPoint) -> (Vec<char>, Vec<GridPoint>) {
    let grid = term.grid();
    let (cols, top) = (grid.columns(), -(grid.history_size() as i32));
    let bottom = grid.screen_lines() as i32 - 1;
    let wraps = |line: i32| {
        grid[GridLine(line)][GridColumn(cols - 1)]
            .flags
            .contains(Flags::WRAPLINE)
    };
    // 很长的一段（一大段没换行的输出）只看鼠标前后几屏
    let (mut first, mut last) = (point.line.0, point.line.0);
    while first > top && wraps(first - 1) && point.line.0 - first < 50 {
        first -= 1;
    }
    while last < bottom && wraps(last) && last - point.line.0 < 50 {
        last += 1;
    }
    let mut chars = Vec::new();
    let mut cells = Vec::new();
    for line in first..=last {
        let row = &grid[GridLine(line)];
        let padded = wraps(line)
            && line < bottom
            && term_text::padded(
                row[GridColumn(cols - 1)].c,
                grid[GridLine(line + 1)][GridColumn(0)]
                    .flags
                    .contains(Flags::WIDE_CHAR),
            );
        for col in 0..cols {
            let cell = &row[GridColumn(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                || (padded && col == cols - 1)
            {
                continue;
            }
            chars.push(cell.c);
            cells.push(GridPoint::new(GridLine(line), GridColumn(col)));
        }
    }
    (chars, cells)
}

/// 问一个路径时记的名字：提示符里的目录（没有是空的）和路径的写法。
fn ask_key(dir: Option<&str>, raw: &str) -> String {
    format!("{}\n{raw}", dir.unwrap_or(""))
}

/// `point` 那一行和它上面，最近一行写着目录的 shell 提示符里的目录（见 [`term_text::prompt_dir`]）：
/// 那一段输出当时所在的目录。往上最多看 [`PROMPT_SCAN`] 行；行首不像提示符的不读整行。
fn prompt_dir_above(term: &Term<Proxy>, point: GridPoint) -> Option<String> {
    let grid = term.grid();
    let (cols, top) = (grid.columns(), -(grid.history_size() as i32));
    let wraps = |line: i32| {
        grid[GridLine(line)][GridColumn(cols - 1)]
            .flags
            .contains(Flags::WRAPLINE)
    };
    // 折开的一行从哪一行起
    let start_of = |mut line: i32| {
        while line > top && wraps(line - 1) {
            line -= 1;
        }
        line
    };
    let lowest = point.line.0 - PROMPT_SCAN;
    let mut line = start_of(point.line.0);
    loop {
        let row = &grid[GridLine(line)];
        let head: String = (0..cols.min(80))
            .map(|col| row[GridColumn(col)].c)
            .collect();
        let head = head.trim_start();
        if head.starts_with("PS ") || head.contains('@') {
            let (chars, _) = line_at(term, GridPoint::new(GridLine(line), GridColumn(0)));
            let text: String = chars.into_iter().collect();
            if let Some(dir) = term_text::prompt_dir(&text) {
                return Some(dir);
            }
        }
        if line <= top || line <= lowest {
            return None;
        }
        line = start_of(line - 1);
    }
}

/// 括号里的一段路径，到右括号或行尾为止；折行之间的空白是程序排版留的。
fn path_part(chars: &[char], start: usize) -> Option<(Range<usize>, bool)> {
    let end = start
        + chars[start..]
            .iter()
            .take_while(|&&ch| ch != ')' && !ch.is_whitespace())
            .count();
    let closed = chars.get(end) == Some(&')');
    (end > start && (closed || chars[end..].iter().all(|ch| ch.is_whitespace())))
        .then_some((start..end, closed))
}

fn wrapped_path_start(chars: &[char]) -> Option<Range<usize>> {
    let open = chars.windows(2).rposition(|pair| pair == ['(', '/'])?;
    let (part, closed) = path_part(chars, open + 1)?;
    (!closed).then_some(part)
}

fn wrapped_path_continuation(chars: &[char]) -> Option<(Range<usize>, bool)> {
    let start = chars.iter().position(|ch| !ch.is_whitespace())?;
    if start == 0 {
        return None;
    }
    path_part(chars, start)
}

/// Codex 排版时会把 `标签 (/C:/…/文件:行)` 里的路径拆到后面的缩进行。
/// 只有括号闭合且连起来仍是完整的路径才认，普通的终端换行各算各的。
fn wrapped_path_at(
    term: &Term<Proxy>,
    point: GridPoint,
    chars: &[char],
    at: usize,
) -> Option<(GridPoint, GridPoint, Target)> {
    if !wrapped_path_start(chars).is_some_and(|part| part.contains(&at))
        && !wrapped_path_continuation(chars).is_some_and(|(part, _)| part.contains(&at))
    {
        return None;
    }

    let grid = term.grid();
    let mut row = (point.line.0 - PATH_WRAP_ROWS).max(-(grid.history_size() as i32));
    let last = (point.line.0 + PATH_WRAP_ROWS).min(grid.screen_lines() as i32 - 1);
    let mut candidate: Option<(GridPoint, String)> = None;
    while row <= last {
        let (chars, cells) = line_at(term, GridPoint::new(GridLine(row), GridColumn(0)));
        row = cells.last()?.line.0 + 1;
        if let Some((start, mut path)) = candidate.take()
            && let Some((part, closed)) = wrapped_path_continuation(&chars)
        {
            let end = cells[part.end - 1];
            path.extend(&chars[part]);
            if !closed {
                candidate = Some((start, path));
                continue;
            }
            if start <= point
                && point <= end
                && let Some(link) = term_link::link_at(&path, 0)
                && link.span == (0..path.chars().count())
                && matches!(link.target, Target::Path { .. })
            {
                return Some((start, end, link.target));
            }
        }
        if let Some(part) = wrapped_path_start(&chars) {
            let start = cells[part.start];
            if start <= point {
                candidate = Some((start, chars[part].iter().collect()));
            }
        }
    }
    None
}

/// 网格上 `point` 那里的链接：从哪一格到哪一格、指向哪。程序用 OSC 8 写的超链接优先。
fn find(term: &Term<Proxy>, point: GridPoint) -> Option<(GridPoint, GridPoint, Target)> {
    let (chars, cells) = line_at(term, point);
    // 宽字的后半格：算前半格那个字
    let at = cells.iter().position(|cell| *cell == point).or_else(|| {
        let left = GridPoint::new(point.line, GridColumn(point.column.0.checked_sub(1)?));
        cells.iter().position(|cell| *cell == left)
    })?;
    let grid = term.grid();
    if let Some(link) = grid[cells[at]].hyperlink() {
        let same = |cell: &GridPoint| grid[*cell].hyperlink().is_some_and(|other| other == link);
        let start = (0..=at).rev().take_while(|&i| same(&cells[i])).last()?;
        let end = (at..cells.len()).take_while(|&i| same(&cells[i])).last()?;
        let uri = link.uri();
        let target = match uri.strip_prefix("file://") {
            Some(_) => match term_link::link_at(uri, 0)?.target {
                path @ Target::Path { .. } => path,
                Target::Url(_) => return None,
            },
            None if uri.starts_with("https://") || uri.starts_with("http://") => {
                Target::Url(uri.to_string())
            }
            None => return None,
        };
        return Some((cells[start], cells[end], target));
    }
    if let Some(link) = wrapped_path_at(term, cells[at], &chars, at) {
        return Some(link);
    }
    let text: String = chars.iter().collect();
    let link = term_link::link_at(&text, at)?;
    Some((
        cells[link.span.start],
        cells[link.span.end - 1],
        link.target,
    ))
}

impl TerminalView {
    /// 告诉终端它开在哪个仓库（认链接里的路径用，见 [`LinkBase`]）。
    pub fn set_link_base(&mut self, base: LinkBase) {
        self.links.base = Some(base);
        self.links.checked.clear();
    }

    /// 鼠标挪到 `position`、按着 Ctrl（`ctrl`）没有：重新看底下的链接。按着 Ctrl 的马上看；没按的
    /// 停够 [`REST`] 才看（见 [`Self::rest_at`]）。正在选字、按着报给程序的鼠标键、右键菜单开着时
    /// 没有链接。
    pub(super) fn hover_link(
        &mut self,
        position: Point<Pixels>,
        ctrl: bool,
        cx: &mut Context<Self>,
    ) {
        self.links.mouse_at = Some(position);
        self.links.ctrl = ctrl;
        self.links.waiting = None;
        self.rest_at(position, cx);
        let usable = !self.selecting && self.mouse_held.is_none() && self.links.menu.is_none();
        let found = match (ctrl || self.links.rested) && usable {
            true => self.link_under(position, cx).map(|hover| Hover {
                soft: !ctrl,
                ..hover
            }),
            false => None,
        };
        if found != self.links.hover {
            self.links.hover = found;
            cx.notify();
        }
    }

    /// 鼠标在 `position`：换了一格、又不在提示着的链接上，就重新计时，停够 [`REST`] 再看底下的链接。
    fn rest_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some((cell, _)) = self.grid_point(position) else {
            return;
        };
        if self.links.rest_at == Some(cell) {
            return;
        }
        self.links.rest_at = Some(cell);
        let on_hint = self
            .links
            .hover
            .as_ref()
            .is_some_and(|hover| hover.soft && hover.start <= cell && cell <= hover.end);
        if on_hint {
            return;
        }
        self.links.rested = false;
        self.links.rest_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REST).await;
            this.update(cx, |this, cx| {
                this.links.rested = true;
                if let Some(position) = this.links.mouse_at {
                    let ctrl = this.links.ctrl;
                    this.hover_link(position, ctrl, cx);
                }
            })
            .ok();
        }));
    }

    /// 打字、按下鼠标：停着的提示收起来（鼠标再挪动、停够了再出）。
    pub(super) fn hide_link_tip(&mut self, cx: &mut Context<Self>) {
        self.links.rested = false;
        self.links.rest_task = None;
        if self.links.hover.as_ref().is_some_and(|hover| hover.soft) {
            self.links.hover = None;
            cx.notify();
        }
    }

    /// 鼠标离开了终端：不再有链接。
    pub(super) fn leave_link(&mut self, cx: &mut Context<Self>) {
        self.links.mouse_at = None;
        self.links.waiting = None;
        self.links.rest_at = None;
        self.links.rested = false;
        self.links.rest_task = None;
        if self.links.hover.take().is_some() {
            cx.notify();
        }
    }

    /// 按下、松开 Ctrl：鼠标还在终端里的话重新看一下。
    pub(super) fn ctrl_changed(&mut self, ctrl: bool, cx: &mut Context<Self>) {
        if let Some(position) = self.links.mouse_at
            && ctrl != self.links.ctrl
        {
            self.hover_link(position, ctrl, cx);
        }
    }

    /// 网格上 `position` 那里的链接：从哪一格到哪一格、指向哪，路径的话还有提示符里的目录。
    fn target_at(
        &self,
        position: Point<Pixels>,
    ) -> Option<(GridPoint, GridPoint, Target, Option<String>)> {
        let (point, _) = self.grid_point(position)?;
        let session = self.session.as_ref()?;
        let term = session.term.lock();
        let (start, end, target) = find(&term, point)?;
        let dir = match target {
            Target::Path { .. } => prompt_dir_above(&term, start),
            Target::Url(_) => None,
        };
        Some((start, end, target, dir))
    }

    fn link_under(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> Option<Hover> {
        let (start, end, target, dir) = self.target_at(position)?;
        let open = match target {
            Target::Url(url) => Open::Url(url),
            Target::Path { path, line, col } => {
                let at = line.map(|line| (line, col.unwrap_or(1)));
                let key = ask_key(dir.as_deref(), &path);
                match self.links.checked.get(&key) {
                    _ if self.links.asking.contains(&key) => {
                        self.links.waiting = Some((key, at));
                        return None;
                    }
                    Some(checked) if checked.at.elapsed() < CHECKED_FOR => {
                        opening(checked.found.clone()?, at)
                    }
                    _ => {
                        if self.check_path(path, dir, key.clone(), cx) {
                            self.links.waiting = Some((key, at));
                        }
                        return None;
                    }
                }
            }
        };
        Some(Hover {
            start,
            end,
            soft: false,
            open,
        })
    }

    /// 路径 `raw` 可能指的几处，挨个问；还有问的时候用的通道。`dir` 是提示符里写的目录（见
    /// [`prompt_dir_above`]）。不知道开在哪个仓库、没有可问的写法时没有。
    ///
    /// 几种写法（git 改动里的 `a/`、`b/` 去掉）挨个按这些目录试：提示符里的目录；它在仓库里或者没有时，
    /// 再试终端开在的目录、仓库根。提示符里的目录在仓库外时只按它算：说的是那里的文件，仓库里同名的
    /// 不是它。
    fn places(
        &self,
        raw: &str,
        dir: Option<&str>,
        cx: &App,
    ) -> Option<(LinkBase, Option<Channel>, Vec<Place>)> {
        let base = self.links.base.clone()?;
        let channel = (base.channel)(cx);
        // 服务器上的 `~`：那台的主目录
        let home = match &channel {
            Some(Channel::Remote(conn)) => Some(conn.hello.home.clone()),
            _ => None,
        };
        let here = dir.and_then(|dir| {
            term_link::resolve_at(&base.root, dir, home.as_deref(), ".").map(|place| (dir, place))
        });
        let mut tries: Vec<Place> = Vec::new();
        let mut add = |place: Option<Place>| {
            if let Some(place) = place
                && !tries.contains(&place)
            {
                tries.push(place);
            }
        };
        for path in term_link::alternatives(raw) {
            if let Some((dir, _)) = &here {
                add(term_link::resolve_at(
                    &base.root,
                    dir,
                    home.as_deref(),
                    path,
                ));
            }
            if matches!(here, Some((_, Place::Outside(_)))) {
                continue;
            }
            let subs = match base.sub.is_empty() {
                true => vec![""],
                false => vec![base.sub.as_str(), ""],
            };
            for sub in subs {
                add(term_link::resolve(&base.root, sub, path));
            }
        }
        (!tries.is_empty()).then_some((base, channel, tries))
    }

    /// 记下问到的结果（`key` 见 [`ask_key`]）。
    fn remember(&mut self, raw: &str, dir: Option<&str>, key: &str, found: &Option<(Place, bool)>) {
        log::info!(
            "终端里的路径 {raw}（提示符里的目录 {}）：{}",
            dir.unwrap_or("没有"),
            match found {
                Some((Place::Repo(path), _)) => format!("仓库里的 {path}"),
                Some((Place::Outside(path), _)) => format!("仓库外的 {path}"),
                None => "不在".to_string(),
            }
        );
        self.links.checked.insert(
            key.to_string(),
            Checked {
                found: found.clone(),
                at: Instant::now(),
            },
        );
    }

    /// 问一下路径 `raw` 在不在，问到了重新看鼠标底下；单击时还在问的，问到了打开它；右键菜单等着的，
    /// 问到了重画菜单。`dir` 是提示符里的目录，`key` 是记结果用的名字。问不了时返回 false。
    fn check_path(
        &mut self,
        raw: String,
        dir: Option<String>,
        key: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((base, channel, tries)) = self.places(&raw, dir.as_deref(), cx) else {
            return false;
        };
        self.links.asking.insert(key.clone());
        cx.spawn(async move |this, cx| {
            let mut found = None;
            for place in tries {
                let dir = match &place {
                    Place::Repo(path) => match &channel {
                        Some(channel) => channel
                            .fs_stat(&base.root, path)
                            .await
                            .ok()
                            .filter(|stat| stat.exists)
                            .map(|stat| stat.dir),
                        None => None,
                    },
                    Place::Outside(path) if base.local => {
                        let path = path.clone();
                        cx.background_executor()
                            .spawn(async move { std::fs::metadata(&path).ok().map(|m| m.is_dir()) })
                            .await
                    }
                    Place::Outside(_) => None,
                };
                if let Some(dir) = dir {
                    found = Some((place, dir));
                    break;
                }
            }
            let (clicked, menu) = this
                .update(cx, |this, cx| {
                    this.remember(&raw, dir.as_deref(), &key, &found);
                    this.links.asking.remove(&key);
                    if let Some(position) = this.links.mouse_at {
                        let ctrl = this.links.ctrl;
                        this.hover_link(position, ctrl, cx);
                    }
                    let menu = this.menu_answered(&key, &found, cx);
                    let clicked = this
                        .links
                        .pending
                        .take_if(|pending| pending.key == key)
                        .and_then(|pending| {
                            Some((opening(found.clone()?, pending.at), pending.window))
                        });
                    (clicked, menu)
                })
                .unwrap_or_default();
            if let Some((open, window)) = clicked {
                cx.update_window(window, |_, window, cx| {
                    this.update(cx, |this, cx| this.open_target(open, window, cx))
                })
                .ok();
            }
            if let Some((menu, window)) = menu {
                let view = this.clone();
                cx.update_window(window, |_, window, cx| {
                    menu.update(cx, |menu, cx| {
                        menu.rebuild(window, cx, |menu, window, cx| {
                            fill_menu(&view, menu, window, cx)
                        })
                    })
                })
                .ok();
            }
        })
        .detach();
        true
    }

    /// 本机的仓库：当场看路径 `raw` 在不在（右键菜单要马上列出来），结果记下。看不了（不是本机的
    /// 仓库、没有可问的写法）时是 None。
    fn check_now(&mut self, raw: &str, dir: Option<&str>, key: &str, cx: &App) -> Option<()> {
        let (base, _, tries) = self.places(raw, dir, cx)?;
        if !base.local {
            return None;
        }
        let found = tries.into_iter().find_map(|place| {
            let dir = match &place {
                Place::Repo(path) => fr_core::files::stat(&base.root, path)
                    .ok()
                    .filter(|stat| stat.exists)
                    .map(|stat| stat.dir),
                Place::Outside(path) => std::fs::metadata(path).ok().map(|meta| meta.is_dir()),
            };
            Some((place, dir?))
        });
        self.remember(raw, dir, key, &found);
        Some(())
    }

    /// 问到了 `key`：右键菜单正等着它的话换成问到的，返回要重画的菜单。
    fn menu_answered(
        &mut self,
        key: &str,
        found: &Option<(Place, bool)>,
        cx: &mut Context<Self>,
    ) -> Option<(Entity<PopupMenu>, AnyWindowHandle)> {
        let menu = self.links.menu.as_mut()?;
        let MenuState::Asking(asked, at) = &menu.state else {
            return None;
        };
        if asked != key {
            return None;
        }
        menu.state = match found {
            Some(found) => MenuState::Ready(opening(found.clone(), *at)),
            None => MenuState::Missing,
        };
        cx.notify();
        let (view, window) = self.links.menu_view.clone()?;
        Some((view.upgrade()?, window))
    }

    /// 鼠标在终端里按下：按着 Ctrl 单击的是链接就打开，返回 true（不再选字、不报给程序）。
    pub(super) fn click_link(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // 又按了一下：前面那一下还在等的不开了；停着的提示收起来
        self.links.pending = None;
        self.hide_link_tip(cx);
        if !event.modifiers.control || event.click_count > 1 {
            return false;
        }
        self.hover_link(event.position, true, cx);
        if let Some(hover) = self.links.hover.clone() {
            self.open_target(hover.open, window, cx);
            return true;
        }
        // 那里的路径还在问：这一下先接住，问到了再开（见 `check_path`）
        let Some((key, at)) = self.links.waiting.clone() else {
            return false;
        };
        log::info!(
            "按住 Ctrl 单击时路径 {} 还在问：问到了再开",
            key.replace('\n', " ")
        );
        self.links.pending = Some(Pending {
            key,
            at,
            window: window.window_handle(),
        });
        true
    }

    fn open_target(&mut self, open: Open, window: &mut Window, cx: &mut Context<Self>) {
        log::info!("按住 Ctrl 单击了终端里的链接");
        match open {
            Open::Url(url) => crate::shell::report(crate::shell::open_link(&url), window, cx),
            Open::Repo { path, at, dir } => cx.emit(TerminalEvent::OpenFile { path, at, dir }),
            Open::Outside { path, dir: true } => {
                crate::shell::report(crate::shell::open_folder(&path), window, cx)
            }
            Open::Outside { path, dir: false } => crate::shell::open_file(&path, window, cx),
        }
    }

    /// 右键点在 `position`：那里是认得出的网址、路径就记下来，返回 true，外面那一层的右键菜单列它的
    /// 几项（见 [`fill_menu`]）。路径还没问过的：本机的当场看，主机上的先列「正在查找…」，问到了重画
    /// 菜单。问过了、不在的路径不算。
    pub(super) fn menu_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        self.links.menu = None;
        self.links.menu_view = None;
        self.hide_link_tip(cx);
        let Some((start, end, target, dir)) = self.target_at(position) else {
            return false;
        };
        let (shown, state) = match target {
            Target::Url(url) => (url.clone(), MenuState::Ready(Open::Url(url))),
            Target::Path { path, line, col } => {
                let at = line.map(|line| (line, col.unwrap_or(1)));
                let shown = match line {
                    Some(line) => format!("{path}:{line}"),
                    None => path.clone(),
                };
                let key = ask_key(dir.as_deref(), &path);
                let fresh = self
                    .links
                    .checked
                    .get(&key)
                    .is_some_and(|checked| checked.at.elapsed() < CHECKED_FOR);
                // 没问过的：本机的当场看；主机上的去问，菜单先列「正在查找…」
                if !fresh
                    && !self.links.asking.contains(&key)
                    && self.check_now(&path, dir.as_deref(), &key, cx).is_none()
                    && !self.check_path(path.clone(), dir.clone(), key.clone(), cx)
                {
                    return false;
                }
                let state = match self.links.checked.get(&key) {
                    _ if self.links.asking.contains(&key) => MenuState::Asking(key, at),
                    Some(Checked {
                        found: Some(found), ..
                    }) => MenuState::Ready(opening(found.clone(), at)),
                    _ => return false,
                };
                (shown, state)
            }
        };
        log::info!("右键点在终端里的链接上：{}", middle_cut(&shown, SHOWN_MAX));
        self.links.menu = Some(LinkMenu {
            start,
            end,
            shown,
            state,
        });
        cx.notify();
        true
    }

    /// 链接菜单里点了一项。
    fn menu_action(&mut self, action: MenuAction, window: &mut Window, cx: &mut Context<Self>) {
        let base = self.links.base.clone();
        match action {
            MenuAction::Files { path, at, dir } => {
                cx.emit(TerminalEvent::OpenFile { path, at, dir })
            }
            MenuAction::Browser(path) => {
                if let Some(base) = &base {
                    let entry = RepoEntry {
                        server: base.server.clone(),
                        path: base.root.clone(),
                    };
                    let channel = (base.channel)(cx);
                    open_with::open_in_browser(&entry, channel, &path, window, cx);
                }
            }
            MenuAction::Default(path) => match &base {
                Some(base) if base.local => {
                    let full = open_with::full_path(&base.root, false, &path);
                    crate::shell::open_file(&full, window, cx);
                }
                Some(base) => match (base.channel)(cx) {
                    Some(channel) => open_with::open_copy(channel, &base.root, &path, window, cx),
                    None => {
                        let text = tr("主机未连接，无法读取此文件。");
                        crate::shell::report(Err(text.into()), window, cx);
                    }
                },
                None => {}
            },
            MenuAction::Url(url) => crate::shell::report(crate::shell::open_link(&url), window, cx),
            MenuAction::OutsideFile(path) => crate::shell::open_file(&path, window, cx),
            MenuAction::OutsideDir(path) => {
                crate::shell::report(crate::shell::open_folder(&path), window, cx)
            }
            MenuAction::Reveal(path) => {
                crate::shell::report(crate::shell::reveal_in_explorer(&path), window, cx)
            }
            MenuAction::CopyText(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            MenuAction::Copy => self.copy(cx),
            MenuAction::Paste => self.paste(cx),
        }
    }

    /// 停着的链接下面那个提示：「Ctrl+单击在文件页中打开 · 右键更多」，放在鼠标那一行下面。
    pub(super) fn link_tip(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let hover = self.links.hover.as_ref().filter(|hover| hover.soft)?;
        let mouse = self.links.mouse_at?;
        let what = match &hover.open {
            Open::Url(_) => tr("Ctrl+单击在浏览器中打开"),
            Open::Repo { dir: true, .. } => tr("Ctrl+单击在文件页中显示"),
            Open::Repo { dir: false, .. } => tr("Ctrl+单击在文件页中打开"),
            Open::Outside { dir: true, .. } if cfg!(windows) => tr("Ctrl+单击在资源管理器中打开"),
            Open::Outside { dir: true, .. } => tr("Ctrl+单击在文件管理器中打开"),
            Open::Outside { dir: false, .. } => tr("Ctrl+单击用默认程序打开"),
        };
        // 程序要了鼠标（opencode）：右键归程序，按着 Shift 才是 Kuai 的
        let more = match self.mouse_mode(false) {
            Some(_) => tr("Shift+右键更多"),
            None => tr("右键更多"),
        };
        let text: SharedString = format!("{what} · {more}").into();
        let tip = match &self.links.tip {
            Some((shown, tip)) if *shown == text => tip.clone(),
            _ => {
                let tip = Tooltip::new(text.clone()).build(window, cx);
                self.links.tip = Some((text, tip.clone()));
                tip
            }
        };
        let geometry = self.geometry.get();
        let cell = geometry.cell.height.max(px(1.));
        let row = ((mouse.y - geometry.origin.y) / cell).floor();
        // 界面库的提示框四周带 12px 的外边距（`m_3`），框本身要落在这一行下面 2px、鼠标左边 10px
        const MARGIN: Pixels = px(12.);
        let below = geometry.origin.y + cell * (row + 1.) + px(2.) - MARGIN;
        Some(
            deferred(
                anchored()
                    .position(point(mouse.x - px(10.) - MARGIN, below))
                    .snap_to_window_with_margin(px(8.))
                    .child(tip),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}

/// 外面那一层的右键菜单打开时：记下这个菜单（路径问到了重画它），关了的时候忘掉右键点的链接；
/// 再按 [`fill_menu`] 列出各项。
pub(super) fn open_menu(
    view: &WeakEntity<TerminalView>,
    menu: PopupMenu,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let opened = (cx.entity().downgrade(), window.window_handle());
    let closing = view.clone();
    cx.subscribe_self(move |_, _: &DismissEvent, cx| {
        closing
            .update(cx, |this, cx| {
                this.links.menu = None;
                this.links.menu_view = None;
                cx.notify();
            })
            .ok();
    })
    .detach();
    view.update(cx, |this, _| this.links.menu_view = Some(opened))
        .ok();
    fill_menu(view, menu, window, cx)
}

/// 终端的右键菜单：右键点在网址、路径上时，顶上一行是认出来的写法，下面是它能做的事（目录：在文件页
/// 中显示、在资源管理器中显示、复制路径；文件：在文件页中打开、在浏览器中打开（网页、SVG）、用默认程序
/// 打开、复制路径；网址：在浏览器中打开、复制链接）；最后都有复制、粘贴。别处（设置里选了菜单时）只有
/// 复制、粘贴。
pub(super) fn fill_menu(
    view: &WeakEntity<TerminalView>,
    menu: PopupMenu,
    _: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let Some(this) = view.upgrade() else {
        return menu;
    };
    let (link, base, has_selection, alive) = {
        let this = this.read(cx);
        (
            this.links
                .menu
                .as_ref()
                .map(|menu| (menu.shown.clone(), menu.state.clone())),
            this.links.base.clone(),
            this.selected_text().is_some(),
            this.is_alive(),
        )
    };
    let muted = cx.theme().muted_foreground;
    let act = |label: &'static str, action: MenuAction| {
        let view = view.clone();
        PopupMenuItem::new(label).on_click(move |_, window, cx| {
            view.update(cx, |this, cx| this.menu_action(action.clone(), window, cx))
                .ok();
        })
    };
    // 第一项（和按住 Ctrl 单击一样）：右边写着「Ctrl+单击」
    let first = |label: &'static str, action: MenuAction| {
        let view = view.clone();
        PopupMenuItem::element(move |_, _| {
            h_flex().w_full().gap(px(24.)).child(label).child(
                div()
                    .ml_auto()
                    .text_xs()
                    .text_color(muted)
                    .child(tr("Ctrl+单击")),
            )
        })
        .on_click(move |_, window, cx| {
            view.update(cx, |this, cx| this.menu_action(action.clone(), window, cx))
                .ok();
        })
    };
    let mut menu = menu;
    if let Some((shown, state)) = link {
        menu = menu.label(middle_cut(&shown, SHOWN_MAX)).separator();
        let local = base.as_ref().is_some_and(|base| base.local);
        let full = |path: &str| match &base {
            Some(base) => open_with::full_path(&base.root, !base.local, path),
            None => path.to_string(),
        };
        menu = match state {
            MenuState::Asking(..) => menu.item(PopupMenuItem::new(tr("正在查找…")).disabled(true)),
            MenuState::Missing => {
                menu.item(PopupMenuItem::new(tr("找不到这个路径")).disabled(true))
            }
            MenuState::Ready(Open::Url(url)) => menu
                .item(first(tr("在浏览器中打开"), MenuAction::Url(url.clone())))
                .item(act(tr("复制链接"), MenuAction::CopyText(url))),
            MenuState::Ready(Open::Repo {
                path,
                at,
                dir: true,
            }) => {
                let shown = MenuAction::Files {
                    path: path.clone(),
                    at,
                    dir: true,
                };
                let mut menu = menu.item(first(tr("在文件页中显示"), shown));
                if local {
                    let reveal = MenuAction::Reveal(full(&path));
                    menu = menu.item(act(crate::shell::reveal_label(), reveal));
                }
                menu.separator()
                    .item(act(tr("复制路径"), MenuAction::CopyText(full(&path))))
                    .item(act(tr("复制相对路径"), MenuAction::CopyText(path)))
            }
            MenuState::Ready(Open::Repo {
                path,
                at,
                dir: false,
            }) => {
                let opened = MenuAction::Files {
                    path: path.clone(),
                    at,
                    dir: false,
                };
                let mut menu = menu.item(first(tr("在文件页中打开"), opened));
                if open_with::is_web(&path) {
                    let browser = MenuAction::Browser(path.clone());
                    menu = menu.item(act(tr("在浏览器中打开"), browser));
                }
                menu.item(act(tr("用默认程序打开"), MenuAction::Default(path.clone())))
                    .separator()
                    .item(act(tr("复制路径"), MenuAction::CopyText(full(&path))))
                    .item(act(tr("复制相对路径"), MenuAction::CopyText(path)))
            }
            MenuState::Ready(Open::Outside { path, dir: true }) => menu
                .item(first(
                    crate::shell::open_folder_label(),
                    MenuAction::OutsideDir(path.clone()),
                ))
                .separator()
                .item(act(tr("复制路径"), MenuAction::CopyText(path))),
            MenuState::Ready(Open::Outside { path, dir: false }) => menu
                .item(first(
                    tr("用默认程序打开"),
                    MenuAction::OutsideFile(path.clone()),
                ))
                .item(act(
                    crate::shell::reveal_label(),
                    MenuAction::Reveal(path.clone()),
                ))
                .separator()
                .item(act(tr("复制路径"), MenuAction::CopyText(path))),
        };
        menu = menu.separator();
    }
    menu.item(act(tr("复制"), MenuAction::Copy).disabled(!has_selection))
        .item(act(tr("粘贴"), MenuAction::Paste).disabled(!alive))
}

/// 太长的写法把中间换成「…」，留下开头和结尾（文件名在结尾）。
fn middle_cut(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let head = max / 3;
    let tail = max - head - 1;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(count - tail).collect();
    format!("{start}…{end}")
}

// 问到的路径（在哪、是不是目录）、要跳到的行列：单击时打开什么。
fn opening((place, dir): (Place, bool), at: Option<(u32, u32)>) -> Open {
    match place {
        Place::Repo(path) => Open::Repo { path, at, dir },
        Place::Outside(path) => Open::Outside { path, dir },
    }
}

#[cfg(test)]
mod tests {
    use super::{GridColumn, GridLine, GridPoint, Target, Term, find, prompt_dir_above};
    use crate::terminal::{Grid, Proxy};
    use alacritty_terminal::vte::ansi::Processor;

    fn term(cols: usize, rows: usize, output: &[u8]) -> Term<Proxy> {
        let (proxy, _rx) = Proxy::new();
        let mut term = Term::new(Default::default(), &Grid { cols, rows }, proxy);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, output);
        term
    }

    fn at(line: i32, col: usize) -> GridPoint {
        GridPoint::new(GridLine(line), GridColumn(col))
    }

    #[test]
    fn 折到下一行的路径整个认() {
        // 20 列：`see crates/fr-core/s` | `rc/lib.rs:12 ok`
        let term = term(20, 5, b"see crates/fr-core/src/lib.rs:12 ok");
        let (start, end, target) = find(&term, at(1, 3)).expect("link");
        assert_eq!((start, end), (at(0, 4), at(1, 11)));
        assert_eq!(
            target,
            Target::Path {
                path: "crates/fr-core/src/lib.rs".into(),
                line: Some(12),
                col: None,
            }
        );
        // 空白上没有
        assert!(find(&term, at(0, 3)).is_none());
    }

    #[test]
    fn 往上找提示符里的目录() {
        // 3 行高：前三行进了历史
        let ps = term(
            40,
            3,
            b"PS C:\\repo> ls\r\nmain.rs\r\nPS C:\\repo> cd src\\deep\r\n\
              PS C:\\repo\\src\\deep> cat util.rs\r\nutil.rs:2 fn x\r\nok",
        );
        let dir = |line, col| prompt_dir_above(&ps, at(line, col));
        assert_eq!(dir(1, 0).as_deref(), Some("C:\\repo\\src\\deep"));
        // 提示符那一行自己
        assert_eq!(dir(0, 25).as_deref(), Some("C:\\repo\\src\\deep"));
        // `cd` 之前的输出按当时的目录
        assert_eq!(dir(-2, 0).as_deref(), Some("C:\\repo"));

        // 折成两行的提示符
        let wrapped = term(20, 4, b"PS C:\\a\\bbbbbbbbbbbbbbbbbb> x\r\nf.rs");
        assert_eq!(
            prompt_dir_above(&wrapped, at(2, 0)).as_deref(),
            Some("C:\\a\\bbbbbbbbbbbbbbbbbb")
        );

        // bash
        let bash = term(
            40,
            4,
            b"root@dev:/data/repo# cd src\r\nroot@dev:/data/repo/src# cat a\r\nx.rs",
        );
        assert_eq!(
            prompt_dir_above(&bash, at(2, 0)).as_deref(),
            Some("/data/repo/src")
        );

        // 上面没有提示符
        let plain = term(20, 3, b"a\r\nb@c d\r\ne");
        assert_eq!(prompt_dir_above(&plain, at(2, 0)), None);
    }

    #[test]
    fn 括号里的绝对路径被程序折成几行() {
        let term = term(
            60,
            6,
            b"see label (/C:/repo/crates/fr-app/src/\r\n  terminal/\r\n  links.rs:826:5) after",
        );
        for point in [at(0, 11), at(1, 3), at(2, 10)] {
            let (start, end, target) = find(&term, point).expect("link");
            assert_eq!((start, end), (at(0, 11), at(2, 15)));
            assert_eq!(
                target,
                Target::Path {
                    path: "/C:/repo/crates/fr-app/src/terminal/links.rs".into(),
                    line: Some(826),
                    col: Some(5),
                }
            );
        }
        assert!(find(&term, at(1, 1)).is_none());
    }

    #[test]
    fn 分开输出的路径各自识别() {
        let term = term(60, 6, b"see (/C:/repo/src/)\r\n  links.rs:826) after");
        let (start, end, target) = find(&term, at(1, 5)).expect("link");
        assert_eq!((start, end), (at(1, 2), at(1, 13)));
        assert_eq!(
            target,
            Target::Path {
                path: "links.rs".into(),
                line: Some(826),
                col: None,
            }
        );
    }

    #[test]
    fn 程序写的超链接() {
        let term = term(
            30,
            3,
            b"x \x1b]8;;https://example.com/a\x1b\\link\x1b]8;;\x1b\\ y",
        );
        let (start, end, target) = find(&term, at(0, 3)).expect("link");
        assert_eq!((start, end), (at(0, 2), at(0, 5)));
        assert_eq!(target, Target::Url("https://example.com/a".into()));
    }
}
