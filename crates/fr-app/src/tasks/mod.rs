//! 定时任务（协议 18，格式见 `fr_core::tasks`）：客户端这一边。设计见 `docs/design/scheduled-tasks/`。
//!
//! 任务存在仓库所在的主机上，由那台的 kuaid 保存、到点运行；Kuai 连上一台就订阅它的
//! （`task_watch`），哪台电脑改的都推过来整份。和仓库列表一样只放这次运行里读到的，断开了留着、只读。
//!
//! - 标题栏「任务」页（[`page`]）：按主机列出任务，右边是详情和运行记录；运行记录里点会话还开着的
//!   那一次，右边换成它的终端，可以接着输入、「转为普通会话」（协议 19）。
//! - 新建、编辑的对话框（[`editor`]）：保存时按终端预设拼好命令（`fr_core::task::compose`）一起存进
//!   任务，主机上没有预设；「将运行」切到「自定义」时存的是人写的命令（`fr_core::task::custom_launch`）。
//! - 主机页「常驻进程」一栏（[`daemon`]）：在运行多久、时区、开机时启动、重启。
//! - 任务开出来的终端只在任务页里看：`term_board` 从这里查哪个终端是哪个任务的哪一次
//!   （[`AppState::task_run_of`]），会话页、仓库的终端页不列它们，标题栏「任务」上数待查看的。

pub mod daemon;
pub mod editor;
pub mod page;
pub mod text;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use fr_channel::{Connection, DaemonStatus};
use fr_core::i18n::tr;
use fr_core::schedule::Wall;
use fr_core::tasks::{Clock, Run, TaskFile, TaskOp};
use fr_core::tf;
use futures::StreamExt as _;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::mapping::Notice;

/// 一台主机上的任务，这次运行里读到的。
pub struct Book {
    pub file: TaskFile,
    /// 在收这台的改动。断开了就是假：留着最后读到的，只读
    pub live: bool,
}

/// 一台主机的常驻进程的情况（`daemon_status`）和问到的时刻。
pub struct Daemon {
    pub status: DaemonStatus,
    pub at: Instant,
}

/// 各台主机的任务和常驻进程，键和 `AppState::machines` 一样。
#[derive(Default)]
pub struct TaskState {
    books: HashMap<String, Book>,
    watches: HashMap<String, Task<()>>,
    daemons: HashMap<String, Daemon>,
    /// 正在问 `daemon_status` 的
    asking: HashSet<String>,
    /// 正在重启常驻进程的（等它断开、重连上）
    restarting: HashSet<String>,
}

impl TaskState {
    /// 这几台不要了（删掉、改了地址、认出来是重复的）。
    pub fn forget(&mut self, gone: impl Fn(&String) -> bool) {
        self.books.retain(|key, _| !gone(key));
        self.watches.retain(|key, _| !gone(key));
        self.daemons.retain(|key, _| !gone(key));
        self.asking.retain(|key| !gone(key));
        self.restarting.retain(|key| !gone(key));
    }
}

/// 现在的 UTC 是 1970 年以来的第几分钟。
fn utc_minutes() -> i64 {
    chrono::Utc::now().timestamp().div_euclid(60)
}

/// 主机现在几点：按它报的时区偏移从现在的 UTC 算（不用它推来时报的那一刻再往后数，那个只到分钟）。
pub fn host_now(clock: &Clock) -> Wall {
    Wall::from_minutes(utc_minutes() + i64::from(clock.offset))
}

/// 这台电脑现在几点、比 UTC 快几分钟。
pub fn local_now() -> (Wall, i32) {
    let offset = chrono::Local::now().offset().local_minus_utc() / 60;
    (
        Wall::from_minutes(utc_minutes() + i64::from(offset)),
        offset,
    )
}

