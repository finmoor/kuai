//! 仓库标签页。
//!
//! 左边是侧栏（仓库的名字、本地修改、所有提交、分支、远程分支、标签、贮藏），从上到下占满；
//! 右边上面是工具栏（当前分支、获取、拉取、推送、贮藏……），下面按侧栏选中的项显示
//! 本地修改（按文件、按块、按行暂存，提交）或提交历史（提交图、详情、改动）。
//! 本机仓库和服务器上的仓库共用这一套，经 [`Channel`] 执行 git。
//!
//! 什么时候刷新：连上以后、切到这个标签页、切回这个窗口、每次写操作之后，还有文件监听报告
//! 仓库有变化时（在编辑器里存了文件、在终端里提交了），都重读工作区状态、引用和贮藏；
//! 引用或 HEAD 变了才重读提交历史。

mod blame;
mod changes;
mod dialogs;
mod file_picker;
mod history;
mod menus;
mod ops;
mod sidebar;
mod stage_diff;

pub(crate) use ops::explain;

use fr_channel::Channel;
use fr_core::i18n::tr;
use fr_core::refs::{self, Refs};
use fr_core::state::{self, InProgress};
use fr_core::status::{self, Status};
use fr_core::tf;
use futures::StreamExt as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::settings::RepoEntry;
use crate::ui;

