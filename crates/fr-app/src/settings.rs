//! 设置：服务器列表、各个工作区开着的标签页、语言、主题、托盘和窗口位置。
//!
//! 存在 `%APPDATA%\kuai\settings.json`，不存密码（服务器的密码、私钥口令在系统的凭据管理器里，
//! 见 `secrets`）。环境变量 FR_SETTINGS 可以指定别的文件（测试用）。
//!
//! 仓库列表和工作区（名字、有哪些仓库）1.14 起不在这里，保存在各台电脑上，见 `machines`。

use fr_core::tf;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result};
use fr_core::i18n::{Language, tr};
use serde::{Deserialize, Serialize};

use crate::machines::WorkspaceKey;

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

/// 可以选的自动获取间隔（分钟）；0 是不自动获取。
pub const AUTO_FETCH_CHOICES: [u32; 6] = [0, 5, 10, 15, 30, 60];

/// 各块的大小（逻辑像素），拖分隔条改的。没拖过的是空，按默认值或窗口大小算。
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
    // 不再要它；旧设置文件里的这一项读的时候忽略，下次保存就没了。1.16 以前的 all_repos
    // （仓库页上「全部仓库」展开着）也一样，改成服务器页以后不要了
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

/// 一个工作区在这台电脑上记的东西：开着哪些标签页、选中哪个、固定的、刚关掉的（各台电脑各记各的，
/// 不同步）。工作区本身（名字、有哪些仓库）保存在它所在的那台电脑上，见 `machines`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    #[serde(flatten)]
    pub key: WorkspaceKey,
    /// 上次看到的工作区名字：连上它所在的电脑之前，标题栏上先显示它
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<RepoEntry>,
    /// 选中的标签页：0 是仓库管理器，1 起是 `tabs` 里的
    #[serde(default, skip_serializing_if = "is_zero")]
    pub active: usize,
    /// 最近关掉的标签页，新的在后；重新打开了就从这里去掉
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed: Vec<RepoEntry>,
    /// 固定的标签页：排在最前面，没有关闭按钮，Ctrl+W 和中键也关不掉
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned: Vec<RepoEntry>,
    /// 最近一次切到这个工作区的时间（1970 年起的毫秒数）：连不上服务器时回到本机最近用的那个
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub used_at: u64,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

impl View {
    pub fn new(key: WorkspaceKey) -> View {
        View {
            key,
            ..View::default()
        }
    }

    /// 仓库移出了工作区：不再算刚关掉的、固定的。开着的标签页由调用方关。
    pub fn forget(&mut self, entry: &RepoEntry) {
        self.closed.retain(|e| e != entry);
        self.pinned.retain(|e| e != entry);
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

    fn normalize(&mut self) {
        self.active = self.active.min(self.tabs.len());
        let tabs = &self.tabs;
        self.closed.retain(|e| !tabs.contains(e));
        self.pinned.retain(|e| tabs.contains(e));
        self.pinned_first();
    }
}

/// 标题栏上显示着的是哪一页。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Front {
    /// 服务器页
    Servers,
    /// 终端页
    Terms,
    /// 工作区页
    #[default]
    Workspaces,
    /// 一个仓库（或子仓库）的标签页
    Repo(RepoEntry),
}

/// 读显示着的那一页：读不懂的（1.17 的终端标签页 `term`，2026-11-20 去掉了）当作工作区页，
/// 不让整个设置文件读不成。
fn lenient_front<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Front, D::Error> {
    let value = serde_json::Value::deserialize(de)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// 标题栏上开着的标签页：全局一份，和选了哪台服务器、哪个工作区都无关，像 VS Code 的编辑器标签：
/// 打开哪个就是哪个。只记在这台电脑上。
/// 1.16 以前是每个工作区各记一份（[`View`]），见 [`Settings::migrate_tabs`]。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tabs {
    /// 仓库和子仓库的标签页，按打开的先后；固定的排在最前。1.17 还有终端的标签页（`terms`），
    /// 后来去掉了，旧设置里的那一项读的时候不管
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoEntry>,
    /// 显示着的那一页
    #[serde(default, deserialize_with = "lenient_front")]
    pub front: Front,
    /// 最近关掉的仓库标签页，新的在后；重新打开了就从这里去掉
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed: Vec<RepoEntry>,
    /// 固定的仓库标签页：排在最前面，没有关闭按钮，Ctrl+W 和中键也关不掉
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned: Vec<RepoEntry>,
}

/// 设置文件里还没有 [`Tabs`]（还没迁过来）时读到的那一份。
static NO_TABS: Tabs = Tabs {
    repos: Vec::new(),
    front: Front::Workspaces,
    closed: Vec::new(),
    pinned: Vec::new(),
};

impl Tabs {
    /// 记下刚关掉的仓库标签页。
    pub fn note_closed(&mut self, entry: RepoEntry) {
        self.closed.retain(|e| *e != entry);
        self.closed.push(entry);
        let extra = self.closed.len().saturating_sub(TabGroup::MAX_CLOSED);
        self.closed.drain(..extra);
    }

    pub fn is_pinned(&self, entry: &RepoEntry) -> bool {
        self.pinned.contains(entry)
    }

    /// 固定的排到最前面，各自内部的先后不变。
    pub fn pinned_first(&mut self) {
        let pinned = &self.pinned;
        self.repos.sort_by_key(|tab| !pinned.contains(tab));
    }

    /// 理一遍：同一个只留一份；服务器移除了的不留；关掉的不含开着的；显示着的那一页不在了就回到
    /// 工作区页。
    fn normalize(&mut self, servers: &[Server]) {
        let known = |entry: &RepoEntry| match entry.server.as_deref() {
            None => true,
            Some(id) => {
                let direct = crate::machines::split_relay(id).map_or(id, |(via, _)| via);
                known_machine(servers, direct)
            }
        };
        let mut seen = std::collections::HashSet::new();
        self.repos.retain(|e| known(e) && seen.insert(e.clone()));
        let repos = &self.repos;
        self.closed.retain(|e| known(e) && !repos.contains(e));
        let extra = self.closed.len().saturating_sub(TabGroup::MAX_CLOSED);
        self.closed.drain(..extra);
        self.pinned.retain(|e| repos.contains(e));
        self.pinned_first();
        let open = match &self.front {
            Front::Repo(entry) => self.repos.contains(entry),
            _ => true,
        };
        if !open {
            self.front = Front::Workspaces;
        }
    }
}

