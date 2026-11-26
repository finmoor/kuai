//! 仓库的终端页：一个仓库可以开几个终端。
//!
//! 每个仓库分 Git、终端两页（侧栏里仓库名下面的切换，或者 Ctrl+`），各记各的。终端页的侧栏里
//! 一个终端一行：新开、切换、结束都在这里，右键还有发文件、重开。右边上面一条工具栏，写着选中的
//! 终端叫什么、开在哪，右边是发文件、重开；下面是选中的终端，占满。只画选中的那个，没选中的
//! 照样收输出，只是不画、不跟着改大小。
//!
//! 终端不跟着标签页结束：
//!
//! - 终端开在 kuaid 里，各有各的会话（名字见 [`crate::terminal::session_key`]）：服务器上的仓库
//!   是服务器的 kuaid，本机仓库是这台电脑上的 kuaid。关标签页、退出 Kuai 以后接着跑；再打开这个
//!   仓库时问一遍 kuaid，原来留着几个就接回几个。
//! - 开着的时候跟着主窗口的那份实时终端列表走（[`TermBoard`]，1.16 起）：会话页（标题栏上的
//!   「会话」）、别的电脑上的 Kuai 在这个仓库里开了、结束了、重启了终端，这里马上多一行、少一行、
//!   接上新的，见 [`RepoView::board_changed`]。
//! - 这台电脑上的 kuaid 用不了时，本机仓库的终端退回到开在 Kuai 进程里。关标签页时，程序还在
//!   运行的交给 [`Parked`] 保管，再打开这个仓库、到终端页时接回来；退出 Kuai（包括重启以完成
//!   更新）时才结束。
//!
//! 和会话页用同一套：名字、状态从实时列表来（起过的名字、同名加序号）；双击
//! 名字、右键「重命名…」、F2 改名；行前的勾（鼠标移上去时盖在序号上）勾 2～4 个并排查看
//! （[`crate::term_split`]）；F6 在侧栏和终端之间换，侧栏上 ↑↓ 换终端、Enter 进去输入。

use std::rc::Rc;
use std::sync::Arc;

use fr_channel::{Channel, Connection};
use fr_core::i18n::tr;
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

use super::{Page, RepoView, TOOLBAR_HEIGHT, tool_button, tool_menu_button};
use crate::presets::{self, Start};
use crate::settings::{RepoEntry, TermSlot};
use crate::term_board::{BoardHandle, Row as TermRow, TermBoard, free_number};
use crate::term_overview::TermOverview;
use crate::term_split::{self, Handler};
use crate::terminal::{self, Closing, TerminalEvent, TerminalView};
use crate::ui;

/// 侧栏里一个终端一行的高度。
const ROW_HEIGHT: f32 = 28.;
/// 带最后一行字时一行的高度（`TermAlerts::last_line`）
const TALL_ROW: f32 = 44.;

/// 一个终端。
struct TermTab {
    /// 侧栏那一行前面的数字：这个仓库的第几个终端。关掉的不补位，新开的用最小的空号
    number: u32,
    view: Entity<TerminalView>,
    /// 终端列表里按这个编号列出过它（和它接着的那个编号一样时才记）。之后列表里没有这个位置了，
    /// 就是在别处结束了；位置上换了别的编号，就是在别处重启了
    listed_as: Option<String>,
    /// kuaid 里留着、接回来的：还没接上（选中才接）时不知道编号，列表里没有它了也算结束了
    adopted: bool,
    /// 里面的程序结束、断开时，切换上的数字要跟着变
    _observe: Subscription,
    /// 按住 Ctrl 单击了终端里仓库的文件：到文件页打开
    _events: Subscription,
}

/// kuaid 里留着、这次还没接上的一个终端。
#[derive(Clone, PartialEq)]
struct Waiting {
    /// 这个仓库的第几个终端
    number: u32,
    /// 里面的程序还在跑
    alive: bool,
    /// kuaid 给它的编号：接上时按编号接（见 [`TerminalView::attach_to`]）
    term: String,
    /// 开了多久（秒，问到的那一刻）：认上次存下的从预设开的终端用（见 `presets::confirm`）
    age: u64,
}

/// 一个仓库的终端们。
#[derive(Default)]
pub(super) struct Terminals {
    /// 点「新终端」「打开终端」才建，到终端页不自己开；切回 Git 页也留着（里面的程序接着跑）
    tabs: Vec<TermTab>,
    /// 终端页上显示的是哪一个
    active: usize,
    /// kuaid 里留着、这次还没接上的终端
    waiting: Vec<Waiting>,
    /// 这次连上以后问到过 kuaid 里留着哪几个（`waiting` 是问到的）；连上新的连接时重新问
    listed: bool,
    /// 主窗口的那份实时终端列表（没有主窗口的地方是 None，只在打开时问一次 kuaid）
    board: Option<Entity<TermBoard>>,
    /// 这边说了要结束的终端的序号：列表里还有它的那一小会儿不当它是留着的
    ending: Vec<u32>,
    /// 显示着的终端在别处结束了，焦点可能跟着没了：下次画之前放到现在显示的那个上
    refocus: bool,
    /// 点了「新终端」、正在问 kuaid 留着哪几个的次数（见 [`RepoView::new_terminal_in`]）：
    /// 一个终端都没有时终端页写「正在连接…」
    asking: usize,
    /// 侧栏终端列表的焦点（F6 从终端回到这里）；用到时才建
    side_focus: Option<FocusHandle>,
    /// 正在改名的终端（序号）和输入框
    renaming: Option<(u32, Entity<InputState>, Subscription)>,
    /// 勾上要并排查看的终端（序号），还没点「并排查看」时
    picks: Vec<u32>,
    /// 并排着的终端（序号，2～4 个）；焦点格是 `active` 那个
    split: Vec<u32>,
    /// 放大着的那一格（序号）
    zoom: Option<u32>,
}

impl Terminals {
    /// 仓库所在的电脑（重新）连上了：断开时停下、启动时连不上的终端接上（[`TerminalView::host_back`]）。
    pub(super) fn host_back(&self, cx: &mut App) {
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| view.host_back(cx));
        }
    }
}

/// 关掉的本机仓库标签页留下的、开在 Kuai 进程里的终端，里面的程序接着运行。再打开这个仓库、到终端页时接回去
/// （[`RepoView::adopt_parked`]）；同一个仓库在别的工作区里也开着的话，先到终端页的那个接走。
#[derive(Default)]
struct Parked(Vec<ParkedTerminal>);

impl Global for Parked {}

struct ParkedTerminal {
    entry: RepoEntry,
    number: u32,
    view: Entity<TerminalView>,
}

/// 关掉的标签页留下的本机终端中，程序还在运行的有几个（退出 Kuai、重启以完成更新前要问）。
pub fn parked_terminals_running(cx: &App) -> usize {
    parked_running(None, cx)
}

/// 留下的本机终端中，程序还在运行的有几个；给了 `entry` 就只数这个仓库的。
fn parked_running(entry: Option<&RepoEntry>, cx: &App) -> usize {
    let Some(parked) = cx.try_global::<Parked>() else {
        return 0;
    };
    parked
        .0
        .iter()
        .filter(|t| entry.is_none_or(|entry| t.entry == *entry))
        .filter(|t| t.view.read(cx).is_running())
        .count()
}

/// 这个仓库留下了几个本机终端（程序还在运行的）。
pub fn parked_terminals_of(entry: &RepoEntry, cx: &App) -> usize {
    parked_running(Some(entry), cx)
}

/// 仓库从列表中移除了：它留下的本机终端一起结束，不然再也没有地方能看到它们。
pub fn end_parked_terminals(entry: &RepoEntry, cx: &mut App) {
    let mine = |t: &ParkedTerminal| t.entry == *entry;
    // 先只读地看一眼：改动会通知所有盯着的仓库页重画
    if !cx
        .try_global::<Parked>()
        .is_some_and(|parked| parked.0.iter().any(mine))
    {
        return;
    }
    let ended: Vec<ParkedTerminal> = {
        let parked = &mut cx.global_mut::<Parked>().0;
        let (ended, rest) = std::mem::take(parked).into_iter().partition(mine);
        *parked = rest;
        ended
    };
    // 都是开在 Kuai 进程里的：丢掉会话就结束了，没有要等的请求
    for terminal in ended {
        terminal.view.update(cx, |view, _| drop(view.kill()));
    }
}

/// 一个终端的右键菜单：发送文件、重启、结束。侧栏里那一行、图标栏上的序号都用它。
/// 在 `view` 这个仓库里的 `dir` 目录（相对仓库根，空的是仓库根）新建终端的菜单（见 [`presets::menu`]），
/// 选了就在那里开；`place` 写在「运行命令」对话框的标题里（见 [`RepoView::launch_place`]）。
pub(super) fn launch_menu(
    view: WeakEntity<RepoView>,
    place: SharedString,
    dir: String,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    let on_start: presets::OnStart = Rc::new(move |start, window, cx| {
        view.update(cx, |this, cx| {
            this.new_terminal_with(dir.clone(), start, window, cx)
        })
        .ok();
    });
    presets::menu_builder(place, on_start)
}

