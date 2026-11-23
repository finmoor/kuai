//! 把仓库里的网页交给浏览器：Kuai 在本机起一个小网页服务，浏览器要的文件经仓库通道读（远程主机上的
//! 经那台的 kuaid，和文件页读文件是同一条路，不在主机上开端口）。同目录的样式、脚本、图片和页面里的
//! 相对链接都能用；改了文件刷新浏览器就是新的。本机仓库也走它：`file://` 下有的脚本跑不了。
//!
//! - 只听 127.0.0.1，端口随机；第一次用时才起，之后一直开着（每个连接一个线程，读完就关）。
//! - 每个仓库一个口令（随机 128 位），地址是 `http://127.0.0.1:端口/口令/相对仓库根的路径`：不知道
//!   口令的（本机别的程序、别的网页）读不到。路径出不了仓库：`..` 不认，kuaid 那边也只读仓库里的；
//!   `.git` 里的不给。
//! - `Host` 只认 127.0.0.1、localhost：别的网站把自己的域名解析到 127.0.0.1（DNS 重绑定）也读不到。
//! - 每次请求整个读一遍（最大 [`LIMIT`]），不让浏览器缓存。

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fr_channel::Channel;
use fr_core::i18n::tr;
use fr_core::tf;
use futures::executor::block_on;

use crate::settings::RepoEntry;

/// 一个文件最大这么大
const LIMIT: u64 = 64 << 20;
/// 请求头最多这么长
const HEAD_MAX: usize = 16 << 10;
/// 浏览器这么久不说话就断开
const IDLE: Duration = Duration::from_secs(15);

/// 一个仓库：用哪个通道读、仓库根在哪。
struct Site {
    entry: RepoEntry,
    channel: Channel,
}

/// 起好的服务。
struct Server {
    port: u16,
    /// 口令 → 仓库
    sites: Arc<Mutex<HashMap<String, Site>>>,
}

static SERVER: Mutex<Option<Server>> = Mutex::new(None);

/// 仓库里 `path`（相对仓库根、`/` 分隔）在浏览器里的地址。第一次用时起服务；`channel` 是这个仓库现在的
/// 通道，同一个仓库再来时换成它（连接断过、换过）。
pub fn url_for(entry: &RepoEntry, channel: Channel, path: &str) -> Result<String, String> {
    let mut server = SERVER.lock().unwrap_or_else(|err| err.into_inner());
    if server.is_none() {
        *server = Some(start().map_err(|err| tf!("无法启动本机网页服务：{err}", err))?);
    }
    let server = server.as_ref().expect("started");
    let mut sites = server.sites.lock().unwrap_or_else(|err| err.into_inner());
    let token = match sites.iter().find(|(_, site)| site.entry == *entry) {
        Some((token, _)) => token.clone(),
        None => new_token(),
    };
    sites.insert(
        token.clone(),
        Site {
            entry: entry.clone(),
            channel,
        },
    );
    Ok(format!(
        "http://127.0.0.1:{}/{token}/{}",
        server.port,
        encode_path(path)
    ))
}

/// 主机 `server` 连上了：这台主机上的仓库换到新的连接上读。
pub fn connected(server: &str, channel: &Channel) {
    let guard = SERVER.lock().unwrap_or_else(|err| err.into_inner());
    let Some(running) = guard.as_ref() else {
        return;
    };
    let mut sites = running.sites.lock().unwrap_or_else(|err| err.into_inner());
    for site in sites.values_mut() {
        if site.entry.server.as_deref() == Some(server) {
            site.channel = channel.clone();
        }
    }
}

fn start() -> std::io::Result<Server> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let sites: Arc<Mutex<HashMap<String, Site>>> = Arc::default();
    let serving = sites.clone();
    std::thread::Builder::new()
        .name("kuai-web".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    continue;
                };
                let sites = serving.clone();
                let _ = std::thread::Builder::new()
                    .name("kuai-web-conn".into())
                    .spawn(move || serve(stream, port, &sites));
            }
        })?;
    log::info!("本机网页服务起在 127.0.0.1:{port}");
    Ok(Server { port, sites })
}

