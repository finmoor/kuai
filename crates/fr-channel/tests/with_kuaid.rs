//! 用真的 kuaid 测客户端这一头（[`Connection`]）：线路断了以后在路上的请求报错、不卡住；重新连上，
//! 终端还在，断线前的输出还原得出来；保存时主机上的文件被别人改过就不写。
//!
//! kuaid 那一头的常驻、升级不断终端在 `crates/kuaid/tests/` 里测；这里测的是客户端怎么对待它。
//! 和 Kuai 连本机 kuaid 一样经转接程序（`kuaid --connect`）说协议；「线路断了」是把转接程序强行
//! 结束，等于 ssh 那条连接没了，常驻进程和里面的终端都还在。
//!
//! 每个测试一个自己的主目录（`FR_KUAID_HOME`），放在 cargo 的 target/tmp 下（Defender 会拦系统
//! 临时目录里没签名的程序）。要先编好 kuaid：`cargo test --workspace` 会编，单独跑这个包前先
//! `cargo build -p kuaid`，或者用环境变量 `KUAID_BIN` 指过去。
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fr_channel::{Channel, Connection, TermItem, TermStream};
use fr_core::protocol::{FsExpect, FsWrite};
use futures::StreamExt as _;
use futures::future::BoxFuture;

const TIMEOUT: Duration = Duration::from_secs(30);

fn kuaid_bin() -> PathBuf {
    if let Some(bin) = std::env::var_os("KUAID_BIN") {
        return PathBuf::from(bin);
    }
    // kuaid.exe 在 target/<档> 下。测试程序原来在 target/<档>/deps 下，rustc 1.100 起在
    // target/<档>/build/<包>/<哈希>/ 下面，所以往上逐级找
    let exe = std::env::current_exe().unwrap();
    exe.ancestors()
        .skip(1)
        .map(|dir| dir.join("kuaid.exe"))
        .find(|bin| bin.is_file())
        .unwrap_or_else(|| {
            panic!(
                "{} 的上级目录里没有 kuaid.exe：先 cargo build -p kuaid",
                exe.display()
            )
        })
}

/// 一个临时的主目录，里面按客户端上传的样子装着 kuaid。丢弃时让常驻进程退出、删掉目录。
struct Home {
    dir: PathBuf,
    exe: PathBuf,
}

impl Home {
    fn new() -> Home {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // CARGO_TARGET_DIR 写成 target/xxx 时这里是正反斜杠混着的，`cut` 按进程的路径找不到，
        // 逐段重拼成反斜杠
        let tmp: PathBuf = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .components()
            .collect();
        let dir = tmp.join(format!(
            "with-kuaid-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let build = dir.join(".kuai").join("test");
        std::fs::create_dir_all(&build).unwrap();
        let exe = build.join("kuaid.exe");
        let conpty = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/conpty/x64");
        for (from, to) in [
            (kuaid_bin(), exe.clone()),
            (conpty.join("conpty.dll"), build.join("conpty.dll")),
            (
                conpty.join("OpenConsole.exe"),
                build.join("OpenConsole.exe"),
            ),
        ] {
            if std::fs::hard_link(&from, &to).is_err() {
                std::fs::copy(&from, &to).unwrap();
            }
        }
        std::fs::create_dir_all(dir.join("files")).unwrap();
        Home { dir, exe }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.env("FR_KUAID_HOME", &self.dir)
            .env_remove("SSH_AUTH_SOCK")
            .current_dir(&self.dir);
        cmd
    }

    /// 像 Kuai 那样连上来：经转接程序。
    fn connect(&self) -> Arc<Connection> {
        let mut cmd = self.command();
        cmd.arg("--connect");
        Arc::new(Connection::spawn_stdio(cmd).unwrap())
    }

    /// 线路断了：强行结束这个主目录里的转接程序（常驻进程不动）。
    fn cut(&self) {
        let script = format!(
            "Get-CimInstance Win32_Process -Filter \"Name='kuaid.exe'\" | Where-Object {{ $_.ExecutablePath -and $_.ExecutablePath.StartsWith('{}', 'OrdinalIgnoreCase') -and $_.CommandLine -like '*--connect*' }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force; $_.ProcessId }}",
            self.dir.display()
        );
        let out = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &script,
            ])
            .output()
            .unwrap();
        let killed = String::from_utf8_lossy(&out.stdout);
        assert!(
            !killed.trim().is_empty(),
            "{} 里没找到转接程序：{}",
            self.dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn files(&self) -> String {
        self.dir.join("files").to_string_lossy().into_owned()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.command().arg("--stop").output();
        // 程序文件要等进程真的退出才删得掉
        for _ in 0..50 {
            if std::fs::remove_dir_all(&self.dir).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// 等一个 future，最多等 `limit`；等不到返回 None（说明卡住了）。
fn wait<T: Send + 'static>(future: BoxFuture<'static, T>, limit: Duration) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(futures::executor::block_on(future));
    });
    rx.recv_timeout(limit).ok()
}

/// 从终端流里一直收，直到收到的输出里有 `text`。返回流（接着用）和收到的全部输出。
fn read_until(stream: TermStream, text: &str) -> (TermStream, String) {
    let wanted = text.to_string();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stream = stream;
        let mut got = Vec::new();
        while let Some(item) = futures::executor::block_on(stream.next()) {
            if let Ok(TermItem::Output(bytes)) = item {
                got.extend(bytes);
                if String::from_utf8_lossy(&got).contains(&wanted) {
                    break;
                }
            }
        }
        let _ = tx.send((stream, String::from_utf8_lossy(&got).into_owned()));
    });
    let (stream, got) = rx.recv_timeout(TIMEOUT).expect("终端里等不到要的输出");
    assert!(got.contains(text), "流结束了也没等到：{got}");
    (stream, got)
}

