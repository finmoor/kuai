//! 监听仓库的变化：工作区里的文件，还有 HEAD、暂存区、引用。客户端的本机通道和 kuaid 共用。
//!
//! 变化攒一小会儿再通知一次，通知里不带内容，收到的人自己去读 `git status`。
//! 被 .gitignore 忽略的目录（target、node_modules 这些）里的变化不通知，
//! 不然一次编译会触发几百次刷新。
//!
//! Windows 上整个仓库只要一个递归监听；Linux 的 inotify 要每个目录一个，
//! 所以自己遍历目录，跳过被忽略的。

use crate::i18n::tr;
use crate::tf;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

/// 这么久没有新的变化才通知
const QUIET: Duration = Duration::from_millis(300);
/// 一直有变化时，最多攒这么久就通知一次
const MAX_WAIT: Duration = Duration::from_secs(2);
/// 两次通知至少隔这么久
const MIN_INTERVAL: Duration = Duration::from_secs(1);
/// 被忽略的目录清单最多这么久重读一次
const IGNORED_TTL: Duration = Duration::from_secs(10);
/// Linux 上最多监听这么多个目录，免得用光 inotify 的配额
#[cfg(not(windows))]
const MAX_DIRS: usize = 50_000;

enum Msg {
    Event(notify::Result<Event>),
    Stop,
}

