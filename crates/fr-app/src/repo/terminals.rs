//! 仓库的终端页：一个仓库可以开几个终端。
//!
//! 每个仓库分 Git、终端两页（侧栏里仓库名下面的切换，或者 Ctrl+`），各记各的。终端页的侧栏里
//! 一个终端一行：新开、切换、结束都在这里，右键还有发文件、重开。右边上面一条工具栏，写着选中的
//! 终端叫什么、开在哪，右边是发文件、重开；下面是选中的终端，占满。只画选中的那个，没选中的
//! 照样收输出，只是不画、不跟着改大小。
//!
//! 终端不跟着标签页结束：
//!
//! - 本机仓库的终端在 Kuai 进程里。关标签页时，程序还在运行的终端交给 [`Parked`] 保管，
//!   再打开这个仓库、到终端页时接回来；退出 Kuai（包括重启以完成更新）时才结束。
//! - 服务器上的终端开在 kuaid 里，各有各的会话（名字见 [`crate::terminal::session_key`]）：
//!   关标签页、退出 Kuai 以后接着跑；再打开这个仓库时问一遍 kuaid，原来留着几个就接回几个。

use fr_channel::Channel;
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{Page, RepoView, TOOLBAR_HEIGHT, tool_button};
use crate::settings::RepoEntry;
use crate::terminal::{self, TerminalView};
use crate::ui;

/// 侧栏里一个终端一行的高度。
const ROW_HEIGHT: f32 = 28.;

/// 一个终端。
struct TermTab {
    /// 侧栏那一行前面的数字：这个仓库的第几个终端。关掉的不补位，新开的用最小的空号
    number: u32,
    view: Entity<TerminalView>,
    /// 里面的程序结束、断开时，切换上的数字要跟着变
    _observe: Subscription,
}

/// 一个仓库的终端们。
#[derive(Default)]
pub(super) struct Terminals {
    /// 第一次到终端页时才建；切回 Git 页也留着（里面的程序接着跑）
    tabs: Vec<TermTab>,
    /// 终端页上显示的是哪一个
    active: usize,
    /// 服务器上留着、这次还没接上的终端：序号，里面的程序还在不在跑
    waiting: Vec<(u32, bool)>,
}

/// 关掉的本机仓库标签页留下的终端，里面的程序接着运行。再打开这个仓库、到终端页时接回去
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
    for terminal in ended {
        terminal.view.update(cx, |view, cx| view.kill(cx));
    }
}

/// 最小的、还没用上的序号。
fn free_number(used: &[u32]) -> u32 {
    (1..).find(|n| !used.contains(n)).unwrap_or(1)
}

impl RepoView {
    /// 这个标签页里有几个本机终端的程序正在运行（退出 Kuai、重启以完成更新时将结束）。
    /// 服务器上的终端开在 kuaid 里，不算。
    pub fn local_terminals_running(&self, cx: &App) -> usize {
        if self.entry.server.is_some() {
            return 0;
        }
        self.terminals
            .tabs
            .iter()
            .filter(|tab| tab.view.read(cx).is_running())
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
        let waiting = self.terminals.waiting.iter().filter(|(_, alive)| *alive);
        opened + waiting.count() + parked_running(Some(&self.entry), cx)
    }

    /// 本机仓库的终端不跟着标签页结束：这个视图释放（标签页关掉）时，程序还在运行的终端交给
    /// [`Parked`] 保管；这个仓库有终端留下时，侧栏切换上的个数跟着变，到终端页时接回来。
    /// 建视图时调用。
    pub(super) fn keep_local_terminals(&mut self, cx: &mut Context<Self>) {
        if self.entry.server.is_some() {
            return;
        }
        cx.on_release(|this, cx| this.park_terminals(cx)).detach();
        let observe = cx.observe_global::<Parked>(|_, cx| cx.notify());
        self._subscriptions.push(observe);
    }

    /// 标签页关掉了：程序还在运行的终端交给 [`Parked`]。已经结束的不留，用户看到过了
    /// （kuaid 里的会话也是这样）。
    fn park_terminals(&mut self, cx: &mut App) {
        let tabs = std::mem::take(&mut self.terminals.tabs);
        let running: Vec<ParkedTerminal> = tabs
            .into_iter()
            .filter(|tab| tab.view.read(cx).is_running())
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
    fn adopt_parked(&mut self, cx: &mut Context<Self>) {
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
            self.insert_terminal(number, terminal.view, cx);
        }
    }

