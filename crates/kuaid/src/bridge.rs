//! 转接程序（`kuaid --connect`）：客户端每次经 ssh 启动它，它把 ssh 的标准输入输出接到常驻进程的
//! 套接字上。接之前先办三件事：
//!
//! 1. 常驻进程没在跑就把它拉起来。
//! 2. 常驻进程不是自己这一版、而且不比自己新，就让它原地换成自己这一版（终端会话留着）。
//!    比自己新就不动它：别的电脑上有更新的客户端，新的 kuaid 认得旧客户端。
//! 3. 告诉常驻进程这条 ssh 连接转发过来的 ssh-agent 在哪。
//!
//! 之后只是原样搬字节，协议由客户端和常驻进程自己说。

use std::fs::{File, OpenOptions};
use std::io::{BufReader, ErrorKind, Read, Write};
use std::mem::ManuallyDrop;
use std::net::Shutdown;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use fr_core::protocol::{self, DaemonInfo, Request, Response, TermInfo};
use serde_json::{Value, json};

use crate::daemon::Paths;

/// 拉起常驻进程、等它换完程序，最多等这么久
const WAIT: Duration = Duration::from_secs(8);
/// 日志文件超过这么大就从头写
const LOG_LIMIT: u64 = 1 << 20;
/// 转接程序自己发的请求用的编号：和客户端的编号不相干（响应在开始搬字节前就收完了）
const OWN_ID: u64 = u64::MAX;

/// `kuaid --connect`。
pub fn connect() -> Result<()> {
    let paths = Paths::new()?;
    paths.create()?;
    let mine = crate::build();
    let exe = std::env::current_exe().context("不知道自己装在哪")?;
    let deadline = Instant::now() + WAIT;
    let mut started: Option<Child> = None;
    let mut asked_upgrade = false;
    let stream = loop {
        let mut stream = connect_or_start(&paths, &mut started, deadline)?;
        // 常驻进程刚好在退出、或者正在换程序时，问不到：重来
        let Ok(info) = call(&mut stream, "info", json!({})) else {
            ensure_time(deadline, "常驻进程没有回应")?;
            std::thread::sleep(Duration::from_millis(30));
            continue;
        };
        let info: DaemonInfo = serde_json::from_value(info).context("常驻进程的回答看不懂")?;
        if info.build == mine || mine.is_empty() || asked_upgrade || !should_replace(&info.build, &mine)
        {
            break stream;
        }
        // 让它换成我们这一版。成功的话它不回答，连接直接断；回答了就是没换成，照旧用它
        asked_upgrade = true;
        match call(&mut stream, "upgrade", json!({ "exe": exe })) {
            Err(_) => continue,
            Ok(_) => break stream,
        }
    };
    let mut stream = stream;
    let auth_sock = std::env::var("SSH_AUTH_SOCK").ok().filter(|s| !s.is_empty());
    send(&mut stream, "bridge", json!({ "auth_sock": auth_sock }))?;
    pump(stream)
}

/// 常驻进程换不换成我们这一版：版本号比我们旧就换；版本号一样、程序不一样（开发时）也换。
fn should_replace(theirs: &str, mine: &str) -> bool {
    let version = |build: &str| -> Vec<u64> {
        build
            .split('-')
            .next()
            .unwrap_or_default()
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    };
    version(theirs) <= version(mine)
}

fn ensure_time(deadline: Instant, what: &str) -> Result<()> {
    if Instant::now() >= deadline {
        bail!("{what}");
    }
    Ok(())
}

