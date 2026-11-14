//! 盘上的读写怎么变成 kuaid 的 `fs_*` 请求。
//!
//! - 读：按块（1 MiB）向服务器要，每个打开的文件留着最近读过的 16 块。连着往后读时预先把后面几块
//!   也要了（请求一发出去就在路上，不占线程），读到那里时多半已经回来。
//! - 写：直接按偏移写到服务器上（`fs_pwrite`），不等回音就告诉系统写好了，同一个文件在路上的写入
//!   最多 8 MiB，满了才等。写坏了（磁盘满、没权限）在下一次写、Flush、关文件时报出来，同时经
//!   `on_error` 告诉界面。和在路上的写入重叠的写、读、截断、改名都先等它们落地：服务器上每个请求
//!   各在各的线程里做，不等的话先后会乱。
//! - 删除：系统在最后一个句柄关掉时（Cleanup）才真的删，那一步报不了错。服务器不让删（没权限）时
//!   经 `on_error` 告诉界面，文件照旧留着。
//! - 列目录、查文件属性的结果留 2 秒：资源管理器会反复问同一批东西，还会试探一堆不存在的名字
//!   （`desktop.ini` 这类），都从留着的那份目录里答，不再问服务器。
//!
//! 同一个文件被打开几次，共用一个 [`Node`]。

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use fr_channel::Channel;
use fr_core::i18n::tr;
use fr_core::protocol::{FsEntry, FsSpace, FsStat};
use fr_core::tf;
use futures::executor::block_on;
use futures::future::BoxFuture;

use crate::winfsp::*;

/// 一次向服务器读这么多字节。
const BLOCK: u64 = 1 << 20;
/// 每个打开的文件留着最近读过的几块。
const CACHE_BLOCKS: usize = 16;
/// 连着往后读时，预先要好后面的几块。
const AHEAD: u64 = 4;
/// 一次写请求最多带这么多字节。
const CHUNK: usize = 1 << 20;
/// 同一个文件在路上（发出去了、服务器还没回音）的写入最多这么多字节。
const WINDOW: u64 = 8 << 20;
/// 目录和文件属性留多久。
pub(crate) const TTL: Duration = Duration::from_secs(2);
/// 盘的大小留多久。
const SPACE_TTL: Duration = Duration::from_secs(10);
/// 留着的目录、属性超过这么多条时，把过期的清掉。
const CACHE_MAX: usize = 4096;

/// Windows 自己会往盘的根上建的目录。
const SYSTEM_DIRS: [&str; 2] = ["$RECYCLE.BIN", "System Volume Information"];

type Nt<T> = std::result::Result<T, NTSTATUS>;
/// 留着的一层目录：什么时候列的、里面有什么。
type Listed = (Instant, Arc<Vec<FsEntry>>);
/// 列目录时的一行：名字（UTF-16，排序和翻页用）和它的情况。
type Row = (Vec<u16>, FsEntry);
type Reading = BoxFuture<'static, Result<(FsStat, Vec<u8>)>>;

#[derive(Default)]
pub struct Counters {
    /// 向服务器发了多少个请求
    pub calls: AtomicU64,
    pub down: AtomicU64,
    pub up: AtomicU64,
}

/// 一个在路上的写入：等它的回音。
struct Pending {
    node: Arc<Node>,
    offset: u64,
    len: u64,
    writing: BoxFuture<'static, Result<FsStat>>,
}

/// 写入、删除没成时要用的：出了错怎么换成状态码、告诉谁。
struct Sink {
    windows: bool,
    /// 本机这边的位置（盘符或文件夹）：告诉界面时写本机的路径，用户认得
    place: String,
    on_error: Box<dyn Fn(String) + Send + Sync>,
}

pub(crate) struct Mount {
    /// 换了连接（断线重连）时换掉；断着的时候是空的
    channel: Mutex<Option<Channel>>,
    /// 服务器上映射的那个目录
    root: String,
    /// 只读：会改动服务器的操作一律拒绝。WinFsp 的 ReadOnlyVolume 标志拦不住写入
    /// （2026-11-14，WinFsp 2.1 实测），所以在这里自己拦
    read_only: bool,
    /// 那边是 Windows：名字不分大小写，系统的错误码也是 Windows 的
    windows: bool,
    label: Vec<u16>,
    security: Vec<u8>,
    add_dir_info: unsafe extern "C" fn(*mut DirInfo, *mut c_void, u32, *mut u32) -> u8,
    /// 挂载的时刻：服务器没给修改时间的（根目录）用它
    started: u64,
    dirs: Mutex<HashMap<String, Listed>>,
    stats: Mutex<HashMap<String, (Instant, FsStat)>>,
    nodes: Mutex<HashMap<String, Weak<Node>>>,
    space: Mutex<Option<(Instant, FsSpace)>>,
    pending: Mutex<mpsc::Sender<Pending>>,
    sink: Arc<Sink>,
    /// 打开着的文件（不算目录）有几个：退出前拿来问「还有文件开着」
    pub(crate) open_files: AtomicUsize,
    pub(crate) counters: Counters,
}

/// 一个打开着的文件或目录。
pub(crate) struct Node {
    state: Mutex<NodeState>,
    /// 在路上的写入有回音了
    landed: Condvar,
}

struct NodeState {
    /// 相对映射目录的路径，`/` 分隔，根目录是空字符串
    path: String,
    stat: FsStat,
    blocks: VecDeque<(u64, Arc<Vec<u8>>)>,
    /// 预先要了、还没用上的块，按块号从小到大
    ahead: VecDeque<(u64, Reading)>,
    last_block: Option<u64>,
    /// 连着往后读了几块
    run: u64,
    /// 在路上的写入：偏移和长度
    flying: Vec<(u64, u64)>,
    flying_bytes: u64,
    /// 有一次写入没成，还没报给调用的程序
    failed: Option<NTSTATUS>,
    /// 有一次写入没成：这边记的大小、时间不准了，等在路上的都有了回音以后重新问
    resync: bool,
    /// 写过：关的时候让留着的目录作废
    dirty: bool,
    /// 这次列目录用的那一份，按名字排好；翻页时要前后一致
    listing: Option<Arc<Vec<Row>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// 1601 年到 1970 年之间的 100 纳秒数。
const EPOCH_GAP: u64 = 116_444_736_000_000_000;

/// 1970 年起的毫秒数换成 Windows 的文件时间（1601 年起的 100 纳秒数）。
pub(crate) fn filetime(millis: u64) -> u64 {
    millis * 10_000 + EPOCH_GAP
}

fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(at) => (&path[..at], &path[at + 1..]),
        None => ("", path),
    }
}

