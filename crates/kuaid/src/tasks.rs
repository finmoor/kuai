//! 定时任务（协议 18，格式见 `fr_core::tasks`）：保存、推给订阅的客户端、到点运行。
//!
//! - 所有客户端共用一份：`task_apply` 改完存盘，把整份（连同运行记录、主机现在的时间）推给每个
//!   `task_watch` 着的客户端。只有常驻进程写这个文件，不用跨进程的锁。
//! - 调度（[`Tasks::start`]，只在常驻进程里）：隔几秒看一次本地时间，到了新的一分钟就运行这一分钟
//!   该运行的任务。两次之间隔了好几分钟（kuaid 没在运行、主机睡着了）时，按任务的设置补运行错过的
//!   最近一次，或者记一条「已跳过」。
//! - 运行：（需要的话）新建工作树，不接客户端开一个终端，等提示符出来输入命令（提示词放在环境变量
//!   里，见 `fr_core::task`），记一条运行记录。超出「保留会话」的旧会话：存下最后的画面再结束，
//!   工作树里没有改动的连同分支删除。
//! - 跟着：每次看时间顺便看一遍各次运行的会话，记下最后一行、用了多久、结束没有。
//! - 转为普通会话（协议 19，`TaskOp::Detach`）：运行记成结束，会话不动。上面这几样都只管没结束的
//!   运行，转过去的会话就不再算任务的：不算进「保留会话」、不跟着、结束它时不存画面也不删工作树。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use fr_core::i18n::tr;
use fr_core::protocol::Response;
use fr_core::schedule::{Schedule, Wall};
use fr_core::task::{self, PROMPT_ENV, PromptVia, Shell, TASK_ENV, TERM_KEY_SEPARATOR};
use fr_core::tasks::{
    self, Missed, Outcome, Overlap, Run, Task, TaskFile, TaskOp, Trigger, WorktreeBase,
};
use fr_core::tf;

use crate::clock;
use crate::conn::Shared;
use crate::out::Outbox;
use crate::term::screen::is_reply;
use crate::term::{Session, Size, session_key};

/// 隔这么久看一次时间（`FR_KUAID_TASK_TICK` 毫秒可以改，测试用）
const TICK: Duration = Duration::from_secs(5);
/// 两次看时间隔的分钟数不超过这个，算一直在运行（中间的每一分钟都照常运行）；超过了算错过了
const GAP: i64 = 3;
/// 新开的终端多大（有客户端接上时会改成它的大小）
const SIZE: (u16, u16) = (120, 36);
/// 等提示符最多等多久，到了照样输入（tty 会把先打的字留给 shell）
const PROMPT_WAIT: Duration = Duration::from_secs(8);
/// 屏幕上的字这么久没变、光标不在行首，算提示符出来了
const PROMPT_QUIET: Duration = Duration::from_millis(300);
/// 要粘贴提示词的：程序启动后屏幕这么久没变再粘贴
const PASTE_QUIET: Duration = Duration::from_secs(3);
/// 上一次的会话这么久之内屏幕还在变，算「仍在运行」
const STILL_RUNNING: Duration = Duration::from_secs(60);
/// 刚开的会话还没输出过时，这么久之内也算「仍在运行」
const STARTING: Duration = Duration::from_secs(120);
/// 屏幕这么久没变，记下用了多久
const SETTLED: Duration = Duration::from_secs(10);
/// 记下用时以后这么久之内屏幕又变了（中间没人按过回车），算程序接着在干，用时等它再停下来重记
/// （见 `settle`）
const RESUMED: Duration = Duration::from_secs(60);

struct Watcher {
    conn: u64,
    id: u64,
    out: Outbox,
}

pub struct Tasks {
    path: PathBuf,
    /// 结束的会话最后的画面放在这里，一次运行一个文件
    screens: PathBuf,
    file: Mutex<TaskFile>,
    watchers: Mutex<Vec<Watcher>>,
    /// 还没记成结束的各次运行的会话（终端编号 → 会话）。程序结束时正好有人接着看的会话，会立刻
    /// 从登记表里拿掉（见 `Session::finish`），自己留一份才存得下它最后的画面。只在持着 `file`
    /// 的锁时动它
    held: Mutex<HashMap<String, Arc<Session>>>,
}

impl Tasks {
    /// 任务存在 `path`。
    pub fn new(path: PathBuf) -> Tasks {
        let file = tasks::load(&path);
        Tasks {
            screens: path.with_file_name("task-screens"),
            path,
            file: Mutex::new(file),
            watchers: Mutex::default(),
            held: Mutex::default(),
        }
    }

