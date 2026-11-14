//! 终端托管进程（`kuaid --pty-host`，只有 Windows 上有）：一个终端一个，拿着伪控制台和里面的 shell。
//!
//! 为什么要它：Linux 上常驻进程升级是原地换程序（`exec`），伪终端的文件描述符留着，终端不断。
//! Windows 没有 `exec`，常驻进程一换终端就没了。所以把伪控制台放在这个小进程里：常驻进程升级时
//! 只是和它断开，新的常驻进程再连上来，里面的程序不受影响。
//!
//! 它活得比常驻进程的这一版久，和常驻进程之间的约定（[`wire`]）只加不改。
//!
//! - 启动：它不继承常驻进程的任何句柄（见 `crate::win`），启动参数放在一个文件里
//!   （[`Launch`]，命令行上给路径，读完就删）；它开好伪控制台和监听口，把回答（[`Launched`]）
//!   写到旁边的 [`answer_path`] 里。
//! - 连接：只听 127.0.0.1 的一个随机端口，连上来先交口令（[`wire::greeting`]）。同一时间只认最新的
//!   那条连接。
//! - 输出：程序的输出一块块发给连着的常驻进程；没有连着的就留在手里等，不丢。常驻进程升级前发
//!   「先停一下」，它回「停好了」之后不再发，直到「接着发」或者新的连接。
//! - 没人管：超过 [`ORPHAN`] 没有常驻进程连着（它崩了），结束里面的程序，自己退出。

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use super::Size;
use super::conpty::Console;

/// 没有常驻进程连着这么久，就当它不会回来了
const ORPHAN: Duration = Duration::from_secs(60);
/// 让程序结束（关掉伪控制台）以后等这么久，还不退就强杀
const KILL_GRACE: Duration = Duration::from_secs(2);

/// 回答写在哪：启动参数的文件名后面加 `.out`。
pub fn answer_path(launch: &Path) -> PathBuf {
    let mut name = launch.as_os_str().to_owned();
    name.push(".out");
    PathBuf::from(name)
}

/// 常驻进程给托管进程的启动参数（一个文件，里面是 JSON）。
#[derive(Serialize, Deserialize)]
pub struct Launch {
    /// 连上来要交的口令
    pub token: String,
    /// 程序和参数
    pub program: Vec<String>,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    /// shell 的全部环境变量
    pub env: Vec<(String, String)>,
}

/// 托管进程的回答（一个文件，里面是 JSON）。
#[derive(Serialize, Deserialize, Default)]
pub struct Launched {
    /// 在 127.0.0.1 上听的端口
    #[serde(default)]
    pub port: u16,
    /// shell 的进程号
    #[serde(default)]
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 常驻进程和托管进程之间的约定。只加不改：托管进程是旧版本留下来的，新的常驻进程要认得它。
///
/// 连上以后常驻进程先发一行 [`greeting`]，之后两边都是一帧帧的：1 个字节的种类、
/// 4 个字节的长度（小端）、内容。不认得的种类跳过。
pub mod wire {
    use std::io::{Read, Write};

    /// 常驻进程 → 托管进程：用户敲的字
    pub const INPUT: u8 = 1;
    /// 改大小：列数、行数，各 2 个字节（小端）
    pub const RESIZE: u8 = 2;
    /// 结束里面的程序
    pub const KILL: u8 = 3;
    /// 先停一下：回 [`PAUSED`] 之后不再发输出
    pub const PAUSE: u8 = 4;
    /// 接着发
    pub const RESUME: u8 = 5;

    /// 托管进程 → 常驻进程：程序的输出
    pub const OUTPUT: u8 = 1;
    /// 程序结束了：1 个字节（有没有退出码）加 4 个字节的退出码（小端）
    pub const EXIT: u8 = 2;
    /// 停好了
    pub const PAUSED: u8 = 3;

    /// 一帧的内容最长这么多，再长就是对面乱了
    pub const MAX_FRAME: usize = 1 << 20;

