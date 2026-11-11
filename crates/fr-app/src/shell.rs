//! 交给别的程序：资源管理器、终端、VS Code、文件的默认程序。这个工具只做 git，不做编辑器。
//!
//! 都是启动了就不管。启动不了（没装、不在 PATH 里）时返回给用户看的原因，由 [`report`] 弹出来。

use fr_core::i18n::tr;
use fr_core::tf;
use std::path::Path;
use std::process::Command;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, Window};

use crate::settings::{Server, Target};

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
        .map_err(|err| tf!("打不开{what}：{err}", what, err))
}

fn windows_path(path: &str) -> String {
    path.replace('/', "\\")
}

/// PATH 里有没有这个程序（按 Windows 的习惯补上常见的扩展名）。
fn in_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        ["", ".exe", ".cmd", ".bat"]
            .iter()
            .any(|ext| dir.join(format!("{name}{ext}")).is_file())
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

/// 用系统给这种文件设的默认程序打开它（解决冲突时用来改文件）。
pub fn open_file(path: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let path = windows_path(path);
    if !Path::new(&path).is_file() {
        return Err(tf!("文件不在了：{path}", path));
    }
    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/c", "start", "", &path])
        .creation_flags(NO_WINDOW);
    spawn(cmd, tr("这个文件"))
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
            "PATH 里没有 ssh。在「设置 › 系统 › 可选功能」里装上「OpenSSH 客户端」再试。",
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
    tr("找不到 VS Code 的命令行（code）。安装 VS Code 时勾上「添加到 PATH」，然后重开本程序。")
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