fn attributes(stat: &FsStat) -> u32 {
    let kind = match stat.dir {
        true => FILE_ATTRIBUTE_DIRECTORY,
        false => FILE_ATTRIBUTE_ARCHIVE,
    };
    match stat.readonly {
        true => kind | FILE_ATTRIBUTE_READONLY,
        false => kind,
    }
}

fn stat_of(entry: &FsEntry) -> FsStat {
    FsStat {
        exists: true,
        dir: entry.dir,
        size: entry.size,
        mtime: entry.mtime,
        readonly: false,
    }
}

/// Windows 上能不能有这个名字。Linux 的文件名里可以有 `:`、`*`、`?` 这些，结尾也可以是空格和点，
/// 这样的名字在映射出来的盘上显示不了，列目录时跳过。
pub(crate) fn representable(name: &str) -> bool {
    let bad = |c: char| c.is_control() || r#"\/:*?"<>|"#.contains(c);
    !name.is_empty() && !name.contains(bad) && !name.ends_with([' ', '.'])
}

/// 错误信息里系统给的错误码（`(os error 13)`）。
fn os_code(message: &str) -> Option<i32> {
    let (_, rest) = message.rsplit_once("(os error ")?;
    rest.split(')').next()?.trim().parse().ok()
}

/// 服务器那边的系统错误换成 Windows 的状态码，认不出来的算读写出错。Linux 和 Windows 的错误码
/// 是两套，按那边是什么系统认。
pub(crate) fn status_of(message: &str, windows: bool) -> NTSTATUS {
    let Some(code) = os_code(message) else {
        return STATUS_UNEXPECTED_IO_ERROR;
    };
    match (windows, code) {
        // ERROR_FILE_NOT_FOUND、ERROR_PATH_NOT_FOUND / ENOENT
        (true, 2 | 3) | (false, 2) => STATUS_OBJECT_NAME_NOT_FOUND,
        // ERROR_ACCESS_DENIED / EPERM、EACCES
        (true, 5) | (false, 1 | 13) => STATUS_ACCESS_DENIED,
        // ERROR_SHARING_VIOLATION、ERROR_LOCK_VIOLATION / ETXTBSY
        (true, 32 | 33) | (false, 26) => STATUS_SHARING_VIOLATION,
        // ERROR_FILE_EXISTS、ERROR_ALREADY_EXISTS / EEXIST
        (true, 80 | 183) | (false, 17) => STATUS_OBJECT_NAME_COLLISION,
        // ERROR_HANDLE_DISK_FULL、ERROR_DISK_FULL / ENOSPC、EDQUOT
        (true, 39 | 112) | (false, 28 | 122) => STATUS_DISK_FULL,
        // ERROR_DIR_NOT_EMPTY / ENOTEMPTY
        (true, 145) | (false, 39) => STATUS_DIRECTORY_NOT_EMPTY,
        // ERROR_WRITE_PROTECT / EROFS
        (true, 19) | (false, 30) => STATUS_MEDIA_WRITE_PROTECTED,
        // ERROR_DIRECTORY / ENOTDIR
        (true, 267) | (false, 20) => STATUS_NOT_A_DIRECTORY,
        // EISDIR
        (false, 21) => STATUS_FILE_IS_A_DIRECTORY,
        _ => STATUS_UNEXPECTED_IO_ERROR,
    }
}

/// 等写入回音的线程：请求发出去的先后就是这里等的先后。
fn settle(incoming: mpsc::Receiver<Pending>, sink: Arc<Sink>) {
    while let Ok(pending) = incoming.recv() {
        let result = block_on(pending.writing);
        let mut state = lock(&pending.node.state);
        let (offset, len) = (pending.offset, pending.len);
        if let Some(at) = state
            .flying
            .iter()
            .position(|range| *range == (offset, len))
        {
            state.flying.swap_remove(at);
        }
        state.flying_bytes = state.flying_bytes.saturating_sub(len);
        match result {
            // 都落地了：修改时间以服务器上的为准。大小不动：这段时间里可能又截断过
            Ok(stat) if state.flying.is_empty() => state.stat.mtime = stat.mtime,
            Ok(_) => {}
            Err(err) => {
                let message = format!("{err:#}");
                log::warn!("写入 /{} 失败：{message}", state.path);
                state.resync = true;
                state.blocks.clear();
                // 只记第一个错：后面的多半是它引出来的
                if state.failed.is_none() {
                    let status = status_of(&message, sink.windows);
                    state.failed = Some(status);
                    let text = failure(&sink.place, &state.path, status, &message, false);
                    (sink.on_error)(text);
                }
            }
        }
        drop(state);
        pending.node.landed.notify_all();
    }
}

/// 写入（`deleting` 为假）或删除落不到服务器上时告诉用户的话：本机上的路径，和认得出来的原因
/// （认不出来的给原话）。
fn failure(place: &str, path: &str, status: NTSTATUS, message: &str, deleting: bool) -> String {
    let root = place.trim_end_matches('\\');
    let local = match path.is_empty() {
        true => format!("{root}\\"),
        false => format!("{root}\\{}", path.replace('/', "\\")),
    };
    let reason = match status {
        STATUS_DISK_FULL => tr("服务器磁盘空间不足"),
        STATUS_ACCESS_DENIED if deleting => tr("没有删除权限"),
        STATUS_ACCESS_DENIED => tr("没有写入权限"),
        STATUS_MEDIA_WRITE_PROTECTED => tr("服务器上的目录是只读的"),
        STATUS_OBJECT_NAME_NOT_FOUND => tr("文件已被删除或移走"),
        STATUS_CONNECTION_DISCONNECTED => tr("与服务器的连接已断开"),
        _ => message,
    };
    match deleting {
        true => tf!("无法删除 {path}：{reason}", path = &local, reason),
        false => tf!("无法写入 {path}：{reason}", path = &local, reason),
    }
}

pub(crate) struct Settings {
    pub root: String,
    /// 本机这边的位置（盘符或文件夹），出错时告诉用户用
    pub place: String,
    pub read_only: bool,
    pub windows: bool,
    /// 盘的卷标（资源管理器里显示的名字），最多 32 个字
    pub label: String,
}