fn terminal_menu(
    view: WeakEntity<RepoView>,
    index: usize,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, window, cx| {
        let (attach, restart, end) = (view.clone(), view.clone(), view.clone());
        let rename = view.clone();
        // 和会话页终端行的右键一样，先是「新建终端 ▸」（在这个仓库里）
        let place = view
            .upgrade()
            .map(|this| this.read(cx).launch_place("", cx))
            .unwrap_or_default();
        let (split, nameable) = view
            .upgrade()
            .map(|this| {
                let this = this.read(cx);
                let number = this.terminals.tabs.get(index).map(|tab| tab.number);
                let split = number.map(|number| (number, this.terminal_split_entry(number, cx)));
                (split, this.board_row(index, cx).is_some())
            })
            .unwrap_or((None, false));
        let rename_label = crate::keymap::with_key(tr("重命名…（F2）"), "hub.rename", cx);
        let menu = menu
            .submenu(
                tr("新建终端"),
                window,
                cx,
                launch_menu(view.clone(), place, String::new()),
            )
            .item(
                PopupMenuItem::new(rename_label)
                    .disabled(!nameable)
                    .on_click(move |_, window, cx| {
                        rename
                            .update(cx, |this, cx| this.start_terminal_rename(index, window, cx))
                            .ok();
                    }),
            );
        let menu = match split {
            Some((number, entry)) => terminal_split_item(menu, entry, &view, number, window, cx),
            None => menu,
        };
        menu.item(
            PopupMenuItem::new(tr("发送图片或文件…")).on_click(move |_, window, cx| {
                attach
                    .update(cx, |this, cx| this.attach_to_terminal(index, window, cx))
                    .ok();
            }),
        )
        .item(
            PopupMenuItem::new(tr("重启")).on_click(move |_, window, cx| {
                restart
                    .update(cx, |this, cx| this.restart_terminal(index, window, cx))
                    .ok();
            }),
        )
        .separator()
        .item(
            PopupMenuItem::new(tr("结束")).on_click(move |_, window, cx| {
                end.update(cx, |this, cx| this.end_terminal(index, window, cx))
                    .ok();
            }),
        )
    }
}

/// 终端的右键菜单里并排查看的那一项（和会话页的一样）。
enum SplitEntry {
    /// 并排着，它在里面：「移出并排」
    Remove,
    /// 并排着，它不在里面：「加入并排」
    Add,
    /// 没并排，右键的不是选中的那个：「与「选中的」并排」
    With { base: u32, name: SharedString },
    /// 没并排，右键的就是选中的那个：「并排查看 ▸」里挑一个
    Choose,
    /// 没有别的终端
    Nothing,
}

/// 右键菜单里并排查看的那一项（见 [`SplitEntry`]）。
fn terminal_split_item(
    menu: PopupMenu,
    entry: SplitEntry,
    view: &WeakEntity<RepoView>,
    number: u32,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let view = view.clone();
    match entry {
        SplitEntry::Remove => menu.item(PopupMenuItem::new(tr("移出并排")).on_click(
            move |_, window, cx| {
                view.update(cx, |this, cx| this.toggle_terminal_pick(number, window, cx))
                    .ok();
            },
        )),
        SplitEntry::Add => menu.item(PopupMenuItem::new(tr("加入并排")).on_click(
            move |_, window, cx| {
                view.update(cx, |this, cx| {
                    this.split_terminal_pair(None, number, window, cx)
                })
                .ok();
            },
        )),
        SplitEntry::With { base, name } => menu.item(
            PopupMenuItem::new(tf!("与「{name}」并排", name = name)).on_click(
                move |_, window, cx| {
                    view.update(cx, |this, cx| {
                        this.split_terminal_pair(Some(base), number, window, cx)
                    })
                    .ok();
                },
            ),
        ),
        SplitEntry::Choose => menu.submenu(
            tr("并排查看"),
            window,
            cx,
            terminal_split_menu(view, Some(number)),
        ),
        SplitEntry::Nothing => menu,
    }
}

/// 挑一个终端来并排的菜单（工具栏的「并排」、并排时的「添加」、右键的「并排查看 ▸」），行尾写状态。
/// `base`：要和它并排的那个；没有时是选中的那个。
fn terminal_split_menu(
    view: WeakEntity<RepoView>,
    base: Option<u32>,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |mut menu, _, cx| {
        let choices = view
            .upgrade()
            .map(|this| this.read(cx).terminal_split_choices(base, cx))
            .unwrap_or_default();
        if choices.is_empty() {
            return menu.item(PopupMenuItem::new(tr("没有其他终端")).disabled(true));
        }
        menu = menu.min_w(px(220.)).max_w(px(400.));
        if choices.len() > 14 {
            menu = menu.max_h(px(440.)).scrollable(true);
        }
        for (number, name, state, color) in choices {
            let view = view.clone();
            menu = menu.item(term_split::choice(name, state, color).on_click(
                move |_, window, cx| {
                    view.update(cx, |this, cx| {
                        this.split_terminal_pair(base, number, window, cx)
                    })
                    .ok();
                },
            ));
        }
        menu
    }
}

impl RepoView {
    /// 这个标签页里有几个开在 Kuai 进程里的终端的程序正在运行（退出 Kuai、重启以完成更新时
    /// 将结束）。开在 kuaid 里的不算。
    pub fn local_terminals_running(&self, cx: &App) -> usize {
        if self.entry.server.is_some() {
            return 0;
        }
        self.terminals
            .tabs
            .iter()
            .filter(|tab| {
                let view = tab.view.read(cx);
                view.in_process() && view.is_running()
            })
            .count()
    }

    /// 这个仓库有几个终端里有东西在跑：这次打开的，加上服务器上留着、还没接上的，
    /// 和关掉的标签页留下、还没接回来的本机终端。
    pub(super) fn terminals_running(&self, cx: &App) -> usize {
        let opened = self
            .terminals
            .tabs
            .iter()
            .filter(|tab| tab.view.read(cx).is_running())
            .count();
        let waiting = self.terminals.waiting.iter().filter(|term| term.alive);
        opened + waiting.count() + parked_running(Some(&self.entry), cx)
    }

    /// 本机仓库开在 Kuai 进程里的终端不跟着标签页结束：这个视图释放（标签页关掉）时，程序还在
    /// 运行的终端交给 [`Parked`] 保管；这个仓库有终端留下时，侧栏切换上的个数跟着变，到终端页时
    /// 接回来。建视图时调用。
    pub(super) fn keep_local_terminals(&mut self, cx: &mut Context<Self>) {
        if self.entry.server.is_some() {
            return;
        }
        cx.on_release(|this, cx| this.park_terminals(cx)).detach();
        let observe = cx.observe_global::<Parked>(|_, cx| cx.notify());
        self._subscriptions.push(observe);
        self.check_local_terminals(cx);
    }

    /// 这台电脑的 kuaid 在跑的话，问一下它里面留着这个仓库的哪几个终端。它没在跑就不问：
    /// 没有留着的终端，用不着为了问这一句把它拉起来。
    fn check_local_terminals(&mut self, cx: &mut Context<Self>) {
        if !Connection::local_daemon_running() {
            return;
        }
        self.terminals.listed = false;
        let connecting = self.app.update(cx, |app, cx| app.connect_local(cx));
        cx.spawn(async move |this, cx| {
            let Ok(conn) = connecting.await else {
                return;
            };
            let Ok(listing) = this.update(cx, |this, _| this.waiting_list(&conn)) else {
                return;
            };
            let Some(found) = listing.await else {
                return;
            };
            this.update(cx, |this, cx| this.set_waiting(found, cx)).ok();
        })
        .detach();
    }

    /// 跟着主窗口的那份实时终端列表走。建视图时调用。
    pub(super) fn follow_board(&mut self, cx: &mut Context<Self>) {
        let Some(board) = cx
            .try_global::<BoardHandle>()
            .map(|handle| handle.0.clone())
        else {
            return;
        };
        let observe = cx.observe(&board, |this, _, cx| this.board_changed(cx));
        self._subscriptions.push(observe);
        self.terminals.board = Some(board);
        self.board_changed(cx);
    }

    /// 这个仓库的终端现在有哪些（实时列表里的）。那台电脑的列表还没读到、没连着是 None。
    fn board_rows(&self, cx: &App) -> Option<Vec<TermRow>> {
        let board = self.terminals.board.as_ref()?;
        board.read(cx).terms_of(&self.entry)
    }

