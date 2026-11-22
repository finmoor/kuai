//! 服务器上的仓库：经一条 ssh 连接和 kuaid 通信，所有请求按编号在这条连接上复用。
//!
//! 用系统自带的 ssh（Windows 上是 `System32\OpenSSH\ssh.exe`），沿用用户的 `~/.ssh/config` 和密钥。
//! Windows 版 OpenSSH 不支持 ControlMaster，所以每台服务器只开一条连接，复用自己做。
//!
//! kuaid 在服务器上常驻：ssh 过去跑的是它的转接程序（`kuaid --connect`），由它接到常驻进程上，
//! 没在跑就拉起来，比客户端旧就让它原地换成新的。所以这条连接断了，开在 kuaid 里的终端还在。
//!
//! 线程：一个写线程把请求依次写进 ssh 的标准输入；一个读线程读响应，按编号交给等待的 future。
//!
//! 协议 10 起还可以经另一台电脑的 kuaid 连它配置的服务器（[`Connection::connect_via`]）：ssh 由那台
//! 电脑起，标准输入输出经和它的连接转（`relay_open`），这边在上面做的和直接连一样。

use fr_core::i18n::tr;
use fr_core::tf;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Cursor, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use fr_core::hop;
use fr_core::protocol::{
    self, DirListing, FilePut, FilePutReply, FsEntry, FsStat, FsWrite, FsWritten, GitResult, Hello,
    PROTOCOL_VERSION, Request, Response, SubRepos, TermExit, TermInfo, TermMarks, TermOpen,
    TermOpened,
};
use fr_core::ssh::{
    ACCEPT_NEW_HOSTS, KEEP_OPTIONS, STAGE_OPTIONS, no_window, program as ssh_program,
};
use fr_core::store::{Op, Store};
use futures::FutureExt as _;
use futures::channel::{mpsc, oneshot};
use futures::future::{self, BoxFuture};
use serde_json::{Value, json};

use crate::binaries::{Binaries, Os};
use crate::{
    Backlog, ChangeStream, GitCancel, GitOutput, GitStream, StateStream, StreamItem, TermItem,
    TermListStream, TermStream, UploadItem, UploadSource, UploadStream,
};

/// 启动脚本找到 kuaid 时先打这一行；前面的输出（比如 `.bashrc` 打印的东西）都跳过
const START_MARKER: &str = "KUAID-START";
/// 没装 kuaid 时打这一行，后面跟系统和 CPU 架构（Linux 上是 `uname -s` 和 `uname -m`）
const MISSING_MARKER: &str = "KUAID-MISSING";
/// ssh 的标准错误留最后这么多，连接失败时拿来提示
const STDERR_TAIL: usize = 4096;
/// 上传文件时一块这么大：每块等 kuaid 回一声再发下一块，进度和中止都按块算
const UPLOAD_CHUNK: usize = 512 * 1024;
/// 经另一台电脑转接时，写给那边 ssh 的标准输入一条消息最多带这么多
const RELAY_CHUNK: usize = 256 * 1024;

/// 和一台服务器的连接。丢弃时断开。
pub struct Connection {
    /// ssh 的目标：主机别名、`user@host` 或 `ssh://user@host:port`
    pub destination: String,
    pub hello: Hello,
    /// 从启动 ssh 到握手完成用的时间（含上传 kuaid）
    pub connect_time: Duration,
    /// 这次连接时上传了 kuaid
    pub installed: bool,
    /// 连的是这台电脑上的 kuaid（[`Connection::connect_local`]），不经 ssh
    pub local: bool,
    inner: Arc<Inner>,
}

struct Inner {
    /// 交给写线程的请求；取走（变成 None）就关闭连接
    outgoing: Mutex<Option<Outgoing>>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
    alive: AtomicBool,
    link: Arc<Link>,
    stderr: Arc<StderrLog>,
}

/// 交给写线程的一条请求和它附带的字节
type Outgoing = std::sync::mpsc::Sender<(Request, Vec<u8>)>;

/// 一个请求的回音：结果和附带的字节
type Reply = BoxFuture<'static, Result<(Value, Vec<u8>)>>;

enum Pending {
    Call(oneshot::Sender<Result<(Value, Vec<u8>)>>),
    Stream(mpsc::UnboundedSender<Result<StreamItem>>),
    Watch(mpsc::UnboundedSender<()>),
    /// 终端的输出，和积着还没取走的有多少（见 [`crate::TERM_BACKLOG`]）
    Term(mpsc::UnboundedSender<Result<TermItem>>, Arc<Backlog>),
    /// `state_watch`：每次推来的整份
    State(mpsc::UnboundedSender<Result<Store>>),
    /// `term_watch`：每次推来的整份终端列表
    TermList(mpsc::UnboundedSender<Result<Vec<TermInfo>>>),
    /// `relay_open`：经这条连接连的另一台服务器，ssh 的输出交给那条连接
    Relay(RelaySink),
}

/// 连接底下的那一段。
enum Link {
    /// 本机起的进程：ssh，或者本机的 kuaid
    Process(Mutex<Child>),
    /// 经另一台电脑的 kuaid 转接（协议 10）
    Relay(RelayLink),
}

impl Link {
    /// 等它结束，返回退出码（被杀掉、经转接时转接断了为空）。
    fn wait(&self) -> Option<i32> {
        match self {
            Link::Process(child) => child
                .lock()
                .unwrap()
                .wait()
                .ok()
                .and_then(|status| status.code()),
            Link::Relay(relay) => relay.exit.wait(),
        }
    }

    /// 结束它。`tree`：连它启动的进程一起（经跳板机的 ssh 下面挂着转接的那几层，见 [`kill_tree`]）。
    fn kill(&self, tree: bool) {
        match self {
            Link::Process(child) => {
                let mut child = child.lock().unwrap();
                match tree {
                    true => kill_tree(&mut child),
                    false => drop(child.kill()),
                }
            }
            Link::Relay(relay) => relay.close(),
        }
    }
}

/// 经另一台电脑的 kuaid 转接：那边的 ssh 是它的 `relay_open` 请求。
struct RelayLink {
    /// 转接的那台电脑的连接：这条连接用着，它就不能断
    via: Arc<Connection>,
    /// `relay_open` 的请求编号
    id: u64,
    exit: Arc<RelayExit>,
}

impl RelayLink {
    /// 不要了：那边的 ssh 结束掉，这边读输出的两头随之读到结尾。
    fn close(&self) {
        self.exit.set(None);
        let removed = self.via.inner.pending.lock().unwrap().remove(&self.id);
        if removed.is_some() {
            let cancel = Request {
                id: 0,
                method: "cancel".into(),
                params: json!({ "id": self.id }),
                bytes: 0,
            };
            let _ = self.via.inner.send(cancel, Vec::new());
        }
    }
}

/// 转接那边的 ssh 的退出码：结束了才有。
#[derive(Default)]
struct RelayExit {
    code: Mutex<Option<Option<i32>>>,
    done: Condvar,
}

impl RelayExit {
    /// 记下退出码；已经记过就不改。
    fn set(&self, code: Option<i32>) {
        let mut slot = self.code.lock().unwrap();
        if slot.is_none() {
            *slot = Some(code);
        }
        self.done.notify_all();
    }

    fn wait(&self) -> Option<i32> {
        let mut slot = self.code.lock().unwrap();
        loop {
            if let Some(code) = *slot {
                return code;
            }
            slot = self.done.wait(slot).unwrap();
        }
    }
}

/// 转接那边的 ssh 的输出往哪送：经它连的那条连接读的两头。丢弃就是读到结尾。
struct RelaySink {
    stdout: std::sync::mpsc::Sender<Vec<u8>>,
    stderr: std::sync::mpsc::Sender<Vec<u8>>,
    exit: Arc<RelayExit>,
}

impl RelaySink {
    fn chunk(&self, err: bool, payload: Vec<u8>) {
        let _ = match err {
            true => self.stderr.send(payload),
            false => self.stdout.send(payload),
        };
    }

    /// ssh 结束了（`Ok` 里是结果 `{code}`），或者开不起来、转接的连接断了（`Err`，原因当作 ssh 的报错）。
    fn finish(self, outcome: Result<Value>) {
        match outcome {
            Ok(result) => {
                let code = result.get("code").and_then(Value::as_i64);
                self.exit.set(code.map(|code| code as i32));
            }
            Err(err) => {
                let _ = self.stderr.send(format!("{err:#}\n").into_bytes());
                self.exit.set(None);
            }
        }
    }
}

/// 从转接送来的一块块输出里读，像读管子一样。送的那头丢弃了就是读到结尾。
struct ChannelReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    at: usize,
}

impl ChannelReader {
    fn new(rx: std::sync::mpsc::Receiver<Vec<u8>>) -> ChannelReader {
        ChannelReader {
            rx,
            chunk: Vec::new(),
            at: 0,
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.at >= self.chunk.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.at = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.chunk.len() - self.at);
        out[..n].copy_from_slice(&self.chunk[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// 往转接那边的 ssh 的标准输入写：每次写变成一条 `relay_input`。丢弃时关掉那边的标准输入。
struct RelayWriter {
    via: Arc<Connection>,
    id: u64,
}

impl RelayWriter {
    fn send(&self, bytes: &[u8], eof: bool) -> std::io::Result<()> {
        let request = Request {
            id: 0,
            method: "relay_input".into(),
            params: json!({ "relay": self.id, "eof": eof }),
            bytes: bytes.len() as u64,
        };
        self.via
            .inner
            .send(request, bytes.to_vec())
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::BrokenPipe, format!("{err:#}")))
    }
}

impl Write for RelayWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = bytes.len().min(RELAY_CHUNK);
        self.send(&bytes[..n], false)?;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for RelayWriter {
    fn drop(&mut self) {
        let _ = self.send(&[], true);
    }
}

/// 连接底下那一段的三根管子，和它本身（结束、等它退出用）。
struct Pipes {
    stdin: Box<dyn Write + Send>,
    stdout: Box<dyn Read + Send>,
    stderr: Box<dyn Read + Send>,
    link: Arc<Link>,
}

/// 起本机的进程（ssh，或者 kuaid 本身），接上它的标准输入输出。
fn spawn_pipes(mut cmd: Command) -> Result<Pipes> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_window(&mut cmd);
    let mut child = cmd.spawn().with_context(|| {
        tf!(
            "无法启动 {to_string_lossy}",
            to_string_lossy = cmd.get_program().to_string_lossy()
        )
    })?;
    let stdin = child.stdin.take().context(tr("无法获取 ssh 的标准输入"))?;
    let stdout = child.stdout.take().context(tr("无法获取 ssh 的标准输出"))?;
    let stderr = child.stderr.take().context(tr("无法获取 ssh 的标准错误"))?;
    Ok(Pipes {
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        link: Arc::new(Link::Process(Mutex::new(child))),
    })
}

/// 请 `via` 那台电脑的 kuaid 起 ssh 连它配置的服务器 `server`，在上面执行 `command`。
fn relay_pipes(via: &Arc<Connection>, server: &str, command: &str) -> Result<Pipes> {
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    let exit = Arc::new(RelayExit::default());
    let sink = RelaySink {
        stdout: out_tx,
        stderr: err_tx,
        exit: exit.clone(),
    };
    let inner = &via.inner;
    let id = inner.next_id();
    inner
        .pending
        .lock()
        .unwrap()
        .insert(id, Pending::Relay(sink));
    let request = Request {
        id,
        method: "relay_open".into(),
        params: json!({ "server": server, "command": command }),
        bytes: 0,
    };
    if let Err(err) = inner.send(request, Vec::new()) {
        inner.pending.lock().unwrap().remove(&id);
        return Err(err);
    }
    Ok(Pipes {
        stdin: Box::new(RelayWriter {
            via: via.clone(),
            id,
        }),
        stdout: Box::new(ChannelReader::new(out_rx)),
        stderr: Box::new(ChannelReader::new(err_rx)),
        link: Arc::new(Link::Relay(RelayLink {
            via: via.clone(),
            id,
            exit,
        })),
    })
}

/// 经哪条路连服务器。
#[derive(Clone, Copy)]
enum Route<'a> {
    /// 本机起 ssh
    Direct,
    /// 经 `via` 那台电脑的 kuaid 连它配置的服务器 `server`（协议 10）
    Via {
        via: &'a Arc<Connection>,
        server: &'a str,
    },
}

impl Route<'_> {
    /// 在服务器上执行 `script`。返回的第二项要留到登录完成（见 [`login`]）。
    fn run(
        &self,
        destination: &str,
        script: &str,
        options: &ConnectOptions,
        forward_agent: bool,
    ) -> Result<(Pipes, Vec<crate::SavedSecret>)> {
        match self {
            Route::Direct => {
                let (cmd, saved) = ssh(destination, script, options, forward_agent);
                Ok((spawn_pipes(cmd)?, saved))
            }
            // 登录方式、要不要转发 ssh-agent 都照那台电脑的配置
            Route::Via { via, server } => Ok((relay_pipes(via, server, script)?, Vec::new())),
        }
    }

    /// 记「这台是 Windows」用的键。
    fn key(&self, destination: &str) -> String {
        match self {
            Route::Direct => destination.to_string(),
            Route::Via { via, .. } => format!("{}>{destination}", via.destination),
        }
    }
}

