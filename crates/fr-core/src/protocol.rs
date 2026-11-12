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
//! - `term_attach`：参数 `{term, cols, rows}`，接上指定的会话，响应同上。
//! - `term_input`：参数 `{term}`，请求带的字节写给程序。不回响应。
//! - `term_resize`：参数 `{term, cols, rows}`。不回响应。
//! - `term_close`：参数 `{term}`，结束会话里的程序并忘掉这个会话。
//! - `term_list`：返回 `{terms: [TermInfo]}`。
//!
//! 传文件（协议 5 起）。往终端里发图片、文件用：终端那条路只能过文字，文件先传上来，
//! 客户端再把它在服务器上的路径贴进终端：
//! - `file_put`：参数 [`FilePut`]，请求带的字节是文件的下一块。第一块给文件名，kuaid 回一个
//!   `upload` 编号，之后的块带着它接着写；每块回一条 [`FilePutReply`]（已经收到多少）。
//!   `done` 的那一块写完后文件才放到位（`~/.kuai/drop/` 下，重名的换个名字），响应里带它的完整路径。
//!   `abort` 是不传了。客户端断开时没传完的自动删掉；传好的文件过几天由 kuaid 清掉。
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
pub const PROTOCOL_VERSION: u32 = 5;

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
