//! fr-probe：测仓库通道的延迟和耗时，也用来检查自动安装 kuaid。
//!
//! 用法：
//!     fr-probe --repo <仓库路径> local                  本机通道（进程内启动 git）
//!     fr-probe --repo <服务器上的仓库路径> <ssh 目标>      连服务器（需要时自动上传 kuaid）
//!     fr-probe --repo <仓库路径> --kuaid <kuaid 程序>   直接启动 kuaid 进程，测协议本身
//! 最后加 `--watch <秒数>`：不测速，只监听仓库这么久，每收到一次变化通知打一行。
//! 例如：
//!     fr-probe --repo .testdata/big-repo local
//!     fr-probe --repo /data/workspaces/big-repo root@my-server
//!
//! 连服务器时可以带登录方式（和客户端「添加服务器」里填的一样），用来不开界面检查登录这一段：
//!     --key <私钥文件>           用这把私钥
//!     --password                 用密码登录，不试密钥
//!     --secret-env <环境变量名>   密码（带 --password 时）或私钥口令放在这个环境变量里，ssh 问的时候自动回答；
//!                                没给、或者答过一次 ssh 还问，就打出它问的话并当作取消
//!     --connect-only             连上就退出，不测速
//!
//! 连服务器时测终端（开在 kuaid 里，名字用 `--repo` 给的路径，和客户端里那个仓库的终端是同一个）：
//!     --term <命令>              开（或者接上）终端，敲这条命令，把之后几秒的输出打出来，然后断开；
//!                                命令给空的只看不敲。终端里的程序接着跑，下次还能接上
//!     --secs <秒数>              `--term` 看多久，默认 3
//!     --term-list                列出 kuaid 里的终端会话
//!     --term-close               结束这个仓库的终端

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use fr_channel::{Channel, ConnectOptions, Connection, StreamItem, TermItem};
use futures::StreamExt as _;
use futures::executor::block_on;

fn main() -> Result<()> {
    // 被 ssh 当作 askpass 启动时：去问已经开着的那个 fr-probe，打出回答就退出
    if let Some(code) = fr_core::askpass::run_helper() {
        std::process::exit(code);
    }
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut flag = |name: &str| {
        let found = args.iter().any(|a| a == name);
        args.retain(|a| a != name);
        found
    };
    let (password_auth, connect_only) = (flag("--password"), flag("--connect-only"));
    let (term_list, term_close) = (flag("--term-list"), flag("--term-close"));
    let value = |name: &str| args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone());
    let options = ConnectOptions {
        forward_agent: false,
        identity_file: value("--key"),
        password_auth,
        secret: value("--secret-env").and_then(|name| std::env::var(name).ok()),
    };
    let ask = password_auth || options.secret.is_some();
    let usage = "用法：fr-probe --repo <仓库路径> (local | <ssh 目标> | --kuaid <kuaid 程序>)";
    let repo = args
        .windows(2)
        .find(|w| w[0] == "--repo")
        .map(|w| w[1].clone())
        .context(usage)?;
    let watch_secs: Option<u64> = args
        .windows(2)
        .find(|w| w[0] == "--watch")
        .and_then(|w| w[1].parse().ok());
    let term_command = value("--term");
    let term_secs: u64 = value("--secs").and_then(|secs| secs.parse().ok()).unwrap_or(3);
    let is_option = |a: &str| {
        matches!(
            a,
            "--repo" | "--watch" | "--key" | "--secret-env" | "--term" | "--secs"
        )
    };
    let rest: Vec<&String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| !is_option(a) && (*i == 0 || !is_option(&args[i - 1])))
        .map(|(_, a)| a)
        .collect();

    let channel = match rest.as_slice() {
        [target] if *target == "local" => Channel::Local,
        [flag, program] if *flag == "--kuaid" => {
            let mut cmd = Command::new(program.as_str());
            cmd.arg("--stdio");
            let conn = Connection::spawn_stdio(cmd)?;
            println!(
                "启动本机 kuaid 并握手：{} ms · {:?}",
                conn.connect_time.as_millis(),
                conn.hello
            );
            Channel::Remote(Arc::new(conn))
        }
        [destination] => {
            if ask {
                // 没有界面：存着的答不了的提问，打出来并当作取消
                fr_channel::init_prompts(|prompt| {
                    println!("ssh 问了没有存着答案的：{}", prompt.trim());
                    None
                })?;
            }
            let conn = Connection::connect(destination, &options)?;
            println!(
                "连接 {destination}：{} ms（{}）· {} · {} {} · 主目录 {}",
                conn.connect_time.as_millis(),
                if conn.installed {
                    "上传了 kuaid"
                } else {
                    "kuaid 已装好"
                },
                conn.hello.git,
                conn.hello.os,
                conn.hello.arch,
                conn.hello.home,
            );
            Channel::Remote(Arc::new(conn))
        }
        _ => anyhow::bail!(usage),
    };

    if term_list || term_close || term_command.is_some() {
        let Channel::Remote(conn) = &channel else {
            anyhow::bail!("终端只有连服务器时能测");
        };
        println!(
            "kuaid {} {}{}",
            conn.hello.version,
            conn.hello.build,
            if conn.hello.daemon { "，常驻" } else { "，不常驻" }
        );
        if let Some(command) = &term_command {
            terminal(conn, &repo, command, Duration::from_secs(term_secs))?;
        }
        if term_close {
            let terms = block_on(conn.term_list())?;
            match terms.iter().find(|term| term.key == repo) {
                Some(term) => {
                    block_on(conn.term_close(&term.term))?;
                    println!("结束了终端 {}", term.term);
                }
                None => println!("这个仓库没有终端"),
            }
        }
        if term_list {
            let terms = block_on(conn.term_list())?;
            println!("终端会话 {} 个", terms.len());
            for term in terms {
                println!(
                    "  {}  {}  {}×{}  {}  接着 {} 个客户端  开了 {} 秒  {}",
                    term.term,
                    term.key,
                    term.cols,
                    term.rows,
                    if term.alive { "在跑" } else { "已结束" },
                    term.attached,
                    term.age,
                    term.title.unwrap_or_default(),
                );
            }
        }
        return Ok(());
    }

    match watch_secs {
        _ if connect_only => Ok(()),
        Some(secs) => watch(&channel, &repo, Duration::from_secs(secs)),
        None => block_on(probe(&channel, &repo)),
    }
}