/// 正在进行的监听。丢弃时停止。
pub struct Watcher {
    tx: Sender<Msg>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

/// 开始监听仓库，有变化时在后台线程里调用 `on_change`。
pub fn watch(repo: &Path, on_change: impl FnMut() + Send + 'static) -> Result<Watcher> {
    if !repo.is_dir() {
        anyhow::bail!("{}", tf!("目录不存在：{repo}", repo = repo.display()));
    }
    let (tx, rx) = mpsc::channel();
    let events = tx.clone();
    let watcher = notify::recommended_watcher(move |event| {
        let _ = events.send(Msg::Event(event));
    })
    .context(tr("无法监听文件变化"))?;
    let mut state = State::new(repo.to_path_buf(), watcher);
    std::thread::Builder::new()
        .name("fr-watch".into())
        .spawn(move || {
            state.start();
            run(rx, state, on_change);
        })
        .context(tr("无法启动监听线程"))?;
    Ok(Watcher { tx })
}

fn run(rx: mpsc::Receiver<Msg>, mut state: State, mut on_change: impl FnMut()) {
    let mut last_fire: Option<Instant> = None;
    loop {
        // 等第一个要紧的变化
        match rx.recv() {
            Ok(Msg::Event(event)) => {
                if !state.handle(event) {
                    continue;
                }
            }
            Ok(Msg::Stop) | Err(_) => return,
        }
        // 再等后面跟着来的变化停下来
        let first = Instant::now();
        loop {
            match rx.recv_timeout(QUIET) {
                Ok(Msg::Event(event)) => {
                    state.handle(event);
                    if first.elapsed() > MAX_WAIT {
                        break;
                    }
                }
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
        if let Some(since) = last_fire.map(|t| t.elapsed())
            && since < MIN_INTERVAL
        {
            std::thread::sleep(MIN_INTERVAL - since);
        }
        on_change();
        last_fire = Some(Instant::now());
        state.reload_ignored_if_stale();
    }
}

struct State {
    root: PathBuf,
    /// 这个工作区自己的 git 目录（HEAD、index）
    git_dir: PathBuf,
    /// 共用的 git 目录（refs、packed-refs）；不是附加工作区时和 `git_dir` 一样
    common_dir: PathBuf,
    watcher: RecommendedWatcher,
    /// 被忽略的文件和目录，相对仓库根
    ignored: HashSet<PathBuf>,
    ignored_at: Instant,
    ignored_stale: bool,
    #[cfg(not(windows))]
    dirs: usize,
}

impl State {
    fn new(root: PathBuf, watcher: RecommendedWatcher) -> Self {
        let (git_dir, common_dir) = git_dirs(&root);
        Self {
            root,
            git_dir,
            common_dir,
            watcher,
            ignored: HashSet::new(),
            ignored_at: Instant::now(),
            ignored_stale: false,
            #[cfg(not(windows))]
            dirs: 0,
        }
    }

    /// 读被忽略的清单，装上监听。在监听线程里做：大仓库遍历目录要一会儿。
    fn start(&mut self) {
        self.reload_ignored();
        let outside = |dir: &Path, root: &Path| !dir.starts_with(root);
        #[cfg(windows)]
        {
            self.add(&self.root.clone(), RecursiveMode::Recursive);
            // git 目录在仓库外面（附加工作区、子模块）时单独监听
            if outside(&self.git_dir, &self.root) {
                self.add(&self.git_dir.clone(), RecursiveMode::NonRecursive);
            }
            if outside(&self.common_dir, &self.root) && self.common_dir != self.git_dir {
                self.add(&self.common_dir.clone(), RecursiveMode::NonRecursive);
                self.add(&self.common_dir.join("refs"), RecursiveMode::Recursive);
            }
        }
        #[cfg(not(windows))]
        {
            let _ = outside;
            self.add_tree(&self.root.clone());
            self.add(&self.git_dir.clone(), RecursiveMode::NonRecursive);
            if self.common_dir != self.git_dir {
                self.add(&self.common_dir.clone(), RecursiveMode::NonRecursive);
            }
            self.add(&self.common_dir.join("refs"), RecursiveMode::Recursive);
            // 工作树的增减：每个工作树在这里有一个目录。还没有这个目录时加不上，建出来时再加
            self.add(
                &self.common_dir.join("worktrees"),
                RecursiveMode::NonRecursive,
            );
        }
    }

    fn add(&mut self, dir: &Path, mode: RecursiveMode) -> bool {
        self.watcher.watch(dir, mode).is_ok()
    }

    /// 监听一个目录和它下面所有没被忽略的目录（inotify 不能递归）。
    #[cfg(not(windows))]
    fn add_tree(&mut self, top: &Path) {
        let mut stack = vec![top.to_path_buf()];
        while let Some(dir) = stack.pop() {
            if self.dirs >= MAX_DIRS {
                return;
            }
            if let Ok(rel) = dir.strip_prefix(&self.root)
                && (rel.starts_with(".git") || self.is_ignored(rel))
            {
                continue;
            }
            // 加不上多半是 inotify 的配额用完了：已经加上的照常用，后面的不再加
            if !self.add(&dir, RecursiveMode::NonRecursive) {
                self.dirs = MAX_DIRS;
                return;
            }
            self.dirs += 1;
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                // file_type 不跟随符号链接：链接到别处的目录不进去
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(entry.path());
                }
            }
        }
    }

    /// 处理一个事件，返回它是不是要紧的变化。
    fn handle(&mut self, event: notify::Result<Event>) -> bool {
        // 事件丢了（缓冲区溢出）之类的错误：当作有变化
        let Ok(event) = event else {
            return true;
        };
        if matches!(event.kind, EventKind::Access(_)) {
            return false;
        }
        #[cfg(not(windows))]
        if matches!(event.kind, EventKind::Create(_)) {
            for path in &event.paths {
                if path.starts_with(&self.root) && path.is_dir() {
                    self.add_tree(path);
                }
                // 第一个工作树建出来：从这时起监听它们的增减
                if *path == self.common_dir.join("worktrees") {
                    self.add(path, RecursiveMode::NonRecursive);
                }
            }
        }
        let created = matches!(event.kind, EventKind::Create(_));
        let mut hit = false;
        for path in &event.paths {
            // 忽略规则可能变了：改了 .gitignore、info/exclude，或者新建了目录（它可能正好被忽略）
            if path.ends_with("info/exclude") {
                self.ignored_stale = true;
            }
            if self.relevant(path) {
                hit = true;
                if path.file_name().is_some_and(|n| n == ".gitignore") || (created && path.is_dir())
                {
                    self.ignored_stale = true;
                }
            }
        }
        hit
    }

    fn relevant(&self, path: &Path) -> bool {
        if let Ok(rel) = path.strip_prefix(&self.git_dir) {
            return git_relevant(rel);
        }
        if let Ok(rel) = path.strip_prefix(&self.common_dir) {
            return git_relevant(rel);
        }
        match path.strip_prefix(&self.root) {
            // 仓库目录本身的事件（比如属性变了）不算
            Ok(rel) => !rel.as_os_str().is_empty() && !self.is_ignored(rel),
            Err(_) => false,
        }
    }

    fn is_ignored(&self, rel: &Path) -> bool {
        rel.ancestors().any(|a| self.ignored.contains(a))
    }

    fn reload_ignored_if_stale(&mut self) {
        if self.ignored_stale || self.ignored_at.elapsed() > IGNORED_TTL {
            self.reload_ignored();
        }
    }

    /// `--directory`：整个目录都被忽略时只列目录本身，不进去，所以很快。
    fn reload_ignored(&mut self) {
        let listed = crate::git::run(
            &self.root,
            &[
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "-z",
            ],
        )
        .unwrap_or_default();
        self.ignored = listed
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| {
                let text = String::from_utf8_lossy(p);
                PathBuf::from(text.trim_end_matches('/'))
            })
            .collect();
        self.ignored_at = Instant::now();
        self.ignored_stale = false;
    }
}

