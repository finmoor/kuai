//! 盘上的读写怎么变成 kuaid 的 `fs_*` 请求。
//!
//! - 读：按格（64 KiB）向服务器要，要多少拉多少：只看开头的（资源管理器认文件类型、取缩略图）
//!   不会多拉。连着往后读时把后面一段也要了，读得越多要得越多，一个请求最多 1 MiB。在路上的读请求
//!   合起来大约 0.3 秒能传完（线路速度边用边量，见 [`Pace`]）：连接是一根管子，先发的先回，在路上的
//!   多了，列目录、别的文件、终端的输出都得排在后面。等回音时不锁着文件，同一个文件别的读、查属性、
//!   再打开都不用等；回音由一个线程收（[`collect`]），放进那个文件的缓存，再叫醒等着的读。每个打开
//!   的文件最多留 16 MiB。
//! - 写：直接按偏移写到服务器上（`fs_pwrite`），不等回音就告诉系统写好了，同一个文件在路上的写入
//!   最多 8 MiB，满了才等。写坏了（磁盘满、没权限）在下一次写、Flush、关文件时报出来，同时经
//!   `on_error` 告诉界面。和在路上的写入重叠的写、读、截断、改名都先等它们落地：服务器上每个请求
//!   各在各的线程里做，不等的话先后会乱。
//! - 删除：系统在最后一个句柄关掉时（Cleanup）才真的删，那一步报不了错。服务器不让删（没权限）时
//!   经 `on_error` 告诉界面，文件照旧留着。
//! - 列目录、查文件属性的结果留 2 秒：资源管理器会反复问同一批东西，还会试探一堆不存在的名字
//!   （`desktop.ini` 这类），都从留着的那份目录里答，不再问服务器。
//! - 等回音有期限（[`REPLY_TIMEOUT`]）：主机上的文件操作卡住时（SSH 照样通着，比如挂死的网络盘），
//!   请求报超时，资源管理器、卸载不跟着一直等。写入按「上一次有写入落地以后」算，线路慢、排着队的
//!   不算卡住。
//!
//! 同一个文件被打开几次，共用一个 [`Node`]。

use std::collections::HashMap;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::task::Poll;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use fr_channel::Channel;
use fr_core::i18n::tr;
use fr_core::protocol::{FsEntry, FsSpace, FsStat};
use fr_core::tf;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::executor::block_on;
use futures::future::{BoxFuture, poll_fn};
use futures::stream::{FuturesUnordered, StreamExt};

use crate::winfsp::*;

/// 缓存和向服务器要数据都按格算，一格这么多字节。
const UNIT: u64 = 64 << 10;
/// 一个读请求最多要这么多格（1 MiB）。
const FETCH_UNITS: u64 = 16;
/// 每个打开的文件最多留这么多格（16 MiB）。
const CACHE_UNITS: usize = 256;
/// 在路上的读请求合起来大约这么多秒能传完。
const AHEAD_SECS: f64 = 0.3;
/// 在路上的读请求合起来最少、最多这么多字节。
const AHEAD_MIN: u64 = 2 * UNIT;
const AHEAD_MAX: u64 = 8 << 20;
/// 线路还没量过时，当它每秒能传这么多：往慢了猜，读回来一两次就准了。
const FIRST_RATE: f64 = 256.0 * 1024.0;
/// 量出来的速度不往上记了：在路上的已经到了上限，再快也一样；记得太高，线路变慢时要好久才降下来。
const MAX_RATE: f64 = 2.0 * AHEAD_MAX as f64 / AHEAD_SECS;
/// 一个文件同时认几路连着往后读的：几个程序、几个线程可能在读同一个文件的不同地方。
const STREAMS: usize = 4;
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
/// 主机多久不回音就不等了：一直等的话 WinFsp 的调度线程卡住，资源管理器跟着卡，卸载、退出也等不完。
/// 前面排着大批写入的慢线路上也够用。
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);
/// 一次读等数据时文件变了，最多从头再读几遍。
const RESTARTS: u32 = 2;

/// Windows 自己会往盘的根上建的目录。
const SYSTEM_DIRS: [&str; 2] = ["$RECYCLE.BIN", "System Volume Information"];

type Nt<T> = std::result::Result<T, NTSTATUS>;
/// 留着的一层目录：什么时候列的、里面有什么。
type Listed = (Instant, Arc<Vec<FsEntry>>);
/// 列目录时的一行：名字（UTF-16，排序和翻页用）和它的情况。
type Row = (Vec<u16>, FsEntry);
type Reading = BoxFuture<'static, Result<(FsStat, Vec<u8>)>>;
/// WinFsp 的 `FspFileSystemAddDirInfo`：列目录时往系统给的缓冲区里加一行。
pub(crate) type AddDirInfo = unsafe extern "C" fn(*mut DirInfo, *mut c_void, u32, *mut u32) -> u8;

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
    /// 发出去的时刻
    sent: Instant,
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
    add_dir_info: AddDirInfo,
    /// 挂载的时刻：服务器没给修改时间的（根目录）用它
    started: u64,
    dirs: Mutex<HashMap<String, Listed>>,
    stats: Mutex<HashMap<String, (Instant, FsStat)>>,
    nodes: Mutex<HashMap<String, Weak<Node>>>,
    space: Mutex<Option<(Instant, FsSpace)>>,
    pending: Mutex<mpsc::Sender<Pending>>,
    /// 发出去的读请求交给收回音的线程（[`collect`]）
    fetches: Mutex<UnboundedSender<Fetch>>,
    next_fetch: AtomicU64,
    pace: Arc<Mutex<Pace>>,
    sink: Arc<Sink>,
    /// 打开着的文件（不算目录）有几个：退出前拿来问「还有文件开着」
    pub(crate) open_files: AtomicUsize,
    pub(crate) counters: Arc<Counters>,
}

/// 一个打开着的文件或目录。
pub(crate) struct Node {
    state: Mutex<NodeState>,
    /// 在路上的写入有回音了
    landed: Condvar,
    /// 读请求有回音了，或者缓存换了代：等数据的读再看一眼
    arrived: Condvar,
}

struct NodeState {
    /// 相对映射目录的路径，`/` 分隔，根目录是空字符串
    path: String,
    stat: FsStat,
    /// 读回来的、正在要的数据
    cache: Cache,
    /// 在路上的写入：偏移和长度
    flying: Vec<(u64, u64)>,
    flying_bytes: u64,
    /// 有一次写入没成，还没报给调用的程序
    failed: Option<NTSTATUS>,
    /// 有一次写入没成：这边记的大小、时间不准了，等在路上的都有了回音以后重新问
    resync: bool,
    /// 写过：关的时候让留着的目录作废
    dirty: bool,
    /// 上次刷盘（[`Mount::flush_file`]）以后写过、改过大小或时间
    unsynced: bool,
    /// 这次列目录用的那一份，按名字排好；翻页时要前后一致
    listing: Option<Arc<Vec<Row>>>,
}

impl NodeState {
    /// 一个读请求带着数据回来了。文件这期间变了（这边写了、截断了）就不要；服务器上的大小、时间
    /// 和这边记的不一样（别人改过），之前留着的、在路上的都不要了，用这一次的。
    fn arrived(&mut self, asked: &Asked, stat: FsStat, bytes: Vec<u8>) {
        if asked.generation != self.cache.generation {
            return;
        }
        if stat.mtime != self.stat.mtime || stat.size != self.stat.size {
            self.cache.renew();
            self.stat = stat;
        }
        self.cache.fill(asked.first, asked.count, bytes);
    }
}

/// 一个打开的文件读回来的数据，按格（[`UNIT`]）存。
#[derive(Default)]
struct Cache {
    units: HashMap<u64, Unit>,
    /// 第几代：文件变了（这边写了、截断了，别处改了）就换一代，之前发出去的读回来了也不要
    generation: u64,
    /// 连着往后读的几路
    streams: Vec<Stream>,
    /// 越来越大的数：记每格、每路最后用到的先后
    tick: u64,
}

