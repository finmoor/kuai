//! 只运行一个 Kuai。
//!
//! 再次启动（开始菜单、桌面图标、命令行 `kuai <路径>`）时不开第二个窗口和第二个托盘图标，把要做的事
//! （显示窗口、打开路径）交给已经运行的那个，自己退出，和 Fork 一样。两个进程用同一个设置文件还会
//! 互相覆盖设置，所以这一点也是必须的。
//!
//! - 有没有已经运行的：命名互斥量 `Local\Kuai-<键>`（`Local\` 只在当前登录会话中可见）。
//! - 怎么交过去：命名管道 `\\.\pipe\Kuai-<会话号>-<键>`，一行 JSON。已经运行的那个在界面线程上处理完
//!   才回一个字节；它正在退出、界面没有响应时不回，后启动的这个就等它退出后自己接手。管道拒绝网络上的
//!   连接，默认的权限只允许本人和管理员写入。
//! - 键取设置文件路径的哈希：测试时用 FR_SETTINGS 指向别的文件，和平时用的 Kuai 互不影响。
//! - 更新时旧的 Kuai 先启动新的再退出，新的带着 `--wait-pid=<旧进程号>`，先等旧的退出（`update.rs`）。

use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// 启动参数：先等这个进程退出再启动（更新后由旧版本带上）。
pub const WAIT_PID_FLAG: &str = "--wait-pid=";

/// 交给已经运行的 Kuai 的请求。
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Request {
    /// 打开这个目录（命令行给的路径，已经换成绝对路径）
    #[serde(default)]
    pub open: Option<String>,
    /// 显示窗口（开机自启带 `--minimized` 时不显示）
    #[serde(default)]
    pub show: bool,
}

/// 别的进程交过来的请求。处理完往 `done` 里发一个 `()`，那个进程才退出。
pub struct Incoming {
    pub request: Request,
    pub done: SyncSender<()>,
}

pub enum Claim {
    /// 只有这一个：`Guard` 要一直拿着（丢掉就让出去了），`Incoming` 从通道里收
    First(Guard, mpsc::UnboundedReceiver<Incoming>),
    /// 已经交给运行着的那个，这个进程直接退出
    Forwarded,
}

/// 占着「只运行一个」的位置；丢掉就让出去。
pub struct Guard {
    _imp: imp::Guard,
}

