//! fr-agent：在服务器上执行 git，经 stdin/stdout 和客户端通信。
//!
//! 客户端经 `ssh <主机> ~/.kuai/<版本>/fr-agent --stdio` 启动它。
//! 不开端口、不常驻：stdin 关闭（SSH 断开）就退出。协议见 `fr_core::protocol`。
//!
//! agent 尽量少做事：`git` 方法执行客户端给的任意 git 参数，原样回传输出，
//! 解析和布局都在客户端做。这样客户端加功能时基本不用升级 agent。

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Stdout};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use fr_core::exec::{self, Killer, OnStderr};
use fr_core::protocol::{self, GitResult, Hello, PROTOCOL_VERSION, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 所有线程共用的输出：每条消息（一行 JSON 加原始字节）在锁里整条写完
type Output = Arc<Mutex<BufWriter<Stdout>>>;
/// 还在跑的 git 进程，按请求编号记，取消时用
type Running = Arc<Mutex<HashMap<u64, Killer>>>;
/// 发给客户端、还没得到回答的密码请求，按询问的编号记
type Asks = Arc<Mutex<HashMap<u64, mpsc::Sender<Option<String>>>>>;

/// 密码请求最多等用户这么久
const ASK_TIMEOUT: Duration = Duration::from_secs(600);

fn main() {
    // 被 git 或 ssh 当作 askpass 启动时：去问主进程，打出回答就退出
    if let Some(code) = fr_core::askpass::run_helper() {
        std::process::exit(code);
    }
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
    let output: Output = Arc::new(Mutex::new(BufWriter::with_capacity(
        1 << 20,
        std::io::stdout(),
    )));
    let running: Running = Arc::default();
    let asks: Asks = Arc::default();
    let mut watchers: HashMap<u64, fr_core::watch::Watcher> = HashMap::new();
    start_askpass(&output, &asks);
    let mut input = BufReader::with_capacity(1 << 16, std::io::stdin().lock());
    while let Some((request, payload)) = protocol::read_message::<_, Request>(&mut input)? {
        match request.method.as_str() {
            "git" => start_git(request, payload, &output, &running),
            "cancel" => {
                if let Some(target) = request.params.get("id").and_then(Value::as_u64) {
                    if let Some(killer) = running.lock().unwrap().get(&target) {
                        killer.kill();
                    }
                    watchers.remove(&target);
                }
            }
            "watch" => {
                let id = request.id;
                let repo = request
                    .params
                    .get("repo")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let out = output.clone();
                let started = fr_core::watch::watch(Path::new(repo), move || {
                    let changed = Response {
                        id,
                        chunk: true,
                        ..Default::default()
                    };
                    send(&out, &changed, &[]);
                });
                match started {
                    Ok(watcher) => {
                        watchers.insert(id, watcher);
                    }
                    Err(err) => send(
                        &output,
                        &Response {
                            id,
                            error: Some(format!("{err:#}")),
                            ..Default::default()
                        },
                        &[],
                    ),
                }
            }
            "answer" => {
                let waiter = request
                    .params
                    .get("id")
                    .and_then(Value::as_u64)
                    .and_then(|id| asks.lock().unwrap().remove(&id));
                if let Some(waiter) = waiter {
                    let answer = request
                        .params
                        .get("answer")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let _ = waiter.send(answer);
                }
            }
            _ => {
                let response = match handle(&request) {
                    Ok(result) => Response {
                        id: request.id,
                        result: Some(result),
                        ..Default::default()
                    },
                    Err(err) => Response {
                        id: request.id,
                        error: Some(format!("{err:#}")),
                        ..Default::default()
                    },
                };
                send(&output, &response, &[]);
            }
        }
    }
    // stdin 关了：客户端断开。杀掉还在跑的 git 再退出
    for killer in running.lock().unwrap().values() {
        killer.kill();
    }
    Ok(())
}

fn handle(request: &Request) -> Result<Value> {
    match request.method.as_str() {
        // 只用来测往返延迟
        "ping" => Ok(json!({})),
        "hello" => Ok(serde_json::to_value(Hello {
            protocol: PROTOCOL_VERSION,
            version: VERSION.into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            git: git_version().into(),
            home: fr_core::dir::home(),
        })?),
        "list_dir" => {
            let path = request
                .params
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("");
            Ok(serde_json::to_value(fr_core::dir::list_dir(path)?)?)
        }
        other => bail!("未知方法：{other}"),
    }
}

#[derive(Deserialize)]
struct GitParams {
    repo: String,
    args: Vec<String>,
    #[serde(default)]
    stream: bool,
    /// 标准错误也一块块发回去（要同时开 `stream`）
    #[serde(default)]
    progress: bool,
}

/// 服务器上的 git、ssh 要密码时转给客户端：把自己设成 askpass 程序，
/// 助手进程连回来后，这里发一条 `ask` 消息，等客户端用 `answer` 回答。
fn start_askpass(output: &Output, asks: &Asks) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let (output, asks) = (output.clone(), asks.clone());
    let next = AtomicU64::new(1);
    let started = fr_core::askpass::serve(&exe, move |prompt| {
        let id = next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        asks.lock().unwrap().insert(id, tx);
        let ask = Response {
            id,
            ask: Some(prompt.to_string()),
            ..Default::default()
        };
        send(&output, &ask, &[]);
        let answer = rx.recv_timeout(ASK_TIMEOUT).ok().flatten();
        asks.lock().unwrap().remove(&id);
        answer
    });
    match started {
        Ok(env) => fr_core::git::set_extra_env(env),
        // 开不了也照常工作，只是要密码的操作会直接失败
        Err(err) => eprintln!("fr-agent: 密码转发不可用：{err:#}"),
    }
}