impl Mount {
    pub(crate) fn new(
        channel: Channel,
        settings: Settings,
        api: &Api,
        on_error: Box<dyn Fn(String) + Send + Sync>,
    ) -> Result<Mount> {
        let (tx, rx) = mpsc::channel();
        let sink = Arc::new(Sink {
            windows: settings.windows,
            place: settings.place.clone(),
            on_error,
        });
        let settling = sink.clone();
        std::thread::Builder::new()
            .name("mount-settle".into())
            .spawn(move || settle(rx, settling))?;
        Ok(Mount {
            channel: Mutex::new(Some(channel)),
            root: settings.root,
            read_only: settings.read_only,
            windows: settings.windows,
            label: settings.label.encode_utf16().take(32).collect(),
            security: open_security_descriptor()?,
            add_dir_info: api.FspFileSystemAddDirInfo,
            started: filetime(now_millis()),
            dirs: Mutex::default(),
            stats: Mutex::default(),
            nodes: Mutex::default(),
            space: Mutex::default(),
            pending: Mutex::new(tx),
            sink,
            open_files: AtomicUsize::new(0),
            counters: Counters::default(),
        })
    }

    /// 换一条连接（重连上了），或者拿掉（断了）。留着的目录、属性都不要了。
    pub(crate) fn set_channel(&self, channel: Option<Channel>) {
        *lock(&self.channel) = channel;
        lock(&self.dirs).clear();
        lock(&self.stats).clear();
    }

    fn channel(&self) -> Nt<Channel> {
        match lock(&self.channel).clone() {
            // 连接断了：等上面换一条新的（见 `set_channel`）
            Some(Channel::Remote(conn)) if !conn.is_alive() => Err(STATUS_CONNECTION_DISCONNECTED),
            Some(channel) => Ok(channel),
            None => Err(STATUS_CONNECTION_DISCONNECTED),
        }
    }

    /// 等服务器的回音。
    fn wait<T>(&self, what: &str, path: &str, future: BoxFuture<'static, Result<T>>) -> Nt<T> {
        self.counters.calls.fetch_add(1, Relaxed);
        block_on(future).map_err(|err| {
            let message = format!("{err:#}");
            let status = status_of(&message, self.windows);
            // 试探不存在的文件是常事，不记
            if status != STATUS_OBJECT_NAME_NOT_FOUND {
                log::warn!("{what} /{path} 失败：{message}");
            }
            status
        })
    }

    /// 只读时拒绝会改动服务器的操作。
    fn writable(&self) -> Nt<()> {
        match self.read_only {
            true => Err(STATUS_MEDIA_WRITE_PROTECTED),
            false => Ok(()),
        }
    }

    fn same_name(&self, a: &str, b: &str) -> bool {
        match self.windows {
            true => a.to_lowercase() == b.to_lowercase(),
            false => a == b,
        }
    }

    /// 留着的目录、属性、打开着的文件按什么认。那边是 Windows 时不分大小写：同一个路径，系统
    /// 给的写法可能不一样（改名时新路径里上级目录是大写的，WinFsp 记的名字就是那样，见
    /// `rename`），都要认成同一个，不然改名后作废的是另一个写法，旧的「不存在」还留着。
    fn key(&self, path: &str) -> String {
        match self.windows {
            true => path.to_lowercase(),
            false => path.to_string(),
        }
    }

    fn remember(&self, path: &str, stat: FsStat) {
        let mut stats = lock(&self.stats);
        if stats.len() > CACHE_MAX {
            stats.retain(|_, (at, _)| at.elapsed() < TTL);
        }
        stats.insert(self.key(path), (Instant::now(), stat));
    }

    /// 这个路径上的东西变了：它自己、它所在的目录留着的那些都不能用了。
    fn invalidate(&self, path: &str) {
        let key = self.key(path);
        lock(&self.stats).remove(&key);
        let mut dirs = lock(&self.dirs);
        dirs.remove(split(&key).0);
        dirs.remove(&key);
    }

    /// 一个目录改了名、删了：它下面留着的都不能用了。
    fn invalidate_tree(&self, path: &str) {
        let key = self.key(path);
        let inside = format!("{key}/");
        lock(&self.stats).retain(|kept, _| *kept != key && !kept.starts_with(&inside));
        lock(&self.dirs).retain(|kept, _| *kept != key && !kept.starts_with(&inside));
        self.invalidate(path);
    }

    /// 列一层目录；`max_age` 以内列过的直接用。Windows 上显示不了的名字不在里面。
    fn list(&self, dir: &str, max_age: Duration) -> Nt<Arc<Vec<FsEntry>>> {
        let key = self.key(dir);
        if let Some((at, entries)) = lock(&self.dirs).get(&key)
            && at.elapsed() < max_age
        {
            return Ok(entries.clone());
        }
        let listing = self.channel()?.fs_list_all(&self.root, dir);
        let mut entries = self.wait("fs_list", dir, listing)?;
        entries.retain(|entry| representable(&entry.name));
        let entries = Arc::new(entries);
        let mut dirs = lock(&self.dirs);
        if dirs.len() > CACHE_MAX {
            dirs.retain(|_, (at, _)| at.elapsed() < TTL);
        }
        dirs.insert(key, (Instant::now(), entries.clone()));
        Ok(entries)
    }

    /// 这个路径上有什么。先看留着的属性，再看留着的上级目录（里面没有就是不存在），都没有才问服务器。
    fn lookup(&self, path: &str) -> Nt<FsStat> {
        if path.is_empty() {
            return Ok(FsStat {
                exists: true,
                dir: true,
                ..FsStat::default()
            });
        }
        if let Some((at, stat)) = lock(&self.stats).get(&self.key(path))
            && at.elapsed() < TTL
        {
            return Ok(*stat);
        }
        let (parent, name) = split(path);
        if let Some((at, entries)) = lock(&self.dirs).get(&self.key(parent))
            && at.elapsed() < TTL
        {
            let found = entries
                .iter()
                .find(|entry| self.same_name(&entry.name, name));
            return Ok(found.map(stat_of).unwrap_or_default());
        }
        if !representable(name) {
            return Ok(FsStat::default());
        }
        let asking = self.channel()?.fs_stat(&self.root, path);
        let stat = self.wait("fs_stat", path, asking)?;
        self.remember(path, stat);
        Ok(stat)
    }

    /// 同一个路径共用一个 [`Node`]。
    fn node(&self, path: &str, stat: FsStat) -> Arc<Node> {
        let (node, reused) = {
            let mut nodes = lock(&self.nodes);
            match nodes.get(&self.key(path)).and_then(Weak::upgrade) {
                Some(node) => (node, true),
                None => {
                    let node = Arc::new(Node {
                        state: Mutex::new(NodeState {
                            path: path.to_string(),
                            stat,
                            blocks: VecDeque::new(),
                            ahead: VecDeque::new(),
                            last_block: None,
                            run: 0,
                            flying: Vec::new(),
                            flying_bytes: 0,
                            failed: None,
                            resync: false,
                            dirty: false,
                            listing: None,
                        }),
                        landed: Condvar::new(),
                    });
                    nodes.insert(self.key(path), Arc::downgrade(&node));
                    (node, false)
                }
            }
        };
        if reused {
            let mut state = lock(&node.state);
            let changed = state.stat.mtime != stat.mtime || state.stat.size != stat.size;
            // 自己正在写的不算别人改的
            if changed && state.flying.is_empty() && !state.dirty {
                state.stat = stat;
                state.blocks.clear();
                state.ahead.clear();
            }
        }
        node
    }