/// 口令：随机 128 位，十六进制。标准库的 `RandomState` 每个线程第一次用时从系统取随机数做密钥，
/// 拿它散列几个数，再混上时间、进程号，过一遍 SHA-256。
fn new_token() -> String {
    use sha2::{Digest as _, Sha256};
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut digest = Sha256::new();
    for round in 0..4u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(round);
        digest.update(hasher.finish().to_le_bytes());
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_nanos());
    digest.update(nanos.to_le_bytes());
    digest.update(std::process::id().to_le_bytes());
    digest.finalize()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 路径放进网址：每一段里字母、数字和 `-._~` 以外的都转成 `%XX`（中文按 UTF-8），`/` 留着。
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 网址里的路径还原：`%XX` 解开，要是合法的 UTF-8。
pub(crate) fn decode_path(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(at + 1..at + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                at += 3;
            }
            byte => {
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// 请求的是什么。
#[derive(Debug, PartialEq)]
enum Asked {
    /// 仓库里的文件或目录（相对仓库根，`/` 分隔；空的是仓库根）。`dir`：网址以 `/` 结尾
    Path {
        token: String,
        path: String,
        dir: bool,
    },
    /// 只有口令、后面没有 `/`：转到带 `/` 的地址（页面里的相对链接才对）
    Slash(String),
}

/// 从请求的网址（`/口令/路径?查询`）里拆出口令和路径。路径里有 `..`、`.`、`.git`、反斜杠、NUL 的
/// 不认。
fn parse_target(target: &str) -> Option<Asked> {
    let target = target.split(['?', '#']).next().unwrap_or_default();
    let rest = target.strip_prefix('/')?;
    let (token, path) = match rest.split_once('/') {
        Some((token, path)) => (token, path),
        None => return (!rest.is_empty()).then(|| Asked::Slash(rest.to_string())),
    };
    if token.is_empty() {
        return None;
    }
    let decoded = decode_path(path)?;
    if decoded.contains(['\\', '\0']) {
        return None;
    }
    let dir = decoded.is_empty() || decoded.ends_with('/');
    let mut parts = Vec::new();
    for part in decoded.split('/') {
        match part {
            "" => {}
            "." | ".." => return None,
            // Git 的数据目录不给（里面的 config 可能带着远程地址里的口令）
            part if fr_core::files::is_git_name(part) => return None,
            part => parts.push(part),
        }
    }
    Some(Asked::Path {
        token: token.to_string(),
        path: parts.join("/"),
        dir,
    })
}

/// `Host` 是不是本机：127.0.0.1、localhost，带不带端口都行（端口要对）。
fn local_host(host: &str, port: u16) -> bool {
    let host = host.trim();
    let (name, given) = match host.rsplit_once(':') {
        Some((name, given)) => (name, Some(given)),
        None => (host, None),
    };
    let port_ok = given.is_none_or(|given| given == port.to_string());
    port_ok && (name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost"))
}

/// 按扩展名给的内容类型。文字类的标上 UTF-8。
fn content_type(path: &str) -> &'static str {
    let ext = path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "xhtml" => "application/xhtml+xml; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "webmanifest" => "application/manifest+json; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "svg" => "image/svg+xml; charset=utf-8",
        "txt" | "md" | "markdown" | "log" | "csv" => "text/plain; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        _ => "application/octet-stream",
    }
}

/// 一个要发回去的回应。
struct Reply {
    status: &'static str,
    kind: &'static str,
    body: Vec<u8>,
    /// 转到这个地址（301）
    location: Option<String>,
}

impl Reply {
    fn page(status: &'static str, title: &str, text: &str) -> Self {
        let body = format!(
            "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
             <body style=\"font:14px system-ui,sans-serif;margin:40px;color:#444\">\
             <h3>{title}</h3><p>{text}</p></body>",
            title = escape(title),
            text = escape(text)
        );
        Self {
            status,
            kind: "text/html; charset=utf-8",
            body: body.into_bytes(),
            location: None,
        }
    }

    fn moved(to: String) -> Self {
        Self {
            status: "301 Moved Permanently",
            kind: "text/plain; charset=utf-8",
            body: Vec::new(),
            location: Some(to),
        }
    }

    fn write(self, stream: &mut TcpStream, head_only: bool) -> std::io::Result<()> {
        let mut head = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
             Referrer-Policy: no-referrer\r\nConnection: close\r\n",
            self.status,
            self.kind,
            self.body.len()
        );
        if let Some(location) = &self.location {
            head.push_str(&format!("Location: {location}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        if !head_only {
            stream.write_all(&self.body)?;
        }
        stream.flush()
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 接一个连接：读请求头，回一个文件（或者报错），关掉。
fn serve(mut stream: TcpStream, port: u16, sites: &Mutex<HashMap<String, Site>>) {
    let _ = stream.set_read_timeout(Some(IDLE));
    let _ = stream.set_write_timeout(Some(IDLE));
    let Ok(reading) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reading);
    let mut lines: Vec<String> = Vec::new();
    let mut size = 0;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(read) => size += read,
        }
        if size > HEAD_MAX {
            return;
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            break;
        }
        lines.push(line);
    }
    let mut first = lines
        .first()
        .map(|line| line.split(' '))
        .into_iter()
        .flatten();
    let (method, target) = (
        first.next().unwrap_or_default(),
        first.next().unwrap_or_default(),
    );
    let host = lines.iter().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case("host").then_some(value)
    });
    let head_only = method == "HEAD";
    let reply = match () {
        _ if method != "GET" && !head_only => {
            Reply::page("405 Method Not Allowed", "405", tr("只能读取文件。"))
        }
        _ if !host.is_some_and(|host| local_host(host, port)) => {
            Reply::page("403 Forbidden", "403", tr("只接受本机的访问。"))
        }
        _ => answer(target, sites),
    };
    if reply.write(&mut stream, head_only).is_err() {
        return;
    }
    // 先说写完了，等浏览器那边也关（最多一会儿）再关：直接关的话 Windows 上对面收到的是重置，
    // 回应可能读不全
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = reader
        .get_mut()
        .set_read_timeout(Some(Duration::from_secs(1)));
    let _ = std::io::copy(&mut reader.take(HEAD_MAX as u64), &mut std::io::sink());
}

