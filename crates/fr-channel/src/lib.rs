//! 仓库通道：界面只认 [`Channel`]，不区分本机仓库和服务器上的仓库。
//!
//! - 本机仓库：在进程内直接启动 git。
//! - 服务器上的仓库：经一条 ssh 连接交给服务器上的 kuaid，见 [`Connection`]。
//!
//! 调用都返回 future，不阻塞调用方；future 或流被丢弃时取消请求，对应的 git 进程会被杀掉。
//!
//! 服务器上的终端也开在 kuaid 里（[`Connection::term_open`]）：流被丢弃只是断开，里面的程序接着跑。
//! 往终端里发图片、文件：先传到服务器上（[`Connection::upload`]），再把路径贴进终端。
//!
//! 服务器可以经跳板机连接（[`ConnectOptions::jumps`]），见 `hop`。
//!
//! 仓库目录里的文件（文件页）：[`Channel::fs_list`] 这一组。本机仓库直接读写磁盘，服务器上的交给
//! kuaid，做事的是同一份代码（`fr_core::files`），路径都相对仓库根。

mod binaries;
mod hop;
mod local;
mod remote;

use fr_core::i18n::tr;
use fr_core::tf;
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Result, bail};
use futures::FutureExt as _;
use futures::Stream;
use futures::channel::mpsc;
use futures::future::BoxFuture;
use serde_json::json;

pub use fr_core::askpass::PromptKind;
pub use fr_core::protocol::{
    DirEntry, DirListing, FsEntry, FsExpect, FsStat, FsWrite, FsWritten, Hello, TermInfo,
};
pub use hop::run_helper as run_ssh_hop;
pub use remote::{ConnectOptions, Connection, Jump, StageHandler, local_kuai_root};

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
            None => bail!(tr("git 已中止")),
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

/// 终端流里的一项。
pub enum TermItem {
    /// 流的第一项：会话的编号；`created` 为假说明接上的是已有的会话
    Opened { term: String, created: bool },
    /// 程序的一块输出（接上已有会话时，开头几块是还原屏幕用的）
    Output(Vec<u8>),
    /// 程序结束了：退出码
    Exited(Option<i32>),
}

/// 一个终端的输出。丢弃时从终端上断开，里面的程序接着跑。连接断了，流以一个错误结束。
pub struct TermStream {
    rx: mpsc::UnboundedReceiver<Result<TermItem>>,
    _cancel: Box<dyn Send>,
}

impl Stream for TermStream {
    type Item = Result<TermItem>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.rx).poll_next(cx)
    }
}

/// 要传到服务器上的东西从哪来。
pub enum UploadSource {
    /// 本机的一个文件，边读边传
    File(PathBuf),
    /// 已经在内存里的（比如剪贴板里的图片）
    Bytes(Vec<u8>),
}

/// 上传、下载流里的一项。
pub enum UploadItem {
    /// 已经传了多少、一共多少（字节）
    Progress { sent: u64, total: u64 },
    /// 传完了：文件的完整路径（上传是服务器上的，下载是本机的）
    Done(String),
}

/// 一次上传或下载。丢弃就是不传了，已经传过去的那部分会删掉。出错时流以一个错误结束。
pub struct UploadStream {
    rx: mpsc::UnboundedReceiver<Result<UploadItem>>,
}

impl UploadStream {
    /// 在自己的线程里做（读写文件、等服务器回音都会卡住）；`work` 往给它的那一头报进度和结果。
    pub(crate) fn spawn(
        work: impl FnOnce(&mpsc::UnboundedSender<Result<UploadItem>>) -> Result<()> + Send + 'static,
    ) -> UploadStream {
        let (tx, rx) = mpsc::unbounded();
        let for_error = tx.clone();
        let spawned = std::thread::Builder::new()
            .name("kuai-transfer".into())
            .spawn(move || {
                if let Err(err) = work(&tx) {
                    let _ = tx.unbounded_send(Err(err));
                }
            });
        if let Err(err) = spawned {
            let _ = for_error.unbounded_send(Err(err.into()));
        }
        UploadStream { rx }
    }
}

impl Stream for UploadStream {
    type Item = Result<UploadItem>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.rx).poll_next(cx)
    }
}

/// 回答密码请求的函数：参数是提示文字，和「是哪台跳板机在问」（不是跳板机问的为 None）；
/// 返回 None 表示用户取消。在后台线程里调用，可以一直等。
type PromptHandler = Arc<dyn Fn(&str, Option<&str>) -> Option<String> + Send + Sync>;

static PROMPT_HANDLER: OnceLock<PromptHandler> = OnceLock::new();
/// 加到 ssh 进程上的环境变量：连接服务器要密码时也来问我们
static SSH_ENV: OnceLock<Vec<(String, String)>> = OnceLock::new();

