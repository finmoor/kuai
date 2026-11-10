//! 仓库通道：界面只认 [`Channel`]，不区分本机仓库和服务器上的仓库。
//!
//! - 本机仓库：在进程内直接启动 git。
//! - 服务器上的仓库：经一条 ssh 连接交给服务器上的 fr-agent，见 [`Connection`]。
//!
//! 调用都返回 future，不阻塞调用方；future 或流被丢弃时取消请求，对应的 git 进程会被杀掉。

mod agents;
mod local;
mod remote;

use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Result, bail};
use futures::Stream;
use futures::channel::mpsc;
use futures::future::BoxFuture;

pub use fr_core::protocol::{DirEntry, DirListing, Hello};
pub use remote::{ConnectOptions, Connection};

/// 一次 git 调用的结果。
#[derive(Debug, Clone, Default)]
pub struct GitOutput {
    /// 退出码；进程被杀掉时为空
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
    /// git 运行了多久（远程时是服务器上的时间，不含传输）
    pub elapsed: Duration,
}

impl GitOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// 退出码为 0 时返回标准输出；否则把标准错误当作错误信息。
    pub fn ok(self) -> Result<Vec<u8>> {
        match self.code {
            Some(0) => Ok(self.stdout),
            Some(code) if self.stderr.is_empty() => bail!("git 退出码 {code}"),
            Some(_) => bail!("{}", self.stderr),
            None => bail!("git 被中止"),
        }
    }
}

/// 流式输出的一项：一块标准输出，一块标准错误（进度），或者最后的结果（此时 `stdout` 为空）。
pub enum StreamItem {
    Chunk(Vec<u8>),
    /// 标准错误的一块，只有 [`Channel::git_progress`] 会给
    Progress(Vec<u8>),
    Done(GitOutput),
}

/// git 的流式输出。丢弃时取消。
pub struct GitStream {
    rx: mpsc::UnboundedReceiver<Result<StreamItem>>,
    _cancel: Box<dyn Send>,
}

impl Stream for GitStream {
    type Item = Result<StreamItem>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.rx).poll_next(cx)
    }
}

/// 仓库的变化通知：工作区的文件、HEAD、暂存区或引用变了就来一项，不带内容。丢弃时停止监听。
/// 监听装不上或者连接断了，流就结束。
pub struct ChangeStream {
    rx: mpsc::UnboundedReceiver<()>,
    _cancel: Box<dyn Send>,
}

impl Stream for ChangeStream {
    type Item = ();

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<()>> {
        Pin::new(&mut self.rx).poll_next(cx)
    }
}

/// 回答密码请求的函数：参数是提示文字，返回 None 表示用户取消。在后台线程里调用，可以一直等。
type PromptHandler = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

static PROMPT_HANDLER: OnceLock<PromptHandler> = OnceLock::new();
/// 加到 ssh 进程上的环境变量：连接服务器要密码时也来问我们
static SSH_ENV: OnceLock<Vec<(String, String)>> = OnceLock::new();

/// 程序启动时调用一次：以后本机的 git、连接服务器的 ssh、服务器上的 git 要密码或密钥口令时，
/// 都调用 `handler`。不调用的话，要密码的操作直接失败。
///
/// 本机这边把当前程序设成 askpass 程序，所以 `main` 的开头要先调用
/// `fr_core::askpass::run_helper`。
pub fn init_prompts(
    handler: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
) -> Result<()> {
    let handler: PromptHandler = Arc::new(handler);
    if PROMPT_HANDLER.set(handler.clone()).is_err() {
        bail!("init_prompts 只能调用一次");
    }
    let exe = std::env::current_exe()?;
    let env = fr_core::askpass::serve(&exe, move |prompt| handler(prompt))?;
    fr_core::git::set_extra_env(env.clone());
    let _ = SSH_ENV.set(env);
    Ok(())
}

pub(crate) fn prompt_handler() -> Option<PromptHandler> {
    PROMPT_HANDLER.get().cloned()
}

pub(crate) fn ssh_env() -> Option<&'static [(String, String)]> {
    SSH_ENV.get().map(Vec::as_slice)
}

/// 仓库所在的地方：本机，或者某台服务器的连接。
#[derive(Clone)]
pub enum Channel {
    Local,
    Remote(Arc<Connection>),
}

impl Channel {
    /// 在仓库里执行 git，收齐输出。
    pub fn git(&self, repo: &str, args: Vec<String>) -> BoxFuture<'static, Result<GitOutput>> {
        self.git_with_input(repo, args, None)
    }

    /// 同 [`Self::git`]，`stdin` 写进 git 的标准输入（补丁、提交信息等）。
    pub fn git_with_input(
        &self,
        repo: &str,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> BoxFuture<'static, Result<GitOutput>> {
        match self {
            Channel::Local => local::git(repo, args, stdin),
            Channel::Remote(conn) => conn.git(repo, args, stdin),
        }
    }

    /// 执行 git，输出边产生边送回来。
    pub fn git_stream(&self, repo: &str, args: Vec<String>) -> GitStream {
        match self {
            Channel::Local => local::git_stream(repo, args, false),
            Channel::Remote(conn) => conn.git_stream(repo, args, false),
        }
    }

    /// 同 [`Self::git_stream`]，标准错误也边产生边送回来（[`StreamItem::Progress`]），
    /// 用来显示获取、推送的进度。
    pub fn git_progress(&self, repo: &str, args: Vec<String>) -> GitStream {
        match self {
            Channel::Local => local::git_stream(repo, args, true),
            Channel::Remote(conn) => conn.git_stream(repo, args, true),
        }
    }

    /// 监听仓库的变化。
    pub fn watch(&self, repo: &str) -> ChangeStream {
        match self {
            Channel::Local => local::watch(repo),
            Channel::Remote(conn) => conn.watch(repo),
        }
    }

    /// 列出目录下的子目录，标出哪些是仓库。
    pub fn list_dir(&self, path: &str) -> BoxFuture<'static, Result<DirListing>> {
        match self {
            Channel::Local => local::list_dir(path),
            Channel::Remote(conn) => conn.list_dir(path),
        }
    }
}
