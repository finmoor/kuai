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
//! 服务器可以经跳板机连接（[`ConnectOptions::jumps`]），见 `fr_core::hop`。
//!
//! 仓库目录里的文件（文件页）：[`Channel::fs_list`] 这一组。本机仓库直接读写磁盘，服务器上的交给
//! kuaid，做事的是同一份代码（`fr_core::files`），路径都相对仓库根。

mod binaries;
mod local;
mod remote;

use fr_core::i18n::tr;
use fr_core::tf;
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
pub use fr_core::hop::run_helper as run_ssh_hop;
pub use fr_core::protocol::{
    DirEntry, DirListing, FsEntry, FsExpect, FsSpace, FsStat, FsWrite, FsWritten, Hello, SubRepos,
    TermInfo, TermMark,
};
pub use remote::{ConnectOptions, Connection, Jump, Probe, StageHandler, local_kuai_root};

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

/// 取消 [`Channel::git_cancellable`] 起的 git：杀掉它，请求的 future 照样等到它真的退出才完成
/// （丢弃 future 也能取消，但不知道那边什么时候停下）。
#[derive(Clone)]
pub struct GitCancel(Arc<dyn Fn() + Send + Sync>);

impl GitCancel {
    pub fn cancel(&self) {
        (self.0)()
    }
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
/// 监听装不上、仓库目录被删掉或换成了另一个、连接断了，流就结束。
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
    /// 流的第一项：会话的编号；`created` 为假说明接上的是已有的会话；`restore` 是后面还原屏幕的
    /// 输出分几段、各多少字节（见 [`fr_core::protocol::TermOpened::restore`]）
    Opened {
        term: String,
        created: bool,
        restore: Vec<u64>,
    },
    /// 程序的一块输出（接上已有会话时，开头几块是还原屏幕用的）
    Output(Vec<u8>),
    /// 这个会话记着的全部发出去的消息，从早到晚（协议 16 起：接上时一次，之后每多一条一次）
    Marks(Vec<TermMark>),
    /// 程序结束了：退出码
    Exited(Option<i32>),
    /// 取得太慢，积着没取走的输出超过了 [`TERM_BACKLOG`]：流到此为止，已经不从 kuaid 收了，积着的
    /// 也不要了。里面的程序接着跑，要接着看就按编号重新接（kuaid 先给一份现在的屏幕）
    Lagging,
}

/// 一个终端流里积着、还没取走的输出最多这么多字节，再多就不收了（见 [`TermItem::Lagging`]）。比接上
/// 时还原屏幕的那一段（往回翻的 2000 行连同样式）大得多：不会刚接上就因为它又落后
pub const TERM_BACKLOG: usize = 64 << 20;

/// 终端流里积着、还没取走的输出有多少（见 [`TERM_BACKLOG`]）：收的线程记上，取走时减掉。
#[derive(Default)]
pub(crate) struct Backlog {
    bytes: AtomicUsize,
    lagging: AtomicBool,
}

impl Backlog {
    /// 收到了 `len` 字节输出：加上它没超过上限就记上，返回 true；超过了就记下落后了，返回 false，
    /// 之后也都是 false。
    pub(crate) fn add(&self, len: usize) -> bool {
        if self.lagging() {
            return false;
        }
        if self.bytes.fetch_add(len, Ordering::AcqRel) + len > TERM_BACKLOG {
            self.lagging.store(true, Ordering::Release);
            return false;
        }
        true
    }

    fn take(&self, len: usize) {
        self.bytes.fetch_sub(len, Ordering::AcqRel);
    }

    fn lagging(&self) -> bool {
        self.lagging.load(Ordering::Acquire)
    }
}

/// 一个终端的输出。丢弃时从终端上断开，里面的程序接着跑。连接断了，流以一个错误结束；取得太慢，
/// 以 [`TermItem::Lagging`] 结束。
pub struct TermStream {
    rx: mpsc::UnboundedReceiver<Result<TermItem>>,
    backlog: Arc<Backlog>,
    /// [`TermItem::Lagging`] 给出去了，之后什么都没有
    ended: bool,
    _cancel: Box<dyn Send>,
}

