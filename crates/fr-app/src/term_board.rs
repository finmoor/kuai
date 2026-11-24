//! 所有工作区里的终端现在怎么样了（会话页、标题栏上的个数、「已停下」的提醒都从这里读）。
//!
//! 终端开在各台电脑的 kuaid 里。连着哪台就订阅哪台的终端列表（`term_watch`，协议 11）：开了、结束了、
//! 有输出变成没输出时 kuaid 推一份过来，每个终端带着「多久没有输出了」和屏幕上最后一行字。
//! 不用把每个终端都接上。没连着的电脑上的终端不知道，也不为了看它去连。
//!
//! 一个终端的三种状态（[`TermState`]）只看输出：
//!
//! - 运行中：[`QUIET`] 以内有过输出。
//! - 已停下：超过这么久没有输出，而且停下以后还没看过。
//! - 空闲：停下以后看过了，或者不知道它什么时候输出过。
//!
//! 「看过」：它正显示在窗口里（仓库标签页的终端页上选中的那个，见 [`TermBoard::set_shown`]），
//! 在会话页里选中过，或者从会话页前往过。会话页里正选中的那个不提醒、不计入标题栏上的个数，
//! 但在列表里仍写着「已停下」，选到别的、离开那一页以后才变成空闲：不然一打开会话页，自动选中的
//! 那个马上变成空闲，就看不出是哪个停下了。启动 Kuai、刚连上一台电脑时已经停着的终端都算看过：
//! 只提醒这次运行期间停下来的。记在内存里，不同步。
//!
//! 不准的情况：十几秒不出字的命令（链接、下载）会先被当成已停下，再出字时变回运行中。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fr_channel::{Connection, TermInfo};
use futures::StreamExt as _;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::machines::{LOCAL, WorkspaceKey, machine_of};
use crate::settings::{RepoEntry, TermSlot};
use crate::terminal;

/// 这么久没有新输出就算停下了
pub const QUIET: Duration = Duration::from_secs(10);
/// 重新算一遍状态的间隔
const TICK: Duration = Duration::from_secs(1);
/// 状态没变也隔这么多次重画一遍：界面上「停了多久」的字要跟着走
const REDRAW_EVERY: u32 = 30;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TermState {
    Running,
    Stopped,
    Idle,
}

/// 按最近一次输出、最近一次看它的时间算状态。
pub fn state_of(last_output: Option<Instant>, seen: Option<Instant>, now: Instant) -> TermState {
    match last_output {
        None => TermState::Idle,
        Some(at) if now.saturating_duration_since(at) < QUIET => TermState::Running,
        Some(at) if seen.is_some_and(|seen| seen >= at) => TermState::Idle,
        Some(_) => TermState::Stopped,
    }
}

/// 哪台电脑上的哪个终端。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct TermId {
    /// 电脑的键：本机是空串，服务器是它的 id（经别的电脑连的是 `那台>它`）
    pub machine: String,
    /// kuaid 给这个终端的编号
    pub term: String,
}

/// 正显示在窗口里的终端。
#[derive(Clone, PartialEq)]
pub enum Shown {
    /// 仓库标签页的终端页上：哪个仓库的第几个终端
    Repo(RepoEntry, u32),
    /// 会话页里选中的
    Term(TermId),
    /// 会话页里正往里输入的：和仓库终端页上显示着的一样，一直算看过
    Typing(TermId),
}

/// 读到的一个终端。
struct Listed {
    info: TermInfo,
    /// 最近一次输出的时刻，按收到时的「多久没有输出」倒推的；不知道是 None
    last_output: Option<Instant>,
    /// 它是哪个仓库的第几个终端（见 [`owner`]）。列表推过来、工作区和仓库列表变了时算好：原来每次
    /// 画都给每个终端把所有工作区的仓库对一遍，一帧里还要算好几遍
    owner: (RepoEntry, u32),
}

/// 列表里的一行。
#[derive(Clone)]
pub struct Row {
    pub id: TermId,
    /// 它是哪个仓库的终端
    pub entry: RepoEntry,
    /// 那个仓库的第几个终端
    pub number: u32,
    /// 程序自己设的标题
    pub title: Option<String>,
    /// 屏幕上最后一行有字的内容
    pub last: String,
    pub cols: u16,
    pub rows: u16,
    /// 里面的程序还在跑
    pub alive: bool,
    pub state: TermState,
    pub last_output: Option<Instant>,
    /// 除了 shell 还在跑的程序（协议 14 起 kuaid 随列表推）。空着是不知道：旧版 kuaid
    pub programs: Option<Vec<String>>,
}