/// 流里的第一项：会话编号和还原的那一段有多少字节。
fn opened(stream: &mut TermStream) -> (String, bool, u64) {
    let item = futures::executor::block_on(stream.next());
    match item {
        Some(Ok(TermItem::Opened {
            term,
            created,
            restore,
        })) => (term, created, restore.iter().sum()),
        other => panic!("第一项不是 Opened：{:?}", other.map(|r| r.is_ok())),
    }
}

/// 终端里开的 shell：不读用户的配置，免得各台电脑不一样。
fn shell() -> Vec<String> {
    ["powershell.exe", "-NoLogo", "-NoProfile"]
        .map(String::from)
        .to_vec()
}

#[test]
fn 线路断了_在路上的请求报错不卡住_之后的请求马上失败() {
    let home = Home::new();
    let conn = home.connect();
    let channel = Channel::Remote(conn.clone());
    let repo = home.files();
    assert!(
        wait(
            channel.git(&repo, vec!["init".into(), "-q".into()]),
            TIMEOUT
        )
        .is_some()
    );
    // 一个自己不会回的请求，等断线来结束它。`cut` 要起 PowerShell 查进程，CI 上慢的时候要二十多秒：
    // 请求睡得比它短就先跑完了，测到的不是断线
    let napping = channel.git(
        &repo,
        ["-c", "alias.nap=!sleep 600", "nap"]
            .map(String::from)
            .to_vec(),
    );
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(futures::executor::block_on(napping).map(|_| ()));
    });
    std::thread::sleep(Duration::from_millis(500));
    home.cut();
    // 从 `cut` 回来算起（它自己用了多久不算）：转接程序已经没了，在路上的请求很快报错
    let result = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("线路断了，请求还卡着");
    assert!(result.is_err(), "线路断了，请求却报成功");
    assert!(!conn.is_alive());
    // 之后的请求发的时候就失败
    let started = Instant::now();
    let ping = wait(conn.ping(), Duration::from_secs(5)).expect("断了以后的请求卡着");
    assert!(ping.is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn 线路断了再连上_终端还在_断线前的输出还原得出来() {
    let home = Home::new();
    let conn = home.connect();
    let mut stream = conn.term_open("reconnect", &home.files(), &shell(), 100, 30);
    let (term, created, _) = opened(&mut stream);
    assert!(created);
    // 等 shell 起来，打一句话
    let (stream, _) = read_until(stream, "PS ");
    conn.term_input(&term, b"echo ('before-' + 'cut')\r".to_vec());
    let (stream, _) = read_until(stream, "before-cut");

    home.cut();
    // 线路断了：终端流结束（报错或者没了），不卡着
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stream = stream;
        loop {
            match futures::executor::block_on(stream.next()) {
                None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            }
        }
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("线路断了，终端流还卡着");

    // 重新连上：终端还在，接上以后还原的那一段里有断线前的输出，接着能用
    let again = home.connect();
    let list = wait(again.term_list(), TIMEOUT).unwrap().unwrap();
    assert!(list.iter().any(|t| t.term == term), "终端没了");
    let mut stream = again.term_attach(&term, 100, 30);
    let (same, created, restore) = opened(&mut stream);
    assert_eq!(same, term);
    assert!(!created);
    assert!(restore > 0);
    let (stream, got) = read_until(stream, "before-cut");
    assert!(got.len() as u64 <= restore + 4096, "还原以后才出现");
    again.term_input(&term, b"echo ('after-' + 'reconnect')\r".to_vec());
    let (_stream, _) = read_until(stream, "after-reconnect");
    assert!(wait(again.term_close(&term), TIMEOUT).unwrap().is_ok());
}

#[test]
fn 保存时主机上的文件被别人改过_不写_按新的样子再存才写() {
    let home = Home::new();
    let channel = Channel::Remote(home.connect());
    let repo = home.files();
    let path = Path::new(&repo).join("a.txt");
    std::fs::write(&path, "打开时的内容\n").unwrap();
    let stat = wait(channel.fs_stat(&repo, "a.txt"), TIMEOUT)
        .unwrap()
        .unwrap();
    let opened_as = FsExpect::from(stat);

    // 编辑的这会儿，别的程序改了它（长度也变了，不靠修改时间的精度）
    std::fs::write(&path, "别人改过的内容，长一点\n").unwrap();
    let params = |expect| FsWrite {
        repo: repo.clone(),
        path: "a.txt".into(),
        create: false,
        expect: Some(expect),
    };
    let written = wait(
        channel.fs_write(params(opened_as), "我的修改\n".as_bytes().to_vec()),
        TIMEOUT,
    )
    .unwrap()
    .unwrap();
    assert!(written.conflict, "别人改过了还写");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "别人改过的内容，长一点\n"
    );

    // 冲突时回来的是磁盘上现在的样子：按它再存就写进去了
    let written = wait(
        channel.fs_write(
            params(FsExpect::from(written.stat)),
            "我的修改\n".as_bytes().to_vec(),
        ),
        TIMEOUT,
    )
    .unwrap()
    .unwrap();
    assert!(!written.conflict);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "我的修改\n");
}

