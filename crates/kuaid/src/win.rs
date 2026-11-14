//! Windows 上起长期进程（常驻进程、终端托管进程）的几件事。
//!
//! 这些进程活得比启动它们的那一个久，所以启动时一个句柄都不让它们继承：标准库起子进程时，
//! 所有「可继承」的句柄都会带过去，`accept` 接进来的连接默认就是可继承的。实测终端托管进程带着
//! 客户端的连接，那条连接常驻进程这头关了对面也收不到，转接程序退不出去；带着 ssh 的管道的话，
//! ssh 断不开。不继承句柄就没有标准输出可接，它们启动后自己把标准输出、标准错误接到日志文件上
//! （[`log_to`]）。

use std::fs::OpenOptions;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::IntoRawHandle as _;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CreateProcessW, DETACHED_PROCESS,
    PROCESS_INFORMATION, STARTUPINFOW, WaitForSingleObject,
};

/// 日志文件超过这么大就从头写
const LOG_LIMIT: u64 = 1 << 20;

/// 用 [`spawn_detached`] 起的进程。丢掉它不会结束进程。
pub struct Detached {
    process: HANDLE,
}

// SAFETY: 里面是一个进程句柄，哪个线程用都一样
unsafe impl Send for Detached {}

impl Detached {
    /// 它退出了没有。接口照着标准库的 `Child::try_wait`：退出了是 `Some`。
    pub fn try_wait(&mut self) -> std::io::Result<Option<()>> {
        // SAFETY: 句柄在我们手里，一直有效
        let exited = unsafe { WaitForSingleObject(self.process, 0) } == WAIT_OBJECT_0;
        Ok(exited.then_some(()))
    }
}

impl Drop for Detached {
    fn drop(&mut self) {
        // SAFETY: 只关这一次
        unsafe { CloseHandle(self.process) };
    }
}

/// 拉起 `exe`：没有控制台，不继承我们的任何句柄，不跟着我们结束。
///
/// Windows 的 sshd 把一条连接起的进程都放在一个作业里，连接断开时全部杀掉；带上「脱离作业」
/// 才活得下来。作业不许脱离时（不是经 sshd 启动的）不带这个标记再试一次。
pub fn spawn_detached(exe: &Path, args: &[&str]) -> std::io::Result<Detached> {
    let mut line = vec![exe.to_string_lossy().into_owned()];
    line.extend(args.iter().map(|arg| arg.to_string()));
    let line = crate::term::conpty::command_line(&line);
    let program: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    let spawn = |flags: u32| -> std::io::Result<Detached> {
        let mut line: Vec<u16> = line.encode_utf16().chain([0]).collect();
        // SAFETY: 全零是这两个结构体的合法初值；各个指针指向的缓冲区在调用期间都有效
        unsafe {
            let mut startup: STARTUPINFOW = std::mem::zeroed();
            startup.cb = size_of::<STARTUPINFOW>() as u32;
            let mut info: PROCESS_INFORMATION = std::mem::zeroed();
            let created = CreateProcessW(
                program.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                // 不继承句柄
                0,
                flags,
                std::ptr::null(),
                std::ptr::null(),
                &startup,
                &mut info,
            );
            if created == 0 {
                return Err(std::io::Error::last_os_error());
            }
            CloseHandle(info.hThread);
            Ok(Detached {
                process: info.hProcess,
            })
        }
    };
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    spawn(base | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| spawn(base))
}

/// 这个进程在哪个登录会话里：经 sshd 进来的在会话 0（没有桌面，开不了带窗口的程序），
/// 在桌面上启动的是 1、2……
pub fn session_id() -> u32 {
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let mut session = 0;
    // SAFETY: 只写进本地变量；失败时按 0 算
    unsafe { ProcessIdToSessionId(std::process::id(), &mut session) };
    session
}

/// 请 Kuai 回话最多等这么久
const ASK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Kuai 运行着的时候在这个文件里写着它守的管道叫什么（见客户端的 `instance.rs`）。
pub fn kuai_pipe_file(run: &Path) -> std::path::PathBuf {
    run.join("kuai.pipe")
}

