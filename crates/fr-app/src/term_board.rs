//! 所有工作区里的终端现在怎么样了（会话页、标题栏上的个数、「待查看」的提醒都从这里读）。
//!
//! 终端开在各台电脑的 kuaid 里。连着哪台就订阅哪台的终端列表（`term_watch`，协议 11）：开了、结束了、
//! 有输出变成没输出时 kuaid 推一份过来，每个终端带着「多久没有输出了」、屏幕上最后一行字和除了 shell
//! 还在跑的程序。不用把每个终端都接上。
//!
//! 状态（[`TermState`]，`docs/design/sessions-status/` 第 1 节，方向 B）看输出和里面跑的程序：
//!
//! - 运行中：[`QUIET`] 以内有过输出。
//! - 空闲：只有 shell（回到了提示符），或者还没输出过。
//! - 静默：有程序在跑、超过 [`QUIET`] 没有输出。普通命令（`pnpm dev`、`cargo test`）一直是静默，
//!   不提醒：长时间不出字是常态。
//! - 待查看：AI 终端（[`Kind::Agent`]）静默超过设置里给这家定的时长，之后还没看过。只有它进标题栏的
//!   角标、发提醒。时长默认 Claude Code、Codex 3 分钟，别的 2 分钟（设置 → 终端 → 提醒）。
//! - 状态未知：所在主机断开了。列表留着（会话页里灰掉，写「未连接」带「重新连接」），不进角标；
//!   重连以后接着算。
//!
//! 「看过」：它正显示在窗口里（仓库标签页的终端页上选中的那个，见 [`TermBoard::set_shown`]），
//! 在会话页里选中过，或者从会话页前往过。会话页里正选中的那个不提醒、不计入标题栏上的个数，
//! 但在列表里仍写着「待查看」，选到别的、离开那一页以后才变：不然一打开会话页，自动选中的那个
//! 马上变了，就看不出是哪个在等。
//!
//! 离开期间：看过的时刻和「最后一次还在收这台的列表」的时刻记在盘上（[`term_memory`]）。
//! 重开 Kuai、断线重连以后，离开期间有过新输出的 AI 终端标成「离开期间有更新」（[`Row::away`]），
//! 进角标，几台主机陆续连上的合成一条提醒（[`AwayUpdates`]）；看过就去掉。设置里可以关，关了
//! 离开期间停下的都算看过。从来没记过的（第一次用这一版）也都算看过：只提醒这次运行期间停下来的。
//!
//! 不准的情况：AI 程序想得久、跑不出字的命令时也会变成静默、待查看（悬停的提示里说了）；Windows
//! 上 npm 装的程序进程名是 node，要靠「从哪个预设开的」认出是哪家（[`kind_of`]）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fr_channel::{Connection, TermInfo};
use fr_core::i18n::tr;
use fr_core::tf;
use futures::StreamExt as _;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::machines::{LOCAL, WorkspaceKey, machine_of};
use crate::presets;
use crate::settings::{RepoEntry, TermAlerts, TermSlot};
use crate::term_memory::{self, MachineMemo, Memory, SeenMemo};
use crate::term_names;
use crate::terminal;

/// 这么久没有新输出就不算在运行了
pub const QUIET: Duration = Duration::from_secs(10);
/// 重新算一遍状态的间隔
const TICK: Duration = Duration::from_secs(1);
/// 每隔几秒问一次各台主机的 kuaid 还在不在（心跳，见 [`TermBoard::heartbeat`]）
const BEAT_EVERY: u32 = 5;
/// 心跳多久没回算连接不稳。ssh 自己要 30 秒才认定断了（`ServerAliveInterval=10`、`CountMax=3`），
/// 这半分钟里画面停着、状态写「静默」，看不出是断网
const BEAT_TIMEOUT: Duration = Duration::from_secs(8);
/// 状态没变也隔这么多次重画一遍：界面上「静默了多久」的字要跟着走
const REDRAW_EVERY: u32 = 30;
/// 「看过」有变化时隔这么多次存一次盘
const SAVE_EVERY: u32 = 30;
/// 启动、重连时各台主机陆续连上：离开期间有更新的，这么久之内的合成一条提醒
const AWAY_GATHER: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TermState {
    /// [`QUIET`] 以内有过输出
    Running,
    /// 有程序在跑、超过 [`QUIET`] 没有输出：普通命令，没到时长、或者已经看过的 AI 终端
    Quiet,
    /// AI 终端静默超过所设时长，之后还没看过
    Waiting,
    /// 只有 shell（回到了提示符），或者还没输出过
    Idle,
    /// 所在主机没连着：终端还在那台上跑，不知道现在怎么样
    Unknown,
}

/// 认作 AI 助手的程序分几档，提醒的时长各设各的。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Agent {
    Claude,
    Codex,
    OpenCode,
    /// 别的（[`AGENTS`] 里的）
    Other,
}

impl Agent {
    /// 进程名（或者命令的第一个词）是哪家的；不是 AI 程序是 None。
    pub fn of(program: &str) -> Option<Agent> {
        let name = program.trim().to_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        match name {
            "claude" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            "opencode" => Some(Agent::OpenCode),
            _ if AGENTS.contains(&name) => Some(Agent::Other),
            _ => None,
        }
    }

    /// 静默多久算「待查看」（设置里定的）。
    pub fn wait(self, alerts: &TermAlerts) -> Duration {
        let minutes = match self {
            Agent::Claude => alerts.claude,
            Agent::Codex => alerts.codex,
            Agent::OpenCode => alerts.opencode,
            Agent::Other => alerts.other,
        };
        Duration::from_secs(u64::from(minutes.max(1)) * 60)
    }
}

/// 提醒上算哪一类：只有 AI 终端静默久了才提醒。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// 只有 shell（回到了提示符），或者程序已经结束
    Shell,
    /// 别的命令，不提醒；不知道在跑什么（旧版 kuaid）也算这一类
    Command,
    Agent(Agent),
}