/// 连上常驻进程的套接字；连不上就拉起常驻进程再连。
fn connect_or_start(
    paths: &Paths,
    started: &mut Option<Child>,
    deadline: Instant,
) -> Result<UnixStream> {
    loop {
        if let Ok(stream) = UnixStream::connect(paths.socket()) {
            return Ok(stream);
        }
        // 没拉起过，或者拉起的那个已经退出了（比如另一个转接程序抢先拉起了一个）：再拉
        let need_start = match started {
            None => true,
            Some(child) => matches!(child.try_wait(), Ok(Some(_)) | Err(_)),
        };
        if need_start {
            *started = Some(start_daemon(paths)?);
        }
        ensure_time(deadline, "常驻进程起不来，看看 ~/.kuai/run/kuaid.log")?;
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// 拉起常驻进程：自己一个会话，标准输入输出都不接在这条 ssh 连接上（不然 ssh 断不开）。
fn start_daemon(paths: &Paths) -> Result<Child> {
    let log_path = paths.log();
    let too_big = std::fs::metadata(&log_path).is_ok_and(|meta| meta.len() > LOG_LIMIT);
    let log = OpenOptions::new()
        .create(true)
        .append(!too_big)
        .write(true)
        .truncate(too_big)
        .open(&log_path)
        .with_context(|| format!("写不了 {}", log_path.display()))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    // SAFETY: 闭包在 fork 之后、exec 之前的子进程里跑，只做一个系统调用
    unsafe {
        cmd.pre_exec(|| {
            // 自己一个会话：这条 ssh 连接断了也不会被带走
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("拉不起常驻进程")
}

fn send(stream: &mut UnixStream, method: &str, params: Value) -> Result<()> {
    let request = Request {
        id: OWN_ID,
        method: method.into(),
        params,
        bytes: 0,
    };
    protocol::write_message(stream, &request, &[]).context("和常驻进程的连接断了")
}

/// 发一个请求，等它的响应。只在开始搬字节之前用：这时连接上没有别的消息。
fn call(stream: &mut UnixStream, method: &str, params: Value) -> Result<Value> {
    send(stream, method, params)?;
    // 一个字节一个字节地读：不能多读，后面的字节是客户端的
    let mut reader = BufReader::with_capacity(1, &*stream);
    let (response, _) = protocol::read_message::<_, Response>(&mut reader)?
        .context("常驻进程断开了连接")?;
    match response.error {
        Some(err) => bail!("{err}"),
        None => Ok(response.result.unwrap_or(Value::Null)),
    }
}

/// 两头原样搬字节，直到一头关了。
fn pump(stream: UnixStream) -> Result<()> {
    // 直接用文件描述符 0 和 1：标准库的标准输出会按行攒着，这里要来多少马上写多少
    // SAFETY: 0 和 1 是进程的标准输入输出，放在 ManuallyDrop 里不会被关掉
    let (mut stdin, mut stdout) = unsafe {
        (
            ManuallyDrop::new(File::from_raw_fd(0)),
            ManuallyDrop::new(File::from_raw_fd(1)),
        )
    };
    let mut to_daemon = stream.try_clone().context("套接字用不了")?;
    std::thread::Builder::new()
        .name("kuaid-bridge-in".into())
        .spawn(move || {
            let _ = copy(&mut *stdin, &mut to_daemon);
            // 客户端走了（ssh 断了）：告诉常驻进程这一头不会再有请求
            let _ = to_daemon.shutdown(Shutdown::Write);
        })?;
    let mut from_daemon = stream;
    let _ = copy(&mut from_daemon, &mut *stdout);
    // 常驻进程关了这条连接（客户端走了以后它收拾完，或者它退出、换程序了）：我们也结束，
    // ssh 跟着断开，客户端知道要重连
    Ok(())
}

fn copy(from: &mut impl Read, to: &mut impl Write) -> std::io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => to.write_all(&buf[..n])?,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

/// 连上正在跑的常驻进程；没在跑就报错（不拉起）。
fn existing() -> Result<UnixStream> {
    let paths = Paths::new()?;
    UnixStream::connect(paths.socket()).context("常驻进程没在跑")
}

/// `kuaid --status`：常驻进程和终端会话的情况。
pub fn status() -> Result<()> {
    let mut stream = existing()?;
    let info: DaemonInfo = serde_json::from_value(call(&mut stream, "info", json!({}))?)?;
    println!(
        "kuaid {} {}，进程号 {}，连着 {} 个客户端",
        info.version,
        info.build,
        info.pid,
        // 自己这一条不算
        info.clients.saturating_sub(1),
    );
    let listed = call(&mut stream, "term_list", json!({}))?;
    let terms: Vec<TermInfo> = serde_json::from_value(listed["terms"].clone()).unwrap_or_default();
    if terms.is_empty() {
        println!("没有终端会话");
    }
    for term in terms {
        println!(
            "{}  {}  {}×{}  {}  接着 {} 个客户端  开了 {} 分钟{}",
            term.term,
            term.cwd,
            term.cols,
            term.rows,
            if term.alive { "在跑" } else { "已结束" },
            term.attached,
            term.age / 60,
            term.title.map(|t| format!("  {t}")).unwrap_or_default(),
        );
    }
    Ok(())
}

/// `kuaid --stop`：结束所有终端会话，让常驻进程退出。
pub fn stop() -> Result<()> {
    let mut stream = existing()?;
    call(&mut stream, "shutdown", json!({}))?;
    // 等它真的退出：连接被关掉
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest);
    println!("常驻进程已退出");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::should_replace;

    #[test]
    fn 比自己旧或者一样新的才换() {
        assert!(should_replace("1.4.0-aaaa", "1.5.0-bbbb"));
        assert!(should_replace("1.5.0-aaaa", "1.5.0-bbbb"));
        assert!(should_replace("1.5.9-aaaa", "1.5.10-bbbb"));
        assert!(!should_replace("1.6.0-aaaa", "1.5.0-bbbb"));
        assert!(!should_replace("2.0.0-aaaa", "1.9.9-bbbb"));
    }
}