/// 程序启动时调用一次：以后本机的 git、连接服务器的 ssh、服务器上的 git 要密码或密钥口令时，
/// 都调用 `handler`。不调用的话，要密码的操作直接失败。
///
/// 本机这边把当前程序设成 askpass 程序，所以 `main` 的开头要先调用
/// `fr_core::askpass::run_helper`。
pub fn init_prompts(
    handler: impl Fn(&str, Option<&str>) -> Option<String> + Send + Sync + 'static,
) -> Result<()> {
    let handler: PromptHandler = Arc::new(handler);
    if PROMPT_HANDLER.set(handler.clone()).is_err() {
        bail!(tr("init_prompts 只能调用一次"));
    }
    let exe = std::env::current_exe()?;
    // 这次连接存了密码、口令的先自动回答；没存的、答过一次还问的，才交给界面弹窗
    let env = fr_core::askpass::serve_tagged(&exe, move |prompt, tag| {
        if let Some(answer) = saved_answer(tag, prompt) {
            return Some(answer);
        }
        // 等用户回答的这段时间不算连接超时
        let _asking = Asking::start();
        handler(prompt, jump_of(tag).as_deref())
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

/// 正有几个密码请求在等用户回答。
static ASKING: AtomicUsize = AtomicUsize::new(0);

struct Asking;

impl Asking {
    fn start() -> Asking {
        ASKING.fetch_add(1, Ordering::SeqCst);
        Asking
    }
}

impl Drop for Asking {
    fn drop(&mut self) {
        ASKING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 是不是正等着用户回答密码请求（经跳板机连接时，这段时间不算超时）。
pub(crate) fn asking() -> bool {
    ASKING.load(Ordering::SeqCst) > 0
}

/// 一个 ssh 进程登记的东西：可以自动回答的密码或口令，和它是哪台跳板机的。
struct Saved {
    /// 存着的密码或口令；没存就只能弹窗问
    secret: Option<String>,
    kind: PromptKind,
    /// 只答一次：答错了 ssh 会再问，那时交给弹窗，不拿同一个错的反复答
    used: bool,
    /// 这个 ssh 连的是跳板机时，跳板机的名字：弹窗里写明是哪一台在要密码
    jump: Option<String>,
}

/// 标记（ssh 进程环境变量 `FR_ASKPASS_TAG` 的值）→ 它可以用的密码、口令
static SAVED: LazyLock<Mutex<HashMap<String, Saved>>> = LazyLock::new(Mutex::default);

/// 登记期间，带着这个标记的 ssh 要密码（或口令）时自动回答；丢弃就撤掉。
pub(crate) struct SavedSecret {
    tag: String,
}

impl SavedSecret {
    #[cfg(test)]
    pub(crate) fn register(secret: &str, kind: PromptKind) -> SavedSecret {
        Self::register_for(Some(secret), kind, None)
    }

    /// `secret` 是存着的密码或口令（`kind` 说是哪一种）；`jump` 是跳板机的名字（连的是目标服务器时为 None）。
    pub(crate) fn register_for(
        secret: Option<&str>,
        kind: PromptKind,
        jump: Option<&str>,
    ) -> SavedSecret {
        let tag = fr_core::askpass::random_token();
        let saved = Saved {
            secret: secret.map(str::to_string),
            kind,
            used: false,
            jump: jump.map(str::to_string),
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
    let secret = entry.secret.clone()?;
    entry.used = true;
    Some(secret)
}

/// 带着这个标记的 ssh 连的是哪台跳板机（不是跳板机就是 None）。
fn jump_of(tag: Option<&str>) -> Option<String> {
    SAVED.lock().unwrap().get(tag?)?.jump.clone()
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

    /// 仓库里 `dir` 这一层的文件和目录（`dir` 相对仓库根，空字符串是仓库根；不含 `.git`）。
    pub fn fs_list(&self, repo: &str, dir: &str) -> BoxFuture<'static, Result<Vec<FsEntry>>> {
        match self {
            Channel::Local => {
                let (repo, dir) = (repo.to_string(), dir.to_string());
                local::blocking(move || fr_core::files::list(&repo, &dir))
            }
            Channel::Remote(conn) => conn.fs_list(repo, dir),
        }
    }

    /// 仓库里的一个文件现在的情况；不在了不算出错，`exists` 为假。
    pub fn fs_stat(&self, repo: &str, path: &str) -> BoxFuture<'static, Result<FsStat>> {
        match self {
            Channel::Local => {
                let (repo, path) = (repo.to_string(), path.to_string());
                local::blocking(move || fr_core::files::stat(&repo, &path))
            }
            Channel::Remote(conn) => conn.fs_stat(repo, path),
        }
    }

    /// 读出整个文件。比 `limit` 大的不读完：返回的字节数少于 [`FsStat::size`]，调用方看大小决定怎么办。
    pub fn fs_open(
        &self,
        repo: &str,
        path: &str,
        limit: u64,
    ) -> BoxFuture<'static, Result<(FsStat, Vec<u8>)>> {
        let (channel, repo, path) = (self.clone(), repo.to_string(), path.to_string());
        async move {
            let first = limit.min(fr_core::files::READ_MAX);
            let (stat, mut bytes) = channel.fs_read(&repo, &path, 0, first).await?;
            if stat.size > limit {
                return Ok((stat, bytes));
            }
            // 读的这段时间里文件可能变长：读到没有了为止，但不超过上限
            while (bytes.len() as u64) < stat.size && (bytes.len() as u64) < limit {
                let rest = limit - bytes.len() as u64;
                let (_, more) = channel
                    .fs_read(&repo, &path, bytes.len() as u64, rest)
                    .await?;
                if more.is_empty() {
                    break;
                }
                bytes.extend_from_slice(&more);
            }
            Ok((stat, bytes))
        }
        .boxed()
    }

    /// 从 `offset` 起读最多 `len` 字节（一次不超过 `fr_core::files::READ_MAX`）。
    pub fn fs_read(
        &self,
        repo: &str,
        path: &str,
        offset: u64,
        len: u64,
    ) -> BoxFuture<'static, Result<(FsStat, Vec<u8>)>> {
        match self {
            Channel::Local => {
                let (repo, path) = (repo.to_string(), path.to_string());
                local::blocking(move || fr_core::files::read(&repo, &path, offset, len))
            }
            Channel::Remote(conn) => conn.fs_read(repo, path, offset, len),
        }
    }

    /// 写一个文件（整个换掉），见 [`FsWrite`]。磁盘上的和 `expect` 对不上时不写，结果里 `conflict` 为真。
    pub fn fs_write(
        &self,
        params: FsWrite,
        bytes: Vec<u8>,
    ) -> BoxFuture<'static, Result<FsWritten>> {
        match self {
            Channel::Local => local::blocking(move || fr_core::files::write(&params, &bytes)),
            Channel::Remote(conn) => conn.fs_write(params, bytes),
        }
    }

    /// 建目录（路径上缺的一起建）；已经有了就失败。
    pub fn fs_mkdir(&self, repo: &str, path: &str) -> BoxFuture<'static, Result<()>> {
        match self {
            Channel::Local => {
                let (repo, path) = (repo.to_string(), path.to_string());
                local::blocking(move || fr_core::files::make_dir(&repo, &path))
            }
            Channel::Remote(conn) => {
                conn.fs_simple("fs_mkdir", json!({ "repo": repo, "path": path }))
            }
        }
    }

    /// 删除文件或目录（连里面的一起）。本机的移到回收站，服务器上的永久删除。
    pub fn fs_remove(&self, repo: &str, path: &str) -> BoxFuture<'static, Result<()>> {
        match self {
            Channel::Local => {
                let (repo, path) = (repo.to_string(), path.to_string());
                local::blocking(move || local::trash(&repo, &path))
            }
            Channel::Remote(conn) => {
                conn.fs_simple("fs_remove", json!({ "repo": repo, "path": path }))
            }
        }
    }

    /// 改名；`to` 已经有了就失败。
    pub fn fs_rename(&self, repo: &str, from: &str, to: &str) -> BoxFuture<'static, Result<()>> {
        match self {
            Channel::Local => {
                let (repo, from, to) = (repo.to_string(), from.to_string(), to.to_string());
                local::blocking(move || fr_core::files::rename(&repo, &from, &to))
            }
            Channel::Remote(conn) => {
                conn.fs_simple("fs_rename", json!({ "repo": repo, "path": from, "to": to }))
            }
        }
    }

    /// 把仓库里的一个文件存到本机的 `dest`（已经有就换掉）。传完给出 `dest`。
    pub fn fs_download(&self, repo: &str, path: &str, dest: PathBuf) -> UploadStream {
        match self {
            Channel::Local => {
                let source = fr_core::files::resolve_inside(repo, path);
                UploadStream::spawn(move |tx| local::copy(&source?, &dest, true, tx))
            }
            Channel::Remote(conn) => conn.fs_download(repo, path, dest),
        }
    }

    /// 把本机的 `source` 放到仓库里的 `path`（相对仓库根，含文件名）。那里已经有文件时，
    /// `overwrite` 为真就换掉，为假就失败。传完给出它的完整路径。
    pub fn fs_upload(
        &self,
        repo: &str,
        path: &str,
        source: PathBuf,
        overwrite: bool,
    ) -> UploadStream {
        match self {
            Channel::Local => {
                let dest = fr_core::files::resolve_inside(repo, path);
                UploadStream::spawn(move |tx| local::copy(&source, &dest?, overwrite, tx))
            }
            Channel::Remote(conn) => conn.fs_upload(repo, path, source, overwrite),
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

    #[test]
    fn 跳板机的标记带着它的名字_没存密码就不自动回答() {
        let jump = SavedSecret::register_for(None, PromptKind::Password, Some("bastion"));
        let tag = jump.tag().to_string();
        assert_eq!(jump_of(Some(&tag)).as_deref(), Some("bastion"));
        assert_eq!(saved_answer(Some(&tag), "u@h's password: "), None);
        // 目标服务器的标记、没有标记：不是跳板机
        let target = SavedSecret::register("pw", PromptKind::Password);
        assert_eq!(jump_of(Some(target.tag())), None);
        assert_eq!(jump_of(None), None);
        drop(jump);
        assert_eq!(jump_of(Some(&tag)), None);
    }
}
