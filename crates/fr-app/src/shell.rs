//! 交给别的程序：资源管理器（Linux 上是文件管理器）、终端、VS Code、文件的默认程序。这个工具只做
//! git，不做编辑器。
//!
//! 都是启动了就不管。启动不了（没装、不在 PATH 里）时返回给用户看的原因，由 [`report`] 弹出来。

use fr_core::i18n::tr;
use fr_core::tf;
use std::path::{Path, PathBuf};
use std::process::Command;

use futures::FutureExt as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, Entity, Window};

use crate::app_state::AppState;
use crate::settings::{RepoEntry, Server, Target};
use crate::terminal::{Launch, LocalLaunch};

/// CREATE_NO_WINDOW：不弹出控制台窗口
#[cfg(windows)]
const NO_WINDOW: u32 = 0x0800_0000;
/// CREATE_NEW_CONSOLE：开在一个新的控制台窗口里（没有 Windows Terminal 时的终端）
#[cfg(windows)]
const NEW_CONSOLE: u32 = 0x0000_0010;

/// 启动失败时弹出原因；成功就什么都不做。
pub fn report(result: Result<(), String>, window: &mut Window, cx: &mut App) {
    if let Err(text) = result {
        window.push_notification(Notification::error(text).autohide(false), cx);
    }
}

fn spawn(mut cmd: Command, what: &str) -> Result<(), String> {
    let child = cmd
        .spawn()
        .map_err(|err| tf!("无法打开{what}：{err}", what, err))?;
    reap(child);
    Ok(())
}

/// 启动了不管的程序退出后要有人收尸：Unix 上不等它，它退出后一直留在进程表里（僵尸进程），
/// 直到 Kuai 退出。在另起的线程上等。Windows 上没有这回事。
fn reap(child: std::process::Child) {
    #[cfg(unix)]
    {
        let mut child = child;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    #[cfg(not(unix))]
    drop(child);
}

/// 路径换成这个系统的写法：Windows 上斜杠换成反斜杠，别的系统不动。
fn native_path(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.to_string()
    }
}

/// PATH 里有没有这个程序（Windows 上按习惯补上常见的扩展名）。
fn in_path(name: &str) -> bool {
    find_in_path(name).is_some()
}

/// 这个程序在 PATH 里的完整路径。
fn find_in_path(name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    const EXTS: &[&str] = &["", ".exe", ".cmd", ".bat"];
    #[cfg(not(windows))]
    const EXTS: &[&str] = &[""];
    find_in_path_ext(name, EXTS)
}

/// 找一个程序：给的是存在的路径就用它，否则在 PATH 里找（外部比较、合并工具用）。
pub(crate) fn find_program(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.is_absolute() || name.contains(['/', '\\']) {
        return is_program(path).then(|| path.to_path_buf());
    }
    find_in_path(name)
}

/// VS Code 的命令行工具在哪（外部比较、合并工具用）。
pub(crate) fn code_program() -> Option<PathBuf> {
    code_cmd().ok()
}

/// 同 [`find_in_path`]，只认这几种扩展名。
fn find_in_path_ext(name: &str, exts: &[&str]) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        exts.iter()
            .map(|ext| dir.join(format!("{name}{ext}")))
            .find(|path| is_program(path))
    })
}

