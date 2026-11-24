//! 把仓库里的网页交给浏览器：Kuai 在本机起一个小网页服务，浏览器要的文件经仓库通道读（远程主机上的
//! 经那台的 kuaid，和文件页读文件是同一条路，不在主机上开端口）。同目录的样式、脚本、图片和页面里的
//! 相对链接都能用；改了文件刷新浏览器就是新的。本机仓库也走它：`file://` 下有的脚本跑不了。
//!
//! - 只听 127.0.0.1，端口随机；第一次用时才起，之后一直开着。每个连接一个线程，读完就关；同时最多
//!   [`MAX_CONNS`] 个连接，请求头最长 [`HEAD_MAX`]、要在 [`HEAD_TIME`] 里读完。
//! - 每个仓库一个口令（随机 128 位），地址是 `http://127.0.0.1:端口/口令/相对仓库根的路径`：不知道
//!   口令的（本机别的程序、别的网页）读不到。交给浏览器打开的是一次性的地址 `/~票`（[`TICKET_TIME`]
//!   里有效，用过就作废），服务再转到带口令的地址：启动浏览器的命令行别的程序（Linux 上别的用户也）
//!   看得到，看到的那张票已经用掉了。
//! - 路径出不了仓库：`..` 不认，`.git` 里的不给；读的时候跟着符号链接走到底还要在仓库里、不在 `.git`
//!   里，只读普通文件（`confine`，本机和 kuaid 两边都查，见 `fr_core::files::confined`）。仓库是
//!   别人的也一样：页面里的脚本是仓库的内容，不能借一条指到 `~/.ssh` 的链接把仓库外的文件读走。
//! - 页面里以 `/` 开头的地址（`/style.css`）：浏览器按网站根来要，前面没有口令。从 `Referer`（页面
//!   自己的地址，只对本服务发，见 [`Reply::write`]）认出是哪个仓库的页面要的，转到那个仓库下。
//! - `Host` 只认 127.0.0.1、localhost：别的网站把自己的域名解析到 127.0.0.1（DNS 重绑定）也读不到。
//! - 主机改了地址、跳板机，或者删掉了：那台上的仓库的口令都作废（[`forget_servers`]）。
//! - 每次请求都重新读（最大 [`LIMIT`]，边读边发），不让浏览器缓存。

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use fr_channel::Channel;
use fr_core::files::READ_MAX;
use fr_core::i18n::tr;
use fr_core::tf;
use futures::executor::block_on;

use crate::settings::RepoEntry;

/// 一个文件最大这么大
const LIMIT: u64 = 64 << 20;
/// 请求头最多这么长
const HEAD_MAX: usize = 16 << 10;
/// 请求头要在这么久里读完：一点点发、一直不发完的连接不能一直占着线程
const HEAD_TIME: Duration = Duration::from_secs(10);
/// 浏览器这么久不说话就断开
const IDLE: Duration = Duration::from_secs(15);
/// 同时最多这么多个连接，再来的直接关掉（只有本机的程序连得上，浏览器同时开不了这么多）
const MAX_CONNS: usize = 64;
/// 交给浏览器打开的一次性地址多久不用就作废
const TICKET_TIME: Duration = Duration::from_secs(120);

/// 一个仓库：用哪个通道读、仓库根在哪。
struct Site {
    entry: RepoEntry,
    channel: Channel,
}

/// 一次性的地址：转到哪个仓库（口令）的哪个文件。
struct Ticket {
    token: String,
    path: String,
    made: Instant,
}

#[derive(Default)]
struct State {
    /// 口令 → 仓库
    sites: HashMap<String, Site>,
    /// 票 → 转到哪
    tickets: HashMap<String, Ticket>,
}

/// 起好的服务。
struct Server {
    port: u16,
    state: Arc<Mutex<State>>,
}

static SERVER: Mutex<Option<Server>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

