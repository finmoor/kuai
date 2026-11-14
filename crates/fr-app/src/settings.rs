//! 设置：服务器列表、仓库列表、工作区和各自开着的标签页、语言、主题、托盘和窗口位置。
//!
//! 存在 `%APPDATA%\kuai\settings.json`，不存密码（服务器的密码、私钥口令在系统的凭据管理器里，
//! 见 `secrets`）。环境变量 FR_SETTINGS 可以指定别的文件（测试用）。

use fr_core::tf;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use fr_core::i18n::{Language, tr};
use serde::{Deserialize, Serialize};

fn default_light_theme() -> String {
    "light".into()
}

fn default_dark_theme() -> String {
    "dark".into()
}

fn default_auto_fetch() -> u32 {
    10
}

fn default_true() -> bool {
    true
}

/// 仓库页「全部仓库」里本机那张卡片的名字（收起的卡片记在 [`Settings::collapsed_cards`] 里）。
pub const LOCAL_CARD: &str = "local";

/// 可以选的自动获取间隔（分钟）；0 是不自动获取。
pub const AUTO_FETCH_CHOICES: [u32; 6] = [0, 5, 10, 15, 30, 60];

/// 各块的大小（逻辑像素），拖分隔条改的。没拖过的是空，按默认值或窗口大小算。
/// 仓库页上「全部仓库」展没展开也记在这里。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// 仓库页侧栏的宽度
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar: Option<f32>,
    /// 仓库页的侧栏收成了图标栏（所有标签页、Git 页和终端页共用）；展开时还是上面的宽度
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sidebar_collapsed: bool,
    /// 本地修改里文件列表的宽度
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes_list: Option<f32>,
    /// 提交历史下面详情区的高度
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_height: Option<f32>,
    /// 详情区左边一栏（说明、改动的文件）的宽度
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_width: Option<f32>,
    // 1.3～1.6 还有 terminal_height（仓库页底下终端的高度）。1.7 起终端单独一页、占满，
    // 不再要它；旧设置文件里的这一项读的时候忽略，下次保存就没了
    /// 仓库页上「全部仓库」展开着（工作区里还没有仓库时不看这一项，一直展开）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_repos: bool,
}

/// 一个仓库自己记的东西：起的名字、隐藏了哪些分支、最近用过的提交说明。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMemo {
    #[serde(flatten)]
    pub repo: RepoEntry,
    /// 自己起的名字（只改显示，目录不动）；没有就显示目录名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 提交列表里隐藏的引用，完整的名字（`refs/heads/…`、`refs/remotes/…`、`refs/tags/…`）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_refs: Vec<String>,
    /// 最近用过的提交说明，新的在前
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_messages: Vec<String>,
}

impl RepoMemo {
    /// 最多记几条提交说明
    pub const MAX_MESSAGES: usize = 10;

    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.hidden_refs.is_empty() && self.recent_messages.is_empty()
    }

    /// 记下一条刚用过的提交说明：放到最前面，重复的去掉，多了去掉最旧的。
    pub fn remember_message(&mut self, message: &str) {
        let message = message.trim();
        if message.is_empty() {
            return;
        }
        self.recent_messages.retain(|m| m != message);
        self.recent_messages.insert(0, message.to_string());
        self.recent_messages.truncate(Self::MAX_MESSAGES);
    }
}

/// 一个工作区：它的仓库，和其中开着的那些标签页。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabGroup {
    pub name: String,
    #[serde(default)]
    pub tabs: Vec<RepoEntry>,
    /// 选中的标签页：0 是仓库管理器，1 起是 `tabs` 里的
    #[serde(default)]
    pub active: usize,
    /// 最近关掉的标签页，新的在后；重新打开了就从这里去掉
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed: Vec<RepoEntry>,
    /// 这个工作区的仓库：在这里打开过的都算，关标签页不移出，要明确「从工作区移除」。
    /// 一个仓库可以在几个工作区里
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoEntry>,
    /// 固定的标签页：排在最前面，没有关闭按钮，Ctrl+W 和中键也关不掉
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned: Vec<RepoEntry>,
}

impl TabGroup {
    /// 最多记几个关掉的标签页
    pub const MAX_CLOSED: usize = 10;

    /// 记下刚关掉的标签页。
    pub fn note_closed(&mut self, entry: RepoEntry) {
        self.closed.retain(|e| *e != entry);
        self.closed.push(entry);
        let extra = self.closed.len().saturating_sub(Self::MAX_CLOSED);
        self.closed.drain(..extra);
    }

    /// 把仓库加进这个工作区（已经在就不动）。
    pub fn adopt(&mut self, entry: &RepoEntry) {
        if !self.repos.contains(entry) {
            self.repos.push(entry.clone());
        }
    }

    /// 把仓库从这个工作区去掉：不再是它的仓库，也不算刚关掉的、固定的。开着的标签页由调用方关。
    pub fn drop_repo(&mut self, entry: &RepoEntry) {
        self.repos.retain(|e| e != entry);
        self.closed.retain(|e| e != entry);
        self.pinned.retain(|e| e != entry);
    }

    pub fn is_pinned(&self, entry: &RepoEntry) -> bool {
        self.pinned.contains(entry)
    }

