//! 会话页（标题栏上的「会话」，Ctrl+Shift+A）：所有工作区里各个仓库的终端合在一起管。
//!
//! 仓库标签页里的终端页是它只看一个仓库的样子：两边是同一批终端（都在各台电脑的 kuaid 里，数据
//! 在 [`TermBoard`]）。设计见 `docs/design/terminal-hub/`（方向 C，带固定）。
//!
//! 左边按工作区分节：工作区是带颜色的分节标题（滚动时吸在顶上），下面是它的仓库和仓库的终端；
//! 工作区和仓库都可以收起（收着时写里面有几个终端停下了、在跑、空着）。终端的图标看里面跑的程序、
//! 颜色看状态。侧栏的设计见 `docs/design/terminal-sidebar/`（方向 C，2026-11-20）。
//! 右边顶上是选中的仓库的终端，排成标签，下面是选中那个的画面。新建、
//! 重启、结束都在这一页：行尾的按钮、标签栏上的 +、标签上的 ×、工具栏的「重启」、右键菜单。里面还在
//! 输出的终端，结束、重启前先问一句。
//!
//! 固定（只记在这台电脑上）：
//!
//! - 终端：侧栏最上面「已固定」一组里一行；在它自己的仓库里排在最前面（侧栏和标签栏都是）。别的
//!   仓库的标签栏里没有它。它结束了
//!   就不再固定；重启不算结束（按「哪个仓库的第几个」认，见 [`TermSlot`]）。
//! - 仓库：排在「已固定」里固定的终端后面，原来的工作区里不再列它。
//!
//! 「已固定」下面有一个下拉，选只列哪台服务器上的工作区（全部、本机、各台服务器；没有配置服务器
//! 时没有它）。固定的不受它影响，右边显示着的终端也不动。设计见 `docs/design/machine-switch/`（方向 A）。
//!
//! 画面有两种接法。平常是「只看」（[`TerminalView::watch`]）：只看一眼不改终端的大小，敲的字照样
//! 写得进去；人把焦点放进画面（开始输入）、往里打字、点画面以后，大小改成这一页放得下的
//! （`TerminalView::claim`）。别处（另一台电脑的 Kuai）改了大小时跟着它，这边再打字才要回来。
//! 这一页自己开的（新建、重启）是正经接着的（[`TerminalView::new`]），大小跟着这一页的画面，也就和
//! 仓库的终端页一样大；选到别的终端再回来，就换成只看。
//!
//! 焦点（`docs/design/sessions-status/` 第 7 节）：打开这一页、点侧栏里的终端、点标签、
//! 新开一个终端时进到终端里，之后的键都归里面的程序，只留 F6（Shift+F6）回到侧栏、Ctrl+Shift+↑、↓
//! 沿着侧栏换终端。侧栏上 ↑↓ 选一行，Enter、→ 进到终端里输入，Ctrl+Enter 打开它的仓库（显示 Git 页），
//! F2 改名，空格勾上并排查看。输入时画面顶上一条主色细线，工具栏写「输入中」。
//!
//! 状态（运行中、静默、待查看、空闲、已退出、状态未知）见 [`TermBoard`]；主机断开时它的终端留在侧栏里
//! 灰掉，分节头写「未连接」带「重新连接」。终端可以改名（右键「重命名…」、双击名字、F2），同一个仓库
//! 里重名的自动加序号。侧栏里勾上 2～4 个可以并排查看（[`crate::term_split`]）。
//!
//! 侧栏和仓库页的侧栏一样宽、一样的底色和字号，右边上面一条和仓库终端页的工具栏一样高，下面全是画面：
//! 这样画面和仓库的终端页一样大，终端是几行几列就正好放得下，字不用缩小。侧栏右边有分隔条，拖它改的
//! 就是仓库页侧栏的那个宽度（见 [`crate::side_split`]）。

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use fr_channel::Channel;
use fr_core::i18n::{tr, trc};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::machines::{LOCAL, WorkspaceKey, machine_of, split_relay};
use crate::presets;
use crate::repo::tool_button;
use crate::settings::{RepoEntry, TermSlot};
use crate::term_board::{
    GroupKey, Program, RepoGroup, Row, Shown, TermBoard, TermId, TermState, free_number,
    state_words,
};
use crate::term_split::{self, Handler};
use crate::terminal::{self, LinkBase, TerminalEvent, TerminalView};
use crate::ui;
use crate::workspace::{PickFor, Picked, QuickOpen, show_picker};

/// 侧栏、画面上面那一条的高度（和仓库页的工具栏一样）
const HEAD_HEIGHT: f32 = 52.;
/// 侧栏里一行的高度（和仓库终端页侧栏里的一行一样）
const ROW_HEIGHT: f32 = 28.;
/// 终端行下面多一行最后一行字时的高度
const TALL_ROW: f32 = 44.;
/// 工作区分节标题的高度（下面 26 是能点的那一块）
const GROUP_HEAD: f32 = 28.;
/// 两个工作区之间空着的高度
const GROUP_GAP: f32 = 8.;
/// 固定的终端不在列表里了，等这么久还不在才取消固定：重启时列表里会空一下
const UNPIN_AFTER: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    /// 待查看、离开期间有更新的
    Waiting,
    Running,
}

/// 要离开会话页去的地方。
pub enum Go {
    /// 打开这个仓库的标签页，显示 Git 页
    Repo(RepoEntry),
    /// 打开这个仓库的标签页，在文件页打开里面的文件、目录（终端里按住 Ctrl 单击的，见
    /// [`TerminalEvent::OpenFile`]）
    File {
        entry: RepoEntry,
        path: String,
        at: Option<(u32, u32)>,
        dir: bool,
    },
}

/// 右边的画面。
struct Peek {
    slot: TermSlot,
    /// 接着的是 kuaid 里的哪个终端；自己开的、还没出现在列表里时不知道
    id: Option<TermId>,
    view: Entity<TerminalView>,
    /// 这一页自己开的：正经接着，大小跟着这里的画面。不是就是只看
    owned: bool,
    /// 终端里按住 Ctrl 单击了仓库里的文件
    _events: Subscription,
}

/// 正在改名的终端：侧栏那一行（标签）的名字换成输入框，Enter、点别处确认，Esc 取消，留空回到默认的。
struct Renaming {
    slot: TermSlot,
    id: TermId,
    input: Entity<InputState>,
    _events: Subscription,
}

/// 并排查看的一格。
struct Cell {
    slot: TermSlot,
    peek: Option<Peek>,
}

/// 并排查看（[`crate::term_split`]）：哪几个、输入进哪一格、放大着哪一格。
struct Split {
    cells: Vec<Cell>,
    focus: usize,
    zoom: Option<usize>,
}

/// 侧栏里能选中的一行。
#[derive(Clone, PartialEq)]
enum Pick {
    /// 「已固定」里的终端
    Pinned(TermSlot),
    Repo(RepoEntry),
    /// 仓库下面的终端
    Term(TermSlot),
}

/// 侧栏里上一次选的是哪种行。固定的终端在侧栏里有两行（「已固定」里一行、仓库下面一行），仓库
/// 选中时右边显示的也是它的某个终端：只看右边显示着谁，分不出该高亮哪一行。
#[derive(Clone, Copy, PartialEq)]
enum Cursor {
    Pinned,
    Repo,
    Term,
}

type Repo = (RepoEntry, Vec<Row>);

/// 侧栏里的一个仓库。
struct SideRepo {
    entry: RepoEntry,
    /// 它的全部终端，固定的在前（和标签栏一个顺序）
    all: Vec<Row>,
    /// 下面列出来的那些（筛过了）
    listed: Vec<Row>,
    collapsed: bool,
    /// 它所在的工作区是第几种颜色（见 [`AppState::workspace_hues`]）；不在工作区里的是 None
    hue: Option<usize>,
}

/// 侧栏里的一节：一个工作区，或者一台主机上未加入工作区的仓库。
struct SideGroup {
    key: GroupKey,
    collapsed: bool,
    repos: Vec<SideRepo>,
    hue: Option<usize>,
}

/// 侧栏里要画的东西（筛过了）。
struct Side {
    pinned_terms: Vec<Row>,
    pinned_repos: Vec<SideRepo>,
    groups: Vec<SideGroup>,
    /// 各个仓库所在工作区的颜色（一个仓库在几个工作区里时按先列出的那个）
    hues: HashMap<RepoEntry, usize>,
    /// 只列哪台电脑上的工作区（没有是全部）
    scope: Option<String>,
    /// 选的那台服务器这次还没连上
    offline: bool,
    /// 没筛「待查看」「运行中」之前，侧栏里看得到的（选的那台电脑上的，加上固定的）：一共几个
    /// 终端，几个待查看（含离开期间有更新的），几个运行中
    total: usize,
    waiting: usize,
    running: usize,
}

impl Side {
    /// 从上到下的仓库，带着它在侧栏里是不是看得见（所在的工作区没收起）。
    fn repos(&self) -> impl Iterator<Item = (bool, &SideRepo)> {
        let pinned = self.pinned_repos.iter().map(|repo| (true, repo));
        let listed = self.groups.iter().flat_map(|group| {
            let seen = !group.collapsed;
            group.repos.iter().map(move |repo| (seen, repo))
        });
        pinned.chain(listed)
    }
}

pub struct TermOverview {
    app: Entity<AppState>,
    board: Entity<TermBoard>,
    /// 侧栏里选中的仓库：标签栏后半截是它的终端，新终端开在它里面
    repo: Option<RepoEntry>,
    /// 右边显示的终端
    shown: Option<TermSlot>,
    /// 侧栏里上一次选的是哪种行：高亮落在哪一行靠它定
    cursor: Cursor,
    filter: Filter,
    peek: Option<Peek>,
    /// 换了终端、离开这一页时还在忙的画面（正在往里传文件、预设的命令还没输入）：先留着，忙完再放；
    /// 切回那个终端时接着用它，看得到传到哪了（见 [`Self::set_peek`]）
    parked: Vec<Peek>,
    parked_task: Option<Task<()>>,
    /// 这一页正在开（新建、重启）的终端：等它出现在列表里
    opening: Option<TermSlot>,
    /// 仓库行上「新建终端」的 ⌄ 正开着菜单：那一行的按钮一直显示（鼠标挪进菜单时离开了那一行）
    menu_open: Option<RepoEntry>,
    /// 这一页说了要结束的终端：列表里还有它的那一小会儿当它不在了
    ending: Vec<TermId>,
    /// 这一页显示着
    visible: bool,
    /// 等着确认固定的终端是不是真的没了
    unpin: Option<Task<()>>,
    /// 侧栏里那棵树的滚动：吸顶的工作区标题按它算
    rows_scroll: ScrollHandle,
    /// 上一帧树的样子（见 [`Self::layout_sig`]）
    layout_sig: u64,
    /// 正在改名的终端
    renaming: Option<Renaming>,
    /// 侧栏里勾上、要并排查看的终端（还没点「并排查看」时）
    picks: Vec<TermSlot>,
    /// 正并排查看着
    split: Option<Split>,
    focus: FocusHandle,
    _observe: Subscription,
}

impl EventEmitter<Go> for TermOverview {}

impl Focusable for TermOverview {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn rows_in(hub: &[RepoGroup]) -> impl Iterator<Item = &Row> {
    hub.iter()
        .flat_map(|group| &group.repos)
        .flat_map(|(_, rows)| rows)
}

fn repo_in<'a>(hub: &'a [RepoGroup], entry: &RepoEntry) -> Option<&'a Repo> {
    hub.iter()
        .flat_map(|group| &group.repos)
        .find(|(other, _)| other == entry)
}

fn row_in<'a>(hub: &'a [RepoGroup], slot: &TermSlot) -> Option<&'a Row> {
    let (_, rows) = repo_in(hub, &slot.entry)?;
    rows.iter().find(|row| row.number == slot.number)
}

/// 一个仓库的终端分成两段：固定的（按固定的先后）、没固定的。标签栏和侧栏都是这个顺序。
/// 仓库在不在 `scope` 那台电脑上。经别的电脑连的算那台电脑的：经 A 连的 B 上的仓库是 A 的。
fn on_machine(scope: &str, entry: &RepoEntry) -> bool {
    home_machine(entry) == scope
}

/// 仓库算哪台电脑的（只看一台时按它筛）：经 A 连的 B 上的仓库是 A 的。
fn home_machine(entry: &RepoEntry) -> &str {
    let machine = machine_of(entry);
    split_relay(machine).map_or(machine, |(via, _)| via)
}

/// 仓库在侧栏里列在哪个工作区下面（不在工作区里是 None）。
fn listed_in(hub: &[RepoGroup], entry: &RepoEntry) -> Option<WorkspaceKey> {
    hub.iter().find_map(|group| match &group.key {
        GroupKey::Workspace(key)
            if group
                .repos
                .iter()
                .any(|(listed, _)| listed.same_repo(entry)) =>
        {
            Some(key.clone())
        }
        _ => None,
    })
}

fn ordered(rows: &[Row], pins: &[TermSlot]) -> (Vec<Row>, Vec<Row>) {
    let slots: Vec<TermSlot> = rows.iter().map(Row::slot).collect();
    let at = |slot: &TermSlot| slots.iter().position(|other| other == slot);
    let pinned = pins.iter().filter_map(at).map(|at| rows[at].clone());
    let own = rows
        .iter()
        .zip(&slots)
        .filter(|(_, slot)| !pins.contains(slot));
    (pinned.collect(), own.map(|(row, _)| row.clone()).collect())
}

/// 终端的右键菜单（标签上、侧栏里终端的那一行上）。
fn term_menu(
    view: WeakEntity<TermOverview>,
    slot: TermSlot,
    pinned: bool,
    repo: SharedString,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, window, cx| {
        let at = |view: &WeakEntity<TermOverview>| (view.clone(), slot.clone());
        let (go, pin, new, attach, restart, end) = (
            at(&view),
            at(&view),
            at(&view),
            at(&view),
            at(&view),
            at(&view),
        );
        let (rename, pick) = (at(&view), at(&view));
        let picked = view
            .upgrade()
            .is_some_and(|view| view.read(cx).picked(&slot));
        let rename_label = crate::keymap::with_key(tr("重命名…（F2）"), "hub.rename", cx);
        let menu = menu
            .item(
                PopupMenuItem::new(tr("打开仓库")).on_click(move |_, _, cx| {
                    go.0.update(cx, |_, cx| cx.emit(Go::Repo(go.1.entry.clone())))
                        .ok();
                }),
            )
            .item(
                PopupMenuItem::new(rename_label).on_click(move |_, window, cx| {
                    rename
                        .0
                        .update(cx, |this, cx| this.start_rename(&rename.1, window, cx))
                        .ok();
                }),
            )
            .item(
                PopupMenuItem::new(match picked {
                    true => tr("移出并排查看"),
                    false => tr("加入并排查看"),
                })
                .on_click(move |_, window, cx| {
                    pick.0
                        .update(cx, |this, cx| this.toggle_pick(&pick.1, window, cx))
                        .ok();
                }),
            );
        workspace_items(menu, &view, &slot.entry, false, window, cx)
            .item(
                PopupMenuItem::new(if pinned {
                    tr("取消固定")
                } else {
                    tr("固定")
                })
                .on_click(move |_, _, cx| {
                    pin.0
                        .update(cx, |this, cx| this.toggle_pin_term(&pin.1, cx))
                        .ok();
                }),
            )
            .separator()
            .submenu(
                tf!("在 {repo} 中新建终端", repo = repo.clone()),
                window,
                cx,
                presets::menu_builder(repo.clone(), on_start_in(&new.0, new.1.entry.clone())),
            )
            .item(
                PopupMenuItem::new(tr("发送图片或文件…")).on_click(move |_, window, cx| {
                    attach
                        .0
                        .update(cx, |this, cx| this.attach(&attach.1, window, cx))
                        .ok();
                }),
            )
            .item(
                PopupMenuItem::new(tr("重启")).on_click(move |_, window, cx| {
                    restart
                        .0
                        .update(cx, |this, cx| this.restart(&restart.1, window, cx))
                        .ok();
                }),
            )
            .separator()
            .item(
                PopupMenuItem::new(tr("结束")).on_click(move |_, window, cx| {
                    end.0
                        .update(cx, |this, cx| this.end(&end.1, window, cx))
                        .ok();
                }),
            )
    }
}

/// 菜单里选了要运行的程序：在这个仓库里新建终端。
fn on_start_in(view: &WeakEntity<TermOverview>, entry: RepoEntry) -> presets::OnStart {
    let view = view.clone();
    Rc::new(move |start, window, cx| {
        view.update(cx, |this, cx| {
            this.new_terminal_with(entry.clone(), start, window, cx)
        })
        .ok();
    })
}

/// 仓库那一行的右键菜单。
fn repo_menu(
    view: WeakEntity<TermOverview>,
    entry: RepoEntry,
    pinned: bool,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, window, cx| {
        let at = |view: &WeakEntity<TermOverview>| (view.clone(), entry.clone());
        let (pin, new, go) = (at(&view), at(&view), at(&view));
        let place = view
            .upgrade()
            .map(|view| view.read(cx).app.read(cx).repo_name(&entry))
            .unwrap_or_default();
        let menu = menu
            .item(
                PopupMenuItem::new(if pinned {
                    tr("取消固定")
                } else {
                    tr("固定到顶部")
                })
                .on_click(move |_, _, cx| {
                    pin.0
                        .update(cx, |this, cx| this.toggle_pin_repo(&pin.1, cx))
                        .ok();
                }),
            )
            .separator()
            .submenu(
                tr("新建终端"),
                window,
                cx,
                presets::menu_builder(place, on_start_in(&new.0, new.1.clone())),
            )
            .item(
                PopupMenuItem::new(tr("打开仓库")).on_click(move |_, _, cx| {
                    go.0.update(cx, |_, cx| cx.emit(Go::Repo(go.1.clone())))
                        .ok();
                }),
            );
        workspace_items(menu, &view, &entry, true, window, cx)
    }
}

/// 一节的标题（工作区名、未加入工作区）的右键菜单：打开工作区（到工作区页选中它）、收起或展开。
fn group_menu(
    view: WeakEntity<TermOverview>,
    key: GroupKey,
    collapsed: bool,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, _, _| {
        let menu = match &key {
            GroupKey::Workspace(workspace) => {
                let workspace = workspace.clone();
                menu.item(
                    PopupMenuItem::new(tr("打开工作区")).on_click(move |_, window, cx| {
                        crate::workspace::go_to_workspace(workspace.clone(), None, window, cx)
                    }),
                )
            }
            GroupKey::Unbound(_) => menu,
        };
        let (view, key) = (view.clone(), key.clone());
        menu.item(
            PopupMenuItem::new(if collapsed {
                tr("展开")
            } else {
                tr("收起")
            })
            .on_click(move |_, _, cx| {
                view.update(cx, |this, cx| this.toggle_group(key.clone(), cx))
                    .ok();
            }),
        )
    }
}

