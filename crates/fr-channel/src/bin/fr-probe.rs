//! fr-probe：测仓库通道的延迟和耗时，也用来检查自动安装 agent。
//!
//! 用法：
//!     fr-probe --repo <仓库路径> local                  本机通道（进程内启动 git）
//!     fr-probe --repo <服务器上的仓库路径> <ssh 目标>      连服务器（需要时自动上传 agent）
//!     fr-probe --repo <仓库路径> --agent <fr-agent 程序>   直接启动 agent 进程，测协议本身
//! 最后加 `--watch <秒数>`：不测速，只监听仓库这么久，每收到一次变化通知打一行。
//! 例如：
//!     fr-probe --repo .testdata/big-repo local
//!     fr-probe --repo /data/workspaces/big-repo root@my-server

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use fr_channel::{Channel, Connection, StreamItem};
use futures::StreamExt as _;
use futures::executor::block_on;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "用法：fr-probe --repo <仓库路径> (local | <ssh 目标> | --agent <fr-agent 程序>)";
    let repo = args
        .windows(2)
        .find(|w| w[0] == "--repo")
        .map(|w| w[1].clone())
        .context(usage)?;
    let watch_secs: Option<u64> = args
        .windows(2)
        .find(|w| w[0] == "--watch")
        .and_then(|w| w[1].parse().ok());
    let is_option = |a: &str| a == "--repo" || a == "--watch";
    let rest: Vec<&String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| !is_option(a) && (*i == 0 || !is_option(&args[i - 1])))
        .map(|(_, a)| a)
        .collect();

    let channel = match rest.as_slice() {
        [target] if *target == "local" => Channel::Local,
        [flag, program] if *flag == "--agent" => {
            let mut cmd = Command::new(program.as_str());
            cmd.arg("--stdio");
            let conn = Connection::spawn_agent(cmd)?;
            println!(
                "启动本机 agent 并握手：{} ms · {:?}",
                conn.connect_time.as_millis(),
                conn.hello
            );
            Channel::Remote(Arc::new(conn))
        }
        [destination] => {
            let conn = Connection::connect(destination, &Default::default())?;
            println!(
                "连接 {destination}：{} ms（{}）· {} · {} {} · 主目录 {}",
                conn.connect_time.as_millis(),
                if conn.installed {
                    "上传了 agent"
                } else {
                    "agent 已装好"
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

    match watch_secs {
        Some(secs) => watch(&channel, &repo, Duration::from_secs(secs)),
        None => block_on(probe(&channel, &repo)),
    }
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
