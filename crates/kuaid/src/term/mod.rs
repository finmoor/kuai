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
#[cfg(unix)]
pub mod pty;
pub mod screen;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use fr_core::protocol::{Response, TermExit, TermInfo, TermOpen, TermOpened};
use serde_json::json;

use crate::out::Outbox;
pub use screen::Size;
use screen::Screen;

/// 程序结束了、一直没有客户端来看的会话，留这么久就忘掉
const KEEP_ENDED: Duration = Duration::from_secs(24 * 3600);
/// 还原屏幕的那一段按这么大一块块发
const DUMP_CHUNK: usize = 64 * 1024;
/// 客户端收得太慢、输出积压太多时，断开它的时候说的话
const LAGGING: &str = "终端输出积压过多，连接已断开；重新连接后可继续使用";
/// 订阅了终端列表（`term_watch`）时，隔这么久看一次有没有变化
const WATCH_EVERY: Duration = Duration::from_secs(1);
/// 最近这么久以内有过输出的算「正在输出」
const BUSY: Duration = Duration::from_secs(3);
/// 有终端正在输出时，每看这么多次推一次列表（客户端靠它知道还在输出）
const BUSY_BEAT: u64 = 3;

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
    /// 升级换程序时带过去的两个数。Linux：伪终端的文件描述符和 shell 的进程号；
    /// Windows：托管进程听的端口和 shell 的进程号。
    fn handle(&self) -> Option<(i32, i32)> {
        None
    }
    /// 升级换程序前：不再收程序的输出，返回时已经收到的都交给了 [`Session::output`]。
    /// Linux 上用不着（整个进程一把锁，见 `pty::freeze`）。
    fn pause(&self) {}
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