impl Stream for TermStream {
    type Item = Result<TermItem>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.ended {
            return Poll::Ready(None);
        }
        // 落后了：积着的不用再取（重新接时 kuaid 给的是现在的屏幕）
        if self.backlog.lagging() {
            self.ended = true;
            return Poll::Ready(Some(Ok(TermItem::Lagging)));
        }
        let item = std::task::ready!(Pin::new(&mut self.rx).poll_next(cx));
        match &item {
            Some(Ok(TermItem::Output(bytes))) => self.backlog.take(bytes.len()),
            Some(Ok(TermItem::Lagging)) => self.ended = true,
            _ => {}
        }
        Poll::Ready(item)
    }
}

/// 一台电脑保存的仓库列表和工作区：先来现在的整份，之后每次有改动来一份新的
/// （见 [`Connection::state_watch`]）。连接断了，流以一个错误结束。丢弃就不订阅了。
pub struct StateStream {
    rx: mpsc::UnboundedReceiver<Result<fr_core::store::Store>>,
    _cancel: Box<dyn Send>,
}

impl Stream for StateStream {
    type Item = Result<fr_core::store::Store>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.rx).poll_next(cx)
    }
}

/// 一台电脑上的终端列表：先来现在的整份，之后有变化（开了、结束了、有输出变成没输出……）来一份
/// 新的，有终端正在输出时每隔几秒也来一份（见 [`Connection::term_watch`]）。连接断了，流以一个
/// 错误结束。丢弃就不订阅了。
pub struct TermListStream {
    rx: mpsc::UnboundedReceiver<Result<Vec<TermInfo>>>,
    _cancel: Box<dyn Send>,
}

impl Stream for TermListStream {
    type Item = Result<Vec<TermInfo>>;

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
        // 这次连接说了不许弹窗（启动时自动连接）：当作取消
        if silent(tag) {
            return None;
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
    /// 存着的答不了（没存、答过一次还问）时不弹窗，当作取消
    silent: bool,
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
        Self::register_for(Some(secret), kind, None, false)
    }

