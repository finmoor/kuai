//! 交给别的程序：资源管理器、终端、VS Code。这个工具只做 git，不做编辑器。
//!
//! 都是启动了就不管；启动不了记一条日志。

use std::process::Command;

/// CREATE_NO_WINDOW：不弹出控制台窗口
const NO_WINDOW: u32 = 0x0800_0000;

fn spawn(mut cmd: Command, what: &str) {
    if let Err(err) = cmd.spawn() {
        log::warn!("打不开{what}：{err}");
    }
}

fn windows_path(path: &str) -> String {
    path.replace('/', "\\")
}

/// 在资源管理器里打开所在目录并选中这个文件。
pub fn reveal_in_explorer(path: &str) {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("explorer.exe");
    // explorer 要的是 /select,"路径" 这种写法，不能让标准库把整个参数加引号
    cmd.raw_arg(format!("/select,\"{}\"", windows_path(path)));
    spawn(cmd, "资源管理器");
}

/// 在资源管理器里打开目录。
pub fn open_folder(dir: &str) {
    let mut cmd = Command::new("explorer.exe");
    cmd.arg(windows_path(dir));
    spawn(cmd, "资源管理器");
}

/// 在终端里打开本机目录：有 Windows Terminal 用它，没有就用 cmd。
pub fn open_terminal(dir: &str) {
    let dir = windows_path(dir);
    let mut wt = Command::new("wt.exe");
    wt.args(["-d", &dir]);
    if wt.spawn().is_err() {
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/c", "start", "", "cmd.exe", "/k", "cd", "/d", &dir]);
        spawn(cmd, "终端");
    }
}

/// 在终端里登录服务器并进到仓库目录。
pub fn open_terminal_ssh(destination: &str, dir: &str) {
    // 目录用单引号包起来，里面的单引号换成 '\''
    let quoted = format!("'{}'", dir.replace('\'', r"'\''"));
    let remote = format!("cd {quoted} && exec \"$SHELL\" -l");
    let mut wt = Command::new("wt.exe");
    wt.args(["ssh", "-t", "--", destination, &remote]);
    if wt.spawn().is_err() {
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/c", "start", "", "ssh", "-t", "--", destination, &remote]);
        spawn(cmd, "终端");
    }
}

/// 用 VS Code 打开本机目录。`code` 是个 cmd 脚本，要经 cmd 启动。
pub fn open_vscode(dir: &str) {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/c", "code", &windows_path(dir)])
        .creation_flags(NO_WINDOW);
    spawn(cmd, " VS Code");
}

/// 用 VS Code 的 Remote-SSH 打开服务器上的目录。
pub fn open_vscode_remote(destination: &str, dir: &str) {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("cmd.exe");
    cmd.args([
        "/c",
        "code",
        "--remote",
        &format!("ssh-remote+{destination}"),
        dir,
    ])
    .creation_flags(NO_WINDOW);
    spawn(cmd, " VS Code");
}
