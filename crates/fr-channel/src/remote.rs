//! 服务器上的仓库：经一条 ssh 连接和 kuaid 通信，所有请求按编号在这条连接上复用。
//!
//! 用系统自带的 ssh（Windows 上是 `System32\OpenSSH\ssh.exe`），沿用用户的 `~/.ssh/config` 和密钥。
//! Windows 版 OpenSSH 不支持 ControlMaster，所以每台服务器只开一条连接，复用自己做。
//!
//! kuaid 在服务器上常驻：ssh 过去跑的是它的转接程序（`kuaid --connect`），由它接到常驻进程上，
//! 没在跑就拉起来，比客户端旧就让它原地换成新的。所以这条连接断了，开在 kuaid 里的终端还在。
//!
//! 线程：一个写线程把请求依次写进 ssh 的标准输入；一个读线程读响应，按编号交给等待的 future。

use fr_core::i18n::tr;
use fr_core::tf;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Cursor, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use fr_core::protocol::{
    self, DirListing, FilePut, FilePutReply, FsEntry, FsStat, FsWrite, FsWritten, GitResult, Hello,
    PROTOCOL_VERSION, Request, Response, TermExit, TermInfo, TermOpen, TermOpened,
};
use futures::FutureExt as _;
use futures::channel::{mpsc, oneshot};
use futures::future::{self, BoxFuture};
use serde_json::{Value, json};

use crate::binaries::Binaries;
use crate::{
    ChangeStream, GitOutput, GitStream, StreamItem, TermItem, TermStream, UploadItem,
    UploadSource, UploadStream,
};

/// 启动脚本找到 kuaid 时先打这一行；前面的输出（比如 `.bashrc` 打印的东西）都跳过
const START_MARKER: &str = "KUAID-START";
/// 没装 kuaid 时打这一行，后面跟 `uname -s` 和 `uname -m`
const MISSING_MARKER: &str = "KUAID-MISSING";
/// ssh 的标准错误留最后这么多，连接失败时拿来提示
const STDERR_TAIL: usize = 4096;
/// 上传文件时一块这么大：每块等 kuaid 回一声再发下一块，进度和中止都按块算
const UPLOAD_CHUNK: usize = 512 * 1024;

/// 和一台服务器的连接。丢弃时断开。
pub struct Connection {
    /// ssh 的目标：主机别名、`user@host` 或 `ssh://user@host:port`
    pub destination: String,
    pub hello: Hello,
    /// 从启动 ssh 到握手完成用的时间（含上传 kuaid）
    pub connect_time: Duration,
    /// 这次连接时上传了 kuaid
    pub installed: bool,
    inner: Arc<Inner>,
}

struct Inner {
    /// 交给写线程的请求；取走（变成 None）就关闭连接
    outgoing: Mutex<Option<Outgoing>>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
    alive: AtomicBool,
    child: Arc<Mutex<Child>>,
    stderr: Arc<StderrLog>,
}

/// 交给写线程的一条请求和它附带的字节
type Outgoing = std::sync::mpsc::Sender<(Request, Vec<u8>)>;

enum Pending {
    Call(oneshot::Sender<Result<(Value, Vec<u8>)>>),
    Stream(mpsc::UnboundedSender<Result<StreamItem>>),
    Watch(mpsc::UnboundedSender<()>),
    Term(mpsc::UnboundedSender<Result<TermItem>>),
}

