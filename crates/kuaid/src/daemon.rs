//! 常驻进程：听 `~/.kuai/run/kuaid.sock`，每个连上来的客户端一个线程。
//!
//! 同一个账号下只跑一个（靠 `kuaid.lock` 上的文件锁）。不开网络端口：套接字文件只有本人能连，
//! 客户端经 ssh 登录后由转接程序（`kuaid --connect`）接进来。客户端都断开、终端会话也都没了，
//! 过 [`IDLE`] 这么久自己退出；服务器重启后不会自己起来，下次有客户端连接时再由转接程序拉起。
//!
//! 升级：新版本的转接程序发现常驻进程比自己旧，就让它原地换成新程序（`exec`）。进程号不变，
//! 终端里的 shell 还是它的子进程，伪终端、监听的套接字、文件锁这几个文件描述符留着不关，
//! 每个终端的屏幕拍下来写进一个交接文件，新程序读了接着管。连着的客户端会断开，自己重连。
//! 交接文件的格式只加字段：永远是旧程序写、新程序读。

use std::fs::{File, OpenOptions};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::conn::{self, Control, Shared};
use crate::files;
use crate::term::pty::{self, Pty};
use crate::term::{Backend, Ended, Frozen, Registry, Size};

/// 没有客户端、没有终端会话这么久就退出
const IDLE: Duration = Duration::from_secs(600);
/// 隔这么久看一次要不要退出
const IDLE_CHECK: Duration = Duration::from_secs(5);
/// 隔这么久清一次客户端传上来的过期文件
const SWEEP_EVERY: Duration = Duration::from_secs(3600);
/// 交接文件的格式版本
const HANDOFF_FORMAT: u32 = 1;

/// `~/.kuai/run` 里的几个文件。
pub struct Paths {
    pub run: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Paths> {
        let home = std::env::var_os("HOME").context("未设置 HOME 环境变量")?;
        Ok(Paths {
            run: PathBuf::from(home).join(".kuai").join("run"),
        })
    }