    /// `secret` 是存着的密码或口令（`kind` 说是哪一种）；`jump` 是跳板机的名字（连的是目标服务器时为 None）；
    /// `silent`：存着的答不了时不弹窗，当作取消。
    pub(crate) fn register_for(
        secret: Option<&str>,
        kind: PromptKind,
        jump: Option<&str>,
        silent: bool,
    ) -> SavedSecret {
        let tag = fr_core::askpass::random_token();
        let saved = Saved {
            secret: secret.map(str::to_string),
            kind,
            used: false,
            jump: jump.map(str::to_string),
            silent,
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
///
/// 私钥口令只答 ssh 自己解私钥时的那一句（`Enter passphrase for key '…'`）。带着存着的口令的 ssh
/// 只用公钥认证（见 `remote::login`），服务器没法在键盘交互认证里问一句像这样的话骗走它；
/// OpenSSH 8.3 起服务器问的话前面还都带「(用户@主机)」，开头也对不上。
fn saved_answer(tag: Option<&str>, prompt: &str) -> Option<String> {
    let mut saved = SAVED.lock().unwrap();
    let entry = saved.get_mut(tag?)?;
    let kind = fr_core::askpass::prompt_kind(prompt);
    if entry.used || entry.kind != kind {
        return None;
    }
    if kind == fr_core::askpass::PromptKind::Passphrase
        && !prompt.trim_start().starts_with("Enter passphrase for key")
    {
        return None;
    }
    let secret = entry.secret.clone()?;
    entry.used = true;
    Some(secret)
}

/// 带着这个标记的 ssh 要密码时是不是不许弹窗。
fn silent(tag: Option<&str>) -> bool {
    let saved = SAVED.lock().unwrap();
    tag.and_then(|tag| saved.get(tag))
        .is_some_and(|entry| entry.silent)
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

    /// 同 [`Self::git`]，另外给一个 [`GitCancel`]：取消以后 future 等 git 真的退出了才完成，
    /// 接着做的事不会和它同时改仓库。
    pub fn git_cancellable(
        &self,
        repo: &str,
        args: Vec<String>,
    ) -> (BoxFuture<'static, Result<GitOutput>>, GitCancel) {
        match self {
            Channel::Local => local::git_cancellable(repo, args),
            Channel::Remote(conn) => conn.git_cancellable(repo, args),
        }
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

    /// 跑 git 的那台电脑上 git 的版本号（主机上的握手时 kuaid 报过）；认不出是 None。
    pub fn git_version(&self) -> Option<(u32, u32, u32)> {
        match self {
            Channel::Local => fr_core::git::local_version(),
            Channel::Remote(conn) => fr_core::git::parse_version(&conn.hello.git),
        }
    }

    /// 仓库目录里面还有哪些仓库（相对仓库根的路径），见 [`fr_core::files::find_repos`]。
    pub fn fs_repos(&self, repo: &str) -> BoxFuture<'static, Result<SubRepos>> {
        match self {
            Channel::Local => {
                let repo = repo.to_string();
                local::blocking(move || fr_core::files::find_repos(&repo))
            }
            Channel::Remote(conn) => conn.fs_repos(repo),
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

    /// 读出整个文件。比 `limit` 大的不读：字节是空的，调用方看 [`FsStat::size`] 决定怎么办。
    pub fn fs_open(
        &self,
        repo: &str,
        path: &str,
        limit: u64,
    ) -> BoxFuture<'static, Result<(FsStat, Vec<u8>)>> {
        let (channel, repo, path) = (self.clone(), repo.to_string(), path.to_string());
        async move {
            let first = limit.min(fr_core::files::READ_MAX);
            let (stat, mut bytes) = channel.fs_read(&repo, &path, 0, first, Some(limit)).await?;
            if stat.size > limit {
                return Ok((stat, bytes));
            }
            // 读的这段时间里文件可能变长：读到没有了为止，但不超过上限
            while (bytes.len() as u64) < stat.size && (bytes.len() as u64) < limit {
                let rest = limit - bytes.len() as u64;
                let (_, more) = channel
                    .fs_read(&repo, &path, bytes.len() as u64, rest, None)
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

    /// 从 `offset` 起读最多 `len` 字节（一次不超过 `fr_core::files::READ_MAX`）。给了 `whole`
    /// 而文件比它大时不读，字节是空的（见 [`fr_core::files::read`]）。
    pub fn fs_read(
        &self,
        repo: &str,
        path: &str,
        offset: u64,
        len: u64,
        whole: Option<u64>,
    ) -> BoxFuture<'static, Result<(FsStat, Vec<u8>)>> {
        match self {
            Channel::Local => {
                let (repo, path) = (repo.to_string(), path.to_string());
                local::blocking(move || fr_core::files::read(&repo, &path, offset, len, whole))
            }
            Channel::Remote(conn) => conn.fs_read(repo, path, offset, len, whole),
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

    /// 同 [`Channel::fs_list`]，`.git` 也列。把目录映射到本机时用（下面几个也是），见 `fr-mount`。
    pub fn fs_list_all(&self, repo: &str, dir: &str) -> BoxFuture<'static, Result<Vec<FsEntry>>> {
        match self {
            Channel::Local => {
                let (repo, dir) = (repo.to_string(), dir.to_string());
                local::blocking(move || fr_core::files::list_with(&repo, &dir, true))
            }
            Channel::Remote(conn) => conn.fs_list_all(repo, dir),
        }
    }

    /// 把 `bytes` 写到文件的 `offset` 那里，别的地方不动；`create` 为真时文件不在就新建。
    pub fn fs_pwrite(
        &self,
        repo: &str,
        path: &str,
        offset: u64,
        bytes: Vec<u8>,
        create: bool,
    ) -> BoxFuture<'static, Result<FsStat>> {
        match self {
            Channel::Local => {
                let (repo, path) = (repo.to_string(), path.to_string());
                local::blocking(move || {
                    fr_core::files::write_at(&repo, &path, offset, &bytes, create)
                })
            }
            Channel::Remote(conn) => {
                let params =
                    json!({ "repo": repo, "path": path, "offset": offset, "create": create });
                conn.fs_call("fs_pwrite", params, bytes)
            }
        }
    }

    /// 把文件截短或接长到 `size`。`new` 为真是新建一个文件：已经有了就失败。
    pub fn fs_resize(
        &self,
        repo: &str,
        path: &str,
        size: u64,
        new: bool,
    ) -> BoxFuture<'static, Result<FsStat>> {
        match self {
            Channel::Local => {
                let (repo, path) = (repo.to_string(), path.to_string());
                local::blocking(move || fr_core::files::resize(&repo, &path, size, new))
            }
            Channel::Remote(conn) => {
                let params = json!({ "repo": repo, "path": path, "size": size, "new": new });
                conn.fs_call("fs_resize", params, Vec::new())
            }
        }
    }

    /// 把文件刷到磁盘上（见 [`fr_core::files::sync`]）。
    pub fn fs_sync(&self, repo: &str, path: &str) -> BoxFuture<'static, Result<FsStat>> {
        match self {
            Channel::Local => {
                let (repo, path) = (repo.to_string(), path.to_string());
                local::blocking(move || fr_core::files::sync(&repo, &path))
            }
            Channel::Remote(conn) => {
                let params = json!({ "repo": repo, "path": path });
                conn.fs_call("fs_sync", params, Vec::new())
            }
        }
    }

    /// 改文件的修改时间（1970 年起的毫秒数）。
    pub fn fs_touch(
        &self,
        repo: &str,
        path: &str,
        mtime: u64,
    ) -> BoxFuture<'static, Result<FsStat>> {
        match self {
            Channel::Local => {
                let (repo, path) = (repo.to_string(), path.to_string());
                local::blocking(move || fr_core::files::set_mtime(&repo, &path, mtime))
            }
            Channel::Remote(conn) => {
                let params = json!({ "repo": repo, "path": path, "mtime": mtime });
                conn.fs_call("fs_touch", params, Vec::new())
            }
        }
    }

    /// 目录所在的盘一共多大、还剩多少。
    pub fn fs_space(&self, repo: &str) -> BoxFuture<'static, Result<FsSpace>> {
        match self {
            Channel::Local => {
                let repo = repo.to_string();
                local::blocking(move || fr_core::files::space(&repo))
            }
            Channel::Remote(conn) => conn.fs_call("fs_space", json!({ "repo": repo }), Vec::new()),
        }
    }

    /// 改名，`to` 已经有文件就盖掉（目录不盖）。
    pub fn fs_rename_over(
        &self,
        repo: &str,
        from: &str,
        to: &str,
    ) -> BoxFuture<'static, Result<()>> {
        match self {
            Channel::Local => {
                let (repo, from, to) = (repo.to_string(), from.to_string(), to.to_string());
                local::blocking(move || fr_core::files::rename_over(&repo, &from, &to))
            }
            Channel::Remote(conn) => {
                let params = json!({ "repo": repo, "path": from, "to": to, "replace": true });
                conn.fs_simple("fs_rename", params)
            }
        }
    }

    /// 只删文件和空目录，永久删除（本机的也不进回收站：这是映射出来的盘上的删除，系统自己管回收站）。
    pub fn fs_remove_shallow(&self, repo: &str, path: &str) -> BoxFuture<'static, Result<()>> {
        match self {
            Channel::Local => {
                let (repo, path) = (repo.to_string(), path.to_string());
                local::blocking(move || fr_core::files::remove_shallow(&repo, &path))
            }
            Channel::Remote(conn) => {
                let params = json!({ "repo": repo, "path": path, "shallow": true });
                conn.fs_simple("fs_remove", params)
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
    use futures::StreamExt as _;

    #[test]
    fn 终端流积着的输出超过上限就不收了_积着的也不再取() {
        // 取走的不算积着的：一直有人取，来多少都收
        let backlog = Backlog::default();
        for _ in 0..3 {
            assert!(backlog.add(TERM_BACKLOG / 2));
            backlog.take(TERM_BACKLOG / 2);
        }
        // 没人取：积过上限就不收了，取走了也不再收
        assert!(backlog.add(TERM_BACKLOG / 2));
        assert!(backlog.add(TERM_BACKLOG / 2));
        assert!(!backlog.add(1));
        backlog.take(TERM_BACKLOG);
        assert!(!backlog.add(1));

        // 流：积着的一块块给，给了就不算积着的；落后以后积着的不再给，说一声落后了，然后结束
        let backlog = Arc::new(Backlog::default());
        let (tx, rx) = mpsc::unbounded();
        let mut stream = TermStream {
            rx,
            backlog: backlog.clone(),
            ended: false,
            _cancel: Box::new(()),
        };
        // 收的线程做的事（见 remote.rs 的 dispatch）：没过上限就放进去，过了放一个 Lagging 叫醒取的那一头
        let deliver = |len: usize| {
            let added = backlog.add(len);
            let item = match added {
                true => TermItem::Output(vec![b'x'; len]),
                false => TermItem::Lagging,
            };
            tx.unbounded_send(Ok(item)).unwrap();
            added
        };
        let next = |stream: &mut TermStream| futures::executor::block_on(stream.next());
        assert!(deliver(10) && deliver(20));
        assert!(
            matches!(next(&mut stream), Some(Ok(TermItem::Output(bytes))) if bytes.len() == 10)
        );
        assert_eq!(backlog.bytes.load(Ordering::Acquire), 20);
        assert!(!deliver(TERM_BACKLOG));
        assert!(matches!(next(&mut stream), Some(Ok(TermItem::Lagging))));
        assert!(next(&mut stream).is_none());
    }

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
        // 服务器在键盘交互认证里问的（前面带着「(用户@主机)」）：不是 ssh 自己解私钥，不答
        assert_eq!(
            saved_answer(Some(&tag), "(u@h) Enter passphrase for key 'k': "),
            None
        );
        assert_eq!(
            saved_answer(Some(&tag), "(u@h) Verification passphrase: "),
            None
        );
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
        let jump = SavedSecret::register_for(None, PromptKind::Password, Some("bastion"), false);
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

    #[test]
    fn 不许弹窗的连接_存着的照样答_答不了的不问() {
        // 平常的连接、没有标记：可以弹窗
        let plain = SavedSecret::register("pw", PromptKind::Password);
        assert!(!silent(Some(plain.tag())));
        assert!(!silent(None));
        // 启动时自动连接的：存着密码的先用存着的答，答过了还问（密码不对）就不再弹窗
        let auto = SavedSecret::register_for(Some("pw"), PromptKind::Password, None, true);
        let tag = auto.tag().to_string();
        assert!(silent(Some(&tag)));
        assert_eq!(
            saved_answer(Some(&tag), "u@h's password: ").as_deref(),
            Some("pw")
        );
        assert_eq!(saved_answer(Some(&tag), "u@h's password: "), None);
        // 没存的也登记着：不然不知道它不许弹窗
        let bare = SavedSecret::register_for(None, PromptKind::Passphrase, None, true);
        assert!(silent(Some(bare.tag())));
        // 连接完了撤掉
        drop(auto);
        assert!(!silent(Some(&tag)));
    }
}