/// 开（或者接上）这个仓库的终端，敲一条命令，把 `duration` 这么久里收到的输出原样打出来，然后断开。
fn terminal(conn: &Connection, repo: &str, command: &str, duration: Duration) -> Result<()> {
    use std::io::Write as _;
    let mut stream = conn.term_open(repo, repo, 100, 30);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        block_on(async {
            while let Some(item) = stream.next().await {
                if tx.send(item).is_err() {
                    return;
                }
            }
        });
    });
    let started = Instant::now();
    let mut bytes = 0usize;
    while let Some(left) = duration.checked_sub(started.elapsed()) {
        match rx.recv_timeout(left) {
            Ok(Ok(TermItem::Opened { term, created })) => {
                println!(
                    "终端 {term}：{}",
                    if created { "新开的" } else { "接上了原来的" }
                );
                if !command.is_empty() {
                    conn.term_input(&term, format!("{command}\r").into_bytes());
                }
            }
            Ok(Ok(TermItem::Output(output))) => {
                bytes += output.len();
                let mut out = std::io::stdout().lock();
                out.write_all(&output)?;
                out.flush()?;
            }
            Ok(Ok(TermItem::Exited(code))) => {
                println!("\n终端里的程序结束了，退出码 {code:?}");
                return Ok(());
            }
            Ok(Err(err)) => return Err(err),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            Err(_) => anyhow::bail!("终端的流意外结束"),
        }
    }
    println!("\n（收到 {bytes} 字节，断开；终端里的程序接着跑）");
    Ok(())
}