    pub fn greeting(token: &str) -> String {
        format!("KUAI-PTY 1 {token}\n")
    }

    pub fn write_frame(to: &mut impl Write, kind: u8, payload: &[u8]) -> std::io::Result<()> {
        let mut frame = Vec::with_capacity(5 + payload.len());
        frame.push(kind);
        frame.extend((payload.len() as u32).to_le_bytes());
        frame.extend(payload);
        to.write_all(&frame)
    }

    /// 读一帧；对面正常关了返回 `None`。
    pub fn read_frame(from: &mut impl Read) -> std::io::Result<Option<(u8, Vec<u8>)>> {
        let mut head = [0u8; 5];
        match from.read_exact(&mut head) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(err),
        }
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
        if len > MAX_FRAME {
            return Err(std::io::Error::other("帧过长"));
        }
        let mut payload = vec![0u8; len];
        from.read_exact(&mut payload)?;
        Ok(Some((head[0], payload)))
    }

    pub fn exit_payload(code: Option<i32>) -> Vec<u8> {
        let mut payload = vec![u8::from(code.is_some())];
        payload.extend(code.unwrap_or(0).to_le_bytes());
        payload
    }

    pub fn parse_exit(payload: &[u8]) -> Option<i32> {
        match payload {
            [1, a, b, c, d, ..] => Some(i32::from_le_bytes([*a, *b, *c, *d])),
            _ => None,
        }
    }
}

/// 现在连着的常驻进程。
#[derive(Default)]
struct Link {
    stream: Option<TcpStream>,
    /// 第几条连接：旧连接的读线程退出时看得出自己是不是已经被换掉了
    generation: u64,
    paused: bool,
    /// 从什么时候起没人连着
    alone_since: Option<Instant>,
}

struct Host {
    console: Console,
    link: Mutex<Link>,
    changed: Condvar,
    input: Sender<Vec<u8>>,
    /// 常驻进程说了结束：之后它走了就不用再等谁来收退出码
    killed: AtomicBool,
}

impl Host {
    /// 发一帧给常驻进程；没有连着（或者叫停了）就等。发不出去的换下一条连接重发，不丢。
    fn deliver(&self, kind: u8, payload: &[u8]) {
        let mut link = self.link.lock().unwrap();
        loop {
            let ready = link.stream.is_some() && !(link.paused && kind == wire::OUTPUT);
            if !ready {
                // 是常驻进程让结束的，它又已经走了（叫停、退出）：没有人要这一帧了
                if link.stream.is_none() && self.killed.load(Ordering::Acquire) {
                    return;
                }
                link = self.changed.wait(link).unwrap();
                continue;
            }
            let stream = link.stream.as_mut().expect("刚看过");
            if wire::write_frame(stream, kind, payload).is_ok() {
                return;
            }
            // 这条连接坏了：等下一条
            self.drop_link(&mut link);
        }
    }

    fn drop_link(&self, link: &mut Link) {
        if let Some(stream) = link.stream.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        link.paused = false;
        link.alone_since = Some(Instant::now());
        self.changed.notify_all();
    }

    /// 结束里面的程序：先关伪控制台（程序收到「窗口关了」），过一会儿还没退就强杀。
    fn kill(self: &Arc<Self>) {
        self.killed.store(true, Ordering::Release);
        let host = self.clone();
        std::thread::spawn(move || {
            let closer = host.clone();
            std::thread::spawn(move || closer.console.close());
            if host
                .console
                .wait(Some(KILL_GRACE.as_millis() as u32))
                .is_none()
            {
                host.console.terminate();
            }
        });
    }

