//! 服务一个客户端：读它的请求，执行，把结果放进它的发送队列。
//!
//! 常驻时每个客户端（一条 ssh 连接经转接程序接进来）一个线程跑 [`serve`]；`--stdio` 时就一个。
//! 终端会话不属于哪个客户端，记在 [`Shared`] 里，客户端断开时只是从会话上断开。

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use fr_core::exec::{self, Killer, OnStderr};
use fr_core::protocol::{
    self, DaemonInfo, FilePut, FsWrite, GitResult, Hello, PROTOCOL_VERSION, Request, Response,
    TermOpen,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::VERSION;
use crate::files::{self, Uploads};
use crate::out::Outbox;
use crate::relay::Relays;
use crate::term::{Caller, Registry, Size};
use crate::workspaces::Workspaces;

/// 密码请求最多等用户这么久
const ASK_TIMEOUT: Duration = Duration::from_secs(600);

/// 只有常驻进程能做的事。
pub trait Control: Send + Sync {
    /// 原地换成 `exe` 这个程序。成功的话不返回（进程已经换了），返回的是失败的原因。
    fn upgrade(&self, shared: &Shared, exe: &str) -> anyhow::Error;
    /// 结束所有终端会话并退出。
    fn shutdown(&self, shared: &Shared) -> !;
    /// 记下一条 ssh 连接转发过来的 ssh-agent 套接字；返回终端里的 shell 该用的地址。
    fn auth_sock(&self, path: &str) -> Option<String>;
}

/// 所有客户端共用的东西。
pub struct Shared {
    pub terms: Registry,
    conns: Mutex<HashMap<u64, Arc<Conn>>>,
    next_conn: AtomicU64,
    control: Option<Box<dyn Control>>,
    /// 装在哪个目录里（`<版本>-<哈希>`）
    pub build: String,
    /// 客户端传上来的文件放在哪
    pub drop_dir: PathBuf,
    /// 这台电脑保存的仓库列表和工作区（协议 10）
    pub workspaces: Workspaces,
    /// 最近一次有客户端连着的时间：闲了多久从它算
    last_busy: Mutex<Instant>,
}

impl Shared {
    pub fn new(terms: Registry, control: Option<Box<dyn Control>>, build: String) -> Arc<Shared> {
        Self::with_dirs(
            terms,
            control,
            build,
            files::default_dir(),
            Workspaces::default_file(),
        )
    }

    /// 同 [`Self::new`]，传上来的文件放到 `drop_dir`、仓库列表和工作区存在 `workspaces` 指定的地方（测试用）。
    pub fn with_dirs(
        terms: Registry,
        control: Option<Box<dyn Control>>,
        build: String,
        drop_dir: PathBuf,
        workspaces: Workspaces,
    ) -> Arc<Shared> {
        Arc::new(Shared {
            terms,
            conns: Mutex::default(),
            next_conn: AtomicU64::new(1),
            control,
            build,
            drop_dir,
            workspaces,
            last_busy: Mutex::new(Instant::now()),
        })
    }

    pub fn clients(&self) -> usize {
        self.conns.lock().unwrap().len()
    }

    /// 没有客户端连着、也没有终端会话，已经这样多久了。还有的话是 0。
    pub fn idle_for(&self) -> Duration {
        let mut last_busy = self.last_busy.lock().unwrap();
        if self.clients() > 0 || self.terms.count() > 0 {
            *last_busy = Instant::now();
            return Duration::ZERO;
        }
        last_busy.elapsed()
    }

    /// 杀掉所有客户端正在跑的 git（升级换程序前）。
    pub fn kill_git(&self) {
        for conn in self.conns.lock().unwrap().values() {
            conn.kill_git();
        }
    }

    pub fn info(&self) -> DaemonInfo {
        DaemonInfo {
            protocol: PROTOCOL_VERSION,
            version: VERSION.into(),
            build: self.build.clone(),
            pid: std::process::id(),
            terms: self.terms.count(),
            clients: self.clients(),
            #[cfg(windows)]
            session: Some(crate::win::session_id()),
            #[cfg(not(windows))]
            session: None,
            #[cfg(windows)]
            guarded: crate::win::redirect_guarded(),
            #[cfg(not(windows))]
            guarded: false,
        }
    }

    /// 服务器上的 git、ssh 要密码时转给客户端：把自己设成 askpass 程序，助手进程连回来时带着
    /// 是哪个客户端的 git 在问（[`Conn::env`] 里的标记），就去问那个客户端。
    /// `dir` 是放套接字文件的目录。
    pub fn start_askpass(self: &Arc<Self>, dir: &Path) {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let shared = Arc::downgrade(self);
        let started = fr_core::askpass::serve_tagged_in(dir, &exe, move |prompt, tag| {
            let shared = shared.upgrade()?;
            let id: u64 = tag?.strip_prefix('c')?.parse().ok()?;
            let conn = shared.conns.lock().unwrap().get(&id).cloned()?;
            conn.ask(prompt)
        });
        match started {
            Ok(env) => fr_core::git::set_extra_env(env),
            // 开不了也照常工作，只是要密码的操作会直接失败
            Err(err) => eprintln!("kuaid: 密码转发不可用：{err:#}"),
        }
    }
}

/// 一个连着的客户端。
pub struct Conn {
    id: u64,
    out: Outbox,
    /// 还在跑的 git 进程，按请求编号记，取消时用
    running: Mutex<HashMap<u64, Killer>>,
    /// 发给客户端、还没得到回答的密码请求，按询问的编号记
    asks: Mutex<HashMap<u64, mpsc::Sender<Option<String>>>>,
    next_ask: AtomicU64,
    /// 这条 ssh 连接转发过来的 ssh-agent 套接字
    auth_sock: Mutex<Option<String>>,
    /// 正在传上来的文件；连接断开时没传完的跟着删掉
    uploads: Mutex<Uploads>,
    /// 替它连这台电脑配置的服务器的 ssh（协议 10），断开时跟着结束
    relays: Arc<Relays>,
    /// 客户端握手（`hello`）时说的协议版本：它认不得的东西不发给它（见
    /// [`fr_core::protocol::TERM_MARKS_SINCE`]）。握手前按现在这一版算（只有测试不握手）；握手时没说
    /// 的是很旧的客户端，按 0 算
    protocol: AtomicU32,
    /// 同一个文件的 `fs_pwrite` 排着队做（见 [`Writes`]）
    writes: Arc<Writes>,
}

/// 同一个文件的 `fs_pwrite` 按收到的先后做（协议 17）。别的文件请求各在各的线程里做（主机上的文件
/// 操作卡住时不挡别的），同一个文件的写入要是也那样，先后会乱：映射盘上连着追加的两段，后发的可能
/// 先写到末尾。按仓库加路径排队，一个文件一个线程，队里空了线程就结束。
#[derive(Default)]
struct Writes(Mutex<HashMap<String, std::collections::VecDeque<Job>>>);

type Job = Box<dyn FnOnce() + Send>;

impl Writes {
    fn push(self: &Arc<Self>, key: String, job: Job) {
        let mut queues = self.0.lock().unwrap();
        if let Some(queue) = queues.get_mut(&key) {
            queue.push_back(job);
            return;
        }
        queues.insert(key.clone(), Default::default());
        drop(queues);
        let writes = self.clone();
        std::thread::spawn(move || {
            let mut next = Some(job);
            while let Some(job) = next.take() {
                job();
                let mut queues = writes.0.lock().unwrap();
                next = queues.get_mut(&key).and_then(|queue| queue.pop_front());
                if next.is_none() {
                    queues.remove(&key);
                }
            }
        });
    }
}

impl Conn {
    /// 这条连接上的 `request` 号请求要接终端。
    fn caller(&self, request: u64) -> Caller {
        Caller {
            conn: self.id,
            request,
            protocol: self.protocol.load(Ordering::Relaxed),
        }
    }

    fn send(&self, response: &Response, payload: &[u8]) {
        self.out.send(response, payload);
    }

    fn reply(&self, id: u64, result: Result<Value>) {
        let response = match result {
            Ok(result) => Response {
                id,
                result: Some(result),
                ..Default::default()
            },
            Err(err) => Response {
                id,
                error: Some(format!("{err:#}")),
                ..Default::default()
            },
        };
        self.send(&response, &[]);
    }

    /// 这个客户端的 git 进程多带的环境变量：标出是谁的（要密码时知道去问谁），
    /// 带上它那条 ssh 连接转发过来的 ssh-agent。
    fn env(&self) -> Vec<(String, String)> {
        let mut env = vec![(
            fr_core::askpass::TAG_ENV.to_string(),
            format!("c{}", self.id),
        )];
        if let Some(sock) = self.auth_sock.lock().unwrap().clone() {
            env.push(("SSH_AUTH_SOCK".into(), sock));
        }
        env
    }

    /// 问这个客户端要密码，等它回答。
    fn ask(&self, prompt: &str) -> Option<String> {
        let id = self.next_ask.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.asks.lock().unwrap().insert(id, tx);
        let ask = Response {
            id,
            ask: Some(prompt.to_string()),
            ..Default::default()
        };
        self.out.push(&ask, &[]);
        // 客户端断开时登记的那一头被丢掉，这里马上返回
        let answer = rx.recv_timeout(ASK_TIMEOUT).ok().flatten();
        self.asks.lock().unwrap().remove(&id);
        answer
    }

    /// 杀掉它正在跑的 git，结束替它连服务器的 ssh。
    fn kill_git(&self) {
        for killer in self.running.lock().unwrap().values() {
            killer.kill();
        }
        self.relays.kill_all();
    }
}

#[derive(Deserialize)]
struct GitParams {
    repo: String,
    args: Vec<String>,
    #[serde(default)]
    stream: bool,
    /// 标准错误也一块块发回去（要同时开 `stream`）
    #[serde(default)]
    progress: bool,
}

#[derive(Deserialize)]
struct StateApply {
    ops: Vec<fr_core::store::Op>,
}

#[derive(Deserialize)]
struct RelayOpen {
    /// 这台电脑配置的服务器的 id
    server: String,
    /// 在那台服务器上执行的命令
    command: String,
    /// 不许问人（客户端在后台自己重连，协议 17）：要密码、要确认主机密钥就失败
    #[serde(default)]
    quiet: bool,
}

#[derive(Deserialize)]
struct TermParams {
    term: String,
    #[serde(default)]
    cols: u16,
    #[serde(default)]
    rows: u16,
    /// 只看：不改会话的大小（协议 11 起）
    #[serde(default)]
    watch: bool,
}

/// 服务一个客户端，直到它断开（`reader` 读到头）。
pub fn serve(shared: &Arc<Shared>, reader: impl Read, writer: impl Write + Send + 'static) {
    let conn = Arc::new(Conn {
        id: shared.next_conn.fetch_add(1, Ordering::Relaxed),
        out: Outbox::new(writer),
        running: Mutex::default(),
        asks: Mutex::default(),
        next_ask: AtomicU64::new(1),
        auth_sock: Mutex::default(),
        uploads: Mutex::new(Uploads::new(shared.drop_dir.clone())),
        relays: Arc::default(),
        protocol: AtomicU32::new(PROTOCOL_VERSION),
        writes: Arc::default(),
    });
    shared.conns.lock().unwrap().insert(conn.id, conn.clone());
    let mut watchers: HashMap<u64, fr_core::watch::Watcher> = HashMap::new();
    let mut input = BufReader::with_capacity(1 << 16, reader);
    loop {
        match protocol::read_message::<_, Request>(&mut input) {
            Ok(Some((request, payload))) => {
                handle(shared, &conn, request, payload, &mut watchers);
            }
            Ok(None) => break,
            Err(err) => {
                eprintln!("kuaid: 客户端 {} 发来的消息读不了：{err:#}", conn.id);
                break;
            }
        }
    }
    // 客户端断开了：它的 git 杀掉、监听停掉、等着的密码请求当作取消；终端会话留着
    conn.kill_git();
    // 它转发过 ssh-agent：固定地址可能正指着它，改指到别的还连着、也转发了的连接上
    if conn.auth_sock.lock().unwrap().is_some()
        && let Some(control) = &shared.control
    {
        let other = shared
            .conns
            .lock()
            .unwrap()
            .values()
            .filter(|other| other.id != conn.id)
            .find_map(|other| other.auth_sock.lock().unwrap().clone());
        if let Some(sock) = other {
            control.auth_sock(&sock);
        }
    }
    drop(watchers);
    shared.workspaces.unwatch(conn.id, None);
    shared.terms.unwatch(conn.id, None);
    shared.terms.detach(conn.id, None);
    conn.asks.lock().unwrap().clear();
    shared.conns.lock().unwrap().remove(&conn.id);
    *shared.last_busy.lock().unwrap() = Instant::now();
    conn.out.close();
}

fn handle(
    shared: &Arc<Shared>,
    conn: &Arc<Conn>,
    request: Request,
    payload: Vec<u8>,
    watchers: &mut HashMap<u64, fr_core::watch::Watcher>,
) {
    let id = request.id;
    let param = |name: &str| request.params.get(name);
    match request.method.as_str() {
        "git" => start_git(conn, request, payload),
        "cancel" => {
            if let Some(target) = param("id").and_then(Value::as_u64) {
                // 不会卡住读下一个请求：Windows 上要启动 taskkill，kill 放在别的线程里做
                if let Some(killer) = conn.running.lock().unwrap().get(&target).cloned() {
                    killer.kill();
                }
                watchers.remove(&target);
                shared.workspaces.unwatch(conn.id, Some(target));
                shared.terms.unwatch(conn.id, Some(target));
                shared.terms.detach(conn.id, Some(target));
                conn.relays.kill(target);
            }
        }
        "relay_open" => {
            let opened = serde_json::from_value::<RelayOpen>(request.params)
                .context("relay_open 参数无效")
                .and_then(|params| {
                    let helper = std::env::current_exe().context("无法确定 kuaid 的安装位置")?;
                    let store = shared.workspaces.get();
                    let cmd = crate::relay::command(
                        &store,
                        (&params.server, &params.command),
                        &conn.env(),
                        &helper,
                        params.quiet,
                    )?;
                    conn.relays.open(id, cmd, &conn.out)
                });
            if let Err(err) = opened {
                conn.reply(id, Err(err));
            }
        }
        "relay_input" => {
            if let Some(relay) = param("relay").and_then(Value::as_u64) {
                let eof = param("eof").and_then(Value::as_bool) == Some(true);
                conn.relays.input(relay, payload, eof);
            }
        }
        "state_watch" => shared.workspaces.watch(conn.id, id, &conn.out),
        "term_watch" => shared.terms.watch(conn.id, id, &conn.out),
        "state_apply" => {
            let applied = serde_json::from_value::<StateApply>(request.params)
                .context("state_apply 参数无效")
                .and_then(|params| shared.workspaces.apply(&params.ops))
                .and_then(|store| Ok(serde_json::to_value(store)?));
            conn.reply(id, applied);
        }
        "watch" => {
            // 自己结束了的（仓库目录没了、换了）不再留着
            watchers.retain(|_, watcher| watcher.is_running());
            let repo = param("repo").and_then(Value::as_str).unwrap_or_default();
            let (out, end) = (conn.out.clone(), conn.out.clone());
            let started = fr_core::watch::watch(
                Path::new(repo),
                move || {
                    let changed = Response {
                        id,
                        chunk: true,
                        ..Default::default()
                    };
                    // 「有变化」都一样：还没发出去的有一条就够了
                    out.push_latest(id, &changed, &[]);
                },
                // 告诉客户端监听结束了：它过一会儿重新装，装在原处新建的那个目录上
                move |reason| {
                    let ended = Response {
                        id,
                        error: Some(reason),
                        ..Default::default()
                    };
                    end.push(&ended, &[]);
                },
            );
            match started {
                Ok(watcher) => {
                    watchers.insert(id, watcher);
                }
                Err(err) => conn.reply(id, Err(err)),
            }
        }
        "answer" => {
            let waiter = param("id")
                .and_then(Value::as_u64)
                .and_then(|ask| conn.asks.lock().unwrap().remove(&ask));
            if let Some(waiter) = waiter {
                let answer = param("answer").and_then(Value::as_str).map(str::to_string);
                let _ = waiter.send(answer);
            }
        }
        "bridge" => {
            let reported = param("auth_sock")
                .and_then(Value::as_str)
                .map(str::to_string);
            // 固定地址马上改指到这条连接转发来的：按编号接回的旧终端（term_attach 不经 term_env）
            // 里的 shell 用的也是固定地址，原来要等下次新开终端才改指，期间指着断掉的连接
            if let (Some(control), Some(sock)) = (&shared.control, &reported) {
                control.auth_sock(sock);
            }
            *conn.auth_sock.lock().unwrap() = reported;
        }
        "term_open" => {
            let opened = serde_json::from_value::<TermOpen>(request.params)
                .context("term_open 参数无效")
                .and_then(|params| {
                    let env = term_env(shared, conn);
                    shared.terms.open(&params, env, conn.caller(id), &conn.out)
                });
            if let Err(err) = opened {
                conn.reply(id, Err(err));
            }
        }
        "term_attach" => {
            let attached = term_params(&request).and_then(|params| {
                let size = Size::new(params.cols, params.rows);
                shared
                    .terms
                    .attach(&params.term, size, params.watch, conn.caller(id), &conn.out)
            });
            if let Err(err) = attached {
                conn.reply(id, Err(err));
            }
        }
        "term_input" => {
            let term = param("term").and_then(Value::as_str).unwrap_or_default();
            if let Some(session) = shared.terms.get(term) {
                session.input(payload);
            }
        }
        "term_resize" => {
            if let Ok(params) = term_params(&request)
                && let Some(session) = shared.terms.get(&params.term)
            {
                session.resize(Size::new(params.cols, params.rows));
            }
        }
        "file_put" => {
            let reply = serde_json::from_value::<FilePut>(request.params)
                .context("file_put 参数无效")
                .and_then(|params| conn.uploads.lock().unwrap().put(params, &payload))
                .and_then(|mut reply| {
                    reply.path = reply.path.map(|path| crate::dirs::shown(&path));
                    Ok(serde_json::to_value(reply)?)
                });
            conn.reply(id, reply);
        }
        method if method.starts_with("fs_") => {
            // 读写文件可能慢（大文件、网络盘）：放到自己的线程里，这边接着读下一个请求。同一个文件的
            // 写入排着队做（见 `Writes`）
            let queue = (method == "fs_pwrite").then(|| {
                let text = |name: &str| param(name).and_then(Value::as_str).unwrap_or_default();
                format!("{}\n{}", text("repo"), text("path"))
            });
            let writes = conn.writes.clone();
            let conn = conn.clone();
            let job = move || match fs_request(request, &payload) {
                Ok((result, bytes)) => {
                    let response = Response {
                        id,
                        result: Some(result),
                        bytes: bytes.len() as u64,
                        ..Default::default()
                    };
                    conn.send(&response, &bytes);
                }
                Err(err) => conn.reply(id, Err(err)),
            };
            match queue {
                Some(key) => writes.push(key, Box::new(job)),
                None => drop(std::thread::spawn(job)),
            }
        }
        "upgrade" => {
            let exe = param("exe").and_then(Value::as_str).unwrap_or_default();
            let failed = match &shared.control {
                Some(control) => control.upgrade(shared, exe),
                None => anyhow::anyhow!("非常驻进程，无法升级"),
            };
            conn.reply(id, Err(failed));
        }
        // 用自己这一版重新起一遍（Windows 上从会话 0 搬到桌面上用）。办成了不回答，连接直接断
        "move" => {
            let failed = match (&shared.control, std::env::current_exe()) {
                (Some(control), Ok(exe)) => control.upgrade(shared, &exe.to_string_lossy()),
                (Some(_), Err(err)) => anyhow::anyhow!("无法确定 kuaid 的安装位置：{err}"),
                (None, _) => anyhow::anyhow!("非常驻进程"),
            };
            conn.reply(id, Err(failed));
        }
        "shutdown" => match &shared.control {
            Some(control) => {
                conn.reply(id, Ok(json!({})));
                conn.out.drain(Duration::from_secs(2));
                control.shutdown(shared);
            }
            None => conn.reply(id, Err(anyhow::anyhow!("非常驻进程"))),
        },
        "hello" => {
            let theirs = param("protocol").and_then(Value::as_u64).unwrap_or(0);
            conn.protocol
                .store(u32::try_from(theirs).unwrap_or(u32::MAX), Ordering::Relaxed);
            conn.reply(id, simple(shared, &request));
        }
        _ => conn.reply(id, simple(shared, &request)),
    }
}

/// 马上就有结果的那些方法。
fn simple(shared: &Shared, request: &Request) -> Result<Value> {
    let param = |name: &str| request.params.get(name).and_then(Value::as_str);
    match request.method.as_str() {
        // 只用来测往返延迟
        "ping" => Ok(json!({})),
        "hello" => Ok(serde_json::to_value(Hello {
            protocol: PROTOCOL_VERSION,
            version: VERSION.into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            git: git_version(),
            home: crate::dirs::home(),
            build: shared.build.clone(),
            daemon: shared.control.is_some(),
            machine: shared.workspaces.machine().to_string(),
        })?),
        "info" => Ok(serde_json::to_value(shared.info())?),
        "state_get" => Ok(serde_json::to_value(shared.workspaces.get())?),
        "list_dir" => Ok(serde_json::to_value(crate::dirs::list_dir(
            param("path").unwrap_or(""),
            request
                .params
                .get("hidden")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )?)?),
        "term_list" => Ok(json!({ "terms": shared.terms.list() })),
        "term_programs" => Ok(json!({
            "programs": shared.terms.programs(param("term").unwrap_or_default())
        })),
        "term_close" => {
            let closed = shared.terms.close(param("term").unwrap_or_default());
            Ok(json!({ "closed": closed }))
        }
        other => bail!("未知方法：{other}"),
    }
}

/// 仓库里的文件（文件页用）：返回结果和跟在后面的原始字节。
fn fs_request(request: Request, payload: &[u8]) -> Result<(Value, Vec<u8>)> {
    use fr_core::files;
    let text = |name: &str| {
        request
            .params
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    let number = |name: &str| request.params.get(name).and_then(Value::as_u64);
    let flag = |name: &str| request.params.get(name).and_then(Value::as_bool) == Some(true);
    let (repo, path) = (text("repo"), text("path"));
    let done = |result: Value| Ok((result, Vec::new()));
    // 只认仓库里的（本机网页服务、Markdown 预览读的，协议 17）
    let confine = flag("confine");
    match request.method.as_str() {
        "fs_list" if confine => done(json!({ "entries": files::list_confined(repo, path)? })),
        "fs_list" => done(json!({ "entries": files::list_with(repo, path, flag("all"))? })),
        "fs_repos" => done(serde_json::to_value(files::find_repos(repo)?)?),
        "fs_stat" if confine => done(serde_json::to_value(files::stat_confined(repo, path)?)?),
        "fs_stat" => done(serde_json::to_value(files::stat(repo, path)?)?),
        "fs_read" => {
            let (offset, len) = (number("offset").unwrap_or(0), number("len"));
            let len = len.unwrap_or(files::READ_MAX);
            let read = match confine {
                true => files::read_confined,
                false => files::read,
            };
            let (stat, bytes) = read(repo, path, offset, len, number("whole"))?;
            Ok((serde_json::to_value(stat)?, bytes))
        }
        "fs_write" => {
            let params: FsWrite =
                serde_json::from_value(request.params.clone()).context("fs_write 参数无效")?;
            done(serde_json::to_value(files::write(&params, payload)?)?)
        }
        "fs_mkdir" => files::make_dir(repo, path).and_then(|()| done(json!({}))),
        "fs_sync" => done(serde_json::to_value(files::sync(repo, path)?)?),
        "fs_remove" => match (text("kind"), flag("shallow")) {
            ("file", _) => files::remove_kind(repo, path, false),
            ("dir", _) => files::remove_kind(repo, path, true),
            (_, true) => files::remove_shallow(repo, path),
            (_, false) => files::remove(repo, path),
        }
        .and_then(|()| done(json!({}))),
        "fs_rename" => match flag("replace") {
            true => files::rename_over(repo, path, text("to")),
            false => files::rename(repo, path, text("to")),
        }
        .and_then(|()| done(json!({}))),
        "fs_pwrite" if flag("append") => {
            done(serde_json::to_value(files::append(repo, path, payload)?)?)
        }
        "fs_pwrite" => {
            let offset = number("offset").unwrap_or(0);
            let stat = files::write_at(repo, path, offset, payload, flag("create"))?;
            done(serde_json::to_value(stat)?)
        }
        "fs_chmod" => {
            let mode = number("mode").context("fs_chmod 没有给权限")?;
            done(serde_json::to_value(files::set_mode(
                repo,
                path,
                mode as u32,
            )?)?)
        }
        "fs_resize" => {
            let size = number("size").context("fs_resize 没有给大小")?;
            done(serde_json::to_value(files::resize(
                repo,
                path,
                size,
                flag("new"),
            )?)?)
        }
        "fs_touch" => {
            let mtime = number("mtime").context("fs_touch 没有给时间")?;
            done(serde_json::to_value(files::set_mtime(repo, path, mtime)?)?)
        }
        "fs_space" => done(serde_json::to_value(files::space(repo)?)?),
        other => bail!("未知方法：{other}"),
    }
}

fn term_params(request: &Request) -> Result<TermParams> {
    serde_json::from_value(request.params.clone()).context("终端参数无效")
}

/// 终端里的 shell 多带的环境变量：这条连接转发了 ssh-agent 的话，给它一个不随连接变的地址。
fn term_env(shared: &Shared, conn: &Conn) -> Vec<(String, String)> {
    let sock = conn.auth_sock.lock().unwrap().clone();
    let stable = match (&shared.control, sock) {
        (Some(control), Some(sock)) => control.auth_sock(&sock),
        (_, sock) => sock,
    };
    stable
        .map(|sock| vec![("SSH_AUTH_SOCK".to_string(), sock)])
        .unwrap_or_default()
}

/// 启动 git，在单独的线程里收输出，处理请求的线程接着读下一个请求。
fn start_git(conn: &Arc<Conn>, request: Request, payload: Vec<u8>) {
    let id = request.id;
    let params: GitParams = match serde_json::from_value(request.params).context("git 参数无效")
    {
        Ok(params) => params,
        Err(err) => return conn.reply(id, Err(err)),
    };
    let stdin = (request.bytes > 0).then_some(payload);
    let on_stderr: Option<OnStderr> = (params.stream && params.progress).then(|| {
        let out = conn.out.clone();
        Box::new(move |chunk: &[u8]| {
            let header = Response {
                id,
                chunk: true,
                err: true,
                bytes: chunk.len() as u64,
                ..Default::default()
            };
            out.send(&header, chunk);
        }) as OnStderr
    });
    let spawned = exec::spawn_env(
        Path::new(&params.repo),
        &params.args,
        stdin,
        on_stderr,
        &conn.env(),
    );
    let (process, killer) = match spawned {
        Ok(started) => started,
        Err(err) => return conn.reply(id, Err(err)),
    };
    conn.running.lock().unwrap().insert(id, killer);

    let conn = conn.clone();
    std::thread::spawn(move || {
        let outcome = if params.stream {
            process
                .finish(|chunk| {
                    let header = Response {
                        id,
                        chunk: true,
                        bytes: chunk.len() as u64,
                        ..Default::default()
                    };
                    conn.send(&header, chunk);
                })
                .map(|finished| (finished, Vec::new()))
        } else {
            process
                .collect()
                .map(|(stdout, finished)| (finished, stdout))
        };
        conn.running.lock().unwrap().remove(&id);
        match outcome {
            Ok((finished, stdout)) => {
                let result = GitResult {
                    code: finished.code,
                    stderr: finished.stderr,
                    ms: finished.elapsed.as_millis() as u64,
                };
                let response = Response {
                    id,
                    result: Some(serde_json::to_value(result).unwrap_or_default()),
                    bytes: stdout.len() as u64,
                    ..Default::default()
                };
                conn.send(&response, &stdout);
            }
            Err(err) => conn.reply(id, Err(err)),
        }
    });
}

/// 这台电脑上 git 的版本（`git --version` 的输出）。每次握手都重新问：常驻进程一直在，主机上
/// 升级了 git，新连上的客户端要看到新的版本（Git 页按它核对最低版本，见
/// `fr_core::git::MIN_VERSION`）。
fn git_version() -> String {
    Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "未找到 git".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::tests::fake_registry;
    use fr_core::protocol::{TermExit, TermMarks, TermOpened};
    use std::io::{PipeReader, PipeWriter};

    /// 测试里的一个客户端：往 `to` 写请求，从 `from` 读响应。
    struct Client {
        to: PipeWriter,
        from: BufReader<PipeReader>,
        next: u64,
    }

    fn connect(shared: &Arc<Shared>) -> Client {
        let (request_reader, request_writer) = std::io::pipe().unwrap();
        let (response_reader, response_writer) = std::io::pipe().unwrap();
        let shared = shared.clone();
        std::thread::spawn(move || serve(&shared, request_reader, response_writer));
        Client {
            to: request_writer,
            from: BufReader::new(response_reader),
            next: 1,
        }
    }

    impl Client {
        fn send(&mut self, method: &str, params: Value, payload: &[u8]) -> u64 {
            let id = self.next;
            self.next += 1;
            let request = Request {
                id,
                method: method.into(),
                params,
                bytes: payload.len() as u64,
            };
            protocol::write_message(&mut self.to, &request, payload).unwrap();
            id
        }

        fn next(&mut self) -> (Response, Vec<u8>) {
            protocol::read_message::<_, Response>(&mut self.from)
                .unwrap()
                .expect("连接关了")
        }

        fn call(&mut self, method: &str, params: Value) -> Value {
            let id = self.send(method, params, &[]);
            let (response, _) = self.next();
            assert_eq!(response.id, id);
            assert_eq!(response.error, None);
            response.result.unwrap()
        }
    }

    fn wait_until(what: &str, check: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(Instant::now() < deadline, "等不到：{what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// 仓库目录删掉了：监听以出错结束，客户端过一会儿重新装。原来一直挂在原来那个目录上，之后在
    /// 原处恢复的仓库再怎么改也不通知，客户端也不知道。
    #[test]
    fn 仓库目录没了_监听以出错结束() {
        let dir = files::tests::TempDir::new();
        let repo = dir.0.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        fr_core::git::run(&repo, &["init", "-q"]).unwrap();
        let (terms, _) = fake_registry();
        let shared = Shared::with_dirs(
            terms,
            None,
            "1.0.0-abc".into(),
            dir.0.clone(),
            Workspaces::new(dir.0.join("workspaces.json")),
        );
        let mut client = connect(&shared);
        let id = client.send("watch", json!({ "repo": repo.to_string_lossy() }), &[]);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || while tx.send(client.next().0).is_ok() {});
        // 等监听装好
        std::thread::sleep(Duration::from_millis(800));
        std::fs::write(repo.join("src/a.txt"), "1").unwrap();
        let changed = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!((changed.id, changed.chunk), (id, true));

        // Windows 上刚退出的 git、杀毒可能还开着里面的文件，多试几次
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Err(err) = std::fs::remove_dir_all(&repo) {
            assert!(Instant::now() < deadline, "删不掉仓库目录：{err}");
            std::thread::sleep(Duration::from_millis(100));
        }
        let ended = loop {
            let response = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("仓库目录删掉以后监听没有结束");
            if !response.chunk {
                break response;
            }
        };
        assert_eq!(ended.id, id);
        let error = ended.error.unwrap_or_default();
        assert!(error.contains("repo"), "{error}");
    }

    #[test]
    fn 握手和未知方法() {
        let dir = files::tests::TempDir::new();
        let file = dir.0.join("kuai").join("workspaces.json");
        let (terms, _) = fake_registry();
        let shared = Shared::with_dirs(
            terms,
            None,
            "1.0.0-abc".into(),
            dir.0.clone(),
            Workspaces::new(file.clone()),
        );
        let mut client = connect(&shared);
        let hello: Hello = serde_json::from_value(client.call("hello", json!({}))).unwrap();
        assert_eq!(hello.protocol, PROTOCOL_VERSION);
        assert_eq!(hello.build, "1.0.0-abc");
        assert!(!hello.daemon);
        // 这台电脑的标识：和状态文件放在一起的那个，再握手一次还是它
        assert_eq!(hello.machine, fr_core::store::machine_id(&file).unwrap());
        let again: Hello =
            serde_json::from_value(connect(&shared).call("hello", json!({}))).unwrap();
        assert_eq!(again.machine, hello.machine);
        let id = client.send("没有这个方法", json!({}), &[]);
        let (response, _) = client.next();
        assert_eq!(response.id, id);
        assert!(response.error.unwrap().contains("未知方法"));
        // 不是常驻进程：升级、退出都不行，但连接照常
        client.send("upgrade", json!({ "exe": "/x" }), &[]);
        assert!(client.next().0.error.is_some());
        client.call("ping", json!({}));
    }

    /// 握手时说自己是旧协议（1.18.0 是 15）的连接收不到消息刻度：它会把刻度当成「开好了」解析，
    /// 报格式错误、断开终端（见 `term::Caller`）。同一个终端的新客户端照常收到。
    #[test]
    fn 握手说是旧协议的客户端_收不到消息刻度() {
        let (terms, fakes) = fake_registry();
        let shared = Shared::new(terms, None, String::new());
        let mut old = connect(&shared);
        old.call("hello", json!({ "protocol": 15 }));
        let mut new = connect(&shared);
        new.call("hello", json!({ "protocol": PROTOCOL_VERSION }));
        let open = json!({ "key": "/repo", "cwd": "/repo", "cols": 20, "rows": 4 });
        old.send("term_open", open.clone(), &[]);
        let opened: TermOpened = serde_json::from_value(old.next().0.result.unwrap()).unwrap();
        let fake = fakes.lock().unwrap()[0].clone();
        wait_until("开始读输出", || fake.session.lock().unwrap().is_some());
        let session = fake.session.lock().unwrap().clone().unwrap();
        new.send("term_open", open, &[]);
        assert!(new.next().0.result.is_some());
        // 还原的屏幕
        new.next();
        session.output(br"PS C:\> git status");
        old.next();
        new.next();
        new.send("term_input", json!({ "term": opened.term }), b"\r");
        let marks: TermMarks = serde_json::from_value(new.next().0.result.unwrap()).unwrap();
        assert_eq!(marks.marks.len(), 1);
        // 旧客户端那边，回车以后接着收到的是输出，不是刻度
        session.output(b"!");
        let (response, payload) = old.next();
        assert_eq!((response.result, payload), (None, b"!".to_vec()));
    }

    #[test]
    fn 两个客户端_一个断开_终端留着给另一个() {
        let (terms, fakes) = fake_registry();
        let shared = Shared::new(terms, None, String::new());
        let mut first = connect(&shared);
        let open = json!({ "key": "/repo", "cwd": "/repo", "cols": 20, "rows": 4 });
        let stream = first.send("term_open", open.clone(), &[]);
        let (response, _) = first.next();
        assert_eq!(response.id, stream);
        let opened: TermOpened = serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(opened.created);
        let fake = fakes.lock().unwrap()[0].clone();
        // 「开好了」的回复比开始读输出早一步发出来（先接上再读，第一个字也不漏）
        wait_until("开始读输出", || fake.session.lock().unwrap().is_some());
        let session = fake.session.lock().unwrap().clone().unwrap();

        // 敲的字到了伪终端；输出回到客户端
        first.send("term_input", json!({ "term": opened.term }), b"echo hi\r");
        wait_until("输入写进伪终端", || {
            *fake.written.lock().unwrap() == b"echo hi\r"
        });
        session.output(b"hi\r\n");
        let (response, payload) = first.next();
        assert_eq!((response.id, response.chunk), (stream, true));
        assert_eq!(payload, b"hi\r\n");
        first.send(
            "term_resize",
            json!({ "term": opened.term, "cols": 33, "rows": 9 }),
            &[],
        );
        wait_until("改大小", || fake.size.lock().unwrap().is_some());
        let listed = first.call("term_list", json!({}));
        assert_eq!(listed["terms"][0]["attached"], 1);
        assert_eq!(listed["terms"][0]["cols"], 33);

        // 第一个客户端断开：会话还在，没人接着
        drop(first);
        wait_until("客户端断开", || shared.clients() == 0);
        assert_eq!(shared.terms.count(), 1);
        assert!(!*fake.killed.lock().unwrap());
        assert_eq!(shared.idle_for(), Duration::ZERO, "还有终端会话，不算闲着");

        // 第二个客户端用同一个名字开：接上原来的
        let mut second = connect(&shared);
        let stream = second.send("term_open", open, &[]);
        let (response, _) = second.next();
        let again: TermOpened = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!((again.term.clone(), again.created), (opened.term, false));
        // 还原屏幕的那一段
        let (response, payload) = second.next();
        assert!(response.chunk && !payload.is_empty());
        // 取消这个流：只是断开
        second.send("cancel", json!({ "id": stream }), &[]);
        wait_until("断开终端", || shared.terms.list()[0].attached == 0);
        // 再按编号接上，然后结束它
        let stream = second.send(
            "term_attach",
            json!({ "term": again.term, "cols": 20, "rows": 4 }),
            &[],
        );
        assert!(second.next().0.result.is_some());
        second.next();
        let closed = second.call("term_close", json!({ "term": again.term }));
        assert_eq!(closed["closed"], true);
        assert!(*fake.killed.lock().unwrap());
        session.finish(Some(0));
        let (response, _) = second.next();
        assert_eq!(response.id, stream);
        let exit: TermExit = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(exit.exit, Some(0));
        assert_eq!(shared.terms.count(), 0);
    }

    #[test]
    fn 传文件_分块传上来_断开时没传完的删掉() {
        let dir = files::tests::TempDir::new();
        let (terms, _) = fake_registry();
        let shared = Shared::with_dirs(
            terms,
            None,
            String::new(),
            dir.0.clone(),
            Workspaces::new(dir.0.join("workspaces.json")),
        );
        let mut client = connect(&shared);
        // 第一块给名字，拿到编号
        let id = client.send(
            "file_put",
            json!({ "name": "说明 1.txt" }),
            "你好，".as_bytes(),
        );
        let (response, _) = client.next();
        assert_eq!(response.id, id);
        let first = response.result.unwrap();
        assert_eq!(first["received"], 9);
        assert!(first.get("path").is_none());
        let upload = first["upload"].as_str().unwrap().to_string();
        let done = json!({ "upload": upload, "done": true });
        let id = client.send("file_put", done, "世界".as_bytes());
        let (response, _) = client.next();
        assert_eq!(response.id, id);
        let last = response.result.unwrap();
        assert_eq!(last["received"], 15);
        let path = PathBuf::from(last["path"].as_str().unwrap());
        assert_eq!(path, dir.0.join("说明_1.txt"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "你好，世界");
        // 编号不对：回错误，连接照常
        client.send("file_put", json!({ "upload": "u99" }), b"x");
        assert!(client.next().0.error.is_some());
        // 传到一半客户端断开：没传完的不留
        client.send("file_put", json!({ "name": "half.bin" }), b"123");
        assert!(client.next().0.result.is_some());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
        drop(client);
        wait_until("没传完的删掉", || {
            std::fs::read_dir(&dir.0).unwrap().count() == 1
        });
        assert!(path.exists());
    }

    #[test]
    fn 仓库里的文件_列_读_写_改名_删除_往里传() {
        let repo = files::tests::TempDir::new();
        std::fs::create_dir_all(repo.0.join(".git")).unwrap();
        let root = repo.0.to_str().unwrap().to_string();
        let (terms, _) = fake_registry();
        let shared = Shared::new(terms, None, String::new());
        let mut client = connect(&shared);
        let at = |path: &str| json!({ "repo": root, "path": path });

        // 新建，再按打开时的样子保存；拿旧的样子保存就是冲突
        let create = json!({ "repo": root, "path": "src/a.txt", "create": true });
        let id = client.send("fs_write", create, b"one");
        let (response, _) = client.next();
        assert_eq!(response.id, id);
        let opened = response.result.unwrap()["stat"].clone();
        assert_eq!(opened["size"], 3);
        let save = json!({ "repo": root, "path": "src/a.txt", "expect": opened });
        client.send("fs_write", save.clone(), b"two!");
        assert!(client.next().0.result.unwrap().get("conflict").is_none());
        client.send("fs_write", save, b"three");
        assert_eq!(client.next().0.result.unwrap()["conflict"], true);

        // 仓库目录里面的仓库：给相对路径，node_modules 里的不报
        assert_eq!(client.call("fs_repos", at(""))["repos"], json!([]));
        std::fs::create_dir_all(repo.0.join("packages/plugin-a/.git")).unwrap();
        std::fs::create_dir_all(repo.0.join("node_modules/dep/.git")).unwrap();
        let found = client.call("fs_repos", at(""));
        assert_eq!(found["repos"], json!(["packages/plugin-a"]));
        assert!(found.get("truncated").is_none());
        std::fs::remove_dir_all(repo.0.join("packages")).unwrap();
        std::fs::remove_dir_all(repo.0.join("node_modules")).unwrap();

        // 列目录没有 .git；读带着文件现在的情况，可以只读一段
        let listed = client.call("fs_list", at(""));
        assert_eq!(listed["entries"].as_array().unwrap().len(), 1);
        assert_eq!(listed["entries"][0]["name"], "src");
        assert_eq!(listed["entries"][0]["dir"], true);
        let id = client.send(
            "fs_read",
            json!({ "repo": root, "path": "src/a.txt", "offset": 1, "len": 2 }),
            &[],
        );
        let (response, payload) = client.next();
        assert_eq!(response.id, id);
        assert_eq!(response.result.unwrap()["size"], 4);
        assert_eq!(payload, b"wo");
        // 出仓库的路径、不存在的文件：回错误
        client.send("fs_read", at("../x"), &[]);
        assert!(client.next().0.error.is_some());
        assert_eq!(client.call("fs_stat", at("nope"))["exists"], false);

        // 往仓库里传：传完才出现正式的名字；已经有了、又没说要换，就不传
        let first = json!({ "repo": root, "path": "up/大 文件.bin" });
        client.send("file_put", first.clone(), b"12");
        let upload = client.next().0.result.unwrap()["upload"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!repo.0.join("up/大 文件.bin").exists());
        client.send("file_put", json!({ "upload": upload, "done": true }), b"34");
        let last = client.next().0.result.unwrap();
        assert_eq!(
            PathBuf::from(last["path"].as_str().unwrap()),
            repo.0.join("up/大 文件.bin")
        );
        assert_eq!(
            std::fs::read(repo.0.join("up/大 文件.bin")).unwrap(),
            b"1234"
        );
        assert_eq!(std::fs::read_dir(repo.0.join("up")).unwrap().count(), 1);
        client.send("file_put", first, b"x");
        assert!(client.next().0.error.unwrap().contains("已存在"));
        let replace =
            json!({ "repo": root, "path": "up/大 文件.bin", "overwrite": true, "done": true });
        client.send("file_put", replace, b"new");
        assert!(client.next().0.result.is_some());
        assert_eq!(
            std::fs::read(repo.0.join("up/大 文件.bin")).unwrap(),
            b"new"
        );

        // 映射到本机用的（协议 9）：按偏移写、截断、改时间、盘的大小、.git 也列、盖掉已有的、只删空目录
        let at_offset = |path: &str, offset: u64, create: bool| json!({ "repo": root, "path": path, "offset": offset, "create": create });
        client.send("fs_pwrite", at_offset("m/a.bin", 0, true), b"hello");
        assert!(client.next().0.error.is_some(), "目录不在不会顺手建");
        client.call("fs_mkdir", at("m"));
        client.send("fs_pwrite", at_offset("m/a.bin", 0, true), b"hello");
        assert_eq!(client.next().0.result.unwrap()["size"], 5);
        client.send("fs_pwrite", at_offset("m/a.bin", 3, false), b"LO!");
        assert_eq!(client.next().0.result.unwrap()["size"], 6);
        assert_eq!(std::fs::read(repo.0.join("m/a.bin")).unwrap(), b"helLO!");
        let resized = client.call(
            "fs_resize",
            json!({ "repo": root, "path": "m/a.bin", "size": 2 }),
        );
        assert_eq!(resized["size"], 2);
        client.send(
            "fs_resize",
            json!({ "repo": root, "path": "m/a.bin", "size": 0, "new": true }),
            &[],
        );
        assert!(client.next().0.error.unwrap().contains("已存在"));
        let touched = client.call(
            "fs_touch",
            json!({ "repo": root, "path": "m/a.bin", "mtime": 1_700_000_000_000u64 }),
        );
        assert_eq!(touched["mtime"], 1_700_000_000_000u64);
        assert!(client.call("fs_space", at(""))["total"].as_u64().unwrap() > 0);
        let all = client.call("fs_list", json!({ "repo": root, "path": "", "all": true }));
        let names: Vec<&str> = all["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&".git") && names.contains(&"m"));
        std::fs::write(repo.0.join("m/b.bin"), b"b").unwrap();
        let onto = json!({ "repo": root, "path": "m/a.bin", "to": "m/b.bin" });
        client.send("fs_rename", onto.clone(), &[]);
        assert!(client.next().0.error.is_some());
        let mut over = onto;
        over["replace"] = json!(true);
        client.call("fs_rename", over);
        assert_eq!(std::fs::read(repo.0.join("m/b.bin")).unwrap(), b"he");
        let shallow = |path: &str| json!({ "repo": root, "path": path, "shallow": true });
        client.send("fs_remove", shallow("m"), &[]);
        assert!(client.next().0.error.is_some(), "目录里有东西不删");
        client.call("fs_remove", shallow("m/b.bin"));
        client.call("fs_remove", shallow("m"));
        assert!(!repo.0.join("m").exists());

        // 协议 17：追加（同一个文件的写入按收到的先后做，连着发的几段不乱）、改权限、按种类删
        client.call("fs_mkdir", at("n"));
        client.send("fs_pwrite", at_offset("n/log", 0, true), b"");
        assert!(client.next().0.result.is_some());
        let pieces: Vec<String> = (0..50).map(|n| format!("{n:02};")).collect();
        let append = json!({ "repo": root, "path": "n/log", "offset": 0, "append": true });
        for piece in &pieces {
            client.send("fs_pwrite", append.clone(), piece.as_bytes());
        }
        for _ in &pieces {
            assert!(client.next().0.result.is_some());
        }
        assert_eq!(
            std::fs::read_to_string(repo.0.join("n/log")).unwrap(),
            pieces.concat()
        );
        #[cfg(unix)]
        {
            let mode = json!({ "repo": root, "path": "n/log", "mode": 0o750 });
            assert_eq!(client.call("fs_chmod", mode)["mode"], 0o750);
        }
        let kind = |path: &str, kind: &str| json!({ "repo": root, "path": path, "kind": kind });
        client.send("fs_remove", kind("n/log", "dir"), &[]);
        assert!(client.next().0.error.is_some(), "说是目录的不删文件");
        client.call("fs_remove", kind("n/log", "file"));
        client.send("fs_remove", kind("n", "file"), &[]);
        assert!(client.next().0.error.is_some(), "说是文件的不删目录");
        client.call("fs_remove", kind("n", "dir"));
        assert!(!repo.0.join("n").exists());

        // 建目录、改名、删除
        client.call("fs_mkdir", at("docs/图"));
        client.call(
            "fs_rename",
            json!({ "repo": root, "path": "src/a.txt", "to": "docs/b.txt" }),
        );
        assert_eq!(std::fs::read(repo.0.join("docs/b.txt")).unwrap(), b"two!");
        client.call("fs_remove", at("docs"));
        assert!(!repo.0.join("docs").exists());
        client.send("fs_remove", at(""), &[]);
        assert!(client.next().0.error.is_some());
    }

    #[test]
    fn 仓库和工作区_一边改了_订阅的都收到整份_存进文件() {
        let dir = files::tests::TempDir::new();
        let file = dir.0.join("kuai").join("workspaces.json");
        let (terms, _) = fake_registry();
        let shared = Shared::with_dirs(
            terms,
            None,
            String::new(),
            dir.0.clone(),
            Workspaces::new(file.clone()),
        );
        let mut first = connect(&shared);
        let mut second = connect(&shared);
        // 订阅：马上收到现在的（还没有文件，是空的）
        let watch = second.send("state_watch", json!({}), &[]);
        let (response, _) = second.next();
        assert!(response.chunk && response.id == watch);
        let store: fr_core::store::Store =
            serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(store.version, 0);
        assert!(!file.exists(), "只是读，不写文件");

        // 第一个客户端改：回改完的整份；订阅的那个也收到。这台电脑上的仓库按这台的写法存
        // （Windows 上是反斜杠），这里给这台上的写法
        let repo = if cfg!(windows) {
            r"C:\data\wms"
        } else {
            "/data/wms"
        };
        let ops = json!({ "ops": [
            { "op": "add_workspace", "id": "w1", "name": "交付" },
            { "op": "add_to_workspace", "id": "w1", "repo": { "path": repo } },
        ]});
        let applied = first.call("state_apply", ops);
        assert_eq!(applied["version"], 1);
        assert_eq!(applied["repos"], json!([repo]));
        let (response, _) = second.next();
        assert_eq!((response.id, response.chunk), (watch, true));
        assert_eq!(response.result.unwrap()["workspaces"][0]["name"], "交付");
        // 存进了文件，别的进程读得到
        assert_eq!(fr_core::store::load(&file).version, 1);
        assert_eq!(first.call("state_get", json!({}))["version"], 1);

        // 做不了的改动：回错误，什么都不变，也不推
        let bad = json!({ "ops": [{ "op": "rename_workspace", "id": "没有", "name": "x" }] });
        let id = first.send("state_apply", bad, &[]);
        let (response, _) = first.next();
        assert_eq!(response.id, id);
        assert!(response.error.is_some());
        // 不订阅了：再改也不推给它
        second.send("cancel", json!({ "id": watch }), &[]);
        second.call("ping", json!({}));
        let rename = json!({ "ops": [{ "op": "rename_workspace", "id": "w1", "name": "WMS" }] });
        assert_eq!(first.call("state_apply", rename)["version"], 2);
        second.call("ping", json!({}));
    }

    #[test]
    fn 开不了终端时回错误() {
        let shared = Shared::new(Registry::unsupported(), None, String::new());
        let mut client = connect(&shared);
        let id = client.send(
            "term_open",
            json!({ "key": "k", "cwd": "/nowhere", "cols": 80, "rows": 24 }),
            &[],
        );
        let (response, _) = client.next();
        assert_eq!(response.id, id);
        assert!(response.error.unwrap().contains("/nowhere"));
        let id = client.send("term_attach", json!({ "term": "t9" }), &[]);
        let (response, _) = client.next();
        assert_eq!(response.id, id);
        assert!(response.error.is_some());
    }

    #[test]
    fn git_的输出照常回来() {
        let (terms, _) = fake_registry();
        let shared = Shared::new(terms, None, String::new());
        let mut client = connect(&shared);
        let id = client.send("git", json!({ "repo": ".", "args": ["--version"] }), &[]);
        let (response, payload) = client.next();
        assert_eq!(response.id, id);
        let result: GitResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(result.code, Some(0));
        assert!(String::from_utf8_lossy(&payload).starts_with("git version"));
    }

    /// 客户端取消 git 时不丢下请求，等这边回话（[`fr_channel`] 的 `git_cancellable` 靠它知道 git
    /// 真的退出了）：杀掉以后照常回一个结果，退出码是空的。
    #[test]
    fn 取消的_git_退出以后照常回话() {
        let (terms, _) = fake_registry();
        let shared = Shared::new(terms, None, String::new());
        let mut client = connect(&shared);
        let args = json!(["-c", "alias.slow=!sleep 60", "slow"]);
        let id = client.send("git", json!({ "repo": ".", "args": args }), &[]);
        let started = Instant::now();
        wait_until("git 开始跑", || {
            shared
                .conns
                .lock()
                .unwrap()
                .values()
                .any(|conn| conn.running.lock().unwrap().contains_key(&id))
        });
        client.send("cancel", json!({ "id": id }), &[]);
        let (response, _) = client.next();
        assert_eq!(response.id, id);
        let result: GitResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_ne!(result.code, Some(0));
        assert!(started.elapsed() < Duration::from_secs(30));
    }
}
