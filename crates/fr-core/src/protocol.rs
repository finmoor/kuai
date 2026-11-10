//! 客户端和 agent 之间的通信协议。
//!
//! 每条消息是一行 JSON；带大块数据时（git 的输出、写给 git 的标准输入），
//! 在 JSON 的 `bytes` 字段写明长度，紧跟在这一行后面发原始字节。
//! 控制消息保持能直接用眼睛看，大块数据也不用为 JSON 转义多付体积。
//!
//! 方法：
//! - `hello`：握手，返回 [`Hello`]。
//! - `ping`：测往返延迟。
//! - `git`：参数 `{repo, args, stream, progress}`，请求带的字节作为 git 的标准输入。
//!   不流式时一条响应：`result` 是 [`GitResult`]，后面跟全部标准输出；
//!   流式时先来若干条 `chunk: true` 的消息（每条带一块标准输出），最后一条带 `result`。
//!   `progress` 为真时标准错误也一块块发回来（`chunk: true, err: true`），拿来显示推拉的进度。
//! - `watch`：参数 `{repo}`，监听仓库的变化。每次有变化回一条 `chunk: true` 的空消息，
//!   直到被 `cancel`；装不上监听时回一条带 `error` 的响应。
//! - `cancel`：参数 `{id}`，杀掉那个请求的 git 进程，或者停掉那个监听。不回响应。
//! - `answer`：参数 `{id, answer}`，回答 agent 发来的密码请求，`answer` 为空表示取消。不回响应。
//!
//! agent 也会主动问客户端：服务器上的 git 要密码时，发一条 `ask` 是提示文字的消息，
//! `id` 是 agent 自己编的号（和请求的编号无关），客户端弹窗让用户填，再用 `answer` 回答。
//! - `list_dir`：参数 `{path}`，返回 [`DirListing`]。`path` 为空或 `~` 表示主目录。

use std::io::{self, BufRead, Write};

use anyhow::{Context as _, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 协议版本，握手时交换。agent 装在按版本区分的目录里，正常不会不一致。
pub const PROTOCOL_VERSION: u32 = 3;

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
    /// agent 问客户端要密码：提示文字。此时 `id` 是这次询问的编号
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
        if r.read_line(&mut line).context("读取消息失败")? == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            break;
        }
    }
    let header: T = serde_json::from_str(&line).with_context(|| {
        let shown: String = line.trim().chars().take(200).collect();
        format!("无法解析消息：{shown}")
    })?;
    let mut payload = vec![0; header.payload_len() as usize];
    r.read_exact(&mut payload)
        .context("读取消息附带的数据时连接中断")?;
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
