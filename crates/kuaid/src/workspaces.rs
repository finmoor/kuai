//! 这台电脑保存的仓库列表和工作区（协议 10 起，格式和改法见 `fr_core::store`）。
//!
//! 所有客户端共用一份：`state_apply` 一次执行一串改动，改完存盘，再把整份推给每个
//! `state_watch` 着的客户端（包括改的那一个）。几台电脑同时改，按到达的先后依次执行。
//! 第一次用到时才读文件，不用它的客户端不碰它。这台电脑的标识和状态文件放在一起，也在这里取。

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use fr_core::protocol::Response;
use fr_core::store::{self, Op, Store};

use crate::out::Outbox;

/// 一个订阅：哪个客户端的哪个请求，往哪发。
struct Watcher {
    conn: u64,
    id: u64,
    out: Outbox,
}

pub struct Workspaces {
    path: PathBuf,
    /// 还没读文件时是 None
    store: Mutex<Option<Store>>,
    watchers: Mutex<Vec<Watcher>>,
    machine: OnceLock<String>,
}

impl Workspaces {
    pub fn new(path: PathBuf) -> Workspaces {
        Workspaces {
            path,
            store: Mutex::new(None),
            watchers: Mutex::default(),
            machine: OnceLock::new(),
        }
    }

    /// 平时用的那个文件（见 `fr_core::store::path`）。
    pub fn default_file() -> Workspaces {
        Self::new(store::path())
    }

    /// 这台电脑的标识（`fr_core::store::machine_id`），第一次握手时才取。存不下（配置目录写不进去）
    /// 就用这次运行现生成的：别的电脑这次照样认得出它，重启以后要重新认。
    pub fn machine(&self) -> &str {
        self.machine.get_or_init(|| {
            store::machine_id(&self.path).unwrap_or_else(|err| {
                eprintln!("kuaid: {err:#}");
                store::new_machine_id()
            })
        })
    }

    pub fn get(&self) -> Store {
        let mut store = self.store.lock().unwrap();
        store.get_or_insert_with(|| store::load(&self.path)).clone()
    }

    /// 执行一串改动。有改动就存盘、推给所有订阅的；存不了盘就不算（报错，内存里的也不动）。
    pub fn apply(&self, ops: &[Op]) -> Result<Store> {
        let mut guard = self.store.lock().unwrap();
        let current = guard.get_or_insert_with(|| store::load(&self.path));
        let mut next = current.clone();
        if !next.apply(ops, cfg!(windows))? {
            return Ok(next);
        }
        store::save(&self.path, &next)?;
        *current = next.clone();
        // 持着锁发：两次改动推出去的先后和执行的先后一致
        self.notify(&next);
        drop(guard);
        Ok(next)
    }

    /// 订阅：马上推一份现在的，之后每次有改动推一份。
    pub fn watch(&self, conn: u64, id: u64, out: &Outbox) {
        // 持着锁登记、发第一份：和 `apply` 的推送不会交错，不漏也不乱序
        let mut guard = self.store.lock().unwrap();
        let store = guard.get_or_insert_with(|| store::load(&self.path));
        self.watchers.lock().unwrap().push(Watcher {
            conn,
            id,
            out: out.clone(),
        });
        out.push(&update(id, store), &[]);
    }

    /// 不订阅了：`id` 为 None 是这个客户端的全部（断开了）。
    pub fn unwatch(&self, conn: u64, id: Option<u64>) {
        self.watchers
            .lock()
            .unwrap()
            .retain(|w| !(w.conn == conn && id.is_none_or(|id| id == w.id)));
    }

    fn notify(&self, store: &Store) {
        for watcher in self.watchers.lock().unwrap().iter() {
            watcher.out.push(&update(watcher.id, store), &[]);
        }
    }
}

/// 推给订阅者的一条：流里的一块，`result` 是整份。
fn update(id: u64, store: &Store) -> Response {
    Response {
        id,
        chunk: true,
        result: serde_json::to_value(store).ok(),
        ..Default::default()
    }
}