/// 启动时调用（在打开任何窗口之前）。
pub fn claim(request: &Request) -> Claim {
    if let Some(pid) = wait_pid() {
        imp::wait_for_exit(pid, Duration::from_secs(15));
    }
    let key = key(&crate::settings::file());
    // 已经运行的那个刚启动、还没建好管道，或者正在退出：等一会儿
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if let Some(guard) = imp::take(&key) {
            let (tx, rx) = mpsc::unbounded();
            imp::listen(&key, tx);
            return Claim::First(Guard { _imp: guard }, rx);
        }
        if imp::forward(&key, request) {
            return Claim::Forwarded;
        }
        if Instant::now() >= deadline {
            imp::alert(fr_core::i18n::tr(
                "Kuai 已在运行，但未响应。请稍后重试，或在任务管理器中结束 Kuai。",
            ));
            return Claim::Forwarded;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 启动参数里的 `--wait-pid=<进程号>`。
fn wait_pid() -> Option<u32> {
    std::env::args().find_map(|arg| arg.strip_prefix(WAIT_PID_FLAG)?.parse().ok())
}

/// 设置文件路径的哈希，取前 16 位。Windows 的路径不分大小写。
fn key(settings: &std::path::Path) -> String {
    let path = std::path::absolute(settings).unwrap_or_else(|_| settings.to_path_buf());
    let text = path.to_string_lossy().to_lowercase();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    Sha256::digest(text.as_bytes())
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(windows)]
mod imp {
    use core::ffi::c_void;
    use std::fs::File;
    use std::io::{Read as _, Write as _};
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use std::ptr::{null, null_mut};
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    use futures::channel::mpsc::UnboundedSender;

    use super::{Incoming, Request};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateMutexW(attributes: *const c_void, owner: i32, name: *const u16) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer: u32,
            in_buffer: u32,
            timeout: u32,
            attributes: *const c_void,
        ) -> *mut c_void;
        fn ConnectNamedPipe(pipe: *mut c_void, overlapped: *mut c_void) -> i32;
        fn GetNamedPipeServerProcessId(pipe: *mut c_void, pid: *mut u32) -> i32;
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn AllowSetForegroundWindow(pid: u32) -> i32;
        fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, kind: u32) -> u32;
    }

    const ERROR_ALREADY_EXISTS: u32 = 183;
    const ERROR_PIPE_CONNECTED: u32 = 535;
    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;
    const PIPE_ACCESS_DUPLEX: u32 = 0x3;
    const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
    /// 字节流、阻塞读写（都是 0），拒绝网络上的连接
    const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x8;
    const PIPE_UNLIMITED_INSTANCES: u32 = 255;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const MB_ICONWARNING: u32 = 0x30;
    /// 一条请求最长这么多字节（一个路径用不了这么多）
    const MAX_REQUEST: usize = 64 * 1024;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    fn pipe_name(key: &str) -> String {
        let mut session = 0;
        // SAFETY：只写进本地变量；失败时会话号按 0 算，只是名字不同
        unsafe { ProcessIdToSessionId(std::process::id(), &mut session) };
        format!(r"\\.\pipe\Kuai-{session}-{key}")
    }

    pub struct Guard(*mut c_void);

    impl Drop for Guard {
        fn drop(&mut self) {
            // SAFETY：句柄是 take 里拿到的，只关这一次
            unsafe { CloseHandle(self.0) };
        }
    }

    /// 建互斥量；已经有了（另一个 Kuai 在运行）返回 None。
    pub fn take(key: &str) -> Option<Guard> {
        let name = wide(&format!(r"Local\Kuai-{key}"));
        // SAFETY：名字以 0 结尾；返回的句柄由 Guard 关，或者马上关掉
        unsafe {
            let mutex = CreateMutexW(null(), 0, name.as_ptr());
            // 建不出来多半是别的权限级别（管理员）运行的 Kuai 建的：也算已经有一个
            if mutex.is_null() {
                return None;
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(mutex);
                return None;
            }
            Some(Guard(mutex))
        }
    }

    /// 后台线程守着管道，收到的请求送进 `tx`。一个连接一个线程：哪个连上来不说话也挡不住别的。
    pub fn listen(key: &str, tx: UnboundedSender<Incoming>) {
        let name = wide(&pipe_name(key));
        let spawned = std::thread::Builder::new()
            .name("kuai-instance".into())
            .spawn(move || {
                let mut mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE;
                while !tx.is_closed() {
                    // SAFETY：名字以 0 结尾；返回的句柄交给 File 管
                    let pipe = unsafe {
                        CreateNamedPipeW(
                            name.as_ptr(),
                            mode,
                            PIPE_REJECT_REMOTE_CLIENTS,
                            PIPE_UNLIMITED_INSTANCES,
                            512,
                            4096,
                            0,
                            null(),
                        )
                    };
                    if pipe == INVALID_HANDLE_VALUE {
                        log::warn!("无法创建管道：{}", std::io::Error::last_os_error());
                        return;
                    }
                    mode = PIPE_ACCESS_DUPLEX;
                    // SAFETY：pipe 是刚建的、有效的句柄
                    let connected = unsafe {
                        ConnectNamedPipe(pipe, null_mut()) != 0
                            || GetLastError() == ERROR_PIPE_CONNECTED
                    };
                    // SAFETY：句柄归 File 所有，丢弃时关掉
                    let file = unsafe { File::from_raw_handle(pipe) };
                    if connected {
                        let tx = tx.clone();
                        std::thread::spawn(move || serve(file, tx));
                    }
                }
            });
        if let Err(err) = spawned {
            log::warn!("无法启动守着管道的线程：{err}");
        }
    }

    /// 读一条请求，交给界面线程，等它处理完再回一个字节。
    fn serve(mut file: File, tx: UnboundedSender<Incoming>) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.contains(&b'\n') && buf.len() < MAX_REQUEST {
            match file.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let line = buf.split(|byte| *byte == b'\n').next().unwrap_or_default();
        let Ok(request) = serde_json::from_slice::<Request>(line) else {
            return;
        };
        let (done, handled) = sync_channel(1);
        if tx.unbounded_send(Incoming { request, done }).is_err() {
            return;
        }
        // 界面线程卡住或者正在退出时不回：那边会再试，等这个进程退出以后自己接手
        if handled.recv_timeout(Duration::from_secs(3)).is_ok() {
            let _ = file.write_all(b"1");
            // 等对方读走再关，不然没读的数据会被丢掉
            let _ = file.sync_all();
        }
    }

    /// 把请求交给正在运行的 Kuai，它处理完返回 true。
    pub fn forward(key: &str, request: &Request) -> bool {
        let Ok(line) = serde_json::to_string(request) else {
            return false;
        };
        let Ok(mut pipe) = File::options().read(true).write(true).open(pipe_name(key)) else {
            return false;
        };
        // 用户刚启动的这个进程有权把前台交出去：让那边能把窗口放到最前面
        let mut pid = 0;
        // SAFETY：句柄是刚打开的管道，pid 是本地变量
        if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } != 0 {
            unsafe { AllowSetForegroundWindow(pid) };
        }
        if pipe.write_all(format!("{line}\n").as_bytes()).is_err() {
            return false;
        }
        // 那边最多等界面线程 3 秒；读放在别的线程上，免得碰上不回话的一直卡在这里
        let (tx, rx) = sync_channel(1);
        std::thread::spawn(move || {
            let mut reply = [0u8; 1];
            let ok = pipe.read_exact(&mut reply).is_ok() && reply[0] == b'1';
            let _ = tx.send(ok);
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap_or(false)
    }

    /// 等进程 `pid` 退出，最多等 `timeout`。已经退出了直接返回。
    pub fn wait_for_exit(pid: u32, timeout: Duration) {
        // SAFETY：只要了等待的权限；拿到的句柄马上关掉
        unsafe {
            let process = OpenProcess(SYNCHRONIZE, 0, pid);
            if process.is_null() {
                return;
            }
            WaitForSingleObject(process, timeout.as_millis() as u32);
            CloseHandle(process);
        }
    }

    /// 还没有窗口时报错用的系统对话框。
    pub fn alert(text: &str) {
        let (text, caption) = (wide(text), wide("Kuai"));
        // SAFETY：两段文字都以 0 结尾，没有父窗口
        unsafe { MessageBoxW(0, text.as_ptr(), caption.as_ptr(), MB_ICONWARNING) };
    }
}