/// 是不是可以运行的程序：Unix 上还要有执行权限。
#[cfg(unix)]
fn is_program(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_program(path: &Path) -> bool {
    path.is_file()
}

/// 传给 Windows Terminal 的参数：它把分号当成几条命令之间的分隔，字面的分号要写成 `\;`。
#[cfg(windows)]
fn wt_arg(arg: &str) -> String {
    arg.replace(';', "\\;")
}

/// 内置终端在本机仓库里可以开的一种 shell。
pub struct LocalShell {
    /// 存在设置里的名字
    pub id: &'static str,
    pub label: &'static str,
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// 这台电脑上找得到的 shell，按自动挑选的先后排：PowerShell 7、Windows PowerShell、
/// 命令提示符、Git Bash。
#[cfg(windows)]
pub fn local_shells() -> Vec<LocalShell> {
    let mut shells = Vec::new();
    let mut add = |id, label, program: Option<PathBuf>, args: &[&str]| {
        if let Some(program) = program {
            shells.push(LocalShell {
                id,
                label,
                program,
                args: args.iter().map(|arg| arg.to_string()).collect(),
            });
        }
    };
    add("pwsh", "PowerShell 7", find_in_path("pwsh"), &["-NoLogo"]);
    add(
        "powershell",
        "Windows PowerShell",
        find_in_path("powershell"),
        &["-NoLogo"],
    );
    add("cmd", tr("命令提示符"), find_in_path("cmd"), &[]);
    // Git Bash：git 装在 …\Git\cmd\git.exe，bash 在旁边的 bin 里
    let bash = find_in_path("git")
        .and_then(|git| Some(git.parent()?.parent()?.join("bin").join("bash.exe")))
        .filter(|bash| bash.is_file());
    add("git-bash", "Git Bash", bash, &["--login", "-i"]);
    shells
}

/// 这台电脑上找得到的 shell：登录 shell（`$SHELL`）排第一，再是 bash、zsh、fish。都不带参数：
/// 开在伪终端里，shell 自己就是交互式的，和桌面上的终端程序一样不当登录 shell。
#[cfg(not(windows))]
pub fn local_shells() -> Vec<LocalShell> {
    const KNOWN: [&str; 5] = ["bash", "zsh", "fish", "nu", "sh"];
    let login = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|path| is_program(path));
    let login_name = login
        .as_deref()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    let mut shells = Vec::new();
    match (
        KNOWN.into_iter().find(|name| Some(*name) == login_name),
        &login,
    ) {
        (Some(name), Some(path)) => shells.push(LocalShell {
            id: name,
            label: name,
            program: path.clone(),
            args: Vec::new(),
        }),
        // 不认识的登录 shell 也排第一
        (None, Some(path)) => shells.push(LocalShell {
            id: "login",
            label: tr("登录 shell"),
            program: path.clone(),
            args: Vec::new(),
        }),
        _ => {}
    }
    for name in KNOWN {
        if shells.iter().any(|shell| shell.id == name) {
            continue;
        }
        if let Some(program) = find_in_path(name) {
            shells.push(LocalShell {
                id: name,
                label: name,
                program,
                args: Vec::new(),
            });
        }
    }
    shells
}

/// 内置终端要跑什么：本机仓库是设置里选的 shell（没选、选的找不到了就用第一个找得到的），开在这台
/// 电脑的 kuaid 里，kuaid 用不了时开在 Kuai 进程里；服务器上的仓库是开在服务器的 kuaid 里的终端，
/// 经这台服务器的连接接上（没连着就先连）。
///
/// `number`：这个仓库的第几个终端（从 1 起）。服务器上的终端靠它起名字，下次打开这个仓库时
/// 接上的还是原来那个。`dir`：开在仓库里的哪个目录（相对仓库根，空的是仓库根）。
pub fn terminal_launch(
    entry: &RepoEntry,
    app: &Entity<AppState>,
    number: u32,
    dir: &str,
    cx: &mut App,
) -> Result<Launch, String> {
    let root = entry.path.trim_end_matches(['/', '\\']);
    let cwd = match dir.is_empty() {
        true => entry.path.clone(),
        false => format!("{root}/{dir}"),
    };
    let Some(server) = &entry.server else {
        let preferred = app.read(cx).settings.terminal_shell.clone();
        let mut shells = local_shells();
        let at = shells
            .iter()
            .position(|shell| shell.id == preferred)
            .unwrap_or(0);
        if shells.is_empty() {
            return Err(no_shell().into());
        }
        let shell = shells.swap_remove(at);
        let program = shell.program.to_string_lossy().into_owned();
        let mut command = vec![program.clone()];
        command.extend(shell.args.iter().cloned());
        let local = LocalLaunch {
            program,
            args: shell.args,
            cwd: Some(PathBuf::from(native_path(&cwd))),
            env: Vec::new(),
            keep: None,
        };
        // 开在这台电脑的 kuaid 里：Kuai 退出、更新以后终端还在，从别的电脑连上来看到的也是它
        let connecting = app.update(cx, |app, cx| app.connect_local(cx));
        return Ok(Launch {
            connect: connecting.boxed_local(),
            key: crate::terminal::session_key(&entry.path, number),
            cwd: native_path(&cwd),
            shell: command,
            fallback: Some(local),
        });
    };
    // 经别的服务器连的（A>B）：连接设置在那台服务器上
    if !app.read(cx).knows_server(server) {
        return Err(tr("未找到该主机的设置").into());
    }
    let connecting = app.update(cx, |app, cx| app.connect(server, cx));
    Ok(Launch {
        connect: connecting.boxed_local(),
        key: crate::terminal::session_key(&entry.path, number),
        cwd,
        shell: Vec::new(),
        fallback: None,
    })
}

fn no_shell() -> &'static str {
    if cfg!(windows) {
        tr("未找到可用的 shell（PowerShell、cmd）。")
    } else {
        tr("未找到可用的 shell（bash、zsh、fish）。")
    }
}