/// 请正在运行的 Kuai 把常驻进程拉起来（`resume` 是交接文件，升级换新时带上）。它照办了返回 true。
///
/// 常驻进程由谁拉起就在谁的登录会话里。经 ssh 进来的转接程序在会话 0：那里开的终端用的是
/// 管理员权限，也开不了带窗口的程序。这台电脑上运行着 Kuai 的话请它来拉起，常驻进程就落在
/// 桌面上，本机的 Kuai 和从别的电脑连上来的 Kuai 用的是同一批、一样的终端。Kuai 没在运行
/// （或者是不认这个请求的旧版本）返回 false，调用方自己拉起。
pub fn ask_kuai(run: &Path, exe: &Path, resume: Option<&str>) -> bool {
    use std::io::{Read as _, Write as _};
    use std::os::windows::fs::OpenOptionsExt as _;
    /// 管道那头只能知道我们是谁，不能拿我们的身份办事（我们可能有管理员权限，它没有）
    const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;

    let Ok(name) = std::fs::read_to_string(kuai_pipe_file(run)) else {
        return false;
    };
    let name = name.trim();
    if !name.starts_with(r"\\.\pipe\Kuai-") {
        return false;
    }
    let opened = OpenOptions::new()
        .read(true)
        .write(true)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(name);
    let Ok(mut pipe) = opened else {
        return false;
    };
    let request = serde_json::json!({ "daemon": exe, "resume": resume });
    if pipe.write_all(format!("{request}\n").as_bytes()).is_err() {
        return false;
    }
    // 读放在别的线程上：碰上不回话的不能一直卡在这里
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut reply = [0u8; 1];
        let ok = pipe.read_exact(&mut reply).is_ok() && reply[0] == b'1';
        let _ = tx.send(ok);
    });
    rx.recv_timeout(ASK_WAIT).unwrap_or(false)
}

/// 把这个进程的标准输出、标准错误接到日志文件上（往后追加；太大了就从头写）。
/// 用 [`spawn_detached`] 起来的进程一开始调用：它们没有继承到标准输出。
pub fn log_to(log: &Path) {
    let too_big = std::fs::metadata(log).is_ok_and(|meta| meta.len() > LOG_LIMIT);
    let opened = OpenOptions::new()
        .create(true)
        .append(!too_big)
        .write(true)
        .truncate(too_big)
        .open(log);
    let Ok(file) = opened else {
        return;
    };
    // 句柄留到进程结束
    let handle = file.into_raw_handle();
    // SAFETY: 句柄是刚打开的文件，之后不再关
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, handle);
        SetStdHandle(STD_ERROR_HANDLE, handle);
    }
}

/// 这个用户现在登录会拿到的环境变量：系统的加上用户自己的，从注册表里现读。
///
/// 常驻进程一跑就是很久，它自己的环境变量停在启动的那一刻；之后装了新程序、改了 PATH，新开的终端
/// 应该看得到（在资源管理器里新开一个终端也是这样）。读不出来返回 `None`。
pub fn fresh_env() -> Option<Vec<(String, String)>> {
    use windows_sys::Win32::Security::{TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY};
    use windows_sys::Win32::System::Environment::{
        CreateEnvironmentBlock, DestroyEnvironmentBlock,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    let access = TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE;
    // SAFETY: 传进去的都是有效的指针；拿到的句柄下面关掉
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token) } == 0 {
        return None;
    }
    let mut block: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: 同上；拿到的那块内存读完就还回去
    let made = unsafe { CreateEnvironmentBlock(&mut block, token, 0) };
    // SAFETY: token 是上面打开的
    unsafe { CloseHandle(token) };
    if made == 0 || block.is_null() {
        return None;
    }
    // 一串以 0 结尾的 UTF-16 的 `名字=值`，最后再多一个 0
    let mut vars = Vec::new();
    let mut at = block.cast::<u16>().cast_const();
    loop {
        let mut len = 0;
        // SAFETY: 这块内存以两个连着的 0 结尾，读到第一个空串就停，不会越界
        while unsafe { *at.add(len) } != 0 {
            len += 1;
        }
        if len == 0 {
            break;
        }
        // SAFETY: 刚数过，这 len 个字都在这块内存里
        let entry = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(at, len) });
        // `=C:=C:\目录` 这种是 cmd 记各个盘当前目录用的，名字以等号开头，不要
        if let Some((key, value)) = entry.split_once('=')
            && !key.is_empty()
        {
            vars.push((key.to_string(), value.to_string()));
        }
        // SAFETY: 跳到下一条的开头，还在这块内存里
        at = unsafe { at.add(len + 1) };
    }
    // SAFETY: block 是上面拿到的，之后不再用
    unsafe { DestroyEnvironmentBlock(block) };
    Some(vars)
}

#[cfg(test)]
mod tests {
    #[test]
    fn 现读的环境变量里有系统的那几个() {
        let vars = super::fresh_env().expect("读不出环境变量");
        let has = |name: &str| vars.iter().any(|(key, _)| key.eq_ignore_ascii_case(name));
        assert!(
            has("PATH") && has("SystemRoot") && has("USERPROFILE"),
            "{vars:?}"
        );
        assert!(
            vars.iter()
                .all(|(key, _)| !key.is_empty() && !key.contains('='))
        );
    }
}
