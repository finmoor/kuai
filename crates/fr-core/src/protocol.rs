//! 客户端和服务器上的 kuaid 之间的通信协议。
//!
//! 每条消息是一行 JSON；带大块数据时（git 的输出、写给 git 的标准输入、终端的输出），
//! 在 JSON 的 `bytes` 字段写明长度，紧跟在这一行后面发原始字节。
//! 控制消息保持能直接用眼睛看，大块数据也不用为 JSON 转义多付体积。
//!
//! 协议只加不改：kuaid 常驻在服务器上，可能比连上来的客户端新，新的 kuaid 要认得旧客户端。
//!
//! 方法：
//! - `hello`：握手，参数 `{protocol}`（客户端的协议版本），返回 [`Hello`]。
//! - `ping`：测往返延迟。
//! - `git`：参数 `{repo, args, stream, progress}`，请求带的字节作为 git 的标准输入。
//!   不流式时一条响应：`result` 是 [`GitResult`]，后面跟全部标准输出；
//!   流式时先来若干条 `chunk: true` 的消息（每条带一块标准输出），最后一条带 `result`。
//!   `progress` 为真时标准错误也一块块发回来（`chunk: true, err: true`），拿来显示推拉的进度。
//! - `watch`：参数 `{repo}`，监听仓库的变化。每次有变化回一条 `chunk: true` 的空消息，
//!   直到被 `cancel`；装不上监听时回一条带 `error` 的响应。
//! - `cancel`：参数 `{id}`，杀掉那个请求的 git 进程、停掉那个监听，或者从那个终端上断开
//!   （终端里的程序接着跑）。不回响应。
//! - `answer`：参数 `{id, answer}`，回答 kuaid 发来的密码请求，`answer` 为空表示取消。不回响应。
//! - `list_dir`：参数 `{path}`，返回 [`DirListing`]。`path` 为空或 `~` 表示主目录。
//!
//! kuaid 也会主动问客户端：服务器上的 git 要密码时，发一条 `ask` 是提示文字的消息，
//! `id` 是 kuaid 自己编的号（和请求的编号无关），客户端弹窗让用户填，再用 `answer` 回答。
//!
//! 终端（协议 4 起）。终端会话开在 kuaid 里，客户端断开、退出以后里面的程序接着跑：
//! - `term_open`：参数 [`TermOpen`]。有同一个 `key` 的会话就接上它，没有就在 `cwd` 里开一个登录 shell。
//!   响应是一个流：第一条 `chunk: true`、`result` 是 [`TermOpened`]；接着是还原屏幕用的输出
//!   （接上已有会话时）和之后的输出，每条 `chunk: true` 带一块原始字节；程序结束时最后一条
//!   `result` 是 [`TermExit`]。`cancel` 这个请求只是断开。
//! - `term_attach`：参数 `{term, cols, rows}`，接上指定的会话，响应同上。带 `watch: true`（协议 11 起）
//!   是只看：不改会话的大小，先收到按会话现在的大小还原的屏幕；看的人不往里写任何东西（包括终端
//!   自己回答程序的话），程序结束时会话也不因为它看过就忘掉。
//! - `term_input`：参数 `{term}`，请求带的字节写给程序。不回响应。
//! - `term_resize`：参数 `{term, cols, rows}`。不回响应。
//! - `term_close`：参数 `{term}`，结束会话里的程序并忘掉这个会话。
//! - `term_list`：返回 `{terms: [TermInfo]}`。
//! - `term_watch`（协议 11 起）：订阅终端列表。先回一条 `chunk: true`、`result` 是 `{terms: [TermInfo]}`，
//!   之后列表有变化（开了、结束了、标题或最后一行变了、有输出变成没输出）时再回一条；有终端正在
//!   输出时每隔几秒也回一条，客户端靠 [`TermInfo::idle_ms`] 知道它还在输出。`cancel` 这个请求就不订阅了。
//!
//! 传文件（协议 5 起）。往终端里发图片、文件用：终端那条路只能过文字，文件先传上来，
//! 客户端再把它在服务器上的路径贴进终端：
//! - `file_put`：参数 [`FilePut`]，请求带的字节是文件的下一块。第一块给文件名，kuaid 回一个
//!   `upload` 编号，之后的块带着它接着写；每块回一条 [`FilePutReply`]（已经收到多少）。
//!   `done` 的那一块写完后文件才放到位（`~/.kuai/drop/` 下，重名的换个名字），响应里带它的完整路径。
//!   `abort` 是不传了。客户端断开时没传完的自动删掉；传好的文件过几天由 kuaid 清掉。
//!
//! 仓库里的文件（协议 6 起，文件页用）。路径都相对仓库根、用 `/` 分隔，不能带 `..`，见 [`crate::files`]：
//! - `fs_list`：参数 `{repo, path}`，返回 `{entries: [FsEntry]}`：这一层的文件和目录，不含 `.git`。
//! - `fs_stat`：参数 `{repo, path}`，返回 [`FsStat`]；文件不在了不算出错，`exists` 为假。
//! - `fs_read`：参数 `{repo, path, offset, len}`，返回 [`FsStat`]，后面跟从 `offset` 起最多 `len` 字节
//!   （一次不超过 [`crate::files::READ_MAX`]）。下载大文件就是一段段读。
//! - `fs_write`：参数 [`FsWrite`]，请求带的字节是文件的全部内容，返回 [`FsWritten`]。先写临时文件再改名。
//!   `expect` 是打开时的修改时间和大小：磁盘上的已经不是这样了就不写，`conflict` 为真。
//! - `fs_mkdir`、`fs_remove`（目录连里面的一起，永久删除）：参数 `{repo, path}`。
//! - `fs_rename`：参数 `{repo, path, to}`，`to` 已经有了就失败。
//! - `file_put` 带上 `repo`、`path` 就是往仓库里传文件：没传完的放在目标旁边，传完才改成正式的名字。
//!
//! 子仓库（协议 8 起）：
//! - `fs_repos`：参数 `{repo}`，返回 [`SubRepos`]：仓库目录里面还有哪些仓库，见 [`crate::files::find_repos`]。
//!
//! 把目录映射到本机（协议 9 起，见 `fr-mount`）。映射出来的盘要按偏移读写、要看得到 `.git`，
//! 文件页那几个方法不够用，加了这些；路径的规矩和上面一样：
//! - `fs_list` 可以带 `all: true`：`.git` 也列。
//! - `fs_pwrite`：参数 `{repo, path, offset, create}`，请求带的字节写到 `offset` 那里（文件不够长就接长），
//!   返回 [`FsStat`]。`create` 为真时文件不在就新建，否则文件不在是出错。
//! - `fs_resize`：参数 `{repo, path, size, new}`，把文件截短或接长到 `size`，返回 [`FsStat`]。
//!   `new` 为真是新建一个文件：已经有了就失败。
//! - `fs_touch`：参数 `{repo, path, mtime}`，改修改时间（1970 年起的毫秒数），返回 [`FsStat`]。
//! - `fs_space`：参数 `{repo}`，返回 [`FsSpace`]：所在的盘一共多大、还剩多少。
//! - `fs_rename` 可以带 `replace: true`：`to` 已经有文件就盖掉（目录不盖）。
//! - `fs_remove` 可以带 `shallow: true`：只删文件和空目录，目录里有东西就失败。
//!
//! 仓库列表和工作区（协议 10 起）。每台电脑的由它的 kuaid 保存，格式和改法见 [`crate::store`]：
//! - `state_get`：返回 [`crate::store::Store`]。
//! - `state_watch`：订阅。先回一条 `chunk: true`、`result` 是现在的整份，之后每次有改动（哪个客户端改的
//!   都算）再回一条整份，直到被 `cancel`。
//! - `state_apply`：参数 `{ops: [Op]}`，依次执行，返回改完的整份。有一个做不了就都不算，回 `error`。
//!
//! 经这台电脑连它配置的服务器（协议 10 起）。工作区里有这台电脑配置的服务器上的仓库时，连上来的
//! 客户端不用自己配置那台服务器，由 kuaid 起 ssh 连过去，标准输入输出经这条连接转：
//! - `relay_open`：参数 `{server, command}`，`server` 是 `Store::servers` 里的 id，`command` 在那台
//!   服务器上执行。登录方式、跳板机照这台电脑的配置；要密码时发 `ask` 问这个客户端。
//!   响应是一个流：ssh 的标准输出每块一条 `chunk: true`，标准错误每块一条 `chunk: true, err: true`，
//!   ssh 结束时最后一条 `result` 是 `{code}`（退出码，被杀掉时为空）。`cancel` 结束 ssh。
//! - `relay_input`：参数 `{relay, eof}`，`relay` 是 `relay_open` 的请求编号，请求带的字节写进 ssh 的
//!   标准输入；`eof` 为真时写完关掉标准输入。不回响应。
//!
//! 给 ssh 转接程序（`kuaid --connect`）和运维用的：
//! - `info`：返回 [`DaemonInfo`]。
//! - `bridge`：参数 `{auth_sock}`，转接程序报告这条 ssh 连接转发过来的 ssh-agent 套接字。不回响应。
//! - `upgrade`：参数 `{exe}`，常驻进程原地换成这个程序，终端会话带过去。成功时不回响应，连接直接断开。
//! - `shutdown`：结束所有终端会话并退出。