/// 右键菜单里的「打开工作区」（在几个工作区里是子菜单）；`more` 时后面跟「添加到其他工作区 ▸」，
/// 一个都不在时是「添加到工作区 ▸」（见 `ws_links`）。放进去以后挪到那个工作区那一节、选中它。
fn workspace_items(
    menu: PopupMenu,
    view: &WeakEntity<TermOverview>,
    entry: &RepoEntry,
    more: bool,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let Some(this) = view.upgrade() else {
        return menu;
    };
    let app = this.read(cx).app.clone();
    let first = this.read(cx).listed_under(entry, cx);
    let add = add_here(view);
    crate::ws_links::context_items(menu, &app, entry, first.as_ref(), more, &add, window, cx)
}

/// 在这一页放进工作区：挪到那一节、选中它（[`TermOverview::add_to_workspace`]）。
fn add_here(view: &WeakEntity<TermOverview>) -> crate::ws_links::AddTo {
    let view = view.clone();
    Rc::new(move |key, entry, window, cx| {
        view.update(cx, |this, cx| this.add_to_workspace(key, entry, window, cx))
            .ok();
    })
}

impl TermOverview {
    pub fn new(
        app: Entity<AppState>,
        board: Entity<TermBoard>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe_in(&board, window, |this, _, window, cx| {
            this.sync(window, cx);
            cx.notify();
        });
        Self {
            app,
            board,
            repo: None,
            shown: None,
            cursor: Cursor::Term,
            filter: Filter::All,
            peek: None,
            parked: Vec::new(),
            parked_task: None,
            opening: None,
            menu_open: None,
            ending: Vec::new(),
            visible: false,
            unpin: None,
            rows_scroll: ScrollHandle::new(),
            layout_sig: 0,
            renaming: None,
            picks: Vec::new(),
            split: None,
            focus: cx.focus_handle(),
            _observe: observe,
        }
    }

    /// 全部工作区里的仓库和各自的终端，后面是各台主机上未加入工作区的仓库。这一页说了要结束的
    /// 不算。
    fn hub(&self, cx: &App) -> Vec<RepoGroup> {
        let mut groups = self.board.read(cx).repos(cx);
        if self.ending.is_empty() {
            return groups;
        }
        let app = self.app.read(cx);
        // 主机仓库列表里没有、因为开着终端才列着的（子仓库），终端都结束了就不列
        let listed = |entry: &RepoEntry| {
            app.repos_on(machine_of(entry))
                .is_some_and(|repos| repos.contains(entry))
        };
        for group in &mut groups {
            for (_, rows) in &mut group.repos {
                rows.retain(|row| !self.ending.contains(&row.id));
            }
            if let GroupKey::Unbound(_) = group.key {
                group
                    .repos
                    .retain(|(entry, rows)| !rows.is_empty() || listed(entry));
            }
        }
        groups.retain(|group| !group.repos.is_empty());
        groups
    }

    fn find(&self, slot: &TermSlot, cx: &App) -> Option<Row> {
        row_in(&self.hub(cx), slot).cloned()
    }

    /// 右边显示的是哪个终端（它算看着的）。主窗口每次画都要：只看那个仓库的终端，不把全部工作区
    /// 分一遍组。
    pub fn shown_id(&self, cx: &App) -> Option<TermId> {
        let slot = self.shown.as_ref()?;
        let rows = self.board.read(cx).terms_of(&slot.entry)?;
        let row = rows.into_iter().find(|row| row.number == slot.number)?;
        (!self.ending.contains(&row.id)).then_some(row.id)
    }

    fn matches(&self, row: &Row) -> bool {
        match self.filter {
            Filter::All => true,
            Filter::Waiting => row.wants_look(),
            Filter::Running => row.online && row.state == TermState::Running,
        }
    }

    fn pinned_term(&self, slot: &TermSlot, cx: &App) -> bool {
        let hub = &self.app.read(cx).settings.term_hub;
        hub.pinned_terms.contains(slot)
    }

    fn pinned_repo(&self, entry: &RepoEntry, cx: &App) -> bool {
        let hub = &self.app.read(cx).settings.term_hub;
        hub.pinned_repos.contains(entry)
    }

    fn collapsed_group(&self, key: &GroupKey, cx: &App) -> bool {
        let hub = &self.app.read(cx).settings.term_hub;
        match key {
            GroupKey::Workspace(key) => hub.collapsed.contains(key),
            GroupKey::Unbound(machine) => hub.collapsed_unbound.contains(machine),
        }
    }

    fn collapsed_repo(&self, entry: &RepoEntry, cx: &App) -> bool {
        let hub = &self.app.read(cx).settings.term_hub;
        hub.collapsed_repos.contains(entry)
    }

    /// 侧栏只列哪台电脑上的工作区：没有是全部，空的是本机。选的服务器已经移除了就当没选。
    fn scope(&self, cx: &App) -> Option<String> {
        let app = self.app.read(cx);
        let scope = app.settings.term_hub.scope.clone()?;
        let known = scope == LOCAL || app.settings.servers.iter().any(|server| server.id == scope);
        known.then_some(scope)
    }

    /// 只看一台电脑时树里列的：那台上的工作区（只在它们之间去重，见 [`TermBoard::repos_on`]），
    /// 加上那台上未加入工作区的仓库（经它连的主机上的也算它的）。
    fn scoped(&self, machine: &str, hub: &[RepoGroup], cx: &App) -> Vec<RepoGroup> {
        let mut groups = self.board.read(cx).repos_on(machine, cx);
        for group in &mut groups {
            for (_, rows) in &mut group.repos {
                rows.retain(|row| !self.ending.contains(&row.id));
            }
        }
        for group in hub {
            if let GroupKey::Unbound(_) = group.key {
                let repos: Vec<Repo> = group
                    .repos
                    .iter()
                    .filter(|(entry, _)| on_machine(machine, entry))
                    .cloned()
                    .collect();
                if !repos.is_empty() {
                    groups.push(RepoGroup {
                        key: group.key.clone(),
                        repos,
                    });
                }
            }
        }
        groups
    }

    /// 侧栏里看得到的终端：树里列的（选了一台电脑就是那台上的），加上固定的终端、固定的仓库里的。
    fn seen(&self, hub: &[RepoGroup], cx: &App) -> Vec<Row> {
        let Some(machine) = self.scope(cx) else {
            return rows_in(hub).cloned().collect();
        };
        let tree = self.scoped(&machine, hub, cx);
        let mut seen: Vec<Row> = rows_in(&tree).cloned().collect();
        let pins = &self.app.read(cx).settings.term_hub;
        for (entry, rows) in hub.iter().flat_map(|group| &group.repos) {
            let whole = pins.pinned_repos.contains(entry);
            for row in rows {
                let pinned = whole || pins.pinned_terms.contains(&row.slot());
                if pinned && !seen.iter().any(|other| other.id == row.id) {
                    seen.push(row.clone());
                }
            }
        }
        seen
    }

    /// 侧栏里要画的：最上面固定的终端、固定的仓库，下面是选的那台电脑上各个工作区里的仓库，仓库
    /// 下面是它的终端。筛「已停下」「运行中」时只留那种终端和有那种终端的仓库。
    fn side(&self, hub: &[RepoGroup], cx: &App) -> Side {
        let app = self.app.read(cx);
        let pins = &app.settings.term_hub;
        let scope = self.scope(cx);
        let here = scope.as_deref();
        let scoped = here.map(|machine| self.scoped(machine, hub, cx));
        let tree: &[RepoGroup] = scoped.as_deref().unwrap_or(hub);
        let workspace_hues = app.workspace_hues();
        let group_hue = |group: &RepoGroup| {
            let key = group.key.workspace()?;
            Some(
                workspace_hues
                    .get(key)
                    .copied()
                    .unwrap_or_else(|| ui::name_slot(&app.workspace_name(key), ui::HUES)),
            )
        };
        let mut hues: HashMap<RepoEntry, usize> = HashMap::new();
        for group in tree.iter().chain(hub) {
            let Some(hue) = group_hue(group) else {
                continue;
            };
            for (entry, _) in &group.repos {
                hues.entry(entry.clone()).or_insert(hue);
            }
        }
        let wanted =
            |rows: &[Row]| self.filter == Filter::All || rows.iter().any(|row| self.matches(row));
        let repo = |(entry, rows): &Repo| {
            let (pinned, own) = ordered(rows, &pins.pinned_terms);
            let all: Vec<Row> = pinned.into_iter().chain(own).collect();
            let listed = all.iter().filter(|row| self.matches(row));
            SideRepo {
                entry: entry.clone(),
                listed: listed.cloned().collect(),
                collapsed: pins.collapsed_repos.contains(entry),
                all,
                hue: hues.get(entry).copied(),
            }
        };
        let pinned_terms = pins
            .pinned_terms
            .iter()
            .filter_map(|slot| row_in(hub, slot))
            .filter(|row| self.matches(row))
            .cloned()
            .collect();
        let pinned_repos = pins
            .pinned_repos
            .iter()
            .filter_map(|entry| repo_in(hub, entry))
            .filter(|(_, rows)| wanted(rows))
            .map(repo)
            .collect();
        let groups = tree
            .iter()
            .filter_map(|group| {
                // 固定了的仓库已经在上面，这里不再列
                let repos: Vec<SideRepo> = group
                    .repos
                    .iter()
                    .filter(|(entry, rows)| !pins.pinned_repos.contains(entry) && wanted(rows))
                    .map(repo)
                    .collect();
                let collapsed = match &group.key {
                    GroupKey::Workspace(key) => pins.collapsed.contains(key),
                    GroupKey::Unbound(machine) => pins.collapsed_unbound.contains(machine),
                };
                (!repos.is_empty()).then(|| SideGroup {
                    key: group.key.clone(),
                    collapsed,
                    repos,
                    hue: group_hue(group),
                })
            })
            .collect();
        let seen = self.seen(hub, cx);
        let waiting = seen.iter().filter(|row| row.wants_look()).count();
        let running = seen
            .iter()
            .filter(|row| row.online && row.state == TermState::Running)
            .count();
        let offline = here.is_some_and(|id| {
            let board = self.board.read(cx);
            id != LOCAL && !board.watching(id) && !board.is_offline(id)
        });
        Side {
            pinned_terms,
            pinned_repos,
            groups,
            hues,
            offline,
            total: seen.len(),
            waiting,
            running,
            scope,
        }
    }

    /// 侧栏里从上到下能选的行（收起的工作区、收起的仓库里的不算）。
    fn picks(side: &Side) -> Vec<Pick> {
        let pinned = side.pinned_terms.iter();
        let mut picks: Vec<Pick> = pinned.map(|row| Pick::Pinned(row.slot())).collect();
        for (_, repo) in side.repos().filter(|(seen, _)| *seen) {
            picks.push(Pick::Repo(repo.entry.clone()));
            if !repo.collapsed {
                picks.extend(repo.listed.iter().map(|row| Pick::Term(row.slot())));
            }
        }
        picks
    }

    /// 侧栏里高亮哪一行：上一次选的是终端，就是显示着的那个终端的那一行（「已固定」里的，或者
    /// 仓库下面的）；那一行没列着（仓库收起了、筛掉了、取消固定了），或者选的是仓库，就是选中的仓库。
    fn highlight(&self, side: &Side) -> Option<Pick> {
        let picks = Self::picks(side);
        let listed = |pick: Pick| picks.contains(&pick).then_some(pick);
        if let Some(split) = &self.split {
            let slot = split.cells.get(split.focus).map(|cell| cell.slot.clone())?;
            return listed(Pick::Term(slot.clone())).or(listed(Pick::Pinned(slot)));
        }
        let term = || self.shown.clone().map(Pick::Term).and_then(listed);
        let on = match self.cursor {
            Cursor::Repo => None,
            Cursor::Pinned => {
                let pinned = self.shown.clone().map(Pick::Pinned);
                pinned.and_then(listed).or_else(term)
            }
            Cursor::Term => term(),
        };
        on.or(self.repo.clone().map(Pick::Repo))
    }

    /// 标签栏：选中的仓库的终端，固定的在前，后面是没固定的。
    fn tabs(&self, hub: &[RepoGroup], cx: &App) -> (Vec<Row>, Vec<Row>) {
        let pins = &self.app.read(cx).settings.term_hub.pinned_terms;
        let repo = self.repo.as_ref().and_then(|entry| repo_in(hub, entry));
        repo.map(|(_, rows)| ordered(rows, pins))
            .unwrap_or_default()
    }