enum Started {
    Ready(Arc<Inner>, Hello),
    Missing {
        os: String,
        arch: String,
    },
    /// 登录上了，但服务器跑不了启动脚本（比如拿 Linux 的写法去问 Windows）。里面是报错
    Refused(String),
}

/// 已经知道是 Windows 的服务器（按 ssh 目标记，只在这次运行期间）：再连时直接用 Windows 的写法，
/// 不先按 Linux 试一次。
static WINDOWS_SERVERS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn known_windows(destination: &str) -> bool {
    WINDOWS_SERVERS
        .lock()
        .unwrap()
        .iter()
        .any(|known| known == destination)
}

fn remember_windows(destination: &str, windows: bool) {
    let mut known = WINDOWS_SERVERS.lock().unwrap();
    known.retain(|item| item != destination);
    if windows {
        known.push(destination.to_string());
    }
}

/// 登录被拒绝了（ssh 说 Permission denied）：密钥、密码不对，或者用户取消了输入。
#[derive(Debug)]
struct LoginDenied(String);

impl std::fmt::Display for LoginDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoginDenied {}

/// 存着的私钥口令只在只用公钥认证的那次 ssh 里自动回答（见 [`login`]）：服务器在键盘交互认证
/// 里也能问一句像解私钥的话，8.3 以前的 OpenSSH（Windows 10 自带的就是）分不出是谁在问。
/// 只用公钥登录不上（服务器还要键盘交互的第二步，比如动态口令）时，不带存着的口令再连一次：
/// ssh 问的每一句都弹窗给用户看。不许弹窗的（启动时自动连接）不再连。
fn with_plain_retry<T>(
    options: &ConnectOptions,
    attempt: impl Fn(&ConnectOptions) -> Result<T>,
) -> Result<T> {
    let result = attempt(options);
    let denied = matches!(&result, Err(err) if err.downcast_ref::<LoginDenied>().is_some());
    if !denied || options.no_prompt || !options.saved_passphrase() {
        return result;
    }
    let mut plain = options.clone();
    if !plain.password_auth {
        plain.secret = None;
    }
    for jump in &mut plain.jumps {
        if !jump.password_auth {
            jump.secret = None;
        }
    }
    attempt(&plain)
}

/// 连接选项。
#[derive(Clone, Default)]
pub struct ConnectOptions {
    /// 转发本机的 ssh-agent（`ssh -A`）：服务器上的 git 推拉可以用本机的密钥。
    /// 服务器上有 root 权限的人在连接期间也能用它，只对信任的服务器打开
    pub forward_agent: bool,
    /// 私钥文件（`ssh -i`）。空着用 ssh 默认的：`~/.ssh` 里的密钥、ssh-agent、`~/.ssh/config`
    pub identity_file: Option<String>,
    /// 用密码登录，不试密钥
    pub password_auth: bool,
    /// 存着的密码（`password_auth`）或私钥口令：ssh 问的时候自动回答一次。空着就弹窗问
    pub secret: Option<String>,
    /// 依次经过的跳板机：第一台从本机直接连，后一台经前一台转接，目标服务器经最后一台。
    /// 跳板机上不装、不运行任何程序，只借它转发连接（见 `hop`）
    pub jumps: Vec<Jump>,
    /// 经跳板机连接时报告走到哪了：参数是已经登录上的台数（跳板机按顺序算，最后是目标服务器）。
    /// 在后台线程里调用
    pub on_stage: Option<StageHandler>,
    /// 要密码、口令、确认主机密钥时不弹窗，存着的答不了就当作取消（启动时自动连接用：没人点它，
    /// 突然弹窗问密码不合适）。跳板机也一样
    pub no_prompt: bool,
    /// 没见过的主机不问，直接记下它的密钥（目标服务器和跳板机都是）。只给没有界面的测试工具用：
    /// 界面上一律让用户核对指纹
    pub accept_new_host_keys: bool,
}

/// 测试连接的结果（见 [`Connection::probe`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// 服务器的系统：`Linux`、`Windows`，别的是它自己报的名字（`Darwin`…）
    pub os: String,
    /// 处理器架构，服务器自己报的（`x86_64`、`AMD64`、`aarch64`…）
    pub arch: String,
    /// 这种系统上装不装得了 kuaid
    pub supported: bool,
}

/// 报告连接进度的函数，见 [`ConnectOptions::on_stage`]。
pub type StageHandler = Arc<dyn Fn(usize) + Send + Sync>;

/// 一台跳板机：登录方式各用各的。
#[derive(Clone, Default)]
pub struct Jump {
    /// 显示用的名字（报错、要密码时写明是哪一台）
    pub name: String,
    /// ssh 的目标
    pub destination: String,
    pub identity_file: Option<String>,
    pub password_auth: bool,
    pub secret: Option<String>,
}

impl ConnectOptions {
    /// 目标服务器或哪台跳板机存着私钥口令（用密钥登录、存了口令）。
    fn saved_passphrase(&self) -> bool {
        let key = |password_auth: bool, secret: &Option<String>| !password_auth && secret.is_some();
        key(self.password_auth, &self.secret)
            || self
                .jumps
                .iter()
                .any(|jump| key(jump.password_auth, &jump.secret))
    }
}

impl std::fmt::Debug for ConnectOptions {
    /// 密码不进日志：只写有没有
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectOptions")
            .field("forward_agent", &self.forward_agent)
            .field("identity_file", &self.identity_file)
            .field("password_auth", &self.password_auth)
            .field("secret", &self.secret.as_ref().map(|_| "…"))
            .field("no_prompt", &self.no_prompt)
            .field(
                "jumps",
                &self
                    .jumps
                    .iter()
                    .map(|jump| jump.destination.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Connection {
    /// 连接服务器：经 ssh 启动 kuaid；没装或版本不对就先上传再连。
    /// 会阻塞（建立连接要几百毫秒，第一次还要上传），放在后台线程里调用。
    pub fn connect(destination: &str, options: &ConnectOptions) -> Result<Connection> {
        with_plain_retry(options, |options| {
            Self::connect_on(Route::Direct, destination, options)
        })
    }

    /// 经 `via` 那台电脑的 kuaid 连它配置的服务器 `server`（那台电脑的 `Store::servers` 里的 id）：
    /// 这台电脑不用配置那台服务器，登录方式、跳板机都照那台电脑的配置，要密码时照样在这边问。
    /// `destination` 是那台服务器的 ssh 目标，只用来显示；`options` 只用 `jumps`（报错时写明是哪一台，
    /// 不用里面的登录方式）和 `on_stage`。会阻塞，放在后台线程里调用。
    pub fn connect_via(
        via: &Arc<Connection>,
        server: &str,
        destination: &str,
        options: &ConnectOptions,
    ) -> Result<Connection> {
        if via.hello.protocol < 10 {
            bail!(
                "{}",
                tf!(
                    "{destination} 上的 kuaid 过旧，无法经它连接其他主机",
                    destination = via.destination
                )
            );
        }
        Self::connect_on(Route::Via { via, server }, destination, options)
    }

    /// 测试连接：只登录上去问一句是什么系统。不装、不启动 kuaid，服务器上什么都不留。
    /// 登录不了、连不上时的报错和 [`Self::connect`] 一样。会阻塞，放在后台线程里调用。
    pub fn probe(destination: &str, options: &ConnectOptions) -> Result<Probe> {
        with_plain_retry(options, |options| Self::probe_once(destination, options))
    }

    fn probe_once(destination: &str, options: &ConnectOptions) -> Result<Probe> {
        // 和连接时一样：先按 Linux 的写法问（已经知道是 Windows 的先按 Windows），跑不了再换另一种
        let mut windows = known_windows(destination);
        let (mut tried_sh, mut tried_cmd) = (false, false);
        let mut refused: Option<String> = None;
        loop {
            let script = match windows {
                true => windows_probe_script(),
                false => probe_script(),
            };
            let (pipes, _saved) = Route::Direct.run(destination, &script, options, false)?;
            match start(pipes, true, destination, Some(options))? {
                Started::Missing { os, arch } => {
                    let kind = server_os(&os);
                    return Ok(Probe {
                        os: match kind {
                            Some(Os::Linux) => "Linux".into(),
                            Some(Os::Windows) => "Windows".into(),
                            None => os,
                        },
                        arch,
                        supported: kind.is_some(),
                    });
                }
                Started::Refused(message) => {
                    match windows {
                        true => tried_cmd = true,
                        false => tried_sh = true,
                    }
                    let first = refused.get_or_insert(message).clone();
                    if tried_sh && tried_cmd {
                        bail!("{first}");
                    }
                    windows = !windows;
                }
                // 这两条脚本不启动 kuaid，不会走到握手
                Started::Ready(..) => bail!(tr("连接失败")),
            }
        }
    }

    fn connect_on(route: Route, destination: &str, options: &ConnectOptions) -> Result<Connection> {
        let started = Instant::now();
        let binaries = Binaries::find()?;
        let dir = binaries.dir_name();
        let mut installed = false;
        // 服务器是 Linux 还是 Windows 事先不知道：先按 Linux 的写法问（上次是 Windows 的先按
        // Windows），服务器跑不了这种写法再换另一种。两种都不行，报头一次的错
        let key = route.key(destination);
        let mut windows = known_windows(&key);
        let (mut tried_sh, mut tried_cmd) = (false, false);
        let mut refused: Option<String> = None;
        loop {
            let script = match windows {
                true => windows_bootstrap_script(&dir),
                false => bootstrap_script(&dir),
            };
            // _saved 留到握手完：登录阶段 ssh 要密码时用得上
            let (pipes, _saved) =
                route.run(destination, &script, options, options.forward_agent)?;
            match start(pipes, true, destination, Some(options))? {
                Started::Ready(inner, hello) => {
                    remember_windows(&key, windows);
                    return Ok(Connection {
                        destination: destination.to_string(),
                        hello,
                        connect_time: started.elapsed(),
                        installed,
                        local: false,
                        inner,
                    });
                }
                Started::Refused(message) => {
                    match windows {
                        true => tried_cmd = true,
                        false => tried_sh = true,
                    }
                    let first = refused.get_or_insert(message).clone();
                    if tried_sh && tried_cmd {
                        bail!("{first}");
                    }
                    windows = !windows;
                }
                Started::Missing { os, arch } => {
                    if installed {
                        bail!(tr("上传 kuaid 后仍无法启动"));
                    }
                    match server_os(&os) {
                        Some(Os::Linux) => {
                            let binary = binaries.binary_for(Os::Linux, &arch)?;
                            let script = install_script(&dir, binary.len());
                            install(route, destination, options, &script, binary)?;
                        }
                        Some(Os::Windows) if windows => {
                            let files = binaries.files_for(Os::Windows, &arch)?;
                            let archive = tar_files(&files);
                            let script = windows_install_script(&dir);
                            install(route, destination, options, &script, archive)?;
                        }
                        // Windows 上 PATH 里有 Git 带的 sh，头一种写法跑得了：按 Windows 的写法再问一次
                        Some(Os::Windows) => {
                            windows = true;
                            continue;
                        }
                        None => bail!(
                            "{}",
                            tf!("目前仅支持 Linux 和 Windows 主机，该主机为 {os}", os)
                        ),
                    }
                    installed = true;
                }
            }
        }
    }

    /// 直接启动一个 kuaid 进程（测试用，比如本机的 `kuaid --stdio`）。
    pub fn spawn_stdio(cmd: Command) -> Result<Connection> {
        let started = Instant::now();
        match start(spawn_pipes(cmd)?, false, tr("本机 kuaid"), None)? {
            Started::Ready(inner, hello) => Ok(Connection {
                destination: tr("本机 kuaid").into(),
                hello,
                connect_time: started.elapsed(),
                installed: false,
                local: true,
                inner,
            }),
            Started::Missing { .. } | Started::Refused(_) => bail!(tr("kuaid 未启动")),
        }
    }

    /// 连这台电脑上的 kuaid：本机仓库的终端开在它里面，Kuai 退出、更新以后终端还在，从别的电脑经
    /// ssh 连上来看到的也是同一批终端。没装（或者不是这一版）就先从 Kuai 带的那份装到 `~/.kuai`
    /// 下；常驻进程没在跑由转接程序拉起。会阻塞，放在后台线程里调用。
    pub fn connect_local() -> Result<Connection> {
        let started = Instant::now();
        let binaries = Binaries::find()?;
        let (exe, installed) = binaries.install_local(&local_home())?;
        if installed {
            // 清掉没人用的旧版本
            let mut clean = Command::new(&exe);
            clean
                .arg("--installed")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            no_window(&mut clean);
            let _ = clean.status();
        }
        let mut cmd = Command::new(&exe);
        cmd.arg("--connect");
        match start(spawn_pipes(cmd)?, false, tr("本机 kuaid"), None)? {
            Started::Ready(inner, hello) => Ok(Connection {
                destination: tr("本机 kuaid").into(),
                hello,
                connect_time: started.elapsed(),
                installed,
                local: true,
                inner,
            }),
            Started::Missing { .. } | Started::Refused(_) => bail!(tr("kuaid 未启动")),
        }
    }

    /// 这台电脑上的 kuaid 常驻进程多半在跑（它在跑的时候留着这个文件）：里面可能有留着的终端。
    /// 只是看一眼，不拉起它。
    pub fn local_daemon_running() -> bool {
        local_kuai_root().join("run").join("kuaid.port").is_file()
    }

    /// 连接还在不在。断开后所有调用都会失败，要重新连接。
    pub fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::Acquire)
    }

    /// 主动断开：别处还拿着它也一样（改了主机地址、登录方式，拿着它的地方要按新的重连）。等待中的
    /// 请求、订阅、终端都以 `reason` 结束，之后的请求在发的时候就失败；ssh 在后台结束。
    pub fn close(&self, reason: String) {
        if !self.is_alive() {
            return;
        }
        self.inner.fail_all(reason);
        let (inner, tree) = (self.inner.clone(), !self.local);
        let reap = move || {
            inner.link.kill(tree);
            let _ = inner.link.wait();
        };
        let spawned = std::thread::Builder::new()
            .name("fr-ssh-reaper".into())
            .spawn(reap.clone());
        if spawned.is_err() {
            reap();
        }
    }

    /// 经哪台电脑的 kuaid 连的（[`Self::connect_via`]）；直接连的为空。
    pub fn via(&self) -> Option<&Arc<Connection>> {
        match &*self.inner.link {
            Link::Relay(relay) => Some(&relay.via),
            Link::Process(_) => None,
        }
    }

    /// 这台电脑保存的仓库列表和工作区（协议 10）。
    pub fn state_get(&self) -> BoxFuture<'static, Result<Store>> {
        let call = self.call("state_get", json!({}), Vec::new());
        async move { parsed(call.await?.0) }.boxed()
    }