/// 仓库里 `path`（相对仓库根、`/` 分隔）交给浏览器打开的地址（一次性的，见模块说明）。第一次用时
/// 起服务；`channel` 是这个仓库现在的通道，同一个仓库再来时换成它（连接断过、换过）。
pub fn url_for(entry: &RepoEntry, channel: Channel, path: &str) -> Result<String, String> {
    let mut server = lock(&SERVER);
    if server.is_none() {
        *server = Some(start().map_err(|err| tf!("无法启动本机网页服务：{err}", err))?);
    }
    let server = server.as_ref().expect("started");
    let mut state = lock(&server.state);
    let token = match state.sites.iter().find(|(_, site)| site.entry == *entry) {
        Some((token, _)) => token.clone(),
        None => new_token(),
    };
    state.sites.insert(
        token.clone(),
        Site {
            entry: entry.clone(),
            channel,
        },
    );
    state
        .tickets
        .retain(|_, ticket| ticket.made.elapsed() < TICKET_TIME);
    let ticket = new_token();
    state.tickets.insert(
        ticket.clone(),
        Ticket {
            token,
            path: path.to_string(),
            made: Instant::now(),
        },
    );
    Ok(format!("http://127.0.0.1:{}/~{ticket}", server.port))
}

/// 主机 `server` 连上了：这台主机上的仓库换到新的连接上读。
pub fn connected(server: &str, channel: &Channel) {
    let guard = lock(&SERVER);
    let Some(running) = guard.as_ref() else {
        return;
    };
    let mut state = lock(&running.state);
    for site in state.sites.values_mut() {
        if site.entry.server.as_deref() == Some(server) {
            site.channel = channel.clone();
        }
    }
}

/// 键满足 `gone` 的主机改了地址、跳板机，或者删掉了：那几台上的仓库的口令、还没用的票都作废，
/// 浏览器里开着的页面再要文件时说地址已失效。不然改成另一台以后，旧地址读到的是新那台上同一路径
/// 的仓库。
pub fn forget_servers(gone: impl Fn(&str) -> bool) {
    let guard = lock(&SERVER);
    let Some(running) = guard.as_ref() else {
        return;
    };
    let mut state = lock(&running.state);
    let State { sites, tickets } = &mut *state;
    sites.retain(|_, site| !site.entry.server.as_deref().is_some_and(&gone));
    tickets.retain(|_, ticket| sites.contains_key(&ticket.token));
}

/// 占着一个连接名额，放掉时还回去。
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
    }
}

fn start() -> std::io::Result<Server> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let state: Arc<Mutex<State>> = Arc::default();
    let serving = state.clone();
    let active = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("kuai-web".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    continue;
                };
                if active.fetch_add(1, Relaxed) >= MAX_CONNS {
                    active.fetch_sub(1, Relaxed);
                    continue;
                }
                let slot = Slot(active.clone());
                let state = serving.clone();
                // 起不了线程：闭包连同名额一起丢掉
                let _ = std::thread::Builder::new()
                    .name("kuai-web-conn".into())
                    .spawn(move || {
                        let _slot = slot;
                        serve(stream, port, &state);
                    });
            }
        })?;
    log::info!("本机网页服务起在 127.0.0.1:{port}");
    Ok(Server { port, state })
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
    /// 一次性的地址 `/~票`
    Ticket(String),
}

