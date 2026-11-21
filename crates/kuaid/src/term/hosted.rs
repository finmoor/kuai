//! Windows 上的伪终端那一层：终端开在托管进程（[`super::host`]）里，这里经本机的一条连接和它说话。
//!
//! 每个会话一个线程读托管进程发来的帧（程序的输出、退出码）。常驻进程升级时先让托管进程停下
//! （[`Backend::pause`]），等它回「停好了」：这之前的输出都进了屏幕，这之后的留在托管进程手里，
//! 新的常驻进程连上去接着收，一个字节也不丢。

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

use super::host::{Launch, Launched, answer_path, wire};
use super::{Backend, Session, Size, Spawn};

/// 没有输出时隔这么久醒一次：同步刷新到没到时间
const TICK: Duration = Duration::from_millis(500);
/// 让托管进程停下，最多等这么久
const PAUSE_WAIT: Duration = Duration::from_secs(3);
/// 托管进程起来、开好终端，最多等这么久
const LAUNCH_WAIT: Duration = Duration::from_secs(15);

pub struct Hosted {
    /// 托管进程听的端口、shell 的进程号：升级时带给新的常驻进程
    port: u16,
    pid: u32,
    /// 往托管进程发帧用的那一头
    writer: Mutex<TcpStream>,
    /// 读线程要用的那一头，`start` 时取走
    reader: Mutex<Option<TcpStream>>,
    /// 读线程收到「停好了」时往这里放一下
    paused_tx: Sender<()>,
    paused_rx: Mutex<Receiver<()>>,
}