/// 按网址找文件，读出来。
fn answer(target: &str, sites: &Mutex<HashMap<String, Site>>) -> Reply {
    let not_found = || Reply::page("404 Not Found", "404", tr("没有这个文件。"));
    let (token, path, dir) = match parse_target(target) {
        Some(Asked::Path { token, path, dir }) => (token, path, dir),
        Some(Asked::Slash(token)) => return Reply::moved(format!("/{token}/")),
        None => return not_found(),
    };
    let found = {
        let sites = sites.lock().unwrap_or_else(|err| err.into_inner());
        sites
            .get(&token)
            .map(|site| (site.channel.clone(), site.entry.path.clone()))
    };
    let Some((channel, root)) = found else {
        return Reply::page(
            "404 Not Found",
            "404",
            tr("地址已失效，请在 Kuai 中重新「在浏览器中打开」。"),
        );
    };
    if dir {
        let index = match path.is_empty() {
            true => "index.html".to_string(),
            false => format!("{path}/index.html"),
        };
        return match read(&channel, &root, &index) {
            Ok(Some(reply)) => reply,
            Ok(None) => listing(&channel, &root, &path).unwrap_or_else(not_found),
            Err(reply) => reply,
        };
    }
    match read(&channel, &root, &path) {
        Ok(Some(reply)) => reply,
        Ok(None) => not_found(),
        // 是个目录：转到带 `/` 的地址，再按目录回
        Err(_) if block_on(channel.fs_stat(&root, &path)).is_ok_and(|stat| stat.dir) => {
            let name = path.rsplit('/').next().unwrap_or(&path);
            Reply::moved(format!("{}/", encode_path(name)))
        }
        Err(reply) => reply,
    }
}

