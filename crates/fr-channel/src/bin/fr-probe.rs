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
//!     --jump <ssh 目标>           经这台跳板机连接（用 ssh 默认的密钥登录它）；打出登录上了几台
//!     --jump-key <私钥文件>       跳板机用这把私钥
//!     --jump-password            跳板机用密码登录
//!     --jump-secret-env <环境变量名>  跳板机的密码或私钥口令放在这个环境变量里
//!     --jump2 <ssh 目标>          第二台跳板机：经第一台连它，再经它连目标
//!     --test-login <ssh 目标>     只做界面上「测试连接」做的事：登录上去问一句是什么系统，打出来就退出。
//!                                不装、不启动 kuaid；不用给 `--repo`
//!
//! 连服务器时测终端（开在 kuaid 里，名字用 `--repo` 给的路径，和客户端里那个仓库的终端是同一个）：
//!     --term <命令>              开（或者接上）终端，敲这条命令，把之后几秒的输出打出来，然后断开；
//!                                命令给空的只看不敲。终端里的程序接着跑，下次还能接上
//!     --secs <秒数>              `--term` 看多久，默认 3
//!     --term-keys <字节>         敲完命令 2 秒后再往终端里送这些字节（`\e` 是 ESC，`\r`、`\xHH` 照常），
//!                                用来模拟终端发给程序的按键、滚轮
//!     --term-size <列>x<行>       开（接上）终端用的大小，默认 100x30。接上界面里开着的终端看屏幕时
//!                                给和它一样的（`--term-list` 里有），程序不会因为改了大小重画
//!     --term-list                列出 kuaid 里的终端会话
//!     --term-close               结束这个仓库的终端
//!
//! 连服务器时测传文件（往终端里发图片、文件走的那条路）：
//!     --put <本机文件>           传到服务器的 ~/.kuai/drop/ 下，打出它在服务器上的路径和速度
//!
//! 测仓库里的文件（文件页走的那条路，本机、服务器都能测）：
//!     --fs                       在仓库里建一个 `.kuai-probe-<进程号>` 目录，把新建、保存、冲突、列目录、
//!                                上传、下载、改名、删除走一遍，核对内容，最后删掉这个目录
//!     --fs-size <MB>             `--fs` 里上传、下载的那个文件多大，默认 9（大于一次读写的上限，要分段）
//!
//! 测找子仓库（子仓库总览走的那条路，本机、服务器都能测，只读）：
//!     --repos                    列出仓库目录里面的仓库，和每个的分支、改动数

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use fr_channel::{
    Channel, ConnectOptions, Connection, FsWrite, StreamItem, TermItem, UploadItem, UploadSource,
    UploadStream,
};
use futures::StreamExt as _;
use futures::executor::block_on;