    /// 订阅这台电脑保存的仓库列表和工作区：先给现在的整份，之后每次有改动（哪台电脑改的都算）
    /// 给一份新的。连接断了，流以一个错误结束。
    pub fn state_watch(&self) -> StateStream {
        let (tx, rx) = mpsc::unbounded();
        let id = self.inner.next_id();
        self.inner
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending::State(tx.clone()));
        let request = Request {
            id,
            method: "state_watch".into(),
            params: json!({}),
            bytes: 0,
        };
        if let Err(err) = self.inner.send(request, Vec::new()) {
            self.inner.pending.lock().unwrap().remove(&id);
            let _ = tx.unbounded_send(Err(err));
        }
        StateStream {
            rx,
            _cancel: Box::new(CancelOnDrop {
                inner: Arc::downgrade(&self.inner),
                id,
            }),
        }
    }

    /// 依次执行这串改动，返回改完的整份。有一个做不了就都不算。
    pub fn state_apply(&self, ops: Vec<Op>) -> BoxFuture<'static, Result<Store>> {
        let call = self.call("state_apply", json!({ "ops": ops }), Vec::new());
        async move { parsed(call.await?.0) }.boxed()
    }

    pub fn ping(&self) -> BoxFuture<'static, Result<()>> {
        let call = self.call("ping", json!({}), Vec::new());
        async move { call.await.map(|_| ()) }.boxed()
    }

    pub(crate) fn list_dir(&self, path: &str) -> BoxFuture<'static, Result<DirListing>> {
        let call = self.call("list_dir", json!({ "path": path }), Vec::new());
        async move {
            let (result, _) = call.await?;
            serde_json::from_value(result).context(tr("kuaid 返回的目录格式错误"))
        }
        .boxed()
    }

    pub(crate) fn git(
        &self,
        repo: &str,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> BoxFuture<'static, Result<GitOutput>> {
        let call = self.call(
            "git",
            json!({ "repo": repo, "args": args }),
            stdin.unwrap_or_default(),
        );
        git_output(call)
    }

    /// 同 [`Self::git`]，另给一个取消的办法：只叫 kuaid 杀掉 git，不丢下请求，kuaid 等它退出以后
    /// 照常回话，future 这时才完成（见 [`GitCancel`]）。
    pub(crate) fn git_cancellable(
        &self,
        repo: &str,
        args: Vec<String>,
    ) -> (BoxFuture<'static, Result<GitOutput>>, GitCancel) {
        let params = json!({ "repo": repo, "args": args });
        let (id, call) = self.inner.call_with_id("git", params, Vec::new());
        let inner = Arc::downgrade(&self.inner);
        let cancel = GitCancel(Arc::new(move || {
            let Some(inner) = inner.upgrade() else {
                return;
            };
            if inner.pending.lock().unwrap().contains_key(&id) {
                let cancel = Request {
                    id: 0,
                    method: "cancel".into(),
                    params: json!({ "id": id }),
                    bytes: 0,
                };
                let _ = inner.send(cancel, Vec::new());
            }
        }));
        (git_output(call), cancel)
    }

    pub(crate) fn watch(&self, repo: &str) -> ChangeStream {
        let (tx, rx) = mpsc::unbounded();
        let id = self.inner.next_id();
        self.inner
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending::Watch(tx));
        let request = Request {
            id,
            method: "watch".into(),
            params: json!({ "repo": repo }),
            bytes: 0,
        };
        if self.inner.send(request, Vec::new()).is_err() {
            // 拿掉等待项，tx 跟着丢弃，流马上结束
            self.inner.pending.lock().unwrap().remove(&id);
        }
        ChangeStream {
            rx,
            _cancel: Box::new(CancelOnDrop {
                inner: Arc::downgrade(&self.inner),
                id,
            }),
        }
    }

    pub(crate) fn git_stream(&self, repo: &str, args: Vec<String>, progress: bool) -> GitStream {
        let (tx, rx) = mpsc::unbounded();
        let id = self.inner.next_id();
        self.inner
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending::Stream(tx.clone()));
        let request = Request {
            id,
            method: "git".into(),
            params: json!({ "repo": repo, "args": args, "stream": true, "progress": progress }),
            bytes: 0,
        };
        if let Err(err) = self.inner.send(request, Vec::new()) {
            self.inner.pending.lock().unwrap().remove(&id);
            let _ = tx.unbounded_send(Err(err));
        }
        GitStream {
            rx,
            _cancel: Box::new(CancelOnDrop {
                inner: Arc::downgrade(&self.inner),
                id,
            }),
        }
    }

    /// 开（或者接上）一个终端：kuaid 里有名字是 `key` 的终端会话就接上它，没有就在 `cwd` 里开一个
    /// 登录 shell。返回的流先给会话的编号，再是程序的输出（接上已有会话时，开头是还原屏幕的那一段），
    /// 程序结束时给退出码。丢弃这个流只是断开，里面的程序接着跑；要结束它用 [`Self::term_close`]。
    /// `shell`：新开时用哪个 shell（程序和参数），空着由 kuaid 定。
    pub fn term_open(
        &self,
        key: &str,
        cwd: &str,
        shell: &[String],
        cols: u16,
        rows: u16,
    ) -> TermStream {
        let params = TermOpen {
            key: key.to_string(),
            cwd: cwd.to_string(),
            cols,
            rows,
            shell: shell.to_vec(),
        };
        self.term_stream("term_open", json!(params))
    }

    /// 接上指定编号的终端会话。
    pub fn term_attach(&self, term: &str, cols: u16, rows: u16) -> TermStream {
        self.term_stream(
            "term_attach",
            json!({ "term": term, "cols": cols, "rows": rows }),
        )
    }

    /// 只看指定编号的终端会话（协议 11）：不改它的大小，先收到按它现在的大小还原的屏幕，再是之后的
    /// 输出。kuaid 不指望看的这一头回答程序的话（问光标位置、颜色），这一头的仿真器也不要答；用户敲的字
    /// 照样用 [`Self::term_input`] 写。
    pub fn term_view(&self, term: &str) -> TermStream {
        self.term_stream("term_attach", json!({ "term": term, "watch": true }))
    }

    /// 订阅这台电脑上的终端列表（协议 11）：先给现在的整份，之后有变化再给。
    pub fn term_watch(&self) -> TermListStream {
        let (tx, rx) = mpsc::unbounded();
        let id = self.inner.next_id();
        self.inner
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending::TermList(tx.clone()));
        let request = Request {
            id,
            method: "term_watch".into(),
            params: json!({}),
            bytes: 0,
        };
        if let Err(err) = self.inner.send(request, Vec::new()) {
            self.inner.pending.lock().unwrap().remove(&id);
            let _ = tx.unbounded_send(Err(err));
        }
        TermListStream {
            rx,
            _cancel: Box::new(CancelOnDrop {
                inner: Arc::downgrade(&self.inner),
                id,
            }),
        }
    }

    fn term_stream(&self, method: &str, params: Value) -> TermStream {
        let (tx, rx) = mpsc::unbounded();
        let backlog = Arc::new(Backlog::default());
        let id = self.inner.next_id();
        self.inner
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending::Term(tx.clone(), backlog.clone()));
        let request = Request {
            id,
            method: method.into(),
            params,
            bytes: 0,
        };
        if let Err(err) = self.inner.send(request, Vec::new()) {
            self.inner.pending.lock().unwrap().remove(&id);
            let _ = tx.unbounded_send(Err(err));
        }
        TermStream {
            rx,
            backlog,
            ended: false,
            _cancel: Box::new(CancelOnDrop {
                inner: Arc::downgrade(&self.inner),
                id,
            }),
        }
    }

    /// 把用户敲的字写给终端里的程序。
    pub fn term_input(&self, term: &str, bytes: Vec<u8>) {
        let request = Request {
            id: 0,
            method: "term_input".into(),
            params: json!({ "term": term }),
            bytes: bytes.len() as u64,
        };
        let _ = self.inner.send(request, bytes);
    }

    pub fn term_resize(&self, term: &str, cols: u16, rows: u16) {
        let request = Request {
            id: 0,
            method: "term_resize".into(),
            params: json!({ "term": term, "cols": cols, "rows": rows }),
            bytes: 0,
        };
        let _ = self.inner.send(request, Vec::new());
    }

    /// 结束终端里的程序，kuaid 不再留着这个会话。
    pub fn term_close(&self, term: &str) -> BoxFuture<'static, Result<()>> {
        let call = self.call("term_close", json!({ "term": term }), Vec::new());
        async move { call.await.map(|_| ()) }.boxed()
    }

    /// 终端里除了 shell 还有哪些程序在跑（名字，查不到名字的是空串）。shell 闲着时是空的。
    pub fn term_programs(&self, term: &str) -> BoxFuture<'static, Result<Vec<String>>> {
        let call = self.call("term_programs", json!({ "term": term }), Vec::new());
        async move {
            let (mut result, _) = call.await?;
            serde_json::from_value(result["programs"].take())
                .context(tr("kuaid 返回的结果格式错误"))
        }
        .boxed()
    }

    /// kuaid 里现在有哪些终端会话。
    pub fn term_list(&self) -> BoxFuture<'static, Result<Vec<TermInfo>>> {
        let call = self.call("term_list", json!({}), Vec::new());
        async move {
            let (mut result, _) = call.await?;
            serde_json::from_value(result["terms"].take()).context(tr("kuaid 返回的结果格式错误"))
        }
        .boxed()
    }

    /// 把一个文件传到服务器上：kuaid 放在 `~/.kuai/drop/` 下（重名的换个名字，过几天自己清掉），
    /// 传完给出它在服务器上的完整路径。往终端里发图片、文件用：终端只能过文字，文件走这里，
    /// 再把路径贴进终端。`name` 是文件名，kuaid 会把贴进 shell 要加引号的字符换掉。
    pub fn upload(&self, name: &str, source: UploadSource) -> UploadStream {
        let inner = Arc::downgrade(&self.inner);
        let first = FilePut {
            name: name.to_string(),
            ..Default::default()
        };
        UploadStream::spawn(move |tx| upload_blocking(&inner, first, source, tx))
    }

    pub(crate) fn fs_list(
        &self,
        repo: &str,
        dir: &str,
    ) -> BoxFuture<'static, Result<Vec<FsEntry>>> {
        let call = self.call("fs_list", json!({ "repo": repo, "path": dir }), Vec::new());
        async move {
            let (mut result, _) = call.await?;
            serde_json::from_value(result["entries"].take()).context(tr("kuaid 返回的结果格式错误"))
        }
        .boxed()
    }

    pub(crate) fn fs_repos(&self, repo: &str) -> BoxFuture<'static, Result<SubRepos>> {
        let call = self.call("fs_repos", json!({ "repo": repo }), Vec::new());
        async move { parsed(call.await?.0) }.boxed()
    }

    pub(crate) fn fs_stat(&self, repo: &str, path: &str) -> BoxFuture<'static, Result<FsStat>> {
        let call = self.call("fs_stat", json!({ "repo": repo, "path": path }), Vec::new());
        async move { parsed(call.await?.0) }.boxed()
    }

    pub(crate) fn fs_read(
        &self,
        repo: &str,
        path: &str,
        offset: u64,
        len: u64,
        whole: Option<u64>,
    ) -> BoxFuture<'static, Result<(FsStat, Vec<u8>)>> {
        let mut params = json!({ "repo": repo, "path": path, "offset": offset, "len": len });
        if let Some(whole) = whole {
            params["whole"] = json!(whole);
        }
        let call = self.call("fs_read", params, Vec::new());
        async move {
            let (result, bytes) = call.await?;
            Ok((parsed(result)?, bytes))
        }
        .boxed()
    }

    pub(crate) fn fs_write(
        &self,
        params: FsWrite,
        bytes: Vec<u8>,
    ) -> BoxFuture<'static, Result<FsWritten>> {
        let call = self.call("fs_write", json!(params), bytes);
        async move { parsed(call.await?.0) }.boxed()
    }

    /// 结果是一个 JSON 对象、别的没有的那几个（映射到本机用的 `fs_pwrite`、`fs_space` 这些）。
    pub(crate) fn fs_call<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        method: &str,
        params: Value,
        bytes: Vec<u8>,
    ) -> BoxFuture<'static, Result<T>> {
        let call = self.call(method, params, bytes);
        async move { parsed(call.await?.0) }.boxed()
    }

    pub(crate) fn fs_list_all(
        &self,
        repo: &str,
        dir: &str,
    ) -> BoxFuture<'static, Result<Vec<FsEntry>>> {
        let params = json!({ "repo": repo, "path": dir, "all": true });
        let call = self.call("fs_list", params, Vec::new());
        async move {
            let (mut result, _) = call.await?;
            serde_json::from_value(result["entries"].take()).context(tr("kuaid 返回的结果格式错误"))
        }
        .boxed()
    }

    /// 只看成没成的那几个（建目录、删除、改名）。
    pub(crate) fn fs_simple(&self, method: &str, params: Value) -> BoxFuture<'static, Result<()>> {
        let call = self.call(method, params, Vec::new());
        async move { call.await.map(|_| ()) }.boxed()
    }

    /// 把仓库里的一个文件一段段读回来，存到本机的 `dest`。
    ///
    /// 每一段 kuaid 都重新打开文件读：读的中途文件被改了（大小、修改时间和第一段时不一样）、变短了
    /// 就失败，临时文件删掉，原来的 `dest` 不动。原来照样存下，是新旧拼起来的、或者截断的内容。
    pub(crate) fn fs_download(&self, repo: &str, path: &str, dest: PathBuf) -> UploadStream {
        let inner = Arc::downgrade(&self.inner);
        let (repo, path) = (repo.to_string(), path.to_string());
        UploadStream::spawn(move |tx| {
            let mut part = crate::local::PartFile::create(&dest)?;
            let mut sent = 0;
            let mut first: Option<FsStat> = None;
            loop {
                let connection = inner.upgrade().context(tr("连接已关闭"))?;
                let params = json!({ "repo": repo, "path": path, "offset": sent });
                let call = connection.call("fs_read", params, Vec::new());
                drop(connection);
                let (result, bytes) = futures::executor::block_on(call)?;
                let stat: FsStat = parsed(result)?;
                let total = first.get_or_insert(stat).size;
                let changed =
                    first.is_some_and(|was| was.size != stat.size || was.mtime != stat.mtime);
                if changed || (bytes.is_empty() && sent < total) {
                    bail!(tr("下载期间文件被改动了，请重试。"));
                }
                part.write(&bytes)?;
                sent += bytes.len() as u64;
                let progress = UploadItem::Progress {
                    sent,
                    total: total.max(sent),
                };
                // 没人等了（用户取消了）：不传了，临时文件跟着 part 删掉
                if tx.unbounded_send(Ok(progress)).is_err() {
                    return Ok(());
                }
                if sent >= total {
                    break;
                }
            }
            part.finish()?;
            let _ = tx.unbounded_send(Ok(UploadItem::Done(dest.to_string_lossy().into_owned())));
            Ok(())
        })
    }

    /// 把本机的 `source` 传到仓库里的 `path`。
    pub(crate) fn fs_upload(
        &self,
        repo: &str,
        path: &str,
        source: PathBuf,
        overwrite: bool,
    ) -> UploadStream {
        let inner = Arc::downgrade(&self.inner);
        let first = FilePut {
            repo: repo.to_string(),
            path: path.to_string(),
            overwrite,
            ..Default::default()
        };
        UploadStream::spawn(move |tx| {
            upload_blocking(&inner, first, UploadSource::File(source), tx)
        })
    }

    /// 发一个请求，等它的响应。返回的 future 被丢弃时向 kuaid 发取消。
    fn call(
        &self,
        method: &str,
        params: Value,
        payload: Vec<u8>,
    ) -> BoxFuture<'static, Result<(Value, Vec<u8>)>> {
        self.inner.call(method, params, payload)
    }
}

