//! 让 git 和 ssh 要密码时来问我们，而不是在不存在的终端里等。客户端和 agent 共用。
//!
//! 主进程（客户端或 agent）开一个只有本机能连的监听口，再把自己的程序路径设成
//! `GIT_ASKPASS` 和 `SSH_ASKPASS`。git、ssh 要密码时会启动这个程序，参数是提示文字；
//! 程序发现环境变量里有监听地址，就进入「助手模式」：连上主进程，把提示交过去，
//! 等回答，打到标准输出后退出。Fork 自带的 `Fork.AskPass.exe` 是同样的思路。
//!
//! 监听口：Linux 上是 agent 目录里的 unix 套接字（权限 600）；Windows 上是 127.0.0.1 的随机端口。
//! 两种都要带随机口令才理会，口令只在 git 进程的环境变量里。

use crate::i18n::tr;
use std::hash::{BuildHasher as _, Hasher as _};
use std::io::{BufRead as _, BufReader, Read, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

const ENV_ADDR: &str = "FR_ASKPASS_ADDR";
const ENV_TOKEN: &str = "FR_ASKPASS_TOKEN";
/// 加在某一个 git、ssh 进程上的标记：askpass 把它原样带回来，主进程就知道是哪一次在问。
/// 客户端连接服务器时用它认出「这台服务器存了密码」，自动回答。
pub const TAG_ENV: &str = "FR_ASKPASS_TAG";
/// 提示文字最长这么多，超过的不理会
const MAX_PROMPT: u64 = 16 * 1024;

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
    let token = random_token();
    let (listener, addr) = Listener::bind(helper)?;
    let on_prompt = Arc::new(on_prompt);
    let expected = token.clone();
    std::thread::Builder::new()
        .name("fr-askpass".into())
        .spawn(move || {
            loop {
                let Ok(stream) = listener.accept() else {
                    // 监听口坏了就不再接；偶发的错误歇一下再试
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                };
                let (on_prompt, expected) = (on_prompt.clone(), expected.clone());
                std::thread::spawn(move || {
                    let _ = answer_one(stream, &expected, &*on_prompt);
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

fn answer_one(
    stream: Box<dyn Stream>,
    expected: &str,
    on_prompt: &(dyn Fn(&str, Option<&str>) -> Option<String> + Send + Sync),
) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    (&mut reader).take(MAX_PROMPT).read_line(&mut line)?;
    let ask: Ask = serde_json::from_str(&line)?;
    if ask.token != expected {
        anyhow::bail!(tr("口令不对"));
    }
    let answer = Answer {
        answer: on_prompt(&ask.prompt, ask.tag.as_deref()),
    };
    let mut stream = reader.into_inner();
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

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

enum Listener {
    #[cfg(not(unix))]
    Tcp(std::net::TcpListener),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener, std::path::PathBuf),
}

impl Listener {
    #[cfg(unix)]
    fn bind(helper: &Path) -> Result<(Listener, String)> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = helper.parent().unwrap_or(Path::new("/tmp"));
        clean_stale_sockets(dir);
        let path = dir.join(format!("askpass-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path)
            .with_context(|| crate::tf!("无法创建 {path}", path = path.display()))?;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        let addr = path.to_string_lossy().to_string();
        Ok((Listener::Unix(listener, path), addr))
    }

    #[cfg(not(unix))]
    fn bind(_helper: &Path) -> Result<(Listener, String)> {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).context(tr("无法在本机开监听口"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn 标记原样带给主进程() {
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
    fn 请求带着口令才回答() {
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