    /// 建好运行目录，只有本人能进。
    pub fn create(&self) -> Result<()> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.run)
            .with_context(|| format!("无法创建 {}", self.run.display()))?;
        // 已经有的话把权限收紧（目录可能是别的版本建的）
        let _ = std::fs::set_permissions(&self.run, std::fs::Permissions::from_mode(0o700));
        Ok(())
    }

    pub fn socket(&self) -> PathBuf {
        self.run.join("kuaid.sock")
    }

    fn lock(&self) -> PathBuf {
        self.run.join("kuaid.lock")
    }

    /// 常驻进程写的日志
    pub fn log(&self) -> PathBuf {
        self.run.join("kuaid.log")
    }

    /// 里面写着常驻进程现在用的是哪个版本目录：上传新版本时不删它
    fn current(&self) -> PathBuf {
        self.run.join("current")
    }

    fn handoff(&self) -> PathBuf {
        self.run.join("handoff")
    }

    /// 指向最近一条连接转发过来的 ssh-agent 套接字的符号链接
    fn auth_sock(&self) -> PathBuf {
        self.run.join("auth.sock")
    }

    /// 退出前删掉只在跑着的时候有用的文件。锁文件和日志留着。
    fn clear(&self) {
        for path in [
            self.socket(),
            self.current(),
            self.auth_sock(),
            fr_core::askpass::socket_path(&self.run),
        ] {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 交接文件开头的那一行 JSON；后面依次跟着每个会话还原屏幕用的字节。
#[derive(Serialize, Deserialize, Default)]
struct Handoff {
    format: u32,
    /// 监听套接字的文件描述符
    listener: RawFd,
    /// 文件锁的文件描述符
    lock: RawFd,
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
    /// 伪终端的文件描述符和 shell 的进程号；程序已经结束的是 -1
    #[serde(default = "minus_one")]
    fd: RawFd,
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
}

fn minus_one() -> i32 {
    -1
}

struct Daemon {
    paths: Paths,
    listener: RawFd,
    lock: RawFd,
    upgrading: AtomicBool,
}

impl Control for Daemon {
    fn upgrade(&self, shared: &Shared, exe: &str) -> anyhow::Error {
        let exe = Path::new(exe);
        let ours = exe.is_absolute()
            && exe.file_name().is_some_and(|name| name == "kuaid")
            && exe.is_file();
        if !ours {
            return anyhow!("{} 不是 kuaid 程序", exe.display());
        }
        if self.upgrading.swap(true, Ordering::AcqRel) {
            return anyhow!("正在升级");
        }
        log(&format!("升级：替换为 {}", exe.display()));
        shared.kill_git();
        // 从这里起不再读终端的输出：程序写的东西留在伪终端里，新程序接着读
        let gate = pty::freeze();
        let (next_term, frozen) = shared.terms.freeze();
        let mut kept = vec![self.listener, self.lock];
        kept.extend(frozen.iter().filter_map(|f| f.handle.map(|(fd, _)| fd)));
        let path = self.paths.handoff();
        let failed = match write_handoff(&path, self.listener, self.lock, next_term, &frozen) {
            Ok(()) => {
                for fd in &kept {
                    pty::set_cloexec(*fd, false);
                }
                // 成功的话到这里为止，后面是新程序了
                let err = Command::new(exe)
                    .arg("--daemon")
                    .arg("--resume")
                    .arg(&path)
                    .exec();
                for fd in &kept {
                    pty::set_cloexec(*fd, true);
                }
                anyhow!(err).context(format!("无法启动 {}", exe.display()))
            }
            Err(err) => err,
        };
        let _ = std::fs::remove_file(&path);
        drop(gate);
        self.upgrading.store(false, Ordering::Release);
        log(&format!("升级失败：{failed:#}"));
        failed
    }

    fn shutdown(&self, shared: &Shared) -> ! {
        log("收到退出请求");
        shared.terms.close_all();
        // 等终端里的程序收到挂断退出
        let deadline = Instant::now() + Duration::from_secs(3);
        while shared.terms.alive() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        self.exit();
    }

    fn auth_sock(&self, path: &str) -> Option<String> {
        // 终端里的 shell 会比这条 ssh 连接活得久：给它一个固定的地址，每次有连接带着转发来就改指过去
        let link = self.paths.auth_sock();
        let temp = self.paths.run.join(format!("auth.sock.{}", std::process::id()));
        let _ = std::fs::remove_file(&temp);
        std::os::unix::fs::symlink(path, &temp).ok()?;
        std::fs::rename(&temp, &link).ok()?;
        Some(link.to_string_lossy().into_owned())
    }
}

impl Daemon {
    fn exit(&self) -> ! {
        self.paths.clear();
        log("退出");
        std::process::exit(0);
    }
}

/// 常驻进程的入口。`resume` 是交接文件：有的话是升级换过来的，接着管原来的东西。
pub fn run(resume: Option<&str>) -> Result<()> {
    // 这些是拉起我们的那条 ssh 连接留下的，连接一断就不对了；每条连接的由转接程序另外报告。
    // SAFETY: 这时还没有开别的线程
    unsafe {
        for key in ["SSH_AUTH_SOCK", "SSH_CLIENT", "SSH_CONNECTION", "SSH_TTY"] {
            std::env::remove_var(key);
        }
    }
    let paths = Paths::new()?;
    paths.create()?;
    if let Some(home) = std::env::var_os("HOME") {
        // 不占着拉起我们时所在的目录
        let _ = std::env::set_current_dir(home);
    }
    let terms = Registry::new(pty::spawner());
    let (listener, lock, adopted) = match resume {
        Some(state) => resume_from(Path::new(state), &terms)?,
        None => match start_fresh(&paths)? {
            Some((listener, lock)) => (listener, lock, 0),
            // 已经有一个在跑了
            None => return Ok(()),
        },
    };
    let build = crate::build();
    let _ = std::fs::write(paths.current(), &build);
    log(&format!(
        "{}：kuaid {} {build}，进程号 {}，接管 {adopted} 个终端",
        if resume.is_some() { "升级完成" } else { "启动" },
        crate::VERSION,
        std::process::id(),
    ));

    let daemon = Daemon {
        paths: Paths::new()?,
        listener: listener.as_raw_fd(),
        lock: lock.as_raw_fd(),
        upgrading: AtomicBool::new(false),
    };
    let shared = Shared::new(terms, Some(Box::new(daemon)), build);
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

    // 文件锁跟着这个描述符，留到进程结束
    std::mem::forget(lock);
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                log(&format!("无法接受新连接：{err}"));
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        let shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("kuaid-conn".into())
            .spawn(move || match stream.try_clone() {
                Ok(reader) => conn::serve(&shared, reader, stream),
                Err(err) => log(&format!("新连接不可用：{err}")),
            });
        if let Err(err) = spawned {
            log(&format!("无法创建线程：{err}"));
        }
    }
    Ok(())
}

/// 头一回启动：拿文件锁、开监听。已经有别的常驻进程在跑时返回 None。
fn start_fresh(paths: &Paths) -> Result<Option<(UnixListener, File)>> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(paths.lock())
        .with_context(|| format!("无法打开 {}", paths.lock().display()))?;
    // SAFETY: lock 是刚打开的文件
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(None);
    }
    // 拿到锁了：套接字文件要是还在，是上一个进程没来得及删的
    let socket = paths.socket();
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("无法创建 {}", socket.display()))?;
    let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    Ok(Some((listener, lock)))
}