/// 工具栏的高度；侧栏顶上写仓库名字的那一块和它一样高，底边对得齐。
const TOOLBAR_HEIGHT: f32 = 52.;

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
    /// 配置里的远程（`git remote`）。刚添加、还没获取到分支的远程，远程分支里看不到，这里有
    remote_names: Vec<String>,
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
    /// 获取、拉取、推送的进度（git 标准错误的最后一行）；有它说明正在做的这一步可以取消
    op_progress: Option<SharedString>,
    /// 往这里发一下，正在做的联网操作就停下
    op_cancel: Option<futures::channel::oneshot::Sender<()>>,
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
        let (sidebar, sidebar_subscription) = sidebar::Sidebar::new(window, cx);
        subscriptions.push(sidebar_subscription);
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
            remote_names: Vec::new(),
            submodules: Vec::new(),
            blame: None,
            history_key: None,
            refresh_task: None,
            refreshing: false,
            refresh_again: false,
            watch_task: None,
            history,
            changes,
            sidebar,
            op: None,
            op_task: None,
            op_progress: None,
            op_cancel: None,
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
        let read_remotes = channel.git(&repo, vec!["remote".into()]);
        self.refreshing = true;
        self.refresh_again = false;
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let (st, rf, sh, state, sm, rm) = futures::join!(
                read_status,
                read_refs,
                read_stashes,
                read_state,
                read_submodules,
                read_remotes
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
                        this.error =
                            Some(tf!("读取仓库状态失败：{err}", err = format!("{:#}", err)).into());
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
                this.remote_names = rm
                    .ok()
                    .filter(|out| out.success())
                    .map(|out| {
                        String::from_utf8_lossy(&out.stdout)
                            .lines()
                            .map(|line| line.trim().to_string())
                            .filter(|line| !line.is_empty())
                            .collect()
                    })
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

    /// 换了界面语言时由主窗口调用。大部分文字每次重画都重新取，自己会变；
    /// 输入框里的提示文字、存在状态里的几句话是建的时候定的，这里重新设一遍。
    pub fn language_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.retranslate_sidebar(window, cx);
        self.retranslate_history(window, cx);
        self.retranslate_changes(window, cx);
        cx.notify();
    }

    /// 工作区里有几个文件有没提交的改动（标签页上的小点用）。
    pub fn change_count(&self) -> usize {
        self.status.as_ref().map_or(0, |s| s.entries.len())
    }

    /// 切到提交历史，把键盘焦点放到搜索框（Ctrl+F）。
    pub fn find_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_search(window, cx);
    }

    /// 仓库在哪：本机或服务器名。
    fn place(&self, cx: &App) -> String {
        match &self.entry.server {
            None => tr("本机").into(),
            Some(id) => self
                .app
                .read(cx)
                .server(id)
                .map_or_else(|| "?".into(), |s| s.name.clone()),
        }
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted, bg, danger, accent, chip) = (
            theme.border,
            theme.muted_foreground,
            theme.background,
            theme.danger,
            theme.primary,
            theme.secondary,
        );
        let ready = self.channel.is_some() && self.status.is_some();
        let idle = ready && self.op.is_none();
        let running = |label: &str| self.op.as_deref() == Some(label);
        // 当前分支；没有分支（分离的 HEAD、空仓库）时写清楚是哪种情况
        let (branch, on_branch): (SharedString, bool) = match &self.status {
            Some(st) => match (&st.branch, &st.head) {
                (Some(branch), _) => (branch.clone().into(), true),
                (None, Some(head)) => (
                    tf!("分离的 HEAD（{head}）", head = &head[..head.len().min(8)]).into(),
                    false,
                ),
                (None, None) => (tr("还没有提交").into(), false),
            },
            None => ("…".into(), false),
        };
        let (ahead, behind) = self
            .status
            .as_ref()
            .map_or((0, 0), |st| (st.ahead as usize, st.behind as usize));
        let has_stash = !self.refs.stashes.is_empty();
        let has_changes = self
            .status
            .as_ref()
            .is_some_and(|st| !st.entries.is_empty());
        let separator = || div().flex_none().w(px(1.)).h(px(24.)).mx(px(6.)).bg(border);

        h_flex()
            .h(px(TOOLBAR_HEIGHT))
            .px(px(10.))
            .gap(px(2.))
            .flex_none()
            .bg(bg)
            .border_b_1()
            .border_color(border)
            // 当前分支。窗口窄、右边又在显示进度时它先让地方，所以可以缩
            .child(
                h_flex()
                    .id("current-branch")
                    .h(px(30.))
                    .min_w(px(30.))
                    .flex_shrink(1.)
                    .px(px(10.))
                    .gap(px(6.))
                    .rounded(px(ui::RADIUS))
                    .bg(chip)
                    .overflow_hidden()
                    .child(
                        ui::icon(IconName::GitBranch)
                            .flex_none()
                            .text_color(if on_branch { accent } else { muted }),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(branch),
                    )
                    .when(ahead > 0, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("↑{ahead}")),
                        )
                    })
                    .when(behind > 0, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("↓{behind}")),
                        )
                    })
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tr(
                            "当前分支。↑ 是比上游多的提交（还没推送），↓ 是比上游少的（还没拉取）",
                        ))
                        .build(window, cx)
                    }),
            )
            .child(div().flex_1().min_w(px(4.)))
            .when_some(self.op.clone(), |bar, op| {
                let text = match &self.op_progress {
                    Some(progress) if !progress.is_empty() => {
                        tf!("正在{op}：{progress}", op = op, progress = progress)
                    }
                    _ => tf!("正在{op}…", op = op),
                };
                // 不让它缩：窗口窄的时候缩的是左边的分支名，不然这里只剩「正在…」两个字
                bar.child(
                    div()
                        .mr_1()
                        .flex_none()
                        .max_w(px(240.))
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
                            .label(tr("取消"))
                            .tooltip(tr("中止这次操作"))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_op(cx))),
                    )
                })
            })
            .when_some(self.error.clone().filter(|_| ready), |bar, err| {
                bar.child(
                    div()
                        .mr_2()
                        .min_w_0()
                        .max_w(px(320.))
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
                    .flex_none()
                    .child(
                        tool_button(
                            "fetch",
                            IconName::ArrowDownToDot,
                            tr("获取"),
                            tr("git fetch --all；右键有更多选项"),
                            running(tr("获取")),
                            idle,
                            0,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.fetch(window, cx))),
                    )
                    .context_menu(self.fetch_menu(cx)),
            )
            .child(
                div()
                    .id("pull-menu")
                    .flex_none()
                    .child(
                        tool_button(
                            "pull",
                            IconName::ArrowDownToLine,
                            tr("拉取"),
                            tr("git pull；右键有更多选项"),
                            running(tr("拉取")),
                            idle,
                            behind,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.pull(false, window, cx))),
                    )
                    .context_menu(self.pull_menu(cx)),
            )
            .child(
                div()
                    .id("push-menu")
                    .flex_none()
                    .child(
                        tool_button(
                            "push",
                            IconName::ArrowUpFromLine,
                            tr("推送"),
                            tr("git push；右键有更多选项"),
                            running(tr("推送")),
                            idle,
                            ahead,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.push(window, cx))),
                    )
                    .context_menu(self.push_menu(cx)),
            )
            .child(separator())
            .child(
                tool_button(
                    "stash",
                    IconName::Archive,
                    tr("贮藏"),
                    if has_changes {
                        tr("git stash push：把现在的改动收起来")
                    } else {
                        tr("工作区是干净的，没有可以贮藏的改动")
                    },
                    running(tr("贮藏")),
                    idle && has_changes,
                    0,
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| this.stash_dialog(window, cx))),
            )
            .child(
                tool_button(
                    "pop",
                    IconName::ArchiveRestore,
                    tr("弹出"),
                    tr("git stash pop：恢复最近一次贮藏，并把它从贮藏列表里删除（会先问一下）"),
                    running(tr("弹出贮藏")),
                    idle && has_stash,
                    0,
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| this.stash_pop_latest(window, cx))),
            )
            .child(
                tool_button(
                    "branch",
                    IconName::GitBranchPlus,
                    tr("分支"),
                    tr("新建分支（git checkout -b）"),
                    running(tr("新建分支")),
                    idle,
                    0,
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| this.new_branch_dialog(window, cx))),
            )
            .child(separator())
            .child(self.render_open_button(cx))
            .child(
                tool_button(
                    "refresh",
                    IconName::RotateCw,
                    tr("刷新"),
                    tr("重读状态和提交历史（F5）"),
                    self.refreshing && self.op.is_none(),
                    true,
                    0,
                    cx,
                )
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
        // 不加悬停提示：提示也出现在按钮下面，会把弹出的菜单第一项盖住
        tool_button_bare(
            "open-in",
            IconName::SquareArrowOutUpRight,
            tr("打开"),
            false,
            0,
            cx,
        )
        .dropdown_menu(move |menu, _, _| {
            let (a, b, c) = (dir.clone(), dir.clone(), dir.clone());
            match destination.clone() {
                // 服务器上的仓库：终端经 ssh 登录，VS Code 走 Remote-SSH
                Some(dest) => {
                    let dest2 = dest.clone();
                    menu.item(PopupMenuItem::new(tr("在终端里打开（ssh）")).on_click(
                        move |_, window, cx| {
                            let result = crate::shell::open_terminal_ssh(&dest, &a);
                            crate::shell::report(result, window, cx);
                        },
                    ))
                    .item(
                        PopupMenuItem::new(tr("在 VS Code 里打开（Remote-SSH）")).on_click(
                            move |_, window, cx| {
                                let result = crate::shell::open_vscode_remote(&dest2, &b);
                                crate::shell::report(result, window, cx);
                            },
                        ),
                    )
                }
                None => menu
                    .item(
                        PopupMenuItem::new(tr("在终端里打开")).on_click(move |_, window, cx| {
                            crate::shell::report(crate::shell::open_terminal(&a), window, cx)
                        }),
                    )
                    .item(PopupMenuItem::new(tr("在 VS Code 里打开")).on_click(
                        move |_, window, cx| {
                            crate::shell::report(crate::shell::open_vscode(&b), window, cx)
                        },
                    ))
                    .item(PopupMenuItem::new(tr("在资源管理器里打开")).on_click(
                        move |_, window, cx| {
                            crate::shell::report(crate::shell::open_folder(&c), window, cx)
                        },
                    )),
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
        let staged = self
            .status
            .as_ref()
            .is_some_and(|s| s.entries.iter().any(|e| e.staged.is_some()));
        // 没有冲突、也没有暂存的改动：这个提交在当前分支上不带来任何改动（内容已经有了）。
        // 变基、挑拣、还原这时「继续」会失败，只能跳过；合并可以照常提交
        let empty = conflicts == 0 && !staged && state != InProgress::Merge;
        let go = if state == InProgress::Merge {
            tr("提交合并")
        } else {
            tr("继续")
        };
        let text = if conflicts > 0 {
            tf!(
                "{name}停在一半：还有 {conflicts} 个文件有冲突。在「本地修改」里解决（右键可以整个文件取一边、交给编辑器），都解决后点「{go}」。",
                name = state.name(),
                conflicts,
                go
            )
        } else if empty {
            tf!(
                "{name}停在一半：这个提交在当前分支上没有带来改动（内容已经有了）。点「跳过这个提交」接着做。",
                name = state.name()
            )
        } else if state == InProgress::Merge {
            tr("合并停在一半：冲突都解决了，点「提交合并」。提交说明框里写了内容就用它，没写用默认的合并说明。").into()
        } else {
            tf!(
                "{name}停在一半：冲突都解决了，点「继续」。",
                name = state.name()
            )
        };
        h_flex()
            .min_h(px(34.))
            .py_1()
            .flex_none()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(border)
            .bg(warning.opacity(0.15))
            .text_sm()
            // 窗口窄的时候折成几行：这里写的是下一步该做什么，不能用省略号截掉
            .child(div().flex_1().min_w_0().whitespace_normal().child(text))
            .child(
                Button::new("op-continue")
                    .small()
                    .primary()
                    .label(go)
                    .disabled(!idle || conflicts > 0 || empty)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.continue_in_progress(state, window, cx)
                    })),
            )
            .when(state != InProgress::Merge, |bar| {
                bar.child(
                    Button::new("op-skip")
                        .small()
                        .label(tr("跳过这个提交"))
                        .disabled(!idle)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.skip_in_progress(state, window, cx)
                        })),
                )
            })
            .child(
                Button::new("op-abort")
                    .small()
                    .label(tr("中止"))
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
                        .label(tr("重试"))
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

/// 工具栏按钮：图标在上、字在下。`badge` 不是 0 时在图标旁边标一个数（拉取、推送有几个提交）。
#[allow(clippy::too_many_arguments)]
fn tool_button(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    tooltip: &'static str,
    running: bool,
    enabled: bool,
    badge: usize,
    cx: &App,
) -> Button {
    tool_button_bare(id, icon, label, running, badge, cx)
        .tooltip(tooltip)
        .disabled(!enabled)
}

fn tool_button_bare(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    running: bool,
    badge: usize,
    cx: &App,
) -> Button {
    let glyph = if running {
        Spinner::new()
            .icon(Icon::new(IconName::LoaderCircle))
            .with_size(px(16.))
            .into_any_element()
    } else {
        Icon::new(icon).size_4().into_any_element()
    };
    Button::new(id)
        .ghost()
        .h(px(42.))
        .min_w(px(46.))
        .px(px(8.))
        .child(
            v_flex()
                .items_center()
                .gap(px(3.))
                .child(
                    h_flex()
                        .h(px(16.))
                        .gap(px(4.))
                        .child(glyph)
                        .when(badge > 0, |row| row.child(ui::count_pill(badge, true, cx))),
                )
                .child(div().text_size(px(11.)).line_height(px(13.)).child(label)),
        )
}

impl Render for RepoView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tick_bench(window, cx);
        self.history.set_viewport(window.viewport_size());
        self.focus_blame_if_new(window, cx);
        let main = if self.channel.is_none() {
            match self.error.clone() {
                Some(err) => self.render_placeholder(err, true, cx),
                None => {
                    let text = tf!("正在连接 {cx}…", cx = self.place(cx));
                    self.render_placeholder(text.into(), false, cx)
                }
            }
        } else if let (Some(err), None) = (self.error.clone(), &self.status) {
            self.render_placeholder(err, true, cx)
        } else if self.status.is_none() {
            self.render_placeholder(tr("正在读取仓库状态…").into(), false, cx)
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
        h_flex().size_full().child(self.render_sidebar(cx)).child(
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(self.render_toolbar(cx))
                .children(banner)
                .child(div().flex_1().min_h_0().child(main)),
        )
    }
}