/// kuaid 回的 git 结果：结果里是退出码和标准错误，附带的字节是标准输出。
fn git_output(
    call: BoxFuture<'static, Result<(Value, Vec<u8>)>>,
) -> BoxFuture<'static, Result<GitOutput>> {
    async move {
        let (result, stdout) = call.await?;
        let result: GitResult =
            serde_json::from_value(result).context(tr("kuaid 返回的结果格式错误"))?;
        Ok(GitOutput {
            code: result.code,
            stdout,
            stderr: result.stderr,
            elapsed: Duration::from_millis(result.ms),
        })
    }
    .boxed()
}

fn parsed<T: serde::de::DeserializeOwned>(result: Value) -> Result<T> {
    serde_json::from_value(result).context(tr("kuaid 返回的结果格式错误"))
}

/// 上传的那个线程：一块块读、一块块发，每块等 kuaid 回一声。`first` 是第一块带的参数：
/// 文件名，或者传到哪个仓库的哪个路径。
fn upload_blocking(
    inner: &Weak<Inner>,
    first: FilePut,
    source: UploadSource,
    tx: &mpsc::UnboundedSender<Result<UploadItem>>,
) -> Result<()> {
    let (mut reader, total): (Box<dyn Read>, u64) = match source {
        UploadSource::File(path) => {
            let file =
                File::open(&path).with_context(|| tf!("无法打开 {path}", path = path.display()))?;
            let total = file.metadata().map(|meta| meta.len()).unwrap_or(0);
            (Box::new(file), total)
        }
        UploadSource::Bytes(bytes) => {
            let total = bytes.len() as u64;
            (Box::new(Cursor::new(bytes)), total)
        }
    };
    let mut read_chunk = move || -> Result<Vec<u8>> {
        let mut chunk = Vec::with_capacity(UPLOAD_CHUNK);
        reader
            .by_ref()
            .take(UPLOAD_CHUNK as u64)
            .read_to_end(&mut chunk)
            .context(tr("读取文件失败"))?;
        Ok(chunk)
    };
    let mut upload = String::new();
    // 多读一块：下一块是空的，这一块就是最后一块
    let mut current = read_chunk()?;
    loop {
        let next = read_chunk()?;
        let done = next.is_empty();
        let params = match upload.is_empty() {
            true => FilePut {
                done,
                ..first.clone()
            },
            false => FilePut {
                upload: upload.clone(),
                done,
                ..Default::default()
            },
        };
        let connection = inner.upgrade().context(tr("连接已关闭"))?;
        let call = connection.call("file_put", json!(params), current);
        drop(connection);
        let (result, _) = futures::executor::block_on(call)?;
        let reply: FilePutReply =
            serde_json::from_value(result).context(tr("kuaid 返回的结果格式错误"))?;
        upload = reply.upload;
        let progress = UploadItem::Progress {
            sent: reply.received,
            total: total.max(reply.received),
        };
        if tx.unbounded_send(Ok(progress)).is_err() {
            // 没人等了（用户取消了）：不传了，让 kuaid 把收到的删掉
            if !done && let Some(connection) = inner.upgrade() {
                let abort = FilePut {
                    upload,
                    abort: true,
                    ..Default::default()
                };
                let request = Request {
                    id: 0,
                    method: "file_put".into(),
                    params: json!(abort),
                    bytes: 0,
                };
                let _ = connection.send(request, Vec::new());
            }
            return Ok(());
        }
        if done {
            let path = reply.path.context(tr("kuaid 返回的结果格式错误"))?;
            let _ = tx.unbounded_send(Ok(UploadItem::Done(path)));
            return Ok(());
        }
        current = next;
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // 关掉标准输入，kuaid 读到结尾就退出；再杀掉 ssh，免得服务器那边卡住时它不退。
        // 本机的转接程序下面挂着它拉起的常驻进程和终端：只结束它自己
        self.inner.outgoing.lock().unwrap().take();
        // 结束 ssh 要起 taskkill（一秒多）、再等它退出：放到后台线程里做，不卡住丢弃最后一个连接的
        // 那个线程（多半是界面线程）。线程起不来才在这里做
        let (inner, tree) = (self.inner.clone(), !self.local);
        let reap = move || {
            inner.link.kill(tree);
            let _ = inner.link.wait();
        };
        let spawned = std::thread::Builder::new()
            .name("fr-ssh-reaper".into())
            .spawn(reap.clone());
        if spawned.is_err() {
            reap();
        }
    }
}

/// 本机的 kuaid 把 `.kuai` 放在谁的主目录下：和 kuaid 自己认的一样（`FR_KUAID_HOME` 测试时用）。
fn local_home() -> PathBuf {
    match std::env::var_os("FR_KUAID_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) if cfg!(windows) => PathBuf::from(dir),
        _ => PathBuf::from(fr_core::dir::home()),
    }
}

/// 这台电脑上的 kuaid 的目录（`~/.kuai`）：各个版本的程序在它下面，`run` 里是常驻进程用的文件。
pub fn local_kuai_root() -> PathBuf {
    local_home().join(".kuai")
}

/// 结束 ssh 和它启动的进程。经跳板机连接时，ssh 下面还挂着转接用的那几层（见 `hop`）：正常情况下
/// 外面的 ssh 一走它们跟着走，但跳板机那头卡住时它们会留下，还占着我们读的那根管道，所以一起结束。
fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let mut taskkill = Command::new("taskkill");
        taskkill
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        no_window(&mut taskkill);
        let _ = taskkill.status();
    }
    let _ = child.kill();
}

