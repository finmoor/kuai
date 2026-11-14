//! 在 Windows 上拿真的 kuaid 程序测常驻这一套：拉起、终端断开后还在、升级不断终端、结束。
//! 和 `linux.rs` 测的是同一批事，系统不一样的地方（终端托管进程、端口加口令、升级时换进程）在这里。
//!
//! 每个测试用一个自己的临时目录当 kuaid 的主目录（环境变量 `FR_KUAID_HOME`），所以各有各的
//! 常驻进程，互不相干，也不碰真的 `~/.kuai`。测的是经转接程序（`kuaid --connect`）说协议，
//! 和客户端经 ssh、本机的 Kuai 用它的方式一样。终端里开的是系统自带的 Windows PowerShell。
#![cfg(windows)]

use std::collections::VecDeque;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use fr_core::protocol::{
    self, DaemonInfo, FilePutReply, Hello, PROTOCOL_VERSION, Request, Response, TermExit, TermInfo,
    TermOpened,
};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);

fn kuaid_bin() -> PathBuf {
    std::env::var_os("KUAID_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_kuaid")))
}

/// 一个临时的主目录，里面按客户端上传的样子装着 kuaid。丢弃时让常驻进程退出、删掉目录。
struct Home {
    dir: PathBuf,
}

impl Home {
    fn new() -> Home {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // 放在 cargo 的 target/tmp 下，不放系统的临时目录：Defender 把复制到临时目录里再启动的
        // 没签名的程序当成木马拦下来（2026-11-14 实测报 Trojan:Win32/Bearfoos.A!ml，Feng 同意挪到这里）
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "kuaid-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Home { dir }
    }

    /// 装一个版本：`~/.kuai/<build>/kuaid.exe`。
    fn install(&self, build: &str) -> PathBuf {
        let dir = self.dir.join(".kuai").join(build);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("kuaid.exe");
        if std::fs::hard_link(kuaid_bin(), &exe).is_err() {
            std::fs::copy(kuaid_bin(), &exe).unwrap();
        }
        exe
    }

    fn command(&self, exe: &Path) -> Command {
        let mut cmd = Command::new(exe);
        cmd.env("FR_KUAID_HOME", &self.dir)
            .env_remove("SSH_AUTH_SOCK")
            .current_dir(&self.dir);
        cmd
    }

    /// 像客户端那样连上来：启动转接程序。
    fn connect(&self, exe: &Path) -> Client {
        let mut child = self
            .command(exe)
            .arg("--connect")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(message)) = protocol::read_message::<_, Response>(&mut reader) {
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        Client {
            child,
            stdin: Some(stdin),
            rx,
            kept: VecDeque::new(),
            next: 1,
        }
    }

