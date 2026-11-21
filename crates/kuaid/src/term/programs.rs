//! 终端里除了 shell 还有什么程序在跑（`term_programs`，协议 14 起也随终端列表推）。客户端结束、重启
//! 终端前用它决定要不要先问：只看有没有输出不够，Claude Code 这类程序在后台干活时屏幕上可以很久不动。
//! 终端页的侧栏按它画终端的图标。
//!
//! - Linux：伪终端的前台那一组进程不是 shell 自己，就是有程序在跑，名字取这一组的组长。后台的作业
//!   不算：zsh 的提示符插件（gitstatusd 这类）一直挂在后台，算上的话 shell 闲着也像在跑东西。
//! - Windows：没有前台进程组，看 shell 起的子进程。

/// 前台那一组进程不是 shell 自己时，返回它的名字；shell 在等输入时是空的。查不到名字（/proc 挂了
/// hidepid 看不到别的用户的进程、组长刚退出）时是一个空串：客户端照样当作有程序在跑，结束前先问，
/// 只是确认框里写不出名字。
#[cfg(unix)]
pub fn foreground(fd: std::os::fd::RawFd, shell: i32) -> Vec<String> {
    let mut group: libc::pid_t = 0;
    // SAFETY: 只读伪终端的前台进程组号，group 是有效的指针
    let got = unsafe { libc::ioctl(fd, libc::TIOCGPGRP as _, &mut group) };
    if got != 0 || group <= 1 || group == shell {
        return Vec::new();
    }
    vec![group_name(group).unwrap_or_default()]
}

/// 一组进程的名字：组长一般就是命令本身；组长先退了就找组里别的。组里一个进程都没有了是 None。
#[cfg(target_os = "linux")]
fn group_name(group: i32) -> Option<String> {
    if let Some(name) = comm(group) {
        return Some(name);
    }
    std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .find_map(|entry| {
            let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
            (pgrp_of(pid)? == group).then(|| comm(pid)).flatten()
        })
}

/// 读不了 /proc 的系统上不知道名字，只知道有程序在跑。
#[cfg(all(unix, not(target_os = "linux")))]
fn group_name(_group: i32) -> Option<String> {
    Some(String::new())
}

/// 进程名。不是 UTF-8 的照样给（换掉认不出的字节），不当成查不到。
#[cfg(target_os = "linux")]
fn comm(pid: i32) -> Option<String> {
    let bytes = std::fs::read(format!("/proc/{pid}/comm")).ok()?;
    Some(
        String::from_utf8_lossy(&bytes)
            .trim_end_matches('\n')
            .to_string(),
    )
}

/// `/proc/<pid>/stat` 里的进程组号。名字在括号里、可能带空格和括号，从最后一个 `)` 往后数：
/// 状态、父进程号、进程组号。
#[cfg(target_os = "linux")]
fn pgrp_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_pgrp(&stat)
}

#[cfg(any(target_os = "linux", test))]
fn parse_pgrp(stat: &str) -> Option<i32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(2)?.parse().ok()
}

/// 一次看好几个终端时共用的进程表：Windows 上拍一次快照，各个终端都从里面找（终端列表每秒看一次，
/// 不能一个终端拍一次）；别的系统用不着。
#[derive(Default)]
pub struct Procs {
    #[cfg(windows)]
    all: Vec<Proc>,
}

impl Procs {
    pub fn take() -> Procs {
        Procs {
            #[cfg(windows)]
            all: win::snapshot(),
        }
    }
}

/// shell 起的子进程的名字（见 [`pick`]）。拍不了进程快照时不知道里面有什么：给一个空串，客户端
/// 当作有程序在跑，结束前先问。
#[cfg(windows)]
pub fn children(procs: &Procs, shell: u32) -> Vec<String> {
    if procs.all.is_empty() {
        return vec![String::new()];
    }
    pick(&procs.all, shell, win::created, win::image)
}

/// 列进程时的一项。
#[cfg(any(windows, test))]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// 程序文件名，比如 `pwsh.exe`
    pub name: String,
}