/// 接着某个会话的一个客户端：会话的输出发到 `out`，挂在它的 `request` 号上。
struct Client {
    conn: u64,
    request: u64,
    out: Outbox,
    /// 只看的（`term_attach` 带 `watch`）：不改大小、不回答程序的话，不算「有人接着」。用户在那一头
    /// 敲的字照样经 `term_input` 写进来
    watch: bool,
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
    /// 最近一次输出是什么时候。升级换程序后还没有过输出时是空的
    last_output: Option<Instant>,
    clients: Vec<Client>,
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
        let mut state = self.state.lock().unwrap();
        let reply = state.screen.advance(bytes);
        state.last_output = Some(Instant::now());
        // 没人接着（只看的不算）：程序问终端的话由我们回答，不然它会一直等
        let answer = !reply.is_empty() && !state.attended();
        state.clients.retain(|client| {
            let header = Response {
                id: client.request,
                chunk: true,
                bytes: bytes.len() as u64,
                ..Default::default()
            };
            if client.out.try_send(&header, bytes) {
                return true;
            }
            // 这个客户端收得太慢（或者已经断了）：断开它，不能让程序等它
            let lagging = Response {
                id: client.request,
                error: Some(LAGGING.into()),
                ..Default::default()
            };
            client.out.push(&lagging, &[]);
            false
        });
        drop(state);
        if answer {
            self.backend.write(reply);
        }
    }

    /// 隔一会儿调用一次（没有输出的时候也要）。
    pub fn tick(&self) {
        self.state.lock().unwrap().screen.flush_sync();
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
        for client in state.clients.drain(..) {
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
    fn attach(
        &self,
        conn: u64,
        request: u64,
        out: &Outbox,
        size: Size,
        created: bool,
        watch: bool,
    ) {
        let mut state = self.state.lock().unwrap();
        let opened = TermOpened {
            term: self.id.clone(),
            created,
        };
        let header = Response {
            id: request,
            chunk: true,
            result: Some(json!(opened)),
            ..Default::default()
        };
        out.push(&header, &[]);
        if !watch && state.exit.is_none() && state.screen.size() != size {
            state.screen.resize(size);
            self.backend.resize(size);
        }
        if !created {
            let dump = state.screen.dump();
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
                    watch,
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

    pub fn input(&self, bytes: Vec<u8>) {
        if self.state.lock().unwrap().exit.is_none() {
            self.backend.write(bytes);
        }
    }

    pub fn resize(&self, size: Size) {
        let mut state = self.state.lock().unwrap();
        if state.exit.is_none() && state.screen.size() != size {
            state.screen.resize(size);
            self.backend.resize(size);
        }
    }

    fn info(&self) -> TermInfo {
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
            idle_ms: state.last_output.map(|at| at.elapsed().as_millis() as u64),
            last: state.screen.last_text(),
        }
    }

    fn is_alive(&self) -> bool {
        self.state.lock().unwrap().exit.is_none()
    }
}

/// 会话的名字记成什么：Windows 上同一个目录有几种写法，统一成一种（见
/// [`fr_core::dir::windows_key`]），本机的 Kuai 和别的电脑上的 Kuai 接上的才是同一个。
fn session_key(key: &str) -> String {
    match cfg!(windows) {
        true => fr_core::dir::windows_key(key),
        false => key.to_string(),
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
}

/// 所有的终端会话。
#[derive(Clone)]
pub struct Registry(Arc<Inner>);

struct Inner {
    sessions: Mutex<BTreeMap<String, Arc<Session>>>,
    next: AtomicU64,
    spawner: Spawner,
    watchers: Mutex<Watchers>,
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

/// 盯着列表的线程：隔一会儿看一次，变了就推给所有订阅的。
fn watch_loop(inner: Weak<Inner>) {
    let (mut sent, mut beat) = (None, 0u64);
    loop {
        std::thread::sleep(WATCH_EVERY);
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let registry = Registry(inner);
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
                watcher.out.push(&listing(watcher.id, &list), &[]);
            }
            sent = Some(now);
        }
    }
}

impl Registry {
    pub fn new(spawner: Spawner) -> Registry {
        Registry(Arc::new(Inner {
            sessions: Mutex::default(),
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
        conn: u64,
        request: u64,
        out: &Outbox,
    ) -> Result<()> {
        let size = Size::new(params.cols, params.rows);
        let key = session_key(&params.key);
        // 整个过程拿着登记表的锁：两个客户端同时开同一个名字，只会开出一个
        let mut sessions = self.0.sessions.lock().unwrap();
        let existing = sessions
            .values()
            .find(|session| !key.is_empty() && session.key == key)
            .cloned();
        if let Some(session) = existing {
            drop(sessions);
            session.attach(conn, request, out, size, false, false);
            return Ok(());
        }
        let id = format!("t{}", self.0.next.fetch_add(1, Ordering::Relaxed));
        let backend = (self.0.spawner)(&Spawn {
            term: id.clone(),
            cwd: params.cwd.clone(),
            size,
            env,
            shell: params.shell.clone(),
        })
        .with_context(|| format!("无法在 {} 中打开终端", params.cwd))?;
        let session = Arc::new(Session {
            id: id.clone(),
            key,
            cwd: params.cwd.clone(),
            created: now_secs(),
            backend,
            state: Mutex::new(State {
                screen: Screen::new(size),
                exit: None,
                ended: None,
                last_output: None,
                clients: Vec::new(),
            }),
            registry: Arc::downgrade(&self.0),
        });
        sessions.insert(id, session.clone());
        drop(sessions);
        // 先接上再开始读输出：第一个字也不会漏
        session.attach(conn, request, out, size, true, false);
        session.backend.start(session.clone());
        Ok(())
    }

    /// `term_attach`：接上指定的会话。`watch` 是只看（见 [`Session::attach`]）。
    pub fn attach(
        &self,
        term: &str,
        size: Size,
        watch: bool,
        conn: u64,
        request: u64,
        out: &Outbox,
    ) -> Result<()> {
        let session = self.get(term).context("终端会话不存在（可能已结束）")?;
        session.attach(conn, request, out, size, false, watch);
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
        out.push(&listing(id, &self.list()), &[]);
        if !watchers.running {
            watchers.running = true;
            let inner = Arc::downgrade(&self.0);
            std::thread::spawn(move || watch_loop(inner));
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
        self.all().iter().map(|session| session.info()).collect()
    }

    /// 现在有几个会话（含程序已经结束、还没人看过的）。
    pub fn count(&self) -> usize {
        self.0.sessions.lock().unwrap().len()
    }

    /// 里面的程序还在跑的有几个。
    pub fn alive(&self) -> usize {
        self.all().iter().filter(|s| s.is_alive()).count()
    }

    /// `term_close`：结束里面的程序，忘掉这个会话。还接着的客户端会收到退出码。
    pub fn close(&self, term: &str) -> bool {
        let Some(session) = self.0.sessions.lock().unwrap().remove(term) else {
            return false;
        };
        session.backend.kill();
        true
    }

    pub fn close_all(&self) {
        let sessions = std::mem::take(&mut *self.0.sessions.lock().unwrap());
        for session in sessions.values() {
            session.backend.kill();
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
    pub fn pause_all(&self) {
        for session in self.all() {
            if session.is_alive() {
                session.backend.pause();
            }
        }
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
        let session = Arc::new(Session {
            id: frozen.id.clone(),
            key: frozen.key,
            cwd: frozen.cwd,
            created: frozen.created,
            backend,
            state: Mutex::new(State {
                screen,
                exit: frozen.exit,
                ended: frozen.exit.map(|_| Instant::now()),
                last_output: None,
                clients: Vec::new(),
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

    fn open(registry: &Registry, key: &str, conn: u64, request: u64, out: &Outbox) {
        let params = TermOpen {
            key: key.into(),
            cwd: "/repo".into(),
            cols: 20,
            rows: 4,
            shell: Vec::new(),
        };
        registry
            .open(&params, Vec::new(), conn, request, out)
            .unwrap();
    }

    /// 把收到的输出在一个空白的终端里画出来，取第一行的字。
    fn first_line(bytes: &[u8]) -> String {
        let mut screen = Screen::new(Size::new(20, 4));
        screen.advance(bytes);
        screen.line_text(0)
    }

    #[test]
    fn 输出转给接着的客户端_断开以后程序接着跑_再接上能还原() {
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 10, &out);
        let opened = inbox.opened();
        assert!(opened.created);
        let fake = fakes.lock().unwrap()[0].clone();
        let session = fake.session.lock().unwrap().clone().expect("没有开始读输出");

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
    fn 接上时大小不一样就先改大小() {
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 1, &out);
        let term = inbox.opened().term;
        registry.detach(1, None);
        let (out2, mut inbox2) = mailbox();
        registry
            .attach(&term, Size::new(50, 10), false, 2, 1, &out2)
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
        let session = fakes.lock().unwrap()[0].session.lock().unwrap().clone().unwrap();
        session.finish(Some(3));
        let (response, _) = inbox.next();
        assert!(!response.chunk);
        let exit: TermExit = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(exit.exit, Some(3));
        assert_eq!(registry.count(), 0);

        // 没人接着的时候结束：留着，下一个来看的人收到最后的画面和退出码，然后才忘掉
        open(&registry, "b", 1, 2, &out);
        inbox.opened();
        let session = fakes.lock().unwrap()[1].session.lock().unwrap().clone().unwrap();
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
        let session = fakes.lock().unwrap()[0].session.lock().unwrap().clone().unwrap();
        session.finish(None);
        assert!(!inbox.next().0.chunk);
    }

    #[test]
    fn 客户端收得太慢就断开它() {
        let (registry, fakes) = fake_registry();
        // 这个客户端的管道没人读
        let (reader, writer) = std::io::pipe().unwrap();
        let out = Outbox::new(writer);
        open(&registry, "/repo", 1, 1, &out);
        let session = fakes.lock().unwrap()[0].session.lock().unwrap().clone().unwrap();
        let block = vec![b'x'; 1 << 20];
        for _ in 0..64 {
            session.output(&block);
        }
        // 程序没被卡住（上面的循环跑完了），那个客户端被断开了
        assert_eq!(registry.list()[0].attached, 0);
        drop(reader);
    }

    #[test]
    fn 升级时拍下来再接着管() {
        let (registry, fakes) = fake_registry();
        let (out, mut inbox) = mailbox();
        open(&registry, "/repo", 1, 1, &out);
        let term = inbox.opened().term;
        let session = fakes.lock().unwrap()[0].session.lock().unwrap().clone().unwrap();
        session.output(b"\x1b[32mbefore upgrade\x1b[m");
        let (next, mut frozen) = registry.freeze();
        assert_eq!(frozen.len(), 1);
        assert_eq!(frozen[0].handle, Some((7, 4242)));

        // 新程序里：按拍下来的重建
        let (fresh, _) = fake_registry();
        fresh.set_next(next);
        let fake = Arc::new(Fake::default());
        fresh.adopt(frozen.remove(0), Box::new(fake.clone()));
        assert!(fake.session.lock().unwrap().is_some(), "没有接着读输出");
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
            .attach(&term, Size::new(50, 10), true, 2, 7, &watch_out)
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
            .attach(&term, Size::new(20, 4), true, 3, 1, &late_out)
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

        // 结束这个终端：推来空的列表
        registry.close(&term);
        assert!(listed(&mut watcher).is_empty());

        // 不订阅了：盯着的线程自己退出
        registry.unwatch(5, Some(1));
        std::thread::sleep(WATCH_EVERY * 3);
        assert!(!registry.0.watchers.lock().unwrap().running);
    }
}