    fn run(&self, exe: &Path, arg: &str) -> (bool, String) {
        let out = self.command(exe).arg(arg).output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    fn run_dir(&self) -> PathBuf {
        self.dir.join(".kuai").join("run")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.run_dir().join("kuaid.log")).unwrap_or_default()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        // 用哪个版本的程序去叫停都行
        if let Ok(entries) = std::fs::read_dir(self.dir.join(".kuai")) {
            for entry in entries.flatten() {
                let exe = entry.path().join("kuaid.exe");
                if exe.is_file() {
                    let _ = self.command(&exe).arg("--stop").output();
                    break;
                }
            }
        }
        if std::thread::panicking() {
            eprintln!("---- 常驻进程的日志 ----\n{}", self.log());
        }
        // 程序文件要等进程真的退出才删得掉
        for _ in 0..50 {
            if std::fs::remove_dir_all(&self.dir).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<(Response, Vec<u8>)>,
    /// 收到了、还没人要的消息：等某一条时别的先留在这里
    kept: VecDeque<(Response, Vec<u8>)>,
    next: u64,
}

/// 终端里开的 shell：不读用户的配置，免得各台电脑不一样。
fn shell() -> Value {
    json!(["powershell.exe", "-NoLogo", "-NoProfile"])
}

impl Client {
    fn send(&mut self, method: &str, params: Value, payload: &[u8]) -> u64 {
        let id = self.next;
        self.next += 1;
        let request = Request {
            id,
            method: method.into(),
            params,
            bytes: payload.len() as u64,
        };
        let stdin = self.stdin.as_mut().unwrap();
        protocol::write_message(stdin, &request, payload).unwrap();
        stdin.flush().unwrap();
        id
    }

    /// 等第一条符合 `want` 的消息；`what` 是在等什么，等不到时报出来。
    fn take(&mut self, what: &str, want: impl Fn(&Response) -> bool) -> (Response, Vec<u8>) {
        if let Some(at) = self.kept.iter().position(|(response, _)| want(response)) {
            return self.kept.remove(at).unwrap();
        }
        loop {
            let Ok(message) = self.rx.recv_timeout(TIMEOUT) else {
                panic!("等不到：{what}");
            };
            if want(&message.0) {
                return message;
            }
            self.kept.push_back(message);
        }
    }

    /// 请求 `id` 最后的那条响应（不是中间的一块，也不是要密码的询问）。
    fn reply(&mut self, id: u64, what: &str) -> (Response, Vec<u8>) {
        self.take(what, |r| r.id == id && !r.chunk && r.ask.is_none())
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params, &[]);
        let (response, _) = self.reply(id, &format!("{method} 的回答"));
        assert_eq!(response.error, None, "{method} 失败");
        response.result.unwrap_or(Value::Null)
    }

    fn hello(&mut self) -> Hello {
        serde_json::from_value(self.call("hello", json!({ "protocol": PROTOCOL_VERSION }))).unwrap()
    }

    fn info(&mut self) -> DaemonInfo {
        serde_json::from_value(self.call("info", json!({}))).unwrap()
    }

    fn terms(&mut self) -> Vec<TermInfo> {
        serde_json::from_value(self.call("term_list", json!({}))["terms"].clone()).unwrap()
    }

    /// 开（或者接上）名字是 `key` 的终端。返回流的编号和开头那条消息。
    /// 开得宽一些：一行放得下测试里打的那些长路径，不折行。
    fn open(&mut self, key: &str, cwd: &Path) -> (u64, TermOpened) {
        let id = self.send(
            "term_open",
            json!({ "key": key, "cwd": cwd, "cols": 220, "rows": 30, "shell": shell() }),
            &[],
        );
        let (response, _) = self.take("term_open 的回答", |r| r.id == id && r.ask.is_none());
        assert_eq!(response.error, None, "开不了终端");
        (
            id,
            serde_json::from_value(response.result.unwrap()).unwrap(),
        )
    }

    /// 敲一行命令（终端里的回车是 `\r`）。
    fn line(&mut self, term: &str, text: &str) {
        self.send(
            "term_input",
            json!({ "term": term }),
            format!("{text}\r").as_bytes(),
        );
    }

    /// 收这个流上的输出，直到里面出现 `needle`。返回收到的全部。
    fn output_until(&mut self, stream: u64, needle: &str) -> String {
        let mut seen = String::new();
        let deadline = Instant::now() + TIMEOUT;
        while !seen.contains(needle) {
            let what = format!("终端里的 {needle:?}，收到的是：{seen:?}");
            assert!(Instant::now() < deadline, "等不到：{what}");
            let (response, payload) = self.take(&what, |r| r.id == stream && r.ask.is_none());
            assert!(
                response.chunk,
                "终端提前结束了：{response:?}，收到的是：{seen:?}"
            );
            seen.push_str(&String::from_utf8_lossy(&payload));
        }
        seen
    }

    /// 收到这个流结束：程序的退出码。
    fn exit_of(&mut self, stream: u64) -> Option<i32> {
        let (response, _) = self.reply(stream, "终端结束的消息");
        assert_eq!(response.error, None);
        let exit: TermExit = serde_json::from_value(response.result.unwrap()).unwrap();
        exit.exit
    }

    /// 等常驻进程来要密码：询问的编号和它问的话。
    fn next_ask(&mut self) -> (u64, String) {
        let (response, _) = self.take("要密码的询问", |r| r.ask.is_some());
        (response.id, response.ask.unwrap())
    }

    /// 像 ssh 断开那样走掉：关掉标准输入，转接程序应当自己退出。
    fn leave(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + TIMEOUT;
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "转接程序没有退出");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// 等转接程序自己退出（常驻进程把连接断了）。
    fn wait_closed(mut self) {
        let deadline = Instant::now() + TIMEOUT;
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "连接没有被断开");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !check() {
        assert!(Instant::now() < deadline, "等不到：{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn process_exists(pid: &str) -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
}

/// 终端里问一句、答一句：敲 `echo "…$(1+1)"` 这类命令，回显里没有算出来的结果，
/// 等到结果出现才算程序真的执行了。
fn pid_of_shell(client: &mut Client, stream: u64, term: &str) -> String {
    client.line(term, r#"echo "pid=$PID ok-$(1+1)""#);
    let seen = client.output_until(stream, "ok-2");
    let at = seen.rfind("pid=").unwrap();
    seen[at + 4..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect()
}

#[test]
fn 拉起常驻进程_终端在客户端走了以后还在() {
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    let mut first = home.connect(&exe);
    let hello = first.hello();
    assert!(hello.daemon);
    assert_eq!(hello.os, "windows");
    assert_eq!(hello.build, "1.0.0-aaaaaaaaaaaa");
    assert_eq!(hello.protocol, PROTOCOL_VERSION);
    // 给客户端的路径用正斜杠
    assert!(!hello.home.contains('\\'), "{}", hello.home);
    let daemon_pid = first.info().pid;

    let (stream, opened) = first.open(r"C:\Repo", &home.dir);
    assert!(opened.created);
    first.line(&opened.term, r#"echo "KUAID-$(6*7)""#);
    first.output_until(stream, "KUAID-42");
    // 在指定的目录里、环境变量对
    first.line(
        &opened.term,
        r#"echo "cwd=$PWD term=$env:TERM in=$env:KUAI_TERM done-$(1+1)""#,
    );
    let seen = first.output_until(stream, "done-2");
    assert!(
        seen.contains(&format!("cwd={}", home.dir.display())),
        "{seen}"
    );
    assert!(seen.contains("term=xterm-256color"), "{seen}");
    assert!(seen.contains(&format!("in={} ", opened.term)), "{seen}");
    let shell_pid = pid_of_shell(&mut first, stream, &opened.term);

    // 改大小
    first.send(
        "term_resize",
        json!({ "term": opened.term, "cols": 180, "rows": 40 }),
        &[],
    );
    first.line(
        &opened.term,
        r#"echo "w=$($Host.UI.RawUI.WindowSize.Width)x$($Host.UI.RawUI.WindowSize.Height)!""#,
    );
    first.output_until(stream, "w=180x40!");

    // 客户端走了
    first.leave();
    let mut second = home.connect(&exe);
    assert_eq!(second.info().pid, daemon_pid, "常驻进程不该换");
    let terms = second.terms();
    assert_eq!(terms.len(), 1);
    assert!(terms[0].alive);
    assert_eq!(terms[0].attached, 0);
    // 名字统一成正斜杠、小写
    assert_eq!(terms[0].key, "c:/repo");
    assert!(process_exists(&shell_pid), "shell 被带走了");

    // 同一个目录换一种写法再开：接上原来的，屏幕还原出来
    let (stream, again) = second.open("c:/repo", &home.dir);
    assert_eq!(
        (again.term.as_str(), again.created),
        (opened.term.as_str(), false)
    );
    second.output_until(stream, "KUAID-42");
    assert_eq!(pid_of_shell(&mut second, stream, &again.term), shell_pid);

    // 状态命令看得到它
    let (ok, status) = home.run(&exe, "--status");
    assert!(ok, "{status}");
    assert!(
        status.contains(&again.term) && status.contains("运行中"),
        "{status}"
    );

    // shell 退出：收到退出码，会话没了
    second.line(&again.term, "exit 7");
    assert_eq!(second.exit_of(stream), Some(7));
    wait_until("会话被忘掉", || second.terms().is_empty());
    wait_until("shell 退出", || !process_exists(&shell_pid));
}

#[test]
fn 不点名时开系统的默认_shell() {
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    let mut client = home.connect(&exe);
    client.hello();
    let id = client.send(
        "term_open",
        json!({ "key": "x", "cwd": home.dir, "cols": 120, "rows": 30 }),
        &[],
    );
    let (response, _) = client.take("term_open 的回答", |r| r.id == id && r.ask.is_none());
    assert_eq!(response.error, None, "开不了终端");
    let opened: TermOpened = serde_json::from_value(response.result.unwrap()).unwrap();
    // PowerShell 和命令提示符都认这一句
    client.line(&opened.term, "echo shell-is-up");
    client.output_until(id, "shell-is-up");
    assert!(client.terms().iter().any(|term| term.alive));
    assert_eq!(
        client.call("term_close", json!({ "term": opened.term }))["closed"],
        true
    );
    client.exit_of(id);
}

#[test]
fn 结束终端会把里面的程序都带走() {
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    let mut client = home.connect(&exe);
    client.hello();
    let (stream, opened) = client.open("repo", &home.dir);
    let shell_pid = pid_of_shell(&mut client, stream, &opened.term);
    // 前台跑着一个不会自己结束的程序，记下它的进程号
    client.line(
        &opened.term,
        r#"$p = Start-Process ping.exe -ArgumentList '-n','600','127.0.0.1' -NoNewWindow -PassThru; echo "child=$($p.Id) up-$(1+1)"; $p.WaitForExit()"#,
    );
    let seen = client.output_until(stream, "up-2");
    let at = seen.rfind("child=").unwrap();
    let child_pid: String = seen[at + 6..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    assert!(process_exists(&child_pid));
    let closed = client.call("term_close", json!({ "term": opened.term }));
    assert_eq!(closed["closed"], true);
    client.exit_of(stream);
    wait_until("shell 退出", || !process_exists(&shell_pid));
    wait_until("shell 起的程序退出", || !process_exists(&child_pid));
    assert!(client.terms().is_empty());
    // 开在不存在的目录里：回错误，连接照常
    let id = client.send(
        "term_open",
        json!({ "key": "x", "cwd": "C:/nonexistent/dir", "cols": 80, "rows": 24, "shell": shell() }),
        &[],
    );
    let (response, _) = client.reply(id, "开不了终端的报错");
    assert!(response.error.is_some());
    client.call("ping", json!({}));
}

#[test]
fn 没人看着时结束的终端_下次来看还看得到() {
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    let mut first = home.connect(&exe);
    first.hello();
    let (stream, opened) = first.open("repo", &home.dir);
    pid_of_shell(&mut first, stream, &opened.term);
    // 两秒后自己退出
    first.line(
        &opened.term,
        r#"Start-Sleep 2; echo "LAST-$(40+2)"; exit 3"#,
    );
    first.output_until(stream, "Start-Sleep");
    first.leave();

    let mut second = home.connect(&exe);
    second.hello();
    wait_until("程序结束", || {
        second.terms().iter().all(|term| !term.alive)
    });
    assert_eq!(second.terms().len(), 1, "还没人看过，应该留着");
    let (stream, again) = second.open("repo", &home.dir);
    assert!(!again.created);
    second.output_until(stream, "LAST-42");
    assert_eq!(second.exit_of(stream), Some(3));
    assert!(second.terms().is_empty());
}

#[test]
fn 新版本连上来_常驻进程换成新的_终端留着() {
    let home = Home::new();
    let old = home.install("1.0.0-aaaaaaaaaaaa");
    let new = home.install("1.0.1-bbbbbbbbbbbb");
    let mut first = home.connect(&old);
    assert_eq!(first.hello().build, "1.0.0-aaaaaaaaaaaa");
    let daemon_pid = first.info().pid;
    let (stream, opened) = first.open("repo", &home.dir);
    first.line(&opened.term, r#"$MARK='kept'; echo "READY-$(6*7)""#);
    first.output_until(stream, "READY-42");
    let shell_pid = pid_of_shell(&mut first, stream, &opened.term);
    // 一个开着但没人看的终端，和一个已经结束、没人看过的
    let (_, idle) = first.open("idle", &home.dir);
    let (ended_stream, ended) = first.open("ended", &home.dir);
    pid_of_shell(&mut first, ended_stream, &ended.term);
    first.send("cancel", json!({ "id": ended_stream }), &[]);
    wait_until("从终端上断开", || {
        first
            .terms()
            .iter()
            .any(|t| t.term == ended.term && t.attached == 0)
    });
    first.line(&ended.term, r#"echo "GONE-$(1+1)"; exit 5"#);
    wait_until("那个终端结束", || {
        first
            .terms()
            .iter()
            .any(|t| t.term == ended.term && !t.alive)
    });
    // 升级的那一刻程序正在不停地输出，每行一个编号
    first.line(
        &opened.term,
        r#"$n=0; while($true){ $n++; echo "tick-$n."; Start-Sleep -Milliseconds 40 }"#,
    );
    first.output_until(stream, "tick-3.");

    // 新版本的客户端连上来
    let mut second = home.connect(&new);
    let hello = second.hello();
    assert_eq!(hello.build, "1.0.1-bbbbbbbbbbbb", "日志：\n{}", home.log());
    let new_pid = second.info().pid;
    assert_ne!(new_pid, daemon_pid, "Windows 上升级是换一个进程");
    // 原来连着的客户端被断开，旧的常驻进程退出
    first.wait_closed();
    wait_until("旧的常驻进程退出", || {
        !process_exists(&daemon_pid.to_string())
    });

    let terms = second.terms();
    assert_eq!(terms.len(), 3, "{terms:?}");
    assert!(terms.iter().any(|t| t.term == idle.term && t.alive));
    assert!(terms.iter().any(|t| t.term == ended.term && !t.alive));
    assert!(process_exists(&shell_pid));

    // 接上原来的终端：屏幕还在，后台的输出还在来，编号不重不漏
    let (stream, again) = second.open("repo", &home.dir);
    assert_eq!(
        (again.term.as_str(), again.created),
        (opened.term.as_str(), false)
    );
    let mut seen = second.output_until(stream, "tick-");
    // 停下那个循环（Ctrl+C），看 shell 还是那个、变量还在
    second.send("term_input", json!({ "term": again.term }), b"\x03");
    // 等提示符出来再敲：打断的那一刻敲进去的字会被 shell 丢掉
    seen.push_str(&second.output_until(stream, ">"));
    second.line(&again.term, r#"echo "mark=$MARK=""#);
    seen.push_str(&second.output_until(stream, "mark=kept="));
    assert_eq!(pid_of_shell(&mut second, stream, &again.term), shell_pid);
    // 还原出来的屏幕加上之后的输出里，最后那一段编号是连着的
    let ticks: Vec<u32> = seen
        .split("tick-")
        .skip(1)
        .filter_map(|rest| rest.split('.').next()?.parse().ok())
        .collect();
    assert!(ticks.len() > 3, "{seen}");
    // 屏幕是从上往下还原的，之后的输出接在后面：从最大的编号往回数，一个都不缺
    let last = *ticks.iter().max().unwrap();
    let tail: Vec<u32> = ticks.iter().copied().filter(|n| *n + 20 > last).collect();
    for n in last.saturating_sub(19).max(1)..=last {
        assert!(tail.contains(&n), "少了 tick-{n}：{ticks:?}");
    }

    // 结束了的那个：最后的画面和退出码带过来了
    let (stream, seen_ended) = second.open("ended", &home.dir);
    assert!(!seen_ended.created);
    second.output_until(stream, "GONE-2");
    assert_eq!(second.exit_of(stream), Some(5));
    // 新开的终端编号不和带过来的重
    let (_, fresh) = second.open("fresh", &home.dir);
    assert!(fresh.created);
    assert!(![&opened.term, &idle.term, &ended.term].contains(&&fresh.term));

    // 旧版本的客户端再连：不把常驻进程换回去，照样能用
    let mut third = home.connect(&old);
    assert_eq!(third.hello().build, "1.0.1-bbbbbbbbbbbb");
    assert_eq!(third.info().pid, new_pid);
    third.call("ping", json!({}));
    // 新版本那个客户端没被断开
    second.call("ping", json!({}));
    // 清理旧版本时不能删的那个目录
    let current = std::fs::read_to_string(home.run_dir().join("current")).unwrap();
    assert_eq!(current, "1.0.1-bbbbbbbbbbbb");
    // 交接文件读完就删
    assert!(!home.run_dir().join("handoff").exists());
}

#[test]
fn 几个客户端同时连_只拉起一个常驻进程() {
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    let mut clients: Vec<Client> = (0..4).map(|_| home.connect(&exe)).collect();
    let pids: Vec<u32> = clients.iter_mut().map(|client| client.info().pid).collect();
    assert!(pids.iter().all(|pid| *pid == pids[0]), "{pids:?}");
    assert_eq!(clients[0].info().clients, 4);
    // 各自执行 git，互不相干
    for client in &mut clients {
        let id = client.send("git", json!({ "repo": ".", "args": ["--version"] }), &[]);
        let (_, payload) = client.reply(id, "git 的结果");
        assert!(String::from_utf8_lossy(&payload).starts_with("git version"));
    }
}

#[test]
fn 两个客户端的_git_要密码_各问各的() {
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    let mut alice = home.connect(&exe);
    let mut bob = home.connect(&exe);
    alice.hello();
    bob.hello();
    // 不联网就能让 git 来要用户名和密码：没有凭据助手时 `git credential fill` 会经 askpass 问
    let args = json!({ "repo": ".", "args": ["-c", "credential.helper=", "credential", "fill"] });
    let a = alice.send("git", args.clone(), b"protocol=https\nhost=a.example\n\n");
    let b = bob.send("git", args, b"protocol=https\nhost=b.example\n\n");
    for (client, host, name) in [
        (&mut bob, "b.example", "bob"),
        (&mut alice, "a.example", "alice"),
    ] {
        let (ask, prompt) = client.next_ask();
        assert!(
            prompt.contains("Username") && prompt.contains(host),
            "{prompt}"
        );
        client.send("answer", json!({ "id": ask, "answer": name }), &[]);
        let (ask, prompt) = client.next_ask();
        assert!(
            prompt.contains("Password") && prompt.contains(host),
            "{prompt}"
        );
        client.send(
            "answer",
            json!({ "id": ask, "answer": format!("pw-{name}") }),
            &[],
        );
    }
    for (client, id, name) in [(&mut alice, a, "alice"), (&mut bob, b, "bob")] {
        let (_, payload) = client.reply(id, "git 的结果");
        let out = String::from_utf8_lossy(&payload);
        assert!(out.contains(&format!("username={name}\n")), "{out}");
        assert!(out.contains(&format!("password=pw-{name}\n")), "{out}");
    }
}

#[test]
fn 叫停以后终端里的程序也结束_端口文件删掉() {
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    // 没在跑的时候：状态命令报错，不会顺手拉起来
    let (ok, _) = home.run(&exe, "--status");
    assert!(!ok);
    assert!(!home.run_dir().join("kuaid.port").exists());

    let mut client = home.connect(&exe);
    client.hello();
    let (stream, opened) = client.open("repo", &home.dir);
    let shell_pid = pid_of_shell(&mut client, stream, &opened.term);
    let (ok, text) = home.run(&exe, "--stop");
    assert!(ok, "{text}");
    client.wait_closed();
    wait_until("shell 退出", || !process_exists(&shell_pid));
    // 只留下锁文件和日志
    let mut left: Vec<String> = std::fs::read_dir(home.run_dir())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["kuaid.lock", "kuaid.log"]);
    // 再连：拉起一个新的
    let mut again = home.connect(&exe);
    assert!(again.hello().daemon);
    assert!(again.terms().is_empty());
}

#[test]
fn 装好新版本后清掉旧版本_正在用的留着() {
    let home = Home::new();
    let old = home.install("1.0.0-aaaaaaaaaaaa");
    let stale = home.install("0.9.0-cccccccccccc");
    let mut client = home.connect(&old);
    client.hello();
    let new = home.install("1.0.1-bbbbbbbbbbbb");
    let (ok, text) = home.run(&new, "--installed");
    assert!(ok, "{text}");
    assert!(new.is_file());
    assert!(old.is_file(), "常驻进程正在用的那一版不能删");
    assert!(!stale.exists(), "没人用的旧版本应该删掉");
    assert!(home.run_dir().is_dir());
}

/// 传一块文件上去，返回 kuaid 的回答。
/// 扮成正在运行的 Kuai：守一根管道，名字写在 `run/kuai.pipe` 里；转接程序（或者正在升级的常驻进程）
/// 来请它拉起常驻进程，它就照客户端 `instance.rs` 那样跑 `kuaid.exe --spawn-daemon`，回一个 `1`。
/// 收到的请求都记下来。
struct FakeKuai {
    asked: Receiver<Value>,
}

impl FakeKuai {
    fn start(home: &Home) -> FakeKuai {
        use std::os::windows::io::FromRawHandle as _;
        use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
        use windows_sys::Win32::System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
        };
        const ERROR_PIPE_CONNECTED: u32 = 535;

        static NEXT: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            r"\\.\pipe\Kuai-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let create = move || {
            // SAFETY: 名字以 0 结尾；返回的句柄下面交给 File 管
            let pipe = unsafe {
                CreateNamedPipeW(
                    wide.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_REJECT_REMOTE_CLIENTS,
                    PIPE_UNLIMITED_INSTANCES,
                    512,
                    4096,
                    0,
                    std::ptr::null(),
                )
            };
            assert_ne!(pipe, INVALID_HANDLE_VALUE, "建不了管道");
            pipe as usize
        };
        // 先建好再写名字：照着名字找过来的一定连得上
        let mut pipe = create();
        std::fs::create_dir_all(home.run_dir()).unwrap();
        std::fs::write(home.run_dir().join("kuai.pipe"), &name).unwrap();
        let (tx, asked) = mpsc::channel();
        let dir = home.dir.clone();
        std::thread::spawn(move || {
            loop {
                let handle = pipe as *mut core::ffi::c_void;
                // SAFETY: 句柄是上面建的；连上以后归 File 所有，丢弃时关掉
                let connected = unsafe {
                    ConnectNamedPipe(handle, std::ptr::null_mut()) != 0
                        || GetLastError() == ERROR_PIPE_CONNECTED
                };
                let mut file = unsafe { std::fs::File::from_raw_handle(handle) };
                pipe = create();
                if !connected {
                    continue;
                }
                let mut line = String::new();
                if std::io::BufRead::read_line(&mut BufReader::new(&file), &mut line).is_err() {
                    continue;
                }
                let Ok(request) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let mut cmd = Command::new(request["daemon"].as_str().unwrap_or_default());
                cmd.arg("--spawn-daemon").env("FR_KUAID_HOME", &dir);
                if let Some(state) = request["resume"].as_str() {
                    cmd.args(["--resume", state]);
                }
                let started = cmd.status().is_ok_and(|status| status.success());
                if tx.send(request).is_err() {
                    return;
                }
                if started {
                    let _ = file.write_all(b"1");
                    let _ = file.sync_all();
                }
            }
        });
        FakeKuai { asked }
    }

    /// 下一条收到的请求。
    fn next(&self) -> Value {
        self.asked.recv_timeout(TIMEOUT).expect("没有来请 Kuai")
    }
}

#[test]
fn 运行着_kuai_时常驻进程由它拉起_升级换新也是() {
    let home = Home::new();
    let old = home.install("1.0.0-aaaaaaaaaaaa");
    let new = home.install("1.0.1-bbbbbbbbbbbb");
    let kuai = FakeKuai::start(&home);
    let mut first = home.connect(&old);
    assert_eq!(first.hello().build, "1.0.0-aaaaaaaaaaaa");
    let asked = kuai.next();
    assert_eq!(asked["daemon"].as_str().map(Path::new), Some(old.as_path()));
    assert!(asked["resume"].is_null(), "{asked}");
    // 只请了这一次：常驻进程是它拉起的那一个
    assert!(kuai.asked.try_recv().is_err());
    let old_pid = first.info().pid;
    let (stream, opened) = first.open("repo", &home.dir);
    let shell_pid = pid_of_shell(&mut first, stream, &opened.term);

    // 新版本连上来：旧的常驻进程把交接文件写好，请 Kuai 起新的
    let mut second = home.connect(&new);
    assert_eq!(second.hello().build, "1.0.1-bbbbbbbbbbbb");
    let asked = kuai.next();
    assert_eq!(asked["daemon"].as_str().map(Path::new), Some(new.as_path()));
    let handoff = home.run_dir().join("handoff");
    assert_eq!(
        asked["resume"].as_str().map(Path::new),
        Some(handoff.as_path())
    );
    first.wait_closed();
    assert_ne!(second.info().pid, old_pid);
    let terms = second.terms();
    assert!(
        terms.iter().any(|t| t.term == opened.term && t.alive),
        "{terms:?}"
    );
    assert!(process_exists(&shell_pid), "终端里的 shell 没了");
    assert!(
        home.log().contains("已请 Kuai 启动新程序"),
        "{}",
        home.log()
    );
}

/// 转接程序发现常驻进程在会话 0、自己在桌面上时发的就是 `move`（那种情形这里造不出来，
/// 直接发这个请求）：常驻进程用自己这一版重新起一遍，请 Kuai 来起，终端留着。
#[test]
fn 搬到桌面上_同一版重新起一遍_终端留着() {
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    let mut first = home.connect(&exe);
    let old_pid = first.info().pid;
    assert!(first.info().session.is_some());
    let (stream, opened) = first.open("repo", &home.dir);
    let shell_pid = pid_of_shell(&mut first, stream, &opened.term);
    // 常驻进程是转接程序自己拉起的；之后 Kuai 启动了
    let kuai = FakeKuai::start(&home);
    first.send("move", json!({}), &[]);
    let asked = kuai.next();
    assert_eq!(asked["daemon"].as_str().map(Path::new), Some(exe.as_path()));
    assert!(asked["resume"].is_string(), "{asked}");
    first.wait_closed();
    let mut second = home.connect(&exe);
    assert_ne!(second.info().pid, old_pid);
    let terms = second.terms();
    assert!(
        terms.iter().any(|t| t.term == opened.term && t.alive),
        "{terms:?}"
    );
    assert!(process_exists(&shell_pid), "终端里的 shell 没了");
    // 接上去还敲得动
    let (stream, again) = second.open("repo", &home.dir);
    assert_eq!(again.term, opened.term);
    assert_eq!(pid_of_shell(&mut second, stream, &again.term), shell_pid);
}

#[test]
fn kuai_的管道连不上时自己拉起常驻进程() {
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    // Kuai 退出时没来得及删的文件：里面写的管道已经没有了
    std::fs::create_dir_all(home.run_dir()).unwrap();
    std::fs::write(home.run_dir().join("kuai.pipe"), r"\\.\pipe\Kuai-test-gone").unwrap();
    let mut client = home.connect(&exe);
    assert!(client.hello().daemon);
    // 写的不是 Kuai 的管道：不去连
    std::fs::write(home.run_dir().join("kuai.pipe"), r"\\.\pipe\other").unwrap();
    assert!(home.connect(&exe).hello().daemon);
}

fn put(client: &mut Client, params: Value, bytes: &[u8]) -> FilePutReply {
    let id = client.send("file_put", params, bytes);
    let (response, _) = client.reply(id, "file_put 的回答");
    assert_eq!(response.error, None, "file_put 失败");
    serde_json::from_value(response.result.unwrap()).unwrap()
}

#[test]
fn 传上来的文件_终端里的程序读得到_列目录从盘符往下() {
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    let mut client = home.connect(&exe);
    client.hello();
    let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let chunks: Vec<&[u8]> = content.chunks(120_000).collect();
    let first = put(&mut client, json!({ "name": "截 图 1.png" }), chunks[0]);
    put(&mut client, json!({ "upload": first.upload }), chunks[1]);
    let last = put(
        &mut client,
        json!({ "upload": first.upload, "done": true }),
        chunks[2],
    );
    assert_eq!(last.received, 300_000);
    // 放在 .kuai/drop 下，名字里的空格换掉了，路径用正斜杠：贴进终端不用加引号
    let path = last.path.unwrap();
    assert!(!path.contains('\\') && !path.contains(' '), "{path}");
    assert!(path.ends_with("/.kuai/drop/截_图_1.png"), "{path}");
    assert_eq!(std::fs::read(&path).unwrap(), content);

    // 终端里的程序拿这个路径读得到
    let (stream, opened) = client.open("repo", &home.dir);
    client.line(
        &opened.term,
        &format!(r#"echo "size=$((Get-Item {path}).Length)!""#),
    );
    client.output_until(stream, "size=300000!");

    // 列目录：虚拟的根下面是各个盘，点进去是盘的根目录
    let top = client.call("list_dir", json!({ "path": "/" }));
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    assert!(
        top["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == drive.as_str()),
        "{top}"
    );
    let root = client.call("list_dir", json!({ "path": format!("/{drive}") }));
    assert_eq!(root["path"], format!("{drive}/"));
    assert_eq!(root["parent"], "/");
}
