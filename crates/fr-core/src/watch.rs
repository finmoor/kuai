//! 监听仓库的变化：工作区里的文件，还有 HEAD、暂存区、引用。客户端的本机通道和 kuaid 共用。
//!
//! 变化攒一小会儿再通知一次，通知里不带内容，收到的人自己去读 `git status`。
//! 被 .gitignore 忽略的目录（target、node_modules 这些）里的变化不通知，
//! 不然一次编译会触发几百次刷新。
//!
//! Windows 上整个仓库只要一个递归监听；Linux 的 inotify 要每个目录一个，
//! 所以自己遍历目录，跳过被忽略的。
//!
//! 仓库目录（或者 git 目录）被删掉、移走、换成另一个时监听就结束：监听装在原来那个目录上，
//! 再也收不到新目录的变化。由用的人过一会儿重新装。

use crate::i18n::tr;
use crate::tf;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

/// 这么久没有新的变化才通知
const QUIET: Duration = Duration::from_millis(300);
/// 一直有变化时，最多攒这么久就通知一次
const MAX_WAIT: Duration = Duration::from_secs(2);
/// 两次通知至少隔这么久
const MIN_INTERVAL: Duration = Duration::from_secs(1);
/// 被忽略的目录清单最多这么久重读一次
const IGNORED_TTL: Duration = Duration::from_secs(10);
/// 隔这么久看一次仓库目录、git 目录还是不是原来那几个：Windows 上目录删掉时收不到事件，
/// 上一级目录整个移走、网络盘上别处删掉的也收不到
const CHECK_EVERY: Duration = Duration::from_secs(2);
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
    running: Arc<AtomicBool>,
}