fn write_handoff(
    path: &Path,
    listener: RawFd,
    lock: RawFd,
    next_term: u64,
    frozen: &[Frozen],
) -> Result<()> {
    let handoff = Handoff {
        format: HANDOFF_FORMAT,
        listener,
        lock,
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
                fd: f.handle.map_or(-1, |(fd, _)| fd),
                pid: f.handle.map_or(-1, |(_, pid)| pid),
                exited: f.exit.is_some(),
                exit: f.exit.flatten(),
                dump: f.dump.len() as u64,
            })
            .collect(),
    };
    // 里面有屏幕上的内容，只有本人能读
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("无法写入 {}", path.display()))?;
    serde_json::to_writer(&mut file, &handoff)?;
    file.write_all(b"\n")?;
    for f in frozen {
        file.write_all(&f.dump)?;
    }
    file.sync_all()?;
    Ok(())
}

/// 读交接文件，把上一个程序留下的监听套接字、文件锁、终端会话接过来。返回接过来几个终端。
fn resume_from(path: &Path, terms: &Registry) -> Result<(UnixListener, File, usize)> {
    let file = File::open(path).with_context(|| format!("无法读取 {}", path.display()))?;
    let _ = std::fs::remove_file(path);
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let handoff: Handoff = serde_json::from_str(&line).context("交接文件格式错误")?;
    if handoff.format == 0 || handoff.listener < 0 || handoff.lock < 0 {
        bail!("交接文件不完整");
    }
    for fd in [handoff.listener, handoff.lock] {
        pty::set_cloexec(fd, true);
    }
    // SAFETY: 这两个描述符是上一个程序专门留给我们的
    let (listener, lock) = unsafe {
        (
            UnixListener::from_raw_fd(handoff.listener),
            File::from_raw_fd(handoff.lock),
        )
    };
    terms.set_next(handoff.next_term);
    let mut adopted = 0;
    for session in handoff.sessions {
        let mut dump = vec![0u8; session.dump as usize];
        reader
            .read_exact(&mut dump)
            .context("交接文件中的屏幕内容不完整")?;
        let exit = session.exited.then_some(session.exit);
        let backend: Box<dyn Backend> = if exit.is_some() {
            Box::new(Ended)
        } else {
            match Pty::adopt(session.fd, session.pid) {
                Ok(pty) => Box::new(pty),
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
            },
            backend,
        );
        adopted += 1;
    }
    Ok((listener, lock, adopted))
}

/// 往日志里写一行（常驻进程的标准错误接在日志文件上）。
pub fn log(text: &str) {
    let secs = std::time::SystemTime::now()
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
                key: "/repo".into(),
                cwd: "/repo".into(),
                created: 1_700_000_000,
                size: Size::new(100, 30),
                handle: Some((9, 4321)),
                exit: None,
                dump: b"\x1b[mhello".to_vec(),
            },
            Frozen {
                id: "t4".into(),
                key: "/other".into(),
                cwd: "/other".into(),
                created: 1_700_000_100,
                size: Size::new(80, 24),
                handle: None,
                exit: Some(Some(2)),
                dump: b"bye".to_vec(),
            },
        ];
        write_handoff(&path, 5, 6, 7, &frozen).unwrap();
        let text = std::fs::read(&path).unwrap();
        let end = text.iter().position(|b| *b == b'\n').unwrap();
        let handoff: Handoff = serde_json::from_slice(&text[..end]).unwrap();
        assert_eq!((handoff.format, handoff.listener, handoff.lock), (1, 5, 6));
        assert_eq!(handoff.next_term, 7);
        let [live, ended] = handoff.sessions.as_slice() else {
            panic!("应该有两个会话");
        };
        assert_eq!((live.fd, live.pid, live.exited), (9, 4321, false));
        assert_eq!((live.cols, live.rows, live.dump), (100, 30, 8));
        assert_eq!((ended.fd, ended.exited, ended.exit), (-1, true, Some(2)));
        assert_eq!(&text[end + 1..], b"\x1b[mhellobye");
        // 以后的版本多写了字段、少写了后加的字段，都读得了
        let future = r#"{"format":2,"listener":3,"lock":4,"added":true,"sessions":[{"id":"t1","cols":80,"rows":24,"x":1}]}"#;
        let handoff: Handoff = serde_json::from_str(future).unwrap();
        assert_eq!(handoff.sessions[0].fd, -1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
