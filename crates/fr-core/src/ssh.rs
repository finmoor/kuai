//! 起 ssh 用的那几样：系统的 ssh 在哪、每次都带的选项、登录一台服务器的选项。
//! 客户端连服务器、kuaid 替客户端连它配置的服务器（协议 10，见 kuaid 的 `relay`）共用。

use crate::i18n::tr;
use std::path::PathBuf;
use std::process::Command;

/// 每个 ssh 都带的：保活。
///
/// 主机密钥怎么核对不在这里设，照 ssh 自己的规矩和用户的 `~/.ssh/config`：没见过的主机 ssh 会问
/// 一句要不要信任，这句话经 askpass 交给界面，显示指纹让用户确认（见 `askpass::host_key_prompt`）；
/// 记录过的密钥对不上，ssh 直接拒绝。没有人可以问的（`BatchMode=yes`、不许弹窗的连接）这一次连不上。
///
/// 线路卡死（不通也不断，比如无线网掉了）时，连着 30 秒没有回音才算断开、去重连：原来是 15 秒
/// 问一次、4 次没回音，模拟线路上实测 76 秒才发现，这期间终端像冻住了。不能再短：慢线路上
/// 刷屏、传文件时 ssh 通道里可能积着 2 MB，300 KB/s 要 7 秒才轮到回音，太短会误断
pub const KEEP_OPTIONS: &[&str] = &[
    "-o",
    "ServerAliveInterval=10",
    "-o",
    "ServerAliveCountMax=3",
];

/// 没见过的主机不问，直接记进 known_hosts。只给测试工具用，界面不用
/// （见 `fr_channel::ConnectOptions::accept_new_host_keys`）
pub const ACCEPT_NEW_HOSTS: &[&str] = &["-o", "StrictHostKeyChecking=accept-new"];

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

/// 找不到 ssh 时给人看的话。Windows 上 OpenSSH 客户端是「可选功能」，一般都装着，精简过的系统、
/// Windows 沙盒里没有。
pub fn missing() -> &'static str {
    if cfg!(windows) {
        tr("找不到 ssh。请在 Windows 设置的「可选功能」中添加「OpenSSH 客户端」，然后重试。")
    } else {
        tr("找不到 ssh。请安装 OpenSSH 客户端，然后重试。")
    }
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
/// `key_only`：只用公钥认证，服务器不能在键盘交互认证里问话（自动回答存着的私钥口令的那一次用）。
pub fn login_args(
    identity_file: Option<&str>,
    password_auth: bool,
    key_only: bool,
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
    } else if key_only {
        add(&["-o", "PreferredAuthentications=publickey"]);
    }
    match askpass {
        // 密码只问一次：ssh 默认问三次，用户点了取消它还会接着再问两次。输错了点「重试」重连
        true => add(&["-o", "NumberOfPasswordPrompts=1"]),
        false => add(&["-o", "BatchMode=yes"]),
    }
    args
}

/// `~/.ssh/config` 里写的主机别名（`Host` 后面的名字，带通配符、取反的不算），按出现的先后，
/// 不重复。添加主机时可以直接选：ssh 照它的设置连（地址、端口、用户名、私钥都在那里）。
pub fn config_hosts(text: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        // `Host a b`，也可以写成 `Host=a`；关键字不分大小写
        let Some((key, rest)) = line.split_once(|c: char| c.is_whitespace() || c == '=') else {
            continue;
        };
        if !key.eq_ignore_ascii_case("host") {
            continue;
        }
        for name in rest
            .trim_start_matches(|c: char| c.is_whitespace() || c == '=')
            .split_whitespace()
        {
            let name = name.trim_matches('"');
            if name.is_empty() || name.contains(['*', '?', '!']) || hosts.iter().any(|h| h == name)
            {
                continue;
            }
            hosts.push(name.to_string());
        }
    }
    hosts
}

/// 读本机用户的 `~/.ssh/config`，见 [`config_hosts`]。没有这个文件是空的。
pub fn user_config_hosts() -> Vec<String> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    let Some(home) = home else {
        return Vec::new();
    };
    let path = PathBuf::from(home).join(".ssh").join("config");
    std::fs::read_to_string(path)
        .map(|text| config_hosts(&text))
        .unwrap_or_default()
}

/// 本机的用户名：ssh 目标里不写用户名时 ssh 用它（`~/.ssh/config` 里给了的除外）。
pub fn local_user() -> String {
    std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 私钥和密码登录变成_ssh_的选项() {
        let has = |args: &[String], what: &str| args.iter().any(|arg| arg == what);
        let plain = login_args(None, false, false, false, true);
        assert_eq!(plain, ["-o", "NumberOfPasswordPrompts=1"]);
        let key = login_args(Some("C:/keys/id"), false, false, true, true);
        assert!(has(&key, "-A") && has(&key, "IdentitiesOnly=yes"));
        let at = key.iter().position(|arg| arg == "-i").unwrap();
        assert_eq!(key[at + 1], "C:/keys/id");
        // 自动回答存着的私钥口令的那一次：只用公钥
        let key_only = login_args(Some("C:/keys/id"), false, true, false, true);
        assert!(has(&key_only, "PreferredAuthentications=publickey"));
        let password = login_args(None, true, false, false, false);
        assert!(has(
            &password,
            "PreferredAuthentications=keyboard-interactive,password"
        ));
        // 没有人可以问：要密码就直接失败，不卡着等
        assert!(has(&password, "BatchMode=yes"));
    }

    #[test]
    fn 读出_ssh_config_里的主机别名() {
        let text = [
            "Host dev-vps",
            "  HostName 203.0.113.8",
            "  User root",
            "",
            "Host *.internal !bastion",
            "Host=gpu box2 \"quoted\"",
            "host dev-vps",
            "  # Host commented",
            "Match host x",
        ]
        .join("\n");
        assert_eq!(
            super::config_hosts(&text),
            ["dev-vps", "gpu", "box2", "quoted"]
        );
        assert!(super::config_hosts("").is_empty());
    }
}
