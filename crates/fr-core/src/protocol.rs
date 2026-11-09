//! 客户端和 agent 之间的通信协议。
//!
//! 每条消息是一行 JSON；需要带大块数据时（例如 `git log` 的原始输出），
//! 在 JSON 的 `bytes` 字段写明长度，紧跟在这一行后面发原始字节。
//! 控制消息保持能直接用眼睛看，大块数据也不用为 JSON 转义多付体积。

use std::io::{self, BufRead, Write};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 协议版本，握手时交换；不兼容时客户端重新上传 agent。
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 紧跟在这一行后面的原始字节数
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
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

/// 同步客户端：一次发一个请求、等一个响应。M0 测试用，正式客户端会改成可并发、可取消。
pub struct Client<R, W> {
    reader: R,
    writer: W,
    next_id: u64,
}

impl<R: BufRead, W: Write> Client<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            next_id: 1,
        }
    }

    /// 调用一个方法，返回结果和附带的原始字节。
    pub fn call(&mut self, method: &str, params: Value) -> Result<(Value, Vec<u8>)> {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request {
            id,
            method: method.into(),
            params,
        };
        write_message(&mut self.writer, &request, &[]).context("向 agent 发送请求失败")?;

        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            bail!("agent 已断开");
        }
        let response: Response = serde_json::from_str(&line)
            .with_context(|| format!("无法解析 agent 的响应：{}", line.trim()))?;
        let mut payload = vec![0; response.bytes as usize];
        self.reader
            .read_exact(&mut payload)
            .context("读取 agent 返回的数据时连接中断")?;
        if response.id != id {
            bail!("响应编号不对：期望 {id}，收到 {}", response.id);
        }
        if let Some(err) = response.error {
            bail!("{err}");
        }
        Ok((response.result.unwrap_or(Value::Null), payload))
    }
}