    /// 一次运行的会话：先找自己留着的，再找登记表（重启、升级后接手的会话只在登记表里，找到了
    /// 也留一份）。调用时持着 `file` 的锁。
    fn session_of(&self, shared: &Shared, term: &str) -> Option<Arc<Session>> {
        let mut held = self.held.lock().unwrap();
        if let Some(session) = held.get(term) {
            return Some(session.clone());
        }
        let session = shared.terms.get(term)?;
        held.insert(term.to_string(), session.clone());
        Some(session)
    }

    /// 现在的整份，带上主机的钟。
    pub fn get(&self) -> TaskFile {
        pushed(&self.file.lock().unwrap())
    }

    /// 有没有启用的、会按时运行的任务：有就不空闲退出。
    pub fn has_scheduled(&self) -> bool {
        self.file.lock().unwrap().has_scheduled()
    }

    /// 启用的、按时运行的任务有几个；任务开出来、还没结束的终端有几个。
    pub fn counts(&self) -> (usize, usize) {
        let file = self.file.lock().unwrap();
        let scheduled = file
            .tasks
            .iter()
            .filter(|task| task.enabled && !matches!(task.schedule, Schedule::Manual))
            .count();
        let terms = file
            .runs
            .iter()
            .filter(|run| run.term.is_some() && run.ended.is_none())
            .count();
        (scheduled, terms)
    }

    /// 订阅：马上推一份现在的，之后每次有改动推一份。
    pub fn watch(&self, conn: u64, id: u64, out: &Outbox) {
        // 持着锁登记、发第一份：和改动的推送不会交错
        let file = self.file.lock().unwrap();
        self.watchers.lock().unwrap().push(Watcher {
            conn,
            id,
            out: out.clone(),
        });
        out.push(&update(id, &pushed(&file)), &[]);
    }

    /// 不订阅了：`id` 为 None 是这个客户端的全部（断开了）。
    pub fn unwatch(&self, conn: u64, id: Option<u64>) {
        self.watchers
            .lock()
            .unwrap()
            .retain(|w| !(w.conn == conn && id.is_none_or(|id| id == w.id)));
    }

    /// 执行一串改动，存盘，推给订阅的。
    pub fn apply(&self, ops: &[TaskOp]) -> Result<TaskFile> {
        let mut file = self.file.lock().unwrap();
        let mut next = file.clone();
        let changed = next.apply(ops)?;
        if changed {
            // 第一次有任务：从现在算起，之前的不算错过
            if next.checked.is_none() {
                next.checked = Some(clock::now().wall);
            }
            tasks::save(&self.path, &next)?;
            *file = next;
            self.notify(&file);
            self.prune_screens(&file);
        }
        Ok(pushed(&file))
    }

    /// 改了内存里的这一份以后：存盘、推给订阅的。存不了盘记日志，内存里的照用。
    fn commit(&self, file: &TaskFile) {
        if let Err(err) = tasks::save(&self.path, file) {
            crate::daemon::log(&format!("无法保存定时任务：{err:#}"));
        }
        self.notify(file);
    }

    fn notify(&self, file: &TaskFile) {
        let copy = pushed(file);
        for watcher in self.watchers.lock().unwrap().iter() {
            watcher
                .out
                .push_latest(watcher.id, &update(watcher.id, &copy), &[]);
        }
    }

    /// `task_screen`：一次运行结束时存下的画面。
    pub fn screen(&self, run: &str) -> Result<Vec<u8>> {
        if !run.starts_with('r') || !run[1..].bytes().all(|b| b.is_ascii_digit()) {
            bail!(tr("运行记录不存在"));
        }
        std::fs::read(self.screens.join(format!("{run}.bin")))
            .map_err(|_| anyhow::anyhow!(tr("未保存此次运行的画面")))
    }