/// git 目录里哪些变化要紧：HEAD、暂存区、引用、进行到一半的合并或变基，还有工作树的增减。
/// 对象库、日志、锁文件、FETCH_HEAD 这些不算。
fn git_relevant(rel: &Path) -> bool {
    if rel.extension().is_some_and(|e| e == "lock") {
        return false;
    }
    let Some(first) = rel.components().next() else {
        return false;
    };
    // 工作树的增减、锁定、换了分支算；它们各自的暂存区、日志不算（那是另一个工作目录的事）
    if first.as_os_str() == "worktrees" {
        let mut rest = rel.components().skip(2);
        return match (rest.next(), rest.next()) {
            (None, _) => true,
            (Some(file), None) => matches!(file.as_os_str().to_str(), Some("HEAD" | "locked")),
            _ => false,
        };
    }
    matches!(
        first.as_os_str().to_str(),
        Some(
            "HEAD"
                | "index"
                | "refs"
                | "packed-refs"
                | "MERGE_HEAD"
                | "CHERRY_PICK_HEAD"
                | "REVERT_HEAD"
                | "REBASE_HEAD"
                | "rebase-merge"
                | "rebase-apply"
        )
    )
}

/// 找仓库的 git 目录。`.git` 是文件时（附加工作区、子模块）里面写着真正的位置。
fn git_dirs(root: &Path) -> (PathBuf, PathBuf) {
    let dot = root.join(".git");
    if dot.is_dir() {
        return (dot.clone(), dot);
    }
    let resolve = |base: &Path, text: &str| {
        let joined = base.join(text.trim());
        std::fs::canonicalize(&joined).unwrap_or(joined)
    };
    let git_dir = std::fs::read_to_string(&dot)
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("gitdir:"))
                .map(|p| resolve(root, p))
        })
        .unwrap_or(dot);
    let common_dir = std::fs::read_to_string(git_dir.join("commondir"))
        .map(|text| resolve(&git_dir, &text))
        .unwrap_or_else(|_| git_dir.clone());
    (git_dir, common_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn git目录里只有要紧的变化算数() {
        assert!(git_relevant(Path::new("HEAD")));
        assert!(git_relevant(Path::new("index")));
        assert!(git_relevant(Path::new("refs/heads/main")));
        assert!(!git_relevant(Path::new("index.lock")));
        assert!(!git_relevant(Path::new("refs/heads/main.lock")));
        assert!(!git_relevant(Path::new("objects/ab/cdef")));
        assert!(!git_relevant(Path::new("FETCH_HEAD")));
        assert!(!git_relevant(Path::new("logs/HEAD")));
        // 工作树：增减、锁定、换分支算，它自己的暂存区和日志不算
        assert!(git_relevant(Path::new("worktrees")));
        assert!(git_relevant(Path::new("worktrees/feat-login")));
        assert!(git_relevant(Path::new("worktrees/feat-login/HEAD")));
        assert!(git_relevant(Path::new("worktrees/feat-login/locked")));
        assert!(!git_relevant(Path::new("worktrees/feat-login/index")));
        assert!(!git_relevant(Path::new("worktrees/feat-login/HEAD.lock")));
        assert!(!git_relevant(Path::new("worktrees/feat-login/logs/HEAD")));
    }

    fn git(dir: &Path, args: &[&str]) {
        crate::git::run(dir, args).unwrap();
    }

    /// 等到计数超过 `seen`，最多等 `secs` 秒。
    fn wait_past(count: &AtomicUsize, seen: usize, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if count.load(Ordering::SeqCst) > seen {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn 改文件和提交会通知_被忽略的目录不会() {
        let dir = std::env::temp_dir().join(format!("fr-watch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(dir.join("src/a.txt"), "1\n").unwrap();
        std::fs::write(dir.join("target/out.bin"), "x").unwrap();
        git(&dir, &["add", "."]);
        git(
            &dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init",
            ],
        );

        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        let watcher = watch(&dir, move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        // 等监听装好
        std::thread::sleep(Duration::from_millis(800));

        // 被忽略的目录里写文件：不通知
        std::fs::write(dir.join("target/out.bin"), "y").unwrap();
        std::fs::write(dir.join("target/new.bin"), "z").unwrap();
        assert!(!wait_past(&count, 0, 2), "被忽略的目录不该触发通知");

        // 改工作区的文件：通知
        std::fs::write(dir.join("src/a.txt"), "2\n").unwrap();
        assert!(wait_past(&count, 0, 5), "改文件后没有收到通知");

        // 暂存（index 变了）：通知
        let seen = count.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1200));
        git(&dir, &["add", "."]);
        assert!(wait_past(&count, seen, 5), "暂存后没有收到通知");

        // 新建的子目录里写文件：通知（Linux 上要给新目录补上监听）
        let seen = count.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1200));
        std::fs::create_dir_all(dir.join("src/sub")).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let seen2 = count.load(Ordering::SeqCst);
        assert!(seen2 > seen, "新建目录后没有收到通知");
        std::fs::write(dir.join("src/sub/b.txt"), "b").unwrap();
        assert!(wait_past(&count, seen2, 5), "新目录里写文件后没有收到通知");

        drop(watcher);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