/// 从请求的网址（`/口令/路径?查询`）里拆出口令和路径。路径里有 `..`、`.`、`.git`、反斜杠、NUL 的
/// 不认。
fn parse_target(target: &str) -> Option<Asked> {
    let target = target.split(['?', '#']).next().unwrap_or_default();
    let rest = target.strip_prefix('/')?;
    if let Some(ticket) = rest.strip_prefix('~') {
        return (!ticket.is_empty() && !ticket.contains('/'))
            .then(|| Asked::Ticket(ticket.to_string()));
    }
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

/// 回应的内容。
enum Body {
    Bytes(Vec<u8>),
    /// HEAD 问的文件：只说多大
    Head(u64),
    /// 文件：先发读到的第一段，其余边读边发（一个连接不用把整个文件放在内存里）
    Stream {
        channel: Channel,
        root: String,
        path: String,
        size: u64,
        first: Vec<u8>,
    },
}

/// 一个要发回去的回应。
struct Reply {
    status: &'static str,
    kind: &'static str,
    body: Body,
    /// 转到这个地址
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
        Self::html(status, body)
    }

    fn html(status: &'static str, body: String) -> Self {
        Self {
            status,
            kind: "text/html; charset=utf-8",
            body: Body::Bytes(body.into_bytes()),
            location: None,
        }
    }

    /// 转到 `to`：`status` 是 301（目录补 `/`）、303（一次性地址）、307（以 `/` 开头的地址）。
    fn redirect(status: &'static str, to: String) -> Self {
        Self {
            status,
            kind: "text/plain; charset=utf-8",
            body: Body::Bytes(Vec::new()),
            location: Some(to),
        }
    }

    fn moved(to: String) -> Self {
        Self::redirect("301 Moved Permanently", to)
    }

    fn file(path: &str, body: Body) -> Self {
        Self {
            status: "200 OK",
            kind: content_type(path),
            body,
            location: None,
        }
    }

    /// 写回去。`Referrer-Policy: same-origin`：页面要本服务的文件时带上自己的地址（以 `/` 开头的
    /// 地址靠它认出是哪个仓库，见 [`from_page`]），点到别的网站时什么都不带（地址里有口令）。
    fn write(self, stream: &mut TcpStream, head_only: bool) -> std::io::Result<()> {
        let length = match &self.body {
            Body::Bytes(bytes) => bytes.len() as u64,
            Body::Head(size) | Body::Stream { size, .. } => *size,
        };
        let mut head = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {length}\r\n\
             Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
             Referrer-Policy: same-origin\r\nConnection: close\r\n",
            self.status, self.kind,
        );
        if let Some(location) = &self.location {
            head.push_str(&format!("Location: {location}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        if !head_only {
            match self.body {
                Body::Bytes(bytes) => stream.write_all(&bytes)?,
                Body::Head(_) => {}
                Body::Stream {
                    channel,
                    root,
                    path,
                    size,
                    first,
                } => stream_file(stream, (&channel, &root, &path), size, first)?,
            }
        }
        stream.flush()
    }
}

/// 发文件：先发 `first`，再一段段读、一段段发，发够 `size` 为止。文件在发的时候变短了就停下（连接
/// 关掉时浏览器看得出没收全）。
fn stream_file(
    stream: &mut TcpStream,
    (channel, root, path): (&Channel, &str, &str),
    size: u64,
    first: Vec<u8>,
) -> std::io::Result<()> {
    let mut sent = (first.len() as u64).min(size);
    stream.write_all(&first[..sent as usize])?;
    while sent < size {
        let len = (size - sent).min(READ_MAX);
        let (_, more) = block_on(channel.fs_read_confined(root, path, sent, len, None))
            .map_err(std::io::Error::other)?;
        if more.is_empty() {
            break;
        }
        let take = (more.len() as u64).min(size - sent);
        stream.write_all(&more[..take as usize])?;
        sent += take;
    }
    Ok(())
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 请求头里要用的几样。
struct Request {
    method: String,
    target: String,
    host: Option<String>,
    referer: Option<String>,
}

/// 读请求头：最长 [`HEAD_MAX`]，要在 [`HEAD_TIME`] 里读完，不然 None（连接直接关掉）。
fn read_head(stream: &TcpStream) -> Option<(Request, BufReader<std::io::Take<TcpStream>>)> {
    let reading = stream.try_clone().ok()?;
    // 先限住总长再按行读：一行很长、一直没有换行也只读这么多
    let mut reader = BufReader::new(reading.take(HEAD_MAX as u64));
    let deadline = Instant::now() + HEAD_TIME;
    let mut lines: Vec<String> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let _ = reader.get_ref().get_ref().set_read_timeout(Some(left));
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return None,
            // 读到上限还没换行：请求头太长
            Ok(_) if !line.ends_with('\n') => return None,
            Ok(_) => {}
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            break;
        }
        lines.push(line);
    }
    let mut first = lines.first()?.split(' ');
    let (method, target) = (first.next()?.to_string(), first.next()?.to_string());
    let header = |wanted: &str| {
        lines.iter().skip(1).find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case(wanted)
                .then(|| value.trim().to_string())
        })
    };
    let request = Request {
        method,
        target,
        host: header("host"),
        referer: header("referer"),
    };
    Some((request, reader))
}