/// 从进程列表里挑出 shell 起的子进程，返回名字（小写、去掉 `.exe`，同名的只留一个）。
/// `created` 查进程什么时候启动的，`image` 查程序的完整路径，查不到都是 None。
///
/// - Git Bash 的 `bin\bash.exe` 只是个启动器，真的 shell 是它起的 `usr\bin\bash.exe`：开出来的那个
///   进程有个同名、路径不同的子进程时，看那个子进程的。同一个程序（在 pwsh 里又敲了 `pwsh`）算
///   在跑的程序。
/// - `conhost.exe` 不算：不在伪控制台里的控制台程序各带一个，不是用户起的程序。
/// - 进程号会被重用：比父进程早启动的不是它的子进程（父进程号是之前用过这个号的进程留下的）。
#[cfg(any(windows, test))]
fn pick(
    all: &[Proc],
    shell: u32,
    created: impl Fn(u32) -> Option<u64>,
    image: impl Fn(u32) -> Option<String>,
) -> Vec<String> {
    let kids = |parent: u32| -> Vec<&Proc> {
        let born = created(parent);
        all.iter()
            .filter(|p| p.ppid == parent && p.pid != parent)
            .filter(|p| match (born, created(p.pid)) {
                (Some(born), Some(at)) => at >= born,
                // 打不开的（别的用户的、刚退出的）认不出是不是，算上：多问一句不要紧
                _ => true,
            })
            .collect()
    };
    let Some(top) = all.iter().find(|p| p.pid == shell) else {
        return Vec::new();
    };
    let mut shell = top.pid;
    let launcher = kids(shell).into_iter().find(|kid| {
        kid.name.eq_ignore_ascii_case(&top.name)
            && matches!(
                (image(top.pid), image(kid.pid)),
                (Some(outer), Some(inner)) if !outer.eq_ignore_ascii_case(&inner)
            )
    });
    if let Some(inner) = launcher {
        shell = inner.pid;
    }
    let mut names: Vec<String> = Vec::new();
    for kid in kids(shell) {
        let lower = kid.name.to_lowercase();
        if lower == "conhost.exe" {
            continue;
        }
        let short = lower.strip_suffix(".exe").unwrap_or(&lower).to_string();
        if !names.contains(&short) {
            names.push(short);
        }
    }
    names
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };

    use super::Proc;

    /// 现在所有的进程。拍不了快照是空的。
    pub fn snapshot() -> Vec<Proc> {
        let mut all = Vec::new();
        // SAFETY: 快照句柄用完关掉；entry 按要求先填好大小
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return all;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            let mut more = Process32FirstW(snap, &mut entry) != 0;
            while more {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                all.push(Proc {
                    pid: entry.th32ProcessID,
                    ppid: entry.th32ParentProcessID,
                    name: String::from_utf16_lossy(&entry.szExeFile[..len]),
                });
                more = Process32NextW(snap, &mut entry) != 0;
            }
            CloseHandle(snap);
        }
        all
    }

    /// 进程什么时候启动的（FILETIME 的值）。打不开是 None。
    pub fn created(pid: u32) -> Option<u64> {
        // SAFETY: 句柄用完关掉；四个时间都是有效的指针
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return None;
            }
            let zero = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
            let ok = GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user);
            CloseHandle(process);
            (ok != 0).then(|| {
                (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)
            })
        }
    }

    /// 进程的程序文件的完整路径。打不开是 None。
    pub fn image(pid: u32) -> Option<String> {
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: 句柄用完关掉；缓冲区和它的长度如实给了
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return None;
            }
            let ok =
                QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
            CloseHandle(process);
            (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 进程组号从最后一个括号往后数() {
        assert_eq!(parse_pgrp("123 (bash) S 1 123 123 34816"), Some(123));
        // 名字里带空格和括号
        assert_eq!(parse_pgrp("77 (my (odd) prog) R 5 70 5 0"), Some(70));
        assert_eq!(parse_pgrp("garbage"), None);
    }

    /// 假的进程列表：(进程号, 父进程号, 名字, 启动时间, 路径)
    fn pick_from(shell: u32, procs: &[(u32, u32, &str, u64, &str)]) -> Vec<String> {
        let all: Vec<Proc> = procs
            .iter()
            .map(|&(pid, ppid, name, _, _)| Proc {
                pid,
                ppid,
                name: name.into(),
            })
            .collect();
        let find = |pid: u32| procs.iter().find(|p| p.0 == pid);
        pick(
            &all,
            shell,
            |pid| find(pid).map(|p| p.3),
            |pid| find(pid).map(|p| p.4.to_string()),
        )
    }

    #[test]
    fn 挑出_shell_起的程序() {
        const PWSH: &str = r"C:\Program Files\PowerShell\7\pwsh.exe";
        // 闲着：只有不在伪控制台里的程序带的 conhost
        assert!(
            pick_from(
                10,
                &[
                    (10, 1, "pwsh.exe", 100, PWSH),
                    (11, 10, "conhost.exe", 101, "c")
                ]
            )
            .is_empty()
        );
        // 名字小写、去掉 .exe，同名的只留一个；孙进程不列
        let names = pick_from(
            10,
            &[
                (10, 1, "pwsh.exe", 100, PWSH),
                (11, 10, "Claude.exe", 101, "c"),
                (12, 11, "node.exe", 102, "n"),
                (13, 10, "claude.exe", 103, "c"),
                (14, 10, "PING.EXE", 104, "p"),
            ],
        );
        assert_eq!(names, ["claude", "ping"]);
        // 进程号被重用：比 shell 早启动的不是它起的
        assert!(
            pick_from(
                10,
                &[(10, 1, "pwsh.exe", 100, PWSH), (11, 10, "old.exe", 50, "o")]
            )
            .is_empty()
        );
        // 在 pwsh 里又开了 pwsh：同一个程序，算在跑的程序
        let nested = pick_from(
            10,
            &[
                (10, 1, "pwsh.exe", 100, PWSH),
                (11, 10, "pwsh.exe", 101, PWSH),
            ],
        );
        assert_eq!(nested, ["pwsh"]);
        // 没有这个进程
        assert!(pick_from(99, &[(10, 1, "pwsh.exe", 100, PWSH)]).is_empty());
    }

    #[test]
    fn git_bash_的启动器往里找一层() {
        const OUTER: &str = r"C:\Program Files\Git\bin\bash.exe";
        const INNER: &str = r"C:\Program Files\Git\usr\bin\bash.exe";
        let launcher = [
            (10, 1, "bash.exe", 100, OUTER),
            (11, 10, "bash.exe", 101, INNER),
        ];
        assert!(pick_from(10, &launcher).is_empty(), "真的 shell 闲着");
        let mut busy = launcher.to_vec();
        busy.push((12, 11, "claude.exe", 102, "c"));
        assert_eq!(pick_from(10, &busy), ["claude"]);
        // 真的 shell 里又敲了 bash：和它同一个程序，算在跑的程序
        busy.push((13, 11, "bash.exe", 103, INNER));
        assert_eq!(pick_from(10, &busy), ["claude", "bash"]);
    }

    #[cfg(windows)]
    #[test]
    fn 真的进程_列得出子进程() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        // cmd 里再起一个 cmd（同一个程序，算在跑的程序），它再起 ping
        let mut outer = Command::new("cmd.exe")
            .args(["/d", "/c", "cmd.exe /d /c ping.exe -n 30 127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let names = loop {
            let names = children(&Procs::take(), outer.id());
            if !names.is_empty() || Instant::now() > deadline {
                break names;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        // 结束整棵树，免得 ping 留下来
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &outer.id().to_string()])
            .output();
        let _ = outer.wait();
        assert_eq!(names, ["cmd"]);
        // 不存在的进程
        assert!(children(&Procs::take(), u32::MAX - 3).is_empty());
    }
}
