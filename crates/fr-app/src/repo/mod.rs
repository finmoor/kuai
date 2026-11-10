//! 仓库标签页。
//!
//! 左边是侧栏（本地修改、所有提交、分支、远程分支、标签、贮藏），右边按侧栏选中的项显示
//! 本地修改（按文件、按块、按行暂存，提交）或提交历史（提交图、详情、改动）。
//! 本机仓库和服务器上的仓库共用这一套，经 [`Channel`] 执行 git。
//!
//! 什么时候刷新：连上以后、切到这个标签页、切回这个窗口、每次写操作之后，还有文件监听报告
//! 仓库有变化时（在编辑器里存了文件、在终端里提交了），都重读工作区状态、引用和贮藏；
//! 引用或 HEAD 变了才重读提交历史。

mod blame;
mod changes;
mod dialogs;
mod history;
mod menus;
mod ops;
mod sidebar;
mod stage_diff;

use fr_channel::Channel;
use fr_core::refs::{self, Refs};
use fr_core::state::{self, InProgress};
use fr_core::status::{self, Status};
use futures::StreamExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::settings::RepoEntry;

/// 右边显示什么
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Changes,
    History,
    /// 逐行追溯，盖在上面两个视图之上
    Blame,
}

/// 仓库页发给工作区的事件。
pub enum RepoEvent {
    /// 在新标签页里打开另一个仓库（子模块）
    Open(RepoEntry),
}

impl EventEmitter<RepoEvent> for RepoView {}