/// 接一个连接：读请求头，回一个文件（或者报错），关掉。
fn serve(mut stream: TcpStream, port: u16, state: &Mutex<State>) {
    let _ = stream.set_write_timeout(Some(IDLE));
    let Some((request, reader)) = read_head(&stream) else {
        return;
    };
    let head_only = request.method == "HEAD";
    let reply = match () {
        _ if request.method != "GET" && !head_only => {
            Reply::page("405 Method Not Allowed", "405", tr("只能读取文件。"))
        }
        _ if !request
            .host
            .as_deref()
            .is_some_and(|host| local_host(host, port)) =>
        {
            Reply::page("403 Forbidden", "403", tr("只接受本机的访问。"))
        }
        _ => answer(&request, port, state, head_only),
    };
    if reply.write(&mut stream, head_only).is_err() {
        return;
    }
    // 先说写完了，等浏览器那边也关（最多一会儿）再关：直接关的话 Windows 上对面收到的是重置，
    // 回应可能读不全
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let rest = reader.into_inner().into_inner();
    let _ = rest.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = std::io::copy(&mut rest.take(HEAD_MAX as u64), &mut std::io::sink());
}

/// 页面里以 `/` 开头的地址（`/style.css`）：浏览器按网站根要，前面没有口令。`Referer` 是本服务上
/// 哪个仓库的页面，就转到那个仓库下的同一路径。
fn from_page(request: &Request, port: u16, state: &Mutex<State>) -> Option<String> {
    if !request.target.starts_with('/') {
        return None;
    }
    let rest = request.referer.as_deref()?.strip_prefix("http://")?;
    let (host, page) = rest.split_once('/')?;
    if !local_host(host, port) {
        return None;
    }
    let token = page.split(['/', '?', '#']).next()?;
    lock(state)
        .sites
        .contains_key(token)
        .then(|| format!("/{token}{}", request.target))
}

/// 按网址找文件，读出来。
fn answer(request: &Request, port: u16, state: &Mutex<State>, head_only: bool) -> Reply {
    let not_found = || Reply::page("404 Not Found", "404", tr("没有这个文件。"));
    let expired = || {
        Reply::page(
            "404 Not Found",
            "404",
            tr("地址已失效，请在 Kuai 中重新「在浏览器中打开」。"),
        )
    };
    let site = |token: &str| {
        lock(state)
            .sites
            .get(token)
            .map(|site| (site.channel.clone(), site.entry.path.clone()))
    };
    let asked = parse_target(&request.target);
    let (token, path, dir) = match asked {
        Some(Asked::Ticket(ticket)) => {
            let mut state = lock(state);
            return match state.tickets.remove(&ticket) {
                Some(ticket)
                    if ticket.made.elapsed() < TICKET_TIME
                        && state.sites.contains_key(&ticket.token) =>
                {
                    let to = format!("/{}/{}", ticket.token, encode_path(&ticket.path));
                    Reply::redirect("303 See Other", to)
                }
                _ => expired(),
            };
        }
        Some(Asked::Slash(token)) if site(&token).is_some() => {
            return Reply::moved(format!("/{token}/"));
        }
        Some(Asked::Path { token, path, dir }) if site(&token).is_some() => (token, path, dir),
        // 不是哪个仓库的口令：可能是页面里以 `/` 开头的地址
        other => {
            if let Some(to) = from_page(request, port, state) {
                return Reply::redirect("307 Temporary Redirect", to);
            }
            return match other {
                None => not_found(),
                Some(_) => expired(),
            };
        }
    };
    let Some((channel, root)) = site(&token) else {
        return expired();
    };
    if dir {
        let index = match path.is_empty() {
            true => "index.html".to_string(),
            false => format!("{path}/index.html"),
        };
        return match file(&channel, &root, &index, head_only) {
            Found::File(reply) | Found::Failed(reply) => reply,
            Found::Missing | Found::Dir => {
                listing(&channel, &root, &path).unwrap_or_else(not_found)
            }
        };
    }
    match file(&channel, &root, &path, head_only) {
        Found::File(reply) | Found::Failed(reply) => reply,
        Found::Missing => not_found(),
        // 是个目录：转到带 `/` 的地址，再按目录回
        Found::Dir => {
            let name = path.rsplit('/').next().unwrap_or(&path);
            Reply::moved(format!("{}/", encode_path(name)))
        }
    }
}

/// 找一个文件的结果。
enum Found {
    File(Reply),
    Missing,
    Dir,
    /// 读不了（太大、出了仓库、主机上出错）：带着报错的页面
    Failed(Reply),
}