/// 终端里跑的是哪一类程序：会话页侧栏按它挑图标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Program {
    /// AI 助手（Claude Code、Codex 这些）
    Agent,
    /// 别的命令
    Command,
    /// 只有 shell，或者程序已经结束
    Shell,
    /// 不知道（旧版 kuaid 不报）
    Unknown,
}

/// 认作 AI 助手的程序名（小写，Windows 上去掉了 `.exe`；Linux 上是 `comm`，最长 15 个字符）。
const AGENTS: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "opencode",
    "agy",
    "aider",
    "qwen",
    "codebuddy",
    "cursor-agent",
    "copilot",
    "amp",
    "goose",
    "crush",
    "kimi",
    "droid",
    "auggie",
];

impl Row {
    pub fn program(&self) -> Program {
        // 已经结束的不用 kuaid 报也知道：旧版 kuaid 的也画成 shell，不画成「不知道」的状态点
        if !self.alive {
            return Program::Shell;
        }
        let Some(programs) = &self.programs else {
            return Program::Unknown;
        };
        if programs.is_empty() {
            return Program::Shell;
        }
        let agent = programs
            .iter()
            .any(|name| AGENTS.contains(&name.to_lowercase().as_str()));
        match agent {
            true => Program::Agent,
            false => Program::Command,
        }
    }

    /// 哪个仓库的第几个终端：终端重启以后编号换了，这个不变。
    pub fn slot(&self) -> TermSlot {
        TermSlot {
            entry: self.entry.clone(),
            number: self.number,
        }
    }
}

/// 最小的、还没用上的终端序号。
pub fn free_number(used: &[u32]) -> u32 {
    (1..).find(|n| !used.contains(n)).unwrap_or(1)
}

/// 会话页侧栏里一节是什么。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum GroupKey {
    /// 一个工作区
    Workspace(WorkspaceKey),
    /// 一台主机上不在任何工作区里的仓库（主机的键，经别的主机连的是 `那台>它`）
    Unbound(String),
}

impl GroupKey {
    pub fn workspace(&self) -> Option<&WorkspaceKey> {
        match self {
            GroupKey::Workspace(key) => Some(key),
            GroupKey::Unbound(_) => None,
        }
    }

    /// 这一节在哪台主机上
    pub fn machine(&self) -> &str {
        match self {
            GroupKey::Workspace(key) => &key.machine,
            GroupKey::Unbound(machine) => machine,
        }
    }
}

/// 一节里的仓库和各自的终端（会话页的侧栏按它画）。
pub struct RepoGroup {
    pub key: GroupKey,
    /// 工作区的按工作区里的先后；未加入工作区的按主机仓库列表的先后，列表里没有、却开着终端的
    /// （子仓库）排在后面。没有终端的仓库也在
    pub repos: Vec<(RepoEntry, Vec<Row>)>,
}

/// 一个不在眼前的终端从运行中变成了已停下。
pub struct Stopped(pub Row);

pub struct TermBoard {
    app: Entity<AppState>,
    lists: HashMap<String, Vec<Listed>>,
    /// 订阅各台电脑终端列表的任务，和订阅用的连接；丢弃就不订阅了
    watches: HashMap<String, (Arc<Connection>, Task<()>)>,
    seen: HashMap<TermId, Instant>,
    /// 上一次算出来的状态：和这一次不一样才重画、才提醒
    states: HashMap<TermId, TermState>,
    shown: Option<Shown>,
    _observe: Subscription,
    _tick: Task<()>,
}

impl EventEmitter<Stopped> for TermBoard {}

/// 主窗口的那一份 [`TermBoard`]：各个仓库页（包括进到子模块、工作树里的）从这里拿，跟着同一份
/// 终端列表走。
pub struct BoardHandle(pub Entity<TermBoard>);

impl Global for BoardHandle {}

