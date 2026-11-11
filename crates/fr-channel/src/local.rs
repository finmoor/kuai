//! 本机仓库：在进程内启动 git，每个调用一个线程收输出。

use fr_core::i18n::tr;
use std::path::Path;

use anyhow::{Result, anyhow};
use fr_core::exec::{self, Killer, OnStderr};
use futures::FutureExt as _;
use futures::channel::{mpsc, oneshot};
use futures::future::{self, BoxFuture};

use crate::{ChangeStream, DirListing, GitOutput, GitStream, StreamItem};

/// 丢弃时杀掉 git 进程；正常拿到结果后解除。
struct KillOnDrop(Option<Killer>);

impl KillOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(killer) = self.0.take() {
            killer.kill();
        }
    }
}

pub(crate) fn git(
    repo: &str,
    args: Vec<String>,
    stdin: Option<Vec<u8>>,
) -> BoxFuture<'static, Result<GitOutput>> {
    let (running, killer) = match exec::spawn(Path::new(repo), &args, stdin) {
        Ok(started) => started,
        Err(err) => return future::ready(Err(err)).boxed(),
    };
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(running.collect());
    });
    let mut guard = KillOnDrop(Some(killer));
    async move {
        let result = rx.await.map_err(|_| anyhow!(tr("git 线程意外退出")));
        guard.disarm();
        let (stdout, finished) = result??;
        Ok(GitOutput {
            code: finished.code,
            stdout,
            stderr: finished.stderr,
            elapsed: finished.elapsed,
        })
    }
    .boxed()
}

pub(crate) fn git_stream(repo: &str, args: Vec<String>, progress: bool) -> GitStream {
    let (tx, rx) = mpsc::unbounded();
    let on_stderr: Option<OnStderr> = progress.then(|| {
        let tx = tx.clone();
        Box::new(move |chunk: &[u8]| {
            let _ = tx.unbounded_send(Ok(StreamItem::Progress(chunk.to_vec())));
        }) as OnStderr
    });
    let (running, killer) = match exec::spawn_with(Path::new(repo), &args, None, on_stderr) {
        Ok(started) => started,
        Err(err) => {
            let _ = tx.unbounded_send(Err(err));
            return GitStream {
                rx,
                _cancel: Box::new(()),
            };
        }
    };
    std::thread::spawn(move || {
        let result = running.finish(|chunk| {
            let _ = tx.unbounded_send(Ok(StreamItem::Chunk(chunk.to_vec())));
        });
        let _ = tx.unbounded_send(result.map(|finished| {
            StreamItem::Done(GitOutput {
                code: finished.code,
                stdout: Vec::new(),
                stderr: finished.stderr,
                elapsed: finished.elapsed,
            })
        }));
    });
    GitStream {
        rx,
        _cancel: Box::new(KillOnDrop(Some(killer))),
    }
}

pub(crate) fn watch(repo: &str) -> ChangeStream {
    let (tx, rx) = mpsc::unbounded();
    let watcher = fr_core::watch::watch(Path::new(repo), move || {
        let _ = tx.unbounded_send(());
    });
    // 装不上监听时 tx 已经丢弃，流马上结束
    ChangeStream {
        rx,
        _cancel: match watcher {
            Ok(watcher) => Box::new(watcher),
            Err(_) => Box::new(()),
        },
    }
}

pub(crate) fn list_dir(path: &str) -> BoxFuture<'static, Result<DirListing>> {
    let path = path.to_string();
    let (tx, rx) = oneshot::channel();
    // 网络盘上读目录可能很慢，不占调用方的线程
    std::thread::spawn(move || {
        let _ = tx.send(fr_core::dir::list_dir(&path));
    });
    async move { rx.await.map_err(|_| anyhow!(tr("读目录的线程意外退出")))? }.boxed()
}
