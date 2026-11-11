//! 仓库通道：界面只认 [`Channel`]，不区分本机仓库和服务器上的仓库。
//!
//! - 本机仓库：在进程内直接启动 git。
//! - 服务器上的仓库：经一条 ssh 连接交给服务器上的 fr-agent，见 [`Connection`]。
//!
//! 调用都返回 future，不阻塞调用方；future 或流被丢弃时取消请求，对应的 git 进程会被杀掉。

mod agents;
mod local;
mod remote;

use fr_core::i18n::tr;
use fr_core::tf;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Result, bail};
use futures::Stream;
use futures::channel::mpsc;
use futures::future::BoxFuture;

pub use fr_core::askpass::PromptKind;
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
            Some(code) if self.stderr.is_empty() => bail!("{}", tf!("git 退出码 {code}", code)),
            Some(_) => bail!("{}", self.stderr),
            None => bail!(tr("git 被中止")),
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
        bail!(tr("init_prompts 只能调用一次"));
    }
    let exe = std::env::current_exe()?;
    // 这次连接存了密码、口令的先自动回答；没存的、答过一次还问的，才交给界面弹窗
    let env = fr_core::askpass::serve_tagged(&exe, move |prompt, tag| {
        saved_answer(tag, prompt).or_else(|| handler(prompt))
    })?;
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

/// 一个 ssh 进程可以自动回答的密码或口令。
struct Saved {
    secret: String,
    kind: PromptKind,
    /// 只答一次：答错了 ssh 会再问，那时交给弹窗，不拿同一个错的反复答
    used: bool,
}

/// 标记（ssh 进程环境变量 `FR_ASKPASS_TAG` 的值）→ 它可以用的密码、口令
static SAVED: LazyLock<Mutex<HashMap<String, Saved>>> = LazyLock::new(Mutex::default);

/// 登记期间，带着这个标记的 ssh 要密码（或口令）时自动回答；丢弃就撤掉。
pub(crate) struct SavedSecret {
    tag: String,
}

impl SavedSecret {
    pub(crate) fn register(secret: &str, kind: PromptKind) -> SavedSecret {
        let tag = fr_core::askpass::random_token();
        let saved = Saved {
            secret: secret.to_string(),
            kind,
            used: false,
        };
        SAVED.lock().unwrap().insert(tag.clone(), saved);
        SavedSecret { tag }
    }

    /// 要设到 ssh 进程环境变量 `FR_ASKPASS_TAG` 上的值。
    pub(crate) fn tag(&self) -> &str {
        &self.tag
    }
}

impl Drop for SavedSecret {
    fn drop(&mut self) {
        SAVED.lock().unwrap().remove(&self.tag);
    }
}

/// 这次提问能不能用存着的密码、口令回答：标记对得上、问的正是存的那一种、还没答过。
fn saved_answer(tag: Option<&str>, prompt: &str) -> Option<String> {
    let mut saved = SAVED.lock().unwrap();
    let entry = saved.get_mut(tag?)?;
    if entry.used || entry.kind != fr_core::askpass::prompt_kind(prompt) {
        return None;
    }
    entry.used = true;
    Some(entry.secret.clone())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 存着的密码只答一次_只答问对的那一种() {
        let password = SavedSecret::register("pw", PromptKind::Password);
        let tag = password.tag().to_string();
        // 没带标记、标记不对：不答
        assert_eq!(saved_answer(None, "u@h's password: "), None);
        assert_eq!(saved_answer(Some("别的"), "u@h's password: "), None);
        // 问的是私钥口令、主机指纹：不答，也不算用掉
        let passphrase_prompt = "Enter passphrase for key 'k': ";
        assert_eq!(saved_answer(Some(&tag), passphrase_prompt), None);
        assert_eq!(saved_answer(Some(&tag), "… (yes/no/[fingerprint])? "), None);
        assert_eq!(
            saved_answer(Some(&tag), "u@h's password: ").as_deref(),
            Some("pw")
        );
        // 答过了还问（密码不对）：不再答，交给弹窗
        assert_eq!(saved_answer(Some(&tag), "u@h's password: "), None);

        // 存的是私钥口令：只答口令
        let passphrase = SavedSecret::register("pp", PromptKind::Passphrase);
        let tag = passphrase.tag().to_string();
        assert_eq!(saved_answer(Some(&tag), "u@h's password: "), None);
        assert_eq!(
            saved_answer(Some(&tag), passphrase_prompt).as_deref(),
            Some("pp")
        );
        // 撤掉以后不答
        let again = SavedSecret::register("pp", PromptKind::Passphrase);
        let tag = again.tag().to_string();
        drop(again);
        assert_eq!(saved_answer(Some(&tag), passphrase_prompt), None);
    }
}