    /// 服务一条连上来的连接，直到它断开。
    fn serve(self: &Arc<Self>, stream: TcpStream, token: &str) {
        let _ = stream.set_nodelay(true);
        let Ok(mut reader) = stream.try_clone() else {
            return;
        };
        if !greeted(&mut reader, token) {
            return;
        }
        let generation = {
            let mut link = self.link.lock().unwrap();
            if let Some(old) = link.stream.take() {
                let _ = old.shutdown(Shutdown::Both);
            }
            link.stream = Some(stream);
            link.generation += 1;
            link.paused = false;
            link.alone_since = None;
            self.changed.notify_all();
            link.generation
        };
        while let Ok(Some((kind, payload))) = wire::read_frame(&mut reader) {
            match kind {
                wire::INPUT => {
                    let _ = self.input.send(payload);
                }
                wire::RESIZE => {
                    if let [a, b, c, d, ..] = payload[..] {
                        let size =
                            Size::new(u16::from_le_bytes([a, b]), u16::from_le_bytes([c, d]));
                        self.console.resize(size);
                    }
                }
                wire::KILL => self.kill(),
                wire::PAUSE => {
                    // 拿着锁回「停好了」：这之后发输出的线程看到 paused，不会再往这条连接上写
                    let mut link = self.link.lock().unwrap();
                    if link.generation == generation {
                        link.paused = true;
                        let sent = link
                            .stream
                            .as_mut()
                            .is_some_and(|s| wire::write_frame(s, wire::PAUSED, &[]).is_ok());
                        if !sent {
                            self.drop_link(&mut link);
                        }
                    }
                }
                wire::RESUME => {
                    let mut link = self.link.lock().unwrap();
                    if link.generation == generation {
                        link.paused = false;
                        self.changed.notify_all();
                    }
                }
                _ => {}
            }
        }
        let mut link = self.link.lock().unwrap();
        if link.generation == generation && link.stream.is_some() {
            self.drop_link(&mut link);
        }
    }
}