impl Inner {
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// 发一个请求，等它的响应。返回的 future 被丢弃时向 kuaid 发取消。
    fn call(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        payload: Vec<u8>,
    ) -> BoxFuture<'static, Result<(Value, Vec<u8>)>> {
        self.call_with_id(method, params, payload).1
    }

    /// 同 [`Self::call`]，另外给出请求的编号（取消要用）。
    fn call_with_id(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        payload: Vec<u8>,
    ) -> (u64, Reply) {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, Pending::Call(tx));
        let request = Request {
            id,
            method: method.into(),
            params,
            bytes: payload.len() as u64,
        };
        if let Err(err) = self.send(request, payload) {
            self.pending.lock().unwrap().remove(&id);
            return (id, future::ready(Err(err)).boxed());
        }
        let guard = CancelOnDrop {
            inner: Arc::downgrade(self),
            id,
        };
        let call = async move {
            let result = rx.await;
            drop(guard);
            result.unwrap_or_else(|_| Err(anyhow!(tr("与主机的连接已断开"))))
        };
        (id, call.boxed())
    }

    fn send(&self, request: Request, payload: Vec<u8>) -> Result<()> {
        let outgoing = self.outgoing.lock().unwrap();
        let sender = outgoing.as_ref().context(tr("连接已关闭"))?;
        sender
            .send((request, payload))
            .map_err(|_| anyhow!(tr("与主机的连接已断开")))
    }

    /// 请 kuaid 停下编号是 `id` 的请求（流不要了）。
    fn cancel(&self, id: u64) {
        let cancel = Request {
            id: 0,
            method: "cancel".into(),
            params: json!({ "id": id }),
            bytes: 0,
        };
        let _ = self.send(cancel, Vec::new());
    }

    /// kuaid 替服务器上的 git 要密码：在另一个线程里问用户（要等用户填），再把回答发回去。
    fn ask(self: &Arc<Self>, ask_id: u64, prompt: String) {
        let inner = Arc::downgrade(self);
        std::thread::spawn(move || {
            // 经这台电脑连别的服务器时，那边的 ssh 要密码也是这样来问：等用户回答的时间不算超时
            let _asking = crate::Asking::start();
            let answer = crate::prompt_handler().and_then(|handler| handler(&prompt, None));
            if let Some(inner) = inner.upgrade() {
                let reply = Request {
                    id: 0,
                    method: "answer".into(),
                    params: json!({ "id": ask_id, "answer": answer }),
                    bytes: 0,
                };
                let _ = inner.send(reply, Vec::new());
            }
        });
    }

    /// 把一条响应交给等它的 future 或流。
    fn dispatch(self: &Arc<Self>, mut response: Response, payload: Vec<u8>) {
        if let Some(prompt) = response.ask.take() {
            return self.ask(response.id, prompt);
        }
        let mut pending = self.pending.lock().unwrap();
        if response.chunk {
            // 终端那一头取得太慢（见 [`TermItem::Lagging`]）：不再收，告诉 kuaid 别发了
            let mut lagging = false;
            match pending.get(&response.id) {
                Some(Pending::Stream(tx)) => {
                    let item = if response.err {
                        StreamItem::Progress(payload)
                    } else {
                        StreamItem::Chunk(payload)
                    };
                    let _ = tx.unbounded_send(Ok(item));
                }
                Some(Pending::Watch(tx)) => {
                    let _ = tx.unbounded_send(());
                }
                Some(Pending::Term(tx, backlog)) => {
                    // 流的第一条带着会话的编号；带 `marks` 的是发出去的消息；其余的是输出
                    let item = match response.result.take() {
                        Some(marks) if marks.get("marks").is_some() => {
                            serde_json::from_value::<TermMarks>(marks)
                                .map(|marks| TermItem::Marks(marks.marks))
                                .context(tr("kuaid 返回的结果格式错误"))
                        }
                        Some(opened) => serde_json::from_value::<TermOpened>(opened)
                            .map(|opened| TermItem::Opened {
                                term: opened.term,
                                created: opened.created,
                                restore: opened.restore,
                            })
                            .context(tr("kuaid 返回的结果格式错误")),
                        None if backlog.add(payload.len()) => Ok(TermItem::Output(payload)),
                        // 发一个过去叫醒取的那一头，它看到落后了就不再取积着的
                        None => {
                            lagging = true;
                            Ok(TermItem::Lagging)
                        }
                    };
                    let _ = tx.unbounded_send(item);
                }
                Some(Pending::State(tx)) => {
                    if let Some(store) = response.result.take() {
                        let _ = tx.unbounded_send(parsed(store));
                    }
                }
                Some(Pending::TermList(tx)) => {
                    if let Some(mut result) = response.result.take() {
                        let _ = tx.unbounded_send(parsed(result["terms"].take()));
                    }
                }
                Some(Pending::Relay(sink)) => sink.chunk(response.err, payload),
                _ => {}
            }
            if lagging {
                pending.remove(&response.id);
                drop(pending);
                self.cancel(response.id);
            }
            return;
        }
        // 找不到说明已经取消了
        let Some(waiter) = pending.remove(&response.id) else {
            return;
        };
        drop(pending);
        let outcome = match response.error {
            Some(err) => Err(anyhow!(err)),
            None => Ok(response.result.unwrap_or(Value::Null)),
        };
        match waiter {
            Pending::Call(tx) => {
                let _ = tx.send(outcome.map(|result| (result, payload)));
            }
            Pending::Stream(tx) => {
                let item = outcome.and_then(|result| {
                    let result: GitResult =
                        serde_json::from_value(result).context(tr("kuaid 返回的结果格式错误"))?;
                    Ok(StreamItem::Done(GitOutput {
                        code: result.code,
                        stdout: Vec::new(),
                        stderr: result.stderr,
                        elapsed: Duration::from_millis(result.ms),
                    }))
                });
                let _ = tx.unbounded_send(item);
            }
            // 监听只会以出错结束（比如目录不存在）：丢弃 tx，流结束
            Pending::Watch(_) => {}
            Pending::Term(tx, _) => {
                let item = outcome.and_then(|result| {
                    let exit: TermExit =
                        serde_json::from_value(result).context(tr("kuaid 返回的结果格式错误"))?;
                    Ok(TermItem::Exited(exit.exit))
                });
                let _ = tx.unbounded_send(item);
            }
            // 订阅只会以出错结束
            Pending::State(tx) => {
                if let Err(err) = outcome {
                    let _ = tx.unbounded_send(Err(err));
                }
            }
            Pending::TermList(tx) => {
                if let Err(err) = outcome {
                    let _ = tx.unbounded_send(Err(err));
                }
            }
            Pending::Relay(sink) => sink.finish(outcome),
        }
    }

    /// 连接断了：所有等待中的请求都以错误结束。
    fn fail_all(&self, reason: String) {
        self.alive.store(false, Ordering::Release);
        // 先关掉发送：之后的请求在发的时候就失败（`send` 报连接已关闭），不会登记进等待表以后再也
        // 等不到回答（原来读的那头退出了、写的那头还开着，新请求能照样发出去、一直等着）。写的线程也
        // 跟着退出，关掉 ssh 的标准输入
        self.outgoing.lock().unwrap().take();
        let tail = self.stderr.tail.lock().unwrap().trim().to_string();
        let message = if tail.is_empty() {
            reason
        } else {
            tf!("{reason}：{tail}", reason, tail)
        };
        for (_, waiter) in self.pending.lock().unwrap().drain() {
            match waiter {
                Pending::Call(tx) => {
                    let _ = tx.send(Err(anyhow!(message.clone())));
                }
                Pending::Stream(tx) => {
                    let _ = tx.unbounded_send(Err(anyhow!(message.clone())));
                }
                Pending::Term(tx, _) => {
                    let _ = tx.unbounded_send(Err(anyhow!(message.clone())));
                }
                Pending::State(tx) => {
                    let _ = tx.unbounded_send(Err(anyhow!(message.clone())));
                }
                Pending::TermList(tx) => {
                    let _ = tx.unbounded_send(Err(anyhow!(message.clone())));
                }
                Pending::Relay(sink) => sink.finish(Err(anyhow!(message.clone()))),
                Pending::Watch(_) => {}
            }
        }
    }
}

/// future 或流被丢弃时，如果请求还没完成，就从等待表里拿掉并通知 kuaid 杀掉 git。
struct CancelOnDrop {
    inner: Weak<Inner>,
    id: u64,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let was_pending = inner.pending.lock().unwrap().remove(&self.id).is_some();
        if was_pending {
            inner.cancel(self.id);
        }
    }
}

/// 在接好的管子上完成握手，开始收发。`options` 是连服务器时的连接选项。
fn start(
    pipes: Pipes,
    expect_marker: bool,
    destination: &str,
    options: Option<&ConnectOptions>,
) -> Result<Started> {
    let Pipes {
        mut stdin,
        stdout,
        stderr,
        link,
    } = pipes;
    let stderr = collect_stderr(stderr);
    let jumps = options.map_or(&[][..], |options| &options.jumps);
    // 经跳板机连接时自己看着超时（见 Watchdog）；握手完或者这个函数返回就不看了
    let watchdog = (!jumps.is_empty()).then(|| {
        let on_stage = options.and_then(|options| options.on_stage.clone());
        Watchdog::start(link.clone(), stderr.clone(), jumps.len(), on_stage)
    });
    let limit = HANDSHAKE_TIMEOUT + STAGE_TIMEOUT * jumps.len() as u32;
    let deadline = Deadline::start(link.clone(), limit);
    let timed_out = || watchdog.as_ref().is_some_and(Watchdog::timed_out) || deadline.timed_out();
    let give_up = |noise: &[String]| {
        let _ = link.wait();
        connect_error(destination, &stderr, noise, jumps, timed_out())
    };

    // 握手请求先发出去，省一次往返。没装 kuaid 时远程的 sh 不读它，丢掉就是了。
    // 拼好了一次写：经转接时一次写是一条消息
    let hello = Request {
        id: 0,
        method: "hello".into(),
        params: json!({ "protocol": PROTOCOL_VERSION }),
        bytes: 0,
    };
    let mut first = Vec::new();
    let _ = protocol::write_message(&mut first, &hello, &[]);
    let _ = stdin.write_all(&first).and_then(|()| stdin.flush());

    let mut reader: BufReader<Box<dyn Read + Send>> = BufReader::with_capacity(1 << 20, stdout);
    if expect_marker {
        let mut noise = Vec::new();
        loop {
            let mut line = String::new();
            // 一行最多读这么多：服务器那头打了很长、不换行的东西也不会一直往内存里攒
            let read = (&mut reader).take(64 << 10).read_line(&mut line);
            if read.unwrap_or(0) == 0 {
                let code = link.wait();
                let timed_out = timed_out();
                let message = give_up(&noise);
                // ssh 自己出的错（连不上、登录不了）退出码是 255；别的退出码是服务器上那条命令的：
                // 登录上了，只是这种写法它跑不了
                if code.is_some_and(|code| code != 255) && !timed_out {
                    return Ok(Started::Refused(message));
                }
                return Err(failed(&stderr, message));
            }
            let line = line.trim_end();
            if line == START_MARKER {
                break;
            }
            if let Some(rest) = line.strip_prefix(MISSING_MARKER) {
                let _ = link.wait();
                let mut parts = rest.split_whitespace();
                return Ok(Started::Missing {
                    os: parts.next().unwrap_or_default().to_string(),
                    arch: parts.next().unwrap_or_default().to_string(),
                });
            }
            if noise.len() < 10 {
                noise.push(line.to_string());
            }
        }
    }

    let response = match protocol::read_message::<_, Response>(&mut reader) {
        Ok(Some((response, _))) => response,
        Err(err) if !timed_out() => return Err(err),
        // 读到头了，或者期限到了、ssh 被结束了：按连接失败报
        Ok(None) | Err(_) => return Err(failed(&stderr, give_up(&[]))),
    };
    drop(watchdog);
    drop(deadline);
    if let Some(err) = response.error {
        bail!("{}", tf!("kuaid 握手失败：{err}", err));
    }
    let result = response.result.unwrap_or_default();
    // 常驻的 kuaid 可以比客户端新（别的电脑上的客户端把它升上去了），它认得旧客户端；比客户端旧不行。
    // 先看版本再按这一版的格式读：旧的 kuaid 回的少几项，读不成也该报这一句
    let protocol = result
        .get("protocol")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if protocol < u64::from(PROTOCOL_VERSION) {
        bail!(
            "{}",
            tf!(
                "主机上的 kuaid 过旧（协议 {protocol}，客户端 {PROTOCOL_VERSION}），无法自动替换。请在主机上运行 ~/.kuai/*/kuaid --stop 后重新连接",
                protocol,
                PROTOCOL_VERSION
            )
        );
    }
    let hello: Hello = serde_json::from_value(result).context(tr("kuaid 握手响应格式错误"))?;

    let (tx, rx) = std::sync::mpsc::channel::<(Request, Vec<u8>)>();
    let inner = Arc::new(Inner {
        outgoing: Mutex::new(Some(tx)),
        pending: Mutex::default(),
        next_id: AtomicU64::new(1),
        alive: AtomicBool::new(true),
        link,
        stderr,
    });
    let writer_inner = Arc::downgrade(&inner);
    std::thread::Builder::new()
        .name("fr-ssh-writer".into())
        .spawn(move || {
            let mut writer = BufWriter::with_capacity(1 << 16, stdin);
            while let Ok((request, payload)) = rx.recv() {
                if protocol::write_message(&mut writer, &request, &payload).is_err() {
                    // 写不进去了：在等回答的、之后要发的都不会有结果，一起结束，不等读的那头发现
                    if let Some(inner) = writer_inner.upgrade() {
                        inner.fail_all(tr("与主机的连接已断开").to_string());
                    }
                    break;
                }
            }
            // 通道关了（连接被丢弃）：退出时关掉 ssh 的标准输入
        })?;
    let weak = Arc::downgrade(&inner);
    std::thread::Builder::new()
        .name("fr-ssh-reader".into())
        .spawn(move || read_loop(weak, reader))?;
    Ok(Started::Ready(inner, hello))
}

fn read_loop(inner: Weak<Inner>, mut reader: BufReader<Box<dyn Read + Send>>) {
    let reason = loop {
        match protocol::read_message::<_, Response>(&mut reader) {
            Ok(Some((response, payload))) => match inner.upgrade() {
                Some(inner) => inner.dispatch(response, payload),
                None => return,
            },
            Ok(None) => break tr("与主机的连接已断开").to_string(),
            Err(err) => break tf!("与主机的连接出错：{err}", err = format!("{:#}", err)),
        }
    };
    if let Some(inner) = inner.upgrade() {
        inner.fail_all(reason);
    }
}

/// ssh 的标准错误里看到的东西。
#[derive(Default)]
struct StderrLog {
    /// 最后一段报错（经跳板机连接时多出来的那几种行不算），连接失败、断开时拿来提示
    tail: Mutex<String>,
    /// 经跳板机连接时：已经登录上几台了（跳板机按顺序，最后是目标服务器）
    authenticated: AtomicUsize,
    /// 经跳板机连接时：最先以失败结束的是第几台跳板机的 ssh
    failed_hop: Mutex<Option<usize>>,
}