    /// 固定的标签页排到最前面，各自内部的先后不变。选中的还是原来那个仓库。
    pub fn pinned_first(&mut self) {
        let current = self
            .active
            .checked_sub(1)
            .and_then(|ix| self.tabs.get(ix))
            .cloned();
        let pinned = &self.pinned;
        self.tabs.sort_by_key(|tab| !pinned.contains(tab));
        if let Some(current) = current {
            self.active = self
                .tabs
                .iter()
                .position(|tab| *tab == current)
                .map_or(0, |ix| ix + 1);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub servers: Vec<Server>,
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
    /// 旧版本（1.0.x）存标签页的地方，读进来以后挪到第一个工作区里，不再写回去
    #[serde(default, skip_serializing)]
    open_tabs: Vec<RepoEntry>,
    #[serde(default, skip_serializing)]
    active_tab: usize,
    /// 工作区：各自记着开着哪些仓库标签页。至少有一个
    #[serde(default)]
    pub workspaces: Vec<TabGroup>,
    /// 现在用的是第几个工作区
    #[serde(default)]
    pub active_workspace: usize,
    #[serde(default)]
    pub language: Language,
    /// 深色还是浅色：跟随系统，或者固定一种
    #[serde(default)]
    pub theme: ThemeChoice,
    /// 浅色时用哪套配色（`appearance::PALETTES` 里的 id）
    #[serde(default = "default_light_theme")]
    pub light_theme: String,
    /// 深色时用哪套配色
    #[serde(default = "default_dark_theme")]
    pub dark_theme: String,
    /// 最小化时收到托盘，任务栏上不留按钮
    #[serde(default)]
    pub minimize_to_tray: bool,
    /// 点窗口的关闭按钮时不退出，收到托盘
    #[serde(default)]
    pub close_to_tray: bool,
    /// 每隔几分钟自动获取一次（只对正在看的那个仓库）；0 是不自动获取
    #[serde(default = "default_auto_fetch")]
    pub auto_fetch_minutes: u32,
    /// 改动并排着看（左边旧的、右边新的），不勾就是上下排在一栏里
    #[serde(default)]
    pub diff_split: bool,
    /// 工作区有改动时切换分支、拉取：先贮藏，做完再恢复
    #[serde(default)]
    pub stash_before_switch: bool,
    /// 标签页上显示关闭按钮；关了以后用中键、Ctrl+W 或右键菜单关，免得手滑点到
    #[serde(default = "default_true")]
    pub tab_close_button: bool,
    /// 本机仓库的终端用哪个程序（`shell::local_shells` 里的 id）；空着自动挑
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub terminal_shell: String,
    /// 终端的字体、光标、滚动和复制（所有终端共用）
    #[serde(default, skip_serializing_if = "TerminalSettings::is_default")]
    pub terminal: TerminalSettings,
    #[serde(default)]
    pub layout: Layout,
    /// 仓库页「全部仓库」里收起的卡片：本机是 [`LOCAL_CARD`]，服务器是它的 id
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collapsed_cards: Vec<String>,
    /// 各个仓库自己记的东西
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_memos: Vec<RepoMemo>,
    /// 上次关闭时窗口的位置和大小
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowState>,
    /// 启动时和之后每 6 小时问一次 GitHub 有没有新版本（见 `update.rs`）
    #[serde(default = "default_true")]
    pub check_updates: bool,
    /// 点过「跳过这一版」的版本号：标题栏上不再提示它
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_version: Option<String>,
}

/// 终端的外观和操作习惯：所有终端共用，改了马上生效。默认值就是 1.9 以前写死的那一套，
/// 升上来以后看不出变化。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    /// 等宽字体
    pub font: String,
    /// 中文这些 `font` 里没有的字用哪个字体；空着由系统挑。字宽由格子定，换它不影响对齐
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cjk_font: String,
    /// 字号（[`TerminalSettings::MIN_SIZE`]～[`TerminalSettings::MAX_SIZE`] 的整数）
    pub font_size: f32,
    /// 行高是字号的几倍（[`TerminalSettings::LINE_HEIGHTS`] 里的一档）
    pub line_height: f32,
    /// 光标的样子；终端里的程序可以临时改成别的
    pub cursor: TerminalCursor,
    pub cursor_blink: bool,
    /// 每个终端留多少行往回翻（[`TerminalSettings::SCROLLBACKS`] 里的一档）
    pub scrollback: usize,
    /// 用鼠标选中文字就复制到剪贴板
    pub copy_on_select: bool,
    pub right_click: RightClick,
}

/// 光标的样子。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalCursor {
    #[default]
    Block,
    Beam,
    Underline,
}

impl TerminalCursor {
    pub const ALL: [TerminalCursor; 3] = [
        TerminalCursor::Block,
        TerminalCursor::Beam,
        TerminalCursor::Underline,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TerminalCursor::Block => tr("方块"),
            TerminalCursor::Beam => tr("竖线"),
            TerminalCursor::Underline => tr("下划线"),
        }
    }
}

/// 在终端里点右键做什么。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RightClick {
    /// 选着字就复制，没选就粘贴（和 Windows 终端一样）
    #[default]
    Paste,
    /// 弹出菜单
    Menu,
}

impl RightClick {
    pub const ALL: [RightClick; 2] = [RightClick::Paste, RightClick::Menu];

    pub fn label(self) -> &'static str {
        match self {
            RightClick::Paste => tr("有选中时复制，否则粘贴"),
            RightClick::Menu => tr("显示菜单"),
        }
    }
}

impl TerminalSettings {
    pub const DEFAULT_FONT: &str = "Consolas";
    pub const DEFAULT_SIZE: f32 = 13.;
    pub const MIN_SIZE: f32 = 9.;
    pub const MAX_SIZE: f32 = 24.;
    pub const LINE_HEIGHTS: [f32; 5] = [1., 1.1, 1.25, 1.4, 1.6];
    pub const SCROLLBACKS: [usize; 5] = [1_000, 5_000, 10_000, 50_000, 100_000];

    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// 手改设置文件写了别的数：收到范围里、最近的一档。
    pub fn normalize(&mut self) {
        if self.font.trim().is_empty() {
            self.font = Self::DEFAULT_FONT.to_string();
        }
        self.font_size = if self.font_size.is_finite() {
            self.font_size.round().clamp(Self::MIN_SIZE, Self::MAX_SIZE)
        } else {
            Self::DEFAULT_SIZE
        };
        let wanted = self.line_height;
        self.line_height = Self::LINE_HEIGHTS
            .into_iter()
            .min_by(|a, b| (a - wanted).abs().total_cmp(&(b - wanted).abs()))
            .unwrap_or(1.25);
        if !self.line_height.is_finite() || !wanted.is_finite() {
            self.line_height = 1.25;
        }
        self.scrollback = Self::SCROLLBACKS
            .into_iter()
            .min_by_key(|choice| choice.abs_diff(self.scrollback))
            .unwrap_or(10_000);
    }