/// 提交走的是「边执行边回输出、说明从标准输入给」这一路（`git_progress_with_input`）：经 kuaid 时
/// 说明作为请求带的字节发过去。提交成了、说明没丢，钩子的输出当进度回来。
#[test]
fn 流式执行时标准输入照样送到_提交的说明和钩子的输出都对() {
    use fr_channel::StreamItem;

    let home = Home::new();
    let channel = Channel::Remote(home.connect());
    let repo = home.files();
    let git = |args: &[&str]| {
        let out = wait(
            channel.git(&repo, args.iter().map(|arg| arg.to_string()).collect()),
            TIMEOUT,
        )
        .expect("git 卡住了")
        .unwrap();
        assert!(out.success(), "git {args:?}：{}", out.stderr);
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "t"]);
    git(&["config", "user.email", "t@example.com"]);
    std::fs::write(Path::new(&repo).join("a.txt"), "1\n").unwrap();
    git(&["add", "a.txt"]);
    // 钩子说一句话（git 把提交钩子的标准输出并进标准错误）
    let hook = Path::new(&repo)
        .join(".git")
        .join("hooks")
        .join("pre-commit");
    std::fs::write(&hook, "#!/bin/sh\necho hook-says-hello\n").unwrap();

    let message = "第一行\n\n正文，带中文";
    let stream = channel.git_progress_with_input(
        &repo,
        ["commit", "-F", "-"].map(String::from).to_vec(),
        Some(message.as_bytes().to_vec()),
    );
    let items = wait(Box::pin(stream.collect::<Vec<_>>()), TIMEOUT).expect("提交卡住了");
    let mut said = Vec::new();
    let mut done = None;
    for item in items {
        match item.expect("流里出了错") {
            StreamItem::Progress(chunk) => said.extend(chunk),
            StreamItem::Chunk(_) => {}
            StreamItem::Done(out) => done = Some(out),
        }
    }
    let done = done.expect("流结束了没有结果");
    assert!(done.success(), "提交没成：{}", done.stderr);
    assert!(
        String::from_utf8_lossy(&said).contains("hook-says-hello"),
        "钩子的输出没当进度回来：{}",
        String::from_utf8_lossy(&said)
    );
    assert_eq!(git(&["log", "-1", "--format=%B"]).trim(), message);
}
