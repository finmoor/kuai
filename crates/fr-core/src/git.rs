//! 执行 git 命令。

use crate::tf;
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

/// [`set_extra_env`] 设的环境变量；没设是空的。kuaid 起 ssh 时也带上（要密码时一样来问）。
pub fn extra_env() -> &'static [(String, String)] {
    EXTRA_ENV.get().map_or(&[], Vec::as_slice)
}

/// 构造在指定仓库里执行 git 的命令：统一的配置和环境变量，Windows 上不弹控制台窗口。
/// 标准输入输出由调用方设置。
/// `git --version` 里的版本号：`git version 2.34.1` → (2, 34, 1)；Windows 版的
/// `2.45.1.windows.1`、macOS 的 `2.39.3 (Apple Git-146)` 取前三段。认不出是 None。
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let version = text.trim().strip_prefix("git version ")?;
    let mut parts = version
        .split(|c: char| !c.is_ascii_digit())
        .map(str::parse::<u32>);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch))) => Some((major, minor, patch)),
        (Some(Ok(major)), Some(Ok(minor)), _) => Some((major, minor, 0)),
        _ => None,
    }
}

/// Kuai 要的最低 git 版本：提交详情用的 `show --diff-merges`（2.31）。用到的还有 `merge
/// --autostash`（2.27）、`stash push --pathspec-from-file`（2.26）。工作树清单的 `-z` 要 2.36，
/// 更老的按行读（Ubuntu 22.04 自带 2.34），见 [`crate::worktree::has_nul`]。
pub const MIN_VERSION: (u32, u32, u32) = (2, 31, 0);

/// git 比 [`MIN_VERSION`] 旧时返回它的版本号（`2.25.1`），给人看。认不出版本的（没装 git、输出
/// 认不得）不算旧：照常执行，出错时 git 自己会说。
pub fn too_old(version: Option<(u32, u32, u32)>) -> Option<String> {
    version
        .filter(|version| *version < MIN_VERSION)
        .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
}

/// 这台电脑上的 git 的版本号，只查一次。
pub fn local_version() -> Option<(u32, u32, u32)> {
    static VERSION: OnceLock<Option<(u32, u32, u32)>> = OnceLock::new();
    *VERSION.get_or_init(|| {
        let out = run(Path::new("."), &["--version"]).ok()?;
        parse_version(&String::from_utf8_lossy(&out))
    })
}

/// 按 sh 的规矩给一个参数加引号：只有字母数字和 `_./+@=,:-%` 的原样，别的用单引号括起来
/// （里面的单引号写成 `'\''`）。给人看的命令行、往 sh 里送的路径用。
pub fn sh_quote(text: &str) -> String {
    let plain = |c: char| c.is_alphanumeric() || "_./+@=,:-%".contains(c);
    match !text.is_empty() && text.chars().all(plain) {
        true => text.to_string(),
        false => format!("'{}'", text.replace('\'', "'\\''")),
    }
}

/// 把文字里地址的账号密码部分换成 `***`（`https://user:token@host/x` → `https://***@host/x`）。
/// 记活动、写日志、显示正在执行的命令时用：带凭据的远程地址不额外留存、不跟着复制出去。
/// `user@host:path` 这种 ssh 写法里没有密码，不动。
pub fn redact_urls(text: &str) -> std::borrow::Cow<'_, str> {
    let mut out = String::new();
    let mut rest = text;
    let mut changed = false;
    while let Some(at) = rest.find("://") {
        let start = at + 3;
        let end = rest[start..]
            .find(|c: char| {
                c == '/' || c == '?' || c == '#' || c == '\'' || c == '"' || c.is_whitespace()
            })
            .map_or(rest.len(), |len| start + len);
        match rest[start..end].rfind('@') {
            Some(user) => {
                out.push_str(&rest[..start]);
                out.push_str("***");
                out.push_str(&rest[start + user..end]);
                changed = true;
            }
            None => out.push_str(&rest[..end]),
        }
        rest = &rest[end..];
    }
    match changed {
        true => {
            out.push_str(rest);
            std::borrow::Cow::Owned(out)
        }
        false => std::borrow::Cow::Borrowed(text),
    }
}

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
        .with_context(|| tf!("无法启动 git {join}", join = args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "{}",
            tf!(
                "git {join} 失败：{stderr}",
                join = args.join(" "),
                stderr = String::from_utf8_lossy(&out.stderr).trim()
            )
        );
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod version_tests {
    use super::{MIN_VERSION, parse_version, too_old};

    #[test]
    fn 比最低版本旧的才算旧() {
        assert_eq!(
            too_old(parse_version("git version 2.25.1")).as_deref(),
            Some("2.25.1")
        );
        assert_eq!(
            too_old(parse_version("git version 2.30.9")).as_deref(),
            Some("2.30.9")
        );
        assert_eq!(too_old(Some(MIN_VERSION)), None);
        assert_eq!(too_old(parse_version("git version 2.34.1")), None);
        assert_eq!(too_old(parse_version("git version 2.53.0.windows.1")), None);
        // 认不出版本的不拦
        assert_eq!(too_old(parse_version("未找到 git")), None);
        assert_eq!(too_old(None), None);
    }
}

#[cfg(test)]
mod redact_tests {
    use super::redact_urls;

    #[test]
    fn 地址里的账号密码换成星号() {
        assert_eq!(
            redact_urls("git remote add origin https://user:tok3n@example.com/a/b.git"),
            "git remote add origin https://***@example.com/a/b.git"
        );
        // 只有令牌（当用户名用）的也遮
        assert_eq!(
            redact_urls("fatal: 'https://ghp_x@github.com/o/r' x http://h/p"),
            "fatal: 'https://***@github.com/o/r' x http://h/p"
        );
        // 没有账号密码的、ssh 写法的不动
        assert!(matches!(
            redact_urls("git@github.com:o/r.git https://example.com/a@b"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(redact_urls("ssh://me@host:22/x"), "ssh://***@host:22/x");
    }
}
