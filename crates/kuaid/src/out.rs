//! 发给一个客户端的消息队列。
//!
//! 每个客户端一个写线程，别的线程只往队列里放，不直接写连接：客户端那头网络卡住时，
//! 终端的输出不能跟着卡住（不然里面的程序会停在写屏幕上）。两种放法：
//!
//! - [`Outbox::send`]：队列里积得多了就等。git 的输出用它，客户端收得慢 git 就慢下来。
//! - [`Outbox::try_send`]：积得太多就不放、返回 false。终端的输出用它，由调用方决定怎么办
//!   （先停发，积着的发掉了再接着，见 [`Outbox::queued`]）。
//! - [`Outbox::push_latest`]：订阅推的整份（终端列表、主机状态、仓库有变化），同一个订阅只留
//!   还没发出去的最新一份。
//! - [`Outbox::push`]：不等也不丢，只有一个很高的总量上限，过了就当对方不收了，断开。

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use fr_core::protocol::{self, Response};

/// 队列里积了这么多字节，[`Outbox::send`] 就等写线程发掉一些
const HIGH_WATER: usize = 4 << 20;
/// 终端的输出（[`Outbox::try_send`]）最多积大约这么久能发完的量：慢线路上积多了，程序刷屏时
/// 按 Ctrl+C，屏幕还要把积着的旧输出放完才停（300 KB/s 的线路原来要放 27 秒）。速度由写线程量
/// （见 [`Queue::rate`]），积多少在 [`TERM_MIN`] 和 [`TERM_MAX`] 之间
const TERM_SECONDS: f64 = 1.0;
/// 终端的输出至少能积这么多：太少的话慢线路上刷一屏就停发、重画
const TERM_MIN: usize = 256 << 10;
/// 还没量出对方收得多快时，终端的输出最多积这么多
const TERM_START: usize = 512 << 10;
/// 终端的输出最多积这么多（快的线路上）
const TERM_MAX: usize = 8 << 20;
/// 写线程一直有活干时，每隔这么久按写出去的量算一次速度；没积压就不算（那时量的是程序的速度）
const RATE_WINDOW: Duration = Duration::from_millis(200);
/// 队列里积了这么多字节还要 [`Outbox::push`]：对方很久没收了，断开（重连以后拿最新的）。比一次
/// 连上时还原很多个终端屏幕的量大得多
const PUSH_LIMIT: usize = 128 << 20;

#[derive(Clone)]
pub struct Outbox(Arc<Inner>);

struct Inner {
    queue: Mutex<Queue>,
    /// 有新消息，或者关了：叫醒写线程
    more: Condvar,
    /// 发掉了一些，或者关了：叫醒在等空位的
    room: Condvar,
}

#[derive(Default)]
struct Queue {
    /// 要发的消息；带编号的是订阅推的整份（见 [`Outbox::push_latest`]）
    frames: VecDeque<(Option<u64>, Vec<u8>)>,
    bytes: usize,
    closed: bool,
    /// 对方收得多快（字节每秒），还没量出来是 0。写线程积着活干的时候量，见 [`RATE_WINDOW`]
    rate: f64,
    /// 正在量的这一段：从什么时候起一直有活干、这期间写出去多少
    busy: Option<(Instant, usize)>,
}

impl Queue {
    /// 终端的输出最多积多少，见 [`TERM_SECONDS`]。
    fn term_limit(&self) -> usize {
        if self.rate <= 0.0 {
            return TERM_START;
        }
        ((self.rate * TERM_SECONDS) as usize).clamp(TERM_MIN, TERM_MAX)
    }

    /// 写出去 `size` 字节（从 `started` 开始写的），记进速度；`backlog`：后面还有等着写的。
    fn wrote(&mut self, size: usize, started: Instant, backlog: bool) {
        let now = Instant::now();
        let (since, bytes) = self.busy.get_or_insert((started, 0));
        *bytes += size;
        let (since, bytes) = (*since, *bytes);
        let elapsed = now.duration_since(since);
        if elapsed >= RATE_WINDOW {
            let sample = bytes as f64 / elapsed.as_secs_f64();
            self.rate = match self.rate > 0.0 {
                true => (self.rate + sample) / 2.0,
                false => sample,
            };
            self.busy = backlog.then_some((now, 0));
        } else if !backlog {
            // 一下子就写完了：对方收得比程序出得快，量不出来
            self.busy = None;
        }
    }
}