use crate::i18n::tr;
use crate::tf;
use std::io::{self, BufRead, Write};

use anyhow::{Context as _, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 协议版本，握手时交换。服务器上的 kuaid 不能比客户端旧（连接时会原地升级），可以比客户端新。
///
/// 7：kuaid 有了 Windows 版；`term_open` 可以带 `shell`。
/// 8：`fs_repos`，找仓库目录里面的仓库。
/// 9：把目录映射到本机用的 `fs_pwrite`、`fs_resize`、`fs_touch`、`fs_space`，和几个方法多出来的选项。
/// 10：kuaid 保存仓库列表和工作区（`state_get`、`state_watch`、`state_apply`），经它连它配置的服务器
/// （`relay_open`、`relay_input`）。
/// 11：`term_watch` 订阅终端列表；`term_attach` 可以只看（`watch`）；[`TermInfo`] 多了 `idle_ms`、`last`。
/// 12：[`Hello`] 多了 `machine`（这台电脑的标识，见 [`crate::store::machine_id`]）。
pub const PROTOCOL_VERSION: u32 = 12;

/// 一条消息后面跟多少字节的原始数据。
pub trait Frame {
    fn payload_len(&self) -> u64;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    /// 紧跟在这一行后面的原始字节数
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    /// 流式请求的一块输出，后面还有消息
    #[serde(default, skip_serializing_if = "is_false")]
    pub chunk: bool,
    /// 这一块是标准错误（进度），不是标准输出
    #[serde(default, skip_serializing_if = "is_false")]
    pub err: bool,
    /// kuaid 问客户端要密码：提示文字。此时 `id` 是这次询问的编号
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 紧跟在这一行后面的原始字节数
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes: u64,
}

impl Frame for Request {
    fn payload_len(&self) -> u64 {
        self.bytes
    }
}

impl Frame for Response {
    fn payload_len(&self) -> u64 {
        self.bytes
    }
}

/// `hello` 的结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub version: String,
    pub os: String,
    pub arch: String,
    /// `git --version` 的输出
    pub git: String,
    /// 主目录，浏览目录时从这里开始
    #[serde(default)]
    pub home: String,
    /// kuaid 装在哪个目录里（`<版本>-<程序内容的哈希>`）；不是装出来的（测试时直接启动）为空
    #[serde(default)]
    pub build: String,
    /// 是常驻进程：终端会话在客户端断开后还留着
    #[serde(default)]
    pub daemon: bool,
    /// 这台电脑的标识（[`crate::store::machine_id`]）：经不同的路连到的是不是同一台电脑，靠它认
    pub machine: String,
}