impl Hosted {
    /// 起一个托管进程，在里面开终端。`token` 是连它要交的口令。
    pub fn spawn(spawn: &Spawn, token: &str) -> Result<Hosted> {
        let exe = std::env::current_exe().context("无法确定 kuaid 的安装位置")?;
        let launch = Launch {
            token: token.to_string(),
            program: match spawn.shell.is_empty() {
                true => default_shell(),
                false => spawn.shell.clone(),
            },
            cwd: spawn.cwd.clone(),
            cols: spawn.size.cols,
            rows: spawn.size.rows,
            env: shell_env(&spawn.env, &spawn.term),
        };
        // 启动参数放在运行目录的一个文件里（托管进程不继承我们的句柄，见 `crate::win`），它读完就删
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let run = crate::dirs::kuai_root().join("run");
        std::fs::create_dir_all(&run).with_context(|| format!("无法创建 {}", run.display()))?;
        let launch_path = run.join(format!(
            "host-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let answer = answer_path(&launch_path);
        let _ = std::fs::remove_file(&answer);
        std::fs::write(&launch_path, serde_json::to_string(&launch)?)
            .context("无法把启动参数交给托管进程")?;
        let launched =
            crate::win::spawn_detached(&exe, &["--pty-host", &launch_path.to_string_lossy()])
                .with_context(|| format!("无法启动 {}", exe.display()))
                .and_then(|mut child| {
                    let deadline = Instant::now() + LAUNCH_WAIT;
                    loop {
                        if let Ok(text) = std::fs::read_to_string(&answer) {
                            return serde_json::from_str::<Launched>(&text)
                                .context("托管进程启动失败");
                        }
                        let gone = matches!(child.try_wait(), Ok(Some(())));
                        // 退出前可能刚写完回答：再看一眼
                        if gone && !answer.exists() {
                            bail!("终端托管进程启动失败，详见 ~/.kuai/run/kuaid.log");
                        }
                        if Instant::now() >= deadline {
                            bail!("终端托管进程没有回答");
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                });
        let _ = std::fs::remove_file(&launch_path);
        let _ = std::fs::remove_file(&answer);
        let launched = launched?;
        if let Some(err) = launched.error {
            bail!("{err}");
        }
        Hosted::connect(launched.port, launched.pid, token)
    }

    /// 升级换程序后：接着管上一版留下的托管进程。
    pub fn adopt(port: i32, pid: i32, token: &str) -> Result<Hosted> {
        let port = u16::try_from(port).context("托管进程的端口无效")?;
        Hosted::connect(port, pid as u32, token)
    }

    fn connect(port: u16, pid: u32, token: &str) -> Result<Hosted> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).context("无法连接终端托管进程")?;
        let _ = stream.set_nodelay(true);
        stream
            .write_all(wire::greeting(token).as_bytes())
            .context("无法连接终端托管进程")?;
        let reader = stream.try_clone().context("连接不可用")?;
        let (paused_tx, paused_rx) = mpsc::channel();
        Ok(Hosted {
            port,
            pid,
            writer: Mutex::new(stream),
            reader: Mutex::new(Some(reader)),
            paused_tx,
            paused_rx: Mutex::new(paused_rx),
        })
    }

    fn send(&self, kind: u8, payload: &[u8]) -> bool {
        wire::write_frame(&mut *self.writer.lock().unwrap(), kind, payload).is_ok()
    }
}

impl Backend for Hosted {
    fn start(&self, session: Arc<Session>) {
        let Some(reader) = self.reader.lock().unwrap().take() else {
            return;
        };
        let paused = self.paused_tx.clone();
        let finisher = session.clone();
        let spawned = std::thread::Builder::new()
            .name("kuaid-pty-out".into())
            .spawn(move || {
                let code = read_loop(reader, &session, &paused);
                session.finish(code);
            });
        if spawned.is_err() {
            finisher.finish(None);
        }
    }

    fn write(&self, bytes: Vec<u8>) {
        // 一帧有长度上限：贴进来一大段时分开发
        for part in bytes.chunks(wire::MAX_FRAME / 2) {
            if !self.send(wire::INPUT, part) {
                return;
            }
        }
    }

    fn resize(&self, size: Size) {
        let mut payload = Vec::with_capacity(4);
        payload.extend(size.cols.to_le_bytes());
        payload.extend(size.rows.to_le_bytes());
        self.send(wire::RESIZE, &payload);
    }

    fn kill(&self) {
        self.send(wire::KILL, &[]);
    }

    fn programs(&self, procs: &super::programs::Procs) -> Vec<String> {
        super::programs::children(procs, self.pid)
    }

    fn handle(&self) -> Option<(i32, i32)> {
        Some((i32::from(self.port), self.pid as i32))
    }

    fn pause(&self) -> bool {
        // 读线程没起来的不用等
        if self.reader.lock().unwrap().is_some() {
            return true;
        }
        let paused = self.paused_rx.lock().unwrap();
        while paused.try_recv().is_ok() {}
        if !self.send(wire::PAUSE, &[]) {
            return false;
        }
        match paused.recv_timeout(PAUSE_WAIT) {
            Ok(()) => true,
            // 读线程已经结束（程序退出了）：没有再要收的输出
            Err(mpsc::RecvTimeoutError::Disconnected) => true,
            Err(mpsc::RecvTimeoutError::Timeout) => false,
        }
    }

    fn resume(&self) {
        self.send(wire::RESUME, &[]);
    }
}

impl Drop for Hosted {
    fn drop(&mut self) {
        // 读线程拿着另一头：关掉连接它才会退出
        let _ = self.writer.lock().unwrap().shutdown(Shutdown::Both);
    }
}

/// 读托管进程发来的帧，直到程序结束或者连接断了。返回退出码。
fn read_loop(mut reader: TcpStream, session: &Session, paused: &Sender<()>) -> Option<i32> {
    let _ = reader.set_read_timeout(Some(TICK));
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // 攒够一帧就处理
        while pending.len() >= 5 {
            let len = u32::from_le_bytes([pending[1], pending[2], pending[3], pending[4]]) as usize;
            if len > wire::MAX_FRAME {
                return None;
            }
            if pending.len() < 5 + len {
                break;
            }
            let kind = pending[0];
            let payload = &pending[5..5 + len];
            match kind {
                wire::OUTPUT => session.output(payload),
                wire::EXIT => return wire::parse_exit(payload),
                wire::PAUSED => {
                    let _ = paused.send(());
                }
                _ => {}
            }
            pending.drain(..5 + len);
        }
        match reader.read(&mut buf) {
            // 托管进程没了（被杀掉、崩了）：里面的程序也没了
            Ok(0) => return None,
            Ok(n) => pending.extend_from_slice(&buf[..n]),
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                session.tick();
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// 终端里开哪个 shell：sshd 配的默认 shell（ssh 登录上来用的就是它），没配就依次找
/// PowerShell 7、Windows PowerShell、命令提示符。
fn default_shell() -> Vec<String> {
    let program = sshd_default_shell()
        .filter(|path| std::path::Path::new(path).is_file())
        .or_else(|| find_in_path("pwsh.exe"))
        .or_else(|| find_in_path("powershell.exe"))
        .or_else(|| std::env::var("COMSPEC").ok())
        .unwrap_or_else(|| "cmd.exe".into());
    let name = program
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or_default()
        .to_lowercase();
    match name.as_str() {
        "pwsh.exe" | "powershell.exe" => vec![program, "-NoLogo".into()],
        "bash.exe" => vec![program, "--login".into(), "-i".into()],
        _ => vec![program],
    }
}

fn find_in_path(name: &str) -> Option<String> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
}

/// 注册表里 `HKLM\SOFTWARE\OpenSSH` 的 `DefaultShell`。
fn sshd_default_shell() -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain([0]).collect() };
    let (key, value) = (wide(r"SOFTWARE\OpenSSH"), wide("DefaultShell"));
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: 缓冲区和它的大小如实给了，两个名字都以 0 结尾
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != 0 {
        return None;
    }
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    let shell = String::from_utf16_lossy(&buf[..len]).trim().to_string();
    (!shell.is_empty()).then_some(shell)
}

/// shell 的环境变量：kuaid 自己的那些，去掉只属于某一条 ssh 连接的和 kuaid 内部用的，
/// 换上系统里现在的值（常驻进程启动以后改的 PATH 之类，新开的终端看得到），再加上终端的类型和
/// 调用方给的。
fn shell_env(extra: &[(String, String)], term: &str) -> Vec<(String, String)> {
    let internal = |key: &str| {
        let key = key.to_uppercase();
        key.starts_with("SSH_")
            || key.starts_with("FR_")
            || matches!(key.as_str(), "GIT_ASKPASS" | "TERM" | "COLORTERM")
    };
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| !internal(key))
        .collect();
    // Windows 的环境变量名不分大小写（`Path` 和 `PATH` 是同一个）
    for (key, value) in crate::win::fresh_env().unwrap_or_default() {
        if internal(&key) {
            continue;
        }
        match env
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case(&key))
        {
            Some(slot) => slot.1 = value,
            None => env.push((key, value)),
        }
    }
    env.extend(
        [
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("TERM_PROGRAM", "Kuai"),
            ("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION")),
            // 里面的程序看得出自己在 Kuai 的哪个终端里
            ("KUAI_TERM", term),
        ]
        .map(|(key, value)| (key.to_string(), value.to_string())),
    );
    env.extend(extra.iter().cloned());
    env
}

/// 给 [`super::Registry`] 用的：每个终端起一个托管进程。
pub fn spawner(token: String) -> super::Spawner {
    Box::new(move |spawn| Ok(Box::new(Hosted::spawn(spawn, &token)?) as Box<dyn Backend>))
}