impl Outbox {
    /// 开一个写线程，把队列里的消息依次写进 `writer`。写失败（对方断开）就关掉。
    pub fn new(writer: impl Write + Send + 'static) -> Outbox {
        let inner = Arc::new(Inner {
            queue: Mutex::default(),
            more: Condvar::new(),
            room: Condvar::new(),
        });
        let for_thread = inner.clone();
        let spawned = std::thread::Builder::new()
            .name("kuaid-out".into())
            .spawn(move || write_loop(&for_thread, writer));
        if spawned.is_err() {
            inner.queue.lock().unwrap().closed = true;
        }
        Outbox(inner)
    }

    /// 放一条消息；队列里积得多了先等。已经关了就丢掉。
    pub fn send(&self, response: &Response, payload: &[u8]) {
        let frame = encode(response, payload);
        let mut queue = self.0.queue.lock().unwrap();
        while queue.bytes > HIGH_WATER && !queue.closed {
            queue = self.0.room.wait(queue).unwrap();
        }
        self.enqueue(queue, None, frame);
    }

    /// 放一条消息，不等；队列里积得太多时不放，返回 false。已经关了也返回 false。
    pub fn try_send(&self, response: &Response, payload: &[u8]) -> bool {
        let queue = self.0.queue.lock().unwrap();
        if queue.closed || queue.bytes > queue.term_limit() {
            return false;
        }
        self.enqueue(queue, None, encode(response, payload));
        true
    }

    /// [`Self::try_send`] 放不进去以后，积着的少到这么多再接着发（停发线的四分之一）。
    pub fn resume_at(&self) -> usize {
        self.0.queue.lock().unwrap().term_limit() / 4
    }

    /// 放一条消息，不等。只用来发不能丢的、不大的消息（流的开头和结尾、还原屏幕）。积到
    /// [`PUSH_LIMIT`] 还没发出去就断开：原来不看积了多少，对方一直不收时只增不减。
    pub fn push(&self, response: &Response, payload: &[u8]) {
        let queue = self.0.queue.lock().unwrap();
        self.enqueue_capped(queue, None, encode(response, payload));
    }

    /// 订阅推的整份（`key` 是订阅的请求编号）：队列里还有这个订阅没发出去的那份就换成这份，不再
    /// 多排一份。原来每秒一份终端列表、每次改动一份主机状态都排进去，对方收得慢时越积越多。
    pub fn push_latest(&self, key: u64, response: &Response, payload: &[u8]) {
        let frame = encode(response, payload);
        let mut queue = self.0.queue.lock().unwrap();
        let queued = queue.frames.iter().position(|(k, _)| *k == Some(key));
        if let Some(at) = queued {
            let old = std::mem::replace(&mut queue.frames[at].1, frame);
            let new = queue.frames[at].1.len();
            queue.bytes = queue.bytes - old.len() + new;
            return;
        }
        self.enqueue_capped(queue, Some(key), frame);
    }

    fn enqueue_capped(
        &self,
        queue: std::sync::MutexGuard<'_, Queue>,
        key: Option<u64>,
        frame: Vec<u8>,
    ) {
        if queue.bytes > PUSH_LIMIT && !queue.closed {
            drop(queue);
            eprintln!("kuaid: 客户端很久没收消息了，断开");
            close(&self.0);
            return;
        }
        self.enqueue(queue, key, frame);
    }

    fn enqueue(
        &self,
        mut queue: std::sync::MutexGuard<'_, Queue>,
        key: Option<u64>,
        frame: Vec<u8>,
    ) {
        if queue.closed {
            return;
        }
        queue.bytes += frame.len();
        queue.frames.push_back((key, frame));
        drop(queue);
        self.0.more.notify_one();
    }