    /// 删掉运行记录里已经没有了的画面。
    fn prune_screens(&self, file: &TaskFile) {
        let Ok(entries) = std::fs::read_dir(&self.screens) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let run = name.trim_end_matches(".bin");
            if !file.runs.iter().any(|r| r.id == run && r.screen) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// 常驻进程启动后调用：起调度的线程。
    pub fn start(shared: &Arc<Shared>) {
        let weak = Arc::downgrade(shared);
        let tick = std::env::var("FR_KUAID_TASK_TICK")
            .ok()
            .and_then(|ms| ms.parse().ok())
            .map_or(TICK, Duration::from_millis);
        let spawned = std::thread::Builder::new()
            .name("kuaid-tasks".into())
            .spawn(move || schedule_loop(weak, tick));
        if let Err(err) = spawned {
            crate::daemon::log(&format!("无法启动定时任务的线程：{err}"));
        }
    }

    /// `task_run`：立即运行一次。
    pub fn run_now(&self, shared: &Shared, id: &str) -> Result<Run> {
        let task = self
            .file
            .lock()
            .unwrap()
            .task(id)
            .cloned()
            .with_context(|| tf!("任务 {id} 不存在", id))?;
        Ok(self.start_run(shared, &task, clock::now().wall, Trigger::Manual))
    }

    /// 看一次时间：运行到点的，跟着在运行的。
    fn tick(&self, shared: &Shared) {
        let now = clock::now().wall;
        let mut due: Vec<(Task, Wall, Trigger)> = Vec::new();
        {
            let mut file = self.file.lock().unwrap();
            let mut changed = false;
            if let Some(prev) = file.checked
                && now > prev
            {
                let gap = now.minutes() - prev.minutes();
                let mut skipped = Vec::new();
                for task in file.tasks.iter().filter(|task| task.enabled) {
                    if gap <= GAP {
                        let mut at = prev;
                        while let Some(next) =
                            task.schedule.next_after(&at).filter(|next| *next <= now)
                        {
                            due.push((task.clone(), next, Trigger::Scheduled));
                            at = next;
                        }
                    } else if let Some(at) = task.schedule.latest_between(&prev, &now) {
                        match (at == now, task.missed) {
                            (true, _) => due.push((task.clone(), at, Trigger::Scheduled)),
                            (false, Missed::CatchUp) => {
                                due.push((task.clone(), at, Trigger::CatchUp))
                            }
                            (false, Missed::Skip) => skipped.push(Run {
                                task: task.id.clone(),
                                at,
                                started: now_secs(),
                                outcome: Outcome::Skipped,
                                reason: tr("主机未运行").to_string(),
                                ..Default::default()
                            }),
                        }
                    }
                }
                for run in skipped {
                    file.add_run(run);
                    changed = true;
                }
            }
            // 时间往回拨了（夏令时、对时）：从现在算起，不重复运行
            if file.checked != Some(now) {
                file.checked = Some(now);
                changed |= !file.tasks.is_empty();
            }
            if changed {
                self.commit(&file);
            }
        }
        for (task, at, trigger) in due {
            self.start_run(shared, &task, at, trigger);
        }
        self.follow(shared);
    }

    /// 运行一次：开终端、输入命令，记一条运行记录（跳过、失败的也记）。
    fn start_run(&self, shared: &Shared, task: &Task, at: Wall, trigger: Trigger) -> Run {
        let mut run = Run {
            task: task.id.clone(),
            at,
            started: now_secs(),
            trigger,
            ..Default::default()
        };
        if task.overlap == Overlap::Skip && self.still_running(shared, &task.id) {
            run.outcome = Outcome::Skipped;
            run.reason = tr("上一次仍在运行").to_string();
            return self.record(shared, task, run);
        }
        match self.launch(shared, task, &at, &mut run) {
            Ok(()) => {}
            Err(err) => {
                run.outcome = Outcome::Failed;
                run.reason = format!("{err:#}");
                crate::daemon::log(&format!("定时任务「{}」运行失败：{err:#}", task.name));
            }
        }
        self.record(shared, task, run)
    }

    /// 新建工作树（需要的话）、开终端、等提示符输入命令。失败时建了的工作树删掉。
    fn launch(&self, shared: &Shared, task: &Task, at: &Wall, run: &mut Run) -> Result<()> {
        let repo = Path::new(&task.repo);
        if !repo.is_dir() {
            bail!("{}", tf!("仓库不存在：{path}", path = task.repo.as_str()));
        }
        let mut cwd = task.repo.clone();
        if let Some(base) = task.worktree {
            let made = make_worktree(task, at, base)?;
            cwd = made.path.clone();
            run.worktree = Some(made.path);
            run.branch = Some(made.branch);
            run.base = Some(made.base);
        }
        let key = free_key(&shared.terms.keys(), &task.repo);
        let env = vec![
            (PROMPT_ENV.to_string(), task.prompt.clone()),
            (TASK_ENV.to_string(), task.id.clone()),
        ];
        let size = Size::new(SIZE.0, SIZE.1);
        let opened = shared
            .terms
            .open_detached(&key, &cwd, size, env, task.shell.clone());
        let session = match opened {
            Ok(session) => session,
            Err(err) => {
                if let Some(path) = &run.worktree {
                    remove_worktree(repo, path, run.branch.as_deref());
                    run.worktree = None;
                    run.branch = None;
                }
                return Err(err);
            }
        };
        run.term = Some(session.id.clone());
        let line = task.launch.line(shell_kind(&task.shell));
        let paste = (task.launch.prompt == PromptVia::Paste).then(|| task.prompt.clone());
        std::thread::spawn(move || type_when_ready(session, line, paste));
        Ok(())
    }

    /// 上一次运行的会话还在、屏幕最近还在变（刚开的还没输出也算）。
    fn still_running(&self, shared: &Shared, task: &str) -> bool {
        let file = self.file.lock().unwrap();
        let Some(run) = file
            .runs_of(task)
            .find(|run| run.outcome == Outcome::Started)
        else {
            return false;
        };
        if run.ended.is_some() {
            return false;
        }
        let Some(session) = run
            .term
            .as_deref()
            .and_then(|term| self.session_of(shared, term))
        else {
            return false;
        };
        let glance = session.glance();
        glance.alive
            && match glance.quiet {
                Some(quiet) => quiet < STILL_RUNNING,
                None => now_secs().saturating_sub(run.started) < STARTING.as_secs(),
            }
    }

    /// 记下一次运行；只运行一次的任务运行过就停用；超出「保留会话」的旧会话结束掉。
    fn record(&self, shared: &Shared, task: &Task, run: Run) -> Run {
        let mut file = self.file.lock().unwrap();
        // 刚开的会话还没人接过，不会被拿掉，这时一定在登记表里
        if let Some(term) = &run.term {
            self.session_of(shared, term);
        }
        let id = file.add_run(run);
        if matches!(task.schedule, Schedule::Once { .. })
            && let Some(task) = file.task_mut(&task.id)
        {
            task.enabled = false;
        }
        let keep = file.task(&task.id).map_or(task.keep, |task| task.keep);
        if keep > 0 {
            let old: Vec<String> = file
                .runs_of(&task.id)
                .filter(|run| run.term.is_some() && run.ended.is_none())
                .skip(keep as usize)
                .map(|run| run.id.clone())
                .collect();
            for id in old {
                self.close_run(shared, &mut file, &id, true);
            }
        }
        self.commit(&file);
        self.prune_screens(&file);
        file.runs
            .iter()
            .find(|run| run.id == id)
            .cloned()
            .unwrap_or_default()
    }

    /// 等 [`Self::stopping`] 返回的线程删完工作树，一共最多等 `wait`。
    pub fn wait_cleanups(handles: Vec<std::thread::JoinHandle<()>>, wait: Duration) {
        let deadline = std::time::Instant::now() + wait;
        for handle in handles {
            while !handle.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    /// 常驻进程要退出了（`shutdown`，重启和 `kuaid --stop` 都是）：还开着的各次运行先存下画面、记成
    /// 结束，不然重启以后这几次只剩「已结束」，看不到最后的画面。返回删没有改动的工作树的线程：
    /// 终端结束以后才删得掉（Windows 上 shell 还在时目录删不掉），退出前等它们一会儿。
    pub fn stopping(&self, shared: &Shared) -> Vec<std::thread::JoinHandle<()>> {
        let mut file = self.file.lock().unwrap();
        let open: Vec<String> = file
            .runs
            .iter()
            .filter(|run| run.ended.is_none() && run.term.is_some())
            .map(|run| run.id.clone())
            .collect();
        if open.is_empty() {
            return Vec::new();
        }
        let cleanups = open
            .iter()
            .filter_map(|id| self.close_run(shared, &mut file, id, false))
            .collect();
        self.commit(&file);
        cleanups
    }

    /// 一次运行的会话结束了（`close` 时由这里结束它）：存下最后的画面，工作树里没有改动的删掉
    /// （在后台删，返回那个线程）。
    fn close_run(
        &self,
        shared: &Shared,
        file: &mut TaskFile,
        id: &str,
        close: bool,
    ) -> Option<std::thread::JoinHandle<()>> {
        let screens = self.screens.clone();
        let task = file
            .runs
            .iter()
            .find(|run| run.id == id)
            .and_then(|run| file.task(&run.task));
        let repo = task.map(|task| task.repo.clone());
        let replies = task.is_some_and(replies);
        let term = file.runs.iter().find(|run| run.id == id)?.term.clone();
        let session = term
            .as_deref()
            .and_then(|term| self.session_of(shared, term));
        if let Some(term) = &term {
            self.held.lock().unwrap().remove(term);
        }
        let run = file.run_mut(id)?;
        if let Some(session) = session {
            let saved = std::fs::create_dir_all(&screens).and_then(|()| {
                std::fs::write(screens.join(format!("{id}.bin")), session.snapshot())
            });
            run.screen = saved.is_ok();
            note_last(run, session.summary(replies), replies);
            if close {
                shared.terms.close(&session.id);
            }
        }
        let ended = now_secs();
        run.ended = Some(ended);
        run.seconds.get_or_insert(ended.saturating_sub(run.started));
        let (Some(path), Some(repo)) = (run.worktree.clone(), repo) else {
            return None;
        };
        let changes = count_changes(Path::new(&path), run.base.as_deref());
        run.changes = Some(changes);
        if changes > 0 {
            return None;
        }
        // 终端里的 shell 刚结束：Windows 上它还没退出时目录删不掉，放到后台多试几次
        let branch = run.branch.clone();
        Some(std::thread::spawn(move || {
            for attempt in 0..20 {
                if attempt > 0 {
                    std::thread::sleep(Duration::from_millis(300));
                }
                if try_remove_worktree(Path::new(&repo), &path, branch.as_deref(), attempt == 19) {
                    return;
                }
            }
        }))
    }

    /// `term_close`：是哪次运行的终端，就先存下画面再结束它、收拾工作树，返回 true；不是的不动。
    pub fn close_term(&self, shared: &Shared, term: &str) -> bool {
        let mut file = self.file.lock().unwrap();
        let id = file
            .runs
            .iter()
            .find(|run| run.term.as_deref() == Some(term) && run.ended.is_none())
            .map(|run| run.id.clone());
        let Some(id) = id else {
            return false;
        };
        self.close_run(shared, &mut file, &id, true);
        self.commit(&file);
        true
    }

    /// 跟着各次运行的会话：最后一行、用了多久、结束没有。
    fn follow(&self, shared: &Shared) {
        let mut file = self.file.lock().unwrap();
        let open: Vec<(String, String, bool)> = file
            .runs
            .iter()
            .filter(|run| run.ended.is_none())
            .filter_map(|run| {
                let replies = file.task(&run.task).is_some_and(replies);
                Some((run.id.clone(), run.term.clone()?, replies))
            })
            .collect();
        // 运行记录已经删掉了的（超出 50 条），不再替它留着会话
        self.held
            .lock()
            .unwrap()
            .retain(|term, _| open.iter().any(|(_, open, _)| open == term));
        let mut changed = false;
        for (id, term, replies) in open {
            let Some(session) = self.session_of(shared, &term) else {
                // 会话已经不在了，存不了画面
                self.close_run(shared, &mut file, &id, false);
                changed = true;
                continue;
            };
            let glance = session.glance();
            if !glance.alive {
                self.close_run(shared, &mut file, &id, false);
                changed = true;
                continue;
            }
            let last = session.summary(replies);
            let run = file.run_mut(&id).expect("上面刚找到");
            changed |= note_last(run, last, replies);
            changed |= settle(run, glance.quiet, glance.entered, now_secs());
        }
        if changed {
            self.commit(&file);
            self.prune_screens(&file);
        }
    }
}

fn schedule_loop(shared: Weak<Shared>, tick: Duration) {
    loop {
        std::thread::sleep(tick);
        let Some(shared) = shared.upgrade() else {
            return;
        };
        shared.tasks.tick(&shared);
    }
}

/// 这个任务运行的是不是 AI 助手（预设认得出是 Claude Code、Codex 这类；命令是自定义的看命令本身）：
/// 运行记录的「最后一行」找它的回复（见 `Screen::summary_text`）。
fn replies(task: &Task) -> bool {
    task.style().is_some()
}

/// 运行记录的「最后一行」换成 `last`，改了返回 true。空的不换；AI 助手的运行已经记下了它的回复
/// （[`is_reply`]）时只换成新的回复：它退出了（Linux 上的 Claude Code 用的是备用屏，退出时整屏的
/// 内容跟着没了）、清了屏、一条回复长得把开头顶出了屏幕，都还留着它最后说的。
fn note_last(run: &mut Run, last: String, replies: bool) -> bool {
    let kept = replies && is_reply(&run.last) && !is_reply(&last);
    if last.is_empty() || last == run.last || kept {
        return false;
    }
    run.last = last;
    true
}

/// 运行的会话屏幕停了够久（[`SETTLED`]，`quiet` 是多久没变了）时记下用了多久：从开始到最后一次
/// 变化。停下以后 [`RESUMED`] 之内又有了输出，是程序接着在干（Codex 冷启动时常常先停十几秒才开始），
/// 先清掉，等再停下来重记。停了更久以后的变化（程序原地重画）不算；停下以后有人在终端里按过回车
/// （`entered` 是多久以前：答了它问的、接着问、退出了它）的，是人在接着用，也不算。改了返回 true。
fn settle(run: &mut Run, quiet: Option<Duration>, entered: Option<Duration>, now: u64) -> bool {
    let Some(quiet) = quiet else {
        return false;
    };
    // 屏幕最后一次变化的时候（1970 年以来的秒数）
    let changed = now.saturating_sub(quiet.as_secs());
    match run.seconds {
        None if quiet >= SETTLED => {
            run.seconds = Some(changed.saturating_sub(run.started));
            true
        }
        Some(seconds) if quiet < SETTLED => {
            let stopped = run.started + seconds;
            let typed = entered.is_some_and(|ago| now.saturating_sub(ago.as_secs()) > stopped);
            let resumed = !typed && changed.saturating_sub(stopped) <= RESUMED.as_secs();
            if resumed {
                run.seconds = None;
            }
            resumed
        }
        _ => false,
    }
}

/// 给客户端的一份：带上主机的钟、常驻进程是不是 root（这两样不存盘）。
fn pushed(file: &TaskFile) -> TaskFile {
    let mut copy = file.clone();
    copy.clock = Some(clock::now().clock);
    #[cfg(unix)]
    {
        // SAFETY: 没有参数，只读进程的有效用户
        copy.root = unsafe { libc::geteuid() } == 0;
    }
    copy
}

/// 推给订阅者的一条：流里的一块，`result` 是整份。
fn update(id: u64, file: &TaskFile) -> Response {
    Response {
        id,
        chunk: true,
        result: serde_json::to_value(file).ok(),
        ..Default::default()
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// 这个仓库的终端还没用过的序号（从 2 起：第 1 个留给仓库页自己开的），拼成会话的名字。
fn free_key(keys: &[String], repo: &str) -> String {
    let normalized = session_key(repo);
    let used: Vec<u32> = keys
        .iter()
        .filter_map(|key| {
            let rest = key.strip_prefix(&normalized)?;
            match rest {
                "" => Some(1),
                _ => rest.strip_prefix(TERM_KEY_SEPARATOR)?.parse().ok(),
            }
        })
        .collect();
    let number = (2..).find(|n| !used.contains(n)).unwrap_or(2);
    task::term_key(repo, number)
}

/// 命令在哪种 shell 里输入。Linux 上是登录 shell（bash、zsh、fish 引用变量的写法一样）；Windows
/// 上看要开的 shell（没指定就是 kuaid 默认开的那个）。
fn shell_kind(shell: &[String]) -> Shell {
    if !cfg!(windows) {
        return Shell::Posix;
    }
    #[cfg(windows)]
    let shell = if shell.is_empty() {
        crate::term::hosted::default_shell()
    } else {
        shell.to_vec()
    };
    let program = shell
        .first()
        .map(|program| {
            program
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or_default()
                .to_lowercase()
        })
        .unwrap_or_default();
    match program.trim_end_matches(".exe") {
        "pwsh" | "powershell" => Shell::PowerShell,
        "cmd" => Shell::Cmd,
        _ => Shell::Posix,
    }
}

/// 等提示符出来（屏幕安静了、光标不在行首），输入命令、回车；等不到也照样输入。提示词要粘贴的，
/// 等程序起来（屏幕又安静下来）再粘贴、回车。
fn type_when_ready(session: Arc<Session>, line: String, paste: Option<String>) {
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let glance = session.glance();
        if !glance.alive {
            return;
        }
        let ready = glance.quiet.is_some_and(|quiet| quiet >= PROMPT_QUIET)
            && glance.column.is_some_and(|column| column > 0);
        if ready || start.elapsed() >= PROMPT_WAIT {
            break;
        }
    }
    session.input(format!("{line}\r").into_bytes());
    let Some(text) = paste else {
        return;
    };
    let typed = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let glance = session.glance();
        if !glance.alive {
            return;
        }
        let settled = typed.elapsed() >= Duration::from_secs(1)
            && glance.quiet.is_some_and(|quiet| quiet >= PASTE_QUIET);
        if settled || typed.elapsed() >= Duration::from_secs(60) {
            break;
        }
    }
    // 括号粘贴：多行的提示词不会被当成几次回车
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    session.input(bytes);
    std::thread::sleep(Duration::from_millis(300));
    session.input(b"\r".to_vec());
}

/// 新建的工作树。
struct Made {
    path: String,
    branch: String,
    base: String,
}

/// 在仓库旁边新建工作树和同名分支（`仓库.worktrees/任务名-月日-时分`），重名的加 `-2`、`-3`。
fn make_worktree(task: &Task, at: &Wall, base: WorktreeBase) -> Result<Made> {
    let repo = Path::new(&task.repo);
    let start = match base {
        WorktreeBase::Head => "HEAD".to_string(),
        WorktreeBase::Remote => {
            fr_core::git::run(repo, &["fetch", "--quiet", "origin"])
                .context(tr("获取远程仓库失败"))?;
            remote_default(repo)
        }
    };
    let sha = text(fr_core::git::run(
        repo,
        &["rev-parse", "--verify", &format!("{start}^{{commit}}")],
    )?);
    let name = tasks::worktree_name(&task.name, at);
    for n in 1..100 {
        let branch = match n {
            1 => name.clone(),
            n => format!("{name}-{n}"),
        };
        let path = fr_core::worktree::default_path(&task.repo, &branch);
        let taken = Path::new(&path).exists()
            || fr_core::git::run(
                repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ],
            )
            .is_ok();
        if taken {
            continue;
        }
        fr_core::git::run(repo, &["worktree", "add", "-b", &branch, &path, &sha])?;
        return Ok(Made {
            path,
            branch,
            base: sha,
        });
    }
    bail!(tr("无法新建工作树：同名的目录和分支过多"))
}

/// 远程的默认分支（`origin/HEAD` 指向的）；没有就用当前分支的上游，再没有就是当前的提交。
fn remote_default(repo: &Path) -> String {
    let found = |args: &[&str]| {
        fr_core::git::run(repo, args)
            .ok()
            .map(text)
            .filter(|name| !name.is_empty() && name != "origin/HEAD")
    };
    found(&["rev-parse", "--abbrev-ref", "origin/HEAD"])
        .or_else(|| found(&["rev-parse", "--abbrev-ref", "@{upstream}"]))
        .unwrap_or_else(|| "HEAD".into())
}

/// 工作树里相对开出来的那个提交改了几个文件：已提交的、没提交的、新加的（不算被忽略的）。
/// 读不了（目录没了）算 0。
fn count_changes(worktree: &Path, base: Option<&str>) -> u32 {
    if !worktree.is_dir() {
        return 0;
    }
    let mut files: Vec<String> = Vec::new();
    let mut add = |out: Result<Vec<u8>>| {
        if let Ok(out) = out {
            files.extend(
                String::from_utf8_lossy(&out)
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(str::to_string),
            );
        }
    };
    add(fr_core::git::run(
        worktree,
        &["diff", "--name-only", base.unwrap_or("HEAD")],
    ));
    add(fr_core::git::run(
        worktree,
        &["ls-files", "--others", "--exclude-standard"],
    ));
    files.sort();
    files.dedup();
    files.len() as u32
}

/// 删除工作树和它的分支。删不了的记日志，不报错。
fn remove_worktree(repo: &Path, path: &str, branch: Option<&str>) {
    try_remove_worktree(repo, path, branch, true);
}

/// 删除工作树和它的分支，删掉了返回 true。`last`：这是最后一次尝试，删不了记日志。
/// 目录被占着时 git 可能已经注销了工作树、只剩目录，这时把目录删掉也算。
fn try_remove_worktree(repo: &Path, path: &str, branch: Option<&str>, last: bool) -> bool {
    let removed = fr_core::git::run(repo, &["worktree", "remove", "--force", path]);
    let dir = Path::new(path);
    if dir.exists() && std::fs::remove_dir_all(dir).is_err() {
        if last {
            let err = removed
                .err()
                .map_or_else(String::new, |err| format!("{err:#}"));
            crate::daemon::log(&format!("无法删除工作树 {path}：{err}"));
        }
        return false;
    }
    let _ = fr_core::git::run(repo, &["worktree", "prune"]);
    // 删的是最后一个：仓库旁边空着的 `仓库.worktrees` 也删掉（里面还有别的就删不掉，不动）
    if let Some(parent) = dir.parent()
        && parent
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".worktrees"))
    {
        let _ = std::fs::remove_dir(parent);
    }
    if let Some(branch) = branch
        && let Err(err) = fr_core::git::run(repo, &["branch", "-D", branch])
    {
        crate::daemon::log(&format!("无法删除分支 {branch}：{err:#}"));
    }
    true
}

