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

mod attach;
mod colors;
mod glyphs;
mod keys;
mod prefs;
mod preview;

pub use attach::Attachment;
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
use fr_channel::{Connection, TermItem, TermStream, UploadItem, UploadSource};
use fr_core::i18n::tr;
use fr_core::tf;
use futures::channel::mpsc;
use futures::future::LocalBoxFuture;
use futures::{FutureExt as _, StreamExt as _};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::prelude::*;
use gpui_kit::*;

use colors::Palette;

/// 字和面板边缘之间留的空
const PADDING: f32 = 8.;

/// 服务器上终端会话的名字里，仓库路径和序号之间的分隔符（路径里不会有的一个控制字符）
const KEY_SEPARATOR: char = '\u{1f}';

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
}

/// 把仿真器的事件送回界面线程。
#[derive(Clone)]
struct Proxy(mpsc::UnboundedSender<Feed>);

impl EventListener for Proxy {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.unbounded_send(Feed::Term(event));
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
        let (tx, rx) = mpsc::unbounded();
        let proxy = Proxy(tx);
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
            _events: Self::deliver(rx, cx),
        })
    }

    /// 接上开在 kuaid 里的终端：请 kuaid 接上（或者新开）名字是 `key` 的会话，它的输出喂给这里的
    /// 仿真器。
    fn attach(
        conn: Arc<Connection>,
        key: &str,
        cwd: &str,
        shell: &[String],
        grid: Grid,
        cell: Size<Pixels>,
        cx: &mut Context<TerminalView>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded();
        let term = Term::new(prefs::config(&prefs::get(cx)), &grid, Proxy(tx.clone()));
        let term = Arc::new(FairMutex::new(term));
        let stream = conn.term_open(key, cwd, shell, grid.cols as u16, grid.rows as u16);
        let executor = cx.background_executor().clone();
        // 解析输出放在后台：大段输出不卡界面
        let pump = cx
            .background_executor()
            .spawn(pump(stream, term.clone(), tx, executor));
        Self {
            term,
            grid,
            cell,
            link: Link::Remote {
                local: conn.local,
                conn,
                term: RefCell::new(None),
                _pump: pump,
            },
            _events: Self::deliver(rx, cx),
        }
    }

    /// 把后台送来的事交给界面线程上的视图。
    fn deliver(mut rx: mpsc::UnboundedReceiver<Feed>, cx: &mut Context<TerminalView>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            while let Some(feed) = rx.next().await {
                if this.update(cx, |this, cx| this.on_feed(feed, cx)).is_err() {
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

/// 收服务器上终端的输出，喂给仿真器。连接断了、程序结束了就停。
async fn pump(
    mut stream: TermStream,
    term: Arc<FairMutex<Term<Proxy>>>,
    tx: mpsc::UnboundedSender<Feed>,
    executor: BackgroundExecutor,
) {
    let mut parser: Processor = Processor::new();
    let send = |feed: Feed| tx.unbounded_send(feed).is_ok();
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
            Some(Ok(TermItem::Opened { term, .. })) => send(Feed::Opened(term)),
            Some(Ok(TermItem::Output(bytes))) => {
                parser.advance(&mut *term.lock(), &bytes);
                send(Feed::Term(TermEvent::Wakeup))
            }
            Some(Ok(TermItem::Exited(code))) => {
                send(Feed::Exited(code));
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
    launcher: Launcher,
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
    /// 这是 kuaid 里留着的终端（上次没结束），还没接上：也算有东西在跑
    restored: bool,
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
    _blink: Task<()>,
    _subscriptions: Vec<Subscription>,
}

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
            launcher: Box::new(launcher),
            session: None,
            connecting: None,
            fitted: None,
            error: None,
            exited: None,
            lost: false,
            restored: false,
            local: false,
            hosted: false,
            sending: None,
            flash: None,
            focus,
            geometry: Rc::default(),
            marked: None,
            selecting: false,
            scroll_rest: 0.,
            title: None,
            blinking: Rc::default(),
            blink_hidden: false,
            typed_at: Instant::now(),
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

    /// 里面的程序开在 Kuai 进程里：关掉标签页要另外保管，退出 Kuai 它就结束。
    pub fn in_process(&self) -> bool {
        self.session.as_ref().is_some_and(Session::in_process)
    }

    /// 程序自己设的标题。Windows 上 shell 刚启动时标题是它自己的完整路径，只留程序名
    /// （`pwsh`，不带 `.exe`）。
    pub fn title(&self) -> Option<SharedString> {
        let title = self.title.as_ref()?;
        let is_program_path =
            title.to_lowercase().ends_with(".exe") && title.contains(['\\', '/']);
        match title.rsplit(['\\', '/']).next() {
            // 结尾的 .exe 是 ASCII，按字节截不会截到半个字
            Some(name) if is_program_path => Some(name[..name.len() - 4].to_string().into()),
            _ => Some(title.clone()),
        }
    }

    /// 结束里面的进程，重新开一个（下一帧知道大小了再启动）。
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.kill(cx);
        self.reopen(cx);
    }

    /// 结束里面的进程。开在 Kuai 进程里的丢掉会话就结束了；开在 kuaid 里的要告诉它，不然接着跑。
    pub fn kill(&mut self, cx: &mut Context<Self>) {
        if let Some(Session {
            link: Link::Remote { conn, term, .. },
            ..
        }) = &self.session
            && let Some(term) = term.borrow().clone()
        {
            let closing = conn.term_close(&term);
            cx.background_executor()
                .spawn(async move {
                    let _ = closing.await;
                })
                .detach();
        }
        self.session = None;
        self.restored = false;
        self.hosted = false;
        self.sending = None;
    }

    /// 丢掉现在的会话，下一帧重新开：开在 Kuai 进程里的是新的进程；kuaid 里有同名的会话就接上它
    /// （连接断了以后重新接上用的就是这个）。
    fn reopen(&mut self, cx: &mut Context<Self>) {
        self.session = None;
        self.connecting = None;
        self.error = None;
        self.exited = None;
        self.lost = false;
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
        self.exited = None;
        self.lost = false;
        let launch = match (self.launcher)(cx) {
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
                        let session = Session::attach(conn, &key, &cwd, &shell, grid, cell, cx);
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
                    }
                    (Err(err), None) => {
                        this.error = Some(tf!("无法连接服务器：{err}", err).into());
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
            Feed::Lost(reason) => {
                if !reason.is_empty() {
                    log::warn!("终端和 kuaid 断开了：{reason}");
                }
                self.lost = true;
            }
        }
        cx.notify();
    }

    fn on_event(&mut self, event: TermEvent, cx: &mut Context<Self>) {
        match event {
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
            TermEvent::Wakeup
            | TermEvent::Bell
            | TermEvent::MouseCursorDirty
            | TermEvent::CursorBlinkingChange => {}
        }
        cx.notify();
    }

    /// 往程序的输入里写。
    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        if let (Some(session), true) = (&self.session, self.is_alive()) {
            session.write(bytes.into());
        }
    }

    /// 用户敲进去的东西：先回到最底下、去掉选区，再写。
    fn input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            let mut term = session.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
        }
        self.write(bytes);
        cx.notify();
    }

    fn focus_changed(&mut self, focused: bool, cx: &mut Context<Self>) {
        self.blink_hidden = false;
        if self.mode().contains(TermMode::FOCUS_IN_OUT) {
            self.write(if focused { &b"\x1b[I"[..] } else { &b"\x1b[O"[..] });
        }
        if !focused {
            self.marked = None;
        }
        cx.notify();
    }

    fn selected_text(&self) -> Option<String> {
        let session = self.session.as_ref()?;
        let text = session.term.lock().selection_to_string()?;
        (!text.is_empty()).then_some(text)
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
        // 不用 `item.text()`：剪贴板里只有文件时它会把几个路径连在一起当文字
        let mut text = String::new();
        let mut files = Vec::new();
        let mut image = None;
        for entry in item.entries() {
            match entry {
                ClipboardEntry::String(string) => text.push_str(string.text()),
                ClipboardEntry::ExternalPaths(paths) => {
                    files.extend(paths.paths().iter().cloned().map(Attachment::File));
                }
                ClipboardEntry::Image(found) => image = image.or(Some(found)),
            }
        }
        if !text.is_empty() {
            let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
            self.input(keys::paste(&text, bracketed), cx);
            return;
        }
        if files.is_empty()
            && let Some(image) = image
        {
            match attach::image_attachment(image, chrono::Local::now()) {
                Ok(image) => files.push(image),
                Err(err) => {
                    self.flash = Some(err.into());
                    cx.notify();
                    return;
                }
            }
        }
        self.send(files, cx);
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
                    // 存图片要写文件，放到后台
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
                    let mut paths = Vec::new();
                    let mut failed: Option<SharedString> = None;
                    for (index, item) in items.into_iter().enumerate() {
                        let name = item.name();
                        let source = match item {
                            Attachment::File(path) if path.is_dir() => {
                                failed = Some(tf!("无法发送文件夹，请先压缩：{name}", name).into());
                                break;
                            }
                            Attachment::File(path) => UploadSource::File(path),
                            Attachment::Image { bytes, .. } => UploadSource::Bytes(bytes),
                        };
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
                                    tf!("{name} 上传失败：与服务器的连接已断开", name).into()
                                });
                                break;
                            }
                        }
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
        let files = paths.paths().iter().cloned().map(Attachment::File).collect();
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
        // 正打着字的时候光标不闪
        self.typed_at = Instant::now();
        if std::mem::take(&mut self.blink_hidden) {
            cx.notify();
        }
        // Ctrl+加号、Ctrl+减号调字号，Ctrl+0 回到默认（和 Windows 终端一样）；不交给程序。
        // Ctrl+Shift+减号（Ctrl+_，有的程序用它撤销）照旧交给程序
        if mods.control && !mods.alt && !mods.platform {
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
            if key == "enter" && self.connecting.is_none() {
                self.reopen(cx);
                cx.stop_propagation();
            }
            return;
        }
        let only_ctrl = mods.control && !mods.alt && !mods.platform;
        // 复制：Ctrl+Shift+C；选着字的时候 Ctrl+C 也是复制（没选字时是中断程序）
        if only_ctrl && key == "c" && (mods.shift || self.selected_text().is_some()) {
            self.copy(cx);
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
            self.input(bytes, cx);
            cx.stop_propagation();
        }
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
        let offset = session.term.lock().grid().display_offset() as i32;
        let point = GridPoint::new(GridLine(y as i32 - offset), GridColumn(x as usize));
        Some((point, side))
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
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

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
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

    /// 右键：选着字就复制，没选就粘贴（和 Windows 终端一样）。
    fn on_right_click(&mut self, _: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
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
        let cell = self.geometry.get().cell.height.max(px(1.));
        let moved = f32::from(event.delta.pixel_delta(cell).y) / f32::from(cell) + self.scroll_rest;
        let lines = moved.trunc() as i32;
        self.scroll_rest = moved - lines as f32;
        cx.stop_propagation();
        if lines == 0 {
            return;
        }
        let mode = *session.term.lock().mode();
        if mode.contains(TermMode::ALT_SCREEN) {
            // 全屏的程序（less、vim）没有可以往回翻的输出：滚轮当上下键
            let app_cursor = mode.contains(TermMode::APP_CURSOR);
            let key: &[u8] = match (lines > 0, app_cursor) {
                (true, false) => b"\x1b[A",
                (true, true) => b"\x1bOA",
                (false, false) => b"\x1b[B",
                (false, true) => b"\x1bOB",
            };
            self.write(key.repeat(lines.unsigned_abs() as usize));
        } else {
            session.term.lock().scroll_display(Scroll::Delta(lines));
        }
        cx.notify();
    }

    /// 进程没在跑（或者够不着）时，盖在画面底下的一句话。
    fn notice(&self) -> Option<SharedString> {
        if let Some(error) = &self.error {
            return Some(tf!("{error} 按 Enter 重试。", error).into());
        }
        if self.connecting.is_some() {
            return Some(match self.local {
                true => tr("正在启动终端…").into(),
                false => tr("正在连接服务器…").into(),
            });
        }
        if self.lost && self.exited.is_none() {
            return Some(match self.local {
                true => {
                    tr("与 kuaid 的连接已断开，终端中的程序仍在运行。按 Enter 重新连接。").into()
                }
                false => {
                    tr("与服务器的连接已断开，终端中的程序仍在运行。按 Enter 重新连接。").into()
                }
            });
        }
        match self.exited {
            Some(Some(0) | None) => return Some(tr("进程已结束。按回车重新启动。").into()),
            Some(Some(code)) => {
                return Some(tf!("进程已结束（退出码 {code}）。按回车重新启动。", code).into());
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
            total => format!(
                "：{} / {}",
                attach::human_size(self.sent),
                attach::human_size(total)
            ),
        };
        if self.count > 1 {
            let (index, count) = (self.index + 1, self.count);
            tf!("正在上传第 {index}/{count} 个文件 {name}{progress}。按 Esc 取消。", index, count, name, progress).into()
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (background, muted, bar) = (theme.background, theme.muted_foreground, theme.secondary);
        let (for_prepaint, for_paint) = (cx.entity(), cx.entity());
        // 右键：默认是「选着字就复制，否则粘贴」；设置里改成菜单的，弹出复制、粘贴
        let menu = prefs::get(cx).right_click == crate::settings::RightClick::Menu;
        let (has_selection, alive) = (self.selected_text().is_some(), self.is_alive());
        let view = cx.entity().downgrade();
        let panel = div()
            .id("terminal")
            .relative()
            .size_full()
            .px(px(PADDING))
            .pt(px(4.))
            .bg(background)
            .cursor_text()
            .track_focus(&self.focus)
            .key_context(KEY_CONTEXT)
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .when(!menu, |panel| {
                panel.on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_click))
            })
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
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
        if !menu {
            return panel.into_any_element();
        }
        div()
            .size_full()
            .child(panel.context_menu(move |menu, _, _| {
                let (copy, paste) = (view.clone(), view.clone());
                menu.item(
                    PopupMenuItem::new(tr("复制"))
                        .disabled(!has_selection)
                        .on_click(move |_, _, cx| {
                            copy.update(cx, |this, cx| this.copy(cx)).ok();
                        }),
                )
                .item(
                    PopupMenuItem::new(tr("粘贴"))
                        .disabled(!alive)
                        .on_click(move |_, _, cx| {
                            paste.update(cx, |this, cx| this.paste(cx)).ok();
                        }),
                )
            }))
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
    style: Style,
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
    cursor: Option<CursorDraw>,
    foreground: Hsla,
    background: Hsla,
    focused: bool,
    marked: Option<String>,
    /// 画字用的字体（不带粗体、斜体）和字号
    font: Font,
    font_size: Pixels,
}

