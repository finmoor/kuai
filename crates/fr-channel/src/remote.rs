//! 服务器上的仓库：经一条 ssh 连接和 fr-agent 通信，所有请求按编号在这条连接上复用。
//!
//! 用系统自带的 ssh（Windows 上是 `System32\OpenSSH\ssh.exe`），沿用用户的 `~/.ssh/config` 和密钥。
//! Windows 版 OpenSSH 不支持 ControlMaster，所以每台服务器只开一条连接，复用自己做。
//!
//! 线程：一个写线程把请求依次写进 ssh 的标准输入；一个读线程读响应，按编号交给等待的 future。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use fr_core::protocol::{self, DirListing, GitResult, Hello, PROTOCOL_VERSION, Request, Response};
use futures::FutureExt as _;
use futures::channel::{mpsc, oneshot};
use futures::future::{self, BoxFuture};
use serde_json::{Value, json};

use crate::agents::Agents;
use crate::{ChangeStream, GitOutput, GitStream, StreamItem};

/// 启动脚本找到 agent 时先打这一行；前面的输出（比如 `.bashrc` 打印的东西）都跳过
const START_MARKER: &str = "FR-AGENT-START";
/// 没装 agent 时打这一行，后面跟 `uname -s` 和 `uname -m`
const MISSING_MARKER: &str = "FR-AGENT-MISSING";
/// ssh 的标准错误留最后这么多，连接失败时拿来提示
const STDERR_TAIL: usize = 4096;

/// 和一台服务器的连接。丢弃时断开。
pub struct Connection {
    /// ssh 的目标：主机别名、`user@host` 或 `ssh://user@host:port`
    pub destination: String,
    pub hello: Hello,
    /// 从启动 ssh 到握手完成用的时间（含上传 agent）
    pub connect_time: Duration,
    /// 这次连接时上传了 agent
    pub installed: bool,
    inner: Arc<Inner>,
}

struct Inner {
    /// 交给写线程的请求；取走（变成 None）就关闭连接
    outgoing: Mutex<Option<Outgoing>>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
    alive: AtomicBool,
    child: Mutex<Child>,
    stderr_tail: Arc<Mutex<String>>,
}

/// 交给写线程的一条请求和它附带的字节
type Outgoing = std::sync::mpsc::Sender<(Request, Vec<u8>)>;

enum Pending {
    Call(oneshot::Sender<Result<(Value, Vec<u8>)>>),
    Stream(mpsc::UnboundedSender<Result<StreamItem>>),
    Watch(mpsc::UnboundedSender<()>),
}

enum Started {
    Ready(Arc<Inner>, Hello),
    Missing { os: String, arch: String },
}

/// 连接选项。
#[derive(Debug, Clone, Default)]
pub struct ConnectOptions {
    /// 转发本机的 ssh-agent（`ssh -A`）：服务器上的 git 推拉可以用本机的密钥。
    /// 服务器上有 root 权限的人在连接期间也能用它，只对信任的服务器打开
    pub forward_agent: bool,
}

impl Connection {
    /// 连接服务器：经 ssh 启动 agent；没装或版本不对就先上传再连。
    /// 会阻塞（建立连接要几百毫秒，第一次还要上传），放在后台线程里调用。
    pub fn connect(destination: &str, options: &ConnectOptions) -> Result<Connection> {
        let started = Instant::now();
        let agents = Agents::find()?;
        let dir = agents.dir_name();
        let mut installed = false;
        loop {
            let cmd = ssh(destination, &bootstrap_script(&dir), options.forward_agent);
            match start(cmd, true, destination)? {
                Started::Ready(inner, hello) => {
                    return Ok(Connection {
                        destination: destination.to_string(),
                        hello,
                        connect_time: started.elapsed(),
                        installed,
                        inner,
                    });
                }
                Started::Missing { os, arch } => {
                    if installed {
                        bail!("上传 agent 后还是启动不了");
                    }
                    if os != "Linux" {
                        bail!("暂时只支持 Linux 服务器，这台是 {os}");
                    }
                    install(destination, &dir, &agents.binary_for(&arch)?)?;
                    installed = true;
                }
            }
        }
    }