fn main() -> Result<()> {
    // 被 ssh 当作 askpass 启动时：去问已经开着的那个 fr-probe，打出回答就退出
    if let Some(code) = fr_core::askpass::run_helper() {
        std::process::exit(code);
    }
    // 被 ssh 当作 ProxyCommand 启动时：把连接转到跳板机上
    if let Some(code) = fr_channel::run_ssh_hop() {
        std::process::exit(code);
    }
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut flag = |name: &str| {
        let found = args.iter().any(|a| a == name);
        args.retain(|a| a != name);
        found
    };
    let (password_auth, connect_only) = (flag("--password"), flag("--connect-only"));
    let jump_password = flag("--jump-password");
    let (term_list, term_close) = (flag("--term-list"), flag("--term-close"));
    let fs = flag("--fs");
    let repos = flag("--repos");
    let value = |name: &str| args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone());
    let options = ConnectOptions {
        forward_agent: false,
        identity_file: value("--key"),
        password_auth,
        secret: value("--secret-env").and_then(|name| std::env::var(name).ok()),
        jumps: [value("--jump"), value("--jump2")]
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, destination)| fr_channel::Jump {
                name: format!("跳板机{}", index + 1),
                destination,
                identity_file: value("--jump-key"),
                password_auth: jump_password,
                secret: value("--jump-secret-env").and_then(|name| std::env::var(name).ok()),
            })
            .collect(),
        on_stage: Some(Arc::new(|n| println!("已登录 {n} 台"))),
        no_prompt: false,
        // 没有界面可以核对指纹：没见过的主机直接记下
        accept_new_host_keys: true,
        typed: Default::default(),
    };
    if let Some(destination) = value("--test-login") {
        let started = Instant::now();
        let probe = Connection::probe(&destination, &options)?;
        println!(
            "登录成功：{} {}，{}，这一版的 kuaid {}，{}，{} ms",
            probe.os,
            probe.arch,
            if probe.supported {
                "可以安装 kuaid"
            } else {
                "不支持的系统"
            },
            match probe.kuaid {
                Some(true) => "已安装",
                Some(false) => "未安装",
                None => "没查",
            },
            probe.git.as_deref().unwrap_or("没有 git"),
            started.elapsed().as_millis()
        );
        return Ok(());
    }
    let ask = password_auth || options.secret.is_some() || jump_password;
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
    let term_keys = value("--term-keys").map(|text| unescape(&text));
    let term_size = match value("--term-size") {
        Some(text) => text
            .split_once('x')
            .and_then(|(cols, rows)| Some((cols.parse().ok()?, rows.parse().ok()?)))
            .context("--term-size 的写法是 <列>x<行>，例如 120x40")?,
        None => (100, 30),
    };
    let put = value("--put");
    let fs_size: u64 = value("--fs-size")
        .and_then(|mb| mb.parse().ok())
        .unwrap_or(9);
    let term_secs: u64 = value("--secs")
        .and_then(|secs| secs.parse().ok())
        .unwrap_or(3);
    let is_option = |a: &str| {
        matches!(
            a,
            "--repo"
                | "--watch"
                | "--key"
                | "--secret-env"
                | "--term"
                | "--term-keys"
                | "--term-size"
                | "--secs"
                | "--put"
                | "--fs-size"
                | "--jump"
                | "--jump-key"
                | "--jump-secret-env"
                | "--jump2"
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
                fr_channel::init_prompts(|prompt, jump, _retry| {
                    let who = jump.map(|name| format!("（{name}）")).unwrap_or_default();
                    println!("ssh 问了没有存着答案的{who}：{}", prompt.trim());
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

    if fs {
        return block_on(files(&channel, &repo, fs_size << 20));
    }
    if repos {
        return block_on(sub_repos(&channel, &repo));
    }

    if let Some(file) = &put {
        let Channel::Remote(conn) = &channel else {
            anyhow::bail!("传文件只有连服务器时能测");
        };
        return upload(conn, file);
    }

    if term_list || term_close || term_command.is_some() {
        let Channel::Remote(conn) = &channel else {
            anyhow::bail!("终端只有连服务器时能测");
        };
        println!(
            "kuaid {} {}{}",
            conn.hello.version,
            conn.hello.build,
            if conn.hello.daemon {
                "，常驻"
            } else {
                "，不常驻"
            }
        );
        if let Some(command) = &term_command {
            terminal(
                conn,
                &repo,
                command,
                term_keys,
                term_size,
                Duration::from_secs(term_secs),
            )?;
        }
        if term_close {
            let terms = block_on(conn.term_list())?;
            // Windows 上的 kuaid 把名字统一成正斜杠、小写
            let key = match conn.hello.os == "windows" {
                true => fr_core::dir::windows_key(&repo),
                false => repo.clone(),
            };
            match terms.iter().find(|term| term.key == key) {
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
                // 结束终端前确认框里写的就是这个：shell 以外还在跑的程序
                match block_on(conn.term_programs(&term.term)) {
                    Ok(programs) => println!("  {} 里在跑：{programs:?}", term.term),
                    Err(err) => println!("  {} 里在跑：问不出来（{err:#}）", term.term),
                }
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
                // 查「待查看」对不对时看：屏幕上的字多久没变了、多久没干活了、最后一行是什么
                let ago = |ms: Option<u64>| {
                    ms.map_or_else(
                        || "不知道多久".to_string(),
                        |ms| format!("{} 秒", ms / 1000),
                    )
                };
                println!(
                    "      屏幕 {} 没变 · {} 没干活 · 最后一行：{}",
                    ago(term.idle_ms),
                    ago(term.work_ms),
                    term.last,
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

/// `--term-keys` 的写法换成字节：`\e` 是 ESC，`\r`、`\n`、`\t`、`\\`、`\xHH` 照常。
fn unescape(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
            continue;
        }
        match chars.next() {
            Some('e') => out.push(0x1b),
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                out.push(u8::from_str_radix(&hex, 16).unwrap_or(b'?'));
            }
            Some(other) => out.extend_from_slice(other.encode_utf8(&mut [0; 4]).as_bytes()),
            None => out.push(b'\\'),
        }
    }
    out
}

/// 开（或者接上）这个仓库的终端，敲一条命令，把 `duration` 这么久里收到的输出原样打出来，然后断开。
/// 给了 `keys`：敲完命令 2 秒后再送这些字节（等命令里的程序起来）。
fn terminal(
    conn: &Connection,
    repo: &str,
    command: &str,
    mut keys: Option<Vec<u8>>,
    (cols, rows): (u16, u16),
    duration: Duration,
) -> Result<()> {
    use std::io::Write as _;
    const KEYS_AFTER: Duration = Duration::from_secs(2);
    let mut stream = conn.term_open(repo, repo, &[], cols, rows);
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
    // 还原屏幕的每一段收齐时一共收到多少字节，收齐一段打一次用了多久（测线路慢时先看到什么）
    let mut restore_ends: std::collections::VecDeque<usize> = Default::default();
    // 敲完命令的时候和终端名，到点送 `keys`
    let mut typed: Option<(Instant, String)> = None;
    // 命令敲进去以后隔一会儿再单独按回车，和人敲的一样（kuaid 只把单独的回车当发出了一条命令）
    const ENTER_AFTER: Duration = Duration::from_millis(300);
    let mut enter = !command.is_empty();
    while let Some(left) = duration.checked_sub(started.elapsed()) {
        if let Some((at, term)) = &typed
            && enter
            && at.elapsed() >= ENTER_AFTER
        {
            conn.term_input(term, b"\r".to_vec());
            enter = false;
        }
        if let Some((at, term)) = &typed
            && at.elapsed() >= KEYS_AFTER
            && let Some(keys) = keys.take()
        {
            conn.term_input(term, keys);
        }
        match rx.recv_timeout(left.min(Duration::from_millis(100))) {
            Ok(Ok(TermItem::Opened {
                term,
                created,
                restore,
            })) => {
                println!(
                    "终端 {term}：{}",
                    if created {
                        "新开的"
                    } else {
                        "接上了原来的"
                    }
                );
                // 打到标准错误：标准输出是终端的原始输出
                if !restore.is_empty() {
                    eprintln!(
                        "（还原屏幕的输出 {} 段：{restore:?} 字节；接上用了 {} 毫秒）",
                        restore.len(),
                        started.elapsed().as_millis()
                    );
                }
                restore_ends = restore
                    .iter()
                    .scan(0, |sum, len| {
                        *sum += *len as usize;
                        Some(*sum)
                    })
                    .collect();
                if !command.is_empty() {
                    conn.term_input(&term, command.as_bytes().to_vec());
                }
                typed = Some((Instant::now(), term));
            }
            Ok(Ok(TermItem::Output(output))) => {
                bytes += output.len();
                while restore_ends.front().is_some_and(|&end| bytes >= end) {
                    let end = restore_ends.pop_front().unwrap_or_default();
                    eprintln!(
                        "（还原的输出收到 {end} 字节：{} 毫秒）",
                        started.elapsed().as_millis()
                    );
                }
                let mut out = std::io::stdout().lock();
                out.write_all(&output)?;
                out.flush()?;
            }
            // 发出去的消息打到标准错误，不和终端的输出混在一起
            Ok(Ok(TermItem::Marks(marks))) => {
                let texts: Vec<&str> = marks.iter().map(|mark| mark.text.as_str()).collect();
                eprintln!("（记着的消息 {} 条：{texts:?}）", marks.len());
            }
            Ok(Ok(TermItem::Exited(code))) => {
                println!("\n终端里的程序结束了，退出码 {code:?}");
                return Ok(());
            }
            Ok(Ok(TermItem::Lagging)) => {
                anyhow::bail!("输出积压过多，断开了（终端里的程序接着跑）")
            }
            Ok(Err(err)) => return Err(err),
            // 每 100 ms 醒一次看要不要送 `keys`；看够了由循环条件结束
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => anyhow::bail!("终端的流意外结束"),
        }
    }
    println!("\n（收到 {bytes} 字节，断开；终端里的程序接着跑）");
    Ok(())
}

/// 把本机的一个文件传到服务器上，打出它在那边的路径和速度。
fn upload(conn: &Connection, file: &str) -> Result<()> {
    let name = file.rsplit(['/', '\\']).next().unwrap_or(file);
    let started = Instant::now();
    let mut stream = conn.upload(name, UploadSource::File(file.into()));
    let (mut chunks, mut size) = (0, 0);
    block_on(async {
        while let Some(item) = stream.next().await {
            match item? {
                UploadItem::Progress { sent, .. } => {
                    chunks += 1;
                    size = sent;
                }
                UploadItem::Done(path) => {
                    let secs = started.elapsed().as_secs_f64();
                    println!(
                        "传完了：{path}，{:.1} MB，{chunks} 块，{:.0} ms，{:.1} MB/s",
                        size as f64 / 1e6,
                        secs * 1e3,
                        size as f64 / 1e6 / secs.max(1e-6),
                    );
                    return Ok(());
                }
            }
        }
        anyhow::bail!("上传中断")
    })
}

/// 等一次上传或下载结束，返回传了几块、多少字节。
async fn transfer(mut stream: UploadStream) -> Result<(u32, u64)> {
    let (mut chunks, mut size) = (0, 0);
    while let Some(item) = stream.next().await {
        match item? {
            UploadItem::Progress { sent, .. } => {
                chunks += 1;
                size = sent;
            }
            UploadItem::Done(_) => return Ok((chunks, size)),
        }
    }
    anyhow::bail!("传输中断")
}

/// 把文件页用的那些操作在仓库里走一遍，每一步核对结果。
async fn files(channel: &Channel, repo: &str, size: u64) -> Result<()> {
    let dir = format!(".kuai-probe-{}", std::process::id());
    let at = |name: &str| format!("{dir}/{name}");
    let write = |path: String, create: bool, expect| FsWrite {
        repo: repo.to_string(),
        path,
        create,
        expect,
    };
    let started = Instant::now();
    let step = |what: &str, since: Instant| println!("  {what}：{:.1} ms", ms(since.elapsed()));

    // 新建、读回来、按打开时的样子保存、拿旧的样子保存（冲突）
    let now = Instant::now();
    let text = "第一行\r\nsecond line\r\n".as_bytes().to_vec();
    let created = channel
        .fs_write(write(at("说明 1.txt"), true, None), text.clone())
        .await?;
    anyhow::ensure!(
        !created.conflict && created.stat.size == text.len() as u64,
        "新建的结果不对"
    );
    anyhow::ensure!(
        channel
            .fs_write(write(at("说明 1.txt"), true, None), text.clone())
            .await
            .is_err(),
        "已经有的文件又新建成功了"
    );
    step("新建", now);
    let now = Instant::now();
    let (stat, bytes) = channel.fs_open(repo, &at("说明 1.txt"), 1 << 20).await?;
    anyhow::ensure!(bytes == text && stat == created.stat, "读回来的内容不对");
    step("读", now);
    let now = Instant::now();
    let saved = channel
        .fs_write(
            write(at("说明 1.txt"), false, Some(stat.into())),
            b"changed".to_vec(),
        )
        .await?;
    anyhow::ensure!(!saved.conflict && saved.stat.size == 7, "保存的结果不对");
    let stale = channel
        .fs_write(
            write(at("说明 1.txt"), false, Some(stat.into())),
            b"stale!!!".to_vec(),
        )
        .await?;
    anyhow::ensure!(
        stale.conflict && stale.stat.size == 7,
        "别人改过的文件被盖掉了"
    );
    step("保存和冲突", now);

    // 上传一个大文件再下载回来，核对内容
    let local = std::env::temp_dir().join(format!("kuai-probe-{}.bin", std::process::id()));
    let back = local.with_extension("back");
    let content: Vec<u8> = (0..size).map(|i| (i * 31 + i / 251) as u8).collect();
    std::fs::write(&local, &content)?;
    let now = Instant::now();
    let (chunks, sent) =
        transfer(channel.fs_upload(repo, &at("大 文件.bin"), local.clone(), false)).await?;
    anyhow::ensure!(sent == size, "上传的大小不对：{sent}");
    let secs = now.elapsed().as_secs_f64().max(1e-6);
    println!(
        "  上传 {:.1} MB：{chunks} 块，{:.0} ms，{:.1} MB/s",
        size as f64 / 1e6,
        secs * 1e3,
        size as f64 / 1e6 / secs
    );
    anyhow::ensure!(
        transfer(channel.fs_upload(repo, &at("大 文件.bin"), local.clone(), false))
            .await
            .is_err(),
        "已经有的文件被上传盖掉了"
    );
    transfer(channel.fs_upload(repo, &at("大 文件.bin"), local.clone(), true)).await?;
    let now = Instant::now();
    let (chunks, got) =
        transfer(channel.fs_download(repo, &at("大 文件.bin"), back.clone())).await?;
    let secs = now.elapsed().as_secs_f64().max(1e-6);
    println!(
        "  下载 {:.1} MB：{chunks} 块，{:.0} ms，{:.1} MB/s",
        got as f64 / 1e6,
        secs * 1e3,
        got as f64 / 1e6 / secs
    );
    anyhow::ensure!(
        std::fs::read(&back)? == content,
        "下载回来的内容和传上去的不一样"
    );
    let _ = std::fs::remove_file(&local);
    let _ = std::fs::remove_file(&back);
    // 比上限大的不读完
    let (stat, part) = channel.fs_open(repo, &at("大 文件.bin"), 1 << 20).await?;
    anyhow::ensure!(
        stat.size == size && (part.len() as u64) < size.max(1),
        "超过上限的文件被读完了"
    );

    // 建目录、列目录、改名、删除
    let now = Instant::now();
    channel.fs_mkdir(repo, &at("子目录/深一层")).await?;
    channel
        .fs_rename(repo, &at("说明 1.txt"), &at("子目录/改名.txt"))
        .await?;
    let mut listed = channel.fs_list(repo, &dir).await?;
    listed.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<_> = listed
        .iter()
        .map(|entry| (entry.name.as_str(), entry.dir))
        .collect();
    anyhow::ensure!(
        names == [("大 文件.bin", false), ("子目录", true)],
        "列出来的不对：{names:?}"
    );
    let inner = channel.fs_list(repo, &at("子目录")).await?;
    anyhow::ensure!(inner.len() == 2, "子目录里的不对：{inner:?}");
    anyhow::ensure!(
        !channel.fs_stat(repo, &at("说明 1.txt")).await?.exists,
        "改名以后原来的还在"
    );
    step("建目录、改名、列目录", now);
    anyhow::ensure!(
        channel.fs_list(repo, "../").await.is_err(),
        "能列仓库外面的目录"
    );
    let root = channel.fs_list(repo, "").await?;
    anyhow::ensure!(root.iter().all(|entry| entry.name != ".git"), "列出了 .git");
    println!("  仓库根：{} 项", root.len());

    let now = Instant::now();
    channel.fs_remove(repo, &dir).await?;
    anyhow::ensure!(!channel.fs_stat(repo, &dir).await?.exists, "删除以后还在");
    step("删除", now);
    println!("文件操作全部通过，共 {:.0} ms", ms(started.elapsed()));
    Ok(())
}

/// 找仓库目录里面的仓库，再读每个的状态（子仓库总览做的事）。只读。
async fn sub_repos(channel: &Channel, repo: &str) -> Result<()> {
    let started = Instant::now();
    let found = channel.fs_repos(repo).await?;
    println!(
        "fs_repos {repo}：{} 个子仓库{}，{:.0} ms",
        found.repos.len(),
        if found.truncated {
            "（没找全）"
        } else {
            ""
        },
        ms(started.elapsed())
    );
    let started = Instant::now();
    for path in &found.repos {
        let full = format!("{}/{path}", repo.trim_end_matches(['/', '\\']));
        let out = channel.git(&full, fr_core::status::args()).await;
        match out.and_then(|out| out.ok()) {
            Ok(out) => {
                let status = fr_core::status::parse(&out);
                println!(
                    "  {path}：{}，{} 个改动，↑{} ↓{}{}",
                    status.branch.as_deref().unwrap_or("（不在分支上）"),
                    status.entries.len(),
                    status.ahead,
                    status.behind,
                    if status.upstream.is_some() {
                        ""
                    } else {
                        "，无上游"
                    }
                );
            }
            Err(err) => println!("  {path}：读不了：{err:#}"),
        }
    }
    println!("读状态共 {:.0} ms", ms(started.elapsed()));
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
