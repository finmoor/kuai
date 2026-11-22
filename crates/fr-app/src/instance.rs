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
//! - Linux 等 Unix 系统上互斥量换成文件锁、管道换成 Unix 套接字，做法一样（见 Unix 的 `imp`）。
//!
//! 这根管道还替这台电脑上的 kuaid 办一件事：把它的常驻进程拉起来（[`start_daemon`]）。从别的电脑
//! 经 ssh 连上来时，kuaid 的转接程序在会话 0（没有桌面），它自己拉起的常驻进程也在那里，里面的终端
//! 用的是管理员权限，开不了带窗口的程序；由运行在桌面上的 Kuai 来拉，常驻进程就和本机开的一样。
//! 管道叫什么写在 `~/.kuai/run/kuai.pipe` 里（kuaid 不知道设置文件在哪，算不出名字）。

use std::path::{Path, PathBuf};
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
    /// kuaid 的转接程序发来的：拉起这个 kuaid 程序的常驻进程。带着它的请求不交给界面
    #[serde(default)]
    pub daemon: Option<String>,
    /// 和 `daemon` 一起：常驻进程升级换新时的交接文件
    #[serde(default)]
    pub resume: Option<String>,
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
    /// 写给 kuaid 看的那个文件和里面的内容：退出时还是我们写的那份就删掉
    announced: Option<(PathBuf, String)>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some((file, name)) = &self.announced
            && std::fs::read_to_string(file).is_ok_and(|text| text.trim() == name)
        {
            let _ = std::fs::remove_file(file);
        }
    }
}

/// 把管道的名字写到 `~/.kuai/run/kuai.pipe`，kuaid 的转接程序照着它找过来。写不了就算了：
/// 转接程序找不到我们就自己拉起常驻进程。
fn announce(name: String) -> Option<(PathBuf, String)> {
    let run = fr_channel::local_kuai_root().join("run");
    std::fs::create_dir_all(&run).ok()?;
    let file = run.join("kuai.pipe");
    std::fs::write(&file, &name).ok()?;
    Some((file, name))
}

/// 替 kuaid 的转接程序拉起常驻进程，办成了返回 true。只认装在 `~/.kuai/<版本>/` 下的 `kuaid.exe`
/// 和 `~/.kuai/run/handoff` 这一个交接文件：管道本人和管理员都能写，不能叫我们启动别的程序。
///
/// 跑的是 `kuaid.exe --spawn-daemon`：它用「什么句柄都不继承」的办法起常驻进程，自己马上退出。
/// 直接从这里起的话，我们手里正开着的管道（和 ssh、git 之间的）可能被常驻进程带走，那些连接就断不开了。
#[cfg_attr(not(windows), allow(dead_code))]
fn start_daemon(exe: &str, resume: Option<&str>) -> bool {
    let root = fr_channel::local_kuai_root();
    let exe = Path::new(exe);
    let version_dir = exe.parent();
    let ours = exe
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("kuaid.exe"))
        && version_dir
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(|c: char| c.is_ascii_digit()))
        && version_dir
            .and_then(Path::parent)
            .is_some_and(|dir| same_path(dir, &root))
        && exe.is_file();
    let handoff = root.join("run").join("handoff");
    if !ours || resume.is_some_and(|state| !same_path(Path::new(state), &handoff)) {
        log::warn!(
            "没有照办：不是 kuaid 自己的程序或交接文件（{}）",
            exe.display()
        );
        return false;
    }
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--spawn-daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(state) = resume {
        cmd.args(["--resume", state]);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NO_WINDOW：不弹出控制台窗口
        cmd.creation_flags(0x0800_0000);
    }
    let started = cmd.status().is_ok_and(|status| status.success());
    log::info!(
        "替 kuaid 拉起常驻进程（{}）：{}",
        exe.display(),
        if started { "已启动" } else { "失败" }
    );
    started
}