    /// 直接启动一个 agent 进程（测试用，比如本机的 `fr-agent --stdio`）。
    pub fn spawn_agent(cmd: Command) -> Result<Connection> {
        let started = Instant::now();
        match start(cmd, false, "本机 agent")? {
            Started::Ready(inner, hello) => Ok(Connection {
                destination: "本机 agent".into(),
                hello,
                connect_time: started.elapsed(),
                installed: false,
                inner,
            }),
            Started::Missing { .. } => bail!("agent 没有启动"),
        }
    }

    /// 连接还在不在。断开后所有调用都会失败，要重新连接。
    pub fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::Acquire)
    }

    pub fn ping(&self) -> BoxFuture<'static, Result<()>> {
        let call = self.call("ping", json!({}), Vec::new());
        async move { call.await.map(|_| ()) }.boxed()
    }

    pub(crate) fn list_dir(&self, path: &str) -> BoxFuture<'static, Result<DirListing>> {
        let call = self.call("list_dir", json!({ "path": path }), Vec::new());
        async move {
            let (result, _) = call.await?;
            serde_json::from_value(result).context("agent 返回的目录格式不对")
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
        async move {
            let (result, stdout) = call.await?;
            let result: GitResult =
                serde_json::from_value(result).context("agent 返回的结果格式不对")?;
            Ok(GitOutput {
                code: result.code,
                stdout,
                stderr: result.stderr,
                elapsed: Duration::from_millis(result.ms),
            })
        }
        .boxed()
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

    /// 发一个请求，等它的响应。返回的 future 被丢弃时向 agent 发取消。
    fn call(
        &self,
        method: &str,
        params: Value,
        payload: Vec<u8>,
    ) -> BoxFuture<'static, Result<(Value, Vec<u8>)>> {
        let id = self.inner.next_id();
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending::Call(tx));
        let request = Request {
            id,
            method: method.into(),
            params,
            bytes: payload.len() as u64,
        };
        if let Err(err) = self.inner.send(request, payload) {
            self.inner.pending.lock().unwrap().remove(&id);
            return future::ready(Err(err)).boxed();
        }
        let guard = CancelOnDrop {
            inner: Arc::downgrade(&self.inner),
            id,
        };
        async move {
            let result = rx.await;
            drop(guard);
            result.unwrap_or_else(|_| Err(anyhow!("与服务器的连接已断开")))
        }
        .boxed()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // 关掉标准输入，agent 读到结尾就退出；再杀掉 ssh，免得服务器那边卡住时它不退
        self.inner.outgoing.lock().unwrap().take();
        if let Ok(mut child) = self.inner.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Inner {
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn send(&self, request: Request, payload: Vec<u8>) -> Result<()> {
        let outgoing = self.outgoing.lock().unwrap();
        let sender = outgoing.as_ref().context("连接已关闭")?;
        sender
            .send((request, payload))
            .map_err(|_| anyhow!("与服务器的连接已断开"))
    }

    /// agent 替服务器上的 git 要密码：在另一个线程里问用户（要等用户填），再把回答发回去。
    fn ask(self: &Arc<Self>, ask_id: u64, prompt: String) {
        let inner = Arc::downgrade(self);
        std::thread::spawn(move || {
            let answer = crate::prompt_handler().and_then(|handler| handler(&prompt));
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
                _ => {}
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
                        serde_json::from_value(result).context("agent 返回的结果格式不对")?;
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
        }
    }

    /// 连接断了：所有等待中的请求都以错误结束。
    fn fail_all(&self, reason: String) {
        self.alive.store(false, Ordering::Release);
        let tail = self.stderr_tail.lock().unwrap().trim().to_string();
        let message = if tail.is_empty() {
            reason
        } else {
            format!("{reason}：{tail}")
        };
        for (_, waiter) in self.pending.lock().unwrap().drain() {
            match waiter {
                Pending::Call(tx) => {
                    let _ = tx.send(Err(anyhow!(message.clone())));
                }
                Pending::Stream(tx) => {
                    let _ = tx.unbounded_send(Err(anyhow!(message.clone())));
                }
                Pending::Watch(_) => {}
            }
        }
    }
}

/// future 或流被丢弃时，如果请求还没完成，就从等待表里拿掉并通知 agent 杀掉 git。
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
            let cancel = Request {
                id: 0,
                method: "cancel".into(),
                params: json!({ "id": self.id }),
                bytes: 0,
            };
            let _ = inner.send(cancel, Vec::new());
        }
    }
}

