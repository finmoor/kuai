//! 伪终端：在里面开登录 shell，读它的输出、写它的输入、改大小、结束它。只有 Linux 上有。
//!
//! 每个会话两个线程：一个读输出交给会话，顺便看 shell 结束了没有；一个把用户敲的字写进去
//! （写可能卡住：程序不读输入时缓冲区会满，不能让它卡住处理请求的线程）。

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock, RwLockWriteGuard};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use super::{Backend, Session, Size, Spawn};

/// 没有输出时隔这么久醒一次：看 shell 结束了没有、同步刷新到没到时间
const TICK_MS: i32 = 500;
/// 让程序结束（挂断）以后等这么久，还不退就强杀
const KILL_GRACE: Duration = Duration::from_secs(2);

/// 读输出的线程读之前都要拿这把锁（读的那一头）。升级换程序前拿住写的那一头：
/// 之后程序的输出都留在伪终端里，新程序接着读，一个字节也不丢。
static GATE: RwLock<()> = RwLock::new(());

/// 让所有会话停止读输出，直到返回值被丢弃。
pub fn freeze() -> RwLockWriteGuard<'static, ()> {
    GATE.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct Pty {
    /// 伪终端的主端；程序结束后放掉（关掉它，还开着从端的后台程序会收到挂断）
    master: Arc<Mutex<Option<Arc<File>>>>,
    pid: i32,
    input: Sender<Option<Vec<u8>>>,
    /// 写线程要用的那一头，`start` 时取走
    input_rx: Mutex<Option<Receiver<Option<Vec<u8>>>>>,
    closing: Arc<AtomicBool>,
    /// 挂断时的前台进程组（见 [`Reader::finish_group`]）
    hung_group: Arc<AtomicI32>,
}