/// 资源管理器（Linux 上是文件管理器），用在「无法打开…」里。
fn file_manager() -> &'static str {
    if cfg!(windows) {
        tr("资源管理器")
    } else {
        tr("文件管理器")
    }
}

/// 菜单里的「在资源管理器中显示」（选中这个文件）。
pub fn reveal_label() -> &'static str {
    if cfg!(windows) {
        tr("在资源管理器中显示")
    } else {
        tr("在文件管理器中显示")
    }
}

/// 菜单里的「在资源管理器中打开」（打开这个目录）。
pub fn open_folder_label() -> &'static str {
    if cfg!(windows) {
        tr("在资源管理器中打开")
    } else {
        tr("在文件管理器中打开")
    }
}

/// 在资源管理器里打开所在目录并选中这个文件。
#[cfg(windows)]
pub fn reveal_in_explorer(path: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("explorer.exe");
    // explorer 要的是 /select,"路径" 这种写法，不能让标准库把整个参数加引号
    cmd.raw_arg(format!("/select,\"{}\"", native_path(path)));
    spawn(cmd, file_manager())
}

/// 在文件管理器里打开所在目录并选中这个文件：经 D-Bus 请文件管理器来做（`FileManager1` 接口，
/// Nautilus、Dolphin、Nemo、Thunar 等都有）；没有这个接口时只打开所在目录。D-Bus 的回话可能要等
/// 文件管理器启动，在另起的线程上等。
#[cfg(not(windows))]
pub fn reveal_in_explorer(path: &str) -> Result<(), String> {
    let path = PathBuf::from(path);
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.clone());
    if !in_path("gdbus") {
        return open_folder(&dir.to_string_lossy());
    }
    std::thread::spawn(move || {
        let items = format!("['{}']", file_uri(&path));
        let shown = Command::new("gdbus")
            .args([
                "call",
                "--session",
                "--timeout",
                "10",
                "--dest",
                "org.freedesktop.FileManager1",
                "--object-path",
                "/org/freedesktop/FileManager1",
                "--method",
                "org.freedesktop.FileManager1.ShowItems",
                &items,
                "",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !shown && let Err(err) = open_folder(&dir.to_string_lossy()) {
            log::warn!("{err}");
        }
    });
    Ok(())
}

/// 本机路径写成 `file://` 网址：除了字母、数字、`-._~/` 都按字节转成 `%XX`（单引号也转，放进
/// gdbus 的参数里不会断开）。
#[cfg(not(windows))]
fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

/// 在资源管理器（Linux 上是默认的文件管理器）里打开目录。
pub fn open_folder(dir: &str) -> Result<(), String> {
    #[cfg(windows)]
    let cmd = {
        let mut cmd = Command::new("explorer.exe");
        cmd.arg(native_path(dir));
        cmd
    };
    #[cfg(not(windows))]
    let cmd = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(dir);
        cmd
    };
    spawn(cmd, file_manager())
}

/// 用系统给这种文件设的默认程序打开它（解决冲突时用来改文件、终端里按住 Ctrl 单击的文件），打不开
/// 弹通知。Windows 上直接交给 ShellExecute，不经 cmd：文件名里的 `&`、`%` 会被 cmd 当成命令分隔、
/// 变量展开，后半截被当成命令执行；Linux 上交给 `xdg-open`。
///
/// 在另起的线程上打开：ShellExecute 要等打开的程序（浏览器、Office）回话，等的时候处理窗口消息，在
/// 界面线程上调的话界面库会重入（日志里一串「RefCell already borrowed」，那一阵画不出来）；
/// `xdg-open` 要等它退出才知道打开没有。
///
/// 程序、脚本、快捷方式（[`runs_code`]）的「默认程序」就是运行它：先问一下。想看脚本写了什么却点了
/// 「用默认程序打开」，或者主机上下载下来的程序，都不该一点就运行；下载的副本也没有「来自网络」的
/// 标记，SmartScreen 不拦。
pub fn open_file(path: &str, window: &mut Window, cx: &mut App) {
    let path = native_path(path);
    if !Path::new(&path).is_file() {
        return report(Err(tf!("文件不存在：{path}", path)), window, cx);
    }
    if runs_code(&path) {
        let name = Path::new(&path)
            .file_name()
            .map_or_else(|| path.clone(), |name| name.to_string_lossy().into_owned());
        let description = tf!(
            "{name} 是程序或脚本，用默认程序打开会直接运行它。请只运行信任的文件。",
            name
        );
        return ask(
            (tr("运行此文件？"), description, tr("运行")),
            move |window, cx| launch_file(path, window, cx),
            window,
            cx,
        );
    }
    launch_file(path, window, cx);
}