/// 启动 ssh（或 agent 本身），完成握手，开始收发。
fn start(mut cmd: Command, expect_marker: bool, destination: &str) -> Result<Started> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_window(&mut cmd);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("无法启动 {}", cmd.get_program().to_string_lossy()))?;
    let mut stdin = child.stdin.take().context("拿不到 ssh 的标准输入")?;
    let stdout = child.stdout.take().context("拿不到 ssh 的标准输出")?;
    let stderr_tail = collect_stderr(child.stderr.take().context("拿不到 ssh 的标准错误")?);

    // 握手请求先发出去，省一次往返。没装 agent 时远程的 sh 不读它，丢掉就是了
    let hello = Request {
        id: 0,
        method: "hello".into(),
        params: json!({}),
        bytes: 0,
    };
    let _ = protocol::write_message(&mut stdin, &hello, &[]);

    let mut reader = BufReader::with_capacity(1 << 20, stdout);
    if expect_marker {
        let mut noise = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                let _ = child.wait();
                bail!("{}", connect_error(destination, &stderr_tail, &noise));
            }
            let line = line.trim_end();
            if line == START_MARKER {
                break;
            }
            if let Some(rest) = line.strip_prefix(MISSING_MARKER) {
                let _ = child.wait();
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

    let Some((response, _)) = protocol::read_message::<_, Response>(&mut reader)? else {
        let _ = child.wait();
        bail!("{}", connect_error(destination, &stderr_tail, &[]));
    };
    if let Some(err) = response.error {
        bail!("agent 握手失败：{err}");
    }
    let hello: Hello = serde_json::from_value(response.result.unwrap_or_default())
        .context("agent 握手结果格式不对")?;
    if hello.protocol != PROTOCOL_VERSION {
        bail!(
            "agent 的协议版本是 {}，客户端是 {PROTOCOL_VERSION}",
            hello.protocol
        );
    }

    let (tx, rx) = std::sync::mpsc::channel::<(Request, Vec<u8>)>();
    let inner = Arc::new(Inner {
        outgoing: Mutex::new(Some(tx)),
        pending: Mutex::default(),
        next_id: AtomicU64::new(1),
        alive: AtomicBool::new(true),
        child: Mutex::new(child),
        stderr_tail,
    });
    std::thread::Builder::new()
        .name("fr-ssh-writer".into())
        .spawn(move || {
            let mut writer = BufWriter::with_capacity(1 << 16, stdin);
            while let Ok((request, payload)) = rx.recv() {
                if protocol::write_message(&mut writer, &request, &payload).is_err() {
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

fn read_loop(inner: Weak<Inner>, mut reader: BufReader<ChildStdout>) {
    let reason = loop {
        match protocol::read_message::<_, Response>(&mut reader) {
            Ok(Some((response, payload))) => match inner.upgrade() {
                Some(inner) => inner.dispatch(response, payload),
                None => return,
            },
            Ok(None) => break "与服务器的连接已断开".to_string(),
            Err(err) => break format!("与服务器的连接出错：{err:#}"),
        }
    };
    if let Some(inner) = inner.upgrade() {
        inner.fail_all(reason);
    }
}

/// 在后台读 ssh 的标准错误，只留最后一段。
fn collect_stderr(mut stderr: ChildStderr) -> Arc<Mutex<String>> {
    let tail = Arc::new(Mutex::new(String::new()));
    let sink = tail.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = stderr.read(&mut buf) {
            if n == 0 {
                break;
            }
            let mut tail = sink.lock().unwrap();
            tail.push_str(&String::from_utf8_lossy(&buf[..n]));
            if tail.len() > STDERR_TAIL {
                let mut cut = tail.len() - STDERR_TAIL;
                while !tail.is_char_boundary(cut) {
                    cut += 1;
                }
                tail.drain(..cut);
            }
        }
    });
    tail
}

/// 把 ssh 的报错翻译成用户看得懂的话。
fn connect_error(destination: &str, stderr_tail: &Mutex<String>, noise: &[String]) -> String {
    // 读线程可能还没把最后的报错收齐
    std::thread::sleep(Duration::from_millis(50));
    let stderr = stderr_tail.lock().unwrap().trim().to_string();
    let hint = if stderr.contains("Permission denied") {
        "登录被拒绝：密钥不被接受，或者密码不对、取消了输入"
    } else if stderr.contains("Could not resolve hostname") {
        "找不到这个主机名"
    } else if stderr.contains("timed out") {
        "连接超时"
    } else if stderr.contains("Connection refused") {
        "对方拒绝连接，检查 SSH 服务和端口"
    } else if stderr.contains("Host key verification failed")
        || stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
    {
        "服务器的主机密钥和 known_hosts 里记录的不一致，先在终端里用 ssh 确认"
    } else {
        "连接失败"
    };
    let mut message = format!("连接 {destination}：{hint}");
    if !stderr.is_empty() {
        message.push_str(&format!("（ssh：{stderr}）"));
    }
    if !noise.is_empty() {
        message.push_str(&format!("（服务器输出：{}）", noise.join(" / ")));
    }
    message
}

/// 构造 ssh 命令。`--` 防止以 `-` 开头的目标被当成选项。
fn ssh(destination: &str, remote_command: &str, forward_agent: bool) -> Command {
    let mut cmd = Command::new(ssh_program());
    if forward_agent {
        cmd.arg("-A");
    }
    match crate::ssh_env() {
        // 要密码或密钥口令时 ssh 启动 askpass 程序来问我们，由界面弹窗
        // 密码只问一次：ssh 默认问三次，用户点了取消它还会接着再问两次。输错了点「重试」重连
        Some(env) => {
            cmd.envs(env.iter().map(|(k, v)| (k, v)));
            cmd.args(["-o", "NumberOfPasswordPrompts=1"]);
        }
        // 没有界面可以弹窗（比如测试工具）：只用密钥，要密码就直接失败
        None => {
            cmd.args(["-o", "BatchMode=yes"]);
        }
    }
    cmd.args([
        "-T",
        // 第一次连的主机自动记进 known_hosts；记录过的密钥对不上仍然拒绝
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=4",
        "--",
        destination,
        remote_command,
    ]);
    cmd
}

fn ssh_program() -> PathBuf {
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        let builtin = PathBuf::from(root).join(r"System32\OpenSSH\ssh.exe");
        if builtin.is_file() {
            return builtin;
        }
    }
    PathBuf::from("ssh")
}

/// 远程执行的启动脚本：agent 在就启动它，不在就报告系统和架构。
/// 包一层 `sh -c`，用户的登录 shell 是 fish 之类时也能跑。
fn bootstrap_script(dir: &str) -> String {
    format!(
        r#"sh -c 'A="$HOME/.fork-remote/{dir}/fr-agent"; if [ -x "$A" ]; then echo {START_MARKER}; exec "$A" --stdio; fi; echo "{MISSING_MARKER} $(uname -s) $(uname -m)"'"#
    )
}

/// 经 ssh 的标准输入上传 agent：先写临时文件再改名，免得留下半个文件；
/// 然后删掉 `~/.fork-remote/` 下其他版本的目录（名字以数字开头的）。
fn install(destination: &str, dir: &str, binary: &[u8]) -> Result<()> {
    let script = format!(
        r#"sh -c 'set -e; R="$HOME/.fork-remote"; V="{dir}"; mkdir -p "$R/$V"; cat > "$R/$V/fr-agent.tmp"; chmod 755 "$R/$V/fr-agent.tmp"; mv -f "$R/$V/fr-agent.tmp" "$R/$V/fr-agent"; cd "$R"; for x in *; do case "$x" in "$V") ;; [0-9]*) rm -rf -- "$x" ;; esac; done'"#
    );
    let mut cmd = ssh(destination, &script, false);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    no_window(&mut cmd);
    let mut child = cmd.spawn().context("无法启动 ssh")?;
    let mut stdin = child.stdin.take().context("拿不到 ssh 的标准输入")?;
    let data = binary.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&data));
    let out = child.wait_with_output().context("上传 agent 失败")?;
    let written = writer.join();
    if !out.status.success() {
        bail!(
            "上传 agent 失败：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if !matches!(written, Ok(Ok(()))) {
        bail!("上传 agent 时连接中断");
    }
    Ok(())
}

fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：不弹出控制台窗口
        cmd.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}
