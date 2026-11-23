//! 内置终端：仓库页底下的一块，在仓库目录里开一个 shell。
//!
//! 终端仿真交给 `alacritty_terminal`：解析转义序列、维护字符格子。程序在哪跑：
//!
//! - 开在常驻的 kuaid 里（[`Session::attach`]）：服务器上的仓库是服务器的 kuaid，输出经仓库通道
//!   那条 ssh 连接送过来；本机仓库是这台电脑自己的 kuaid（1.11 起）。我们只管把输出喂给仿真器。
//!   关掉标签页、退出、断线都只是断开，里面的程序接着跑；再打开时接上原来的，kuaid 把屏幕还原出来。
//! - 本机的 kuaid 用不了时的退路（[`Session::spawn`]）：这个库在 Windows 上用 ConPTY 开伪终端，
//!   它的后台线程读程序的输出。程序活在 Kuai 进程里：关掉标签页时视图交给仓库页留着
//!   （见 `repo/terminals.rs`），退出 Kuai 才结束。
//!
//! 这里做三件事：
//!
//! - 画：每帧把格子拍下来（[`snapshot`]），背景一块块填，字按样式连成一段段画。等宽靠
//!   「每个字形固定前进一格」（中文等宽字符是两格），不靠字体自己的宽度，回退字体也对得齐。
//! - 输入：功能键和 Ctrl、Alt 组合在 `keys` 里编码；普通的字和输入法上屏的字由系统经
//!   [`EntityInputHandler`] 送进来。
//! - 生命周期：面板第一次画出来、知道能放几行几列时才启动进程；进程结束后画面留着，
//!   按回车重新启动；这个视图丢弃时，开在 Kuai 进程里的跟着结束，开在 kuaid 里的只是断开。
//!
//! 一个仓库可以开几个终端（仓库页的面板上一个标签一个），各是各的视图。每个终端
//! 在 kuaid 里有自己的会话，名字见 [`session_key`]。
//!
//! 往终端里发图片、文件（粘贴、拖进来、点按钮选）见 [`attach`]：发过去的是文件的路径。
//!
//! 字体、字号、行高、光标、往回翻多少行、选中即复制、右键做什么都可以在设置里改（见 [`prefs`]），
//! 所有终端共用，改了马上生效。
//!
//! 滚动条、往回翻时固定住的输入区、左边的消息刻度见 [`scroll`]。

mod attach;
mod colors;
mod glyphs;
mod keys;
mod links;
mod prefs;
mod preview;
mod scroll;

pub use attach::Attachment;
pub use links::LinkBase;
pub use prefs::{font_choices, set_prefs, set_prefs_writer};
pub use preview::preview;

use std::any::Any;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{
    Column as GridColumn, Line as GridLine, Point as GridPoint, Side as GridSide,
};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::tty::{self, Options, Shell};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor};
use fr_channel::{Connection, TermItem, TermMark, TermStream, UploadItem};
use fr_core::i18n::tr;
use fr_core::tf;
use futures::channel::mpsc;
use futures::future::LocalBoxFuture;
use futures::{FutureExt as _, StreamExt as _};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::win;
use colors::Palette;
use keys::WheelTarget;

/// 字和面板边缘之间留的空
const PADDING: f32 = 8.;
/// 按终端自己的行列数画（见 [`TerminalView::watch`]）时字最小缩到这么大，再小看不清；还放不下就只
/// 显示左上角那一块
const MIN_WATCH_FONT: f32 = 8.;

/// 服务器上终端会话的名字里，仓库路径和序号之间的分隔符（路径里不会有的一个控制字符）
const KEY_SEPARATOR: char = '\u{1f}';

/// 请 kuaid 结束一个终端的请求（已经发出去了，等的是它的回答）。
pub type Closing = futures::future::BoxFuture<'static, anyhow::Result<()>>;

/// 显示终端的标题：Windows 上 shell 刚启动时标题是它自己的完整路径
/// （`C:\Program Files\PowerShell\7\pwsh.exe`），只留程序名（`pwsh`，不带 `.exe`），别的照原样。
/// 仓库的终端页、终端页、停下的提醒都这样写。
pub fn short_title(title: &str) -> SharedString {
    let is_program_path = title.to_lowercase().ends_with(".exe") && title.contains(['\\', '/']);
    match title.rsplit(['\\', '/']).next() {
        // 结尾的 .exe 是 ASCII，按字节截不会截到半个字
        Some(name) if is_program_path => name[..name.len() - 4].to_string().into(),
        _ => title.to_string().into(),
    }
}

/// 仓库的第 `number` 个终端在 kuaid 里的会话名字。第 1 个就是仓库路径（1.5 只有一个终端时
/// 用的就是它，升上来以后接得上），之后的在后面加分隔符和序号。
pub fn session_key(repo: &str, number: u32) -> String {
    match number {
        0 | 1 => repo.to_string(),
        n => format!("{repo}{KEY_SEPARATOR}{n}"),
    }
}

/// 反过来：这个会话是不是这个仓库的终端，是第几个。
///
/// Windows 上的 kuaid 把名字统一成正斜杠、小写（同一个目录有几种写法，见
/// [`fr_core::dir::windows_key`]），所以带盘符的路径按那种写法比。
pub fn session_number(repo: &str, key: &str) -> Option<u32> {
    let windows = repo.as_bytes().get(1) == Some(&b':');
    let normalized = windows.then(|| fr_core::dir::windows_key(repo));
    let rest = normalized
        .as_deref()
        .and_then(|repo| key.strip_prefix(repo))
        .or_else(|| key.strip_prefix(repo))?;
    if rest.is_empty() {
        return Some(1);
    }
    let number: u32 = rest.strip_prefix(KEY_SEPARATOR)?.parse().ok()?;
    (number >= 2).then_some(number)
}

/// 不知道是哪个仓库的终端时，把会话的名字拆成路径和序号（终端总览页列别处的终端用）。
pub fn split_session_key(key: &str) -> (&str, u32) {
    match key.rsplit_once(KEY_SEPARATOR) {
        Some((path, number)) => match number.parse() {
            Ok(number) if number >= 2 => (path, number),
            _ => (key, 1),
        },
        None => (key, 1),
    }
}

/// 开在 Kuai 进程里的本机程序：这台电脑上的 kuaid 用不了时才用。
pub struct LocalLaunch {
    pub program: String,
    pub args: Vec<String>,
    /// 启动时所在的目录；空着用本程序的
    pub cwd: Option<PathBuf>,
    /// 额外的环境变量
    pub env: Vec<(String, String)>,
    /// 进程活着的时候要一直留着的东西
    pub keep: Option<Box<dyn Any>>,
}

/// 终端里要跑什么：开在 kuaid 里的终端，服务器上的或者这台电脑上的。
pub struct Launch {
    /// 等这台服务器（或者这台电脑上的 kuaid）的连接
    pub connect: LocalBoxFuture<'static, Result<Arc<Connection>, SharedString>>,
    /// 终端会话的名字：kuaid 里有同名的就接上，不另开。见 [`session_key`]
    pub key: String,
    /// 在哪个目录里开 shell
    pub cwd: String,
    /// 开哪个 shell（程序和参数）；空着由 kuaid 定。只有 Windows 上的 kuaid 认
    pub shell: Vec<String>,
    /// 这是这台电脑上的终端：kuaid 用不了（找不到、被安全软件拦下）时改为开在 Kuai 进程里
    pub fallback: Option<LocalLaunch>,
}

/// 送回界面线程的事。
enum Feed {
    /// 仿真器的事件（有新内容、标题变了、程序问颜色……）
    Term(TermEvent),
    /// 服务器上的终端接上了：会话的编号
    Opened(String),
    /// 服务器上的终端里的程序结束了：退出码
    Exited(Option<i32>),
    /// 和服务器的连接断了（终端里的程序还在跑）
    Lost(String),
    /// 输出处理不过来、积压太多，流断了（见 [`TermItem::Lagging`]，终端里的程序还在跑）
    Lagging,
    /// kuaid 记着的发出去的消息（全部，从早到晚）
    Marks(Vec<TermMark>),
}

/// 把仿真器的事件送回界面线程。「有新内容」只在上一个还没处理时不送：输出来得再快，排着的也
/// 只有一个，界面线程不会被一长串一样的唤醒占住（画面本来就是一帧画一次，画的是那时的内容）。
#[derive(Clone)]
struct Proxy {
    tx: mpsc::UnboundedSender<Feed>,
    /// 有一个「有新内容」送出去了、界面还没处理（见 [`Session::deliver`]）
    woken: Arc<AtomicBool>,
}

impl Proxy {
    fn new() -> (Proxy, mpsc::UnboundedReceiver<Feed>) {
        let (tx, rx) = mpsc::unbounded();
        let woken = Arc::new(AtomicBool::new(false));
        (Proxy { tx, woken }, rx)
    }

    /// 送一件事给界面。界面那头没了返回假。
    fn send(&self, feed: Feed) -> bool {
        if matches!(feed, Feed::Term(TermEvent::Wakeup)) && self.woken.swap(true, Ordering::AcqRel)
        {
            return !self.tx.is_closed();
        }
        self.tx.unbounded_send(feed).is_ok()
    }
}

impl EventListener for Proxy {
    fn send_event(&self, event: TermEvent) {
        self.send(Feed::Term(event));
    }
}

/// 几行几列。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Grid {
    cols: usize,
    rows: usize,
}

impl Dimensions for Grid {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

fn window_size(grid: Grid, cell: Size<Pixels>) -> WindowSize {
    WindowSize {
        num_lines: grid.rows as u16,
        num_cols: grid.cols as u16,
        cell_width: f32::from(cell.width).round() as u16,
        cell_height: f32::from(cell.height).round() as u16,
    }
}

/// 上一帧画在哪、每格多大：鼠标的位置靠它换成第几行第几列。
#[derive(Clone, Copy, Default)]
struct Geometry {
    origin: Point<Pixels>,
    cell: Size<Pixels>,
    grid: Option<Grid>,
    /// 光标那一格在窗口里的位置（输入法的候选框跟着它）
    cursor: Option<Bounds<Pixels>>,
}

/// 一个开着的终端。丢弃时本机的进程结束，服务器上的只是断开。
struct Session {
    term: Arc<FairMutex<Term<Proxy>>>,
    grid: Grid,
    cell: Size<Pixels>,
    link: Link,
    _events: Task<()>,
}

/// 程序在哪跑、怎么把输入和大小交给它。
enum Link {
    Local {
        sender: EventLoopSender,
        _keep: Option<Box<dyn Any>>,
    },
    Remote {
        conn: Arc<Connection>,
        /// 连的是这台电脑上的 kuaid：文件不用传，路径就是本机的
        local: bool,
        /// kuaid 给这个会话的编号；刚发出请求、还没回来时是空的
        term: RefCell<Option<String>>,
        /// 收输出、喂给仿真器的任务；丢弃就从会话上断开
        _pump: Task<()>,
    },
}

impl Session {
    fn spawn(
        launch: LocalLaunch,
        grid: Grid,
        cell: Size<Pixels>,
        cx: &mut Context<TerminalView>,
    ) -> std::io::Result<Self> {
        let (proxy, rx) = Proxy::new();
        let woken = proxy.woken.clone();
        let term = Term::new(prefs::config(&prefs::get(cx)), &grid, proxy.clone());
        let term = Arc::new(FairMutex::new(term));
        let mut env: HashMap<String, String> = launch.env.into_iter().collect();
        env.entry("TERM".into())
            .or_insert_with(|| "xterm-256color".into());
        env.entry("COLORTERM".into())
            .or_insert_with(|| "truecolor".into());
        let options = Options {
            // 命令行是直接拼的，程序路径里有空格要自己加引号；参数由库按 C 运行时的规则转义
            shell: Some(Shell::new(quoted(&launch.program), launch.args)),
            working_directory: launch.cwd,
            drain_on_exit: true,
            env,
            #[cfg(target_os = "windows")]
            escape_args: true,
        };
        let pty = tty::new(&options, window_size(grid, cell), 0)?;
        let event_loop = EventLoop::new(term.clone(), proxy, pty, true, false)?;
        let sender = event_loop.channel();
        event_loop.spawn();
        Ok(Self {
            term,
            grid,
            cell,
            link: Link::Local {
                sender,
                _keep: launch.keep,
            },
            _events: Self::deliver(rx, woken, cx),
        })
    }

    /// 接上开在 kuaid 里的终端：请 kuaid 接上（或者新开）名字是 `key` 的会话，它的输出喂给这里的
    /// 仿真器。给了 `id` 就只按编号接那一个，不在了也不新开。
    #[allow(clippy::too_many_arguments)]
    fn attach(
        conn: Arc<Connection>,
        key: &str,
        cwd: &str,
        shell: &[String],
        id: Option<&str>,
        grid: Grid,
        cell: Size<Pixels>,
        cx: &mut Context<TerminalView>,
    ) -> Self {
        let (cols, rows) = (grid.cols as u16, grid.rows as u16);
        let stream = match id {
            Some(id) => conn.term_attach(id, cols, rows),
            None => conn.term_open(key, cwd, shell, cols, rows),
        };
        Self::remote(conn, stream, id.map(str::to_string), grid, cell, cx)
    }

    /// 把 kuaid 里一个终端的输出流 `stream` 喂给这里新建的仿真器。`id` 是会话的编号（按名字开的
    /// 要等 kuaid 回话才知道）。
    fn remote(
        conn: Arc<Connection>,
        stream: TermStream,
        id: Option<String>,
        grid: Grid,
        cell: Size<Pixels>,
        cx: &mut Context<TerminalView>,
    ) -> Self {
        let (proxy, rx) = Proxy::new();
        let woken = proxy.woken.clone();
        let term = Term::new(prefs::config(&prefs::get(cx)), &grid, proxy.clone());
        let term = Arc::new(FairMutex::new(term));
        let executor = cx.background_executor().clone();
        // 解析输出放在后台：大段输出不卡界面
        let pump = cx
            .background_executor()
            .spawn(pump(stream, term.clone(), proxy, executor));
        Self {
            term,
            grid,
            cell,
            link: Link::Remote {
                local: conn.local,
                conn,
                term: RefCell::new(id),
                _pump: pump,
            },
            _events: Self::deliver(rx, woken, cx),
        }
    }