    /// 这一页显示出来了：先选中一个要看的（待查看的优先），焦点放进终端（没有终端时在侧栏上）。
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = true;
        let hub = self.hub(cx);
        let still = self
            .shown
            .as_ref()
            .is_some_and(|slot| row_in(&hub, slot).is_some());
        if !still && self.opening.is_none() {
            // 侧栏里看得到的优先（选了只看一台服务器时，别的服务器上的不在侧栏里）
            let seen = self.seen(&hub, cx);
            let stopped = |row: &&Row| row.wants_look();
            let first = seen.iter().find(stopped).or(seen.first());
            let first = first.or(rows_in(&hub).next());
            self.shown = first.map(Row::slot);
            if let Some(row) = first {
                self.repo = Some(row.entry.clone());
                self.cursor = Cursor::Term;
            }
        }
        // 选中的仓库不在了（从工作区里移走了，或者那台服务器断开了）：换成第一个
        let listed = |entry: &RepoEntry| repo_in(&hub, entry).is_some();
        if !self.repo.as_ref().is_some_and(listed) {
            // 侧栏的树里列着的优先
            let scope = self.scope(cx);
            let tree = scope.map(|machine| self.scoped(&machine, &hub, cx));
            let first_in = |groups: &[RepoGroup]| {
                let first = groups.iter().flat_map(|group| &group.repos).next();
                first.map(|(entry, _)| entry.clone())
            };
            self.repo = tree.as_deref().and_then(first_in).or(first_in(&hub));
        }
        self.sync(window, cx);
        self.focus.focus(window, cx);
        self.type_in(window, cx);
        cx.notify();
    }

    /// 这一页不显示了：不用再接着那个终端。刚开的、预设的命令还没输入的先留着，输入了再放：
    /// 放掉的话 kuaid 里已经开好的 shell 等不到这条命令（等提示符的计时在画面上）。
    pub fn hidden(&mut self, cx: &mut Context<Self>) {
        self.visible = false;
        self.cancel_rename(cx);
        // 并排着的几格：不用再接着，回来时重新接（正在往里传文件的停着，传完再放）
        let cells: Vec<Peek> = self
            .split
            .as_mut()
            .map(|split| {
                split
                    .cells
                    .iter_mut()
                    .filter_map(|cell| cell.peek.take())
                    .collect()
            })
            .unwrap_or_default();
        for peek in cells {
            if peek.view.read(cx).busy() {
                self.parked.push(peek);
                self.watch_parked(cx);
            }
        }
        let starting = self
            .peek
            .as_ref()
            .is_some_and(|peek| peek.owned && peek.view.read(cx).run_pending());
        // 正在往里传文件的（不是自己开的也算）：停到 `parked` 里，传完再放
        if !starting
            && self
                .peek
                .as_ref()
                .is_some_and(|peek| peek.view.read(cx).busy())
        {
            self.set_peek(None, cx);
            return;
        }
        if starting {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(200))
                        .await;
                    let done = this.update(cx, |this, cx| {
                        // 又回到这一页了：接着用它
                        if this.visible {
                            return true;
                        }
                        let pending = this
                            .peek
                            .as_ref()
                            .is_some_and(|peek| peek.view.read(cx).run_pending());
                        if !pending {
                            this.peek = None;
                            cx.notify();
                        }
                        !pending
                    });
                    if done.unwrap_or(true) {
                        break;
                    }
                }
            })
            .detach();
        } else {
            self.peek = None;
        }
        cx.notify();
    }

    /// 焦点在右边的终端里：敲的字都归它。
    pub fn typing(&self, window: &Window, cx: &App) -> bool {
        let focused = |peek: &Peek| peek.view.focus_handle(cx).is_focused(window);
        match &self.split {
            Some(split) => split
                .cells
                .iter()
                .any(|cell| cell.peek.as_ref().is_some_and(focused)),
            None => self.peek.as_ref().is_some_and(focused),
        }
    }

    /// 告诉 [`TermBoard`] 正显示着哪个（几个）终端：并排时是那几格，不然是选中的（正往里输入的另算）。
    pub fn shown_for_board(&self, window: &Window, cx: &App) -> Option<Shown> {
        if let Some(split) = &self.split {
            let board = self.board.read(cx);
            let ids: Vec<TermId> = split
                .cells
                .iter()
                .filter_map(|cell| {
                    let rows = board.terms_of(&cell.slot.entry)?;
                    let row = rows
                        .into_iter()
                        .find(|row| row.number == cell.slot.number)?;
                    Some(row.id)
                })
                .collect();
            return Some(Shown::Split(ids));
        }
        let shown = match self.typing(window, cx) {
            true => Shown::Typing,
            false => Shown::Term,
        };
        self.shown_id(cx).map(shown)
    }

    /// 让右边的画面跟上：显示的终端换了一个、改了大小、别处把它重启了就重新接；它没了就换成这个
    /// 仓库里别的。终端列表有变化、选了别的、开了新的以后都来一遍。
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 说了要结束的，列表里没有了就不用再记
        if !self.ending.is_empty() {
            let listed: Vec<TermId> = {
                let groups = self.board.read(cx).repos(cx);
                rows_in(&groups).map(|row| row.id.clone()).collect()
            };
            self.ending.retain(|id| listed.contains(id));
        }
        self.check_pins(cx);
        if !self.visible {
            return;
        }
        if self.split.is_some() {
            self.sync_split(window, cx);
            return;
        }
        let typing = self.typing(window, cx);
        let hub = self.hub(cx);
        // 正在开的出现在列表里了
        if let Some(slot) = &self.opening
            && let Some(row) = row_in(&hub, slot)
        {
            if let Some(peek) = &mut self.peek
                && peek.owned
                && peek.slot == *slot
            {
                peek.id = Some(row.id.clone());
            }
            self.opening = None;
        }
        // 显示着的终端没了（在别处结束了）：换成这个仓库里别的
        let gone = |slot: &TermSlot| row_in(&hub, slot).is_none();
        if self.shown.as_ref().is_some_and(gone) && self.shown != self.opening {
            self.shown = self.first_in_repo(&hub, cx);
        }
        let Some(slot) = self.shown.clone() else {
            self.peek = None;
            // 正往里输入的那个终端没了：焦点回到侧栏上，不然键盘没人接
            if typing {
                self.focus.focus(window, cx);
            }
            return;
        };
        let Some(row) = row_in(&hub, &slot) else {
            // 正在开、列表里还没有：自己开的那个画面留着（没有就建），它负责去开
            let mine = |peek: &Peek| peek.owned && peek.slot == slot;
            if !self.peek.as_ref().is_some_and(mine) {
                self.open_view(slot, window, cx);
            }
            return;
        };
        let Some(conn) = self.board.read(cx).conn(&row.id.machine) else {
            self.peek = None;
            if typing {
                self.focus.focus(window, cx);
            }
            return;
        };
        let size = Some((row.cols.max(2), row.rows.max(1)));
        let same = self.peek.as_ref().is_some_and(|peek| peek.slot == slot);
        let current = self.peek.as_ref().is_some_and(|peek| {
            let view = peek.view.read(cx);
            peek.slot == slot
                && peek.id.as_ref() == Some(&row.id)
                // 自己开的管着它的大小，不用跟
                && (peek.owned || view.size() == size)
                // 那台电脑换了连接（改了地址）：两边终端编号、大小一样也得重新接，不然输入还发给旧的
                // 那台。自己开的还没接上时没有连接，不算
                && view.conn().is_none_or(|old| std::sync::Arc::ptr_eq(&old, &conn))
        });
        if current {
            return;
        }
        // 刚才切走时还在传文件、停着的就是这个终端：接着用它（传输、进度都在它身上）
        if let Some(at) = self
            .parked
            .iter()
            .position(|peek| peek.slot == slot && peek.id.as_ref() == Some(&row.id))
        {
            let peek = self.parked.remove(at);
            self.set_peek(Some(peek), cx);
            return;
        }
        let view =
            cx.new(|cx| TerminalView::watch(&conn, &row.id.term, row.cols, row.rows, window, cx));
        let events = self.link_view(&view, &row.entry, cx);
        // 正往里输入时它改了大小、被别处重启了：换上新建的那个，焦点跟过去。大小是别处改的，跟着它，
        // 这边打字、点画面时再要回来（不然两台电脑焦点都停在这个终端里时一直来回改，见
        // `TerminalView::wants_size`）
        if typing {
            match same {
                true => {
                    view.update(cx, |view, _| view.follow_size());
                    view.focus_handle(cx).focus(window, cx);
                }
                false => self.focus.focus(window, cx),
            }
        }
        let peek = Peek {
            slot,
            id: Some(row.id.clone()),
            view,
            owned: false,
            _events: events,
        };
        self.set_peek(Some(peek), cx);
    }

    /// 换上显示的画面。换下来的还在忙（正在往里传文件、预设的命令还没输入）就先停着，忙完再放：
    /// 原来一换终端、离开这一页，传了一半的文件就停了。
    fn set_peek(&mut self, peek: Option<Peek>, cx: &mut Context<Self>) {
        if let Some(old) = self.peek.take() {
            let reused = peek
                .as_ref()
                .is_some_and(|new| new.view.entity_id() == old.view.entity_id());
            if !reused && old.view.read(cx).busy() {
                self.parked.push(old);
                self.watch_parked(cx);
            }
        }
        self.peek = peek;
    }

    /// 隔一会儿看一眼停着的画面，忙完的放掉；都放完了就不看了。
    fn watch_parked(&mut self, cx: &mut Context<Self>) {
        if self.parked_task.is_some() {
            return;
        }
        self.parked_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(300))
                    .await;
                let left = this.update(cx, |this, cx| {
                    this.parked.retain(|peek| peek.view.read(cx).busy());
                    if this.parked.is_empty() {
                        this.parked_task = None;
                    }
                    this.parked.len()
                });
                if left.unwrap_or(0) == 0 {
                    break;
                }
            }
        }));
    }

    /// 告诉画面它开在哪个仓库（认终端里的路径用），按住 Ctrl 单击了仓库里的文件时离开这一页去
    /// 打开它（见 [`Go::File`]）。
    fn link_view(
        &self,
        view: &Entity<TerminalView>,
        entry: &RepoEntry,
        cx: &mut Context<Self>,
    ) -> Subscription {
        let local = entry.server.is_none();
        let (board, machine) = (self.board.downgrade(), machine_of(entry).to_string());
        let base = LinkBase {
            root: entry.path.clone(),
            sub: String::new(),
            local,
            server: entry.server.clone(),
            // 本机的直接看；服务器上的问那台的 kuaid（每次现拿连接：会断、会换）
            channel: Rc::new(move |cx: &App| match local {
                true => Some(Channel::Local),
                false => board
                    .upgrade()?
                    .read(cx)
                    .conn(&machine)
                    .map(Channel::Remote),
            }),
        };
        view.update(cx, |view, _| view.set_link_base(base));
        let entry = entry.clone();
        cx.subscribe(view, move |_, _, event: &TerminalEvent, cx| match event {
            TerminalEvent::OpenFile { path, at, dir } => cx.emit(Go::File {
                entry: entry.clone(),
                path: path.clone(),
                at: *at,
                dir: *dir,
            }),
        })
    }

    /// 选中的仓库里先显示哪个终端：标签栏里的第一个。
    fn first_in_repo(&self, hub: &[RepoGroup], cx: &App) -> Option<TermSlot> {
        let (pinned, own) = self.tabs(hub, cx);
        pinned.first().or(own.first()).map(Row::slot)
    }

    /// 在侧栏里选中一个仓库：右边换成它的终端。
    fn select_repo(&mut self, entry: RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        let hub = self.hub(cx);
        // 已经显示着这个仓库里的终端：不动
        let keep = self
            .shown
            .as_ref()
            .is_some_and(|slot| slot.entry == entry && row_in(&hub, slot).is_some());
        self.repo = Some(entry);
        self.cursor = Cursor::Repo;
        if !keep {
            self.opening = None;
            self.shown = self.first_in_repo(&hub, cx);
        }
        self.sync(window, cx);
        cx.notify();
    }

    /// 别处要看一个仓库的终端（工作区页上点了终端个数）：侧栏选中它，右边显示它的终端。
    pub fn reveal_repo(&mut self, entry: &RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_split(cx);
        self.select_repo(entry.clone(), window, cx);
        self.focus.focus(window, cx);
        self.type_in(window, cx);
    }

    /// 别处要在这一页看一个终端（提醒里点「前往」、点了系统通知）：右边显示它，焦点进去（并排着的、
    /// 它在并排里就换到那一格）。
    pub fn reveal_term(&mut self, slot: &TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        let in_split = self
            .split
            .as_ref()
            .is_some_and(|split| split.cells.iter().any(|cell| cell.slot == *slot));
        if !in_split {
            self.leave_split(cx);
        }
        self.select_term(slot.clone(), window, cx);
        self.focus.focus(window, cx);
        self.type_in(window, cx);
    }

    /// 只看「待查看」的（标题栏的提醒、离开期间有更新的那条提醒里点「查看」）。
    pub fn show_waiting(&mut self, cx: &mut Context<Self>) {
        self.filter = Filter::Waiting;
        cx.notify();
    }

    /// 显示一个终端（点标签、点侧栏里仓库下面它那一行）。并排着时：它在并排里就把焦点换过去，不在就
    /// 换掉焦点格里的那个。
    fn select_term(&mut self, slot: TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(split) = &mut self.split {
            match split.cells.iter().position(|cell| cell.slot == slot) {
                Some(at) => split.focus = at,
                None => {
                    let at = split.focus.min(split.cells.len().saturating_sub(1));
                    if let Some(cell) = split.cells.get_mut(at) {
                        cell.slot = slot.clone();
                        cell.peek = None;
                    }
                }
            }
            self.repo = Some(slot.entry.clone());
            self.sync(window, cx);
            cx.notify();
            return;
        }
        if self.opening.as_ref() != Some(&slot) {
            self.opening = None;
        }
        self.repo = Some(slot.entry.clone());
        self.cursor = Cursor::Term;
        self.shown = Some(slot);
        self.sync(window, cx);
        cx.notify();
    }

    /// 显示一个固定的终端（点「已固定」里它那一行）：高亮留在那一行上。
    fn select_pinned(&mut self, slot: TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        self.select_term(slot, window, cx);
        self.cursor = Cursor::Pinned;
    }

    fn pick(&mut self, pick: Pick, window: &mut Window, cx: &mut Context<Self>) {
        match pick {
            Pick::Pinned(slot) => self.select_pinned(slot, window, cx),
            Pick::Term(slot) => self.select_term(slot, window, cx),
            Pick::Repo(entry) => self.select_repo(entry, window, cx),
        }
    }

    /// 这个仓库在侧栏里列在哪个工作区下面（「打开工作区」在几个工作区里时它排第一）；在「未加入
    /// 工作区」里、不在侧栏里是 None。
    pub fn listed_under(&self, entry: &RepoEntry, cx: &App) -> Option<WorkspaceKey> {
        listed_in(&self.hub(cx), entry)
    }

    /// 焦点放到右边的终端里，开始输入（后退回到这一页时，离开前就在输入的也放回去）。
    pub fn type_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let peek = match &self.split {
            Some(split) => split
                .cells
                .get(split.focus)
                .and_then(|cell| cell.peek.as_ref()),
            None => self.peek.as_ref(),
        };
        if let Some(peek) = peek {
            peek.view.focus_handle(cx).focus(window, cx);
            cx.notify();
        }
    }

    /// 打开显示着的终端所在的仓库；没有显示着的就打开选中的仓库（工具栏的「打开仓库」、Enter）。
    /// 1.17 是把终端开成标签页，2026-11-20 去掉了终端标签页。
    fn go(&mut self, cx: &mut Context<Self>) {
        let entry = match (&self.shown, &self.repo) {
            (Some(slot), _) => slot.entry.clone(),
            (None, Some(entry)) => entry.clone(),
            (None, None) => return,
        };
        cx.emit(Go::Repo(entry));
    }

    fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.filter = filter;
        cx.notify();
    }

    /// 换一台服务器看（没有是全部）。右边显示着的终端不动。
    /// 只看一台电脑时，要显示的终端在侧栏里列不出来（Ctrl+Shift+N 选了别台主机的仓库）：换成看
    /// 列着它的那台，哪台都不单独列它（比如它在本机的工作区里）就看全部。固定了的在上面列着，不用换。
    fn reveal(&mut self, slot: &TermSlot, hub: &[RepoGroup], cx: &mut Context<Self>) {
        let Some(scope) = self.scope(cx) else {
            return;
        };
        if self.pinned_term(slot, cx) || self.pinned_repo(&slot.entry, cx) {
            return;
        }
        let lists = |machine: &str, cx: &App| {
            self.scoped(machine, hub, cx)
                .iter()
                .any(|group| group.repos.iter().any(|(entry, _)| *entry == slot.entry))
        };
        if lists(&scope, cx) {
            return;
        }
        let home = home_machine(&slot.entry).to_string();
        let to = lists(&home, cx).then_some(home);
        self.set_scope(to, cx);
    }

    fn set_scope(&mut self, scope: Option<String>, cx: &mut Context<Self>) {
        self.app
            .update(cx, |app, cx| app.edit_term_hub(cx, |hub| hub.scope = scope));
        cx.notify();
    }

    fn toggle_group(&mut self, key: GroupKey, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            app.edit_term_hub(cx, |hub| match key {
                GroupKey::Workspace(key) => match hub.collapsed.contains(&key) {
                    true => hub.collapsed.retain(|other| *other != key),
                    false => hub.collapsed.push(key),
                },
                GroupKey::Unbound(machine) => match hub.collapsed_unbound.contains(&machine) {
                    true => hub.collapsed_unbound.retain(|other| *other != machine),
                    false => hub.collapsed_unbound.push(machine),
                },
            })
        });
        cx.notify();
    }

    /// 把仓库放进工作区（右键菜单的「添加到工作区」）：它挪到那个工作区那一节里，那一节收着就
    /// 展开，选中它（右边换成它的终端；原来只改了选中的，右边还显示着别的仓库的终端）。
    fn add_to_workspace(
        &mut self,
        key: WorkspaceKey,
        entry: RepoEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let added = self.app.update(cx, |app, cx| {
            let added = app.add_to_workspace(&key, &entry, cx);
            if added {
                app.edit_term_hub(cx, |hub| hub.collapsed.retain(|other| *other != key));
            }
            added
        });
        if added {
            self.select_repo(entry, window, cx);
        }
        cx.notify();
    }

    /// 收起、展开一个仓库下面的终端。
    fn toggle_repo(&mut self, entry: &RepoEntry, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            app.edit_term_hub(cx, |hub| match hub.collapsed_repos.contains(entry) {
                true => hub.collapsed_repos.retain(|other| other != entry),
                false => hub.collapsed_repos.push(entry.clone()),
            })
        });
        cx.notify();
    }

    fn toggle_pin_term(&mut self, slot: &TermSlot, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            app.edit_term_hub(cx, |hub| match hub.pinned_terms.contains(slot) {
                true => hub.pinned_terms.retain(|other| other != slot),
                false => hub.pinned_terms.push(slot.clone()),
            })
        });
        cx.notify();
    }

    fn toggle_pin_repo(&mut self, entry: &RepoEntry, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            app.edit_term_hub(cx, |hub| match hub.pinned_repos.contains(entry) {
                true => hub.pinned_repos.retain(|other| other != entry),
                false => hub.pinned_repos.push(entry.clone()),
            })
        });
        cx.notify();
    }

    /// 固定的终端里，哪些已经不在了：那台电脑的终端列表读到了，里面却没有它。没连着的电脑上的
    /// 不算（连上以后回来）。
    fn missing_pins(&self, cx: &App) -> Vec<TermSlot> {
        let board = self.board.read(cx);
        let pins = &self.app.read(cx).settings.term_hub.pinned_terms;
        pins.iter()
            .filter(|slot| self.opening.as_ref() != Some(*slot))
            .filter(|slot| {
                board
                    .terms_of(&slot.entry)
                    .is_some_and(|rows| rows.iter().all(|row| row.number != slot.number))
            })
            .cloned()
            .collect()
    }

    /// 固定的终端结束了就不再固定。重启时列表里会空一下，所以等一会儿再看一遍。
    fn check_pins(&mut self, cx: &mut Context<Self>) {
        if self.unpin.is_some() || self.missing_pins(cx).is_empty() {
            return;
        }
        self.unpin = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(UNPIN_AFTER).await;
            this.update(cx, |this, cx| {
                this.unpin = None;
                let gone = this.missing_pins(cx);
                if gone.is_empty() {
                    return;
                }
                this.app.update(cx, |app, cx| {
                    app.edit_term_hub(cx, |hub| {
                        hub.pinned_terms.retain(|slot| !gone.contains(slot))
                    })
                });
                cx.notify();
            })
            .ok();
        }));
    }

    /// 在一个仓库里新开一个终端，运行「默认运行」的那个预设。
    pub fn new_terminal(&mut self, entry: RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        let start = presets::default_start(cx);
        self.new_terminal_with(entry, start, window, cx);
    }

    /// 在一个仓库里新开一个终端，显示它，焦点进去；新开的 shell 里输入 `start`（None 是只开 shell）。
    /// 序号用这个仓库里最小的空号：先问一遍那台电脑的 kuaid 现在有哪几个（它可能刚被拉起来，列表
    /// 还没推过来）。
    pub fn new_terminal_with(
        &mut self,
        entry: RepoEntry,
        start: Option<presets::Start>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connecting = self.app.update(cx, |app, cx| match &entry.server {
            Some(server) if !app.knows_server(server) => None,
            Some(server) => Some(app.connect(server, cx)),
            None => Some(app.connect_local(cx)),
        });
        let Some(connecting) = connecting else {
            let text = tf!("终端启动失败：{err}", err = tr("未找到该主机的设置"));
            window.push_notification(Notification::error(text), cx);
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let listed = match connecting.await {
                Ok(conn) => conn.term_list().await.map_err(|err| format!("{err:#}")),
                Err(err) => Err(err.to_string()),
            };
            this.update_in(cx, |this, window, cx| match listed {
                Ok(terms) => {
                    let mut used: Vec<u32> = terms
                        .iter()
                        .filter_map(|term| terminal::session_number(&entry.path, &term.key))
                        .collect();
                    // 连着点了两次：前一个刚开、kuaid 里还没有它，问到的列表里也就没有。同名的
                    // term_open 只是接上，两次开的会是同一个终端
                    let opening = this.opening.iter().filter(|slot| slot.entry == entry);
                    used.extend(opening.map(|slot| slot.number));
                    if let Some((_, rows)) = repo_in(&this.hub(cx), &entry) {
                        used.extend(rows.iter().map(|row| row.number));
                    }
                    let slot = TermSlot {
                        entry,
                        number: free_number(&used),
                    };
                    presets::remember(slot.clone(), start, cx);
                    this.open(slot, window, cx);
                }
                Err(err) => {
                    let text = tf!("终端启动失败：{err}", err);
                    window.push_notification(Notification::error(text), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// 选一个仓库，在里面新建终端（侧栏标题行的 +、Ctrl+Shift+N）。各台主机列表里的仓库都能选，
    /// 不管在不在工作区里。
    pub fn pick_new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let picker = cx.new(|cx| QuickOpen::new(app, PickFor::Terminal, window, cx));
        cx.subscribe_in(
            &picker,
            window,
            |this, picker, event: &Picked, window, cx| {
                // 框上面那一排选的要运行的程序
                let start = picker.read(cx).chosen_start(cx);
                window.close_dialog(cx);
                this.new_terminal_with(event.0.clone(), start, window, cx);
            },
        )
        .detach();
        show_picker(picker, tr("新建终端").into(), window, cx);
    }

    /// 在选中的仓库里新开一个终端，输入 `start`（Ctrl+Shift+`、给预设设的快捷键）。
    pub fn new_terminal_here(
        &mut self,
        start: Option<presets::Start>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self.repo.clone() {
            self.new_terminal_with(entry, start, window, cx);
        }
    }

    /// 开（或者重新开）这个位置上的终端，显示它，焦点进去。
    fn open(&mut self, slot: TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_split(cx);
        // 仓库、它所在的那一节收着的话展开：新开的终端要在侧栏里看得见
        if self.collapsed_repo(&slot.entry, cx) {
            self.toggle_repo(&slot.entry, cx);
        }
        let hub = self.hub(cx);
        self.reveal(&slot, &hub, cx);
        let home = hub
            .iter()
            .find(|group| group.repos.iter().any(|(entry, _)| *entry == slot.entry));
        if let Some(group) = home
            && self.collapsed_group(&group.key, cx)
        {
            self.toggle_group(group.key.clone(), cx);
        }
        self.repo = Some(slot.entry.clone());
        self.cursor = Cursor::Term;
        self.shown = Some(slot.clone());
        self.opening = Some(slot.clone());
        if self.visible {
            self.open_view(slot, window, cx);
        }
        cx.notify();
    }

    /// 建一个正经接着的画面：kuaid 里没有这个名字的终端就开一个。
    fn open_view(&mut self, slot: TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        let (entry, app, number) = (slot.entry.clone(), self.app.clone(), slot.number);
        // 从预设开的（新建、重启）：新开出来的 shell 里替用户输入命令
        let start = presets::started(&slot, None, cx);
        let view = cx.new(|cx| {
            let mut view = TerminalView::new(
                move |cx| {
                    let mut launch = crate::shell::terminal_launch(&entry, &app, number, "", cx)?;
                    // 退回到开在 Kuai 进程里的终端跟着画面走，这一页换一个终端它就结束了：不退
                    launch.fallback = None;
                    Ok(launch)
                },
                window,
                cx,
            );
            view.run_on_start(start, Some(slot.clone()));
            view
        });
        view.focus_handle(cx).focus(window, cx);
        let events = self.link_view(&view, &slot.entry, cx);
        let peek = Peek {
            slot,
            id: None,
            view,
            owned: true,
            _events: events,
        };
        self.set_peek(Some(peek), cx);
    }

    /// 列表、标签上写的终端名字：起过的名字；没起过是程序设的标题（完整路径只留程序名）或预设的
    /// 名字，同一个仓库里重名的加序号；都没有的写「终端 N」（[`TermBoard`] 算好的，见 [`Row::label`]）。
    pub(crate) fn term_name(row: &Row, _cx: &App) -> SharedString {
        row.label.clone()
    }

    /// 结束、重启前先问一句：里面还在输出、或者除了 shell 还有程序在跑的（见 [`crate::term_end`]）。
    fn ask_first(
        &mut self,
        slot: &TermSlot,
        restart: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.find(slot, cx) else {
            return;
        };
        // 问的就是这个会话：问的这一会儿这个位置上换成了别的，不能做到别的身上
        let id = row.id.clone();
        let ending = crate::term_end::Ending {
            conn: self.board.read(cx).conn(&row.id.machine),
            repo: self.app.read(cx).repo_name(&row.entry),
            name: Self::term_name(&row, cx),
            row: Some(row),
            term: None,
            exited: false,
            restart,
        };
        let slot = slot.clone();
        crate::term_end::ask_first(
            self,
            ending,
            window,
            cx,
            move |this, window, cx| match restart {
                true => this.restart_now(&slot, &id, window, cx),
                false => this.end_now(&slot, &id, window, cx),
            },
        );
    }

    /// 结束一个终端。里面还在输出的先问。
    fn end(&mut self, slot: &TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_first(slot, false, window, cx);
    }

    /// 结束一个终端里的程序，重新开一个，并显示它。里面还在输出的先问。
    fn restart(&mut self, slot: &TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_first(slot, true, window, cx);
    }

    /// 请 kuaid 结束这个终端。列表里还有它的那一小会儿当它不在了。没连着那台电脑是 None。
    fn close(&mut self, row: &Row, cx: &mut Context<Self>) -> Option<terminal::Closing> {
        let conn = self.board.read(cx).conn(&row.id.machine)?;
        let closing = conn.term_close(&row.id.term);
        self.ending.push(row.id.clone());
        Some(closing)
    }

    /// 确认以后这个位置上已经换成了别的终端（原来那个结束了，别处又开了一个）：不做，说一声。
    /// 返回换没换。
    fn changed(row: Option<&Row>, id: &TermId, window: &mut Window, cx: &mut App) -> bool {
        let changed = row.is_some_and(|row| row.id != *id);
        if changed {
            let text = tr("终端已发生变化，操作已取消。");
            window.push_notification(Notification::warning(text), cx);
        }
        changed
    }

    /// 结束、重启失败了：说一声。
    fn failed(restart: bool, err: &str, window: &mut Window, cx: &mut App) {
        let text = match restart {
            true => tf!("终端重启失败：{err}", err),
            false => tf!("结束终端失败：{err}", err),
        };
        window.push_notification(Notification::error(text), cx);
    }

    fn end_now(
        &mut self,
        slot: &TermSlot,
        id: &TermId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hub = self.hub(cx);
        // 问的这一会儿它自己没了：不用再结束
        let Some(row) = row_in(&hub, slot).cloned() else {
            return;
        };
        if Self::changed(Some(&row), id, window, cx) {
            return;
        }
        // 显示的是它：换到同一个仓库里旁边那个（按标签栏的顺序）
        let next = (self.shown.as_ref() == Some(slot)).then(|| {
            let (_, rows) = repo_in(&hub, &slot.entry)?;
            let pins = &self.app.read(cx).settings.term_hub.pinned_terms;
            let (pinned, own) = ordered(rows, pins);
            let rows: Vec<Row> = pinned.into_iter().chain(own).collect();
            let at = rows.iter().position(|other| other.number == slot.number)?;
            let beside = rows
                .get(at + 1)
                .or(at.checked_sub(1).and_then(|at| rows.get(at)));
            beside.map(Row::slot)
        });
        let Some(closing) = self.close(&row, cx) else {
            return Self::failed(false, tr("未连接"), window, cx);
        };
        presets::ended(slot, &row.id.term, cx);
        if let Some(next) = next {
            self.shown = next;
        }
        if self.opening.as_ref() == Some(slot) {
            self.opening = None;
        }
        if self.peek.as_ref().is_some_and(|peek| peek.slot == *slot) {
            self.peek = None;
            self.focus.focus(window, cx);
        }
        // 固定着的：不用等列表推过来再取消
        let pinned = self.pinned_term(slot, cx);
        if pinned {
            self.toggle_pin_term(slot, cx);
        }
        // 没结束掉（连接断了）：不再藏着它，固定也还回去，说一声。原来只记日志，终端在主机上接着跑，
        // 这一页却一直不列它
        let (id, slot) = (row.id, slot.clone());
        cx.spawn_in(window, async move |this, cx| {
            let Err(err) = closing.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.ending.retain(|other| *other != id);
                if pinned && !this.pinned_term(&slot, cx) {
                    this.toggle_pin_term(&slot, cx);
                }
                Self::failed(false, &format!("{err:#}"), window, cx);
                this.sync(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        self.sync(window, cx);
        cx.notify();
    }

    fn restart_now(
        &mut self,
        slot: &TermSlot,
        id: &TermId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row = self.find(slot, cx);
        if Self::changed(row.as_ref(), id, window, cx) {
            return;
        }
        // 从预设开的：重启以后新开的再输入一遍（记着的是别的会话的就不输入）；起的名字跟到新开的上
        if let Some(row) = &row {
            presets::restarting(slot, &row.id.term, cx);
            self.board
                .update(cx, |board, cx| board.restarting(&row.id, cx));
        }
        // 问的这一会儿它自己没了（结束以后被清掉了）：不用关，直接在这个位置上开一个新的
        let closing = match &row {
            Some(row) => match self.close(row, cx) {
                Some(closing) => Some(closing),
                None => return Self::failed(true, tr("未连接"), window, cx),
            },
            None => None,
        };
        // 先不显示画面：原来那个正在结束，等它结束了再开，不然接上的还是它
        self.repo = Some(slot.entry.clone());
        self.shown = Some(slot.clone());
        self.opening = Some(slot.clone());
        self.peek = None;
        self.focus.focus(window, cx);
        let slot = slot.clone();
        let Some(closing) = closing else {
            self.open(slot, window, cx);
            return;
        };
        let id = id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let closed = closing.await;
            this.update_in(cx, |this, window, cx| {
                // 等的这一会儿没换到别的终端上去
                let waiting = this.opening.as_ref() == Some(&slot);
                match closed {
                    Ok(()) if waiting => this.open(slot, window, cx),
                    Ok(()) => {}
                    // 没结束掉：它还在，不开新的（按名字开接上的还是它），说一声
                    Err(err) => {
                        this.ending.retain(|other| *other != id);
                        if waiting {
                            this.opening = None;
                        }
                        Self::failed(true, &format!("{err:#}"), window, cx);
                        this.sync(window, cx);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// 给一个终端选图片、文件发进去，并显示它。
    fn attach(&mut self, slot: &TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        self.select_term(slot.clone(), window, cx);
        if let Some(peek) = &self.peek {
            peek.view.update(cx, |view, cx| view.pick_files(cx));
        }
    }

    /// 正往终端里输入时，抢在终端前面看一眼按键：Ctrl+Shift+↑、↓ 沿着侧栏换到上一个、下一个
    /// 终端，接着输入，能跨仓库。收起的工作区、收起的仓库里的跳过（正显示着的那个仓库除外）。
    /// 并排时 Ctrl+Alt+方向键（输入时、在侧栏上都行）、Ctrl+Shift+↑↓（输入时）在几格之间换。
    fn on_key_first(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let mods = &event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        if let Some(split) = &self.split {
            let cells = mods.control && mods.alt && !mods.shift && !mods.platform;
            let hop = mods.control && mods.shift && !mods.alt && !mods.platform;
            let typing = self.typing(window, cx);
            if cells || (hop && typing && matches!(key, "up" | "down")) {
                let key = match key {
                    "down" => "right",
                    "up" => "left",
                    other => other,
                };
                if let Some(next) = term_split::step(key, split.focus, split.cells.len()) {
                    cx.stop_propagation();
                    match typing {
                        true => self.focus_cell(next, window, cx),
                        false => self.set_cell(next, cx),
                    }
                }
            }
            return;
        }
        let hop = mods.control && mods.shift && !mods.alt && !mods.platform;
        let down = match key {
            "down" => true,
            "up" => false,
            _ => return,
        };
        if !hop || !self.typing(window, cx) {
            return;
        }
        cx.stop_propagation();
        let side = self.side(&self.hub(cx), cx);
        let here = self.shown.as_ref().map(|slot| &slot.entry);
        let order: Vec<TermSlot> = side
            .repos()
            .filter(|(seen, repo)| (*seen && !repo.collapsed) || Some(&repo.entry) == here)
            .flat_map(|(_, repo)| &repo.listed)
            .map(Row::slot)
            .collect();
        let at = self
            .shown
            .as_ref()
            .and_then(|slot| order.iter().position(|other| other == slot));
        let next = match (at, down) {
            (Some(at), true) => at + 1,
            (Some(at), false) => match at.checked_sub(1) {
                Some(next) => next,
                None => return,
            },
            (None, _) => 0,
        };
        if let Some(slot) = order.get(next) {
            self.select_term(slot.clone(), window, cx);
            self.type_in(window, cx);
        }
    }

    /// 侧栏上的键：↑↓ 选一行（并排时在几格之间换），Enter、→ 进到终端里输入，Ctrl+Enter 打开仓库
    /// （改了「打开所在仓库」的键以后也认），空格勾上、去掉并排查看。
    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // 焦点在右边的终端里时，它没要的键也不拿来换行、前往
        if !self.focus.is_focused(window) {
            return;
        }
        let mods = &event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        if let Some(split) = &self.split {
            match key {
                "down" | "up" => {
                    let step = if key == "down" { "right" } else { "left" };
                    if let Some(next) = term_split::step(step, split.focus, split.cells.len()) {
                        self.set_cell(next, cx);
                    }
                }
                "right" | "enter" if !mods.control => self.type_in(window, cx),
                "enter" => self.go(cx),
                "space" => {
                    let at = split.focus;
                    self.remove_cell(at, window, cx);
                }
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        let side = self.side(&self.hub(cx), cx);
        let picks = Self::picks(&side);
        let on = self.highlight(&side);
        let at = on
            .as_ref()
            .and_then(|on| picks.iter().position(|pick| pick == on));
        let step = |next: usize| picks.get(next).cloned();
        match key {
            "down" => {
                let next = at.map_or(0, |at| (at + 1).min(picks.len().saturating_sub(1)));
                if let Some(pick) = step(next) {
                    self.pick(pick, window, cx);
                }
            }
            "up" => {
                if let Some(pick) = step(at.map_or(0, |at| at.saturating_sub(1))) {
                    self.pick(pick, window, cx);
                }
            }
            "right" => self.type_in(window, cx),
            "enter" if mods.control => self.go(cx),
            "enter" => self.type_in(window, cx),
            "space" => match on {
                Some(Pick::Term(slot) | Pick::Pinned(slot)) => self.toggle_pick(&slot, window, cx),
                _ => return,
            },
            _ => return,
        }
        cx.stop_propagation();
    }

    /// F6、Shift+F6：焦点在侧栏和终端（并排时是焦点格）之间换。
    fn switch_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.typing(window, cx) {
            self.focus.focus(window, cx);
        } else {
            self.type_in(window, cx);
        }
        cx.notify();
    }

    /// F2：给侧栏上选着的终端（并排时是焦点格里的）改名。
    fn rename_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming.is_some() {
            return;
        }
        let slot = match &self.split {
            Some(split) => split.cells.get(split.focus).map(|cell| cell.slot.clone()),
            None => self.shown.clone(),
        };
        if let Some(slot) = slot {
            self.start_rename(&slot, window, cx);
        }
    }

    // ---------- 改名 ----------

    /// 开始给一个终端改名：侧栏里它那一行的名字换成输入框（收着的仓库、工作区展开，筛掉了的换回
    /// 「全部」）。
    pub(crate) fn start_rename(
        &mut self,
        slot: &TermSlot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.find(slot, cx) else {
            return;
        };
        if !self.matches(&row) {
            self.filter = Filter::All;
        }
        if self.collapsed_repo(&slot.entry, cx) {
            self.toggle_repo(&slot.entry, cx);
        }
        let hub = self.hub(cx);
        if let Some(group) = hub
            .iter()
            .find(|group| group.repos.iter().any(|(entry, _)| *entry == slot.entry))
            && self.collapsed_group(&group.key, cx)
        {
            self.toggle_group(group.key.clone(), cx);
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr("留空则使用默认名称")));
        input.update(cx, |state, cx| {
            state.set_value(row.label.to_string(), window, cx);
            state.select_all(window, cx);
        });
        let events =
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.finish_rename(true, true, window, cx),
                    InputEvent::Blur => this.finish_rename(true, false, window, cx),
                    _ => {}
                },
            );
        input.update(cx, |state, cx| state.focus(window, cx));
        self.renaming = Some(Renaming {
            slot: slot.clone(),
            id: row.id,
            input,
            _events: events,
        });
        cx.notify();
    }

    /// 改完了：`keep` 是确认（Enter、点别处），不然是取消（Esc）；`refocus` 时焦点回到侧栏上（点别处
    /// 确认的不抢焦点）。和默认的名字一样（没改）不算起了名字：程序自己设的标题还会变。
    fn finish_rename(
        &mut self,
        keep: bool,
        refocus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(renaming) = self.renaming.take() else {
            return;
        };
        if keep {
            let name = renaming.input.read(cx).value().to_string();
            let row = self.board.read(cx).row(&renaming.id);
            let unchanged = row
                .as_ref()
                .is_some_and(|row| !row.named && row.label.as_ref() == name.trim());
            if !unchanged {
                self.board
                    .update(cx, |board, cx| board.rename(&renaming.id, &name, cx));
            }
        }
        if refocus {
            self.focus.focus(window, cx);
        }
        cx.notify();
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            cx.notify();
        }
    }

    /// 侧栏一行里的改名输入框（正在改这个终端的名字时）。
    fn rename_box(&self, slot: &TermSlot, cx: &mut Context<Self>) -> Option<AnyElement> {
        let renaming = self.renaming.as_ref().filter(|r| r.slot == *slot)?;
        Some(
            div()
                .id("hub-rename")
                .flex_1()
                .min_w_0()
                .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.finish_rename(false, true, window, cx);
                    }
                }))
                // 点输入框不当作点了这一行
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(Input::new(&renaming.input).small())
                .into_any_element(),
        )
    }

    // ---------- 并排查看 ----------

    /// 这个终端勾上了要并排查看，或者正在并排里。
    fn picked(&self, slot: &TermSlot) -> bool {
        match &self.split {
            Some(split) => split.cells.iter().any(|cell| cell.slot == *slot),
            None => self.picks.contains(slot),
        }
    }

    /// 勾上、去掉一个要并排查看的终端（侧栏行前的勾、空格、右键菜单）。并排着时是加一格、移出一格。
    pub(crate) fn toggle_pick(
        &mut self,
        slot: &TermSlot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(split) = &mut self.split {
            match split.cells.iter().position(|cell| cell.slot == *slot) {
                Some(at) => {
                    self.remove_cell(at, window, cx);
                    return;
                }
                None if split.cells.len() >= term_split::MAX_CELLS => {
                    Self::too_many(window, cx);
                    return;
                }
                None => {
                    split.cells.push(Cell {
                        slot: slot.clone(),
                        peek: None,
                    });
                    split.zoom = None;
                }
            }
            self.sync(window, cx);
            cx.notify();
            return;
        }
        match self.picks.iter().position(|other| other == slot) {
            Some(at) => {
                self.picks.remove(at);
            }
            None if self.picks.len() >= term_split::MAX_CELLS => {
                Self::too_many(window, cx);
                return;
            }
            None => self.picks.push(slot.clone()),
        }
        cx.notify();
    }

    fn too_many(window: &mut Window, cx: &mut App) {
        let text = tf!(
            "最多并排查看 {n} 个终端",
            n = term_split::MAX_CELLS.to_string()
        );
        window.push_notification(Notification::info(text), cx);
    }

    /// 并排查看勾上的那几个（2～4 个），焦点进第一格。
    fn start_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picks.len() < term_split::MIN_CELLS {
            return;
        }
        let picks = std::mem::take(&mut self.picks);
        self.cancel_rename(cx);
        // 单个看着的画面放掉（忙着的停着，见 `set_peek`）
        self.set_peek(None, cx);
        self.repo = picks.first().map(|slot| slot.entry.clone());
        self.split = Some(Split {
            cells: picks
                .into_iter()
                .map(|slot| Cell { slot, peek: None })
                .collect(),
            focus: 0,
            zoom: None,
        });
        self.sync(window, cx);
        self.type_in(window, cx);
        cx.notify();
    }

    /// 换下来的画面还在忙（正在往里传文件）就停着，忙完再放。
    fn park(&mut self, peek: Option<Peek>, cx: &mut Context<Self>) {
        if let Some(peek) = peek
            && peek.view.read(cx).busy()
        {
            self.parked.push(peek);
            self.watch_parked(cx);
        }
    }

    /// 退出并排：回到单个，显示焦点格里的那个（接着用它的画面），焦点进去。
    fn exit_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(split) = self.split.take() else {
            return;
        };
        let mut keep = None;
        let mut focused = None;
        for (at, cell) in split.cells.into_iter().enumerate() {
            if at == split.focus {
                focused = Some(cell.slot);
                keep = cell.peek;
            } else {
                self.park(cell.peek, cx);
            }
        }
        if let Some(slot) = focused {
            self.repo = Some(slot.entry.clone());
            self.cursor = Cursor::Term;
            self.shown = Some(slot);
        }
        self.peek = keep;
        self.sync(window, cx);
        self.type_in(window, cx);
        cx.notify();
    }

    /// 不再并排（新开终端、别处要看别的终端）：几格的画面放掉，勾上的也不要了。
    fn leave_split(&mut self, cx: &mut Context<Self>) {
        self.picks.clear();
        let Some(split) = self.split.take() else {
            return;
        };
        for cell in split.cells {
            self.park(cell.peek, cx);
        }
        cx.notify();
    }

    /// 把一格移出并排；只剩一格时回到单个。
    fn remove_cell(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(split) = &mut self.split else {
            return;
        };
        if at >= split.cells.len() {
            return;
        }
        let cell = split.cells.remove(at);
        split.zoom = None;
        if split.focus > at || split.focus >= split.cells.len() {
            split.focus = split.focus.saturating_sub(1);
        }
        let few = split.cells.len() < term_split::MIN_CELLS;
        self.park(cell.peek, cx);
        if few {
            self.exit_split(window, cx);
            return;
        }
        self.sync(window, cx);
        cx.notify();
    }

    /// 焦点格换成第 `at` 格（侧栏上 ↑↓），焦点不动。
    fn set_cell(&mut self, at: usize, cx: &mut Context<Self>) {
        if let Some(split) = &mut self.split
            && at < split.cells.len()
        {
            split.focus = at;
            self.repo = Some(split.cells[at].slot.entry.clone());
            cx.notify();
        }
    }

    /// 焦点格换成第 `at` 格，焦点进去（点格子、Ctrl+Alt+方向键）。
    fn focus_cell(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.set_cell(at, cx);
        self.type_in(window, cx);
    }

    /// 放大第 `at` 格；已经放大着的还原。
    fn zoom_cell(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(split) = &mut self.split {
            split.zoom = match split.zoom {
                Some(zoomed) if zoomed == at => None,
                _ => Some(at),
            };
        }
        self.focus_cell(at, window, cx);
    }

    /// 让并排的几格跟上：终端没了的移出（剩一格时回到单个），没接上的接上，换了连接、被别处重启的
    /// 重新接。和单个的一样是「只看」，往里输入时大小改成格子放得下的。
    fn sync_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let hub = self.hub(cx);
        let typing = self.typing(window, cx);
        let Some(split) = self.split.as_mut() else {
            return;
        };
        let before = split.cells.len();
        split
            .cells
            .retain(|cell| row_in(&hub, &cell.slot).is_some());
        if split.cells.len() != before {
            split.zoom = None;
            split.focus = split.focus.min(split.cells.len().saturating_sub(1));
        }
        if split.cells.len() < term_split::MIN_CELLS {
            self.exit_split(window, cx);
            return;
        }
        let mut stale: Vec<(usize, Row, std::sync::Arc<fr_channel::Connection>)> = Vec::new();
        let board = self.board.read(cx);
        for (at, cell) in split.cells.iter_mut().enumerate() {
            let Some(row) = row_in(&hub, &cell.slot) else {
                continue;
            };
            let Some(conn) = board.conn(&row.id.machine) else {
                cell.peek = None;
                continue;
            };
            let current = cell.peek.as_ref().is_some_and(|peek| {
                peek.id.as_ref() == Some(&row.id)
                    && peek
                        .view
                        .read(cx)
                        .conn()
                        .is_none_or(|old| std::sync::Arc::ptr_eq(&old, &conn))
            });
            if !current {
                stale.push((at, row.clone(), conn));
            }
        }
        for (at, row, conn) in stale {
            let view = cx
                .new(|cx| TerminalView::watch(&conn, &row.id.term, row.cols, row.rows, window, cx));
            let events = self.link_view(&view, &row.entry, cx);
            let old = self
                .split
                .as_mut()
                .and_then(|split| split.cells.get_mut(at))
                .and_then(|cell| {
                    let peek = Peek {
                        slot: cell.slot.clone(),
                        id: Some(row.id.clone()),
                        view,
                        owned: false,
                        _events: events,
                    };
                    cell.peek.replace(peek)
                });
            self.park(old, cx);
        }
        if typing {
            self.type_in(window, cx);
        }
    }

    /// 并排时的右边：上面一条（并排查看、几个、怎么输入、退出并排），中间是格子，底下一条提示。
    fn render_split(
        &self,
        hub: &[RepoGroup],
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(split) = &self.split else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let (bg, border, muted, chip_bg, primary) = (
            theme.background,
            theme.border,
            theme.muted_foreground,
            theme.secondary,
            theme.primary,
        );
        let typing = self.typing(window, cx);
        let this = cx.entity().downgrade();
        let handler =
            |run: fn(&mut TermOverview, usize, &mut Window, &mut Context<TermOverview>),
             at: usize|
             -> Handler {
                let this = this.clone();
                Rc::new(move |window, cx| {
                    this.update(cx, |view, cx| run(view, at, window, cx)).ok();
                })
            };
        let indices: Vec<usize> = match split.zoom {
            Some(at) if at < split.cells.len() => vec![at],
            _ => (0..split.cells.len()).collect(),
        };
        let app = self.app.read(cx);
        let cells: Vec<AnyElement> = indices
            .iter()
            .map(|&at| {
                let cell = &split.cells[at];
                let row = row_in(hub, &cell.slot);
                let head = match row {
                    Some(row) => term_split::Head {
                        name: Self::term_name(row, cx),
                        place: Self::cell_place(app, row),
                        state: Self::state_text(row),
                        state_color: Self::state_color(row, cx),
                        dot: Self::dot_color(row, cx),
                        focused: at == split.focus,
                        zoomed: split.zoom == Some(at),
                    },
                    None => term_split::Head {
                        name: tr("终端").into(),
                        place: SharedString::default(),
                        state: SharedString::default(),
                        state_color: muted,
                        dot: muted,
                        focused: at == split.focus,
                        zoomed: split.zoom == Some(at),
                    },
                };
                let body = match (&cell.peek, row) {
                    (Some(peek), _) => div()
                        .size_full()
                        .child(peek.view.clone())
                        .into_any_element(),
                    (None, Some(row)) if !row.online => term_split::placeholder(
                        tf!(
                            "{host} 未连接，此终端的状态未知。终端仍在主机上运行。",
                            host = app.machine_name(&row.id.machine)
                        )
                        .into(),
                        cx,
                    ),
                    _ => term_split::placeholder(tr("正在连接…").into(), cx),
                };
                term_split::cell(
                    ("hub-cell", at),
                    head,
                    body,
                    handler(Self::focus_cell, at),
                    handler(Self::zoom_cell, at),
                    handler(Self::remove_cell, at),
                    cx,
                )
                .into_any_element()
            })
            .collect();
        let target = split
            .cells
            .get(split.focus)
            .and_then(|cell| row_in(hub, &cell.slot))
            .map(|row| Self::term_name(row, cx));
        // 最小的那格大概多大：右边的宽高按格数分（放大着的是整块）
        let viewport = window.viewport_size();
        let side = crate::repo::sidebar_open(&app.settings.layout, f32::from(viewport.width));
        let (w, h) = (
            f32::from(viewport.width) - side - 12.,
            f32::from(viewport.height) - HEAD_HEIGHT - 26. - 12. - 40.,
        );
        let size = match (indices.len(), split.cells.len()) {
            (1, _) | (_, 0..=1) => (w, h),
            (_, 2) => (w / 2., h),
            _ => (w / 2., h / 2.),
        };
        let focus_key = crate::keymap::label("hub.focus", cx).unwrap_or_else(|| "F6".into());
        let hint: SharedString = match typing {
            true => tf!("输入中 · {key} 回到侧栏", key = focus_key).into(),
            false => tf!("按 {key} 或点击格子输入", key = focus_key).into(),
        };
        let bar = h_flex()
            .h(px(HEAD_HEIGHT))
            .flex_none()
            .px(px(12.))
            .gap(px(8.))
            .border_b_1()
            .border_color(border)
            .child(ui::icon(IconName::Columns2).text_color(muted))
            .child(
                div()
                    .flex_none()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("并排查看")),
            )
            .child(
                div()
                    .flex_none()
                    .h(px(18.))
                    .px(px(7.))
                    .rounded_full()
                    .bg(chip_bg)
                    .text_size(px(11.))
                    .line_height(px(18.))
                    .text_color(muted)
                    .child(tf!("{n} 个", n = split.cells.len().to_string())),
            )
            .child(div().flex_1().min_w(px(4.)))
            .child(
                ui::clipped("hub-split-hint", hint)
                    .flex_shrink(100.)
                    .mr(px(6.))
                    .text_xs()
                    .text_color(if typing { primary } else { muted }),
            )
            .child(
                Button::new("hub-split-exit")
                    .small()
                    .flex_none()
                    .icon(ui::icon(IconName::Square))
                    .label(tr("退出并排"))
                    .tooltip(tr("回到单个终端，显示焦点格中的终端"))
                    .on_click(cx.listener(|this, _, window, cx| this.exit_split(window, cx))),
            );
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(bg)
            .child(bar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p(px(6.))
                    .flex()
                    .child(term_split::grid(cells).size_full()),
            )
            .child(term_split::foot(target, Some(size), cx))
            .into_any_element()
    }

    /// 格头上写的位置：仓库，主机上的带主机名。
    fn cell_place(app: &AppState, row: &Row) -> SharedString {
        let repo = app.repo_name(&row.entry);
        match row.entry.server.is_some() {
            true => format!("{repo} · {}", Self::place(app, &row.entry)).into(),
            false => repo,
        }
    }

    /// 侧栏一行前面的勾：勾上要并排查看的（并排着时是在不在并排里）。平时鼠标移到这一行上才出现，
    /// 勾了一个以后都出现。
    fn pick_box(&self, index: usize, slot: &TermSlot, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = cx.theme();
        let (primary, muted, bg) = (theme.primary, theme.muted_foreground, theme.background);
        let checked = self.picked(slot);
        let picking = !self.picks.is_empty() || self.split.is_some();
        let slot = slot.clone();
        let tip = match checked {
            true => tr("移出并排查看"),
            false => tr("加入并排查看（勾选 2～4 个）"),
        };
        div()
            .id(("hub-pick", index))
            .flex_none()
            .size(px(14.))
            .rounded(px(4.))
            .border_1()
            .border_color(if checked { primary } else { muted })
            .bg(if checked { primary } else { bg })
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .when(checked, |el| {
                el.child(
                    Icon::new(IconName::Check)
                        .size(px(10.))
                        .text_color(gpui_kit::white()),
                )
            })
            .when(!checked && !picking, |el| {
                el.invisible().group_hover("hub-row", |s| s.visible())
            })
            .tooltip(move |window, cx| ui::tip(tip, window, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.toggle_pick(&slot, window, cx);
            }))
    }

    /// 重新连上一台主机（断开时侧栏分节头、画面上的「重新连接」）。
    fn reconnect(&mut self, machine: &str, cx: &mut Context<Self>) {
        let machine = machine.to_string();
        self.app.update(cx, |app, cx| match machine.as_str() {
            LOCAL => drop(app.connect_local(cx)),
            id => drop(app.connect(id, cx)),
        });
        cx.notify();
    }

    /// 状态前面的圆点：运行中实心主色，待查看实心警示色，静默实心灰，空闲空心，状态未知淡一档的空心。
    pub(crate) fn dot(state: TermState, size: f32, cx: &App) -> Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let dot = div().flex_none().size(px(size)).rounded_full();
        match state {
            TermState::Running => dot.bg(theme.primary),
            TermState::Waiting => dot.bg(theme.warning),
            TermState::Quiet => dot.bg(muted.opacity(0.7)),
            TermState::Idle => dot.border_1().border_color(muted),
            TermState::Unknown => dot.border_1().border_color(muted.opacity(0.5)),
        }
    }

    /// 状态的颜色（行尾的字、格头）：运行中主色，要人看一眼的警示色，别的灰。
    pub(crate) fn state_color(row: &Row, cx: &App) -> Hsla {
        let theme = cx.theme();
        if row.away && row.online {
            return theme.primary;
        }
        match (row.online && row.alive, row.state) {
            (true, TermState::Running) => theme.primary,
            (true, TermState::Waiting) => theme.warning,
            _ => theme.muted_foreground,
        }
    }

    /// 格头前面圆点的颜色。
    fn dot_color(row: &Row, cx: &App) -> Hsla {
        let theme = cx.theme();
        match (row.wants_look(), row.online && row.alive, row.state) {
            (true, _, _) => theme.warning,
            (false, true, TermState::Running) => theme.primary,
            _ => theme.muted_foreground,
        }
    }

    /// 状态写成字：运行中、静默 3 分钟、待查看 · 5 分钟、空闲、已退出、状态未知；离开期间有更新的
    /// 写这个。
    pub(crate) fn state_text(row: &Row) -> SharedString {
        if row.away && row.online {
            return tr("离开期间有更新").into();
        }
        state_words(row).into()
    }

    /// 鼠标停在一个终端上时的提示：叫什么、哪个仓库的、状态，里面除了 shell 还在跑的程序（图标只分得
    /// 出是哪一类），再说一句这个状态是怎么回事。
    fn term_tip(&self, row: &Row, cx: &App) -> String {
        let app = self.app.read(cx);
        let repo = app.repo_name(&row.entry);
        // 括号、逗号要翻译：英文界面里是半角的
        let mut lines = vec![tf!(
            "{name}（{repo}，{state}）",
            name = Self::term_name(row, cx),
            repo,
            state = Self::state_text(row)
        )];
        let programs = row
            .programs
            .as_ref()
            .filter(|names| row.alive && row.online && !names.is_empty());
        if let Some(names) = programs {
            // 不知道名字的是空串（PowerShell 自己执行的命令、看不到别的用户的进程）
            let names: Vec<&str> = names
                .iter()
                .map(|name| name.trim())
                .filter(|name| !name.is_empty())
                .collect();
            lines.push(match names.is_empty() {
                true => tr("有程序正在运行").to_string(),
                false => tf!("正在运行：{names}", names = names.join(tr("、"))),
            });
        }
        let agent = matches!(row.kind, crate::term_board::Kind::Agent(_));
        if !row.online {
            lines.push(tf!(
                "{host} 未连接，无法得知此终端的状态。终端仍在主机上运行。",
                host = app.machine_name(&row.id.machine)
            ));
        } else if row.away {
            lines.push(tr("离开期间有新输出，选中后清除标记。").to_string());
        } else if row.alive && agent && matches!(row.state, TermState::Quiet | TermState::Waiting) {
            lines.push(
                tr("没有新输出：可能在等你确认，也可能在思考或执行不输出内容的命令。").to_string(),
            );
        } else if row.alive && row.state == TermState::Quiet {
            lines
                .push(tr("普通命令可能长时间不输出（编译、下载、开发服务），不提醒。").to_string());
        }
        if row.named {
            lines.push(tr("名称是起的，右键「重命名…」留空可恢复默认名称").to_string());
        }
        lines.join("\n")
    }

    /// 它在哪台电脑上：经别的电脑连的写成「B，经由 A」。
    pub(crate) fn place(app: &AppState, entry: &RepoEntry) -> SharedString {
        let machine = machine_of(entry);
        match split_relay(machine) {
            Some((via, _)) => tf!(
                "{name}，经由 {via}",
                name = app.machine_name(machine),
                via = app.machine_name(via)
            )
            .into(),
            None => app.machine_name(machine),
        }
    }

    /// 几个待查看、几个在跑、几个别的（静默、空闲、状态未知）：仓库那一行的行尾、收起的工作区那一行上。
    fn counts(rows: &[Row], cx: &App) -> Div {
        let muted = cx.theme().muted_foreground;
        let waiting = rows.iter().filter(|row| row.wants_look()).count();
        let running = rows
            .iter()
            .filter(|row| !row.wants_look() && row.online && row.state == TermState::Running)
            .count();
        let rest = rows.len() - waiting - running;
        let counts = [
            (TermState::Waiting, waiting),
            (TermState::Running, running),
            (TermState::Idle, rest),
        ];
        h_flex()
            .flex_none()
            .gap(px(7.))
            .text_size(px(11.))
            .font_weight(FontWeight::NORMAL)
            .text_color(muted)
            .children(
                counts
                    .into_iter()
                    .filter(|(_, n)| *n > 0)
                    .map(|(state, n)| {
                        h_flex()
                            .gap(px(4.))
                            .child(Self::dot(state, 7., cx))
                            .child(n.to_string())
                    }),
            )
    }

    /// 行尾鼠标移上去才出现的那几个按钮：盖在行尾原来的字上面（那些字这时藏起来）。悬停的样式只在
    /// 画的时候起作用、不改排版，所以按钮一直占着位置，只是平时不画；行尾的字要留出它们的宽度。
    fn row_actions() -> Div {
        h_flex()
            .absolute()
            .top_0()
            .h(px(ROW_HEIGHT - 2.))
            .right(px(4.))
            .gap(px(1.))
            .invisible()
            .group_hover("hub-row", |s| s.visible())
    }

    /// 行尾鼠标移上去才出现的小按钮。
    fn row_button(id: impl Into<ElementId>, icon: IconName, cx: &App) -> Stateful<Div> {
        let theme = cx.theme();
        let (fg, muted, hover) = (
            theme.sidebar_foreground,
            theme.muted_foreground,
            theme.secondary,
        );
        div()
            .id(id)
            .flex_none()
            .size(px(22.))
            .rounded(px(5.))
            .flex()
            .items_center()
            .justify_center()
            .text_color(muted)
            .hover(|s| s.bg(hover).text_color(fg))
            .child(ui::icon_small(icon))
    }

    /// 侧栏里一行的壳：一样高、一样的圆角和选中、悬停的底色。里面的东西从左边 4 像素起，各列的位置
    /// 由各行自己用定宽的空白排（见 [`Self::space`]）。
    ///
    /// 各列在侧栏里的横坐标（方向 C，`docs/design/terminal-sidebar/`）：工作区标题的色块 13、名字 29；仓库
    /// 的箭头 10、图标 26、名字 46；终端的图标 46、名字 66，竖线在 33（仓库图标的中线）；「已固定」里
    /// 的终端和仓库的图标、名字对齐。
    fn row_shell(
        id: impl Into<ElementId>,
        on: bool,
        tall: bool,
        cx: &App,
    ) -> (Stateful<Div>, Stateful<Div>) {
        let theme = cx.theme();
        let (selected_bg, hover) = (theme.sidebar_accent, theme.list_hover);
        // 带最后一行字的终端行高一些（`TermAlerts::last_line`）
        let height = if tall { TALL_ROW } else { ROW_HEIGHT };
        let outer = h_flex()
            .id(id)
            .group("hub-row")
            .h(px(height))
            .w_full()
            .cursor_pointer();
        let inner = h_flex()
            // 要有 id：悬停样式只有带 id 的元素才会在鼠标进出时重画
            .id("inner")
            .h(px(height - 2.))
            .flex_1()
            .min_w_0()
            .relative()
            .mx(px(6.))
            .pl(px(4.))
            .pr(px(4.))
            .rounded(px(ui::RADIUS))
            .text_size(px(13.))
            .when(on, |el| el.bg(selected_bg))
            .when(!on, |el| el.hover(|s| s.bg(hover)));
        (outer, inner)
    }

    /// 定宽的一块空白：行里的各列靠它对齐。
    fn space(width: f32) -> Div {
        div().flex_none().w(px(width))
    }

    /// 工作区的颜色；不在工作区里的（「其他」）是 None，用次要文字的灰。
    fn hue_color(hue: Option<usize>, cx: &App) -> Hsla {
        match hue {
            Some(slot) => ui::hue(slot, cx),
            None => cx.theme().muted_foreground,
        }
    }

    /// 终端前面的图标（占 14 像素宽）：形状看里面跑的程序（AI 助手是星芒，别的命令是三角，只有
    /// shell 是提示符），颜色看状态（运行中主色、已停下警示色、空闲和已结束灰色）。旧版 kuaid 不报
    /// 程序，退回状态圆点。
    pub(crate) fn term_icon(row: &Row, cx: &App) -> Div {
        let theme = cx.theme();
        let color = match (row.alive && row.online, row.wants_look(), row.state) {
            (_, true, _) => theme.warning,
            (true, false, TermState::Running) => theme.primary,
            (true, ..) => theme.muted_foreground,
            // 主机没连着：淡一档
            (false, ..) => theme.muted_foreground.opacity(0.55),
        };
        let slot = h_flex().flex_none().size(px(14.)).justify_center();
        let glyph = |name: IconName, size: f32| Icon::new(name).size(px(size)).text_color(color);
        // 认得出的程序（内置预设的那几家、自己加的预设）画它的图标。进程名认不出（npm 装的程序在
        // Windows 上叫 node）、这个终端又是从预设开的，画那个预设的
        let known = match row.program() {
            Program::Agent | Program::Command => row
                .programs
                .as_deref()
                .and_then(|programs| presets::program_icon(programs, cx))
                .or_else(|| {
                    presets::started(&row.slot(), Some(&row.id.term), cx).map(|start| start.icon)
                }),
            Program::Shell | Program::Unknown => None,
        };
        if let Some(icon) = known {
            return slot.child(presets::icon(&icon).size(px(13.)).text_color(color));
        }
        match row.program() {
            Program::Unknown => slot.child(Self::dot(row.state, 8., cx)),
            Program::Agent => slot.child(glyph(IconName::Sparkles, 14.)),
            Program::Command => slot.child(glyph(IconName::Play, 11.)),
            Program::Shell => slot.child(glyph(IconName::Terminal, 14.)),
        }
    }

    /// 终端名字那一格：要人看一眼的加粗，主机没连着、程序结束了的淡一档；双击名字改名。正在改名时
    /// 换成输入框。
    fn term_label(
        &self,
        id: impl Into<ElementId>,
        row: &Row,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let slot = row.slot();
        if editable && let Some(editor) = self.rename_box(&slot, cx) {
            return editor;
        }
        let theme = cx.theme();
        let (fg, muted) = (theme.sidebar_foreground, theme.muted_foreground);
        div()
            .id(id)
            .flex_1()
            .min_w_0()
            .truncate()
            .text_color(if row.alive && row.online { fg } else { muted })
            .when(row.wants_look(), |el| el.font_weight(FontWeight::SEMIBOLD))
            .child(Self::term_name(row, cx))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() >= 2 {
                    cx.stop_propagation();
                    this.start_rename(&slot, window, cx);
                }
            }))
            .into_any_element()
    }

    /// 带最后一行字时名字下面那一行（等宽小字，左边和名字对齐）。
    fn last_line(&self, row: &Row, indent: f32, cx: &App) -> Option<Div> {
        let app = self.app.read(cx);
        if !app.settings.term_alerts.last_line {
            return None;
        }
        let text: SharedString = match (row.online, row.last.trim().is_empty()) {
            (false, _) => "—".into(),
            (true, true) => SharedString::default(),
            (true, false) => row.last.trim().to_string().into(),
        };
        Some(
            div()
                .w_full()
                .h(px(15.))
                .pl(px(indent))
                .pr(px(6.))
                .truncate()
                .font_family(app.settings.terminal.font.clone())
                .text_size(px(11.))
                .line_height(px(15.))
                .text_color(cx.theme().muted_foreground)
                .child(text),
        )
    }

    /// 「已固定」里一个终端的那一行：程序和状态的图标、名字，行尾是工作区的色块和仓库名；鼠标移上去
    /// 行尾是取消固定。图标、名字和仓库那一行的对齐。行前的勾是并排查看。
    fn render_pinned_term(
        &self,
        index: usize,
        row: &Row,
        on: bool,
        hue: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let mark = Self::hue_color(hue, cx);
        let repo = self.app.read(cx).repo_name(&row.entry);
        let slot = row.slot();
        let (for_click, for_unpin) = (slot.clone(), slot.clone());
        let tip = self.term_tip(row, cx);
        let unpin = Self::row_button(("hub-unpin", index), IconName::PinOff, cx)
            .tooltip(|window, cx| ui::tip(tr("取消固定"), window, cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_pin_term(&for_unpin, cx);
            }));
        let second = self.last_line(row, 20., cx);
        let tall = second.is_some();
        let (outer, inner) = Self::row_shell(("hub-pinned-term", index), on, tall, cx);
        let line = h_flex()
            .w_full()
            .h(px(ROW_HEIGHT - 2.))
            .child(Self::term_icon(row, cx))
            .child(Self::space(6.))
            .child(self.term_label(("hub-pinned-name", index), row, true, cx))
            .child(
                h_flex()
                    .flex_none()
                    .min_w(px(22.))
                    .max_w(px(96.))
                    .ml(px(8.))
                    .pr(px(4.))
                    .gap(px(4.))
                    .text_size(px(11.))
                    .text_color(muted)
                    .group_hover("hub-row", |s| s.invisible())
                    .child(ui::hue_mark(mark, 6.))
                    .child(div().min_w_0().truncate().child(repo)),
            );
        outer
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.focus.focus(window, cx);
                this.select_pinned(for_click.clone(), window, cx);
                this.type_in(window, cx);
            }))
            .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
            .child(
                inner
                    .pl(px(20.))
                    .child(
                        div()
                            .absolute()
                            .left(px(3.))
                            .top(px((ROW_HEIGHT - 2. - 14.) / 2.))
                            .child(self.pick_box(index, &slot, cx)),
                    )
                    .child(v_flex().flex_1().min_w_0().child(line).children(second))
                    .child(Self::row_actions().child(unpin)),
            )
            .context_menu(term_menu(
                cx.entity().downgrade(),
                slot,
                true,
                self.app.read(cx).repo_name(&row.entry),
            ))
            .into_any_element()
    }

    /// 仓库下面一个终端的那一行：程序和状态的图标、名字，行尾是状态（静默、待查看的带多久；固定着的
    /// 是图钉）；鼠标移上去行尾换成固定和结束。图标对着仓库的名字，左边一条工作区颜色的竖线从仓库
    /// 图标垂下来，选中的仓库的那几条深一些；竖线左边的空里是并排查看的勾。点一下显示它、焦点进去，
    /// 双击名字改名。
    fn render_term(
        &self,
        index: usize,
        row: &Row,
        on: bool,
        hue: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let accent = cx.theme().primary;
        let active = self.repo.as_ref() == Some(&row.entry);
        let guide = Self::hue_color(hue, cx).opacity(if active { 0.75 } else { 0.4 });
        let slot = row.slot();
        let pinned = self.pinned_term(&slot, cx);
        let (for_click, for_pin, for_end) = (slot.clone(), slot.clone(), slot.clone());
        let tip = self.term_tip(row, cx);
        let pin_tip = if pinned {
            tr("取消固定")
        } else {
            tr("固定")
        };
        let pin_icon = if pinned {
            IconName::PinOff
        } else {
            IconName::Pin
        };
        let pin = Self::row_button(("hub-pin-term", index), pin_icon, cx)
            .tooltip(move |window, cx| ui::tip(pin_tip, window, cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_pin_term(&for_pin, cx);
            }));
        let end = Self::row_button(("hub-end-term", index), IconName::X, cx)
            .tooltip(|window, cx| ui::tip(tr("结束此终端"), window, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.end(&for_end, window, cx);
            }));
        let tail = h_flex()
            .flex_none()
            .min_w(px(46.))
            .ml(px(8.))
            .justify_end()
            .pr(px(4.))
            .text_size(px(11.))
            .text_color(Self::state_color(row, cx))
            .group_hover("hub-row", |s| s.invisible());
        let tail = match pinned {
            true => tail.child(
                Icon::new(IconName::Pin)
                    .size(px(11.))
                    .text_color(accent.opacity(0.8)),
            ),
            false => tail.child(Self::state_text(row)),
        };
        let second = self.last_line(row, 20., cx);
        let tall = second.is_some();
        let (outer, inner) = Self::row_shell(("hub-term", index), on, tall, cx);
        // 固定着的终端在「已固定」那一行里改名，这里只显示
        let line = h_flex()
            .w_full()
            .h(px(ROW_HEIGHT - 2.))
            .child(Self::term_icon(row, cx))
            .child(Self::space(6.))
            .child(self.term_label(("hub-term-name", index), row, !pinned, cx))
            .child(tail);
        outer
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.focus.focus(window, cx);
                this.select_term(for_click.clone(), window, cx);
                this.type_in(window, cx);
            }))
            .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
            .child(
                inner
                    .pl(px(40.))
                    .child(
                        div()
                            .absolute()
                            .left(px(27.))
                            .top(px(-1.))
                            .bottom(px(-1.))
                            .w(px(1.))
                            .bg(guide),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(7.))
                            .top(px((ROW_HEIGHT - 2. - 14.) / 2.))
                            .child(self.pick_box(index, &slot, cx)),
                    )
                    .child(v_flex().flex_1().min_w_0().child(line).children(second))
                    .child(Self::row_actions().child(pin).child(end)),
            )
            .context_menu(term_menu(
                cx.entity().downgrade(),
                slot,
                pinned,
                self.app.read(cx).repo_name(&row.entry),
            ))
            .into_any_element()
    }

    /// 一个仓库的那一行：最前面一列是收起、展开的箭头（没有终端的空着，名字照样对齐），然后是工作区
    /// 颜色的图标（没有终端的是灰的）和名字；收着时行尾写几个停下了、在跑、空着；鼠标移上去行尾换成
    /// 固定和新建。点一下选中它，有箭头的同时收起或展开（和 Git 侧栏的文件夹一样）；双击打开仓库，
    /// 收起、展开不变（第二下把第一下的切换回去）。
    fn render_repo(
        &self,
        index: usize,
        repo: &SideRepo,
        on: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (entry, rows) = (&repo.entry, &repo.all);
        let theme = cx.theme();
        let (fg, muted, accent) = (
            theme.sidebar_foreground,
            theme.muted_foreground,
            theme.primary,
        );
        let app = self.app.read(cx);
        let name = app.repo_name(entry);
        let pinned = self.pinned_repo(entry, cx);
        let empty = rows.is_empty();
        let tip = match entry.server.is_some() {
            true => tf!(
                "{path}（{place}）",
                path = &entry.path,
                place = Self::place(app, entry)
            ),
            false => entry.path.clone(),
        };
        let (for_click, for_go, for_pin, for_new, for_fold) = (
            entry.clone(),
            entry.clone(),
            entry.clone(),
            entry.clone(),
            entry.clone(),
        );
        // 下面列着终端（有箭头）才收得起来
        let foldable = !repo.listed.is_empty();
        let twistie = h_flex()
            .id(("hub-fold", index))
            .flex_none()
            .w(px(14.))
            .h_full()
            .justify_center()
            .text_color(muted);
        let twistie = match foldable {
            false => twistie,
            true => {
                let arrow = match repo.collapsed {
                    true => IconName::ChevronRight,
                    false => IconName::ChevronDown,
                };
                twistie
                    .hover(|s| s.text_color(fg))
                    .child(Icon::new(arrow).size(px(12.)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.focus.focus(window, cx);
                        this.toggle_repo(&for_fold, cx);
                    }))
            }
        };
        // 展开着时终端就列在下面，行尾不用再写个数；位置留着，鼠标移上去按钮才不压着名字。主机断开了
        // 的写「未连接」
        let offline = self.board.read(cx).is_offline(machine_of(entry));
        let counts = match (offline, repo.collapsed) {
            (true, _) => h_flex()
                .flex_none()
                .text_size(px(11.))
                .text_color(muted)
                .child(tr("未连接")),
            (false, true) => Self::counts(rows, cx),
            (false, false) => h_flex().flex_none(),
        };
        let pin_tip = if pinned {
            tr("取消固定")
        } else {
            tr("固定到顶部")
        };
        let new_tip = tf!("在 {repo} 中新建终端", repo = name.clone());
        let pin = Self::row_button(
            ("hub-pin-repo", index),
            if pinned {
                IconName::PinOff
            } else {
                IconName::Pin
            },
            cx,
        )
        .when(pinned, |el| el.text_color(accent))
        .tooltip(move |window, cx| ui::tip(pin_tip, window, cx))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.toggle_pin_repo(&for_pin, cx);
        }));
        // +：点了弹新建终端的菜单（原来点了直接开「默认运行」的那个、旁边另有 ⌄，⌄ 和分组的折叠箭头
        // 长得一样，认不出是它的）。菜单开着时这一行的按钮一直显示
        let menu_open = self.menu_open.as_ref() == Some(&for_new);
        let view = cx.entity().downgrade();
        let new = Button::new(("hub-new", index))
            .ghost()
            .xsmall()
            .icon(ui::icon_small(IconName::Plus))
            .tooltip(new_tip)
            // 不当作点了这一行（选中它会把焦点拿走，菜单就关了）
            .on_click(|_, _, cx| cx.stop_propagation())
            .dropdown_menu(presets::menu_builder(
                name.clone(),
                on_start_in(&view, for_new.clone()),
            ))
            .on_open_change({
                let (view, entry) = (view.clone(), for_new.clone());
                move |open, _, cx| {
                    let (open, entry) = (*open, entry.clone());
                    view.update(cx, |this, cx| {
                        this.menu_open = open.then_some(entry);
                        cx.notify();
                    })
                    .ok();
                }
            });
        let icon = match entry.server.is_some() {
            true => IconName::Server,
            false => IconName::FolderGit2,
        };
        let icon_color = match empty {
            true => muted,
            false => Self::hue_color(repo.hue, cx),
        };
        let (outer, inner) = Self::row_shell(("hub-repo", index), on, false, cx);
        outer
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.focus.focus(window, cx);
                let clicks = event.click_count();
                if foldable && clicks <= 2 {
                    this.toggle_repo(&for_click, cx);
                }
                // 并排着时右边不换：点仓库只收起、展开
                match clicks >= 2 {
                    true => cx.emit(Go::Repo(for_go.clone())),
                    false if this.split.is_none() => {
                        this.select_repo(for_click.clone(), window, cx)
                    }
                    false => {}
                }
            }))
            .child(
                inner
                    .child(twistie)
                    .child(Self::space(2.))
                    .child(ui::icon(icon).flex_none().text_color(icon_color))
                    .child(Self::space(6.))
                    .child(
                        ui::clipped_with_tip(("hub-repo-name", index), name, tip)
                            .flex_1()
                            // 没有终端的仓库淡一档
                            .text_color(if empty { muted } else { fg }),
                    )
                    .child(
                        counts
                            .min_w(px(46.))
                            .ml(px(8.))
                            .justify_end()
                            .pr(px(4.))
                            .group_hover("hub-row", |s| s.invisible()),
                    )
                    .child(
                        Self::row_actions()
                            .when(menu_open, |actions| actions.visible())
                            .child(pin)
                            .child(new),
                    ),
            )
            .context_menu(repo_menu(cx.entity().downgrade(), entry.clone(), pinned))
            .into_any_element()
    }

    /// 一节的标题：色块（未加入工作区的是空心的灰框）、名字（主机上的在列着全部主机时带主机名）、
    /// 一条细线、几个终端（收着时写几个停下、在跑、空着）、箭头。点一下收起、展开。
    /// `sticky` 是吸在树顶上的那一份（滚动时盖在树上面，见 [`Self::sticky_group`]）。
    fn render_group_head(
        &self,
        index: usize,
        group: &SideGroup,
        scope: Option<&str>,
        sticky: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, border) = (
            theme.sidebar_foreground,
            theme.muted_foreground,
            theme.sidebar_border,
        );
        let (chip_bg, hover, panel) = (theme.secondary, theme.list_hover, theme.sidebar);
        let app = self.app.read(cx);
        // 未加入工作区的那一节名字淡一档，和工作区分开
        let (name, soft): (SharedString, bool) = match &group.key {
            GroupKey::Workspace(key) => (app.workspace_name(key), false),
            GroupKey::Unbound(_) => (tr("未加入工作区").into(), true),
        };
        // 主机上的标出是哪台；只看一台时，那台自己的不用标（经它连的别台照样标，不然几节都叫
        // 「未加入工作区」，分不出是哪台）。断开了的总标，写「未连接」，后面跟「重新连接」
        let at = group.key.machine().to_string();
        let offline = self.board.read(cx).is_offline(&at);
        let named = at != LOCAL && scope != Some(at.as_str());
        let machine: Option<SharedString> = match (offline, named) {
            (true, false) => Some(tr("未连接").into()),
            (_, true) => Some(app.machine_name(&at)),
            (false, false) => None,
        };
        let chip = |text: SharedString| {
            h_flex()
                .flex_shrink(1.)
                .min_w_0()
                .h(px(16.))
                .ml(px(6.))
                .px(px(6.))
                .gap(px(3.))
                .rounded_full()
                .bg(chip_bg)
                .text_size(px(11.))
                .line_height(px(16.))
                .font_weight(FontWeight::NORMAL)
                .text_color(muted)
                .when(offline, |el| {
                    el.child(Icon::new(IconName::Unplug).size(px(10.)))
                })
                .child(div().min_w_0().truncate().child(text))
        };
        let all: Vec<Row> = group
            .repos
            .iter()
            .flat_map(|repo| repo.all.iter().cloned())
            .collect();
        // 收着时分状态写（几个停下、在跑、空着）；展开着、或者一个终端都没有时写总数（没有就是 0）
        let count = match group.collapsed && !all.is_empty() {
            true => Self::counts(&all, cx),
            false => h_flex()
                .flex_none()
                .text_size(px(11.))
                .font_weight(FontWeight::NORMAL)
                .text_color(muted)
                .child(all.len().to_string()),
        };
        let arrow = match group.collapsed {
            true => IconName::ChevronRight,
            false => IconName::ChevronDown,
        };
        let (id, name_id, reconnect_id) = match sticky {
            true => (
                "hub-group-sticky",
                "hub-group-sticky-name",
                "hub-reconnect-sticky",
            ),
            false => ("hub-group", "hub-group-name", "hub-reconnect"),
        };
        let primary = cx.theme().primary;
        let reconnect = offline.then(|| {
            let machine = at.clone();
            div()
                .id((reconnect_id, index))
                .flex_none()
                .mr(px(8.))
                .text_size(px(11.))
                .font_weight(FontWeight::NORMAL)
                .text_color(primary)
                .hover(|s| s.underline())
                .child(tr("重新连接"))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.reconnect(&machine, cx);
                }))
        });
        let key = group.key.clone();
        let mark = match group.key {
            GroupKey::Workspace(_) => ui::hue_mark(Self::hue_color(group.hue, cx), 8.),
            GroupKey::Unbound(_) => div()
                .flex_none()
                .size(px(8.))
                .rounded(px(2.))
                .border(px(1.5))
                .border_color(muted),
        };
        let head = h_flex()
            .id((id, index))
            .h(px(26.))
            .flex_1()
            .min_w_0()
            .pl(px(7.))
            .pr(px(2.))
            .rounded(px(ui::RADIUS))
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(fg)
            .child(mark)
            .child(Self::space(8.))
            .child(
                ui::clipped((name_id, index), name)
                    .flex_shrink(1.)
                    .when(soft || offline, |el| el.text_color(muted)),
            )
            .children(machine.map(chip))
            .child(
                div()
                    .flex_1()
                    .min_w(px(12.))
                    .h(px(1.))
                    .mx(px(8.))
                    .bg(border),
            )
            .children(reconnect)
            .child(count)
            .cursor_pointer()
            .hover(|s| s.bg(hover))
            .child(
                h_flex()
                    .flex_none()
                    .w(px(16.))
                    .ml(px(4.))
                    .justify_center()
                    .text_color(muted)
                    .child(Icon::new(arrow).size(px(12.))),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus.focus(window, cx);
                this.toggle_group(key.clone(), cx);
            }))
            .context_menu(group_menu(
                cx.entity().downgrade(),
                group.key.clone(),
                group.collapsed,
            ));
        h_flex()
            .h(px(GROUP_HEAD))
            .px(px(6.))
            .items_end()
            .bg(panel)
            .child(head)
            .into_any_element()
    }

    /// 吸在树顶上的是哪个工作区的标题、往上推了多少（下一个工作区顶上来时跟着往上走）。按上一帧
    /// 排好的位置算：树里每个工作区是滚动区域里的一个孩子，上面空着 [`GROUP_GAP`]（第一个除外）。
    fn sticky_group(&self, side: &Side) -> Option<(usize, f32)> {
        let handle = &self.rows_scroll;
        // 树的可见部分的顶，在没滚动时的坐标里
        let top = f32::from(handle.bounds().top() - handle.offset().y);
        for at in 0..side.groups.len() {
            let bounds = handle.bounds_for_item(at)?;
            let (start, end) = (f32::from(bounds.top()), f32::from(bounds.bottom()));
            if end <= top {
                continue;
            }
            let gap = if at == 0 { 0. } else { GROUP_GAP };
            if start + gap >= top {
                return None;
            }
            return Some((at, (end - top - GROUP_HEAD).min(0.)));
        }
        None
    }

    /// 树的样子变了（工作区、仓库收起展开，多了少了几行）：滚动区域里各个孩子的位置要等这一帧排完
    /// 才知道，吸顶的标题按的是上一帧的，所以变了就多画一帧。
    fn layout_sig(side: &Side) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for group in &side.groups {
            (&group.key, group.collapsed, group.repos.len()).hash(&mut hasher);
            for repo in &group.repos {
                (repo.collapsed, repo.listed.len()).hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    /// 只看哪些：样子和仓库页侧栏顶上切页面的那一条一样，占满一行。
    /// 全部、已停下、运行中三格，各带个数。侧栏窄了先不写个数，还放不下再省略字，鼠标停上去看全名。
    fn render_filter(
        &self,
        side: &Side,
        width: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (track, fg, muted) = (theme.secondary, theme.foreground, theme.muted_foreground);
        let dark = theme.is_dark();
        // 选中的那一格：浅色主题是白底加一点影子，深色主题比底下亮一档
        let on_bg = if dark {
            track.blend(fg.opacity(0.12))
        } else {
            theme.background
        };
        let items = [
            (Filter::All, "overview-all", tr("全部"), side.total, ""),
            (
                Filter::Waiting,
                "overview-waiting",
                // 三格平分侧栏：英文用短的写法（Needs attention 放不下，被截成 Needs att…）
                fr_core::i18n::trc("筛选", "待查看"),
                side.waiting,
                tr("静默超过所设时长且尚未查看的 AI 终端，以及离开期间有新输出的"),
            ),
            (
                Filter::Running,
                "overview-running",
                tr("运行中"),
                side.running,
                tr("10 秒内有新输出"),
            ),
        ];
        // 一格多宽：侧栏减去底板两边的外边距（10）、内边距（2）和格子之间的空（2）；字离格子两边各留 4
        let room = (width - 2. * 10. - 2. * 2. - 2. * 2.) / 3. - 2. * 4.;
        // 都按选中时的粗细量：换一格选中，个数不会忽有忽无
        let label_width =
            |label: &str| ui::text_width(window, label, 12., None, FontWeight::SEMIBOLD);
        let counts = items.iter().all(|&(_, _, label, count, _)| {
            let count = ui::text_width(window, &count.to_string(), 11., None, FontWeight::NORMAL);
            label_width(label) + 5. + count <= room
        });
        h_flex()
            .flex_none()
            .mx(px(10.))
            .mt(px(8.))
            .mb(px(2.))
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(7.))
            .bg(track)
            .children(items.into_iter().map(|(filter, id, label, count, tip)| {
                let on = self.filter == filter;
                let cut = label_width(label) > room;
                h_flex()
                    .id(id)
                    .flex_1()
                    .min_w_0()
                    .h(px(26.))
                    .gap(px(5.))
                    .justify_center()
                    .rounded(px(5.))
                    .text_size(px(12.))
                    .text_color(if on { fg } else { muted })
                    .when(on, |el| {
                        el.bg(on_bg)
                            .font_weight(FontWeight::SEMIBOLD)
                            .when(!dark, |el| el.shadow_xs())
                    })
                    .when(!on, |el| el.cursor_pointer().hover(|s| s.text_color(fg)))
                    .child(div().min_w_0().truncate().child(label))
                    .when(counts, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .font_weight(FontWeight::NORMAL)
                                .text_color(muted)
                                .child(count.to_string()),
                        )
                    })
                    // 名字被省略了，提示里先写全名
                    .when(cut || !tip.is_empty(), |el| {
                        el.tooltip(move |window, cx| match (cut, tip.is_empty()) {
                            (true, true) => ui::tip(label, window, cx),
                            (true, false) => ui::two_line_tip(label.into(), tip.into(), window, cx),
                            (false, _) => ui::tip(tip, window, cx),
                        })
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.set_filter(filter, cx)))
            }))
    }

    /// 「已固定」下面的那个下拉：只列哪台服务器上的工作区。没有配置服务器时不显示。
    fn render_scope(&self, side: &Side, cx: &mut Context<Self>) -> Option<AnyElement> {
        let app = self.app.read(cx);
        if app.settings.servers.is_empty() {
            return None;
        }
        let muted = cx.theme().muted_foreground;
        // 右边那一句：连着的写有几个工作区，没连着的写状态
        let note = |machine: &str| -> SharedString {
            match app.workspaces_on(machine) {
                Some(list) => tf!("{n} 个工作区", n = list.len()).into(),
                None => match app.status(machine) {
                    crate::app_state::ConnStatus::Connecting => tr("正在连接…").into(),
                    _ => tr("未连接").into(),
                },
            }
        };
        let mut choices: Vec<(Option<String>, IconName, SharedString, SharedString)> = vec![
            (None, IconName::Layers, tr("全部").into(), "".into()),
            (
                Some(LOCAL.to_string()),
                IconName::Monitor,
                app.machine_name(LOCAL),
                note(LOCAL),
            ),
        ];
        for server in &app.settings.servers {
            choices.push((
                Some(server.id.clone()),
                IconName::Server,
                app.machine_name(&server.id),
                note(&server.id),
            ));
        }
        let now = side.scope.clone();
        let (icon, name) = choices
            .iter()
            .find(|choice| choice.0 == now)
            .map(|choice| (choice.1, choice.2.clone()))?;
        let this = cx.entity().downgrade();
        let button = Button::new("hub-scope")
            .outline()
            .small()
            .w_full()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(6.))
                    .child(ui::icon_small(icon).flex_none().text_color(muted))
                    .child(ui::clipped("hub-scope-name", name).flex_shrink(1.)),
            )
            .dropdown_caret(true)
            .tooltip(tr("只列出所选主机上的工作区；已固定的不受影响"))
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.min_w(px(232.));
                for (at, (scope, icon, name, note)) in choices.iter().enumerate() {
                    // 「全部」和各台电脑之间隔一条线
                    if at == 1 {
                        menu = menu.separator();
                    }
                    let (this, to) = (this.clone(), scope.clone());
                    let (icon, name, note) = (*icon, name.clone(), note.clone());
                    menu = menu.item(
                        PopupMenuItem::element(move |_, _| {
                            h_flex()
                                .w_full()
                                .gap(px(8.))
                                .child(ui::icon_small(icon).flex_none())
                                .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(note.clone()),
                                )
                        })
                        .checked(*scope == now)
                        .on_click(move |_, _, cx| {
                            let to = to.clone();
                            this.update(cx, |this, cx| this.set_scope(to, cx)).ok();
                        }),
                    );
                }
                menu
            });
        Some(
            div()
                .flex_none()
                .mx(px(10.))
                .mt(px(8.))
                .child(button)
                .into_any_element(),
        )
    }

    /// 选的那台服务器还没连上：说一声，给一个「连接」。
    fn render_connect(&self, side: &Side, cx: &mut Context<Self>) -> AnyElement {
        let (muted, danger) = (cx.theme().muted_foreground, cx.theme().danger);
        let app = self.app.read(cx);
        let id = side.scope.clone().unwrap_or_default();
        let status = app.status(&id);
        let connecting = matches!(status, crate::app_state::ConnStatus::Connecting);
        // 刚才连过、没连上：把原因写出来，不然点了「连接」像没反应
        let failed = match status {
            crate::app_state::ConnStatus::Failed(err) => Some(err),
            _ => None,
        };
        let label = tf!("连接 {name}", name = app.machine_name(&id));
        let app = self.app.clone();
        v_flex()
            .px(px(14.))
            .py(px(24.))
            .gap(px(12.))
            .items_center()
            .text_size(px(12.))
            .text_color(muted)
            .child(tr("尚未连接此主机，连接后显示其终端。"))
            .children(failed.map(|err| {
                v_flex()
                    .w_full()
                    .gap(px(2.))
                    .text_color(danger)
                    .child(tr("连接失败"))
                    .child(div().w_full().text_size(px(11.)).child(err))
            }))
            .child(
                Button::new("hub-connect")
                    .outline()
                    .small()
                    .icon(ui::icon(IconName::Plug))
                    .label(label)
                    .loading(connecting)
                    .disabled(connecting)
                    .on_click(move |_, _, cx| {
                        app.update(cx, |app, cx| drop(app.connect(&id, cx)));
                    }),
            )
            .into_any_element()
    }

    /// 侧栏里一个仓库的那一行，下面是它的终端（没收起的话）。
    fn push_repo(
        &self,
        repo: &SideRepo,
        on: &Option<Pick>,
        index: &mut usize,
        items: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let is_on = |pick: Pick| on.as_ref() == Some(&pick);
        let lit = is_on(Pick::Repo(repo.entry.clone()));
        items.push(self.render_repo(*index, repo, lit, cx));
        *index += 1;
        if repo.collapsed {
            return;
        }
        for row in &repo.listed {
            let lit = is_on(Pick::Term(row.slot()));
            items.push(self.render_term(*index, row, lit, repo.hue, cx));
            *index += 1;
        }
    }

    /// 左边的侧栏。和仓库页的侧栏一样宽、一样的底色和字号：右边的画面才和仓库的终端页一样大。
    fn render_side(
        &self,
        width: f32,
        side: &Side,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (panel, border, muted, accent) = (
            theme.sidebar,
            theme.sidebar_border,
            theme.muted_foreground,
            theme.primary,
        );
        let (head_fg, head_hover) = (theme.sidebar_foreground, theme.secondary);
        let summary = match side.waiting {
            0 => tf!("{n} 个终端", n = side.total.to_string()),
            _ => tf!(
                "{n} 个终端，{m} 个待查看",
                n = side.total.to_string(),
                m = side.waiting.to_string()
            ),
        };
        // 重开 Kuai、重连以后离开期间有新输出的：顶上一条，可以全部标为已看
        let away = self.board.read(cx).away_count();
        let away = (away > 0).then(|| {
            h_flex()
                .flex_none()
                .mx(px(10.))
                .mt(px(6.))
                .px(px(8.))
                .py(px(5.))
                .gap(px(6.))
                .rounded(px(ui::RADIUS))
                .bg(accent.opacity(0.12))
                .text_size(px(12.))
                .child(Icon::new(IconName::Bell).size(px(13.)).text_color(accent))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(tf!("离开期间 {n} 个终端有新输出", n = away.to_string())),
                )
                .child(
                    div()
                        .id("hub-clear-away")
                        .flex_none()
                        .text_color(accent)
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .child(tr("全部标为已看"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.board.update(cx, |board, cx| board.clear_away(cx));
                        })),
                )
        });
        // 勾了要并排查看的：底下一条「已选 n 个」「并排查看」「取消」
        let picking = (!self.picks.is_empty() && self.split.is_none()).then(|| {
            let n = self.picks.len();
            h_flex()
                .flex_none()
                .px(px(10.))
                .py(px(7.))
                .gap(px(6.))
                .border_t_1()
                .border_color(border)
                .text_size(px(12.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(tf!("已选 {n} 个", n = n.to_string())),
                )
                .child(
                    Button::new("hub-split-go")
                        .small()
                        .primary()
                        .label(tr("并排查看"))
                        .tooltip(tr("勾选 2～4 个终端并排查看"))
                        .disabled(n < term_split::MIN_CELLS)
                        .on_click(cx.listener(|this, _, window, cx| this.start_split(window, cx))),
                )
                .child(
                    Button::new("hub-split-cancel")
                        .small()
                        .label(tr("取消"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.picks.clear();
                            cx.notify();
                        })),
                )
        });
        let on = self.highlight(side);
        let is_on = |pick: Pick| on.as_ref() == Some(&pick);
        // 「已固定」单独一块，不跟着下面的树滚动，也不受换服务器影响
        let mut pinned: Vec<AnyElement> = Vec::new();
        let mut items: Vec<AnyElement> = Vec::new();
        let mut index = 0;
        let has_pinned = !side.pinned_terms.is_empty() || !side.pinned_repos.is_empty();
        if has_pinned {
            // 和工作区的分节标题一个样子：图标、字、一条细线，字和工作区的名字对齐
            pinned.push(
                h_flex()
                    .h(px(26.))
                    .mt(px(6.))
                    .mx(px(6.))
                    .pl(px(5.))
                    .pr(px(2.))
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(muted)
                    .child(ui::icon_small(IconName::Pin).flex_none())
                    .child(Self::space(6.))
                    .child(tr("已固定"))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(12.))
                            .h(px(1.))
                            .mx(px(8.))
                            .bg(border),
                    )
                    .into_any_element(),
            );
            for row in &side.pinned_terms {
                let on = is_on(Pick::Pinned(row.slot()));
                let hue = side.hues.get(&row.entry).copied();
                pinned.push(self.render_pinned_term(index, row, on, hue, cx));
                index += 1;
            }
            for repo in &side.pinned_repos {
                self.push_repo(repo, &on, &mut index, &mut pinned, cx);
            }
        }
        let scoped = side.scope.is_some();
        let scope = side.scope.as_deref();
        // 一个工作区是滚动区域里的一个孩子（标题和它下面的行），吸顶的标题按孩子的位置算
        for (at, group) in side.groups.iter().enumerate() {
            let mut rows = vec![self.render_group_head(at, group, scope, false, cx)];
            if !group.collapsed {
                for repo in &group.repos {
                    self.push_repo(repo, &on, &mut index, &mut rows, cx);
                }
            }
            items.push(
                v_flex()
                    .flex_none()
                    .when(at > 0, |el| el.pt(px(GROUP_GAP)))
                    .children(rows)
                    .into_any_element(),
            );
        }
        let sticky = self.sticky_group(side).map(|(at, push)| {
            div()
                .absolute()
                .top(px(push))
                .left_0()
                .right_0()
                .child(self.render_group_head(at, &side.groups[at], scope, true, cx))
        });
        // 树是空的：列着全部电脑、上面又有固定的时候不用说（没有的已经在上面了）
        let empty = side.groups.is_empty() && !side.offline && (scoped || !has_pinned);
        let switcher = self.render_scope(side, cx);
        let connect = side.offline.then(|| self.render_connect(side, cx));
        // 没连着的服务器上的终端不知道，也不为了看它去连：说一声。只看一台的时候不用说别的
        let offline: Vec<String> = {
            let (app, board) = (self.app.read(cx), self.board.read(cx));
            let servers = app.settings.servers.iter();
            servers
                .filter(|server| {
                    !scoped && !board.watching(&server.id) && !board.is_offline(&server.id)
                })
                .map(|server| server.name.clone())
                .collect()
        };
        let offline = (!offline.is_empty()).then(|| {
            tf!(
                "未连接的主机：{names}。连接后显示其终端。",
                names = offline.join(tr("、"))
            )
        });
        let hint = |keys: SharedString, text: &'static str| {
            h_flex()
                .flex_none()
                .gap(px(4.))
                .child(div().font_weight(FontWeight::MEDIUM).child(keys))
                .child(text)
        };
        // 底下一行按键提示：哪一条放不下就整条不显示，不截成半截。左边留 14，右边不够时可以占掉
        // 一些、至少离边 4。改过的键照改过的写
        let hints = {
            let mut room = width - 1. - 14. - 4.;
            let mut shown = Vec::new();
            let key = |id: &str, default: &str| -> SharedString {
                crate::keymap::label(id, cx)
                    .map(SharedString::from)
                    .unwrap_or_else(|| default.to_string().into())
            };
            let all = [
                (SharedString::from("↑ ↓"), tr("选择")),
                ("Enter".into(), tr("输入")),
                (key("hub.open_repo", "Ctrl+Enter"), tr("打开仓库")),
                (key("hub.focus", "F6"), tr("到终端")),
            ];
            for (keys, text) in all {
                let gap = if shown.is_empty() { 0. } else { 10. };
                let need = gap
                    + ui::text_width(window, &keys, 11., None, FontWeight::MEDIUM)
                    + 4.
                    + ui::text_width(window, text, 11., None, FontWeight::NORMAL);
                if need <= room {
                    room -= need;
                    shown.push(hint(keys, text));
                }
            }
            shown
        };
        v_flex()
            .w(px(width))
            .flex_none()
            .relative()
            .h_full()
            .bg(panel)
            .border_r_1()
            .border_color(border)
            // 顶上那一条和仓库页侧栏的一样：图标、名字、一行小字；行尾是「新建终端…」
            .child(
                h_flex()
                    .h(px(HEAD_HEIGHT))
                    .flex_none()
                    .pl(px(12.))
                    .pr(px(8.))
                    .gap(px(10.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .flex_none()
                            .size(px(28.))
                            .rounded(px(7.))
                            .bg(accent.opacity(0.14))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(ui::icon(IconName::SquareTerminal).text_color(accent)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                ui::clipped("overview-title", trc("页面", "会话"))
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                ui::clipped("overview-summary", summary)
                                    .text_size(px(11.))
                                    .text_color(muted),
                            ),
                    )
                    .child(
                        div()
                            .id("overview-new")
                            .flex_none()
                            .size(px(26.))
                            .rounded(px(6.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .text_color(muted)
                            .hover(|s| s.bg(head_hover).text_color(head_fg))
                            .child(ui::icon(IconName::Plus))
                            .tooltip(|window, cx| {
                                let tip = crate::keymap::with_key(
                                    tr("新建终端…（Ctrl+Shift+N）"),
                                    "new.pick",
                                    cx,
                                );
                                ui::tip(tip, window, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.pick_new_terminal(window, cx)
                            })),
                    ),
            )
            .child(self.render_filter(side, width, window, cx))
            .children(away)
            .when(has_pinned, |side| {
                side.child(
                    v_flex()
                        .id("overview-pinned")
                        .flex_none()
                        // 固定得再多也给下面的树留一半
                        .max_h(relative(0.45))
                        .pb(px(8.))
                        .border_b_1()
                        .border_color(border)
                        .overflow_y_scroll()
                        .children(pinned),
                )
            })
            .children(switcher)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .id("overview-rows")
                            .size_full()
                            .pb(px(10.))
                            .overflow_y_scroll()
                            .track_scroll(&self.rows_scroll)
                            .children(items)
                            .children(connect)
                            .when(empty, |list| {
                                list.child(
                                    div()
                                        .px(px(14.))
                                        .py(px(24.))
                                        .text_size(px(12.))
                                        .text_color(muted)
                                        .text_center()
                                        .child(match self.filter {
                                            Filter::All => tr("没有仓库"),
                                            Filter::Waiting => tr("没有待查看的终端"),
                                            Filter::Running => tr("没有运行中的终端"),
                                        }),
                                )
                            })
                            .children(offline.map(|text| {
                                div()
                                    .px(px(14.))
                                    .pt(px(12.))
                                    .text_size(px(11.))
                                    .text_color(muted)
                                    .child(text)
                            })),
                    )
                    .children(sticky),
            )
            .children(picking)
            .child(
                h_flex()
                    .h(px(28.))
                    .flex_none()
                    .pl(px(14.))
                    .pr(px(4.))
                    .gap(px(10.))
                    .border_t_1()
                    .border_color(border)
                    .overflow_hidden()
                    .text_size(px(11.))
                    .text_color(muted)
                    .children(hints),
            )
            .child(crate::side_split::handle("hub-side-split", &self.app))
    }

    /// 标签栏上的一个终端：固定的前面有图钉；显示着的、鼠标移上去的有结束的 ×。
    fn render_tab(
        &self,
        index: usize,
        row: &Row,
        pinned: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, on_bg, hover, close_hover, accent) = (
            theme.foreground,
            theme.muted_foreground,
            theme.secondary,
            theme.list_hover,
            theme.border,
            theme.primary,
        );
        let slot = row.slot();
        let on = self.shown.as_ref() == Some(&slot);
        let repo = self.app.read(cx).repo_name(&row.entry);
        let tip = self.term_tip(row, cx);
        let (for_click, for_end) = (slot.clone(), slot.clone());
        let close = div()
            .id(("hub-tab-end", index))
            .flex_none()
            .size(px(18.))
            .rounded(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .text_color(muted)
            .hover(|s| s.bg(close_hover).text_color(fg))
            .when(!on, |close| {
                close.invisible().group_hover("hub-tab", |s| s.visible())
            })
            .child(ui::icon_small(IconName::X))
            .tooltip(|window, cx| ui::tip(tr("结束此终端"), window, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.end(&for_end, window, cx);
            }));
        h_flex()
            .id(("hub-tab", index))
            .group("hub-tab")
            .h(px(30.))
            .min_w(px(60.))
            .max_w(px(220.))
            .flex_shrink(1.)
            .pl(px(if pinned { 8. } else { 10. }))
            .pr(px(6.))
            .gap(px(7.))
            .rounded(px(ui::RADIUS))
            .text_size(px(13.))
            .text_color(if on { fg } else { muted })
            .cursor_pointer()
            .when(on, |el| el.bg(on_bg).font_weight(FontWeight::MEDIUM))
            .when(!on, |el| el.hover(|s| s.bg(hover)))
            .when(pinned, |el| {
                el.child(
                    Icon::new(IconName::Pin)
                        .size(px(11.))
                        .flex_none()
                        .text_color(if on { accent } else { muted }),
                )
            })
            .child(Self::term_icon(row, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(Self::term_name(row, cx)),
            )
            .child(close)
            .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                match event.click_count() >= 2 {
                    true => this.start_rename(&for_click, window, cx),
                    false => {
                        this.select_term(for_click.clone(), window, cx);
                        this.type_in(window, cx);
                    }
                }
            }))
            .context_menu(term_menu(cx.entity().downgrade(), slot, pinned, repo))
            .into_any_element()
    }

    /// 右边上面那一条：标签栏、+，右边是怎么输入的提示、发送文件、重启、前往。
    fn render_toolbar(
        &self,
        hub: &[RepoGroup],
        typing: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted, hover, fg) = (
            theme.border,
            theme.muted_foreground,
            theme.list_hover,
            theme.foreground,
        );
        let (pinned, own) = self.tabs(hub, cx);
        let split = !pinned.is_empty() && !own.is_empty();
        let mut tabs: Vec<AnyElement> = Vec::new();
        for (index, row) in pinned.iter().enumerate() {
            tabs.push(self.render_tab(index, row, true, cx));
        }
        if split {
            tabs.push(
                div()
                    .flex_none()
                    .w(px(1.))
                    .h(px(18.))
                    .mx(px(4.))
                    .bg(border)
                    .into_any_element(),
            );
        }
        for (index, row) in own.iter().enumerate() {
            tabs.push(self.render_tab(pinned.len() + index, row, false, cx));
        }
        let app = self.app.read(cx);
        let repo = self.repo.clone();
        let repo_name = repo.as_ref().map(|entry| app.repo_name(entry));
        let shown = self
            .shown
            .as_ref()
            .and_then(|slot| row_in(hub, slot))
            .cloned();
        // 没有显示着的终端时，右边写选中的是哪个仓库、在哪
        let place = repo
            .as_ref()
            .filter(|_| shown.is_none())
            .map(|entry| format!("{} · {}", app.repo_name(entry), Self::place(app, entry)));
        // + 和 ⌄ 连成一组：悬停时一起有底色、中间一条分隔线，看得出 ⌄ 是 + 的。+ 开「默认运行」的
        // 那个，⌄ 是新建终端的菜单
        let program = presets::default_preset(cx).map(|preset| preset.name);
        let view = cx.entity().downgrade();
        let plus = repo.clone().zip(repo_name).map(|(entry, repo_name)| {
            let tip = match program {
                Some(program) => tf!(
                    "在 {repo} 中新建终端并运行 {name}（Ctrl+Shift+`）",
                    repo = repo_name.clone(),
                    name = program
                ),
                None => tf!(
                    "在 {repo} 中新建终端（Ctrl+Shift+`）",
                    repo = repo_name.clone()
                ),
            };
            let tip = crate::keymap::with_key(&tip, "new.default", cx);
            h_flex()
                .id("hub-new-split")
                .flex_none()
                .h(px(24.))
                .rounded(px(ui::RADIUS))
                .group("hub-new-split")
                .hover(|s| s.bg(hover.opacity(0.6)))
                .child(
                    div()
                        .id("hub-new-tab")
                        .size(px(24.))
                        .rounded_l(px(ui::RADIUS))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(muted)
                        .cursor_pointer()
                        .hover(|s| s.bg(hover).text_color(fg))
                        .child(ui::icon(IconName::Plus))
                        .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
                        .on_click(cx.listener({
                            let entry = entry.clone();
                            move |this, _, window, cx| this.new_terminal(entry.clone(), window, cx)
                        })),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(1.))
                        .h(px(14.))
                        .group_hover("hub-new-split", |s| s.bg(border)),
                )
                .child(
                    Button::new("hub-new-tab-menu")
                        .ghost()
                        .xsmall()
                        .icon(ui::icon_small(IconName::ChevronDown))
                        .tooltip(tr("选择要运行的程序"))
                        .dropdown_menu(presets::menu_builder(repo_name, on_start_in(&view, entry))),
                )
        });
        // 怎么开始往终端里输入、怎么回到侧栏（键照设置里改过的写）
        let focus_key = crate::keymap::label("hub.focus", cx).unwrap_or_else(|| "F6".into());
        let hint: Option<SharedString> =
            shown
                .as_ref()
                .filter(|row| row.alive && row.online)
                .map(|_| match typing {
                    true => tf!("输入中 · {key} 回到侧栏", key = focus_key).into(),
                    false => tf!("点击画面或按 {key} 输入", key = focus_key).into(),
                });
        let primary = cx.theme().primary;
        let slot = shown.as_ref().map(Row::slot);
        let (for_attach, for_restart) = (slot.clone(), slot);
        // 两个按钮对着哪个仓库（和 `go` 一样）、它在侧栏里列在哪个工作区下面
        let go_to = shown.as_ref().map(|row| row.entry.clone()).or(repo.clone());
        let target = go_to.clone().map(|entry| {
            let home = listed_in(hub, &entry);
            (entry, home)
        });
        h_flex()
            .h(px(HEAD_HEIGHT))
            .flex_none()
            .px(px(10.))
            .gap(px(4.))
            .border_b_1()
            .border_color(border)
            .child(
                h_flex()
                    .flex_shrink(1.)
                    .min_w_0()
                    .gap(px(2.))
                    .overflow_hidden()
                    .children(tabs),
            )
            .children(plus)
            .child(div().flex_1().min_w(px(4.)))
            .children(place.map(|place| {
                ui::clipped("hub-place", place)
                    .flex_shrink(100.)
                    .text_xs()
                    .text_color(muted)
            }))
            // 地方不够时先省略这一句，标签留着
            .children(hint.map(|text| {
                ui::clipped("hub-hint", text)
                    .flex_shrink(100.)
                    .mr(px(6.))
                    .text_xs()
                    .text_color(if typing { primary } else { muted })
            }))
            .when(shown.is_some(), |bar| {
                bar.child(
                    tool_button(
                        "hub-attach",
                        IconName::Paperclip,
                        tr("发送文件"),
                        tr("发送图片或文件（也可拖放，或按 Ctrl+V 粘贴）"),
                        false,
                        true,
                        0,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(slot) = &for_attach {
                            this.attach(slot, window, cx);
                        }
                    })),
                )
                .child(
                    tool_button(
                        "hub-restart",
                        IconName::RotateCw,
                        tr("重启"),
                        tr("结束当前程序并重新启动"),
                        false,
                        true,
                        0,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(slot) = &for_restart {
                            this.restart(slot, window, cx);
                        }
                    })),
                )
            })
            // 打开工作区、打开仓库：显示着的终端所在的仓库，没有显示着的终端时是选中的仓库
            .children(target.map(|(entry, home)| {
                let app = self.app.read(cx);
                let homes = crate::ws_links::homes(app, &entry, home.as_ref()).len();
                let tip = crate::ws_links::tip(app, &entry, home.as_ref());
                let label = match homes {
                    0 => tr("添加到工作区"),
                    _ => tr("打开工作区"),
                };
                let button = Button::new("overview-workspace")
                    .small()
                    .flex_none()
                    .ml(px(6.))
                    .icon(ui::icon(IconName::Layers))
                    .label(label)
                    .tooltip(tip);
                let add = add_here(&cx.entity().downgrade());
                crate::ws_links::button(
                    button,
                    &self.app,
                    &entry,
                    home,
                    add,
                    Anchor::TopRight,
                    true,
                    cx,
                )
            }))
            .when(go_to.is_some(), |bar| {
                bar.child(
                    Button::new("overview-go")
                        .primary()
                        .small()
                        .flex_none()
                        .ml(px(6.))
                        .label(tr("打开仓库"))
                        .tooltip(crate::keymap::with_key(
                            match shown.is_some() {
                                true => tr("打开此终端所在的仓库（Ctrl+Enter）"),
                                false => tr("打开此仓库（Ctrl+Enter）"),
                            },
                            "hub.open_repo",
                            cx,
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.go(cx))),
                )
            })
    }

    /// 右边：上面是标签栏，下面是显示着的终端的画面，占满；选中的仓库没有终端时是空的样子。
    fn render_main(
        &self,
        hub: &[RepoGroup],
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.split.is_some() {
            return self.render_split(hub, window, cx);
        }
        let theme = cx.theme();
        let (bg, muted, primary) = (theme.background, theme.muted_foreground, theme.primary);
        let typing = self.typing(window, cx);
        let centered = || {
            v_flex()
                .flex_1()
                .min_h_0()
                .items_center()
                .justify_center()
                .gap_3()
        };
        let body = match (&self.peek, &self.shown, &self.repo) {
            (Some(peek), _, _) => div()
                .relative()
                .flex_1()
                .min_h_0()
                // 点进终端里焦点就换了（终端自己接的）：这一页和主窗口跟着重画，顶上的细线、
                // 提示、「看没看过」才跟得上
                .on_any_mouse_down(cx.listener(|_, _, _, cx| cx.notify()))
                .child(peek.view.clone())
                // 正在输入：画面顶上一条主色细线，和别处表示选中的线一样 2 宽（原来是一圈边框，
                // 太显眼）。盖在终端顶上的留白里画，不占地方、不挡字
                .when(typing, |el| {
                    el.child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .right_0()
                            .h(px(2.))
                            .bg(primary),
                    )
                })
                .into_any_element(),
            // 主机断开了：终端还在那台上跑，这边接不上
            (None, Some(slot), _) if row_in(hub, slot).is_some_and(|row| !row.online) => {
                let machine = machine_of(&slot.entry).to_string();
                let host = self.app.read(cx).machine_name(&machine);
                centered()
                    .px(px(24.))
                    .text_color(muted)
                    .child(Icon::new(IconName::Unplug).size(px(28.)))
                    .child(
                        div()
                            .text_size(px(13.))
                            .child(tf!("{host} 未连接，此终端的状态未知。", host)),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .child(tr("终端仍在主机上运行，重新连接后可继续查看和输入。")),
                    )
                    .child(
                        Button::new("hub-reconnect-main")
                            .small()
                            .primary()
                            .icon(ui::icon(IconName::Plug))
                            .label(tr("重新连接"))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.reconnect(&machine, cx)),
                            ),
                    )
                    .into_any_element()
            }
            // 重启：原来那个正在结束
            (None, Some(_), _) => centered()
                .text_color(muted)
                .child(div().text_size(px(13.)).child(tr("正在启动终端…")))
                .into_any_element(),
            (None, None, Some(entry)) => {
                let entry = entry.clone();
                centered()
                    .child(
                        Icon::new(IconName::SquareTerminal)
                            .size(px(28.))
                            .text_color(muted),
                    )
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr("此仓库没有打开的终端")),
                    )
                    .child(presets::first_buttons(
                        self.app.read(cx).repo_name(&entry),
                        on_start_in(&cx.entity().downgrade(), entry),
                        cx,
                    ))
                    .into_any_element()
            }
            (None, None, None) => centered()
                .text_color(muted)
                .child(Icon::new(IconName::SquareTerminal).size(px(28.)))
                .child(div().text_size(px(13.)).child(tr("选择左侧的仓库")))
                .into_any_element(),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(bg)
            .child(self.render_toolbar(hub, typing, cx))
            .child(body)
            .into_any_element()
    }
}