    /// 连上服务器以后问一下：kuaid 里留着这个仓库的哪几个终端（侧栏的切换上标出个数，
    /// 到终端页时都接回来）。
    pub(super) fn check_server_terminals(&mut self, channel: &Channel, cx: &mut Context<Self>) {
        let Channel::Remote(conn) = channel else {
            return;
        };
        let listing = conn.term_list();
        let repo = self.entry.path.clone();
        cx.spawn(async move |this, cx| {
            let Ok(terms) = listing.await else {
                return;
            };
            let mut found: Vec<(u32, bool)> = terms
                .iter()
                .filter_map(|term| Some((terminal::session_number(&repo, &term.key)?, term.alive)))
                .collect();
            found.sort();
            found.dedup_by_key(|(number, _)| *number);
            this.update(cx, |this, cx| {
                // 已经开着的不用再接
                let opened: Vec<u32> = this.terminals.tabs.iter().map(|tab| tab.number).collect();
                found.retain(|(number, _)| !opened.contains(number));
                this.terminals.waiting = found;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 加一个终端，返回它在第几个。`restored`：它是服务器上留着的、里面的程序还在跑。
    fn add_terminal(
        &mut self,
        number: u32,
        restored: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let (entry, app) = (self.entry.clone(), self.app.clone());
        let view = cx.new(|cx| {
            let mut view = TerminalView::new(
                move |cx| crate::shell::terminal_launch(&entry, &app, number, cx),
                window,
                cx,
            );
            if restored {
                view.mark_restored();
            }
            view
        });
        self.insert_terminal(number, view, cx)
    }

    /// 把终端按序号排进去，返回它在第几个。
    fn insert_terminal(
        &mut self,
        number: u32,
        view: Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) -> usize {
        let observe = cx.observe(&view, |_, _, cx| cx.notify());
        let tab = TermTab {
            number,
            view,
            _observe: observe,
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

    /// 留着、还没接上的终端都加进来：关掉的标签页留下的本机终端，服务器上留着的（选中哪个
    /// 才去接哪个）。显示的还是原来那个。
    fn adopt_waiting(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self
            .terminals
            .tabs
            .get(self.terminals.active)
            .map(|tab| tab.number);
        let before = self.terminals.tabs.len();
        self.adopt_parked(cx);
        for (number, alive) in std::mem::take(&mut self.terminals.waiting) {
            self.add_terminal(number, alive, window, cx);
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
        if self.page == Page::Terminal {
            self.adopt_waiting(window, cx);
        }
    }

    /// 切到 Git 页或终端页。到终端页时留着的都接回来，一个都没有就开第一个，焦点放进终端；
    /// 回 Git 页不结束里面的程序。
    pub(super) fn show_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        match page {
            Page::Terminal => {
                self.adopt_waiting(window, cx);
                if self.terminals.tabs.is_empty() {
                    self.terminals.active = self.add_terminal(1, false, window, cx);
                }
                self.focus_terminal(window, cx);
            }
            // 焦点还留在看不见的终端上的话，键盘就没反应了
            Page::Git if self.terminal_focused(window, cx) => window.blur(cx),
            Page::Git => {}
        }
        self.page = page;
        cx.notify();
    }

    /// Ctrl+`：在 Git、终端两页之间来回切。
    pub fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = match self.page {
            Page::Git => Page::Terminal,
            Page::Terminal => Page::Git,
        };
        self.show_page(page, window, cx);
    }

    /// 再开一个终端（侧栏的「新终端」、Ctrl+Shift+`），切到终端页显示它。
    pub fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 留着的先接回来，新的排在它们后面
        self.adopt_waiting(window, cx);
        let used: Vec<u32> = self.terminals.tabs.iter().map(|tab| tab.number).collect();
        let number = free_number(&used);
        self.terminals.active = self.add_terminal(number, false, window, cx);
        self.page = Page::Terminal;
        self.focus_terminal(window, cx);
        cx.notify();
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

    fn terminal_focused(&self, window: &Window, cx: &App) -> bool {
        self.active_terminal()
            .is_some_and(|view| view.focus_handle(cx).is_focused(window))
    }

    /// 结束第 `index` 个终端。显示的是它就换到旁边那个；一个都不剩时留在终端页，显示空的样子。
    fn end_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.terminals.tabs.len() {
            return;
        }
        let tab = self.terminals.tabs.remove(index);
        // 服务器上的终端不说一声的话会接着跑
        tab.view.update(cx, |view, cx| view.kill(cx));
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

    /// 结束第 `index` 个终端里的程序，重新开一个，并显示它。
    fn restart_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.terminals.tabs.get(index).map(|tab| tab.view.clone()) else {
            return;
        };
        self.select_terminal(index, window, cx);
        view.update(cx, |view, cx| view.restart(cx));
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
        // 和 Git 页的过滤框一样大、一样的边框，切页时下面的东西不跳
        let new_button = h_flex()
            .id("terminal-new")
            .h(px(24.))
            .px(px(8.))
            .gap(px(6.))
            .rounded(px(ui::RADIUS))
            .border_1()
            .border_color(border)
            .bg(bg)
            .text_size(px(13.))
            .text_color(fg)
            .cursor_pointer()
            .hover(|s| s.bg(hover))
            .child(ui::icon(IconName::Plus).flex_none().text_color(muted))
            .child(div().flex_1().min_w_0().truncate().child(tr("新终端")))
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child("Ctrl+Shift+`"),
            )
            .on_click(cx.listener(|this, _, window, cx| this.new_terminal(window, cx)));
        let rows: Vec<AnyElement> = (0..self.terminals.tabs.len())
            .map(|index| self.render_terminal_row(index, cx))
            .collect();
        let empty = rows.is_empty();
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
    }

    /// 侧栏里一个终端的一行：序号、名字（程序自己设的标题），选中的和鼠标停着的行尾有结束的 ×。
    /// 点一下切过去，右键有发文件、重开、结束。
    fn render_terminal_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, selected_bg, hover, border, close_hover) = (
            theme.sidebar_foreground,
            theme.muted_foreground,
            theme.sidebar_accent,
            theme.list_hover,
            theme.input,
            theme.secondary,
        );
        let tab = &self.terminals.tabs[index];
        let selected = index == self.terminals.active;
        let terminal = tab.view.read(cx);
        // 程序自己设了标题（多半是「用户@主机: 目录」，或者正在跑的程序的名字）就显示它
        let title: SharedString = terminal.title().unwrap_or_else(|| tr("终端").into());
        // 里面的程序已经结束了：名字变淡
        let running = terminal.is_running();
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
        let view = cx.entity().downgrade();
        h_flex()
            .id(("terminal-row", index))
            .group("terminal-row")
            .h(px(ROW_HEIGHT))
            .w_full()
            .cursor_pointer()
            .on_click(
                cx.listener(move |this, _, window, cx| this.select_terminal(index, window, cx)),
            )
            .child(
                h_flex()
                    .h(px(ROW_HEIGHT - 2.))
                    .flex_1()
                    .min_w_0()
                    .mx(px(6.))
                    .pl(px(6.))
                    .pr(px(5.))
                    .gap(px(8.))
                    .rounded(px(ui::RADIUS))
                    .text_size(px(13.))
                    .text_color(if running { fg } else { muted })
                    .when(selected, |row| row.bg(selected_bg))
                    .when(!selected, |row| row.hover(|s| s.bg(hover)))
                    .child(
                        div()
                            .flex_none()
                            .size(px(18.))
                            .rounded(px(4.))
                            .border_1()
                            .border_color(border)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(tab.number.to_string()),
                    )
                    .child(ui::clipped(("terminal-title", index), title).flex_1())
                    .child(close),
            )
            .context_menu(move |menu, _, _| {
                let (attach, restart, end) = (view.clone(), view.clone(), view.clone());
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
            })
            .into_any_element()
    }

    /// 终端页的右边：上面一条工具栏，下面是选中的终端，占满；一个都没有时是空的样子。
    pub(super) fn render_terminal_page(&self, cx: &mut Context<Self>) -> AnyElement {
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
                "运行在 {place} 上，关闭 Kuai 后仍会继续运行",
                place = self.place(cx)
            )
            .into(),
            None => tr("运行在本机，关闭标签页后仍会继续运行").into(),
        };
        let index = self.terminals.active;
        let title = self.active_terminal().map(|view| {
            let title: SharedString = view.read(cx).title().unwrap_or_else(|| tr("终端").into());
            title
        });
        let shown = title.is_some();
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
                    tool_button(
                        "terminal-attach",
                        IconName::Paperclip,
                        tr("发送文件"),
                        tr("发送图片或文件（也可拖放到此处，或复制后按 Ctrl+V）"),
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

    /// 终端页上一个终端都没有了（最后一个结束了）：不自己跳回 Git 页，留在这里等再开。
    fn render_no_terminal(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, bg) = (theme.muted_foreground, theme.background);
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
            .child(
                Button::new("terminal-first")
                    .primary()
                    .small()
                    .icon(ui::icon(IconName::Plus))
                    .label(tr("打开终端"))
                    .on_click(cx.listener(|this, _, window, cx| this.new_terminal(window, cx))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(tr("或按 Ctrl+Shift+`")),
            )
            .into_any_element()
    }
}