pub struct RepoView {
    entry: RepoEntry,
    app: Entity<AppState>,
    channel: Option<Channel>,
    connecting: bool,
    connect_task: Option<Task<()>>,
    /// 连接失败或读不了仓库的原因
    error: Option<SharedString>,
    mode: Mode,
    /// 是不是当前显示的标签页
    visible: bool,
    status: Option<Status>,
    /// 停在一半的合并、变基、挑拣或还原
    in_progress: Option<InProgress>,
    refs: Refs,
    /// 子模块的路径（相对仓库根）
    submodules: Vec<String>,
    blame: Option<blame::BlameView>,
    /// 上次读历史时的引用快照（所有分支、标签加 HEAD），变了才重读历史
    history_key: Option<Vec<u8>>,
    refresh_task: Option<Task<()>>,
    /// 正在刷新
    refreshing: bool,
    /// 刷新期间又收到了变化通知：这次刷完再刷一次
    refresh_again: bool,
    /// 文件监听；流结束（装不上监听、连接断了）后任务也结束
    watch_task: Option<Task<()>>,
    history: history::History,
    changes: changes::Changes,
    sidebar: sidebar::Sidebar,
    /// 正在执行的写操作；同一时间只做一个
    op: Option<SharedString>,
    op_task: Option<Task<()>>,
    /// 获取、拉取、推送的进度（git 标准错误的最后一行）；有它说明这个操作可以取消
    op_progress: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl RepoView {
    pub fn new(
        entry: RepoEntry,
        app: Entity<AppState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (changes, mut subscriptions) = changes::Changes::new(window, cx);
        // 切回这个窗口时刷新：用户多半刚在编辑器或终端里改过文件
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() && this.visible {
                this.refresh(cx);
            }
        }));
        let (history, history_subscriptions) = history::History::new(window, cx);
        subscriptions.extend(history_subscriptions);
        let mut this = Self {
            entry,
            app,
            channel: None,
            connecting: false,
            connect_task: None,
            error: None,
            mode: Mode::History,
            visible: false,
            status: None,
            in_progress: None,
            refs: Refs::default(),
            submodules: Vec::new(),
            blame: None,
            history_key: None,
            refresh_task: None,
            refreshing: false,
            refresh_again: false,
            watch_task: None,
            history,
            changes,
            sidebar: sidebar::Sidebar::default(),
            op: None,
            op_task: None,
            op_progress: None,
            _subscriptions: subscriptions,
        };
        // 测试开关：FR_TEST_VIEW=changes 打开仓库时直接显示本地修改
        if std::env::var("FR_TEST_VIEW").is_ok_and(|v| v == "changes") {
            this.show_changes(cx);
        }
        this.connect(cx);
        this
    }

    /// 取通道：本机仓库马上就有，服务器上的仓库要等连接。连上后读状态和历史。
    fn connect(&mut self, cx: &mut Context<Self>) {
        let channel = self.app.update(cx, |app, cx| {
            app.channel_for(self.entry.server.as_deref(), cx)
        });
        self.connecting = true;
        self.error = None;
        self.connect_task = Some(cx.spawn(async move |this, cx| {
            let result = channel.await;
            this.update(cx, |this, cx| {
                this.connecting = false;
                match result {
                    Ok(channel) => {
                        this.start_watch(&channel, cx);
                        this.channel = Some(channel);
                        this.refresh(cx);
                    }
                    Err(err) => this.error = Some(err),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// 监听仓库的变化，有变化就刷新。
    fn start_watch(&mut self, channel: &Channel, cx: &mut Context<Self>) {
        let mut changes = channel.watch(&self.entry.path);
        self.watch_task = Some(cx.spawn(async move |this, cx| {
            while changes.next().await.is_some() {
                if this
                    .update(cx, |this, cx| this.on_repo_changed(cx))
                    .is_err()
                {
                    return;
                }
            }
            log::info!("文件监听结束");
        }));
    }

    fn on_repo_changed(&mut self, cx: &mut Context<Self>) {
        // 不在前台的标签页不刷：切过来时会刷。正在做写操作也不刷：做完会刷
        if !self.visible || self.op.is_some() {
            return;
        }
        if self.refreshing {
            // 不打断正在进行的刷新：大仓库读状态慢，一直打断就永远刷不完
            self.refresh_again = true;
        } else {
            self.refresh(cx);
        }
    }

    /// 切换标签页时由工作区调用；切到这个标签页时刷新。
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        let shown = visible && !self.visible;
        self.visible = visible;
        if shown {
            self.refresh(cx);
        }
    }

    /// 重读工作区状态、引用和贮藏；引用或 HEAD 变了再重读历史。
    fn refresh(&mut self, cx: &mut Context<Self>) {
        // 服务器连接断了：重新连，连上后会再刷新
        if let Some(Channel::Remote(conn)) = &self.channel
            && !conn.is_alive()
        {
            self.channel = None;
        }
        let Some(channel) = self.channel.clone() else {
            if !self.connecting {
                self.connect(cx);
            }
            return;
        };
        let repo = self.entry.path.clone();
        let read_status = channel.git(&repo, status::args());
        let read_refs = channel.git(&repo, refs::args());
        let read_stashes = channel.git(&repo, refs::stash_args());
        let read_state = channel.git_with_input(&repo, state::args(), Some(state::stdin()));
        let read_submodules = channel.git(&repo, menus::submodule_args());
        self.refreshing = true;
        self.refresh_again = false;
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let (st, rf, sh, state, sm) = futures::join!(
                read_status,
                read_refs,
                read_stashes,
                read_state,
                read_submodules
            );
            this.update(cx, |this, cx| {
                this.refreshing = false;
                if std::mem::take(&mut this.refresh_again) {
                    let view = cx.entity().downgrade();
                    cx.defer(move |cx| {
                        view.update(cx, |this, cx| this.refresh(cx)).ok();
                    });
                }
                let parsed = match st.and_then(|out| out.ok()) {
                    Ok(out) => status::parse(&out),
                    Err(err) => {
                        this.error = Some(format!("读取仓库状态失败：{err:#}").into());
                        this.changes.settle();
                        cx.notify();
                        return;
                    }
                };
                this.error = None;
                this.in_progress = state.ok().and_then(|out| state::parse(&out.stdout));
                // 没有 .gitmodules 时这条命令失败，就是没有子模块
                this.submodules = sm
                    .ok()
                    .filter(|out| out.success())
                    .map(|out| menus::parse_submodules(&out.stdout))
                    .unwrap_or_default();
                let head = parsed.head.clone().unwrap_or_default();
                this.apply_status(parsed, cx);
                let refs_out = rf.and_then(|out| out.ok()).unwrap_or_default();
                let stash_out = sh.map(|out| out.stdout).unwrap_or_default();
                let old_refs =
                    std::mem::replace(&mut this.refs, refs::parse(&refs_out, &stash_out));
                this.remap_sidebar_selection(&old_refs);
                // 提交历史只含分支、远程分支、标签和 HEAD，这些都没变就不用重读
                let mut key = refs_out;
                key.extend_from_slice(head.as_bytes());
                if this.history_key.as_ref() != Some(&key) {
                    this.history_key = Some(key);
                    this.reload_history(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// 丢掉缓存，重读状态和提交历史（F5 和「刷新」按钮）。
    pub fn refresh_now(&mut self, cx: &mut Context<Self>) {
        self.history_key = None;
        self.refresh(cx);
    }

    /// 切到提交历史，把键盘焦点放到搜索框（Ctrl+F）。
    pub fn find_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_search(window, cx);
    }

    /// 仓库在哪：本机或服务器名。
    fn place(&self, cx: &App) -> String {
        match &self.entry.server {
            None => "本机".into(),
            Some(id) => self
                .app
                .read(cx)
                .server(id)
                .map_or_else(|| "?".into(), |s| s.name.clone()),
        }
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted, bar, danger) = (
            theme.border,
            theme.muted_foreground,
            theme.title_bar,
            theme.danger,
        );
        let ready = self.channel.is_some() && self.status.is_some();
        let idle = ready && self.op.is_none();
        let running = |label: &str| self.op.as_deref() == Some(label);
        let branch: SharedString = match &self.status {
            Some(st) => {
                let mut text = match (&st.branch, &st.head) {
                    (Some(branch), _) => branch.clone(),
                    (None, Some(head)) => format!("分离的 HEAD（{}）", &head[..head.len().min(8)]),
                    (None, None) => "还没有提交".into(),
                };
                if st.ahead > 0 {
                    text.push_str(&format!("  ↑{}", st.ahead));
                }
                if st.behind > 0 {
                    text.push_str(&format!("  ↓{}", st.behind));
                }
                text.into()
            }
            None => "…".into(),
        };
        let has_stash = !self.refs.stashes.is_empty();
        let place = self.place(cx);

        h_flex()
            .h(px(42.))
            .px_3()
            .gap_1()
            .flex_none()
            .bg(bar)
            .border_b_1()
            .border_color(border)
            .child(
                v_flex()
                    .min_w_0()
                    .max_w(px(420.))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.entry.name().to_string()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .truncate()
                            .child(format!("{place} · {}", self.entry.path)),
                    ),
            )
            .child(
                div()
                    .ml_3()
                    .px_2()
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .text_xs()
                    .child(branch),
            )
            .child(div().flex_1())
            .when_some(self.op.clone(), |bar, op| {
                let text = match &self.op_progress {
                    Some(progress) if !progress.is_empty() => format!("正在{op}：{progress}"),
                    _ => format!("正在{op}…"),
                };
                bar.child(
                    div()
                        .mr_1()
                        .max_w(px(420.))
                        .truncate()
                        .text_xs()
                        .text_color(muted)
                        .child(text),
                )
                // 只有联网的操作能取消：中途杀掉改暂存区的 git 会留下锁文件
                .when(self.op_progress.is_some(), |bar| {
                    bar.child(
                        Button::new("cancel-op")
                            .xsmall()
                            .label("取消")
                            .tooltip("中止这次操作")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_op(cx))),
                    )
                })
            })
            .when_some(self.error.clone().filter(|_| ready), |bar, err| {
                bar.child(
                    div()
                        .mr_2()
                        .max_w(px(360.))
                        .truncate()
                        .text_xs()
                        .text_color(danger)
                        .child(err),
                )
            })
            // 这三个按钮点右键有不常用的选项
            .child(
                div()
                    .id("fetch-menu")
                    .child(
                        tool_button(
                            "fetch",
                            "获取",
                            "git fetch --all；右键有更多选项",
                            running("获取"),
                            idle,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.fetch(window, cx))),
                    )
                    .context_menu(self.fetch_menu(cx)),
            )
            .child(
                div()
                    .id("pull-menu")
                    .child(
                        tool_button(
                            "pull",
                            "拉取",
                            "git pull；右键有更多选项",
                            running("拉取"),
                            idle,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.pull(window, cx))),
                    )
                    .context_menu(self.pull_menu(cx)),
            )
            .child(
                div()
                    .id("push-menu")
                    .child(
                        tool_button(
                            "push",
                            "推送",
                            "git push；右键有更多选项",
                            running("推送"),
                            idle,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.push(window, cx))),
                    )
                    .context_menu(self.push_menu(cx)),
            )
            .child(div().w(px(1.)).h(px(20.)).mx_1().bg(border))
            .child(
                tool_button("stash", "贮藏…", "git stash push", running("贮藏"), idle)
                    .on_click(cx.listener(|this, _, window, cx| this.stash_dialog(window, cx))),
            )
            .child(
                tool_button(
                    "pop",
                    "弹出贮藏",
                    "git stash pop：恢复最近一次贮藏",
                    running("弹出贮藏"),
                    idle && has_stash,
                )
                .on_click(cx.listener(|this, _, window, cx| this.stash_pop(None, window, cx))),
            )
            .child(
                tool_button(
                    "branch",
                    "新建分支…",
                    "git checkout -b",
                    running("新建分支"),
                    idle,
                )
                .on_click(cx.listener(|this, _, window, cx| this.new_branch_dialog(window, cx))),
            )
            .child(div().w(px(1.)).h(px(20.)).mx_1().bg(border))
            .child(self.render_open_button(cx))
            .child(
                Button::new("refresh")
                    .small()
                    .ghost()
                    .label("刷新")
                    .tooltip("重读状态和提交历史（F5）")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_now(cx))),
            )
    }

    /// 「打开」按钮：把仓库交给终端、VS Code 或资源管理器。
    fn render_open_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dir = self.entry.path.clone();
        let destination = self
            .entry
            .server
            .as_deref()
            .and_then(|id| self.app.read(cx).server(id))
            .map(|server| server.destination.clone());
        Button::new("open-in")
            .small()
            .ghost()
            .label("打开")
            .tooltip("在终端、VS Code 或资源管理器里打开这个仓库")
            .dropdown_menu(move |menu, _, _| {
                let (a, b, c) = (dir.clone(), dir.clone(), dir.clone());
                match destination.clone() {
                    // 服务器上的仓库：终端经 ssh 登录，VS Code 走 Remote-SSH
                    Some(dest) => {
                        let dest2 = dest.clone();
                        menu.item(
                            PopupMenuItem::new("在终端里打开（ssh）").on_click(move |_, _, _| {
                                crate::shell::open_terminal_ssh(&dest, &a)
                            }),
                        )
                        .item(
                            PopupMenuItem::new("在 VS Code 里打开（Remote-SSH）").on_click(
                                move |_, _, _| crate::shell::open_vscode_remote(&dest2, &b),
                            ),
                        )
                    }
                    None => menu
                        .item(
                            PopupMenuItem::new("在终端里打开")
                                .on_click(move |_, _, _| crate::shell::open_terminal(&a)),
                        )
                        .item(
                            PopupMenuItem::new("在 VS Code 里打开")
                                .on_click(move |_, _, _| crate::shell::open_vscode(&b)),
                        )
                        .item(
                            PopupMenuItem::new("在资源管理器里打开")
                                .on_click(move |_, _, _| crate::shell::open_folder(&c)),
                        ),
                }
            })
    }

    /// 合并、变基、挑拣、还原停在一半时的横幅：说明状态，给出继续、跳过、中止。
    fn render_in_progress(&self, state: InProgress, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, warning) = (theme.border, theme.warning);
        let conflicts = self.status.as_ref().map_or(0, |s| {
            s.entries
                .iter()
                .filter(|e| e.unstaged == Some(status::Kind::Conflicted))
                .count()
        });
        let idle = self.op.is_none();
        let text = if conflicts > 0 {
            format!(
                "{}停在一半：还有 {conflicts} 个文件有冲突。在「本地修改」里解决后点继续。",
                state.name()
            )
        } else {
            format!("{}停在一半：冲突都解决了，点继续。", state.name())
        };
        h_flex()
            .h(px(34.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(border)
            .bg(warning.opacity(0.15))
            .text_sm()
            .child(div().flex_1().min_w_0().truncate().child(text))
            .child(
                Button::new("op-continue")
                    .small()
                    .primary()
                    .label("继续")
                    .disabled(!idle || conflicts > 0)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.continue_in_progress(state, window, cx)
                    })),
            )
            .when(state != InProgress::Merge, |bar| {
                bar.child(
                    Button::new("op-skip")
                        .small()
                        .label("跳过这个提交")
                        .disabled(!idle)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.skip_in_progress(state, window, cx)
                        })),
                )
            })
            .child(
                Button::new("op-abort")
                    .small()
                    .label("中止")
                    .disabled(!idle)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.abort_in_progress(state, window, cx)
                    })),
            )
    }

    fn render_placeholder(
        &self,
        text: SharedString,
        error: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let color = if error {
            theme.danger
        } else {
            theme.muted_foreground
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .max_w(px(640.))
                    .text_sm()
                    .text_color(color)
                    .child(text),
            )
            .when(error, |d| {
                d.child(
                    Button::new("retry")
                        .small()
                        .label("重试")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.error = None;
                            this.history_key = None;
                            this.refresh(cx);
                        })),
                )
            })
            .into_any_element()
    }
}