    fn info(&self, stat: &FsStat) -> FileInfo {
        let size = if stat.dir { 0 } else { stat.size };
        let time = match stat.mtime {
            0 => self.started,
            millis => filetime(millis),
        };
        FileInfo {
            FileAttributes: attributes(stat),
            AllocationSize: size.div_ceil(4096) * 4096,
            FileSize: size,
            CreationTime: time,
            LastAccessTime: time,
            LastWriteTime: time,
            ChangeTime: time,
            ..FileInfo::default()
        }
    }

    fn open(&self, path: &str) -> Nt<Arc<Node>> {
        let stat = self.lookup(path)?;
        if !stat.exists {
            return Err(STATUS_OBJECT_NAME_NOT_FOUND);
        }
        Ok(self.node(path, stat))
    }

    fn create(&self, path: &str, dir: bool) -> Nt<Arc<Node>> {
        self.writable()?;
        if !representable(split(path).1) {
            return Err(STATUS_OBJECT_NAME_INVALID);
        }
        // 系统想在盘的根上建回收站、索引：不让，免得服务器的目录里多出这些。没有回收站时
        // 资源管理器会改成直接删除（删之前会问）
        let top = path.split('/').next().unwrap_or(path);
        if SYSTEM_DIRS
            .iter()
            .any(|name| top.eq_ignore_ascii_case(name))
        {
            return Err(STATUS_ACCESS_DENIED);
        }
        if self.lookup(path)?.exists {
            return Err(STATUS_OBJECT_NAME_COLLISION);
        }
        let channel = self.channel()?;
        let stat = if dir {
            self.wait("fs_mkdir", path, channel.fs_mkdir(&self.root, path))?;
            FsStat {
                exists: true,
                dir: true,
                mtime: now_millis(),
                ..FsStat::default()
            }
        } else {
            // 马上在服务器上建出来：别的程序接着就会来查它在不在
            let making = channel.fs_resize(&self.root, path, 0, true);
            self.wait("fs_resize", path, making)?
        };
        self.invalidate(path);
        self.remember(path, stat);
        Ok(self.node(path, stat))
    }

