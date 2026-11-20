//! 终端标签页（标题栏右边那一排，第二版定的 T3）：一个终端占满窗口。设计见 `design/server-workspace/`。
//!
//! 顶上一条：状态点、名字、哪个仓库在哪台、状态；右边发送文件、重启、打开仓库、⋯（在终端页中显示、
//! 结束终端…）。下面是终端的画面。
//!
//! 接法和终端页右边的画面一样（见 `TermOverview::sync`）：kuaid 里已经有的终端「只看」着接，焦点进去
//! 往里输入时把大小要过来（2.40）；这个标签页自己新开的（工作区页的「新建终端」）正经接着，大小跟着
//! 这里。不显示的时候不接着（画面丢掉），显示出来再接上。
//!
//! 关掉标签页不结束终端，它接着在 kuaid 里跑。终端不在了（那台电脑的终端列表读到了，里面没有它，
//! 等一会儿还是没有）就告诉主窗口关掉这个标签页；那台电脑没连着时留着，写「未连接」。

use std::time::Duration;

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::settings::{RepoEntry, TermSlot};
use crate::term_board::{Row, TermBoard, TermId, TermState};
use crate::term_overview::TermOverview;
use crate::terminal::TerminalView;
use crate::ui;

/// 列表里没有它以后等多久再关掉标签页：重启时列表里会空一下
const GONE_AFTER: Duration = Duration::from_secs(3);

pub enum TermTabEvent {
    /// 终端不在了（结束了，或者在别处关掉了）：关掉这个标签页
    Gone,
    /// 打开它所在的仓库
    OpenRepo(RepoEntry),
    /// 到终端页看它
    Reveal(TermSlot),
}

/// 显示着的画面。
struct Peek {
    /// 接着的是 kuaid 里的哪个终端；自己开的、还没出现在列表里时不知道
    id: Option<TermId>,
    view: Entity<TerminalView>,
    /// 这个标签页自己开的：正经接着，大小跟着这里。不是就是只看
    owned: bool,
}

pub struct TermTabView {
    app: Entity<AppState>,
    board: Entity<TermBoard>,
    slot: TermSlot,
    peek: Option<Peek>,
    /// 这个标签页正在开（新建、重启）这个终端：等它出现在列表里
    opening: bool,
    /// 重启：原来那个正在结束，结束了再开
    restarting: bool,
    visible: bool,
    /// 刚显示出来：画的时候接上画面
    pending_show: bool,
    /// 显示出来以后焦点还没放进画面里：画面可能要等那台电脑的终端列表到了才建得出来（启动时恢复的
    /// 标签页），建出来时再放
    want_focus: bool,
    /// 等着确认它是不是真的没了
    gone: Option<Task<()>>,
    _observe: Subscription,
}

impl EventEmitter<TermTabEvent> for TermTabView {}