/// 默认程序就是运行它的文件：Windows 上的程序、脚本、快捷方式，Linux 上的桌面启动文件（`.desktop`，
/// `xdg-open` 会照里面写的命令运行）。
pub fn runs_code(path: &str) -> bool {
    let ext = Path::new(path)
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    #[cfg(windows)]
    let listed = [
        "exe",
        "com",
        "bat",
        "cmd",
        "scr",
        "pif",
        "msi",
        "msp",
        "cpl",
        "msc",
        "lnk",
        "url",
        "vbs",
        "vbe",
        "js",
        "jse",
        "wsf",
        "wsh",
        "hta",
        "reg",
        "jar",
        "appref-ms",
        "application",
        "scf",
    ];
    #[cfg(not(windows))]
    let listed = ["desktop"];
    listed.contains(&ext.as_str())
}

/// 问一句：`title`、`description`、确定按钮上的字；点了确定才做 `action`。
pub fn ask(
    (title, description, ok): (&'static str, String, &'static str),
    action: impl FnOnce(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
    use gpui_kit::component::dialog::DialogFooter;
    use gpui_kit::{ClickEvent, ParentElement as _};
    // 对话框每次重画都会调一遍下面的闭包，要做的事只能取走一次
    let job = std::rc::Rc::new(std::cell::Cell::new(Some(action)));
    window.open_alert_dialog(cx, move |alert, _, _| {
        let job = job.clone();
        let go = move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            window.close_dialog(cx);
            if let Some(action) = job.take() {
                action(window, cx);
            }
        };
        alert
            .title(title)
            .description(description.clone())
            .on_ok(|_, _, _| false)
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("cancel")
                            .label(tr("取消"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("ok")
                            .label(ok)
                            .with_variant(ButtonVariant::Primary)
                            .on_click(go),
                    ),
            )
    });
}

/// [`open_file`] 真正去打开（已经问过了）。
fn launch_file(path: String, window: &mut Window, cx: &mut App) {
    let (send, done) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = send.send(system_open(&path));
    });
    window
        .spawn(cx, async move |cx| {
            if let Ok(Err(err)) = done.await {
                let text = tf!("无法打开{what}：{err}", what = tr("此文件"), err);
                cx.update(|window, cx| report(Err(text), window, cx)).ok();
            }
        })
        .detach();
}

/// 用默认程序打开文件（在 [`open_file`] 另起的线程上）。
#[cfg(windows)]
fn system_open(path: &str) -> Result<(), String> {
    use windows::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
    };
    // SAFETY: 这个线程上先初始化 COM（ShellExecute 可能交给外壳扩展，要单线程套间），用完再收掉
    unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let opened = shell_open(path);
        if com.is_ok() {
            CoUninitialize();
        }
        opened
    }
}

/// 用默认程序打开文件：`xdg-open` 退出码 3、4 是没有能打开的程序、打开失败。
#[cfg(not(windows))]
fn system_open(path: &str) -> Result<(), String> {
    let status = Command::new("xdg-open")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|err| err.to_string())?;
    match status.code() {
        Some(0) => Ok(()),
        Some(3 | 4) => Err(tr("没有可以打开此类文件的程序").into()),
        _ => Err(status.to_string()),
    }
}

