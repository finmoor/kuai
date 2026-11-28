//! 终端会话：开在 kuaid 里的 shell，客户端断开、退出以后里面的程序接着跑。
//!
//! 一个会话 = 一个伪终端里的登录 shell + 记着屏幕的仿真器（[`screen`]）+ 现在接着的客户端。
//! 程序的输出喂给仿真器，同时转给接着的每个客户端；客户端接上来时先收到还原屏幕的那一段。
//! 伪终端那一层：Linux 上在 [`pty`]；Windows 上终端开在托管进程里（[`host`]），常驻进程这边是
//! [`hosted`]。这里不碰系统调用，测试里换成假的。

#[cfg(windows)]
pub mod conpty;
#[cfg(windows)]
pub mod host;
#[cfg(windows)]
pub mod hosted;
mod programs;
#[cfg(unix)]
pub mod pty;
pub mod screen;

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use fr_core::protocol::{
    Response, TERM_MARKS_SINCE, TermExit, TermInfo, TermMark, TermMarks, TermOpen, TermOpened,
};
use serde_json::json;

use crate::out::Outbox;
use screen::Screen;
pub use screen::Size;

/// 程序结束了、一直没有客户端来看的会话，留这么久就忘掉
const KEEP_ENDED: Duration = Duration::from_secs(24 * 3600);
/// 还原屏幕的那一段按这么大一块块发
const DUMP_CHUNK: usize = 64 * 1024;
/// 订阅了终端列表（`term_watch`）时，隔这么久看一次有没有变化
const WATCH_EVERY: Duration = Duration::from_secs(1);
/// 最近这么久以内有过输出的算「正在输出」
const BUSY: Duration = Duration::from_secs(3);
/// 有终端正在输出时，每看这么多次推一次列表（客户端靠它知道还在输出）
const BUSY_BEAT: u64 = 3;
/// 一直在输出时，隔这么久才核对一次屏幕上的字变没变（见 [`Activity`]）
const CHECK_EVERY: Duration = Duration::from_millis(250);
/// 改了大小以后这么久以内屏幕上的字变了不算有输出：程序按新的大小整个重画（Claude Code 会），换一台
/// 电脑往里输入、拖窗口时都会改大小
const RESIZE_GRACE: Duration = Duration::from_secs(2);
/// 屏幕上的字连续变了这么久，才算程序在干活（AI 助手干活时转圈、计时每秒都在变，见 [`Activity::work`]）
const SUSTAINED: Duration = Duration::from_secs(3);
/// 两次变化隔了这么久，后一次算新的一阵
const BURST_GAP: Duration = Duration::from_secs(10);
/// Claude Code 压缩对话时转圈写的话（2.1.332）。闲着快到提示缓存过期时它自己压缩，转好一阵
const COMPACTING: &str = "Compacting conversation";
/// AI 助手干活时写着、闲着时不写的提示：Claude Code（2.1.332）写在底栏，Codex（0.172）写在输入框上面
/// 的状态行。它一冒出来就算干活，不用等连续变了 [`SUSTAINED`]：AI 自己接着干、两三秒就干完的也认得出
/// （真的 Claude Code 后台命令跑完接着干，一阵只有一两秒）。压缩对话时底栏也写着，靠 [`COMPACTING`]
/// 排除
const INTERRUPT_HINT: &str = "esc to interrupt";
/// 在最后一行有字的往上这么多行里找 [`COMPACTING`]、[`INTERRUPT_HINT`]：转圈、Codex 的状态行在输入框
/// 和底栏上面
const SPINNER_ROWS: usize = 12;

/// 屏幕上的字什么时候变过：终端列表里的「多久没有输出了」（客户端据此算运行中、已停下）。
///
/// 程序写来的字节不都算：Claude Code 在焦点进出时擦掉、画回它自己画的光标（只改反色），改了大小
/// 以后整个重画，屏幕上的字没变。这些也算的话，闲着的 Claude Code 一切走就成了「运行中」，十秒后
/// 又成了「已停下」来提醒（闲置很久的终端图标还会亮，点开后过一会又亮）。所以看屏幕上
/// 的字（连同往回翻的记录有多少行）的摘要变没变，一直在输出时隔 [`CHECK_EVERY`] 核对一次，停下后
/// 由 [`Session::tick`] 补上最后一次。
///
/// 字变了也不都是干了活，另记在 [`Self::work`]。
///
/// 不准的情况：往回翻的记录满了以后一直打一样的行（`yes`），屏幕上的字不变，会被当成停了。
struct Activity {
    /// 屏幕上的字最近一次变了是什么时候（那块输出来的时候）。升级换程序后还没变过时是空的
    last_change: Option<Instant>,
    /// 最近一次「干活」的变化（终端列表里的「多久没干活了」，客户端拿它算 AI 终端的运行中、静默、
    /// 待查看）。一阵变化（前后两次隔不到 [`BURST_GAP`]）连续了 [`SUSTAINED`] 以上，或者这一阵里冒出了
    /// [`INTERRUPT_HINT`]，或者这一阵前后有人按过回车，这一阵里的变化才算。闲着的 Claude Code 自己会
    /// 改屏幕：检查更新时底栏闪一秒「Checking for updates…」，闲久了底栏加「new task? /clear to save …」，
    /// 闲了半个钟头左右加一段「※ recap: …」，都是一下子，不算；快到提示缓存过期时自己压缩对话，转好
    /// 一阵，靠转圈的字（[`COMPACTING`]）认出来，也不算。原来这些都算，看过的终端过一阵又成了「待查看」，
    /// 时间也从那一下重新算
    work: Option<Instant>,
    /// 眼下这一阵变化
    burst: Option<Burst>,
    /// 上一次核对时最后几行里有没有 [`INTERRUPT_HINT`]：算的是它冒出来的那一下。一直在的不算，对话里
    /// 正好写着这几个字时，闲着时的一下子变化不会因此算成干活
    hint: bool,
    /// 上一次核对时的摘要、核对的时候
    digest: u64,
    checked: Option<Instant>,
    /// 还没核对过的最近一块输出是什么时候来的
    pending: Option<Instant>,
    /// 最近一次改大小的时候
    resized: Option<Instant>,
}

/// 一阵变化（见 [`Activity::work`]）。
#[derive(Clone, Copy)]
struct Burst {
    start: Instant,
    /// 算干活了：这一阵之后的变化都记进 `work`
    counted: bool,
    /// 还没算上干活时屏幕上转着压缩对话的圈：这一阵不算（有人按过回车的除外）
    compacting: bool,
}

impl Activity {
    fn new(screen: &Screen) -> Activity {
        Activity {
            last_change: None,
            work: None,
            burst: None,
            hint: screen.shows_near_end(SPINNER_ROWS, INTERRUPT_HINT),
            digest: screen.digest(),
            checked: None,
            pending: None,
            resized: None,
        }
    }

    /// 来了一块输出（已经交给了仿真器）：离上一次核对够久了就核对。`entered` 是最后一次往里按回车
    /// 的时候（客户端发来的、定时任务自己敲的都算，见 `State::asked`）。
    fn output(&mut self, screen: &Screen, now: Instant, entered: Option<Instant>) {
        self.pending = Some(now);
        if self
            .checked
            .is_none_or(|at| now.saturating_duration_since(at) >= CHECK_EVERY)
        {
            self.check(screen, now, entered);
        }
    }

    /// 有没核对过的输出就核对：屏幕上的字变了，又不是刚改过大小，记下那块输出来的时候。
    fn check(&mut self, screen: &Screen, now: Instant, entered: Option<Instant>) {
        let Some(at) = self.pending.take() else {
            return;
        };
        let digest = screen.digest();
        if digest != self.digest {
            // 改大小以后的重画也跟着记下有没有，免得过后被当成冒出来
            let hint = screen.shows_near_end(SPINNER_ROWS, INTERRUPT_HINT);
            let appeared = hint && !self.hint;
            self.hint = hint;
            let resizing = self
                .resized
                .is_some_and(|resized| at.saturating_duration_since(resized) < RESIZE_GRACE);
            if !resizing {
                self.changed(screen, at, entered, appeared);
            }
        }
        self.digest = digest;
        self.checked = Some(now);
    }

    /// 屏幕上的字在 `at` 变了：记下，再看这一阵算不算干活（见 [`Self::work`]）。`appeared`：这一下
    /// 冒出了 [`INTERRUPT_HINT`]。
    fn changed(&mut self, screen: &Screen, at: Instant, entered: Option<Instant>, appeared: bool) {
        let fresh = self
            .last_change
            .is_none_or(|last| at.saturating_duration_since(last) >= BURST_GAP);
        self.last_change = Some(at);
        let burst = match self.burst.as_mut() {
            Some(burst) if !fresh => burst,
            _ => self.burst.insert(Burst {
                start: at,
                counted: false,
                compacting: false,
            }),
        };
        // 回车按在这一阵开始前不久、或者这一阵里：是问了它的（人问的，或者定时任务敲的命令），马上算
        let typed = entered.is_some_and(|enter| enter + BURST_GAP >= burst.start);
        if !burst.counted && !typed && screen.shows_near_end(SPINNER_ROWS, COMPACTING) {
            burst.compacting = true;
        }
        let sustained = at.saturating_duration_since(burst.start) >= SUSTAINED;
        if typed || (!burst.compacting && (appeared || sustained)) {
            burst.counted = true;
        }
        if burst.counted {
            self.work = Some(at);
        }
    }

    /// 改了大小（仿真器已经按新的大小折好了行）：折行带来的变化不算。
    fn resized(&mut self, screen: &Screen, now: Instant) {
        self.digest = screen.digest();
        self.hint = screen.shows_near_end(SPINNER_ROWS, INTERRUPT_HINT);
        self.resized = Some(now);
    }
}