#[cfg(not(windows))]
mod imp {
    use futures::channel::mpsc::UnboundedSender;
    use std::time::Duration;

    use super::{Incoming, Request};

    pub struct Guard;

    pub fn take(_: &str) -> Option<Guard> {
        Some(Guard)
    }
    pub fn listen(_: &str, _: UnboundedSender<Incoming>) {}
    pub fn forward(_: &str, _: &Request) -> bool {
        false
    }
    pub fn wait_for_exit(_: u32, _: Duration) {}
    pub fn alert(text: &str) {
        eprintln!("{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 设置文件路径不分大小写() {
        let a = key(std::path::Path::new(
            r"C:\Users\Feng\AppData\Roaming\kuai\settings.json",
        ));
        let b = key(std::path::Path::new(
            r"c:\users\feng\appdata\roaming\KUAI\settings.json",
        ));
        let c = key(std::path::Path::new(r"C:\Temp\test-settings.json"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn 请求按一行_json_传() {
        let request = Request {
            open: Some(r"C:\work\有空格 的目录".into()),
            show: true,
        };
        let line = serde_json::to_string(&request).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
        // 以后加了字段，旧版本也读得懂；少了字段按默认值
        let old: Request = serde_json::from_str(r#"{"show":true,"later":1}"#).unwrap();
        assert_eq!(
            old,
            Request {
                open: None,
                show: true
            }
        );
    }
}