impl TermBoard {
    pub fn new(app: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // 连上、断开了哪台电脑，跟着订阅、退订
        let observe = cx.observe(&app, |this, _, cx| this.sync_watches(cx));
        let tick = cx.spawn(async move |this, cx| {
            let mut count = 0;
            loop {
                cx.background_executor().timer(TICK).await;
                count += 1;
                let redraw = count % REDRAW_EVERY == 0;
                let done = this.update(cx, |this, cx| {
                    this.recompute(cx);
                    if redraw && !this.lists.is_empty() {
                        cx.notify();
                    }
                });
                if done.is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            app,
            lists: HashMap::new(),
            watches: HashMap::new(),
            seen: HashMap::new(),
            states: HashMap::new(),
            shown: None,
            _observe: observe,
            _tick: tick,
        };
        this.sync_watches(cx);
        this
    }

    fn sync_watches(&mut self, cx: &mut Context<Self>) {
        let links: Vec<(String, Arc<Connection>)> = self
            .app
            .read(cx)
            .links
            .iter()
            .map(|(key, conn)| (key.clone(), conn.clone()))
            .collect();
        let before = self.watches.len();
        let mut replaced: Vec<String> = Vec::new();
        self.watches.retain(|key, (conn, _)| {
            let kept = links.iter().any(|(k, c)| k == key && Arc::ptr_eq(c, conn));
            if !kept && links.iter().any(|(k, _)| k == key) {
                replaced.push(key.clone());
            }
            kept
        });
        // 同一个键换了连接（改了地址，可能已经是另一台电脑）：旧连接读到的列表不能留到新的推过来，
        // 不然这期间列着的是旧那台的终端，经新连接去操作的却是这一台
        for key in replaced {
            self.forget(&key);
        }
        let mut changed = self.watches.len() != before;
        for (key, conn) in links {
            // 旧版 kuaid 不会有（连接时就升级了），保险起见
            if self.watches.contains_key(&key) || conn.hello.protocol < 11 {
                continue;
            }
            let task = cx.spawn({
                let (key, conn) = (key.clone(), conn.clone());
                async move |this, cx| {
                    let mut updates = conn.term_watch();
                    let mut first = true;
                    while let Some(Ok(list)) = updates.next().await {
                        let arrived = this.update(cx, |this, cx| {
                            this.arrived(&key, list, first, cx);
                        });
                        if arrived.is_err() {
                            return;
                        }
                        first = false;
                    }
                    this.update(cx, |this, cx| this.dropped(&key, &conn, cx))
                        .ok();
                }
            });
            self.watches.insert(key, (conn, task));
            changed = true;
        }
        // 断开了的电脑：它上面的终端不再列着
        let gone: Vec<String> = self
            .lists
            .keys()
            .filter(|key| !self.watches.contains_key(*key))
            .cloned()
            .collect();
        for key in gone {
            self.forget(&key);
            changed = true;
        }
        changed |= self.refresh_owners(cx);
        if changed {
            cx.notify();
        }
    }

    /// 工作区、仓库列表可能变了：重新认一遍每个终端是哪个仓库的。返回有没有变。
    fn refresh_owners(&mut self, cx: &App) -> bool {
        let app = self.app.read(cx);
        let mut changed = false;
        for (machine, list) in &mut self.lists {
            for listed in list {
                let owner = owner(app, machine, &listed.info.key);
                if listed.owner != owner {
                    listed.owner = owner;
                    changed = true;
                }
            }
        }
        changed
    }

    fn forget(&mut self, machine: &str) {
        self.lists.remove(machine);
        self.seen.retain(|id, _| id.machine != machine);
        self.states.retain(|id, _| id.machine != machine);
    }

    /// 订阅断了（连接断开）。
    fn dropped(&mut self, machine: &str, conn: &Arc<Connection>, cx: &mut Context<Self>) {
        // 已经换上了新的连接：旧的断了不算
        if self
            .watches
            .get(machine)
            .is_some_and(|(current, _)| Arc::ptr_eq(current, conn))
        {
            self.watches.remove(machine);
            self.forget(machine);
            cx.notify();
        }
    }

    /// 一台电脑推来一份终端列表。`first`：这次订阅的第一份。
    fn arrived(&mut self, machine: &str, list: Vec<TermInfo>, first: bool, cx: &mut Context<Self>) {
        let now = Instant::now();
        let old = self.lists.remove(machine).unwrap_or_default();
        let app = self.app.read(cx);
        let listed: Vec<Listed> = list
            .into_iter()
            .map(|info| {
                let before = old
                    .iter()
                    .find(|listed| listed.info.term == info.term)
                    .and_then(|listed| listed.last_output);
                // 正在输出时 kuaid 隔几秒才推一次，这个时刻会落后几秒；输出停下以后它还会推一次，
                // 到判断「已停下还是空闲」的时候（停下 10 秒后）已经是准的
                let fresh = info
                    .idle_ms
                    .and_then(|idle| now.checked_sub(Duration::from_millis(idle)));
                let last_output = fresh.or(before);
                let owner = owner(app, machine, &info.key);
                Listed {
                    info,
                    last_output,
                    owner,
                }
            })
            .collect();
        // 不在了的终端不用再记着
        let here = |id: &TermId| {
            id.machine != machine || listed.iter().any(|listed| listed.info.term == id.term)
        };
        self.seen.retain(|id, _| here(id));
        self.states.retain(|id, _| here(id));
        if first {
            // 刚连上时已经停着的都算看过：只提醒这次运行期间停下来的
            for item in &listed {
                let id = TermId {
                    machine: machine.to_string(),
                    term: item.info.term.clone(),
                };
                if state_of(item.last_output, None, now) == TermState::Stopped {
                    self.seen.insert(id, now);
                }
            }
        }
        self.lists.insert(machine.to_string(), listed);
        self.recompute(cx);
        // 标题、最后一行也可能变了
        cx.notify();
    }

    /// 重新算一遍各个终端的状态：变了就重画；不在眼前的从运行中变成已停下的，说一声。
    fn recompute(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let shown = self.shown_id();
        // 仓库终端页上正显示着的、会话页里正往里输入的一直算看过。会话页里只是选中的那个先不记：它在
        // 列表里还得写着「已停下」，选到别的、离开这一页时才记（见 `set_shown`）
        if let (Some(id), Some(Shown::Repo(..) | Shown::Typing(_))) = (&shown, &self.shown) {
            self.seen.insert(id.clone(), now);
        }
        let mut changed = false;
        let mut stopped = Vec::new();
        for (machine, list) in &self.lists {
            for item in list {
                let id = TermId {
                    machine: machine.clone(),
                    term: item.info.term.clone(),
                };
                let state = state_of(item.last_output, self.seen.get(&id).copied(), now);
                let before = self.states.insert(id.clone(), state);
                if before == Some(state) {
                    continue;
                }
                changed = true;
                // 只在 FR_LOG=1 时打出来：断开远程桌面、看不到画面时靠它核对
                log::info!(
                    "终端 {}（{}）：{before:?} -> {state:?}",
                    item.info.key,
                    if machine.is_empty() {
                        "本机"
                    } else {
                        machine
                    }
                );
                let watching = shown.as_ref() == Some(&id);
                if before == Some(TermState::Running) && state == TermState::Stopped && !watching {
                    stopped.push(id);
                }
            }
        }
        if changed {
            cx.notify();
        }
        for id in stopped {
            if let Some(row) = self.row(&id) {
                cx.emit(Stopped(row));
            }
        }
    }

    /// 正显示在窗口里的是哪个终端（窗口不在前台、显示的不是终端时是 None）。
    pub fn set_shown(&mut self, shown: Option<Shown>, cx: &mut Context<Self>) {
        if self.shown != shown {
            // 会话页里刚才选中的那个：看过了
            if let Some(Shown::Term(id) | Shown::Typing(id)) = &self.shown {
                self.seen.insert(id.clone(), Instant::now());
            }
            self.shown = shown;
            match &self.shown {
                None => log::info!("正显示的终端：没有"),
                Some(Shown::Repo(entry, number)) => {
                    log::info!("正显示的终端：{} 的第 {number} 个", entry.path)
                }
                Some(Shown::Term(id)) => log::info!("正显示的终端：会话页里选中的 {}", id.term),
                Some(Shown::Typing(id)) => {
                    log::info!("正显示的终端：会话页里正在输入的 {}", id.term)
                }
            }
            self.recompute(cx);
            // 状态没变，标题栏上的个数也可能变了（正选中的那个不算）
            cx.notify();
        }
    }

    fn shown_id(&self) -> Option<TermId> {
        match self.shown.as_ref()? {
            Shown::Term(id) | Shown::Typing(id) => Some(id.clone()),
            Shown::Repo(entry, number) => {
                let machine = machine_of(entry);
                let term = self.lists.get(machine)?.iter().find(|listed| {
                    terminal::session_number(&entry.path, &listed.info.key) == Some(*number)
                })?;
                Some(TermId {
                    machine: machine.to_string(),
                    term: term.info.term.clone(),
                })
            }
        }
    }

    /// 看过了（从会话页前往它）。
    pub fn mark_seen(&mut self, id: &TermId, cx: &mut Context<Self>) {
        self.seen.insert(id.clone(), Instant::now());
        self.recompute(cx);
    }

    /// 那台电脑的终端列表订阅着没有（连着才订阅）。
    pub fn watching(&self, machine: &str) -> bool {
        self.watches.contains_key(machine)
    }

    /// 订阅那台电脑用的连接（预览经它接上终端）。
    pub fn conn(&self, machine: &str) -> Option<Arc<Connection>> {
        self.watches.get(machine).map(|(conn, _)| conn.clone())
    }

    /// 已停下、还没看过的有几个（会话页里正选中的那个不算：正看着它）。
    pub fn attention(&self) -> usize {
        let shown = self.shown_id();
        self.states
            .iter()
            .filter(|(id, state)| **state == TermState::Stopped && shown.as_ref() != Some(*id))
            .count()
    }

    fn make_row(&self, machine: &str, listed: &Listed) -> Row {
        let id = TermId {
            machine: machine.to_string(),
            term: listed.info.term.clone(),
        };
        let (entry, number) = listed.owner.clone();
        let state = self.states.get(&id).copied().unwrap_or(TermState::Idle);
        Row {
            id,
            entry,
            number,
            title: listed.info.title.clone().filter(|title| !title.is_empty()),
            last: listed.info.last.clone(),
            cols: listed.info.cols,
            rows: listed.info.rows,
            alive: listed.info.alive,
            state,
            last_output: listed.last_output,
            programs: listed.info.programs.clone(),
        }
    }

    pub fn row(&self, id: &TermId) -> Option<Row> {
        let listed = self
            .lists
            .get(&id.machine)?
            .iter()
            .find(|listed| listed.info.term == id.term)?;
        Some(self.make_row(&id.machine, listed))
    }
}

impl TermBoard {
    /// 全部终端：一台电脑一台电脑地排，同一台里按仓库、序号。
    fn all_rows(&self) -> Vec<Row> {
        let mut machines: Vec<&String> = self.lists.keys().collect();
        machines.sort();
        let mut rows: Vec<Row> = Vec::new();
        for machine in machines {
            let mut list: Vec<Row> = self.lists[machine]
                .iter()
                .map(|listed| self.make_row(machine, listed))
                .collect();
            list.sort_by(|a, b| (&a.entry.path, a.number).cmp(&(&b.entry.path, b.number)));
            rows.extend(list);
        }
        rows
    }

    /// 这个仓库的终端，按序号。那台电脑的终端列表还没读到是 None。
    ///
    /// 按会话的名字认（Windows 上不分大小写、斜杠，见 [`terminal::session_number`]），不按 [`owner`]
    /// 认出的仓库：没登记过的子仓库 owner 认不出，给的是 kuaid 那种小写、正斜杠的写法，和打开的这个
    /// 标签页的写法对不上，仓库的终端页就看不到自己的终端。
    pub fn terms_of(&self, entry: &RepoEntry) -> Option<Vec<Row>> {
        let machine = machine_of(entry);
        let mut rows: Vec<Row> = self
            .lists
            .get(machine)?
            .iter()
            .filter_map(|listed| {
                let number = terminal::session_number(&entry.path, &listed.info.key)?;
                let mut row = self.make_row(machine, listed);
                row.entry = entry.clone();
                row.number = number;
                Some(row)
            })
            .collect();
        rows.sort_by_key(|row| row.number);
        Some(rows)
    }

    /// 全部工作区里的仓库和各自的终端，后面是各台主机上未加入工作区的仓库（主机仓库列表里不在
    /// 任何工作区中的，没有终端的也列；列表里没有、却开着终端的也归在这里）。一个仓库在几个工作区
    /// 里时只列在第一个里。没连着的主机上的仓库不列：上面的终端不知道，也开不了。
    pub fn repos(&self, cx: &App) -> Vec<RepoGroup> {
        let app = self.app.read(cx);
        let order = workspace_order(app);
        let grouped: Vec<RepoEntry> = order.iter().flat_map(|key| app.members(key)).collect();
        let workspaces = self.reachable_members(order, app);
        let (mut groups, rows) = group_by_workspace(workspaces, self.all_rows());
        let listed: Vec<RepoEntry> = app
            .all_repos()
            .into_iter()
            .filter(|entry| self.reachable(entry) && !grouped.contains(entry))
            .collect();
        groups.extend(group_unbound(listed, rows));
        groups
    }

    /// 电脑 `machine` 上的工作区里的仓库和各自的终端（会话页只看一台主机时用）。和 [`Self::repos`]
    /// 不一样的地方：只在这台电脑的工作区之间去重。同一个仓库也在别的电脑的工作区里时，[`Self::repos`]
    /// 把它归到排在前面的那个工作区（比如本机现在用的），这里照样列在这台电脑的工作区里；不然
    /// 这台电脑的工作区会因为仓库都被别处占了而整个不见。未加入工作区的那几节这里不管（从
    /// [`Self::repos`] 里挑）。
    pub fn repos_on(&self, machine: &str, cx: &App) -> Vec<RepoGroup> {
        let app = self.app.read(cx);
        let mut keys = workspace_order(app);
        keys.retain(|key| key.machine == machine);
        let workspaces = self.reachable_members(keys, app);
        group_by_workspace(workspaces, self.all_rows()).0
    }

    /// 这次连着、开得了终端：本机的总是，主机上的要那台连着。
    fn reachable(&self, entry: &RepoEntry) -> bool {
        entry.server.is_none() || self.watching(machine_of(entry))
    }

    /// 各个工作区里的仓库，去掉这次还没连上的服务器上的。
    fn reachable_members(
        &self,
        keys: Vec<WorkspaceKey>,
        app: &AppState,
    ) -> Vec<(WorkspaceKey, Vec<RepoEntry>)> {
        keys.into_iter()
            .map(|key| {
                let mut members = app.members(&key);
                members.retain(|entry| self.reachable(entry));
                (key, members)
            })
            .collect()
    }
}

/// 未加入工作区的仓库按主机分节：`listed` 是主机仓库列表里的（已经按主机排好），`rows` 是没归到
/// 哪个工作区的终端。列表里的仓库按列表的先后，终端归到各自的仓库；剩下的终端（子仓库这类列表里
/// 没有的）按仓库接在它所在主机那一节的后面，那台没有这一节就新起一节。
fn group_unbound(listed: Vec<RepoEntry>, mut rows: Vec<Row>) -> Vec<RepoGroup> {
    let mut groups: Vec<RepoGroup> = Vec::new();
    let add = |groups: &mut Vec<RepoGroup>, entry: RepoEntry, mine: Vec<Row>| {
        let key = GroupKey::Unbound(machine_of(&entry).to_string());
        match groups.iter_mut().find(|group| group.key == key) {
            Some(group) => group.repos.push((entry, mine)),
            None => groups.push(RepoGroup {
                key,
                repos: vec![(entry, mine)],
            }),
        }
    };
    for entry in listed {
        let (mine, rest): (Vec<Row>, Vec<Row>) =
            rows.into_iter().partition(|row| row.entry == entry);
        rows = rest;
        add(&mut groups, entry, mine);
    }
    while let Some(first) = rows.first() {
        let entry = first.entry.clone();
        let (mine, rest): (Vec<Row>, Vec<Row>) =
            rows.into_iter().partition(|row| row.entry == entry);
        rows = rest;
        add(&mut groups, entry, mine);
    }
    groups
}

/// 把终端按工作区、仓库分组。同一个仓库在几个工作区里时只列在最前面的那个里；没有仓库可列的
/// 工作区不出现。返回分好的组，和不属于其中任何仓库的终端。
fn group_by_workspace(
    workspaces: Vec<(WorkspaceKey, Vec<RepoEntry>)>,
    mut rows: Vec<Row>,
) -> (Vec<RepoGroup>, Vec<Row>) {
    let mut seen: Vec<RepoEntry> = Vec::new();
    let mut groups: Vec<RepoGroup> = Vec::new();
    for (key, members) in workspaces {
        let mut repos: Vec<(RepoEntry, Vec<Row>)> = Vec::new();
        for entry in members {
            if seen.contains(&entry) {
                continue;
            }
            let (mine, rest): (Vec<Row>, Vec<Row>) =
                rows.into_iter().partition(|row| row.entry == entry);
            rows = rest;
            seen.push(entry.clone());
            repos.push((entry, mine));
        }
        if !repos.is_empty() {
            groups.push(RepoGroup {
                key: GroupKey::Workspace(key),
                repos,
            });
        }
    }
    (groups, rows)
}

/// 全部读到了的工作区：本机的在前，再是各台服务器的。1.16 以前现在用的工作区排最前；现在没有
/// 「现在用的」了（标签页不跟工作区走）。
fn workspace_order(app: &AppState) -> Vec<WorkspaceKey> {
    let mut machines: Vec<&String> = app.machines.keys().collect();
    machines.sort_by(|a, b| (a.as_str() != LOCAL, a).cmp(&(b.as_str() != LOCAL, b)));
    machines
        .into_iter()
        .flat_map(|machine| app.workspaces_on(machine).unwrap_or_default())
        .map(|(key, _, _)| key)
        .collect()
}

/// 这个终端是哪个仓库的第几个：会话的名字是仓库路径加序号（见 [`terminal::session_key`]）。
/// Windows 上的 kuaid 把名字统一成了小写、正斜杠，所以先拿知道的仓库去对；对不上就用名字里的路径。
fn owner(app: &AppState, machine: &str, key: &str) -> (RepoEntry, u32) {
    let known = app
        .machines
        .keys()
        .flat_map(|at| app.workspaces_on(at).unwrap_or_default())
        .flat_map(|(workspace, _, _)| app.members(&workspace))
        .chain(app.repos_on(machine).unwrap_or_default())
        .filter(|entry| machine_of(entry) == machine)
        .find_map(|entry| {
            let number = terminal::session_number(&entry.path, key)?;
            Some((entry, number))
        });
    known.unwrap_or_else(|| {
        let (path, number) = terminal::split_session_key(key);
        let entry = RepoEntry {
            server: (machine != LOCAL).then(|| machine.to_string()),
            path: path.to_string(),
        };
        (entry, number)
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{GroupKey, Program, RepoEntry, Row, TermId, WorkspaceKey, group_by_workspace};
    use super::{TermState, free_number, group_unbound, state_of};

    #[test]
    fn 未加入工作区的仓库按主机分节_没有终端的也列() {
        let entry = |server: Option<&str>, path: &str| RepoEntry {
            server: server.map(str::to_string),
            path: path.into(),
        };
        let row = |entry: RepoEntry, number: u32| Row {
            id: TermId {
                machine: entry.server.clone().unwrap_or_default(),
                term: format!("{}-{number}", entry.path),
            },
            entry,
            number,
            title: None,
            last: String::new(),
            cols: 80,
            rows: 24,
            alive: true,
            state: TermState::Idle,
            last_output: None,
            programs: None,
        };
        // 主机仓库列表里的：本机两个、s1 一个（按主机排好）；终端开在本机的 b、本机列表里没有的
        // 子仓库 a/sub、s1 列表里没有的 /x
        let listed = vec![
            entry(None, "C:/a"),
            entry(None, "C:/b"),
            entry(Some("s1"), "/c"),
        ];
        let rows = vec![
            row(entry(Some("s1"), "/x"), 1),
            row(entry(None, "C:/b"), 2),
            row(entry(None, "C:/a/sub"), 1),
            row(entry(None, "C:/b"), 1),
        ];
        let groups = group_unbound(listed, rows);
        let keys: Vec<&GroupKey> = groups.iter().map(|group| &group.key).collect();
        assert_eq!(
            keys,
            [
                &GroupKey::Unbound(String::new()),
                &GroupKey::Unbound("s1".into())
            ]
        );
        let view = |group: &super::RepoGroup| -> Vec<(String, Vec<u32>)> {
            group
                .repos
                .iter()
                .map(|(entry, rows)| {
                    let numbers = rows.iter().map(|row| row.number).collect();
                    (entry.path.clone(), numbers)
                })
                .collect()
        };
        assert_eq!(
            view(&groups[0]),
            [
                ("C:/a".to_string(), vec![]),
                ("C:/b".to_string(), vec![2, 1]),
                ("C:/a/sub".to_string(), vec![1]),
            ]
        );
        assert_eq!(
            view(&groups[1]),
            [("/c".to_string(), vec![]), ("/x".to_string(), vec![1])]
        );
        // 没有未加入工作区的仓库、也没有剩下的终端：一节都没有
        assert!(group_unbound(Vec::new(), Vec::new()).is_empty());
    }

    #[test]
    fn 终端里跑的是哪一类程序() {
        let row = |alive: bool, programs: Option<&[&str]>| Row {
            id: TermId {
                machine: String::new(),
                term: "t1".into(),
            },
            entry: RepoEntry {
                server: None,
                path: "C:/repo".into(),
            },
            number: 1,
            title: None,
            last: String::new(),
            cols: 80,
            rows: 24,
            alive,
            state: TermState::Idle,
            last_output: None,
            programs: programs.map(|names| names.iter().map(|name| name.to_string()).collect()),
        };
        assert_eq!(row(true, None).program(), Program::Unknown, "旧版 kuaid");
        assert_eq!(
            row(false, None).program(),
            Program::Shell,
            "旧版 kuaid、已结束"
        );
        assert_eq!(row(true, Some(&[])).program(), Program::Shell);
        assert_eq!(row(true, Some(&["claude"])).program(), Program::Agent);
        assert_eq!(row(true, Some(&["Codex"])).program(), Program::Agent);
        assert_eq!(
            row(true, Some(&["ping", "codex"])).program(),
            Program::Agent
        );
        assert_eq!(row(true, Some(&["cargo"])).program(), Program::Command);
        assert_eq!(
            row(false, Some(&["claude"])).program(),
            Program::Shell,
            "已结束"
        );
    }

    #[test]
    fn 同一个仓库在两台电脑的工作区里_合着看归前面的_只看一台时照样列() {
        let entry = |path: &str| RepoEntry {
            server: Some("s1".into()),
            path: path.into(),
        };
        let key = |machine: &str, id: &str| WorkspaceKey {
            machine: machine.into(),
            id: id.into(),
        };
        // 本机的工作区和 s1 自己的工作区里是同两个仓库（s1 上的）
        let local = (key("", "w1"), vec![entry("/a"), entry("/b")]);
        let remote = (key("s1", "w9"), vec![entry("/a"), entry("/b"), entry("/c")]);
        // 合着看：本机的在前，两个仓库归它，s1 的工作区只剩它独有的那个
        let (groups, _) = group_by_workspace(vec![local, remote.clone()], Vec::new());
        let paths = |repos: &[(RepoEntry, _)]| -> Vec<String> {
            repos.iter().map(|(entry, _)| entry.path.clone()).collect()
        };
        assert_eq!(paths(&groups[0].repos), ["/a", "/b"]);
        assert_eq!(paths(&groups[1].repos), ["/c"]);
        // 只看 s1：它的工作区里三个都在
        let (groups, _) = group_by_workspace(vec![remote], Vec::new());
        assert_eq!(groups.len(), 1);
        assert_eq!(paths(&groups[0].repos), ["/a", "/b", "/c"]);
        // 仓库全在别处的工作区：合着看时整组不出现
        let first = (key("", "w1"), vec![entry("/a")]);
        let second = (key("s1", "w9"), vec![entry("/a")]);
        let (groups, _) = group_by_workspace(vec![first, second], Vec::new());
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn 终端的三种状态() {
        let now = Instant::now() + Duration::from_secs(3600);
        let ago = |secs| Some(now - Duration::from_secs(secs));
        // 不知道什么时候输出过：空闲
        assert_eq!(state_of(None, None, now), TermState::Idle);
        // 10 秒以内有过输出：运行中，看没看过都一样
        assert_eq!(state_of(ago(3), None, now), TermState::Running);
        assert_eq!(state_of(ago(3), ago(1), now), TermState::Running);
        // 停下了、没看过：已停下
        assert_eq!(state_of(ago(10), None, now), TermState::Stopped);
        // 停下以前看过不算（之后又有过输出）
        assert_eq!(state_of(ago(30), ago(60), now), TermState::Stopped);
        // 停下以后看过：空闲
        assert_eq!(state_of(ago(30), ago(20), now), TermState::Idle);
        assert_eq!(state_of(ago(30), ago(30), now), TermState::Idle);
    }

    #[test]
    fn 新终端用最小的空号() {
        assert_eq!(free_number(&[]), 1);
        assert_eq!(free_number(&[1, 2]), 3);
        // 前面的结束了，空出来的号先用
        assert_eq!(free_number(&[2, 3]), 1);
        assert_eq!(free_number(&[1, 3]), 2);
    }
}
