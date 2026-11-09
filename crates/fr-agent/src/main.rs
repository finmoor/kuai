//! fr-agent：在服务器上执行 git，经 stdin/stdout 和客户端通信。
//!
//! 客户端通过 `ssh <主机> ~/.fork-remote/<版本>/fr-agent --stdio` 启动它。
//! 不开端口、不常驻：stdin 关闭（SSH 断开）就退出。

use std::io::{BufRead, BufWriter};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail};
use fr_core::protocol::{self, PROTOCOL_VERSION, Request, Response};
use serde_json::{Value, json};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version") => println!("fr-agent {VERSION}"),
        Some("--stdio") => {
            if let Err(err) = serve() {
                eprintln!("fr-agent: {err:#}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("用法：fr-agent --stdio | --version");
            std::process::exit(2);
        }
    }
}

fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = BufWriter::with_capacity(1 << 20, std::io::stdout().lock());
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let (id, outcome) = match serde_json::from_str::<Request>(&line) {
            Ok(request) => (request.id, handle(&request)),
            Err(err) => (0, Err(anyhow!("无法解析请求：{err}"))),
        };
        let (response, payload) = match outcome {
            Ok((result, payload)) => (
                Response {
                    id,
                    result: Some(result),
                    error: None,
                    bytes: payload.len() as u64,
                },
                payload,
            ),
            Err(err) => (
                Response {
                    id,
                    result: None,
                    error: Some(format!("{err:#}")),
                    bytes: 0,
                },
                Vec::new(),
            ),
        };
        protocol::write_message(&mut out, &response, &payload)?;
    }
    Ok(())
}

fn handle(request: &Request) -> Result<(Value, Vec<u8>)> {
    match request.method.as_str() {
        // 只用来测往返延迟
        "ping" => Ok((json!({}), Vec::new())),
        "hello" => Ok((
            json!({
                "protocol": PROTOCOL_VERSION,
                "version": VERSION,
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "git": git_version(),
            }),
            Vec::new(),
        )),
        "log" => {
            let repo = param(request, "repo")?;
            let started = Instant::now();
            let out = fr_core::git::run(Path::new(repo), fr_core::log::LOG_ARGS)?;
            Ok((
                json!({ "git_ms": started.elapsed().as_millis() as u64 }),
                out,
            ))
        }
        "show" => {
            let repo = param(request, "repo")?;
            let rev = param(request, "rev")?;
            let mut args = fr_core::log::SHOW_ARGS.to_vec();
            args.push(rev);
            let started = Instant::now();
            let out = fr_core::git::run(Path::new(repo), &args)?;
            Ok((
                json!({ "git_ms": started.elapsed().as_millis() as u64 }),
                out,
            ))
        }
        other => bail!("未知方法：{other}"),
    }
}

fn param<'a>(request: &'a Request, key: &str) -> Result<&'a str> {
    request
        .params
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("缺少参数 {key}"))
}

/// git 版本只查一次：在 Windows 上启动一次 git 进程就要十几毫秒。
fn git_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        Command::new("git")
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "未找到 git".into())
    })
}