fn tool_button(
    id: &'static str,
    label: &'static str,
    tooltip: &'static str,
    running: bool,
    enabled: bool,
) -> Button {
    Button::new(id)
        .small()
        .ghost()
        .label(label)
        .tooltip(tooltip)
        .loading(running)
        .disabled(!enabled)
}

impl Render for RepoView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tick_bench(window, cx);
        let main = if self.channel.is_none() {
            match self.error.clone() {
                Some(err) => self.render_placeholder(err, true, cx),
                None => {
                    let text = format!("正在连接 {}…", self.place(cx));
                    self.render_placeholder(text.into(), false, cx)
                }
            }
        } else if let (Some(err), None) = (self.error.clone(), &self.status) {
            self.render_placeholder(err, true, cx)
        } else if self.status.is_none() {
            self.render_placeholder("正在读取仓库状态…".into(), false, cx)
        } else {
            match self.mode {
                Mode::Changes => self.render_changes(cx),
                Mode::History => self.render_history(cx),
                Mode::Blame => self.render_blame(cx),
            }
        };
        let banner = self
            .in_progress
            .map(|state| self.render_in_progress(state, cx).into_any_element());
        v_flex()
            .size_full()
            .child(self.render_toolbar(cx))
            .children(banner)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(div().flex_1().min_w_0().h_full().child(main)),
            )
    }
}