/// 监听仓库一段时间，每收到一次变化通知打一行。
fn watch(channel: &Channel, repo: &str, duration: Duration) -> Result<()> {
    let started = Instant::now();
    let mut changes = channel.watch(repo);
    println!("监听 {repo} {} 秒…", duration.as_secs());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        block_on(async {
            while changes.next().await.is_some() {
                if tx.send(()).is_err() {
                    return;
                }
            }
        });
    });
    let mut count = 0;
    while let Some(left) = duration.checked_sub(started.elapsed()) {
        match rx.recv_timeout(left) {
            Ok(()) => {
                count += 1;
                println!(
                    "{:>6.1} s  第 {count} 次变化通知",
                    started.elapsed().as_secs_f64()
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            Err(_) => anyhow::bail!("监听中断（目录不存在、装不上监听或者连接断了）"),
        }
    }
    println!("共 {count} 次变化通知");
    Ok(())
}

async fn probe(channel: &Channel, repo: &str) -> Result<()> {
    if let Channel::Remote(conn) = channel {
        let mut samples = Vec::new();
        for _ in 0..100 {
            let t = Instant::now();
            conn.ping().await?;
            samples.push(t.elapsed());
        }
        samples.sort();
        println!(
            "ping 往返 {} 次：p50 {:.2} ms，p95 {:.2} ms，最慢 {:.2} ms",
            samples.len(),
            ms(samples[samples.len() / 2]),
            ms(samples[samples.len() * 95 / 100]),
            ms(samples[samples.len() - 1]),
        );
    }

    // 一次收齐
    let t = Instant::now();
    let out = channel.git(repo, fr_core::log::log_args()).await?;
    let total = t.elapsed();
    let git_time = out.elapsed;
    let raw = out.ok()?;
    let t = Instant::now();
    let commits = fr_core::log::parse(&raw);
    let parse = t.elapsed();
    let t = Instant::now();
    let graph = fr_core::graph::Graph::build(&commits);
    let layout = t.elapsed();
    println!(
        "log：{} 个提交，{:.1} MB，请求到收完 {} ms（其中 git {} ms），解析 {} ms，布局 {} ms，最多 {} 条泳道",
        commits.len(),
        raw.len() as f64 / 1e6,
        total.as_millis(),
        git_time.as_millis(),
        parse.as_millis(),
        layout.as_millis(),
        graph.max_lanes,
    );

    // 流式：看第一块多快到
    let t = Instant::now();
    let mut stream = channel.git_stream(repo, fr_core::log::log_args());
    let (mut first, mut bytes, mut chunks) = (None, 0usize, 0usize);
    while let Some(item) = stream.next().await {
        match item? {
            StreamItem::Chunk(chunk) => {
                first.get_or_insert(t.elapsed());
                bytes += chunk.len();
                chunks += 1;
            }
            StreamItem::Progress(_) => {}
            StreamItem::Done(done) => {
                println!(
                    "流式 log：第一块 {} ms 到，{chunks} 块共 {:.1} MB，全部收完 {} ms，退出码 {:?}",
                    first.map_or(0, |d| d.as_millis()),
                    bytes as f64 / 1e6,
                    t.elapsed().as_millis(),
                    done.code,
                );
                break;
            }
        }
    }

    // 取消：发出请求后马上丢弃，连接应当照常能用
    let t = Instant::now();
    drop(channel.git(repo, fr_core::log::log_args()));
    drop(channel.git_stream(repo, fr_core::log::log_args()));
    let again = channel
        .git(repo, vec!["rev-parse".into(), "HEAD".into()])
        .await?
        .ok()?;
    println!(
        "取消两个请求后再查 HEAD：{} ms，{}",
        t.elapsed().as_millis(),
        String::from_utf8_lossy(&again).trim()
    );

    for needle in ["大文件改动", "Merge branch 'feature"] {
        if let Some(c) = commits.iter().find(|c| c.subject.contains(needle)) {
            let t = Instant::now();
            let out = channel.git(repo, fr_core::show::args(&c.id)).await?;
            let git_time = out.elapsed;
            let raw = out.ok()?;
            let (details, patch) = fr_core::show::parse(&raw);
            println!(
                "show「{}」：补丁 {:.1} KB，请求到收完 {} ms（其中 git {} ms），作者 {}，父提交 {} 个",
                c.subject,
                patch.len() as f64 / 1024.,
                t.elapsed().as_millis(),
                git_time.as_millis(),
                details.author,
                details.parents.len(),
            );
        }
    }

    let t = Instant::now();
    let listing = channel.list_dir(repo).await?;
    println!(
        "list_dir {}：{} 个子目录，本身是仓库：{}，{} ms",
        listing.path,
        listing.entries.len(),
        listing.repo,
        t.elapsed().as_millis()
    );
    Ok(())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.
}