/// 伪终端那一层。
pub trait Backend: Send + Sync {
    /// 会话建好、第一个客户端接上以后调用一次：开始把程序的输出交给 [`Session::output`]，
    /// 程序结束时调用 [`Session::finish`]。
    fn start(&self, session: Arc<Session>);
    /// 写给程序（用户敲的字）。不等。
    fn write(&self, bytes: Vec<u8>);
    fn resize(&self, size: Size);
    /// 结束里面的程序。
    fn kill(&self);
    /// 除了 shell 还有哪些程序在跑（见 [`programs`]）。`procs` 是这一轮共用的进程表。
    fn programs(&self, procs: &programs::Procs) -> Vec<String> {
        let _ = procs;
        Vec::new()
    }
    /// 升级换程序时带过去的两个数。Linux：伪终端的文件描述符和 shell 的进程号；
    /// Windows：托管进程听的端口和 shell 的进程号。
    fn handle(&self) -> Option<(i32, i32)> {
        None
    }
    /// 升级换程序前：不再收程序的输出，返回时已经收到的都交给了 [`Session::output`]。返回
    /// false：没确认停下（发不过去、等不到回话），不能交接。Linux 上用不着（整个进程一把锁，
    /// 见 `pty::freeze`）。
    fn pause(&self) -> bool {
        true
    }
    /// 升级没换成：接着收。
    fn resume(&self) {}
}

/// 开一个新会话要的东西。
pub struct Spawn {
    /// 会话的编号
    pub term: String,
    pub cwd: String,
    pub size: Size,
    /// 除了继承来的，shell 再多带的环境变量
    pub env: Vec<(String, String)>,
    /// 客户端点名要开的 shell（程序和参数）；空着由伪终端那一层自己定。只有 Windows 上认
    #[cfg_attr(not(windows), allow(dead_code))]
    pub shell: Vec<String>,
}

pub type Spawner = Box<dyn Fn(&Spawn) -> Result<Box<dyn Backend>> + Send + Sync>;

/// 要接上终端的是谁：哪条连接、挂在它的哪个请求号上、它握手时说的协议版本。
#[derive(Debug, Clone, Copy)]
pub struct Caller {
    pub conn: u64,
    pub request: u64,
    pub protocol: u32,
}

impl Caller {
    /// 认得消息刻度和分段还原（见 [`TERM_MARKS_SINCE`]）。
    fn current(&self) -> bool {
        self.protocol >= TERM_MARKS_SINCE
    }
}

/// 接着某个会话的一个客户端：会话的输出发到 `out`，挂在它的 `request` 号上。
struct Client {
    conn: u64,
    request: u64,
    out: Outbox,
    /// 认得消息刻度（[`Caller::current`]）：旧客户端收到会断开这个终端，不发给它
    marks: bool,
    /// 只看的（`term_attach` 带 `watch`）：不改大小、不回答程序的话，不算「有人接着」。用户在那一头
    /// 敲的字照样经 `term_input` 写进来
    watch: bool,
    /// 收得太慢，队列里积满了，程序的输出停发给它了（不能让程序等它）。积着的发得差不多了，先把它的
    /// 终端重置、发一份现在的屏幕，再接着发（见 [`catch_up`]）
    lagging: bool,
}

pub struct Session {
    pub id: String,
    /// 客户端起的名字（仓库路径）
    pub key: String,
    pub cwd: String,
    /// 什么时候开的（1970 年以来的秒数，升级换程序后还对）
    created: u64,
    backend: Box<dyn Backend>,
    state: Mutex<State>,
    registry: Weak<Inner>,
}

struct State {
    screen: Screen,
    /// 程序结束了：里面是退出码
    exit: Option<Option<i32>>,
    ended: Option<Instant>,
    /// 屏幕上的字什么时候变过
    activity: Activity,
    /// 客户端最后一次发来回车的时候（见 [`Session::typed`]）
    entered: Option<Instant>,
    /// 最后一次往里按回车的时候：客户端发来的，也算定时任务自己敲的命令、粘的提示词。这之后的输出
    /// 算干活（见 [`Activity::work`]）
    asked: Option<Instant>,
    clients: Vec<Client>,
    /// 发出去的消息，从早到晚，最多 [`fr_core::protocol::TERM_MARKS`] 条（见 [`Session::input`]）
    marks: Vec<TermMark>,
}

impl State {
    /// 有没有能回答程序、能看到结束的客户端接着（只看的不算）。
    fn attended(&self) -> bool {
        self.clients.iter().any(|client| !client.watch)
    }
}

impl Session {
    /// 程序的一块输出。
    pub fn output(&self, bytes: &[u8]) {
        let mut guard = self.state.lock().unwrap();
        let state = &mut *guard;
        let reply = state.screen.advance(bytes);
        state
            .activity
            .output(&state.screen, Instant::now(), state.asked);
        // 没人接着（只看的不算）：程序问终端的话由我们回答，不然它会一直等
        let answer = !reply.is_empty() && !state.attended();
        // 连接已经断了的不再发（断开的连接随后会整个摘掉）
        state.clients.retain(|client| !client.out.is_closed());
        for client in state.clients.iter_mut().filter(|client| !client.lagging) {
            let header = Response {
                id: client.request,
                chunk: true,
                bytes: bytes.len() as u64,
                ..Default::default()
            };
            client.lagging = !client.out.try_send(&header, bytes);
        }
        catch_up(state);
        drop(guard);
        if answer {
            self.backend.write(reply);
        }
    }

    /// 隔一会儿调用一次（没有输出的时候也要）。
    pub fn tick(&self) {
        let mut guard = self.state.lock().unwrap();
        let state = &mut *guard;
        state.screen.flush_sync();
        state
            .activity
            .check(&state.screen, Instant::now(), state.asked);
        state.clients.retain(|client| !client.out.is_closed());
        catch_up(state);
    }

    /// 程序结束了。接着的客户端收到退出码；有人看到了就不用再留着这个会话。
    pub fn finish(&self, code: Option<i32>) {
        let mut state = self.state.lock().unwrap();
        if state.exit.is_some() {
            return;
        }
        state.exit = Some(code);
        state.ended = Some(Instant::now());
        // 只看的客户端看到了不算：正经接上来的人还没见过最后的画面和退出码
        let seen = state.attended();
        // 停发了输出的客户端先补上最后的画面
        let dump = state
            .clients
            .iter()
            .any(|client| client.lagging)
            .then(|| state.screen.redump());
        for client in state.clients.drain(..) {
            if let (true, Some(dump)) = (client.lagging, &dump) {
                push_dump(&client.out, client.request, dump);
            }
            client.out.push(&exit_message(client.request, code), &[]);
        }
        drop(state);
        if seen {
            self.forget();
        }
    }

    /// 从登记表里拿掉自己。
    fn forget(&self) {
        if let Some(registry) = self.registry.upgrade() {
            let mut sessions = registry.sessions.lock().unwrap();
            // 同一个编号下已经不是自己了就不动
            if sessions
                .get(&self.id)
                .is_some_and(|other| std::ptr::eq(Arc::as_ptr(other), self))
            {
                sessions.remove(&self.id);
            }
        }
    }

    /// 接上一个客户端：先告诉它会话的编号，再把现在的屏幕还原给它（新开的会话是空的，不用），
    /// 之后的输出都发给它。程序已经结束的话，它收到最后的画面和退出码，这个会话就此忘掉。
    ///
    /// `watch` 是只看：不改会话的大小（`size` 不用），程序已经结束时也不忘掉这个会话。
    /// 旧客户端（协议 16 以前）还原的输出给一整份、不说分几段，也不发消息刻度，和原来一样。
    fn attach(&self, caller: Caller, out: &Outbox, size: Size, created: bool, watch: bool) {
        let Caller { conn, request, .. } = caller;
        let mut state = self.state.lock().unwrap();
        // 先改大小再拍屏幕：还原的是改过大小的样子
        if !watch && state.exit.is_none() && state.screen.size() != size {
            state.screen.resize(size);
            self.backend.resize(size);
        }
        let parts = match (created, caller.current()) {
            (true, _) => Vec::new(),
            (false, true) => state.screen.restore(),
            (false, false) => vec![state.screen.dump()],
        };
        let opened = TermOpened {
            term: self.id.clone(),
            created,
            restore: match caller.current() {
                true => parts.iter().map(|part| part.len() as u64).collect(),
                false => Vec::new(),
            },
        };
        let header = Response {
            id: request,
            chunk: true,
            result: Some(json!(opened)),
            ..Default::default()
        };
        out.push(&header, &[]);
        if caller.current() && !state.marks.is_empty() {
            out.push(&marks_message(request, &state.marks), &[]);
        }
        for part in &parts {
            push_dump(out, request, part);
        }
        match state.exit {
            Some(code) => {
                out.push(&exit_message(request, code), &[]);
                drop(state);
                if !watch {
                    self.forget();
                }
            }
            None => {
                // 同一个请求号重复接（不该有）时不发两份
                state
                    .clients
                    .retain(|client| !(client.conn == conn && client.request == request));
                state.clients.push(Client {
                    conn,
                    request,
                    out: out.clone(),
                    marks: caller.current(),
                    watch,
                    lagging: false,
                });
            }
        }
    }

    /// 断开一个客户端（`request` 为空时是这条连接上所有的）。程序接着跑。
    fn detach(&self, conn: u64, request: Option<u64>) {
        self.state
            .lock()
            .unwrap()
            .clients
            .retain(|client| client.conn != conn || request.is_some_and(|r| r != client.request));
    }