    /// 等在路上的写入都落地。有一次没成的话，这边记的大小、时间是照「写成了」算的，
    /// 重新问服务器要一次。
    fn drain<'a>(
        &self,
        node: &'a Node,
        mut state: MutexGuard<'a, NodeState>,
    ) -> MutexGuard<'a, NodeState> {
        while !state.flying.is_empty() {
            state = node
                .landed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        if std::mem::take(&mut state.resync)
            && let Ok(channel) = self.channel()
        {
            let path = state.path.clone();
            if let Ok(stat) = self.wait("fs_stat", &path, channel.fs_stat(&self.root, &path)) {
                state.stat = stat;
                self.remember(&path, stat);
            }
        }
        state
    }

    /// 同 [`Mount::drain`]，给会改文件的操作用（Flush、截断、改名、改时间）：有一次写入没成的话
    /// 在这里报出来，只报一次。读不走这里：别的程序（杀毒、索引）来读一下，不该把错误领走。
    fn settled<'a>(
        &self,
        node: &'a Node,
        state: MutexGuard<'a, NodeState>,
    ) -> Nt<MutexGuard<'a, NodeState>> {
        let mut state = self.drain(node, state);
        match state.failed.take() {
            Some(status) => Err(status),
            None => Ok(state),
        }
    }

    /// 一块读回来了：留着。别人改过这个文件的话，之前留着的不能再用。
    fn keep(
        &self,
        state: &mut NodeState,
        index: u64,
        stat: FsStat,
        bytes: Vec<u8>,
    ) -> Arc<Vec<u8>> {
        self.counters.down.fetch_add(bytes.len() as u64, Relaxed);
        if stat.mtime != state.stat.mtime || stat.size != state.stat.size {
            state.blocks.clear();
            state.stat = stat;
        }
        let data = Arc::new(bytes);
        state.blocks.push_back((index, data.clone()));
        if state.blocks.len() > CACHE_BLOCKS {
            state.blocks.pop_front();
        }
        data
    }

    /// 要第 `index` 块：留着就用留着的，预先要了的等它回来，都没有就向服务器读。
    fn block(&self, state: &mut NodeState, index: u64) -> Nt<Arc<Vec<u8>>> {
        // 是不是接着上一块往后读
        state.run = match state.last_block {
            Some(last) if last + 1 == index => state.run + 1,
            Some(last) if last == index => state.run,
            _ => 0,
        };
        state.last_block = Some(index);
        let cached = state.blocks.iter().find(|(at, _)| *at == index);
        let data = match cached.map(|(_, data)| data.clone()) {
            Some(data) => data,
            None => {
                // 预先要的那些里，这一块前面的用不上了
                while state.ahead.front().is_some_and(|(at, _)| *at < index) {
                    state.ahead.pop_front();
                }
                let reading = match state.ahead.front() {
                    Some((at, _)) if *at == index => state.ahead.pop_front().map(|(_, r)| r),
                    // 跳到别处读了：预先要的都不要了
                    _ => {
                        state.ahead.clear();
                        None
                    }
                };
                let reading = match reading {
                    Some(reading) => reading,
                    None => self
                        .channel()?
                        .fs_read(&self.root, &state.path, index * BLOCK, BLOCK),
                };
                let path = state.path.clone();
                let (stat, bytes) = self.wait("fs_read", &path, reading)?;
                self.keep(state, index, stat, bytes)
            }
        };
        // 连着读了两块以上：把后面几块也要了，请求这就发出去，读到那里时多半已经回来
        if state.run >= 1 {
            let channel = self.channel()?;
            let mut next = state.ahead.back().map_or(index + 1, |(at, _)| at + 1);
            while next <= index + AHEAD && next * BLOCK < state.stat.size {
                if !state.blocks.iter().any(|(at, _)| *at == next) {
                    let reading = channel.fs_read(&self.root, &state.path, next * BLOCK, BLOCK);
                    self.counters.calls.fetch_add(1, Relaxed);
                    state.ahead.push_back((next, reading));
                }
                next += 1;
            }
        }
        Ok(data)
    }

    fn read(&self, node: &Node, buffer: &mut [u8], offset: u64) -> Nt<usize> {
        let mut state = self.drain(node, lock(&node.state));
        if offset >= state.stat.size {
            return Err(STATUS_END_OF_FILE);
        }
        let end = (offset + buffer.len() as u64).min(state.stat.size);
        let mut done = 0usize;
        while offset + (done as u64) < end {
            let at = offset + done as u64;
            let index = at / BLOCK;
            let data = self.block(&mut state, index)?;
            let within = (at - index * BLOCK) as usize;
            if within >= data.len() {
                // 文件变短了
                break;
            }
            let len = (data.len() - within).min((end - at) as usize);
            buffer[done..done + len].copy_from_slice(&data[within..within + len]);
            done += len;
        }
        match done {
            0 => Err(STATUS_END_OF_FILE),
            done => Ok(done),
        }
    }

    fn write(
        &self,
        node: &Arc<Node>,
        data: &[u8],
        offset: u64,
        to_end: bool,
        constrained: bool,
    ) -> Nt<(usize, FileInfo)> {
        self.writable()?;
        let mut state = lock(&node.state);
        if let Some(status) = state.failed.take() {
            return Err(status);
        }
        let offset = if to_end { state.stat.size } else { offset };
        let mut len = data.len() as u64;
        // 系统把缓存里的页写回来时不能把文件写长
        if constrained {
            len = len.min(state.stat.size.saturating_sub(offset));
        }
        if len == 0 {
            return Ok((0, self.info(&state.stat)));
        }
        // 在路上的太多、或者和在路上的重叠：等它们落地。服务器上每个请求各在各的线程里做，
        // 重叠的两次写不等的话先后会乱
        loop {
            let overlaps = state
                .flying
                .iter()
                .any(|(at, size)| offset < at + size && *at < offset + len);
            let full = state.flying_bytes + len > WINDOW && !state.flying.is_empty();
            if !overlaps && !full {
                break;
            }
            state = node
                .landed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(status) = state.failed.take() {
                return Err(status);
            }
        }
        let channel = self.channel()?;
        let sender = lock(&self.pending).clone();
        let mut at = offset;
        for chunk in data[..len as usize].chunks(CHUNK) {
            let size = chunk.len() as u64;
            let writing = channel.fs_pwrite(&self.root, &state.path, at, chunk.to_vec(), false);
            self.counters.calls.fetch_add(1, Relaxed);
            self.counters.up.fetch_add(size, Relaxed);
            state.flying.push((at, size));
            state.flying_bytes += size;
            let pending = Pending {
                node: node.clone(),
                offset: at,
                len: size,
                writing,
            };
            if sender.send(pending).is_err() {
                // 等回音的线程没了：这个盘正在卸载
                state.flying.pop();
                state.flying_bytes -= size;
                return Err(STATUS_CONNECTION_DISCONNECTED);
            }
            at += size;
        }
        state.stat.size = state.stat.size.max(offset + len);
        state.stat.mtime = now_millis();
        state.blocks.clear();
        state.ahead.clear();
        state.dirty = true;
        self.remember(&state.path, state.stat);
        Ok((len as usize, self.info(&state.stat)))
    }

    /// 等写入落地；写过的话让留着的目录作废（大小、时间变了）。
    fn commit(&self, node: &Node) -> Nt<FileInfo> {
        let mut state = self.settled(node, lock(&node.state))?;
        if std::mem::take(&mut state.dirty) {
            self.invalidate(&state.path);
            self.remember(&state.path, state.stat);
        }
        Ok(self.info(&state.stat))
    }

    fn remove(&self, node: &Node) {
        if self.read_only {
            return;
        }
        // 要删了：没落地的写入成没成都无所谓
        let mut state = self.drain(node, lock(&node.state));
        state.failed = None;
        let path = state.path.clone();
        // 原话留着：认不出来的原因要照原话告诉用户
        let removed = match self.channel() {
            Ok(channel) => {
                self.counters.calls.fetch_add(1, Relaxed);
                block_on(channel.fs_remove_shallow(&self.root, &path)).map_err(|err| {
                    let message = format!("{err:#}");
                    (status_of(&message, self.windows), message)
                })
            }
            Err(status) => Err((status, String::new())),
        };
        match removed {
            // 服务器上已经没有了（别人删的）：也算删掉了
            Ok(()) | Err((STATUS_OBJECT_NAME_NOT_FOUND, _)) => {}
            // 系统这时已经当作删掉了（Cleanup 报不了错）：告诉用户没删成，留着的照旧
            Err((status, message)) => {
                log::warn!("fs_remove /{path} 失败：{message}");
                let text = failure(&self.sink.place, &path, status, &message, true);
                (self.sink.on_error)(text);
                return;
            }
        }
        state.stat = FsStat::default();
        state.dirty = false;
        self.invalidate_tree(&path);
        self.remember(&path, FsStat::default());
    }

    /// Windows 的规矩：目录里有东西就不能删。问服务器要现在的，Windows 上显示不了的名字也算。
    fn can_delete(&self, node: &Node) -> Nt<()> {
        self.writable()?;
        let state = lock(&node.state);
        if !state.stat.dir {
            return Ok(());
        }
        let listing = self.channel()?.fs_list_all(&self.root, &state.path);
        match self.wait("fs_list", &state.path, listing)?.is_empty() {
            true => Ok(()),
            false => Err(STATUS_DIRECTORY_NOT_EMPTY),
        }
    }

    fn rename(&self, node: &Node, to: &str, replace: bool) -> Nt<()> {
        self.writable()?;
        if !representable(split(to).1) {
            return Err(STATUS_OBJECT_NAME_INVALID);
        }
        let mut state = self.settled(node, lock(&node.state))?;
        let from = state.path.clone();
        // 不分大小写的盘上，系统给的新路径里上级目录是大写的。在同一个目录里改名（最常见）时
        // 换回原来的写法：之后发给服务器、出错时告诉用户的路径都还是原样
        let (old_parent, new_parent) = (split(&from).0, split(to).0);
        let to = match old_parent != new_parent && self.same_name(old_parent, new_parent) {
            true if old_parent.is_empty() => split(to).1.to_string(),
            true => format!("{old_parent}/{}", split(to).1),
            false => to.to_string(),
        };
        let to = to.as_str();
        let channel = self.channel()?;
        let target = self.wait("fs_stat", to, channel.fs_stat(&self.root, to))?;
        let moving = if target.exists && !self.same_name(&from, to) {
            if !replace {
                return Err(STATUS_OBJECT_NAME_COLLISION);
            }
            if target.dir {
                return Err(STATUS_ACCESS_DENIED);
            }
            channel.fs_rename_over(&self.root, &from, to)
        } else {
            channel.fs_rename(&self.root, &from, to)
        };
        self.wait("fs_rename", &from, moving)?;
        state.path = to.to_string();
        {
            let mut nodes = lock(&self.nodes);
            if let Some(weak) = nodes.remove(&self.key(&from)) {
                nodes.insert(self.key(to), weak);
            }
        }
        self.invalidate_tree(&from);
        self.invalidate_tree(to);
        Ok(())
    }

    /// 截短或接长。`allocation` 是系统在预留空间：比文件小才算截断，别的不用管。
    fn resize(&self, node: &Node, size: u64, allocation: bool) -> Nt<FileInfo> {
        self.writable()?;
        let mut state = self.settled(node, lock(&node.state))?;
        let current = state.stat.size;
        if size == current || (allocation && size >= current) {
            return Ok(self.info(&state.stat));
        }
        let path = state.path.clone();
        let resizing = self.channel()?.fs_resize(&self.root, &path, size, false);
        state.stat = self.wait("fs_resize", &path, resizing)?;
        state.blocks.clear();
        state.ahead.clear();
        state.dirty = true;
        self.remember(&path, state.stat);
        Ok(self.info(&state.stat))
    }

    /// 改修改时间。`write` 是 Windows 的文件时间，0 是不改。属性（只读、隐藏）不管。
    fn set_times(&self, node: &Node, write: u64) -> Nt<FileInfo> {
        let mut state = self.settled(node, lock(&node.state))?;
        // 0 和全 1 都是「不改」；1970 年以前的也不管
        let keep = write == 0 || write == u64::MAX || write < EPOCH_GAP;
        if keep || self.read_only || state.stat.dir {
            return Ok(self.info(&state.stat));
        }
        let mtime = (write - EPOCH_GAP) / 10_000;
        let path = state.path.clone();
        let touching = self.channel()?.fs_touch(&self.root, &path, mtime);
        state.stat = self.wait("fs_touch", &path, touching)?;
        state.dirty = true;
        self.remember(&path, state.stat);
        Ok(self.info(&state.stat))
    }

    fn close(&self, node: Arc<Node>) {
        // 系统可能在 Cleanup 之后才把缓存里的页写回来，这里再等一次
        let _ = self.commit(&node);
        let key = self.key(&lock(&node.state).path);
        drop(node);
        let mut nodes = lock(&self.nodes);
        if nodes.get(&key).is_some_and(|weak| weak.strong_count() == 0) {
            nodes.remove(&key);
        }
    }

    /// 卸载前：等所有开着的文件在路上的写入落地。
    pub(crate) fn finish(&self) {
        let nodes: Vec<Arc<Node>> = lock(&self.nodes)
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for node in nodes {
            drop(self.drain(&node, lock(&node.state)));
        }
    }

    /// 盘的大小。问不到（旧的 kuaid、目录没了）时给一个假的，免得系统以为盘满了。
    fn volume(&self) -> FsSpace {
        if let Some((at, space)) = *lock(&self.space)
            && at.elapsed() < SPACE_TTL
        {
            return space;
        }
        let asked = self
            .channel()
            .and_then(|channel| self.wait("fs_space", "", channel.fs_space(&self.root)));
        let space = asked.unwrap_or(FsSpace {
            total: 1 << 40,
            free: 1 << 39,
        });
        *lock(&self.space) = Some((Instant::now(), space));
        space
    }

    /// 列目录。`marker` 是上一批的最后一个名字：只给排在它后面的。
    fn read_directory(
        &self,
        node: &Node,
        marker: Option<&[u16]>,
        buffer: *mut c_void,
        length: u32,
        transferred: *mut u32,
    ) -> Nt<()> {
        let listing = {
            let mut state = lock(&node.state);
            if marker.is_none() || state.listing.is_none() {
                let entries = self.list(&state.path, TTL)?;
                let mut rows: Vec<(Vec<u16>, FsEntry)> = entries
                    .iter()
                    .map(|entry| (entry.name.encode_utf16().collect(), entry.clone()))
                    .collect();
                if !state.path.is_empty() {
                    for name in [".", ".."] {
                        let entry = FsEntry {
                            name: name.to_string(),
                            dir: true,
                            mtime: state.stat.mtime,
                            ..FsEntry::default()
                        };
                        rows.push((name.encode_utf16().collect(), entry));
                    }
                }
                rows.sort_by(|a, b| a.0.cmp(&b.0));
                state.listing = Some(Arc::new(rows));
            }
            state.listing.clone().unwrap_or_default()
        };
        for (name, entry) in listing.iter() {
            if marker.is_some_and(|marker| name.as_slice() <= marker) {
                continue;
            }
            let bytes = size_of::<DirInfo>() + name.len() * 2;
            // 用 u64 的数组当缓冲区，对齐才够
            let mut record = vec![0u64; bytes.div_ceil(8)];
            let info = record.as_mut_ptr().cast::<DirInfo>();
            // SAFETY：record 够 bytes 这么大、按 8 字节对齐；名字紧跟在定长部分后面，没有越界。
            // buffer、transferred 是 WinFsp 给这次回调的
            let added = unsafe {
                (*info).Size = bytes as u16;
                (*info).FileInfo = self.info(&stat_of(entry));
                std::ptr::copy_nonoverlapping(
                    name.as_ptr(),
                    info.cast::<u8>().add(size_of::<DirInfo>()).cast::<u16>(),
                    name.len(),
                );
                (self.add_dir_info)(info, buffer, length, transferred)
            };
            if added == 0 {
                // 这一批装满了，系统会带着最后一个名字再来要
                return Ok(());
            }
        }
        // SAFETY：空指针是告诉 WinFsp 列完了
        unsafe { (self.add_dir_info)(std::ptr::null_mut(), buffer, length, transferred) };
        Ok(())
    }

    /// # Safety
    /// 指针是 WinFsp 给回调的那两个。
    unsafe fn copy_security(&self, descriptor: *mut c_void, size: *mut usize) -> Nt<()> {
        if size.is_null() {
            return Ok(());
        }
        let need = self.security.len();
        unsafe {
            if *size < need {
                *size = need;
                return Err(STATUS_BUFFER_OVERFLOW);
            }
            *size = need;
            if !descriptor.is_null() {
                std::ptr::copy_nonoverlapping(self.security.as_ptr(), descriptor.cast(), need);
            }
        }
        Ok(())
    }
}

