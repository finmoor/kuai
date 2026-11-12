//! 仓库页底下的终端面板：一个仓库可以开几个终端，面板上一个标签一个。
//!
//! 本机仓库的终端跟着仓库的标签页走，关了就结束。服务器上的终端开在 kuaid 里，各有各的会话
//! （名字见 [`crate::terminal::session_key`]）：关标签页、退出 Kuai 以后接着跑；再打开这个仓库时
//! 问一遍 kuaid，原来留着几个就接回几个。

use fr_channel::Channel;
use fr_core::i18n::tr;
use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{RepoView, layout};
use crate::terminal::{self, TerminalView};
use crate::ui;

/// 面板上的一个终端。
struct TermTab {
    /// 标签上的数字：这个仓库的第几个终端。关掉的不补位，新开的用最小的空号
    number: u32,
    view: Entity<TerminalView>,
    /// 里面的程序结束、断开时，工具栏按钮上的数字要跟着变
    _observe: Subscription,
}

/// 一个仓库的终端们。
#[derive(Default)]
pub(super) struct Terminals {
    /// 第一次打开面板时才建；之后收起面板也留着（里面的程序接着跑）
    tabs: Vec<TermTab>,
    /// 面板上显示的是哪一个
    active: usize,
    /// 面板开没开
    pub(super) open: bool,
    /// 服务器上留着、这次还没接上的终端：序号，里面的程序还在不在跑
    waiting: Vec<(u32, bool)>,
}

impl RepoView {
    /// 关掉这个标签页会不会结束终端里还在跑的程序（会的话关之前要问一下）。只有本机的终端会：
    /// 服务器上的终端开在 kuaid 里，标签页关了它接着跑，再打开这个仓库时接上。
    pub fn terminal_alive(&self, cx: &App) -> bool {
        self.terminals_alive(cx) > 0
    }

    /// 关掉这个标签页会结束几个终端里还在跑的程序。
    pub fn terminals_alive(&self, cx: &App) -> usize {
        let dies = |tab: &&TermTab| tab.view.read(cx).dies_with_view();
        self.terminals.tabs.iter().filter(dies).count()
    }

    /// 这个仓库有几个终端里有东西在跑：这次打开的，加上服务器上留着、还没接上的。
    pub(super) fn terminals_running(&self, cx: &App) -> usize {
        let opened = self
            .terminals
            .tabs
            .iter()
            .filter(|tab| tab.view.read(cx).is_running())
            .count();
        let waiting = self.terminals.waiting.iter().filter(|(_, alive)| *alive);
        opened + waiting.count()
    }

    /// 连上服务器以后问一下：kuaid 里留着这个仓库的哪几个终端（工具栏的按钮上标出个数，
    /// 打开面板时都接回来）。
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