impl TermTabView {
    /// `fresh`：这个标签页去开这个终端（新建终端）；不然是接上 kuaid 里已经有的。
    pub fn new(
        app: Entity<AppState>,
        board: Entity<TermBoard>,
        slot: TermSlot,
        fresh: bool,
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
            slot,
            peek: None,
            opening: fresh,
            restarting: false,
            visible: false,
            pending_show: false,
            want_focus: false,
            gone: None,
            _observe: observe,
        }
    }

    /// 列表里的它。那台电脑的终端列表还没读到、里面没有它时是 None。
    pub fn row(&self, cx: &App) -> Option<Row> {
        let rows = self.board.read(cx).terms_of(&self.slot.entry, cx)?;
        rows.into_iter().find(|row| row.number == self.slot.number)
    }

    /// 那台电脑的终端列表读到了，里面却没有它（也不是这里正在开、正在重启的）。
    fn missing(&self, cx: &App) -> bool {
        if self.opening || self.restarting {
            return false;
        }
        let rows = self.board.read(cx).terms_of(&self.slot.entry, cx);
        rows.is_some_and(|rows| rows.iter().all(|row| row.number != self.slot.number))
    }

    /// 显示、不显示（主窗口切到它、切走）。显示出来时下一次画的时候接上画面、焦点放进去：
    /// 点终端标签就是要往里输入。
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        self.pending_show = visible;
        self.want_focus = visible;
        if !visible {
            self.peek = None;
        }
        cx.notify();
    }

    /// 焦点在画面里：敲的字都归它。
    pub fn typing(&self, window: &Window, cx: &App) -> bool {
        self.peek
            .as_ref()
            .is_some_and(|peek| peek.view.focus_handle(cx).is_focused(window))
    }

    /// 焦点放进画面里。
    pub fn focus_terminal(&self, window: &mut Window, cx: &mut App) {
        if let Some(peek) = &self.peek {
            peek.view.focus_handle(cx).focus(window, cx);
        }
    }

    /// 让画面跟上：列表里出现了（自己开的）、改了大小、别处重启了就重新接；它没了等一会儿再看，
    /// 还是没有就关掉标签页。终端列表有变化、显示出来时都来一遍。
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.missing(cx) && self.gone.is_none() {
            self.gone = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(GONE_AFTER).await;
                this.update(cx, |this, cx| {
                    this.gone = None;
                    if this.missing(cx) {
                        log::info!(
                            "终端标签页的终端不在了：{} 的第 {} 个",
                            this.slot.entry.path,
                            this.slot.number
                        );
                        cx.emit(TermTabEvent::Gone);
                    }
                })
                .ok();
            }));
        }
        if !self.visible || self.restarting {
            return;
        }
        let typing = self.typing(window, cx);
        let Some(row) = self.row(cx) else {
            // 这里正在开、列表里还没有：自己开的那个画面留着（没有就建），它负责去开
            if self.opening {
                if !self.peek.as_ref().is_some_and(|peek| peek.owned) {
                    self.open_view(window, cx);
                }
            } else {
                self.peek = None;
            }
            return;
        };
        if self.opening {
            if let Some(peek) = self.peek.as_mut().filter(|peek| peek.owned) {
                peek.id = Some(row.id.clone());
            }
            self.opening = false;
        }
        let Some(conn) = self.board.read(cx).conn(&row.id.machine) else {
            self.peek = None;
            return;
        };
        let size = Some((row.cols.max(2), row.rows.max(1)));
        let current = self.peek.as_ref().is_some_and(|peek| {
            peek.id.as_ref() == Some(&row.id)
                // 自己开的管着它的大小，不用跟
                && (peek.owned || peek.view.read(cx).size() == size)
        });
        if current {
            return;
        }
        let view =
            cx.new(|cx| TerminalView::watch(&conn, &row.id.term, row.cols, row.rows, window, cx));
        // 正往里输入时它改了大小、被别处重启了：换上新建的那个，焦点跟过去（刚显示出来、还没放过
        // 焦点的，画的时候放，见 `render`）
        if typing {
            view.focus_handle(cx).focus(window, cx);
        }
        self.peek = Some(Peek {
            id: Some(row.id),
            view,
            owned: false,
        });
    }

    /// 建一个正经接着的画面：kuaid 里没有这个名字的终端就开一个。
    fn open_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (entry, app, number) = (self.slot.entry.clone(), self.app.clone(), self.slot.number);
        let view = cx.new(|cx| {
            TerminalView::new(
                move |cx| {
                    let mut launch = crate::shell::terminal_launch(&entry, &app, number, "", cx)?;
                    // 退回到开在 Kuai 进程里的终端跟着画面走，切走就结束了：不退
                    launch.fallback = None;
                    Ok(launch)
                },
                window,
                cx,
            )
        });
        view.focus_handle(cx).focus(window, cx);
        self.want_focus = false;
        self.peek = Some(Peek {
            id: None,
            view,
            owned: true,
        });
    }

    fn name(&self, row: Option<&Row>) -> SharedString {
        match row {
            Some(row) => TermOverview::term_name(row),
            None => tf!("终端 {n}", n = self.slot.number.to_string()).into(),
        }
    }

    /// 里面还在输出的，结束、重启前先问一句（和终端页一样）。
    fn ask_first(&mut self, restart: bool, window: &mut Window, cx: &mut Context<Self>) {
        let row = self.row(cx);
        let busy = row
            .as_ref()
            .is_some_and(|row| row.alive && row.state == TermState::Running);
        if !busy {
            match restart {
                true => self.restart_now(window, cx),
                false => self.end_now(cx),
            }
            return;
        }
        let repo = self.app.read(cx).repo_name(&self.slot.entry);
        let name = self.name(row.as_ref());
        let (title, text, ok) = match restart {
            true => (
                tr("重启终端？"),
                tf!(
                    "将终止 {repo} 中的终端「{name}」里正在运行的程序，并重新启动。此操作无法撤销。",
                    repo,
                    name
                ),
                tr("重启"),
            ),
            false => (
                tr("结束终端？"),
                tf!(
                    "将结束 {repo} 中的终端「{name}」，其中正在运行的程序将终止。此操作无法撤销。",
                    repo,
                    name
                ),
                tr("结束"),
            ),
        };
        let this = cx.entity().downgrade();
        crate::manager::confirm_in(
            title.to_string(),
            text,
            ok,
            true,
            window,
            cx,
            move |window, cx| {
                this.update(cx, |this, cx| match restart {
                    true => this.restart_now(window, cx),
                    false => this.end_now(cx),
                })
                .ok();
            },
        );
    }

    /// 结束终端（标签页右键菜单里的「结束终端…」）：里面还在输出的先问。
    pub fn end_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_first(false, window, cx);
    }

    /// 请 kuaid 结束这个终端（列表里没有它时什么也不做）。
    fn close_term(&self, cx: &mut Context<Self>) -> Option<Task<()>> {
        let row = self.row(cx)?;
        let conn = self.board.read(cx).conn(&row.id.machine)?;
        let closing = conn.term_close(&row.id.term);
        Some(cx.background_executor().spawn(async move {
            if let Err(err) = closing.await {
                log::warn!("结束终端失败：{err:#}");
            }
        }))
    }

    /// 结束终端，关掉这个标签页。
    fn end_now(&mut self, cx: &mut Context<Self>) {
        if let Some(closing) = self.close_term(cx) {
            closing.detach();
        }
        self.peek = None;
        cx.emit(TermTabEvent::Gone);
    }

    /// 结束里面的程序，重新开一个。原来那个结束了再开，不然接上的还是它。
    fn restart_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(closing) = self.close_term(cx) else {
            return;
        };
        self.restarting = true;
        self.peek = None;
        cx.spawn_in(window, async move |this, cx| {
            closing.await;
            this.update_in(cx, |this, window, cx| {
                this.restarting = false;
                this.opening = true;
                if this.visible {
                    this.open_view(window, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// 选图片、文件发进去。
    fn attach(&mut self, cx: &mut Context<Self>) {
        if let Some(peek) = &self.peek {
            peek.view.update(cx, |view, cx| view.pick_files(cx));
        }
    }

    /// 顶上一条：状态点、名字、哪个仓库在哪台、状态；右边的按钮。
    fn render_bar(&self, row: Option<&Row>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted, fg) = (theme.border, theme.muted_foreground, theme.foreground);
        let app = self.app.read(cx);
        let place = format!(
            "{} · {}",
            app.repo_name(&self.slot.entry),
            TermOverview::place(app, &self.slot.entry)
        );
        let connected = app.writable(crate::machines::machine_of(&self.slot.entry));
        let listed = self.board.read(cx).terms_of(&self.slot.entry, cx).is_some();
        let state: SharedString = match row {
            Some(row) => {
                // 停下、空闲了多久
                let since = row
                    .last_output
                    .filter(|_| row.alive && row.state != TermState::Running)
                    .map(|at| format!(" · {}", crate::term_overview::ago(at)))
                    .unwrap_or_default();
                format!("{}{since}", TermOverview::state_text(row)).into()
            }
            None if self.opening || self.restarting => tr("正在启动…").into(),
            None if !listed && connected => tr("正在连接…").into(),
            None if !listed => tr("未连接").into(),
            None => tr("已结束").into(),
        };
        let dot = match row {
            Some(row) => {
                TermOverview::dot(row.state, 8., cx).when(!row.alive, |dot| dot.opacity(0.4))
            }
            None => TermOverview::dot(TermState::Idle, 8., cx).opacity(0.4),
        };
        let live = self.peek.is_some();
        let can_restart = row.is_some() && !self.restarting;
        let (this, reveal, repo) = (
            cx.entity().downgrade(),
            self.slot.clone(),
            self.slot.entry.clone(),
        );
        h_flex()
            .h(px(40.))
            .flex_none()
            .px(px(12.))
            .gap(px(8.))
            .border_b_1()
            .border_color(border)
            .child(dot)
            .child(
                ui::clipped("term-tab-name", self.name(row))
                    .flex_shrink(1.)
                    .text_size(px(13.))
                    .font_family("Consolas")
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(fg),
            )
            .child(
                ui::clipped("term-tab-place", place)
                    .flex_shrink(4.)
                    .text_xs()
                    .text_color(muted),
            )
            .child(div().flex_none().text_xs().text_color(muted).child(state))
            .child(div().flex_1().min_w(px(8.)))
            .child(
                Button::new("term-tab-attach")
                    .ghost()
                    .small()
                    .disabled(!live)
                    .icon(ui::icon(IconName::Paperclip))
                    .label(tr("发送文件"))
                    .tooltip(tr("发送图片或文件（也可拖放，或按 Ctrl+V 粘贴）"))
                    .on_click(cx.listener(|this, _, _, cx| this.attach(cx))),
            )
            .child(
                Button::new("term-tab-restart")
                    .ghost()
                    .small()
                    .disabled(!can_restart)
                    .icon(ui::icon(IconName::RotateCw))
                    .label(tr("重启"))
                    .tooltip(tr("结束当前程序并重新启动"))
                    .on_click(cx.listener(|this, _, window, cx| this.ask_first(true, window, cx))),
            )
            .child(
                Button::new("term-tab-repo")
                    .ghost()
                    .small()
                    .icon(ui::icon(IconName::FolderGit2))
                    .label(tr("打开仓库"))
                    .tooltip(tr("在标签页中打开此终端所在的仓库"))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(TermTabEvent::OpenRepo(repo.clone()))
                    })),
            )
            .child(
                Button::new("term-tab-more")
                    .ghost()
                    .small()
                    .icon(ui::icon(IconName::Ellipsis))
                    .dropdown_menu(move |menu, _, _| {
                        let (show, end) = (this.clone(), this.clone());
                        let reveal = reveal.clone();
                        menu.item(PopupMenuItem::new(tr("在终端页中显示")).on_click(
                            move |_, _, cx| {
                                show.update(cx, |_, cx| {
                                    cx.emit(TermTabEvent::Reveal(reveal.clone()))
                                })
                                .ok();
                            },
                        ))
                        .separator()
                        .item(
                            PopupMenuItem::new(tr("结束终端…"))
                                .disabled(!can_restart)
                                .on_click(move |_, window, cx| {
                                    end.update(cx, |this, cx| this.ask_first(false, window, cx))
                                        .ok();
                                }),
                        )
                    }),
            )
    }
}

impl Render for TermTabView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.pending_show {
            self.pending_show = false;
            self.sync(window, cx);
        }
        // 画面已经在了（切回这个标签页时，sync 不用重建）：焦点等这一轮画完再放进去
        if self.want_focus && self.peek.is_some() {
            self.want_focus = false;
            cx.defer_in(window, |this, window, cx| this.focus_terminal(window, cx));
        }
        let theme = cx.theme();
        let (bg, muted, primary) = (theme.background, theme.muted_foreground, theme.primary);
        let row = self.row(cx);
        let typing = self.typing(window, cx);
        let body = match &self.peek {
            Some(peek) => div()
                .relative()
                .flex_1()
                .min_h_0()
                // 点进终端里焦点就换了（终端自己接的）：主窗口跟着重画，标题栏上「看没看过」才跟得上
                .on_any_mouse_down(cx.listener(|_, _, _, cx| cx.notify()))
                .child(peek.view.clone())
                .when(typing, |el| {
                    el.child(
                        div()
                            .absolute()
                            .inset_0()
                            .border_1()
                            .border_color(primary.opacity(0.7)),
                    )
                })
                .into_any_element(),
            None => {
                let machine = crate::machines::machine_of(&self.slot.entry).to_string();
                let connected = self.app.read(cx).writable(&machine);
                let listed = self.board.read(cx).terms_of(&self.slot.entry, cx).is_some();
                let text = if self.opening || self.restarting {
                    tr("正在启动终端…")
                } else if row.is_some() || (!listed && connected) {
                    tr("正在连接…")
                } else if !listed {
                    tr("尚未连接此服务器，连接后显示此终端。")
                } else {
                    tr("此终端已结束。")
                };
                // 那台服务器没连着（手动连接的、刚才没连上的）：给一个「连接」
                let connect = (!listed && !connected).then(|| {
                    Button::new("term-tab-connect")
                        .small()
                        .primary()
                        .icon(ui::icon(IconName::Plug))
                        .label(tr("连接"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.app
                                .update(cx, |app, cx| drop(app.connect(&machine, cx)));
                        }))
                });
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .text_color(muted)
                    .child(Icon::new(IconName::SquareTerminal).size(px(28.)))
                    .child(div().text_size(px(13.)).child(text))
                    .children(connect)
                    .into_any_element()
            }
        };
        v_flex()
            .id("term-tab")
            .size_full()
            .bg(bg)
            .child(self.render_bar(row.as_ref(), cx))
            .child(body)
    }
}