/// ShellExecute 的「打开」。这种文件没有关联的程序时，像双击时一样弹出「打开方式」。
#[cfg(windows)]
fn shell_open(path: &str) -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};
    let file = HSTRING::from(path);
    // 返回值大于 32 是成功，否则是错误号（SE_ERR_*）
    let run = |verb: PCWSTR| {
        let done = unsafe {
            ShellExecuteW(
                None,
                verb,
                &file,
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        done.0 as isize
    };
    // SE_ERR_NOASSOC
    const NO_ASSOC: isize = 31;
    let mut code = run(w!("open"));
    if code == NO_ASSOC {
        code = run(w!("openas"));
    }
    match code {
        33.. => Ok(()),
        // SE_ERR_ASSOCINCOMPLETE、SE_ERR_NOASSOC：错误号和系统的错误码对不上，自己说
        27 | NO_ASSOC => Err(tr("没有可以打开此类文件的程序").into()),
        code => Err(std::io::Error::from_raw_os_error(code as i32).to_string()),
    }
}

/// 在默认浏览器里打开网址（只开 https 的）。不经 cmd：网址里的 `&` 会被 cmd 当成命令分隔。
pub fn open_url(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err(tf!("不支持打开此网址：{url}", url));
    }
    browse(url)
}

/// 在默认浏览器里打开终端里按住 Ctrl 单击的网址：http 的也开（`http://localhost:3000` 这种），
/// 别的协议不开。
pub fn open_link(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("https://") && !lower.starts_with("http://") {
        return Err(tf!("不支持打开此网址：{url}", url));
    }
    browse(url)
}

fn browse(url: &str) -> Result<(), String> {
    #[cfg(windows)]
    let cmd = {
        let mut cmd = Command::new("rundll32.exe");
        cmd.args(["url.dll,FileProtocolHandler", url]);
        cmd
    };
    #[cfg(not(windows))]
    let cmd = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };
    spawn(cmd, tr("浏览器"))
}

/// 在终端里打开本机目录：有 Windows Terminal 用它，没有就在新的控制台窗口里开 cmd。目录不拼进
/// cmd 的命令行（`cmd /c start … cd /d 目录`）：目录名里的 `&` 会被 cmd 当成命令分隔。
#[cfg(windows)]
pub fn open_terminal(dir: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let dir = native_path(dir);
    let mut wt = Command::new("wt.exe");
    wt.args(["-d", &wt_arg(&dir)]);
    if wt.spawn().is_ok() {
        return Ok(());
    }
    let mut cmd = Command::new("cmd.exe");
    cmd.arg("/k").current_dir(&dir).creation_flags(NEW_CONSOLE);
    spawn(cmd, tr("终端"))
}

/// Linux 上的终端程序，按先后试：系统设的默认终端（Debian 系的 `x-terminal-emulator`），再是常见
/// 的几种。第二项是在终端里执行命令时命令前面加的参数（空的是直接跟命令）。
#[cfg(not(windows))]
const TERMINALS: [(&str, &str); 10] = [
    ("x-terminal-emulator", "-e"),
    ("gnome-terminal", "--"),
    ("ptyxis", "--"),
    ("kgx", "--"),
    ("konsole", "-e"),
    ("xfce4-terminal", "-x"),
    ("alacritty", "-e"),
    ("kitty", ""),
    ("foot", ""),
    ("xterm", "-e"),
];

/// 在终端程序里开，`command` 空的是开 shell。都靠工作目录进到目录里：这些终端都在启动时所在的
/// 目录里开 shell。
#[cfg(not(windows))]
fn run_in_terminal(dir: Option<&str>, command: &[&str]) -> Result<(), String> {
    let mut last = None;
    for (program, exec) in TERMINALS {
        let mut cmd = Command::new(program);
        if !command.is_empty() {
            if !exec.is_empty() {
                cmd.arg(exec);
            }
            cmd.args(command);
        }
        if let Some(dir) = dir {
            cmd.current_dir(dir);
        }
        match cmd.spawn() {
            Ok(child) => {
                reap(child);
                return Ok(());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => last = Some(err),
        }
    }
    Err(match last {
        Some(err) => tf!("无法打开{what}：{err}", what = tr("终端"), err),
        None => tr("未找到终端程序（如 gnome-terminal、konsole、xterm）。").into(),
    })
}

/// 在终端里打开本机目录。
#[cfg(not(windows))]
pub fn open_terminal(dir: &str) -> Result<(), String> {
    run_in_terminal(Some(dir), &[])
}

/// ssh 不在 PATH 里时的说明。
fn no_ssh() -> &'static str {
    if cfg!(windows) {
        tr("PATH 中未找到 ssh。请在「设置 › 系统 › 可选功能」中安装「OpenSSH 客户端」后重试。")
    } else {
        tr("PATH 中未找到 ssh。请安装 OpenSSH 客户端（如 openssh-client 软件包）后重试。")
    }
}

