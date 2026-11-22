//! 让 git 和 ssh 要密码时来问我们，而不是在不存在的终端里等。客户端和 kuaid 共用。
//!
//! 主进程（客户端或 kuaid）开一个只有本机能连的监听口，再把自己的程序路径设成
//! `GIT_ASKPASS` 和 `SSH_ASKPASS`。git、ssh 要密码时会启动这个程序，参数是提示文字；
//! 程序发现环境变量里有监听地址，就进入「助手模式」：连上主进程，把提示交过去，
//! 等回答，打到标准输出后退出。Fork 自带的 `Fork.AskPass.exe` 是同样的思路。
//!
//! 监听口：Linux 上是 kuaid 目录里的 unix 套接字（权限 600）；Windows 上是 127.0.0.1 的随机端口。
//! 两种都要带随机口令才理会，口令只在 git 进程的环境变量里。

use crate::i18n::tr;
use std::hash::{BuildHasher as _, Hasher as _};
use std::io::{BufRead as _, BufReader, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

const ENV_ADDR: &str = "FR_ASKPASS_ADDR";
const ENV_TOKEN: &str = "FR_ASKPASS_TOKEN";
/// 加在某一个 git、ssh 进程上的标记：askpass 把它原样带回来，主进程就知道是哪一次在问。
/// 客户端连接服务器时用它认出「这台服务器存了密码」，自动回答。
pub const TAG_ENV: &str = "FR_ASKPASS_TAG";
/// 提示文字最长这么多，超过的不理会
const MAX_PROMPT: u64 = 16 * 1024;
/// 连上来以后这么久还没把请求说完就断开；同时在说请求（还没对上口令）的最多这么多个，多了的
/// 直接断开。本机别的程序连上来不说话、一个字节一个字节慢慢发，都占不住线程
const ASK_LIMIT: Duration = match cfg!(test) {
    true => Duration::from_millis(300),
    false => Duration::from_secs(10),
};
const MAX_PENDING: usize = 16;
/// 加在不许弹窗的那一条 git 命令前面（自动获取）：要密码、口令时当作取消。git 把 `-c` 的设置经
/// 环境变量 `GIT_CONFIG_PARAMETERS` 传给它启动的程序，askpass（git 直接启动的，或者经 ssh 启动的）
/// 看到它就不去问主进程（见 [`run_helper`]）。只管这一条命令：同时在做的别的操作照常弹窗（原来是
/// 进程级的开关，自动获取期间别的仓库推拉、连别的主机要密码也被当作取消）。
pub const NO_PROMPT: [&str; 2] = ["-c", "kuai.noprompt=true"];

/// 发问的 git 命令带着 [`NO_PROMPT`]。
fn no_prompt(config_parameters: Option<&str>) -> bool {
    config_parameters.is_some_and(|params| params.contains("'kuai.noprompt"))
}

#[derive(Serialize, Deserialize)]
struct Ask {
    token: String,
    prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Answer {
    /// 用户取消时为空
    answer: Option<String>,
}

/// 开始接收密码请求。`on_prompt` 在后台线程里调用，可以一直等到用户回答；返回 None 表示取消。
/// 返回要加到 git、ssh 进程上的环境变量。`helper` 是进入助手模式的程序，一般就是当前程序。
pub fn serve(
    helper: &Path,
    on_prompt: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
) -> Result<Vec<(String, String)>> {
    serve_tagged(helper, move |prompt, _| on_prompt(prompt))
}

/// 同 [`serve`]，`on_prompt` 多拿到一个参数：发问的进程环境变量 [`TAG_ENV`] 的值（没设就是 None）。
pub fn serve_tagged(
    helper: &Path,
    on_prompt: impl Fn(&str, Option<&str>) -> Option<String> + Send + Sync + 'static,
) -> Result<Vec<(String, String)>> {
    let dir = helper.parent().unwrap_or(Path::new("/tmp"));
    serve_tagged_in(dir, helper, on_prompt)
}

/// 同 [`serve_tagged`]，套接字文件放在 `dir` 里（Windows 上用不到）。常驻的 kuaid 把它放在
/// 自己的运行目录里：程序所在的目录会随升级换掉，套接字不能跟着没。
pub fn serve_tagged_in(
    dir: &Path,
    helper: &Path,
    on_prompt: impl Fn(&str, Option<&str>) -> Option<String> + Send + Sync + 'static,
) -> Result<Vec<(String, String)>> {
    let token = random_token();
    let (listener, addr) = Listener::bind(dir)?;
    let on_prompt = Arc::new(on_prompt);
    let expected = token.clone();
    let pending = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("fr-askpass".into())
        .spawn(move || {
            loop {
                let Ok(stream) = listener.accept() else {
                    // 监听口坏了就不再接；偶发的错误歇一下再试
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                };
                if pending.fetch_add(1, Ordering::AcqRel) >= MAX_PENDING {
                    pending.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                let waiting = Pending(pending.clone());
                let (on_prompt, expected) = (on_prompt.clone(), expected.clone());
                std::thread::spawn(move || {
                    let _ = answer_one(stream, waiting, &expected, &*on_prompt);
                });
            }
        })
        .context(tr("无法启动密码请求线程"))?;
    let helper = helper.to_string_lossy().to_string();
    Ok(vec![
        ("GIT_ASKPASS".into(), helper.clone()),
        ("SSH_ASKPASS".into(), helper),
        // 没有终端时 ssh 默认不用 askpass，要强制
        ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
        (ENV_ADDR.into(), addr),
        (ENV_TOKEN.into(), token),
    ])
}

/// 一个还在说请求的连接；放掉就少一个（见 [`MAX_PENDING`]）。
struct Pending(Arc<AtomicUsize>);

impl Drop for Pending {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 在期限之前读：每次读之前把读超时设成剩下的时间（读超时管的只是一次读）。
struct Deadline<'a> {
    stream: &'a mut dyn Stream,
    until: Instant,
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.stream.read_timeout(Some(left))?;
        self.stream.read(buf)
    }
}

fn answer_one(
    mut stream: Box<dyn Stream>,
    waiting: Pending,
    expected: &str,
    on_prompt: &(dyn Fn(&str, Option<&str>) -> Option<String> + Send + Sync),
) -> Result<()> {
    let mut line = String::new();
    let deadline = Deadline {
        stream: &mut *stream,
        until: Instant::now() + ASK_LIMIT,
    };
    BufReader::new(deadline)
        .take(MAX_PROMPT)
        .read_line(&mut line)?;
    let ask: Ask = serde_json::from_str(&line)?;
    if ask.token != expected {
        anyhow::bail!(tr("口令错误"));
    }
    // 对上口令了：等用户回答可以等很久，不算还在说请求的
    drop(waiting);
    stream.read_timeout(None)?;
    let answer = Answer {
        answer: on_prompt(&ask.prompt, ask.tag.as_deref()),
    };
    serde_json::to_writer(&mut stream, &answer)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

/// 程序启动时最先调用：如果是被 git 或 ssh 当作 askpass 启动的，就去问主进程，
/// 把回答打到标准输出，返回退出码；不是的话返回 None，照常启动。
pub fn run_helper() -> Option<i32> {
    let addr = std::env::var(ENV_ADDR).ok()?;
    let token = std::env::var(ENV_TOKEN).ok()?;
    let mut args = std::env::args().skip(1);
    let (Some(prompt), None) = (args.next(), args.next()) else {
        return None;
    };
    // 程序自己的选项（--stdio 之类）不是提示文字
    if prompt.starts_with("--") {
        return None;
    }
    // 不许弹窗的那条命令在问：当作取消
    if no_prompt(std::env::var("GIT_CONFIG_PARAMETERS").ok().as_deref()) {
        return Some(1);
    }
    let ask = || -> Result<Option<String>> {
        let mut stream = connect(&addr)?;
        let tag = std::env::var(TAG_ENV).ok();
        serde_json::to_writer(&mut stream, &Ask { token, prompt, tag })?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        let answer: Answer = serde_json::from_str(&line)?;
        Ok(answer.answer)
    };
    match ask() {
        Ok(Some(answer)) => {
            println!("{answer}");
            Some(0)
        }
        // 取消或者连不上主进程：以失败退出，git 和 ssh 会放弃这次认证
        _ => Some(1),
    }
}

trait Stream: Read + Write + Send {
    /// 一次读最多等多久，None 是一直等
    fn read_timeout(&self, limit: Option<Duration>) -> std::io::Result<()>;
}

impl Stream for std::net::TcpStream {
    fn read_timeout(&self, limit: Option<Duration>) -> std::io::Result<()> {
        self.set_read_timeout(limit)
    }
}

#[cfg(unix)]
impl Stream for std::os::unix::net::UnixStream {
    fn read_timeout(&self, limit: Option<Duration>) -> std::io::Result<()> {
        self.set_read_timeout(limit)
    }
}

enum Listener {
    #[cfg(not(unix))]
    Tcp(std::net::TcpListener),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener, std::path::PathBuf),
}

impl Listener {
    #[cfg(unix)]
    fn bind(dir: &Path) -> Result<(Listener, String)> {
        use std::os::unix::fs::PermissionsExt as _;
        clean_stale_sockets(dir);
        let path = socket_path(dir);
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path)
            .with_context(|| crate::tf!("无法创建 {path}", path = path.display()))?;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        let addr = path.to_string_lossy().to_string();
        Ok((Listener::Unix(listener, path), addr))
    }

    #[cfg(not(unix))]
    fn bind(_dir: &Path) -> Result<(Listener, String)> {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).context(tr("无法在本机打开监听端口"))?;
        let addr = listener.local_addr()?.to_string();
        Ok((Listener::Tcp(listener), addr))
    }

    fn accept(&self) -> std::io::Result<Box<dyn Stream>> {
        match self {
            #[cfg(not(unix))]
            Listener::Tcp(l) => l.accept().map(|(s, _)| Box::new(s) as Box<dyn Stream>),
            #[cfg(unix)]
            Listener::Unix(l, _) => l.accept().map(|(s, _)| Box::new(s) as Box<dyn Stream>),
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        #[cfg(unix)]
        #[allow(irrefutable_let_patterns)]
        if let Listener::Unix(_, path) = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 这个进程在 `dir` 里听的套接字文件。用 `exit` 直接退出的程序（kuaid 的常驻进程）走之前自己删。
#[cfg(unix)]
pub fn socket_path(dir: &Path) -> std::path::PathBuf {
    dir.join(format!("askpass-{}.sock", std::process::id()))
}

/// 删掉已经退出的进程留下的套接字文件（进程被杀掉时来不及删）。
#[cfg(unix)]
fn clean_stale_sockets(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("askpass-"))
            .and_then(|n| n.strip_suffix(".sock"))
        else {
            continue;
        };
        if !Path::new("/proc").join(pid).exists() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn connect(addr: &str) -> Result<Box<dyn Stream>> {
    #[cfg(unix)]
    if addr.contains('/') {
        return Ok(Box::new(std::os::unix::net::UnixStream::connect(addr)?));
    }
    Ok(Box::new(std::net::TcpStream::connect(addr)?))
}

/// 128 位随机口令。标准库的 `RandomState` 每次新建都从系统取随机种子，借它生成，不用另加依赖。
pub fn random_token() -> String {
    let part = || {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        hasher.finish()
    };
    format!("{:016x}{:016x}", part(), part())
}

/// ssh、git 在要什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// 登录密码
    Password,
    /// 私钥文件的口令
    Passphrase,
    /// 别的：用户名、确认主机指纹、验证码……
    Other,
}

/// 从提示文字看它在要什么。ssh、git 的提示不随系统语言变。
pub fn prompt_kind(prompt: &str) -> PromptKind {
    let lower = prompt.to_lowercase();
    if lower.contains("(yes/no") {
        PromptKind::Other
    } else if lower.contains("passphrase") {
        PromptKind::Passphrase
    } else if lower.contains("password") {
        PromptKind::Password
    } else {
        PromptKind::Other
    }
}

/// 提示是不是在要密码（要遮住输入）。用户名、yes/no 确认这些不遮。
pub fn is_secret(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    !(lower.starts_with("username") || lower.contains("(yes/no"))
}

/// 提示是不是要回答 yes 或 no 的确认（ssh 问要不要信任一台没见过的主机）。
pub fn is_confirm(prompt: &str) -> bool {
    prompt.to_lowercase().contains("(yes/no")
}

/// ssh 第一次连一台主机时问的那一句（`The authenticity of host … can't be established.`）里的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyPrompt {
    /// 主机，照 ssh 的写法：`example.com`、`[127.0.0.1]:2222`
    pub host: String,
    /// 解析出来的地址，和 `host` 不一样时才有
    pub address: Option<String>,
    /// 密钥类型：`ED25519`、`ECDSA`、`RSA`
    pub key_type: String,
    /// 指纹：`SHA256:…`
    pub fingerprint: String,
    /// 提示里别的话，原样一行一条：这把密钥还记在哪些名字下、DNS 里对没对上
    pub notes: Vec<String>,
}

/// 从 ssh 的提示文字里拆出主机和指纹。不是这一问、或者拆不出来（别的程序、很旧的 ssh）返回 None，
/// 那时照原文显示。
pub fn host_key_prompt(prompt: &str) -> Option<HostKeyPrompt> {
    if !is_confirm(prompt) {
        return None;
    }
    let (mut host, mut key) = (None, None);
    let mut notes = Vec::new();
    for line in prompt
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if let Some(rest) = line.strip_prefix("The authenticity of host '") {
            // 引号里是「主机 (地址)」；经 ProxyCommand 连的，地址那里是一句说明。
            // 后面那句 can't 里也有一个单引号，所以找的是第一个
            let inner = rest.split_once('\'')?.0;
            let (name, address) = match inner.rsplit_once(" (") {
                Some((name, address)) => (name, Some(address.trim_end_matches(')'))),
                None => (inner, None),
            };
            let address = address
                .filter(|address| *address != name && !address.starts_with('<'))
                .map(str::to_string);
            host = Some((name.to_string(), address));
        } else if let Some((kind, print)) = line.split_once(" key fingerprint is ") {
            key = Some((
                kind.trim().to_string(),
                print.trim().trim_end_matches('.').to_string(),
            ));
        } else if is_confirm(line)
            || line.starts_with("This key is not known by any other names")
            // 设了 `VisualHostKey yes` 时提示里带一幅字符画
            || line.starts_with('+')
            || line.starts_with('|')
        {
            // 这几行不用显示
        } else {
            notes.push(line.to_string());
        }
    }
    let ((host, address), (key_type, fingerprint)) = (host?, key?);
    if host.is_empty() || fingerprint.is_empty() {
        return None;
    }
    Some(HostKeyPrompt {
        host,
        address,
        key_type,
        fingerprint,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 拆出主机和指纹() {
        let ask = "Are you sure you want to continue connecting (yes/no/[fingerprint])? ";
        let plain = [
            "The authenticity of host '[127.0.0.1]:2291 ([127.0.0.1]:2291)' can't be established.",
            "RSA key fingerprint is SHA256:abcDEF/123+456.",
            "This key is not known by any other names.",
            ask,
        ]
        .join("\n");
        let key = host_key_prompt(&plain).unwrap();
        assert_eq!(key.host, "[127.0.0.1]:2291");
        assert_eq!(key.address, None);
        assert_eq!(key.key_type, "RSA");
        assert_eq!(key.fingerprint, "SHA256:abcDEF/123+456");
        assert!(key.notes.is_empty());

        // 名字和地址不一样；同一把密钥已经记在别的名字下（Windows 的换行）
        let known = [
            "The authenticity of host 'dev.example.com (203.0.113.7)' can't be established.",
            "ED25519 key fingerprint is SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU.",
            "This host key is known by the following other names/addresses:",
            r"    C:\Users\me/.ssh/known_hosts:3: dev",
            ask,
        ]
        .join("\r\n");
        let key = host_key_prompt(&known).unwrap();
        assert_eq!(key.host, "dev.example.com");
        assert_eq!(key.address.as_deref(), Some("203.0.113.7"));
        assert_eq!(key.key_type, "ED25519");
        assert_eq!(
            key.fingerprint,
            "SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU"
        );
        assert_eq!(
            key.notes,
            [
                "This host key is known by the following other names/addresses:",
                r"C:\Users\me/.ssh/known_hosts:3: dev"
            ]
        );

        // 经跳板机连的：地址那里是一句说明；旧版 ssh 只问 (yes/no)；字符画不算进别的话里
        let proxied = [
            "The authenticity of host 'inner (<no hostip for proxy command>)' can't be established.",
            "ECDSA key fingerprint is SHA256:xyz.",
            "+---[ECDSA 256]---+",
            "|  .o+            |",
            "+----[SHA256]-----+",
            "Are you sure you want to continue connecting (yes/no)? ",
        ]
        .join("\n");
        let key = host_key_prompt(&proxied).unwrap();
        assert_eq!((key.host.as_str(), key.address), ("inner", None));
        assert_eq!(key.fingerprint, "SHA256:xyz");
        assert!(key.notes.is_empty());

        // 不是这一问的、拆不出来的
        assert!(is_confirm(ask));
        assert_eq!(host_key_prompt(ask), None);
        assert_eq!(host_key_prompt("user@host's password: "), None);
        assert!(!is_confirm("Verification code: "));
    }

    #[test]
    fn 认出不许弹窗的命令() {
        // git 2.31 起的写法，和更早的写法
        assert!(no_prompt(Some(
            "'credential.helper'='' 'kuai.noprompt'='true'"
        )));
        assert!(no_prompt(Some("'kuai.noprompt=true'")));
        assert!(!no_prompt(Some("'core.quotepath'='false'")));
        assert!(!no_prompt(None));
        assert_eq!(NO_PROMPT[1], "kuai.noprompt=true");
    }

    #[test]
    fn 口令每次不同() {
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }

    #[test]
    fn 密码要遮住_用户名不遮() {
        assert!(is_secret("Password for 'https://user@example.com': "));
        assert!(is_secret(
            "Enter passphrase for key '/home/u/.ssh/id_ed25519': "
        ));
        assert!(is_secret("user@host's password: "));
        assert!(!is_secret("Username for 'https://example.com': "));
        assert!(!is_secret(
            "Are you sure you want to continue connecting (yes/no/[fingerprint])? "
        ));
    }

    #[test]
    fn 认得出在要密码还是密钥口令() {
        assert_eq!(prompt_kind("user@host's password: "), PromptKind::Password);
        assert_eq!(prompt_kind("(user@host) Password: "), PromptKind::Password);
        assert_eq!(
            prompt_kind("Enter passphrase for key 'C:\\Users\\u/.ssh/id_ed25519': "),
            PromptKind::Passphrase
        );
        assert_eq!(prompt_kind("Verification code: "), PromptKind::Other);
        assert_eq!(
            prompt_kind("Are you sure you want to continue connecting (yes/no/[fingerprint])? "),
            PromptKind::Other
        );
    }

    /// Unix 上套接字文件按进程号起名，同一个测试进程里同时开两个，后开的会把先开的顶掉
    /// （先开的那个测试连到后开的上，口令对不上），所以开监听的测试排着队跑。
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn 标记原样带给主进程() {
        let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        let dir = std::env::temp_dir();
        let env = serve_tagged(&dir.join("fr-askpass-test-helper"), |prompt, tag| {
            Some(format!("{prompt}/{}", tag.unwrap_or("无")))
        })
        .unwrap();
        let get = |key: &str| env.iter().find(|(k, _)| k == key).unwrap().1.clone();
        let (addr, token) = (get(ENV_ADDR), get(ENV_TOKEN));
        let ask = |tag: Option<&str>| -> Option<String> {
            let mut stream = connect(&addr).unwrap();
            let ask = Ask {
                token: token.clone(),
                prompt: "问".into(),
                tag: tag.map(str::to_string),
            };
            serde_json::to_writer(&mut stream, &ask).unwrap();
            stream.write_all(b"\n").unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).ok()?;
            serde_json::from_str::<Answer>(&line).ok()?.answer
        };
        assert_eq!(ask(Some("t1")).as_deref(), Some("问/t1"));
        assert_eq!(ask(None).as_deref(), Some("问/无"));
    }

    #[test]
    fn 连上来不说话_慢慢发的_到时间就断开() {
        let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        let dir = std::env::temp_dir();
        let env = serve(&dir.join("fr-askpass-test-helper"), |_| Some("答".into())).unwrap();
        let addr = env.iter().find(|(k, _)| k == ENV_ADDR).unwrap().1.clone();
        // 一个字节一个字节慢慢发：每次读都不超时，总期限到了照样断开
        let mut slow = connect(&addr).unwrap();
        let started = Instant::now();
        let mut cut = false;
        while started.elapsed() < Duration::from_secs(5) {
            if slow.write_all(b"{").is_err() {
                cut = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(cut, "慢慢发的应该被断开");
        // 不说话的：读得到对方已经关了，不是一直等
        let mut silent = connect(&addr).unwrap();
        silent.read_timeout(Some(Duration::from_secs(5))).unwrap();
        let started = Instant::now();
        let mut byte = [0u8; 1];
        let _ = silent.read(&mut byte);
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn 请求带着口令才回答() {
        let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        let dir = std::env::temp_dir();
        let env = serve(&dir.join("fr-askpass-test-helper"), |prompt| {
            (prompt == "问").then(|| "答".to_string())
        })
        .unwrap();
        let get = |key: &str| env.iter().find(|(k, _)| k == key).unwrap().1.clone();
        let (addr, token) = (get(ENV_ADDR), get(ENV_TOKEN));

        let ask = |token: &str, prompt: &str| -> Option<Answer> {
            let mut stream = connect(&addr).unwrap();
            let ask = Ask {
                token: token.into(),
                prompt: prompt.into(),
                tag: None,
            };
            serde_json::to_writer(&mut stream, &ask).unwrap();
            stream.write_all(b"\n").unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).ok()?;
            serde_json::from_str(&line).ok()
        };
        assert_eq!(ask(&token, "问").unwrap().answer.as_deref(), Some("答"));
        assert_eq!(ask(&token, "别的").unwrap().answer, None);
        // 口令不对：直接断开，不回答
        assert!(ask("wrong", "问").is_none());
    }
}