/// `info` 的结果：常驻进程现在的情况。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub protocol: u32,
    pub version: String,
    /// 同 [`Hello::build`]
    #[serde(default)]
    pub build: String,
    #[serde(default)]
    pub pid: u32,
    /// 现在有几个终端会话（含程序已经结束、还没人看过的）
    #[serde(default)]
    pub terms: usize,
    /// 现在连着几个客户端（含发这个请求的）
    #[serde(default)]
    pub clients: usize,
    /// 常驻进程在哪个登录会话里（只有 Windows 上报）：0 是没有桌面的那个，经 ssh 拉起的在这里
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<u32>,
    /// 常驻进程带着 Windows 的 Redirection Guard（只有 Windows 上报）：它起的终端进不了普通用户建的
    /// 目录联接和符号链接。带着它的 Kuai 拉起来的才会这样，换一个不带的 Kuai 来起就没有了
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub guarded: bool,
}

/// `term_open` 的参数。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TermOpen {
    /// 客户端起的名字（现在用仓库路径）：有同名的会话就接上，不另开
    #[serde(default)]
    pub key: String,
    /// 在哪个目录里开 shell
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    /// 开哪个 shell：程序和参数（协议 7 起）。空着由 kuaid 定：Linux 上是登录 shell，
    /// Windows 上是 sshd 配的默认 shell。只在新开会话时用，接上已有的会话时不管。
    /// 现在只有 Windows 上的 kuaid 认：本机的 Kuai 经它开终端时带上设置里选的 shell
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shell: Vec<String>,
}