    /// 输出处理不过来、流断了（见 [`Feed::Lagging`]）：按编号重新接上同一个终端，换一个新的仿真器，
    /// 从 kuaid 给的现在的屏幕接着。`watch` 是只看的接法（见 [`Self::view`]）。
    fn resync(&mut self, watch: bool, cx: &mut Context<TerminalView>) {
        let Link::Remote { conn, term, .. } = &self.link else {
            return;
        };
        let Some(id) = term.borrow().clone() else {
            return;
        };
        let conn = conn.clone();
        let stream = match watch {
            true => conn.term_view(&id),
            false => conn.term_attach(&id, self.grid.cols as u16, self.grid.rows as u16),
        };
        *self = Self::remote(conn, stream, Some(id), self.grid, self.cell, cx);
    }

    /// 接上 kuaid 里编号是 `id` 的终端，用「只看」的接法（`term_attach` 带 `watch`）：不改它的大小，
    /// 这里的仿真器按它现在的行列数 `grid` 建。敲的字照样写得进去（按编号写，和怎么接的无关）。
    fn view(
        conn: Arc<Connection>,
        id: &str,
        grid: Grid,
        cell: Size<Pixels>,
        cx: &mut Context<TerminalView>,
    ) -> Self {
        let stream = conn.term_view(id);
        Self::remote(conn, stream, Some(id.to_string()), grid, cell, cx)
    }

    /// 把后台送来的事交给界面线程上的视图。攒着的一次交过去（一次更新）。
    fn deliver(
        mut rx: mpsc::UnboundedReceiver<Feed>,
        woken: Arc<AtomicBool>,
        cx: &mut Context<TerminalView>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            while let Some(feed) = rx.next().await {
                let mut feeds = vec![feed];
                while let Ok(feed) = rx.try_recv() {
                    feeds.push(feed);
                }
                // 先放下「排着」再处理：处理的时候又来了新内容的，会再送一个
                woken.store(false, Ordering::Release);
                let delivered = this.update(cx, |this, cx| {
                    for feed in feeds {
                        this.on_feed(feed, cx);
                    }
                    // 程序要不要焦点报告，可能这批输出里才知道（刚接上时还原屏幕的那几段）
                    this.tell_focus();
                });
                if delivered.is_err() {
                    break;
                }
            }
        })
    }

    /// 写给程序。
    fn write(&self, bytes: Cow<'static, [u8]>) {
        match &self.link {
            Link::Local { sender, .. } => {
                let _ = sender.send(Msg::Input(bytes));
            }
            // 会话的编号还没回来的那一瞬间敲的字不要了
            Link::Remote { conn, term, .. } => {
                if let Some(term) = term.borrow().as_deref() {
                    conn.term_input(term, bytes.into_owned());
                }
            }
        }
    }

    fn resize(&mut self, grid: Grid, cell: Size<Pixels>) {
        self.grid = grid;
        self.cell = cell;
        self.term.lock().resize(grid);
        match &self.link {
            Link::Local { sender, .. } => {
                let _ = sender.send(Msg::Resize(window_size(grid, cell)));
            }
            Link::Remote { conn, term, .. } => {
                if let Some(term) = term.borrow().as_deref() {
                    conn.term_resize(term, grid.cols as u16, grid.rows as u16);
                }
            }
        }
    }

    /// 开在别的电脑（服务器）上。
    fn is_remote(&self) -> bool {
        matches!(self.link, Link::Remote { local: false, .. })
    }

    /// 开在 Kuai 进程里：Kuai 退出它就结束。
    fn in_process(&self) -> bool {
        matches!(self.link, Link::Local { .. })
    }

    /// 里面的 shell 是 Windows 上的（路径加引号的规则不一样）。
    fn on_windows(&self) -> bool {
        match &self.link {
            Link::Local { .. } => true,
            Link::Remote { conn, local, .. } => *local || conn.hello.os == "windows",
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // 本机：读输出的线程收到后退出，伪终端跟着关掉，里面的进程结束。
        // 服务器上的：收输出的任务跟着丢弃，kuaid 那边只是少了一个接着的客户端
        if let Link::Local { sender, .. } = &self.link {
            let _ = sender.send(Msg::Shutdown);
        }
    }
}

/// 接上已有的会话时，开头的输出是还原屏幕用的，分几段（见 `TermOpened::restore`）：一段收齐了才
/// 一起交给仿真器，中间的样子不画出来（远程的线路上整份要传一会儿，照着一块块画，看到的是从最早的
/// 记录一路滚下来）。
#[derive(Default)]
struct Restore {
    /// 还没收齐的各段多长，最前面的是正在收的
    parts: std::collections::VecDeque<usize>,
    /// 正在收的那一段已经到了的
    buf: Vec<u8>,
    /// 什么时候接上的（日志里写每段收齐时过了多久）
    since: Option<Instant>,
}

impl Restore {
    /// 接上了：后面还原屏幕的输出分这几段（空的是新开的会话，或者旧的 kuaid 不说）。
    fn start(&mut self, parts: Vec<u64>) {
        self.parts = parts
            .into_iter()
            .map(|len| len as usize)
            .filter(|&len| len > 0)
            .collect();
        self.buf.clear();
        self.since = Some(Instant::now());
        if !self.parts.is_empty() {
            // 只在 FR_LOG=1 时打出来：慢线路上量还原要多久用
            log::info!("接上终端：还原屏幕的输出分 {:?} 字节", self.parts);
        }
    }

    /// 收到一块输出：凑齐了的段和还原以后的实时输出按顺序交给 `feed`。交过东西返回真（要重画）。
    fn take(&mut self, mut bytes: &[u8], mut feed: impl FnMut(&[u8])) -> bool {
        let mut fed = false;
        while let Some(&want) = self.parts.front() {
            let take = (want - self.buf.len()).min(bytes.len());
            self.buf.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buf.len() < want {
                return fed;
            }
            self.parts.pop_front();
            let ms = self.since.map_or(0, |at| at.elapsed().as_millis());
            log::info!("还原屏幕的一段收齐了：{want} 字节，接上后 {ms} 毫秒");
            feed(&std::mem::take(&mut self.buf));
            fed = true;
        }
        if !bytes.is_empty() {
            feed(bytes);
            fed = true;
        }
        fed
    }
}

/// 收服务器上终端的输出，喂给仿真器。连接断了、程序结束了就停。
async fn pump(
    mut stream: TermStream,
    term: Arc<FairMutex<Term<Proxy>>>,
    proxy: Proxy,
    executor: BackgroundExecutor,
) {
    let mut parser: Processor = Processor::new();
    let mut restore = Restore::default();
    let send = |feed: Feed| proxy.send(feed);
    loop {
        // 程序开了「同步刷新」（攒着一起画）又迟迟不结束：到时间就画出来
        let item = match parser.sync_timeout().sync_timeout() {
            Some(deadline) => {
                let wait = deadline.saturating_duration_since(Instant::now());
                futures::select_biased! {
                    item = stream.next().fuse() => item,
                    _ = executor.timer(wait).fuse() => {
                        parser.stop_sync(&mut *term.lock());
                        if !send(Feed::Term(TermEvent::Wakeup)) {
                            return;
                        }
                        continue;
                    }
                }
            }
            None => stream.next().await,
        };
        let alive = match item {
            Some(Ok(TermItem::Opened {
                term: id,
                restore: parts,
                ..
            })) => {
                restore.start(parts);
                send(Feed::Opened(id))
            }
            Some(Ok(TermItem::Marks(marks))) => send(Feed::Marks(marks)),
            Some(Ok(TermItem::Output(bytes))) => {
                let fed = restore.take(&bytes, |part| parser.advance(&mut *term.lock(), part));
                !fed || send(Feed::Term(TermEvent::Wakeup))
            }
            Some(Ok(TermItem::Exited(code))) => {
                send(Feed::Exited(code));
                false
            }
            Some(Ok(TermItem::Lagging)) => {
                send(Feed::Lagging);
                false
            }
            Some(Err(err)) => {
                send(Feed::Lost(format!("{err:#}")));
                false
            }
            None => {
                send(Feed::Lost(String::new()));
                false
            }
        };
        if !alive {
            return;
        }
    }
}

fn quoted(program: &str) -> String {
    if program.contains([' ', '\t']) && !program.starts_with('"') {
        format!("\"{program}\"")
    } else {
        program.to_string()
    }
}

/// 启动终端时调用，算出要跑什么；失败时返回给用户看的原因。每次（重新）启动都算一遍。
type Launcher = Box<dyn Fn(&mut App) -> Result<Launch, String>>;

/// 正在把文件发给终端里的程序（服务器上的终端要先把文件传上去）。
struct Sending {
    /// 正在传的那个文件
    name: SharedString,
    /// 第几个、一共几个
    index: usize,
    count: usize,
    /// 服务器已经收到多少、一共多少（字节）
    sent: u64,
    total: u64,
    /// 丢弃就是不传了
    _task: Task<()>,
}

pub struct TerminalView {
    /// 按终端自己的行列数画的（见 [`Self::watch`]）没有：它不会自己开、重新开
    launcher: Option<Launcher>,
    /// 接的是别处开着的终端，不改它的大小：它是几行几列就按几行几列画
    watch: Option<Grid>,
    session: Option<Session>,
    /// 正在等服务器的连接
    connecting: Option<Task<()>>,
    /// 最近一次画的时候面板能放几行几列（连接好的那一刻按它开）
    fitted: Option<(Grid, Size<Pixels>)>,
    /// 启动不了的原因
    error: Option<SharedString>,
    /// 进程结束了（里面是退出码），画面留着
    exited: Option<Option<i32>>,
    /// 和服务器的连接断了，画面留着；终端里的程序还在服务器上跑
    lost: bool,
    /// 启动时连不上那台电脑（`error` 里写着）：它连上以后自己再开（[`Self::host_back`]）
    unreachable: bool,
    /// 这是 kuaid 里留着的终端（上次没结束），还没接上：也算有东西在跑
    restored: bool,
    /// 第一次接上时按这个编号接（列表里看到的那个会话，见 [`Self::attach_to`]），不按名字开
    attach: Option<String>,
    /// 这是这台电脑上的终端（开在本机的 kuaid 里，或者退回到 Kuai 进程里）：提示里不说「服务器」
    local: bool,
    /// 在本机的 kuaid 里开过（或者本来就是留在那里的）：之后 kuaid 连不上时不能改开在 Kuai 进程里，
    /// 那是另一个新的 shell，原来的还在 kuaid 里跑着
    hosted: bool,
    /// 正在往里发文件
    sending: Option<Sending>,
    /// 要提示一下的话（发不了的原因之类）；再敲键盘就消失
    flash: Option<SharedString>,
    focus: FocusHandle,
    geometry: Rc<Cell<Geometry>>,
    /// 输入法正在组的字，还没上屏
    marked: Option<String>,
    /// 正按着左键拖选
    selecting: bool,
    /// 报给了程序、还按着的鼠标键（左 0、中 1、右 2）：拖动、松开接着报给它（见 [`Self::press_mouse`]）
    mouse_held: Option<u8>,
    /// 上一次报给程序的鼠标在第几列、第几行：在同一格里挪不重复报
    mouse_cell: Option<(usize, usize)>,
    /// 在要了鼠标的程序里按着左键：拖到别的格了没有（拖了就是在程序里选字，见 `program_selected`）
    program_drag: Option<bool>,
    /// 刚在要了鼠标的程序里拖选过，之后没按键、没点：松开时剪贴板里的字（见 `copy_in_program`）
    program_selected: Option<Option<String>>,
    /// 滚轮滚了不到一行的零头
    scroll_rest: f32,
    /// 程序自己设的标题（OSC 0、2）
    title: Option<SharedString>,
    /// 光标该不该闪（设置里开了，或者终端里的程序要它闪；焦点得在这里）：每帧画的时候记下来
    blinking: Rc<Cell<bool>>,
    /// 光标正闪到「灭」的那半拍
    blink_hidden: bool,
    /// 上一次敲键盘的时刻：正打着字的时候光标不闪
    typed_at: Instant,
    /// 焦点在这里（`focus_changed` 里记下：收到输出时没有窗口可问）
    focused: bool,
    /// 焦点进来以后告诉过程序了（`CSI I`，见 [`Self::tell_focus`]）
    focus_told: bool,
    /// 「只看」接着的终端，这边要把大小要过来（见 [`Self::wants_size`]）：往里打了字、点了画面、
    /// 焦点由人放进来、这边的面板改了大小。下一帧看过就放下
    size_wanted: bool,
    /// 下一次焦点进来不算要大小：终端页跟着别处改的大小换上新建的画面时（见 [`Self::follow_size`]）
    follow: bool,
    /// 上一帧这块面板按设置里的字号放得下几行几列
    roomy: Option<Grid>,
    /// 发出去的消息，从早到晚（kuaid 推来的；开在 Kuai 进程里的终端自己记，见 [`scroll`]）
    marks: Vec<TermMark>,
    /// 滚动条、固定的输入区、消息刻度的状态
    scroll: scroll::ScrollUi,
    /// 按住 Ctrl 指着的链接、认路径要的东西（见 [`links`]）
    links: links::LinkUi,
    _blink: Task<()>,
    _subscriptions: Vec<Subscription>,
}

