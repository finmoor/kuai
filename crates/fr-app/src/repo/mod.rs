//! 仓库标签页。
//!
//! 分 Git、终端、文件三页，侧栏里仓库名下面的切换（或者 Ctrl+`、Ctrl+Shift+E）来回切，每个标签页各记各的：
//! - Git 页：左边是侧栏（仓库的名字、本地修改、所有提交、分支、远程分支、标签、贮藏），
//!   从上到下占满；右边上面是工具栏（当前分支、获取、拉取、推送、贮藏……），下面按侧栏选中的项
//!   显示本地修改（按文件、按块、按行暂存，提交）或提交历史（提交图、详情、改动）。
//! - 终端页：侧栏里列着这个仓库的几个终端，右边是选中的那个，见 [`terminals`]。在仓库目录里的
//!   shell，切回 Git 页、关标签页都不结束里面的程序，再到这个仓库的终端页时接上。本机仓库的
//!   终端在退出 Kuai 时结束；服务器上的仓库的终端开在 kuaid 里，退出、断线它都接着跑。
//! - 文件页：侧栏里是仓库目录的树，右边是打开的文件，能看、能小改、能上传下载，见 [`files`]。
//!
//! 侧栏可以收成 48 宽的图标栏（侧栏右上角的按钮、Ctrl+Shift+B，或者把分隔条往左拖到头），
//! 三页都一样，见 [`rail`]。
//!
//! 本机仓库和服务器上的仓库共用这一套，经 [`Channel`] 执行 git。
//!
//! 什么时候刷新：连上以后、切到这个标签页、切回这个窗口、每次写操作之后，还有文件监听报告
//! 仓库有变化时（在编辑器里存了文件、在终端里提交了），都重读工作区状态、引用和贮藏；
//! 引用或 HEAD 变了才重读提交历史。

mod activity;
mod auto_fetch;
mod blame;
mod changes;
mod compare;
mod detached;
mod dialogs;
mod diff_search;
mod diff_tools;
mod file_picker;
mod file_tree;
mod files;
mod files_ops;
mod files_view;
mod history;
mod image_diff;
mod impact;
mod inline_diff;
mod layout;
mod markdown;
mod menus;
mod merge;
mod ops;
mod rail;
mod rebase;
mod recover;
mod sidebar;
mod stage_diff;
mod subs;
mod sweep;
mod terminals;
mod worktrees;

pub(crate) use files::local_file_name;
pub(crate) use files_ops::new_copy_dir;
pub(crate) use layout::sidebar_open;
pub(crate) use ops::explain;
pub(crate) use subs::{PARALLEL, Summary, summarize};
pub(crate) use terminals::{end_parked_terminals, parked_terminals_of, parked_terminals_running};

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

use crate::app_state::{AppState, HostConnected};
use crate::settings::RepoEntry;
use crate::ui;

/// 工具栏的高度；侧栏顶上写仓库名字的那一块和它一样高，底边对得齐。
const TOOLBAR_HEIGHT: f32 = 52.;
/// 文件监听断了（仓库目录一度没了、换了，装不上）以后，第一次隔这么久自己刷新、重新装；还是装不上
/// 就加倍等，最多等 [`WATCH_RETRY_MAX`]
const WATCH_RETRY_MIN: std::time::Duration = std::time::Duration::from_secs(2);
const WATCH_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);
/// 装上的监听保持了这么久才断，算是好好用了一阵，下次重装又从最短的等起
const WATCH_STABLE: std::time::Duration = std::time::Duration::from_secs(60);

/// 右边显示什么
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Changes,
    History,
    /// 逐行追溯，盖在上面两个视图之上
    Blame,
    /// 合并编辑器：解决一个文件里的冲突，也是盖在上面
    Merge,
    /// 子仓库总览（仓库目录里面还有别的仓库时才有），见 [`subs`]
    Subs,
    /// 工作树总览，见 [`worktrees`]
    Worktrees,
    /// 比较两端，见 [`compare`]
    Compare,
    /// 找回提交，见 [`recover`]；下半部分的详情和改动和提交历史共用
    Recover,
}

