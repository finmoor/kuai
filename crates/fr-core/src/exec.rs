//! 启动 git 进程、读它的输出，并且能从别的线程取消。kuaid 和客户端的本机通道共用这一套。

use crate::i18n::tr;
use std::io::{self, ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

/// 标准错误最多留这么多，免得失控的输出占满内存
const STDERR_LIMIT: usize = 1 << 20;
/// 读标准输出时每次最多读这么多
pub const CHUNK_SIZE: usize = 64 * 1024;
/// Windows 上取消时，杀掉进程树以后等这么久，让输出读完、最外层回收；到时候还没好，就是有成了
/// 孤儿的子进程还占着输出，结束作业里剩下的进程（见 [`Tree`]）
#[cfg(windows)]
const ORPHAN_GRACE: Duration = Duration::from_secs(2);

/// git 进程结束后的状态。
#[derive(Debug, Clone, Default)]
pub struct Finished {
    /// 退出码；进程被杀掉（比如取消）时为 None
    pub code: Option<i32>,
    pub stderr: String,
    pub elapsed: Duration,
}

/// 启动了的进程，连同它启动的子进程：取消时一起结束。kuaid 替客户端起的 ssh 也用它。
///
/// 只杀最外层不够：`git fetch` 会再启动 `git fetch <远程>`、`git-remote-https`、ssh，
/// Windows 上 PATH 里的 git.exe 还只是个转发程序，真正干活的都是子进程；钩子还可能在后台留下
/// 进程，最外层退出了，它还占着我们在读的输出。Unix 上进程自己一个进程组，取消时给整个组发信号：
/// 最外层回收之前，它的进程号不会给别的进程，组一直认得准。Windows 上启动后马上放进一个作业
/// （job），子进程成了孤儿也还在作业里。
///
/// 等它结束（[`Tree::wait`]）时锁只在每次看一眼时拿着，取消随时拿得到。
pub struct Tree {
    proc: Mutex<Proc>,
    /// 回收了最外层时通知
    ended: Condvar,
    #[cfg(windows)]
    job: Option<std::os::windows::io::OwnedHandle>,
}

struct Proc {
    child: Child,
    /// 等到了最外层结束、回收了它。之后取消什么也不做：占着输出的都已经结束，还在的是有意留在
    /// 后台的；Unix 上进程号也可能已经给了别的进程
    reaped: bool,
}

/// 启动时取走的标准输入输出（没设成管道的是 None）。
pub struct Pipes {
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

impl Tree {
    /// 启动 `cmd`：Unix 上让它自己一个进程组；Windows 上马上放进一个新建的作业，放不进（极少见）
    /// 就只按父子关系杀。
    pub fn spawn(cmd: &mut Command) -> io::Result<(Arc<Tree>, Pipes)> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        // 作业先建好，启动后马上放进去：这时它刚开始运行，几乎不可能已经启动了子进程（真有，
        // 取消时 taskkill 还会按父子关系找）
        #[cfg(windows)]
        let job = job::create();
        let mut child = cmd.spawn()?;
        #[cfg(windows)]
        let job = job.filter(|job| job::assign(job, &child));
        let pipes = Pipes {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
        };
        let tree = Tree {
            proc: Mutex::new(Proc {
                child,
                reaped: false,
            }),
            ended: Condvar::new(),
            #[cfg(windows)]
            job,
        };
        Ok((Arc::new(tree), pipes))
    }

    pub fn killer(self: &Arc<Self>) -> Killer {
        Killer(self.clone())
    }

    /// 等最外层结束，回收它。
    pub fn wait(&self) -> io::Result<ExitStatus> {
        let mut pause = Duration::from_millis(1);
        loop {
            {
                let mut proc = self.proc();
                if let Some(status) = proc.child.try_wait()? {
                    proc.reaped = true;
                    self.ended.notify_all();
                    return Ok(status);
                }
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(50));
        }
    }

    fn proc(&self) -> MutexGuard<'_, Proc> {
        self.proc.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 从别的线程结束进程树（见 [`Tree`]）。已经等到它结束了就什么也不做。不会卡住调用的线程：
/// Windows 上要启动 taskkill、等输出读完，放在别的线程里做。
#[derive(Clone)]
pub struct Killer(Arc<Tree>);

impl Killer {
    pub fn kill(&self) {
        #[cfg(unix)]
        {
            let proc = self.0.proc();
            if !proc.reaped {
                // SAFETY: 只是发信号；最外层还没回收，进程号和进程组都还是我们启动的那个。
                // 用 SIGTERM 而不是 SIGKILL，git 来得及删掉自己的锁文件
                unsafe {
                    libc::kill(-(proc.child.id() as libc::pid_t), libc::SIGTERM);
                }
            }
        }
        #[cfg(windows)]
        {
            let tree = self.0.clone();
            std::thread::spawn(move || tree.kill_tree());
        }
    }
}

#[cfg(windows)]
impl Tree {
    /// 先按父子关系杀（taskkill /T：最外层还在时找得到整棵树），再等一会儿让输出读完、最外层回收；
    /// 到时候还没好，就是有成了孤儿的子进程还占着输出，结束作业里剩下的所有进程。不一上来就结束
    /// 整个作业：子进程打开的、和输出无关的程序（比如登录时打开的浏览器）不跟着结束。
    fn kill_tree(&self) {
        let pid = {
            let proc = self.proc();
            if proc.reaped {
                return;
            }
            proc.child.id()
        };
        // 进程号不会给别的进程：Child 还开着它的句柄
        taskkill(pid);
        let mut proc = self.proc();
        if proc.reaped {
            return;
        }
        let _ = proc.child.kill();
        let Some(job) = &self.job else {
            return;
        };
        let (proc, _) = self
            .ended
            .wait_timeout_while(proc, ORPHAN_GRACE, |proc| !proc.reaped)
            .unwrap_or_else(PoisonError::into_inner);
        if !proc.reaped {
            job::terminate(job);
        }
    }
}

/// Windows：让 taskkill 按父子关系杀掉整棵进程树。
#[cfg(windows)]
fn taskkill(pid: u32) {
    use std::os::windows::process::CommandExt;
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // CREATE_NO_WINDOW
        .creation_flags(0x0800_0000)
        .status();
}

/// Windows 的作业：放进去的进程启动的子进程也都在里面，父进程退出了也一样。
#[cfg(windows)]
mod job {
    use std::os::windows::io::{AsRawHandle, HandleOrNull, OwnedHandle, RawHandle};
    use std::process::Child;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const core::ffi::c_void, name: *const u16) -> RawHandle;
        fn AssignProcessToJobObject(job: RawHandle, process: RawHandle) -> i32;
        fn TerminateJobObject(job: RawHandle, code: u32) -> i32;
    }

    /// 新建一个什么限制也不设的作业。不许里面的进程离开：Git 带的 sh、ssh 这类 MSYS2 程序看到
    /// 作业许离开，启动子进程时就会让它离开。建不成是 None。
    pub fn create() -> Option<OwnedHandle> {
        // SAFETY: 两个参数都可以是空；返回的句柄归我们，建不成时是空
        let handle = unsafe {
            HandleOrNull::from_raw_handle(CreateJobObjectW(std::ptr::null(), std::ptr::null()))
        };
        OwnedHandle::try_from(handle).ok()
    }

    /// 把进程放进作业。不成（比如所在的作业不许再套）返回 false。
    pub fn assign(job: &OwnedHandle, child: &Child) -> bool {
        // SAFETY: 两个句柄都开着
        unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) != 0 }
    }

    /// 结束作业里的所有进程。
    pub fn terminate(job: &OwnedHandle) {
        // SAFETY: 句柄开着
        unsafe {
            TerminateJobObject(job.as_raw_handle(), 1);
        }
    }
}

