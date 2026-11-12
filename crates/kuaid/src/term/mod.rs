//! 终端会话：开在 kuaid 里的 shell，客户端断开、退出以后里面的程序接着跑。
//!
//! 一个会话 = 一个伪终端里的登录 shell + 记着屏幕的仿真器（[`screen`]）+ 现在接着的客户端。
//! 程序的输出喂给仿真器，同时转给接着的每个客户端；客户端接上来时先收到还原屏幕的那一段。
//! 伪终端那一层在 [`pty`]（只有 Linux 上有），这里不碰系统调用，测试里换成假的。

// Windows 上的 kuaid 只拿来测协议，开不了终端，下面有些东西用不上
#![cfg_attr(not(unix), allow(dead_code))]

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
const LAGGING: &str = "终端的输出积压太多，已经断开；重新接上就能接着用";

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
    /// 伪终端的文件描述符和 shell 的进程号：升级换程序时带过去。
    fn handle(&self) -> Option<(i32, i32)> {
        None
    }
}

/// 开一个新会话要的东西。
pub struct Spawn {
    /// 会话的编号
    pub term: String,
    pub cwd: String,
    pub size: Size,
    /// 除了继承来的，shell 再多带的环境变量
    pub env: Vec<(String, String)>,
}

pub type Spawner = Box<dyn Fn(&Spawn) -> Result<Box<dyn Backend>> + Send + Sync>;

/// 接着某个会话的一个客户端：会话的输出发到 `out`，挂在它的 `request` 号上。
struct Client {
    conn: u64,
    request: u64,
    out: Outbox,
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
    clients: Vec<Client>,
}

impl Session {
    /// 程序的一块输出。
    pub fn output(&self, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap();
        let reply = state.screen.advance(bytes);
        if state.clients.is_empty() {
            drop(state);
            // 没人接着：程序问终端的话由我们回答，不然它会一直等
            if !reply.is_empty() {
                self.backend.write(reply);
            }
            return;
        }
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
        let seen = !state.clients.is_empty();
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
    fn attach(&self, conn: u64, request: u64, out: &Outbox, size: Size, created: bool) {
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
        if state.exit.is_none() && state.screen.size() != size {
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
                self.forget();
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
            attached: state.clients.len(),
            age: now_secs().saturating_sub(self.created),
        }
    }

    fn is_alive(&self) -> bool {
        self.state.lock().unwrap().exit.is_none()
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
    /// 伪终端的文件描述符和 shell 的进程号；程序已经结束的没有
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
}

impl Registry {
    pub fn new(spawner: Spawner) -> Registry {
        Registry(Arc::new(Inner {
            sessions: Mutex::default(),
            next: AtomicU64::new(1),
            spawner,
        }))
    }

    /// 这个系统上开不了终端（Windows 上的 kuaid 只拿来测协议）。
    #[cfg(any(test, not(unix)))]
    pub fn unsupported() -> Registry {
        Registry::new(Box::new(|_| anyhow::bail!("这个系统上的 kuaid 不能开终端")))
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
        // 整个过程拿着登记表的锁：两个客户端同时开同一个名字，只会开出一个
        let mut sessions = self.0.sessions.lock().unwrap();
        let existing = sessions
            .values()
            .find(|session| !params.key.is_empty() && session.key == params.key)
            .cloned();
        if let Some(session) = existing {
            drop(sessions);
            session.attach(conn, request, out, size, false);
            return Ok(());
        }
        let id = format!("t{}", self.0.next.fetch_add(1, Ordering::Relaxed));
        let backend = (self.0.spawner)(&Spawn {
            term: id.clone(),
            cwd: params.cwd.clone(),
            size,
            env,
        })
        .with_context(|| format!("在 {} 里开不了终端", params.cwd))?;
        let session = Arc::new(Session {
            id: id.clone(),
            key: params.key.clone(),
            cwd: params.cwd.clone(),
            created: now_secs(),
            backend,
            state: Mutex::new(State {
                screen: Screen::new(size),
                exit: None,
                ended: None,
                clients: Vec::new(),
            }),
            registry: Arc::downgrade(&self.0),
        });
        sessions.insert(id, session.clone());
        drop(sessions);
        // 先接上再开始读输出：第一个字也不会漏
        session.attach(conn, request, out, size, true);
        session.backend.start(session.clone());
        Ok(())
    }

    /// `term_attach`：接上指定的会话。
    pub fn attach(
        &self,
        term: &str,
        size: Size,
        conn: u64,
        request: u64,
        out: &Outbox,
    ) -> Result<()> {
        let session = self.get(term).context("没有这个终端会话（可能已经结束了）")?;
        session.attach(conn, request, out, size, false);
        Ok(())
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
            .attach(&term, Size::new(50, 10), 2, 1, &out2)
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
}