/// 终端流的第一条消息。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TermOpened {
    /// 会话的编号，之后的 `term_input`、`term_resize`、`term_close` 用它
    pub term: String,
    /// 这次新开的；为假说明接上的是已有的会话
    #[serde(default)]
    pub created: bool,
}

/// 终端流的最后一条消息：程序结束了。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TermExit {
    /// 退出码；被信号杀掉或者查不到时为空
    pub exit: Option<i32>,
}

/// `term_list` 里的一项。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TermInfo {
    pub term: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub cwd: String,
    /// 程序自己设的标题
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    /// 里面的程序还在跑
    #[serde(default)]
    pub alive: bool,
    /// 现在接着几个客户端
    #[serde(default)]
    pub attached: usize,
    /// 开了多久（秒）
    #[serde(default)]
    pub age: u64,
    /// 距最近一次输出多久了（毫秒，协议 11 起）。空着是不知道：旧版 kuaid，或者 kuaid 升级换程序后
    /// 还没有过输出。用毫秒：客户端拿它倒推「最近一次输出的时刻」，和「什么时候看过」比先后，
    /// 整秒的话差一两秒就比错了
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_ms: Option<u64>,
    /// 屏幕上最后一行有字的内容（协议 11 起），列表里给人看一眼用
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last: String,
}

/// `file_put` 的参数。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilePut {
    /// 第一块：文件名（只看最后一段，不能指定目录）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// 之后的块：第一块的响应里给的编号
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub upload: String,
    /// 这是最后一块
    #[serde(default, skip_serializing_if = "is_false")]
    pub done: bool,
    /// 不传了，删掉已经收到的
    #[serde(default, skip_serializing_if = "is_false")]
    pub abort: bool,
    /// 第一块：传到这个仓库里（协议 6 起）。为空就是放到 `~/.kuai/drop/` 下
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo: String,
    /// 第一块：传到仓库里的哪个路径（相对仓库根，含文件名），此时不看 `name`
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// 第一块：`path` 已经有文件时换掉它；为假就失败
    #[serde(default, skip_serializing_if = "is_false")]
    pub overwrite: bool,
}

/// `file_put` 每一块的响应。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilePutReply {
    pub upload: String,
    /// 已经收到多少字节
    #[serde(default)]
    pub received: u64,
    /// 传完了：文件在服务器上的完整路径
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// `fs_list` 里的一项。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FsEntry {
    pub name: String,
    /// 是目录（符号链接看它指向的）
    #[serde(default, skip_serializing_if = "is_false")]
    pub dir: bool,
    /// 是符号链接
    #[serde(default, skip_serializing_if = "is_false")]
    pub link: bool,
    /// 文件大小（字节）；目录为 0
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: u64,
    /// 修改时间：1970 年起的毫秒数
    #[serde(default, skip_serializing_if = "is_zero")]
    pub mtime: u64,
}

/// 一个文件现在的情况。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsStat {
    /// 为假：这个路径上什么都没有，其余几项没有意义
    #[serde(default)]
    pub exists: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dir: bool,
    #[serde(default)]
    pub size: u64,
    /// 修改时间：1970 年起的毫秒数
    #[serde(default)]
    pub mtime: u64,
    /// 只读（没有写权限）
    #[serde(default, skip_serializing_if = "is_false")]
    pub readonly: bool,
}

/// `fs_space` 的结果：目录所在的盘的大小（字节）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsSpace {
    pub total: u64,
    /// 当前用户还能用的
    pub free: u64,
}

