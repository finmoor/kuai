//! 不开界面把一个目录映射到本机，测 `fr-mount` 用。
//!
//! 用法：
//!     mount local <目录> <盘符或文件夹> [选项]             本机目录，进程内读写（只测挂载这一层）
//!     mount --kuaid <kuaid 程序> <目录> <盘符或文件夹> [选项]   经本机的 kuaid 进程（协议那一段也走到）
//!     mount <ssh 目标> <目录> <盘符或文件夹> [选项]        服务器上的目录
//! `--kuaid` 给的程序会带一个 `--stdio` 参数启动，也可以是一个 .cmd：在里面 ssh 到服务器，
//! 跑放在临时目录里的 kuaid（`HOME` 也指到临时目录），测服务器又不装进它的 `~/.kuai`。
//! 选项：
//!     --read-only          只读
//!     --windows            那边是 Windows（不给的话：local 按本机算，--kuaid、ssh 目标按连上后报的算）
//!     --key <私钥文件>     登录服务器用这把私钥
//!     --stop-file <文件>   这个文件一出现就取消映射并退出
//!     --drop-file <文件>   这个文件在的时候当作断线（盘还在，读写都出错），删掉就算重连上了
//! 每秒看一次那两个文件；写入、删除落不到服务器上时打一行「没成：」加原因。退出时打出发了多少请求。

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use fr_channel::{Channel, ConnectOptions, Connection};

fn main() -> Result<()> {
    if let Some(code) = fr_core::askpass::run_helper() {
        std::process::exit(code);
    }
    if let Some(code) = fr_channel::run_ssh_hop() {
        std::process::exit(code);
    }
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut flag = |name: &str| {
        let found = args.iter().any(|arg| arg == name);
        args.retain(|arg| arg != name);
        found
    };
    let (read_only, windows) = (flag("--read-only"), flag("--windows"));
    let mut value = |name: &str| {
        let at = args.iter().position(|arg| arg == name)?;
        let value = args.get(at + 1).cloned();
        args.drain(at..(at + 2).min(args.len()));
        value
    };
    let key = value("--key");
    let stop_file = value("--stop-file");
    let drop_file = value("--drop-file");
    let kuaid = value("--kuaid");

    let (channel, windows, root, place) = match (kuaid, args.as_slice()) {
        (Some(program), [root, place]) => {
            let mut command = Command::new(program);
            command.arg("--stdio");
            let conn = Connection::spawn_stdio(command)?;
            println!(
                "kuaid：{} {} · 协议 {}",
                conn.hello.os, conn.hello.arch, conn.hello.protocol
            );
            let remote_windows = windows || conn.hello.os == "windows";
            (Channel::Remote(Arc::new(conn)), remote_windows, root, place)
        }
        (None, [target, root, place]) if target == "local" => {
            (Channel::Local, cfg!(windows), root, place)
        }
        (None, [target, root, place]) => {
            let options = ConnectOptions {
                identity_file: key,
                // 没有界面可以核对指纹：没见过的主机直接记下
                accept_new_host_keys: true,
                ..ConnectOptions::default()
            };
            let conn = Connection::connect(target, &options)?;
            let remote_windows = windows || conn.hello.os == "windows";
            println!(
                "连接 {target}：{} ms · {} {} · 协议 {}",
                conn.connect_time.as_millis(),
                conn.hello.os,
                conn.hello.arch,
                conn.hello.protocol
            );
            (Channel::Remote(Arc::new(conn)), remote_windows, root, place)
        }
        _ => bail!(
            "用法：mount (local | --kuaid <kuaid 程序> | <ssh 目标>) <目录> <盘符或文件夹> [选项]"
        ),
    };

    println!(
        "WinFsp：{}",
        if fr_mount::installed() {
            "已安装"
        } else {
            "没有安装"
        }
    );
    let options = fr_mount::Options {
        root: root.clone(),
        place: place.clone(),
        read_only,
        windows,
        label: "Kuai".into(),
        share: format!("probe-{}", std::process::id()),
    };
    let mounted = fr_mount::mount(channel.clone(), options, |message| {
        println!("没成：{message}");
    })?;
    println!(
        "已把 {root} 映射到 {}{}",
        mounted.place(),
        if read_only { "（只读）" } else { "" }
    );

    let exists =
        |file: &Option<String>| file.as_deref().is_some_and(|file| Path::new(file).exists());
    let mut dropped = false;
    while !exists(&stop_file) {
        let drop_now = exists(&drop_file);
        if drop_now != dropped {
            dropped = drop_now;
            mounted.set_channel((!dropped).then(|| channel.clone()));
            println!("{}", if dropped { "断线" } else { "重连" });
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let stats = mounted.stats();
    println!(
        "取消映射。还开着 {} 个文件；发了 {} 个请求，读回 {:.1} MB，写回 {:.1} MB",
        mounted.open_files(),
        stats.calls,
        stats.down as f64 / 1048576.,
        stats.up as f64 / 1048576.,
    );
    drop(mounted);
    Ok(())
}