enum Unit {
    /// 正在向服务器要：第几个读请求
    Wanted(u64),
    /// 读回来了，是 `bytes[at..at + len]`：文件末尾那格不满，过了末尾的是空的
    Ready {
        bytes: Arc<Vec<u8>>,
        at: usize,
        len: usize,
        used: u64,
    },
    /// 要的时候出错了：等着这个请求的读都报这个错，之后的读重新要
    Failed(u64, NTSTATUS),
}

/// 一路连着往后读的。
#[derive(Clone, Copy, Debug, PartialEq)]
struct Stream {
    /// 下一次接着读该从哪开始
    next: u64,
    /// 这一路已经读了多少字节
    run: u64,
    /// 接着读了几次，头一次不算
    hits: u32,
    used: u64,
}

impl Cache {
    /// 换一代：留着的都不要了，在路上的回来了也不要。
    fn renew(&mut self) {
        self.generation += 1;
        self.units.clear();
    }

    /// 这一格既没读回来，也没在要。
    fn missing(&self, unit: u64) -> bool {
        !matches!(
            self.units.get(&unit),
            Some(Unit::Wanted(_) | Unit::Ready { .. })
        )
    }

    /// 这次读的 `offset..end` 是哪一路接着往后读的：从上次停下的地方（前后差一两格以内，系统的
    /// 预读会稍微打乱先后）往后读。接不上的另开一路，顶掉最久没用的那一路。
    fn follow(&mut self, offset: u64, end: u64) -> Stream {
        self.tick += 1;
        let tick = self.tick;
        let near = |stream: &Stream| {
            end > stream.next && offset + UNIT >= stream.next && offset <= stream.next + 2 * UNIT
        };
        if let Some(stream) = self.streams.iter_mut().find(|stream| near(stream)) {
            stream.run += end - offset;
            stream.next = end;
            stream.hits += 1;
            stream.used = tick;
            return *stream;
        }
        let stream = Stream {
            next: end,
            run: end - offset,
            hits: 0,
            used: tick,
        };
        match self.streams.len() < STREAMS {
            true => self.streams.push(stream),
            false => {
                if let Some(oldest) = self.streams.iter_mut().min_by_key(|stream| stream.used) {
                    *oldest = stream;
                }
            }
        }
        stream
    }

    /// 从第 `first` 格起的 `count` 格读回来了。
    fn fill(&mut self, first: u64, count: u64, bytes: Vec<u8>) {
        self.tick += 1;
        let (bytes, used) = (Arc::new(bytes), self.tick);
        for index in 0..count {
            let at = ((index * UNIT) as usize).min(bytes.len());
            let len = (bytes.len() - at).min(UNIT as usize);
            let unit = Unit::Ready {
                bytes: bytes.clone(),
                at,
                len,
                used,
            };
            self.units.insert(first + index, unit);
        }
        self.trim();
    }

    /// 这个请求出错了：还在等它的格记下错误。
    fn fail(&mut self, asked: &Asked, status: NTSTATUS) {
        for unit in asked.first..asked.first + asked.count {
            if let Some(slot) = self.units.get_mut(&unit)
                && matches!(slot, Unit::Wanted(id) if *id == asked.id)
            {
                *slot = Unit::Failed(asked.id, status);
            }
        }
    }

    /// 第 `id` 个读请求不等了（主机一直不回）：在等它的格记下错误，之后的读重新要。它晚到了
    /// 照样放进来（见 [`NodeState::arrived`]）。
    fn give_up(&mut self, id: u64, status: NTSTATUS) {
        for slot in self.units.values_mut() {
            if matches!(slot, Unit::Wanted(wanted) if *wanted == id) {
                *slot = Unit::Failed(id, status);
            }
        }
    }

    /// 超过上限时扔掉最久没用的（在要的不扔）。
    fn trim(&mut self) {
        let extra = self.units.len().saturating_sub(CACHE_UNITS);
        if extra == 0 {
            return;
        }
        let mut done: Vec<(u64, u64)> = self
            .units
            .iter()
            .filter_map(|(unit, slot)| match slot {
                Unit::Ready { used, .. } => Some((*used, *unit)),
                Unit::Failed(..) => Some((0, *unit)),
                Unit::Wanted(_) => None,
            })
            .collect();
        done.sort_unstable();
        for (_, unit) in done.into_iter().take(extra) {
            self.units.remove(&unit);
        }
    }
}

/// 线路有多快，现在有多少读请求在路上。一个盘一个。
///
/// 连接是一根管子，先发的先回：在路上的读请求合起来不超过 [`AHEAD_SECS`] 秒能传完的量，别的请求
/// 就最多排这么久。速度是边读边量的：读请求回来的字节数除以它在管子里占的时间。
struct Pace {
    /// 每秒多少字节，近几次的加权平均
    rate: f64,
    /// 量准过（不是猜的）
    measured: bool,
    /// 上一个读请求回来的时刻
    last: Option<Instant>,
    /// 在路上的读请求一共要了多少字节
    flying: u64,
}

impl Pace {
    fn new() -> Pace {
        Pace {
            rate: FIRST_RATE,
            measured: false,
            last: None,
            flying: 0,
        }
    }

    /// 换了连接：速度重新量。
    fn reset(&mut self) {
        self.rate = FIRST_RATE;
        self.measured = false;
        self.last = None;
    }

    /// 在路上的读请求最多合起来这么多字节。
    fn budget(&self) -> u64 {
        ((self.rate * AHEAD_SECS) as u64).clamp(AHEAD_MIN, AHEAD_MAX)
    }

    /// 要了 `wanted` 字节的一个读请求在 `at` 回来了，带回 `got` 字节。回音一个接一个地到：发出时
    /// 前一个还没回来的，前一个到了它才开始传，从那时算起，量得准（头一次量准的直接用，之后取
    /// 平均）；线路本来空着的，从发出时算起，多算了一个往返，量出来只会比真的慢，只拿来往上调。
    fn landed(&mut self, wanted: u64, got: usize, issued: Instant, at: Instant) {
        self.flying = self.flying.saturating_sub(wanted);
        let queued = self.last.filter(|last| *last > issued);
        self.last = Some(at);
        if got == 0 {
            return;
        }
        let start = queued.unwrap_or(issued);
        let secs = at.saturating_duration_since(start).as_secs_f64().max(1e-4);
        let sample = got as f64 / secs;
        let rate = match queued {
            Some(_) if !self.measured => sample,
            Some(_) => self.rate + (sample - self.rate) * 0.5,
            None => self.rate.max(sample),
        };
        self.measured |= queued.is_some();
        self.rate = rate.min(MAX_RATE);
    }
}

/// 在路上的上限是 `budget` 时，一个读请求最多要几格：上限的四分之一，线路慢时请求小，别的请求
/// 插得进来；在路上同时有好几个，前一个到了后一个还在路上。
fn fetch_units(budget: u64) -> u64 {
    (budget / 4 / UNIT).clamp(1, FETCH_UNITS)
}

/// 发出去的一个读请求：从第 `first` 格起要 `count` 格。
struct Asked {
    id: u64,
    node: Arc<Node>,
    /// 发出时那个文件的缓存是第几代：回来时换了代就不要了
    generation: u64,
    first: u64,
    count: u64,
    issued: Instant,
}

struct Fetch {
    asked: Asked,
    reading: Reading,
}

/// 收读请求回音的线程：哪个先回来先放进那个文件的缓存，再叫醒等着的读。盘卸载了（发请求的
/// 那一头没了）就退出，没回来的不要了。
fn collect(
    mut incoming: UnboundedReceiver<Fetch>,
    pace: Arc<Mutex<Pace>>,
    counters: Arc<Counters>,
    windows: bool,
) {
    let mut running = FuturesUnordered::new();
    let mut closed = false;
    block_on(poll_fn(|cx| {
        while !closed {
            match incoming.poll_next_unpin(cx) {
                Poll::Ready(Some(Fetch { asked, reading })) => {
                    running.push(async move { (asked, reading.await, Instant::now()) })
                }
                Poll::Ready(None) => closed = true,
                Poll::Pending => break,
            }
        }
        while let Poll::Ready(Some((asked, result, at))) = running.poll_next_unpin(cx) {
            let landing = || land(&asked, result, at, &pace, &counters, windows);
            // 出了意外这个线程也不能没了，不然等数据的读会一直等下去：让等这个请求的读报错
            if catch_unwind(AssertUnwindSafe(landing)).is_err() {
                lock(&asked.node.state)
                    .cache
                    .fail(&asked, STATUS_UNEXPECTED_IO_ERROR);
                asked.node.arrived.notify_all();
            }
        }
        match closed {
            true => Poll::Ready(()),
            false => Poll::Pending,
        }
    }));
}