/// 正在运行的 git 进程。
pub struct Running {
    tree: Arc<Tree>,
    stdout: ChildStdout,
    stdin_writer: Option<JoinHandle<()>>,
    stderr_reader: JoinHandle<Vec<u8>>,
    started: Instant,
}

/// 标准错误每来一块就调用一次的回调，拿来显示 fetch、push 的进度。
pub type OnStderr = Box<dyn FnMut(&[u8]) + Send>;

/// 启动 git。`stdin` 有内容时由一个线程写进 git 的标准输入，写完就关掉；没有时标准输入为空。
pub fn spawn(repo: &Path, args: &[String], stdin: Option<Vec<u8>>) -> Result<(Running, Killer)> {
    spawn_with(repo, args, stdin, None)
}

/// 同 [`spawn`]；给了 `on_stderr` 时，标准错误边产生边交给它（结束时照样整个带回来）。
pub fn spawn_with(
    repo: &Path,
    args: &[String],
    stdin: Option<Vec<u8>>,
    on_stderr: Option<OnStderr>,
) -> Result<(Running, Killer)> {
    spawn_env(repo, args, stdin, on_stderr, &[])
}

/// 同 [`spawn_with`]，这一个 git 进程再多带几个环境变量。kuaid 同时服务几个客户端时用它
/// 标出是哪个客户端的 git（要密码时知道去问谁），带上那条连接转发过来的 ssh-agent。
pub fn spawn_env(
    repo: &Path,
    args: &[String],
    stdin: Option<Vec<u8>>,
    mut on_stderr: Option<OnStderr>,
    env: &[(String, String)],
) -> Result<(Running, Killer)> {
    let mut cmd = crate::git::command(repo, args);
    cmd.envs(env.iter().map(|(key, value)| (key, value)));
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let started = Instant::now();
    // 取消时连子进程一起结束（见 [`Tree`]）
    let (tree, pipes) = Tree::spawn(&mut cmd).map_err(|err| crate::git::spawn_error(err, args))?;
    let stdout = pipes.stdout.context(tr("无法获取 git 的标准输出"))?;
    let mut stderr = pipes.stderr.context(tr("无法获取 git 的标准错误"))?;
    let stdin_writer = match (stdin, pipes.stdin) {
        (Some(data), Some(mut pipe)) => Some(std::thread::spawn(move || {
            // git 提前退出时这里会写失败，结果以退出码为准
            let _ = pipe.write_all(&data);
        })),
        _ => None,
    };
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(on_stderr) = on_stderr.as_mut() {
                        on_stderr(&chunk[..n]);
                    }
                    // 超出上限的部分读掉丢弃，免得 git 写不进去卡住
                    let room = STDERR_LIMIT.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        buf
    });
    let killer = tree.killer();
    Ok((
        Running {
            tree,
            stdout,
            stdin_writer,
            stderr_reader,
            started,
        },
        killer,
    ))
}

