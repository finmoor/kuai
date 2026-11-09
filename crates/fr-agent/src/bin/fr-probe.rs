//! fr-probe：启动 agent（本机进程或经 ssh），测往返延迟和读取历史、diff 的耗时。M0 测试用。
//!
//! 用法：
//!     fr-probe --repo <agent 那一侧的仓库路径> -- <启动 agent 的命令…>
//! 例如：
//!     fr-probe --repo .testdata/big-repo -- target/debug/fr-agent --stdio
//!     fr-probe --repo /home/me/big-repo -- ssh -o BatchMode=yes dev170 ~/.fork-remote/fr-agent --stdio

use std::io::{BufReader, BufWriter};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use fr_core::protocol::Client;
use serde_json::json;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sep = args
        .iter()
        .position(|a| a == "--")
        .context("用法：fr-probe --repo <仓库路径> -- <启动 agent 的命令…>")?;
    let repo = args[..sep]
        .windows(2)
        .find(|w| w[0] == "--repo")
        .map(|w| w[1].clone())
        .context("缺少 --repo")?;
    let command = &args[sep + 1..];
    anyhow::ensure!(!command.is_empty(), "缺少启动 agent 的命令");

    let started = Instant::now();
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("无法启动 {}", command.join(" ")))?;
    let reader = BufReader::with_capacity(1 << 20, child.stdout.take().unwrap());
    let writer = BufWriter::new(child.stdin.take().unwrap());
    let mut client = Client::new(reader, writer);

    let (hello, _) = client.call("hello", json!({}))?;
    println!("启动并握手：{} ms · {hello}", started.elapsed().as_millis());

    let mut samples = Vec::new();
    for _ in 0..100 {
        let t = Instant::now();
        client.call("ping", json!({}))?;
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

    let t = Instant::now();
    let (meta, raw) = client.call("log", json!({ "repo": repo }))?;
    let total = t.elapsed();
    let t = Instant::now();
    let commits = fr_core::log::parse(&raw);
    let parse = t.elapsed();
    let t = Instant::now();
    let graph = fr_core::graph::Graph::build(&commits);
    let layout = t.elapsed();
    println!(
        "log：{} 个提交，{:.1} MB，请求到收完 {} ms（其中 agent 端 git {} ms），本地解析 {} ms，布局 {} ms，最多 {} 条泳道",
        commits.len(),
        raw.len() as f64 / 1e6,
        total.as_millis(),
        meta["git_ms"],
        parse.as_millis(),
        layout.as_millis(),
        graph.max_lanes,
    );

    for needle in ["大文件改动", "Merge branch 'feature"] {
        if let Some(c) = commits.iter().find(|c| c.subject.contains(needle)) {
            let t = Instant::now();
            let (meta, patch) = client.call("show", json!({ "repo": repo, "rev": &*c.id }))?;
            println!(
                "show「{}」：{:.1} KB，请求到收完 {} ms（其中 agent 端 git {} ms）",
                c.subject,
                patch.len() as f64 / 1024.,
                t.elapsed().as_millis(),
                meta["git_ms"],
            );
        }
    }

    // 关掉 agent 的 stdin，它应当自己退出
    drop(client);
    let status = child.wait()?;
    println!(
        "agent 退出：{status}，总用时 {:.1} s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.
}