impl Pty {
    /// 开一个伪终端，在里面启动登录 shell。
    pub fn spawn(spawn: &Spawn) -> Result<Pty> {
        let master = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open("/dev/ptmx")
            .context("无法打开 /dev/ptmx")?;
        let fd = master.as_raw_fd();
        let mut name = [0 as libc::c_char; 128];
        // SAFETY: fd 是刚打开的伪终端主端；name 的长度如实给了
        let ready = unsafe {
            libc::grantpt(fd) == 0
                && libc::unlockpt(fd) == 0
                && libc::ptsname_r(fd, name.as_mut_ptr(), name.len()) == 0
        };
        if !ready {
            return Err(std::io::Error::last_os_error()).context("伪终端不可用");
        }
        // SAFETY: ptsname_r 成功后 name 里是以 0 结尾的字符串
        let slave_path = unsafe { CStr::from_ptr(name.as_ptr()) }
            .to_str()
            .context("伪终端名称不是 UTF-8")?
            .to_string();
        set_size(fd, spawn.size);
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(&slave_path)
            .with_context(|| format!("无法打开 {slave_path}"))?;

        let shell = login_shell();
        let name = shell.rsplit('/').next().unwrap_or("sh");
        let mut cmd = Command::new(&shell);
        // 名字前面加 - ：shell 按登录 shell 启动，会读 profile
        cmd.arg0(format!("-{name}"))
            .current_dir(&spawn.cwd)
            .env_clear()
            .envs(shell_env(&shell, &spawn.env))
            // 里面的程序看得出自己在 Kuai 的哪个终端里
            .env("KUAI_TERM", &spawn.term)
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: 闭包在 fork 之后、exec 之前的子进程里跑，只做系统调用
        unsafe {
            cmd.pre_exec(|| {
                // 自己一个会话，把伪终端（这时已经是标准输入）设成控制终端
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().with_context(|| format!("无法启动 {shell}"))?;
        // 不留 Child：结束没结束自己按进程号查（升级换程序后也只剩进程号）
        Ok(Pty::new(master, child.id() as i32))
    }

    /// 升级换程序后：接着管带过来的伪终端。
    pub fn adopt(fd: RawFd, pid: i32) -> Result<Pty> {
        // SAFETY: fcntl 只是查这个描述符在不在
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(std::io::Error::last_os_error()).context("接管的伪终端已不存在");
        }
        set_cloexec(fd, true);
        // SAFETY: 这个描述符是上一个程序专门留给我们的，没有别人用
        let master = unsafe { File::from_raw_fd(fd) };
        Ok(Pty::new(master, pid))
    }

    fn new(master: File, pid: i32) -> Pty {
        let (input, input_rx) = mpsc::channel();
        Pty {
            master: Arc::new(Mutex::new(Some(Arc::new(master)))),
            pid,
            input,
            input_rx: Mutex::new(Some(input_rx)),
            closing: Arc::default(),
            hung_group: Arc::default(),
        }
    }

    fn master(&self) -> Option<Arc<File>> {
        self.master.lock().unwrap().clone()
    }
}

impl Backend for Pty {
    fn start(&self, session: Arc<Session>) {
        let (Some(master), Some(input_rx)) = (self.master(), self.input_rx.lock().unwrap().take())
        else {
            return;
        };
        let writer_master = master.clone();
        let _ = std::thread::Builder::new()
            .name("kuaid-pty-in".into())
            .spawn(move || write_loop(&writer_master, &input_rx));
        let reader = Reader {
            session: session.clone(),
            master,
            slot: self.master.clone(),
            pid: self.pid,
            closing: self.closing.clone(),
            hung_group: self.hung_group.clone(),
            input: self.input.clone(),
        };
        let spawned = std::thread::Builder::new()
            .name("kuaid-pty-out".into())
            .spawn(move || reader.run());
        if spawned.is_err() {
            session.finish(None);
        }
    }

    fn write(&self, bytes: Vec<u8>) {
        let _ = self.input.send(Some(bytes));
    }

    fn resize(&self, size: Size) {
        if let Some(master) = self.master() {
            set_size(master.as_raw_fd(), size);
        }
    }

    fn kill(&self) {
        // 先挂断；读线程看到这个标记，过一会儿程序还没退就强杀
        if let Some(master) = self.master() {
            // 记下这时的前台进程组：shell 一收到挂断就退了的话，之后就问不到了
            if let Some(group) = foreground_group(master.as_raw_fd()) {
                self.hung_group.store(group, Ordering::Release);
            }
            signal(master.as_raw_fd(), self.pid, libc::SIGHUP);
        }
        self.closing.store(true, Ordering::Release);
    }

    fn programs(&self, _: &super::programs::Procs) -> Vec<String> {
        self.master()
            .map(|master| super::programs::foreground(master.as_raw_fd(), self.pid))
            .unwrap_or_default()
    }

    fn handle(&self) -> Option<(i32, i32)> {
        self.master().map(|master| (master.as_raw_fd(), self.pid))
    }
}

struct Reader {
    session: Arc<Session>,
    master: Arc<File>,
    slot: Arc<Mutex<Option<Arc<File>>>>,
    pid: i32,
    closing: Arc<AtomicBool>,
    hung_group: Arc<AtomicI32>,
    input: Sender<Option<Vec<u8>>>,
}

impl Reader {
    fn run(self) {
        let fd = self.master.as_raw_fd();
        let mut buf = vec![0u8; 64 * 1024];
        let mut exit: Option<Option<i32>> = None;
        let mut closing_since: Option<Instant> = None;
        loop {
            if self.closing.load(Ordering::Acquire) {
                let since = *closing_since.get_or_insert_with(Instant::now);
                if since.elapsed() > KILL_GRACE {
                    signal(fd, self.pid, libc::SIGKILL);
                    kill_group(self.hung_group.load(Ordering::Acquire), libc::SIGKILL);
                    break;
                }
            }
            let timeout = if closing_since.is_some() {
                100
            } else {
                TICK_MS
            };
            match wait_readable(fd, timeout) {
                Ready::Data => {
                    if !self.read_once(&mut buf) {
                        break;
                    }
                }
                Ready::Nothing => {
                    self.session.tick();
                    if let Some(code) = try_wait(self.pid) {
                        exit = Some(code);
                        // shell 退出前最后写的那些
                        while matches!(wait_readable(fd, 0), Ready::Data)
                            && self.read_once(&mut buf)
                        {}
                        break;
                    }
                }
                Ready::Closed => break,
            }
        }
        if self.closing.load(Ordering::Acquire) {
            self.finish_group(closing_since.unwrap_or_else(Instant::now));
        }
        let code = exit.unwrap_or_else(|| wait_exit(self.pid));
        // 放掉主端：写线程退出后它就关了
        *self.slot.lock().unwrap() = None;
        let _ = self.input.send(None);
        drop(self.master);
        self.session.finish(code);
    }

    /// 叫停时 shell 先退了：挂断时的前台程序还在（不理挂断的）就等它，过了宽限强杀；等完才算会话
    /// 结束，叫停的地方（`close_all`）也就等到它没了。原来 shell 一退读线程就结束，没人强杀它，
    /// 常驻进程退出以后它一直留着。
    fn finish_group(&self, since: Instant) {
        let group = self.hung_group.load(Ordering::Acquire);
        while group_alive(group) {
            if since.elapsed() > KILL_GRACE {
                kill_group(group, libc::SIGKILL);
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 读一块输出交给会话。返回 false：对面都关了，没有可读的了。
    fn read_once(&self, buf: &mut [u8]) -> bool {
        let _gate = GATE.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        match (&*self.master).read(buf) {
            Ok(0) => false,
            Ok(n) => {
                self.session.output(&buf[..n]);
                true
            }
            Err(err) if matches!(err.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => {
                true
            }
            // 从端都关了（shell 和它的子进程都退了）读到的是 EIO
            Err(_) => false,
        }
    }
}

enum Ready {
    Data,
    Nothing,
    Closed,
}

fn wait_readable(fd: RawFd, timeout_ms: i32) -> Ready {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: 只给了一个 pollfd，数量如实
    let ready = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
    if ready < 0 {
        let interrupted = std::io::Error::last_os_error().kind() == ErrorKind::Interrupted;
        return if interrupted {
            Ready::Nothing
        } else {
            Ready::Closed
        };
    }
    if ready == 0 {
        Ready::Nothing
    } else if poll.revents & libc::POLLIN != 0 {
        // 挂断的同时可能还有没读的，先读
        Ready::Data
    } else {
        Ready::Closed
    }
}

fn write_loop(master: &File, input: &Receiver<Option<Vec<u8>>>) {
    let mut master = master;
    while let Ok(Some(bytes)) = input.recv() {
        if master.write_all(&bytes).is_err() {
            break;
        }
    }
}

/// shell 结束了没有：`None` 还在跑，`Some(退出码)` 结束了（被信号杀掉的没有退出码）。
fn try_wait(pid: i32) -> Option<Option<i32>> {
    let mut status = 0;
    // SAFETY: 只查我们自己启动的那个进程，不等
    let got = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if got == pid {
        return Some(libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status)));
    }
    if got == 0 {
        return None;
    }
    // 查不了（不是我们的子进程了）：看进程还在不在
    // SAFETY: 信号 0 不发信号，只检查
    let gone = unsafe { libc::kill(pid, 0) } != 0;
    gone.then_some(None)
}

/// 伪终端已经没人用了，等 shell 真的退出，拿它的退出码。等不到就算了。
fn wait_exit(pid: i32) -> Option<i32> {
    let deadline = Instant::now() + KILL_GRACE;
    while Instant::now() < deadline {
        if let Some(code) = try_wait(pid) {
            return code;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // 它以后退出时收一下，免得留下僵尸进程
    let _ = std::thread::Builder::new()
        .name("kuaid-reap".into())
        .spawn(move || {
            let mut status = 0;
            // SAFETY: 只等我们自己启动的那个进程
            unsafe { libc::waitpid(pid, &mut status, 0) };
        });
    None
}

/// 伪终端现在的前台进程组。
fn foreground_group(fd: RawFd) -> Option<libc::pid_t> {
    let mut group: libc::pid_t = 0;
    // SAFETY: TIOCGPGRP 往 group 里写一个进程组号
    let got = unsafe { libc::ioctl(fd, libc::TIOCGPGRP as _, &mut group) } == 0;
    (got && group > 1).then_some(group)
}

/// 进程组里还有进程。
fn group_alive(group: libc::pid_t) -> bool {
    // SAFETY: 信号 0 不发信号，只检查
    group > 1 && unsafe { libc::kill(-group, 0) } == 0
}

fn kill_group(group: libc::pid_t, signal: libc::c_int) {
    if group > 1 {
        // SAFETY: 这个进程组是这个伪终端挂断时的前台那一组
        unsafe { libc::kill(-group, signal) };
    }
}

/// 给终端里的程序发信号：前台的那一组、shell 自己那一组、shell 本身。
fn signal(fd: RawFd, pid: i32, signal: libc::c_int) {
    if let Some(group) = foreground_group(fd) {
        kill_group(group, signal);
    }
    // SAFETY: 进程号都是这个伪终端里的；发信号没有内存上的事
    unsafe {
        libc::kill(-pid, signal);
        libc::kill(pid, signal);
        if signal == libc::SIGHUP {
            // 停着的作业收到挂断也不会动，让它们继续才会处理
            libc::kill(-pid, libc::SIGCONT);
        }
    }
}

fn set_size(fd: RawFd, size: Size) {
    let size = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: fd 是伪终端的主端，size 是完整的 winsize
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &size) };
}

/// 换程序（exec）时这个描述符关不关。
pub fn set_cloexec(fd: RawFd, on: bool) {
    let flags = if on { libc::FD_CLOEXEC } else { 0 };
    // SAFETY: 只改这个描述符的标记
    unsafe { libc::fcntl(fd, libc::F_SETFD, flags) };
}

/// 用户的登录 shell：环境变量里的（sshd 按账号设的），没有就用 sh。
fn login_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| shell.starts_with('/'))
        .unwrap_or_else(|| "/bin/sh".into())
}

/// shell 的环境变量：kuaid 自己的那些，去掉只属于某一条 ssh 连接的和 kuaid 内部用的，
/// 再加上终端的类型和调用方给的。
fn shell_env(shell: &str, extra: &[(String, String)]) -> Vec<(String, String)> {
    let internal = |key: &str| {
        key.starts_with("SSH_")
            || key.starts_with("FR_")
            || matches!(key, "GIT_ASKPASS" | "TERM" | "COLORTERM" | "SHELL")
    };
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| !internal(key))
        .collect();
    env.extend(
        [
            ("SHELL", shell),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("TERM_PROGRAM", "Kuai"),
            ("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION")),
        ]
        .map(|(key, value)| (key.to_string(), value.to_string())),
    );
    env.extend(extra.iter().cloned());
    env
}

/// 给 [`super::Registry`] 用的：开一个真的伪终端。
pub fn spawner() -> super::Spawner {
    Box::new(|spawn| Ok(Box::new(Pty::spawn(spawn)?) as Box<dyn Backend>))
}