/// 在终端里登录服务器并进到仓库目录。指定了私钥文件的带上；密码在终端里自己输。
pub fn open_terminal_ssh(server: &Server, dir: &str) -> Result<(), String> {
    if !in_path("ssh") {
        return Err(no_ssh().into());
    }
    // 目录用单引号包起来，里面的单引号换成 '\''
    let quoted = format!("'{}'", dir.replace('\'', r"'\''"));
    let remote = format!("cd {quoted} && exec \"$SHELL\" -l");
    let mut ssh = vec!["ssh", "-t"];
    if let Some(key) = server.key_file() {
        ssh.extend(["-i", key]);
    }
    ssh.extend(["--", &server.destination, &remote]);
    ssh_in_terminal(&ssh)
}

/// 在终端程序里跑 `ssh`（`ssh` 是整条命令）：有 Windows Terminal 用它，没有就在新的控制台窗口里
/// 直接起 ssh，不经 `cmd /c start`（目录里的 `&`、引号会被 cmd 解释）。
#[cfg(windows)]
fn ssh_in_terminal(ssh: &[&str]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let mut wt = Command::new("wt.exe");
    wt.args(ssh.iter().map(|arg| wt_arg(arg)));
    if wt.spawn().is_ok() {
        return Ok(());
    }
    let mut cmd = Command::new("ssh.exe");
    cmd.args(&ssh[1..]).creation_flags(NEW_CONSOLE);
    spawn(cmd, tr("终端"))
}

#[cfg(not(windows))]
fn ssh_in_terminal(ssh: &[&str]) -> Result<(), String> {
    run_in_terminal(None, ssh)
}

fn no_code() -> &'static str {
    if cfg!(windows) {
        tr("未找到 VS Code 命令行工具（code）。请安装 VS Code 时勾选「添加到 PATH」，并重启 Kuai。")
    } else {
        tr("未找到 VS Code 命令行工具（code）。请安装 VS Code，并确认 code 在 PATH 中。")
    }
}

/// VS Code 的命令行工具：Windows 上是 `code.cmd`（PATH 里它旁边还有一个没有扩展名的 `code`，是给
/// Git Bash 用的 shell 脚本，起不了），别的系统上是 `code`。
fn code_cmd() -> Result<PathBuf, String> {
    #[cfg(windows)]
    let found = find_in_path_ext("code", &[".cmd", ".exe"]);
    #[cfg(not(windows))]
    let found = find_in_path("code");
    found.ok_or_else(|| no_code().to_string())
}

/// 起 VS Code 的命令行工具。Windows 上直接起 `code.cmd`，不写成 `cmd /c code …`：标准库起批处理
/// 文件时会给参数加引号、转义 `%`，路径里的 `&` 不会被 cmd 当成命令分隔。
fn run_code(args: &[&str]) -> Result<(), String> {
    let mut cmd = Command::new(code_cmd()?);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(NO_WINDOW);
    }
    spawn(cmd, " VS Code")
}

/// 用 VS Code 打开本机的目录或文件。
pub fn open_vscode(path: &str) -> Result<(), String> {
    run_code(&[&native_path(path)])
}

/// 用 VS Code 的 Remote-SSH 打开服务器上的目录或文件。
pub fn open_vscode_remote(destination: &str, path: &str) -> Result<(), String> {
    // Remote-SSH 不认 ssh:// 的写法，端口要写成 user@host:port
    let host = Target::parse(destination).authority();
    run_code(&["--remote", &format!("ssh-remote+{host}"), path])
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn 本机路径写成_file_网址() {
        let uri = file_uri(Path::new("/home/me/有 空格/it's&a%b.txt"));
        assert_eq!(
            uri,
            "file:///home/me/%E6%9C%89%20%E7%A9%BA%E6%A0%BC/it%27s%26a%25b.txt"
        );
        assert!(!uri.contains('\''));
    }

    #[test]
    fn 登录_shell_排第一() {
        let shells = local_shells();
        let Some(login) = std::env::var_os("SHELL").filter(|path| is_program(Path::new(path)))
        else {
            return;
        };
        assert_eq!(shells[0].program, PathBuf::from(login));
        // 不重复
        let mut ids: Vec<_> = shells.iter().map(|shell| shell.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), shells.len());
    }
}