/// 终端要外面那一层做的事。
pub enum TerminalEvent {
    /// 按住 Ctrl 单击了仓库里的文件、目录：到这个仓库的文件页打开它。`path` 相对仓库根、`/`
    /// 分隔；`at` 是行号、列号（从 1 起）
    OpenFile {
        path: String,
        at: Option<(u32, u32)>,
        dir: bool,
    },
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TerminalView {
    pub fn new(
        launcher: impl Fn(&mut App) -> Result<Launch, String> + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(Some(Box::new(launcher)), window, cx)
    }

    /// 接上 `conn` 那台电脑上编号是 `term` 的终端，它现在是 `cols` 列 `rows` 行（终端总览页右边的
    /// 画面）。和 [`Self::new`] 的区别：
    ///
    /// - 用「只看」的接法，不改那个终端的大小：按它自己的行列数画，地方不够就把字缩小。它改了大小
    ///   要重新建一个（见 [`Self::size`]）。只看一眼不改大小，是因为一改里面的程序就要重画（Claude
    ///   Code 会把整段对话重画一遍）。焦点进来、开始往里输入以后例外：大小改成这边画面放得下的（见
    ///   [`Self::claim`]），不然窗口变大以后四周一直空着。
    /// - 键盘、输入法、粘贴、鼠标、发文件和平常一样，都写得进去。
    /// - 仿真器要回答程序的话（问光标位置、颜色、终端大小）不发：那些由正经接着它的那一头或者
    ///   kuaid 回答，这里再答就重了。
    /// - 程序结束、连接断了只说一声，不能在这里重新开、重新接。
    pub fn watch(
        conn: &Arc<Connection>,
        term: &str,
        cols: u16,
        rows: u16,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let grid = Grid {
            cols: usize::from(cols).max(2),
            rows: usize::from(rows).max(1),
        };
        let mut this = Self::build(None, window, cx);
        this.watch = Some(grid);
        this.local = conn.local;
        // 每格多大画的时候才知道，这里只有回答程序「终端多大」时用得上，而那个不回答
        let cell = size(px(0.), px(0.));
        this.session = Some(Session::view(conn.clone(), term, grid, cell, cx));
        this
    }

    /// [`Self::watch`] 建的这个画面按几列几行画：建的时候那个终端的大小，往里输入以后是这边要过来的
    /// 大小（见 [`Self::claim`]）。
    pub fn size(&self) -> Option<(u16, u16)> {
        self.watch.map(|grid| (grid.cols as u16, grid.rows as u16))
    }

    /// 「只看」接着的终端要不要把大小要过来：焦点在这里（正往里输入）、里面的程序还在、这块面板按
    /// 设置里的字号放得下的行列数 `roomy` 和它现在的不一样，而且是这边有了动静（[`Self::size_wanted`]，
    /// 或者窗口在前台时这块面板改了大小）。每帧画之前问一次。
    ///
    /// 只看焦点在不在不行：两台电脑的终端页焦点都停在同一个终端里时（一台的窗口在后面没人用也算），
    /// 一边要过去，另一边的终端页跟着换上新建的画面，焦点在就又要回来，画面一直在两个大小之间跳。
    fn wants_size(&mut self, roomy: Grid, window: &Window) -> bool {
        let resized = self.roomy.replace(roomy).is_some_and(|old| old != roomy);
        let wanted =
            std::mem::take(&mut self.size_wanted) || (resized && window.is_window_active());
        wanted
            && self.watch.is_some_and(|grid| grid != roomy)
            && self.is_alive()
            && self.focus.is_focused(window)
    }

    /// 终端页跟着别处改的大小换上了这个新建的画面，焦点随后挪进来：这一下不算这边要大小，往里打字、
    /// 点画面时再要（见 [`Self::wants_size`]）。
    pub fn follow_size(&mut self) {
        self.follow = true;
    }

    /// 把「只看」接着的终端改成 `grid` 这么大：这里的仿真器跟着改，再告诉 kuaid，里面的程序按新的
    /// 大小重画。之后还是只看的接法（不回答程序的话），只是大小由这边定了：终端列表报上来的大小和
    /// 它一样，终端页不会再重建这个画面。别处的画面（仓库的终端页、别的电脑上的 Kuai）焦点进去时再
    /// 把大小要回去（见 [`Self::focus_changed`]），谁在输入就按谁的大小。
    fn claim(&mut self, grid: Grid, cx: &mut Context<Self>) {
        let Some(session) = &mut self.session else {
            return;
        };
        // 只在 FR_LOG=1 时打出来：数一数改了几次，看有没有来回改
        log::info!(
            "终端的大小要过来：{}×{}（原来 {}×{}）",
            grid.cols,
            grid.rows,
            session.grid.cols,
            session.grid.rows
        );
        let cell = session.cell;
        session.resize(grid, cell);
        self.watch = Some(grid);
        cx.notify();
    }

    fn build(launcher: Option<Launcher>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let subscriptions = vec![
            cx.on_focus(&focus, window, |this, _, cx| this.focus_changed(true, cx)),
            cx.on_blur(&focus, window, |this, _, cx| this.focus_changed(false, cx)),
            // 终端的设置变了：往回翻的行数、光标的样子交给仿真器，字体、字号重画时就用上了
            cx.observe_global::<prefs::Prefs>(|this, cx| this.prefs_changed(cx)),
        ];
        // 光标闪烁的节拍。不该闪的时候只是空转，不重画
        let blink = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK_INTERVAL).await;
                if this.update(cx, |this, cx| this.blink_tick(cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            launcher,
            watch: None,
            session: None,
            connecting: None,
            fitted: None,
            error: None,
            exited: None,
            lost: false,
            unreachable: false,
            restored: false,
            attach: None,
            local: false,
            hosted: false,
            sending: None,
            flash: None,
            focus,
            geometry: Rc::default(),
            marked: None,
            selecting: false,
            mouse_held: None,
            mouse_cell: None,
            program_drag: None,
            program_selected: None,
            scroll_rest: 0.,
            title: None,
            blinking: Rc::default(),
            blink_hidden: false,
            typed_at: Instant::now(),
            focused: false,
            focus_told: false,
            size_wanted: false,
            follow: false,
            roomy: None,
            marks: Vec::new(),
            scroll: scroll::ScrollUi::default(),
            links: links::LinkUi::default(),
            _blink: blink,
            _subscriptions: subscriptions,
        }
    }

    fn prefs_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session
                .term
                .lock()
                .set_options(prefs::config(&prefs::get(cx)));
        }
        cx.notify();
    }

    /// 每半拍来一次：该闪就把光标亮灭换一下。
    fn blink_tick(&mut self, cx: &mut Context<Self>) {
        let blink = self.blinking.get() && self.typed_at.elapsed() >= BLINK_INTERVAL;
        let hidden = blink && !self.blink_hidden;
        if hidden != self.blink_hidden {
            self.blink_hidden = hidden;
            cx.notify();
        }
    }

    /// 调字号：`step` 是加几号，0 是回到默认。存进设置，所有终端一起变。
    fn zoom(&mut self, step: f32, cx: &mut Context<Self>) {
        use crate::settings::TerminalSettings;
        let mut prefs = prefs::get(cx);
        let size = if step == 0. {
            TerminalSettings::DEFAULT_SIZE
        } else {
            (prefs.font_size + step).clamp(TerminalSettings::MIN_SIZE, TerminalSettings::MAX_SIZE)
        };
        self.flash = Some(tf!("字号 {size}", size = size as u32).into());
        if size != prefs.font_size {
            prefs.font_size = size;
            prefs::write(prefs, cx);
        }
        cx.notify();
    }

    /// 所在的那台电脑（重新）连上了：断开时停下（程序还在 kuaid 里跑）、启动时连不上的，自己接上、
    /// 开起来，和按 Enter 一样（看不见的等画出来时再开）。只看的不管，它的画面由终端页换。
    pub fn host_back(&mut self, cx: &mut Context<Self>) {
        let stopped = (self.lost && self.exited.is_none()) || self.unreachable;
        if stopped && self.connecting.is_none() && self.watch.is_none() && self.launcher.is_some() {
            self.reopen(cx);
        }
    }

    /// 里面的进程还在跑，而且连着（敲的字它收得到）。
    pub fn is_alive(&self) -> bool {
        self.session.is_some() && self.exited.is_none() && !self.lost
    }

    /// 有东西在跑：本机的进程，或者服务器上的终端（连接断了它也在跑；上次留下、
    /// 这次还没接上的也算）。
    pub fn is_running(&self) -> bool {
        match &self.session {
            Some(_) => self.exited.is_none(),
            None => self.restored && self.error.is_none() && self.exited.is_none(),
        }
    }

    /// 这是 kuaid 里留着的终端，里面的程序还在跑：没接上之前 [`Self::is_running`] 也为真。
    pub fn mark_restored(&mut self) {
        self.restored = true;
        self.hosted = true;
    }

    /// 第一次接上时按 kuaid 给的编号接（`term_attach`），不按名字开。接回列表里看到的终端用：
    /// 它刚在别处结束、列表还没跟上的那一会儿，按名字开就是新开一个 shell；按编号接，不在了就
    /// 接不上（显示已断开），列表跟上以后那一行自己去掉。之后断开重连、重启还是按名字。
    pub fn attach_to(&mut self, term: String) {
        self.attach = Some(term);
    }

    /// 里面的程序开在 Kuai 进程里：关掉标签页要另外保管，退出 Kuai 它就结束。
    pub fn in_process(&self) -> bool {
        self.session.as_ref().is_some_and(Session::in_process)
    }

    /// 程序自己设的标题（见 [`short_title`]）。
    pub fn title(&self) -> Option<SharedString> {
        self.title.as_deref().map(short_title)
    }

    /// kuaid 给这个终端的编号：开在 Kuai 进程里的、还没接上的是 None。
    pub fn term_id(&self) -> Option<String> {
        match &self.session.as_ref()?.link {
            Link::Remote { term, .. } => term.borrow().clone(),
            Link::Local { .. } => None,
        }
    }

    /// 还没接上、等着按编号接的那个会话（见 [`Self::attach_to`]）。
    pub fn pending(&self) -> Option<String> {
        self.attach.clone()
    }

    /// 接着的那台电脑的 kuaid 的连接：开在 Kuai 进程里的、还没接上的是 None。
    pub fn conn(&self) -> Option<Arc<Connection>> {
        match &self.session.as_ref()?.link {
            Link::Remote { conn, .. } => Some(conn.clone()),
            Link::Local { .. } => None,
        }
    }

    /// 别处把这个终端重启了（同一个名字，kuaid 里换了一个会话 `term`）：丢掉手上这个已经结束的，
    /// 下一帧按编号接上新的。按名字开的话，新的那个要是也已经没了，就成了自己新开一个 shell。
    pub fn reattach(&mut self, term: String, cx: &mut Context<Self>) {
        if self.launcher.is_some() {
            self.attach = Some(term);
            self.reopen(cx);
        }
    }

    /// 结束里面的进程，重新开一个（下一帧知道大小了再启动）。kuaid 里的会话要等它关掉了再开，
    /// 不然按名字开接上的还是它；`closing` 是调用的另外去关的（还没接上的终端 [`Self::kill`]
    /// 关不了，见仓库终端页的 `close_unattached`）。关不掉就不开，说一声，按 Enter 再试。
    pub fn restart(&mut self, closing: Option<Closing>, cx: &mut Context<Self>) {
        let closing = self.kill().or(closing);
        self.reopen(cx);
        let Some(closing) = closing else {
            return;
        };
        self.connecting = Some(cx.spawn(async move |this, cx| {
            let closed = closing.await;
            this.update(cx, |this, cx| {
                this.connecting = None;
                if let Err(err) = closed {
                    let err = format!("{err:#}");
                    this.error = Some(tf!("终端重启失败：{err}", err).into());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// 结束里面的进程。开在 Kuai 进程里的丢掉会话就结束了；开在 kuaid 里的要告诉它，不然接着跑：
    /// 返回这个请求，结果由调用的去等（失败时界面上要说一声）。还没接上的不再按编号接。
    pub fn kill(&mut self) -> Option<Closing> {
        let closing = match &self.session {
            Some(Session {
                link: Link::Remote { conn, term, .. },
                ..
            }) => term.borrow().as_deref().map(|term| conn.term_close(term)),
            _ => None,
        };
        self.session = None;
        self.attach = None;
        self.restored = false;
        self.hosted = false;
        self.sending = None;
        closing
    }

    /// 丢掉现在的会话，下一帧重新开：开在 Kuai 进程里的是新的进程；kuaid 里有同名的会话就接上它
    /// （连接断了以后重新接上用的就是这个）。
    fn reopen(&mut self, cx: &mut Context<Self>) {
        self.session = None;
        self.connecting = None;
        self.error = None;
        self.exited = None;
        self.lost = false;
        self.unreachable = false;
        self.title = None;
        self.marked = None;
        self.sending = None;
        self.flash = None;
        cx.notify();
    }

    fn mode(&self) -> TermMode {
        self.session
            .as_ref()
            .map_or(TermMode::NONE, |session| *session.term.lock().mode())
    }

    /// 面板画出来时调用：还没启动就按这个大小启动，大小变了就告诉程序。
    fn fit(&mut self, grid: Grid, cell: Size<Pixels>, cx: &mut Context<Self>) {
        self.fitted = Some((grid, cell));
        match &mut self.session {
            None if self.error.is_none() && self.connecting.is_none() => {
                self.start(grid, cell, cx);
            }
            Some(session) if session.grid != grid => session.resize(grid, cell),
            _ => {}
        }
    }

    fn start(&mut self, grid: Grid, cell: Size<Pixels>, cx: &mut Context<Self>) {
        let Some(launcher) = &self.launcher else {
            return;
        };
        self.exited = None;
        self.lost = false;
        // 换了一个会话：接上 kuaid 里留着的会话时它再把记着的消息发过来
        self.marks.clear();
        self.scroll.reset();
        let launch = match launcher(cx) {
            Ok(launch) => launch,
            Err(err) => {
                self.error = Some(tf!("终端启动失败：{err}", err).into());
                return;
            }
        };
        // 先等连接（多半已经连着，马上就好）
        let Launch {
            connect,
            key,
            cwd,
            shell,
            fallback,
        } = launch;
        self.local = fallback.is_some();
        self.connecting = Some(cx.spawn(async move |this, cx| {
            let connected = connect.await;
            this.update(cx, |this, cx| {
                this.connecting = None;
                // 等的这一会儿面板的大小可能变了，用现在的
                let (grid, cell) = this.fitted.unwrap_or((grid, cell));
                match (connected, fallback) {
                    (Ok(conn), _) => {
                        this.hosted |= conn.local;
                        let attach = this.attach.take();
                        let session = Session::attach(
                            conn,
                            &key,
                            &cwd,
                            &shell,
                            attach.as_deref(),
                            grid,
                            cell,
                            cx,
                        );
                        this.session = Some(session);
                    }
                    // 本机的 kuaid 用不了：这个终端还没在它里面开过，就开在 Kuai 进程里
                    (Err(err), Some(fallback)) if !this.hosted => {
                        log::warn!("本机的 kuaid 用不了，终端开在 Kuai 进程里：{err}");
                        match Session::spawn(fallback, grid, cell, cx) {
                            Ok(session) => this.session = Some(session),
                            Err(err) => {
                                this.error = Some(tf!("终端启动失败：{err}", err).into());
                            }
                        }
                    }
                    (Err(err), Some(_)) => {
                        this.error = Some(tf!("无法连接 kuaid：{err}", err).into());
                        this.unreachable = true;
                    }
                    (Err(err), None) => {
                        this.error = Some(tf!("无法连接主机：{err}", err).into());
                        this.unreachable = true;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn on_feed(&mut self, feed: Feed, cx: &mut Context<Self>) {
        match feed {
            Feed::Term(event) => return self.on_event(event, cx),
            // 只看的不改那个终端的大小，编号也早就知道
            Feed::Opened(_) if self.watch.is_some() => {}
            Feed::Opened(id) => {
                if let Some(Session {
                    link: Link::Remote { conn, term, .. },
                    grid,
                    ..
                }) = &self.session
                {
                    // 请求发出去以后面板的大小可能又变了，那时还不知道编号，没法说
                    conn.term_resize(&id, grid.cols as u16, grid.rows as u16);
                    *term.borrow_mut() = Some(id);
                }
            }
            Feed::Exited(code) => self.exited = Some(code),
            Feed::Marks(marks) => {
                self.marks = marks;
                self.scroll.hovered = None;
            }
            Feed::Lost(reason) => {
                if !reason.is_empty() {
                    log::warn!("终端和 kuaid 断开了：{reason}");
                }
                self.lost = true;
            }
            // 画面换成新接上的；正在发的文件、输入法正在组的字不动（还是同一个终端）
            Feed::Lagging => {
                log::warn!("终端输出积压过多，重新接上");
                if let Some(session) = &mut self.session {
                    session.resync(self.watch.is_some(), cx);
                }
                self.title = None;
                self.selecting = false;
                self.scroll_rest = 0.;
            }
        }
        cx.notify();
    }

    fn on_event(&mut self, event: TermEvent, cx: &mut Context<Self>) {
        match event {
            // 只看的不回答程序的话（见 [`Self::watch`]）
            TermEvent::PtyWrite(_)
            | TermEvent::ColorRequest(..)
            | TermEvent::TextAreaSizeRequest(_)
                if self.watch.is_some() => {}
            TermEvent::PtyWrite(text) => self.write(text.into_bytes()),
            TermEvent::Title(title) => self.title = Some(title.into()),
            TermEvent::ResetTitle => self.title = None,
            TermEvent::ClipboardStore(_, text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            // 程序问某个颜色是什么（常用来判断终端是深色还是浅色）
            TermEvent::ColorRequest(index, format) => {
                let theme = cx.theme();
                let palette = Palette::new(theme.foreground, theme.background, theme.is_dark());
                let color = match index {
                    0..=255 => Color::Indexed(index as u8),
                    256 => Color::Named(NamedColor::Foreground),
                    257 => Color::Named(NamedColor::Background),
                    258 => Color::Named(NamedColor::Cursor),
                    _ => return,
                };
                if let Some(session) = &self.session {
                    let rgb = {
                        let term = session.term.lock();
                        colors::to_rgb(palette.resolve(color, term.colors()))
                    };
                    self.write(format(rgb).into_bytes());
                }
            }
            TermEvent::TextAreaSizeRequest(format) => {
                if let Some(session) = &self.session {
                    let size = window_size(session.grid, session.cell);
                    self.write(format(size).into_bytes());
                }
            }
            TermEvent::ChildExit(status) => self.exited = Some(status.code()),
            TermEvent::Exit => {
                self.exited.get_or_insert(None);
            }
            // 读剪贴板（OSC 52 的读）不给：服务器上的程序不该能拿到本机剪贴板里的东西
            TermEvent::ClipboardLoad(..) => {}
            // 往回翻着时来了新输出：回到底部的按钮上一个蓝点。只是输入区里的字变了（往回翻着打字的
            // 回显）不算，见 `scroll::outside_digest`、`settle_outside`
            TermEvent::Wakeup => self.watch_outside(cx),
            TermEvent::Bell | TermEvent::MouseCursorDirty | TermEvent::CursorBlinkingChange => {}
        }
        cx.notify();
    }

    /// 往程序的输入里写。
    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        if let (Some(session), true) = (&self.session, self.is_alive()) {
            session.write(bytes.into());
        }
    }

    /// 按了回车：开在 Kuai 进程里的终端自己从屏幕上读出这条消息记下（kuaid 里的由 kuaid 记，见
    /// 协议 16）。
    fn record_mark(&mut self) {
        let Some(session) = self.session.as_ref().filter(|session| session.in_process()) else {
            return;
        };
        let Some((text, continued)) = scroll::message(&session.term.lock()) else {
            return;
        };
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        fr_core::term_text::record(&mut self.marks, text, continued, at);
    }

    /// 用户敲进去的东西（粘贴、输入法）：先回到最底下、去掉选区，再写。
    fn input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        self.type_bytes(bytes, false, cx);
    }

    /// 写用户敲的东西。往回翻着、输入区固定住时画面不动（看得见输入区里打的字），`enter`（按的是
    /// 回车）才回到最底下；没固定住时和原来一样，一打字就回到最底下，不然看不见光标。
    fn type_bytes(&mut self, bytes: Vec<u8>, enter: bool, cx: &mut Context<Self>) {
        // 这边在往里输入：「只看」的画面把大小要过来（见 `wants_size`）
        self.size_wanted = true;
        // 按了键：程序里选的字多半没了（见 `copy_in_program`）
        self.program_selected = None;
        if let Some(session) = &self.session {
            let mut term = session.term.lock();
            if enter || self.scroll.fixed.get().len == 0 {
                term.scroll_display(Scroll::Bottom);
            } else {
                log::info!(
                    "往回翻着打字：画面不动（往回翻 {} 行）",
                    term.grid().display_offset()
                );
            }
            term.selection = None;
        }
        self.write(bytes);
        cx.notify();
    }

    fn focus_changed(&mut self, focused: bool, cx: &mut Context<Self>) {
        self.blink_hidden = false;
        self.focused = focused;
        self.focus_told = false;
        // 人把焦点放进来（点了、按了 →）：「只看」的画面把大小要过来；跟着别处的大小换上的不算
        let follow = std::mem::take(&mut self.follow);
        self.size_wanted = focused && !follow;
        if !focused && self.mode().contains(TermMode::FOCUS_IN_OUT) {
            self.write(&b"\x1b[O"[..]);
        }
        self.tell_focus();
        if !focused {
            self.marked = None;
        }
        // 焦点进来时把大小要回来：终端页、别的电脑上的 Kuai 往里输入时可能按它们的画面改过（见
        // `claim`）。只发给 kuaid，大小没变它不动；开在 Kuai 进程里的终端没有别人改大小
        if focused
            && self.watch.is_none()
            && let Some((grid, cell)) = self.fitted
            && let Some(session) = &mut self.session
            && !session.in_process()
        {
            session.resize(grid, cell);
        }
        cx.notify();
    }

    /// 焦点在这里、程序要焦点报告（`CSI ? 1004 h`）、还没告诉它焦点进来了：告诉它（`CSI I`）。
    /// Claude Code 这类程序焦点不在时不画光标，没告诉它的话要等敲了字才画出来。刚接上时还原屏幕的
    /// 输出还没到，不知道程序要不要，焦点先进来了的等输出到了再说（每批输出处理完都来一次）。
    fn tell_focus(&mut self) {
        if self.focused
            && !self.focus_told
            && self.is_alive()
            && self.mode().contains(TermMode::FOCUS_IN_OUT)
        {
            self.focus_told = true;
            self.write(&b"\x1b[I"[..]);
        }
    }

    /// 鼠标点了画面：焦点放到这里。焦点本来就在这里时再告诉程序一次焦点在这里：同一个终端的别的
    /// 画面（另一页、别的电脑上的 Kuai）焦点出去时说过「焦点出去了」，程序就当没人在看。
    fn click_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 点了画面就是这边要用：「只看」的画面把大小要过来（见 `wants_size`）。不靠焦点进来的事件：
        // 窗口不在前台时界面库不发它
        self.size_wanted = true;
        match self.focus.is_focused(window) {
            true => {
                self.focus_told = false;
                self.tell_focus();
            }
            false => self.focus.focus(window, cx),
        }
        cx.notify();
    }

    fn selected_text(&self) -> Option<String> {
        let session = self.session.as_ref()?;
        let text = session.term.lock().selection_to_string()?;
        (!text.is_empty()).then_some(text)
    }

    /// Ctrl+Shift+C 时 Kuai 自己没选字，但刚在要了鼠标的程序里拖选过（opencode 这类自己画选区的）：
    /// 当 Ctrl+C 转给程序，它用 Ctrl+C 复制自己选的字（Windows 终端没选字时也是把这个键交给程序）。
    /// 拖完以后按过键、点过，或者剪贴板已经变了（程序松开时自己复制了，比如 tmux、opencode 设成
    /// 选中即复制）不转：程序那边多半已经没有选区，Ctrl+C 会清空输入框、退出程序或者打断命令。
    /// 返回转了没有。
    fn copy_in_program(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(before) = self.program_selected.take() else {
            return false;
        };
        if self.selected_text().is_some() || self.mouse_mode(false).is_none() {
            return false;
        }
        if cx.read_from_clipboard().and_then(|item| item.text()) != before {
            log::info!("Ctrl+Shift+C：程序拖选后已经自己复制了，不再转 Ctrl+C");
            return false;
        }
        log::info!("Ctrl+Shift+C：Kuai 没选字，刚在程序里拖选过，当 Ctrl+C 转给程序");
        self.type_bytes(vec![0x03], false, cx);
        true
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        if let Some(session) = &self.session {
            session.term.lock().selection = None;
        }
        cx.notify();
    }

    /// 粘贴。剪贴板里有文字就贴文字；没有文字、是文件（资源管理器里复制的）或者图片（截图）
    /// 就发过去，见 [`Self::send`]。
    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        match attach::from_clipboard(item, false, chrono::Local::now()) {
            Some(attach::Pasted::Text(text)) => {
                let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
                self.input(keys::paste(&text, bracketed), cx);
            }
            Some(attach::Pasted::Send(files)) => self.send(files, cx),
            None => {}
        }
    }

    /// Alt+V：Claude Code 在 Windows 上用它读剪贴板里的图。它读的是它所在那台主机的剪贴板，终端开在
    /// 别的主机上时读不到这台电脑上截的图。所以这时剪贴板里有图片（或复制的文件）就由 Kuai 发过去，
    /// 和 Ctrl+V 一样传上去、贴路径（不看文字：截图工具有时连带放一段文字）；没有就照旧交给程序。
    /// 开在这台电脑上的终端不接：程序自己读得到。返回接了没有。
    fn paste_image(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.session.as_ref().is_some_and(Session::is_remote) {
            return false;
        }
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        match attach::from_clipboard(item, true, chrono::Local::now()) {
            Some(attach::Pasted::Send(files)) => {
                self.send(files, cx);
                true
            }
            _ => false,
        }
    }

    /// 让用户选文件，发给终端里的程序。
    pub fn pick_files(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.send(paths.into_iter().map(Attachment::File).collect(), cx);
            })
            .ok();
        })
        .detach();
    }

    /// 把图片、文件发给终端里的程序：发过去的是它们的路径（每个作为一次粘贴）。本机的终端直接给
    /// 本机的路径；服务器上的终端先把文件传到服务器，给服务器上的路径。
    pub fn send(&mut self, items: Vec<Attachment>, cx: &mut Context<Self>) {
        if items.is_empty() {
            return;
        }
        self.flash = None;
        let refuse = if !self.is_alive() {
            Some(tr("终端未运行，无法发送。"))
        } else if self.sending.is_some() {
            Some(tr("上一批文件尚未发送完成。"))
        } else {
            None
        };
        if let Some(why) = refuse {
            self.flash = Some(why.into());
            cx.notify();
            return;
        }
        let Some(session) = &self.session else {
            return;
        };
        let (name, count) = (SharedString::from(items[0].name()), items.len());
        let task = match &session.link {
            Link::Local { .. } | Link::Remote { local: true, .. } => {
                cx.spawn(async move |this, cx| {
                    // 图片要转格式、写文件，放到后台
                    let prepared = cx
                        .background_spawn(async move { attach::local_paths(items) })
                        .await;
                    this.update(cx, |this, cx| {
                        this.sending = None;
                        match prepared {
                            Ok(paths) => this.paste_paths(&paths, cx),
                            Err(err) => this.flash = Some(err.into()),
                        }
                        cx.notify();
                    })
                    .ok();
                })
            }
            Link::Remote { conn, .. } => {
                let conn = conn.clone();
                cx.spawn(async move |this, cx| {
                    // 图片转格式、看是不是文件夹放到后台
                    let prepared = cx
                        .background_spawn(async move { attach::uploads(items) })
                        .await;
                    let (uploads, folder) = match prepared {
                        Ok(prepared) => prepared,
                        Err(err) => {
                            this.update(cx, |this, cx| {
                                this.sending = None;
                                this.flash = Some(err.into());
                                cx.notify();
                            })
                            .ok();
                            return;
                        }
                    };
                    let mut paths = Vec::new();
                    let mut failed: Option<SharedString> = None;
                    for (index, (name, source)) in uploads.into_iter().enumerate() {
                        let shown = SharedString::from(name.clone());
                        let begun = this.update(cx, |this, cx| {
                            if let Some(sending) = &mut this.sending {
                                (sending.name, sending.index) = (shown, index);
                                (sending.sent, sending.total) = (0, 0);
                            }
                            cx.notify();
                        });
                        if begun.is_err() {
                            return;
                        }
                        let mut stream = conn.upload(&name, source);
                        let mut path = None;
                        while let Some(item) = stream.next().await {
                            match item {
                                Ok(UploadItem::Progress { sent, total }) => {
                                    let shown = this.update(cx, |this, cx| {
                                        if let Some(sending) = &mut this.sending {
                                            (sending.sent, sending.total) = (sent, total);
                                        }
                                        cx.notify();
                                    });
                                    if shown.is_err() {
                                        return;
                                    }
                                }
                                Ok(UploadItem::Done(remote)) => path = Some(remote),
                                Err(err) => {
                                    let err = format!("{err:#}");
                                    failed = Some(tf!("{name} 上传失败：{err}", name, err).into());
                                }
                            }
                        }
                        match path {
                            Some(path) => paths.push(path),
                            None => {
                                failed.get_or_insert_with(|| {
                                    tf!("{name} 上传失败：与主机的连接已断开", name).into()
                                });
                                break;
                            }
                        }
                    }
                    // 文件夹前面的都传上去了才说文件夹（中途有没传上去的，说的是那一个）
                    if failed.is_none()
                        && let Some(name) = folder
                    {
                        failed = Some(tf!("无法发送文件夹，请先压缩：{name}", name).into());
                    }
                    this.update(cx, |this, cx| {
                        this.sending = None;
                        // 传上去了的照样给程序，没传上去的说一声
                        this.paste_paths(&paths, cx);
                        this.flash = failed;
                        cx.notify();
                    })
                    .ok();
                })
            }
        };
        self.sending = Some(Sending {
            name,
            index: 0,
            count,
            sent: 0,
            total: 0,
            _task: task,
        });
        cx.notify();
    }

    /// 把几个路径贴给程序。
    fn paste_paths(&mut self, paths: &[String], cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        let windows = self.session.as_ref().is_some_and(Session::on_windows);
        self.input(attach::paste_paths(paths, bracketed, windows), cx);
    }

    /// 从外面拖文件进来。
    fn on_drop(&mut self, paths: &ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        let files = paths
            .paths()
            .iter()
            .cloned()
            .map(Attachment::File)
            .collect();
        self.send(files, cx);
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let mods = &keystroke.modifiers;
        let key = keystroke.key.as_str();
        // Alt+F4 留给系统关窗口
        if mods.alt && key == "f4" {
            return;
        }
        if self.flash.take().is_some() {
            cx.notify();
        }
        // 鼠标停着的链接提示：打字时收起来
        self.hide_link_tip(cx);
        // 正打着字的时候光标不闪
        self.typed_at = Instant::now();
        if std::mem::take(&mut self.blink_hidden) {
            cx.notify();
        }
        // Ctrl+加号、Ctrl+减号调字号，Ctrl+0 回到默认（和 Windows 终端一样）；不交给程序。
        // Ctrl+Shift+减号（Ctrl+_，有的程序用它撤销）照旧交给程序
        if mods.control && !mods.alt && !mods.platform && self.watch.is_none() {
            let step = match key {
                "=" | "+" => Some(1.),
                "-" if !mods.shift => Some(-1.),
                "0" if !mods.shift => Some(0.),
                _ => None,
            };
            if let Some(step) = step {
                self.zoom(step, cx);
                cx.stop_propagation();
                return;
            }
        }
        // 正在传文件时 Esc 是不传了（不交给程序）
        if key == "escape" && self.sending.take().is_some() {
            self.flash = Some(tr("已取消上传。").into());
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if !self.is_alive() {
            // 正在连的时候按回车不管；连接断了是重新接上，别的是重新开
            if key == "enter" && self.connecting.is_none() && self.watch.is_none() {
                self.reopen(cx);
                cx.stop_propagation();
            }
            return;
        }
        let only_ctrl = mods.control && !mods.alt && !mods.platform;
        // 复制：Ctrl+Shift+C；选着字的时候 Ctrl+C 也是复制（没选字时是中断程序）。Kuai 没选字、刚在
        // 要了鼠标的程序里拖选过时 Ctrl+Shift+C 交给程序复制（见 `copy_in_program`）
        if only_ctrl && key == "c" && (mods.shift || self.selected_text().is_some()) {
            if !(mods.shift && self.copy_in_program(cx)) {
                self.copy(cx);
            }
            cx.stop_propagation();
            return;
        }
        // 粘贴：Ctrl+V、Ctrl+Shift+V、Shift+Insert
        let shift_insert = key == "insert" && mods.shift && !mods.control && !mods.alt;
        if (only_ctrl && key == "v") || shift_insert {
            self.paste(cx);
            cx.stop_propagation();
            return;
        }
        // Alt+V：主机上的终端里贴这台电脑剪贴板里的图（见 `paste_image`）
        let only_alt = mods.alt && !mods.control && !mods.shift && !mods.platform;
        if only_alt && key == "v" && self.paste_image(cx) {
            cx.stop_propagation();
            return;
        }
        // Shift+翻页：翻看前面的输出
        if mods.shift && !mods.control && !mods.alt && matches!(key, "pageup" | "pagedown") {
            if let Some(session) = &self.session {
                session.term.lock().scroll_display(if key == "pageup" {
                    Scroll::PageUp
                } else {
                    Scroll::PageDown
                });
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let app_cursor = self.mode().contains(TermMode::APP_CURSOR);
        if let Some(bytes) = keys::encode(keystroke, app_cursor) {
            // 只是一个回车（不是 Shift、Alt 加回车那种换行）：发出去一条消息
            let enter = key == "enter" && !mods.modified();
            if enter {
                self.record_mark();
            }
            self.type_bytes(bytes, enter, cx);
            cx.stop_propagation();
        }
    }

    /// 窗口里的一个位置在屏幕上的第几列、第几行（从 0 起，不管往回翻了多少），报给程序用。
    fn screen_cell(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        let geometry = self.geometry.get();
        let grid = geometry.grid?;
        let at = |offset: Pixels, size: Pixels, count: usize| {
            let cell = (f32::from(offset) / f32::from(size.max(px(1.)))).max(0.) as usize;
            cell.min(count.saturating_sub(1))
        };
        Some((
            at(
                position.x - geometry.origin.x,
                geometry.cell.width,
                grid.cols,
            ),
            at(
                position.y - geometry.origin.y,
                geometry.cell.height,
                grid.rows,
            ),
        ))
    }

    /// 窗口里的一个位置在第几行第几列，偏在这一格的左半边还是右半边。
    fn grid_point(&self, position: Point<Pixels>) -> Option<(GridPoint, GridSide)> {
        let geometry = self.geometry.get();
        let grid = geometry.grid?;
        let session = self.session.as_ref()?;
        let x = (f32::from(position.x - geometry.origin.x) / f32::from(geometry.cell.width))
            .clamp(0., grid.cols as f32 - 0.01);
        let y = (f32::from(position.y - geometry.origin.y) / f32::from(geometry.cell.height))
            .clamp(0., grid.rows as f32 - 0.01);
        let side = if x.fract() < 0.5 {
            GridSide::Left
        } else {
            GridSide::Right
        };
        let offset = session.term.lock().grid().display_offset();
        // 固定住的输入区里是现在的屏幕，不按往回翻的行数挪
        let line = self.scroll.fixed.get().line(y as usize, grid.rows, offset);
        let point = GridPoint::new(GridLine(line), GridColumn(x as usize));
        Some((point, side))
    }

    /// 鼠标交不交给程序：程序要了鼠标（opencode、全屏的 Claude Code、开了鼠标的 vim、htop）、没按
    /// Shift、没往回翻时交给它，给的是它要的那几样模式。按着 Shift 是 Kuai 自己选字（和 Windows
    /// 终端一样）。原来只把滚轮报给程序，点击、拖动都留给自己选字，opencode 的按钮点不了。
    fn mouse_mode(&self, shift: bool) -> Option<TermMode> {
        let session = self.session.as_ref()?;
        let term = session.term.lock();
        let mode = *term.mode();
        (!shift && mode.intersects(TermMode::MOUSE_MODE) && term.grid().display_offset() == 0)
            .then_some(mode)
    }

    /// 报一下鼠标（`button` 见 [`keys::mouse_report`]，Alt、Ctrl 在这里加上）。
    fn report_mouse(
        &mut self,
        mode: TermMode,
        button: u8,
        release: bool,
        position: Point<Pixels>,
        modifiers: Modifiers,
    ) {
        let Some((col, row)) = self.screen_cell(position) else {
            return;
        };
        let extra = if modifiers.alt { 8 } else { 0 } + if modifiers.control { 16 } else { 0 };
        self.mouse_cell = Some((col, row));
        let (sgr, utf8) = (
            mode.contains(TermMode::SGR_MOUSE),
            mode.contains(TermMode::UTF8_MOUSE),
        );
        if let Some(bytes) = keys::mouse_report(button + extra, release, col, row, sgr, utf8) {
            self.write(bytes);
        }
    }

    /// 按下鼠标键：程序要了鼠标就报给它，不再自己选字、粘贴、弹菜单（返回 true）。按着的时候
    /// 挪动、松开由 `paint` 里挂在整个窗口上的监听接着报，挪出终端、在外面松开也报得到。
    fn press_mouse(
        &mut self,
        button: u8,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(mode) = self.mouse_mode(event.modifiers.shift) else {
            return false;
        };
        self.click_focus(window, cx);
        self.report_mouse(mode, button, false, event.position, event.modifiers);
        self.mouse_held = Some(button);
        // 又点了：程序里原来选的字多半没了；左键拖开就是重新选
        self.program_selected = None;
        self.program_drag = (button == 0).then_some(false);
        // 右键：设置里选了菜单时，外面那一层的菜单不弹
        if button == 2 {
            cx.stop_propagation();
        }
        cx.notify();
        true
    }

    /// 按着报过的键挪动：要了拖动、全部移动的程序，挪到另一格时报。
    fn held_mouse_moved(&mut self, event: &MouseMoveEvent) {
        let Some(button) = self.mouse_held else {
            return;
        };
        let mode = self.mode();
        if !mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
            return;
        }
        if self.screen_cell(event.position) != self.mouse_cell {
            self.report_mouse(mode, button + 32, false, event.position, event.modifiers);
            if let Some(moved) = &mut self.program_drag {
                *moved = true;
            }
        }
    }

    /// 松开报过的键。程序这时已经不要鼠标了就不报（不然成了打进去的字）。
    fn release_mouse(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some(button) = self.mouse_held.take() else {
            return;
        };
        // 在程序里拖选完了：记下这时剪贴板里的字。程序松开时自己复制了的（tmux、opencode 设成选中
        // 即复制），之后剪贴板会变，Ctrl+Shift+C 就不再转给它（见 `copy_in_program`）
        if self.program_drag.take() == Some(true) {
            self.program_selected = Some(cx.read_from_clipboard().and_then(|item| item.text()));
        }
        let mode = self.mode();
        if mode.intersects(TermMode::MOUSE_MODE) {
            self.report_mouse(mode, button, true, event.position, event.modifiers);
        }
        cx.notify();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 按住 Ctrl 单击链接：打开它，不选字、不报给程序（见 [`links`]）
        if self.click_link(event, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.press_mouse(0, event, window, cx) {
            return;
        }
        self.click_focus(window, cx);
        let Some((point, side)) = self.grid_point(event.position) else {
            return;
        };
        // 单击开始拖选，双击选一个词，三击选一行
        let kind = match event.click_count {
            0 | 1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        if let Some(session) = &self.session {
            session.term.lock().selection = Some(Selection::new(kind, point, side));
        }
        self.selecting = true;
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        // 按着 Ctrl：鼠标底下的链接画上下划线
        self.hover_link(event.position, event.modifiers.control, cx);
        // 什么都没按地挪：要了全部移动的程序（opencode 用它做悬停）挪到另一格时报。按着报过的键时
        // 由窗口上的监听报，这里不重复
        if self.mouse_held.is_none()
            && !self.selecting
            && let Some(mode) = self.mouse_mode(event.modifiers.shift)
            && mode.contains(TermMode::MOUSE_MOTION)
            && self.screen_cell(event.position) != self.mouse_cell
        {
            self.report_mouse(mode, 35, false, event.position, event.modifiers);
        }
        if !self.selecting {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.selecting = false;
            return;
        }
        let Some((point, side)) = self.grid_point(event.position) else {
            return;
        };
        if let Some(session) = &self.session
            && let Some(selection) = session.term.lock().selection.as_mut()
        {
            selection.update(point, side);
        }
        cx.notify();
    }

    /// 松开右键、中键：按下时报给了程序的，松开也报。
    fn on_other_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.release_mouse(event, cx);
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        // 按下时报给了程序的：松开也报。窗口上那个监听要等按下以后画一次才挂上，按下、松开挨得很近
        // （触摸板轻点、模拟的点击）时松开先到，原来就丢了，Kuai 当键一直按着，之后一挪就报成拖动
        self.release_mouse(event, cx);
        let was_selecting = std::mem::take(&mut self.selecting);
        // 只点了一下没拖：不算选了东西
        if let Some(session) = &self.session {
            let mut term = session.term.lock();
            if term.selection.as_ref().is_some_and(Selection::is_empty) {
                term.selection = None;
            }
        }
        // 设置里开了「选中即复制」：松开鼠标时选着字就复制
        if was_selecting && prefs::get(cx).copy_on_select && self.selected_text().is_some() {
            self.copy(cx);
        }
        cx.notify();
    }

    /// 中键：程序要了鼠标就报给它，不然不做什么。
    fn on_middle_click(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.press_mouse(1, event, window, cx);
    }

    /// 右键：程序要了鼠标就报给它（按着 Shift 的除外）。点在网址、路径上弹它的菜单（见
    /// [`links::fill_menu`]）；别处选着字就复制，没选就粘贴（和 Windows 终端一样），设置里选了菜单时
    /// 弹复制、粘贴的菜单。菜单挂在外面那一层，这里不让它弹时拦下。
    fn on_right_click(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.press_mouse(2, event, window, cx) {
            return;
        }
        self.click_focus(window, cx);
        if self.menu_at(event.position, cx)
            || prefs::get(cx).right_click == crate::settings::RightClick::Menu
        {
            return;
        }
        cx.stop_propagation();
        if self.selected_text().is_some() {
            self.copy(cx);
        } else if self.is_alive() {
            self.paste(cx);
        }
    }

    fn on_scroll(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        cx.stop_propagation();
        let cell = self.geometry.get().cell.height.max(px(1.));
        let mods = event.modifiers;
        let delta = event.delta.pixel_delta(cell);
        // Windows 把 Shift+滚轮当成横着滚
        let dy = if mods.shift && delta.y == px(0.) {
            delta.x
        } else {
            delta.y
        };
        let moved = f32::from(dy) / f32::from(cell) + self.scroll_rest;
        let mode = *session.term.lock().mode();
        let target = keys::wheel_target(mode, mods.shift);
        // 报给程序的滚一格报一次（和 Windows 终端一样，程序自己定一格翻多少），别的按行数。
        // 触摸板一次只给零点几行，攒够了再算
        let unit = match target {
            WheelTarget::Report => win::wheel_lines(),
            WheelTarget::Arrows | WheelTarget::Scrollback => 1.,
        };
        let steps = (moved / unit).trunc() as i32;
        self.scroll_rest = moved - steps as f32 * unit;
        if steps == 0 {
            return;
        }
        let up = steps > 0;
        let count = steps.unsigned_abs() as usize;
        match target {
            WheelTarget::Report => {
                let Some((col, row)) = self.screen_cell(event.position) else {
                    return;
                };
                let extra = if mods.alt { 8 } else { 0 } + if mods.control { 16 } else { 0 };
                let (sgr, utf8) = (
                    mode.contains(TermMode::SGR_MOUSE),
                    mode.contains(TermMode::UTF8_MOUSE),
                );
                if let Some(report) = keys::wheel_report(up, col, row, extra, sgr, utf8) {
                    self.write(report.repeat(count));
                }
            }
            WheelTarget::Arrows => {
                let key: &[u8] = match (up, mode.contains(TermMode::APP_CURSOR)) {
                    (true, false) => b"\x1b[A",
                    (true, true) => b"\x1bOA",
                    (false, false) => b"\x1b[B",
                    (false, true) => b"\x1bOB",
                };
                self.write(key.repeat(count));
            }
            WheelTarget::Scrollback => {
                session.term.lock().scroll_display(Scroll::Delta(steps));
                cx.notify();
            }
        }
    }

    /// 进程没在跑（或者够不着）时，盖在画面底下的一句话。
    fn notice(&self) -> Option<SharedString> {
        // 只看的不能在这里重新开、重新接，只说一声
        if self.watch.is_some() && (self.lost || self.exited.is_some()) {
            return Some(match self.exited {
                Some(Some(code)) => {
                    tf!("程序已结束（退出码 {code}）", code = code.to_string()).into()
                }
                Some(None) => tr("程序已结束").into(),
                None => tr("连接已断开").into(),
            });
        }
        if let Some(error) = &self.error {
            return Some(tf!("{error} 按 Enter 重试。", error).into());
        }
        if self.connecting.is_some() {
            return Some(match self.local {
                true => tr("正在启动终端…").into(),
                false => tr("正在连接主机…").into(),
            });
        }
        if self.lost && self.exited.is_none() {
            return Some(match self.local {
                true => tr("与 kuaid 的连接已断开，程序仍在运行。按 Enter 重新连接。").into(),
                false => tr("与主机的连接已断开，程序仍在运行。按 Enter 重新连接。").into(),
            });
        }
        match self.exited {
            Some(Some(0) | None) => return Some(tr("进程已结束。按 Enter 重新启动。").into()),
            Some(Some(code)) => {
                return Some(tf!("进程已结束（退出码 {code}）。按 Enter 重新启动。", code).into());
            }
            None => {}
        }
        // 本机的终端不用传，一闪就过，不说
        if let Some(sending) = &self.sending
            && self.session.as_ref().is_some_and(Session::is_remote)
        {
            return Some(sending.notice());
        }
        self.flash.clone()
    }
}

impl Sending {
    /// 传到哪了。
    fn notice(&self) -> SharedString {
        let name = &self.name;
        let progress = match self.total {
            0 => String::new(),
            total => tf!(
                "：{sent} / {total}",
                sent = crate::text::human_size(self.sent),
                total = crate::text::human_size(total)
            ),
        };
        if self.count > 1 {
            let (index, count) = (self.index + 1, self.count);
            tf!(
                "正在上传第 {index}/{count} 个文件 {name}{progress}。按 Esc 取消。",
                index,
                count,
                name,
                progress
            )
            .into()
        } else {
            tf!("正在上传 {name}{progress}。按 Esc 取消。", name, progress).into()
        }
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|marked| 0..marked.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    /// 打出来的字、输入法上屏的字。
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = None;
        if !text.is_empty() && self.is_alive() {
            self.input(text.as_bytes().to_vec(), cx);
        }
        cx.notify();
    }

    /// 输入法正在组字：先画在光标那里，不发给程序。
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        new_text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = (!new_text.is_empty()).then(|| new_text.to_string());
        // 往回翻着时开始组字：和打字一样（见 `type_bytes`），输入区没固定住时先回到最底下，光标（组的
        // 字画在那里、候选框跟着它）才看得见；固定住时光标就在底下固定的那几行里，画面不动
        if self.marked.is_some()
            && self.scroll.fixed.get().len == 0
            && let Some(session) = &self.session
        {
            session.term.lock().scroll_display(Scroll::Bottom);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.geometry.get().cursor
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (background, muted, bar) = (theme.background, theme.muted_foreground, theme.secondary);
        let (for_prepaint, for_paint) = (cx.entity(), cx.entity());
        let view = cx.entity().downgrade();
        let overlays = self.scroll_overlays(cx);
        // 鼠标在网址、路径上停着：下面一个提示
        let tip = self.link_tip(window, cx);
        let panel = div()
            .id("terminal")
            .relative()
            .size_full()
            .pl(px(scroll::GUTTER))
            .pr(px(PADDING))
            .pt(px(scroll::TOP))
            .bg(background)
            // 按住 Ctrl 指着链接时是手形（见 [`links`]）
            .when(self.links.pointer(), |panel| panel.cursor_pointer())
            .when(!self.links.pointer(), |panel| panel.cursor_text())
            // 打字时鼠标没离开：界面库默认打过字就当鼠标不在上面（离开了），鼠标没动、接着按 Ctrl
            // 就认不出底下的链接
            .hover_listener_mode(HoverListenerMode::InputModalityIndependent)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered {
                    this.leave_link(cx);
                }
            }))
            .track_focus(&self.focus)
            .key_context(KEY_CONTEXT)
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_click))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_middle_click))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_other_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_other_mouse_up))
            .on_scroll(cx.listener(Self::on_scroll))
            // 从资源管理器拖文件进来：拖到上面时底色变一下（不能加边框，会挤得行列数变）
            .drag_over::<ExternalPaths>(|style, _, _, cx| {
                style.bg(cx.theme().primary.opacity(0.12))
            })
            .on_drop(cx.listener(Self::on_drop))
            .child(
                canvas(
                    move |bounds, window, cx| prepaint(&for_prepaint, bounds, window, cx),
                    move |bounds, frame, window, cx| paint(&for_paint, bounds, frame, window, cx),
                )
                .size_full(),
            )
            .children(overlays)
            .when_some(self.notice(), |this, notice| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .px(px(PADDING))
                        .py_1()
                        .bg(bar)
                        .text_xs()
                        .text_color(muted)
                        .child(notice),
                )
            });
        // 右键菜单：点在网址、路径上，或者设置里选了菜单时才弹（不弹的在 `on_right_click` 里拦下）
        div()
            .size_full()
            .child(
                panel.context_menu(move |menu, window, cx| {
                    links::open_menu(&view, menu, window, cx)
                }),
            )
            .children(tip)
            .into_any_element()
    }
}