/// 一个读请求回来了：放进那个文件的缓存，叫醒等着的读。
fn land(
    asked: &Asked,
    result: Result<(FsStat, Vec<u8>)>,
    at: Instant,
    pace: &Mutex<Pace>,
    counters: &Counters,
    windows: bool,
) {
    let got = result.as_ref().map_or(0, |(_, bytes)| bytes.len());
    lock(pace).landed(asked.count * UNIT, got, asked.issued, at);
    counters.down.fetch_add(got as u64, Relaxed);
    let mut state = lock(&asked.node.state);
    match result {
        Ok((stat, bytes)) => state.arrived(asked, stat, bytes),
        Err(err) => {
            let message = format!("{err:#}");
            let status = status_of(&message, windows);
            if status != STATUS_OBJECT_NAME_NOT_FOUND {
                log::warn!("fs_read /{} 失败：{message}", state.path);
            }
            state.cache.fail(asked, status);
        }
    }
    drop(state);
    asked.node.arrived.notify_all();
}

/// 等 `future` 好，最多等到 `deadline`；到时没好返回 None，`future` 就此放下（回音晚到了也没人要）。
fn block_until<F: Future>(future: F, deadline: Instant) -> Option<F::Output> {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return Some(output);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        // 醒早了（别处叫醒了这个线程）也没关系，再看一眼
        std::thread::park_timeout(left);
    }
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
        // 原来一律可写：同一个只读文件列目录时没有只读属性、看属性时又是只读
        readonly: entry.readonly,
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