    /// 一格有多高（像素）：字号乘行高，取整，画出来的线上下才接得上。
    pub fn cell_height(&self) -> f32 {
        (self.font_size * self.line_height).round()
    }
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            font: Self::DEFAULT_FONT.to_string(),
            cjk_font: String::new(),
            font_size: Self::DEFAULT_SIZE,
            line_height: 1.25,
            cursor: TerminalCursor::Block,
            cursor_blink: false,
            scrollback: 10_000,
            copy_on_select: false,
            right_click: RightClick::Paste,
        }
    }
}

/// 用哪套主题。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// 跟着 Windows 的深色、浅色设置走
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark];

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => tr("跟随系统"),
            ThemeChoice::Light => tr("浅色"),
            ThemeChoice::Dark => tr("深色"),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        let mut settings = Self {
            servers: Vec::new(),
            repos: Vec::new(),
            open_tabs: Vec::new(),
            active_tab: 0,
            workspaces: Vec::new(),
            active_workspace: 0,
            language: Language::default(),
            // 新装的默认深色（2026-11-13 Feng 定）。设置文件里没有这一项（1.1 以前的）
            // 时取 ThemeChoice 的默认值，照旧跟随系统，见 parse 的测试
            theme: ThemeChoice::Dark,
            light_theme: default_light_theme(),
            dark_theme: default_dark_theme(),
            minimize_to_tray: false,
            close_to_tray: false,
            auto_fetch_minutes: default_auto_fetch(),
            diff_split: false,
            stash_before_switch: false,
            tab_close_button: true,
            terminal_shell: String::new(),
            terminal: TerminalSettings::default(),
            layout: Layout::default(),
            collapsed_cards: Vec::new(),
            repo_memos: Vec::new(),
            window: None,
            check_updates: true,
            skipped_version: None,
        };
        settings.normalize();
        settings
    }
}

impl Settings {
    /// 读进来以后理一遍：旧版本的标签页挪进第一个工作区；保证至少有一个工作区、下标不越界。
    pub fn normalize(&mut self) {
        if self.workspaces.is_empty() {
            self.workspaces.push(TabGroup {
                name: tr("默认").to_string(),
                tabs: std::mem::take(&mut self.open_tabs),
                active: self.active_tab,
                ..TabGroup::default()
            });
        }
        self.open_tabs.clear();
        self.active_tab = 0;
        self.active_workspace = self.active_workspace.min(self.workspaces.len() - 1);
        let repos = &self.repos;
        for group in &mut self.workspaces {
            group.active = group.active.min(group.tabs.len());
            // 关掉的标签页：又开着的、仓库已经不在列表里的都不用记
            let tabs = &group.tabs;
            group
                .closed
                .retain(|e| repos.contains(e) && !tabs.contains(e));
            // 工作区的仓库：开着的、刚关掉的都算（1.4 以前的设置里没有这一项，就是这样补出来的）；
            // 已经不在仓库列表里、也没开着的去掉
            for entry in group.tabs.iter().chain(&group.closed) {
                if !group.repos.contains(entry) {
                    group.repos.push(entry.clone());
                }
            }
            let tabs = &group.tabs;
            group
                .repos
                .retain(|e| repos.contains(e) || tabs.contains(e));
            // 固定的只能是开着的
            group.pinned.retain(|e| tabs.contains(e));
            group.pinned_first();
        }
        // 手改设置文件写了别的数：收到最近的一档
        if !AUTO_FETCH_CHOICES.contains(&self.auto_fetch_minutes) {
            self.auto_fetch_minutes = AUTO_FETCH_CHOICES
                .into_iter()
                .min_by_key(|choice| choice.abs_diff(self.auto_fetch_minutes))
                .unwrap_or(0);
        }
        self.terminal.normalize();
        // 跳板机：指向不存在的服务器、绕回自己的（手改设置文件才会有）都去掉
        for index in 0..self.servers.len() {
            let Some(jump) = self.servers[index].jump.clone() else {
                continue;
            };
            let id = self.servers[index].id.clone();
            if !self.can_jump(&id, &jump) {
                self.servers[index].jump = None;
            }
        }
        // 收起的卡片：服务器已经不在了的不用记
        let servers = &self.servers;
        self.collapsed_cards
            .retain(|key| key == LOCAL_CARD || servers.iter().any(|server| server.id == *key));
        // 仓库已经不在列表里、也没开着了，它记的东西就不留
        let (repos, groups) = (&self.repos, &self.workspaces);
        // 子仓库（在列表里某个仓库的目录下面）不在列表里，它记的东西跟着那个仓库留着
        let inside = |memo: &RepoMemo| repos.iter().any(|repo| repo.contains(&memo.repo));
        self.repo_memos.retain(|memo| {
            !memo.is_empty()
                && (repos.contains(&memo.repo)
                    || groups.iter().any(|group| group.tabs.contains(&memo.repo))
                    || inside(memo))
        });
    }

    pub fn server(&self, id: &str) -> Option<&Server> {
        self.servers.iter().find(|server| server.id == id)
    }

    /// 连接这台服务器要依次经过的跳板机：第一台从本机直接连，最后一台连到这台服务器。
    /// 不经跳板机的是空的。
    pub fn route(&self, id: &str) -> Vec<&Server> {
        let mut route: Vec<&Server> = Vec::new();
        let mut at = self.server(id);
        while let Some(jump) = at
            .and_then(|server| server.jump.as_deref())
            .and_then(|jump| self.server(jump))
        {
            // 绕回来了（normalize 会去掉，这里只是不死循环）
            if jump.id == id || route.iter().any(|seen| seen.id == jump.id) {
                break;
            }
            route.push(jump);
            at = Some(jump);
        }
        route.reverse();
        route
    }