/// 光标闪烁：亮、灭各这么久
const BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// 终端面板的按键上下文：焦点在终端里时哪些快捷键让给里面的程序，按它来定（见 main.rs）。
pub const KEY_CONTEXT: &str = "Terminal";

/// 一段样式相同、挨在一起的字。
struct Run {
    row: usize,
    col: usize,
    /// 下一个字该在第几列；不是就得另起一段
    next: usize,
    text: String,
    /// 两格宽的字（中文等）
    wide: bool,
    /// 带组合符号的字：单独画，不强制每个字形一格
    single: bool,
    /// 只有这一个字、后面不再接别的字（见 [`stands_alone`]）
    lone: bool,
    style: Style,
}

/// 这个字要不要单独成一段。
///
/// 一段字交给界面库排，每个字形摆到「第几个字形 × 格宽」的位置上；它把排出来比前一个字形靠后不到
/// 半格的字形当成组合符号，叠在前一个上面、不占位置。等宽字体里没有的字由别的字体补，那些字体里
/// 有比半格还窄的字形：两格宽的「」【】在一些字体里只有半个字宽，后面的字就叠了上去，整段再往左
/// 缩两格（1.15 以前「看改动」的前两个字叠在一起）。所以只有宽度靠得住的字连成段：一格宽的只连
/// ASCII（等宽字体自己的），两格宽的只连文字（汉字、假名、谚文、全角字母，各种字体里都是一个字宽）；
/// 标点、符号、表情和其余的字一个一段，摆在它自己的格子上。
fn stands_alone(c: char, wide: bool) -> bool {
    match wide {
        true => !c.is_alphabetic(),
        false => !c.is_ascii(),
    }
}