    /// 终端列表有变化：别处开的记到 `waiting` 里（到终端页时接上），别处结束了的把这边的那一行
    /// 去掉，别处重启了的接上新的。开在 Kuai 进程里的终端不在列表里，不管。
    fn board_changed(&mut self, cx: &mut Context<Self>) {
        let Some(rows) = self.board_rows(cx) else {
            return;
        };
        let listed = |number: u32| rows.iter().find(|row| row.number == number);
        let before = self.terminals.ending.len();
        self.terminals
            .ending
            .retain(|number| listed(*number).is_some());
        let mut changed = self.terminals.ending.len() != before;
        let mut gone: Vec<usize> = Vec::new();
        let mut restarted: Vec<(Entity<TerminalView>, String)> = Vec::new();
        for (index, tab) in self.terminals.tabs.iter_mut().enumerate() {
            let view = tab.view.read(cx);
            if view.in_process() {
                continue;
            }
            let mine = view.term_id();
            match (listed(tab.number), &mine) {
                (Some(row), Some(id)) if *id == row.id.term => tab.listed_as = mine,
                // 位置上换了别的编号，手上这个已经结束：别处把它重启了，按新的编号接上
                (Some(row), Some(_)) if tab.listed_as == mine && !view.is_alive() => {
                    restarted.push((tab.view.clone(), row.id.term.clone()));
                }
                (None, Some(_)) if tab.listed_as == mine => gone.push(index),
                (None, None) if tab.adopted => gone.push(index),
                _ => {}
            }
        }
        for (view, term) in restarted {
            view.update(cx, |view, cx| view.reattach(term, cx));
            changed = true;
        }
        for index in gone.into_iter().rev() {
            self.terminals.tabs.remove(index);
            if index < self.terminals.active {
                self.terminals.active -= 1;
            } else if index == self.terminals.active {
                self.terminals.refocus = true;
            }
            changed = true;
        }
        let last = self.terminals.tabs.len().saturating_sub(1);
        self.terminals.active = self.terminals.active.min(last);
        // 列表里有、这边还没开的：留着等接上
        let opened: Vec<u32> = self.terminals.tabs.iter().map(|tab| tab.number).collect();
        let waiting: Vec<Waiting> = rows
            .iter()
            .filter(|row| !opened.contains(&row.number))
            .filter(|row| !self.terminals.ending.contains(&row.number))
            .map(|row| Waiting {
                number: row.number,
                alive: row.alive,
                term: row.id.term.clone(),
                // 实时列表到的时候已经对过存下的预设了（`TermBoard::arrived`），用不着
                age: 0,
            })
            .collect();
        if waiting != self.terminals.waiting {
            self.terminals.waiting = waiting;
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// 标签页关掉了：程序还在运行的终端交给 [`Parked`]。已经结束的不留，用户看到过了
    /// （kuaid 里的会话也是这样）。
    fn park_terminals(&mut self, cx: &mut App) {
        let tabs = std::mem::take(&mut self.terminals.tabs);
        let running: Vec<ParkedTerminal> = tabs
            .into_iter()
            .filter(|tab| {
                let view = tab.view.read(cx);
                // 开在 kuaid 里的不用保管，它们本来就留着
                view.in_process() && view.is_running()
            })
            .map(|tab| ParkedTerminal {
                entry: self.entry.clone(),
                number: tab.number,
                view: tab.view,
            })
            .collect();
        if !running.is_empty() {
            log::info!(
                "标签页已关闭，保留 {} 个本机终端：{}",
                running.len(),
                self.entry.path
            );
            cx.default_global::<Parked>().0.extend(running);
        }
    }

    /// 接回关掉的标签页留下的、这个仓库的本机终端。序号和已经打开的重了（同一个仓库在别的
    /// 工作区里也开着）就换成最小的空号。
    fn adopt_parked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entry = self.entry.clone();
        let mine = |t: &ParkedTerminal| t.entry == entry;
        // 先只读地看一眼：取出来会通知所有盯着的仓库页重画，每帧都取就停不下来了
        if !cx
            .try_global::<Parked>()
            .is_some_and(|parked| parked.0.iter().any(mine))
        {
            return;
        }
        let taken: Vec<ParkedTerminal> = {
            let parked = &mut cx.global_mut::<Parked>().0;
            let (taken, rest) = std::mem::take(parked).into_iter().partition(mine);
            *parked = rest;
            taken
        };
        log::info!("接回 {} 个本机终端：{}", taken.len(), entry.path);
        for terminal in taken {
            let used: Vec<u32> = self.terminals.tabs.iter().map(|tab| tab.number).collect();
            let number = match used.contains(&terminal.number) {
                true => free_number(&used),
                false => terminal.number,
            };
            self.insert_terminal(number, terminal.view, String::new(), window, cx);
        }
    }

    /// 连上服务器以后问一下：kuaid 里留着这个仓库的哪几个终端（侧栏的切换上标出个数，
    /// 到终端页时都接回来）。
    pub(super) fn check_server_terminals(&mut self, channel: &Channel, cx: &mut Context<Self>) {
        let Channel::Remote(conn) = channel else {
            return;
        };
        self.terminals.listed = false;
        let listing = self.waiting_list(conn);
        cx.spawn(async move |this, cx| {
            let Some(found) = listing.await else {
                return;
            };
            this.update(cx, |this, cx| this.set_waiting(found, cx)).ok();
        })
        .detach();
    }

    /// 问 kuaid 里留着这个仓库的哪几个终端；问不到是 None。
    fn waiting_list(
        &self,
        conn: &Connection,
    ) -> impl Future<Output = Option<Vec<Waiting>>> + use<> {
        let listing = conn.term_list();
        let repo = self.entry.path.clone();
        async move {
            let terms = listing.await.ok()?;
            let mut found: Vec<Waiting> = terms
                .iter()
                .filter_map(|term| {
                    Some(Waiting {
                        number: terminal::session_number(&repo, &term.key)?,
                        alive: term.alive,
                        term: term.term.clone(),
                        age: term.age,
                    })
                })
                .collect();
            found.sort_by_key(|term| term.number);
            found.dedup_by_key(|term| term.number);
            Some(found)
        }
    }

    /// 记下问到的留着的终端（[`Self::waiting_list`]），到终端页时接上。
    fn set_waiting(&mut self, mut found: Vec<Waiting>, cx: &mut Context<Self>) {
        self.terminals.listed = true;
        // 上次运行时存下的从预设开的终端：对上了，接回来时叫预设的名字
        for waiting in &found {
            let slot = TermSlot {
                entry: self.entry.clone(),
                number: waiting.number,
            };
            presets::confirm(&slot, &waiting.term, waiting.age, cx);
        }
        // 实时列表读到了就以它为准（它一直跟着变，这里问到的只是这一刻的）
        if self.board_rows(cx).is_some() {
            return;
        }
        // 已经开着的不用再接
        let opened: Vec<u32> = self.terminals.tabs.iter().map(|tab| tab.number).collect();
        found.retain(|term| !opened.contains(&term.number));
        self.terminals.waiting = found;
        cx.notify();
    }

    /// 加一个接 kuaid 里留着的会话 `term` 的终端，返回它在第几个。`restored`：里面的程序还在跑。
    fn add_terminal(
        &mut self,
        number: u32,
        restored: bool,
        term: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        self.add_terminal_in(number, restored, String::new(), Some(term), window, cx)
    }

    /// 加一个终端，开在仓库里的 `dir` 目录（相对仓库根），返回它在第几个。`term`：要接的是 kuaid 里的
    /// 哪个会话；新开的是 None。
    fn add_terminal_in(
        &mut self,
        number: u32,
        restored: bool,
        dir: String,
        term: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let (entry, app, sub) = (self.entry.clone(), self.app.clone(), dir.clone());
        // 从预设开的（见 [`Self::open_new_terminal`]）：新开出来的 shell 里替用户输入命令。接的是别处
        // 开的会话（这个序号原来那个结束了，别处又用它开了一个）就不算，不叫预设的名字，重启也不输入
        let slot = TermSlot {
            entry: entry.clone(),
            number,
        };
        let start = presets::started(&slot, term, cx);
        let view = cx.new(|cx| {
            let mut view = TerminalView::new(
                move |cx| crate::shell::terminal_launch(&entry, &app, number, &dir, cx),
                window,
                cx,
            );
            if restored {
                view.mark_restored();
            }
            view.run_on_start(start, Some(slot));
            view
        });
        self.insert_terminal(number, view, sub, window, cx)
    }

    /// 把终端按序号排进去，返回它在第几个。`sub`：终端开在仓库里的哪个目录（相对仓库根，认终端里
    /// 的相对路径用）。
    fn insert_terminal(
        &mut self,
        number: u32,
        view: Entity<TerminalView>,
        sub: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let observe = cx.observe(&view, |_, _, cx| cx.notify());
        // 终端里按住 Ctrl 单击的路径按这个仓库认；通道每次问的时候现拿（连接会断、会换）
        let repo = cx.entity().downgrade();
        let base = terminal::LinkBase {
            root: self.entry.path.clone(),
            sub,
            local: self.entry.server.is_none(),
            server: self.entry.server.clone(),
            channel: Rc::new(move |cx: &App| repo.upgrade()?.read(cx).channel.clone()),
        };
        view.update(cx, |view, _| view.set_link_base(base));
        let events = cx.subscribe_in(
            &view,
            window,
            |this, _, event: &TerminalEvent, window, cx| match event {
                TerminalEvent::OpenFile { path, at, dir } => {
                    this.show_path(path.clone(), *at, *dir, window, cx)
                }
            },
        );
        let tab = TermTab {
            number,
            view,
            listed_as: None,
            adopted: false,
            _observe: observe,
            _events: events,
        };
        let at = self
            .terminals
            .tabs
            .iter()
            .position(|other| other.number > number)
            .unwrap_or(self.terminals.tabs.len());
        self.terminals.tabs.insert(at, tab);
        at
    }

    /// 留着、还没接上的终端都加进来：关掉的标签页留下的本机终端，kuaid 里留着的（选中哪个
    /// 才去接哪个）。显示的还是原来那个。
    fn adopt_waiting(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self
            .terminals
            .tabs
            .get(self.terminals.active)
            .map(|tab| tab.number);
        let before = self.terminals.tabs.len();
        self.adopt_parked(window, cx);
        for waiting in std::mem::take(&mut self.terminals.waiting) {
            let at = self.add_terminal(waiting.number, waiting.alive, &waiting.term, window, cx);
            let tab = &mut self.terminals.tabs[at];
            tab.adopted = true;
            // 列表里它是这个编号：列表里没有它了就是结束了（接上以前也算）
            tab.listed_as = Some(waiting.term.clone());
            tab.view.update(cx, |view, _| view.attach_to(waiting.term));
        }
        if self.terminals.tabs.len() == before {
            return;
        }
        // 新加的可能排在前面，显示的那个的位置跟着变
        let at = |shown| {
            self.terminals
                .tabs
                .iter()
                .position(|tab| tab.number == shown)
        };
        self.terminals.active = shown.and_then(at).unwrap_or(0);
    }

    /// 画之前调用。正在终端页上、这时才知道还留着别的终端（服务器连得慢，或者断了重连；
    /// 同一个本机仓库在别的工作区里的标签页关掉了）：补进来。
    pub(super) fn sync_terminals(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 并排着的、勾上的终端结束了：去掉；并排只剩一个时回到单个
        let numbers: Vec<u32> = self.terminals.tabs.iter().map(|tab| tab.number).collect();
        let terms = &mut self.terminals;
        terms.picks.retain(|number| numbers.contains(number));
        terms.split.retain(|number| numbers.contains(number));
        if terms.split.len() < term_split::MIN_CELLS {
            terms.split.clear();
            terms.zoom = None;
        }
        if terms.zoom.is_some_and(|zoom| !terms.split.contains(&zoom)) {
            terms.zoom = None;
        }
        if self.page == Page::Terminal {
            self.adopt_waiting(window, cx);
            // 显示着的那个在别处结束了：焦点放到换上来的这个上
            if std::mem::take(&mut self.terminals.refocus) && window.focused(cx).is_none() {
                self.focus_terminal(window, cx);
            }
        }
    }

    /// 到 Git 页（终端页的「打开仓库」）。
    pub fn show_git_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_page(Page::Git, window, cx);
    }