impl Watcher {
    /// 还在监听：没有因为仓库目录没了、换了而自己结束。
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

/// 开始监听仓库，有变化时在后台线程里调用 `on_change`。仓库目录（或者 git 目录）被删掉、移走、
/// 换成另一个，或者仓库目录根本装不上监听时结束：调用 `on_end` 说明原因，之后不再调用
/// `on_change`。丢弃返回的 [`Watcher`] 叫停的不调用 `on_end`。
pub fn watch(
    repo: &Path,
    on_change: impl FnMut() + Send + 'static,
    on_end: impl FnOnce(String) + Send + 'static,
) -> Result<Watcher> {
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
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    std::thread::Builder::new()
        .name("fr-watch".into())
        .spawn(move || {
            state.start();
            let ended = run(rx, state, on_change);
            flag.store(false, Ordering::Release);
            if let Some(reason) = ended {
                on_end(reason);
            }
        })
        .context(tr("无法启动监听线程"))?;
    Ok(Watcher { tx, running })
}

/// 收事件、攒一会儿、通知，直到被叫停（返回 `None`）或者监听该结束了（返回原因）。
fn run(rx: mpsc::Receiver<Msg>, mut state: State, mut on_change: impl FnMut()) -> Option<String> {
    let mut last_fire: Option<Instant> = None;
    loop {
        if let Some(reason) = state.lost() {
            return Some(reason);
        }
        // 等第一个要紧的变化；一直没有变化也隔一会儿回到上面，看看目录还是不是原来那个
        match rx.recv_timeout(CHECK_EVERY) {
            Ok(Msg::Event(event)) => {
                if !state.handle(event) {
                    continue;
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return None,
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
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return None,
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

/// 目录的身份：删掉再在原处建一个同名的，就不一样了。
#[derive(PartialEq, Eq)]
struct DirId {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    /// 创建时间：inode 号删掉以后可能马上给新建的目录重用（拿不到时是空的）
    #[cfg(unix)]
    born: Option<std::time::SystemTime>,
    #[cfg(windows)]
    volume: u64,
    #[cfg(windows)]
    index: u64,
}

impl DirId {
    /// 不存在、不是目录、打不开的是空的。
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok().filter(|meta| meta.is_dir())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Some(Self {
                dev: meta.dev(),
                ino: meta.ino(),
                born: meta.created().ok(),
            })
        }
        #[cfg(windows)]
        {
            let _ = meta;
            let handle = winapi_util::Handle::from_path_any(path).ok()?;
            let info = winapi_util::file::information(&handle).ok()?;
            Some(Self {
                volume: info.volume_serial_number(),
                index: info.file_index(),
            })
        }
    }
}

struct State {
    root: PathBuf,
    /// 这个工作区自己的 git 目录（HEAD、index）
    git_dir: PathBuf,
    /// 共用的 git 目录（refs、packed-refs）；不是附加工作区时和 `git_dir` 一样
    common_dir: PathBuf,
    /// 开始监听时这三个目录的身份，见 [`Self::lost`]
    ids: [Option<DirId>; 3],
    /// 上次看它们还是不是原来那几个的时刻
    checked: Instant,
    /// 监听该结束了：原因
    gone: Option<String>,
    watcher: RecommendedWatcher,
    /// 被忽略的文件和目录，相对仓库根
    ignored: HashSet<PathBuf>,
    ignored_at: Instant,
    ignored_stale: bool,
    /// Linux 上装了监听的目录（inotify 不能递归，每个目录一个）
    #[cfg(not(windows))]
    watched: HashSet<PathBuf>,
    /// 装不上了（inotify 的配额用完了，或者到了 [`MAX_DIRS`]）：先不再装，放掉一些以后再试
    #[cfg(not(windows))]
    full: bool,
}

impl State {
    fn new(root: PathBuf, watcher: RecommendedWatcher) -> Self {
        let (git_dir, common_dir) = git_dirs(&root);
        let ids = [&root, &git_dir, &common_dir].map(|dir| DirId::of(dir));
        Self {
            root,
            git_dir,
            common_dir,
            ids,
            checked: Instant::now(),
            gone: None,
            watcher,
            ignored: HashSet::new(),
            ignored_at: Instant::now(),
            ignored_stale: false,
            #[cfg(not(windows))]
            watched: HashSet::new(),
            #[cfg(not(windows))]
            full: false,
        }
    }

    /// 读被忽略的清单，装上监听。在监听线程里做：大仓库遍历目录要一会儿。
    fn start(&mut self) {
        self.reload_ignored();
        self.install();
    }

    /// 装上监听。仓库目录本身装不上（没有权限、配额用完了）就没法监听，结束。
    fn install(&mut self) {
        let outside = |dir: &Path, root: &Path| !dir.starts_with(root);
        #[cfg(windows)]
        {
            if self
                .add(&self.root.clone(), RecursiveMode::Recursive)
                .is_err()
            {
                self.gone = Some(tr("无法监听文件变化").to_string());
                return;
            }
            // git 目录在仓库外面（附加工作区、子模块）时单独监听
            if outside(&self.git_dir, &self.root) {
                let _ = self.add(&self.git_dir.clone(), RecursiveMode::NonRecursive);
            }
            if outside(&self.common_dir, &self.root) && self.common_dir != self.git_dir {
                let _ = self.add(&self.common_dir.clone(), RecursiveMode::NonRecursive);
                let _ = self.add(&self.common_dir.join("refs"), RecursiveMode::Recursive);
            }
        }
        #[cfg(not(windows))]
        {
            let _ = outside;
            self.add_tree(&self.root.clone());
            if !self.watched.contains(&self.root) {
                self.gone = Some(tr("无法监听文件变化").to_string());
                return;
            }
            let _ = self.add(&self.git_dir.clone(), RecursiveMode::NonRecursive);
            if self.common_dir != self.git_dir {
                let _ = self.add(&self.common_dir.clone(), RecursiveMode::NonRecursive);
            }
            let _ = self.add(&self.common_dir.join("refs"), RecursiveMode::Recursive);
            // 工作树的增减：每个工作树在这里有一个目录。还没有这个目录时加不上，建出来时再加
            let _ = self.add(
                &self.common_dir.join("worktrees"),
                RecursiveMode::NonRecursive,
            );
        }
    }

    fn add(&mut self, dir: &Path, mode: RecursiveMode) -> notify::Result<()> {
        self.watcher.watch(dir, mode)
    }

    /// 监听一个目录和它下面所有没被忽略的目录（inotify 不能递归）。已经装了的目录不再进去：
    /// 装它的时候下面的也装过了。
    #[cfg(not(windows))]
    fn add_tree(&mut self, top: &Path) {
        let mut stack = vec![top.to_path_buf()];
        while let Some(dir) = stack.pop() {
            if self.full || self.watched.len() >= MAX_DIRS {
                self.full = true;
                return;
            }
            if self.watched.contains(&dir) {
                continue;
            }
            if let Ok(rel) = dir.strip_prefix(&self.root)
                && (rel.starts_with(".git") || self.is_ignored(rel))
            {
                continue;
            }
            match self.add(&dir, RecursiveMode::NonRecursive) {
                Ok(()) => {}
                // inotify 的配额用完了：已经加上的照常用，后面的先不加
                Err(err) if matches!(err.kind, notify::ErrorKind::MaxFilesWatch) => {
                    self.full = true;
                    return;
                }
                // 刚建就又删掉了、没有权限进去：跳过这一个。原来也当成配额用完，之后新建的目录
                // 一个都不再装
                Err(_) => continue,
            }
            self.watched.insert(dir.clone());
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

    /// Linux：删掉、移走的目录从记录里拿掉（notify 已经拿掉了它们的监听），新建、移进来的装上。
    /// 原来只装不拿：删掉再建出来的目录（切分支时常见）还记着「装过了」，就不再装，改名的也一样，
    /// 里面的改动再也不通知。
    #[cfg(not(windows))]
    fn track_dirs(&mut self, event: &Event) {
        match event.kind {
            EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                for path in &event.paths {
                    self.forget(path);
                }
            }
            EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                for path in &event.paths {
                    if path.starts_with(&self.root) && path.is_dir() {
                        // 记录里还有（删掉的事件丢了）也重新装：这是新来的目录，原来的监听不在它上面
                        self.forget(path);
                        self.add_tree(path);
                    }
                    // 第一个工作树建出来：从这时起监听它们的增减
                    if *path == self.common_dir.join("worktrees") {
                        let _ = self.add(path, RecursiveMode::NonRecursive);
                    }
                }
            }
            _ => {}
        }
    }

    /// 拿掉 `top` 和它下面的目录的记录。
    #[cfg(not(windows))]
    fn forget(&mut self, top: &Path) {
        let before = self.watched.len();
        self.watched.retain(|dir| !dir.starts_with(top));
        if self.watched.len() < before {
            self.full = false;
        }
    }

    /// 事件丢了（缓冲区溢出）以后：哪些目录删了、建了都不知道，记录清掉从头装一遍。已经装着的
    /// 再装一次不要紧，删掉又建的这样也能装上新的。
    #[cfg(not(windows))]
    fn rescan(&mut self) {
        self.watched.clear();
        self.full = false;
        self.install();
    }

    /// 处理一个事件，返回它是不是要紧的变化。
    fn handle(&mut self, event: notify::Result<Event>) -> bool {
        // 出错（比如新目录装不上监听）：当作有变化
        let Ok(event) = event else {
            return true;
        };
        // 事件丢了（缓冲区溢出）：不知道变了什么，当作有变化。原来不算，什么都不通知
        if event.need_rescan() {
            #[cfg(not(windows))]
            self.rescan();
            return true;
        }
        if matches!(event.kind, EventKind::Access(_)) {
            return false;
        }
        // 仓库目录、git 目录本身被删掉、移走：监听装在原来那个上面，结束（见 [`Self::lost`]）
        let away = matches!(
            event.kind,
            EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From))
        );
        if away
            && self.gone.is_none()
            && event
                .paths
                .iter()
                .any(|path| [&self.root, &self.git_dir, &self.common_dir].contains(&path))
        {
            self.gone = Some(self.lost_reason());
        }
        #[cfg(not(windows))]
        self.track_dirs(&event);
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

    /// 监听是不是该结束了，返回原因：仓库目录、git 目录被删掉、移走，或者换成了另一个（删掉再在
    /// 原处建一个）。监听还装在原来那个上面，再也收不到新目录的变化。原来不管，监听一直挂着，
    /// 用的人也不知道。
    fn lost(&mut self) -> Option<String> {
        if self.gone.is_none() && self.checked.elapsed() >= CHECK_EVERY {
            self.checked = Instant::now();
            let now = [&self.root, &self.git_dir, &self.common_dir].map(|dir| DirId::of(dir));
            if now != self.ids {
                self.gone = Some(self.lost_reason());
            }
        }
        self.gone.take()
    }

    fn lost_reason(&self) -> String {
        tf!(
            "仓库目录已删除、移走或被替换：{repo}",
            repo = self.root.display()
        )
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
        let ignored = listed
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| {
                let text = String::from_utf8_lossy(p);
                PathBuf::from(text.trim_end_matches('/'))
            })
            .collect();
        let before = std::mem::replace(&mut self.ignored, ignored);
        self.ignored_at = Instant::now();
        self.ignored_stale = false;
        #[cfg(not(windows))]
        self.rewatch(&before);
        #[cfg(windows)]
        let _ = before;
    }

    /// 忽略规则变了以后（Linux）：不再忽略的目录补装监听（装的时候跳过了它们），新忽略的目录连同
    /// 下面的放掉，省 inotify 的配额。
    #[cfg(not(windows))]
    fn rewatch(&mut self, before: &HashSet<PathBuf>) {
        // 装了监听的目录，上面的目录一定也装了：新忽略的不是装着的目录，下面也就没有装着的
        let dropped: Vec<PathBuf> = self
            .ignored
            .iter()
            .filter(|rel| !before.contains(*rel))
            .map(|rel| self.root.join(rel))
            .filter(|dir| self.watched.contains(dir))
            .collect();
        if !dropped.is_empty() {
            let under: Vec<PathBuf> = self
                .watched
                .iter()
                .filter(|dir| dropped.iter().any(|top| dir.starts_with(top)))
                .cloned()
                .collect();
            for dir in under {
                let _ = self.watcher.unwatch(&dir);
                self.watched.remove(&dir);
            }
            self.full = false;
        }
        let back: Vec<PathBuf> = before
            .iter()
            .filter(|rel| !self.ignored.contains(*rel) && !self.is_ignored(rel))
            .map(|rel| self.root.join(rel))
            .filter(|dir| dir.is_dir())
            .collect();
        for dir in back {
            self.add_tree(&dir);
        }
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
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

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

    /// 建一个有一次提交的仓库，`dirs` 里的目录各放一个文件。
    fn make_repo(name: &str, dirs: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fr-watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        for sub in dirs {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
            std::fs::write(dir.join(sub).join("a.txt"), "1\n").unwrap();
        }
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
                "--allow-empty",
            ],
        );
        dir
    }

    /// 监听 `dir`：返回通知次数、结束的原因和监听本身。
    fn start(dir: &Path) -> (Arc<AtomicUsize>, Arc<Mutex<Option<String>>>, Watcher) {
        let count = Arc::new(AtomicUsize::new(0));
        let ended = Arc::new(Mutex::new(None));
        let (counter, end) = (count.clone(), ended.clone());
        let watcher = watch(
            dir,
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
            move |reason| *end.lock().unwrap() = Some(reason),
        )
        .unwrap();
        // 等监听装好
        std::thread::sleep(Duration::from_millis(800));
        (count, ended, watcher)
    }

    /// 等监听结束，最多等 `secs` 秒。
    fn wait_end(ended: &Mutex<Option<String>>, secs: u64) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Some(reason) = ended.lock().unwrap().clone() {
                return Some(reason);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// 改名、删目录：Windows 上别的程序（杀毒、刚退出的 git）可能还开着里面的文件，多试几次。
    fn retry(what: &str, mut op: impl FnMut() -> std::io::Result<()>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match op() {
                Ok(()) => return,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(err) => panic!("{what}：{err}"),
            }
        }
    }

    #[test]
    fn 改文件和提交会通知_被忽略的目录不会() {
        let dir = make_repo("basic", &["src"]);
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(dir.join("target/out.bin"), "x").unwrap();
        git(&dir, &["add", ".gitignore"]);

        let (count, ended, watcher) = start(&dir);

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
        assert_eq!(*ended.lock().unwrap(), None, "仓库好好的，监听不该结束");

        drop(watcher);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 不再忽略的目录里的变化也通知() {
        let dir = make_repo("unignore", &[]);
        std::fs::create_dir_all(dir.join("target/deep")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(dir.join("target/deep/out.txt"), "x").unwrap();

        let (count, _ended, watcher) = start(&dir);

        // 不再忽略 target：这一次改 .gitignore 本身就通知，之后重读忽略的清单
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        assert!(wait_past(&count, 0, 5), "改 .gitignore 后没有收到通知");
        std::thread::sleep(Duration::from_millis(1500));

        // Linux 上要给原来跳过的目录补上监听
        let seen = count.load(Ordering::SeqCst);
        std::fs::write(dir.join("target/deep/out.txt"), "y").unwrap();
        assert!(
            wait_past(&count, seen, 5),
            "不再忽略的目录里写文件后没有收到通知"
        );

        drop(watcher);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 切分支时目录会删掉再建出来。原来 Linux 上记着「装过了」不再装，之后里面的改动都不通知；
    /// 改了名的目录也一样。
    #[test]
    fn 删掉再建的目录和改了名的目录里的改动照样通知() {
        let dir = make_repo("recreate", &["src/sub", "src/old"]);
        let (count, ended, watcher) = start(&dir);

        // 删掉再建
        retry("删目录", || std::fs::remove_dir_all(dir.join("src/sub")));
        std::thread::sleep(Duration::from_millis(1500));
        std::fs::create_dir_all(dir.join("src/sub/deeper")).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let seen = count.load(Ordering::SeqCst);
        std::fs::write(dir.join("src/sub/deeper/b.txt"), "b").unwrap();
        assert!(
            wait_past(&count, seen, 5),
            "删掉再建的目录里写文件后没有收到通知"
        );

        // 改名
        std::thread::sleep(Duration::from_millis(1200));
        retry("改名", || {
            std::fs::rename(dir.join("src/old"), dir.join("src/new"))
        });
        std::thread::sleep(Duration::from_millis(1500));
        let seen = count.load(Ordering::SeqCst);
        std::fs::write(dir.join("src/new/c.txt"), "c").unwrap();
        assert!(
            wait_past(&count, seen, 5),
            "改了名的目录里写文件后没有收到通知"
        );
        assert_eq!(*ended.lock().unwrap(), None, "仓库好好的，监听不该结束");

        drop(watcher);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 仓库目录删掉了_监听结束() {
        let dir = make_repo("gone", &["src"]);
        let (_count, ended, watcher) = start(&dir);
        retry("删仓库", || std::fs::remove_dir_all(&dir));
        let reason = wait_end(&ended, 6).expect("仓库目录删掉以后监听没有结束");
        assert!(reason.contains("fr-watch-gone"), "{reason}");
        assert!(!watcher.is_running());
    }

    /// 移走以后在原处建一个新的（`cp -a` 一份、删掉原来的、再改回原来的名字）：监听还在原来那个
    /// 上面，得结束，让用的人在新的上面重新装。
    #[test]
    fn 仓库目录换成了另一个_监听结束() {
        let dir = make_repo("swap", &["src"]);
        let (_count, ended, watcher) = start(&dir);
        let away = dir.with_file_name(format!(
            "{}-away",
            dir.file_name().unwrap().to_string_lossy()
        ));
        let _ = std::fs::remove_dir_all(&away);
        retry("移走仓库", || std::fs::rename(&dir, &away));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        assert!(
            wait_end(&ended, 6).is_some(),
            "仓库目录换成另一个以后监听没有结束"
        );
        assert!(!watcher.is_running());
        drop(watcher);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&away);
    }
}
