//! 交给别的程序：资源管理器、终端、VS Code、文件的默认程序。这个工具只做 git，不做编辑器。
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
const NO_WINDOW: u32 = 0x0800_0000;
/// CREATE_NEW_CONSOLE：开在一个新的控制台窗口里（没有 Windows Terminal 时的终端）
const NEW_CONSOLE: u32 = 0x0000_0010;

/// 启动失败时弹出原因；成功就什么都不做。
pub fn report(result: Result<(), String>, window: &mut Window, cx: &mut App) {
    if let Err(text) = result {
        window.push_notification(Notification::error(text).autohide(false), cx);
    }
}

fn spawn(mut cmd: Command, what: &str) -> Result<(), String> {
    cmd.spawn()
        .map(|_| ())
        .map_err(|err| tf!("无法打开{what}：{err}", what, err))
}

fn windows_path(path: &str) -> String {
    path.replace('/', "\\")
}

/// PATH 里有没有这个程序（按 Windows 的习惯补上常见的扩展名）。
fn in_path(name: &str) -> bool {
    find_in_path(name).is_some()
}

/// 这个程序在 PATH 里的完整路径。
fn find_in_path(name: &str) -> Option<PathBuf> {
    find_in_path_ext(name, &["", ".exe", ".cmd", ".bat"])
}

/// 同 [`find_in_path`]，只认这几种扩展名。
fn find_in_path_ext(name: &str, exts: &[&str]) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        exts.iter()
            .map(|ext| dir.join(format!("{name}{ext}")))
            .find(|path| path.is_file())
    })
}

/// 传给 Windows Terminal 的参数：它把分号当成几条命令之间的分隔，字面的分号要写成 `\;`。
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
            return Err(tr("未找到可用的 shell（PowerShell、cmd）。").into());
        }
        let shell = shells.swap_remove(at);
        let program = shell.program.to_string_lossy().into_owned();
        let mut command = vec![program.clone()];
        command.extend(shell.args.iter().cloned());
        let local = LocalLaunch {
            program,
            args: shell.args,
            cwd: Some(PathBuf::from(windows_path(&cwd))),
            env: Vec::new(),
            keep: None,
        };
        // 开在这台电脑的 kuaid 里：Kuai 退出、更新以后终端还在，从别的电脑连上来看到的也是它
        let connecting = app.update(cx, |app, cx| app.connect_local(cx));
        return Ok(Launch {
            connect: connecting.boxed_local(),
            key: crate::terminal::session_key(&entry.path, number),
            cwd: windows_path(&cwd),
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

/// 在资源管理器里打开所在目录并选中这个文件。
pub fn reveal_in_explorer(path: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("explorer.exe");
    // explorer 要的是 /select,"路径" 这种写法，不能让标准库把整个参数加引号
    cmd.raw_arg(format!("/select,\"{}\"", windows_path(path)));
    spawn(cmd, tr("资源管理器"))
}

/// 在资源管理器里打开目录。
pub fn open_folder(dir: &str) -> Result<(), String> {
    let mut cmd = Command::new("explorer.exe");
    cmd.arg(windows_path(dir));
    spawn(cmd, tr("资源管理器"))
}

/// 用系统给这种文件设的默认程序打开它（解决冲突时用来改文件）。直接交给 ShellExecute，不经 cmd：
/// 文件名里的 `&`、`%` 会被 cmd 当成命令分隔、变量展开，后半截被当成命令执行。
pub fn open_file(path: &str) -> Result<(), String> {
    let path = windows_path(path);
    if !Path::new(&path).is_file() {
        return Err(tf!("文件不存在：{path}", path));
    }
    shell_open(&path).map_err(|err| tf!("无法打开{what}：{err}", what = tr("此文件"), err))
}

/// ShellExecute 的「打开」。这种文件没有关联的程序时，像双击时一样弹出「打开方式」。
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
    let mut cmd = Command::new("rundll32.exe");
    cmd.args(["url.dll,FileProtocolHandler", url]);
    spawn(cmd, tr("浏览器"))
}

/// 在终端里打开本机目录：有 Windows Terminal 用它，没有就在新的控制台窗口里开 cmd。目录不拼进
/// cmd 的命令行（`cmd /c start … cd /d 目录`）：目录名里的 `&` 会被 cmd 当成命令分隔。
pub fn open_terminal(dir: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let dir = windows_path(dir);
    let mut wt = Command::new("wt.exe");
    wt.args(["-d", &wt_arg(&dir)]);
    if wt.spawn().is_ok() {
        return Ok(());
    }
    let mut cmd = Command::new("cmd.exe");
    cmd.arg("/k").current_dir(&dir).creation_flags(NEW_CONSOLE);
    spawn(cmd, tr("终端"))
}

/// 在终端里登录服务器并进到仓库目录。指定了私钥文件的带上；密码在终端里自己输。
pub fn open_terminal_ssh(server: &Server, dir: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    if !in_path("ssh") {
        return Err(tr(
            "PATH 中未找到 ssh。请在「设置 › 系统 › 可选功能」中安装「OpenSSH 客户端」后重试。",
        )
        .into());
    }
    // 目录用单引号包起来，里面的单引号换成 '\''
    let quoted = format!("'{}'", dir.replace('\'', r"'\''"));
    let remote = format!("cd {quoted} && exec \"$SHELL\" -l");
    let mut ssh = vec!["ssh", "-t"];
    if let Some(key) = server.key_file() {
        ssh.extend(["-i", key]);
    }
    ssh.extend(["--", &server.destination, &remote]);
    let mut wt = Command::new("wt.exe");
    wt.args(ssh.iter().map(|arg| wt_arg(arg)));
    if wt.spawn().is_ok() {
        return Ok(());
    }
    // 没有 Windows Terminal：在新的控制台窗口里直接起 ssh，不经 `cmd /c start`（目录里的 `&`、
    // 引号会被 cmd 解释）
    let mut cmd = Command::new("ssh.exe");
    cmd.args(&ssh[1..]).creation_flags(NEW_CONSOLE);
    spawn(cmd, tr("终端"))
}

fn no_code() -> &'static str {
    tr("未找到 VS Code 命令行工具（code）。请安装 VS Code 时勾选「添加到 PATH」，并重启 Kuai。")
}

/// VS Code 的命令行工具 `code.cmd`。PATH 里它旁边还有一个没有扩展名的 `code`，是给 Git Bash 用的
/// shell 脚本，起不了。
fn code_cmd() -> Result<PathBuf, String> {
    find_in_path_ext("code", &[".cmd", ".exe"]).ok_or_else(|| no_code().to_string())
}

/// 起 VS Code 的命令行工具。直接起 `code.cmd`，不写成 `cmd /c code …`：标准库起批处理文件时会给
/// 参数加引号、转义 `%`，路径里的 `&` 不会被 cmd 当成命令分隔。
fn run_code(args: &[&str]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new(code_cmd()?);
    cmd.args(args).creation_flags(NO_WINDOW);
    spawn(cmd, " VS Code")
}

/// 用 VS Code 打开本机的目录或文件。
pub fn open_vscode(path: &str) -> Result<(), String> {
    run_code(&[&windows_path(path)])
}

/// 用 VS Code 的 Remote-SSH 打开服务器上的目录或文件。
pub fn open_vscode_remote(destination: &str, path: &str) -> Result<(), String> {
    // Remote-SSH 不认 ssh:// 的写法，端口要写成 user@host:port
    let host = Target::parse(destination).authority();
    run_code(&["--remote", &format!("ssh-remote+{host}"), path])
}