    /// 客户端敲进来的。只是一个回车（不是粘贴里带的、不是 Alt+回车）时，先从屏幕上的输入区读出这条
    /// 消息记下来，推给接着的客户端（终端左边的刻度），再写给程序：写过去以后程序就把输入区清掉了。
    pub fn input(&self, bytes: Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        if state.exit.is_some() {
            return;
        }
        if bytes.contains(&b'\r') {
            state.asked = Some(Instant::now());
        }
        if bytes == b"\r"
            && let Some((text, continued)) = state.screen.entered()
        {
            let at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            fr_core::term_text::record(&mut state.marks, text, continued, at);
            state.clients.retain(|client| !client.out.is_closed());
            for client in state.clients.iter().filter(|client| client.marks) {
                client
                    .out
                    .push(&marks_message(client.request, &state.marks), &[]);
            }
        }
        drop(state);
        self.backend.write(bytes);
    }

    /// 客户端发来的（`term_input`）：同 [`Self::input`]，带回车的另外记下时间。定时任务靠它分辨程序
    /// 停下以后屏幕又变了，是程序自己接着在干，还是有人接着在用（见 [`Glance::entered`]）；程序问终端
    /// 的话由客户端回答、焦点和鼠标的报告，都不带回车。
    pub fn typed(&self, bytes: Vec<u8>) {
        if bytes.contains(&b'\r') {
            self.state.lock().unwrap().entered = Some(Instant::now());
        }
        self.input(bytes);
    }

    pub fn resize(&self, size: Size) {
        let mut guard = self.state.lock().unwrap();
        let state = &mut *guard;
        if state.exit.is_none() && state.screen.size() != size {
            state.screen.resize(size);
            state.activity.resized(&state.screen, Instant::now());
            self.backend.resize(size);
        }
    }

    /// 定时任务用：程序还在不在、光标在第几列（备用屏是 None）、屏幕上的字多久没变了（没变过是
    /// None）、客户端多久以前按过回车。
    pub fn glance(&self) -> Glance {
        let state = self.state.lock().unwrap();
        Glance {
            alive: state.exit.is_none(),
            column: state.screen.cursor_column(),
            quiet: state.activity.last_change.map(|at| at.elapsed()),
            entered: state.entered.map(|at| at.elapsed()),
        }
    }

    /// 定时任务的运行记录里写的「最后一行」，`replies`：运行的是不是 AI 助手（见
    /// [`Screen::summary_text`]）。
    pub fn summary(&self, replies: bool) -> String {
        self.state.lock().unwrap().screen.summary_text(replies)
    }

    /// 现在的画面（往回翻的记录加屏幕），在空白的终端里照着输出一遍就是这个样子。
    pub fn snapshot(&self) -> Vec<u8> {
        self.state.lock().unwrap().screen.dump()
    }

    fn info(&self, procs: &programs::Procs) -> TermInfo {
        // 先问在跑的程序，再拿锁：问的时候要读进程表，别让输出等着
        let programs = self.running(procs);
        let state = self.state.lock().unwrap();
        let size = state.screen.size();
        TermInfo {
            term: self.id.clone(),
            key: self.key.clone(),
            cwd: self.cwd.clone(),
            title: state.screen.title().map(str::to_string),
            cols: size.cols,
            rows: size.rows,
            alive: state.exit.is_none(),
            attached: state.clients.iter().filter(|c| !c.watch).count(),
            age: now_secs().saturating_sub(self.created),
            idle_ms: state
                .activity
                .last_change
                .map(|at| at.elapsed().as_millis() as u64),
            work_ms: state
                .activity
                .work
                .map(|at| at.elapsed().as_millis() as u64),
            last: state.screen.last_text(),
            programs: Some(programs),
        }
    }

    fn is_alive(&self) -> bool {
        self.state.lock().unwrap().exit.is_none()
    }

    /// 除了 shell 还有哪些程序在跑；程序已经结束的是空的。Windows 上只看得到 shell 起的子进程，
    /// PowerShell 自己执行的命令（`Start-Sleep`、不出声的 `Copy-Item`）不起子进程：敲过命令、光标停在
    /// 空行的行首（新的提示符还没出来）时算有程序在跑、不知道名字（一个空串），结束前照样先问。
    fn running(&self, procs: &programs::Procs) -> Vec<String> {
        if !self.is_alive() {
            return Vec::new();
        }
        let mut names = self.backend.programs(procs);
        if cfg!(windows) && names.is_empty() {
            let state = self.state.lock().unwrap();
            if !state.marks.is_empty() && state.screen.at_blank_line() {
                names.push(String::new());
            }
        }
        names
    }
}

/// [`Session::glance`] 看到的。
pub struct Glance {
    pub alive: bool,
    pub column: Option<usize>,
    pub quiet: Option<Duration>,
    /// 客户端多久以前在这个终端里按过回车（见 [`Session::typed`]；没按过是 None）
    pub entered: Option<Duration>,
}

/// 会话的名字记成什么：Windows 上同一个目录有几种写法，统一成一种（见
/// [`fr_core::dir::windows_key`]），本机的 Kuai 和别的电脑上的 Kuai 接上的才是同一个。
pub fn session_key(key: &str) -> String {
    match cfg!(windows) {
        true => fr_core::dir::windows_key(key),
        false => key.to_string(),
    }
}

/// 停发了输出的客户端（见 [`Client::lagging`]），队列里积着的发得差不多了：先发一份现在的屏幕（把它的
/// 终端重置了再画，见 [`Screen::redump`]），之后照常发输出。
fn catch_up(state: &mut State) {
    let ready: Vec<usize> = (0..state.clients.len())
        .filter(|&at| {
            let out = &state.clients[at].out;
            state.clients[at].lagging && out.queued() <= out.resume_at()
        })
        .collect();
    if ready.is_empty() {
        return;
    }
    let dump = state.screen.redump();
    for at in ready {
        let client = &mut state.clients[at];
        push_dump(&client.out, client.request, &dump);
        client.lagging = false;
    }
}

/// 把还原屏幕的那一段按 [`DUMP_CHUNK`] 一块块放进 `out`，挂在 `request` 号上。
fn push_dump(out: &Outbox, request: u64, dump: &[u8]) {
    for part in dump.chunks(DUMP_CHUNK) {
        let header = Response {
            id: request,
            chunk: true,
            bytes: part.len() as u64,
            ..Default::default()
        };
        out.push(&header, part);
    }
}

/// 终端流里的一条：会话现在记着的全部消息。
fn marks_message(request: u64, marks: &[TermMark]) -> Response {
    Response {
        id: request,
        chunk: true,
        result: Some(json!(TermMarks {
            marks: marks.to_vec()
        })),
        ..Default::default()
    }
}

fn exit_message(request: u64, code: Option<i32>) -> Response {
    Response {
        id: request,
        result: Some(json!(TermExit { exit: code })),
        ..Default::default()
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// 升级换程序时带过去的一个会话。
pub struct Frozen {
    pub id: String,
    pub key: String,
    pub cwd: String,
    pub created: u64,
    pub size: Size,
    /// [`Backend::handle`] 给的那两个数；程序已经结束的没有
    pub handle: Option<(i32, i32)>,
    /// 程序已经结束、还没人看过：退出码
    pub exit: Option<Option<i32>>,
    /// 还原屏幕用的那一段
    pub dump: Vec<u8>,
    /// 发出去的消息
    pub marks: Vec<TermMark>,
    /// 屏幕上的字多久没变了（没变过是 None）。接过来以后接着算：不然程序刚停下就换了程序的，
    /// 要等屏幕再变一次才知道它停在什么时候，定时任务的用时会记到那时候
    pub quiet: Option<Duration>,
    /// 多久没干活了（见 `Activity::work`；没干过的、更早的程序交过来的是 None）。接过来以后接着算，
    /// 不然换了程序以后看过的又成了待查看
    pub work: Option<Duration>,
}

/// 所有的终端会话。
#[derive(Clone)]
pub struct Registry(Arc<Inner>);

struct Inner {
    sessions: Mutex<BTreeMap<String, Arc<Session>>>,
    /// 正在开的会话（见 [`Registry::open`]）；开完了叫醒 `opened` 上等着的
    opening: Mutex<Opening>,
    opened: Condvar,
    next: AtomicU64,
    spawner: Spawner,
    watchers: Mutex<Watchers>,
}

/// 正在开的会话，和升级时的「先别开了」。
#[derive(Default)]
struct Opening {
    /// 正在开的会话的名字
    keys: HashSet<String>,
    /// 正在开的有几个（没名字的也算）
    in_flight: usize,
    /// 升级要交接了，不再开新的（见 [`Registry::hold_opens`]）
    held: bool,
}

/// 开完（成没成都算）时把它从「正在开」里拿掉，叫醒等着的。
struct OpenGuard<'a> {
    inner: &'a Inner,
    key: String,
}

impl Drop for OpenGuard<'_> {
    fn drop(&mut self) {
        let mut opening = self.inner.opening.lock().unwrap();
        opening.keys.remove(&self.key);
        opening.in_flight -= 1;
        drop(opening);
        self.inner.opened.notify_all();
    }
}

/// 订阅了终端列表的客户端（`term_watch`）。
#[derive(Default)]
struct Watchers {
    list: Vec<ListWatcher>,
    /// 盯着变化的线程在跑。没人订阅了它自己退出
    running: bool,
}

struct ListWatcher {
    conn: u64,
    id: u64,
    out: Outbox,
}

/// 列表里一个终端「变没变」看的那几样。正在输出的多带一个隔几秒变一次的数，所以每隔几秒推一次。
#[derive(PartialEq)]
struct Brief {
    term: String,
    title: Option<String>,
    size: (u16, u16),
    alive: bool,
    attached: usize,
    last: String,
    known: bool,
    busy: Option<u64>,
    programs: Option<Vec<String>>,
}