/// 两格宽的前括号、前引号：字形只有半个字宽时靠右摆，贴着后面的字。
fn opens(c: char) -> bool {
    matches!(
        c,
        '「' | '『'
            | '（'
            | '［'
            | '｛'
            | '〈'
            | '《'
            | '【'
            | '〔'
            | '〖'
            | '〘'
            | '〚'
            | '｟'
            | '〝'
    )
}

#[derive(Clone, Copy, PartialEq)]
struct Style {
    color: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

/// 光标该怎么画。
struct CursorDraw {
    row: usize,
    col: usize,
    wide: bool,
    shape: CursorShape,
    /// 光标下面的字：实心块光标要用背景色把它重画一遍
    ch: char,
}

/// 一帧要画的东西：从格子上拍下来的，画的时候不再碰仿真器。
struct Frame {
    origin: Point<Pixels>,
    cell: Size<Pixels>,
    /// 背景和默认不一样的格子：第几行、从第几列起、几列、什么颜色
    rects: Vec<(usize, usize, usize, Hsla)>,
    /// 自己画的字（方块、制表线，见 [`glyphs`]）：第几行、第几列、哪个字、什么颜色
    shapes: Vec<(usize, usize, char, Hsla)>,
    runs: Vec<Run>,
    /// 画虚线的格子（没按 Ctrl、鼠标停着的链接）：第几行、从第几列起、几列
    dashes: Vec<(usize, usize, usize)>,
    cursor: Option<CursorDraw>,
    foreground: Hsla,
    background: Hsla,
    focused: bool,
    marked: Option<String>,
    /// 画字用的字体（不带粗体、斜体）和字号
    font: Font,
    font_size: Pixels,
    /// 往回翻着时固定住的输入区从第几行起（上边画一条线和一点阴影）
    fixed_top: Option<usize>,
    /// 刚跳到的那条消息在第几行：亮一下
    flash: Option<usize>,
    /// 那条线、亮的那一下用的颜色
    accents: (Hsla, Hsla),
}

fn prepaint(
    view: &Entity<TerminalView>,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Frame> {
    let prefs = prefs::get(cx);
    let base_font = prefs::base_font(&prefs);
    let font_id = window.text_system().resolve_font(&base_font);
    let advance = |size: Pixels| {
        window
            .text_system()
            .advance(font_id, size, 'm')
            .map(|advance| advance.width)
            .unwrap_or(size * 0.55)
    };
    // 按设置里的字号，一格多大；这块面板放得下几行几列
    let full = size(advance(px(prefs.font_size)), px(prefs.cell_height()));
    let roomy = Grid {
        cols: ((bounds.size.width / full.width).floor() as usize).max(2),
        rows: ((bounds.size.height / full.height).floor() as usize).max(1),
    };
    // 「只看」接着的终端，这边正往里输入：大小交给这一边（见 `wants_size`、`claim`）。面板小得放不下
    // 几个字时（正在收起、窗口极小）不改
    let room = bounds.size.width >= full.width * 4. && bounds.size.height >= full.height;
    if room && view.update(cx, |this, _| this.wants_size(roomy, window)) {
        view.update(cx, |this, cx| this.claim(roomy, cx));
    }
    let watch = view.read(cx).watch;
    let (grid, cell, font_size) = match watch {
        // 行列数是那个终端定的：放不下就按比例把字缩小
        Some(grid) => {
            let (cols, rows) = (grid.cols as f32, grid.rows as f32);
            let fit = (bounds.size.width / (full.width * cols))
                .min(bounds.size.height / (full.height * rows))
                .min(1.);
            let font_size = px((prefs.font_size * fit).max(MIN_WATCH_FONT));
            let scale = font_size / px(prefs.font_size);
            let cell = size(advance(font_size), px(prefs.cell_height() * scale));
            (grid, cell, font_size)
        }
        None => (roomy, full, px(prefs.font_size)),
    };
    // 面板小得放不下几个字（正在收起、窗口极小）：不启动、不改大小
    if bounds.size.width < cell.width * 4. || bounds.size.height < cell.height {
        return None;
    }
    if watch.is_none() {
        view.update(cx, |this, cx| this.fit(grid, cell, cx));
    }

    let theme = cx.theme();
    let palette = Palette::new(theme.foreground, theme.background, theme.is_dark());
    let (selection, accents) = (theme.selection, (theme.border, theme.primary.opacity(0.22)));
    let this = view.read(cx);
    let session = this.session.as_ref()?;
    let focused = this.focus.is_focused(window);
    let (rects, shapes, runs, cursor, dashes, fixed, flash) = {
        let term = session.term.lock();
        this.blinking.set(focused && term.cursor_style().blinking);
        // 往回翻着时最下面的输入区固定住（见 [`scroll`]）
        let fixed = scroll::fixed_area(&term);
        let offset = term.grid().display_offset();
        if this.scroll.fixed.replace(fixed).len != fixed.len {
            // 只在 FR_LOG=1 时打出来：断开远程桌面、看不到画面时靠它核对
            log::info!(
                "终端输入区固定 {} 行（屏幕第 {} 行起，往回翻 {offset} 行）",
                fixed.len,
                fixed.top
            );
        }
        let rows = term.grid().screen_lines();
        let split = fixed.split(rows);
        let flash = scroll::flash_row(this.scroll.flash, offset, split);
        // 按住 Ctrl 指着的链接、右键菜单说的那一段画实线；没按 Ctrl 停着的画虚线（见 [`links`]）
        let (link, dashed) = this.links.lines();
        let (rects, shapes, runs, cursor, dashes) =
            snapshot(&term, &palette, selection, fixed, link, dashed);
        let fixed_top = (fixed.len > 0).then_some(split);
        (rects, shapes, runs, cursor, dashes, fixed_top, flash)
    };
    let mut frame = Frame {
        origin: bounds.origin,
        cell,
        rects,
        shapes,
        runs,
        dashes,
        // 进程结束了、够不着了就不画光标
        cursor: cursor.filter(|_| this.is_alive()),
        foreground: palette.foreground,
        background: palette.background,
        focused,
        marked: this.marked.clone(),
        font: base_font,
        font_size,
        fixed_top: fixed,
        flash,
        accents,
    };
    let cursor_bounds = frame
        .cursor
        .as_ref()
        .map(|cursor| frame.cell_bounds(cursor.row, cursor.col, 1));
    // 闪到「灭」的那半拍不画光标；它的位置照记（输入法的候选框跟着它）
    if this.blink_hidden && focused && this.marked.is_none() {
        frame.cursor = None;
    }
    let moved = this.geometry.get().cursor != cursor_bounds;
    let before = this.geometry.get();
    if before.origin != bounds.origin || before.cell != cell || before.grid != Some(grid) {
        log::info!(
            "终端画面：左上角 ({:.1}, {:.1})，每格 {:.2}×{:.2}，{} 列 {} 行",
            f32::from(bounds.origin.x),
            f32::from(bounds.origin.y),
            f32::from(cell.width),
            f32::from(cell.height),
            grid.cols,
            grid.rows
        );
    }
    this.geometry.set(Geometry {
        origin: bounds.origin,
        cell,
        grid: Some(grid),
        cursor: cursor_bounds,
    });
    // 正在组字时光标挪了地方（程序还在输出、刚翻回最底下）：让候选框跟过去。
    // 界面库只在开始组字时问一次位置
    if moved && this.marked.is_some() {
        window.invalidate_character_coordinates();
    }
    Some(frame)
}

type Snapshot = (
    Vec<(usize, usize, usize, Hsla)>,
    Vec<(usize, usize, char, Hsla)>,
    Vec<Run>,
    Option<CursorDraw>,
    Vec<(usize, usize, usize)>,
);

/// 把现在看得见的格子拍成要画的背景块、自己画的字、文字段和光标。往回翻着、输入区固定在最下面时
/// （见 [`scroll::Fixed`]），那几行画的是现在的屏幕，不是往回翻到的内容。`link`：按住 Ctrl 指着
/// 的链接从哪一格到哪一格，画下划线；`dashed`：没按 Ctrl、鼠标停着的链接，画虚线（见 [`links`]）。
fn snapshot(
    term: &Term<Proxy>,
    palette: &Palette,
    selection_color: Hsla,
    fixed: scroll::Fixed,
    link: Option<(GridPoint, GridPoint)>,
    dashed: Option<(GridPoint, GridPoint)>,
) -> Snapshot {
    let content = term.renderable_content();
    let offset = content.display_offset as i32;
    let grid = term.grid();
    let (rows, cols) = (grid.screen_lines(), grid.columns());
    let split = fixed.split(rows) as i32;
    let viewport = content
        .display_iter
        .filter(move |indexed| indexed.point.line.0 + offset < split)
        .map(move |indexed| (indexed.point.line.0 + offset, indexed.point, indexed.cell));
    let live = (split..rows as i32).flat_map(move |row| {
        let line = fixed.top as i32 + row - split;
        (0..cols).map(move |col| {
            let point = GridPoint::new(GridLine(line), GridColumn(col));
            (row, point, &grid[point])
        })
    });
    let overrides = content.colors;
    let selection = content.selection;
    let cursor_at = content.cursor.point;
    let cursor_shape = content.cursor.shape;
    let mut rects: Vec<(usize, usize, usize, Hsla)> = Vec::new();
    let mut shapes: Vec<(usize, usize, char, Hsla)> = Vec::new();
    let mut runs: Vec<Run> = Vec::new();
    let mut current: Option<Run> = None;
    let mut cursor = None;
    let mut dashes: Vec<(usize, usize, usize)> = Vec::new();
    let flush = |current: &mut Option<Run>, runs: &mut Vec<Run>| {
        if let Some(mut run) = current.take() {
            let kept = run.text.trim_end_matches(' ').len();
            run.text.truncate(kept);
            if !run.text.is_empty() {
                runs.push(run);
            }
        }
    };
    for (row, point, cell) in viewport.chain(live) {
        if row < 0 {
            continue;
        }
        let (row, col) = (row as usize, point.column.0);
        let flags = cell.flags;
        // 两格宽的字后面那一格是占位的，不画
        if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }
        let wide = flags.contains(Flags::WIDE_CHAR);
        let width = if wide { 2 } else { 1 };
        let mut fg = palette.resolve(cell.fg, overrides);
        let mut bg = palette.resolve(cell.bg, overrides);
        let mut default_bg = cell.bg == Color::Named(NamedColor::Background);
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
            default_bg = false;
        }
        if flags.contains(Flags::DIM) {
            fg = colors::dim(fg);
        }
        if selection.is_some_and(|range| range.contains(point)) {
            bg = selection_color;
            default_bg = false;
        }
        if !default_bg {
            match rects.last_mut() {
                Some((r, c, n, color)) if *r == row && *c + *n == col && *color == bg => {
                    *n += width;
                }
                _ => rects.push((row, col, width, bg)),
            }
        }
        // 程序把光标藏起来时（Claude Code 自己画一个）也记下它在哪：输入法的候选框、正在组的字
        // 都跟着真光标；画的时候形状是 Hidden，什么都不画
        if point == cursor_at {
            cursor = Some(CursorDraw {
                row,
                col,
                wide,
                shape: cursor_shape,
                ch: cell.c,
            });
        }
        let underline = flags.intersects(Flags::ALL_UNDERLINES)
            || link.is_some_and(|(start, end)| start <= point && point <= end);
        if dashed.is_some_and(|(start, end)| start <= point && point <= end) {
            match dashes.last_mut() {
                Some((r, c, n)) if *r == row && *c + *n == col => *n += width,
                _ => dashes.push((row, col, width)),
            }
        }
        let strike = flags.contains(Flags::STRIKEOUT);
        let hidden = flags.contains(Flags::HIDDEN);
        // 方块、制表线不交给字体：按格子画，一格接一格没有缝。在文字段里它算一个空格
        let drawn = !hidden && glyphs::is_drawn(cell.c);
        if drawn {
            shapes.push((row, col, cell.c, fg));
        }
        let blank = drawn || hidden || cell.c == ' ' || cell.c == '\t';
        if blank {
            // 空格接在前一段后面（省得一个词一段），划线不一样时才断开；开头的空格不用画
            match &mut current {
                Some(run)
                    if run.row == row
                        && run.next == col
                        && !run.wide
                        && !run.single
                        && !run.lone
                        && run.style.underline == underline
                        && run.style.strike == strike =>
                {
                    run.text.push(' ');
                    run.next += 1;
                }
                _ => flush(&mut current, &mut runs),
            }
            continue;
        }
        let style = Style {
            color: fg,
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline,
            strike,
        };
        let marks = cell.zerowidth().filter(|marks| !marks.is_empty());
        let lone = stands_alone(cell.c, wide);
        let joins = !lone
            && current.as_ref().is_some_and(|run| {
                run.row == row
                    && run.next == col
                    && run.wide == wide
                    && !run.single
                    && !run.lone
                    && run.style == style
            });
        if joins && marks.is_none() {
            let run = current.as_mut().unwrap();
            run.text.push(cell.c);
            run.next += width;
        } else {
            flush(&mut current, &mut runs);
            let mut text = String::from(cell.c);
            text.extend(marks.into_iter().flatten());
            current = Some(Run {
                row,
                col,
                next: col + width,
                text,
                wide,
                single: marks.is_some(),
                lone,
                style,
            });
        }
    }
    flush(&mut current, &mut runs);
    (rects, shapes, runs, cursor, dashes)
}