// ---- WinFsp 的回调：把指针换成上面那些函数的参数 ----
//
// 这些函数由 WinFsp 的调度线程调用：`fs` 是 `FspFileSystemCreate` 给的那个，它的 UserContext
// 指向一个活着的 [`Mount`]（卸载时先停调度线程再放掉它）；`context` 是 `open`、`create` 交出去的
// `Box<Handle>`，`close` 时收回。别的指针都是这一次回调里有效的。

type Fs = *mut FileSystem;
type Ctx = *mut c_void;

unsafe fn mount<'a>(fs: Fs) -> &'a Mount {
    unsafe { &*user_context(fs).cast::<Mount>() }
}

/// 一次打开。
struct Handle {
    node: Arc<Node>,
    /// 算在「开着的文件」里。程序关掉句柄（Cleanup）就不算了：系统的缓存还会把它留一阵才
    /// Close，那一阵不算开着
    counted: AtomicBool,
}

impl Handle {
    fn uncount(&self, mount: &Mount) {
        if self.counted.swap(false, Relaxed) {
            mount.open_files.fetch_sub(1, Relaxed);
        }
    }
}

unsafe fn handle<'a>(context: Ctx) -> &'a Handle {
    unsafe { &*context.cast::<Handle>() }
}

unsafe fn node<'a>(context: Ctx) -> &'a Arc<Node> {
    unsafe { &handle(context).node }
}