fn prepaint(
    view: &Entity<TerminalView>,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Frame> {
    let prefs = prefs::get(cx);
    let font_size = px(prefs.font_size);
    let base_font = prefs::base_font(&prefs);
    let font_id = window.text_system().resolve_font(&base_font);
    let cell_width = window
        .text_system()
        .advance(font_id, font_size, 'm')
        .map(|advance| advance.width)
        .unwrap_or(font_size * 0.55);
    let cell = size(cell_width, px(prefs.cell_height()));
    let grid = Grid {
        cols: ((bounds.size.width / cell.width).floor() as usize).max(2),
        rows: ((bounds.size.height / cell.height).floor() as usize).max(1),
    };
    // 面板小得放不下几个字（正在收起、窗口极小）：不启动、不改大小
    if bounds.size.width < cell.width * 4. || bounds.size.height < cell.height {
        return None;
    }
    view.update(cx, |this, cx| this.fit(grid, cell, cx));

    let theme = cx.theme();
    let palette = Palette::new(theme.foreground, theme.background, theme.is_dark());
    let selection = theme.selection;
    let this = view.read(cx);
    let session = this.session.as_ref()?;
    let focused = this.focus.is_focused(window);
    let (rects, shapes, runs, cursor) = {
        let term = session.term.lock();
        this.blinking.set(focused && term.cursor_style().blinking);
        snapshot(&term, &palette, selection)
    };
    let mut frame = Frame {
        origin: bounds.origin,
        cell,
        rects,
        shapes,
        runs,
        // 进程结束了、够不着了就不画光标
        cursor: cursor.filter(|_| this.is_alive()),
        foreground: palette.foreground,
        background: palette.background,
        focused,
        marked: this.marked.clone(),
        font: base_font,
        font_size,
    };
    let cursor_bounds = frame.cursor.as_ref().map(|cursor| frame.cell_bounds(cursor.row, cursor.col, 1));
    // 闪到「灭」的那半拍不画光标；它的位置照记（输入法的候选框跟着它）
    if this.blink_hidden && focused && this.marked.is_none() {
        frame.cursor = None;
    }
    this.geometry.set(Geometry {
        origin: bounds.origin,
        cell,
        grid: Some(grid),
        cursor: cursor_bounds,
    });
    Some(frame)
}

type Snapshot = (
    Vec<(usize, usize, usize, Hsla)>,
    Vec<(usize, usize, char, Hsla)>,
    Vec<Run>,
    Option<CursorDraw>,
);

/// 把现在看得见的格子拍成要画的背景块、自己画的字、文字段和光标。
fn snapshot(term: &Term<Proxy>, palette: &Palette, selection_color: Hsla) -> Snapshot {
    let content = term.renderable_content();
    let offset = content.display_offset as i32;
    let overrides = content.colors;
    let selection = content.selection;
    let cursor_at = content.cursor.point;
    let cursor_shape = content.cursor.shape;
    let mut rects: Vec<(usize, usize, usize, Hsla)> = Vec::new();
    let mut shapes: Vec<(usize, usize, char, Hsla)> = Vec::new();
    let mut runs: Vec<Run> = Vec::new();
    let mut current: Option<Run> = None;
    let mut cursor = None;
    let flush = |current: &mut Option<Run>, runs: &mut Vec<Run>| {
        if let Some(mut run) = current.take() {
            let kept = run.text.trim_end_matches(' ').len();
            run.text.truncate(kept);
            if !run.text.is_empty() {
                runs.push(run);
            }
        }
    };
    for indexed in content.display_iter {
        let point = indexed.point;
        let row = point.line.0 + offset;
        if row < 0 {
            continue;
        }
        let (row, col) = (row as usize, point.column.0);
        let cell = indexed.cell;
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
        if point == cursor_at && cursor_shape != CursorShape::Hidden {
            cursor = Some(CursorDraw {
                row,
                col,
                wide,
                shape: cursor_shape,
                ch: cell.c,
            });
        }
        let underline = flags.intersects(Flags::ALL_UNDERLINES);
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
        let joins = current.as_ref().is_some_and(|run| {
            run.row == row
                && run.next == col
                && run.wide == wide
                && !run.single
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
                style,
            });
        }
    }
    flush(&mut current, &mut runs);
    (rects, shapes, runs, cursor)
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
        underline: style.underline.then_some(UnderlineStyle {
            thickness: px(1.),
            color: Some(style.color),
            wavy: false,
        }),
        strikethrough: style.strike.then_some(StrikethroughStyle {
            thickness: px(1.),
            color: Some(style.color),
        }),
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
        for &(row, col, ch, color) in &frame.shapes {
            glyphs::paint(ch, frame.cell_bounds(row, col, 1), color, window);
        }
        for run in &frame.runs {
            let force = match (run.single, run.wide) {
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
            let origin = frame.cell_bounds(run.row, run.col, 1).origin;
            let _ = line.paint(origin, frame.cell.height, TextAlign::Left, None, window, cx);
        }
        if let Some(cursor) = &frame.cursor {
            let cols = if cursor.wide { 2 } else { 1 };
            let area = frame.cell_bounds(cursor.row, cursor.col, cols);
            let color = frame.foreground;
            let shape = if frame.focused {
                cursor.shape
            } else {
                CursorShape::HollowBlock
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
            let origin = frame.cell_bounds(cursor.row, cursor.col, 1).origin;
            let cover = Bounds::new(origin, size(line.width, frame.cell.height));
            window.paint_quad(fill(cover, frame.background));
            let _ = line.paint(origin, frame.cell.height, TextAlign::Left, None, window, cx);
        }
    });
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：那样 `#[test]` 会变成界面库里同名的那个宏
    use super::{session_key, session_number};

    #[test]
    fn 服务器上终端的名字_第一个就是仓库路径() {
        let repo = "/data/workspaces/kuai";
        // 第一个和 1.5 用的名字一样，升上来以后接得上原来的
        assert_eq!(session_key(repo, 1), repo);
        assert_eq!(session_key(repo, 3), "/data/workspaces/kuai\u{1f}3");
        for number in [1, 2, 3, 10, 120] {
            assert_eq!(session_number(repo, &session_key(repo, number)), Some(number));
        }
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
        let repo = r"C:\Users\Feng\Repo";
        assert_eq!(session_number(repo, "c:/users/feng/repo"), Some(1));
        assert_eq!(session_number(repo, "c:/users/feng/repo\u{1f}3"), Some(3));
        assert_eq!(
            session_number("C:/Users/Feng/Repo", "c:/users/feng/repo"),
            Some(1)
        );
        assert_eq!(session_number(repo, "c:/users/feng/repo2"), None);
        // Linux 上分大小写
        assert_eq!(session_number("/Data/Repo", "/data/repo"), None);
    }
}