fn text(out: Vec<u8>) -> String {
    String::from_utf8_lossy(&out).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 挑没用过的序号_从_2_起() {
        let sep = TERM_KEY_SEPARATOR;
        let repo = "/home/me/app";
        assert_eq!(free_key(&[], repo), format!("{repo}{sep}2"));
        let keys = vec![
            repo.to_string(),
            format!("{repo}{sep}2"),
            format!("{repo}{sep}4"),
            format!("/home/me/app2{sep}3"),
        ];
        assert_eq!(free_key(&keys, repo), format!("{repo}{sep}3"));
    }

    #[test]
    fn 用时_启动时停过一会儿_接着干完的按最后停下的算() {
        let secs = Duration::from_secs;
        let mut run = Run {
            started: 1000,
            ..Default::default()
        };
        // 还没输出过、刚有输出：不记
        assert!(!settle(&mut run, None, None, 1002));
        assert!(!settle(&mut run, Some(secs(1)), None, 1003));
        assert_eq!(run.seconds, None);
        // 启动后第 3 秒起屏幕停了 10 秒（冷启动在等）：先记成 3 秒
        assert!(settle(&mut run, Some(secs(10)), None, 1013));
        assert_eq!(run.seconds, Some(3));
        assert!(!settle(&mut run, Some(secs(15)), None, 1018));
        // 停了 17 秒后干起活来：清掉，干完停下以后按最后一次变化重记
        assert!(settle(&mut run, Some(secs(1)), None, 1021));
        assert_eq!(run.seconds, None);
        assert!(!settle(&mut run, Some(secs(2)), None, 1030));
        assert!(settle(&mut run, Some(secs(12)), None, 1045));
        assert_eq!(run.seconds, Some(33));
        // 停下半小时后屏幕又变了（程序原地重画）：不再改
        assert!(!settle(&mut run, Some(secs(1)), None, 2900));
        assert!(!settle(&mut run, Some(secs(20)), None, 2920));
        assert_eq!(run.seconds, Some(33));
    }

    #[test]
    fn 用时_停下以后有人按了回车_接着的变化不算() {
        let secs = Duration::from_secs;
        let mut run = Run {
            started: 1000,
            seconds: Some(11),
            ..Default::default()
        };
        // 停下 45 秒后有人输入 /exit、回车（第 1056 秒），屏幕随即变了：不清掉
        assert!(!settle(&mut run, Some(secs(0)), Some(secs(0)), 1056));
        assert!(!settle(&mut run, Some(secs(11)), Some(secs(11)), 1067));
        assert_eq!(run.seconds, Some(11));
        // 回车是在停下之前按的（第 1010 秒，答了一开始问的）：之后屏幕又变了，照样是程序接着在干
        let mut asked = Run {
            started: 1000,
            seconds: Some(11),
            ..Default::default()
        };
        assert!(settle(&mut asked, Some(secs(0)), Some(secs(30)), 1040));
        assert_eq!(asked.seconds, None);
    }

    #[test]
    fn 最后一行_ai_助手记下了回复就只换成新的回复() {
        let mut run = Run::default();
        // 空的不记；先弹出来的框照记
        assert!(!note_last(&mut run, String::new(), true));
        assert!(note_last(
            &mut run,
            "enter continue \u{b7} esc back".into(),
            true
        ));
        assert!(note_last(&mut run, "\u{25cf} kuai-42".into(), true));
        // 退出以后屏幕上只剩 shell 的输出：留着回复
        assert!(!note_last(&mut run, "claude --resume 1234".into(), true));
        assert_eq!(run.last, "\u{25cf} kuai-42");
        assert!(note_last(&mut run, "\u{2022} done".into(), true));
        // 不是 AI 助手的：照换
        let mut script = Run {
            last: "\u{2022} step 1".into(),
            ..Default::default()
        };
        assert!(note_last(&mut script, "all done".into(), false));
        assert_eq!(script.last, "all done");
    }

    #[test]
    fn linux_上一律按_sh_的写法引用() {
        if !cfg!(windows) {
            assert_eq!(shell_kind(&[]), Shell::Posix);
        }
        #[cfg(windows)]
        {
            let kind = |program: &str| shell_kind(&[program.to_string()]);
            assert_eq!(
                kind(r"C:\Program Files\PowerShell\7\pwsh.exe"),
                Shell::PowerShell
            );
            assert_eq!(kind("powershell.exe"), Shell::PowerShell);
            assert_eq!(kind(r"C:\Windows\system32\cmd.exe"), Shell::Cmd);
            assert_eq!(kind(r"C:\Program Files\Git\bin\bash.exe"), Shell::Posix);
        }
    }
}