/// 一个工作区：它的仓库，和其中开着的那些标签页。1.14 起工作区保存在它所在的电脑上（见 `machines`），
/// 这里只剩旧设置读进来、交给本机的 kuaid 时用（[`Settings::store_migrated`]）。
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
    /// 1.13 以前的仓库列表。1.14 起仓库列表保存在各台电脑上（见 `machines`）：本机的启动时交给本机
    /// 的 kuaid，服务器的第一次连上时并进那台服务器的列表。这一项原样留着不再改，退回旧版本还能用
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
    /// 旧版本（1.0.x）存标签页的地方，读进来以后挪到第一个工作区里，不再写回去
    #[serde(default, skip_serializing)]
    open_tabs: Vec<RepoEntry>,
    #[serde(default, skip_serializing)]
    active_tab: usize,
    /// 1.13 以前的工作区，和 [`Self::repos`] 一样只在交给本机的 kuaid 时用，原样留着
    #[serde(default)]
    pub workspaces: Vec<TabGroup>,
    /// 1.13 以前现在用的是第几个工作区
    #[serde(default)]
    pub active_workspace: usize,
    /// 旧设置里的仓库列表和工作区已经交给本机的 kuaid 了（1.14 起）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub store_migrated: bool,
    /// 旧设置里各台服务器上的仓库（和起的名字）已经并进那台服务器的列表了：服务器的 id
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merged_servers: Vec<String>,
    /// 工作区页上选中的工作区（1.16 以前是「现在用的工作区」，标题栏的标签页跟着它换）
    #[serde(default)]
    pub current: WorkspaceKey,
    /// 1.16 以前各个工作区在这台电脑上记的标签页。现在只用来记工作区的名字、最近一次选它的时间；
    /// 开着的标签页在 [`Self::tabs`]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub views: Vec<View>,
    /// 标题栏上开着的标签页（全局一份）。没有是还没从 [`Self::views`] 迁过来
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tabs: Option<Tabs>,
    /// 用过上面的工作区的服务器（工作区菜单里列出它们），按第一次用的先后
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub used_machines: Vec<String>,
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
    /// 第一次把目录映射到本机时提示过了（映射的目录不适合跑 git、构建）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mapping_tip_shown: bool,
    /// 点窗口的关闭按钮时不退出，收到托盘
    #[serde(default)]
    pub close_to_tray: bool,
    /// 每隔几分钟自动获取一次（只对正在看的那个仓库）；0 是不自动获取
    #[serde(default = "default_auto_fetch")]
    pub auto_fetch_minutes: u32,
    /// 改动并排着看（左边旧的、右边新的），不勾就是上下排在一栏里
    #[serde(default)]
    pub diff_split: bool,
    /// 文件清单按目录排成树（本地修改、提交详情、比较三处共用），不勾就是平铺
    #[serde(default)]
    pub file_tree: bool,
    /// 本地修改里不列未跟踪的文件
    #[serde(default)]
    pub hide_untracked: bool,
    /// 图片的改动怎么对比：0 并排，1 滑动，2 叠加
    #[serde(default)]
    pub image_diff: u8,
    /// 工作区有改动时切换分支、拉取：先贮藏，做完再恢复
    #[serde(default)]
    pub stash_before_switch: bool,
    /// 标签页上显示关闭按钮；关了以后用中键、Ctrl+W 或右键菜单关，免得手滑点到
    #[serde(default = "default_true")]
    pub tab_close_button: bool,
    /// 不在眼前的终端停下来（超过 10 秒没有新输出）时提醒
    #[serde(default = "default_true")]
    pub notify_terminal_stopped: bool,
    /// 上面那项开着、Kuai 又不在前台时，再发一条 Windows 通知（见 `toast`）
    #[serde(default = "default_true")]
    pub notify_terminal_system: bool,
    /// 本机仓库的终端用哪个程序（`shell::local_shells` 里的 id）；空着自动挑
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub terminal_shell: String,
    /// 终端的字体、光标、滚动和复制（所有终端共用）
    #[serde(default, skip_serializing_if = "TerminalSettings::is_default")]
    pub terminal: TerminalSettings,
    #[serde(default)]
    pub layout: Layout,
    // 1.16 以前仓库页「全部仓库」里卡片的收起、展开记在 collapsed_cards、expanded_cards 里；
    // 改成服务器页以后不要了，旧设置文件里的这两项读的时候忽略，下次保存就没了
    /// 工作区页里收起了子仓库的仓库（那一页默认展开）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folded_subs: Vec<RepoEntry>,
    /// 服务器页里展开了子仓库的仓库（那一页默认收着：列的是整台的仓库，全展开太长）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unfolded_subs: Vec<RepoEntry>,
    /// 各个仓库自己记的东西
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_memos: Vec<RepoMemo>,
    /// 终端页（标题栏上的「终端」）里固定到顶上的终端和仓库、收起的工作区
    #[serde(default, skip_serializing_if = "TermHub::is_empty")]
    pub term_hub: TermHub,
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

/// 一个仓库的第几个终端（终端页里固定的终端按它认：终端重启以后编号换了，位置还是这个）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TermSlot {
    #[serde(flatten)]
    pub entry: RepoEntry,
    pub number: u32,
}