/// `\目录\文件` 换成 `目录/文件`。
unsafe fn remote_path(name: PWSTR) -> String {
    let name = String::from_utf16_lossy(unsafe { wide_slice(name) });
    name.trim_start_matches('\\').replace('\\', "/")
}

/// 回调里出了错、崩了，都变成一个状态码交回去；不能让崩溃穿过 C 的调用栈。
fn guard(run: impl FnOnce() -> Nt<()>) -> NTSTATUS {
    match catch_unwind(AssertUnwindSafe(run)) {
        Ok(Ok(())) => STATUS_SUCCESS,
        Ok(Err(status)) => status,
        Err(_) => STATUS_UNEXPECTED_IO_ERROR,
    }
}

/// 交出去一个打开的文件：记一下有几个文件开着。
unsafe fn hand_out(mount: &Mount, node: Arc<Node>, context: *mut Ctx, info: *mut FileInfo) {
    let stat = lock(&node.state).stat;
    if !stat.dir {
        mount.open_files.fetch_add(1, Relaxed);
    }
    let handle = Handle {
        node,
        counted: AtomicBool::new(!stat.dir),
    };
    unsafe {
        *info = mount.info(&stat);
        *context = Box::into_raw(Box::new(handle)).cast();
    }
}

unsafe extern "C" fn get_volume_info(fs: Fs, info: *mut VolumeInfo) -> NTSTATUS {
    guard(|| unsafe {
        let mount = mount(fs);
        let space = mount.volume();
        let info = &mut *info;
        info.TotalSize = space.total;
        info.FreeSize = space.free;
        info.VolumeLabelLength = (mount.label.len() * 2) as u16;
        info.VolumeLabel[..mount.label.len()].copy_from_slice(&mount.label);
        Ok(())
    })
}