fn briefs(list: &[TermInfo], beat: u64) -> Vec<Brief> {
    list.iter()
        .map(|info| Brief {
            term: info.term.clone(),
            title: info.title.clone(),
            size: (info.cols, info.rows),
            alive: info.alive,
            attached: info.attached,
            last: info.last.clone(),
            known: info.idle_ms.is_some(),
            busy: info
                .idle_ms
                .filter(|idle| *idle < BUSY.as_millis() as u64)
                .map(|_| beat / BUSY_BEAT),
            programs: info.programs.clone(),
        })
        .collect()
}

/// 推给订阅者的一条：流里的一块，`result` 是整份列表。
fn listing(id: u64, list: &[TermInfo]) -> Response {
    Response {
        id,
        chunk: true,
        result: Some(json!({ "terms": list })),
        ..Default::default()
    }
}

/// 盯着列表的线程：隔一会儿看一次，变了就推给所有订阅的。`sent` 是开始时刚推过的那份（订阅时
/// 先推了一份，一秒后不用再推一遍一样的）。
fn watch_loop(inner: Weak<Inner>, mut sent: Option<Vec<Brief>>) {
    let mut beat = 0u64;
    loop {
        std::thread::sleep(WATCH_EVERY);
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let registry = Registry(inner);
        // 先看还有没有人订阅：列表要扫一遍进程，没人看就不扫了
        {
            let mut watchers = registry.0.watchers.lock().unwrap();
            if watchers.list.is_empty() {
                watchers.running = false;
                return;
            }
        }
        let list = registry.list();
        beat += 1;
        let now = briefs(&list, beat);
        let mut watchers = registry.0.watchers.lock().unwrap();
        if watchers.list.is_empty() {
            watchers.running = false;
            return;
        }
        if sent.as_ref() != Some(&now) {
            for watcher in &watchers.list {
                watcher
                    .out
                    .push_latest(watcher.id, &listing(watcher.id, &list), &[]);
            }
            sent = Some(now);
        }
    }
}

impl Registry {
    pub fn new(spawner: Spawner) -> Registry {
        Registry(Arc::new(Inner {
            sessions: Mutex::default(),
            opening: Mutex::default(),
            opened: Condvar::new(),
            next: AtomicU64::new(1),
            spawner,
            watchers: Mutex::default(),
        }))
    }

    /// 开不了终端的登记表（测试用）。
    #[cfg(test)]
    pub fn unsupported() -> Registry {
        Registry::new(Box::new(|_| anyhow::bail!("当前系统上的 kuaid 不支持终端")))
    }

    /// `term_open`：有同名的会话就接上，没有就开一个。
    pub fn open(
        &self,
        params: &TermOpen,
        env: Vec<(String, String)>,
        caller: Caller,
        out: &Outbox,
    ) -> Result<()> {
        let size = Size::new(params.cols, params.rows);
        let key = session_key(&params.key);
        // 两个客户端同时开同一个名字只开出一个：同名的正在开就等它开完再看。起伪终端不拿着
        // 登记表的锁（Windows 上要等托管进程起来，最长 15 秒），期间列表、接上、结束别的终端照常
        let mut opening = self.0.opening.lock().unwrap();
        let _opening = loop {
            // 升级正在交接：现在开的不会被交接过去，开了也会丢
            if opening.held {
                anyhow::bail!("kuaid 正在升级，请稍后再打开终端");
            }
            let existing = self
                .0
                .sessions
                .lock()
                .unwrap()
                .values()
                .find(|session| !key.is_empty() && session.key == key)
                .cloned();
            if let Some(session) = existing {
                drop(opening);
                session.attach(caller, out, size, false, false);
                return Ok(());
            }
            if key.is_empty() || opening.keys.insert(key.clone()) {
                opening.in_flight += 1;
                drop(opening);
                break OpenGuard {
                    inner: &self.0,
                    key: key.clone(),
                };
            }
            opening = self.0.opened.wait(opening).unwrap();
        };
        let session = self.create(key, &params.cwd, size, env, params.shell.clone())?;
        // 先接上再开始读输出：第一个字也不会漏
        session.attach(caller, out, size, true, false);
        session.backend.start(session.clone());
        Ok(())
    }

    /// 起伪终端、登记一个新会话，还没开始读输出。
    fn create(
        &self,
        key: String,
        cwd: &str,
        size: Size,
        env: Vec<(String, String)>,
        shell: Vec<String>,
    ) -> Result<Arc<Session>> {
        let id = format!("t{}", self.0.next.fetch_add(1, Ordering::Relaxed));
        let backend = (self.0.spawner)(&Spawn {
            term: id.clone(),
            cwd: cwd.to_string(),
            size,
            env,
            shell,
        })
        .with_context(|| format!("无法在 {cwd} 中打开终端"))?;
        let screen = Screen::new(size);
        let session = Arc::new(Session {
            id: id.clone(),
            key,
            cwd: cwd.to_string(),
            created: now_secs(),
            backend,
            state: Mutex::new(State {
                activity: Activity::new(&screen),
                screen,
                exit: None,
                ended: None,
                entered: None,
                asked: None,
                clients: Vec::new(),
                marks: Vec::new(),
            }),
            registry: Arc::downgrade(&self.0),
        });
        self.0.sessions.lock().unwrap().insert(id, session.clone());
        Ok(session)
    }

    /// 定时任务开的会话：没有客户端接着，叫 `key`（已经有这个名字的会话就报错）。
    pub fn open_detached(
        &self,
        key: &str,
        cwd: &str,
        size: Size,
        env: Vec<(String, String)>,
        shell: Vec<String>,
    ) -> Result<Arc<Session>> {
        let key = session_key(key);
        {
            let mut opening = self.0.opening.lock().unwrap();
            if opening.held {
                anyhow::bail!("kuaid 正在升级，请稍后再打开终端");
            }
            let taken = self
                .0
                .sessions
                .lock()
                .unwrap()
                .values()
                .any(|session| session.key == key);
            if taken || !opening.keys.insert(key.clone()) {
                anyhow::bail!("已有同名的终端会话");
            }
            opening.in_flight += 1;
        }
        let _opening = OpenGuard {
            inner: &self.0,
            key: key.clone(),
        };
        let session = self.create(key, cwd, size, env, shell)?;
        session.backend.start(session.clone());
        Ok(session)
    }