    /// 不再发了：队列里剩下的丢掉，写线程退出。
    pub fn close(&self) {
        close(&self.0);
    }

    /// 对方已经断开（或者自己关了）。
    pub fn is_closed(&self) -> bool {
        self.0.queue.lock().unwrap().closed
    }

    /// 队列里还有多少字节没写到对方那里。
    pub fn queued(&self) -> usize {
        self.0.queue.lock().unwrap().bytes
    }

    /// 等队列里的都写出去（最多等 `timeout`）。退出前用。
    pub fn drain(&self, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        let mut queue = self.0.queue.lock().unwrap();
        while queue.bytes > 0 && !queue.closed {
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return;
            };
            queue = self.0.room.wait_timeout(queue, left).unwrap().0;
        }
    }
}

fn close(inner: &Inner) {
    let mut queue = inner.queue.lock().unwrap();
    queue.closed = true;
    queue.frames.clear();
    queue.bytes = 0;
    drop(queue);
    inner.more.notify_all();
    inner.room.notify_all();
}

fn encode(response: &Response, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 96);
    // 往内存里写不会失败
    let _ = protocol::write_message(&mut frame, response, payload);
    frame
}

fn write_loop(inner: &Inner, mut writer: impl Write) {
    loop {
        let batch = {
            let mut queue = inner.queue.lock().unwrap();
            while queue.frames.is_empty() && !queue.closed {
                queue = inner.more.wait(queue).unwrap();
            }
            if queue.closed {
                return;
            }
            std::mem::take(&mut queue.frames)
        };
        let count = batch.len();
        for (at, (_, frame)) in batch.into_iter().enumerate() {
            let started = Instant::now();
            let last = at + 1 == count;
            let written = writer
                .write_all(&frame)
                .and_then(|()| if last { writer.flush() } else { Ok(()) });
            if written.is_err() {
                close(inner);
                return;
            }
            // 写完才算发掉：等空位的看的是「还没到对方手里的」有多少。一条条记，不等整批：
            // 慢线路上一批要写好几秒，速度、空位都要跟着动
            // 期间被关掉的话计数已经清零了
            let mut queue = inner.queue.lock().unwrap();
            queue.bytes = queue.bytes.saturating_sub(frame.len());
            let backlog = !last || !queue.frames.is_empty();
            queue.wrote(frame.len(), started, backlog);
            drop(queue);
            inner.room.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Read};
    use std::time::Duration;

    fn chunk(id: u64, len: usize) -> (Response, Vec<u8>) {
        let response = Response {
            id,
            chunk: true,
            bytes: len as u64,
            ..Default::default()
        };
        (response, vec![b'x'; len])
    }

    #[test]
    fn 消息按放进去的顺序写出来() {
        let (reader, writer) = std::io::pipe().unwrap();
        let out = Outbox::new(writer);
        for id in 1..=3 {
            let (response, payload) = chunk(id, id as usize * 10);
            out.send(&response, &payload);
        }
        let mut reader = BufReader::new(reader);
        for id in 1..=3u64 {
            let (got, payload) = protocol::read_message::<_, Response>(&mut reader)
                .unwrap()
                .unwrap();
            assert_eq!(got.id, id);
            assert_eq!(payload.len(), id as usize * 10);
        }
        out.close();
        assert!(out.is_closed());
    }

    #[test]
    fn 对方不收时终端的输出不等_积多了就不放() {
        // 管道没人读：写线程卡在写上，队列越积越多
        let (reader, writer) = std::io::pipe().unwrap();
        let out = Outbox::new(writer);
        let (response, payload) = chunk(1, 64 << 10);
        let mut accepted = 0;
        while out.try_send(&response, &payload) {
            accepted += 1;
            assert!(accepted < 1024, "一直放得进去，没有上限");
        }
        // 还没量出对方多快：积到开头的上限就停
        assert!(accepted >= 4);
        assert!(out.queued() <= TERM_START + (128 << 10), "{}", out.queued());
        // 不能丢的照样放得进去
        out.push(&Response::default(), &[]);
        // 对方开始收：积着的发出去，又放得进了
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = BufReader::new(reader).read_to_end(&mut sink);
        });
        out.drain(Duration::from_secs(10));
        assert!(out.try_send(&response, &payload));
        out.close();
    }

    #[test]
    fn 订阅推的整份只留最新的一份() {
        // 管道没人读：写线程拿走第一批以后卡在写上，后面的都积在队列里
        let (reader, writer) = std::io::pipe().unwrap();
        let out = Outbox::new(writer);
        let (big, payload) = chunk(9, 1 << 20);
        while out.try_send(&big, &payload) {}
        let before = out.0.queue.lock().unwrap().bytes;
        for round in 0..100u64 {
            let (snapshot, payload) = chunk(5, 1000 + round as usize);
            out.push_latest(5, &snapshot, &payload);
        }
        let queue = out.0.queue.lock().unwrap();
        let snapshots: Vec<_> = queue.frames.iter().filter(|(k, _)| *k == Some(5)).collect();
        assert_eq!(snapshots.len(), 1);
        // 写线程可能正拿着一份在写（还没算发掉），队列里只排着一份：一百份只多出一两份的量
        assert!(
            queue.bytes < before + 3 * 1200,
            "{} {}",
            queue.bytes,
            before
        );
        drop(queue);
        // 最后收到的是最新的那份
        let reading = std::thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            let mut last = 0;
            while let Ok(Some((got, payload))) = protocol::read_message::<_, Response>(&mut reader)
            {
                if got.id == 5 {
                    last = payload.len();
                }
            }
            last
        });
        out.drain(Duration::from_secs(10));
        out.close();
        assert_eq!(reading.join().unwrap(), 1099);
    }

    #[test]
    fn 对方断开后不再等() {
        let (reader, writer) = std::io::pipe().unwrap();
        let out = Outbox::new(writer);
        drop(reader);
        let (response, payload) = chunk(1, 1 << 20);
        // 写失败后关掉；之后的 send 不会卡在等空位上
        for _ in 0..32 {
            out.send(&response, &payload);
        }
        out.drain(Duration::from_secs(5));
        assert!(out.is_closed());
        assert!(!out.try_send(&response, &payload));
    }

    /// 按限定的速度收的对方（像一条慢线路）。
    struct Throttled {
        rate: f64,
    }

    impl Write for Throttled {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let len = buf.len().min(16 << 10);
            std::thread::sleep(Duration::from_secs_f64(len as f64 / self.rate));
            Ok(len)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 程序一直刷屏、往里放 `secs` 秒，返回最后半秒里队列积到过多少。
    fn flood(out: &Outbox, secs: f64) -> usize {
        let (response, payload) = chunk(1, 32 << 10);
        let started = std::time::Instant::now();
        let mut most = 0;
        while started.elapsed().as_secs_f64() < secs {
            if !out.try_send(&response, &payload) {
                std::thread::sleep(Duration::from_millis(2));
            }
            if started.elapsed().as_secs_f64() > secs - 0.5 {
                most = most.max(out.queued());
            }
        }
        most
    }

    #[test]
    fn 终端的输出积多少看对方收得多快() {
        // 300 KB/s 的线路：积到大约一秒的量就停，不会积几 MB（按 Ctrl+C 以后要放半分钟）
        let slow = Outbox::new(Throttled { rate: 300e3 });
        let most = flood(&slow, 1.5);
        assert!(most < 400 << 10, "慢线路上积了 {most} 字节");
        assert!(most > 200 << 10, "慢线路上只积了 {most} 字节");
        assert!(slow.resume_at() < 100 << 10);
        slow.close();

        // 40 MB/s 的线路：积到上限（8 MB），刷屏时少停发、少重画
        let fast = Outbox::new(Throttled { rate: 40e6 });
        let most = flood(&fast, 1.5);
        assert!(most > 4 << 20, "快线路上只积了 {most} 字节");
        assert!(most <= TERM_MAX + (64 << 10));
        fast.close();
    }
}