impl Frame {
    fn cell_bounds(&self, row: usize, col: usize, cols: usize) -> Bounds<Pixels> {
        Bounds::new(
            point(
                self.origin.x + self.cell.width * col as f32,
                self.origin.y + self.cell.height * row as f32,
            ),
            size(self.cell.width * cols as f32, self.cell.height),
        )
    }
}

/// 单独成段的字在它的格子里往右挪多少：`width` 是字形排出来的宽度。
///
/// - 一格宽、字形比格子窄的（别的字体补的间隔号、细的符号）居中。
/// - 两格宽的前括号，字形只有半个字宽时靠右：汉字从格子左边起占一个字宽，它的右边和那里对齐，
///   和整个字宽的前括号（笔画在右半边）看起来一样。
/// - 其余的从格子左边起，和连成段的字一样。
fn lone_shift(run: &Run, width: Pixels, cell_width: Pixels, font_size: Pixels) -> Pixels {
    let first = run.text.chars().next().unwrap_or(' ');
    match run.wide {
        false if width < cell_width => (cell_width - width) / 2.,
        true if opens(first) && width < font_size * 0.75 => font_size - width,
        _ => px(0.),
    }
}

fn text_run(len: usize, style: Style, base: &Font) -> TextRun {
    let mut font = base.clone();
    if style.bold {
        font = font.bold();
    }
    if style.italic {
        font = font.italic();
    }
    TextRun {
        len,
        font,
        color: style.color,
        background_color: None,
        // 下划线、删除线不交给排版画（那样画不出来），按格子自己画，见 [`paint_decorations`]
        underline: None,
        strikethrough: None,
    }
}