/// 终端页（标题栏上的「终端」）在这台电脑上记的东西，不同步到别的电脑。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermHub {
    /// 固定的终端，按固定的先后
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_terms: Vec<TermSlot>,
    /// 固定的仓库，按固定的先后
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_repos: Vec<RepoEntry>,
    /// 收起的工作区
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collapsed: Vec<WorkspaceKey>,
    /// 收起的仓库：下面不列它的终端
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collapsed_repos: Vec<RepoEntry>,
    /// 侧栏只列哪台电脑上的工作区：没有是全部，空的是本机，别的是服务器的 id。固定的不受它影响
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl TermHub {
    fn is_empty(&self) -> bool {
        self.pinned_terms.is_empty()
            && self.pinned_repos.is_empty()
            && self.collapsed.is_empty()
            && self.collapsed_repos.is_empty()
            && self.scope.is_none()
    }
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
            store_migrated: false,
            merged_servers: Vec::new(),
            current: WorkspaceKey::default(),
            views: Vec::new(),
            tabs: None,
            used_machines: Vec::new(),
            language: Language::default(),
            // 新装的默认深色。设置文件里没有这一项（1.1 以前的）
            // 时取 ThemeChoice 的默认值，照旧跟随系统，见 parse 的测试
            theme: ThemeChoice::Dark,
            light_theme: default_light_theme(),
            dark_theme: default_dark_theme(),
            minimize_to_tray: false,
            mapping_tip_shown: false,
            close_to_tray: false,
            auto_fetch_minutes: default_auto_fetch(),
            diff_split: false,
            file_tree: false,
            image_diff: 0,
            hide_untracked: false,
            stash_before_switch: false,
            tab_close_button: true,
            notify_terminal_stopped: true,
            notify_terminal_system: true,
            terminal_shell: String::new(),
            terminal: TerminalSettings::default(),
            layout: Layout::default(),
            folded_subs: Vec::new(),
            unfolded_subs: Vec::new(),
            repo_memos: Vec::new(),
            term_hub: TermHub::default(),
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
        // 服务器移除了，记的它上面的东西就不留。经服务器连的那几台（`A>B`）跟着那台服务器
        let servers = &self.servers;
        let known = |key: &str| {
            let direct = crate::machines::split_relay(key).map_or(key, |(via, _)| via);
            servers.iter().any(|server| server.id == direct)
        };
        // 仓库记的东西：空的不留，服务器移除了它上面的仓库的也不留。仓库从列表里移除时另外清掉
        self.repo_memos
            .retain(|memo| !memo.is_empty() && memo.repo.server.as_deref().is_none_or(known));
        let mut seen = std::collections::HashSet::new();
        self.folded_subs
            .retain(|e| e.server.as_deref().is_none_or(known) && seen.insert(e.clone()));
        let mut seen = std::collections::HashSet::new();
        self.unfolded_subs
            .retain(|e| e.server.as_deref().is_none_or(known) && seen.insert(e.clone()));
        // 各个工作区在这台电脑上记的标签页：同一个只留一份，服务器移除了它上面的不留
        let mut seen = std::collections::HashSet::new();
        self.views.retain(|view| {
            known_machine(servers, &view.key.machine) && seen.insert(view.key.clone())
        });
        for view in &mut self.views {
            view.normalize();
        }
        let mut seen = std::collections::HashSet::new();
        self.used_machines.retain(|id| {
            known_machine(servers, id) && *id != crate::machines::LOCAL && seen.insert(id.clone())
        });
        self.merged_servers.retain(|id| known_machine(servers, id));
        if !known_machine(servers, &self.current.machine) {
            self.current = WorkspaceKey::default();
        }
        self.migrate_tabs();
        let servers = &self.servers;
        if let Some(tabs) = &mut self.tabs {
            tabs.normalize(servers);
        }
    }

    /// 1.16 以前开着的标签页是每个工作区各记一份（[`View`]）。改成全局一份时只带上现在用的工作区
    /// 那一份，别的工作区的不自动开出来，免得一升级标题栏就堆满。
    /// 迁过了（[`Self::tabs`] 有了）就不再动。
    pub(crate) fn migrate_tabs(&mut self) {
        if self.tabs.is_some() {
            return;
        }
        let mut tabs = Tabs::default();
        if let Some(view) = self.view(&self.current).cloned() {
            // 0 是原来的「仓库」页，现在是服务器页
            tabs.front = match view.active.checked_sub(1).and_then(|ix| view.tabs.get(ix)) {
                Some(entry) => Front::Repo(entry.clone()),
                None if view.active == 0 && !view.tabs.is_empty() => Front::Servers,
                None => Front::Workspaces,
            };
            tabs.repos = view.tabs;
            tabs.closed = view.closed;
            tabs.pinned = view.pinned;
        }
        self.tabs = Some(tabs);
    }

    /// 标题栏上开着的标签页。
    pub fn tabs(&self) -> &Tabs {
        self.tabs.as_ref().unwrap_or(&NO_TABS)
    }

    pub fn tabs_mut(&mut self) -> &mut Tabs {
        self.tabs.get_or_insert_with(Tabs::default)
    }

    /// 这个工作区在这台电脑上记的标签页；没记过就是空的。
    pub fn view(&self, key: &WorkspaceKey) -> Option<&View> {
        self.views.iter().find(|view| view.key == *key)
    }

    /// 同 [`Self::view`]，没记过就新建一份。
    pub fn view_mut(&mut self, key: &WorkspaceKey) -> &mut View {
        let at = match self.views.iter().position(|view| view.key == *key) {
            Some(at) => at,
            None => {
                self.views.push(View::new(key.clone()));
                self.views.len() - 1
            }
        };
        &mut self.views[at]
    }

    pub fn server(&self, id: &str) -> Option<&Server> {
        self.servers.iter().find(|server| server.id == id)
    }

    /// 盘符映射的共享名（网络路径 `\\kuai\<共享名>`，资源管理器里也显示它）：服务器名加目录名。
    /// 网络路径里只能有英文字母、数字（见 [`fr_mount::unc_share`]），中文的服务器名都变成横线，
    /// 更容易和别的映射重；在网络路径里重了，就再加上这条是所有映射里的第几条。
    pub fn share_name(&self, server: &Server, mapping: &Mapping) -> String {
        let name = |server: &Server, mapping: &Mapping| match base_name(&mapping.remote) {
            "" => server.name.clone(),
            dir => format!("{}-{dir}", server.name),
        };
        let mine = name(server, mapping);
        let unc = fr_mount::unc_share(&mine);
        let all = || {
            self.servers
                .iter()
                .flat_map(|s| s.mappings.iter().map(move |m| (s, m)))
        };
        let clash =
            all().any(|(s, m)| m.id != mapping.id && fr_mount::unc_share(&name(s, m)) == unc);
        match (clash, all().position(|(_, m)| m.id == mapping.id)) {
            (true, Some(at)) => format!("{mine}-{}", at + 1),
            _ => mine,
        }
    }

    /// 资源管理器里这个盘的共享名（网络路径里实际的那个），和要写进 `_LabelFromReg` 的名称：
    /// 起了名称用名称；没起、共享名里有网络路径放不下的字（中文服务器名等）时用原来的共享名，
    /// 不然显示成一串横线。映射成文件夹的没有名称。
    pub fn explorer_name(&self, server: &Server, mapping: &Mapping) -> (String, Option<String>) {
        let share = self.share_name(server, mapping);
        let unc = fr_mount::unc_share(&share);
        let label = match mapping.drive_name() {
            Some(name) => Some(name.to_string()),
            None if mapping.is_drive() && unc != share => Some(share),
            None => None,
        };
        (unc, label)
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

    /// 仓库从列表里移除了：它（和它里面的子仓库）记的东西不留，各个工作区里记着它刚关掉、
    /// 固定着的也去掉。
    pub fn forget_repo(&mut self, entry: &RepoEntry) {
        self.repo_memos
            .retain(|memo| memo.repo != *entry && !entry.contains(&memo.repo));
        for view in &mut self.views {
            view.forget(entry);
        }
        if let Some(tabs) = &mut self.tabs {
            tabs.closed.retain(|e| e != entry);
        }
    }
}