impl Running {
    /// 读完标准输出，每读到一块就调用一次 `on_chunk`，然后等进程结束。
    pub fn finish(mut self, mut on_chunk: impl FnMut(&[u8])) -> Result<Finished> {
        let mut buf = vec![0u8; CHUNK_SIZE];
        loop {
            match self.stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => on_chunk(&buf[..n]),
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => return Err(err).context(tr("读取 git 的输出失败")),
            }
        }
        if let Some(writer) = self.stdin_writer.take() {
            let _ = writer.join();
        }
        let stderr = self.stderr_reader.join().unwrap_or_default();
        let status = self.tree.wait().context(tr("等待 git 退出失败"))?;
        Ok(Finished {
            code: status.code(),
            stderr: String::from_utf8_lossy(&stderr).trim_end().to_string(),
            elapsed: self.started.elapsed(),
        })
    }

    /// 读完全部标准输出。
    pub fn collect(self) -> Result<(Vec<u8>, Finished)> {
        let mut out = Vec::new();
        let finished = self.finish(|chunk| out.extend_from_slice(chunk))?;
        Ok((out, finished))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn 读出标准输出和退出码() {
        let (running, _) = spawn(Path::new("."), &args(&["--version"]), None).unwrap();
        let (out, finished) = running.collect().unwrap();
        assert_eq!(finished.code, Some(0));
        assert!(String::from_utf8_lossy(&out).starts_with("git version"));
    }

    #[test]
    fn 标准输入写进git() {
        let (running, _) = spawn(
            Path::new("."),
            &args(&["hash-object", "--stdin"]),
            Some(b"hello\n".to_vec()),
        )
        .unwrap();
        let (out, finished) = running.collect().unwrap();
        assert_eq!(finished.code, Some(0));
        // `echo hello | git hash-object --stdin` 的结果
        assert_eq!(
            String::from_utf8_lossy(&out).trim(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn 失败时带回标准错误() {
        let (running, _) = spawn(Path::new("."), &args(&["no-such-command"]), None).unwrap();
        let (_, finished) = running.collect().unwrap();
        assert_ne!(finished.code, Some(0));
        assert!(!finished.stderr.is_empty());
    }

    #[test]
    fn 杀掉时连子进程一起() {
        // 别名里的 shell 命令是 git 的子进程；它活着就一直占着标准输出，finish 不会返回
        let (running, killer) = spawn(
            Path::new("."),
            &args(&["-c", "alias.frwait=!sleep 30", "frwait"]),
            None,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        killer.kill();
        running.finish(|_| {}).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "子进程没被杀掉"
        );
    }

    #[test]
    fn 可以从别的线程杀掉() {
        // 一直等标准输入的 git：不杀就不会结束
        let (running, killer) = spawn(
            Path::new("."),
            &args(&["hash-object", "--stdin"]),
            Some(Vec::new()),
        )
        .unwrap();
        killer.kill();
        let finished = running.finish(|_| {}).unwrap();
        // 被杀掉或者读到空输入正常结束都可以，关键是不会卡住
        assert!(finished.elapsed < Duration::from_secs(10));
    }

    #[test]
    fn 最外层退出了_占着输出的子进程也结束() {
        // 别名里的 shell 把 sleep 放到后台就退出，git 跟着退出；sleep 还占着标准输出，finish 不会返回
        let (running, killer) = spawn(
            Path::new("."),
            &args(&["-c", "alias.frbg=!sleep 30 &", "frbg"]),
            None,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let started = Instant::now();
        killer.kill();
        running.finish(|_| {}).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "后台的子进程没被结束"
        );
    }

    #[test]
    fn 等结束时取消拿得到锁() {
        // 标准输入输出都不接：等的那一头一直在等进程结束，取消要插得进去
        let mut cmd = crate::git::command(
            Path::new("."),
            &args(&["-c", "alias.frwait=!sleep 30", "frwait"]),
        );
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let (tree, _) = Tree::spawn(&mut cmd).unwrap();
        let killer = tree.killer();
        let waiting = std::thread::spawn(move || tree.wait().unwrap());
        std::thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        killer.kill();
        waiting.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "取消被等结束挡住了"
        );
    }
}
