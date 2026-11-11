//! 启动 git 进程、读它的输出，并且能从别的线程取消。agent 和客户端的本机通道共用这一套。

use crate::i18n::tr;
use crate::tf;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};

/// 标准错误最多留这么多，免得失控的输出占满内存
const STDERR_LIMIT: usize = 1 << 20;
/// 读标准输出时每次最多读这么多
pub const CHUNK_SIZE: usize = 64 * 1024;

/// git 进程结束后的状态。
#[derive(Debug, Clone, Default)]
pub struct Finished {
    /// 退出码；进程被杀掉（比如取消）时为 None
    pub code: Option<i32>,
    pub stderr: String,
    pub elapsed: Duration,
}

/// 从别的线程杀掉 git 进程，连同它启动的子进程。进程已经结束时什么也不做。
///
/// 只杀最外层不够：`git fetch` 会再启动 `git fetch <远程>`、`git-remote-https`、ssh，
/// Windows 上 PATH 里的 git.exe 还只是个转发程序，真正干活的都是子进程。
#[derive(Clone)]
pub struct Killer(Arc<Mutex<Child>>);

impl Killer {
    pub fn kill(&self) {
        if let Ok(mut child) = self.0.lock() {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            kill_tree(&mut child);
        }
    }
}

/// Windows：让 taskkill 按父子关系杀掉整棵进程树。
#[cfg(windows)]
fn kill_tree(child: &mut Child) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // CREATE_NO_WINDOW
        .creation_flags(0x0800_0000)
        .status();
    let _ = child.kill();
}

/// Unix：git 启动时自己一个进程组（见 [`spawn_with`]），给整个组发 SIGTERM。
/// 用 SIGTERM 而不是 SIGKILL，git 来得及删掉自己的锁文件。
#[cfg(unix)]
fn kill_tree(child: &mut Child) {
    // SAFETY: kill 只是发信号，参数是我们自己启动的进程组
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGTERM);
    }
}

/// 正在运行的 git 进程。
pub struct Running {
    child: Arc<Mutex<Child>>,
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
    mut on_stderr: Option<OnStderr>,
) -> Result<(Running, Killer)> {
    let mut cmd = crate::git::command(repo, args);
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 自己一个进程组，取消时连子进程一起杀
        cmd.process_group(0);
    }
    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .with_context(|| tf!("无法启动 git {join}", join = args.join(" ")))?;
    let stdout = child.stdout.take().context(tr("拿不到 git 的标准输出"))?;
    let mut stderr = child.stderr.take().context(tr("拿不到 git 的标准错误"))?;
    let stdin_writer = match (stdin, child.stdin.take()) {
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
    let child = Arc::new(Mutex::new(child));
    Ok((
        Running {
            child: child.clone(),
            stdout,
            stdin_writer,
            stderr_reader,
            started,
        },
        Killer(child),
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
        let status = self
            .child
            .lock()
            .map_err(|_| anyhow!(tr("git 进程状态损坏")))?
            .wait()
            .context(tr("等待 git 结束失败"))?;
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
}