    /// `id` 这台服务器能不能把 `jump` 当跳板机：得是另一台已配置的服务器，而且它不经 `id` 连接
    /// （不然绕圈）。`id` 是还没加进来的新服务器时传空串。
    pub fn can_jump(&self, id: &str, jump: &str) -> bool {
        jump != id
            && self.server(jump).is_some()
            && !self.route(jump).iter().any(|server| server.id == id)
    }

    /// 经这台服务器连接的服务器（直接把它选作跳板机的）。
    pub fn jump_users(&self, id: &str) -> Vec<&Server> {
        self.servers
            .iter()
            .filter(|server| server.jump.as_deref() == Some(id))
            .collect()
    }

    /// 这个仓库记的东西。
    pub fn memo(&self, repo: &RepoEntry) -> Option<&RepoMemo> {
        self.repo_memos.iter().find(|memo| memo.repo == *repo)
    }

    /// 改这个仓库记的东西；原来没有就新建一份，改完是空的就不留。
    pub fn edit_memo(&mut self, repo: &RepoEntry, edit: impl FnOnce(&mut RepoMemo)) {
        let ix = match self.repo_memos.iter().position(|memo| memo.repo == *repo) {
            Some(ix) => ix,
            None => {
                self.repo_memos.push(RepoMemo {
                    repo: repo.clone(),
                    name: None,
                    hidden_refs: Vec::new(),
                    recent_messages: Vec::new(),
                });
                self.repo_memos.len() - 1
            }
        };
        edit(&mut self.repo_memos[ix]);
        if self.repo_memos[ix].is_empty() {
            self.repo_memos.remove(ix);
        }
    }

    /// 仓库显示的名字：起过名字就用它，没有就是目录名。
    pub fn repo_name<'a>(&'a self, repo: &'a RepoEntry) -> &'a str {
        self.memo(repo)
            .and_then(|memo| memo.name.as_deref())
            .unwrap_or_else(|| repo.name())
    }

    /// 给仓库起名字。空的、或者和目录名一样，就是不要自己起的名字了。
    pub fn set_repo_name(&mut self, repo: &RepoEntry, name: &str) {
        let name = name.trim();
        let name = (!name.is_empty() && name != repo.name()).then(|| name.to_string());
        self.edit_memo(repo, |memo| memo.name = name);
    }

    /// 现在用的工作区。
    pub fn workspace(&self) -> &TabGroup {
        &self.workspaces[self.active_workspace.min(self.workspaces.len() - 1)]
    }

    pub fn workspace_mut(&mut self) -> &mut TabGroup {
        let ix = self.active_workspace.min(self.workspaces.len() - 1);
        &mut self.workspaces[ix]
    }
}

/// 窗口的位置和大小（逻辑像素）。最大化时记的是还原后的位置。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
}

/// 登录服务器的方式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Auth {
    /// 密钥：指定的私钥文件，或者 ssh 默认的那些（`~/.ssh` 里的、ssh-agent、`~/.ssh/config`）
    #[default]
    Key,
    /// 密码，不试密钥
    Password,
}

impl Auth {
    fn is_key(&self) -> bool {
        *self == Auth::Key
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub id: String,
    pub name: String,
    /// ssh 的目标：`~/.ssh/config` 里的主机别名、`user@host` 或 `ssh://user@host:port`。
    /// 界面上分成主机、用户名、端口三个框填，见 [`Target`]
    pub destination: String,
    /// 连接时转发本机的 ssh-agent，服务器上的 git 推拉可以用本机的密钥
    #[serde(default)]
    pub forward_agent: bool,
    /// 登录方式。密码、私钥口令不在这里，存在系统的凭据管理器里（见 `secrets`）
    #[serde(default, skip_serializing_if = "Auth::is_key")]
    pub auth: Auth,
    /// 私钥文件（只在用密钥登录时有用）；空着用 ssh 默认的密钥
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    /// 跳板机：另一台已配置的服务器的 id。连接时先登录它，再从它连到这台；
    /// 它自己也可以有跳板机，见 [`Settings::route`]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<String>,
    /// 初始目录：「打开仓库」「克隆」从这里开始；空着从主目录开始
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_dir: Option<String>,
    /// 「打开仓库」上次打开的仓库所在的目录，下次从这里开始。不在表单里；改了初始目录就清掉
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_dir: Option<String>,
}

impl Server {
    /// 「打开仓库」依次试着从哪里开始：上次打开的目录、初始目录，都进不去就是主目录（空串）。
    pub fn browse_starts(&self) -> Vec<String> {
        let mut starts: Vec<String> = Vec::new();
        for dir in [&self.last_dir, &self.start_dir].into_iter().flatten() {
            if !dir.is_empty() && !starts.contains(dir) {
                starts.push(dir.clone());
            }
        }
        starts.push(String::new());
        starts
    }

    /// 连接时用的私钥文件：用密码登录的不带。
    pub fn key_file(&self) -> Option<&str> {
        match self.auth {
            Auth::Key => self.identity_file.as_deref(),
            Auth::Password => None,
        }
    }
}

/// ssh 目标拆开的三段；没有的那段是空串。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    /// 主机名、IP，或者 `~/.ssh/config` 里的主机别名
    pub host: String,
    pub user: String,
    pub port: String,
}

