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

use fr_core::protocol::{self, Response};

/// 队列里积了这么多字节，[`Outbox::send`] 就等写线程发掉一些
const HIGH_WATER: usize = 4 << 20;
/// 队列里积了这么多字节，[`Outbox::try_send`] 就不再放
const TERM_LIMIT: usize = 8 << 20;
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
        if queue.closed || queue.bytes > TERM_LIMIT {
            return false;
        }
        self.enqueue(queue, None, encode(response, payload));
        true
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
        let size: usize = batch.iter().map(|(_, frame)| frame.len()).sum();
        let written = batch
            .iter()
            .try_for_each(|(_, frame)| writer.write_all(frame))
            .and_then(|()| writer.flush());
        if written.is_err() {
            close(inner);
            return;
        }
        // 写完才算发掉：等空位的看的是「还没到对方手里的」有多少
        // 期间被关掉的话计数已经清零了
        let mut queue = inner.queue.lock().unwrap();
        queue.bytes = queue.bytes.saturating_sub(size);
        drop(queue);
        inner.room.notify_all();
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
        let (response, payload) = chunk(1, 1 << 20);
        let mut accepted = 0;
        while out.try_send(&response, &payload) {
            accepted += 1;
            assert!(accepted < 64, "一直放得进去，没有上限");
        }
        assert!(accepted >= 8);
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
}