/// 下划线（程序写的、按住 Ctrl 指着的链接、输入法正在组的字）、删除线：在 `cells` 这几格里按格子画。
///
/// 不用排版带的下划线：终端里的字是一段段强制每字一格排的，那样带的下划线、删除线画不出来
/// （`\e[4m`、`\e[9m` 的字、指着的链接原来都没有线）。
fn paint_decorations(cells: Bounds<Pixels>, style: Style, window: &mut Window) {
    let line = |y: Pixels| Bounds::new(point(cells.origin.x, y), size(cells.size.width, px(1.)));
    if style.underline {
        window.paint_quad(fill(line(cells.bottom() - px(2.)), style.color));
    }
    if style.strike {
        window.paint_quad(fill(
            line(cells.origin.y + cells.size.height / 2.),
            style.color,
        ));
    }
}

fn paint(
    view: &Entity<TerminalView>,
    bounds: Bounds<Pixels>,
    frame: Option<Frame>,
    window: &mut Window,
    cx: &mut App,
) {
    let focus = view.read(cx).focus.clone();
    // 不管里面有没有东西，焦点在这里时打的字都归它
    window.handle_input(&focus, ElementInputHandler::new(bounds, view.clone()), cx);
    // 鼠标在终端里时按下、松开 Ctrl：重新看鼠标底下是不是链接（焦点不在终端里也看）
    if view.read(cx).links.mouse_at.is_some() {
        let watching = view.clone();
        window.on_modifiers_changed(move |event: &ModifiersChangedEvent, _, cx| {
            watching.update(cx, |this, cx| {
                this.ctrl_changed(event.modifiers.control, cx)
            });
        });
    }
    // 按着报给了程序的鼠标键：挪出终端、在外面松开也接着报（见 `TerminalView::press_mouse`）
    if view.read(cx).mouse_held.is_some() {
        let moving = view.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                moving.update(cx, |this, _| this.held_mouse_moved(event));
            }
        });
        let ending = view.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                ending.update(cx, |this, cx| this.release_mouse(event, cx));
            }
        });
    }
    // 拖着滚动条的滑块：鼠标挪出终端也接着跟（见 [`scroll`]）
    if view.read(cx).scroll.drag.is_some() {
        let moving = view.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble && event.pressed_button == Some(MouseButton::Left) {
                moving.update(cx, |this, cx| this.drag_bar(event.position.y, cx));
            }
        });
        let ending = view.clone();
        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                ending.update(cx, |this, cx| {
                    this.scroll.drag = None;
                    if let Some(session) = &this.session {
                        let offset = session.term.lock().grid().display_offset();
                        log::info!("拖完滚动条：往回翻 {offset} 行");
                    }
                    cx.notify();
                });
            }
        });
    }
    if let Some(frame) = frame {
        paint_frame(&frame, bounds, window, cx);
    }
}