impl Render for TermOverview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 侧栏和仓库页的侧栏一样宽（那边拖宽了它也跟着宽）
        let viewport = f32::from(window.viewport_size().width);
        let width = crate::repo::sidebar_open(&self.app.read(cx).settings.layout, viewport);
        let hub = self.hub(cx);
        let side = self.side(&hub, cx);
        let sig = Self::layout_sig(&side);
        if std::mem::replace(&mut self.layout_sig, sig) != sig {
            window.request_animation_frame();
        }
        h_flex()
            .id("term-overview")
            .size_full()
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::on_key_first))
            .on_key_down(cx.listener(Self::on_key))
            .on_action(
                cx.listener(|this, _: &crate::FocusSwitch, window, cx| {
                    this.switch_focus(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &crate::FocusSwitchBack, window, cx| {
                this.switch_focus(window, cx)
            }))
            .on_action(cx.listener(|this, _: &crate::RenameTerminal, window, cx| {
                this.rename_current(window, cx)
            }))
            .on_action(cx.listener(|this, _: &crate::OpenTerminalRepo, _, cx| this.go(cx)))
            // 拖侧栏的分隔条：鼠标拖到画面上面也由这里按位置算宽度
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<crate::side_split::DragSide>, window, cx| {
                    crate::side_split::drag(&this.app, event, window, cx);
                    cx.notify();
                },
            ))
            .child(self.render_side(width, &side, window, cx))
            .child(self.render_main(&hub, window, cx))
    }
}

