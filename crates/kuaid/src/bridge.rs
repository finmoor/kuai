//! 转接程序（`kuaid --connect`）：客户端每次经 ssh 启动它，它把 ssh 的标准输入输出接到常驻进程的
//! 套接字上。接之前先办三件事：
//!
//! 1. 常驻进程没在跑就把它拉起来（Windows 上运行着 Kuai 的话请它来拉，常驻进程落在桌面上）。
//! 2. 常驻进程不是自己这一版、而且不比自己新，就让它原地换成自己这一版（终端会话留着）。
//!    比自己新就不动它：别的电脑上有更新的客户端，新的 kuaid 认得旧客户端。
//! 3. 告诉常驻进程这条 ssh 连接转发过来的 ssh-agent 在哪。
//!
//! 之后只是原样搬字节，协议由客户端和常驻进程自己说。

use std::fs::File;
use std::io::{BufReader, ErrorKind, Read, Write};
use std::mem::ManuallyDrop;
use std::net::Shutdown;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use fr_core::protocol::{self, DaemonInfo, Request, Response, TermInfo};
use serde_json::{Value, json};

use crate::daemon::{Paths, Stream, dial};

/// 我们拉起的那个常驻进程：只用来看它退出了没有。
#[cfg(unix)]
type Child = std::process::Child;
#[cfg(windows)]
type Child = Started;

/// Windows 上常驻进程是谁拉起的。
#[cfg(windows)]
enum Started {
    /// 我们自己
    Own(crate::win::Detached),
    /// 请运行着的 Kuai 拉起的（它说办了的时刻）：手里没有那个进程
    ByKuai(Instant),
}

#[cfg(windows)]
impl Started {
    /// 照着标准库的 `Child::try_wait`：`Some` 是它退出了（该再拉一次）。Kuai 拉起的看不到进程，
    /// 过了 [`KUAI_WAIT`] 还连不上就当它没起来。
    fn try_wait(&mut self) -> std::io::Result<Option<()>> {
        match self {
            Started::Own(child) => child.try_wait(),
            Started::ByKuai(at) => Ok((at.elapsed() >= KUAI_WAIT).then_some(())),
        }
    }
}

/// Kuai 说常驻进程拉起来了，等它听上端口最多这么久
#[cfg(windows)]
const KUAI_WAIT: Duration = Duration::from_secs(3);

/// 拉起常驻进程、等它换完程序，最多等这么久
const WAIT: Duration = Duration::from_secs(8);
/// 日志文件超过这么大就从头写
#[cfg(unix)]
const LOG_LIMIT: u64 = 1 << 20;
/// 转接程序自己发的请求用的编号：和客户端的编号不相干（响应在开始搬字节前就收完了）
const OWN_ID: u64 = u64::MAX;

/// `kuaid --connect`。
pub fn connect() -> Result<()> {
    let paths = Paths::new()?;
    paths.create()?;
    // Windows 上被进程占着的目录删不掉：不占着启动时所在的版本目录
    #[cfg(windows)]
    let _ = std::env::set_current_dir(crate::dirs::home_dir());
    let mine = crate::build();
    let exe = std::env::current_exe().context("无法确定 kuaid 的安装位置")?;
    let deadline = Instant::now() + WAIT;
    let mut started: Option<Child> = None;
    let mut asked_upgrade = false;
    let stream = loop {
        let mut stream = connect_or_start(&paths, &mut started, deadline)?;
        // 常驻进程刚好在退出、或者正在换程序时，问不到：重来
        let Ok(info) = call(&mut stream, "info", json!({})) else {
            ensure_time(deadline, "常驻进程无响应")?;
            std::thread::sleep(Duration::from_millis(30));
            continue;
        };
        let info: DaemonInfo = serde_json::from_value(info).context("无法解析常驻进程的响应")?;
        if asked_upgrade {
            break stream;
        }
        let replace = info.build != mine && !mine.is_empty() && should_replace(&info.build, &mine);
        let request = if replace {
            // 让它换成我们这一版
            Some(("upgrade", json!({ "exe": exe })))
        } else if misplaced(&paths, &info) {
            // 版本不用换，只是该搬到桌面上：让它用自己那一版重新起一遍
            Some(("move", json!({})))
        } else {
            None
        };
        let Some((method, params)) = request else {
            break stream;
        };
        // 成功的话它不回答，连接直接断；回答了就是没办成，照旧用它
        asked_upgrade = true;
        match call(&mut stream, method, params) {
            Err(_) => continue,
            Ok(_) => break stream,
        }
    };
    let mut stream = stream;
    let auth_sock = std::env::var("SSH_AUTH_SOCK")
        .ok()
        .filter(|s| !s.is_empty());
    send(&mut stream, "bridge", json!({ "auth_sock": auth_sock }))?;
    // 开头这几步可能设了读超时（Windows），之后客户端多久不说话都行
    let _ = stream.set_read_timeout(None);
    pump(stream)
}