/// 进程名是这些的认不出是什么程序（Windows 上 npm 装的程序叫 node，脚本叫 python）：要看这个终端
/// 是从哪个预设开的。
const LAUNCHERS: &[&str] = &[
    "node", "bun", "deno", "python", "python3", "py", "cmd", "sh", "bash", "npx", "pnpm",
];

/// 终端里跑的是哪一类：先看进程名，认不出（node 这类）再看这个终端是从哪个预设开的。
pub fn kind_of(programs: Option<&[String]>, alive: bool, start: Option<&presets::Start>) -> Kind {
    if !alive {
        return Kind::Shell;
    }
    let Some(programs) = programs else {
        return Kind::Command;
    };
    if programs.is_empty() {
        return Kind::Shell;
    }
    if let Some(agent) = programs.iter().find_map(|name| Agent::of(name)) {
        return Kind::Agent(agent);
    }
    let launcher = programs.iter().any(|name| {
        let name = name.trim().to_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        name.is_empty() || LAUNCHERS.contains(&name)
    });
    let started = start.and_then(|start| Agent::of(&presets::program_of(&start.command)));
    match (launcher, started) {
        (true, Some(agent)) => Kind::Agent(agent),
        _ => Kind::Command,
    }
}

/// 按最近一次输出、最近一次看它的时刻和里面跑的程序算状态。`wait`：AI 终端静默多久算待查看。
pub fn state_of(
    last_output: Option<Instant>,
    seen: Option<Instant>,
    kind: Kind,
    wait: Duration,
    now: Instant,
) -> TermState {
    let Some(at) = last_output else {
        return TermState::Idle;
    };
    let quiet = now.saturating_duration_since(at);
    if quiet < QUIET {
        return TermState::Running;
    }
    match kind {
        Kind::Shell => TermState::Idle,
        Kind::Command => TermState::Quiet,
        Kind::Agent(_) if quiet >= wait && !seen.is_some_and(|seen| seen >= at) => {
            TermState::Waiting
        }
        Kind::Agent(_) => TermState::Quiet,
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
    /// 并排查看的几格（会话页、仓库终端页）：都算看过
    Split(Vec<TermId>),
}

/// 读到的一个终端。
struct Listed {
    info: TermInfo,
    /// 最近一次输出的时刻，按收到时的「多久没有输出」倒推的；不知道是 None
    last_output: Option<Instant>,
    /// 它是哪个仓库的第几个终端（见 [`owner`]）。列表推过来、工作区和仓库列表变了时算好：原来每次
    /// 画都给每个终端把所有工作区的仓库对一遍，一帧里还要算好几遍
    owner: (RepoEntry, u32),
    /// 开出来的时刻（这台电脑的 Unix 秒），第一次读到时按「开了多久」倒推，之后不变
    born: u64,
}

/// 列表里的一行。
#[derive(Clone)]
pub struct Row {
    pub id: TermId,
    /// 它是哪个仓库的终端
    pub entry: RepoEntry,
    /// 那个仓库的第几个终端
    pub number: u32,
    /// 列表、标签、提醒里写的名字：起过的名字；没起过是程序的标题或预设的名字（同一个仓库里重名的
    /// 第二个起加序号），都没有是「终端 N」
    pub label: SharedString,
    /// 名字是起的（重命名过）
    pub named: bool,
    /// 屏幕上最后一行有字的内容
    pub last: String,
    pub cols: u16,
    pub rows: u16,
    /// 里面的程序还在跑
    pub alive: bool,
    pub state: TermState,
    /// 提醒上算哪一类
    pub kind: Kind,
    /// 离开期间（重开 Kuai、断线重连）有过新输出，还没看过
    pub away: bool,
    /// 所在主机连着
    pub online: bool,
    pub last_output: Option<Instant>,
    /// 开出来的时刻（这台电脑的 Unix 秒）
    pub born: u64,
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
pub const AGENTS: &[&str] = &[
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
        if let Kind::Agent(_) = self.kind {
            return Program::Agent;
        }
        let Some(programs) = &self.programs else {
            return Program::Unknown;
        };
        if programs.is_empty() {
            return Program::Shell;
        }
        let agent = programs.iter().any(|name| Agent::of(name).is_some());
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

    /// 要人看一眼：待查看，或者离开期间有更新（主机没连着的不算：状态未知）。
    pub fn wants_look(&self) -> bool {
        self.online && (self.state == TermState::Waiting || self.away)
    }

    /// 静默、待查看了多久（从最后一次输出算）；在运行、不知道的是 None。
    pub fn quiet_for(&self) -> Option<Duration> {
        if !self.alive || !self.online {
            return None;
        }
        match self.state {
            TermState::Quiet | TermState::Waiting => Some(self.last_output?.elapsed()),
            _ => None,
        }
    }
}

/// 一段时长粗略地说：40 秒、3 分钟、2 小时、1 天。
pub fn span(duration: Duration) -> String {
    let secs = duration.as_secs();
    match secs {
        0..60 => tf!("{n} 秒", n = secs.max(1).to_string()),
        60..3600 => tf!("{n} 分钟", n = (secs / 60).to_string()),
        3600..86400 => tf!("{n} 小时", n = (secs / 3600).to_string()),
        _ => tf!("{n} 天", n = (secs / 86400).to_string()),
    }
}

/// 状态写成一个词（行尾、标签、提示里）：静默的、待查看的带上多久。
pub fn state_words(row: &Row) -> String {
    if !row.online {
        return tr("状态未知").to_string();
    }
    if !row.alive {
        return tr("已退出").to_string();
    }
    match (row.state, row.quiet_for()) {
        (TermState::Running, _) => tr("运行中").to_string(),
        (TermState::Waiting, Some(quiet)) => tf!("待查看 · {span}", span = span(quiet)),
        (TermState::Waiting, None) => tr("待查看").to_string(),
        (TermState::Quiet, Some(quiet)) => tf!("静默 {span}", span = span(quiet)),
        (TermState::Quiet, None) => tr("静默").to_string(),
        (TermState::Idle, _) => tr("空闲").to_string(),
        (TermState::Unknown, _) => tr("状态未知").to_string(),
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

/// 一个不在眼前的 AI 终端静默久了，等人看一眼（提醒，带「前往」）。
pub struct Waiting(pub Row);

/// 重开 Kuai、重新连上主机以后，离开期间有过新输出的终端（几台陆续连上的合成一条）。
pub struct AwayUpdates(pub usize);

pub struct TermBoard {
    app: Entity<AppState>,
    lists: HashMap<String, Vec<Listed>>,
    /// 订阅各台电脑终端列表的任务，和订阅用的连接；丢弃就不订阅了
    watches: HashMap<String, (Arc<Connection>, Task<()>)>,
    /// 断开了、列表还留着的主机（上面的终端状态未知）
    offline: HashSet<String>,
    /// 心跳发出去还没回的：哪台、什么时候发的、经哪个连接发的
    beats: HashMap<String, (Instant, Arc<Connection>)>,
    /// 心跳超过 [`BEAT_TIMEOUT`] 没回的主机：还没断开，但多半不通了（上面的终端状态未知，不提醒）
    unstable: HashSet<String>,
    /// 这次运行中断开的主机：断开的时刻（Unix 毫秒）；重连以后比它晚的输出算离开期间的
    until: HashMap<String, u64>,
    seen: HashMap<TermId, Instant>,
    /// 离开期间有更新、还没看过的
    away: HashSet<TermId>,
    /// 上一次算出来的状态：和这一次不一样才重画、才提醒
    states: HashMap<TermId, TermState>,
    /// 各终端提醒上算哪一类、列表里写什么名字（列表到了、每秒重算状态时一起算：要读预设和设置）
    kinds: HashMap<TermId, Kind>,
    labels: HashMap<TermId, (SharedString, bool)>,
    shown: Option<Shown>,
    /// 上次运行（和这次断开过的主机）存下的看过的时刻、离开的时刻
    memory: Memory,
    /// `seen`、`until` 变了，还没存
    dirty: bool,
    /// 等着合成一条提醒的离开期间有更新的个数，和到时候发出去的任务
    away_pending: usize,
    away_task: Option<Task<()>>,
    _observe: Subscription,
    _tick: Task<()>,
}

impl EventEmitter<Waiting> for TermBoard {}
impl EventEmitter<AwayUpdates> for TermBoard {}

/// 主窗口的那一份 [`TermBoard`]：各个仓库页（包括进到子模块、工作树里的）从这里拿，跟着同一份
/// 终端列表走。
pub struct BoardHandle(pub Entity<TermBoard>);

impl Global for BoardHandle {}

fn now_secs() -> u64 {
    term_memory::now_ms() / 1000
}

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
                let save = count % SAVE_EVERY == 0;
                let beat = count % BEAT_EVERY == 0;
                let done = this.update(cx, |this, cx| {
                    this.heartbeat(beat, cx);
                    this.recompute(cx);
                    if redraw && !this.lists.is_empty() {
                        cx.notify();
                    }
                    if save && (this.dirty || !this.watches.is_empty()) {
                        this.save_memory();
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
            offline: HashSet::new(),
            beats: HashMap::new(),
            unstable: HashSet::new(),
            until: HashMap::new(),
            seen: HashMap::new(),
            away: HashSet::new(),
            states: HashMap::new(),
            kinds: HashMap::new(),
            labels: HashMap::new(),
            shown: None,
            memory: term_memory::load(),
            dirty: false,
            away_pending: 0,
            away_task: None,
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
        let mut cut: Vec<String> = Vec::new();
        self.watches.retain(|key, (conn, _)| {
            let kept = links.iter().any(|(k, c)| k == key && Arc::ptr_eq(c, conn));
            if !kept {
                match links.iter().any(|(k, _)| k == key) {
                    true => replaced.push(key.clone()),
                    false => cut.push(key.clone()),
                }
            }
            kept
        });
        // 同一个键换了连接（改了地址，可能已经是另一台电脑）：旧连接读到的列表不能留到新的推过来，
        // 不然这期间列着的是旧那台的终端，经新连接去操作的却是这一台
        for key in replaced {
            self.forget(&key);
        }
        // 断开了：列表留着，状态未知
        for key in cut {
            self.went_offline(&key);
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
            // 新接上的连接：旧连接上的心跳、「连接不稳」不算了
            self.beats.remove(&key);
            self.unstable.remove(&key);
            self.watches.insert(key, (conn, task));
            changed = true;
        }
        // 列表留着的主机已经移除了（或者认出是重复的）：不用再列
        let gone: Vec<String> = {
            let app = self.app.read(cx);
            self.lists
                .keys()
                .filter(|key| !self.watches.contains_key(*key))
                .filter(|key| key.as_str() != LOCAL && app.machine(key).is_none())
                .cloned()
                .collect()
        };
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
        self.offline.remove(machine);
        self.unstable.remove(machine);
        self.beats.remove(machine);
        self.until.remove(machine);
        self.seen.retain(|id, _| id.machine != machine);
        self.away.retain(|id| id.machine != machine);
        self.states.retain(|id, _| id.machine != machine);
    }

    /// 断开了一台：终端还在那台上跑，列表留着（状态未知），记下断开的时刻（重连以后比它晚的输出算
    /// 离开期间的）。
    fn went_offline(&mut self, machine: &str) {
        if !self.lists.contains_key(machine) {
            return;
        }
        log::info!(
            "{} 断开了，终端列表留着",
            if machine.is_empty() {
                "本机"
            } else {
                machine
            }
        );
        self.offline.insert(machine.to_string());
        self.until
            .insert(machine.to_string(), term_memory::now_ms());
        self.dirty = true;
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
            self.went_offline(machine);
            self.recompute(cx);
            cx.notify();
        }
    }

    /// 一台电脑推来一份终端列表。`first`：这次订阅的第一份。
    fn arrived(&mut self, machine: &str, list: Vec<TermInfo>, first: bool, cx: &mut Context<Self>) {
        // 上次运行时存下的从预设开的终端，趁「开了多久」还是这一刻的对一遍
        presets::reconcile(machine, &list, cx);
        let secs = now_secs();
        self.app.update(cx, |app, cx| {
            let mut names = app.settings.term_names.clone();
            if term_names::reconcile(&mut names, machine, &list, secs) {
                app.update_settings(cx, |settings| settings.term_names = names);
            }
        });
        let now = Instant::now();
        let old = self.lists.remove(machine).unwrap_or_default();
        let app = self.app.read(cx);
        let listed: Vec<Listed> = list
            .into_iter()
            .map(|info| {
                let before = old.iter().find(|listed| listed.info.term == info.term);
                // 正在输出时 kuaid 隔几秒才推一次，这个时刻会落后几秒；输出停下以后它还会推一次，
                // 到判断状态的时候（停下 10 秒后）已经是准的
                let fresh = info
                    .idle_ms
                    .and_then(|idle| now.checked_sub(Duration::from_millis(idle)));
                let last_output = fresh.or(before.and_then(|listed| listed.last_output));
                let born = before
                    .map(|listed| listed.born)
                    .unwrap_or_else(|| secs.saturating_sub(info.age));
                let owner = owner(app, machine, &info.key);
                Listed {
                    info,
                    last_output,
                    owner,
                    born,
                }
            })
            .collect();
        // 不在了的终端不用再记着
        let here = |id: &TermId| {
            id.machine != machine || listed.iter().any(|listed| listed.info.term == id.term)
        };
        self.seen.retain(|id, _| here(id));
        self.away.retain(|id| here(id));
        self.states.retain(|id, _| here(id));
        self.offline.remove(machine);
        self.lists.insert(machine.to_string(), listed);
        if first {
            self.came_back(machine, now, cx);
        }
        self.recompute(cx);
        // 标题、最后一行也可能变了
        cx.notify();
    }

    /// 刚连上一台（启动时、断线重连）：接上次记的看过的时刻；离开期间有新输出的 AI 终端标出来
    /// （设置里关了就算看过），别的离开期间停下的都算看过。第一次连这台（没记过）时已经停着的都算
    /// 看过：只提醒这次运行期间停下来的。
    fn came_back(&mut self, machine: &str, now: Instant, cx: &mut Context<Self>) {
        let now_ms = term_memory::now_ms();
        let memo = self.memory.machines.get(machine).cloned();
        // 这次运行中断开过的按断开的时刻，不然按上次运行时记的
        let until = self
            .until
            .remove(machine)
            .or(memo.as_ref().map(|memo| memo.until));
        let mark_away = self.app.read(cx).settings.term_alerts.away;
        self.refresh_meta(cx);
        let mut flagged = 0;
        let Some(list) = self.lists.get(machine) else {
            return;
        };
        for item in list {
            let id = TermId {
                machine: machine.to_string(),
                term: item.info.term.clone(),
            };
            // 上次运行时看过的（这次运行里看过的还在 `seen` 里）
            if !self.seen.contains_key(&id)
                && let Some(at) = memo
                    .as_ref()
                    .and_then(|memo| memo.seen(&id.term, item.born))
                && let Some(at) = term_memory::from_ms(at, now, now_ms)
            {
                self.seen.insert(id.clone(), at);
            }
            let stopped = item
                .last_output
                .is_some_and(|at| now.saturating_duration_since(at) >= QUIET);
            let Some(until) = until else {
                if stopped {
                    self.seen.insert(id, now);
                }
                continue;
            };
            let after_leaving = item
                .last_output
                .is_some_and(|at| term_memory::to_ms(at, now, now_ms) > until);
            if !after_leaving {
                // 离开期间没动过：接着离开前的样子（没看过的照样待查看）
                continue;
            }
            let agent = matches!(self.kinds.get(&id), Some(Kind::Agent(_)));
            if agent && mark_away {
                if self.away.insert(id) {
                    flagged += 1;
                }
            } else if stopped {
                self.seen.insert(id, now);
            }
        }
        if flagged > 0 {
            log::info!(
                "{}：离开期间 {flagged} 个终端有新输出",
                if machine.is_empty() {
                    "本机"
                } else {
                    machine
                }
            );
            self.away_pending += flagged;
            if self.away_task.is_none() {
                self.away_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(AWAY_GATHER).await;
                    this.update(cx, |this, cx| {
                        this.away_task = None;
                        // 合起来的这几秒里已经看过的不算
                        let pending = std::mem::take(&mut this.away_pending);
                        let left = this.away_count().min(pending);
                        if left > 0 {
                            cx.emit(AwayUpdates(left));
                        }
                    })
                    .ok();
                }));
            }
        }
        self.dirty = true;
    }

    /// 各终端提醒上算哪一类、列表里写什么名字：起过名字的用起的；别的按程序的标题、预设的名字，同一个
    /// 仓库里重名的，序号最小的那个不加，别的后面加上自己的序号（Claude Code、Claude Code 2，和仓库终端页
    /// 侧栏里那一格序号一样）。起了名字的照样占着默认名字：改一个的名字、结束一个，别的名字都不变。
    fn refresh_meta(&mut self, cx: &App) {
        let names = &self.app.read(cx).settings.term_names;
        self.kinds.clear();
        self.labels.clear();
        for (machine, list) in &self.lists {
            let mut order: Vec<&Listed> = list.iter().collect();
            order.sort_by(|a, b| (&a.owner.0.path, a.owner.1).cmp(&(&b.owner.0.path, b.owner.1)));
            // 先算各个终端默认叫什么，再看同一个仓库里有没有重名的
            let mut bases: Vec<(TermId, &Listed, Option<SharedString>)> = Vec::new();
            for listed in order {
                let id = TermId {
                    machine: machine.clone(),
                    term: listed.info.term.clone(),
                };
                let slot = TermSlot {
                    entry: listed.owner.0.clone(),
                    number: listed.owner.1,
                };
                let start = presets::started(&slot, Some(&listed.info.term), cx);
                let kind = kind_of(
                    listed.info.programs.as_deref(),
                    listed.info.alive,
                    start.as_ref(),
                );
                self.kinds.insert(id.clone(), kind);
                let title = listed
                    .info
                    .title
                    .as_deref()
                    .filter(|title| !title.is_empty());
                bases.push((id, listed, presets::display_name(title, start.as_ref())));
            }
            for (id, listed, base) in &bases {
                if let Some(name) = term_names::find(names, machine, &listed.info.term, listed.born)
                {
                    self.labels
                        .insert(id.clone(), (name.to_string().into(), true));
                    continue;
                }
                let label = match base {
                    Some(base) => {
                        let first = bases
                            .iter()
                            .filter(|(_, other, other_base)| {
                                other.owner.0 == listed.owner.0 && other_base.as_ref() == Some(base)
                            })
                            .map(|(_, other, _)| other.owner.1)
                            .min();
                        match first == Some(listed.owner.1) {
                            true => base.clone(),
                            false => format!("{base} {}", listed.owner.1).into(),
                        }
                    }
                    None => tf!("终端 {n}", n = listed.owner.1.to_string()).into(),
                };
                self.labels.insert(id.clone(), (label, false));
            }
        }
    }

    /// 重新算一遍各个终端的状态：变了就重画；不在眼前的 AI 终端变成待查看的，说一声。
    fn recompute(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        self.refresh_meta(cx);
        let shown = self.shown_ids();
        // 仓库终端页上正显示着的、会话页里正往里输入的、并排着的一直算看过。会话页里只是选中的那个
        // 先不记：它在列表里还得写着「待查看」，选到别的、离开这一页时才记（见 `set_shown`）
        if matches!(
            self.shown,
            Some(Shown::Repo(..) | Shown::Typing(_) | Shown::Split(_))
        ) {
            for id in &shown {
                self.see(id.clone(), now);
            }
        }
        let alerts = self.app.read(cx).settings.term_alerts.clone();
        let mut changed = false;
        let mut waiting = Vec::new();
        for (machine, list) in &self.lists {
            // 心跳超时的也当状态未知：画面停着不是终端停了，不能算「待查看」去提醒
            let online = !self.offline.contains(machine) && !self.unstable.contains(machine);
            for item in list {
                let id = TermId {
                    machine: machine.clone(),
                    term: item.info.term.clone(),
                };
                let kind = self.kinds.get(&id).copied().unwrap_or(Kind::Command);
                let wait = match kind {
                    Kind::Agent(agent) => agent.wait(&alerts),
                    _ => Duration::MAX,
                };
                let state = match online {
                    false => TermState::Unknown,
                    true => state_of(
                        item.last_output,
                        self.seen.get(&id).copied(),
                        kind,
                        wait,
                        now,
                    ),
                };
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
                let watching = shown.contains(&id);
                let from_live = matches!(before, Some(TermState::Running | TermState::Quiet));
                if state == TermState::Waiting && from_live && !watching && !self.away.contains(&id)
                {
                    waiting.push(id);
                }
            }
        }
        if changed {
            cx.notify();
        }
        for id in waiting {
            if let Some(row) = self.row(&id) {
                cx.emit(Waiting(row));
            }
        }
    }

    /// 看过了：记下时刻，去掉「离开期间有更新」。
    fn see(&mut self, id: TermId, now: Instant) {
        self.away.remove(&id);
        self.seen.insert(id, now);
        self.dirty = true;
    }

    /// 正显示在窗口里的是哪个终端（窗口不在前台、显示的不是终端时是 None）。
    pub fn set_shown(&mut self, shown: Option<Shown>, cx: &mut Context<Self>) {
        if self.shown != shown {
            let now = Instant::now();
            // 会话页里刚才选中的那个：看过了
            if let Some(Shown::Term(id) | Shown::Typing(id)) = &self.shown {
                let id = id.clone();
                self.see(id, now);
            }
            // 选中就去掉「离开期间有更新」（待查看要等选到别的才变，见上面）
            if let Some(Shown::Term(id)) = &shown
                && self.away.remove(id)
            {
                self.dirty = true;
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
                Some(Shown::Split(ids)) => log::info!("正显示的终端：并排的 {} 个", ids.len()),
            }
            self.recompute(cx);
            // 状态没变，标题栏上的个数也可能变了（正选中的那个不算）
            cx.notify();
        }
    }

    /// 正显示着的终端（并排时是几个）。
    fn shown_ids(&self) -> Vec<TermId> {
        let one = |entry: &RepoEntry, number: u32| -> Option<TermId> {
            let machine = machine_of(entry);
            let term = self.lists.get(machine)?.iter().find(|listed| {
                terminal::session_number(&entry.path, &listed.info.key) == Some(number)
            })?;
            Some(TermId {
                machine: machine.to_string(),
                term: term.info.term.clone(),
            })
        };
        match &self.shown {
            None => Vec::new(),
            Some(Shown::Term(id) | Shown::Typing(id)) => vec![id.clone()],
            Some(Shown::Split(ids)) => ids.clone(),
            Some(Shown::Repo(entry, number)) => one(entry, *number).into_iter().collect(),
        }
    }

    /// 看过了（从会话页前往它、选中它）。
    pub fn mark_seen(&mut self, id: &TermId, cx: &mut Context<Self>) {
        self.see(id.clone(), Instant::now());
        self.recompute(cx);
        cx.notify();
    }

    /// 离开期间有更新的都算看过了（侧栏顶上那条的「全部标为已看」）。
    pub fn clear_away(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let ids: Vec<TermId> = self.away.drain().collect();
        for id in ids {
            self.see(id, now);
        }
        self.recompute(cx);
        cx.notify();
    }

    /// 离开期间有更新、还没看过的有几个（主机没连着的不算）。
    pub fn away_count(&self) -> usize {
        self.away
            .iter()
            .filter(|id| !self.offline.contains(&id.machine))
            .count()
    }

    /// 那台电脑的终端列表订阅着没有（连着才订阅）。
    pub fn watching(&self, machine: &str) -> bool {
        self.watches.contains_key(machine)
    }

    /// 断开了、列表还留着的主机（按名字排好的键）。
    pub fn offline_machines(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.offline.iter().cloned().collect();
        keys.sort();
        keys
    }

    /// 这台主机断开了、列表还留着（上面的终端状态未知）。
    pub fn is_offline(&self, machine: &str) -> bool {
        self.offline.contains(machine)
    }

    /// 这台主机的心跳超时了：还连着，但多半不通（见 [`Self::heartbeat`]）。
    pub fn is_unstable(&self, machine: &str) -> bool {
        self.unstable.contains(machine) && !self.offline.contains(machine)
    }

    /// 心跳：每秒看一眼发出去的有没有超时（超时的标成连接不稳），`send` 时给没在等的主机各发一个。
    /// 回了就去掉标记。只问订阅着终端列表的主机（本机的 kuaid 也问：它卡住了一样看不出来）。
    fn heartbeat(&mut self, send: bool, cx: &mut Context<Self>) {
        let now = Instant::now();
        // 换了连接、不订阅了的：那次心跳不算了
        let watches = &self.watches;
        self.beats.retain(|key, (_, conn)| {
            watches
                .get(key)
                .is_some_and(|(current, _)| Arc::ptr_eq(current, conn))
        });
        let late: HashSet<String> = (self.beats.iter())
            .filter(|(_, (sent, _))| now.duration_since(*sent) >= BEAT_TIMEOUT)
            .map(|(key, _)| key.clone())
            .collect();
        let before = self.unstable.len();
        self.unstable.retain(|key| self.watches.contains_key(key));
        for key in &late {
            if self.unstable.insert(key.clone()) {
                log::info!(
                    "{key} 的心跳 {} 秒没有回应，标为连接不稳",
                    BEAT_TIMEOUT.as_secs()
                );
            }
        }
        if self.unstable.len() != before || !late.is_empty() {
            cx.notify();
        }
        if !send {
            return;
        }
        let idle: Vec<(String, Arc<Connection>)> = (self.watches.iter())
            .filter(|(key, _)| !self.beats.contains_key(*key))
            .map(|(key, (conn, _))| (key.clone(), conn.clone()))
            .collect();
        for (key, conn) in idle {
            self.beats.insert(key.clone(), (now, conn.clone()));
            let ping = conn.ping();
            cx.spawn(async move |this, cx| {
                let answered = ping.await.is_ok();
                this.update(cx, |this, cx| {
                    // 还是这一次的心跳（没换连接）才算
                    let current = (this.beats.get(&key))
                        .is_some_and(|(_, sent_on)| Arc::ptr_eq(sent_on, &conn));
                    if !current {
                        return;
                    }
                    this.beats.remove(&key);
                    // 回了错（连接断了）：交给订阅断开那边去标「未连接」
                    if answered && this.unstable.remove(&key) {
                        log::info!("{key} 的心跳恢复了");
                        this.recompute(cx);
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    /// 订阅那台电脑用的连接（预览经它接上终端）。
    pub fn conn(&self, machine: &str) -> Option<Arc<Connection>> {
        self.watches.get(machine).map(|(conn, _)| conn.clone())
    }

    /// 要人看一眼的有几个：待查看、离开期间有更新（正显示着的不算：正看着它）。
    pub fn attention(&self) -> usize {
        let shown = self.shown_ids();
        let mut ids: HashSet<&TermId> = self
            .states
            .iter()
            .filter(|(_, state)| **state == TermState::Waiting)
            .map(|(id, _)| id)
            .collect();
        ids.extend(
            self.away
                .iter()
                .filter(|id| !self.offline.contains(&id.machine)),
        );
        ids.retain(|id| !shown.contains(id));
        ids.len()
    }

    /// 给终端起名字（空的是去掉起的名字）。
    pub fn rename(&mut self, id: &TermId, name: &str, cx: &mut Context<Self>) {
        let Some(row) = self.row(id) else {
            return;
        };
        let slot = row.slot();
        self.app.update(cx, |app, cx| {
            let mut names = app.settings.term_names.clone();
            if term_names::rename(&mut names, &id.machine, &slot, &id.term, row.born, name) {
                app.update_settings(cx, |settings| settings.term_names = names);
            }
        });
        self.refresh_meta(cx);
        cx.notify();
    }

    /// 要在 Kuai 里重启 kuaid 里的 `term`：起的名字跟到新开的那个上。
    pub fn restarting(&mut self, id: &TermId, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            let mut names = app.settings.term_names.clone();
            if term_names::restarting(&mut names, &id.machine, &id.term) {
                app.update_settings(cx, |settings| settings.term_names = names);
            }
        });
    }

    /// 把看过的时刻、各台主机最后一次还在收列表的时刻存盘（隔一会儿、退出时）。这次没连过的主机
    /// 照旧留着上次的。
    pub fn save_memory(&mut self) {
        let (now, now_ms) = (Instant::now(), term_memory::now_ms());
        for (machine, list) in &self.lists {
            let until = match self.offline.contains(machine) {
                true => self.until.get(machine).copied().unwrap_or(now_ms),
                false => now_ms,
            };
            let seen = list
                .iter()
                .filter_map(|listed| {
                    let id = TermId {
                        machine: machine.clone(),
                        term: listed.info.term.clone(),
                    };
                    let at = self.seen.get(&id)?;
                    Some(SeenMemo {
                        term: listed.info.term.clone(),
                        born: listed.born,
                        at: term_memory::to_ms(*at, now, now_ms),
                    })
                })
                .collect();
            self.memory
                .machines
                .insert(machine.clone(), MachineMemo { until, seen });
        }
        term_memory::save(&self.memory);
        self.dirty = false;
    }

    fn make_row(&self, machine: &str, listed: &Listed) -> Row {
        let id = TermId {
            machine: machine.to_string(),
            term: listed.info.term.clone(),
        };
        let (entry, number) = listed.owner.clone();
        let online = !self.offline.contains(machine);
        let state = match online {
            false => TermState::Unknown,
            true => self.states.get(&id).copied().unwrap_or(TermState::Idle),
        };
        let (label, named) = self
            .labels
            .get(&id)
            .cloned()
            .unwrap_or_else(|| (tf!("终端 {n}", n = number.to_string()).into(), false));
        Row {
            away: self.away.contains(&id),
            kind: self.kinds.get(&id).copied().unwrap_or(Kind::Command),
            id,
            entry,
            number,
            label,
            named,
            last: listed.info.last.clone(),
            cols: listed.info.cols,
            rows: listed.info.rows,
            alive: listed.info.alive,
            state,
            online,
            last_output: listed.last_output,
            born: listed.born,
            programs: listed.info.programs.clone(),
        }
    }

    /// 在跑着的 AI 终端（主机连着的）：关窗口前问一句用。
    pub fn agents(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .all_rows()
            .into_iter()
            .filter(|row| row.alive && row.online && matches!(row.kind, Kind::Agent(_)))
            .collect();
        // 要人看一眼的排前面
        rows.sort_by_key(|row| !row.wants_look());
        rows
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
    /// 里时只列在第一个里。没连过的主机上的仓库不列：上面的终端不知道，也开不了；连过又断开的照列
    /// （终端状态未知）。
    pub fn repos(&self, cx: &App) -> Vec<RepoGroup> {
        let app = self.app.read(cx);
        let order = workspace_order(app);
        let grouped: HashSet<RepoEntry> = order.iter().flat_map(|key| app.members(key)).collect();
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

    /// 侧栏里列不列：本机的总列，主机上的要那台连着，或者断开了、列表还留着（灰掉，见
    /// [`TermState::Unknown`]）。
    fn reachable(&self, entry: &RepoEntry) -> bool {
        let machine = machine_of(entry);
        entry.server.is_none() || self.watching(machine) || self.offline.contains(machine)
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

/// 终端按仓库分好（各仓库的终端保持原来的先后），会话页每次重画都要分组：不再对每个仓库扫一遍
/// 全部终端。
struct ByRepo {
    rows: HashMap<RepoEntry, Vec<Row>>,
    /// 各仓库第一次出现的先后
    order: Vec<RepoEntry>,
}

impl ByRepo {
    fn new(rows: Vec<Row>) -> Self {
        let mut by = ByRepo {
            rows: HashMap::new(),
            order: Vec::new(),
        };
        for row in rows {
            match by.rows.get_mut(&row.entry) {
                Some(mine) => mine.push(row),
                None => {
                    by.order.push(row.entry.clone());
                    by.rows.insert(row.entry.clone(), vec![row]);
                }
            }
        }
        by
    }

    /// 拿走这个仓库的终端。
    fn take(&mut self, entry: &RepoEntry) -> Vec<Row> {
        self.rows.remove(entry).unwrap_or_default()
    }

    /// 还没拿走的，按仓库第一次出现的先后。
    fn rest(mut self) -> Vec<(RepoEntry, Vec<Row>)> {
        let order = std::mem::take(&mut self.order);
        order
            .into_iter()
            .filter_map(|entry| {
                let mine = self.rows.remove(&entry)?;
                Some((entry, mine))
            })
            .collect()
    }
}

/// 未加入工作区的仓库按主机分节：`listed` 是主机仓库列表里的（已经按主机排好），`rows` 是没归到
/// 哪个工作区的终端。列表里的仓库按列表的先后，终端归到各自的仓库；剩下的终端（子仓库这类列表里
/// 没有的）按仓库接在它所在主机那一节的后面，那台没有这一节就新起一节。
fn group_unbound(listed: Vec<RepoEntry>, rows: ByRepo) -> Vec<RepoGroup> {
    let mut rows = rows;
    let mut groups: Vec<RepoGroup> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    let mut add = |groups: &mut Vec<RepoGroup>, entry: RepoEntry, mine: Vec<Row>| {
        let machine = machine_of(&entry).to_string();
        match at.get(&machine) {
            Some(&index) => groups[index].repos.push((entry, mine)),
            None => {
                at.insert(machine.clone(), groups.len());
                groups.push(RepoGroup {
                    key: GroupKey::Unbound(machine),
                    repos: vec![(entry, mine)],
                });
            }
        }
    };
    for entry in listed {
        let mine = rows.take(&entry);
        add(&mut groups, entry, mine);
    }
    for (entry, mine) in rows.rest() {
        add(&mut groups, entry, mine);
    }
    groups
}

/// 把终端按工作区、仓库分组。同一个仓库在几个工作区里时只列在最前面的那个里；没有仓库可列的
/// 工作区不出现。返回分好的组，和不属于其中任何仓库的终端。
fn group_by_workspace(
    workspaces: Vec<(WorkspaceKey, Vec<RepoEntry>)>,
    rows: Vec<Row>,
) -> (Vec<RepoGroup>, ByRepo) {
    let mut rows = ByRepo::new(rows);
    let mut seen: HashSet<RepoEntry> = HashSet::new();
    let mut groups: Vec<RepoGroup> = Vec::new();
    for (key, members) in workspaces {
        let mut repos: Vec<(RepoEntry, Vec<Row>)> = Vec::new();
        for entry in members {
            if !seen.insert(entry.clone()) {
                continue;
            }
            let mine = rows.take(&entry);
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
pub(crate) mod tests {
    use std::time::{Duration, Instant};

    use super::{Agent, ByRepo, Kind, TermState, free_number, group_unbound, kind_of, state_of};
    use super::{GroupKey, Program, RepoEntry, Row, TermId, WorkspaceKey, group_by_workspace};

    /// 测试用的一行：不在乎的都给默认的。
    pub(crate) fn plain_row(id: TermId, entry: RepoEntry, number: u32) -> Row {
        Row {
            id,
            entry,
            number,
            label: format!("终端 {number}").into(),
            named: false,
            last: String::new(),
            cols: 80,
            rows: 24,
            alive: true,
            state: TermState::Idle,
            kind: Kind::Shell,
            away: false,
            online: true,
            last_output: None,
            born: 0,
            programs: None,
        }
    }

    #[test]
    fn 未加入工作区的仓库按主机分节_没有终端的也列() {
        let entry = |server: Option<&str>, path: &str| RepoEntry {
            server: server.map(str::to_string),
            path: path.into(),
        };
        let row = |entry: RepoEntry, number: u32| {
            let id = TermId {
                machine: entry.server.clone().unwrap_or_default(),
                term: format!("{}-{number}", entry.path),
            };
            plain_row(id, entry, number)
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
        let groups = group_unbound(listed, ByRepo::new(rows));
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
        assert!(group_unbound(Vec::new(), ByRepo::new(Vec::new())).is_empty());
    }

    #[test]
    fn 终端里跑的是哪一类程序() {
        let row = |alive: bool, programs: Option<&[&str]>| {
            let id = TermId {
                machine: String::new(),
                term: "t1".into(),
            };
            let entry = RepoEntry {
                server: None,
                path: "C:/repo".into(),
            };
            let programs: Option<Vec<String>> =
                programs.map(|names| names.iter().map(|name| name.to_string()).collect());
            Row {
                alive,
                kind: kind_of(programs.as_deref(), alive, None),
                programs,
                ..plain_row(id, entry, 1)
            }
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
    fn 终端的状态_只有_ai_终端静默久了才待查看() {
        let now = Instant::now() + Duration::from_secs(3600);
        let ago = |secs| Some(now - Duration::from_secs(secs));
        let claude = Kind::Agent(Agent::Claude);
        let wait = Duration::from_secs(180);
        let state = |last, seen, kind| state_of(last, seen, kind, wait, now);
        // 还没输出过：空闲
        assert_eq!(state(None, None, claude), TermState::Idle);
        // 10 秒以内有过输出：运行中，看没看过、跑的是什么都一样
        assert_eq!(state(ago(3), None, claude), TermState::Running);
        assert_eq!(state(ago(3), ago(1), Kind::Shell), TermState::Running);
        // 回到提示符的 shell：空闲；普通命令：一直是静默，不提醒
        assert_eq!(state(ago(600), None, Kind::Shell), TermState::Idle);
        assert_eq!(state(ago(3600), None, Kind::Command), TermState::Quiet);
        // AI 终端：没到时长是静默，到了、没看过是待查看
        assert_eq!(state(ago(40), None, claude), TermState::Quiet);
        assert_eq!(state(ago(180), None, claude), TermState::Waiting);
        // 停下以前看过不算（之后又有过输出）
        assert_eq!(state(ago(300), ago(400), claude), TermState::Waiting);
        // 停下以后看过：静默
        assert_eq!(state(ago(300), ago(200), claude), TermState::Quiet);
        assert_eq!(state(ago(300), ago(300), claude), TermState::Quiet);
    }

    #[test]
    fn 认出是哪家_ai_程序() {
        let names = |list: &[&str]| -> Vec<String> { list.iter().map(|s| s.to_string()).collect() };
        assert_eq!(Agent::of("claude"), Some(Agent::Claude));
        assert_eq!(Agent::of("Codex.exe"), Some(Agent::Codex));
        assert_eq!(Agent::of("gemini"), Some(Agent::Other));
        assert_eq!(Agent::of("cargo"), None);
        assert_eq!(kind_of(Some(&names(&[])), true, None), Kind::Shell);
        assert_eq!(kind_of(None, true, None), Kind::Command, "旧版 kuaid");
        assert_eq!(
            kind_of(Some(&names(&["claude"])), false, None),
            Kind::Shell,
            "已结束"
        );
        assert_eq!(
            kind_of(Some(&names(&["opencode"])), true, None),
            Kind::Agent(Agent::OpenCode)
        );
        // Windows 上 npm 装的 Codex 进程名是 node：看从哪个预设开的
        let codex = crate::presets::Start::command("codex --yolo");
        assert_eq!(
            kind_of(Some(&names(&["node"])), true, Some(&codex)),
            Kind::Agent(Agent::Codex)
        );
        assert_eq!(kind_of(Some(&names(&["node"])), true, None), Kind::Command);
        // 从 Codex 的预设开的终端里后来跑了 cargo：不算
        assert_eq!(
            kind_of(Some(&names(&["cargo"])), true, Some(&codex)),
            Kind::Command
        );
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
