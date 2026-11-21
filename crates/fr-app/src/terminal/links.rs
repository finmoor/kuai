//! 终端里的链接：按住 Ctrl，鼠标底下的网址、文件路径画上下划线、鼠标变成手形，单击打开（和
//! Windows 终端、VS Code 的终端一样）。程序要了鼠标的（opencode、Codex）也是按住 Ctrl 单击。
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
use std::rc::Rc;
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column as GridColumn, Line as GridLine, Point as GridPoint};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;
use fr_channel::Channel;
use fr_core::term_link::{self, Place, Target};
use fr_core::term_text;
use gpui_kit::*;

use super::{Proxy, TerminalEvent, TerminalView};

/// 问过的路径在不在，记这么久
const CHECKED_FOR: Duration = Duration::from_secs(5);
/// 找提示符时往上最多看这么多行
const PROMPT_SCAN: i32 = 5000;

/// 认链接里的路径用的：终端开在哪个仓库、哪个目录，怎么去问文件在不在。
#[derive(Clone)]
pub struct LinkBase {
    /// 仓库根目录（终端里的程序看到的写法：本机是 `C:\…`，服务器上是那边的路径）
    pub root: String,
    /// 终端开在仓库里的哪个子目录（相对仓库根，`/` 分隔；空的是仓库根）
    pub sub: String,
    /// 仓库在这台电脑上：仓库外的文件也能打开
    pub local: bool,
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
    open: Open,
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
    /// 按住 Ctrl 时鼠标底下的链接：画下划线，单击打开
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

    /// 鼠标挪到 `position`、按着 Ctrl（`ctrl`）没有：重新看底下的链接。没按 Ctrl、正在选字、按着
    /// 报给程序的鼠标键时没有链接。
    pub(super) fn hover_link(
        &mut self,
        position: Point<Pixels>,
        ctrl: bool,
        cx: &mut Context<Self>,
    ) {
        self.links.mouse_at = Some(position);
        self.links.ctrl = ctrl;
        self.links.waiting = None;
        let found = match ctrl && !self.selecting && self.mouse_held.is_none() {
            true => self.link_under(position, cx),
            false => None,
        };
        if found != self.links.hover {
            self.links.hover = found;
            cx.notify();
        }
    }

    /// 鼠标离开了终端：不再有链接。
    pub(super) fn leave_link(&mut self, cx: &mut Context<Self>) {
        self.links.mouse_at = None;
        self.links.waiting = None;
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

    fn link_under(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> Option<Hover> {
        let (point, _) = self.grid_point(position)?;
        let (start, end, target, dir) = {
            let session = self.session.as_ref()?;
            let term = session.term.lock();
            let (start, end, target) = find(&term, point)?;
            let dir = match target {
                Target::Path { .. } => prompt_dir_above(&term, start),
                Target::Url(_) => None,
            };
            (start, end, target, dir)
        };
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
        Some(Hover { start, end, open })
    }

    /// 问一下路径 `raw` 在不在，问到了重新看鼠标底下；单击时还在问的，问到了打开它。`dir` 是提示符
    /// 里写的目录（见 [`prompt_dir_above`]），`key` 是记结果用的名字。不知道开在哪个仓库、没有可问的
    /// 写法时返回 false。
    ///
    /// 几种写法（git 改动里的 `a/`、`b/` 去掉）挨个按这些目录试：提示符里的目录；它在仓库里或者没有时，
    /// 再试终端开在的目录、仓库根。提示符里的目录在仓库外时只按它算：说的是那里的文件，仓库里同名的
    /// 不是它。
    fn check_path(
        &mut self,
        raw: String,
        dir: Option<String>,
        key: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(base) = self.links.base.clone() else {
            return false;
        };
        let channel = (base.channel)(cx);
        // 服务器上的 `~`：那台的主目录
        let home = match &channel {
            Some(Channel::Remote(conn)) => Some(conn.hello.home.clone()),
            _ => None,
        };
        let here = dir.as_deref().and_then(|dir| {
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
        for path in term_link::alternatives(&raw) {
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
        if tries.is_empty() {
            return false;
        }
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
            let clicked = this
                .update(cx, |this, cx| {
                    log::info!(
                        "终端里的路径 {raw}（提示符里的目录 {}）：{}",
                        dir.as_deref().unwrap_or("没有"),
                        match &found {
                            Some((Place::Repo(path), _)) => format!("仓库里的 {path}"),
                            Some((Place::Outside(path), _)) => format!("仓库外的 {path}"),
                            None => "不在".to_string(),
                        }
                    );
                    this.links.asking.remove(&key);
                    this.links.checked.insert(
                        key.clone(),
                        Checked {
                            found: found.clone(),
                            at: Instant::now(),
                        },
                    );
                    if let Some(position) = this.links.mouse_at {
                        let ctrl = this.links.ctrl;
                        this.hover_link(position, ctrl, cx);
                    }
                    let pending = this.links.pending.take_if(|pending| pending.key == key)?;
                    Some((opening(found?, pending.at), pending.window))
                })
                .ok()
                .flatten();
            if let Some((open, window)) = clicked {
                cx.update_window(window, |_, window, cx| {
                    this.update(cx, |this, cx| this.open_target(open, window, cx))
                })
                .ok();
            }
        })
        .detach();
        true
    }

    /// 鼠标在终端里按下：按着 Ctrl 单击的是链接就打开，返回 true（不再选字、不报给程序）。
    pub(super) fn click_link(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // 又按了一下：前面那一下还在等的不开了
        self.links.pending = None;
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
}

/// 问到的路径（在哪、是不是目录）、要跳到的行列：单击时打开什么。
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