/// 把一帧画出来：背景块、自己画的字、文字、光标、输入法正在组的字。设置页的预览也用它。
fn paint_frame(frame: &Frame, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    let font_size = frame.font_size;
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        for &(row, col, cols, color) in &frame.rects {
            window.paint_quad(fill(frame.cell_bounds(row, col, cols), color));
        }
        let width = bounds.size.width;
        if let Some(row) = frame.flash {
            let line = frame.cell_bounds(row, 0, 1);
            window.paint_quad(fill(
                Bounds::new(line.origin, size(width, line.size.height)),
                frame.accents.1,
            ));
        }
        for &(row, col, ch, color) in &frame.shapes {
            glyphs::paint(ch, frame.cell_bounds(row, col, 1), color, window);
        }
        for run in &frame.runs {
            let force = match (run.single || run.lone, run.wide) {
                (true, _) => None,
                (false, true) => Some(frame.cell.width * 2.),
                (false, false) => Some(frame.cell.width),
            };
            let text: SharedString = run.text.clone().into();
            let line = window.text_system().shape_line(
                text,
                font_size,
                &[text_run(run.text.len(), run.style, &frame.font)],
                force,
            );
            let mut origin = frame.cell_bounds(run.row, run.col, 1).origin;
            if run.lone {
                origin.x += lone_shift(run, line.width, frame.cell.width, font_size);
            }
            let _ = line.paint(origin, frame.cell.height, TextAlign::Left, None, window, cx);
            let cells = frame.cell_bounds(run.row, run.col, run.next - run.col);
            paint_decorations(cells, run.style, window);
        }
        // 鼠标停着的链接：字下面一条灰的虚线
        let dash = frame.foreground.opacity(0.55);
        for &(row, col, cols) in &frame.dashes {
            let cells = frame.cell_bounds(row, col, cols);
            let y = cells.bottom() - px(2.);
            let mut x = cells.origin.x;
            while x < cells.right() {
                let len = px(3.).min(cells.right() - x);
                window.paint_quad(fill(Bounds::new(point(x, y), size(len, px(1.))), dash));
                x += px(5.);
            }
        }
        if let Some(cursor) = &frame.cursor {
            let cols = if cursor.wide { 2 } else { 1 };
            let area = frame.cell_bounds(cursor.row, cursor.col, cols);
            let color = frame.foreground;
            // 藏起来的光标，焦点不在这里时也不画成空心块
            let shape = match cursor.shape {
                CursorShape::Hidden => CursorShape::Hidden,
                shape if frame.focused => shape,
                _ => CursorShape::HollowBlock,
            };
            match shape {
                CursorShape::Block => {
                    window.paint_quad(fill(area, color));
                    if glyphs::is_drawn(cursor.ch) {
                        glyphs::paint(cursor.ch, area, frame.background, window);
                    } else if cursor.ch != ' ' {
                        let text: SharedString = cursor.ch.to_string().into();
                        let style = Style {
                            color: frame.background,
                            bold: false,
                            italic: false,
                            underline: false,
                            strike: false,
                        };
                        let line = window.text_system().shape_line(
                            text.clone(),
                            font_size,
                            &[text_run(text.len(), style, &frame.font)],
                            None,
                        );
                        let _ = line.paint(
                            area.origin,
                            frame.cell.height,
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    }
                }
                CursorShape::Beam => {
                    let beam = Bounds::new(area.origin, size(px(2.), area.size.height));
                    window.paint_quad(fill(beam, color));
                }
                CursorShape::Underline => {
                    let line = Bounds::new(
                        point(area.origin.x, area.origin.y + area.size.height - px(2.)),
                        size(area.size.width, px(2.)),
                    );
                    window.paint_quad(fill(line, color));
                }
                CursorShape::HollowBlock => {
                    window.paint_quad(outline(area, color, BorderStyle::Solid));
                }
                CursorShape::Hidden => {}
            }
        }
        // 输入法正在组的字：画在光标那里，带下划线
        if let (Some(marked), Some(cursor)) = (&frame.marked, &frame.cursor) {
            let text: SharedString = marked.clone().into();
            let style = Style {
                color: frame.foreground,
                bold: false,
                italic: false,
                underline: true,
                strike: false,
            };
            let line = window.text_system().shape_line(
                text.clone(),
                font_size,
                &[text_run(text.len(), style, &frame.font)],
                None,
            );
            let mut origin = frame.cell_bounds(cursor.row, cursor.col, 1).origin;
            // 光标靠近右边、放不下时往左挪，不让正在组的字被裁掉
            let right = bounds.origin.x + bounds.size.width;
            if origin.x + line.width > right {
                origin.x = (right - line.width).max(bounds.origin.x);
            }
            let cover = Bounds::new(origin, size(line.width, frame.cell.height));
            window.paint_quad(fill(cover, frame.background));
            let _ = line.paint(origin, frame.cell.height, TextAlign::Left, None, window, cx);
            paint_decorations(cover, style, window);
        }
        // 固定住的输入区上边：一条线，上面一点阴影，表示上面的内容在它后面滚
        if let Some(top) = frame.fixed_top {
            let y = frame.cell_bounds(top, 0, 1).origin.y;
            let x = bounds.origin.x;
            for (step, alpha) in [(3., 0.05), (2., 0.08), (1., 0.12)] {
                let shade = Bounds::new(point(x, y - px(step)), size(width, px(1.)));
                window.paint_quad(fill(shade, gpui_kit::black().opacity(alpha)));
            }
            window.paint_quad(fill(
                Bounds::new(point(x, y), size(width, px(1.))),
                frame.accents.0,
            ));
        }
    });
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：那样 `#[test]` 会变成界面库里同名的那个宏
    use super::{
        CursorShape, Feed, Grid, Palette, Processor, Proxy, Restore, Term, TermEvent, session_key,
        session_number, short_title, snapshot,
    };
    use std::sync::atomic::Ordering;

    #[test]
    fn 还原屏幕的几段收齐了才交出去() {
        let mut restore = Restore::default();
        restore.start(vec![3, 0, 4]);
        assert_eq!(restore.parts, [3, 4], "空的段不算");
        let mut fed: Vec<Vec<u8>> = Vec::new();
        let mut take = |restore: &mut Restore, bytes: &[u8]| {
            let woke = restore.take(bytes, |part| fed.push(part.to_vec()));
            (woke, fed.clone())
        };
        // 第一段没收齐：什么都不交，不用重画
        assert_eq!(take(&mut restore, b"ab"), (false, vec![]));
        // 一块里收齐了第一段、第二段收了一截
        assert_eq!(take(&mut restore, b"cdef"), (true, vec![b"abc".to_vec()]));
        // 第二段收齐，同一块里后面的是实时的输出，照常交
        let (woke, got) = take(&mut restore, b"gHI");
        assert!(woke);
        assert_eq!(got, [&b"abc"[..], b"defg", b"HI"]);
        assert!(restore.parts.is_empty() && restore.buf.is_empty());
        assert_eq!(take(&mut restore, b"J").1.last().unwrap(), b"J");
        // 没有要还原的（新开的、旧的 kuaid）：收到多少交多少
        let mut plain = Restore::default();
        let mut out = Vec::new();
        assert!(plain.take(b"xy", |part| out.extend_from_slice(part)));
        assert!(!plain.take(b"", |part| out.extend_from_slice(part)));
        assert_eq!(out, b"xy");
    }

    #[test]
    fn 有新内容的唤醒排着的只有一个() {
        let (proxy, mut rx) = Proxy::new();
        for _ in 0..100 {
            assert!(proxy.send(Feed::Term(TermEvent::Wakeup)));
        }
        // 别的事照送
        assert!(proxy.send(Feed::Term(TermEvent::ResetTitle)));
        let mut queued = Vec::new();
        while let Ok(feed) = rx.try_recv() {
            queued.push(feed);
        }
        assert_eq!(queued.len(), 2);
        assert!(matches!(queued[0], Feed::Term(TermEvent::Wakeup)));
        // 界面处理了（放下「排着」）以后再来新内容，再送一个
        proxy.woken.store(false, Ordering::Release);
        assert!(proxy.send(Feed::Term(TermEvent::Wakeup)));
        assert!(proxy.send(Feed::Term(TermEvent::Wakeup)));
        assert!(matches!(rx.try_recv(), Ok(Feed::Term(TermEvent::Wakeup))));
        assert!(rx.try_recv().is_err());
        // 界面那头没了：送不出去
        drop(rx);
        assert!(!proxy.send(Feed::Term(TermEvent::Wakeup)));
        assert!(!proxy.send(Feed::Exited(None)));
    }

    #[test]
    fn 标题是程序的完整路径时只留程序名() {
        assert_eq!(
            short_title(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            "pwsh"
        );
        assert_eq!(short_title(r"C:\WINDOWS\system32\cmd.EXE"), "cmd");
        // 不是路径、路径后面还跟着别的：照原样
        assert_eq!(short_title("✳ Claude Code"), "✳ Claude Code");
        assert_eq!(short_title("me@dev: ~/src"), "me@dev: ~/src");
        assert_eq!(
            short_title(r"C:\WINDOWS\system32\cmd.exe - git status"),
            r"C:\WINDOWS\system32\cmd.exe - git status"
        );
    }

    /// 喂一段程序的输出，看光标记在哪、什么形状。
    #[test]
    fn 往回翻的记录里只有空行时不出滚动条() {
        use super::scroll::history_has_text;
        use alacritty_terminal::grid::Dimensions as _;
        let (proxy, _rx) = Proxy::new();
        let grid = Grid { cols: 20, rows: 5 };
        let mut term = Term::new(Default::default(), &grid, proxy);
        let mut parser: Processor = Processor::new();
        assert!(!history_has_text(&term));
        // 光标在最下面一行换两次行：空着的两行滚进了记录
        parser.advance(&mut term, b"\x1b[5;1H\r\n\r\n");
        assert_eq!(term.grid().history_size(), 2);
        assert!(!history_has_text(&term), "只有空行");
        // 最上面一行写了字再滚上去
        parser.advance(&mut term, b"\x1b[1;1Hhello\x1b[5;1H\r\n");
        assert_eq!(term.grid().history_size(), 3);
        assert!(history_has_text(&term));
        // 记录比一屏还长：不一行行看，当有
        parser.advance(&mut term, "\r\n".repeat(20).as_bytes());
        assert!(history_has_text(&term));
    }

    #[test]
    fn 输入区里的字变了不算新输出() {
        use super::scroll::outside_digest;
        use alacritty_terminal::grid::Dimensions as _;
        let (proxy, _rx) = Proxy::new();
        let grid = Grid { cols: 30, rows: 6 };
        let mut term = Term::new(Default::default(), &grid, proxy);
        let mut parser: Processor = Processor::new();
        for n in 1..=10 {
            parser.advance(&mut term, format!("l{n}\r\n").as_bytes());
        }
        parser.advance(&mut term, b"PS C:\\> ");
        let before = outside_digest(&term);
        // 往回翻着打字：程序回显的字在输入区里
        parser.advance(&mut term, b"git sta");
        assert_eq!(outside_digest(&term), before);
        // 打的字长得折到下一行，屏幕往上滚了一行：从输入区往上数还是那几行
        parser.advance(&mut term, "x".repeat(20).as_bytes());
        assert_eq!(term.grid().history_size(), 6);
        assert_eq!(outside_digest(&term), before);
        // 程序真的输出了
        parser.advance(&mut term, b"\r\nclean\r\nPS C:\\> ");
        assert_ne!(outside_digest(&term), before);
    }

    fn cursor_after(output: &[u8]) -> Option<(usize, usize, CursorShape)> {
        let (proxy, _rx) = Proxy::new();
        let grid = Grid { cols: 40, rows: 10 };
        let mut term = Term::new(Default::default(), &grid, proxy);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, output);
        let white = gpui_kit::rgb(0xeeeeee).into();
        let black = gpui_kit::rgb(0x111111).into();
        let palette = Palette::new(white, black, true);
        let (_, _, _, cursor, _) = snapshot(
            &term,
            &palette,
            white,
            super::scroll::Fixed::default(),
            None,
            None,
        );
        cursor.map(|cursor| (cursor.row, cursor.col, cursor.shape))
    }

    #[test]
    fn 藏起来的光标也记位置() {
        // Claude Code 把光标藏起来、自己画一个，真光标停在输入的地方。位置要照记：
        // 输入法的候选框、正在组的字都跟着它（以前藏起来就不记，候选框落在别处、拼音不显示）
        let hidden = cursor_after(b"\x1b[?25l\x1b[5;10H");
        assert!(matches!(hidden, Some((4, 9, CursorShape::Hidden))));
        // 再显示出来，位置不变、形状回来
        let shown = cursor_after(b"\x1b[?25l\x1b[5;10H\x1b[?25h");
        assert!(matches!(shown, Some((4, 9, shape)) if shape != CursorShape::Hidden));
        // 备用屏上也一样（Claude Code 用的是备用屏）
        let alt = cursor_after(b"\x1b[?1049h\x1b[?25l\x1b[3;7H");
        assert!(matches!(alt, Some((2, 6, CursorShape::Hidden))));
    }

    /// 喂一段输出，看拍下来的文字段：第几列起、什么字。
    fn runs_of(output: &str) -> Vec<(usize, String)> {
        let (proxy, _rx) = Proxy::new();
        let grid = Grid { cols: 40, rows: 10 };
        let mut term = Term::new(Default::default(), &grid, proxy);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, output.as_bytes());
        let white = gpui_kit::rgb(0xeeeeee).into();
        let black = gpui_kit::rgb(0x111111).into();
        let palette = Palette::new(white, black, true);
        let (_, _, runs, _, _) = snapshot(
            &term,
            &palette,
            white,
            super::scroll::Fixed::default(),
            None,
            None,
        );
        runs.into_iter().map(|run| (run.col, run.text)).collect()
    }

    #[test]
    fn 标点和符号单独成段() {
        // 两格宽的：汉字连成段，「」，各占自己的格子（有的字体里它们只有半个字宽，连在一段里
        // 后面的字会叠上去）
        let runs = runs_of("写「已停下」，好");
        let expected = [
            (0, "写"),
            (2, "「"),
            (4, "已停下"),
            (10, "」"),
            (12, "，"),
            (14, "好"),
        ];
        let expected: Vec<(usize, String)> = expected
            .iter()
            .map(|(col, text)| (*col, text.to_string()))
            .collect();
        assert_eq!(runs, expected);
        // 一格宽的：ASCII 连成段（中间的空格也在里面），别的字一个一段，后面的空格、字母不接上去
        let runs = runs_of("ab cd→ef é");
        let expected = [(0, "ab cd"), (5, "→"), (6, "ef"), (9, "é")];
        let expected: Vec<(usize, String)> = expected
            .iter()
            .map(|(col, text)| (*col, text.to_string()))
            .collect();
        assert_eq!(runs, expected);
    }

    #[test]
    fn 服务器上终端的名字_第一个就是仓库路径() {
        let repo = "/data/workspaces/kuai";
        // 第一个和 1.5 用的名字一样，升上来以后接得上原来的
        assert_eq!(session_key(repo, 1), repo);
        assert_eq!(session_key(repo, 3), "/data/workspaces/kuai\u{1f}3");
        for number in [1, 2, 3, 10, 120] {
            assert_eq!(
                session_number(repo, &session_key(repo, number)),
                Some(number)
            );
        }
    }

    #[test]
    fn 会话的名字拆成路径和序号() {
        use super::split_session_key;
        assert_eq!(split_session_key("/data/kuai"), ("/data/kuai", 1));
        assert_eq!(
            split_session_key(&session_key("/data/kuai", 3)),
            ("/data/kuai", 3)
        );
        assert_eq!(split_session_key(""), ("", 1));
    }

    #[test]
    fn 别的仓库的终端不认() {
        let repo = "/data/workspaces/kuai";
        // 路径只是开头一样的另一个仓库
        assert_eq!(session_number(repo, "/data/workspaces/kuai2"), None);
        assert_eq!(session_number(repo, "/data/workspaces/kuai/sub"), None);
        assert_eq!(session_number(repo, "/data/workspaces/kuai2\u{1f}2"), None);
        assert_eq!(session_number(repo, "/data/workspaces"), None);
        // 不是序号、或者不该出现的序号
        assert_eq!(session_number(repo, "/data/workspaces/kuai\u{1f}"), None);
        assert_eq!(session_number(repo, "/data/workspaces/kuai\u{1f}x"), None);
        assert_eq!(session_number(repo, "/data/workspaces/kuai\u{1f}1"), None);
        assert_eq!(session_number(repo, "/data/workspaces/kuai\u{1f}0"), None);
        // 不是仓库页开的（名字是空的）
        assert_eq!(session_number(repo, ""), None);
        // Windows 上的仓库：kuaid 记的名字是正斜杠、小写的
        let repo = r"C:\Users\me\Repo";
        assert_eq!(session_number(repo, "c:/users/me/repo"), Some(1));
        assert_eq!(session_number(repo, "c:/users/me/repo\u{1f}3"), Some(3));
        assert_eq!(
            session_number("C:/Users/me/Repo", "c:/users/me/repo"),
            Some(1)
        );
        assert_eq!(session_number(repo, "c:/users/me/repo2"), None);
        // Linux 上分大小写
        assert_eq!(session_number("/Data/Repo", "/data/repo"), None);
    }
}