    /// 加一个终端的标签，返回它在第几个。`restored`：它是服务器上留着的、里面的程序还在跑。
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
        let observe = cx.observe(&view, |_, _, cx| cx.notify());
        let tab = TermTab {
            number,
            view,
            _observe: observe,
        };
        // 按序号排着放
        let at = self
            .terminals
            .tabs
            .iter()
            .position(|other| other.number > number)
            .unwrap_or(self.terminals.tabs.len());
        self.terminals.tabs.insert(at, tab);
        at
    }

    /// 服务器上留着、还没接上的终端都加成标签（选中哪个才去接哪个）。面板上显示的还是原来那个。
    fn adopt_waiting(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminals.waiting.is_empty() {
            return;
        }
        let waiting = std::mem::take(&mut self.terminals.waiting);
        let shown = self
            .terminals
            .tabs
            .get(self.terminals.active)
            .map(|tab| tab.number);
        for (number, alive) in waiting {
            self.add_terminal(number, alive, window, cx);
        }
        // 新加的可能排在前面，显示的那个的位置跟着变
        let at = |shown| self.terminals.tabs.iter().position(|tab| tab.number == shown);
        self.terminals.active = shown.and_then(at).unwrap_or(0);
    }

    /// 画之前调用。面板已经开着、这时才知道服务器上还留着别的终端（连得慢，或者断了重连）：
    /// 补成标签。
    pub(super) fn sync_terminals(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminals.open {
            self.adopt_waiting(window, cx);
        }
    }

    /// 打开或收起终端的面板。打开时焦点放进终端；收起不结束里面的程序。
    pub fn show_terminal(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if open {
            // 服务器上留着的都接回来；一个都没有就开第一个
            self.adopt_waiting(window, cx);
            if self.terminals.tabs.is_empty() {
                self.terminals.active = self.add_terminal(1, false, window, cx);
            }
            self.focus_terminal(window, cx);
        } else if self.terminal_focused(window, cx) {
            // 焦点还留在看不见的终端上的话，键盘就没反应了
            window.blur(cx);
        }
        self.terminals.open = open;
        cx.notify();
    }

    /// 再开一个终端（Ctrl+Shift+`）。
    pub fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 服务器上留着的先接回来，新的排在它们后面
        self.adopt_waiting(window, cx);
        if self.terminals.tabs.is_empty() {
            // 还没开过：打开面板开的第一个就是新的
            return self.show_terminal(true, window, cx);
        }
        let used: Vec<u32> = self.terminals.tabs.iter().map(|tab| tab.number).collect();
        let number = (1..).find(|n| !used.contains(n)).unwrap_or(1);
        self.terminals.active = self.add_terminal(number, false, window, cx);
        self.terminals.open = true;
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

    /// Ctrl+`：没开就打开；开着但焦点不在里面就把焦点放进去；焦点在里面就收起。
    pub fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = !(self.terminals.open && self.terminal_focused(window, cx));
        self.show_terminal(open, window, cx);
    }

    /// 结束面板上显示的这个终端。还有别的就换过去，没有了就收起面板。
    fn end_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminals.active >= self.terminals.tabs.len() {
            return;
        }
        let tab = self.terminals.tabs.remove(self.terminals.active);
        // 服务器上的终端不说一声的话会接着跑
        tab.view.update(cx, |view, cx| view.kill(cx));
        if self.terminals.tabs.is_empty() {
            self.terminals.active = 0;
            self.show_terminal(false, window, cx);
            // 面板收起时焦点可能还在刚丢掉的终端上
            window.blur(cx);
        } else {
            self.terminals.active = self.terminals.active.min(self.terminals.tabs.len() - 1);
            self.focus_terminal(window, cx);
        }
        cx.notify();
    }

    /// 终端的面板：上面一条是各个终端的标签和几个按钮，下面是选中的那个终端。
    pub(super) fn render_terminal(&self, height: f32, cx: &mut Context<Self>) -> Option<AnyElement> {
        let active = self.active_terminal()?.clone();
        let theme = cx.theme();
        let (border, muted, fg) = (theme.border, theme.muted_foreground, theme.foreground);
        let (selected_bg, hover_bg) = (theme.secondary, theme.accent);
        let tab = |(index, tab): (usize, &TermTab)| {
            let selected = index == self.terminals.active;
            // 程序自己设了标题（多半是「用户@主机: 目录」，或者正在跑的程序的名字）就显示它
            let title = tab.view.read(cx).title();
            let title = title.unwrap_or_else(|| tr("终端").into());
            h_flex()
                .id(("terminal-tab", index))
                .flex_shrink(1.)
                .min_w(px(40.))
                .max_w(px(200.))
                .h(px(22.))
                .px(px(8.))
                .gap(px(6.))
                .rounded(px(ui::RADIUS))
                .text_xs()
                .cursor_pointer()
                .when(selected, |tab| tab.bg(selected_bg).text_color(fg))
                .when(!selected, |tab| {
                    tab.hover(|style| style.bg(hover_bg).text_color(fg))
                })
                .child(div().flex_none().child(tab.number.to_string()))
                .child(
                    ui::clipped(("terminal-title", index), title)
                        .flex_1()
                        .min_w_0(),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.select_terminal(index, window, cx);
                }))
        };
        let tabs: Vec<_> = self.terminals.tabs.iter().enumerate().map(tab).collect();
        let (for_attach, for_restart) = (active.clone(), active.clone());
        let panel = v_flex()
            .relative()
            .flex_none()
            .h(px(height))
            .border_t_1()
            .border_color(border)
            .child(self.split_handle(layout::Split::TerminalHeight, cx))
            .child(
                h_flex()
                    .flex_none()
                    .h(px(30.))
                    .pl(px(10.))
                    .pr(px(6.))
                    .gap(px(4.))
                    .text_color(muted)
                    .child(ui::icon(IconName::SquareTerminal).flex_none())
                    .child(
                        h_flex()
                            .flex_shrink(1.)
                            .min_w_0()
                            .gap(px(2.))
                            .pl(px(4.))
                            .overflow_hidden()
                            .children(tabs),
                    )
                    .child(
                        Button::new("terminal-new")
                            .ghost()
                            .xsmall()
                            .icon(ui::icon(IconName::Plus))
                            .tooltip(tr("再开一个终端（Ctrl+Shift+`）"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.new_terminal(window, cx);
                            })),
                    )
                    .child(div().flex_1().min_w(px(4.)))
                    .child(
                        Button::new("terminal-attach")
                            .ghost()
                            .xsmall()
                            .icon(ui::icon(IconName::Paperclip))
                            .tooltip(tr("发图片、文件（也可以拖进来，或者复制后按 Ctrl+V）"))
                            .on_click(move |_, _, cx| {
                                for_attach.update(cx, |terminal, cx| terminal.pick_files(cx));
                            }),
                    )
                    .child(
                        Button::new("terminal-restart")
                            .ghost()
                            .xsmall()
                            .icon(ui::icon(IconName::RotateCw))
                            .tooltip(tr("结束里面的程序，重新开一个"))
                            .on_click(move |_, _, cx| {
                                for_restart.update(cx, |terminal, cx| terminal.restart(cx));
                            }),
                    )
                    .child(
                        Button::new("terminal-end")
                            .ghost()
                            .xsmall()
                            .icon(ui::icon(IconName::Trash))
                            .tooltip(tr("结束这个终端"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.end_terminal(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("terminal-hide")
                            .ghost()
                            .xsmall()
                            .icon(ui::icon(IconName::ChevronDown))
                            .tooltip(tr("收起，里面的程序接着跑（Ctrl+`）"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_terminal(false, window, cx)
                            })),
                    ),
            )
            .child(div().flex_1().min_h_0().child(active));
        Some(panel.into_any_element())
    }
}
