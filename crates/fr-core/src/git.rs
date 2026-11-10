//! 执行 git 命令。

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{Context as _, Result, bail};

/// 每次调用 git 都带的配置：路径里的中文不转义、日志按 UTF-8 输出、不显示签名、不带颜色。
const CONFIG: &[&str] = &[
    "-c",
    "core.quotepath=false",
    "-c",
    "i18n.logOutputEncoding=UTF-8",
    "-c",
    "log.showSignature=false",
    "-c",
    "color.ui=false",
];

/// 每个 git 进程额外带的环境变量：让 git 要密码时来问我们，见 [`crate::askpass`]。
static EXTRA_ENV: OnceLock<Vec<(String, String)>> = OnceLock::new();

/// 设置每个 git 进程额外带的环境变量。只有第一次调用起作用，程序启动时调用。
pub fn set_extra_env(env: Vec<(String, String)>) {
    let _ = EXTRA_ENV.set(env);
}

/// 构造在指定仓库里执行 git 的命令：统一的配置和环境变量，Windows 上不弹控制台窗口。
/// 标准输入输出由调用方设置。
pub fn command<S: AsRef<OsStr>>(repo: &Path, args: &[S]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args(CONFIG)
        .args(args)
        // 不让 git 在终端里等用户输入密码
        .env("GIT_TERMINAL_PROMPT", "0")
        // 只读操作不去抢 index.lock，免得和用户在终端里同时执行的 git 冲突
        .env("GIT_OPTIONAL_LOCKS", "0")
        // 输出接到管道时 git 默认每个提交刷新一次，20 万个提交就是 20 万次写；让它按块缓冲
        .env("GIT_FLUSH", "0")
        // 路径一律按字面理解：文件名里有 * 或 [ 也不当通配符
        .env("GIT_LITERAL_PATHSPECS", "1")
        // 没有终端可以开编辑器：要编辑说明的地方（合并、继续变基、挑拣）直接用现成的说明。
        // git 经 sh 启动编辑器，Git for Windows 自带的 sh 里也有 true
        .env("GIT_EDITOR", "true");
    if let Some(extra) = EXTRA_ENV.get() {
        cmd.envs(extra.iter().map(|(k, v)| (k, v)));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：图形程序调用 git 时不弹出控制台窗口
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// 在指定仓库里执行一条 git 命令，返回标准输出的原始字节；退出码不为 0 时报错。
pub fn run(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = command(repo, args)
        .stdin(Stdio::null())
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