/// 读仓库里的一个文件（HEAD 只问大小）。
fn file(channel: &Channel, root: &str, path: &str, head_only: bool) -> Found {
    let failed = |err: anyhow::Error| {
        Found::Failed(Reply::page(
            "502 Bad Gateway",
            "502",
            &tf!("读取 {path} 失败：{err}", path, err = format!("{err:#}")),
        ))
    };
    let too_big = |size: u64| {
        Found::Failed(Reply::page(
            "413 Content Too Large",
            "413",
            &tf!(
                "文件太大（{size}），不经本机网页服务打开。",
                size = crate::text::human_size(size)
            ),
        ))
    };
    if head_only {
        return match block_on(channel.fs_stat_confined(root, path)) {
            Ok(stat) if !stat.exists => Found::Missing,
            Ok(stat) if stat.dir => Found::Dir,
            Ok(stat) if stat.size > LIMIT => too_big(stat.size),
            Ok(stat) => Found::File(Reply::file(path, Body::Head(stat.size))),
            Err(err) => failed(err),
        };
    }
    match block_on(channel.fs_read_confined(root, path, 0, READ_MAX, Some(LIMIT))) {
        Ok((stat, _)) if stat.size > LIMIT => too_big(stat.size),
        Ok((stat, first)) => Found::File(Reply::file(
            path,
            Body::Stream {
                channel: channel.clone(),
                root: root.to_string(),
                path: path.to_string(),
                size: stat.size,
                first,
            },
        )),
        Err(err) => match block_on(channel.fs_stat_confined(root, path)) {
            Ok(stat) if !stat.exists => Found::Missing,
            Ok(stat) if stat.dir => Found::Dir,
            _ => failed(err),
        },
    }
}