    /// 切到 Git、终端或文件页。到终端页时留着的都接回来，焦点放进终端；一个都没有就显示空的
    /// 样子，不自己开：终端开在 kuaid 里，开了就一直留着，只是过来看一眼不该留下一个空终端。
    /// 离开终端页不结束里面的程序。到文件页时焦点放到打开的文件（没有就是目录树）上。
    pub(super) fn show_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        // 焦点还留在看不见的那一页上的话，键盘就没反应了（或者改到看不见的文件里去）
        let stray = match self.page {
            Page::Terminal => page != Page::Terminal && self.terminal_focused(window, cx),
            Page::Files => page != Page::Files && self.files_focused(window, cx),
            Page::Git => false,
        };
        if stray {
            window.blur(cx);
        }
        let from = std::mem::replace(&mut self.page, page);
        match page {
            Page::Terminal => {
                if from != Page::Terminal {
                    self.back_page = from;
                }
                self.adopt_waiting(window, cx);
                if self.terminals.tabs.is_empty() {
                    // 焦点留在刚才那一页的输入框里的话，敲的字会进到看不见的地方
                    window.blur(cx);
                } else {
                    self.focus_terminal(window, cx);
                }
            }
            Page::Files => {
                self.focus_files(window, cx);
                if from != Page::Files {
                    self.refresh_files(cx);
                }
            }
            Page::Git => {}
        }
        cx.notify();
    }

    /// Ctrl+`：在终端页和刚才那一页（Git 或者文件）之间来回切。
    pub fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = match self.page {
            Page::Terminal => self.back_page,
            Page::Git | Page::Files => Page::Terminal,
        };
        self.show_page(page, window, cx);
    }

    /// Ctrl+Shift+E：切到文件页；已经在文件页就把焦点放到目录树上。
    pub fn show_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page == Page::Files {
            self.files.focus.focus(window, cx);
            return;
        }
        self.show_page(Page::Files, window, cx);
    }

    /// 再开一个终端，开在仓库里的 `dir` 目录（相对仓库根，空的是仓库根；文件页的「终端」、目录树的
    /// 「在此新建终端」），切到终端页显示它，在新开的 shell 里输入 `start`（None 是只开 shell）。
    ///
    /// 终端按名字（第几个）开，kuaid 里正好留着同名的就是接上它。还不知道留着哪几个时（还在连、
    /// 刚连上，实时列表和问的那一次都还没回来）先问一遍：留着的先接回来，新的用空着的序号。
    pub(super) fn new_terminal_with(
        &mut self,
        dir: String,
        start: Option<Start>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminals.listed || self.board_rows(cx).is_some() {
            return self.open_new_terminal(dir, start, window, cx);
        }
        // 去哪问：开终端时连的那一台（见 [`crate::shell::terminal_launch`]），还在连就等连上。
        // 本机的问这台电脑在跑的 kuaid，没在跑就没有留着的
        let server = self.entry.server.clone();
        let connecting = self.app.update(cx, |app, cx| match &server {
            Some(server) if app.knows_server(server) => Some(app.connect(server, cx)),
            Some(_) => None,
            None if Connection::local_daemon_running() => Some(app.connect_local(cx)),
            None => None,
        });
        let Some(connecting) = connecting else {
            return self.open_new_terminal(dir, start, window, cx);
        };
        // 连得慢时点了也看得出在开：先到终端页
        self.terminals.asking += 1;
        self.show_page(Page::Terminal, window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let found = match connecting.await {
                Ok(conn) => match this.update(cx, |this, _| this.waiting_list(&conn)) {
                    Ok(listing) => Ok(listing.await),
                    Err(_) => return,
                },
                Err(err) => Err(err),
            };
            this.update_in(cx, |this, window, cx| {
                this.terminals.asking -= 1;
                match found {
                    Ok(found) => {
                        if let Some(found) = found {
                            this.set_waiting(found, cx);
                        }
                        this.open_new_terminal(dir, start, window, cx);
                    }
                    // 本机的 kuaid 连不上：开终端时退回到 Kuai 进程里
                    Err(_) if server.is_none() => this.open_new_terminal(dir, start, window, cx),
                    Err(err) => {
                        let text = tf!("终端启动失败：{err}", err);
                        window.push_notification(Notification::error(text), cx);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn open_new_terminal(
        &mut self,
        dir: String,
        start: Option<Start>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 留着的先接回来，新的排在它们后面
        self.adopt_waiting(window, cx);
        let used: Vec<u32> = self.terminals.tabs.iter().map(|tab| tab.number).collect();
        let number = free_number(&used);
        let slot = TermSlot {
            entry: self.entry.clone(),
            number,
        };
        presets::remember(slot, start, cx);
        self.terminals.active = self.add_terminal_in(number, false, dir, None, window, cx);
        self.show_page(Page::Terminal, window, cx);
    }

    /// 再开一个终端（侧栏的「新终端」、Ctrl+Shift+`），切到终端页显示它。运行「默认运行」的那个预设。
    pub fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let start = presets::default_start(cx);
        self.new_terminal_with(String::new(), start, window, cx);
    }

    /// 在仓库根新开一个终端，输入 `start`（None 是只开 shell）：给预设设的快捷键。
    pub fn new_terminal_running(
        &mut self,
        start: Option<Start>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_terminal_with(String::new(), start, window, cx);
    }

    /// 新建终端的菜单（「新终端」的 ⌄、图标栏上的 +、没有终端时的 ⌄）：选了就在这个仓库里开。
    fn presets_menu(
        &self,
        cx: &mut Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        self.presets_menu_in(String::new(), cx)
    }

    /// 在仓库里的 `dir` 目录（相对仓库根，空的是仓库根）新建终端的菜单：文件页工具栏的「终端」、目录树
    /// 右键的「在此新建终端」也用它。「运行命令」对话框的标题写这个目录。
    pub(super) fn presets_menu_in(
        &self,
        dir: String,
        cx: &mut Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        launch_menu(cx.entity().downgrade(), self.launch_place(&dir, cx), dir)
    }

    /// 「运行命令」对话框的标题里写的开在哪：仓库根是仓库名，子目录是它相对仓库根的路径。
    pub(super) fn launch_place(&self, dir: &str, cx: &App) -> SharedString {
        match dir.is_empty() {
            true => self.app.read(cx).repo_name(&self.entry),
            false => dir.to_string().into(),
        }
    }

    /// 第 `index` 个终端叫什么：实时列表里算好的（起过的名字、同名加序号，见 [`TermRow::label`]）；
    /// 还不在列表里的（开在 Kuai 进程里的、刚开的）用程序设的标题。
    fn terminal_name(&self, index: usize, cx: &App) -> SharedString {
        if let Some(row) = self.board_row(index, cx) {
            return row.label;
        }
        self.terminals
            .tabs
            .get(index)
            .and_then(|tab| tab.view.read(cx).label())
            .unwrap_or_else(|| tr("终端").into())
    }

    /// 终端页上并排着的终端（几个序号）；没并排、不在终端页、这一页没显示出来时是空的。
    pub fn split_terminals(&self) -> Vec<(RepoEntry, u32)> {
        if !self.visible || self.page != Page::Terminal || self.terminals.split.len() < 2 {
            return Vec::new();
        }
        self.terminals
            .split
            .iter()
            .map(|number| (self.entry.clone(), *number))
            .collect()
    }

    /// 终端页上正显示着的是第几个终端；不在终端页、这一页没显示出来时是 None。
    pub fn shown_terminal(&self) -> Option<(RepoEntry, u32)> {
        if !self.visible || self.page != Page::Terminal {
            return None;
        }
        let tab = self.terminals.tabs.get(self.terminals.active)?;
        Some((self.entry.clone(), tab.number))
    }

    fn select_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.terminals.tabs.len() {
            self.terminals.active = index;
            self.focus_terminal(window, cx);
            cx.notify();
        }
    }

    fn active_terminal(&self) -> Option<&Entity<TerminalView>> {
        self.terminals
            .tabs
            .get(self.terminals.active)
            .map(|tab| &tab.view)
    }

    fn focus_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active_terminal() {
            view.focus_handle(cx).focus(window, cx);
        }
    }

    /// 这一页显示的是终端页时，把焦点放进选中的终端（从会话页回来时用）。
    pub fn focus_shown_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page == Page::Terminal {
            self.focus_terminal(window, cx);
        }
    }

    fn terminal_focused(&self, window: &Window, cx: &App) -> bool {
        self.active_terminal()
            .is_some_and(|view| view.focus_handle(cx).is_focused(window))
    }

    /// 第 `index` 个终端在实时列表里的那一行。
    fn board_row(&self, index: usize, cx: &App) -> Option<TermRow> {
        let number = self.terminals.tabs.get(index)?.number;
        let rows = self.board_rows(cx)?;
        rows.into_iter().find(|row| row.number == number)
    }

    /// 实时列表订阅这个仓库所在那台电脑用的连接：没连着是 None。
    fn board_conn(&self, cx: &App) -> Option<Arc<Connection>> {
        let board = self.terminals.board.as_ref()?;
        board
            .read(cx)
            .conn(crate::machines::machine_of(&self.entry))
    }

    /// 第 `index` 个终端还没接上（没选中过，或者还在连）时，[`TerminalView::kill`] 关不了 kuaid 里的
    /// 那个会话：照它等着接的编号（没有就照实时列表里这个位置上的那一行）经实时列表的连接去关。
    /// 返回发出去的请求；没有要关的、关不了的是 None。
    fn close_unattached(&self, index: usize, cx: &App) -> Option<Closing> {
        let view = self.terminals.tabs.get(index)?.view.read(cx);
        if view.in_process() || view.term_id().is_some() {
            return None;
        }
        let term = self.tab_term(index, cx)?;
        Some(self.board_conn(cx)?.term_close(&term))
    }

    /// 第 `index` 个终端是 kuaid 里的哪个会话：接上的是它接着的；还没接上的是它等着接的，没有就照实时
    /// 列表里这个位置上的那一行。开在 Kuai 进程里的、都不知道的是 None。
    fn tab_term(&self, index: usize, cx: &App) -> Option<String> {
        let view = self.terminals.tabs.get(index)?.view.read(cx);
        if view.in_process() {
            return None;
        }
        view.term_id()
            .or_else(|| view.pending())
            .or_else(|| Some(self.board_row(index, cx)?.id.term))
    }

    /// 结束、重启第 `index` 个终端。里面还在输出、或者除了 shell 还有程序在跑的先问一句，
    /// 和终端页一样（见 [`crate::term_end`]）。
    fn ask_first(
        &mut self,
        index: usize,
        restart: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.terminals.tabs.get(index) else {
            return;
        };
        let (number, view) = (tab.number, tab.view.clone());
        let terminal = view.read(cx);
        let name: SharedString = terminal
            .label()
            .unwrap_or_else(|| tf!("终端 {n}", n = number.to_string()).into());
        // 它接着的、等着接的那个会话。实时列表里这个位置上是别的会话（列表还没跟上）时，那一行不算它的
        let target = terminal.term_id().or_else(|| terminal.pending());
        let row = self
            .board_row(index, cx)
            .filter(|row| target.as_ref().is_none_or(|term| *term == row.id.term));
        let ending = crate::term_end::Ending {
            term: target,
            exited: !terminal.is_running(),
            conn: terminal.conn().or_else(|| self.board_conn(cx)),
            repo: self.app.read(cx).repo_name(&self.entry),
            name,
            row,
            restart,
        };
        // 问的这一会儿列表可能变了：按序号再找一遍，还得是同一个终端（不是结束以后新开在这个位置上的）
        let run = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            let at = this
                .terminals
                .tabs
                .iter()
                .position(|tab| tab.number == number && tab.view == view);
            match (at, restart) {
                (Some(at), true) => this.restart_terminal_now(at, window, cx),
                (Some(at), false) => this.end_terminal_now(at, window, cx),
                (None, _) => {}
            }
        };
        crate::term_end::ask_first(self, ending, window, cx, run);
    }

    /// 结束第 `index` 个终端。里面还在输出的先问。
    fn end_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_first(index, false, window, cx);
    }

    /// 结束第 `index` 个终端。显示的是它就换到旁边那个；一个都不剩时留在终端页，显示空的样子。
    fn end_terminal_now(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.terminals.tabs.len() {
            return;
        }
        let number = self.terminals.tabs[index].number;
        if self.board_row(index, cx).is_some() {
            self.terminals.ending.push(number);
        }
        // 留着、还没接上的（没选中过）`kill` 说不了：照编号去结束
        let unattached = self.close_unattached(index, cx);
        let tab = self.terminals.tabs.remove(index);
        if let Some(term) = tab.view.read(cx).term_id() {
            let slot = TermSlot {
                entry: self.entry.clone(),
                number,
            };
            presets::ended(&slot, &term, cx);
        }
        // 服务器上的终端不说一声的话会接着跑
        let closing = tab.view.update(cx, |view, _| view.kill()).or(unattached);
        if let Some(closing) = closing {
            self.watch_closing(number, closing, window, cx);
        }
        if index < self.terminals.active {
            self.terminals.active -= 1;
        }
        if self.terminals.tabs.is_empty() {
            self.terminals.active = 0;
            // 焦点可能还在刚丢掉的终端上
            window.blur(cx);
        } else {
            self.terminals.active = self.terminals.active.min(self.terminals.tabs.len() - 1);
            self.focus_terminal(window, cx);
        }
        cx.notify();
    }

    /// 结束第 `index` 个终端里的程序，重新开一个，并显示它。里面还在输出的先问。
    fn restart_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_first(index, true, window, cx);
    }

    /// 等结束终端的请求的回答。没结束掉（连接断了）：不再当它是要结束的，列表里它还在就回到「留着」
    /// 里（到终端页时接回来），说一声。原来只记日志，终端在主机上接着跑，这边却一直藏着它。
    fn watch_closing(
        &mut self,
        number: u32,
        closing: Closing,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let Err(err) = closing.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.terminals.ending.retain(|other| *other != number);
                let text = tf!("结束终端失败：{err}", err = format!("{err:#}"));
                window.push_notification(Notification::error(text), cx);
                this.board_changed(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn restart_terminal_now(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        // 还没接上的：先照编号关掉 kuaid 里的那个会话，关掉了再开（`restart` 自己关不了，原来重启
        // 只是接回了原来那个）
        let closing = self.close_unattached(index, cx);
        // 从预设开的：记着的是这个会话的，重启以后新开的再输入一遍；是别的会话的去掉。还没接上的按它
        // 等着接的那个会话认（关的也是它）：原来这时不说，新开的会话就认不出是这个预设的了
        let term = self.tab_term(index, cx);
        let Some(tab) = self.terminals.tabs.get_mut(index) else {
            return;
        };
        // 重开以后是新的终端，不再是「留着、接回来的」
        tab.adopted = false;
        let view = tab.view.clone();
        let slot = TermSlot {
            entry: self.entry.clone(),
            number: tab.number,
        };
        if let Some(term) = term {
            presets::restarting(&slot, &term, cx);
            // 起的名字跟到新开的那个上
            if let Some(board) = self.terminals.board.clone() {
                let id = crate::term_board::TermId {
                    machine: crate::machines::machine_of(&self.entry).to_string(),
                    term,
                };
                board.update(cx, |board, cx| board.restarting(&id, cx));
            }
        }
        self.select_terminal(index, window, cx);
        view.update(cx, |view, cx| view.restart(closing, cx));
    }

    // ---------- 改名 ----------

    /// 开始给第 `index` 个终端改名：侧栏那一行的名字换成输入框。不在实时列表里的（开在 Kuai 进程
    /// 里的）改不了。
    pub(super) fn start_terminal_rename(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(tab), Some(row)) = (self.terminals.tabs.get(index), self.board_row(index, cx))
        else {
            return;
        };
        let number = tab.number;
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
                    InputEvent::PressEnter { .. } => {
                        this.finish_terminal_rename(true, true, window, cx)
                    }
                    InputEvent::Blur => this.finish_terminal_rename(true, false, window, cx),
                    _ => {}
                },
            );
        input.update(cx, |state, cx| state.focus(window, cx));
        self.terminals.renaming = Some((number, input, events));
        cx.notify();
    }

    /// 改完了（`keep`：确认，不然是取消）；`refocus` 时焦点回到侧栏的终端列表上。
    fn finish_terminal_rename(
        &mut self,
        keep: bool,
        refocus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((number, input, _)) = self.terminals.renaming.take() else {
            return;
        };
        let index = self
            .terminals
            .tabs
            .iter()
            .position(|tab| tab.number == number);
        let row = index.and_then(|index| self.board_row(index, cx));
        if keep && let (Some(row), Some(board)) = (row, self.terminals.board.clone()) {
            let name = input.read(cx).value().to_string();
            // 和默认的名字一样（没改）不算起了名字：程序自己设的标题还会变
            if row.named || row.label.as_ref() != name.trim() {
                board.update(cx, |board, cx| board.rename(&row.id, &name, cx));
            }
        }
        if refocus {
            let focus = self.terminal_side_focus(cx);
            focus.focus(window, cx);
        }
        cx.notify();
    }

    /// 侧栏一行里的改名输入框（正在改这个终端的名字时）。
    fn terminal_rename_box(&self, number: u32, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (_, input, _) = self
            .terminals
            .renaming
            .as_ref()
            .filter(|(at, _, _)| *at == number)?;
        Some(
            div()
                .id("terminal-rename")
                .flex_1()
                .min_w_0()
                .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.finish_terminal_rename(false, true, window, cx);
                    }
                }))
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(Input::new(input).small())
                .into_any_element(),
        )
    }

    // ---------- 并排查看 ----------

    fn terminal_picked(&self, number: u32) -> bool {
        match self.terminals.split.is_empty() {
            true => self.terminals.picks.contains(&number),
            false => self.terminals.split.contains(&number),
        }
    }

    /// 勾上、去掉要并排查看的终端；并排着时是加一格、移出一格。
    pub(super) fn toggle_terminal_pick(
        &mut self,
        number: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let max = term_split::MAX_CELLS;
        let too_many = |window: &mut Window, cx: &mut App| {
            let text = tf!("最多并排查看 {n} 个终端", n = max.to_string());
            window.push_notification(Notification::info(text), cx);
        };
        let terms = &mut self.terminals;
        if !terms.split.is_empty() {
            match terms.split.iter().position(|other| *other == number) {
                Some(_) => return self.remove_terminal_cell(number, window, cx),
                None if terms.split.len() >= max => return too_many(window, cx),
                None => {
                    terms.split.push(number);
                    terms.zoom = None;
                }
            }
            cx.notify();
            return;
        }
        match terms.picks.iter().position(|other| *other == number) {
            Some(at) => {
                terms.picks.remove(at);
            }
            None if terms.picks.len() >= max => return too_many(window, cx),
            None => terms.picks.push(number),
        }
        cx.notify();
    }

    /// 右键菜单里并排查看的那一项怎么写（见 [`terminal_split_item`]）。
    fn terminal_split_entry(&self, number: u32, cx: &App) -> SplitEntry {
        let terms = &self.terminals;
        if !terms.split.is_empty() {
            return match terms.split.contains(&number) {
                true => SplitEntry::Remove,
                false => SplitEntry::Add,
            };
        }
        match terms.tabs.get(terms.active).map(|tab| tab.number) {
            Some(base) if base != number => SplitEntry::With {
                base,
                name: term_split::menu_name(&self.terminal_name(terms.active, cx)),
            },
            _ if terms.tabs.len() < term_split::MIN_CELLS => SplitEntry::Nothing,
            _ => SplitEntry::Choose,
        }
    }

    /// 能拿来并排的终端（序号、名字、状态和它的颜色）：`base` 以外的（没有时是选中的那个）；并排着时是
    /// 还没在并排里的。
    fn terminal_split_choices(
        &self,
        base: Option<u32>,
        cx: &App,
    ) -> Vec<(u32, SharedString, SharedString, Hsla)> {
        let terms = &self.terminals;
        let base = base.or(terms.tabs.get(terms.active).map(|tab| tab.number));
        let muted = cx.theme().muted_foreground;
        terms
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| match terms.split.is_empty() {
                true => Some(tab.number) != base,
                false => !terms.split.contains(&tab.number),
            })
            .map(|(index, tab)| {
                let row = self.board_row(index, cx);
                (
                    tab.number,
                    self.terminal_name(index, cx),
                    row.as_ref()
                        .map(TermOverview::state_text)
                        .unwrap_or_default(),
                    row.as_ref()
                        .map(|row| TermOverview::state_color(row, cx))
                        .unwrap_or(muted),
                )
            })
            .collect()
    }

    /// 把序号是 `number` 的终端和 `base` 并排查看（`base` 没有时是选中的那个）：右键「与「…」并排」、
    /// 工具栏的「并排」里挑的。并排着时是加一格（用不到 `base`）。新加的那一格拿到输入焦点。
    fn split_terminal_pair(
        &mut self,
        base: Option<u32>,
        number: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let max = term_split::MAX_CELLS;
        let terms = &mut self.terminals;
        if !terms.split.is_empty() {
            if !terms.split.contains(&number) {
                if terms.split.len() >= max {
                    let text = tf!("最多并排查看 {n} 个终端", n = max.to_string());
                    window.push_notification(Notification::info(text), cx);
                    return;
                }
                terms.split.push(number);
            }
            terms.zoom = None;
            return self.focus_terminal_cell(number, window, cx);
        }
        let active = terms.tabs.get(terms.active).map(|tab| tab.number);
        if let Some(base) = base.or(active).filter(|base| *base != number) {
            terms.picks.clear();
            terms.split = vec![base, number];
            terms.zoom = None;
        }
        self.focus_terminal_cell(number, window, cx);
    }

    /// 并排查看勾上的那几个，焦点进第一格。
    fn start_terminal_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let terms = &mut self.terminals;
        if terms.picks.len() < term_split::MIN_CELLS {
            return;
        }
        terms.split = std::mem::take(&mut terms.picks);
        terms.zoom = None;
        let first = terms.split[0];
        if let Some(index) = terms.tabs.iter().position(|tab| tab.number == first) {
            self.select_terminal(index, window, cx);
        }
        cx.notify();
    }

    /// 退出并排：回到单个，显示焦点格里的那个。
    fn exit_terminal_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminals.split.clear();
        self.terminals.zoom = None;
        self.focus_terminal(window, cx);
        cx.notify();
    }

    /// 把一格移出并排；只剩一格时回到单个。
    fn remove_terminal_cell(&mut self, number: u32, window: &mut Window, cx: &mut Context<Self>) {
        let terms = &mut self.terminals;
        terms.split.retain(|other| *other != number);
        terms.zoom = None;
        if terms.split.len() < term_split::MIN_CELLS {
            if let Some(&last) = terms.split.first()
                && let Some(index) = terms.tabs.iter().position(|tab| tab.number == last)
            {
                terms.active = index;
            }
            return self.exit_terminal_split(window, cx);
        }
        // 移出的是焦点格：焦点换到第一格
        let active = terms.tabs.get(terms.active).map(|tab| tab.number);
        if active == Some(number) {
            let first = terms.split[0];
            if let Some(index) = terms.tabs.iter().position(|tab| tab.number == first) {
                self.select_terminal(index, window, cx);
            }
        }
        cx.notify();
    }

    /// 焦点格换成序号是 `number` 的那个，焦点进去。
    fn focus_terminal_cell(&mut self, number: u32, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self
            .terminals
            .tabs
            .iter()
            .position(|tab| tab.number == number)
        {
            self.select_terminal(index, window, cx);
        }
    }

    /// 放大序号是 `number` 的那一格；已经放大着的还原。
    fn zoom_terminal_cell(&mut self, number: u32, window: &mut Window, cx: &mut Context<Self>) {
        let terms = &mut self.terminals;
        terms.zoom = match terms.zoom {
            Some(zoomed) if zoomed == number => None,
            _ => Some(number),
        };
        self.focus_terminal_cell(number, window, cx);
    }

    // ---------- 键盘 ----------

    /// 侧栏终端列表的焦点（用到时才建）。
    fn terminal_side_focus(&mut self, cx: &mut Context<Self>) -> FocusHandle {
        self.terminals
            .side_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone()
    }

    /// F6、Shift+F6：焦点在侧栏的终端列表和终端之间换。
    pub(super) fn switch_terminal_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page != Page::Terminal {
            return;
        }
        if self.terminal_focused(window, cx) {
            let focus = self.terminal_side_focus(cx);
            focus.focus(window, cx);
        } else {
            self.focus_terminal(window, cx);
        }
        cx.notify();
    }

    /// F2：给选中的终端改名。
    pub(super) fn rename_active_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page == Page::Terminal && self.terminals.renaming.is_none() {
            let index = self.terminals.active;
            self.start_terminal_rename(index, window, cx);
        }
    }

    /// 侧栏终端列表上的键：↑↓ 换终端（焦点不动），Enter、→ 进去输入，空格勾上并排查看。
    fn on_terminal_side_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let n = self.terminals.tabs.len();
        let at = self.terminals.active;
        match event.keystroke.key.as_str() {
            "down" if at + 1 < n => self.terminals.active = at + 1,
            "up" if at > 0 => self.terminals.active = at - 1,
            "down" | "up" => {}
            "enter" | "right" => self.focus_terminal(window, cx),
            "space" => {
                if let Some(number) = self.terminals.tabs.get(at).map(|tab| tab.number) {
                    self.toggle_terminal_pick(number, window, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// 给第 `index` 个终端选图片、文件发进去，并显示它。
    fn attach_to_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.terminals.tabs.get(index).map(|tab| tab.view.clone()) else {
            return;
        };
        self.select_terminal(index, window, cx);
        view.update(cx, |view, cx| view.pick_files(cx));
    }

    /// 终端页的侧栏，接在仓库名和两页的切换下面：Git 页过滤框的位置是「新终端」，
    /// 下面一个终端一行。
    pub(super) fn render_terminal_side(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, muted, bg, border, hover) = (
            theme.sidebar_foreground,
            theme.muted_foreground,
            theme.background,
            theme.input,
            theme.list_hover,
        );
        // 分体按钮：左边开「默认运行」的那个预设，右边的 ⌄ 列出全部预设。和 Git 页的过滤框一样大、
        // 一样的边框，切页时下面的东西不跳
        let default = presets::default_preset(cx);
        let (icon, label, tip): (Icon, SharedString, SharedString) = match &default {
            Some(preset) => (
                presets::icon(&preset.icon)
                    .size_3p5()
                    .text_color(presets::color(&preset.icon, preset.hue, cx)),
                preset.name.clone().into(),
                crate::keymap::with_key(
                    &tf!(
                        "新建终端并运行 {name}（Ctrl+Shift+`）",
                        name = preset.name.clone()
                    ),
                    "new.default",
                    cx,
                ),
            ),
            None => (
                ui::icon(IconName::Plus).text_color(muted),
                tr("新终端").into(),
                crate::keymap::with_key(tr("新建终端（Ctrl+Shift+`）"), "new.default", cx),
            ),
        };
        let shortcut = crate::keymap::label("new.default", cx);
        let new_button = h_flex()
            .h(px(24.))
            .rounded(px(ui::RADIUS))
            .border_1()
            .border_color(border)
            .bg(bg)
            .text_size(px(13.))
            .text_color(fg)
            .child(
                h_flex()
                    .id("terminal-new")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .pl(px(8.))
                    .pr(px(6.))
                    .gap(px(6.))
                    .rounded_l(px(ui::RADIUS))
                    .cursor_pointer()
                    .hover(|s| s.bg(hover))
                    .child(icon.flex_none())
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .when_some(shortcut, |row, keys| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(muted)
                                .child(keys),
                        )
                    })
                    .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
                    .on_click(cx.listener(|this, _, window, cx| this.new_terminal(window, cx))),
            )
            .child(div().flex_none().w(px(1.)).h_full().bg(border))
            .child(
                Button::new("terminal-new-menu")
                    .ghost()
                    .xsmall()
                    .icon(ui::icon_small(IconName::ChevronDown))
                    .tooltip(tr("选择要运行的程序"))
                    .dropdown_menu(self.presets_menu(cx)),
            );
        let rows: Vec<AnyElement> = (0..self.terminals.tabs.len())
            .map(|index| self.render_terminal_row(index, cx))
            .collect();
        let empty = rows.is_empty();
        // 勾了要并排查看的：底下一条「已选 n 个」「并排查看」「取消」
        let picking =
            (!self.terminals.picks.is_empty() && self.terminals.split.is_empty()).then(|| {
                let n = self.terminals.picks.len();
                h_flex()
                    .flex_none()
                    .px(px(10.))
                    .py(px(7.))
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_size(px(12.))
                    .child(div().flex_1().min_w_0().truncate().child(
                        match n < term_split::MIN_CELLS {
                            true => tf!(
                                "再选 {more} 个",
                                more = (term_split::MIN_CELLS - n).to_string()
                            ),
                            false => tf!("已选 {n} 个", n = n.to_string()),
                        },
                    ))
                    .child(
                        Button::new("terminal-split-go")
                            .small()
                            .primary()
                            .label(tr("并排查看"))
                            .tooltip(tr("勾选 2～4 个终端并排查看"))
                            .disabled(n < term_split::MIN_CELLS)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_terminal_split(window, cx)
                            })),
                    )
                    .child(
                        Button::new("terminal-split-cancel")
                            .small()
                            .label(tr("取消"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.terminals.picks.clear();
                                cx.notify();
                            })),
                    )
            });
        let side_focus = self.terminals.side_focus.clone();
        v_flex()
            .flex_1()
            .min_h_0()
            .child(div().px(px(10.)).pt(px(8.)).pb(px(4.)).child(new_button))
            .child(
                v_flex()
                    .id("terminal-rows")
                    .flex_1()
                    .min_h_0()
                    .py(px(4.))
                    .overflow_y_scroll()
                    .when_some(side_focus, |list, focus| {
                        list.track_focus(&focus)
                            .on_key_down(cx.listener(Self::on_terminal_side_key))
                    })
                    .children(rows)
                    .when(empty, |list| {
                        list.child(
                            div()
                                .px(px(16.))
                                .py(px(4.))
                                .text_xs()
                                .text_color(muted)
                                .child(tr("没有打开的终端")),
                        )
                    }),
            )
            .children(picking)
    }

    /// 侧栏里一个终端的一行：序号（鼠标移上去、勾选并排时换成勾）、程序和状态的图标、名字（和会话页
    /// 一样：起过的名字、同名加序号；双击改名）、状态，选中的和鼠标停着的行尾有结束的 ×。点一下切
    /// 过去，右键有改名、并排查看、发文件、重开、结束。设置里开了「最后一行」的，名字下面多一行。
    fn render_terminal_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, selected_bg, hover, border, close_hover, primary, bg) = (
            theme.sidebar_foreground,
            theme.muted_foreground,
            theme.sidebar_accent,
            theme.list_hover,
            theme.input,
            theme.secondary,
            theme.primary,
            theme.background,
        );
        let tab = &self.terminals.tabs[index];
        let number = tab.number;
        let selected = index == self.terminals.active;
        let terminal = tab.view.read(cx);
        let title = self.terminal_name(index, cx);
        // 里面的程序已经结束了：名字变淡
        let running = terminal.is_running();
        let row = self.board_row(index, cx);
        // 程序和状态的图标、状态的字，和会话页的一样；还不在实时列表里的空着
        let (icon, state) = match &row {
            Some(row) => (
                TermOverview::term_icon(row, cx),
                Some((
                    TermOverview::state_text(row),
                    TermOverview::state_color(row, cx),
                )),
            ),
            None => (div().flex_none().w(px(14.)), None),
        };
        let bold = row.as_ref().is_some_and(|row| row.wants_look());
        let last_line = self.app.read(cx).settings.term_alerts.last_line;
        let second = row.as_ref().filter(|_| last_line).map(|row| {
            let text: SharedString = match row.last.trim() {
                "" => SharedString::default(),
                last => last.to_string().into(),
            };
            div()
                .w_full()
                .h(px(15.))
                .pl(px(52.))
                .pr(px(6.))
                .truncate()
                .font_family(self.app.read(cx).settings.terminal.font.clone())
                .text_size(px(11.))
                .line_height(px(15.))
                .text_color(muted)
                .child(text)
        });
        let height = if second.is_some() {
            TALL_ROW
        } else {
            ROW_HEIGHT
        };
        let close = div()
            .id(("terminal-end", index))
            .flex_none()
            .size(px(18.))
            .rounded(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .text_color(muted)
            .hover(|s| s.bg(close_hover).text_color(fg))
            .when(!selected, |close| {
                close
                    .invisible()
                    .group_hover("terminal-row", |s| s.visible())
            })
            .child(ui::icon_small(IconName::X))
            .tooltip(|window, cx| ui::tip(tr("结束此终端"), window, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.end_terminal(index, window, cx);
            }));
        // 序号那一格：勾选并排时（或者鼠标移上去时）换成勾
        let checked = self.terminal_picked(number);
        let picking = !self.terminals.picks.is_empty() || !self.terminals.split.is_empty();
        let pick_tip = match checked {
            true => tr("移出并排查看"),
            false => tr("加入并排查看（勾选 2～4 个）"),
        };
        let check = div()
            .id(("terminal-pick", index))
            .absolute()
            .top(px(2.))
            .left(px(2.))
            .size(px(14.))
            .rounded(px(4.))
            .border_1()
            .border_color(if checked { primary } else { muted })
            .bg(if checked { primary } else { bg })
            .flex()
            .items_center()
            .justify_center()
            .when(checked, |el| {
                el.child(
                    Icon::new(IconName::Check)
                        .size(px(10.))
                        .text_color(gpui_kit::white()),
                )
            })
            .when(!checked && !picking, |el| {
                el.invisible().group_hover("terminal-row", |s| s.visible())
            })
            .tooltip(move |window, cx| ui::tip(pick_tip, window, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.toggle_terminal_pick(number, window, cx);
            }));
        let badge = div()
            .flex_none()
            .size(px(18.))
            .relative()
            .child(
                div()
                    .size_full()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(border)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.))
                    .text_color(muted)
                    .when(checked || picking, |el| el.invisible())
                    .when(!checked && !picking, |el| {
                        el.group_hover("terminal-row", |s| s.invisible())
                    })
                    .child(number.to_string()),
            )
            .child(check);
        let name = match self.terminal_rename_box(number, cx) {
            Some(editor) => editor,
            None => ui::clipped(("terminal-title", index), title)
                .flex_1()
                .when(bold, |el| el.font_weight(FontWeight::SEMIBOLD))
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    if event.click_count() >= 2 {
                        cx.stop_propagation();
                        this.start_terminal_rename(index, window, cx);
                    }
                }))
                .into_any_element(),
        };
        let tail = state.map(|(text, color)| {
            div()
                .flex_none()
                .max_w(px(110.))
                .truncate()
                .text_size(px(11.))
                .text_color(color)
                .group_hover("terminal-row", |s| s.invisible())
                .child(text)
        });
        let tip = row.as_ref().map(|row| {
            tf!(
                "{name}（{state}）",
                name = row.label.clone(),
                state = TermOverview::state_text(row)
            )
        });
        let view = cx.entity().downgrade();
        let line = h_flex()
            .w_full()
            .h(px(ROW_HEIGHT - 2.))
            .gap(px(8.))
            .child(badge)
            .child(icon)
            .child(name)
            .children(tail)
            .child(close);
        h_flex()
            .id(("terminal-row", index))
            .group("terminal-row")
            .h(px(height))
            .w_full()
            .cursor_pointer()
            .on_click(
                cx.listener(move |this, _, window, cx| this.select_terminal(index, window, cx)),
            )
            .when_some(tip, |row, tip| {
                row.tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
            })
            .child(
                v_flex()
                    // 要有 id：悬停样式只有带 id 的元素才会在鼠标进出时重画，不然颜色要等别的事触发重画才变
                    .id("inner")
                    .h(px(height - 2.))
                    .flex_1()
                    .min_w_0()
                    .mx(px(6.))
                    .pl(px(6.))
                    .pr(px(5.))
                    .rounded(px(ui::RADIUS))
                    .text_size(px(13.))
                    .text_color(if running { fg } else { muted })
                    .when(selected, |row| row.bg(selected_bg))
                    .when(!selected, |row| row.hover(|s| s.bg(hover)))
                    .child(line)
                    .children(second),
            )
            .context_menu(terminal_menu(view, index))
            .into_any_element()
    }

    /// 侧栏收成图标栏时终端页的那几项：各终端的序号（点一下切过去，右键和侧栏里那一行一样，
    /// 程序已经结束的变淡），最后是「新终端」。
    pub(super) fn render_terminal_rail(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme();
        let (fg, muted, selected_bg, hover, border, accent) = (
            theme.sidebar_foreground,
            theme.muted_foreground,
            theme.sidebar_accent,
            theme.list_hover,
            theme.input,
            theme.primary,
        );
        let view = cx.entity().downgrade();
        let mut items: Vec<AnyElement> = self
            .terminals
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let selected = index == self.terminals.active;
                let terminal = tab.view.read(cx);
                let title: SharedString = terminal.label().unwrap_or_else(|| tr("终端").into());
                let running = terminal.is_running();
                div()
                    .id(("rail-term", index))
                    .flex_none()
                    .size(px(30.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(if selected {
                        accent.opacity(0.7)
                    } else {
                        border
                    })
                    .when(selected, |d| {
                        d.bg(selected_bg).font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(!selected, |d| d.hover(|s| s.bg(hover)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.))
                    .text_color(if running { fg } else { muted })
                    .cursor_pointer()
                    .child(tab.number.to_string())
                    .tooltip(move |window, cx| ui::tip(title.clone(), window, cx))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_terminal(index, window, cx)
                    }))
                    .context_menu(terminal_menu(view.clone(), index))
                    .into_any_element()
            })
            .collect();
        // 最后的 +：只有图标，点了弹新建终端的菜单（和侧栏的「新终端」不同，那里一下开默认运行的）
        let menu = self.presets_menu(cx);
        items.push(
            super::rail::rail_button("rail-term-new", IconName::Plus, false)
                .tooltip(tr("新建终端"))
                .dropdown_menu_with_anchor(Anchor::LeftCenter, menu)
                .into_any_element(),
        );
        items
    }

    /// 终端页的右边：上面一条工具栏，下面是选中的终端，占满；一个都没有时是空的样子。
    pub(super) fn render_terminal_page(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.terminals.split.len() >= term_split::MIN_CELLS {
            return self.render_terminal_split(cx);
        }
        let body = match self.active_terminal() {
            Some(view) => div()
                .flex_1()
                .min_h_0()
                .child(view.clone())
                .into_any_element(),
            None => self.render_no_terminal(cx),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(self.render_terminal_toolbar(cx))
            .child(body)
            .into_any_element()
    }

    /// 并排查看时的终端页：上面一条（并排查看、几个、退出并排），中间是格子，底下一条提示。
    fn render_terminal_split(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (bg, border, muted, chip_bg) = (
            theme.background,
            theme.border,
            theme.muted_foreground,
            theme.secondary,
        );
        let terms = &self.terminals;
        let focused = terms.tabs.get(terms.active).map(|tab| tab.number);
        let this = cx.entity().downgrade();
        let handler = |run: fn(&mut RepoView, u32, &mut Window, &mut Context<RepoView>),
                       number: u32|
         -> Handler {
            let this = this.clone();
            Rc::new(move |window, cx| {
                this.update(cx, |view, cx| run(view, number, window, cx))
                    .ok();
            })
        };
        let shown: Vec<u32> = match terms.zoom {
            Some(zoom) => vec![zoom],
            None => terms.split.clone(),
        };
        let cells: Vec<AnyElement> = shown
            .iter()
            .filter_map(|&number| {
                let index = terms.tabs.iter().position(|tab| tab.number == number)?;
                let row = self.board_row(index, cx);
                let head = term_split::Head {
                    name: self.terminal_name(index, cx),
                    place: tf!("终端 {n}", n = number.to_string()).into(),
                    state: row
                        .as_ref()
                        .map(TermOverview::state_text)
                        .unwrap_or_default(),
                    state_color: row
                        .as_ref()
                        .map(|row| TermOverview::state_color(row, cx))
                        .unwrap_or(muted),
                    dot: row
                        .as_ref()
                        .map(|row| TermOverview::state_color(row, cx))
                        .unwrap_or(muted),
                    focused: focused == Some(number),
                    zoomed: terms.zoom == Some(number),
                };
                let body = div()
                    .size_full()
                    .child(terms.tabs[index].view.clone())
                    .into_any_element();
                Some(
                    term_split::cell(
                        ("terminal-cell", number as usize),
                        head,
                        body,
                        handler(Self::focus_terminal_cell, number),
                        handler(Self::zoom_terminal_cell, number),
                        handler(Self::remove_terminal_cell, number),
                        cx,
                    )
                    .into_any_element(),
                )
            })
            .collect();
        let target = focused
            .and_then(|number| terms.tabs.iter().position(|tab| tab.number == number))
            .map(|index| self.terminal_name(index, cx));
        let (w, h) = (
            f32::from(self.viewport.width) - 280.,
            f32::from(self.viewport.height) - TOOLBAR_HEIGHT - 26. - 52.,
        );
        let size = match (shown.len(), terms.split.len()) {
            (1, _) => (w, h),
            (_, 2) => (w / 2., h),
            _ => (w / 2., h / 2.),
        };
        let bar = h_flex()
            .h(px(TOOLBAR_HEIGHT))
            .flex_none()
            .px(px(12.))
            .gap(px(8.))
            .bg(bg)
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
                    .child(tf!("{n} 个", n = terms.split.len().to_string())),
            )
            .child(div().flex_1())
            .child(
                term_split::add_button(
                    "terminal-split-add",
                    terms.split.len() >= term_split::MAX_CELLS,
                    terms
                        .tabs
                        .iter()
                        .any(|tab| !terms.split.contains(&tab.number)),
                )
                .dropdown_menu(terminal_split_menu(this.clone(), None)),
            )
            .child(
                Button::new("terminal-split-exit")
                    .small()
                    .icon(ui::icon(IconName::Square))
                    .label(tr("退出并排"))
                    .tooltip(tr("回到单个终端，显示焦点格中的终端"))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.exit_terminal_split(window, cx)),
                    ),
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

    /// 终端页的工具栏，和 Git 页的一样高：左边是选中的终端叫什么、开在哪，右边是发文件、重开。
    /// 结束在侧栏那一行上，这里不重复。
    fn render_terminal_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted, bg, chip) = (
            theme.border,
            theme.muted_foreground,
            theme.background,
            theme.secondary,
        );
        let place: SharedString = match &self.entry.server {
            Some(_) => tf!(
                "运行在 {place} 上，关闭 Kuai 后继续运行",
                place = self.place(cx)
            )
            .into(),
            // 退回到开在 Kuai 进程里的：退出 Kuai 就结束
            None if self
                .active_terminal()
                .is_some_and(|view| view.read(cx).in_process()) =>
            {
                tr("运行在本机，关闭标签页后继续运行").into()
            }
            None => tr("运行在本机，关闭 Kuai 后继续运行").into(),
        };
        let index = self.terminals.active;
        let title = self
            .active_terminal()
            .map(|_| self.terminal_name(index, cx));
        let shown = title.is_some();
        let view = cx.entity().downgrade();
        let can_split = self.terminals.tabs.len() >= term_split::MIN_CELLS;
        h_flex()
            .h(px(TOOLBAR_HEIGHT))
            .px(px(10.))
            .gap(px(2.))
            .flex_none()
            .bg(bg)
            .border_b_1()
            .border_color(border)
            .when_some(title, |bar, title| {
                bar.child(
                    h_flex()
                        .h(px(30.))
                        .min_w(px(30.))
                        .max_w(px(320.))
                        .flex_shrink(1.)
                        .mr(px(8.))
                        .px(px(10.))
                        .gap(px(6.))
                        .rounded(px(ui::RADIUS))
                        .bg(chip)
                        .overflow_hidden()
                        .child(
                            ui::icon(IconName::SquareTerminal)
                                .flex_none()
                                .text_color(muted),
                        )
                        .child(
                            ui::clipped("terminal-name", title)
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM),
                        ),
                )
            })
            .child(
                ui::clipped("terminal-place", place)
                    .flex_shrink(1.)
                    .text_xs()
                    .text_color(muted),
            )
            .child(div().flex_1().min_w(px(4.)))
            .when(shown, |bar| {
                bar.child(
                    tool_menu_button(
                        "terminal-split",
                        IconName::Columns2,
                        tr("并排"),
                        (!can_split).then(|| tr("没有其他终端可并排查看")),
                        cx,
                    )
                    .dropdown_menu(terminal_split_menu(view.clone(), None)),
                )
                .child(
                    tool_button(
                        "terminal-attach",
                        IconName::Paperclip,
                        tr("发送文件"),
                        tr("发送图片或文件（也可拖放，或按 Ctrl+V 粘贴）"),
                        false,
                        true,
                        0,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.attach_to_terminal(index, window, cx)
                    })),
                )
                .child(
                    tool_button(
                        "terminal-restart",
                        IconName::RotateCw,
                        tr("重启"),
                        tr("结束当前程序并重新启动"),
                        false,
                        true,
                        0,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.restart_terminal(index, window, cx)
                    })),
                )
            })
    }

    /// 终端页上一个终端都没有（还没开过，或者最后一个结束了）：不自己开，也不跳回 Git 页，
    /// 留在这里等用户开。点了「新终端」、还在连时写「正在连接…」。
    fn render_no_terminal(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, bg) = (theme.muted_foreground, theme.background);
        if self.terminals.asking > 0 {
            return v_flex()
                .flex_1()
                .min_h_0()
                .items_center()
                .justify_center()
                .gap_3()
                .bg(bg)
                .child(
                    Icon::new(IconName::SquareTerminal)
                        .size(px(28.))
                        .text_color(muted),
                )
                .child(div().text_color(muted).child(tr("正在连接…")))
                .into_any_element();
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(bg)
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
            .child(self.render_first_terminal(cx))
            .into_any_element()
    }

    /// 没有终端时的按钮（见 [`presets::first_buttons`]）：选了就在这个仓库里开。
    fn render_first_terminal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let on_start: presets::OnStart = Rc::new(move |start, window, cx| {
            view.update(cx, |this, cx| {
                this.new_terminal_with(String::new(), start, window, cx)
            })
            .ok();
        });
        let place = self.app.read(cx).repo_name(&self.entry);
        presets::first_buttons(place, on_start, cx)
    }
}
