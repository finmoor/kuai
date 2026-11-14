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
use crate::terminal::{Launch, LocalLaunch, RemoteLaunch};

/// CREATE_NO_WINDOW：不弹出控制台窗口
const NO_WINDOW: u32 = 0x0800_0000;

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
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        ["", ".exe", ".cmd", ".bat"]
            .iter()
            .map(|ext| dir.join(format!("{name}{ext}")))
            .find(|path| path.is_file())
    })
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

/// 内置终端要跑什么：本机仓库是设置里选的 shell（没选、选的找不到了就用第一个找得到的）；
/// 服务器上的仓库是开在 kuaid 里的终端，经这台服务器的连接接上（没连着就先连）。
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
        return Ok(Launch::Local(LocalLaunch {
            program: shell.program.to_string_lossy().into_owned(),
            args: shell.args,
            cwd: Some(PathBuf::from(windows_path(&cwd))),
            env: Vec::new(),
            keep: None,
        }));
    };
    if app.read(cx).server(server).is_none() {
        return Err(tr("未找到该服务器的设置").into());
    }
    let connecting = app.update(cx, |app, cx| app.connect(server, cx));
    Ok(Launch::Remote(RemoteLaunch {
        connect: connecting.boxed_local(),
        key: crate::terminal::session_key(&entry.path, number),
        cwd,
    }))
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

/// 用系统给这种文件设的默认程序打开它（解决冲突时用来改文件）。
pub fn open_file(path: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let path = windows_path(path);
    if !Path::new(&path).is_file() {
        return Err(tf!("文件不存在：{path}", path));
    }
    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/c", "start", "", &path])
        .creation_flags(NO_WINDOW);
    spawn(cmd, tr("此文件"))
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

/// 在终端里打开本机目录：有 Windows Terminal 用它，没有就用 cmd。
pub fn open_terminal(dir: &str) -> Result<(), String> {
    let dir = windows_path(dir);
    let mut wt = Command::new("wt.exe");
    wt.args(["-d", &dir]);
    if wt.spawn().is_ok() {
        return Ok(());
    }
    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/c", "start", "", "cmd.exe", "/k", "cd", "/d", &dir]);
    spawn(cmd, tr("终端"))
}

/// 在终端里登录服务器并进到仓库目录。指定了私钥文件的带上；密码在终端里自己输。
pub fn open_terminal_ssh(server: &Server, dir: &str) -> Result<(), String> {
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
    wt.args(&ssh);
    if wt.spawn().is_ok() {
        return Ok(());
    }
    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/c", "start", ""]).args(&ssh);
    spawn(cmd, tr("终端"))
}

fn no_code() -> &'static str {
    tr("未找到 VS Code 命令行工具（code）。请在安装 VS Code 时勾选「添加到 PATH」，然后重启 Kuai。")
}

/// 用 VS Code 打开本机的目录或文件。`code` 是个 cmd 脚本，要经 cmd 启动。
pub fn open_vscode(path: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    if !in_path("code") {
        return Err(no_code().into());
    }
    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/c", "code", &windows_path(path)])
        .creation_flags(NO_WINDOW);
    spawn(cmd, " VS Code")
}

/// 用 VS Code 的 Remote-SSH 打开服务器上的目录或文件。
pub fn open_vscode_remote(destination: &str, path: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    if !in_path("code") {
        return Err(no_code().into());
    }
    // Remote-SSH 不认 ssh:// 的写法，端口要写成 user@host:port
    let host = Target::parse(destination).authority();
    let mut cmd = Command::new("cmd.exe");
    cmd.args([
        "/c",
        "code",
        "--remote",
        &format!("ssh-remote+{host}"),
        path,
    ])
    .creation_flags(NO_WINDOW);
    spawn(cmd, " VS Code")
}