/// 读仓库里的一个文件：没有这个文件是 `Ok(None)`，读不了是 `Err`（带着报错的页面）。
fn read(channel: &Channel, root: &str, path: &str) -> Result<Option<Reply>, Reply> {
    match block_on(channel.fs_open(root, path, LIMIT)) {
        Ok((stat, _)) if !stat.exists => Ok(None),
        Ok((stat, _)) if stat.size > LIMIT => Err(Reply::page(
            "413 Content Too Large",
            "413",
            &tf!(
                "文件太大（{size}），不经本机网页服务打开。",
                size = crate::text::human_size(stat.size)
            ),
        )),
        Ok((_, body)) => Ok(Some(Reply {
            status: "200 OK",
            kind: content_type(path),
            body,
            location: None,
        })),
        Err(err) => match block_on(channel.fs_stat(root, path)) {
            Ok(stat) if !stat.exists => Ok(None),
            _ => Err(Reply::page(
                "502 Bad Gateway",
                "502",
                &tf!("读取 {path} 失败：{err}", path, err = format!("{err:#}")),
            )),
        },
    }
}

/// 没有 index.html 的目录：列出里面的文件和目录。
fn listing(channel: &Channel, root: &str, dir: &str) -> Option<Reply> {
    let mut entries = block_on(channel.fs_list(root, dir)).ok()?;
    entries.sort_by_cached_key(|entry| (!entry.dir, entry.name.to_lowercase()));
    let title = match dir.is_empty() {
        true => crate::settings::base_name(root).to_string(),
        false => dir.to_string(),
    };
    let mut body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
         <body style=\"font:14px system-ui,sans-serif;margin:32px\"><h3>{title}/</h3><ul>",
        title = escape(&title)
    );
    if !dir.is_empty() {
        body.push_str("<li><a href=\"../\">../</a></li>");
    }
    for entry in entries {
        let slash = if entry.dir { "/" } else { "" };
        body.push_str(&format!(
            "<li><a href=\"{href}{slash}\">{name}{slash}</a></li>",
            href = encode_path(&entry.name),
            name = escape(&entry.name)
        ));
    }
    body.push_str("</ul></body>");
    Some(Reply {
        status: "200 OK",
        kind: "text/html; charset=utf-8",
        body: body.into_bytes(),
        location: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 网址里的路径() {
        let path = |target: &str| match parse_target(target) {
            Some(Asked::Path { token, path, dir }) => Some((token, path, dir)),
            _ => None,
        };
        assert_eq!(
            path("/abc/docs/a%20b/%E4%B8%AD.html?x=1#top"),
            Some(("abc".into(), "docs/a b/中.html".into(), false))
        );
        assert_eq!(path("/abc/"), Some(("abc".into(), String::new(), true)));
        assert_eq!(
            path("/abc/docs/"),
            Some(("abc".into(), "docs".into(), true))
        );
        assert_eq!(
            path("/abc//x.css"),
            Some(("abc".into(), "x.css".into(), false))
        );
        assert_eq!(parse_target("/abc"), Some(Asked::Slash("abc".into())));
        for bad in [
            "/abc/../x",
            "/abc/a/%2e%2e/b",
            "/abc/./x",
            "/abc/a%5Cb",
            "/abc/a%00b",
            "/abc/%ff",
            "/abc/%4",
            "/abc/.git/config",
            "/abc/sub/.GIT/HEAD",
            "//x",
            "/",
            "abc/x",
        ] {
            assert_eq!(parse_target(bad), None, "{bad}");
        }
        // 编码、解码来回一样
        let name = "docs/设计 稿/a+b#1.html";
        assert_eq!(decode_path(&encode_path(name)).as_deref(), Some(name));
        assert_eq!(encode_path("a b/中"), "a%20b/%E4%B8%AD");
    }

    #[test]
    fn 只认本机的主机名() {
        assert!(local_host("127.0.0.1:8080", 8080));
        assert!(local_host(" localhost:8080", 8080));
        assert!(local_host("127.0.0.1", 8080));
        assert!(!local_host("127.0.0.1:9000", 8080));
        assert!(!local_host("evil.example:8080", 8080));
        assert!(!local_host("127.0.0.1.evil.example:8080", 8080));
    }

    #[test]
    fn 口令每次不同() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }

    /// 发一个请求，拿回状态行、头和内容。
    fn get(port: u16, target: &str, host: &str) -> (String, String, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(stream, "GET {target} HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
        let mut all = Vec::new();
        stream.read_to_end(&mut all).unwrap();
        let split = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8(all[..split].to_vec()).unwrap();
        let status = head.lines().next().unwrap().to_string();
        (status, head, all[split + 4..].to_vec())
    }

    #[test]
    fn 本机仓库的文件经网页服务读() {
        let temp = std::env::temp_dir().join(format!("kuai-web-serve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        let root = temp.join("repo").to_string_lossy().into_owned();
        std::fs::create_dir_all(format!("{root}/docs/sub")).unwrap();
        std::fs::write(format!("{root}/docs/页面.html"), "<p>hi</p>").unwrap();
        std::fs::write(format!("{root}/docs/a.css"), "p{}").unwrap();
        std::fs::write(format!("{root}/docs/sub/index.html"), "idx").unwrap();
        std::fs::write(format!("{root}/secret.txt"), "no").unwrap();
        let entry = RepoEntry {
            server: None,
            path: root.clone(),
        };
        let url = url_for(&entry, Channel::Local, "docs/页面.html").unwrap();
        let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
        let (port, target) = rest.split_once('/').unwrap();
        let port: u16 = port.parse().unwrap();
        let target = format!("/{target}");
        let host = format!("127.0.0.1:{port}");
        let token = target.split('/').nth(1).unwrap().to_string();
        // 同一个仓库再来，口令不变
        let again = url_for(&entry, Channel::Local, "docs/a.css").unwrap();
        assert!(again.contains(&token));

        let (status, head, body) = get(port, &target, &host);
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(head.contains("Content-Type: text/html; charset=utf-8"));
        assert!(head.contains("Cache-Control: no-store"));
        assert_eq!(body, b"<p>hi</p>");
        // 同目录的样式
        let (status, head, _) = get(port, &format!("/{token}/docs/a.css"), &host);
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(head.contains("text/css"));
        // 目录：有 index.html 的回它，没 `/` 的先转过去
        let (status, head, _) = get(port, &format!("/{token}/docs/sub"), &host);
        assert!(status.starts_with("HTTP/1.1 301"), "{status}");
        assert!(head.contains("Location: sub/"), "{head}");
        let (_, _, body) = get(port, &format!("/{token}/docs/sub/"), &host);
        assert_eq!(body, b"idx");
        // 没有 index.html 的目录列出来
        let (status, _, body) = get(port, &format!("/{token}/docs/"), &host);
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(String::from_utf8_lossy(&body).contains("a.css"));
        // 没有的、出仓库的、口令不对的、主机名不对的
        let (status, _, _) = get(port, &format!("/{token}/docs/none.html"), &host);
        assert!(status.starts_with("HTTP/1.1 404"), "{status}");
        let (status, _, _) = get(port, &format!("/{token}/docs/../../x"), &host);
        assert!(status.starts_with("HTTP/1.1 404"), "{status}");
        let (status, _, _) = get(port, "/0000/secret.txt", &host);
        assert!(status.starts_with("HTTP/1.1 404"), "{status}");
        let (status, _, _) = get(port, &target, "evil.example");
        assert!(status.starts_with("HTTP/1.1 403"), "{status}");
        let _ = std::fs::remove_dir_all(&temp);
    }
}
