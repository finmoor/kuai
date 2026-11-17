//! Windows 的 Redirection Guard（进程不跟随普通用户建的目录联接和符号链接）：启动时发现自己带着，
//! 就请资源管理器把自己重新启动一遍，甩掉它。
//!
//! 这项防护是给管理员权限的安装程序用的，Inno Setup 6.5 起默认给安装程序开。它会传给子进程，
//! 开了就关不掉，创建子进程时也没有「不要」的选项。1.14 及以前，安装完、更新完都由安装程序
//! 拉起 Kuai，于是 Kuai 带着它，再传给 kuaid、终端和终端里的所有程序：nvm 的 `node`（联接）、
//! scoop 的 `current`（联接）、pnpm 的 `node_modules`、rustup 的 `cargo`（符号链接）都进不去，
//! 报「The path cannot be traversed because it contains an untrusted mount point」。
//!
//! 三处一起管：
//!
//! - 安装脚本关掉它（`tools/installer/kuai.iss` 的 `RedirectionGuard=no`）。
//! - 这里：带着它启动的 Kuai 重新启动一遍。只关安装脚本不够，更新是由运行着的 Kuai 拉起安装程序，
//!   它带着，新的安装程序、新的 Kuai 就接着带；从带着它的终端里启动 Kuai 也一样。资源管理器起的
//!   进程不带，所以请它来起（它桌面那个窗口的自动化对象上的 `ShellExecute`，在资源管理器的进程里
//!   执行；微软给「从管理员进程里起一个普通权限的进程」推荐的也是这个办法）。
//! - kuaid：常驻进程带着而转接程序没带时，让它由 Kuai 重新起一遍（`kuaid` 的 `bridge.rs`）。
//!   已经开着的终端甩不掉，要重开。
//!
//! 资源管理器起的进程用的是它的环境变量：测试时指路用的 `FR_*` 变量用 `--env=` 参数带过去，
//! 不然重启后的进程会去动平时用的设置和 `~/.kuai`。

/// 启动参数：这个进程是甩掉防护后重新启动的。还带着就不再试，免得来回启动。
pub const RELAUNCHED_FLAG: &str = "--relaunched";

/// 启动参数：重新启动时带过来的环境变量，`--env=FR_SETTINGS=<文件>`。只认 `FR_` 开头的。
pub const ENV_FLAG: &str = "--env=";

/// 进程一开始调用：把 `--env=` 带过来的环境变量设上。
pub fn restore_env() {
    for arg in std::env::args() {
        if let Some((name, value)) = arg
            .strip_prefix(ENV_FLAG)
            .and_then(|pair| pair.split_once('='))
            && name.starts_with("FR_")
        {
            // SAFETY: 进程刚启动，只有这一个线程
            unsafe { std::env::set_var(name, value) };
        }
    }
}