impl Target {
    /// 拆开 `host`、`user@host`、`ssh://user@host:port`；`host:端口`、`[IPv6]:端口` 也认。
    pub fn parse(destination: &str) -> Target {
        let text = destination.trim();
        let rest = text
            .strip_prefix("ssh://")
            .map_or(text, |rest| rest.trim_end_matches('/'));
        // 用户名里可以有 @（比如域账号），以最后一个为界
        let (user, rest) = rest.rsplit_once('@').unwrap_or(("", rest));
        let (host, port) = match rest.strip_prefix('[').and_then(|r| r.split_once(']')) {
            Some((host, tail)) => (host, tail.strip_prefix(':').unwrap_or("")),
            None => match rest.rsplit_once(':') {
                // 只有一个冒号、后面全是数字才是端口；冒号多的是不带方括号的 IPv6 地址
                Some((host, port))
                    if !host.contains(':')
                        && !port.is_empty()
                        && port.chars().all(|c| c.is_ascii_digit()) =>
                {
                    (host, port)
                }
                _ => (rest, ""),
            },
        };
        Target {
            host: host.to_string(),
            user: user.to_string(),
            port: port.to_string(),
        }
    }

    /// `user@host:port`，没有的段不写。VS Code 的 Remote-SSH 认这种写法。
    pub fn authority(&self) -> String {
        let mut text = String::new();
        if !self.user.is_empty() {
            text.push_str(&self.user);
            text.push('@');
        }
        if self.port.is_empty() {
            text.push_str(&self.host);
        } else if self.host.contains(':') {
            text.push_str(&format!("[{}]:{}", self.host, self.port));
        } else {
            text.push_str(&format!("{}:{}", self.host, self.port));
        }
        text
    }

    /// 拼成交给 ssh 的目标：有端口的写成 `ssh://…`（ssh 只在这种写法里认端口）。
    pub fn compose(&self) -> String {
        if self.port.is_empty() {
            self.authority()
        } else {
            format!("ssh://{}", self.authority())
        }
    }