/// 两个路径是不是同一个：Windows 上不分大小写、两种斜杠都行。只比写法，不去解析 `..`
/// （带 `..` 的一律算不同）。
#[cfg_attr(not(windows), allow(dead_code))]
fn same_path(a: &Path, b: &Path) -> bool {
    let plain = |path: &Path| {
        let text = path.to_string_lossy().replace('/', "\\").to_lowercase();
        let text = text.strip_prefix(r"\\?\").unwrap_or(&text).to_string();
        text.trim_end_matches('\\').to_string()
    };
    let (a, b) = (plain(a), plain(b));
    a == b && !a.split('\\').any(|part| part == "..")
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
            let guard = Guard {
                _imp: guard,
                announced: imp::pipe_name(&key).and_then(announce),
            };
            return Claim::First(guard, rx);
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
    use std::time::{Duration, Instant};

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
        fn PeekNamedPipe(
            pipe: *mut c_void,
            buffer: *mut c_void,
            size: u32,
            read: *mut u32,
            available: *mut u32,
            left: *mut u32,
        ) -> i32;
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
    /// 连上来以后最多等这么久把请求说完、等这么久把回答读走：连上来不说话、不读的占不住线程和管道实例
    const READ_LIMIT: Duration = Duration::from_secs(3);
    const REPLY_LIMIT: Duration = Duration::from_secs(3);

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    fn name_of(key: &str) -> String {
        let mut session = 0;
        // SAFETY：只写进本地变量；失败时会话号按 0 算，只是名字不同
        unsafe { ProcessIdToSessionId(std::process::id(), &mut session) };
        format!(r"\\.\pipe\Kuai-{session}-{key}")
    }

    /// 我们守的管道叫什么。
    pub fn pipe_name(key: &str) -> Option<String> {
        Some(name_of(key))
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
        let name = wide(&name_of(key));
        let spawned = std::thread::Builder::new()
            .name("kuai-instance".into())
            .spawn(move || {
                let mut mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE;
                let mut retry = Duration::from_millis(500);
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
                    // 建不出来（实例暂时用完了之类）：歇一会儿再建。原来就此不守了，之后再启动的
                    // Kuai 都交不过来
                    if pipe == INVALID_HANDLE_VALUE {
                        log::warn!("无法创建管道：{}", std::io::Error::last_os_error());
                        std::thread::sleep(retry);
                        retry = (retry * 2).min(Duration::from_secs(30));
                        continue;
                    }
                    retry = Duration::from_millis(500);
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

    /// 管道里有几个字节可以读；对方已经断开时是 None。管道是同步的，`read` 没有期限：先问一声，
    /// 有了才读。
    fn available(file: &File) -> Option<u32> {
        let mut available = 0u32;
        // SAFETY：句柄是有效的管道；只拿可读的字节数，不读走
        let ok = unsafe {
            PeekNamedPipe(
                file.as_raw_handle(),
                null_mut(),
                0,
                null_mut(),
                &mut available,
                null_mut(),
            )
        };
        (ok != 0).then_some(available)
    }

    /// 读一条请求（到换行为止），最多等 [`READ_LIMIT`]。
    fn read_request(file: &mut File) -> Vec<u8> {
        let deadline = Instant::now() + READ_LIMIT;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.contains(&b'\n') && buf.len() < MAX_REQUEST {
            match available(file) {
                None => break,
                Some(0) if Instant::now() >= deadline => break,
                Some(0) => std::thread::sleep(Duration::from_millis(10)),
                Some(count) => {
                    let want = (count as usize).min(chunk.len());
                    match file.read(&mut chunk[..want]) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => buf.extend_from_slice(&chunk[..read]),
                    }
                }
            }
        }
        buf
    }

    /// 回一个字节，等对方读走（它读完就关）再返回，最多等 [`REPLY_LIMIT`]：先关的话没读的会被丢掉。
    /// 原来用 FlushFileBuffers 等，对方一直不读就一直等。
    fn reply(file: &mut File) {
        if file.write_all(b"1").is_err() {
            return;
        }
        let deadline = Instant::now() + REPLY_LIMIT;
        while available(file).is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// 读一条请求，交给界面线程，等它处理完再回一个字节。
    fn serve(mut file: File, tx: UnboundedSender<Incoming>) {
        let buf = read_request(&mut file);
        let line = buf.split(|byte| *byte == b'\n').next().unwrap_or_default();
        let Ok(request) = serde_json::from_slice::<Request>(line) else {
            return;
        };
        // kuaid 的转接程序请我们拉起常驻进程：就在这个线程上办，用不着界面
        if let Some(exe) = &request.daemon {
            if super::start_daemon(exe, request.resume.as_deref()) {
                reply(&mut file);
            }
            return;
        }
        let (done, handled) = sync_channel(1);
        if tx.unbounded_send(Incoming { request, done }).is_err() {
            return;
        }
        // 界面线程卡住或者正在退出时不回：那边会再试，等这个进程退出以后自己接手
        if handled.recv_timeout(Duration::from_secs(3)).is_ok() {
            reply(&mut file);
        }
    }

    /// 把请求交给正在运行的 Kuai，它处理完返回 true。
    pub fn forward(key: &str, request: &Request) -> bool {
        let Ok(line) = serde_json::to_string(request) else {
            return false;
        };
        let Ok(mut pipe) = File::options().read(true).write(true).open(name_of(key)) else {
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

/// Linux 等 Unix 系统：有没有已经运行的看文件锁 `kuai-<键>.lock`（进程退出、崩溃时系统自动放开）；
/// 请求经 Unix 套接字 `kuai-<键>.sock` 交过去，一样是一行 JSON、处理完回一个字节。两个文件放在
/// `$XDG_RUNTIME_DIR`（只有本人能进的目录），没有这个变量时放在 `~/.kuai/run/`。
///
/// kuaid 在 Unix 上自己拉起常驻进程，不来找 Kuai（Windows 上那样做是为了离开会话 0），所以不写
/// `kuai.pipe`。
#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::mpsc::sync_channel;
    use std::time::{Duration, Instant};

    use futures::channel::mpsc::UnboundedSender;

    use super::{Incoming, Request};

    /// 一条请求最长这么多字节（一个路径用不了这么多）
    const MAX_REQUEST: u64 = 64 * 1024;
    /// 连上来以后最多等这么久把请求说完：连上来不说话的占不住线程
    const READ_LIMIT: Duration = Duration::from_secs(3);
    const REPLY_LIMIT: Duration = Duration::from_secs(3);

    /// 放锁和套接字的目录。
    fn dir() -> PathBuf {
        match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
            Some(dir) if dir.is_absolute() && dir.is_dir() => dir,
            _ => {
                let dir = fr_channel::local_kuai_root().join("run");
                let _ = std::fs::create_dir_all(&dir);
                dir
            }
        }
    }

    fn socket_path(key: &str) -> PathBuf {
        dir().join(format!("kuai-{key}.sock"))
    }

    pub fn pipe_name(_: &str) -> Option<String> {
        None
    }

    /// 拿着锁文件；`None` 是锁文件建不出来，这时不管有没有别的 Kuai，照常运行。
    pub struct Guard(#[allow(dead_code)] Option<File>);

    /// 锁上锁文件；锁不上（另一个 Kuai 在运行）返回 None。
    pub fn take(key: &str) -> Option<Guard> {
        let path = dir().join(format!("kuai-{key}.lock"));
        let file = match File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(err) => {
                log::warn!("无法创建 {}：{err}", path.display());
                return Some(Guard(None));
            }
        };
        match file.try_lock() {
            Ok(()) => Some(Guard(Some(file))),
            Err(std::fs::TryLockError::WouldBlock) => None,
            Err(std::fs::TryLockError::Error(err)) => {
                log::warn!("无法锁定 {}：{err}", path.display());
                Some(Guard(None))
            }
        }
    }

    /// 后台线程守着套接字，收到的请求送进 `tx`。一个连接一个线程：哪个连上来不说话也挡不住别的。
    pub fn listen(key: &str, tx: UnboundedSender<Incoming>) {
        let path = socket_path(key);
        // 上一个 Kuai 没收拾掉的（崩溃了）：锁在我们手里，这个文件肯定没人守了
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(err) => {
                log::warn!("无法创建 {}：{err}", path.display());
                return;
            }
        };
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        let spawned = std::thread::Builder::new()
            .name("kuai-instance".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if tx.is_closed() {
                        break;
                    }
                    match stream {
                        Ok(stream) => {
                            let tx = tx.clone();
                            std::thread::spawn(move || serve(stream, tx));
                        }
                        Err(err) => {
                            log::warn!("套接字接受连接失败：{err}");
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                }
            });
        if let Err(err) = spawned {
            log::warn!("无法启动守着套接字的线程：{err}");
        }
    }

    /// 读一条请求，交给界面线程，等它处理完再回一个字节。
    fn serve(stream: UnixStream, tx: UnboundedSender<Incoming>) {
        let _ = stream.set_read_timeout(Some(READ_LIMIT));
        let _ = stream.set_write_timeout(Some(REPLY_LIMIT));
        let mut buf = Vec::new();
        let deadline = Instant::now() + READ_LIMIT;
        let mut reader = (&stream).take(MAX_REQUEST);
        let mut chunk = [0u8; 1024];
        while !buf.contains(&b'\n') && Instant::now() < deadline {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => buf.extend_from_slice(&chunk[..read]),
            }
        }
        let line = buf.split(|byte| *byte == b'\n').next().unwrap_or_default();
        let Ok(request) = serde_json::from_slice::<Request>(line) else {
            return;
        };
        // 拉起 kuaid 常驻进程只在 Windows 上替它办
        if request.daemon.is_some() {
            return;
        }
        let (done, handled) = sync_channel(1);
        if tx.unbounded_send(Incoming { request, done }).is_err() {
            return;
        }
        // 界面线程卡住或者正在退出时不回：那边会再试，等这个进程退出以后自己接手
        if handled.recv_timeout(Duration::from_secs(3)).is_ok() {
            let _ = (&stream).write_all(b"1");
        }
    }

    /// 把请求交给正在运行的 Kuai，它处理完返回 true。
    pub fn forward(key: &str, request: &Request) -> bool {
        let Ok(line) = serde_json::to_string(request) else {
            return false;
        };
        let Ok(mut stream) = UnixStream::connect(socket_path(key)) else {
            return false;
        };
        // 那边最多等界面线程 3 秒
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        if stream.write_all(format!("{line}\n").as_bytes()).is_err() {
            return false;
        }
        let mut reply = [0u8; 1];
        stream.read_exact(&mut reply).is_ok() && reply[0] == b'1'
    }

    /// 等进程 `pid` 退出，最多等 `timeout`。看 `/proc/<pid>/stat`：没有了或者成了僵尸（退出了、
    /// 还没被收尸）都算退出了。
    pub fn wait_for_exit(pid: u32, timeout: Duration) {
        let stat = PathBuf::from(format!("/proc/{pid}/stat"));
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let running = std::fs::read_to_string(&stat).is_ok_and(|text| {
                // 第三项是状态；第二项（程序名）带括号，可能有空格，从最后一个右括号往后数
                text.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    .is_some_and(|state| state != "Z" && state != "X")
            });
            if !running {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 还没有窗口时报错用的对话框：GNOME 有 zenity、KDE 有 kdialog，都没有就只打到标准错误。
    pub fn alert(text: &str) {
        eprintln!("{text}");
        let shown = std::process::Command::new("zenity")
            .args([
                "--warning",
                "--no-markup",
                "--title",
                "Kuai",
                "--text",
                text,
            ])
            .status()
            .is_ok();
        if !shown {
            let _ = std::process::Command::new("kdialog")
                .args(["--title", "Kuai", "--sorry", text])
                .status();
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    use futures::channel::mpsc::UnboundedSender;
    use std::time::Duration;

    use super::{Incoming, Request};

    pub struct Guard;

    pub fn take(_: &str) -> Option<Guard> {
        Some(Guard)
    }
    pub fn pipe_name(_: &str) -> Option<String> {
        None
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

    #[cfg(windows)]
    #[test]
    fn 管道一来一回_连上不说话的挡不住别人() {
        use futures::StreamExt as _;
        let key = format!("test-{}", std::process::id());
        let (tx, mut rx) = mpsc::unbounded();
        imp::listen(&key, tx);
        // 界面那头：收到就算处理完
        std::thread::spawn(move || {
            while let Some(incoming) = futures::executor::block_on(rx.next()) {
                let _ = incoming.done.send(());
            }
        });
        let name = imp::pipe_name(&key).unwrap();
        // 一个连上来不说话的（原来它占着的线程一直读下去）
        let started = Instant::now();
        let silent = loop {
            match std::fs::File::options().read(true).write(true).open(&name) {
                Ok(file) => break file,
                Err(_) if started.elapsed() < Duration::from_secs(5) => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(err) => panic!("连不上管道：{err}"),
            }
        };
        // 另一个照样交得过去：守管道的线程接着建下一个实例
        let request = Request {
            show: true,
            ..Default::default()
        };
        let forwarded = (0..100).any(|_| {
            let done = imp::forward(&key, &request);
            if !done {
                std::thread::sleep(Duration::from_millis(20));
            }
            done
        });
        assert!(forwarded);
        drop(silent);
    }

    #[cfg(unix)]
    #[test]
    fn 套接字一来一回_连上不说话的挡不住别人_锁住了别人拿不到() {
        use futures::StreamExt as _;
        let key = format!("test-{}", std::process::id());
        let guard = imp::take(&key).expect("第一个拿得到");
        assert!(imp::take(&key).is_none(), "锁着的时候第二个也拿到了");
        let (tx, mut rx) = mpsc::unbounded();
        imp::listen(&key, tx);
        // 界面那头：收到就算处理完，把请求转回来核对
        let (seen_tx, seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Some(incoming) = futures::executor::block_on(rx.next()) {
                let _ = seen_tx.send(incoming.request);
                let _ = incoming.done.send(());
            }
        });
        let dir = match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
            Some(dir) if dir.is_absolute() && dir.is_dir() => dir,
            _ => fr_channel::local_kuai_root().join("run"),
        };
        let socket = dir.join(format!("kuai-{key}.sock"));
        // 一个连上来不说话的
        let silent = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        let request = Request {
            open: Some("/home/me/有 空格".into()),
            show: true,
            ..Default::default()
        };
        assert!(imp::forward(&key, &request));
        assert_eq!(seen.recv_timeout(Duration::from_secs(5)).unwrap(), request);
        drop(silent);
        drop(guard);
        // 放开以后下一个拿得到。别的测试这时可能正在起子进程：fork 出来、还没 exec 的那一下，子进程
        // 也拿着锁文件（exec 时才关），多试几次
        let again = (0..100)
            .find_map(|_| {
                let taken = imp::take(&key);
                if taken.is_none() {
                    std::thread::sleep(Duration::from_millis(20));
                }
                taken
            })
            .expect("放开以后拿不到");
        drop(again);
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_file(dir.join(format!("kuai-{key}.lock")));
    }

    #[cfg(unix)]
    #[test]
    fn 等退出_已经退出的和僵尸都不等() {
        let started = Instant::now();
        // 不存在的进程
        imp::wait_for_exit(u32::MAX - 1, Duration::from_secs(5));
        // 退出了、还没被收尸的子进程
        let mut child = std::process::Command::new("true").spawn().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        imp::wait_for_exit(child.id(), Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(3));
        let _ = child.wait();
        // 还在跑的要等满
        let mut running = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let started = Instant::now();
        imp::wait_for_exit(running.id(), Duration::from_millis(300));
        assert!(started.elapsed() >= Duration::from_millis(300));
        let _ = running.kill();
        let _ = running.wait();
    }

    #[test]
    fn 设置文件路径不分大小写() {
        let a = key(std::path::Path::new(
            r"C:\Users\me\AppData\Roaming\kuai\settings.json",
        ));
        let b = key(std::path::Path::new(
            r"c:\users\me\appdata\roaming\KUAI\settings.json",
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
            ..Default::default()
        };
        let line = serde_json::to_string(&request).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
        // 以后加了字段，旧版本也读得懂；少了字段按默认值
        let old: Request = serde_json::from_str(r#"{"show":true,"later":1}"#).unwrap();
        assert_eq!(
            old,
            Request {
                show: true,
                ..Default::default()
            }
        );
        // kuaid 的转接程序发来的
        let asked: Request =
            serde_json::from_str(r#"{"daemon":"C:\\k\\kuaid.exe","resume":null}"#).unwrap();
        assert_eq!(asked.daemon.as_deref(), Some(r"C:\k\kuaid.exe"));
        assert_eq!(asked.resume, None);
    }

    #[test]
    fn 路径只比写法_大小写和斜杠不算() {
        use super::same_path;
        use std::path::Path;
        let same = |a: &str, b: &str| same_path(Path::new(a), Path::new(b));
        assert!(same(r"C:\Users\me\.kuai", "c:/users/me/.kuai/"));
        assert!(same(r"\\?\C:\Users\me\.kuai", r"C:\Users\me\.kuai"));
        assert!(!same(r"C:\Users\me\.kuai", r"C:\Users\me\.kuai2"));
        // 带 .. 的不认：绕出去再绕回来也不行
        assert!(!same(r"C:\a\..\a", r"C:\a\..\a"));
    }

    #[test]
    fn 只替装在_kuai_目录下的_kuaid_拉起常驻进程() {
        use super::start_daemon;
        let root = fr_channel::local_kuai_root();
        // 别处的程序、别的名字、版本目录之外的都不启动（这些文件本来也不存在）
        assert!(!start_daemon(r"C:\Windows\System32\cmd.exe", None));
        let other = root.join("1.0.0-abc").join("other.exe");
        assert!(!start_daemon(&other.to_string_lossy(), None));
        let outside = root.join("run").join("kuaid.exe");
        assert!(!start_daemon(&outside.to_string_lossy(), None));
        let escaped = root
            .join("1.0.0-abc")
            .join("..")
            .join("..")
            .join("kuaid.exe");
        assert!(!start_daemon(&escaped.to_string_lossy(), None));
    }
}