/// 启动 git，在单独的线程里收输出，主线程接着读下一个请求。
fn start_git(request: Request, payload: Vec<u8>, output: &Output, running: &Running) {
    let id = request.id;
    let fail = |err: anyhow::Error| {
        send(
            output,
            &Response {
                id,
                error: Some(format!("{err:#}")),
                ..Default::default()
            },
            &[],
        );
    };
    let params: GitParams = match serde_json::from_value(request.params).context("git 参数不对")
    {
        Ok(params) => params,
        Err(err) => return fail(err),
    };
    let stdin = (request.bytes > 0).then_some(payload);
    let on_stderr: Option<OnStderr> = (params.stream && params.progress).then(|| {
        let output = output.clone();
        Box::new(move |chunk: &[u8]| {
            let header = Response {
                id,
                chunk: true,
                err: true,
                bytes: chunk.len() as u64,
                ..Default::default()
            };
            send(&output, &header, chunk);
        }) as OnStderr
    });
    let spawned = exec::spawn_with(Path::new(&params.repo), &params.args, stdin, on_stderr);
    let (process, killer) = match spawned {
        Ok(started) => started,
        Err(err) => return fail(err),
    };
    running.lock().unwrap().insert(id, killer);

    let output = output.clone();
    let running = running.clone();
    std::thread::spawn(move || {
        let outcome = if params.stream {
            process
                .finish(|chunk| {
                    let header = Response {
                        id,
                        chunk: true,
                        bytes: chunk.len() as u64,
                        ..Default::default()
                    };
                    send(&output, &header, chunk);
                })
                .map(|finished| (finished, Vec::new()))
        } else {
            process
                .collect()
                .map(|(stdout, finished)| (finished, stdout))
        };
        running.lock().unwrap().remove(&id);
        match outcome {
            Ok((finished, stdout)) => {
                let result = GitResult {
                    code: finished.code,
                    stderr: finished.stderr,
                    ms: finished.elapsed.as_millis() as u64,
                };
                let response = Response {
                    id,
                    result: Some(serde_json::to_value(result).unwrap_or_default()),
                    bytes: stdout.len() as u64,
                    ..Default::default()
                };
                send(&output, &response, &stdout);
            }
            Err(err) => send(
                &output,
                &Response {
                    id,
                    error: Some(format!("{err:#}")),
                    ..Default::default()
                },
                &[],
            ),
        }
    });
}

/// 发一条消息。写失败说明客户端已经断开，主线程读 stdin 时会发现并退出，这里不用管。
fn send(output: &Output, response: &Response, payload: &[u8]) {
    if let Ok(mut out) = output.lock() {
        let _ = protocol::write_message(&mut *out, response, payload);
    }
}

/// git 版本只查一次。
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