#[cfg(test)]
mod tests {
    // 不整个引进来：界面库里也有一个叫 test 的属性宏，会顶掉标准的 #[test]
    use super::{RepoEntry, Row, TermId, TermSlot};
    use super::{on_machine, ordered};

    fn row(number: u32) -> Row {
        let id = TermId {
            machine: String::new(),
            term: number.to_string(),
        };
        let entry = RepoEntry {
            server: None,
            path: "/repo".into(),
        };
        crate::term_board::tests::plain_row(id, entry, number)
    }

    #[test]
    fn 仓库里的终端_固定的在前_别的仓库的固定不算() {
        let rows = [row(1), row(2), row(3)];
        let numbers = |rows: Vec<Row>| rows.iter().map(|row| row.number).collect::<Vec<_>>();
        let other = TermSlot {
            entry: RepoEntry {
                server: None,
                path: "/other".into(),
            },
            number: 1,
        };
        // 按固定的先后排，不是按序号；别的仓库里固定着的「终端 1」不影响这个仓库
        let pins = [rows[2].slot(), other, rows[1].slot()];
        let (pinned, own) = ordered(&rows, &pins);
        assert_eq!(numbers(pinned), [3, 2]);
        assert_eq!(numbers(own), [1]);
        // 固定的终端不在了（列表里没有它）：不出现
        let (pinned, own) = ordered(&rows[..1], &pins);
        assert!(pinned.is_empty());
        assert_eq!(numbers(own), [1]);
    }
    #[test]
    fn 不在工作区里的仓库_按它自己在哪台电脑算() {
        let entry = |server: Option<&str>| RepoEntry {
            server: server.map(str::to_string),
            path: "/repo".into(),
        };
        assert!(on_machine("", &entry(None)));
        assert!(!on_machine("", &entry(Some("s1"))));
        assert!(on_machine("s1", &entry(Some("s1"))));
        // 经 s1 连的 s2 上的仓库：算 s1 的，不算本机配置的 s2 的
        assert!(on_machine("s1", &entry(Some("s1>s2"))));
        assert!(!on_machine("s2", &entry(Some("s1>s2"))));
    }
}