    /// 端口是不是 1～65535 的数字。
    pub fn port_ok(&self) -> bool {
        self.port.parse::<u16>().is_ok_and(|port| port > 0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoEntry {
    /// 所在服务器的 id；本机仓库为空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// 仓库根目录；服务器上的是 Linux 路径
    pub path: String,
}

impl RepoEntry {
    /// 目录名。
    pub fn name(&self) -> &str {
        let path = self.path.trim_end_matches(['/', '\\']);
        path.rsplit(['/', '\\']).next().unwrap_or(path)
    }

    /// `other` 是不是在这个仓库的目录下面（同一台机器上、路径以这个仓库的路径加一个斜杠开头）。
    pub fn contains(&self, other: &RepoEntry) -> bool {
        let root = self.path.trim_end_matches(['/', '\\']);
        self.server == other.server
            && other
                .path
                .strip_prefix(root)
                .is_some_and(|rest| rest.len() > 1 && rest.starts_with(['/', '\\']))
    }
}

/// `%APPDATA%` 下放设置的文件夹。
const DIR: &str = "kuai";
/// 1.2.2 以前产品还叫 fork-remote 时用的文件夹。
const LEGACY_DIR: &str = "fork-remote";
const FILE: &str = "settings.json";

fn appdata() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn file() -> PathBuf {
    if let Some(path) = std::env::var_os("FR_SETTINGS") {
        return path.into();
    }
    appdata().join(DIR).join(FILE)
}

/// 改名以前的设置搬过来：新文件夹里还没有设置、旧文件夹里有的时候才搬，只会发生一次。
fn adopt_legacy(appdata: &std::path::Path) {
    let (old, new) = (appdata.join(LEGACY_DIR), appdata.join(DIR));
    if new.join(FILE).exists() || !old.join(FILE).exists() {
        return;
    }
    // 整个文件夹改名；新文件夹已经在了就改不成，那就只拷设置文件
    if std::fs::rename(&old, &new).is_ok() {
        return;
    }
    let copied =
        std::fs::create_dir_all(&new).and_then(|()| std::fs::copy(old.join(FILE), new.join(FILE)));
    if let Err(err) = copied {
        log::warn!("旧设置（{}）没搬过来：{err}", old.display());
    }
}

pub fn load() -> Settings {
    if std::env::var_os("FR_SETTINGS").is_none() {
        adopt_legacy(&appdata());
    }
    let path = file();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Settings::default();
    };
    match parse(&text) {
        Ok(settings) => settings,
        Err(err) => {
            // 文件坏了：改名留着，从空设置开始，免得下次保存把它覆盖掉
            let backup = path.with_extension(format!(
                "json.broken-{}",
                chrono::Local::now().format("%Y%m%d-%H%M%S")
            ));
            let _ = std::fs::rename(&path, &backup);
            log::warn!("设置文件解析失败（{err}），已改名为 {}", backup.display());
            Settings::default()
        }
    }
}

/// 解析设置文件的内容。
///
/// 1.1 以前的设置文件里没有语言这一项：那时界面只有中文，升级上来的保持中文，不因为系统的
/// 显示语言是英文就突然换成英文界面。没有设置文件（新装）才跟随系统。
fn parse(text: &str) -> serde_json::Result<Settings> {
    let mut settings: Settings = serde_json::from_str(text)?;
    let has_language = serde_json::from_str::<serde_json::Value>(text)
        .is_ok_and(|value| value.get("language").is_some());
    if !has_language {
        settings.language = Language::Chinese;
    }
    settings.normalize();
    Ok(settings)
}

/// 先写临时文件再改名，写到一半退出也不会留下坏文件。
pub fn save(settings: &Settings) -> Result<()> {
    let path = file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| tf!("创建目录失败：{dir}", dir = dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(settings)?)
        .with_context(|| tf!("保存设置失败：{tmp}", tmp = tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| tf!("保存设置失败：{path}", path = path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 仓库名取最后一级目录() {
        let name = |path: &str| {
            RepoEntry {
                server: None,
                path: path.into(),
            }
            .name()
            .to_string()
        };
        assert_eq!(name(r"C:\work\kuai"), "kuai");
        assert_eq!(name("/data/workspaces/big-repo/"), "big-repo");
        assert_eq!(name("repo"), "repo");
    }

    #[test]
    fn 工作区的仓库清单() {
        let repo = |path: &str| RepoEntry {
            server: None,
            path: path.into(),
        };
        // 旧设置里没有清单：开着的、刚关掉的补进去
        let old = r#"{"repos":[{"path":"a"},{"path":"b"},{"path":"c"},{"path":"d"}],
            "workspaces":[{"name":"w","tabs":[{"path":"a"},{"path":"b"}],"active":2,
            "closed":[{"path":"c"}]}]}"#;
        let mut settings: Settings = serde_json::from_str(old).unwrap();
        settings.normalize();
        let group = &settings.workspaces[0];
        assert_eq!(group.repos, [repo("a"), repo("b"), repo("c")]);

        // 仓库从列表里去掉了：清单里也没有；固定的排最前，选中的还是原来那个
        settings.repos.retain(|r| r.path != "c");
        settings.workspaces[0].pinned = vec![repo("b"), repo("不存在")];
        settings.normalize();
        let group = &settings.workspaces[0];
        assert_eq!(group.repos, [repo("a"), repo("b")]);
        assert_eq!(group.pinned, [repo("b")]);
        assert_eq!(group.tabs, [repo("b"), repo("a")]);
        assert_eq!(group.active, 1);

        // 从工作区去掉：不再补回来
        settings.workspaces[0].tabs.retain(|r| r.path != "a");
        settings.workspaces[0].note_closed(repo("a"));
        settings.workspaces[0].drop_repo(&repo("a"));
        settings.normalize();
        assert_eq!(settings.workspaces[0].repos, [repo("b")]);
        assert!(settings.workspaces[0].closed.is_empty());
    }

    #[test]
    fn 改名以前的设置搬到新文件夹() {
        let root = std::env::temp_dir().join(format!("kuai-test-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (old, new) = (root.join(LEGACY_DIR), root.join(DIR));

        // 两边都没有：什么也不做
        std::fs::create_dir_all(&root).unwrap();
        adopt_legacy(&root);
        assert!(!new.exists());

        // 只有旧的：整个文件夹搬过来
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join(FILE), "旧的").unwrap();
        adopt_legacy(&root);
        assert_eq!(std::fs::read_to_string(new.join(FILE)).unwrap(), "旧的");
        assert!(!old.exists());

        // 新的已经有设置：旧的再出现也不覆盖
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join(FILE), "又一份旧的").unwrap();
        adopt_legacy(&root);
        assert_eq!(std::fs::read_to_string(new.join(FILE)).unwrap(), "旧的");

        // 新文件夹在、里面没有设置：只拷设置文件
        std::fs::remove_file(new.join(FILE)).unwrap();
        adopt_legacy(&root);
        assert_eq!(
            std::fs::read_to_string(new.join(FILE)).unwrap(),
            "又一份旧的"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn 旧版本的标签页挪进第一个工作区() {
        let old = r#"{"repos":[{"path":"C:/a"}],"open_tabs":[{"path":"C:/a"}],"active_tab":1,"theme":"dark"}"#;
        let mut settings: Settings = serde_json::from_str(old).unwrap();
        settings.normalize();
        assert_eq!(settings.workspaces.len(), 1);
        assert_eq!(settings.workspace().tabs.len(), 1);
        assert_eq!(settings.workspace().active, 1);
        assert_eq!(settings.theme, ThemeChoice::Dark);
        assert_eq!(settings.light_theme, "light");
        // 写回去以后没有旧字段，再读一遍结果一样
        let text = serde_json::to_string(&settings).unwrap();
        assert!(!text.contains("open_tabs"));
        let mut again: Settings = serde_json::from_str(&text).unwrap();
        again.normalize();
        assert_eq!(again.workspaces, settings.workspaces);
    }

    #[test]
    fn 仓库记的东西跟着仓库走() {
        let repo = |path: &str| RepoEntry {
            server: None,
            path: path.into(),
        };
        let mut settings = Settings {
            repos: vec![repo("C:/a"), repo("C:/b")],
            ..Settings::default()
        };
        settings.edit_memo(&repo("C:/a"), |memo| {
            memo.hidden_refs.push("refs/heads/wip".into());
            for n in 0..12 {
                memo.remember_message(&format!("说明 {n}"));
            }
            memo.remember_message("说明 5");
            memo.remember_message("  ");
        });
        let memo = settings.memo(&repo("C:/a")).unwrap();
        assert_eq!(memo.recent_messages.len(), RepoMemo::MAX_MESSAGES);
        assert_eq!(memo.recent_messages[0], "说明 5");
        assert_eq!(memo.recent_messages[1], "说明 11");
        assert_eq!(
            memo.recent_messages
                .iter()
                .filter(|m| *m == "说明 5")
                .count(),
            1
        );
        // 存一遍再读回来还在
        let text = serde_json::to_string(&settings).unwrap();
        let again = super::parse(&text).unwrap();
        assert_eq!(again.repo_memos, settings.repo_memos);
        // 改完是空的就不留；仓库从列表里去掉以后也不留
        settings.edit_memo(&repo("C:/b"), |_| {});
        assert!(settings.memo(&repo("C:/b")).is_none());
        settings.repos.remove(0);
        settings.normalize();
        assert!(settings.repo_memos.is_empty());
    }

    #[test]
    fn 子仓库记的东西跟着根仓库留着() {
        let repo = |path: &str| RepoEntry {
            server: None,
            path: path.into(),
        };
        let mut settings = Settings {
            repos: vec![repo(r"C:\a")],
            ..Settings::default()
        };
        let hide = |memo: &mut RepoMemo| memo.hidden_refs.push("refs/heads/wip".into());
        // 子仓库不在仓库列表里；名字只是以根仓库的名字开头的不算在它里面
        settings.edit_memo(&repo(r"C:\a\packages\plugin-x"), hide);
        settings.edit_memo(&repo(r"C:\ab"), hide);
        settings.normalize();
        assert!(settings.memo(&repo(r"C:\a\packages\plugin-x")).is_some());
        assert!(settings.memo(&repo(r"C:\ab")).is_none());
        let server = RepoEntry {
            server: Some("s1".into()),
            path: "/srv/a/plugin".into(),
        };
        assert!(!repo("/srv/a").contains(&server));
        assert!(!repo("/srv/a").contains(&repo("/srv/a")));
        // 根仓库移除以后不留
        settings.repos.clear();
        settings.normalize();
        assert!(settings.repo_memos.is_empty());
    }

    #[test]
    fn ssh目标拆开再拼回去() {
        let target = |host: &str, user: &str, port: &str| Target {
            host: host.into(),
            user: user.into(),
            port: port.into(),
        };
        for (text, parts) in [
            ("dev", target("dev", "", "")),
            ("root@192.0.2.8", target("192.0.2.8", "root", "")),
            (
                "ssh://root@192.0.2.8:2222",
                target("192.0.2.8", "root", "2222"),
            ),
            (
                "ssh://git.example.com:2222",
                target("git.example.com", "", "2222"),
            ),
            (
                "u@corp.example.com@bastion",
                target("bastion", "u@corp.example.com", ""),
            ),
            (
                "ssh://u@[2001:db8::1]:2222",
                target("2001:db8::1", "u", "2222"),
            ),
            ("u@2001:db8::1", target("2001:db8::1", "u", "")),
        ] {
            assert_eq!(Target::parse(text), parts, "{text}");
            assert_eq!(parts.compose(), text, "{text}");
        }
        // 这几种写法也认，拼回去是规范的写法
        assert_eq!(Target::parse(" ssh://dev/ "), target("dev", "", ""));
        assert_eq!(Target::parse("dev:2200"), target("dev", "", "2200"));
        assert_eq!(target("dev", "", "2200").compose(), "ssh://dev:2200");
        // Remote-SSH 用的写法不带 ssh://
        assert_eq!(target("dev", "root", "2200").authority(), "root@dev:2200");

        assert!(target("dev", "", "22").port_ok());
        assert!(target("dev", "", "65535").port_ok());
        for bad in ["", "0", "65536", "22a", "-1"] {
            assert!(!target("dev", "", bad).port_ok(), "{bad}");
        }
    }

    #[test]
    fn 旧设置里的服务器按密钥登录() {
        let old = r#"{"servers":[{"id":"s1","name":"dev","destination":"root@dev"}]}"#;
        let settings = super::parse(old).unwrap();
        assert_eq!(settings.servers[0].auth, Auth::Key);
        assert_eq!(settings.servers[0].identity_file, None);
        // 默认值不写进文件
        let text = serde_json::to_string(&settings.servers[0]).unwrap();
        assert!(
            !text.contains("auth") && !text.contains("identity_file"),
            "{text}"
        );

        let server = Server {
            auth: Auth::Password,
            identity_file: Some("C:/keys/id".into()),
            ..settings.servers[0].clone()
        };
        let text = serde_json::to_string(&server).unwrap();
        assert!(text.contains(r#""auth":"password""#), "{text}");
        // 用密码登录的，连接时不带私钥文件
        assert_eq!(server.key_file(), None);
        let again: Server = serde_json::from_str(&text).unwrap();
        assert_eq!(again, server);
    }

    #[test]
    fn 打开仓库从上次的目录开始_没有就用初始目录() {
        let mut server = Server::default();
        assert_eq!(server.browse_starts(), [""]);
        server.start_dir = Some("/data/workspaces".into());
        assert_eq!(server.browse_starts(), ["/data/workspaces", ""]);
        server.last_dir = Some("/data/build/src".into());
        assert_eq!(
            server.browse_starts(),
            ["/data/build/src", "/data/workspaces", ""]
        );
        // 两个一样时只试一次
        server.last_dir = server.start_dir.clone();
        assert_eq!(server.browse_starts(), ["/data/workspaces", ""]);
        // 没设的不写进文件
        let text = serde_json::to_string(&Server::default()).unwrap();
        assert!(
            !text.contains("start_dir") && !text.contains("last_dir"),
            "{text}"
        );
    }

    #[test]
    fn 收起的卡片_服务器不在了就不记() {
        let mut settings = Settings::default();
        settings.servers.push(Server {
            id: "s1".into(),
            ..Server::default()
        });
        settings.collapsed_cards = vec![LOCAL_CARD.into(), "s1".into(), "gone".into()];
        settings.normalize();
        assert_eq!(settings.collapsed_cards, [LOCAL_CARD, "s1"]);
        // 没有收起的不写进文件
        let text = serde_json::to_string(&Settings::default()).unwrap();
        assert!(!text.contains("collapsed_cards"), "{text}");
    }

    #[test]
    fn 跳板机按顺序排成一条路_不能绕圈() {
        let server = |id: &str, jump: Option<&str>| Server {
            id: id.into(),
            name: id.into(),
            destination: id.into(),
            jump: jump.map(str::to_string),
            ..Server::default()
        };
        let mut settings = Settings {
            servers: vec![
                server("bastion", None),
                server("inner", Some("bastion")),
                server("dev", Some("inner")),
                server("alone", None),
            ],
            ..Settings::default()
        };
        let ids = |settings: &Settings, id: &str| -> Vec<String> {
            settings.route(id).iter().map(|s| s.id.clone()).collect()
        };
        // 从本机出发先到的排前面
        assert_eq!(ids(&settings, "dev"), ["bastion", "inner"]);
        assert_eq!(ids(&settings, "inner"), ["bastion"]);
        assert!(ids(&settings, "bastion").is_empty());
        let users: Vec<&str> = settings
            .jump_users("bastion")
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(users, ["inner"]);
        // 不能选自己、不存在的、经自己连接的；新服务器（还没有 id）谁都可以选
        assert!(!settings.can_jump("bastion", "bastion"));
        assert!(!settings.can_jump("bastion", "nope"));
        assert!(!settings.can_jump("bastion", "dev"));
        assert!(!settings.can_jump("inner", "dev"));
        assert!(settings.can_jump("alone", "dev"));
        assert!(settings.can_jump("dev", "alone"));
        assert!(settings.can_jump("", "dev"));
        // 手改出来的圈、指向不存在的服务器：normalize 去掉
        settings.servers[0].jump = Some("dev".into());
        settings.servers[3].jump = Some("gone".into());
        assert!(settings.route("dev").len() <= 3);
        settings.normalize();
        assert_eq!(settings.servers[0].jump, None);
        assert_eq!(settings.servers[3].jump, None);
        assert_eq!(ids(&settings, "dev"), ["bastion", "inner"]);
        // 没有跳板机的不写进文件
        let text = serde_json::to_string(&settings.servers[0]).unwrap();
        assert!(!text.contains("jump"), "{text}");
    }

    #[test]
    fn 终端设置的默认值和以前写死的一样_不写进文件() {
        use super::{RightClick, TerminalCursor, TerminalSettings};
        let settings = Settings::default();
        let terminal = &settings.terminal;
        assert_eq!(terminal.font, "Consolas");
        assert_eq!(terminal.font_size, 13.);
        // 1.9 以前行高写死 16 像素
        assert_eq!(terminal.cell_height(), 16.);
        assert_eq!(terminal.cursor, TerminalCursor::Block);
        assert!(!terminal.cursor_blink && !terminal.copy_on_select);
        assert_eq!(terminal.scrollback, 10_000);
        assert_eq!(terminal.right_click, RightClick::Paste);
        let text = serde_json::to_string(&settings).unwrap();
        assert!(!text.contains("\"terminal\":"), "{text}");
        // 旧设置文件没有这一项：取默认值
        let old = super::parse(r#"{"servers":[],"repos":[]}"#).unwrap();
        assert_eq!(old.terminal, TerminalSettings::default());
        // 只写了几项的：别的取默认值；写了别的数的收到范围里、最近的一档
        let partial = super::parse(
            r#"{"terminal":{"font":"Cascadia Mono","font_size":40,"line_height":1.3,"scrollback":7000,"cursor":"beam","right_click":"menu"}}"#,
        )
        .unwrap();
        let terminal = &partial.terminal;
        assert_eq!(terminal.font, "Cascadia Mono");
        assert_eq!(terminal.font_size, 24.);
        assert_eq!(terminal.line_height, 1.25);
        assert_eq!(terminal.scrollback, 5_000);
        assert_eq!(terminal.cursor, TerminalCursor::Beam);
        assert_eq!(terminal.right_click, RightClick::Menu);
        assert!(
            serde_json::to_string(&partial)
                .unwrap()
                .contains("\"terminal\":")
        );
        let empty = super::parse(r#"{"terminal":{"font":" ","font_size":2}}"#).unwrap();
        assert_eq!(empty.terminal.font, "Consolas");
        assert_eq!(empty.terminal.font_size, 9.);
    }

    #[test]
    fn 仓库可以另起名字() {
        let repo = RepoEntry {
            server: None,
            path: r"C:\work\chrome-mcp".into(),
        };
        let mut settings = Settings {
            repos: vec![repo.clone()],
            ..Settings::default()
        };
        assert_eq!(settings.repo_name(&repo), "chrome-mcp");
        settings.set_repo_name(&repo, "  咸鱼插件 ");
        assert_eq!(settings.repo_name(&repo), "咸鱼插件");
        // 存一遍再读回来还在
        let again = super::parse(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(again.repo_name(&repo), "咸鱼插件");
        // 清空、或者改回目录名：不留自己起的名字
        settings.set_repo_name(&repo, "chrome-mcp");
        assert!(settings.memo(&repo).is_none());
        settings.set_repo_name(&repo, "x");
        settings.set_repo_name(&repo, "");
        assert_eq!(settings.repo_name(&repo), "chrome-mcp");
        assert!(settings.repo_memos.is_empty());
    }

    #[test]
    fn 自动获取的间隔收到最近的一档() {
        let read = |text: &str| super::parse(text).unwrap().auto_fetch_minutes;
        assert_eq!(read("{}"), 10);
        assert_eq!(read(r#"{"auto_fetch_minutes":0}"#), 0);
        assert_eq!(read(r#"{"auto_fetch_minutes":7}"#), 5);
        assert_eq!(read(r#"{"auto_fetch_minutes":999}"#), 60);
    }

    #[test]
    fn 工作区的下标越界时收回来() {
        let text = r#"{"workspaces":[{"name":"a","tabs":[],"active":5}],"active_workspace":9}"#;
        let mut settings: Settings = serde_json::from_str(text).unwrap();
        settings.normalize();
        assert_eq!(settings.active_workspace, 0);
        assert_eq!(settings.workspace().active, 0);
    }

    #[test]
    fn 旧设置文件没有语言这一项时保持中文() {
        use fr_core::i18n::Language;
        // 1.1 以前存的：没有 language
        let old = super::parse(r#"{"servers":[],"repos":[]}"#).unwrap();
        assert_eq!(old.language, Language::Chinese);
        // 存过语言的照它来
        let system = super::parse(r#"{"language":"system"}"#).unwrap();
        assert_eq!(system.language, Language::System);
        let english = super::parse(r#"{"language":"en"}"#).unwrap();
        assert_eq!(english.language, Language::English);
        // 新装的（没有设置文件）跟随系统
        assert_eq!(super::Settings::default().language, Language::System);
    }

    #[test]
    fn 新装的默认深色_旧设置文件不变() {
        use super::ThemeChoice;
        assert_eq!(super::Settings::default().theme, ThemeChoice::Dark);
        // 没存过颜色模式的旧设置文件照旧跟随系统，存过的照存的来
        let old = super::parse(r#"{"servers":[],"repos":[]}"#).unwrap();
        assert_eq!(old.theme, ThemeChoice::System);
        let light = super::parse(r#"{"theme":"light"}"#).unwrap();
        assert_eq!(light.theme, ThemeChoice::Light);
    }
}