/// 没有 index.html 的目录：列出里面的文件和目录。
fn listing(channel: &Channel, root: &str, dir: &str) -> Option<Reply> {
    let mut entries = block_on(channel.fs_list_confined(root, dir)).ok()?;
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
    Some(Reply::html("200 OK", body))
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
        assert_eq!(parse_target("/~t1"), Some(Asked::Ticket("t1".into())));
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
            "/~",
            "/~t1/x",
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

    /// 发一个请求（`extra` 是另外的请求头，每行以 `\r\n` 结尾），拿回状态行、头和内容。
    fn send(
        port: u16,
        method: &str,
        target: &str,
        host: &str,
        extra: &str,
    ) -> (String, String, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: {host}\r\n{extra}\r\n"
        )
        .unwrap();
        let mut all = Vec::new();
        stream.read_to_end(&mut all).unwrap();
        let split = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8(all[..split].to_vec()).unwrap();
        let status = head.lines().next().unwrap().to_string();
        (status, head, all[split + 4..].to_vec())
    }

    fn get(port: u16, target: &str, host: &str) -> (String, String, Vec<u8>) {
        send(port, "GET", target, host, "")
    }

    fn location(head: &str) -> String {
        head.lines()
            .find_map(|line| line.strip_prefix("Location: "))
            .unwrap()
            .to_string()
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
        std::fs::create_dir_all(temp.join("outside")).unwrap();
        std::fs::write(temp.join("outside").join("key"), "k").unwrap();
        // 比一段（READ_MAX）长的文件：分几段发
        let big: Vec<u8> = (0..READ_MAX + 1000).map(|i| (i % 251) as u8).collect();
        std::fs::write(format!("{root}/big.bin"), &big).unwrap();
        let entry = RepoEntry {
            server: None,
            path: root.clone(),
        };
        let url = url_for(&entry, Channel::Local, "docs/页面.html").unwrap();
        let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
        let (port, ticket) = rest.split_once('/').unwrap();
        let port: u16 = port.parse().unwrap();
        let ticket = format!("/{ticket}");
        let host = format!("127.0.0.1:{port}");

        // 一次性的地址：转到带口令的地址；再用一次就失效了
        let (status, head, _) = get(port, &ticket, &host);
        assert!(status.starts_with("HTTP/1.1 303"), "{status}");
        let target = location(&head);
        assert!(
            target.ends_with("/docs/%E9%A1%B5%E9%9D%A2.html"),
            "{target}"
        );
        let (status, _, body) = get(port, &ticket, &host);
        assert!(status.starts_with("HTTP/1.1 404"), "{status}");
        assert!(String::from_utf8_lossy(&body).contains("已失效"));
        let token = target.split('/').nth(1).unwrap().to_string();
        // 同一个仓库再来，口令不变，票是新的
        let again = url_for(&entry, Channel::Local, "docs/a.css").unwrap();
        assert!(!again.contains(&token));
        let (_, head, _) = get(
            port,
            again.strip_prefix(&format!("http://{host}")).unwrap(),
            &host,
        );
        assert!(location(&head).starts_with(&format!("/{token}/")));

        let (status, head, body) = get(port, &target, &host);
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(head.contains("Content-Type: text/html; charset=utf-8"));
        assert!(head.contains("Cache-Control: no-store"));
        assert!(head.contains("Referrer-Policy: same-origin"));
        assert_eq!(body, b"<p>hi</p>");
        // 同目录的样式
        let (status, head, _) = get(port, &format!("/{token}/docs/a.css"), &host);
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(head.contains("text/css"));
        // 页面里以 `/` 开头的地址：看 Referer 转到这个仓库下
        let referer = format!("Referer: http://{host}{target}\r\n");
        let (status, head, _) = send(port, "GET", "/docs/a.css?v=1", &host, &referer);
        assert!(status.starts_with("HTTP/1.1 307"), "{status}");
        assert_eq!(location(&head), format!("/{token}/docs/a.css?v=1"));
        let other = "Referer: http://evil.example/0/\r\n";
        let (status, _, _) = send(port, "GET", "/docs/a.css", &host, other);
        assert!(status.starts_with("HTTP/1.1 404"), "{status}");
        // 大文件分段发，内容完整；HEAD 只说多大
        let (status, _, body) = get(port, &format!("/{token}/big.bin"), &host);
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(body == big, "大文件内容不对（{} 字节）", body.len());
        let (status, head, body) = send(port, "HEAD", &format!("/{token}/big.bin"), &host, "");
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(
            head.contains(&format!("Content-Length: {}", big.len())),
            "{head}"
        );
        assert!(body.is_empty());
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
        // 跟着符号链接出了仓库的不给（建不了链接时跳过：Windows 上要开发者模式）
        #[cfg(unix)]
        let linked =
            std::os::unix::fs::symlink(temp.join("outside"), format!("{root}/out")).is_ok();
        #[cfg(windows)]
        let linked =
            std::os::windows::fs::symlink_dir(temp.join("outside"), format!("{root}/out")).is_ok();
        if linked {
            let (status, _, body) = get(port, &format!("/{token}/out/key"), &host);
            assert!(!status.starts_with("HTTP/1.1 200"), "{status}");
            assert_ne!(body, b"k");
            let (_, _, body) = get(port, &format!("/{token}/"), &host);
            assert!(String::from_utf8_lossy(&body).contains("out"));
        }
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn 主机改了地址以后旧口令作废() {
        let temp = std::env::temp_dir().join(format!("kuai-web-forget-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        std::fs::write(temp.join("a.html"), "a").unwrap();
        let entry = RepoEntry {
            server: Some("web-forget-host".into()),
            path: temp.to_string_lossy().into_owned(),
        };
        let url = url_for(&entry, Channel::Local, "a.html").unwrap();
        let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
        let (port, ticket) = rest.split_once('/').unwrap();
        let port: u16 = port.parse().unwrap();
        let host = format!("127.0.0.1:{port}");
        let (_, head, _) = get(port, &format!("/{ticket}"), &host);
        let target = location(&head);
        assert_eq!(get(port, &target, &host).2, b"a");
        // 还没用的票也一起作废
        let unused = url_for(&entry, Channel::Local, "a.html").unwrap();
        forget_servers(|server| server == "web-forget-host");
        let (status, _, body) = get(port, &target, &host);
        assert!(status.starts_with("HTTP/1.1 404"), "{status}");
        assert!(String::from_utf8_lossy(&body).contains("已失效"));
        let (status, _, _) = get(
            port,
            unused.strip_prefix(&format!("http://{host}")).unwrap(),
            &host,
        );
        assert!(status.starts_with("HTTP/1.1 404"), "{status}");
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn 请求头太长的直接断开() {
        let entry = RepoEntry {
            server: None,
            path: std::env::temp_dir().to_string_lossy().into_owned(),
        };
        let url = url_for(&entry, Channel::Local, "x").unwrap();
        let port: u16 = url["http://127.0.0.1:".len()..]
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let long = "a".repeat(HEAD_MAX + 10);
        let _ = write!(stream, "GET /{long} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        let mut all = Vec::new();
        let _ = stream.read_to_end(&mut all);
        assert!(all.is_empty(), "{}", String::from_utf8_lossy(&all));
    }
}