/// `kuaid --boot`：开机时（Windows 上是登录时）由启动项运行。常驻进程没在跑就拉起来，然后退出。
pub fn boot() -> Result<()> {
    // 登录时由 Run 键启动，系统会给我们开一个控制台窗口：马上脱开，窗口跟着关掉
    #[cfg(windows)]
    // SAFETY: 没有参数，只是不再连着控制台
    unsafe {
        windows_sys::Win32::System::Console::FreeConsole();
    }
    // 开机时 cron 给的 SHELL 是 /bin/sh：拉起的常驻进程开的终端就成了 sh，读不到 ~/.bashrc 里加的
    // PATH（nvm 装的 claude、codex）。按账号登记的登录 shell 来
    #[cfg(unix)]
    if let Some(shell) = crate::term::pty::passwd_shell() {
        // SAFETY: 刚启动，还没有别的线程
        unsafe { std::env::set_var("SHELL", shell) };
    }
    let paths = Paths::new()?;
    paths.create()?;
    #[cfg(windows)]
    let _ = std::env::set_current_dir(crate::dirs::home_dir());
    let mut started = None;
    let stream = connect_or_start(&paths, &mut started, Instant::now() + WAIT)?;
    drop(stream);
    Ok(())
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

/// 常驻进程该不该由 Kuai 重新起一遍。两种情况，都要我们在桌面上、这台电脑上还运行着 Kuai
/// （由它来起）：
///
/// - 它在会话 0（经 ssh 拉起的，没有桌面）。搬到桌面上以后新开的终端才开得了带窗口的程序。
/// - 它带着 Redirection Guard（[`crate::win::redirect_guarded`]）而我们没带：它是带着防护的 Kuai
///   起的，里面的终端进不了目录联接；我们是现在这个 Kuai 起的，没带，说明 Kuai 已经甩掉了，
///   由它重新起的常驻进程也不带。我们自己也带着的话不动：重新起一遍还是带着，白折腾。
///
/// 已经开着的终端留在原来的进程里，照常用（防护甩不掉，要重开）。
#[cfg(windows)]
fn misplaced(paths: &Paths, info: &DaemonInfo) -> bool {
    should_move(
        crate::win::session_id() != 0,
        info.session == Some(0),
        info.guarded,
        crate::win::redirect_guarded(),
    ) && crate::win::kuai_pipe_file(&paths.run).is_file()
}

/// [`misplaced`] 的判断：我们在不在桌面上、常驻进程在不在会话 0、常驻进程和我们各自带没带着防护。
#[cfg(any(windows, test))]
fn should_move(
    on_desktop: bool,
    daemon_in_session_0: bool,
    daemon_guarded: bool,
    guarded: bool,
) -> bool {
    on_desktop && (daemon_in_session_0 || (daemon_guarded && !guarded))
}

#[cfg(unix)]
fn misplaced(_paths: &Paths, _info: &DaemonInfo) -> bool {
    false
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
) -> Result<Stream> {
    loop {
        if let Ok(stream) = dial(paths) {
            return Ok(stream);
        }
        // 没拉起过，或者拉起的那个已经退出了（比如另一个转接程序抢先拉起了一个）：再拉
        let need_start = match started {
            None => true,
            Some(child) => matches!(child.try_wait(), Ok(Some(_)) | Err(_)),
        };
        if need_start {
            let first = started.is_none();
            *started = Some(start_daemon(paths, first)?);
        }
        ensure_time(deadline, "常驻进程启动失败，详见 ~/.kuai/run/kuaid.log")?;
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// 拉起常驻进程：自己一个会话，标准输入输出都不接在这条 ssh 连接上（不然 ssh 断不开）。
#[cfg(unix)]
fn start_daemon(paths: &Paths, _first: bool) -> Result<Child> {
    use std::fs::OpenOptions;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let log_path = paths.log();
    let too_big = std::fs::metadata(&log_path).is_ok_and(|meta| meta.len() > LOG_LIMIT);
    let log = OpenOptions::new()
        .create(true)
        .append(!too_big)
        .write(true)
        .truncate(too_big)
        .open(&log_path)
        .with_context(|| format!("无法写入 {}", log_path.display()))?;
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
    cmd.spawn().context("无法启动常驻进程")
}

/// 拉起常驻进程。这台电脑上运行着 Kuai 就先请它来拉（常驻进程落在桌面上，见
/// [`crate::win::ask_kuai`]），只问头一次；不然自己拉：脱离这条 ssh 连接的作业（不然连接一断就被
/// sshd 杀掉），不带着我们的标准输入输出（它自己把输出接到日志文件上）。
#[cfg(windows)]
fn start_daemon(paths: &Paths, first: bool) -> Result<Child> {
    let exe = std::env::current_exe()?;
    if first && crate::win::ask_kuai(&paths.run, &exe, None) {
        return Ok(Started::ByKuai(Instant::now()));
    }
    crate::win::spawn_detached(&exe, &["--daemon"])
        .map(Started::Own)
        .context("无法启动常驻进程")
}

fn send(stream: &mut Stream, method: &str, params: Value) -> Result<()> {
    let request = Request {
        id: OWN_ID,
        method: method.into(),
        params,
        bytes: 0,
    };
    protocol::write_message(stream, &request, &[]).context("与常驻进程的连接已断开")
}

/// 发一个请求，等它的响应。只在开始搬字节之前用：这时连接上没有别的消息。
fn call(stream: &mut Stream, method: &str, params: Value) -> Result<Value> {
    send(stream, method, params)?;
    // 一个字节一个字节地读：不能多读，后面的字节是客户端的
    let mut reader = BufReader::with_capacity(1, &*stream);
    let (response, _) =
        protocol::read_message::<_, Response>(&mut reader)?.context("常驻进程已断开连接")?;
    match response.error {
        Some(err) => bail!("{err}"),
        None => Ok(response.result.unwrap_or(Value::Null)),
    }
}

/// 两头原样搬字节，直到一头关了。
fn pump(stream: Stream) -> Result<()> {
    // 直接用标准输入输出底下的文件：标准库的标准输出会按行攒着，这里要来多少马上写多少
    let (mut stdin, mut stdout) = raw_stdio();
    let mut to_daemon = stream.try_clone().context("套接字不可用")?;
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

#[cfg(unix)]
fn raw_stdio() -> (ManuallyDrop<File>, ManuallyDrop<File>) {
    use std::os::fd::FromRawFd;
    // SAFETY: 0 和 1 是进程的标准输入输出，放在 ManuallyDrop 里不会被关掉
    unsafe {
        (
            ManuallyDrop::new(File::from_raw_fd(0)),
            ManuallyDrop::new(File::from_raw_fd(1)),
        )
    }
}

#[cfg(windows)]
fn raw_stdio() -> (ManuallyDrop<File>, ManuallyDrop<File>) {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    // SAFETY: 这两个句柄是进程的标准输入输出，放在 ManuallyDrop 里不会被关掉
    unsafe {
        (
            ManuallyDrop::new(File::from_raw_handle(std::io::stdin().as_raw_handle())),
            ManuallyDrop::new(File::from_raw_handle(std::io::stdout().as_raw_handle())),
        )
    }
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
fn existing() -> Result<Stream> {
    let paths = Paths::new()?;
    dial(&paths).context("常驻进程未运行")
}

/// `kuaid --status`：常驻进程和终端会话的情况。
pub fn status() -> Result<()> {
    let mut stream = existing()?;
    let info: DaemonInfo = serde_json::from_value(call(&mut stream, "info", json!({}))?)?;
    println!(
        "kuaid {} {}，进程号 {}，已连接 {} 个客户端",
        info.version,
        info.build,
        info.pid,
        // 自己这一条不算
        info.clients.saturating_sub(1),
    );
    let listed = call(&mut stream, "term_list", json!({}))?;
    let terms: Vec<TermInfo> = serde_json::from_value(listed["terms"].clone()).unwrap_or_default();
    if terms.is_empty() {
        println!("暂无终端会话");
    }
    for term in terms {
        println!(
            "{}  {}  {}×{}  {}  已连接 {} 个客户端  已运行 {} 分钟{}",
            term.term,
            term.cwd,
            term.cols,
            term.rows,
            if term.alive { "运行中" } else { "已结束" },
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
    use super::{should_move, should_replace};

    #[test]
    fn 常驻进程什么时候让_kuai_重新起一遍() {
        // 它在会话 0、我们在桌面上：搬过来
        assert!(should_move(true, true, false, false));
        // 它带着 Redirection Guard、我们没带（Kuai 已经甩掉了）：重新起一遍就不带了
        assert!(should_move(true, false, true, false));
        // 都带着：重新起一遍还是带着，不动
        assert!(!should_move(true, false, true, true));
        // 都好好的
        assert!(!should_move(true, false, false, false));
        // 我们自己在会话 0（经 ssh 来的）：不替桌面上的做主，哪种情况都不动
        assert!(!should_move(false, true, false, false));
        assert!(!should_move(false, false, true, false));
    }

    #[test]
    fn 比自己旧或者一样新的才换() {
        assert!(should_replace("1.4.0-aaaa", "1.5.0-bbbb"));
        assert!(should_replace("1.5.0-aaaa", "1.5.0-bbbb"));
        assert!(should_replace("1.5.9-aaaa", "1.5.10-bbbb"));
        assert!(!should_replace("1.6.0-aaaa", "1.5.0-bbbb"));
        assert!(!should_replace("2.0.0-aaaa", "1.9.9-bbbb"));
    }
}