/// `fs_write` 的参数。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FsWrite {
    pub repo: String,
    pub path: String,
    /// 新建：已经有这个文件就失败；路径上缺的目录一起建
    #[serde(default, skip_serializing_if = "is_false")]
    pub create: bool,
    /// 打开时文件的样子：磁盘上的已经不是这样了就不写。为空是不管，直接换掉
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<FsExpect>,
}

/// 打开文件时记下的修改时间和大小，保存时拿来看别人有没有动过。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsExpect {
    pub mtime: u64,
    pub size: u64,
}

impl From<FsStat> for FsExpect {
    fn from(stat: FsStat) -> FsExpect {
        FsExpect {
            mtime: stat.mtime,
            size: stat.size,
        }
    }
}

/// `fs_write` 的结果。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct FsWritten {
    /// 没写：磁盘上的文件和 `expect` 对不上（别的程序改过，或者删了）
    #[serde(default, skip_serializing_if = "is_false")]
    pub conflict: bool,
    /// 文件现在的情况（写成了是写完以后的，冲突了是磁盘上的）
    pub stat: FsStat,
}

/// `git` 方法的结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitResult {
    /// 退出码；进程被杀掉时为空
    pub code: Option<i32>,
    #[serde(default)]
    pub stderr: String,
    /// git 运行了多久（毫秒）
    #[serde(default)]
    pub ms: u64,
}

/// `list_dir` 的结果：只列目录，不列文件，隐藏目录（以 `.` 开头）也不列。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DirListing {
    /// 规范化后的绝对路径
    pub path: String,
    pub parent: Option<String>,
    /// 这个目录本身是不是仓库
    #[serde(default)]
    pub repo: bool,
    pub entries: Vec<DirEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    /// 下面有 `.git`
    #[serde(default)]
    pub repo: bool,
}

/// `fs_repos` 的结果：仓库目录里面的仓库。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubRepos {
    /// 相对仓库根的路径，用 `/` 分隔，按名字排
    pub repos: Vec<String>,
    /// 目录太多、没找全
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// 写一条消息：一行 JSON，后面跟原始字节。
pub fn write_message<W: Write, T: Serialize>(
    w: &mut W,
    header: &T,
    payload: &[u8],
) -> io::Result<()> {
    serde_json::to_writer(&mut *w, header)?;
    w.write_all(b"\n")?;
    w.write_all(payload)?;
    w.flush()
}

/// 读一条消息和它附带的原始字节；对方正常关闭时返回 None。
pub fn read_message<R: BufRead, T: DeserializeOwned + Frame>(
    r: &mut R,
) -> Result<Option<(T, Vec<u8>)>> {
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).context(tr("读取消息失败"))? == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            break;
        }
    }
    let header: T = serde_json::from_str(&line).with_context(|| {
        let shown: String = line.trim().chars().take(200).collect();
        tf!("无法解析消息：{shown}", shown)
    })?;
    let mut payload = vec![0; header.payload_len() as usize];
    r.read_exact(&mut payload)
        .context(tr("读取消息附带的数据时连接中断"))?;
    Ok(Some((header, payload)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 消息和原始字节可以往返() {
        let mut buf = Vec::new();
        let request = Request {
            id: 7,
            method: "git".into(),
            params: serde_json::json!({ "repo": "/r", "args": ["log"] }),
            bytes: 3,
        };
        write_message(&mut buf, &request, b"a\nb").unwrap();
        let response = Response {
            id: 7,
            chunk: true,
            bytes: 2,
            ..Default::default()
        };
        write_message(&mut buf, &response, b"\0\n").unwrap();

        let mut reader = io::BufReader::new(&buf[..]);
        let (got, payload) = read_message::<_, Request>(&mut reader).unwrap().unwrap();
        assert_eq!(got.id, 7);
        assert_eq!(got.params["args"][0], "log");
        assert_eq!(payload, b"a\nb");
        let (got, payload) = read_message::<_, Response>(&mut reader).unwrap().unwrap();
        assert!(got.chunk);
        assert_eq!(payload, b"\0\n");
        assert!(read_message::<_, Response>(&mut reader).unwrap().is_none());
    }

    #[test]
    fn 默认值不写进消息() {
        let line = serde_json::to_string(&Response {
            id: 1,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(line, r#"{"id":1}"#);
    }
}