/// 是本机，或者还配置着的服务器。
fn known_machine(servers: &[Server], machine: &str) -> bool {
    machine == crate::machines::LOCAL || servers.iter().any(|server| server.id == machine)
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
    /// 映射到本机的目录（见 `mapping`）。不在服务器的表单里：加、改、删各是各的，马上存
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mappings: Vec<Mapping>,
    /// 启动 Kuai 时不自动连接这台（表单里是「启动时自动连接」，默认勾着，所以存的是反过来的：
    /// 旧的设置文件里没有这一项，也就是自动连）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub manual_connect: bool,
    /// 它是哪台电脑：连上时它的 kuaid 报的标识（`fr_core::store::machine_id`），随服务器列表交给本机的
    /// kuaid，别的电脑经本机连它时也认得。不在表单里；改了地址、跳板机就清掉，下次连上再记
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// 把服务器上的一个目录映射成本机的盘符或文件夹。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    pub id: String,
    /// 服务器上的目录（完整路径）
    pub remote: String,
    /// 本机的位置：盘符（`K:`）或者文件夹
    pub local: String,
    /// 连接服务器后自动映射；否则要在卡片上点「立即映射」
    #[serde(default)]
    pub auto: bool,
    /// 只读：本机改不了服务器上的东西
    #[serde(default)]
    pub read_only: bool,
    /// 盘符映射在资源管理器里显示的名称（「名称 (K:)」）；空着显示「服务器名-目录名 (\\kuai)」。
    /// 映射成文件夹的没有这一项
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

impl Mapping {
    /// 名称最多几个字：资源管理器的卷标最多 32 个字符。
    pub const MAX_NAME: usize = 32;

    /// 起了名称的盘符映射的名称；没起、映射成文件夹的是 `None`。
    pub fn drive_name(&self) -> Option<&str> {
        (self.is_drive() && !self.name.trim().is_empty()).then(|| self.name.trim())
    }

    /// 界面上怎么称呼这条映射的本机位置：起了名称的盘符写「名称 (K:)」，和资源管理器里一样；
    /// 没起的写盘符；文件夹写文件夹名。
    pub fn local_label(&self) -> String {
        match (self.drive_name(), self.is_drive()) {
            (Some(name), _) => format!("{name} ({})", self.local),
            (None, true) => self.local.clone(),
            (None, false) => {
                let path = self.local.trim_end_matches(['/', '\\']);
                path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
            }
        }
    }

    /// 本机的位置是盘符（`K:`），不是文件夹。
    pub fn is_drive(&self) -> bool {
        fr_mount::is_drive(&self.local)
    }

    /// 映射出来的根目录在本机怎么写：`K:\`，或者那个文件夹。
    pub fn local_root(&self) -> String {
        match self.is_drive() {
            true => format!("{}\\", self.local),
            false => self.local.clone(),
        }
    }

    /// 服务器上的 `path` 在不在这条映射里；在的话给出它在本机的路径。
    pub fn local_path(&self, path: &str) -> Option<String> {
        let clean = |path: &str| path.replace('\\', "/").trim_end_matches('/').to_string();
        let (root, path) = (clean(&self.remote), clean(path));
        let rest = match path.strip_prefix(&root) {
            Some("") => "",
            // 根是 `/` 时 root 是空的，path 的开头就是 `/`
            Some(rest) if rest.starts_with('/') => &rest[1..],
            _ => return None,
        };
        let base = self.local.trim_end_matches('\\');
        Some(match rest.is_empty() {
            true => self.local_root(),
            false => format!("{base}\\{}", rest.replace('/', "\\")),
        })
    }
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

    /// 地址里有没有 [`Self::parse`] 解释不了、会悄悄丢掉或者原样交给 ssh 的部分：方括号后面跟的
    /// 不是「:端口」、只有左方括号、一个冒号后面不是端口、主机名里有空白。表单保存和测试连接前拿它
    /// 校验，免得填错的地址被改成另一个目标。
    pub fn well_formed(destination: &str) -> bool {
        let text = destination.trim();
        let rest = text
            .strip_prefix("ssh://")
            .map_or(text, |rest| rest.trim_end_matches('/'));
        let (_, rest) = rest.rsplit_once('@').unwrap_or(("", rest));
        let port = |port: &str| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit());
        if rest.chars().any(char::is_whitespace) {
            return false;
        }
        match rest.strip_prefix('[') {
            Some(inner) => inner.split_once(']').is_some_and(|(host, tail)| {
                !host.is_empty() && (tail.is_empty() || tail.strip_prefix(':').is_some_and(port))
            }),
            None if rest.contains(']') => false,
            // 只有一个冒号：后面得是端口；冒号多的是不带方括号的 IPv6 地址
            None => match rest.split_once(':') {
                Some((_, tail)) if !tail.contains(':') => port(tail),
                _ => true,
            },
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

/// 路径的最后一段（`/data/work/` 是 `work`，`/` 是空的）。两种斜杠都认。
pub fn base_name(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
}

impl RepoEntry {
    /// 目录名。
    pub fn name(&self) -> &str {
        base_name(&self.path)
    }

    /// 是不是同一个仓库。Windows 上的路径不分大小写、正反斜杠：同一个目录有几种写法，kuaid 报上来的
    /// 终端名字是小写、正斜杠的（见 [`fr_core::dir::windows_key`]），没登记过的子仓库的终端就按那种
    /// 写法列着，从终端页打开它时要认得出它就是开着的那个标签页。
    pub fn same_repo(&self, other: &RepoEntry) -> bool {
        let windows = |path: &str| path.as_bytes().get(1) == Some(&b':');
        self.server == other.server
            && (self.path == other.path
                || (windows(&self.path)
                    && windows(&other.path)
                    && fr_core::dir::windows_key(&self.path)
                        == fr_core::dir::windows_key(&other.path)))
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

/// 读设置时出的问题。原来只写一行日志：设置悄悄回到默认（主机列表、标签页都没了），用户不知道
/// 为什么，也不知道原文件去了哪。启动后在界面上说一声（见 [`LoadIssue::text`]）。
#[derive(Debug, Clone)]
pub enum LoadIssue {
    /// 文件在，读不了（被占用、没权限、不是 UTF-8）：用的是默认设置，这次运行不写它
    Unreadable { path: PathBuf, err: String },
    /// 有几项读不出来（字段的类型不对），用了默认值，别的照读；原文件另存了一份
    Partial { items: Vec<String>, backup: PathBuf },
    /// 整份读不出来，用的是默认设置；原文件另存了一份
    Broken { backup: PathBuf },
    /// 读不出来，也没能另存一份：这次运行不写它，免得把它盖掉
    Kept { path: PathBuf, err: String },
}

impl LoadIssue {
    /// 给用户看的话：出了什么事、原文件在哪、改动存不存。
    pub fn text(&self) -> String {
        match self {
            LoadIssue::Unreadable { path, err } => tf!(
                "无法读取设置文件 {path}（{err}），现在用的是默认设置。这次运行中的改动不会保存，以免覆盖原文件。",
                path = path.display(),
                err
            ),
            LoadIssue::Partial { items, backup } => tf!(
                "设置文件中有几项无法读取（{items}），已改用默认值，其余设置照常读取。原文件另存为 {backup}。",
                items = items.join(", "),
                backup = backup.display()
            ),
            LoadIssue::Broken { backup } => tf!(
                "设置文件无法读取，现在用的是默认设置。原文件另存为 {backup}。",
                backup = backup.display()
            ),
            LoadIssue::Kept { path, err } => tf!(
                "设置文件 {path} 无法读取，也无法另存备份（{err}），现在用的是默认设置。这次运行中的改动不会保存，以免覆盖原文件。",
                path = path.display(),
                err
            ),
        }
    }

    /// 读出来的设置不全：主机列表可能是空的，不能拿它去盖掉本机 kuaid 记着的那份。
    pub fn incomplete(&self) -> bool {
        !matches!(self, LoadIssue::Partial { items, .. } if !items.iter().any(|item| item == "servers"))
    }
}

/// 这次运行不写设置文件：它读不了，或者坏了又没能另存一份，写了就把原来的盖掉了。
static READ_ONLY: AtomicBool = AtomicBool::new(false);

/// 这次运行不写设置文件（见 [`READ_ONLY`]）。
pub fn read_only() -> bool {
    READ_ONLY.load(Ordering::Relaxed)
}

/// 读设置。没有设置文件（新装）是默认设置，不算出问题。
pub fn load() -> (Settings, Option<LoadIssue>) {
    if std::env::var_os("FR_SETTINGS").is_none() {
        adopt_legacy(&appdata());
    }
    let path = file();
    let unreadable = |err: String| {
        log::warn!("无法读取设置文件 {}：{err}", path.display());
        READ_ONLY.store(true, Ordering::Relaxed);
        let issue = LoadIssue::Unreadable {
            path: path.clone(),
            err,
        };
        (Settings::default(), Some(issue))
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return (Settings::default(), None);
        }
        Err(err) => return unreadable(err.to_string()),
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return unreadable(tr("不是 UTF-8 编码的文本").to_string());
    };
    let err = match parse(&text) {
        Ok(settings) => return (settings, None),
        Err(err) => err,
    };
    log::warn!("设置文件解析失败：{err}");
    let lenient = parse_lenient(&text);
    // 原文件另存一份（复制，不改名：读得出来的那些项接着用，存盘时才盖掉它）
    let backup = path.with_extension(format!(
        "json.broken-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    if let Err(copy) = std::fs::copy(&path, &backup) {
        log::warn!("设置文件另存不了（{copy}），这次运行不写它");
        READ_ONLY.store(true, Ordering::Relaxed);
        let issue = LoadIssue::Kept {
            path: path.clone(),
            err: copy.to_string(),
        };
        let settings = lenient.map(|(settings, _)| settings).unwrap_or_default();
        return (settings, Some(issue));
    }
    log::warn!("设置文件已另存为 {}", backup.display());
    match lenient {
        Some((settings, items)) => (settings, Some(LoadIssue::Partial { items, backup })),
        None => (Settings::default(), Some(LoadIssue::Broken { backup })),
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

/// 整份读不出来（多半是某一项的类型不对：手改坏了，或者旧版本读到了新格式）时一项一项地读：
/// 读得出来的项照用，读不出来的用默认值；列表（主机、标签页这些）里读不出来的那几个丢掉，别的
/// 留着。返回读出来的设置和读坏了的项的名字。连 JSON 都不是时是 None。
fn parse_lenient(text: &str) -> Option<(Settings, Vec<String>)> {
    use serde_json::Value;
    let Value::Object(file) = serde_json::from_str::<Value>(text).ok()? else {
        return None;
    };
    let Value::Object(mut merged) = serde_json::to_value(Settings::default()).ok()? else {
        return None;
    };
    let fits = |map: &serde_json::Map<String, Value>| {
        serde_json::from_value::<Settings>(Value::Object(map.clone())).is_ok()
    };
    let has_language = file.contains_key("language");
    let mut bad = Vec::new();
    for (key, value) in file {
        let mut trial = merged.clone();
        trial.insert(key.clone(), value.clone());
        if fits(&trial) {
            merged = trial;
            continue;
        }
        bad.push(key.clone());
        let Value::Array(items) = value else {
            continue;
        };
        let mut kept: Vec<Value> = Vec::new();
        for item in items {
            let mut trial = merged.clone();
            let mut list = kept.clone();
            list.push(item.clone());
            trial.insert(key.clone(), Value::Array(list));
            if fits(&trial) {
                kept.push(item);
            }
        }
        merged.insert(key, Value::Array(kept));
    }
    let mut settings: Settings = serde_json::from_value(Value::Object(merged)).ok()?;
    if !has_language {
        settings.language = Language::Chinese;
    }
    settings.normalize();
    Some((settings, bad))
}

/// 先写临时文件、刷到盘上再改名：写到一半退出、断电也不会留下坏文件（原来不刷盘，NTFS 上改名
/// 可能先落盘，重启后是个空文件，下次读又回到默认设置）。读设置时出了问题、这次运行不写它的，
/// 返回出错。
pub fn save(settings: &Settings) -> Result<()> {
    if READ_ONLY.load(Ordering::Relaxed) {
        anyhow::bail!(tr("设置文件读取失败，这次运行中的改动不会保存。"));
    }
    let path = file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| tf!("创建目录失败：{dir}", dir = dir.display()))?;
    }
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let written = (|| {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(settings)?)?;
        file.sync_all()
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| tf!("保存设置失败：{tmp}", tmp = tmp.display()));
    }
    std::fs::rename(&tmp, &path).with_context(|| tf!("保存设置失败：{path}", path = path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 设置里有几项读不出来_别的照读() {
        let text = r#"{
            "language": "en",
            "servers": [
                {"id": "a", "name": "A", "destination": "user@a"},
                {"id": "b", "name": 5, "destination": "user@b"}
            ],
            "auto_fetch_minutes": "10",
            "diff_split": true
        }"#;
        assert!(parse(text).is_err(), "整份读是读不出来的");
        let (settings, bad) = parse_lenient(text).unwrap();
        assert_eq!(settings.language, Language::English);
        let ids: Vec<&str> = settings.servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["a"], "读不出来的那台丢掉，别的留着");
        assert!(settings.diff_split);
        assert_eq!(
            settings.auto_fetch_minutes,
            Settings::default().auto_fetch_minutes
        );
        assert_eq!(bad, ["servers", "auto_fetch_minutes"]);
        // 少了主机的不能拿去盖本机 kuaid 的那份
        let issue = |items: &[&str]| LoadIssue::Partial {
            items: items.iter().map(|item| item.to_string()).collect(),
            backup: PathBuf::new(),
        };
        assert!(issue(&["servers"]).incomplete());
        assert!(!issue(&["auto_fetch_minutes"]).incomplete());
        // 没有语言这一项的老设置还是中文
        let (old, _) = parse_lenient(r#"{"auto_fetch_minutes": "x"}"#).unwrap();
        assert_eq!(old.language, Language::Chinese);
        // 连 JSON 都不是
        assert!(parse_lenient("{oops").is_none());
    }

    #[test]
    fn 同一个仓库的几种写法() {
        let at = |server: Option<&str>, path: &str| RepoEntry {
            server: server.map(str::to_string),
            path: path.into(),
        };
        let tab = at(None, r"D:\Work\Kuai\sub");
        assert!(tab.same_repo(&at(None, "d:/work/kuai/sub")), "kuaid 的写法");
        assert!(!tab.same_repo(&at(None, "d:/work/kuai/sub2")));
        assert!(
            !tab.same_repo(&at(Some("sa"), "d:/work/kuai/sub")),
            "别的电脑"
        );
        // Linux 上的路径分大小写
        assert!(!at(Some("sb"), "/data/Repo").same_repo(&at(Some("sb"), "/data/repo")));
        assert!(at(Some("sb"), "/data/repo").same_repo(&at(Some("sb"), "/data/repo")));
    }

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
        assert_eq!(base_name("/"), "");
        assert_eq!(base_name("C:/"), "C:");
    }

    #[test]
    fn 盘符映射的共享名和资源管理器里的名称() {
        let mapping = |id: &str, remote: &str, local: &str| Mapping {
            id: id.into(),
            remote: remote.into(),
            local: local.into(),
            ..Mapping::default()
        };
        let server = |id: &str, name: &str, mappings: Vec<Mapping>| Server {
            id: id.into(),
            name: name.into(),
            mappings,
            ..Server::default()
        };
        let settings = Settings {
            servers: vec![
                server("a", "devbox", vec![mapping("m1", "/data/src", "K:")]),
                server("b", "测试机", vec![mapping("m2", "/data/src", "L:")]),
                server("c", "开发机", vec![mapping("m3", "/home/me/src", "M:")]),
                server("d", "devbox", vec![mapping("m4", "/", r"C:\Kuai\root")]),
            ],
            ..Settings::default()
        };
        let names = |ix: usize| {
            let server = &settings.servers[ix];
            settings.explorer_name(server, &server.mappings[0])
        };
        // 英文的名字：网络路径里原样，资源管理器显示「devbox-src (\\kuai) (K:)」，不用另写名称
        assert_eq!(names(0), ("devbox-src".into(), None));
        // 两台中文名的服务器都变成「----src」：加上是第几条，各不相同；名称写回原来的中文
        assert_eq!(names(1), ("----src-2".into(), Some("测试机-src-2".into())));
        assert_eq!(names(2), ("----src-3".into(), Some("开发机-src-3".into())));
        // 映射成文件夹的没有名称
        assert_eq!(names(3).1, None);
        // 起了名称用名称
        let mut named = settings.servers[1].mappings[0].clone();
        named.name = "源码".into();
        assert_eq!(
            settings.explorer_name(&settings.servers[1], &named),
            ("----src-2".into(), Some("源码".into()))
        );
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
        assert_eq!(settings.workspaces[0].tabs.len(), 1);
        assert_eq!(settings.workspaces[0].active, 1);
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
        // 改完是空的就不留；仓库从列表里移除以后也不留
        settings.edit_memo(&repo("C:/b"), |_| {});
        assert!(settings.memo(&repo("C:/b")).is_none());
        settings.forget_repo(&repo("C:/a"));
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
        // 子仓库不在仓库列表里；仓库列表保存在各台电脑上，这里也不知道全不全：都照样留着
        settings.edit_memo(&repo(r"C:\a\packages\plugin-x"), hide);
        settings.edit_memo(&repo(r"C:\ab"), hide);
        settings.normalize();
        assert!(settings.memo(&repo(r"C:\a\packages\plugin-x")).is_some());
        assert!(settings.memo(&repo(r"C:\ab")).is_some());
        let server = RepoEntry {
            server: Some("s1".into()),
            path: "/srv/a/plugin".into(),
        };
        assert!(!repo("/srv/a").contains(&server));
        assert!(!repo("/srv/a").contains(&repo("/srv/a")));
        // 根仓库移除以后，它里面的子仓库记的也不留；名字只是以根仓库的名字开头的不算在它里面
        settings.forget_repo(&repo(r"C:\a"));
        assert!(settings.memo(&repo(r"C:\a\packages\plugin-x")).is_none());
        assert!(settings.memo(&repo(r"C:\ab")).is_some());
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

        for good in [
            "dev",
            "u@dev:2200",
            "ssh://u@[2001:db8::1]:2222",
            "[::1]",
            "u@2001:db8::1",
        ] {
            assert!(Target::well_formed(good), "{good}");
        }
        // 方括号后面多了东西、只有左方括号、一个冒号后面不是端口、有空格：parse 会丢掉或者原样交给 ssh
        for bad in [
            "[::1]xyz", "[::1]:", "[::1", "::1]", "dev:abc", "dev:", "my host",
        ] {
            assert!(!Target::well_formed(bad), "{bad}");
        }

        assert!(target("dev", "", "22").port_ok());
        assert!(target("dev", "", "65535").port_ok());
        for bad in ["", "0", "65536", "22a", "-1"] {
            assert!(!target("dev", "", bad).port_ok(), "{bad}");
        }
    }

    #[test]
    fn 服务器上的路径在本机怎么写() {
        let map = |remote: &str, local: &str| Mapping {
            remote: remote.into(),
            local: local.into(),
            ..Mapping::default()
        };
        let drive = map("/data/work", "K:");
        assert!(drive.is_drive());
        assert_eq!(drive.local_root(), r"K:\");
        assert_eq!(drive.local_path("/data/work").as_deref(), Some(r"K:\"));
        assert_eq!(drive.local_path("/data/work/").as_deref(), Some(r"K:\"));
        assert_eq!(
            drive.local_path("/data/work/app/src/main.rs").as_deref(),
            Some(r"K:\app\src\main.rs")
        );
        // 名字只是开头一样的不算在里面
        assert_eq!(drive.local_path("/data/workspace/x"), None);
        assert_eq!(drive.local_path("/data"), None);

        let folder = map("/var/log/app/", r"C:\Users\me\Kuai\logs");
        assert!(!folder.is_drive());
        assert_eq!(folder.local_root(), r"C:\Users\me\Kuai\logs");
        assert_eq!(
            folder.local_path("/var/log/app/today.log").as_deref(),
            Some(r"C:\Users\me\Kuai\logs\today.log")
        );
        // 映射的是根目录
        let root = map("/", "M:");
        assert_eq!(root.local_path("/").as_deref(), Some(r"M:\"));
        assert_eq!(
            root.local_path("/etc/hosts").as_deref(),
            Some(r"M:\etc\hosts")
        );
        // Windows 服务器：反斜杠、正斜杠都认
        let windows = map(r"D:\repos", "N:");
        assert_eq!(
            windows.local_path("D:/repos/kuai").as_deref(),
            Some(r"N:\kuai")
        );

        // 旧设置里没有映射；有映射的存了再读回来一样
        let old = r#"{"servers":[{"id":"s1","name":"dev","destination":"root@dev"}]}"#;
        let mut server = super::parse(old).unwrap().servers.remove(0);
        assert!(server.mappings.is_empty());
        assert!(!serde_json::to_string(&server).unwrap().contains("mappings"));
        server.mappings.push(Mapping {
            id: "m1".into(),
            auto: true,
            ..drive
        });
        let text = serde_json::to_string(&server).unwrap();
        assert_eq!(serde_json::from_str::<Server>(&text).unwrap(), server);
        // 没起名称的不写进文件
        assert!(!text.contains("\"name\":\"\""), "{text}");
    }

    #[test]
    fn 映射的名称() {
        let map = |local: &str, name: &str| Mapping {
            remote: "/data/work".into(),
            local: local.into(),
            name: name.into(),
            ..Mapping::default()
        };
        // 盘符：起了名称写成资源管理器里的样子，没起写盘符
        assert_eq!(map("K:", "项目").drive_name(), Some("项目"));
        assert_eq!(map("K:", "项目").local_label(), "项目 (K:)");
        assert_eq!(map("K:", "").local_label(), "K:");
        assert_eq!(map("K:", "  ").drive_name(), None);
        // 文件夹没有名称这一项，写文件夹名
        assert_eq!(map(r"C:\Users\me\Kuai\logs", "日志").drive_name(), None);
        assert_eq!(map(r"C:\Users\me\Kuai\logs", "").local_label(), "logs");
        // 旧的映射没有名称
        let old = r#"{"id":"m1","remote":"/data","local":"K:"}"#;
        assert_eq!(serde_json::from_str::<Mapping>(old).unwrap().name, "");
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
    fn 旧的仓库页卡片收起记录读的时候忽略() {
        // 1.16 以前仓库页「全部仓库」里卡片的收起、展开和「全部仓库」展没展开记在这几项里，
        // 改成服务器页以后不要了：照样能读，存的时候不再写
        let old = r#"{"collapsed_cards":["local"],"expanded_cards":["s1"],
            "layout":{"all_repos":true,"sidebar":300.0}}"#;
        let settings: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(settings.layout.sidebar, Some(300.0));
        let text = serde_json::to_string(&settings).unwrap();
        assert!(
            !text.contains("_cards") && !text.contains("all_repos"),
            "{text}"
        );
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
        assert_eq!(settings.workspaces[0].active, 0);
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

    #[test]
    fn 服务器默认启动时自动连接_不连的才写进设置() {
        // 旧的设置文件里没有这一项：自动连
        let old = r#"{"servers":[{"id":"s1","name":"dev","destination":"u@h"}]}"#;
        let mut settings = super::parse(old).unwrap();
        assert!(!settings.servers[0].manual_connect);
        assert!(
            !serde_json::to_string(&settings)
                .unwrap()
                .contains("manual_connect")
        );
        // 去掉了「启动时自动连接」的勾：存下来，读回来还是不连
        settings.servers[0].manual_connect = true;
        let text = serde_json::to_string(&settings).unwrap();
        assert!(text.contains(r#""manual_connect":true"#));
        assert!(super::parse(&text).unwrap().servers[0].manual_connect);
    }

    #[test]
    fn 终端页的固定_没有就不写_终端的位置和仓库写在一层() {
        // 什么都没固定、没收起：设置文件里没有这一项，旧的设置文件也读得进来
        let text = serde_json::to_string(&Settings::default()).unwrap();
        assert!(!text.contains("term_hub"));
        assert_eq!(super::parse(&text).unwrap().term_hub, TermHub::default());
        let slot = TermSlot {
            entry: RepoEntry {
                server: Some("s1".into()),
                path: "/data/kuai".into(),
            },
            number: 2,
        };
        let json = serde_json::to_value(&slot).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"server": "s1", "path": "/data/kuai", "number": 2})
        );
        assert_eq!(serde_json::from_value::<TermSlot>(json).unwrap(), slot);
        let mut settings = Settings::default();
        settings.term_hub.pinned_terms.push(slot.clone());
        let text = serde_json::to_string(&settings).unwrap();
        assert_eq!(
            super::parse(&text).unwrap().term_hub.pinned_terms,
            vec![slot.clone()]
        );
        // 只收起了一个仓库也要存下来
        let mut settings = Settings::default();
        settings.term_hub.collapsed_repos.push(slot.entry.clone());
        let text = serde_json::to_string(&settings).unwrap();
        assert_eq!(
            super::parse(&text).unwrap().term_hub.collapsed_repos,
            vec![slot.entry]
        );
        // 只选了看哪台服务器也要存下来；选的是本机（空的）和没选不是一回事
        let mut settings = Settings::default();
        settings.term_hub.scope = Some(String::new());
        let text = serde_json::to_string(&settings).unwrap();
        assert_eq!(
            super::parse(&text).unwrap().term_hub.scope,
            Some(String::new())
        );
    }

    #[test]
    fn 开着的标签页改成全局一份_只带现在用的工作区那一份() {
        let repo = |path: &str| RepoEntry {
            server: None,
            path: path.into(),
        };
        let text = r#"{
            "current": {"id": "w2"},
            "views": [
                {"id": "w1", "tabs": [{"path": "C:/a"}], "active": 1},
                {"id": "w2", "tabs": [{"path": "C:/b"}, {"path": "C:/c"}], "active": 1,
                 "closed": [{"path": "C:/d"}, {"path": "C:/b"}], "pinned": [{"path": "C:/c"}]}
            ]
        }"#;
        let settings = super::parse(text).unwrap();
        let tabs = settings.tabs();
        // 别的工作区（w1）的不带；固定的排在最前，选中的还是原来那个
        assert_eq!(tabs.repos, [repo("C:/c"), repo("C:/b")]);
        assert_eq!(tabs.front, Front::Repo(repo("C:/b")));
        assert_eq!(tabs.pinned, [repo("C:/c")]);
        // 开着的不算刚关掉的
        assert_eq!(tabs.closed, [repo("C:/d")]);
        // 迁过一次就不再动：存下来再读还是这一份
        let saved = serde_json::to_string(&settings).unwrap();
        assert_eq!(super::parse(&saved).unwrap().tabs(), tabs);
        // 原来选中的是「仓库」页（0）：现在是服务器页；什么都没开：工作区页
        let servers = text.replace("\"active\": 1,\n", "\"active\": 0,\n");
        assert_eq!(super::parse(&servers).unwrap().tabs().front, Front::Servers);
        let fresh = super::parse("{}").unwrap();
        assert_eq!(fresh.tabs(), &Tabs::default());
        assert_eq!(fresh.tabs().front, Front::Workspaces);
    }

    #[test]
    fn 标签页里服务器移除了的不留_显示着的不在了回到工作区页() {
        let entry = |server: Option<&str>, path: &str| RepoEntry {
            server: server.map(str::to_string),
            path: path.into(),
        };
        let mut settings = Settings::default();
        settings.servers.push(Server {
            id: "s1".into(),
            ..Server::default()
        });
        let gone = entry(Some("s9"), "/x");
        let relayed = entry(Some("s1>s7"), "/y");
        let tabs = settings.tabs_mut();
        tabs.repos = vec![
            entry(None, "C:/a"),
            gone.clone(),
            relayed.clone(),
            gone.clone(),
        ];
        tabs.front = Front::Repo(gone.clone());
        settings.normalize();
        let tabs = settings.tabs();
        assert_eq!(tabs.repos, [entry(None, "C:/a"), relayed]);
        assert_eq!(tabs.front, Front::Workspaces);
    }

    #[test]
    fn 旧设置里的终端标签页_读的时候不管() {
        // 1.17 显示着终端标签页时存下的样子
        let text = r#"{"tabs": {
            "repos": [{"path": "C:/a"}],
            "terms": [{"path": "C:/a", "number": 1}],
            "front": {"term": {"path": "C:/a", "number": 1}}
        }}"#;
        let settings: Settings = serde_json::from_str(text).unwrap();
        let tabs = settings.tabs();
        assert_eq!(tabs.repos.len(), 1);
        assert_eq!(tabs.front, Front::Workspaces);
        // 读得懂的照常读
        let text =
            r#"{"tabs": {"repos": [{"path": "C:/a"}], "front": {"repo": {"path": "C:/a"}}}}"#;
        let settings: Settings = serde_json::from_str(text).unwrap();
        assert!(matches!(settings.tabs().front, Front::Repo(_)));
        let text = r#"{"tabs": {"front": "servers"}}"#;
        let settings: Settings = serde_json::from_str(text).unwrap();
        assert_eq!(settings.tabs().front, Front::Servers);
    }
}
