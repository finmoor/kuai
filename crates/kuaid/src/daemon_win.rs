//! Windows 上的常驻进程：听 127.0.0.1 的一个随机端口，每个连上来的客户端一个线程。
//!
//! 和 Linux 上（`daemon.rs`）做的是同一件事，系统给的东西不一样的地方：
//!
//! - **谁能连**：Windows 没有带权限的套接字文件。端口和一段随机口令写在 `~/.kuai/run/kuaid.port`
//!   里（在用户自己的目录下），连上来先交口令。经 ssh 登录后由转接程序（`kuaid --connect`）接进来，
//!   本机的 Kuai 也是启动转接程序来接。
//! - **只跑一个**：独占打开 `kuaid.lock`，开着就是锁着。
//! - **升级**：没有 `exec`，换不了自己。终端都开在托管进程里（`term::host`），升级时先让它们停止
//!   发输出、把每个终端的屏幕拍进交接文件，再启动新程序、自己退出；新程序读交接文件，重新连上
//!   各个托管进程。终端里的程序一直是托管进程的子进程，不受影响。进程号会变，端口也会变。
//!   交接文件的格式只加字段：永远是旧程序写、新程序读。

use std::fs::{File, OpenOptions};
use std::io::{BufRead as _, BufReader, ErrorKind, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::conn::{self, Control, Shared};
use crate::files;
use crate::term::hosted::{self, Hosted};
use crate::term::{Backend, Ended, Frozen, Registry, Size};
use fr_core::protocol::TermMark;

/// 转接程序和常驻进程之间的连接。
pub type Stream = TcpStream;

/// 没有客户端、没有终端会话这么久就退出
const IDLE: Duration = Duration::from_secs(600);
/// 隔这么久看一次要不要退出
const IDLE_CHECK: Duration = Duration::from_secs(5);
/// 隔这么久清一次客户端传上来的过期文件
const SWEEP_EVERY: Duration = Duration::from_secs(3600);
/// 交接文件的格式版本
const HANDOFF_FORMAT: u32 = 1;
/// 交接文件写下这么久以内，算是正在升级：这时不另起一个常驻进程
const HANDOFF_FRESH: Duration = Duration::from_secs(30);
/// 升级换过来的新程序等旧程序放开锁，最多等这么久
const LOCK_WAIT: Duration = Duration::from_secs(15);
/// 连上来以后交口令、问版本这几步，最多等这么久
const GREETING_WAIT: Duration = Duration::from_secs(10);

/// `~/.kuai/run` 里的几个文件。
pub struct Paths {
    pub run: PathBuf,
}

/// `kuaid.port` 的内容。
#[derive(Serialize, Deserialize)]
struct Port {
    port: u16,
    token: String,
    #[serde(default)]
    pid: u32,
}

impl Paths {
    pub fn new() -> Result<Paths> {
        Ok(Paths {
            run: crate::dirs::kuai_root().join("run"),
        })
    }

    pub fn create(&self) -> Result<()> {
        std::fs::create_dir_all(&self.run)
            .with_context(|| format!("无法创建 {}", self.run.display()))
    }

    /// 里面写着常驻进程听的端口和连它要交的口令
    fn port(&self) -> PathBuf {
        self.run.join("kuaid.port")
    }

    fn lock(&self) -> PathBuf {
        self.run.join("kuaid.lock")
    }

    /// 常驻进程写的日志
    pub fn log(&self) -> PathBuf {
        self.run.join("kuaid.log")
    }

    /// 里面写着常驻进程现在用的是哪个版本目录：清理旧版本时不删它
    fn current(&self) -> PathBuf {
        self.run.join("current")
    }

    fn handoff(&self) -> PathBuf {
        self.run.join("handoff")
    }

    /// 退出前删掉只在跑着的时候有用的文件。锁文件和日志留着。
    fn clear(&self) {
        for path in [self.port(), self.current()] {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 连上常驻进程：按 `kuaid.port` 里写的端口连，交上口令。没在跑就连不上。
/// 开头这几步设了读超时（那个端口万一已经是别的程序在用），开始搬字节前由调用方取消。
pub fn dial(paths: &Paths) -> std::io::Result<Stream> {
    let text = std::fs::read_to_string(paths.port())?;
    let port: Port = serde_json::from_str(&text).map_err(std::io::Error::other)?;
    let mut stream = TcpStream::connect(("127.0.0.1", port.port))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(GREETING_WAIT))?;
    stream.write_all(greeting(&port.token).as_bytes())?;
    Ok(stream)
}

fn greeting(token: &str) -> String {
    format!("KUAID {token}\n")
}

/// 读连接开头的那一行，对上口令才算数。一个字节一个字节地读：后面的字节是协议的。整行共用
/// 一个期限：读超时管的只是一次读，一个字节一个字节慢慢发的能把线程一直占着。
fn greeted(stream: &mut TcpStream, token: &str) -> bool {
    let expected = greeting(token);
    let deadline = Instant::now() + GREETING_WAIT;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() <= expected.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return false;
        }
        match stream.read(&mut byte) {
            Ok(1) => {
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            _ => return false,
        }
    }
    let _ = stream.set_read_timeout(None);
    line == expected.as_bytes()
}

/// 交接文件开头的那一行 JSON；后面依次跟着每个会话还原屏幕用的字节。
#[derive(Serialize, Deserialize, Default)]
struct Handoff {
    format: u32,
    /// 连各个托管进程要交的口令
    #[serde(default)]
    host_token: String,
    /// 下一个终端会话的编号
    #[serde(default)]
    next_term: u64,
    #[serde(default)]
    sessions: Vec<HandoffSession>,
}

#[derive(Serialize, Deserialize, Default)]
struct HandoffSession {
    id: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    created: u64,
    cols: u16,
    rows: u16,
    /// 托管进程听的端口和 shell 的进程号；程序已经结束的是 -1
    #[serde(default = "minus_one")]
    port: i32,
    #[serde(default = "minus_one")]
    pid: i32,
    /// 程序已经结束
    #[serde(default)]
    exited: bool,
    #[serde(default)]
    exit: Option<i32>,
    /// 还原屏幕用的字节有多长
    #[serde(default)]
    dump: u64,
    /// 发出去的消息（协议 16 起）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    marks: Vec<TermMark>,
    /// 屏幕上的字多久没变了，毫秒（协议 18 起；没变过的、更早的程序交过来的没有）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quiet_ms: Option<u64>,
    /// 多久没干活了，毫秒（协议 19 起，见 `term::Frozen::work`；没干过的、更早的程序交过来的没有）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    work_ms: Option<u64>,
}

fn minus_one() -> i32 {
    -1
}

struct Daemon {
    paths: Paths,
    host_token: String,
    upgrading: AtomicBool,
}

impl Control for Daemon {
    fn upgrade(&self, shared: &Shared, exe: &str) -> anyhow::Error {
        let exe = Path::new(exe);
        let ours = exe.is_absolute()
            && exe
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("kuaid.exe"))
            && exe.is_file();
        if !ours {
            return anyhow!("{} 不是 kuaid 程序", exe.display());
        }
        if self.upgrading.swap(true, Ordering::AcqRel) {
            return anyhow!("正在升级");
        }
        log(&format!("升级：替换为 {}", exe.display()));
        shared.kill_git();
        // 不再开新的终端，正在开的开完（暂停、拍快照都包括它）
        shared.terms.hold_opens();
        // 从这里起托管进程不再发输出：程序写的东西留在它们手里，新程序连上去接着收。有一个没
        // 确认停下就不换：交接出去的屏幕会缺一段
        if !shared.terms.pause_all() {
            shared.terms.release_opens();
            self.upgrading.store(false, Ordering::Release);
            log("升级失败：终端托管进程没有确认暂停");
            return anyhow!("终端托管进程没有确认暂停，未升级");
        }
        let (next_term, frozen) = shared.terms.freeze();
        let path = self.paths.handoff();
        let started = write_handoff(&path, &self.host_token, next_term, &frozen).and_then(|()| {
            let state = path.to_string_lossy();
            // 运行着 Kuai 就请它来起：新的常驻进程落在桌面上（原来在会话 0 的就此搬过去）
            if crate::win::ask_kuai(&self.paths.run, exe, Some(&state)) {
                log("已请 Kuai 启动新程序");
                return Ok(());
            }
            crate::win::spawn_detached(exe, &["--daemon", "--resume", &state])
                .map(drop)
                .with_context(|| format!("无法启动 {}", exe.display()))
        });
        match started {
            Ok(()) => {
                // 新程序在等我们放开锁；连着的客户端跟着断开，自己重连
                log("已启动新程序，退出");
                std::process::exit(0);
            }
            Err(failed) => {
                let _ = std::fs::remove_file(&path);
                shared.terms.resume_all();
                shared.terms.release_opens();
                self.upgrading.store(false, Ordering::Release);
                log(&format!("升级失败：{failed:#}"));
                failed
            }
        }
    }

    fn shutdown(&self, shared: &Shared) -> ! {
        log("收到退出请求");
        // 定时任务还开着的几次运行先存下最后的画面
        let cleanups = shared.tasks.stopping(shared);
        // 等终端里的程序退出，不理的等它被强杀
        shared.terms.close_all(Duration::from_secs(3));
        crate::tasks::Tasks::wait_cleanups(cleanups, Duration::from_secs(5));
        self.paths.clear();
        log("退出");
        std::process::exit(0);
    }

    fn auth_sock(&self, path: &str) -> Option<String> {
        Some(path.to_string())
    }
}

/// `kuaid --spawn-daemon`：用这个程序再起一个常驻进程（`args` 是给它的参数），自己马上退出。
pub fn spawn(args: &[&str]) -> Result<()> {
    let exe = std::env::current_exe().context("无法确定 kuaid 的安装位置")?;
    crate::win::spawn_detached(&exe, args)
        .map(drop)
        .context("无法启动常驻进程")
}

/// 常驻进程的入口。`resume` 是交接文件：有的话是升级换过来的，接着管原来的终端。
pub fn run(resume: Option<&str>) -> Result<()> {
    // 这些是拉起我们的那条 ssh 连接留下的，连接一断就不对了
    // SAFETY: 这时还没有开别的线程
    unsafe {
        for key in ["SSH_AUTH_SOCK", "SSH_CLIENT", "SSH_CONNECTION", "SSH_TTY"] {
            std::env::remove_var(key);
        }
    }
    let paths = Paths::new()?;
    paths.create()?;
    // 启动时没有继承到标准输出：日志自己接
    crate::win::log_to(&paths.log());
    // 不占着拉起我们时所在的目录（Windows 上被占着的目录删不掉）
    let _ = std::env::set_current_dir(crate::dirs::home_dir());

    let (lock, handoff) = match resume {
        Some(state) => {
            let lock = wait_lock(&paths)?;
            (lock, Some(read_handoff(Path::new(state))?))
        }
        None => {
            // 正在升级：新程序马上接手，这里不抢
            if handoff_is_fresh(&paths) {
                return Ok(());
            }
            let Some(lock) = try_lock(&paths) else {
                // 已经有一个在跑了
                return Ok(());
            };
            if handoff_is_fresh(&paths) {
                return Ok(());
            }
            (lock, None)
        }
    };
    let host_token = match &handoff {
        Some((handoff, _)) if !handoff.host_token.is_empty() => handoff.host_token.clone(),
        _ => fr_core::askpass::random_token(),
    };
    let terms = Registry::new(hosted::spawner(host_token.clone()));
    let adopted = match handoff {
        Some((handoff, dumps)) => adopt(&terms, handoff, dumps, &host_token),
        None => 0,
    };

    let listener = TcpListener::bind(("127.0.0.1", 0)).context("无法在本机打开监听端口")?;
    let token = fr_core::askpass::random_token();
    write_port(&paths, listener.local_addr()?.port(), &token)?;
    let build = crate::build();
    let _ = std::fs::write(paths.current(), &build);
    log(&format!(
        "{}：kuaid {} {build}，进程号 {}，接管 {adopted} 个终端，终端用{}的 ConPTY",
        if resume.is_some() {
            "升级完成"
        } else {
            "启动"
        },
        crate::VERSION,
        std::process::id(),
        if crate::term::conpty::uses_bundled() {
            "随带"
        } else {
            "系统自带"
        },
    ));

    let daemon = Daemon {
        paths: Paths::new()?,
        host_token,
        upgrading: AtomicBool::new(false),
    };
    let shared = Shared::new(terms, Some(Box::new(daemon)), build);
    crate::tasks::Tasks::start(&shared);
    // 开着开机时启动的话，改成启动这一版
    crate::autostart::refresh();
    shared.start_askpass(&paths.run);

    let for_idle = shared.clone();
    let idle_paths = Paths::new()?;
    std::thread::Builder::new()
        .name("kuaid-idle".into())
        .spawn(move || {
            // 一启动先清一次：上次退出以后过期的
            let mut swept: Option<Instant> = None;
            loop {
                if swept.is_none_or(|at| at.elapsed() >= SWEEP_EVERY) {
                    files::sweep(&for_idle.drop_dir);
                    swept = Some(Instant::now());
                }
                std::thread::sleep(IDLE_CHECK);
                for_idle.terms.sweep();
                if for_idle.idle_for() >= IDLE {
                    idle_paths.clear();
                    log("空闲超时，退出");
                    std::process::exit(0);
                }
            }
        })?;

    // 锁跟着这个句柄，留到进程结束
    std::mem::forget(lock);
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                log(&format!("无法接受新连接：{err}"));
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        // 还没对上口令的连接太多：新来的直接断开（本机别的用户也连得到这个端口）
        if GREETING.fetch_add(1, Ordering::AcqRel) >= MAX_GREETING {
            GREETING.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let (shared, token) = (shared.clone(), token.clone());
        let spawned = std::thread::Builder::new()
            .name("kuaid-conn".into())
            .spawn(move || {
                let _ = stream.set_nodelay(true);
                // 本机上别的用户也连得到这个端口：口令不对的不理
                let ok = greeted(&mut stream, &token);
                GREETING.fetch_sub(1, Ordering::AcqRel);
                if !ok {
                    return;
                }
                match stream.try_clone() {
                    Ok(reader) => conn::serve(&shared, reader, stream),
                    Err(err) => log(&format!("新连接不可用：{err}")),
                }
            });
        if let Err(err) = spawned {
            GREETING.fetch_sub(1, Ordering::AcqRel);
            log(&format!("无法创建线程：{err}"));
        }
    }
    Ok(())
}

/// 正在读开头那一行（还没对上口令）的连接有几个；最多 [`MAX_GREETING`] 个。
static GREETING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_GREETING: usize = 16;

/// 独占打开锁文件：开着就是锁着，进程一结束系统自己放开。已经被别的常驻进程拿着时返回 None。
fn try_lock(paths: &Paths) -> Option<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .share_mode(0)
        .open(paths.lock())
        .ok()
}