impl StderrLog {
    fn line(&self, line: &str) {
        let line = line.trim_end_matches('\r');
        // 经跳板机连接时各个 ssh 都开着 LogLevel=VERBOSE，登录成功会打这一行
        if line.starts_with("Authenticated to ") {
            self.authenticated.fetch_add(1, Ordering::SeqCst);
            return;
        }
        if let Some(index) = line.strip_prefix(hop::FAILED) {
            if let Ok(index) = index.trim().parse() {
                self.failed_hop.lock().unwrap().get_or_insert(index);
            }
            return;
        }
        // VERBOSE 结束时的流量统计
        if line.starts_with("Transferred: ") || line.starts_with("Bytes per second: ") {
            return;
        }
        let mut tail = self.tail.lock().unwrap();
        tail.push_str(line);
        tail.push('\n');
        if tail.len() > STDERR_TAIL {
            let mut cut = tail.len() - STDERR_TAIL;
            while !tail.is_char_boundary(cut) {
                cut += 1;
            }
            tail.drain(..cut);
        }
    }
}

/// 在后台读 ssh 的标准错误，一行一行地记（见 [`StderrLog`]）。
fn collect_stderr(mut stderr: impl Read + Send + 'static) -> Arc<StderrLog> {
    let log = Arc::new(StderrLog::default());
    let sink = log.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut pending: Vec<u8> = Vec::new();
        loop {
            let n = stderr.read(&mut buf).unwrap_or(0);
            pending.extend_from_slice(&buf[..n]);
            while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                sink.line(String::from_utf8_lossy(&line[..end]).as_ref());
            }
            // 读完了，或者一直没有换行（不是一行一行的报错）：有多少记多少
            if (n == 0 || pending.len() > STDERR_TAIL) && !pending.is_empty() {
                sink.line(String::from_utf8_lossy(&pending).as_ref());
                pending.clear();
            }
            if n == 0 {
                break;
            }
        }
    });
    log
}

/// 经跳板机连接时看着进度：一段时间没有进展就结束 ssh，算超时。
///
/// 直接连接时 ssh 自己的 `ConnectTimeout` 就够了。经跳板机时不行：目标服务器的 ssh 从一启动就开始
/// 计时，而跳板机可能正等着用户输密码。所以那个 ssh 不设超时，由这里看着：每登录上一台算一次进展，
/// 等用户回答密码的时间不算。目标服务器登录上以后就不管了（后面是启动 kuaid）。
struct Watchdog {
    done: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
}

/// 这么久没有进展就算超时
const STAGE_TIMEOUT: Duration = Duration::from_secs(30);

impl Watchdog {
    fn start(
        link: Arc<Link>,
        stderr: Arc<StderrLog>,
        jumps: usize,
        on_stage: Option<StageHandler>,
    ) -> Watchdog {
        let done = Arc::new(AtomicBool::new(false));
        let timed_out = Arc::new(AtomicBool::new(false));
        let (stop, flag) = (done.clone(), timed_out.clone());
        std::thread::spawn(move || {
            let mut seen = 0;
            let mut progress = Instant::now();
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(150));
                let now = stderr.authenticated.load(Ordering::SeqCst);
                if now != seen {
                    seen = now;
                    progress = Instant::now();
                    if let Some(on_stage) = &on_stage {
                        on_stage(seen);
                    }
                }
                if seen > jumps {
                    return;
                }
                if crate::asking() {
                    progress = Instant::now();
                } else if progress.elapsed() > STAGE_TIMEOUT && !stop.load(Ordering::Acquire) {
                    flag.store(true, Ordering::Release);
                    link.kill(true);
                    return;
                }
            }
        });
        Watchdog { done, timed_out }
    }

    fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::Acquire)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
    }
}

/// 从开始连到和 kuaid 握手完最多等这么久（等用户回答密码、口令的时间不算）。经跳板机时每一台另加
/// [`STAGE_TIMEOUT`]：那几段由 [`Watchdog`] 看着，这里只兜底。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// 握手的总期限（见 [`HANDSHAKE_TIMEOUT`]）：到了还没握手完就结束 ssh，算超时。原来登录上以后服务器
/// 那头既不说话也不退出（启动脚本卡在挂死的网络盘上）时一直停在「连接中」，丢掉任务也取消不了。
struct Deadline {
    done: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
}

impl Deadline {
    fn start(link: Arc<Link>, limit: Duration) -> Deadline {
        let done = Arc::new(AtomicBool::new(false));
        let timed_out = Arc::new(AtomicBool::new(false));
        let (stop, flag) = (done.clone(), timed_out.clone());
        std::thread::spawn(move || {
            let step = Duration::from_millis(200);
            let mut waited = Duration::ZERO;
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(step);
                if !crate::asking() {
                    waited += step;
                }
                if waited > limit && !stop.load(Ordering::Acquire) {
                    flag.store(true, Ordering::Release);
                    link.kill(true);
                    return;
                }
            }
        });
        Deadline { done, timed_out }
    }

    fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::Acquire)
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
    }
}

/// 连接是在哪一段出的问题。
#[derive(Debug, PartialEq, Eq)]
enum Trouble {
    /// 连不上、登录不了第几台跳板机
    Jump(usize),
    /// 第几台跳板机登录上了，但从它连不到下一台（或者目标服务器）
    Beyond(usize),
    /// 目标服务器（没有跳板机时都是这一种）
    Target,
}

/// 从「登录上了几台」「最先失败的是第几台跳板机的 ssh」「是不是超时」看出问题在哪一段。
/// `forward_failed`：ssh 报了转接失败（`-W` 的那条通道没开成）。
fn trouble(
    jumps: usize,
    authenticated: usize,
    failed_hop: Option<usize>,
    forward_failed: bool,
    timed_out: bool,
) -> Trouble {
    if jumps == 0 || authenticated > jumps {
        return Trouble::Target;
    }
    // 还没登录上的第一台是第 authenticated 台（等于 jumps 时是目标服务器）
    match failed_hop {
        // 它自己的 ssh 先失败：连不上或者登录不了这台跳板机
        Some(index) if index >= authenticated => Trouble::Jump(index.min(jumps - 1)),
        // 登录上了的跳板机的 ssh 失败：转接不过去。目标服务器登录失败以后各台跳板机的 ssh 也会跟着
        // 结束，那不算，所以要看到 ssh 明说转接失败才算
        Some(index) if forward_failed => Trouble::Beyond(index),
        _ if timed_out && authenticated == 0 => Trouble::Jump(0),
        // 超时：登录上的最后一台一直没接通下一台
        _ if timed_out => Trouble::Beyond(authenticated - 1),
        _ if authenticated < jumps => Trouble::Jump(authenticated),
        _ => Trouble::Target,
    }
}

/// 把 ssh 的报错翻译成用户看得懂的话。
/// 连接失败的错误：ssh 说登录被拒绝的是 [`LoginDenied`]（见 [`with_plain_retry`]）。
fn failed(log: &StderrLog, message: String) -> anyhow::Error {
    match log.tail.lock().unwrap().contains("Permission denied") {
        true => LoginDenied(message).into(),
        false => anyhow!(message),
    }
}

