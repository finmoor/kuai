//! kuaid：Kuai 放在服务器上的常驻程序。替客户端在服务器上执行 git、托管终端会话。
//!
//! 不开网络端口、不要 root。几种运行方式：
//!
//! - `kuaid --connect`：客户端每次经 ssh 启动的转接程序。把 ssh 的标准输入输出接到常驻进程的
//!   套接字上；常驻进程没在跑就先把它拉起来，比自己旧就让它原地换成自己这一版（见 `bridge`）。
//! - `kuaid --daemon`：常驻进程，由转接程序拉起来，听 `~/.kuai/run/kuaid.sock`。
//!   客户端都断开、终端会话也都结束了，过一阵自己退出（见 `daemon`）。
//! - `kuaid --stdio`：不常驻，只服务标准输入输出这一个客户端，读到头就退出。测试协议用，
//!   Windows 上也能跑（开不了终端）。
//! - `kuaid --status`、`kuaid --stop`：在服务器上看常驻进程的情况、让它结束。
//!
//! kuaid 尽量少做事：`git` 方法执行客户端给的任意 git 参数，原样回传输出，解析和布局都在
//! 客户端做。协议见 `fr_core::protocol`。

#[cfg(unix)]
mod bridge;
mod conn;
#[cfg(unix)]
mod daemon;
mod files;
mod out;
mod term;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "用法：kuaid --connect | --daemon | --stdio | --status | --stop | --version";

fn main() {
    // 被 git 或 ssh 当作 askpass 启动时：去问主进程，打出回答就退出
    if let Some(code) = fr_core::askpass::run_helper() {
        std::process::exit(code);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result: anyhow::Result<()> = match args.as_slice() {
        ["--version"] => {
            println!("kuaid {VERSION}");
            Ok(())
        }
        ["--stdio"] => {
            stdio();
            Ok(())
        }
        #[cfg(unix)]
        ["--connect"] => bridge::connect(),
        #[cfg(unix)]
        ["--daemon"] => daemon::run(None),
        #[cfg(unix)]
        ["--daemon", "--resume", state] => daemon::run(Some(state)),
        #[cfg(unix)]
        ["--status"] => bridge::status(),
        #[cfg(unix)]
        ["--stop"] => bridge::stop(),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(err) = result {
        eprintln!("kuaid: {err:#}");
        std::process::exit(1);
    }
}

/// 只服务标准输入输出这一个客户端。
fn stdio() {
    #[cfg(unix)]
    let terms = term::Registry::new(term::pty::spawner());
    #[cfg(not(unix))]
    let terms = term::Registry::unsupported();
    let shared = conn::Shared::new(terms, None, build());
    // 以前传上来、已经过期的文件
    files::sweep(&shared.drop_dir);
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        shared.start_askpass(dir);
    }
    conn::serve(&shared, std::io::stdin().lock(), std::io::stdout());
    // 不常驻：客户端走了，终端里的程序也结束
    shared.terms.close_all();
}

/// 自己装在哪个目录里：`~/.kuai/<版本>-<哈希>/kuaid` 的那一段目录名。不是装出来的就是空的。
pub fn build() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            let name = exe.parent()?.file_name()?.to_str()?.to_string();
            name.starts_with(|c: char| c.is_ascii_digit()).then_some(name)
        })
        .unwrap_or_default()
}