/// 升级换过来的：等旧程序退出、放开锁。
fn wait_lock(paths: &Paths) -> Result<File> {
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        if let Some(lock) = try_lock(paths) {
            return Ok(lock);
        }
        if Instant::now() >= deadline {
            bail!("上一个常驻进程没有退出");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn handoff_is_fresh(paths: &Paths) -> bool {
    std::fs::metadata(paths.handoff())
        .and_then(|meta| meta.modified())
        .is_ok_and(|at| {
            SystemTime::now()
                .duration_since(at)
                .is_ok_and(|age| age < HANDOFF_FRESH)
        })
}

/// 先写临时文件再改名：转接程序不会读到半个。
fn write_port(paths: &Paths, port: u16, token: &str) -> Result<()> {
    let content = serde_json::to_string(&Port {
        port,
        token: token.to_string(),
        pid: std::process::id(),
    })?;
    let temp = paths.run.join(format!("kuaid.port.{}", std::process::id()));
    std::fs::write(&temp, content)
        .and_then(|()| std::fs::rename(&temp, paths.port()))
        .with_context(|| format!("无法写入 {}", paths.port().display()))
}

fn write_handoff(path: &Path, host_token: &str, next_term: u64, frozen: &[Frozen]) -> Result<()> {
    let handoff = Handoff {
        format: HANDOFF_FORMAT,
        host_token: host_token.to_string(),
        next_term,
        sessions: frozen
            .iter()
            .map(|f| HandoffSession {
                id: f.id.clone(),
                key: f.key.clone(),
                cwd: f.cwd.clone(),
                created: f.created,
                cols: f.size.cols,
                rows: f.size.rows,
                port: f.handle.map_or(-1, |(port, _)| port),
                pid: f.handle.map_or(-1, |(_, pid)| pid),
                exited: f.exit.is_some(),
                exit: f.exit.flatten(),
                dump: f.dump.len() as u64,
                marks: f.marks.clone(),
                quiet_ms: f.quiet.map(|quiet| quiet.as_millis() as u64),
                work_ms: f.work.map(|work| work.as_millis() as u64),
            })
            .collect(),
    };
    // 里面有屏幕上的内容和连托管进程的口令：放在用户自己的目录下，新程序读完就删
    let mut file = File::create(path).with_context(|| format!("无法写入 {}", path.display()))?;
    serde_json::to_writer(&mut file, &handoff)?;
    file.write_all(b"\n")?;
    for f in frozen {
        file.write_all(&f.dump)?;
    }
    file.sync_all()?;
    Ok(())
}

/// 读交接文件（读完就删）：开头那一行，和每个会话还原屏幕用的字节。
fn read_handoff(path: &Path) -> Result<(Handoff, Vec<Vec<u8>>)> {
    let file = File::open(path).with_context(|| format!("无法读取 {}", path.display()));
    let read = file.and_then(|file| {
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let handoff: Handoff = serde_json::from_str(&line).context("交接文件格式错误")?;
        if handoff.format == 0 {
            bail!("交接文件不完整");
        }
        let mut dumps = Vec::new();
        for session in &handoff.sessions {
            let mut dump = vec![0u8; session.dump as usize];
            reader
                .read_exact(&mut dump)
                .context("交接文件中的屏幕内容不完整")?;
            dumps.push(dump);
        }
        Ok((handoff, dumps))
    });
    let _ = std::fs::remove_file(path);
    read
}

/// 把上一个程序留下的终端会话接过来。返回接过来几个。
fn adopt(terms: &Registry, handoff: Handoff, dumps: Vec<Vec<u8>>, host_token: &str) -> usize {
    terms.set_next(handoff.next_term);
    let mut adopted = 0;
    for (session, dump) in handoff.sessions.into_iter().zip(dumps) {
        let exit = session.exited.then_some(session.exit);
        let backend: Box<dyn Backend> = if exit.is_some() {
            Box::new(Ended)
        } else {
            match Hosted::adopt(session.port, session.pid, host_token) {
                Ok(hosted) => Box::new(hosted),
                Err(err) => {
                    log(&format!("无法接管终端 {}：{err:#}", session.id));
                    continue;
                }
            }
        };
        terms.adopt(
            Frozen {
                id: session.id,
                key: session.key,
                cwd: session.cwd,
                created: session.created,
                size: Size::new(session.cols, session.rows),
                handle: None,
                exit,
                dump,
                marks: session.marks,
                quiet: session.quiet_ms.map(Duration::from_millis),
                work: session.work_ms.map(Duration::from_millis),
            },
            backend,
        );
        adopted += 1;
    }
    adopted
}

/// `kuaid --installed`：刚装好这一版时调用，删掉 `~/.kuai/` 下别的版本的目录（名字以数字开头的），
/// 留下自己这一版和常驻进程正在用的那一版。还有程序在用的（上一版留下的终端托管进程）删不掉，
/// 留到下一次。
pub fn clean_old_versions() {
    let root = crate::dirs::kuai_root();
    let mine = crate::build();
    if mine.is_empty() {
        return;
    }
    let current = std::fs::read_to_string(root.join("run").join("current")).unwrap_or_default();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let version = name.starts_with(|c: char| c.is_ascii_digit());
        if version && name != mine && name != current.trim() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// 往日志里写一行（常驻进程的标准错误接在日志文件上）。
pub fn log(text: &str) {
    let secs = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    eprintln!("[{secs}] {text}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 交接文件新程序读得回来() {
        let dir = std::env::temp_dir().join(format!("kuaid-handoff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("handoff");
        let frozen = vec![
            Frozen {
                id: "t3".into(),
                key: "c:/repo".into(),
                cwd: "C:/repo".into(),
                created: 1_700_000_000,
                size: Size::new(100, 30),
                handle: Some((50123, 4321)),
                exit: None,
                dump: b"\x1b[mhello".to_vec(),
                marks: vec![TermMark {
                    text: "git status".into(),
                    at: 1_700_000_000_123,
                }],
                quiet: Some(Duration::from_millis(12_345)),
                work: Some(Duration::from_millis(60_000)),
            },
            Frozen {
                id: "t4".into(),
                key: "c:/other".into(),
                cwd: "C:/other".into(),
                created: 1_700_000_100,
                size: Size::new(80, 24),
                handle: None,
                exit: Some(Some(2)),
                dump: b"bye".to_vec(),
                marks: Vec::new(),
                quiet: None,
                work: None,
            },
        ];
        write_handoff(&path, "secret", 7, &frozen).unwrap();
        let (handoff, dumps) = read_handoff(&path).unwrap();
        assert!(!path.exists(), "读完要删掉");
        assert_eq!((handoff.format, handoff.next_term), (1, 7));
        assert_eq!(handoff.host_token, "secret");
        let [live, ended] = handoff.sessions.as_slice() else {
            panic!("应该有两个会话");
        };
        assert_eq!((live.port, live.pid, live.exited), (50123, 4321, false));
        assert_eq!((live.cols, live.rows), (100, 30));
        assert_eq!(live.marks, frozen[0].marks, "发出去的消息带过去");
        assert_eq!(live.quiet_ms, Some(12_345), "屏幕多久没变带过去");
        assert_eq!(live.work_ms, Some(60_000), "多久没干活带过去");
        assert_eq!((ended.port, ended.exited, ended.exit), (-1, true, Some(2)));
        assert_eq!((ended.quiet_ms, ended.work_ms), (None, None));
        assert_eq!(dumps, [b"\x1b[mhello".to_vec(), b"bye".to_vec()]);
        // 以后的版本多写了字段、少写了后加的字段，都读得了
        let future =
            r#"{"format":2,"added":true,"sessions":[{"id":"t1","cols":80,"rows":24,"x":1}]}"#;
        let handoff: Handoff = serde_json::from_str(future).unwrap();
        assert_eq!(handoff.sessions[0].port, -1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