/// 等写入回音的线程：请求发出去的先后就是这里等的先后。主机一直不回音的（文件操作卡住了）
/// 算没写成，不然等它的写、Flush、卸载都跟着一直等；主机还在回音的（线路慢、前面排着队）接着等。
fn settle(incoming: mpsc::Receiver<Pending>, sink: Arc<Sink>) {
    // 上一次有写入回音的时刻
    let mut progress = Instant::now();
    while let Ok(pending) = incoming.recv() {
        let deadline = pending.sent.max(progress) + REPLY_TIMEOUT;
        let result = match block_until(pending.writing, deadline) {
            Some(result) => {
                progress = Instant::now();
                result.map_err(|err| {
                    let message = format!("{err:#}");
                    (status_of(&message, sink.windows), message)
                })
            }
            None => Err((STATUS_IO_TIMEOUT, tr("主机长时间没有回应").to_string())),
        };
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
            Err((status, message)) => {
                log::warn!("写入 /{} 失败：{message}", state.path);
                state.resync = true;
                state.cache.renew();
                // 只记第一个错：后面的多半是它引出来的
                if state.failed.is_none() {
                    state.failed = Some(status);
                    let text = failure(&sink.place, &state.path, status, &message, false);
                    (sink.on_error)(text);
                }
            }
        }
        drop(state);
        pending.node.landed.notify_all();
        pending.node.arrived.notify_all();
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
        STATUS_DISK_FULL => tr("主机磁盘空间不足"),
        STATUS_ACCESS_DENIED if deleting => tr("没有删除权限"),
        STATUS_ACCESS_DENIED => tr("没有写入权限"),
        STATUS_MEDIA_WRITE_PROTECTED => tr("主机上的目录是只读的"),
        STATUS_OBJECT_NAME_NOT_FOUND => tr("文件已被删除或移走"),
        STATUS_CONNECTION_DISCONNECTED => tr("与主机的连接已断开"),
        STATUS_IO_TIMEOUT => tr("主机长时间没有回应"),
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
        add_dir_info: AddDirInfo,
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
        let (fetches, incoming) = unbounded();
        let pace = Arc::new(Mutex::new(Pace::new()));
        let counters = Arc::new(Counters::default());
        let collecting = (pace.clone(), counters.clone(), settings.windows);
        std::thread::Builder::new()
            .name("mount-fetch".into())
            .spawn(move || collect(incoming, collecting.0, collecting.1, collecting.2))?;
        Ok(Mount {
            channel: Mutex::new(Some(channel)),
            root: settings.root,
            read_only: settings.read_only,
            windows: settings.windows,
            label: settings.label.encode_utf16().take(32).collect(),
            security: open_security_descriptor()?,
            add_dir_info,
            started: filetime(now_millis()),
            dirs: Mutex::default(),
            stats: Mutex::default(),
            nodes: Mutex::default(),
            space: Mutex::default(),
            pending: Mutex::new(tx),
            fetches: Mutex::new(fetches),
            next_fetch: AtomicU64::new(0),
            pace,
            sink,
            open_files: AtomicUsize::new(0),
            counters,
        })
    }

    /// 换一条连接（重连上了），或者拿掉（断了）。留着的目录、属性都不要了；新的连接可能走的是
    /// 别的路（换了网络），线路速度重新量。
    pub(crate) fn set_channel(&self, channel: Option<Channel>) {
        *lock(&self.channel) = channel;
        lock(&self.dirs).clear();
        lock(&self.stats).clear();
        lock(&self.pace).reset();
    }

    fn channel(&self) -> Nt<Channel> {
        match lock(&self.channel).clone() {
            // 连接断了：等上面换一条新的（见 `set_channel`）
            Some(Channel::Remote(conn)) if !conn.is_alive() => Err(STATUS_CONNECTION_DISCONNECTED),
            Some(channel) => Ok(channel),
            None => Err(STATUS_CONNECTION_DISCONNECTED),
        }
    }

    /// 等服务器的回音，最多等 [`REPLY_TIMEOUT`]。
    fn wait<T>(&self, what: &str, path: &str, future: BoxFuture<'static, Result<T>>) -> Nt<T> {
        self.counters.calls.fetch_add(1, Relaxed);
        let Some(result) = block_until(future, Instant::now() + REPLY_TIMEOUT) else {
            log::warn!("{what} /{path} 超过 {} 秒没有回音", REPLY_TIMEOUT.as_secs());
            return Err(STATUS_IO_TIMEOUT);
        };
        result.map_err(|err| {
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
                            cache: Cache::default(),
                            flying: Vec::new(),
                            flying_bytes: 0,
                            failed: None,
                            resync: false,
                            dirty: false,
                            unsynced: false,
                            listing: None,
                        }),
                        landed: Condvar::new(),
                        arrived: Condvar::new(),
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
                state.cache.renew();
                drop(state);
                node.arrived.notify_all();
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

    /// 把第 `from` 到 `to` 格里还没有的向服务器要。`must` 为真时 `from` 那格不管线路忙不忙都要
    /// （有读在等它），别的格线路有余量才要。在路上的写入先等它们落地，免得读到写之前的。
    fn request<'a>(
        &self,
        node: &'a Arc<Node>,
        mut state: MutexGuard<'a, NodeState>,
        from: u64,
        to: u64,
        must: bool,
    ) -> Nt<MutexGuard<'a, NodeState>> {
        if !(from..=to).any(|unit| state.cache.missing(unit)) {
            return Ok(state);
        }
        if !state.flying.is_empty() {
            state = self.drain(node, state);
        }
        let channel = self.channel()?;
        let budget = lock(&self.pace).budget();
        let most = fetch_units(budget);
        let mut unit = from;
        while unit <= to {
            if !state.cache.missing(unit) {
                unit += 1;
                continue;
            }
            let mut count = 1;
            while count < most && unit + count <= to && state.cache.missing(unit + count) {
                count += 1;
            }
            {
                let mut pace = lock(&self.pace);
                if pace.flying + count * UNIT > budget && !(must && unit == from) {
                    break;
                }
                pace.flying += count * UNIT;
            }
            self.issue(&channel, node, &mut state, unit, count)?;
            unit += count;
        }
        Ok(state)
    }

    /// 发一个读请求：从第 `first` 格起要 `count` 格。回音由收回音的线程放进缓存。
    fn issue(
        &self,
        channel: &Channel,
        node: &Arc<Node>,
        state: &mut NodeState,
        first: u64,
        count: u64,
    ) -> Nt<()> {
        let id = self.next_fetch.fetch_add(1, Relaxed);
        let reading = channel.fs_read(&self.root, &state.path, first * UNIT, count * UNIT, None);
        self.counters.calls.fetch_add(1, Relaxed);
        let asked = Asked {
            id,
            node: node.clone(),
            generation: state.cache.generation,
            first,
            count,
            issued: Instant::now(),
        };
        if lock(&self.fetches)
            .unbounded_send(Fetch { asked, reading })
            .is_err()
        {
            // 收回音的线程没了：这个盘正在卸载
            let mut pace = lock(&self.pace);
            pace.flying = pace.flying.saturating_sub(count * UNIT);
            return Err(STATUS_CONNECTION_DISCONNECTED);
        }
        // 收回音的线程要拿着这个文件的锁才放得进去：它这时还看不到这些格
        for unit in first..first + count {
            state.cache.units.insert(unit, Unit::Wanted(id));
        }
        Ok(())
    }

    /// 连着往后读的：把后面一段也要了，读到那里时多半已经回来。这一段是这一路已经读了的两倍
    /// （越读越多，延迟高的线路上也很快就能填满），线路有余量才要；在路上的写入还没落地时不要。
    /// 空出一个整请求（这一段还短时是半段）的量才要：一格一格地要，请求太碎，快的线路上反而慢。
    /// 一个请求只占这一段的几分之一，后一个请求在路上时前一个就到了，线路不会空着等。
    fn prefetch<'a>(
        &self,
        node: &'a Arc<Node>,
        state: MutexGuard<'a, NodeState>,
        stream: Stream,
    ) -> MutexGuard<'a, NodeState> {
        let size = state.stat.size;
        if stream.next >= size || !state.flying.is_empty() {
            return state;
        }
        let budget = lock(&self.pace).budget();
        let window = (2 * stream.run).clamp(AHEAD_MIN, budget);
        let end = (stream.next + window).min(size);
        let (from, to) = (stream.next / UNIT, (end - 1) / UNIT);
        let missing = (from..=to)
            .filter(|unit| state.cache.missing(*unit))
            .count() as u64;
        let need = fetch_units(budget).min((to + 1 - from).div_ceil(2));
        // 到了文件末尾，剩下的不够也要
        if missing == 0 || (missing < need && end < size) {
            return state;
        }
        // 要不到（断线了）也不碍事：读到那里时再要
        match self.request(node, state, from, to, false) {
            Ok(state) => state,
            Err(_) => lock(&node.state),
        }
    }

    fn read(&self, node: &Arc<Node>, buffer: &mut [u8], offset: u64) -> Nt<usize> {
        let mut state = self.drain(node, lock(&node.state));
        // 是哪一路连着往后读的：头一遍认，从头再读时不再算一次
        let mut followed = None;
        let mut restarts = 0;
        'again: loop {
            // 这次读到的都要是同一代的。等数据时不锁着文件，这期间文件可能变了（这边写了、截断了，
            // 或者读回来的数据说主机上的变了）：已经拷出去的是旧的，从头再读，不然前半是旧的、
            // 后半是新的
            let generation = state.cache.generation;
            if offset >= state.stat.size {
                return Err(STATUS_END_OF_FILE);
            }
            let end = (offset + buffer.len() as u64).min(state.stat.size);
            let stream = *followed.get_or_insert_with(|| state.cache.follow(offset, end));
            let last = end.saturating_sub(1) / UNIT;
            // 先要这次要读的，紧接着要后面的（连着往后读的），再等：后面的不用等这次的到了才发。
            // 先发的先回，这次要的不会被后面的挡住
            state = self.request(node, state, offset / UNIT, last, true)?;
            if stream.hits > 0 {
                state = self.prefetch(node, state, stream);
            }
            let mut done = 0usize;
            // 在等的那个读请求、从什么时候等起：它出了错，这次读就报这个错
            let mut waiting: Option<(u64, Instant)> = None;
            while offset + (done as u64) < end {
                let at = offset + done as u64;
                let unit = at / UNIT;
                if let Some(Unit::Failed(id, status)) = state.cache.units.get(&unit)
                    && waiting.is_some_and(|(waited, _)| waited == *id)
                {
                    return Err(*status);
                }
                // 这一格和后面还要的那些，没有的都要了（后面的看线路有没有余量）。别的读要的时候
                // 出了错的、文件变了不在了的，也重新要
                state = self.request(node, state, unit, last, true)?;
                // 一直在变的（一边读一边有人往里写）从头读过两遍就不再从头来了：和本机磁盘上一边
                // 写一边读一样，读到的可能新旧都有
                if state.cache.generation != generation && restarts < RESTARTS {
                    restarts += 1;
                    continue 'again;
                }
                state.cache.tick += 1;
                let tick = state.cache.tick;
                if let Some(Unit::Ready { used, .. }) = state.cache.units.get_mut(&unit) {
                    *used = tick;
                }
                match state.cache.units.get(&unit) {
                    Some(Unit::Ready {
                        bytes,
                        at: start,
                        len,
                        ..
                    }) => {
                        let within = (at - unit * UNIT) as usize;
                        // 文件变短了
                        if within >= *len {
                            break;
                        }
                        let count = (*len - within).min((end - at) as usize);
                        let from = start + within;
                        buffer[done..done + count].copy_from_slice(&bytes[from..from + count]);
                        done += count;
                        waiting = None;
                    }
                    // 等它回来（在等的请求出了错，下一圈报出来）。主机一直不回就不等了：这个请求
                    // 要的格记成超时，这次读报错，之后的读重新要
                    Some(Unit::Wanted(id)) => {
                        let id = *id;
                        let since = match waiting {
                            Some((waited, since)) if waited == id => since,
                            _ => Instant::now(),
                        };
                        waiting = Some((id, since));
                        let left = REPLY_TIMEOUT.saturating_sub(since.elapsed());
                        if left.is_zero() {
                            let secs = REPLY_TIMEOUT.as_secs();
                            log::warn!("fs_read /{} 超过 {secs} 秒没有回音", state.path);
                            state.cache.give_up(id, STATUS_IO_TIMEOUT);
                            drop(state);
                            node.arrived.notify_all();
                            return Err(STATUS_IO_TIMEOUT);
                        }
                        state = node
                            .arrived
                            .wait_timeout(state, left)
                            .unwrap_or_else(PoisonError::into_inner)
                            .0;
                    }
                    // 刚要过，到不了这里；真到了，下一圈再要
                    _ => waiting = None,
                }
            }
            // 读的这段时间里线路可能量得更快了，能多要一些
            if stream.hits > 0 {
                drop(self.prefetch(node, state, stream));
            }
            return match done {
                0 => Err(STATUS_END_OF_FILE),
                done => Ok(done),
            };
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
        // 在路上的太多、或者和在路上的重叠：等它们落地。服务器上每个请求各在各的线程里做，
        // 重叠的两次写不等的话先后会乱。写到哪、写多少每次醒来重新算：等的时候锁是放开的，别的
        // 写可能已经把文件写长了，追加的还按原来的末尾写，就把人家刚写的盖掉了
        let (offset, len) = loop {
            let offset = if to_end { state.stat.size } else { offset };
            let mut len = data.len() as u64;
            // 系统把缓存里的页写回来时不能把文件写长
            if constrained {
                len = len.min(state.stat.size.saturating_sub(offset));
            }
            if len == 0 {
                return Ok((0, self.info(&state.stat)));
            }
            let overlaps = state
                .flying
                .iter()
                .any(|(at, size)| offset < at + size && *at < offset + len);
            let full = state.flying_bytes + len > WINDOW && !state.flying.is_empty();
            if !overlaps && !full {
                break (offset, len);
            }
            state = node
                .landed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(status) = state.failed.take() {
                return Err(status);
            }
        };
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
                sent: Instant::now(),
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
        state.cache.renew();
        state.dirty = true;
        state.unsynced = true;
        self.remember(&state.path, state.stat);
        node.arrived.notify_all();
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

    /// 程序要求刷盘（`FlushFileBuffers`）：等写入落地，写过的再让主机把文件刷到它的磁盘上，
    /// 刷不成照实报。关文件时不刷（[`Self::commit`]），和本机的文件一样交给系统。
    fn flush_file(&self, node: &Node) -> Nt<FileInfo> {
        let mut state = self.settled(node, lock(&node.state))?;
        if std::mem::take(&mut state.dirty) {
            self.invalidate(&state.path);
            self.remember(&state.path, state.stat);
        }
        if state.unsynced && !state.stat.dir {
            let path = state.path.clone();
            let syncing = self.channel()?.fs_sync(&self.root, &path);
            state.stat = self.wait("fs_sync", &path, syncing)?;
            state.unsynced = false;
            self.remember(&path, state.stat);
        }
        Ok(self.info(&state.stat))
    }

    /// 刷整个盘：开着的文件里写过的都刷一遍，有一个刷不成就报那一个的错。
    fn flush_all(&self) -> Nt<()> {
        let nodes: Vec<Arc<Node>> = lock(&self.nodes)
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        let mut result = Ok(());
        for node in nodes {
            if let Err(status) = self.flush_file(&node) {
                result = Err(status);
            }
        }
        result
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
                let removing = channel.fs_remove_shallow(&self.root, &path);
                match block_until(removing, Instant::now() + REPLY_TIMEOUT) {
                    Some(result) => result.map_err(|err| {
                        let message = format!("{err:#}");
                        (status_of(&message, self.windows), message)
                    }),
                    None => Err((STATUS_IO_TIMEOUT, tr("主机长时间没有回应").to_string())),
                }
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

    /// 要写内容（`granted` 里有写数据、追加）而主机上这个用户写不了这个文件：打开时就拒绝。原来
    /// 打开照样成功，写入在后台失败，程序以为写好了（只在 Kuai 里提示一句）。只改属性、时间的不拦。
    fn check_write(&self, node: &Node, granted: u32) -> Nt<()> {
        let state = lock(&node.state);
        let writing = granted & (FILE_WRITE_DATA | FILE_APPEND_DATA) != 0;
        match writing && !state.stat.dir && state.stat.readonly {
            true => Err(STATUS_ACCESS_DENIED),
            false => Ok(()),
        }
    }

    /// Windows 的规矩：目录里有东西就不能删。问服务器要现在的，Windows 上显示不了的名字也算。
    /// 所在目录主机上的这个用户写不了的，也不能删：系统真正删的时候（Cleanup）已经报不了错，
    /// 程序会以为删掉了。
    fn can_delete(&self, node: &Node) -> Nt<()> {
        self.writable()?;
        // 只看 Linux 主机：那边的只读是按这个用户实际的权限算的。Windows 上目录的只读属性不管删
        // 里面的东西，「文档」「桌面」这些文件夹还都带着它
        if !self.windows {
            let path = lock(&node.state).path.clone();
            let parent = split(&path).0;
            // 映射的根目录 `lookup` 不问主机（当它一直在），这里要真的问一次
            let parent = match parent.is_empty() {
                true => self.wait("fs_stat", "", self.channel()?.fs_stat(&self.root, ""))?,
                false => self.lookup(parent)?,
            };
            if parent.readonly {
                return Err(STATUS_ACCESS_DENIED);
            }
        }
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
        drop(state);
        self.moved(&from, to);
        self.invalidate_tree(&from);
        self.invalidate_tree(to);
        Ok(())
    }

    /// 改了名：打开着的跟着换到新路径上。改的是目录时，里面打开着的也换：主机上它们已经在新目录
    /// 里了，之后的读写要发新路径（原来只换这一个，里面的还发旧路径，读写都报「已被删除或移走」）。
    /// 改了名的那个自己的路径调用前已经换好了。
    fn moved(&self, from: &str, to: &str) {
        let (old, new) = (self.key(from), self.key(to));
        let inside = format!("{old}/");
        let mut children = Vec::new();
        {
            let mut nodes = lock(&self.nodes);
            let keys: Vec<String> = nodes
                .keys()
                .filter(|key| **key == old || key.starts_with(&inside))
                .cloned()
                .collect();
            for key in keys {
                if let Some(weak) = nodes.remove(&key) {
                    if key != old
                        && let Some(node) = weak.upgrade()
                    {
                        children.push(node);
                    }
                    nodes.insert(format!("{new}{}", &key[old.len()..]), weak);
                }
            }
        }
        // 按段换：前面几段就是 `from`（大小写可能不一样）
        let depth = from.split('/').count();
        for node in children {
            let mut state = lock(&node.state);
            let rest: Vec<&str> = state.path.split('/').skip(depth).collect();
            state.path = format!("{to}/{}", rest.join("/"));
        }
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
        state.cache.renew();
        node.arrived.notify_all();
        state.dirty = true;
        state.unsynced = true;
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
        state.unsynced = true;
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
    granted: u32,
    context: *mut Ctx,
    info: *mut FileInfo,
) -> NTSTATUS {
    guard(|| unsafe {
        let mount = mount(fs);
        let node = mount.open(&remote_path(name))?;
        mount.check_write(&node, granted)?;
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
        // 空指针是刷整个盘
        match context.is_null() {
            true => mount(fs).flush_all(),
            false => {
                *info = mount(fs).flush_file(node(context))?;
                Ok(())
            }
        }
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
            "无法写入 D:\\map\\logs\\a.txt：主机磁盘空间不足"
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
            "无法删除 N:\\a.txt：与主机的连接已断开"
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

    /// 当作服务器上映射的那个目录，用完删掉。
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "fr-mount-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn write(&self, name: &str, bytes: &[u8]) {
            std::fs::write(self.0.join(name), bytes).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    unsafe extern "C" fn no_dir_info(_: *mut DirInfo, _: *mut c_void, _: u32, _: *mut u32) -> u8 {
        0
    }

    /// 经本机通道映射这个目录：不经 WinFsp，直接调读写。
    fn local_mount(dir: &TempDir) -> Mount {
        let settings = Settings {
            root: dir.0.to_string_lossy().into_owned(),
            place: "T:".into(),
            read_only: false,
            windows: true,
            label: "test".into(),
        };
        Mount::new(Channel::Local, settings, no_dir_info, Box::new(|_| {})).unwrap()
    }

    /// 一串看着随机、每次都一样的字节。
    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn read_at(mount: &Mount, node: &Arc<Node>, offset: u64, len: usize) -> Vec<u8> {
        let mut buffer = vec![0; len];
        let got = mount.read(node, &mut buffer, offset).unwrap_or(0);
        buffer.truncate(got);
        buffer
    }

    /// 把文件或目录设成只读（或者去掉只读）。
    fn set_readonly(path: &std::path::Path, readonly: bool) {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(readonly);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn 写不了的文件打开来写内容就拒绝_读和改属性不拦() {
        let dir = TempDir::new();
        dir.write("ro.txt", b"keep");
        set_readonly(&dir.0.join("ro.txt"), true);
        let mount = local_mount(&dir);
        let node = mount.open("ro.txt").unwrap();
        for granted in [FILE_WRITE_DATA, FILE_APPEND_DATA, 0x1 | FILE_APPEND_DATA] {
            assert_eq!(mount.check_write(&node, granted), Err(STATUS_ACCESS_DENIED));
        }
        // 读、改属性和时间（0x100）照样打得开
        assert_eq!(mount.check_write(&node, 0x1 | 0x80 | 0x100), Ok(()));
        dir.write("rw.txt", b"x");
        let writable = mount.open("rw.txt").unwrap();
        assert_eq!(mount.check_write(&writable, FILE_WRITE_DATA), Ok(()));
        set_readonly(&dir.0.join("ro.txt"), false);
    }

    #[test]
    fn 删之前看所在目录写不写得了_windows_上目录的只读属性不算() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.0.join("locked")).unwrap();
        dir.write("locked/keep.txt", b"x");
        set_readonly(&dir.0.join("locked"), true);
        // Windows 主机：「文档」这些文件夹都带只读属性，里面的照样能删
        let mount = local_mount(&dir);
        let node = mount.open("locked/keep.txt").unwrap();
        assert_eq!(mount.can_delete(&node), Ok(()));
        // Linux 主机：只读是按这个用户的权限算的，写不了的目录里不让删（不然系统以为删掉了）
        let settings = Settings {
            root: dir.0.to_string_lossy().into_owned(),
            place: "T:".into(),
            read_only: false,
            windows: false,
            label: "test".into(),
        };
        let linux = Mount::new(Channel::Local, settings, no_dir_info, Box::new(|_| {})).unwrap();
        let node = linux.open("locked/keep.txt").unwrap();
        assert_eq!(linux.can_delete(&node), Err(STATUS_ACCESS_DENIED));
        set_readonly(&dir.0.join("locked"), false);
        let node = linux.open("locked/keep.txt").unwrap();
        linux.set_channel(Some(Channel::Local));
        assert_eq!(linux.can_delete(&node), Ok(()));
    }

    #[test]
    fn 只看开头时只拉一格() {
        let dir = TempDir::new();
        let content = noise(3_000_000, 1);
        dir.write("a.bin", &content);
        let mount = local_mount(&dir);
        let node = mount.open("a.bin").unwrap();
        assert_eq!(read_at(&mount, &node, 0, 4096), content[..4096]);
        assert_eq!(mount.counters.down.load(Relaxed), UNIT);
        // 再看一遍开头：不算接着往后读，也不再向服务器要
        assert_eq!(read_at(&mount, &node, 100, 3000), content[100..3100]);
        assert_eq!(mount.counters.down.load(Relaxed), UNIT);
        assert_eq!(lock(&node.state).cache.units.len(), 1);
    }

    #[test]
    fn 连着往后读时提前要后面的_每格只要一次() {
        let dir = TempDir::new();
        let content = noise(5_000_000, 2);
        dir.write("b.bin", &content);
        let mount = local_mount(&dir);
        let node = mount.open("b.bin").unwrap();
        let mut got = Vec::new();
        while got.len() < content.len() {
            let chunk = read_at(&mount, &node, got.len() as u64, 65536);
            assert!(!chunk.is_empty(), "读到 {} 时没有了", got.len());
            got.extend_from_slice(&chunk);
            if got.len() == 3 * 65536 {
                // 读了三次：后面的已经在要了
                assert!(!lock(&node.state).cache.missing(3));
            }
        }
        assert_eq!(got, content);
        // 不拉文件以外的，读过的也不再拉
        assert_eq!(mount.counters.down.load(Relaxed), content.len() as u64);
    }

    #[test]
    fn 几个线程同时乱着读_读到的都对() {
        let dir = TempDir::new();
        let content = noise(4_000_000, 3);
        dir.write("c.bin", &content);
        let mount = local_mount(&dir);
        let node = mount.open("c.bin").unwrap();
        std::thread::scope(|scope| {
            for seed in 0..8u64 {
                let (mount, node, content) = (&mount, &node, &content);
                scope.spawn(move || {
                    let mut x = seed * 7919 + 1;
                    for _ in 0..200 {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        let offset = x % (content.len() as u64 + 1000);
                        let len = (x >> 20) as usize % 300_000 + 1;
                        let got = read_at(mount, node, offset, len);
                        let start = (offset as usize).min(content.len());
                        let end = (offset as usize + len).min(content.len());
                        assert!(got == content[start..end], "从 {offset} 读 {len} 不对");
                    }
                });
            }
        });
    }

    #[test]
    fn 写了以后读到的是新的() {
        let dir = TempDir::new();
        let mut model = noise(2_000_000, 4);
        dir.write("d.bin", &model);
        let mount = local_mount(&dir);
        let node = mount.open("d.bin").unwrap();
        // 先整个读一遍，缓存里都有了
        assert!(read_at(&mount, &node, 0, model.len()) == model);
        let data = noise(100_000, 5);
        mount.write(&node, &data, 500_000, false, false).unwrap();
        model[500_000..600_000].copy_from_slice(&data);
        assert!(read_at(&mount, &node, 450_000, 200_000) == model[450_000..650_000]);
        // 写到末尾以外：中间补 0，文件变长
        mount.write(&node, &data, 2_050_000, false, false).unwrap();
        model.resize(2_050_000, 0);
        model.extend_from_slice(&data);
        assert!(read_at(&mount, &node, 0, model.len()) == model);
        // 截短
        mount.resize(&node, 1_000_000, false).unwrap();
        model.truncate(1_000_000);
        assert!(read_at(&mount, &node, 0, 2_000_000) == model);
        mount.commit(&node).unwrap();
        assert!(std::fs::read(dir.0.join("d.bin")).unwrap() == model);
    }

    #[test]
    fn 一边读一边写_写完以后读到的都是新的() {
        let dir = TempDir::new();
        let mut model = noise(3_000_000, 8);
        dir.write("w.bin", &model);
        let mount = local_mount(&dir);
        let node = mount.open("w.bin").unwrap();
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            for seed in 0..3u64 {
                let (mount, node, stop) = (&mount, &node, &stop);
                scope.spawn(move || {
                    let mut x = seed + 99;
                    while !stop.load(Relaxed) {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        read_at(mount, node, x % 3_000_000, (x >> 24) as usize % 200_000 + 1);
                    }
                });
            }
            let mut x = 12345u64;
            for round in 0..40 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let at = (x % 2_900_000) as usize;
                let data = noise((x >> 30) as usize % 100_000 + 1, round);
                mount.write(&node, &data, at as u64, false, false).unwrap();
                model[at..at + data.len()].copy_from_slice(&data);
            }
            mount.commit(&node).unwrap();
            stop.store(true, Relaxed);
        });
        assert!(read_at(&mount, &node, 0, model.len()) == model);
    }

    #[test]
    fn 文件被别人改了以后读到新的() {
        let dir = TempDir::new();
        let old = noise(1_000_000, 6);
        dir.write("e.bin", &old);
        let mount = local_mount(&dir);
        let node = mount.open("e.bin").unwrap();
        assert_eq!(read_at(&mount, &node, 0, 1000), old[..1000]);
        // 别人改了：内容和大小都变了
        let new = noise(1_200_000, 7);
        dir.write("e.bin", &new);
        // 读还没留着的地方：回来时发现大小变了，之前留着的都不要了
        assert_eq!(read_at(&mount, &node, 900_000, 1000), new[900_000..901_000]);
        assert_eq!(read_at(&mount, &node, 0, 1000), new[..1000]);
        assert_eq!(lock(&node.state).stat.size, 1_200_000);
    }

    #[test]
    fn 等回音有期限() {
        let started = Instant::now();
        let never = futures::future::pending::<u32>();
        assert_eq!(
            block_until(never, started + Duration::from_millis(200)),
            None
        );
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(block_until(async { 7 }, Instant::now()), Some(7));
        // 别的线程送来的回音也收得到
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            tx.send(5).unwrap();
        });
        let got = block_until(rx, Instant::now() + Duration::from_secs(10));
        assert_eq!(got.map(|got| got.unwrap()), Some(5));
    }

    #[test]
    fn 读的时候文件变了_从头再读_不会前半旧后半新() {
        let dir = TempDir::new();
        let old = noise(2 * UNIT as usize, 10);
        dir.write("g.bin", &old);
        let mount = local_mount(&dir);
        let node = mount.open("g.bin").unwrap();
        // 第一格留着旧的，第二格假装正在要（这个请求不会回来）：读到那里要等
        {
            let mut state = lock(&node.state);
            let bytes = Arc::new(old[..UNIT as usize].to_vec());
            let len = UNIT as usize;
            let ready = Unit::Ready {
                bytes,
                at: 0,
                len,
                used: 0,
            };
            state.cache.units.insert(0, ready);
            state.cache.units.insert(1, Unit::Wanted(u64::MAX));
        }
        let new = noise(2 * UNIT as usize, 13);
        std::thread::scope(|scope| {
            let reading = scope.spawn(|| read_at(&mount, &node, 0, new.len()));
            std::thread::sleep(Duration::from_millis(200));
            // 等的时候主机上换了内容，这边换一代（和读回来的数据发现变了时一样）
            dir.write("g.bin", &new);
            let stat = block_on(Channel::Local.fs_stat(&mount.root, "g.bin")).unwrap();
            let mut state = lock(&node.state);
            state.stat = stat;
            state.cache.renew();
            drop(state);
            node.arrived.notify_all();
            assert!(reading.join().unwrap() == new);
        });
    }

    #[test]
    fn 追加时等过以后按新的末尾写() {
        let dir = TempDir::new();
        dir.write("log.txt", b"0123456789");
        let mount = local_mount(&dir);
        let node = mount.open("log.txt").unwrap();
        // 假装有一大批写入在路上：满了，追加的要等
        {
            let mut state = lock(&node.state);
            state.flying.push((0, WINDOW));
            state.flying_bytes = WINDOW;
        }
        std::thread::scope(|scope| {
            let appending = scope.spawn(|| mount.write(&node, b"XY", 0, true, false));
            std::thread::sleep(Duration::from_millis(200));
            // 等的时候别的写把文件写长了，在路上的都落了地
            dir.write("log.txt", b"0123456789abcde");
            let mut state = lock(&node.state);
            state.stat.size = 15;
            state.flying.clear();
            state.flying_bytes = 0;
            drop(state);
            node.landed.notify_all();
            appending.join().unwrap().unwrap();
        });
        mount.commit(&node).unwrap();
        let written = std::fs::read(dir.0.join("log.txt")).unwrap();
        assert_eq!(written, b"0123456789abcdeXY");
    }

    #[test]
    fn 目录改了名_里面打开着的文件跟着换路径() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.0.join("d")).unwrap();
        dir.write("d/f.txt", b"hello");
        let mount = local_mount(&dir);
        let file = mount.open("d/f.txt").unwrap();
        let folder = mount.open("D").unwrap();
        mount.rename(&folder, "e", false).unwrap();
        assert_eq!(lock(&file.state).path, "e/f.txt");
        assert!(lock(&mount.nodes).contains_key("e/f.txt"));
        assert_eq!(read_at(&mount, &file, 0, 100), b"hello");
    }

    #[test]
    fn 读不到时报错_不卡住() {
        let dir = TempDir::new();
        dir.write("gone.bin", &noise(500_000, 9));
        let mount = local_mount(&dir);
        let node = mount.open("gone.bin").unwrap();
        std::fs::remove_file(dir.0.join("gone.bin")).unwrap();
        let mut buffer = vec![0; 4096];
        for _ in 0..2 {
            let read = mount.read(&node, &mut buffer, 0);
            assert_eq!(read, Err(STATUS_OBJECT_NAME_NOT_FOUND));
        }
    }

    #[test]
    fn 认出连着往后读的() {
        let mut cache = Cache::default();
        assert_eq!(cache.follow(0, 4096).hits, 0);
        assert_eq!(cache.follow(4096, 8192).hits, 1);
        // 另一路从中间开始，两路交替着读
        assert_eq!(cache.follow(1 << 20, (1 << 20) + 65536).hits, 0);
        assert_eq!(cache.follow(8192, 16384).hits, 2);
        let second = cache.follow((1 << 20) + 65536, (1 << 20) + 131072);
        assert_eq!((second.hits, second.run), (1, 131072));
        // 再读一遍读过的：不算接着读
        assert_eq!(cache.follow(0, 4096).hits, 0);
        // 稍微跳过一段（系统的预读打乱了先后）还算接着读
        assert_eq!(cache.follow(16384 + 65536, 16384 + 131072).hits, 3);
        // 跳得远了另开一路
        assert_eq!(cache.follow(3 << 20, (3 << 20) + 4096).hits, 0);
    }

    #[test]
    fn 留的数据有上限_先扔最久没用的() {
        let mut cache = Cache::default();
        cache.units.insert(10_000, Unit::Wanted(1));
        for unit in 0..(CACHE_UNITS as u64 + 10) {
            cache.fill(unit, 1, vec![unit as u8; UNIT as usize]);
        }
        assert_eq!(cache.units.len(), CACHE_UNITS);
        assert!(cache.missing(10));
        assert!(!cache.missing(11));
        // 在要的不扔
        assert!(matches!(cache.units.get(&10_000), Some(Unit::Wanted(1))));
        // 回来的比要的少：过了文件末尾的格是空的
        cache.fill(20_000, 3, vec![7; 100_000]);
        let len = |unit| match cache.units.get(&unit) {
            Some(Unit::Ready { len, .. }) => *len,
            _ => usize::MAX,
        };
        assert_eq!((len(20_000), len(20_001), len(20_002)), (65536, 34464, 0));
    }

    #[test]
    fn 线路的速度边读边量() {
        // 400 KiB/s、往返 60 ms 的线路，一个接一个地要 128 KiB：前一个回来时后一个已经在路上
        let mut pace = Pace::new();
        let rate = 400.0 * 1024.0;
        let size = 2 * UNIT;
        let start = Instant::now();
        let mut landed = start + Duration::from_millis(60);
        for _ in 0..20 {
            let issued = landed - Duration::from_millis(10);
            landed += Duration::from_secs_f64(size as f64 / rate);
            pace.flying += size;
            pace.landed(size, size as usize, issued, landed);
        }
        assert!((pace.rate / rate - 1.0).abs() < 0.05, "{}", pace.rate);
        assert_eq!(pace.flying, 0);
        // 在路上的最多 0.3 秒的量：这么慢的线路上是下限，两格
        assert_eq!(pace.budget(), AHEAD_MIN);
        // 线路空着时发的，量到的时间里有一个往返，比真的慢：不往下拉
        let issued = landed + Duration::from_secs(1);
        landed = issued + Duration::from_millis(60) + Duration::from_secs_f64(UNIT as f64 / rate);
        pace.landed(UNIT, UNIT as usize, issued, landed);
        assert!((pace.rate / rate - 1.0).abs() < 0.05, "{}", pace.rate);
        // 换到 100 MB/s 的线路，1 MiB 一个：很快到上限
        let rate = 100e6;
        for _ in 0..10 {
            let issued = landed - Duration::from_millis(1);
            landed += Duration::from_secs_f64((16 * UNIT) as f64 / rate);
            pace.landed(16 * UNIT, 16 * UNIT as usize, issued, landed);
        }
        assert_eq!(pace.budget(), AHEAD_MAX);
    }

    /// 经真的 kuaid 映射：和 Kuai 一样经转接程序（`kuaid --connect`）说协议，盘上的读写变成
    /// `fs_*` 请求。主目录放在 target/tmp 下（Defender 会拦系统临时目录里没签名的程序），
    /// 要先编好 kuaid（`cargo test --workspace` 会编；或者用 `KUAID_BIN` 指过去）。
    struct Daemon {
        dir: std::path::PathBuf,
        exe: std::path::PathBuf,
    }

    impl Daemon {
        fn new() -> Daemon {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            // 测试程序在 target/<档>/deps 下
            let test_exe = std::env::current_exe().unwrap();
            let profile = test_exe.parent().unwrap().parent().unwrap();
            let kuaid = std::env::var_os("KUAID_BIN")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| profile.join("kuaid.exe"));
            assert!(
                kuaid.is_file(),
                "找不到 {}：先 cargo build -p kuaid",
                kuaid.display()
            );
            let dir = profile.parent().unwrap().join("tmp").join(format!(
                "fr-mount-kuaid-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let build = dir.join(".kuai").join("test");
            std::fs::create_dir_all(&build).unwrap();
            std::fs::create_dir_all(dir.join("files")).unwrap();
            let exe = build.join("kuaid.exe");
            if std::fs::hard_link(&kuaid, &exe).is_err() {
                std::fs::copy(&kuaid, &exe).unwrap();
            }
            Daemon { dir, exe }
        }

        fn command(&self) -> std::process::Command {
            let mut cmd = std::process::Command::new(&self.exe);
            cmd.env("FR_KUAID_HOME", &self.dir)
                .env_remove("SSH_AUTH_SOCK")
                .current_dir(&self.dir);
            cmd
        }

        fn connect(&self) -> Channel {
            let mut cmd = self.command();
            cmd.arg("--connect");
            Channel::Remote(Arc::new(fr_channel::Connection::spawn_stdio(cmd).unwrap()))
        }

        /// 映射的那个目录（主机上的）
        fn files(&self) -> std::path::PathBuf {
            self.dir.join("files")
        }

        /// 线路断了：强行结束转接程序，等于 ssh 没了（常驻进程不动）。
        fn cut(&self) {
            let script = format!(
                "Get-CimInstance Win32_Process -Filter \"Name='kuaid.exe'\" | Where-Object {{ $_.ExecutablePath -and $_.ExecutablePath.StartsWith('{}', 'OrdinalIgnoreCase') -and $_.CommandLine -like '*--connect*' }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force; $_.ProcessId }}",
                self.dir.display()
            );
            let out = std::process::Command::new("powershell.exe")
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    &script,
                ])
                .output()
                .unwrap();
            assert!(
                !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
                "没找到转接程序"
            );
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.command().arg("--stop").output();
            for _ in 0..50 {
                if std::fs::remove_dir_all(&self.dir).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    /// 经 kuaid 映射 `daemon` 的 files 目录；告诉用户的话收进 `errors`。
    fn remote_mount(daemon: &Daemon, errors: &Arc<Mutex<Vec<String>>>) -> Mount {
        let settings = Settings {
            root: daemon.files().to_string_lossy().into_owned(),
            place: "T:".into(),
            read_only: false,
            windows: true,
            label: "test".into(),
        };
        let errors = errors.clone();
        let sink = Box::new(move |text: String| errors.lock().unwrap().push(text));
        Mount::new(daemon.connect(), settings, no_dir_info, sink).unwrap()
    }

    #[test]
    fn 经_kuaid_写入_截断_新建_改名盖掉_删除_主机上的文件都对() {
        let daemon = Daemon::new();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let files = daemon.files();
        let mut model = noise(3_000_000, 21);
        std::fs::write(files.join("a.bin"), &model).unwrap();
        let mount = remote_mount(&daemon, &errors);

        // 改中间、写到末尾以外、截短，落地以后主机上的和算的一样
        let node = mount.open("a.bin").unwrap();
        let data = noise(700_000, 22);
        mount.write(&node, &data, 1_000_000, false, false).unwrap();
        model[1_000_000..1_700_000].copy_from_slice(&data);
        mount.write(&node, &data, 3_100_000, false, false).unwrap();
        model.resize(3_100_000, 0);
        model.extend_from_slice(&data);
        mount.flush_file(&node).unwrap();
        assert!(std::fs::read(files.join("a.bin")).unwrap() == model);
        mount.resize(&node, 2_000_000, false).unwrap();
        model.truncate(2_000_000);
        mount.commit(&node).unwrap();
        assert!(std::fs::read(files.join("a.bin")).unwrap() == model);
        assert!(read_at(&mount, &node, 0, 4_000_000) == model);
        mount.close(node);

        // 新建一个（含新目录），写进去，再改名盖掉 a.bin：资源管理器、编辑器存盘就是这样
        mount.create("sub", true).unwrap();
        let made = mount.create("sub/b.txt", false).unwrap();
        let text = "新的内容\r\n".repeat(1000).into_bytes();
        mount.write(&made, &text, 0, false, false).unwrap();
        mount.commit(&made).unwrap();
        assert_eq!(
            std::fs::read(files.join("sub").join("b.txt")).unwrap(),
            text
        );
        mount.rename(&made, "a.bin", true).unwrap();
        assert_eq!(std::fs::read(files.join("a.bin")).unwrap(), text);
        assert!(!files.join("sub").join("b.txt").exists());
        mount.remove(&made);
        mount.close(made);
        assert!(!files.join("a.bin").exists());
        assert!(
            errors.lock().unwrap().is_empty(),
            "{:?}",
            errors.lock().unwrap()
        );
    }

    #[test]
    fn 线路断了_没落到主机上的写入不报成功_换上新连接接着写() {
        let daemon = Daemon::new();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let files = daemon.files();
        std::fs::write(files.join("c.bin"), b"").unwrap();
        let mount = Arc::new(remote_mount(&daemon, &errors));
        let node = mount.open("c.bin").unwrap();

        // 先写好一份、刷到主机上：之后断线不能把它弄坏
        const SIZE: usize = 8 << 20;
        const PIECE: usize = 256 * 1024;
        let mut model = noise(SIZE, 50);
        mount.write(&node, &model, 0, false, false).unwrap();
        mount.flush_file(&node).unwrap();
        assert!(std::fs::read(files.join("c.bin")).unwrap() == model);

        // 一边在这 8 MB 里来回覆盖着写，一边把线路断掉。写入不等主机回音就返回（像本机
        // 磁盘的写缓存），断线时总有几块在路上：落不了地的，之后的写入或刷盘必须报错，不能
        // 一直报成功
        let (writer_mount, writer_node) = (mount.clone(), node.clone());
        let writer = std::thread::spawn(move || {
            let started = Instant::now();
            for round in 0u64.. {
                let at = (round as usize * PIECE) % SIZE;
                let data = noise(PIECE, 100 + round);
                if let Err(status) =
                    writer_mount.write(&writer_node, &data, at as u64, false, false)
                {
                    return (round, Some(status), model);
                }
                model[at..at + PIECE].copy_from_slice(&data);
                if started.elapsed() > Duration::from_secs(20) {
                    return (round, None, model);
                }
            }
            unreachable!()
        });
        std::thread::sleep(Duration::from_millis(300));
        daemon.cut();
        let (rounds, failed, model) = writer.join().unwrap();
        assert!(
            failed.is_some(),
            "线路断了 20 秒，写入还一直报成功（写了 {rounds} 块）"
        );
        let flushed = mount.flush_file(&node);
        let told = errors.lock().unwrap().clone();
        eprintln!(
            "断线前后写了 {rounds} 块，第 {rounds} 块报错 {:#x}；刷盘{}；告诉用户：{told:?}",
            failed.unwrap(),
            if flushed.is_ok() { "成功" } else { "报错" },
        );
        // 刷盘也照实报：没落地的不能报成刷好了
        assert!(flushed.is_err(), "有写入没落地，刷盘却报成功");
        let on_disk = std::fs::read(files.join("c.bin")).unwrap();
        assert_eq!(on_disk.len(), SIZE, "断线把文件长度弄坏了");
        let _ = model;
        // 断了以后再写：马上报断开，不卡
        let started = Instant::now();
        while lock(&mount.channel)
            .as_ref()
            .is_some_and(|channel| match channel {
                Channel::Remote(conn) => conn.is_alive(),
                Channel::Local => true,
            })
        {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "断了 10 秒还当连着"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let again = mount.write(&node, b"x", 0, false, false);
        assert_eq!(again.err(), Some(STATUS_CONNECTION_DISCONNECTED));

        // 重新连上（Kuai 换上新连接）：同一个打开着的文件接着写，落地以后主机上是新的
        mount.set_channel(Some(daemon.connect()));
        let _ = mount.commit(&node); // 断线时记下的错在这里取走
        let fresh = noise(100_000, 999);
        mount.write(&node, &fresh, 0, false, false).unwrap();
        mount.resize(&node, fresh.len() as u64, false).unwrap();
        mount.flush_file(&node).unwrap();
        assert!(std::fs::read(files.join("c.bin")).unwrap() == fresh);
        mount.close(node);
    }
}