impl AppState {
    /// 和一台主机接上了（`machine_connected`）：订阅它的任务。kuaid 旧于协议 18 的没有。
    pub(crate) fn watch_tasks(&mut self, key: &str, conn: Arc<Connection>, cx: &mut Context<Self>) {
        if conn.hello.protocol < 18 {
            return;
        }
        let key = key.to_string();
        self.tasks.restarting.remove(&key);
        let task = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let mut updates = conn.task_watch();
                while let Some(update) = updates.next().await {
                    let Ok(file) = update else {
                        break;
                    };
                    let arrived = this.update(cx, |this, cx| this.tasks_arrived(&key, file, cx));
                    if arrived.is_err() {
                        return;
                    }
                }
                this.update(cx, |this, cx| {
                    if let Some(book) = this.tasks.books.get_mut(&key) {
                        book.live = false;
                    }
                    cx.notify();
                })
                .ok();
            }
        });
        self.tasks.watches.insert(key.clone(), task);
        self.refresh_daemon(&key, cx);
    }

    /// 和一台主机断开了：读到的留着、只读。
    pub(crate) fn tasks_dropped(&mut self, key: &str) {
        self.tasks.watches.remove(key);
        if let Some(book) = self.tasks.books.get_mut(key) {
            book.live = false;
        }
        self.tasks.daemons.remove(key);
    }

    /// 这几台不要了。
    pub(crate) fn forget_tasks(&mut self, gone: impl Fn(&String) -> bool) {
        self.tasks.forget(gone);
    }

    fn tasks_arrived(&mut self, key: &str, file: TaskFile, cx: &mut Context<Self>) {
        // 启用的任务个数变了：常驻进程那一栏跟着
        let count = |file: &TaskFile| file.tasks.iter().filter(|t| t.enabled).count();
        let changed = self
            .tasks
            .books
            .get(key)
            .is_none_or(|old| count(&old.file) != count(&file));
        self.tasks
            .books
            .insert(key.to_string(), Book { file, live: true });
        if changed && self.tasks.daemons.contains_key(key) {
            self.refresh_daemon(key, cx);
        }
        cx.notify();
    }

    /// 主机 `key` 上的任务；没读到过是 None。
    pub fn task_book(&self, key: &str) -> Option<&Book> {
        self.tasks.books.get(key)
    }

    /// 有任务可列的主机（读到过任务的，连着、断开的都算），按主机页的先后。
    pub fn task_machines(&self) -> Vec<String> {
        self.machine_keys()
            .into_iter()
            .filter(|key| self.tasks.books.contains_key(key))
            .collect()
    }

    /// 能在上面新建任务的主机：连着、kuaid 认得定时任务的。
    pub fn task_hosts(&self) -> Vec<String> {
        self.machine_keys()
            .into_iter()
            .filter(|key| self.tasks.books.get(key).is_some_and(|book| book.live))
            .collect()
    }

    /// 主机 `key` 的常驻进程说的协议版本（连着时）：新加的操作看它能不能用。
    pub fn task_protocol(&self, key: &str) -> Option<u32> {
        let conn = self.links.get(key).filter(|conn| conn.is_alive())?;
        Some(conn.hello.protocol)
    }

    /// 终端 `term`（主机 `key` 上的）是哪个任务的哪一次运行。已经结束、转成了普通会话的不算。
    pub fn task_run_of(&self, key: &str, term: &str) -> Option<(&fr_core::tasks::Task, &Run)> {
        let book = self.tasks.books.get(key)?;
        let run = book
            .file
            .runs
            .iter()
            .find(|run| run.term.as_deref() == Some(term) && run.ended.is_none())?;
        Some((book.file.task(&run.task)?, run))
    }

    fn task_conn(&self, key: &str, cx: &mut Context<Self>) -> Option<Arc<Connection>> {
        let conn = self.links.get(key).filter(|conn| conn.is_alive()).cloned();
        if conn.is_none() {
            let text = tf!(
                "与 {name} 的连接已断开，暂时无法修改。",
                name = self.machine_name(key)
            );
            cx.emit(Notice::Warning(text.into()));
        }
        conn
    }

    /// 改主机 `key` 上的任务，返回改完的整份；做不了的说一声，返回 None。
    pub fn apply_tasks(
        &mut self,
        key: &str,
        ops: Vec<TaskOp>,
        cx: &mut Context<Self>,
    ) -> Task<Option<TaskFile>> {
        let Some(conn) = self.task_conn(key, cx) else {
            return Task::ready(None);
        };
        let key = key.to_string();
        cx.spawn(async move |this, cx| {
            let result = conn.task_apply(ops).await;
            this.update(cx, |this, cx| match result {
                Ok(file) => {
                    this.tasks_arrived(&key, file.clone(), cx);
                    Some(file)
                }
                Err(err) => {
                    let text = tf!("保存失败：{err}", err = format!("{err:#}"));
                    cx.emit(Notice::Error(text.into()));
                    None
                }
            })
            .ok()
            .flatten()
        })
    }

    /// 立即运行一次。跳过了、没运行成的说一声原因。
    pub fn run_task(&mut self, key: &str, id: &str, cx: &mut Context<Self>) {
        let Some(conn) = self.task_conn(key, cx) else {
            return;
        };
        let run = conn.task_run(id);
        cx.spawn(async move |this, cx| {
            let result = run.await;
            this.update(cx, |_, cx| {
                use fr_core::tasks::Outcome;
                match result {
                    Ok(run) if run.outcome == Outcome::Skipped => {
                        let text = tf!(
                            "未运行：{reason}",
                            reason = text::reason(&run.reason).to_string()
                        );
                        cx.emit(Notice::Info(text.into()));
                    }
                    Ok(run) if run.outcome == Outcome::Failed => {
                        let text = tf!("运行失败：{reason}", reason = run.reason);
                        cx.emit(Notice::Error(text.into()));
                    }
                    Ok(_) => {}
                    Err(err) => {
                        let text = tf!("运行失败：{reason}", reason = format!("{err:#}"));
                        cx.emit(Notice::Error(text.into()));
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// 一次运行结束时存下的画面（还原屏幕用的输出）；没连着是 None。
    pub fn task_screen(
        &self,
        key: &str,
        run: &str,
    ) -> Option<futures::future::BoxFuture<'static, anyhow::Result<Vec<u8>>>> {
        let conn = self.links.get(key).filter(|conn| conn.is_alive())?;
        Some(conn.task_screen(run))
    }

    // ---- 常驻进程 ----

    /// 主机 `key` 的常驻进程的情况，问到过的话。
    pub fn daemon(&self, key: &str) -> Option<&Daemon> {
        self.tasks.daemons.get(key)
    }

    /// 正在重启这台的常驻进程。
    pub fn daemon_restarting(&self, key: &str) -> bool {
        self.tasks.restarting.contains(key)
    }

    /// 再问一遍常驻进程的情况（主机页显示着、改了开机时启动、任务个数变了时）。
    pub fn refresh_daemon(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(conn) = self.links.get(key).filter(|conn| conn.is_alive()).cloned() else {
            return;
        };
        if conn.hello.protocol < 18 || !self.tasks.asking.insert(key.to_string()) {
            return;
        }
        let key = key.to_string();
        let ask = conn.daemon_status();
        cx.spawn(async move |this, cx| {
            let result = ask.await;
            this.update(cx, |this, cx| {
                this.tasks.asking.remove(&key);
                match result {
                    Ok(status) => {
                        this.tasks.daemons.insert(
                            key,
                            Daemon {
                                status,
                                at: Instant::now(),
                            },
                        );
                    }
                    Err(err) => log::warn!("问不到 {key} 的常驻进程：{err:#}"),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 打开、关掉主机 `key` 的开机时启动。
    pub fn set_autostart(&mut self, key: &str, on: bool, cx: &mut Context<Self>) {
        let Some(conn) = self.task_conn(key, cx) else {
            return;
        };
        let key = key.to_string();
        let set = conn.autostart_set(on);
        cx.spawn(async move |this, cx| {
            let result = set.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(status) => {
                        this.tasks.daemons.insert(
                            key,
                            Daemon {
                                status,
                                at: Instant::now(),
                            },
                        );
                    }
                    Err(err) => {
                        let text = tf!("无法设置开机时启动：{err}", err = format!("{err:#}"));
                        cx.emit(Notice::Error(text.into()));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 重启主机 `key` 的常驻进程：叫它退出，断开以后照常重连，转接程序拉起新的。
    pub fn restart_daemon(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(conn) = self.task_conn(key, cx) else {
            return;
        };
        log::info!(
            "重启 {} 的常驻进程",
            if key == crate::machines::LOCAL {
                "本机"
            } else {
                key
            }
        );
        self.tasks.restarting.insert(key.to_string());
        cx.notify();
        let key = key.to_string();
        let stop = conn.shutdown();
        cx.spawn(async move |this, cx| {
            let result = stop.await;
            this.update(cx, |this, cx| {
                if let Err(err) = result {
                    this.tasks.restarting.remove(&key);
                    let text = tf!("无法重启常驻进程：{err}", err = format!("{err:#}"));
                    cx.emit(Notice::Error(text.into()));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// 有没有正在运行的定时任务（标题栏「任务」后面的小点）。
pub fn busy(app: &AppState, board: &crate::term_board::TermBoard) -> bool {
    app.tasks.books.iter().any(|(machine, book)| {
        book.live
            && book
                .file
                .runs
                .iter()
                .any(|run| page::run_state(run, machine, true, board) == page::RunState::Running)
    })
}

/// 现在是 1970 年以来的第几秒（算还在运行的那一次已经运行了多久）。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// 新任务的 id：`t` 加毫秒数，同一台上不重。
pub fn new_task_id(file: &TaskFile) -> String {
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let mut id = format!("t{base}");
    let mut n = 2;
    while file.task(&id).is_some() {
        id = format!("t{base}-{n}");
        n += 1;
    }
    id
}

/// 「未运行」「运行失败」这些说法在没有连上时用的：主机未连接。
pub fn offline_text() -> &'static str {
    tr("主机未连接，显示的是断开前的内容。")
}
