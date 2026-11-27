//! 在 Linux 上拿真的 kuaid 程序测常驻这一套：拉起、终端断开后还在、原地升级、结束。
//!
//! 每个测试用一个自己的临时主目录（`HOME`），所以各有各的常驻进程，互不相干，也不碰真的 `~/.kuai`。
//! 测的是经转接程序（`kuaid --connect`）说协议，和客户端经 ssh 用它的方式一样。
//!
//! kuaid 程序：环境变量 `KUAID_BIN` 给的，没给就用 cargo 编出来的。开发机是 Windows，
//! 这个文件是交叉编译成 Linux 的测试程序，拿到 Linux 机器上跑：
//!
//! ```text
//! cargo test --no-run --release -p kuaid --target x86_64-unknown-linux-musl
//! # 把 target/x86_64-unknown-linux-musl/release/ 下的 kuaid 和 deps/linux-<哈希> 拷到 Linux 机器上
//! KUAID_BIN=./kuaid ./linux-<哈希>
//! ```
#![cfg(unix)]

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

const TIMEOUT: Duration = Duration::from_secs(15);

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
        let dir = std::env::temp_dir().join(format!(
            "kuaid-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Home { dir }
    }

    /// 装一个版本：`~/.kuai/<build>/kuaid`。
    fn install(&self, build: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.dir.join(".kuai").join(build);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("kuaid");
        // 硬链接，不拷贝：拷贝要开着文件写，这时别的测试线程正好在启动子进程的话，子进程会带着
        // 这个开着写的文件，接下来启动这个程序就报「Text file busy」。不在一个盘上才拷贝
        if std::fs::hard_link(kuaid_bin(), &exe).is_err() {
            std::fs::copy(kuaid_bin(), &exe).unwrap();
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        exe
    }

    fn command(&self, exe: &Path) -> Command {
        let mut cmd = Command::new(exe);
        // 状态文件和机器标识也在临时主目录下（~/.config/kuai/），不碰跑测试的人自己的
        cmd.env("HOME", &self.dir)
            .env("SHELL", "/bin/sh")
            .env_remove("SSH_AUTH_SOCK")
            .env_remove("XDG_CONFIG_HOME")
            .current_dir(&self.dir);
        cmd
    }

    /// 像客户端那样连上来：启动转接程序。
    fn connect(&self, exe: &Path) -> Client {
        self.connect_with(exe, None)
    }

    /// `agent`：这条 ssh 连接转发过来的 ssh-agent 的套接字。
    fn connect_with(&self, exe: &Path, agent: Option<&str>) -> Client {
        let mut cmd = self.command(exe);
        if let Some(agent) = agent {
            cmd.env("SSH_AUTH_SOCK", agent);
        }
        let mut child = cmd
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

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join(".kuai/run/kuaid.log")).unwrap_or_default()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        // 用哪个版本的程序去叫停都行
        if let Ok(entries) = std::fs::read_dir(self.dir.join(".kuai")) {
            for entry in entries.flatten() {
                let exe = entry.path().join("kuaid");
                if exe.is_file() {
                    let _ = self.command(&exe).arg("--stop").output();
                    break;
                }
            }
        }
        if std::thread::panicking() {
            eprintln!("---- 常驻进程的日志 ----\n{}", self.log());
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<(Response, Vec<u8>)>,
    /// 收到了、还没人要的消息：等某一条时别的先留在这里。几条流上的消息谁先到不一定
    /// （比如结束终端的回答和终端的退出消息），不能因为先等了一个就把另一个丢了
    kept: VecDeque<(Response, Vec<u8>)>,
    next: u64,
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

    /// 请求 `id` 最后的那条响应（不是中间的一块，也不是要密码的询问：询问的编号是另一套）。
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
    fn open(&mut self, key: &str, cwd: &Path) -> (u64, TermOpened) {
        let id = self.send(
            "term_open",
            json!({ "key": key, "cwd": cwd, "cols": 80, "rows": 24 }),
            &[],
        );
        // 这个流上的第一条：开好了，或者开不了的报错
        let (response, _) = self.take("term_open 的回答", |r| r.id == id && r.ask.is_none());
        assert_eq!(response.error, None, "开不了终端");
        (
            id,
            serde_json::from_value(response.result.unwrap()).unwrap(),
        )
    }

    fn input(&mut self, term: &str, text: &str) {
        self.send("term_input", json!({ "term": term }), text.as_bytes());
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

    /// 接上已有的终端后，先收完还原屏幕的那几段（`TermOpened::restore`），返回里面的字。之后收到的
    /// 才是接上以后的输出：等命令的结果时不会把还原出来的旧结果当成新的。
    fn restored(&mut self, stream: u64, opened: &TermOpened) -> String {
        let mut left: u64 = opened.restore.iter().sum();
        assert!(left > 0, "接上已有的终端应当有还原屏幕的输出");
        let mut seen = String::new();
        while left > 0 {
            let what = format!("还原屏幕的输出（还差 {left} 字节），收到的是：{seen:?}");
            let (response, payload) = self.take(&what, |r| {
                r.id == stream && r.ask.is_none() && r.result.is_none()
            });
            assert!(response.chunk, "终端提前结束了：{response:?}");
            left = left
                .checked_sub(payload.len() as u64)
                .expect("还原屏幕的输出比说的多");
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
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn process_exists(pid: &str) -> bool {
    Path::new("/proc").join(pid).exists()
}

/// 终端里问一句、答一句：敲 `echo <前缀>$((6*7))` 这类命令，回显里没有算出来的结果，
/// 等到结果出现才算程序真的执行了。
fn pid_of_shell(client: &mut Client, stream: u64, term: &str) -> String {
    client.input(term, "echo pid=$$ ok-$((1+1))\n");
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
    // 拉起常驻进程的这条连接带着 ssh-agent 转发
    let mut first = home.connect_with(&exe, Some("/nonexistent/first-connection"));
    let hello = first.hello();
    assert!(hello.daemon);
    assert_eq!(hello.build, "1.0.0-aaaaaaaaaaaa");
    assert_eq!(hello.protocol, PROTOCOL_VERSION);
    let daemon_pid = first.info().pid;

    let (stream, opened) = first.open("repo", &home.dir);
    assert!(opened.created);
    first.input(&opened.term, "echo KUAID-$((6*7))\n");
    first.output_until(stream, "KUAID-42");
    // 在仓库目录里、环境变量对
    first.input(
        &opened.term,
        "echo cwd=$PWD term=$TERM in=$KUAI_TERM auth=[$SSH_AUTH_SOCK] done-$((1+1))\n",
    );
    let seen = first.output_until(stream, "done-2");
    assert!(
        seen.contains(&format!("cwd={}", home.dir.display())),
        "{seen}"
    );
    assert!(seen.contains("term=xterm-256color"), "{seen}");
    assert!(seen.contains(&format!("in={}", opened.term)), "{seen}");
    // 终端拿到的是固定的地址，指向这条连接的 ssh-agent：以后换了连接，改指过去就行
    let link = home.dir.join(".kuai/run/auth.sock");
    assert!(
        seen.contains(&format!("auth=[{}]", link.display())),
        "{seen}"
    );
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        Path::new("/nonexistent/first-connection")
    );
    let shell_pid = pid_of_shell(&mut first, stream, &opened.term);
    // 起一个一直跑着的程序（测完收掉：shell 正常退出时不挂断后台作业，它会一直留着）
    first.input(&opened.term, "sleep 600 & echo bg=$! ok-$((3+3))\n");
    let seen = first.output_until(stream, "ok-6");
    let at = seen.rfind("bg=").unwrap();
    let background: String = seen[at + 3..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();

    // 改大小
    first.send(
        "term_resize",
        json!({ "term": opened.term, "cols": 100, "rows": 30 }),
        &[],
    );
    first.input(&opened.term, "stty size\n");
    first.output_until(stream, "30 100");

    // 客户端走了
    first.leave();
    let mut second = home.connect(&exe);
    assert_eq!(second.info().pid, daemon_pid, "常驻进程不该换");
    // 这台电脑的标识：存在状态文件旁边（~/.config/kuai/），每次握手都是它
    let store = home.dir.join(".config/kuai/workspaces.json");
    assert_eq!(hello.machine, fr_core::store::machine_id(&store).unwrap());
    assert_eq!(second.hello().machine, hello.machine);
    let terms = second.terms();
    assert_eq!(terms.len(), 1);
    assert!(terms[0].alive);
    assert_eq!(terms[0].attached, 0);
    assert_eq!(terms[0].key, "repo");
    assert!(process_exists(&shell_pid), "shell 被带走了");
    assert!(process_exists(&background), "终端里的程序被带走了");

    // 同一个名字再开：接上原来的，屏幕还原出来
    let (stream, again) = second.open("repo", &home.dir);
    assert_eq!(
        (again.term.as_str(), again.created),
        (opened.term.as_str(), false)
    );
    let seen = second.restored(stream, &again);
    assert!(seen.contains("KUAID-42"), "{seen}");
    assert_eq!(pid_of_shell(&mut second, stream, &again.term), shell_pid);

    // 状态命令看得到它
    let (ok, status) = home.run(&exe, "--status");
    assert!(ok, "{status}");
    assert!(
        status.contains(&again.term) && status.contains("运行中"),
        "{status}"
    );

    // 这条连接没带 ssh-agent 转发：它新开的终端里没有，常驻进程没记着拉起它的那条连接的
    let (plain_stream, plain) = second.open("plain", &home.dir);
    second.input(&plain.term, "echo auth=[$SSH_AUTH_SOCK] done-$((1+1))\n");
    let seen = second.output_until(plain_stream, "done-2");
    assert!(seen.contains("auth=[]"), "{seen}");
    assert_eq!(
        second.call("term_close", json!({ "term": plain.term }))["closed"],
        true
    );
    second.exit_of(plain_stream);

    // shell 退出：收到退出码，会话没了
    second.input(&again.term, "exit 7\n");
    assert_eq!(second.exit_of(stream), Some(7));
    wait_until("会话被忘掉", || second.terms().is_empty());
    let _ = Command::new("kill").args(["-9", &background]).status();
}

#[test]
fn 结束终端会把里面的程序都带走() {
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    let mut client = home.connect(&exe);
    client.hello();
    let (stream, opened) = client.open("repo", &home.dir);
    let shell_pid = pid_of_shell(&mut client, stream, &opened.term);
    let programs = |client: &mut Client| {
        client.call("term_programs", json!({ "term": opened.term }))["programs"].clone()
    };
    assert_eq!(programs(&mut client), json!([]), "shell 闲着时没有别的程序");
    // 前台跑着一个不会自己结束的程序
    client.input(&opened.term, "sleep 600\n");
    wait_until("看到前台的 sleep", || {
        programs(&mut client) == json!(["sleep"])
    });
    // 终端列表里也带着（协议 14）
    let listed = client.terms();
    assert_eq!(listed[0].programs, Some(vec!["sleep".to_string()]));
    let closed = client.call("term_close", json!({ "term": opened.term }));
    assert_eq!(closed["closed"], true);
    client.exit_of(stream);
    wait_until("shell 退出", || !process_exists(&shell_pid));
    assert!(client.terms().is_empty());
    // 开在不存在的目录里：回错误，连接照常
    let id = client.send(
        "term_open",
        json!({ "key": "x", "cwd": "/nonexistent/dir", "cols": 80, "rows": 24 }),
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
    // 两秒后自己退出
    first.input(&opened.term, "(sleep 2; echo LAST-$((40+2))) ; exit 3\n");
    first.output_until(stream, "sleep 2");
    first.leave();

    let mut second = home.connect(&exe);
    second.hello();
    wait_until("程序结束", || {
        second.terms().iter().all(|term| !term.alive)
    });
    assert_eq!(second.terms().len(), 1, "还没人看过，应该留着");
    let (stream, again) = second.open("repo", &home.dir);
    assert!(!again.created);
    let seen = second.restored(stream, &again);
    assert!(seen.contains("LAST-42"), "{seen}");
    assert_eq!(second.exit_of(stream), Some(3));
    assert!(second.terms().is_empty());
}

#[test]
fn 新版本连上来_常驻进程原地换掉_终端留着() {
    let home = Home::new();
    let old = home.install("1.0.0-aaaaaaaaaaaa");
    let new = home.install("1.0.1-bbbbbbbbbbbb");
    let mut first = home.connect(&old);
    assert_eq!(first.hello().build, "1.0.0-aaaaaaaaaaaa");
    let daemon_pid = first.info().pid;
    let (stream, opened) = first.open("repo", &home.dir);
    first.input(&opened.term, "MARK=kept; echo READY-$((6*7))\n");
    first.output_until(stream, "READY-42");
    let shell_pid = pid_of_shell(&mut first, stream, &opened.term);
    // 一个开着但没人看的终端，和一个已经结束、没人看过的
    let (_, idle) = first.open("idle", &home.dir);
    let (ended_stream, ended) = first.open("ended", &home.dir);
    first.send("cancel", json!({ "id": ended_stream }), &[]);
    wait_until("从终端上断开", || {
        first
            .terms()
            .iter()
            .any(|t| t.term == ended.term && t.attached == 0)
    });
    first.input(&ended.term, "echo GONE-$((1+1)); exit 5\n");
    wait_until("那个终端结束", || {
        first
            .terms()
            .iter()
            .any(|t| t.term == ended.term && !t.alive)
    });
    // 升级的那一刻程序正在不停地输出
    first.input(&opened.term, "while :; do echo tick; sleep 0.05; done &\n");
    first.output_until(stream, "tick");

    // 新版本的客户端连上来
    let mut second = home.connect(&new);
    let hello = second.hello();
    assert_eq!(hello.build, "1.0.1-bbbbbbbbbbbb", "日志：\n{}", home.log());
    assert_eq!(second.info().pid, daemon_pid, "换程序不换进程号");
    // 原来连着的客户端被断开
    first.wait_closed();

    let terms = second.terms();
    assert_eq!(terms.len(), 3, "{terms:?}");
    assert!(terms.iter().any(|t| t.term == idle.term && t.alive));
    assert!(terms.iter().any(|t| t.term == ended.term && !t.alive));
    assert!(process_exists(&shell_pid));

    // 接上原来的终端：屏幕还在，shell 还是那个，变量还在，后台的输出还在来
    let (stream, again) = second.open("repo", &home.dir);
    assert_eq!(
        (again.term.as_str(), again.created),
        (opened.term.as_str(), false)
    );
    // 先收完还原出来的屏幕（往回翻的记录多时分两段）：上面有那几行 tick
    let seen = second.restored(stream, &again);
    assert!(seen.contains("tick"), "{seen}");
    second.input(&again.term, "kill %1; echo mark=$MARK=\n");
    second.output_until(stream, "mark=kept=");
    assert_eq!(pid_of_shell(&mut second, stream, &again.term), shell_pid);
    // 结束了的那个：最后的画面和退出码带过来了
    let (stream, seen_ended) = second.open("ended", &home.dir);
    assert!(!seen_ended.created);
    let seen = second.restored(stream, &seen_ended);
    assert!(seen.contains("GONE-2"), "{seen}");
    assert_eq!(second.exit_of(stream), Some(5));
    // 新开的终端编号不和带过来的重
    let (_, fresh) = second.open("fresh", &home.dir);
    assert!(fresh.created);
    assert!(![&opened.term, &idle.term, &ended.term].contains(&&fresh.term));

    // 旧版本的客户端再连：不把常驻进程换回去，照样能用
    let mut third = home.connect(&old);
    assert_eq!(third.hello().build, "1.0.1-bbbbbbbbbbbb");
    assert_eq!(third.info().pid, daemon_pid);
    third.call("ping", json!({}));
    // 新版本那个客户端没被断开
    second.call("ping", json!({}));
    // 上传新版本时不能删的那个目录
    let current = std::fs::read_to_string(home.dir.join(".kuai/run/current")).unwrap();
    assert_eq!(current, "1.0.1-bbbbbbbbbbbb");
}

#[test]
fn 两个客户端同时连_只拉起一个常驻进程() {
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
    // 两个 git 同时在等；各自的客户端被问到的是自己那个地址的
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
fn 叫停以后终端里的程序也结束_套接字删掉() {
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    // 没在跑的时候：状态命令报错，不会顺手拉起来
    let (ok, _) = home.run(&exe, "--status");
    assert!(!ok);
    assert!(!home.dir.join(".kuai/run/kuaid.sock").exists());

    let mut client = home.connect(&exe);
    client.hello();
    let (stream, opened) = client.open("repo", &home.dir);
    let shell_pid = pid_of_shell(&mut client, stream, &opened.term);
    let (ok, text) = home.run(&exe, "--stop");
    assert!(ok, "{text}");
    client.wait_closed();
    wait_until("shell 退出", || !process_exists(&shell_pid));
    // 套接字文件（常驻进程听的、密码转发用的）都删了，只留下锁文件和日志
    let mut left: Vec<String> = std::fs::read_dir(home.dir.join(".kuai/run"))
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

/// 叫停时，前台一个不理挂断的程序也被强杀，常驻进程等到它没了才退出。shell 收到挂断马上就退了，
/// 读线程原来跟着结束、不再强杀，常驻进程一退它就一直留着。
#[test]
fn 叫停时不理挂断的前台程序也被强杀() {
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    let mut client = home.connect(&exe);
    client.hello();
    let (stream, opened) = client.open("repo", &home.dir);
    // 忽略了的信号 exec 以后还是忽略：这个 sleep 不理挂断。先打出它的进程号（exec 不换进程号）
    client.input(
        &opened.term,
        "sh -c 'trap \"\" HUP; echo stubborn=$$ ok-$((2+2)); exec sleep 600'\n",
    );
    let seen = client.output_until(stream, "ok-4");
    let at = seen.rfind("stubborn=").unwrap();
    let pid: String = seen[at + 9..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    assert!(process_exists(&pid), "{seen}");
    let (ok, text) = home.run(&exe, "--stop");
    assert!(ok, "{text}");
    client.wait_closed();
    wait_until("不理挂断的程序被强杀", || !process_exists(&pid));
}

/// 固定的 ssh-agent 地址（~/.kuai/run/auth.sock）跟着连接改指：新连上一条转发了 agent 的就指过去
/// （按编号接回的旧终端也用上新的），它断开了改指回别的还连着、也转发了的。
#[test]
fn ssh_agent_的固定地址跟着连接改指() {
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    let link = home.dir.join(".kuai/run/auth.sock");
    let target = || std::fs::read_link(&link).ok();
    let mut first = home.connect_with(&exe, Some("/nonexistent/first"));
    first.hello();
    wait_until("指向第一条连接的", || {
        target().as_deref() == Some(Path::new("/nonexistent/first"))
    });
    let mut second = home.connect_with(&exe, Some("/nonexistent/second"));
    second.hello();
    wait_until("改指到第二条连接的", || {
        target().as_deref() == Some(Path::new("/nonexistent/second"))
    });
    // 没转发 agent 的连接来来去去，不动它
    let mut plain = home.connect(&exe);
    plain.hello();
    plain.leave();
    assert_eq!(target().as_deref(), Some(Path::new("/nonexistent/second")));
    // 第二条断开：改指回还连着的第一条
    second.leave();
    wait_until("改指回第一条连接的", || {
        target().as_deref() == Some(Path::new("/nonexistent/first"))
    });
    first.leave();
}

/// 传一块文件上去，返回 kuaid 的回答。
fn put(client: &mut Client, params: Value, bytes: &[u8]) -> FilePutReply {
    let id = client.send("file_put", params, bytes);
    let (response, _) = client.reply(id, "file_put 的回答");
    assert_eq!(response.error, None, "file_put 失败");
    serde_json::from_value(response.result.unwrap()).unwrap()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn 传上来的文件_终端里的程序读得到_没传完的客户端走了就删() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new();
    let exe = home.install("1.0.0-aaaaaaaaaaaa");
    let mut client = home.connect(&exe);
    client.hello();
    // 分三块传：开头、中间、最后一块带上「传完了」
    let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let chunks: Vec<&[u8]> = content.chunks(120_000).collect();
    let first = put(&mut client, json!({ "name": "截 图 1.png" }), chunks[0]);
    assert_eq!((first.received, &first.path), (120_000, &None));
    let second = put(&mut client, json!({ "upload": first.upload }), chunks[1]);
    assert_eq!((second.received, &second.path), (240_000, &None));
    let last = put(
        &mut client,
        json!({ "upload": first.upload, "done": true }),
        chunks[2],
    );
    assert_eq!(last.received, 300_000);
    // 放在主目录的 .kuai/drop 下，名字里的空格换掉了：贴进终端不用加引号
    let drop_dir = home.dir.join(".kuai/drop");
    let path = PathBuf::from(last.path.unwrap());
    assert_eq!(path, drop_dir.join("截_图_1.png"));
    assert_eq!(std::fs::read(&path).unwrap(), content);
    // 只有本人能读
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&drop_dir), 0o700);
    // 传完的那次上传不在了，不能接着往里写
    let id = client.send("file_put", json!({ "upload": first.upload }), b"more");
    let (response, _) = client.reply(id, "传完以后再传的回答");
    assert!(response.error.is_some());

    // 终端里的程序拿这个路径读得到
    let (stream, opened) = client.open("repo", &home.dir);
    let command = format!("wc -c < {}; echo read-$?\n", path.display());
    client.input(&opened.term, &command);
    let out = client.output_until(stream, "read-0");
    assert!(out.contains("300000"), "{out}");

    // 另一个客户端传到一半走了：没传完的不留
    let mut other = home.connect(&exe);
    other.hello();
    let begun = put(&mut other, json!({ "name": "big.bin" }), &content[..1000]);
    assert_eq!(begun.path, None);
    let partial = || names(&drop_dir).iter().any(|name| name.ends_with(".part"));
    assert!(partial(), "{:?}", names(&drop_dir));
    other.leave();
    wait_until("没传完的被删掉", || !partial());
    assert_eq!(names(&drop_dir), ["截_图_1.png"]);
}

/// 这个用户现在的 crontab；还没有是空的。
fn crontab() -> String {
    let out = Command::new("crontab").arg("-l").output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// 测试改的是这个用户真的 crontab：中途失败时把测试加的那一行（含这个脚本路径的）删掉，别的行
/// 不动；只剩测试那一行时整份删掉。测试走完时那一行已经由「关掉」删了，这里什么都不做。
struct CrontabLine(String);

impl Drop for CrontabLine {
    fn drop(&mut self) {
        let listed = crontab();
        if !listed.contains(&self.0) {
            return;
        }
        let kept: String = listed
            .lines()
            .filter(|line| !line.contains(&self.0))
            .map(|line| format!("{line}\n"))
            .collect();
        if kept.trim().is_empty() {
            let _ = Command::new("crontab").arg("-r").status();
        } else if let Ok(mut child) = Command::new("crontab")
            .arg("-")
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(kept.as_bytes());
            }
            let _ = child.wait();
        }
    }
}

#[test]
fn 开机时启动_写进_crontab_脚本拉起常驻进程() {
    if Command::new("crontab").arg("-l").output().is_err() {
        eprintln!("这台机器没有 crontab，跳过");
        return;
    }
    let home = Home::new();
    let exe = home.install("1.0.0-boot");
    let mut client = home.connect(&exe);
    client.hello();
    let status = client.call("daemon_status", json!({}));
    assert_eq!(status["autostart"], false, "{status}");
    assert!(status["clock"]["now"]["year"].as_u64().unwrap() >= 2026);
    let before = crontab();

    let script = home.dir.join(".kuai/boot.sh");
    let _restore = CrontabLine(script.display().to_string());
    let status = client.call("autostart_set", json!({ "enabled": true }));
    assert_eq!(status["autostart"], true, "{status}");
    let listed = crontab();
    assert!(
        listed.contains(&format!("@reboot '{}'", script.display())),
        "{listed}"
    );
    // 原来的那些行原样留着
    for line in before.lines() {
        assert!(listed.contains(line), "{listed}");
    }
    let text = std::fs::read_to_string(&script).unwrap();
    assert!(text.contains(&*exe.to_string_lossy()), "{text}");

    // 常驻进程停了；像开机时那样跑脚本（cron 给的 SHELL 是 /bin/sh）：拉起来了
    let (ok, text) = home.run(&exe, "--stop");
    assert!(ok, "{text}");
    client.wait_closed();
    wait_until("常驻进程退出", || !home.run(&exe, "--status").0);
    let out = Command::new("/bin/sh")
        .arg(&script)
        .env("HOME", &home.dir)
        .env("SHELL", "/bin/sh")
        .env_remove("SSH_AUTH_SOCK")
        .current_dir("/")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (ok, text) = home.run(&exe, "--status");
    assert!(ok, "{text}");

    let mut client = home.connect(&exe);
    client.hello();
    // 终端开的是账号登记的登录 shell，不是 cron 给的 sh：~/.bashrc 里加的 PATH（nvm 装的 claude）才有
    let passwd = Command::new("sh")
        .args(["-c", "getent passwd \"$(id -u)\" | cut -d: -f7"])
        .output()
        .unwrap();
    let passwd = String::from_utf8_lossy(&passwd.stdout).trim().to_string();
    if let Some(name) = passwd
        .rsplit('/')
        .next()
        .filter(|_| passwd.starts_with('/'))
    {
        let (stream, opened) = client.open("boot-shell", &home.dir);
        client.input(&opened.term, "echo shell=$0 ok-$((1+1))\n");
        let seen = client.output_until(stream, "ok-2");
        assert!(seen.contains(&format!("shell=-{name}")), "{passwd}：{seen}");
        client.call("term_close", json!({ "term": opened.term }));
    }

    // 关掉：那一行和脚本都删掉，别的行不动
    let status = client.call("autostart_set", json!({ "enabled": false }));
    assert_eq!(status["autostart"], false);
    assert_eq!(crontab().trim_end(), before.trim_end());
    assert!(!script.exists());
    // 什么都不剩时整份删掉，不留一份空的 crontab
    let left = Command::new("crontab").arg("-l").output().unwrap();
    assert_eq!(left.status.success(), !before.trim().is_empty());
}

/// 在 `dir` 里建一个有一个提交的仓库。
fn make_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}：{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&["add", "a.txt"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "一",
    ]);
}

#[test]
fn 定时任务_sh_里提示词原样传进去_工作树用完删掉() {
    let home = Home::new();
    let exe = home.install("1.0.0-tasks");
    let repo = home.dir.join("code").join("app");
    make_repo(&repo);
    let mut client = home.connect(&exe);
    client.hello();
    // 提示词里有引号、$、中文：放在环境变量里传，shell 不展开、不拆开
    let prompt = "kuai-task-hello \"引号\" $HOME 'x' `id`";
    let task = json!({
        "id": "t1",
        "name": "夜间 测试",
        "prompt": prompt,
        "repo": repo,
        "preset": { "name": "echo", "command": "printf '%s\\n'" },
        "launch": { "command": "printf '%s\\n'", "prompt": "arg" },
        "schedule": { "kind": "manual" },
        "worktree": "head",
    });
    client.call(
        "task_apply",
        json!({ "ops": [{ "op": "set", "task": task }] }),
    );
    let run = client.call("task_run", json!({ "id": "t1" }));
    assert_eq!(run["outcome"], "started", "{run}");
    let term = run["term"].as_str().unwrap().to_string();
    let worktree = PathBuf::from(run["worktree"].as_str().unwrap());
    assert!(worktree.is_dir(), "工作树没建：{worktree:?}");
    assert_eq!(
        worktree.parent().unwrap(),
        repo.with_file_name("app.worktrees")
    );
    let name = worktree.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("夜间-测试-"), "{name}");
    let info = client.terms().into_iter().find(|t| t.term == term).unwrap();
    assert_eq!(PathBuf::from(&info.cwd), worktree);
    let stream = client.send(
        "term_attach",
        json!({ "term": term, "cols": 120, "rows": 36, "watch": true }),
        &[],
    );
    // 命令行里只有变量名，原样的提示词只会出现在输出里
    let seen = client.output_until(stream, "$HOME 'x' `id`");
    assert!(seen.contains("KUAI_TASK_PROMPT"), "{seen:?}");
    client.send("cancel", json!({ "id": stream }), &[]);

    client.call("term_close", json!({ "term": term }));
    let ended = client.call("task_get", json!({}))["runs"][0].clone();
    assert!(ended["ended"].is_u64(), "{ended}");
    assert_eq!(ended["screen"], true, "{ended}");
    assert_eq!(ended["changes"], 0, "{ended}");
    wait_until("没改动的工作树删掉", || !worktree.exists());
    wait_until("空着的 app.worktrees 一起删掉", || {
        !worktree.parent().unwrap().exists()
    });
    let branches = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["branch", "--list"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&branches.stdout).contains("夜间"),
        "分支没删"
    );
    let id = client.send("task_screen", json!({ "run": ended["id"] }), &[]);
    let (response, screen) = client.reply(id, "task_screen 的回答");
    assert_eq!(response.error, None);
    let screen = String::from_utf8_lossy(&screen);
    assert!(screen.contains("$HOME 'x' `id`"), "{screen:?}");
}
