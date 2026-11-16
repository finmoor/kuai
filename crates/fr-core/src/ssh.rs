//! 起 ssh 用的那几样：系统的 ssh 在哪、每次都带的选项、登录一台服务器的选项。
//! 客户端连服务器、kuaid 替客户端连它配置的服务器（协议 10，见 kuaid 的 `relay`）共用。

use std::path::PathBuf;
use std::process::Command;

/// 每个 ssh 都带的：第一次连的主机自动记进 known_hosts（记录过的密钥对不上仍然拒绝）；保活
pub const KEEP_OPTIONS: &[&str] = &[
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=4",
];

/// 经跳板机连接时每个 ssh 都带的：登录成功时打一行「Authenticated to …」，用来看进度
pub const STAGE_OPTIONS: &[&str] = &["-o", "LogLevel=VERBOSE"];

/// 系统的 ssh：Windows 上是 `System32\OpenSSH\ssh.exe`（不用 PATH 里别的，比如 Git 带的那个）。
pub fn program() -> PathBuf {
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        let builtin = PathBuf::from(root).join(r"System32\OpenSSH\ssh.exe");
        if builtin.is_file() {
            return builtin;
        }
    }
    PathBuf::from("ssh")
}

/// Windows 上起进程时不弹出控制台窗口。
pub fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW
        cmd.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// 登录一台服务器的 ssh 选项：转发 ssh-agent、私钥、密码登录、要密码时怎么问。
/// `askpass`：要密码、口令时有人可以问（设了 askpass 程序），只问一次；没有就只用密钥，要密码直接失败。
pub fn login_args(
    identity_file: Option<&str>,
    password_auth: bool,
    forward_agent: bool,
    askpass: bool,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut add = |items: &[&str]| args.extend(items.iter().map(|item| item.to_string()));
    if forward_agent {
        add(&["-A"]);
    }
    if let Some(key) = identity_file {
        // 指定了私钥就只用它，不把 ssh-agent 里的密钥挨个试一遍（试多了服务器会直接断开）
        add(&["-i", key, "-o", "IdentitiesOnly=yes"]);
    }
    if password_auth {
        add(&[
            "-o",
            "PreferredAuthentications=keyboard-interactive,password",
        ]);
    }
    match askpass {
        // 密码只问一次：ssh 默认问三次，用户点了取消它还会接着再问两次。输错了点「重试」重连
        true => add(&["-o", "NumberOfPasswordPrompts=1"]),
        false => add(&["-o", "BatchMode=yes"]),
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 私钥和密码登录变成_ssh_的选项() {
        let has = |args: &[String], what: &str| args.iter().any(|arg| arg == what);
        let plain = login_args(None, false, false, true);
        assert_eq!(plain, ["-o", "NumberOfPasswordPrompts=1"]);
        let key = login_args(Some("C:/keys/id"), false, true, true);
        assert!(has(&key, "-A") && has(&key, "IdentitiesOnly=yes"));
        let at = key.iter().position(|arg| arg == "-i").unwrap();
        assert_eq!(key[at + 1], "C:/keys/id");
        let password = login_args(None, true, false, false);
        assert!(has(
            &password,
            "PreferredAuthentications=keyboard-interactive,password"
        ));
        // 没有人可以问：要密码就直接失败，不卡着等
        assert!(has(&password, "BatchMode=yes"));
    }
}