/// 这个进程带着防护、又不是已经重新启动过的：请资源管理器重新启动自己。办成了返回 true，
/// 调用方马上退出（新的进程等这个退出后才接手，见 `instance.rs` 的 `--wait-pid`）。
/// `open` 是命令行给的要打开的目录（已经换成绝对路径）。
pub fn relaunch_if_inherited(open: Option<&str>) -> bool {
    if !inherited() || std::env::args().any(|arg| arg == RELAUNCHED_FLAG) {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = exe.to_string_lossy();
    let exe = exe.strip_prefix(r"\\?\").unwrap_or(&exe);
    let args = relaunch_args(
        std::env::args().skip(1),
        open,
        std::process::id(),
        std::env::vars(),
    );
    let params = args
        .iter()
        .map(|arg| quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    match imp::shell_execute(exe, &params) {
        Ok(()) => {
            log::info!("带着 Redirection Guard 启动：已请资源管理器重新启动 Kuai");
            true
        }
        // 没有资源管理器（经 ssh 启动的、它正好在重启）：照旧带着运行
        Err(err) => {
            log::warn!("带着 Redirection Guard 启动，无法经资源管理器重新启动：{err}");
            false
        }
    }
}

/// 这个进程有没有带着防护（强制的那种）。
pub fn inherited() -> bool {
    imp::inherited()
}

/// 重新启动时带哪些参数：原来的开关照带（等谁退出、带过来的环境变量、「已经重启过」除外），
/// 要打开的目录，等这个进程退出，「已经重启过」，再把 `FR_*` 环境变量带上。
fn relaunch_args(
    args: impl Iterator<Item = String>,
    open: Option<&str>,
    pid: u32,
    vars: impl Iterator<Item = (String, String)>,
) -> Vec<String> {
    let mut out: Vec<String> = args
        .filter(|arg| {
            arg.starts_with("--")
                && !arg.starts_with(crate::instance::WAIT_PID_FLAG)
                && !arg.starts_with(ENV_FLAG)
                && arg != RELAUNCHED_FLAG
        })
        .collect();
    out.extend(open.map(str::to_string));
    out.push(format!("{}{pid}", crate::instance::WAIT_PID_FLAG));
    out.push(RELAUNCHED_FLAG.to_string());
    out.extend(
        vars.filter(|(name, _)| name.starts_with("FR_"))
            .map(|(name, value)| format!("{ENV_FLAG}{name}={value}")),
    );
    out
}

/// 按 Windows 命令行的规矩给一个参数加引号，对面拆出来还是原样。
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut slashes = 0;
    for ch in arg.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        // 引号前面的反斜杠要成对，引号自己再用一个反斜杠转义；别的字前面的反斜杠原样
        let repeat = if ch == '"' { slashes * 2 + 1 } else { slashes };
        out.push_str(&"\\".repeat(repeat));
        slashes = 0;
        out.push(ch);
    }
    // 结尾的反斜杠后面紧跟收尾的引号，也要成对
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

#[cfg(windows)]
mod imp {
    use core::ffi::c_void;

    use windows::Win32::System::Com::{
        CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize, IDispatch, IServiceProvider,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetProcessMitigationPolicy, PROCESS_MITIGATION_POLICY,
    };
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Shell::{
        IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellWindows, SID_STopLevelBrowser,
        SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
    };
    use windows::core::{BSTR, Interface as _};

    /// `ProcessRedirectionTrustPolicy`
    const REDIRECTION_TRUST: PROCESS_MITIGATION_POLICY = PROCESS_MITIGATION_POLICY(16);
    /// `PROCESS_MITIGATION_REDIRECTION_TRUST_POLICY` 的第 0 位：强制（第 1 位是只记录）
    const ENFORCE: u32 = 1;

    pub fn inherited() -> bool {
        let mut flags = 0u32;
        // SAFETY: 只写进这个 4 字节的本地变量。不认这一项的旧系统返回失败，按没带算
        let asked = unsafe {
            GetProcessMitigationPolicy(
                GetCurrentProcess(),
                REDIRECTION_TRUST,
                (&raw mut flags).cast::<c_void>(),
                size_of::<u32>(),
            )
        };
        asked.is_ok() && flags & ENFORCE != 0
    }

    /// 请资源管理器执行 `exe params`。放在另起的线程上做：这时界面还没起来，不动主线程的 COM。
    pub fn shell_execute(exe: &str, params: &str) -> Result<(), String> {
        let (exe, params) = (exe.to_string(), params.to_string());
        std::thread::spawn(move || {
            // SAFETY: 这个线程上先初始化 COM，用完再收掉；各个接口指针都由 windows 库管着
            unsafe {
                CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                    .ok()
                    .map_err(|err| err.to_string())?;
                let done = via_desktop(&exe, &params).map_err(|err| err.to_string());
                CoUninitialize();
                done
            }
        })
        .join()
        .unwrap_or_else(|_| Err("线程异常结束".to_string()))
    }

    /// 找到资源管理器的桌面窗口，拿它的自动化对象（活在资源管理器的进程里）调 `ShellExecute`。
    unsafe fn via_desktop(exe: &str, params: &str) -> windows::core::Result<()> {
        unsafe {
            let windows: IShellWindows =
                CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)?;
            let mut hwnd = 0;
            // 第一个参数是 CSIDL_DESKTOP（0）
            let desktop = windows.FindWindowSW(
                &VARIANT::from(0i32),
                &VARIANT::default(),
                SWC_DESKTOP,
                &mut hwnd,
                SWFO_NEEDDISPATCH,
            )?;
            let browser: IShellBrowser = desktop
                .cast::<IServiceProvider>()?
                .QueryService(&SID_STopLevelBrowser)?;
            let background: IDispatch = browser
                .QueryActiveShellView()?
                .GetItemObject(SVGIO_BACKGROUND)?;
            let shell: IShellDispatch2 = background
                .cast::<IShellFolderViewDual>()?
                .Application()?
                .cast()?;
            // 最后一个参数是 SW_SHOWNORMAL（1）
            shell.ShellExecute(
                &BSTR::from(exe),
                &VARIANT::from(params),
                &VARIANT::default(),
                &VARIANT::from("open"),
                &VARIANT::from(1i32),
            )
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn inherited() -> bool {
        false
    }

    pub fn shell_execute(_: &str, _: &str) -> Result<(), String> {
        Err("只有 Windows 上有".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{quote, relaunch_args};

    #[test]
    fn 参数加引号() {
        assert_eq!(quote("--minimized"), "--minimized");
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote(r"C:\My Repos\kuai"), r#""C:\My Repos\kuai""#);
        // 结尾的反斜杠后面是收尾的引号：要成对，不然引号被转义掉
        assert_eq!(quote(r"C:\My Repos\"), r#""C:\My Repos\\""#);
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote(r#"a\"b c"#), r#""a\\\"b c""#);
    }

    #[test]
    fn 重新启动带的参数() {
        let args = [
            "--minimized",
            "relative dir",
            "--wait-pid=11",
            "--env=FR_OLD=1",
        ];
        let vars = [
            ("PATH", r"C:\Windows"),
            ("FR_SETTINGS", r"D:\t e\settings.json"),
            ("FR_LOG", "1"),
        ];
        let got = relaunch_args(
            args.iter().map(|arg| arg.to_string()),
            Some(r"C:\work\relative dir"),
            42,
            vars.iter().map(|(k, v)| (k.to_string(), v.to_string())),
        );
        assert_eq!(
            got,
            [
                "--minimized",
                r"C:\work\relative dir",
                "--wait-pid=42",
                "--relaunched",
                r"--env=FR_SETTINGS=D:\t e\settings.json",
                "--env=FR_LOG=1",
            ]
        );
    }
}