unsafe extern "C" fn get_security_by_name(
    fs: Fs,
    name: PWSTR,
    attributes_out: *mut u32,
    descriptor: *mut c_void,
    size: *mut usize,
) -> NTSTATUS {
    guard(|| unsafe {
        let mount = mount(fs);
        let stat = mount.lookup(&remote_path(name))?;
        if !stat.exists {
            return Err(STATUS_OBJECT_NAME_NOT_FOUND);
        }
        if !attributes_out.is_null() {
            *attributes_out = attributes(&stat);
        }
        mount.copy_security(descriptor, size)
    })
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn create(
    fs: Fs,
    name: PWSTR,
    options: u32,
    _granted: u32,
    _attributes: u32,
    _descriptor: *mut c_void,
    _allocation: u64,
    context: *mut Ctx,
    info: *mut FileInfo,
) -> NTSTATUS {
    guard(|| unsafe {
        let mount = mount(fs);
        let node = mount.create(&remote_path(name), options & FILE_DIRECTORY_FILE != 0)?;
        hand_out(mount, node, context, info);
        Ok(())
    })
}

unsafe extern "C" fn open(
    fs: Fs,
    name: PWSTR,
    _options: u32,
    _granted: u32,
    context: *mut Ctx,
    info: *mut FileInfo,
) -> NTSTATUS {
    guard(|| unsafe {
        let mount = mount(fs);
        let node = mount.open(&remote_path(name))?;
        hand_out(mount, node, context, info);
        Ok(())
    })
}

unsafe extern "C" fn overwrite(
    fs: Fs,
    context: Ctx,
    _attributes: u32,
    _replace: u8,
    _allocation: u64,
    info: *mut FileInfo,
) -> NTSTATUS {
    guard(|| unsafe {
        *info = mount(fs).resize(node(context), 0, false)?;
        Ok(())
    })
}

unsafe extern "C" fn cleanup(fs: Fs, context: Ctx, _name: PWSTR, flags: u32) {
    guard(|| unsafe {
        let (mount, node) = (mount(fs), node(context));
        handle(context).uncount(mount);
        if flags & CLEANUP_DELETE != 0 {
            mount.remove(node);
            return Ok(());
        }
        mount.commit(node).map(|_| ())
    });
}

unsafe extern "C" fn close(fs: Fs, context: Ctx) {
    guard(|| unsafe {
        let mount = mount(fs);
        let handle = Box::from_raw(context.cast::<Handle>());
        handle.uncount(mount);
        mount.close(handle.node);
        Ok(())
    });
}

unsafe extern "C" fn read(
    fs: Fs,
    context: Ctx,
    buffer: *mut c_void,
    offset: u64,
    length: u32,
    transferred: *mut u32,
) -> NTSTATUS {
    guard(|| unsafe {
        let buffer = std::slice::from_raw_parts_mut(buffer.cast::<u8>(), length as usize);
        *transferred = mount(fs).read(node(context), buffer, offset)? as u32;
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn write(
    fs: Fs,
    context: Ctx,
    buffer: *mut c_void,
    offset: u64,
    length: u32,
    to_end: u8,
    constrained: u8,
    transferred: *mut u32,
    info: *mut FileInfo,
) -> NTSTATUS {
    guard(|| unsafe {
        let data = std::slice::from_raw_parts(buffer.cast::<u8>(), length as usize);
        let (written, now) =
            mount(fs).write(node(context), data, offset, to_end != 0, constrained != 0)?;
        *transferred = written as u32;
        *info = now;
        Ok(())
    })
}

unsafe extern "C" fn flush(fs: Fs, context: Ctx, info: *mut FileInfo) -> NTSTATUS {
    guard(|| unsafe {
        // 空指针是刷整个盘：没有要做的
        if !context.is_null() {
            *info = mount(fs).commit(node(context))?;
        }
        Ok(())
    })
}

unsafe extern "C" fn get_file_info(fs: Fs, context: Ctx, info: *mut FileInfo) -> NTSTATUS {
    guard(|| unsafe {
        *info = mount(fs).info(&lock(&node(context).state).stat);
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn set_basic_info(
    fs: Fs,
    context: Ctx,
    _attributes: u32,
    _creation: u64,
    _access: u64,
    write: u64,
    _change: u64,
    info: *mut FileInfo,
) -> NTSTATUS {
    guard(|| unsafe {
        *info = mount(fs).set_times(node(context), write)?;
        Ok(())
    })
}

unsafe extern "C" fn set_file_size(
    fs: Fs,
    context: Ctx,
    size: u64,
    allocation: u8,
    info: *mut FileInfo,
) -> NTSTATUS {
    guard(|| unsafe {
        *info = mount(fs).resize(node(context), size, allocation != 0)?;
        Ok(())
    })
}

unsafe extern "C" fn can_delete(fs: Fs, context: Ctx, _name: PWSTR) -> NTSTATUS {
    guard(|| unsafe { mount(fs).can_delete(node(context)) })
}

unsafe extern "C" fn rename(
    fs: Fs,
    context: Ctx,
    _name: PWSTR,
    new_name: PWSTR,
    replace: u8,
) -> NTSTATUS {
    guard(|| unsafe { mount(fs).rename(node(context), &remote_path(new_name), replace != 0) })
}

unsafe extern "C" fn get_security(
    fs: Fs,
    _context: Ctx,
    descriptor: *mut c_void,
    size: *mut usize,
) -> NTSTATUS {
    guard(|| unsafe { mount(fs).copy_security(descriptor, size) })
}

unsafe extern "C" fn read_directory(
    fs: Fs,
    context: Ctx,
    _pattern: PWSTR,
    marker: PWSTR,
    buffer: *mut c_void,
    length: u32,
    transferred: *mut u32,
) -> NTSTATUS {
    guard(|| unsafe {
        let marker = (!marker.is_null()).then(|| wide_slice(marker));
        mount(fs).read_directory(node(context), marker, buffer, length, transferred)
    })
}

pub(crate) static INTERFACE: Interface = Interface {
    GetVolumeInfo: Some(get_volume_info),
    SetVolumeLabel: 0,
    GetSecurityByName: Some(get_security_by_name),
    Create: Some(create),
    Open: Some(open),
    Overwrite: Some(overwrite),
    Cleanup: Some(cleanup),
    Close: Some(close),
    Read: Some(read),
    Write: Some(write),
    Flush: Some(flush),
    GetFileInfo: Some(get_file_info),
    SetBasicInfo: Some(set_basic_info),
    SetFileSize: Some(set_file_size),
    CanDelete: Some(can_delete),
    Rename: Some(rename),
    GetSecurity: Some(get_security),
    SetSecurity: 0,
    ReadDirectory: Some(read_directory),
    Rest: [0; 45],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 系统错误码换成状态码() {
        let linux = |text: &str| status_of(text, false);
        let windows = |text: &str| status_of(text, true);
        assert_eq!(
            linux("无法写入 /x: Permission denied (os error 13)"),
            STATUS_ACCESS_DENIED
        );
        assert_eq!(
            linux("x: No space left on device (os error 28)"),
            STATUS_DISK_FULL
        );
        assert_eq!(
            linux("x 已存在: File exists (os error 17)"),
            STATUS_OBJECT_NAME_COLLISION
        );
        assert_eq!(linux("x (os error 39)"), STATUS_DIRECTORY_NOT_EMPTY);
        // 同一个数在两边不是一回事
        assert_eq!(windows("x (os error 39)"), STATUS_DISK_FULL);
        assert_eq!(windows("x (os error 5)"), STATUS_ACCESS_DENIED);
        assert_eq!(linux("x (os error 5)"), STATUS_UNEXPECTED_IO_ERROR);
        assert_eq!(windows("x (os error 145)"), STATUS_DIRECTORY_NOT_EMPTY);
        assert_eq!(linux("连接已关闭"), STATUS_UNEXPECTED_IO_ERROR);
    }

    #[test]
    fn 哪些名字在_windows_上显示不了() {
        for good in ["a.txt", "中文 名字.md", ".git", "a b", "x-1_2"] {
            assert!(representable(good), "{good}");
        }
        for bad in [
            "a:b",
            "what?.txt",
            "a*",
            "tab\there",
            "dot.",
            "space ",
            "",
            "a|b",
            "q\"q",
        ] {
            assert!(!representable(bad), "{bad}");
        }
    }

    #[test]
    fn 写入删除失败时说本机路径和原因() {
        let message = "无法写入 /data/x/a.txt: Permission denied (os error 13)";
        assert_eq!(
            failure("K:", "src/a.txt", STATUS_ACCESS_DENIED, message, false),
            "无法写入 K:\\src\\a.txt：没有写入权限"
        );
        assert_eq!(
            failure(r"D:\map\logs\", "a.txt", STATUS_DISK_FULL, message, false),
            "无法写入 D:\\map\\logs\\a.txt：服务器磁盘空间不足"
        );
        // 认不出来的原因给原话
        assert_eq!(
            failure(
                "K:",
                "a.txt",
                STATUS_UNEXPECTED_IO_ERROR,
                "连接已关闭",
                false
            ),
            "无法写入 K:\\a.txt：连接已关闭"
        );
        assert_eq!(
            failure(
                "N:",
                "rootdir/keep.txt",
                STATUS_ACCESS_DENIED,
                message,
                true
            ),
            "无法删除 N:\\rootdir\\keep.txt：没有删除权限"
        );
        assert_eq!(
            failure("N:", "a.txt", STATUS_CONNECTION_DISCONNECTED, "", true),
            "无法删除 N:\\a.txt：与服务器的连接已断开"
        );
    }

    #[test]
    fn 时间的换算() {
        // 2023-11-14 22:13:20 UTC
        assert_eq!(filetime(1_700_000_000_000), 133_444_736_000_000_000);
        assert_eq!(
            (133_444_736_000_000_000 - EPOCH_GAP) / 10_000,
            1_700_000_000_000
        );
    }
}