    /// 已经有的会话的名字（定时任务挑没用过的序号）。
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .all()
            .iter()
            .map(|session| session.key.clone())
            .collect();
        keys.extend(self.0.opening.lock().unwrap().keys.iter().cloned());
        keys
    }

    /// `term_attach`：接上指定的会话。`watch` 是只看（见 [`Session::attach`]）。
    pub fn attach(
        &self,
        term: &str,
        size: Size,
        watch: bool,
        caller: Caller,
        out: &Outbox,
    ) -> Result<()> {
        let session = self.get(term).context("终端会话不存在（可能已结束）")?;
        session.attach(caller, out, size, false, watch);
        Ok(())
    }

    /// `term_watch`：订阅终端列表。马上推一份现在的，之后有变化再推（见 [`watch_loop`]）。
    pub fn watch(&self, conn: u64, id: u64, out: &Outbox) {
        let mut watchers = self.0.watchers.lock().unwrap();
        watchers.list.push(ListWatcher {
            conn,
            id,
            out: out.clone(),
        });
        let list = self.list();
        out.push(&listing(id, &list), &[]);
        if !watchers.running {
            watchers.running = true;
            let inner = Arc::downgrade(&self.0);
            let sent = briefs(&list, 0);
            std::thread::spawn(move || watch_loop(inner, Some(sent)));
        }
    }

    /// 不订阅了：`id` 为 None 是这个客户端的全部（断开了）。
    pub fn unwatch(&self, conn: u64, id: Option<u64>) {
        self.0
            .watchers
            .lock()
            .unwrap()
            .list
            .retain(|w| !(w.conn == conn && id.is_none_or(|id| id == w.id)));
    }

    pub fn get(&self, term: &str) -> Option<Arc<Session>> {
        self.0.sessions.lock().unwrap().get(term).cloned()
    }

    fn all(&self) -> Vec<Arc<Session>> {
        self.0.sessions.lock().unwrap().values().cloned().collect()
    }

    pub fn list(&self) -> Vec<TermInfo> {
        let procs = programs::Procs::take();
        self.all()
            .iter()
            .map(|session| session.info(&procs))
            .collect()
    }

    /// 现在有几个会话（含程序已经结束、还没人看过的）。
    pub fn count(&self) -> usize {
        self.0.sessions.lock().unwrap().len()
    }

    /// `term_programs`：会话里除了 shell 还有哪些程序在跑。程序已经结束、没有这个会话时是空的。
    pub fn programs(&self, term: &str) -> Vec<String> {
        match self.get(term) {
            Some(session) => session.running(&programs::Procs::take()),
            None => Vec::new(),
        }
    }

    /// `term_close`：结束里面的程序，忘掉这个会话。还接着的客户端会收到退出码。
    pub fn close(&self, term: &str) -> bool {
        let Some(session) = self.0.sessions.lock().unwrap().remove(term) else {
            return false;
        };
        session.backend.kill();
        true
    }

    /// 结束所有会话，等里面的程序退出，最多等 `wait`（比强杀前的宽限长一点，不理挂断的也等到
    /// 被强杀）。等的是拿出来的这些会话：原来叫停的地方先清空表、再按表里还剩几个活着的等，
    /// 表已经空了，根本没等；Linux 上过一会儿强杀靠读线程，进程先退了就没杀成。
    pub fn close_all(&self, wait: Duration) {
        let sessions = std::mem::take(&mut *self.0.sessions.lock().unwrap());
        for session in sessions.values() {
            session.backend.kill();
        }
        let deadline = Instant::now() + wait;
        while sessions.values().any(|session| session.is_alive()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 一个客户端不看了（`request` 为空：它整个断开了）。
    pub fn detach(&self, conn: u64, request: Option<u64>) {
        for session in self.all() {
            session.detach(conn, request);
        }
    }

    /// 程序结束很久、一直没人来看的会话，忘掉。隔一会儿调用一次。
    pub fn sweep(&self) {
        self.0.sessions.lock().unwrap().retain(|_, session| {
            let state = session.state.lock().unwrap();
            state.ended.is_none_or(|at| at.elapsed() < KEEP_ENDED)
        });
    }

    /// 升级换程序前：让每个还在跑的会话停止收输出（见 [`Backend::pause`]）。
    #[cfg_attr(not(windows), allow(dead_code))]
    /// 有一个没确认停下就把停了的都恢复，返回 false：原来不管确认没确认都接着交接，已经从
    /// 伪终端读出来、还没进旧进程屏幕的输出可能就丢了。
    pub fn pause_all(&self) -> bool {
        let mut paused: Vec<Arc<Session>> = Vec::new();
        for session in self.all() {
            if !session.is_alive() {
                continue;
            }
            if !session.backend.pause() {
                session.backend.resume();
                for session in &paused {
                    session.backend.resume();
                }
                return false;
            }
            paused.push(session);
        }
        true
    }

    /// 升级没换成：接着收。
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn resume_all(&self) {
        for session in self.all() {
            if session.is_alive() {
                session.backend.resume();
            }
        }
    }

    /// 升级交接前：不再开新的终端，等正在开的开完再返回。之后开的会被拒绝，不会有开好了却没
    /// 交接过去的（原来升级只挡住再升级，冻结之后照样开得出新终端，客户端收到「开好了」，
    /// 换了程序以后它就没了）。
    pub fn hold_opens(&self) {
        let mut opening = self.0.opening.lock().unwrap();
        opening.held = true;
        while opening.in_flight > 0 {
            opening = self.0.opened.wait(opening).unwrap();
        }
    }

    /// 升级没成：又可以开终端了。
    pub fn release_opens(&self) {
        self.0.opening.lock().unwrap().held = false;
    }

    /// 升级换程序前：把每个会话拍下来。调用前要先让伪终端那一层停止读输出。
    pub fn freeze(&self) -> (u64, Vec<Frozen>) {
        let frozen = self
            .all()
            .iter()
            .map(|session| {
                let mut state = session.state.lock().unwrap();
                Frozen {
                    id: session.id.clone(),
                    key: session.key.clone(),
                    cwd: session.cwd.clone(),
                    created: session.created,
                    size: state.screen.size(),
                    handle: match state.exit {
                        None => session.backend.handle(),
                        Some(_) => None,
                    },
                    exit: state.exit,
                    dump: state.screen.dump(),
                    marks: state.marks.clone(),
                    quiet: state.activity.last_change.map(|at| at.elapsed()),
                    work: state.activity.work.map(|at| at.elapsed()),
                }
            })
            .collect();
        (self.0.next.load(Ordering::Relaxed), frozen)
    }

    /// 升级换程序后：把带过来的会话接着管起来。`backend` 是用带过来的文件描述符重建的那一层，
    /// 程序已经结束的会话给 [`Ended`]。
    pub fn adopt(&self, frozen: Frozen, backend: Box<dyn Backend>) {
        let mut screen = Screen::new(frozen.size);
        screen.advance(&frozen.dump);
        let alive = frozen.exit.is_none();
        let mut activity = Activity::new(&screen);
        let ago = |past: Duration| Instant::now().checked_sub(past);
        activity.last_change = frozen.quiet.and_then(ago);
        // 更早的程序交过来的不知道多久没干活：当作最后一次变化就是，和那一版算的一样
        activity.work = frozen.work.or(frozen.quiet).and_then(ago);
        let session = Arc::new(Session {
            id: frozen.id.clone(),
            key: frozen.key,
            cwd: frozen.cwd,
            created: frozen.created,
            backend,
            state: Mutex::new(State {
                activity,
                screen,
                exit: frozen.exit,
                ended: frozen.exit.map(|_| Instant::now()),
                entered: None,
                asked: None,
                clients: Vec::new(),
                marks: frozen.marks,
            }),
            registry: Arc::downgrade(&self.0),
        });
        self.0
            .sessions
            .lock()
            .unwrap()
            .insert(frozen.id, session.clone());
        if alive {
            session.backend.start(session.clone());
        }
    }

    /// 升级换程序后接着编号。
    pub fn set_next(&self, next: u64) {
        self.0.next.fetch_max(next, Ordering::Relaxed);
    }
}

/// 程序已经结束的会话用的那一层：什么都不做。
pub struct Ended;

impl Backend for Ended {
    fn start(&self, _: Arc<Session>) {}
    fn write(&self, _: Vec<u8>) {}
    fn resize(&self, _: Size) {}
    fn kill(&self) {}
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use fr_core::protocol;
    use std::io::{BufReader, PipeReader};

    /// 假的伪终端：记下写给它的东西，输出和结束由测试自己喂。
    #[derive(Default)]
    pub(crate) struct Fake {
        pub written: Mutex<Vec<u8>>,
        pub size: Mutex<Option<Size>>,
        pub killed: Mutex<bool>,
        pub session: Mutex<Option<Arc<Session>>>,
        /// 假装在跑的程序
        pub programs: Mutex<Vec<String>>,
    }

    impl Backend for Arc<Fake> {
        fn start(&self, session: Arc<Session>) {
            *self.session.lock().unwrap() = Some(session);
        }

        fn write(&self, bytes: Vec<u8>) {
            self.written.lock().unwrap().extend_from_slice(&bytes);
        }

        fn resize(&self, size: Size) {
            *self.size.lock().unwrap() = Some(size);
        }

        fn kill(&self) {
            *self.killed.lock().unwrap() = true;
        }

        fn programs(&self, _: &programs::Procs) -> Vec<String> {
            self.programs.lock().unwrap().clone()
        }

        fn handle(&self) -> Option<(i32, i32)> {
            Some((7, 4242))
        }
    }

    /// 开出来的假终端都记在返回的列表里。
    pub(crate) fn fake_registry() -> (Registry, Arc<Mutex<Vec<Arc<Fake>>>>) {
        let fakes: Arc<Mutex<Vec<Arc<Fake>>>> = Arc::default();
        let made = fakes.clone();
        let registry = Registry::new(Box::new(move |_| {
            let fake = Arc::new(Fake::default());
            made.lock().unwrap().push(fake.clone());
            Ok(Box::new(fake) as Box<dyn Backend>)
        }));
        (registry, fakes)
    }

    /// 一个客户端的收件箱：读发给它的消息。
    pub(crate) struct Inbox(BufReader<PipeReader>);

    pub(crate) fn mailbox() -> (Outbox, Inbox) {
        let (reader, writer) = std::io::pipe().unwrap();
        (Outbox::new(writer), Inbox(BufReader::new(reader)))
    }

    impl Inbox {
        pub(crate) fn next(&mut self) -> (Response, Vec<u8>) {
            protocol::read_message::<_, Response>(&mut self.0)
                .unwrap()
                .expect("连接关了")
        }

        /// 流的第一条：会话的编号、是不是新开的。
        pub(crate) fn opened(&mut self) -> TermOpened {
            let (response, _) = self.next();
            assert!(response.chunk);
            serde_json::from_value(response.result.unwrap()).unwrap()
        }
    }

    /// 现在这一版的客户端。
    fn caller(conn: u64, request: u64) -> Caller {
        Caller {
            conn,
            request,
            protocol: protocol::PROTOCOL_VERSION,
        }
    }

    fn open(registry: &Registry, key: &str, conn: u64, request: u64, out: &Outbox) {
        let params = TermOpen {
            key: key.into(),
            cwd: "/repo".into(),
            cols: 20,
            rows: 4,
            shell: Vec::new(),
        };
        registry
            .open(&params, Vec::new(), caller(conn, request), out)
            .unwrap();
    }

    /// 把收到的输出在一个空白的终端里画出来，取第一行的字。
    fn first_line(bytes: &[u8]) -> String {
        let mut screen = Screen::new(Size::new(20, 4));
        screen.advance(bytes);
        screen.line_text(0)
    }

    #[test]
    fn 里面在跑的程序_结束以后和没有的会话都是空的() {
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 10, &out);
        let term = inbox.opened().term;
        let fake = fakes.lock().unwrap()[0].clone();
        assert!(registry.programs(&term).is_empty(), "shell 闲着");
        *fake.programs.lock().unwrap() = vec!["claude".into()];
        assert_eq!(registry.programs(&term), ["claude"]);
        // 程序结束了：会话还在（等人来看），但没有在跑的了
        registry.detach(1, None);
        let session = fake.session.lock().unwrap().clone().unwrap();
        session.finish(Some(0));
        assert_eq!(registry.list().len(), 1);
        assert!(registry.programs(&term).is_empty());
        assert!(registry.programs("no-such-term").is_empty());
    }

    #[test]
    fn 没有子进程_敲过命令还没出提示符_windows_上算在跑() {
        let (registry, _fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 10, &out);
        let term = inbox.opened().term;
        let session = registry.get(&term).unwrap();
        // 刚开、提示符还没出来：没敲过命令，不算
        assert!(registry.programs(&term).is_empty());
        session.output(br"PS C:\> Start-Sleep 15");
        session.input(b"\r".to_vec());
        session.output(b"\r\n");
        // PowerShell 自己执行的命令不起子进程，只有 Windows 上这样看
        let busy: &[&str] = match cfg!(windows) {
            true => &[""],
            false => &[],
        };
        assert_eq!(registry.programs(&term), busy);
        assert_eq!(registry.list()[0].programs.as_deref().unwrap(), busy);
        session.output(br"PS C:\> ");
        assert!(registry.programs(&term).is_empty(), "提示符出来了");
    }

    #[test]
    fn 输出转给接着的客户端_断开以后程序接着跑_再接上能还原() {
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 10, &out);
        let opened = inbox.opened();
        assert!(opened.created);
        let fake = fakes.lock().unwrap()[0].clone();
        let session = fake
            .session
            .lock()
            .unwrap()
            .clone()
            .expect("没有开始读输出");

        session.output(b"hello");
        let (response, payload) = inbox.next();
        assert_eq!((response.id, response.chunk), (10, true));
        assert_eq!(payload, b"hello");

        // 断开（客户端退出了）：程序没被杀，之后的输出记在屏幕里
        registry.detach(1, None);
        session.output(b" world");
        assert!(!*fake.killed.lock().unwrap());
        assert_eq!(registry.list()[0].attached, 0);

        // 另一条连接用同一个名字再开：接上的是原来那个，先收到还原出来的屏幕
        let (out2, mut inbox2) = mailbox();
        open(&registry, "/repo", 2, 5, &out2);
        let again = inbox2.opened();
        assert_eq!((again.term, again.created), (opened.term.clone(), false));
        assert_eq!(fakes.lock().unwrap().len(), 1, "不应该再开一个");
        let (_, dump) = inbox2.next();
        assert_eq!(first_line(&dump), "hello world");
        session.output(b"!");
        assert_eq!(inbox2.next().1, b"!");

        // 敲的字、改的大小交给伪终端
        registry.get(&opened.term).unwrap().input(b"ls\r".to_vec());
        assert_eq!(*fake.written.lock().unwrap(), b"ls\r");
        registry.get(&opened.term).unwrap().resize(Size::new(30, 8));
        assert_eq!(*fake.size.lock().unwrap(), Some(Size::new(30, 8)));
    }

    #[test]
    fn 回车时记下输入区里的消息_推给接着的_再接上时先收到() {
        let marks_of = |response: Response| -> Vec<String> {
            let marks: TermMarks = serde_json::from_value(response.result.unwrap()).unwrap();
            marks.marks.into_iter().map(|mark| mark.text).collect()
        };
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 10, &out);
        let term = inbox.opened().term;
        let fake = fakes.lock().unwrap()[0].clone();
        let session = registry.get(&term).unwrap();
        session.output(br"PS C:\> git status");
        inbox.next();
        // 只是一个回车：记下，推给客户端，再写给程序
        session.input(b"\r".to_vec());
        let (response, _) = inbox.next();
        assert_eq!((response.id, response.chunk), (10, true));
        assert_eq!(marks_of(response), ["git status"]);
        assert_eq!(*fake.written.lock().unwrap(), b"\r");
        // 粘贴里带的回车、空着的提示符按回车：不记
        session.output(b"\r\nPS C:\\> ");
        inbox.next();
        session.input(b"ls\r".to_vec());
        session.input(b"\r".to_vec());
        // 全屏程序里按回车：不记
        session.output(b"\x1b[?1049hPS C:\\> vim");
        inbox.next();
        session.input(b"\r".to_vec());
        session.output(b"\x1b[?1049l");
        inbox.next();
        assert_eq!(*fake.written.lock().unwrap(), b"\rls\r\r\r");
        // 再接上：会话的编号后面紧跟着记着的消息
        registry.detach(1, None);
        let (out2, mut inbox2) = mailbox();
        open(&registry, "/repo", 2, 5, &out2);
        inbox2.opened();
        assert_eq!(marks_of(inbox2.next().0), ["git status"]);
        // 最多留 TERM_MARKS 条，丢掉最早的
        for n in 0..protocol::TERM_MARKS {
            session.output(format!("\r\nPS C:\\> echo {n}").as_bytes());
            session.input(b"\r".to_vec());
        }
        let kept = session.state.lock().unwrap().marks.clone();
        assert_eq!(kept.len(), protocol::TERM_MARKS);
        assert_eq!(kept[0].text, "echo 0");
        assert!(kept.windows(2).all(|pair| pair[0].at <= pair[1].at));
    }

    /// 1.18.0（协议 15）的客户端把终端流里带结果的消息都当成「开好了」，收到消息刻度就报格式错误、
    /// 断开这个终端：另一台电脑上的 Kuai 更新了、换上了新的 kuaid 时，没更新的那台就会这样。所以不给
    /// 它发刻度，还原的输出也照原来给一整份；同时接着的新客户端照常收到刻度。
    #[test]
    fn 旧客户端_不发消息刻度_还原给一整份() {
        let marks_of = |response: Response| -> Vec<String> {
            let marks: TermMarks = serde_json::from_value(response.result.unwrap()).unwrap();
            marks.marks.into_iter().map(|mark| mark.text).collect()
        };
        let (registry, _) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 10, &out);
        let term = inbox.opened().term;
        let session = registry.get(&term).unwrap();
        // 往回翻的记录里有字：新客户端接上时会分两段
        for n in 0..8 {
            session.output(format!("line {n}\r\n").as_bytes());
            inbox.next();
        }
        session.output(br"PS C:\> git status");
        inbox.next();
        session.input(b"\r".to_vec());
        assert_eq!(marks_of(inbox.next().0), ["git status"]);

        // 旧客户端接上：编号后面直接是还原的输出，一整份，和原来的 `dump` 一样
        let (old_out, mut old_inbox) = mailbox();
        let old = Caller {
            conn: 2,
            request: 5,
            protocol: TERM_MARKS_SINCE - 1,
        };
        registry
            .attach(&term, Size::new(20, 4), false, old, &old_out)
            .unwrap();
        let opened = old_inbox.opened();
        assert!(opened.restore.is_empty(), "旧客户端不认得 restore");
        let (response, dump) = old_inbox.next();
        assert!(response.result.is_none(), "不该给旧客户端发消息刻度");
        assert_eq!(dump, session.state.lock().unwrap().screen.dump());
        // 新客户端同时接上：分两段
        let (new_out, mut new_inbox) = mailbox();
        registry
            .attach(&term, Size::new(20, 4), false, caller(3, 6), &new_out)
            .unwrap();
        assert_eq!(new_inbox.opened().restore.len(), 2);

        // 再记一条：新的两个客户端收到刻度，旧客户端只收到输出
        session.output(b"\r\nPS C:\\> ls");
        inbox.next();
        old_inbox.next();
        assert_eq!(marks_of(new_inbox.next().0), ["git status"]);
        new_inbox.next();
        new_inbox.next();
        new_inbox.next();
        session.input(b"\r".to_vec());
        assert_eq!(marks_of(inbox.next().0), ["git status", "ls"]);
        session.output(b"!");
        let (response, payload) = old_inbox.next();
        assert_eq!((response.result, payload), (None, b"!".to_vec()));
    }

    #[test]
    fn 接上时大小不一样就先改大小() {
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 1, &out);
        let term = inbox.opened().term;
        registry.detach(1, None);
        let (out2, mut inbox2) = mailbox();
        registry
            .attach(&term, Size::new(50, 10), false, caller(2, 1), &out2)
            .unwrap();
        inbox2.opened();
        let fake = fakes.lock().unwrap()[0].clone();
        assert_eq!(*fake.size.lock().unwrap(), Some(Size::new(50, 10)));
        assert_eq!((registry.list()[0].cols, registry.list()[0].rows), (50, 10));
    }

    #[test]
    fn 程序结束_有人看着就忘掉_没人看着先留着() {
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "a", 1, 1, &out);
        inbox.opened();
        let session = fakes.lock().unwrap()[0]
            .session
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        session.finish(Some(3));
        let (response, _) = inbox.next();
        assert!(!response.chunk);
        let exit: TermExit = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(exit.exit, Some(3));
        assert_eq!(registry.count(), 0);

        // 没人接着的时候结束：留着，下一个来看的人收到最后的画面和退出码，然后才忘掉
        open(&registry, "b", 1, 2, &out);
        inbox.opened();
        let session = fakes.lock().unwrap()[1]
            .session
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        registry.detach(1, Some(2));
        session.output(b"bye");
        session.finish(None);
        assert_eq!(registry.count(), 1);
        assert!(!registry.list()[0].alive);
        open(&registry, "b", 1, 3, &out);
        assert!(!inbox.opened().created);
        assert_eq!(first_line(&inbox.next().1), "bye");
        let (response, _) = inbox.next();
        let exit: TermExit = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(exit.exit, None);
        assert_eq!(registry.count(), 0);
        // 同一个名字再开，是新的
        open(&registry, "b", 1, 4, &out);
        assert!(inbox.opened().created);
    }

    #[test]
    fn 没人接着时替终端回答程序的提问() {
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 1, &out);
        inbox.opened();
        let fake = fakes.lock().unwrap()[0].clone();
        let session = fake.session.lock().unwrap().clone().unwrap();
        // 有客户端接着：由客户端的终端回答，这里不回
        session.output(b"\x1b[6n");
        assert!(fake.written.lock().unwrap().is_empty());
        registry.detach(1, None);
        session.output(b"\x1b[6n");
        assert_eq!(*fake.written.lock().unwrap(), b"\x1b[1;1R");
    }

    #[test]
    fn 结束会话() {
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 1, &out);
        let term = inbox.opened().term;
        assert!(registry.close(&term));
        assert!(*fakes.lock().unwrap()[0].killed.lock().unwrap());
        assert_eq!(registry.count(), 0);
        assert!(!registry.close(&term));
        // 程序真的退出时，还接着的客户端收到退出码
        let session = fakes.lock().unwrap()[0]
            .session
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        session.finish(None);
        assert!(!inbox.next().0.chunk);
    }

    #[test]
    fn 程序一直刷屏时客户端一直收得到_停发了也会接着发() {
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 1, &out);
        let session = fakes.lock().unwrap()[0]
            .session
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        // 客户端一直在收，记下每一刻收到了多少
        let received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counting = received.clone();
        std::thread::spawn(move || {
            inbox.opened();
            loop {
                let (_, payload) = inbox.next();
                counting.fetch_add(payload.len(), std::sync::atomic::Ordering::Relaxed);
            }
        });
        // 程序刷 3 秒屏，比管道收得快
        let block = b"flood line flood line flood line\r\n".repeat(2000);
        let started = Instant::now();
        let mut halfway = 0;
        while started.elapsed() < Duration::from_secs(3) {
            session.output(&block);
            session.tick();
            if halfway == 0 && started.elapsed() > Duration::from_millis(1500) {
                halfway = received.load(std::sync::atomic::Ordering::Relaxed);
            }
        }
        let late = received.load(std::sync::atomic::Ordering::Relaxed) - halfway;
        assert!(
            late > 1 << 20,
            "后一半时间只收到 {late} 字节，停发以后没接着发"
        );
    }

    #[test]
    fn 客户端收得太慢就停发_积着的收完以后从现在的屏幕接着() {
        let (registry, fakes) = fake_registry();
        // 这个客户端的管道先没人读
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 1, &out);
        let session = fakes.lock().unwrap()[0]
            .session
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        let block = vec![b'x'; 1 << 20];
        for _ in 0..64 {
            session.output(&block);
        }
        // 程序没被卡住（上面的循环跑完了），那个客户端还接着，只是停发了
        assert_eq!(registry.list()[0].attached, 1);
        assert!(out.queued() < 16 << 20, "停发以后还在往里放");
        session.output(b"\x1b[2J\x1b[Hfirst\r\nsecond");
        // 客户端开始收：先是积着的，然后是重置加现在的屏幕，之后的输出照常
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            inbox.opened();
            let (mut screen, mut skipped) = (Screen::new(Size::new(20, 4)), 0);
            loop {
                let (response, payload) = inbox.next();
                assert!(response.chunk && response.error.is_none());
                // 停发前积着的那些不用画：后面先重置
                if payload.iter().all(|&byte| byte == b'x') {
                    skipped += payload.len();
                    continue;
                }
                screen.advance(&payload);
                if payload.ends_with(b"!") {
                    let _ = done.send((screen.line_text(0), screen.line_text(1), skipped));
                    return;
                }
            }
        });
        let started = Instant::now();
        while out.queued() > out.resume_at() {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "积着的一直发不出去"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        session.tick();
        session.output(b"!");
        let (first, second, skipped) = result
            .recv_timeout(Duration::from_secs(60))
            .expect("没收到重置以后的输出");
        assert_eq!((first.as_str(), second.as_str()), ("first", "second!"));
        // 停发以后的没发：收到的只有停发前积着的那些
        assert!(skipped < 16 << 20, "{skipped}");
        assert_eq!(registry.list()[0].attached, 1);
    }

    #[test]
    fn 停发了的客户端在程序结束时先补上最后的画面() {
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 1, &out);
        let session = fakes.lock().unwrap()[0]
            .session
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        let block = vec![b'x'; 1 << 20];
        for _ in 0..64 {
            session.output(&block);
        }
        session.output(b"\x1b[2J\x1b[Hbye");
        session.finish(Some(0));
        inbox.opened();
        let mut screen = Screen::new(Size::new(20, 4));
        loop {
            let (response, payload) = inbox.next();
            if !response.chunk {
                let exit: TermExit = serde_json::from_value(response.result.unwrap()).unwrap();
                assert_eq!(exit.exit, Some(0));
                break;
            }
            if !payload.iter().all(|&byte| byte == b'x') {
                screen.advance(&payload);
            }
        }
        assert_eq!(screen.line_text(0), "bye");
    }

    #[test]
    fn 升级时拍下来再接着管() {
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 1, &out);
        let term = inbox.opened().term;
        let session = fakes.lock().unwrap()[0]
            .session
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        session.output(b"\x1b[32mbefore upgrade\x1b[m");
        let (next, mut frozen) = registry.freeze();
        assert_eq!(frozen.len(), 1);
        assert_eq!(frozen[0].handle, Some((7, 4242)));
        assert!(frozen[0].quiet.is_some(), "屏幕变过：带上多久没变了");
        assert_eq!(frozen[0].work, None, "没人按回车的一下子不算干活");
        frozen[0].quiet = Some(Duration::from_secs(30));

        // 新程序里：按拍下来的重建
        let (fresh, _) = fake_registry();
        fresh.set_next(next);
        let fake = Arc::new(Fake::default());
        fresh.adopt(frozen.remove(0), Box::new(fake.clone()));
        assert!(fake.session.lock().unwrap().is_some(), "没有接着读输出");
        // 屏幕多久没变接着算（定时任务的用时靠它）
        let quiet = fresh.get(&term).unwrap().glance().quiet.unwrap();
        assert!((30..40).contains(&quiet.as_secs()), "{quiet:?}");
        // 没带多久没干活（更早的程序交过来的就这样）：当作最后一次变化就是，和那一版算的一样
        let work = fresh.list()[0].work_ms.unwrap();
        assert!((30_000..40_000).contains(&work), "{work}");
        let (out2, mut inbox2) = mailbox();
        open(&fresh, "/repo", 9, 1, &out2);
        let again = inbox2.opened();
        assert_eq!((again.term, again.created), (term, false));
        assert_eq!(first_line(&inbox2.next().1), "before upgrade");
        // 新开的会话编号接着往下排，不和带过来的重
        open(&fresh, "/other", 9, 2, &out2);
        assert_eq!(inbox2.opened().term, format!("t{next}"));
    }

    #[test]
    fn 升级交接时不开新终端_没成接着开() {
        let (registry, _) = fake_registry();
        let (out, mut inbox) = mailbox();
        registry.hold_opens();
        let params = TermOpen {
            key: "/repo".into(),
            cwd: "/repo".into(),
            cols: 20,
            rows: 4,
            shell: Vec::new(),
        };
        let refused = registry.open(&params, Vec::new(), caller(1, 1), &out);
        assert!(refused.is_err());
        assert!(registry.list().is_empty());
        registry.release_opens();
        open(&registry, "/repo", 1, 2, &out);
        assert!(inbox.opened().created);
    }

    #[test]
    fn 只看的客户端不改大小_不算接着的() {
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 1, &out);
        let term = inbox.opened().term;
        let fake = fakes.lock().unwrap()[0].clone();
        let session = fake.session.lock().unwrap().clone().unwrap();
        session.output(b"hello");
        inbox.next();

        // 只看：大小给得不一样也不改，先收到现在的屏幕，之后的输出照收
        let (watch_out, mut watcher) = mailbox();
        registry
            .attach(&term, Size::new(50, 10), true, caller(2, 7), &watch_out)
            .unwrap();
        assert!(!watcher.opened().created);
        assert_eq!(*fake.size.lock().unwrap(), None);
        assert_eq!((registry.list()[0].cols, registry.list()[0].rows), (20, 4));
        assert_eq!(first_line(&watcher.next().1), "hello");
        session.output(b"!");
        assert_eq!(watcher.next().1, b"!");
        assert_eq!(registry.list()[0].attached, 1, "只看的不算接着的");

        // 正经接着的那个走了，只剩只看的：程序问终端的话还是由 kuaid 回答
        registry.detach(1, None);
        assert_eq!(registry.list()[0].attached, 0);
        session.output(b"\x1b[6n");
        assert_eq!(*fake.written.lock().unwrap(), b"\x1b[1;7R");

        // 程序结束：只看的收到退出码，会话还留着等正经接上来的人
        session.finish(Some(0));
        watcher.next();
        let (response, _) = watcher.next();
        assert!(!response.chunk);
        assert_eq!(registry.count(), 1);
        // 结束以后再来只看：收到最后的画面和退出码，会话仍然留着
        let (late_out, mut late) = mailbox();
        registry
            .attach(&term, Size::new(20, 4), true, caller(3, 1), &late_out)
            .unwrap();
        late.opened();
        assert_eq!(first_line(&late.next().1), "hello!");
        assert!(!late.next().0.chunk);
        assert_eq!(registry.count(), 1);
    }

    /// 订阅的流里的一条：整份列表。
    fn listed(inbox: &mut Inbox) -> Vec<TermInfo> {
        let (response, _) = inbox.next();
        assert!(response.chunk);
        serde_json::from_value(response.result.unwrap()["terms"].clone()).unwrap()
    }

    #[test]
    fn 屏幕上的字没变的输出不算_改大小后的重画不算() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut screen = Screen::new(Size::new(40, 5));
        let mut activity = Activity::new(&screen);
        // 提示符出来了：算
        screen.advance(b"PS C:\\> ");
        activity.output(&screen, ms(0), None);
        assert_eq!(activity.last_change, Some(ms(0)));
        // Claude Code 2.1.330 焦点出去时写的（在测试终端里抓的）：擦掉自己画的反色光标
        screen.advance(b"\x1b[7m \x1b[27m\x08\x1b[K\x1b[8C");
        activity.output(&screen, ms(1000), None);
        assert_eq!(activity.last_change, Some(ms(0)));
        // 一直在输出：隔一会儿才核对，没核对的那块由 tick 补上，记的是它来的时候
        screen.advance(b"a");
        activity.output(&screen, ms(1100), None);
        assert_eq!(activity.last_change, Some(ms(0)));
        activity.check(&screen, ms(1600), None);
        assert_eq!(activity.last_change, Some(ms(1100)));
        // 改了大小以后程序整个重画：不算
        screen.resize(Size::new(30, 5));
        activity.resized(&screen, ms(5000));
        screen.advance(b"\r\x1b[Kredrawn");
        activity.output(&screen, ms(5500), None);
        assert_eq!(activity.last_change, Some(ms(1100)));
        // 过了那一会儿再变：算
        screen.advance(b"!");
        activity.output(&screen, ms(8000), None);
        assert_eq!(activity.last_change, Some(ms(8000)));
    }

    #[test]
    fn 终端列表带多久没干活_按过回车之后的输出算() {
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 1, &out);
        inbox.opened();
        let session = fakes.lock().unwrap()[0]
            .session
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        // 程序自己冒出来一行：屏幕变了，没干活
        session.output(b"update available");
        let list = registry.list();
        assert!(list[0].idle_ms.is_some());
        assert_eq!(list[0].work_ms, None);
        // 客户端发来回车，程序答了：算
        session.typed(b"hi\r".to_vec());
        session.output(b"\r\nhello");
        session.tick();
        assert!(registry.list()[0].work_ms.is_some_and(|ms| ms < 5_000));
    }

    #[test]
    fn 干活才记_一下子的变化和自己压缩对话不算() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        // 屏幕比内容高：对话短的时候 Claude Code 画在屏幕上半截，下面是空的（只看屏幕最底下几行时，
        // 认不出压缩对话的转圈，在测试实例里录到过）
        let mut screen = Screen::new(Size::new(60, 30));
        let mut activity = Activity::new(&screen);
        // 闲着的 Claude Code 2.1.332 检查更新：底栏闪一秒「Checking for updates…」又擦掉（在真的
        // 会话里录到的）。字变了两次，连起来不到一阵，不算干活
        screen.advance(b"\x1b[8;30HChecking for updates\xe2\x80\xa6");
        activity.output(&screen, ms(0), None);
        screen.advance(b"\x1b[8;30H\x1b[K");
        activity.output(&screen, ms(1000), None);
        assert_eq!(activity.last_change, Some(ms(1000)));
        assert_eq!(activity.work, None);
        // 自己接着干活（后台任务做完了接着写）：转圈的计时每秒在变，连着三秒才算，之后每次都记
        for n in 0..5u64 {
            screen.advance(format!("\x1b[6;1H* Working ({n}s)").as_bytes());
            activity.output(&screen, ms(20_000 + n * 1000), None);
            let counted = n >= 3;
            assert_eq!(activity.work.is_some(), counted, "第 {n} 秒");
        }
        assert_eq!(activity.work, Some(ms(24_000)));
        // 停了一阵以后又是一下子（闲久了底栏加的提示）：不算
        screen.advance(b"\x1b[8;20Hnew task? /clear to save 148.9k tokens");
        activity.output(&screen, ms(60_000), None);
        assert_eq!(activity.last_change, Some(ms(60_000)));
        assert_eq!(activity.work, Some(ms(24_000)));
        // 闲着快到缓存过期时自己压缩对话：转好一阵，转圈写着 Compacting conversation，也不算
        for n in 0..10u64 {
            let spinner = format!("\x1b[6;1H* Compacting conversation\u{2026} ({n}s)");
            screen.advance(spinner.as_bytes());
            activity.output(&screen, ms(120_000 + n * 1000), None);
        }
        screen.advance(
            b"\x1b[6;1H\x1b[K\x1b[5;1HCompacted while idle, before the prompt cache expired",
        );
        activity.output(&screen, ms(130_000), None);
        assert_eq!(activity.last_change, Some(ms(130_000)));
        assert_eq!(activity.work, Some(ms(24_000)));
        // 有人按了回车（客户端发来的）：一下子就答完的也算
        screen.advance(b"\x1b[7;1H> hi\x1b[6;1HHello!");
        activity.output(&screen, ms(200_500), Some(ms(200_000)));
        assert_eq!(activity.work, Some(ms(200_500)));
        // 有人敲 /compact：是人要的压缩，算
        for n in 0..2u64 {
            let spinner = format!("\x1b[6;1H* Compacting conversation\u{2026} ({n}s)");
            screen.advance(spinner.as_bytes());
            activity.output(&screen, ms(300_000 + n * 1000), Some(ms(299_000)));
        }
        assert_eq!(activity.work, Some(ms(301_000)));
    }

    #[test]
    fn 冒出_esc_to_interrupt_马上算干活_压缩时和一直在的不算() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        // Claude Code 2.1.332 的输入框和底栏（对话短，画在屏幕上半截）；干活时底栏多了 esc to interrupt
        let idle = "\x1b[10;3H\u{23f5}\u{23f5} auto mode on (shift+tab to cycle)\x1b[K";
        let busy =
            "\x1b[10;3H\u{23f5}\u{23f5} auto mode on (shift+tab to cycle) \u{b7} esc to interrupt";
        let mut screen = Screen::new(Size::new(70, 30));
        screen.advance(format!("\x1b[8;1H\u{276f} {idle}").as_bytes());
        let mut activity = Activity::new(&screen);
        // 后台命令跑完，它自己接着干了两秒（在测试实例里录到的）：底栏一冒出 esc to interrupt 就算
        screen.advance(format!("\x1b[6;1H\u{2722} Choreographing\u{2026}{busy}").as_bytes());
        activity.output(&screen, ms(0), None);
        assert_eq!(activity.work, Some(ms(0)));
        screen.advance(format!("\x1b[6;1H\u{273b} Cooked for 2s\x1b[K{idle}").as_bytes());
        activity.output(&screen, ms(2000), None);
        assert_eq!(activity.work, Some(ms(2000)));
        // 闲着快到缓存过期时自己压缩：转圈和底栏的 esc to interrupt 一起冒出来，转好一阵，不算
        for n in 0..6u64 {
            let frame =
                format!("\x1b[6;1H\u{273b} Compacting conversation\u{2026} ({n}s)\x1b[K{busy}");
            screen.advance(frame.as_bytes());
            activity.output(&screen, ms(60_000 + n * 1000), None);
        }
        screen.advance(format!("\x1b[6;1HConversation compacted\x1b[K{idle}").as_bytes());
        activity.output(&screen, ms(66_000), None);
        assert_eq!(activity.last_change, Some(ms(66_000)));
        assert_eq!(activity.work, Some(ms(2000)));
        // 对话里正好写着这几个字（有人问了才答的，算）；之后闲着时底栏闪一下检查更新：这几个字一直
        // 在屏幕上，不是冒出来的，不算
        screen.advance(b"\x1b[5;1HPress esc to interrupt a reply.\x1b[K");
        activity.output(&screen, ms(120_000), Some(ms(119_000)));
        assert_eq!(activity.work, Some(ms(120_000)));
        screen.advance("\x1b[10;50HChecking for updates\u{2026}".as_bytes());
        activity.output(&screen, ms(200_000), None);
        assert_eq!(activity.last_change, Some(ms(200_000)));
        assert_eq!(activity.work, Some(ms(120_000)));

        // Codex 0.172：状态行在输入框上面，底下还有两行；大小写不论
        let mut screen = Screen::new(Size::new(80, 24));
        screen.advance(
            "\x1b[18;1H\u{203a} Ask Codex to do anything\x1b[20;3Hmodel \u{b7} ~/repo\x1b[21;3H? for shortcuts"
                .as_bytes(),
        );
        let mut activity = Activity::new(&screen);
        screen.advance("\x1b[16;1H\u{2022} Working (0s \u{2022} Esc to interrupt)".as_bytes());
        activity.output(&screen, ms(0), None);
        assert_eq!(activity.work, Some(ms(0)));
    }

    #[test]
    fn 订阅终端列表_有变化就推() {
        let (registry, fakes) = fake_registry();
        let (watch_out, mut watcher) = mailbox();
        registry.watch(5, 1, &watch_out);
        assert!(listed(&mut watcher).is_empty(), "订阅时先推一份现在的");

        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 1, &out);
        let term = inbox.opened().term;
        let list = listed(&mut watcher);
        assert_eq!(list.len(), 1);
        assert_eq!(
            (list[0].term.as_str(), list[0].idle_ms),
            (term.as_str(), None)
        );
        assert_eq!(list[0].programs, Some(Vec::new()), "shell 闲着");

        // 有了输出：推来最后一行和「多久以前」
        let session = fakes.lock().unwrap()[0]
            .session
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        session.output(b"building 62%");
        let list = listed(&mut watcher);
        assert_eq!(list[0].last, "building 62%");
        assert!(list[0].idle_ms.is_some_and(|idle| idle <= 2000));

        // 一直没有新输出：「正在输出」的那几秒里隔一会儿推一次，过了以后推一次「停了」的
        loop {
            let list = listed(&mut watcher);
            if list[0]
                .idle_ms
                .is_some_and(|idle| idle >= BUSY.as_millis() as u64)
            {
                break;
            }
        }

        // 屏幕不动，里面起了程序：也推
        *fakes.lock().unwrap()[0].programs.lock().unwrap() = vec!["claude".into()];
        let list = listed(&mut watcher);
        assert_eq!(list[0].programs, Some(vec!["claude".to_string()]));

        // 结束这个终端：推来空的列表
        registry.close(&term);
        assert!(listed(&mut watcher).is_empty());

        // 不订阅了：盯着的线程自己退出
        registry.unwatch(5, Some(1));
        std::thread::sleep(WATCH_EVERY * 3);
        assert!(!registry.0.watchers.lock().unwrap().running);
    }
}