enum Started {
    Ready(Arc<Inner>, Hello),
    Missing { os: String, arch: String },
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

impl std::fmt::Debug for ConnectOptions {
    /// 密码不进日志：只写有没有
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectOptions")
            .field("forward_agent", &self.forward_agent)
            .field("identity_file", &self.identity_file)
            .field("password_auth", &self.password_auth)
            .field("secret", &self.secret.as_ref().map(|_| "…"))
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
        let started = Instant::now();
        let binaries = Binaries::find()?;
        let dir = binaries.dir_name();
        let mut installed = false;
        loop {
            // _saved 留到握手完：登录阶段 ssh 要密码时用得上
            let (cmd, _saved) = ssh(
                destination,
                &bootstrap_script(&dir),
                options,
                options.forward_agent,
            );
            match start(cmd, true, destination, Some(options))? {
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
                        bail!(tr("上传 kuaid 后仍无法启动"));
                    }
                    if os != "Linux" {
                        bail!("{}", tf!("目前仅支持 Linux 服务器，该服务器为 {os}", os));
                    }
                    install(destination, options, &dir, &binaries.binary_for(&arch)?)?;
                    installed = true;
                }
            }
        }
    }

    /// 直接启动一个 kuaid 进程（测试用，比如本机的 `kuaid --stdio`）。
    pub fn spawn_stdio(cmd: Command) -> Result<Connection> {
        let started = Instant::now();
        match start(cmd, false, tr("本机 kuaid"), None)? {
            Started::Ready(inner, hello) => Ok(Connection {
                destination: tr("本机 kuaid").into(),
                hello,
                connect_time: started.elapsed(),
                installed: false,
                inner,
            }),
            Started::Missing { .. } => bail!(tr("kuaid 未启动")),
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
    pub fn term_open(&self, key: &str, cwd: &str, cols: u16, rows: u16) -> TermStream {
        let params = TermOpen {
            key: key.to_string(),
            cwd: cwd.to_string(),
            cols,
            rows,
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

    fn term_stream(&self, method: &str, params: Value) -> TermStream {
        let (tx, rx) = mpsc::unbounded();
        let id = self.inner.next_id();
        self.inner
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending::Term(tx.clone()));
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

    /// kuaid 里现在有哪些终端会话。
    pub fn term_list(&self) -> BoxFuture<'static, Result<Vec<TermInfo>>> {
        let call = self.call("term_list", json!({}), Vec::new());
        async move {
            let (mut result, _) = call.await?;
            serde_json::from_value(result["terms"].take())
                .context(tr("kuaid 返回的结果格式错误"))
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
    ) -> BoxFuture<'static, Result<(FsStat, Vec<u8>)>> {
        let params = json!({ "repo": repo, "path": path, "offset": offset, "len": len });
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

    /// 只看成没成的那几个（建目录、删除、改名）。
    pub(crate) fn fs_simple(&self, method: &str, params: Value) -> BoxFuture<'static, Result<()>> {
        let call = self.call(method, params, Vec::new());
        async move { call.await.map(|_| ()) }.boxed()
    }

    /// 把仓库里的一个文件一段段读回来，存到本机的 `dest`。
    pub(crate) fn fs_download(&self, repo: &str, path: &str, dest: PathBuf) -> UploadStream {
        let inner = Arc::downgrade(&self.inner);
        let (repo, path) = (repo.to_string(), path.to_string());
        UploadStream::spawn(move |tx| {
            let mut part = crate::local::PartFile::create(&dest)?;
            let mut sent = 0;
            loop {
                let connection = inner.upgrade().context(tr("连接已关闭"))?;
                let params = json!({ "repo": repo, "path": path, "offset": sent });
                let call = connection.call("fs_read", params, Vec::new());
                drop(connection);
                let (result, bytes) = futures::executor::block_on(call)?;
                let stat: FsStat = parsed(result)?;
                part.write(&bytes)?;
                sent += bytes.len() as u64;
                let progress = UploadItem::Progress {
                    sent,
                    total: stat.size.max(sent),
                };
                // 没人等了（用户取消了）：不传了，临时文件跟着 part 删掉
                if tx.unbounded_send(Ok(progress)).is_err() {
                    return Ok(());
                }
                if bytes.is_empty() || sent >= stat.size {
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
        // 关掉标准输入，kuaid 读到结尾就退出；再杀掉 ssh，免得服务器那边卡住时它不退
        self.inner.outgoing.lock().unwrap().take();
        if let Ok(mut child) = self.inner.child.lock() {
            kill_tree(&mut child);
            let _ = child.wait();
        }
    }
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
            return future::ready(Err(err)).boxed();
        }
        let guard = CancelOnDrop {
            inner: Arc::downgrade(self),
            id,
        };
        async move {
            let result = rx.await;
            drop(guard);
            result.unwrap_or_else(|_| Err(anyhow!(tr("与服务器的连接已断开"))))
        }
        .boxed()
    }

    fn send(&self, request: Request, payload: Vec<u8>) -> Result<()> {
        let outgoing = self.outgoing.lock().unwrap();
        let sender = outgoing.as_ref().context(tr("连接已关闭"))?;
        sender
            .send((request, payload))
            .map_err(|_| anyhow!(tr("与服务器的连接已断开")))
    }

    /// kuaid 替服务器上的 git 要密码：在另一个线程里问用户（要等用户填），再把回答发回去。
    fn ask(self: &Arc<Self>, ask_id: u64, prompt: String) {
        let inner = Arc::downgrade(self);
        std::thread::spawn(move || {
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
                Some(Pending::Term(tx)) => {
                    // 流的第一条带着会话的编号，其余的是输出
                    let item = match response.result.take() {
                        Some(opened) => serde_json::from_value::<TermOpened>(opened)
                            .map(|opened| TermItem::Opened {
                                term: opened.term,
                                created: opened.created,
                            })
                            .context(tr("kuaid 返回的结果格式错误")),
                        None => Ok(TermItem::Output(payload)),
                    };
                    let _ = tx.unbounded_send(item);
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
            Pending::Term(tx) => {
                let item = outcome.and_then(|result| {
                    let exit: TermExit = serde_json::from_value(result)
                        .context(tr("kuaid 返回的结果格式错误"))?;
                    Ok(TermItem::Exited(exit.exit))
                });
                let _ = tx.unbounded_send(item);
            }
        }
    }

    /// 连接断了：所有等待中的请求都以错误结束。
    fn fail_all(&self, reason: String) {
        self.alive.store(false, Ordering::Release);
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
                Pending::Term(tx) => {
                    let _ = tx.unbounded_send(Err(anyhow!(message.clone())));
                }
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

/// 启动 ssh（或 kuaid 本身），完成握手，开始收发。`options` 是连服务器时的连接选项。
fn start(
    mut cmd: Command,
    expect_marker: bool,
    destination: &str,
    options: Option<&ConnectOptions>,
) -> Result<Started> {
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
    let mut stdin = child.stdin.take().context(tr("无法获取 ssh 的标准输入"))?;
    let stdout = child.stdout.take().context(tr("无法获取 ssh 的标准输出"))?;
    let stderr = collect_stderr(child.stderr.take().context(tr("无法获取 ssh 的标准错误"))?);
    let child = Arc::new(Mutex::new(child));
    let jumps = options.map_or(&[][..], |options| &options.jumps);
    // 经跳板机连接时自己看着超时（见 Watchdog）；握手完或者这个函数返回就不看了
    let watchdog = (!jumps.is_empty()).then(|| {
        let on_stage = options.and_then(|options| options.on_stage.clone());
        Watchdog::start(child.clone(), stderr.clone(), jumps.len(), on_stage)
    });
    let give_up = |noise: &[String]| {
        let _ = child.lock().unwrap().wait();
        let timed_out = watchdog.as_ref().is_some_and(Watchdog::timed_out);
        connect_error(destination, &stderr, noise, jumps, timed_out)
    };

    // 握手请求先发出去，省一次往返。没装 kuaid 时远程的 sh 不读它，丢掉就是了
    let hello = Request {
        id: 0,
        method: "hello".into(),
        params: json!({ "protocol": PROTOCOL_VERSION }),
        bytes: 0,
    };
    let _ = protocol::write_message(&mut stdin, &hello, &[]);

    let mut reader = BufReader::with_capacity(1 << 20, stdout);
    if expect_marker {
        let mut noise = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                bail!("{}", give_up(&noise));
            }
            let line = line.trim_end();
            if line == START_MARKER {
                break;
            }
            if let Some(rest) = line.strip_prefix(MISSING_MARKER) {
                let _ = child.lock().unwrap().wait();
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
        bail!("{}", give_up(&[]));
    };
    drop(watchdog);
    if let Some(err) = response.error {
        bail!("{}", tf!("kuaid 握手失败：{err}", err));
    }
    let hello: Hello = serde_json::from_value(response.result.unwrap_or_default())
        .context(tr("kuaid 握手响应格式错误"))?;
    // 常驻的 kuaid 可以比客户端新（别的电脑上的客户端把它升上去了），它认得旧客户端；比客户端旧不行
    if hello.protocol < PROTOCOL_VERSION {
        bail!(
            "{}",
            tf!(
                "服务器上的 kuaid 版本过旧（协议 {protocol}，客户端为 {PROTOCOL_VERSION}），且无法自动替换。请在服务器上运行 ~/.kuai/*/kuaid --stop 后重新连接",
                protocol = hello.protocol,
                PROTOCOL_VERSION
            )
        );
    }

    let (tx, rx) = std::sync::mpsc::channel::<(Request, Vec<u8>)>();
    let inner = Arc::new(Inner {
        outgoing: Mutex::new(Some(tx)),
        pending: Mutex::default(),
        next_id: AtomicU64::new(1),
        alive: AtomicBool::new(true),
        child,
        stderr,
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
            Ok(None) => break tr("与服务器的连接已断开").to_string(),
            Err(err) => break tf!("与服务器的连接出错：{err}", err = format!("{:#}", err)),
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
        if let Some(index) = line.strip_prefix(crate::hop::FAILED) {
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
fn collect_stderr(mut stderr: ChildStderr) -> Arc<StderrLog> {
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
        child: Arc<Mutex<Child>>,
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
                    kill_tree(&mut child.lock().unwrap());
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
            "已连接跳板机 {name}，但无法从它连接 {next}：{hint}。请确认主机填写的是跳板机能访问到的地址。",
            name = jumps[index].name,
            next = jumps
                .get(index + 1)
                .map_or(destination, |jump| jump.destination.as_str()),
            hint = forward_hint(&stderr, timed_out)
        ),
    };
    if !stderr.is_empty() {
        message.push_str(&tf!("（ssh：{stderr}）", stderr));
    }
    if !noise.is_empty() {
        message.push_str(&tf!("（服务器输出：{join}）", join = noise.join(" / ")));
    }
    message
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
    } else if stderr.contains("Host key verification failed")
        || stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
    {
        tr("主机密钥与 known_hosts 中的记录不一致，请先在终端中使用 ssh 确认")
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
/// `jump` 是跳板机的名字（登录的是目标服务器时为 None）。
/// 第三项要留到登录完成：留着它，这个 ssh 要密码、口令时才会用存着的自动回答、弹窗里才写得出是哪台跳板机。
fn login(
    identity_file: Option<&str>,
    password_auth: bool,
    secret: Option<&str>,
    jump: Option<&str>,
    forward_agent: bool,
) -> (
    Vec<String>,
    Vec<(String, String)>,
    Option<crate::SavedSecret>,
) {
    let mut args: Vec<String> = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();
    let mut saved = None;
    let mut add = |items: &[&str]| args.extend(items.iter().map(|item| item.to_string()));
    if forward_agent {
        add(&["-A"]);
    }
    if let Some(key) = identity_file {
        // 指定了私钥就只用它，不把 ssh-agent 里的密钥挨个试一遍（试多了服务器会直接断开）
        add(&["-i", key, "-o", "IdentitiesOnly=yes"]);
    }
    if password_auth {
        add(&[
            "-o",
            "PreferredAuthentications=keyboard-interactive,password",
        ]);
    }
    match crate::ssh_env() {
        // 要密码或密钥口令时 ssh 启动 askpass 程序来问我们：存着的自动回答，没存的由界面弹窗
        // 密码只问一次：ssh 默认问三次，用户点了取消它还会接着再问两次。输错了点「重试」重连
        Some(askpass) => {
            env.extend(askpass.iter().cloned());
            add(&["-o", "NumberOfPasswordPrompts=1"]);
            if secret.is_some() || jump.is_some() {
                let kind = if password_auth {
                    crate::PromptKind::Password
                } else {
                    crate::PromptKind::Passphrase
                };
                let entry = crate::SavedSecret::register_for(secret, kind, jump);
                env.push((
                    fr_core::askpass::TAG_ENV.to_string(),
                    entry.tag().to_string(),
                ));
                saved = Some(entry);
            }
        }
        // 没有界面可以弹窗（比如测试工具）：只用密钥，要密码就直接失败
        None => add(&["-o", "BatchMode=yes"]),
    }
    (args, env, saved)
}

/// 每个 ssh 都带的：第一次连的主机自动记进 known_hosts（记录过的密钥对不上仍然拒绝）；保活
pub(crate) const KEEP_OPTIONS: &[&str] = &[
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=4",
];
/// 经跳板机连接时每个 ssh 都带的：登录成功时打一行「Authenticated to …」，用来看进度（见 [`Watchdog`]）
pub(crate) const STAGE_OPTIONS: &[&str] = &["-o", "LogLevel=VERBOSE"];

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
            );
            args.extend(STAGE_OPTIONS.iter().map(|item| item.to_string()));
            let tag = env
                .iter()
                .find(|(name, _)| name == fr_core::askpass::TAG_ENV)
                .map(|(_, tag)| tag.clone());
            held.extend(saved);
            hops.push(crate::hop::Hop {
                destination: jump.destination.clone(),
                args,
                tag,
            });
        }
        cmd.env(crate::hop::HOPS_ENV, crate::hop::encode(&hops));
        let helper = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("kuai"));
        let proxy = crate::hop::proxy_command(&helper, hops.len() - 1);
        cmd.args(["-o", &format!("ProxyCommand={proxy}")]);
        cmd.args(STAGE_OPTIONS);
    }
    cmd.arg("-T");
    cmd.args(KEEP_OPTIONS);
    cmd.args(["--", destination, remote_command]);
    (cmd, held)
}

pub(crate) fn ssh_program() -> PathBuf {
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        let builtin = PathBuf::from(root).join(r"System32\OpenSSH\ssh.exe");
        if builtin.is_file() {
            return builtin;
        }
    }
    PathBuf::from("ssh")
}

/// 远程执行的启动脚本：这一版的 kuaid 装了就启动它的转接程序，没装就报告系统和架构。
/// 包一层 `sh -c`，用户的登录 shell 是 fish 之类时也能跑。
fn bootstrap_script(dir: &str) -> String {
    format!(
        r#"sh -c 'A="$HOME/.kuai/{dir}/kuaid"; if [ -x "$A" ]; then echo {START_MARKER}; exec "$A" --connect; fi; echo "{MISSING_MARKER} $(uname -s) $(uname -m)"'"#
    )
}

/// 远程执行的安装脚本，kuaid 程序从标准输入进来。
fn install_script(dir: &str) -> String {
    format!(
        r#"sh -c 'set -e; R="$HOME/.kuai"; V="{dir}"; mkdir -p "$R/$V"; cat > "$R/$V/kuaid.tmp"; chmod 755 "$R/$V/kuaid.tmp"; mv -f "$R/$V/kuaid.tmp" "$R/$V/kuaid"; K=$(cat "$R/run/current" 2>/dev/null || true); cd "$R"; for x in *; do case "$x" in "$V"|"$K") ;; [0-9]*) rm -rf -- "$x" ;; esac; done; rm -rf -- "$HOME/.fork-remote"'"#
    )
}

/// 经 ssh 的标准输入上传 kuaid：先写临时文件再改名，免得留下半个文件；
/// 然后删掉 `~/.kuai/` 下其他版本的目录（名字以数字开头的），只留刚装的这一版和常驻进程正在用的
/// 那一版（它记在 `run/current` 里：下次连接时才换成新的，换之前它的程序不能没）；
/// 还有 1.2.2 以前产品叫 fork-remote 时装在 `~/.fork-remote` 的旧程序。
fn install(destination: &str, options: &ConnectOptions, dir: &str, binary: &[u8]) -> Result<()> {
    let script = install_script(dir);
    let (mut cmd, _saved) = ssh(destination, &script, options, false);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    no_window(&mut cmd);
    let mut child = cmd.spawn().context(tr("无法启动 ssh"))?;
    let mut stdin = child.stdin.take().context(tr("无法获取 ssh 的标准输入"))?;
    let data = binary.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&data));
    let out = child.wait_with_output().context(tr("上传 kuaid 失败"))?;
    let written = writer.join();
    if !out.status.success() {
        bail!(
            "{}",
            tf!(
                "上传 kuaid 失败：{stderr}",
                stderr = String::from_utf8_lossy(&out.stderr).trim()
            )
        );
    }
    if !matches!(written, Ok(Ok(()))) {
        bail!(tr("上传 kuaid 时连接中断"));
    }
    Ok(())
}

pub(crate) fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：不弹出控制台窗口
        cmd.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(options: &ConnectOptions) -> Vec<String> {
        let (cmd, _saved) = ssh("ssh://u@h:2222", "true", options, options.forward_agent);
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn 私钥文件和密码登录变成ssh的选项() {
        let has = |args: &[String], what: &str| args.iter().any(|arg| arg == what);
        let plain = args(&ConnectOptions::default());
        assert!(!has(&plain, "-i") && !has(&plain, "-A"));
        assert!(!plain.iter().any(|arg| arg.starts_with("Preferred")));
        // 目标和命令在最后，前面有 --
        assert_eq!(plain[plain.len() - 3..], ["--", "ssh://u@h:2222", "true"]);

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
        let install = install_script("1.5.0-abc");
        assert!(install.contains(r#"K=$(cat "$R/run/current" 2>/dev/null || true)"#));
        assert!(install.contains(r#""$V"|"$K") ;;"#));
        // 整个脚本包在单引号里，里面不能再有单引号
        for script in [&boot, &install] {
            assert_eq!(script.matches('\'').count(), 2, "{script}");
        }
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
                    destination: "feng@10.0.0.5".into(),
                    password_auth: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let (cmd, _held) = ssh("feng@10.20.1.15", "true", &options, false);
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
        assert_eq!(args[args.len() - 3..], ["--", "feng@10.20.1.15", "true"]);
        // 各台跳板机的登录选项在环境变量里，按顺序
        let hops = cmd
            .get_envs()
            .find(|(name, _)| *name == crate::hop::HOPS_ENV)
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
        assert!(
            plain
                .get_envs()
                .all(|(name, _)| name != crate::hop::HOPS_ENV)
        );
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
        let message = connect_error("feng@10.20.1.15", &log, &[], &jumps, false);
        assert_eq!(
            message,
            "已连接跳板机 bastion，但无法从它连接 feng@10.20.1.15：连接被拒绝。请确认主机填写的是跳板机能访问到的地址。\
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