/// 读连接开头的那一行，对上口令才算数。一个字节一个字节地读，不多读后面的帧。
fn greeted(reader: &mut TcpStream, token: &str) -> bool {
    let _ = reader.set_read_timeout(Some(Duration::from_secs(5)));
    let expected = wire::greeting(token);
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < expected.len() + 16 {
        match reader.read(&mut byte) {
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
    let _ = reader.set_read_timeout(None);
    line == expected.as_bytes()
}

/// `kuaid --pty-host <启动参数文件>` 的入口。
pub fn run(launch_path: &str) -> Result<()> {
    let launch_path = Path::new(launch_path);
    if let Some(dir) = launch_path.parent() {
        // 启动时没有继承到标准输出：出了错写到常驻进程的日志里
        crate::win::log_to(&dir.join("kuaid.log"));
    }
    // 不占着启动时所在的目录
    let _ = std::env::set_current_dir(crate::dirs::home_dir());
    // 我们是带着「新进程组」起来的，系统顺手把「不理 Ctrl+C」记在了我们身上，子进程会沿用：
    // 不改回来的话，终端里按 Ctrl+C 停不下正在跑的程序
    // SAFETY: 只是改这个进程的一个标记
    unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 0) };
    let text = std::fs::read_to_string(launch_path).context("读不到启动参数");
    // 里面有口令和环境变量，读完就删
    let _ = std::fs::remove_file(launch_path);
    let launch: Launch = serde_json::from_str(&text?).context("启动参数格式错误")?;
    let report = |launched: &Launched| {
        // 先写临时文件再改名：常驻进程不会读到半个
        let answer = answer_path(launch_path);
        let mut temp = answer.clone().into_os_string();
        temp.push(".tmp");
        if let Ok(text) = serde_json::to_string(launched) {
            let _ = std::fs::write(&temp, text).and_then(|()| std::fs::rename(&temp, &answer));
        }
    };
    let prepared = (|| -> Result<_> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).context("无法在本机打开监听端口")?;
        let port = listener.local_addr()?.port();
        let size = Size::new(launch.cols, launch.rows);
        let (console, output, input) =
            Console::spawn(&launch.program, &launch.cwd, &launch.env, size)?;
        Ok((listener, port, console, output, input))
    })();
    let (listener, port, console, output, input) = match prepared {
        Ok(prepared) => prepared,
        Err(err) => {
            report(&Launched {
                error: Some(format!("{err:#}")),
                ..Default::default()
            });
            return Ok(());
        }
    };
    report(&Launched {
        port,
        pid: console.pid,
        error: None,
    });

    let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
    let host = Arc::new(Host {
        console,
        link: Mutex::new(Link {
            alone_since: Some(Instant::now()),
            ..Default::default()
        }),
        changed: Condvar::new(),
        input: input_tx,
        killed: AtomicBool::new(false),
    });

    // 用户敲的字：写可能卡住（程序不读输入），不能卡住读连接的线程
    std::thread::spawn(move || {
        let mut input: File = input;
        while let Ok(bytes) = input_rx.recv() {
            if input.write_all(&bytes).is_err() {
                break;
            }
        }
    });

    // shell 退出：关掉伪控制台，输出的那一头才读得到头
    let exit: Arc<Mutex<Option<i32>>> = Arc::default();
    {
        let (host, exit) = (host.clone(), exit.clone());
        std::thread::spawn(move || {
            let code = host.console.wait(None);
            *exit.lock().unwrap() = code;
            host.console.close();
        });
    }

    // 程序的输出；读到头了报告退出码，然后退出
    {
        let (host, exit) = (host.clone(), exit.clone());
        std::thread::spawn(move || {
            let mut output: File = output;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match output.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => host.deliver(wire::OUTPUT, &buf[..n]),
                    Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            // 伪控制台关了才读得到头，这时退出码已经有了；稳妥起见再等一下
            let code = exit
                .lock()
                .unwrap()
                .or_else(|| host.console.wait(Some(KILL_GRACE.as_millis() as u32)));
            host.deliver(wire::EXIT, &wire::exit_payload(code));
            finish(&host);
        });
    }

    // 没人管太久：结束
    {
        let host = host.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let alone = host.link.lock().unwrap().alone_since;
                if alone.is_some_and(|since| since.elapsed() >= ORPHAN) {
                    host.console.terminate();
                    std::process::exit(0);
                }
            }
        });
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        let (host, token) = (host.clone(), launch.token.clone());
        std::thread::spawn(move || host.serve(stream, &token));
    }
    Ok(())
}

/// 退出码送到了：把连接好好关掉再退出（作业对象跟着关，剩下的程序一起结束）。
fn finish(host: &Host) -> ! {
    if let Some(stream) = host.link.lock().unwrap().stream.take() {
        let _ = stream.shutdown(Shutdown::Write);
        // 等对面读完关掉
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut rest = [0u8; 256];
        let mut reader = &stream;
        while matches!(reader.read(&mut rest), Ok(n) if n > 0) {}
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::wire;

    #[test]
    fn 帧写出去读得回来() {
        let mut bytes = Vec::new();
        wire::write_frame(&mut bytes, wire::OUTPUT, b"hello").unwrap();
        wire::write_frame(&mut bytes, wire::PAUSED, &[]).unwrap();
        wire::write_frame(&mut bytes, wire::EXIT, &wire::exit_payload(Some(-3))).unwrap();
        let mut reader = &bytes[..];
        assert_eq!(
            wire::read_frame(&mut reader).unwrap(),
            Some((wire::OUTPUT, b"hello".to_vec()))
        );
        assert_eq!(
            wire::read_frame(&mut reader).unwrap(),
            Some((wire::PAUSED, vec![]))
        );
        let (kind, payload) = wire::read_frame(&mut reader).unwrap().unwrap();
        assert_eq!((kind, wire::parse_exit(&payload)), (wire::EXIT, Some(-3)));
        assert_eq!(wire::read_frame(&mut reader).unwrap(), None);
        assert_eq!(wire::parse_exit(&wire::exit_payload(None)), None);
    }
}