/// 显示哪一页（侧栏里的切换，Ctrl+` 来回切）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    /// 侧栏、提交历史、本地修改
    Git,
    /// 这个仓库的终端们
    Terminal,
    /// 仓库目录里的文件
    Files,
}

/// 仓库页发给工作区的事件。
pub enum RepoEvent {
    /// 在新标签页里打开另一个仓库（子模块）
    Open(RepoEntry),
    /// 给这个仓库改名（对话框由主窗口开）
    Rename(RepoEntry),
}

impl EventEmitter<RepoEvent> for RepoView {}

/// 文件监听断了以后的重装（见 [`RepoView::watch_ended`]）：刷新时马上重装，没有刷新就隔一会儿
/// 自己刷新一次（越装不上隔得越久）。
#[derive(Default)]
struct WatchRetry {
    /// 监听结束了，还没重新装上
    lost: bool,
    /// 这一次监听是什么时候装上的
    since: Option<std::time::Instant>,
    /// 这一次自己刷新前等了多久
    delay: std::time::Duration,
    /// 等着自己刷新的任务
    task: Option<Task<()>>,
}

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
    merge: Option<merge::MergeView>,
    /// 上次读历史时的引用快照（所有分支、标签加 HEAD），变了才重读历史
    history_key: Option<Vec<u8>>,
    refresh_task: Option<Task<()>>,
    /// 正在刷新
    refreshing: bool,
    /// 刷新期间又收到了变化通知：这次刷完再刷一次
    refresh_again: bool,
    /// 文件监听；流结束（装不上监听、仓库目录没了或换了、连接断了）后任务也结束
    watch_task: Option<Task<()>>,
    watch_retry: WatchRetry,
    /// 从哪个工作区打开的（工作区页上打开的、会话页里列在哪个工作区下面）：侧栏顶上的工作区按钮
    /// 先写它，在几个工作区里时菜单里它排第一
    home_hint: Option<crate::machines::WorkspaceKey>,
    history: history::History,
    changes: changes::Changes,
    sidebar: sidebar::Sidebar,
    /// 正在执行的写操作；同一时间只做一个
    op: Option<SharedString>,
    /// 写操作期间占着这个仓库的「正在写」（见 [`ops::Writing`]）：别的页面同时写同一个仓库会被挡住
    op_writing: Option<ops::Writing>,
    op_task: Option<Task<()>>,
    /// 获取、拉取、推送的进度（git 标准错误的最后一行）；有它说明正在做的这一步可以取消
    op_progress: Option<SharedString>,
    /// 往这里发一下，正在做的联网操作就停下
    op_cancel: Option<futures::channel::oneshot::Sender<()>>,
    /// 执行过的 git 命令和它们的结果
    activity: activity::Log,
    auto_fetch: auto_fetch::AutoFetch,
    /// 窗口内容区的大小，每帧更新；各块的大小按它收
    viewport: Size<Pixels>,
    /// 显示的是 Git 页还是终端页
    page: Page,
    /// 终端页上的终端们
    terminals: terminals::Terminals,
    /// 文件页：目录树和打开的文件
    files: files::Files,
    /// 从终端页按 Ctrl+` 回到哪一页（Git 或者文件）
    back_page: Page,
    /// 这是哪个根仓库的子仓库；平常打开的仓库没有
    sub_of: Option<subs::SubOf>,
    /// 仓库目录里面的仓库，和进过的那些（子仓库、工作树）的页面
    subs: subs::Subs,
    /// 这个仓库的工作树
    worktrees: worktrees::Worktrees,
    /// 比较：两端、文件清单、选中文件的改动
    compare: compare::Compare,
    /// 找回提交：HEAD 和各分支移动过的记录
    recover: recover::Recover,
    /// 看改动时忽略空白（这个标签页里三处共用，不存盘），见 [`diff_tools`]
    ignore_ws: bool,
    /// 在改动中搜索：三处各一个搜索条，见 [`diff_search`]
    diff_search: diff_search::DiffSearches,
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
        // 所在的电脑断开以后在后台连回来了：跟着接上
        subscriptions.push(cx.subscribe(&app, |this, _, event: &HostConnected, cx| {
            this.host_connected(&event.0, cx)
        }));
        // 还在连：连不上要重试时「正在连接」那句话跟着写第几次
        subscriptions.push(cx.observe(&app, |this, _, cx| {
            if this.channel.is_none() && this.error.is_none() {
                cx.notify();
            }
        }));
        let (history, history_subscriptions) = history::History::new(window, cx);
        subscriptions.extend(history_subscriptions);
        let (sidebar, sidebar_subscription) = sidebar::Sidebar::new(window, cx);
        subscriptions.push(sidebar_subscription);
        let (files, files_subscription) = files::Files::new(window, cx);
        subscriptions.push(files_subscription);
        let (subs, subs_subscription) = subs::Subs::new(window, cx);
        subscriptions.push(subs_subscription);
        let (compare, compare_subscription) = compare::Compare::new(window, cx);
        subscriptions.push(compare_subscription);
        let (recover, recover_subscription) = recover::Recover::new(window, cx);
        subscriptions.push(recover_subscription);
        let (diff_search, search_subscriptions) = diff_search::DiffSearches::new(window, cx);
        subscriptions.extend(search_subscriptions);
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
            merge: None,
            history_key: None,
            refresh_task: None,
            refreshing: false,
            refresh_again: false,
            watch_task: None,
            watch_retry: WatchRetry::default(),
            home_hint: None,
            history,
            changes,
            sidebar,
            op: None,
            op_writing: None,
            op_task: None,
            op_progress: None,
            op_cancel: None,
            activity: activity::Log::default(),
            auto_fetch: auto_fetch::AutoFetch::new(cx),
            viewport: window.viewport_size(),
            page: Page::Git,
            terminals: terminals::Terminals::default(),
            files,
            back_page: Page::Git,
            sub_of: None,
            subs,
            worktrees: worktrees::Worktrees::default(),
            compare,
            recover,
            ignore_ws: false,
            diff_search,
            _subscriptions: subscriptions,
        };
        // 失败的命令记进诊断的「最近的错误」时写上是哪个仓库
        this.activity.name = this
            .entry
            .path
            .rsplit(['/', '\\'])
            .find(|part| !part.is_empty())
            .unwrap_or_default()
            .to_string();
        // 测试开关：FR_TEST_VIEW=changes 打开仓库时直接显示本地修改
        if std::env::var("FR_TEST_VIEW").is_ok_and(|v| v == "changes") {
            this.show_changes(cx);
        }
        this.keep_local_terminals(cx);
        this.follow_board(cx);
        this.connect(cx);
        this
    }

    /// 根仓库里的一个子仓库的页面（见 [`subs`]）：和平常的仓库页一样，侧栏顶上多一个返回按钮。
    fn new_sub(
        entry: RepoEntry,
        app: Entity<AppState>,
        of: subs::SubOf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new(entry, app, window, cx);
        this.sub_of = Some(of);
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
                        this.check_server_terminals(&channel, cx);
                        this.channel = Some(channel);
                        this.refresh(cx);
                        this.scan_subs(cx);
                    }
                    Err(err) => this.error = Some(err),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// 所在的电脑（重新）连上了（断网、睡眠以后在后台连回来，见 `AppState::schedule_reconnect`）：
    /// 连接断了、连不上停着的重新连，看不见的切过来时再连（见 [`Self::refresh`]）；断开时停下的终端
    /// （程序还在那台的 kuaid 里跑）接回去，不用按 Enter。
    fn host_connected(&mut self, key: &str, cx: &mut Context<Self>) {
        if self
            .entry
            .server
            .as_deref()
            .unwrap_or(crate::machines::LOCAL)
            != key
        {
            return;
        }
        let dead = match &self.channel {
            Some(Channel::Remote(conn)) => !conn.is_alive(),
            Some(_) => false,
            None => true,
        };
        if self.entry.server.is_some() && dead && !self.connecting && self.visible {
            self.connect(cx);
        }
        self.terminals.host_back(cx);
    }

    /// 监听仓库的变化，有变化就刷新。
    fn start_watch(&mut self, channel: &Channel, cx: &mut Context<Self>) {
        let mut changes = channel.watch(&self.entry.path);
        self.watch_retry.lost = false;
        self.watch_retry.since = Some(std::time::Instant::now());
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
            this.update(cx, |this, cx| this.watch_ended(cx)).ok();
        }));
    }

    /// 文件监听结束了。连接断了就马上刷新，按现在的设置重连（改了主机设置时也会主动断开）。别的原因
    /// （仓库目录一度没了、换了，装不上）记下来，刷新时重新装，没有刷新就隔一会儿自己刷新一次。原来
    /// 只在连上时装：目录删掉又恢复以后的改动再也不自动刷新。
    fn watch_ended(&mut self, cx: &mut Context<Self>) {
        if matches!(&self.channel, Some(Channel::Remote(conn)) if !conn.is_alive()) {
            self.refresh(cx);
            return;
        }
        let retry = &mut self.watch_retry;
        retry.lost = true;
        let stable = retry
            .since
            .is_some_and(|since| since.elapsed() >= WATCH_STABLE);
        retry.delay = if stable || retry.delay.is_zero() {
            WATCH_RETRY_MIN
        } else {
            (retry.delay * 2).min(WATCH_RETRY_MAX)
        };
        let delay = retry.delay;
        retry.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            // 不在前台的不刷：切过来时会刷新，那时重新装
            this.update(cx, |this, cx| {
                if this.visible && this.watch_retry.lost {
                    this.refresh(cx);
                }
            })
            .ok();
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
    /// 进到了子仓库里时，可见的是那个子仓库的页面，根仓库自己不算。
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.subs.tab_visible = visible;
        self.apply_visibility(cx);
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
        // git 太旧，提交详情这些用不了：直接说清楚，不让它们一样样地报 git 的错
        if let Some(have) = fr_core::git::too_old(channel.git_version()) {
            let (major, minor, _) = fr_core::git::MIN_VERSION;
            let need = format!("{major}.{minor}");
            let place = self.place(cx);
            let text = tf!(
                "{place}上的 git 是 {have}，Kuai 需要 {need} 或更新的版本。升级 git 后重新打开 Kuai。",
                place,
                have,
                need
            );
            self.error = Some(text.into());
            cx.notify();
            return;
        }
        if self.watch_retry.lost {
            self.start_watch(&channel, cx);
        }
        let repo = self.entry.path.clone();
        let read_status = channel.git(&repo, status::args());
        let read_refs = channel.git(&repo, refs::args());
        let read_stashes = channel.git(&repo, refs::stash_args());
        let read_state = channel.git_with_input(&repo, state::args(), Some(state::stdin()));
        let read_submodules = channel.git(&repo, menus::submodule_args());
        let read_remotes = channel.git(&repo, vec!["remote".into()]);
        let nul = fr_core::worktree::has_nul(channel.git_version());
        let read_worktrees = channel.git(&repo, fr_core::worktree::list_args(nul));
        self.refreshing = true;
        self.refresh_again = false;
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let (st, rf, sh, state, sm, rm, wt) = futures::join!(
                read_status,
                read_refs,
                read_stashes,
                read_state,
                read_submodules,
                read_remotes,
                read_worktrees
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
                // 很旧的 git 没有这条命令：当成没有工作树，侧栏上不出现那一项
                let trees = wt
                    .ok()
                    .filter(|out| out.success())
                    .map(|out| fr_core::worktree::parse(&out.stdout, nul))
                    .unwrap_or_default();
                this.set_worktrees(trees, cx);
                let head = parsed.head.clone().unwrap_or_default();
                this.apply_status(parsed, cx);
                this.files_status_changed(cx);
                let refs_out = rf.and_then(|out| out.ok()).unwrap_or_default();
                let stash_out = sh.map(|out| out.stdout).unwrap_or_default();
                let old_refs =
                    std::mem::replace(&mut this.refs, refs::parse(&refs_out, &stash_out));
                this.remap_sidebar_selection(&old_refs, cx);
                this.compare_refreshed(cx);
                // 提交历史只含分支、远程分支、标签和 HEAD，这些都没变就不用重读
                let mut key = refs_out;
                key.extend_from_slice(head.as_bytes());
                if this.history_key.as_ref() != Some(&key) {
                    this.history_key = Some(key);
                    this.reload_history(cx);
                    this.recover_refs_changed(cx);
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
        match self.mode {
            Mode::Subs => self.load_sub_summaries(cx),
            Mode::Worktrees => self.load_worktree_summaries(cx),
            Mode::Recover => self.load_recover(cx),
            _ => {}
        }
    }

    /// 换了界面语言时由主窗口调用。大部分文字每次重画都重新取，自己会变；
    /// 输入框里的提示文字、存在状态里的几句话是建的时候定的，这里重新设一遍。
    pub fn language_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.retranslate_sidebar(window, cx);
        self.retranslate_history(window, cx);
        self.retranslate_changes(window, cx);
        self.retranslate_subs(window, cx);
        self.retranslate_compare(window, cx);
        self.retranslate_files(window, cx);
        self.retranslate_recover(window, cx);
        self.diff_search.retranslate(window, cx);
        cx.notify();
    }

    /// 工作区里有几个文件有没提交的改动（标签页上的小点、侧栏「本地修改」后面的数字用）。
    /// 状态读失败时不知道有几个，算 0：不留着上一次的数字。
    pub fn change_count(&self) -> usize {
        match self.error {
            Some(_) => 0,
            None => self.status.as_ref().map_or(0, |s| s.entries.len()),
        }
    }

    /// Ctrl+F：在正看着的东西里找，不为了找东西换到别的页面、别的视图。
    ///
    /// - 文件页：在打开着的文件里查找（编辑器自带的查找条）；没有打开文本文件时把焦点放到
    ///   按名称过滤文件的框里。
    /// - 终端页：没有可找的，不动（焦点在终端里时这个键归里面的程序，到不了这里）。
    /// - Git 页：合并编辑器里手工编辑时在编辑器里查找；逐行追溯里没有查找，不动（原来会被
    ///   换到提交历史，正解着的冲突、看着的追溯就不见了）；子仓库总览是它的过滤框；正在看改动
    ///   （本地修改里选了文件、比较，或者焦点在提交的改动里）就在改动里搜；其余的是提交历史的
    ///   搜索框。
    ///
    /// 1.16.1 以前不管在哪一页都切到 Git 页去搜提交：文件页上写着「Ctrl+F 在文件中查找」，按了
    /// 却跳走。
    pub fn find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.page {
            Page::Files => self.find_in_file(window, cx),
            Page::Terminal => {}
            Page::Git => match self.mode {
                Mode::Merge => self.find_in_merge(window, cx),
                Mode::Blame => {}
                Mode::Subs => self.focus_subs_filter(window, cx),
                _ => {
                    if !self.find_in_diff(window, cx) {
                        self.focus_search(window, cx);
                    }
                }
            },
        }
    }

    /// 仓库在哪：本机或服务器名（经别的服务器连的是它在那台服务器上的名字）。
    fn place(&self, cx: &App) -> String {
        let machine = crate::machines::machine_of(&self.entry);
        self.app.read(cx).machine_name(machine).to_string()
    }

    /// 侧栏、图标栏上写的位置：经别的服务器连的写明经由哪台（「B · 经由 A」）。
    fn place_label(&self, cx: &App) -> String {
        let place = self.place(cx);
        match self
            .entry
            .server
            .as_deref()
            .and_then(crate::machines::split_relay)
        {
            Some((via, _)) => tf!(
                "{place} · 经由 {via}",
                place,
                via = self.app.read(cx).machine_name(via)
            ),
            None => place,
        }
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted, bg, accent, chip) = (
            theme.border,
            theme.muted_foreground,
            theme.background,
            theme.primary,
            theme.secondary,
        );
        // 读状态失败时主区域是带「重试」的说明，工具栏的操作也先停用
        let ready = self.channel.is_some() && self.status.is_some() && self.error.is_none();
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
                (None, None) => (tr("暂无提交").into(), false),
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
                            .child(branch.clone()),
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
                    // 分支名长了会被截掉：提示里有全名
                    .tooltip(move |window, cx| {
                        ui::two_line_tip(
                            branch.clone(),
                            tr("↑ 待推送，↓ 待拉取").into(),
                            window,
                            cx,
                        )
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
                    ui::clipped("op-progress", text)
                        .mr_1()
                        .flex_none()
                        .max_w(px(240.))
                        .text_xs()
                        .text_color(muted),
                )
                // 只有联网的操作能取消：中途杀掉改暂存区的 git 会留下锁文件
                .when(self.op_progress.is_some(), |bar| {
                    bar.child(
                        Button::new("cancel-op")
                            .xsmall()
                            .label(tr("取消"))
                            .tooltip(tr("中止此操作"))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_op(cx))),
                    )
                })
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
                            tr("git fetch --all；右键查看更多选项"),
                            running(tr("获取")) || self.auto_fetching(),
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
                            tr("git pull；右键查看更多选项"),
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
                            tr("git push；右键查看更多选项"),
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
                        tr("git stash push：贮藏当前改动")
                    } else {
                        tr("没有可贮藏的改动")
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
                    tr("git stash pop：恢复最近一次贮藏并将其删除"),
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
                    tr("刷新状态和提交历史（F5）"),
                    self.refreshing && self.op.is_none(),
                    true,
                    0,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.refresh_now(cx))),
            )
            .child(
                tool_button(
                    "activity",
                    IconName::Activity,
                    tr("活动"),
                    tr("已执行的 git 命令及输出"),
                    false,
                    true,
                    self.activity_running(),
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| this.show_activity(window, cx))),
            )
    }

    /// 「打开」按钮：把仓库交给外面的终端、VS Code 或资源管理器。
    fn render_open_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dir = self.entry.path.clone();
        let server = self
            .entry
            .server
            .as_deref()
            .and_then(|id| self.app.read(cx).server(id))
            .cloned();
        // 经别的服务器连的（A>B）：本机连不到它，外部终端和 VS Code 都用不了，也不能映射
        let via = self
            .entry
            .server
            .as_deref()
            .and_then(crate::machines::split_relay)
            .map(|(via, _)| self.app.read(cx).machine_name(via));
        let app = self.app.clone();
        // 不加悬停提示：提示也出现在按钮下面，会把弹出的菜单第一项盖住
        tool_button_bare(
            "open-in",
            IconName::SquareArrowOutUpRight,
            tr("打开"),
            false,
            0,
            cx,
        )
        .dropdown_menu(move |menu, _, cx| {
            let (a, b, c) = (dir.clone(), dir.clone(), dir.clone());
            if let Some(via) = &via {
                return menu
                    .item(PopupMenuItem::new(tr("在外部终端中打开（ssh）")).disabled(true))
                    .item(PopupMenuItem::new(tr("在 VS Code 中打开（Remote-SSH）")).disabled(true))
                    .label(tf!("经由 {via} 连接的仓库不支持以上两项。", via = via));
            }
            match server.clone() {
                // 服务器上的仓库：终端经 ssh 登录，VS Code 走 Remote-SSH
                Some(server) => {
                    let dest2 = server.destination.clone();
                    let server_id = server.id.clone();
                    let menu = menu
                        .item(PopupMenuItem::new(tr("在外部终端中打开（ssh）")).on_click(
                            move |_, window, cx| {
                                let result = crate::shell::open_terminal_ssh(&server, &a);
                                crate::shell::report(result, window, cx);
                            },
                        ))
                        .item(
                            PopupMenuItem::new(tr("在 VS Code 中打开（Remote-SSH）")).on_click(
                                move |_, window, cx| {
                                    let result = crate::shell::open_vscode_remote(&dest2, &b);
                                    crate::shell::report(result, window, cx);
                                },
                            ),
                        )
                        .separator();
                    // 映射到本机了：可以直接交给本机的资源管理器、VS Code；没有就给映射的入口
                    match app.read(cx).local_path(&server_id, &c) {
                        Some(local) => {
                            let code = local.clone();
                            menu.item(
                                PopupMenuItem::new(crate::shell::open_folder_label()).on_click(
                                    move |_, window, cx| {
                                        let result = crate::shell::open_folder(&local);
                                        crate::shell::report(result, window, cx)
                                    },
                                ),
                            )
                            .item(
                                PopupMenuItem::new(tr("在本机的 VS Code 中打开")).on_click(
                                    move |_, window, cx| {
                                        let result = crate::shell::open_vscode(&code);
                                        crate::shell::report(result, window, cx)
                                    },
                                ),
                            )
                        }
                        None => {
                            crate::mapping::menu_items(menu, &app, &server_id, &c, true, &c, cx)
                        }
                    }
                }
                None => menu
                    .item(PopupMenuItem::new(tr("在外部终端中打开")).on_click(
                        move |_, window, cx| {
                            crate::shell::report(crate::shell::open_terminal(&a), window, cx)
                        },
                    ))
                    .item(PopupMenuItem::new(tr("在 VS Code 中打开")).on_click(
                        move |_, window, cx| {
                            crate::shell::report(crate::shell::open_vscode(&b), window, cx)
                        },
                    ))
                    .item(
                        PopupMenuItem::new(crate::shell::open_folder_label()).on_click(
                            move |_, window, cx| {
                                crate::shell::report(crate::shell::open_folder(&c), window, cx)
                            },
                        ),
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
                "{name}已暂停：{conflicts} 个文件有冲突。请在「本地修改」中解决后点击「{go}」。",
                name = state.name(),
                conflicts,
                go
            )
        } else if empty {
            tf!(
                "{name}已暂停：此提交没有改动（内容已存在）。请点击「跳过此提交」。",
                name = state.name()
            )
        } else if state == InProgress::Merge {
            tr("合并尚未完成：冲突已全部解决，请点击「提交合并」。说明留空则使用默认说明。").into()
        } else {
            tf!(
                "{name}已暂停：冲突已全部解决，请点击「继续」。",
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
                        .label(tr("跳过此提交"))
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
                // 连不上主机：旁边多一个「诊断…」，逐步查是哪一步出的问题
                let server = self.entry.server.clone().filter(|_| self.channel.is_none());
                d.child(
                    h_flex()
                        .gap_2()
                        .child(Button::new("retry").small().label(tr("重试")).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.error = None;
                                this.history_key = None;
                                this.refresh(cx);
                            }),
                        ))
                        .when_some(server, |row, server| {
                            row.child(
                                Button::new("diagnose")
                                    .small()
                                    .icon(ui::icon(IconName::Stethoscope))
                                    .label(tr("诊断…"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        // 诊断连上了：这一页借这个连接重新读取
                                        let view = cx.entity().downgrade();
                                        let retry: crate::diagnose::Connected =
                                            std::rc::Rc::new(move |cx: &mut App| {
                                                view.update(cx, |this, cx| {
                                                    if this.channel.is_none() {
                                                        this.error = None;
                                                        this.history_key = None;
                                                        this.refresh(cx);
                                                    }
                                                })
                                                .ok();
                                            });
                                        crate::diagnose::Diagnose::open(
                                            this.app.clone(),
                                            &server,
                                            Some(retry),
                                            window,
                                            cx,
                                        )
                                    })),
                            )
                        }),
                )
            })
            .into_any_element()
    }

    /// Git 页的右边：工具栏，停在一半的操作的横幅，下面是本地修改或提交历史。
    fn render_git_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let main = if self.channel.is_none() {
            match self.error.clone() {
                Some(err) => self.render_placeholder(err, true, cx),
                None => {
                    let machine = crate::machines::machine_of(&self.entry);
                    let text = match self.app.read(cx).retrying(machine) {
                        Some(retry) => tf!(
                            "正在重试连接 {cx}（{retry}/{total}）…",
                            cx = self.place(cx),
                            retry,
                            total = crate::app_state::CONNECT_RETRIES
                        ),
                        None => tf!("正在连接 {cx}…", cx = self.place(cx)),
                    };
                    self.render_placeholder(text.into(), false, cx)
                }
            }
        } else if let Some(err) = self.error.clone() {
            // 读状态失败：手上的是上一次的状态，接着显示的话看着正常、还能对它暂存丢弃。换成带
            // 「重试」的说明（原来只有从没读到过时才这样，之后的失败只在工具栏上截着显示）
            self.render_placeholder(err, true, cx)
        } else if self.status.is_none() {
            self.render_placeholder(tr("正在读取仓库状态…").into(), false, cx)
        } else {
            match self.mode {
                Mode::Changes => self.render_changes(cx),
                Mode::History => self.render_history(cx),
                Mode::Blame => self.render_blame(cx),
                Mode::Merge => self.render_merge(cx),
                Mode::Subs => self.render_subs(cx),
                Mode::Worktrees => self.render_worktrees(cx),
                Mode::Compare => self.render_compare(cx),
                Mode::Recover => self.render_history(cx),
            }
        };
        let banner = self
            .in_progress
            .map(|state| self.render_in_progress(state, cx).into_any_element());
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(self.render_toolbar(cx))
            .children(banner)
            .child(div().flex_1().min_h_0().child(main))
            .into_any_element()
    }
}

/// 工具栏按钮：图标在上、字在下。`badge` 不是 0 时在图标旁边标一个数（拉取、推送有几个提交）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn tool_button(
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
        // 进到了子仓库、别的工作树里：整个页面是它的
        if let Some(sub) = self.active_sub() {
            return div().size_full().child(sub).into_any_element();
        }
        self.tick_bench(window, cx);
        self.viewport = window.viewport_size();
        let sizes = self.sizes(cx);
        self.history.set_layout(self.viewport, sizes);
        self.focus_blame_if_new(window, cx);
        self.sync_terminals(window, cx);
        self.sync_files(window, cx);
        let right = match self.page {
            Page::Git => self.render_git_page(cx),
            Page::Terminal => self.render_terminal_page(cx),
            Page::Files => self.render_files_page(cx),
        };
        h_flex()
            .size_full()
            // 拖分隔条：不管鼠标拖到哪一块上面，都由这里按位置算大小
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<layout::DragSplit>, _, cx| {
                    this.drag_split(event, cx)
                }),
            )
            .child(self.render_sidebar(window, cx))
            .child(right)
            .into_any_element()
    }
}
