//! 服务一个客户端：读它的请求，执行，把结果放进它的发送队列。
//!
//! 常驻时每个客户端（一条 ssh 连接经转接程序接进来）一个线程跑 [`serve`]；`--stdio` 时就一个。
//! 终端会话不属于哪个客户端，记在 [`Shared`] 里，客户端断开时只是从会话上断开。

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use fr_core::exec::{self, Killer, OnStderr};
use fr_core::protocol::{
    self, DaemonInfo, GitResult, Hello, PROTOCOL_VERSION, Request, Response, TermOpen,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::VERSION;
use crate::out::Outbox;
use crate::term::{Registry, Size};

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
    /// 最近一次有客户端连着的时间：闲了多久从它算
    last_busy: Mutex<Instant>,
}

impl Shared {
    pub fn new(terms: Registry, control: Option<Box<dyn Control>>, build: String) -> Arc<Shared> {
        Arc::new(Shared {
            terms,
            conns: Mutex::default(),
            next_conn: AtomicU64::new(1),
            control,
            build,
            last_busy: Mutex::new(Instant::now()),
        })
    }

    pub fn clients(&self) -> usize {
        self.conns.lock().unwrap().len()
    }

    /// 没有客户端连着、也没有终端会话，已经这样多久了。还有的话是 0。
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn idle_for(&self) -> Duration {
        let mut last_busy = self.last_busy.lock().unwrap();
        if self.clients() > 0 || self.terms.count() > 0 {
            *last_busy = Instant::now();
            return Duration::ZERO;
        }
        last_busy.elapsed()
    }

    /// 杀掉所有客户端正在跑的 git（升级换程序前）。
    #[cfg_attr(not(unix), allow(dead_code))]
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
}

impl Conn {
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

    fn kill_git(&self) {
        for killer in self.running.lock().unwrap().values() {
            killer.kill();
        }
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
struct TermParams {
    term: String,
    #[serde(default)]
    cols: u16,
    #[serde(default)]
    rows: u16,
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
    drop(watchers);
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
                if let Some(killer) = conn.running.lock().unwrap().get(&target) {
                    killer.kill();
                }
                watchers.remove(&target);
                shared.terms.detach(conn.id, Some(target));
            }
        }
        "watch" => {
            let repo = param("repo").and_then(Value::as_str).unwrap_or_default();
            let out = conn.out.clone();
            let started = fr_core::watch::watch(Path::new(repo), move || {
                let changed = Response {
                    id,
                    chunk: true,
                    ..Default::default()
                };
                out.push(&changed, &[]);
            });
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
            let reported = param("auth_sock").and_then(Value::as_str).map(str::to_string);
            *conn.auth_sock.lock().unwrap() = reported;
        }
        "term_open" => {
            let opened = serde_json::from_value::<TermOpen>(request.params)
                .context("term_open 参数不对")
                .and_then(|params| {
                    let env = term_env(shared, conn);
                    shared.terms.open(&params, env, conn.id, id, &conn.out)
                });
            if let Err(err) = opened {
                conn.reply(id, Err(err));
            }
        }
        "term_attach" => {
            let attached = term_params(&request).and_then(|params| {
                let size = Size::new(params.cols, params.rows);
                shared.terms.attach(&params.term, size, conn.id, id, &conn.out)
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
        "upgrade" => {
            let exe = param("exe").and_then(Value::as_str).unwrap_or_default();
            let failed = match &shared.control {
                Some(control) => control.upgrade(shared, exe),
                None => anyhow::anyhow!("不是常驻进程，不能升级"),
            };
            conn.reply(id, Err(failed));
        }
        "shutdown" => match &shared.control {
            Some(control) => {
                conn.reply(id, Ok(json!({})));
                conn.out.drain(Duration::from_secs(2));
                control.shutdown(shared);
            }
            None => conn.reply(id, Err(anyhow::anyhow!("不是常驻进程"))),
        },
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
            git: git_version().into(),
            home: fr_core::dir::home(),
            build: shared.build.clone(),
            daemon: shared.control.is_some(),
        })?),
        "info" => Ok(serde_json::to_value(shared.info())?),
        "list_dir" => Ok(serde_json::to_value(fr_core::dir::list_dir(
            param("path").unwrap_or(""),
        )?)?),
        "term_list" => Ok(json!({ "terms": shared.terms.list() })),
        "term_close" => {
            let closed = shared.terms.close(param("term").unwrap_or_default());
            Ok(json!({ "closed": closed }))
        }
        other => bail!("未知方法：{other}"),
    }
}

fn term_params(request: &Request) -> Result<TermParams> {
    serde_json::from_value(request.params.clone()).context("终端的参数不对")
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
    let params: GitParams = match serde_json::from_value(request.params).context("git 参数不对")
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

/// git 版本只查一次。
fn git_version() -> &'static str {
    static GIT: OnceLock<String> = OnceLock::new();
    GIT.get_or_init(|| {
        Command::new("git")
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "未找到 git".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::tests::fake_registry;
    use fr_core::protocol::{TermExit, TermOpened};
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

    #[test]
    fn 握手和未知方法() {
        let (terms, _) = fake_registry();
        let shared = Shared::new(terms, None, "1.0.0-abc".into());
        let mut client = connect(&shared);
        let hello: Hello = serde_json::from_value(client.call("hello", json!({}))).unwrap();
        assert_eq!(hello.protocol, PROTOCOL_VERSION);
        assert_eq!(hello.build, "1.0.0-abc");
        assert!(!hello.daemon);
        let id = client.send("没有这个方法", json!({}), &[]);
        let (response, _) = client.next();
        assert_eq!(response.id, id);
        assert!(response.error.unwrap().contains("未知方法"));
        // 不是常驻进程：升级、退出都不行，但连接照常
        client.send("upgrade", json!({ "exe": "/x" }), &[]);
        assert!(client.next().0.error.is_some());
        client.call("ping", json!({}));
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
        first.send("term_resize", json!({ "term": opened.term, "cols": 33, "rows": 9 }), &[]);
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
}