fn connect_error(
    destination: &str,
    log: &StderrLog,
    noise: &[String],
    jumps: &[Jump],
    timed_out: bool,
) -> String {
    // 读线程可能还没把最后的报错收齐
    std::thread::sleep(Duration::from_millis(50));
    let mut stderr = log.tail.lock().unwrap().trim().to_string();
    let forward_failed = stderr.contains("open failed") || stderr.contains("forwarding failed");
    let trouble = trouble(
        jumps.len(),
        log.authenticated.load(Ordering::SeqCst),
        *log.failed_hop.lock().unwrap(),
        forward_failed,
        timed_out,
    );
    if trouble != Trouble::Target {
        // 跳板机那一段断了以后，经它连接的那个 ssh 只会说「对方关了连接」，没有用，不显示
        stderr = stderr
            .lines()
            .filter(|line| !line.contains("Connection closed by"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mut message = match trouble {
        Trouble::Target => {
            let hint = login_hint(&stderr, timed_out);
            tf!("连接 {destination}：{hint}", destination, hint)
        }
        Trouble::Jump(index) => tf!(
            "无法连接跳板机 {name}（{destination}）：{hint}",
            name = jumps[index].name,
            destination = jumps[index].destination,
            hint = login_hint(&stderr, timed_out)
        ),
        Trouble::Beyond(index) => tf!(
            "已连接跳板机 {name}，但它无法连接 {next}：{hint}。请确认跳板机能访问该主机地址。",
            name = jumps[index].name,
            next = jumps
                .get(index + 1)
                .map_or(destination, |jump| jump.destination.as_str()),
            hint = forward_hint(&stderr, timed_out)
        ),
    };
    let stderr = without_banner(&stderr);
    if !stderr.is_empty() {
        message.push_str(&tf!("（ssh：{stderr}）", stderr));
    }
    if !noise.is_empty() {
        message.push_str(&tf!("（主机输出：{join}）", join = noise.join(" / ")));
    }
    message
}

/// ssh 的大字警告（主机密钥变了那一种）四周围着一圈 `@`：去掉这一圈，只留里面的字。
fn without_banner(stderr: &str) -> String {
    stderr
        .lines()
        .map(|line| line.trim().trim_matches('@').trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 连不上、登录不了一台服务器的原因。
fn login_hint(stderr: &str, timed_out: bool) -> &'static str {
    if stderr.contains("Permission denied") {
        tr("登录被拒绝：密钥或密码错误，或已取消输入")
    } else if stderr.contains("Could not resolve hostname") {
        tr("无法解析主机名")
    } else if timed_out || stderr.contains("timed out") {
        tr("连接超时")
    } else if stderr.contains("Connection refused") {
        tr("连接被拒绝，请检查 SSH 服务和端口")
    } else if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        tr("主机密钥与 known_hosts 中的记录不一致，请先在终端中用 ssh 确认")
    } else if stderr.contains("Host key verification failed") {
        // 没见过的主机：确认框里点了取消，或者这次连接不许弹窗
        tr("尚未信任此主机的密钥")
    } else {
        tr("连接失败")
    }
}

/// 跳板机转接不到下一台的原因。
fn forward_hint(stderr: &str, timed_out: bool) -> &'static str {
    if stderr.contains("administratively prohibited") {
        tr("跳板机不允许转发连接")
    } else if stderr.contains("Connection refused") {
        tr("连接被拒绝")
    } else if stderr.contains("not known") || stderr.contains("resolution") {
        tr("无法解析主机名")
    } else if stderr.contains("No route to host") || stderr.contains("unreachable") {
        tr("网络不可达")
    } else if timed_out || stderr.contains("timed out") {
        tr("连接超时")
    } else {
        tr("连接失败")
    }
}

/// 登录一台服务器用的那部分 ssh 选项和环境变量：转发 ssh-agent、私钥、密码登录、要密码时怎么问。
/// `jump` 是跳板机的名字（登录的是目标服务器时为 None）；`silent`：存着的答不了时不弹窗。
/// 第三项要留到登录完成：留着它，这个 ssh 要密码、口令时才会用存着的自动回答、弹窗里才写得出是哪台跳板机。
fn login(
    identity_file: Option<&str>,
    password_auth: bool,
    secret: Option<&str>,
    jump: Option<&str>,
    forward_agent: bool,
    silent: bool,
) -> (
    Vec<String>,
    Vec<(String, String)>,
    Option<crate::SavedSecret>,
) {
    // 要密码或密钥口令时 ssh 启动 askpass 程序来问我们：存着的自动回答，没存的由界面弹窗。
    // 没有界面可以弹窗（比如测试工具）：只用密钥，要密码就直接失败
    let askpass = crate::ssh_env();
    // 存着私钥口令：这次只用公钥认证，服务器问不了话，自动回答的只能是 ssh 自己解私钥那一句
    let key_only = !password_auth && secret.is_some();
    let args = fr_core::ssh::login_args(
        identity_file,
        password_auth,
        key_only,
        forward_agent,
        askpass.is_some(),
    );
    let mut env: Vec<(String, String)> = Vec::new();
    let mut saved = None;
    if let Some(askpass) = askpass {
        env.extend(askpass.iter().cloned());
        if secret.is_some() || jump.is_some() || silent {
            let kind = if password_auth {
                crate::PromptKind::Password
            } else {
                crate::PromptKind::Passphrase
            };
            let entry = crate::SavedSecret::register_for(secret, kind, jump, silent);
            env.push((
                fr_core::askpass::TAG_ENV.to_string(),
                entry.tag().to_string(),
            ));
            saved = Some(entry);
        }
    }
    (args, env, saved)
}

/// 构造 ssh 命令。`--` 防止以 `-` 开头的目标被当成选项。
/// 返回的第二项要留到登录完成（见 [`login`]）。
fn ssh(
    destination: &str,
    remote_command: &str,
    options: &ConnectOptions,
    forward_agent: bool,
) -> (Command, Vec<crate::SavedSecret>) {
    let mut cmd = Command::new(ssh_program());
    let (args, env, saved) = login(
        options.identity_file.as_deref(),
        options.password_auth,
        options.secret.as_deref(),
        None,
        forward_agent,
        options.no_prompt,
    );
    let mut held: Vec<crate::SavedSecret> = saved.into_iter().collect();
    cmd.args(args);
    cmd.envs(env);
    if options.jumps.is_empty() {
        cmd.args(["-o", "ConnectTimeout=10"]);
    } else {
        // 经跳板机：各台的登录选项放进环境变量，由转接的那一层取用（见 hop）。这里不设超时
        let mut hops = Vec::new();
        for jump in &options.jumps {
            let (mut args, env, saved) = login(
                jump.identity_file.as_deref(),
                jump.password_auth,
                jump.secret.as_deref(),
                Some(&jump.name),
                false,
                options.no_prompt,
            );
            args.extend(STAGE_OPTIONS.iter().map(|item| item.to_string()));
            if options.accept_new_host_keys {
                args.extend(ACCEPT_NEW_HOSTS.iter().map(|item| item.to_string()));
            }
            let tag = env
                .iter()
                .find(|(name, _)| name == fr_core::askpass::TAG_ENV)
                .map(|(_, tag)| tag.clone());
            held.extend(saved);
            hops.push(hop::Hop {
                destination: jump.destination.clone(),
                args,
                tag,
            });
        }
        cmd.env(hop::HOPS_ENV, hop::encode(&hops));
        let helper = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("kuai"));
        let proxy = hop::proxy_command(&helper, hops.len() - 1);
        cmd.args(["-o", &format!("ProxyCommand={proxy}")]);
        cmd.args(STAGE_OPTIONS);
    }
    cmd.arg("-T");
    if options.accept_new_host_keys {
        cmd.args(ACCEPT_NEW_HOSTS);
    }
    cmd.args(KEEP_OPTIONS);
    cmd.args(["--", destination, remote_command]);
    (cmd, held)
}

/// 远程执行的启动脚本：这一版的 kuaid 装了就启动它的转接程序，没装就报告系统和架构。
/// 包一层 `sh -c`，用户的登录 shell 是 fish 之类时也能跑。
fn bootstrap_script(dir: &str) -> String {
    format!(
        r#"sh -c 'A="$HOME/.kuai/{dir}/kuaid"; if [ -x "$A" ]; then echo {START_MARKER}; exec "$A" --connect; fi; echo "{MISSING_MARKER} $(uname -s) $(uname -m)"'"#
    )
}

/// 测试连接用的脚本：只报告系统和架构，报法和 [`bootstrap_script`] 没装 kuaid 时一样。
fn probe_script() -> String {
    format!(r#"sh -c 'echo "{MISSING_MARKER} $(uname -s) $(uname -m)"'"#)
}

/// Windows 服务器上测试连接用的脚本，报法和 [`windows_bootstrap_script`] 没装 kuaid 时一样。
fn windows_probe_script() -> String {
    format!(r#"cmd /c "echo {MISSING_MARKER} Windows %PROCESSOR_ARCHITECTURE%""#)
}

/// 服务器报的系统名是哪种系统。Windows 上经 Git 带的 sh 问到的是 `MINGW64_NT-10.0-…` 这类。
fn server_os(name: &str) -> Option<Os> {
    let upper = name.to_uppercase();
    if upper == "LINUX" {
        Some(Os::Linux)
    } else if upper == "WINDOWS"
        || ["MINGW", "MSYS", "CYGWIN"]
            .iter()
            .any(|p| upper.starts_with(p))
    {
        Some(Os::Windows)
    } else {
        None
    }
}

/// Windows 服务器上的启动脚本，做的事和 [`bootstrap_script`] 一样。
///
/// 包一层 `cmd /c`：sshd 的默认 shell 是命令提示符、Windows PowerShell 还是 PowerShell 7，这一行
/// 都能原样交给 cmd（里面没有 `$`、反引号和双引号），而且经 cmd 启动的程序直接拿到 ssh 的标准输入
/// 输出，字节原样过。先进到版本目录再按名字启动：主目录带空格时路径不用加引号。
/// `if` 外面那层括号不能省：`if 条件 (…) & 后面的` 在条件不成立时连「后面的」也不执行。
///
/// 主目录用延迟展开（`/v:on` 加 `!USERPROFILE!`）：`%USERPROFILE%` 是先展开再解析的，用户名里合法的
/// `&`、括号会被 cmd 当成语法，启动、安装都会坏；延迟展开在解析之后，里面是什么都只是路径。
fn windows_bootstrap_script(dir: &str) -> String {
    format!(
        r#"cmd /v:on /c "2>nul cd /d !USERPROFILE!\.kuai\{dir} || (echo {MISSING_MARKER} Windows %PROCESSOR_ARCHITECTURE%& exit /b 0) & (if not exist kuaid.exe (echo {MISSING_MARKER} Windows %PROCESSOR_ARCHITECTURE%& exit /b 0)) & echo {START_MARKER}& kuaid.exe --connect""#
    )
}

/// Windows 服务器上的安装脚本：标准输入进来的是一个 tar 包，里面是 `kuaid.exe` 和它带的 ConPTY，
/// 用系统自带的 `tar.exe` 解开（Windows 10 1803 起就有；PowerShell 从标准输入收二进制会卡住，不用它）。
/// 先解到临时目录再改名，免得留下半个文件。清掉旧版本的事交给刚装好的 kuaid 自己做
/// （`kuaid --installed`）：正在用的那一版、还有程序占着的目录它会留着。
fn windows_install_script(dir: &str) -> String {
    format!(
        r#"cmd /v:on /c "cd /d !USERPROFILE! && (if not exist .kuai mkdir .kuai) && cd .kuai && (if exist {dir}.tmp rmdir /s /q {dir}.tmp) && mkdir {dir}.tmp && tar -xf - -C {dir}.tmp && (if exist {dir} rmdir /s /q {dir}) && ren {dir}.tmp {dir} && {dir}\kuaid.exe --installed""#
    )
}

/// 几个文件打成一个 tar 包（ustar 格式），文件名不超过 100 字节。
fn tar_files(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let total: usize = files.iter().map(|(_, data)| 512 + data.len() + 511).sum();
    let mut archive = Vec::with_capacity(total + 1024);
    for (name, data) in files {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        let mut field =
            |at: usize, text: &str| header[at..at + text.len()].copy_from_slice(text.as_bytes());
        field(100, "0000755\0");
        field(108, "0000000\0");
        field(116, "0000000\0");
        field(124, &format!("{:011o}\0", data.len()));
        field(136, "00000000000\0");
        // 校验和算的时候这一格当成 8 个空格
        field(148, "        ");
        // 普通文件
        field(156, "0");
        field(257, "ustar\0");
        field(263, "00");
        let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        archive.extend_from_slice(&header);
        archive.extend_from_slice(data);
        // 内容补齐到 512 的整数倍
        archive.resize(archive.len().div_ceil(512) * 512, 0);
    }
    // 最后是两块全零
    archive.resize(archive.len() + 1024, 0);
    archive
}

/// 安装脚本收到的程序不完整（上传中途断了）时的退出码。
const INSTALL_INCOMPLETE: i32 = 3;

/// 远程执行的安装脚本，kuaid 程序从标准输入进来，一共 `size` 字节。
///
/// 收完先核对大小，对不上就删掉、以 [`INSTALL_INCOMPLETE`] 退出：`ssh -T` 没有伪终端，上传中途断开时
/// 远端只是读到标准输入的结尾，cat 照样成功，原来会把半个程序当成装好了发布出去，之后启动脚本只看
/// 文件在不在，连不上也不会重装。临时文件名带上 shell 的进程号，同一版本同时装的两边不共用它。
fn install_script(dir: &str, size: usize) -> String {
    format!(
        r#"sh -c 'set -e; R="$HOME/.kuai"; V="{dir}"; T="$R/$V/kuaid.tmp.$$"; mkdir -p "$R/$V"; cat > "$T"; if [ "$(wc -c < "$T" | tr -d " ")" != "{size}" ]; then rm -f "$T"; exit {INSTALL_INCOMPLETE}; fi; chmod 755 "$T"; mv -f "$T" "$R/$V/kuaid"; K=$(cat "$R/run/current" 2>/dev/null || true); cd "$R"; for x in *; do case "$x" in "$V"|"$K") ;; [0-9]*) rm -rf -- "$x" ;; esac; done; rm -rf -- "$HOME/.fork-remote"'"#
    )
}

/// 经 ssh 的标准输入上传 kuaid：先写临时文件再改名，免得留下半个文件；
/// 然后删掉 `~/.kuai/` 下其他版本的目录（名字以数字开头的），只留刚装的这一版和常驻进程正在用的
/// 那一版（它记在 `run/current` 里：下次连接时才换成新的，换之前它的程序不能没）；
/// 还有 1.2.2 以前产品叫 fork-remote 时装在 `~/.fork-remote` 的旧程序。
/// `script` 是服务器上执行的安装脚本，`data` 是喂给它标准输入的内容。
fn install(
    route: Route,
    destination: &str,
    options: &ConnectOptions,
    script: &str,
    data: Vec<u8>,
) -> Result<()> {
    let (pipes, _saved) = route.run(destination, script, options, false)?;
    let Pipes {
        mut stdin,
        mut stdout,
        mut stderr,
        link,
    } = pipes;
    // 写完就关掉标准输入：安装脚本读到结尾才往下走
    let writer = std::thread::spawn(move || {
        let written = stdin.write_all(&data).and_then(|()| stdin.flush());
        drop(stdin);
        written
    });
    let drain = std::thread::spawn(move || std::io::copy(&mut stdout, &mut std::io::sink()));
    let mut errors = Vec::new();
    let _ = stderr.read_to_end(&mut errors);
    let _ = drain.join();
    let code = link.wait();
    let written = writer.join();
    if code == Some(INSTALL_INCOMPLETE) {
        bail!(tr("上传 kuaid 时连接中断"));
    }
    if code != Some(0) {
        bail!(
            "{}",
            tf!(
                "上传 kuaid 失败：{stderr}",
                stderr = String::from_utf8_lossy(&errors).trim()
            )
        );
    }
    if !matches!(written, Ok(Ok(()))) {
        bail!(tr("上传 kuaid 时连接中断"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 只用公钥被拒绝时不带存着的口令再连一次() {
        let options = ConnectOptions {
            secret: Some("口令".into()),
            jumps: vec![Jump {
                password_auth: true,
                secret: Some("跳板机密码".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let tried = std::cell::RefCell::new(Vec::new());
        let result = with_plain_retry(&options, |options| -> Result<()> {
            tried
                .borrow_mut()
                .push((options.secret.clone(), options.jumps[0].secret.clone()));
            Err(LoginDenied("登录被拒绝".into()).into())
        });
        assert!(result.is_err());
        // 第二次不带私钥口令；跳板机用的是密码，照旧带着
        assert_eq!(
            *tried.borrow(),
            [
                (Some("口令".into()), Some("跳板机密码".into())),
                (None, Some("跳板机密码".into())),
            ]
        );
        // 别的失败、不许弹窗、没存口令的都不再连
        let once = |options: &ConnectOptions, error: fn() -> anyhow::Error| {
            let count = std::cell::Cell::new(0);
            let _ = with_plain_retry(options, |_| -> Result<()> {
                count.set(count.get() + 1);
                Err(error())
            });
            count.get()
        };
        assert_eq!(once(&options, || anyhow!("连接超时")), 1);
        let silent = ConnectOptions {
            no_prompt: true,
            ..options.clone()
        };
        assert_eq!(once(&silent, || LoginDenied(String::new()).into()), 1);
        let password = ConnectOptions {
            password_auth: true,
            ..options.clone()
        };
        assert_eq!(once(&password, || LoginDenied(String::new()).into()), 1);
        // 带着口令的那次只用公钥认证
        let key = args(&options);
        assert!(
            key.iter()
                .any(|arg| arg == "PreferredAuthentications=publickey")
        );
        assert!(
            !args(&ConnectOptions::default())
                .iter()
                .any(|arg| arg == "PreferredAuthentications=publickey")
        );
    }

    fn args(options: &ConnectOptions) -> Vec<String> {
        let (cmd, _saved) = ssh("ssh://u@h:2222", "true", options, options.forward_agent);
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn 主机密钥的两种报错分得开_警告四周的一圈去掉() {
        let changed = [
            "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@",
            "@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @",
            "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@",
            "IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!",
            "Host key verification failed.",
        ]
        .join("\r\n");
        assert!(login_hint(&changed, false).contains("known_hosts"));
        // 没见过的主机：确认框里点了取消，或者这次连接不许弹窗
        let untrusted = "Host key verification failed.";
        assert_eq!(login_hint(untrusted, false), tr("尚未信任此主机的密钥"));
        assert_eq!(
            without_banner(&changed).lines().collect::<Vec<_>>(),
            [
                "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!",
                "IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!",
                "Host key verification failed.",
            ]
        );
        // 别的报错里的 @ 不动
        assert_eq!(
            without_banner("user@host: Permission denied"),
            "user@host: Permission denied"
        );
    }

    #[test]
    fn 私钥文件和密码登录变成ssh的选项() {
        let has = |args: &[String], what: &str| args.iter().any(|arg| arg == what);
        let plain = args(&ConnectOptions::default());
        assert!(!has(&plain, "-i") && !has(&plain, "-A"));
        assert!(!plain.iter().any(|arg| arg.starts_with("Preferred")));
        // 目标和命令在最后，前面有 --
        assert_eq!(plain[plain.len() - 3..], ["--", "ssh://u@h:2222", "true"]);
        // 主机密钥怎么核对不设：没见过的主机由 ssh 来问。测试工具才直接记下
        assert!(
            !plain
                .iter()
                .any(|arg| arg.contains("StrictHostKeyChecking"))
        );
        let trusting = args(&ConnectOptions {
            accept_new_host_keys: true,
            ..Default::default()
        });
        assert!(has(&trusting, "StrictHostKeyChecking=accept-new"));

        let key = args(&ConnectOptions {
            identity_file: Some("C:/keys/id".into()),
            forward_agent: true,
            ..Default::default()
        });
        let at = key.iter().position(|arg| arg == "-i").unwrap();
        assert_eq!(key[at + 1], "C:/keys/id");
        assert!(has(&key, "IdentitiesOnly=yes") && has(&key, "-A"));

        let password = args(&ConnectOptions {
            password_auth: true,
            ..Default::default()
        });
        assert!(has(
            &password,
            "PreferredAuthentications=keyboard-interactive,password"
        ));
        assert!(!has(&password, "-i"));
    }

    #[test]
    fn 启动脚本接到常驻进程上_安装脚本不删正在用的那一版() {
        let boot = bootstrap_script("1.5.0-abc");
        assert!(boot.contains(r#"exec "$A" --connect"#));
        assert!(boot.contains("/.kuai/1.5.0-abc/kuaid"));
        let install = install_script("1.5.0-abc", 1234);
        assert!(install.contains(r#"!= "1234""#), "{install}");
        assert!(install.contains(r#"K=$(cat "$R/run/current" 2>/dev/null || true)"#));
        assert!(install.contains(r#""$V"|"$K") ;;"#));
        // 整个脚本包在单引号里，里面不能再有单引号
        for script in [&boot, &install] {
            assert_eq!(script.matches('\'').count(), 2, "{script}");
        }
    }

    /// 安装脚本真交给 sh 跑：上传中途断了（只收到一半）就删掉临时文件、以 INSTALL_INCOMPLETE 退出，
    /// 不留下半个程序；收全了装好、可以执行。
    #[cfg(unix)]
    #[test]
    fn 安装脚本收不全就不装_收全了装好() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        let home = std::env::temp_dir().join(format!("fr-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let data = b"#!/bin/sh\necho kuaid\n".repeat(4096);
        let run = |sent: &[u8]| {
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(install_script("9.9.9-test", data.len()))
                .env("HOME", &home)
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(sent).unwrap();
            child.wait().unwrap().code()
        };
        let dir = home.join(".kuai/9.9.9-test");
        assert_eq!(run(&data[..data.len() / 2]), Some(INSTALL_INCOMPLETE));
        assert!(!dir.join("kuaid").exists());
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "临时文件没删掉"
        );
        assert_eq!(run(&data), Some(0));
        assert_eq!(std::fs::read(dir.join("kuaid")).unwrap(), data);
        let mode = std::fs::metadata(dir.join("kuaid"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn 测试连接的脚本只报告系统_不碰_kuaid() {
        let (sh, cmd) = (probe_script(), windows_probe_script());
        for script in [&sh, &cmd] {
            assert!(script.contains(MISSING_MARKER), "{script}");
            assert!(!script.contains("kuai"), "{script}");
        }
        assert_eq!(sh.matches('\'').count(), 2, "{sh}");
        // 交给 cmd 的那一条：外面一对双引号，里面没有会被 PowerShell 改掉的字符
        assert_eq!(cmd.matches('"').count(), 2, "{cmd}");
        assert!(!cmd.contains('$') && !cmd.contains('`'), "{cmd}");
    }

    #[test]
    fn windows_的脚本交给_cmd_里面没有会被_powershell_改掉的字符() {
        let boot = windows_bootstrap_script("1.11.0-abc");
        // 主目录延迟展开：用户名里的 & 和括号不会被 cmd 当成语法
        assert!(boot.starts_with("cmd /v:on /c "), "{boot}");
        assert!(boot.contains(r"!USERPROFILE!\.kuai\1.11.0-abc"), "{boot}");
        assert!(!boot.contains("%USERPROFILE%"), "{boot}");
        assert!(
            boot.ends_with(r#"echo KUAID-START& kuaid.exe --connect""#),
            "{boot}"
        );
        let install = windows_install_script("1.11.0-abc");
        assert!(install.contains("tar -xf - -C 1.11.0-abc.tmp"), "{install}");
        assert!(
            install.ends_with(r#"1.11.0-abc\kuaid.exe --installed""#),
            "{install}"
        );
        for script in [&boot, &install] {
            // 整个脚本包在一对双引号里，PowerShell 原样交给 cmd
            assert!(script.starts_with("cmd /v:on /c \""), "{script}");
            assert_eq!(script.matches('"').count(), 2, "{script}");
            assert!(!script.contains(['$', '`', '\'']), "{script}");
        }
    }

    #[test]
    fn 认得出服务器是哪种系统() {
        assert_eq!(server_os("Linux"), Some(Os::Linux));
        assert_eq!(server_os("Windows"), Some(Os::Windows));
        assert_eq!(server_os("MINGW64_NT-10.0-26300"), Some(Os::Windows));
        assert_eq!(server_os("Darwin"), None);
    }

    #[test]
    fn tar_包系统的_tar_解得开() {
        let data: Vec<u8> = (0..70_001u32).map(|i| (i % 251) as u8).collect();
        let dll: Vec<u8> = (0..1_000u32).map(|i| (i % 7) as u8).collect();
        let archive = tar_files(&[("kuaid.exe", data.clone()), ("conpty.dll", dll.clone())]);
        assert_eq!(archive.len() % 512, 0);
        assert_eq!(&archive[..9], b"kuaid.exe");
        assert_eq!(&archive[124..136], b"00000210561\0");
        assert_eq!(&archive[512..512 + data.len()], &data[..]);
        // 第二个文件的头从第一个文件补齐后的下一块开始
        let second = 512 + data.len().div_ceil(512) * 512;
        assert_eq!(&archive[second..second + 10], b"conpty.dll");
        // 校验和对得上：按 tar 的算法重算一遍
        for at in [0, second] {
            let mut header = archive[at..at + 512].to_vec();
            let stored = std::str::from_utf8(&header[148..154]).unwrap().to_string();
            header[148..156].copy_from_slice(b"        ");
            let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
            assert_eq!(stored, format!("{sum:06o}"));
        }
        // 有 tar 的话真的解一遍（Windows 10 1803 起自带）
        let dir = std::env::temp_dir().join(format!("fr-tar-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let spawned = Command::new("tar")
            .args(["-xf", "-", "-C"])
            .arg(&dir)
            .stdin(Stdio::piped())
            .spawn();
        if let Ok(mut child) = spawned {
            child.stdin.take().unwrap().write_all(&archive).unwrap();
            assert!(child.wait().unwrap().success());
            assert_eq!(std::fs::read(dir.join("kuaid.exe")).unwrap(), data);
            assert_eq!(std::fs::read(dir.join("conpty.dll")).unwrap(), dll);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn 经跳板机时用_proxy_command_转接_各台的选项放进环境变量() {
        let options = ConnectOptions {
            jumps: vec![
                Jump {
                    name: "bastion".into(),
                    destination: "ops@203.0.113.10".into(),
                    identity_file: Some("C:/keys/b".into()),
                    ..Default::default()
                },
                Jump {
                    name: "inner".into(),
                    destination: "me@10.0.0.5".into(),
                    password_auth: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let (cmd, _held) = ssh("me@10.20.1.15", "true", &options, false);
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        // 目标服务器经最后一台跳板机（第 1 台）转接；自己不设超时，开着 VERBOSE 看进度
        let proxy = args
            .iter()
            .find(|arg| arg.starts_with("ProxyCommand="))
            .unwrap();
        assert!(proxy.ends_with("--ssh-hop 1 %h %p"), "{proxy}");
        assert!(!args.iter().any(|arg| arg.starts_with("ConnectTimeout")));
        assert!(args.iter().any(|arg| arg == "LogLevel=VERBOSE"));
        assert_eq!(args[args.len() - 3..], ["--", "me@10.20.1.15", "true"]);
        // 各台跳板机的登录选项在环境变量里，按顺序
        let hops = cmd
            .get_envs()
            .find(|(name, _)| *name == hop::HOPS_ENV)
            .and_then(|(_, value)| value)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let hops: Value = serde_json::from_str(&hops).unwrap();
        assert_eq!(hops[0]["destination"], "ops@203.0.113.10");
        let first: Vec<&str> = hops[0]["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(first.contains(&"C:/keys/b") && first.contains(&"LogLevel=VERBOSE"));
        assert!(
            hops[1]["args"]
                .as_array()
                .unwrap()
                .iter()
                .any(|arg| arg == "PreferredAuthentications=keyboard-interactive,password")
        );

        // 不经跳板机：和以前一样
        let (plain, _held) = ssh("u@h", "true", &ConnectOptions::default(), false);
        assert!(plain.get_envs().all(|(name, _)| name != hop::HOPS_ENV));
        let plain: Vec<String> = plain
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(plain.iter().any(|arg| arg == "ConnectTimeout=10"));
        assert!(!plain.iter().any(|arg| arg.starts_with("ProxyCommand")));
        assert!(!plain.iter().any(|arg| arg == "LogLevel=VERBOSE"));
    }

    #[test]
    fn 看得出是哪一段连不上() {
        use Trouble::*;
        // 没有跳板机：都是目标服务器的事
        assert_eq!(trouble(0, 0, None, false, false), Target);
        // 一台跳板机：它自己连不上、登录不了
        assert_eq!(trouble(1, 0, Some(0), false, false), Jump(0));
        // 登录上了，转接不到目标
        assert_eq!(trouble(1, 1, Some(0), true, false), Beyond(0));
        // 登录上了，目标服务器拒绝登录；跳板机的 ssh 跟着结束不算它的错
        assert_eq!(trouble(1, 1, Some(0), false, false), Target);
        assert_eq!(trouble(1, 1, None, false, false), Target);
        // 都登录上了以后出的事（比如 kuaid 起不来）
        assert_eq!(trouble(1, 2, Some(0), false, false), Target);
        // 超时：跳板机登录上了，目标一直没接通；一台都没登录上
        assert_eq!(trouble(1, 1, None, false, true), Beyond(0));
        assert_eq!(trouble(1, 0, None, false, true), Jump(0));
        // 两台：第二台登录不了；第一台转接不到第二台
        assert_eq!(trouble(2, 1, Some(1), false, false), Jump(1));
        assert_eq!(trouble(2, 1, Some(0), true, false), Beyond(0));
        assert_eq!(trouble(2, 2, Some(1), true, false), Beyond(1));
    }

    #[test]
    fn 标准错误里的进度和标记不算报错() {
        let log = StderrLog::default();
        for line in [
            "Authenticated to 203.0.113.10 ([203.0.113.10]:22) using \"publickey\".\r",
            "channel 0: open failed: connect failed: Connection refused\r",
            "stdio forwarding failed\r",
            "KUAI-HOP-FAILED 0",
            "KUAI-HOP-FAILED 1",
            "kex_exchange_identification: Connection closed by remote host",
            "Connection closed by UNKNOWN port 65535",
            "Transferred: sent 2332, received 2816 bytes, in 0.4 seconds",
        ] {
            log.line(line);
        }
        assert_eq!(log.authenticated.load(Ordering::SeqCst), 1);
        // 最先失败的那一台
        assert_eq!(*log.failed_hop.lock().unwrap(), Some(0));
        assert!(log.tail.lock().unwrap().starts_with(
            "channel 0: open failed: connect failed: Connection refused\nstdio forwarding failed\n"
        ));
        let jumps = [Jump {
            name: "bastion".into(),
            destination: "ops@203.0.113.10".into(),
            ..Default::default()
        }];
        let message = connect_error("me@10.20.1.15", &log, &[], &jumps, false);
        assert_eq!(
            message,
            "已连接跳板机 bastion，但它无法连接 me@10.20.1.15：连接被拒绝。请确认跳板机能访问该主机地址。\
             （ssh：channel 0: open failed: connect failed: Connection refused\nstdio forwarding failed）"
        );
    }

    #[test]
    fn 密码不进调试输出() {
        let options = ConnectOptions {
            secret: Some("hunter2".into()),
            ..Default::default()
        };
        assert!(!format!("{options:?}").contains("hunter2"));
    }
}
