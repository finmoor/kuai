//! 执行 git 命令。

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

/// 在指定仓库里执行一条 git 命令，返回标准输出的原始字节。
pub fn run(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args([
            "-c",
            "core.quotepath=false",
            "-c",
            "i18n.logOutputEncoding=UTF-8",
            "-c",
            "log.showSignature=false",
            "-c",
            "color.ui=false",
        ])
        .args(args)
        // 不让 git 在终端里等用户输入密码
        .env("GIT_TERMINAL_PROMPT", "0")
        // 只读操作不去抢 index.lock，免得和用户在终端里同时执行的 git 冲突
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：图形程序调用 git 时不弹出控制台窗口
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd
        .output()
        .with_context(|| format!("无法启动 git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} 失败：{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}
