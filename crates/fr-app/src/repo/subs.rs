//! 子仓库：仓库目录里面还有别的仓库（一个项目的各个插件各是一个仓库，NocoBase 的业务插件就是
//! 这样）时，在同一个标签页里看它们，不用每个开一个标签页。
//!
//! - 连上以后找一遍（[`fr_core::files::find_repos`]，服务器上的由 kuaid 找）。找到了，侧栏「所有提交」
//!   下面多一项「子仓库」，点它右边是总览表：每个子仓库在哪个分支、有几个改动、和上游差几个提交、
//!   最近一次提交，上面可以过滤、只看有改动的、全部获取、全部拉取。
//! - 点表里的一行进到那个子仓库：它是一个完整的仓库页（[`RepoView`]，`sub_of` 指回根仓库），Git、
//!   终端、文件三页都是它自己的，终端开在它的目录里。根仓库的页面这时整个换成它；侧栏顶上是
//!   「返回 <根仓库>」（Alt+←），回到总览表。进过的子仓库的页面留着，再进去还是原来的样子，
//!   里面的终端也接着跑。
//! - 子仓库里面不再找子仓库。
//!
//! 进到另一个工作树（见 [`super::worktrees`]）走的也是这一套：整页换成它、返回按钮、Alt+←。
//! 子仓库里还能再进它自己的工作树，所以最多两层，返回一次退一层。

use std::collections::HashMap;

use fr_channel::{Channel, GitOutput};
use fr_core::i18n::tr;
use fr_core::status;
use fr_core::tf;
use futures::StreamExt as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::sidebar::SideItem;
use super::{Mode, Page, RepoEvent, RepoView};
use crate::settings::RepoEntry;
use crate::ui;

/// 同时对几个子仓库执行 git（读状态、获取、拉取）。服务器上每条命令是一个 git 进程。
pub(crate) const PARALLEL: usize = 6;
/// 总览表一行的高度
pub(super) const ROW_HEIGHT: f32 = 44.;

/// 这个仓库页是哪个根仓库的子仓库。
pub(super) struct SubOf {
    pub root: WeakEntity<RepoView>,
    /// 根仓库显示的名字，写在返回按钮上
    pub name: SharedString,
    /// 是从工作树总览进来的（不是子仓库总览）
    pub worktree: bool,
}

/// 从总览表进到的是哪一个页面。
#[derive(Clone, PartialEq, Eq)]
pub(super) enum Child {
    /// 仓库目录里的子仓库：相对根仓库的路径
    Sub(String),
    /// 这个仓库的另一个工作树：它的目录
    Worktree(String),
}

impl Child {
    /// 目录名：标签页的标题后面带上它。
    fn name(&self) -> &str {
        let (Child::Sub(path) | Child::Worktree(path)) = self;
        let path = path.trim_end_matches(['/', '\\']);
        path.rsplit(['/', '\\']).next().unwrap_or(path)
    }
}

/// 总览表里一个子仓库（或者工作树）的情况。
#[derive(Clone, Default)]
pub(crate) struct Summary {
    pub branch: Option<String>,
    pub head: Option<String>,
    pub upstream: bool,
    pub changes: usize,
    pub ahead: u32,
    pub behind: u32,
    /// 最近一次提交的说明和时间
    pub subject: String,
    pub time: i64,
    /// 读不了（目录没了、不是完整的仓库）
    pub error: Option<SharedString>,
}

/// 进到子仓库以后做什么。
#[derive(Clone, Copy)]
pub(super) enum Then {
    /// 留在它上次的那一页
    Stay,
    NewTerminal,
    Files,
}

/// 批量操作里一个子仓库的结果：没做成的写原因。
enum Outcome {
    Done,
    /// 没去做：不是独立的仓库、正在别的页面里写
    Skipped(String),
    Failed(String),
}

/// 正在对所有子仓库做的事（全部获取、全部拉取）。
struct Bulk {
    label: &'static str,
    done: usize,
    total: usize,
}

pub(super) struct Subs {
    /// 相对根仓库的路径，按名字排
    paths: Vec<String>,
    /// 目录太多，没找全
    truncated: bool,
    scanning: bool,
    scan_task: Option<Task<()>>,
    summaries: HashMap<String, Summary>,
    loading: bool,
    summary_task: Option<Task<()>>,
    /// 进过的子仓库、工作树的页面
    views: Vec<(Child, Entity<RepoView>)>,
    /// 现在显示的是哪一个；空着是这个仓库自己
    active: Option<Child>,
    /// 这个标签页是不是当前显示的（根仓库自己的 `visible` 在显示子仓库时是假）
    pub(super) tab_visible: bool,
    filter: Entity<InputState>,
    only_changed: bool,
    bulk: Option<Bulk>,
    bulk_task: Option<Task<()>>,
}

impl Subs {
    pub(super) fn new(window: &mut Window, cx: &mut Context<RepoView>) -> (Self, Subscription) {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(tr("过滤子仓库…")));
        let subscription = cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let subs = Self {
            paths: Vec::new(),
            truncated: false,
            scanning: false,
            scan_task: None,
            summaries: HashMap::new(),
            loading: false,
            summary_task: None,
            views: Vec::new(),
            active: None,
            tab_visible: false,
            filter,
            only_changed: false,
            bulk: None,
            bulk_task: None,
        };
        (subs, subscription)
    }
}

/// 路径的最后一段：子仓库显示的名字。
fn leaf(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// 路径去掉最后一段：子仓库在哪个目录下（在根目录下的是空的）。
fn folder(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

pub(crate) async fn summarize(channel: Channel, repo: String) -> Summary {
    let read_status = channel.git(&repo, status::args());
    let last = ["log", "-1", "--format=%ct%x09%s"]
        .map(String::from)
        .to_vec();
    let read_last = channel.git(&repo, last);
    let (st, last) = futures::join!(read_status, read_last);
    let parsed = match st.and_then(|out| out.ok()) {
        Ok(out) => status::parse(&out),
        Err(err) => {
            return Summary {
                error: Some(format!("{err:#}").into()),
                ..Default::default()
            };
        }
    };
    // 还没有提交的仓库这条命令失败：最近提交空着
    let last = last
        .and_then(|out| out.ok())
        .map(|out| String::from_utf8_lossy(&out).trim().to_string())
        .unwrap_or_default();
    let (time, subject) = last.split_once('\t').unwrap_or(("", ""));
    Summary {
        upstream: parsed.upstream.is_some(),
        changes: parsed.entries.len(),
        ahead: parsed.ahead,
        behind: parsed.behind,
        branch: parsed.branch,
        head: parsed.head,
        subject: subject.to_string(),
        time: time.parse().unwrap_or(0),
        error: None,
    }
}

/// 读仓库根的参数（[`toplevel`] 解析）。
pub(super) fn toplevel_args() -> Vec<String> {
    ["rev-parse", "--show-toplevel"].map(String::from).to_vec()
}

/// [`toplevel_args`] 的结果：仓库根；不在仓库里（或者读不了）是 None。
pub(super) fn toplevel(out: anyhow::Result<GitOutput>) -> Option<String> {
    out.ok()
        .filter(GitOutput::success)
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|top| !top.is_empty())
}

/// `path` 是不是独立的仓库：有自己的仓库根，不是外面那个（`outer`，外面仓库的 [`toplevel`]）。
/// 子仓库、子模块的目录被删了、没初始化时，git 在里面会往上找到外面的仓库。两边的仓库根都由
/// git 算，路径里有符号链接也比得上。
pub(super) async fn own_repo(channel: &Channel, path: &str, outer: Option<&str>) -> bool {
    let inner = toplevel(channel.git(path, toplevel_args()).await);
    inner.is_some() && inner.as_deref() != outer
}
impl RepoView {
    /// 换了界面语言：过滤框里的提示文字重新设一遍，进过的子仓库的页面也跟着换。
    pub(super) fn retranslate_subs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.subs.filter.update(cx, |state, cx| {
            state.set_placeholder(tr("过滤子仓库…"), window, cx)
        });
        for (_, view) in &self.subs.views {
            view.update(cx, |view, cx| view.language_changed(window, cx));
        }
    }

    /// 现在显示的子仓库的页面；显示的是根仓库自己时没有。
    pub fn active_sub(&self) -> Option<Entity<RepoView>> {
        let active = self.subs.active.as_ref()?;
        self.subs
            .views
            .iter()
            .find(|(path, _)| path == active)
            .map(|(_, view)| view.clone())
    }

    /// 现在显示的子仓库、工作树的名字（标签页的标题后面带上它）；进了两层的写成「子仓库 / 工作树」。
    pub fn sub_title(&self, cx: &App) -> Option<SharedString> {
        let active = self.subs.active.as_ref()?;
        let inner = self
            .active_sub()
            .and_then(|view| view.read(cx).sub_title(cx));
        Some(match inner {
            Some(inner) => format!("{} / {inner}", active.name()).into(),
            None => active.name().to_string().into(),
        })
    }

    /// 根仓库和各个子仓库里一共有几个文件有没提交的改动（标签页上的小点用）。
    pub fn change_count_with_subs(&self) -> usize {
        let subs: usize = self.subs.summaries.values().map(|s| s.changes).sum();
        self.change_count() + subs
    }

    /// 根仓库或者进过的子仓库正在做的写操作的名字（关标签页前要看）。总览的批量获取、拉取也算：
    /// 原来不算，正在全部拉取时标签页说关就关。
    pub fn busy_with_subs(&self, cx: &App) -> Option<SharedString> {
        let mut subs = self.subs.views.iter();
        self.busy()
            .or_else(|| self.subs.bulk.as_ref().map(|bulk| bulk.label.into()))
            .or_else(|| subs.find_map(|(_, view)| view.read(cx).busy_with_subs(cx)))
    }

    /// 正在做的写操作是不是都能取消。
    pub fn cancellable_with_subs(&self, cx: &App) -> bool {
        let own = self.busy().is_none() || self.cancellable();
        let mut subs = self.subs.views.iter();
        own && subs.all(|(_, view)| view.read(cx).cancellable_with_subs(cx))
    }

    /// 根仓库和进过的子仓库里一共有几个文件没保存。
    pub fn unsaved_with_subs(&self, cx: &App) -> usize {
        let subs = self.subs.views.iter();
        self.unsaved_files()
            + subs
                .map(|(_, view)| view.read(cx).unsaved_with_subs(cx))
                .sum::<usize>()
    }

    /// 根仓库和进过的子仓库里一共有几个本机终端的程序正在运行。
    pub fn local_terminals_with_subs(&self, cx: &App) -> usize {
        let subs = self.subs.views.iter();
        self.local_terminals_running(cx)
            + subs
                .map(|(_, view)| view.read(cx).local_terminals_with_subs(cx))
                .sum::<usize>()
    }

    pub(super) fn has_subs(&self) -> bool {
        self.sub_of.is_none() && !self.subs.paths.is_empty()
    }

    pub(super) fn sub_count(&self) -> usize {
        self.subs.paths.len()
    }

    /// 子仓库在磁盘上的路径。本机的 Windows 路径用反斜杠，和单独打开它时的写法一样
    /// （终端、记住的东西按路径认）。
    pub(super) fn sub_path(&self, relative: &str) -> String {
        let windows_local = self.entry.server.is_none() && self.entry.path.contains('\\');
        match windows_local {
            true => format!(
                "{}\\{}",
                self.entry.path.trim_end_matches('\\'),
                relative.replace('/', "\\")
            ),
            false => format!("{}/{relative}", self.entry.path.trim_end_matches('/')),
        }
    }

    fn sub_entry(&self, relative: &str) -> RepoEntry {
        RepoEntry {
            server: self.entry.server.clone(),
            path: self.sub_path(relative),
        }
    }

    /// 找一遍子仓库（连上以后、点「重新扫描」）。子仓库里面不找。
    pub(super) fn scan_subs(&mut self, cx: &mut Context<Self>) {
        if self.sub_of.is_some() {
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let request = channel.fs_repos(&self.entry.path);
        self.subs.scanning = true;
        self.subs.scan_task = Some(cx.spawn(async move |this, cx| {
            let found = request.await;
            this.update(cx, |this, cx| {
                this.subs.scanning = false;
                match found {
                    Ok(found) => {
                        this.subs.truncated = found.truncated;
                        this.subs.paths = found.repos;
                        this.drop_worktrees_from_subs();
                        let paths = &this.subs.paths;
                        this.subs.summaries.retain(|path, _| paths.contains(path));
                    }
                    // 旧的 kuaid 不认识这个请求、目录读不了：当成没有子仓库
                    Err(err) => log::info!("{}：找子仓库失败：{err:#}", this.entry.path),
                }
                // 正在看总览表、子仓库却都没了：回到提交历史
                if this.subs.paths.is_empty() && this.mode == Mode::Subs {
                    this.sidebar_select_history();
                }
                if this.mode == Mode::Subs || this.subs.summaries.is_empty() {
                    this.load_sub_summaries(cx);
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// 读每个子仓库的分支、改动数、最近提交。
    pub(super) fn load_sub_summaries(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        if self.subs.paths.is_empty() {
            return;
        }
        let jobs: Vec<_> = self
            .subs
            .paths
            .iter()
            .map(|relative| {
                let (channel, relative) = (channel.clone(), relative.clone());
                let repo = self.sub_path(&relative);
                async move { (relative, summarize(channel, repo).await) }
            })
            .collect();
        self.subs.loading = true;
        self.subs.summary_task = Some(cx.spawn(async move |this, cx| {
            let mut results = futures::stream::iter(jobs).buffer_unordered(PARALLEL);
            while let Some((relative, summary)) = results.next().await {
                let updated = this.update(cx, |this, cx| {
                    this.subs.summaries.insert(relative, summary);
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                this.subs.loading = false;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// 侧栏点了「子仓库」：右边显示总览表。
    pub(super) fn show_subs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_file_views(window, cx);
        self.sidebar.selected = SideItem::SubRepos;
        self.mode = Mode::Subs;
        self.load_sub_summaries(cx);
        cx.notify();
    }

    /// 标签页显示、不显示时：显示着的那一个（根仓库自己，或者进去的那个子仓库）才算可见。
    pub(super) fn apply_visibility(&mut self, cx: &mut Context<Self>) {
        let tab = self.subs.tab_visible;
        let active = self.subs.active.clone();
        for (path, view) in &self.subs.views {
            let on = tab && active.as_ref() == Some(path);
            view.update(cx, |view, cx| view.set_visible(on, cx));
        }
        let own = tab && active.is_none();
        let shown = own && !self.visible;
        self.visible = own;
        if shown {
            self.refresh(cx);
            match self.mode {
                Mode::Subs => self.load_sub_summaries(cx),
                Mode::Worktrees => self.load_worktree_summaries(cx),
                _ => {}
            }
        }
    }

    /// 子仓库的清单里去掉这个仓库自己的工作树：工作树的 `.git` 也是一个文件，建在仓库目录里面时
    /// 会被当成子仓库找出来，它归工作树总览管，不列两遍。
    pub(super) fn drop_worktrees_from_subs(&mut self) {
        let trees: Vec<String> = self.worktree_paths();
        if trees.is_empty() || self.subs.paths.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut self.subs.paths);
        self.subs.paths = paths
            .into_iter()
            .filter(|relative| {
                let full = self.sub_path(relative);
                !trees
                    .iter()
                    .any(|tree| fr_core::worktree::same_path(tree, &full))
            })
            .collect();
        if self.subs.paths.is_empty() && self.mode == Mode::Subs {
            self.sidebar_select_history();
        }
    }

    /// 进到一个子仓库：页面整个换成它的。
    pub(super) fn enter_sub(
        &mut self,
        relative: &str,
        then: Then,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entry = self.sub_entry(relative);
        self.enter_child(Child::Sub(relative.to_string()), entry, then, window, cx);
    }

    /// 进到一个子仓库或者工作树：页面整个换成它的。进过的接着用原来的页面。
    pub(super) fn enter_child(
        &mut self,
        child: Child,
        entry: RepoEntry,
        then: Then,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let found = self.subs.views.iter().find(|(key, _)| *key == child);
        let view = match found {
            Some((_, view)) => view.clone(),
            None => {
                let app = self.app.clone();
                let of = SubOf {
                    root: cx.entity().downgrade(),
                    name: self.app.read(cx).repo_name(&self.entry),
                    worktree: matches!(child, Child::Worktree(_)),
                };
                let view = cx.new(|cx| RepoView::new_sub(entry, app, of, window, cx));
                // 子仓库里要求在新标签页里打开别的仓库（它的子模块）：转给工作区
                cx.subscribe(&view, |_, _, event: &RepoEvent, cx| {
                    if let RepoEvent::Open(entry) = event {
                        cx.emit(RepoEvent::Open(entry.clone()));
                    }
                })
                .detach();
                // 标签上的「正在做」、标题跟着它变
                cx.observe(&view, |_, _, cx| cx.notify()).detach();
                self.subs.views.push((child.clone(), view.clone()));
                view
            }
        };
        // 焦点还留在总览表的过滤框里的话，按键会进到看不见的输入框
        window.blur(cx);
        self.subs.active = Some(child);
        self.apply_visibility(cx);
        view.update(cx, |view, cx| match then {
            Then::Stay => {}
            Then::NewTerminal => view.new_terminal(window, cx),
            Then::Files => view.show_files(window, cx),
        });
        cx.notify();
    }

    /// 从子仓库、工作树回到上一层的总览表（返回按钮、Alt+←），一次退一层。没进到里面时返回假。
    /// Ctrl+F 在子仓库总览里：焦点放到过滤框。
    pub(super) fn focus_subs_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.subs.filter.focus_handle(cx).focus(window, cx);
    }

    pub fn back_to_root(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // 里面还进了一层（子仓库的工作树）：先退那一层
        if let Some(inner) = self.active_sub()
            && inner.update(cx, |inner, cx| inner.back_to_root(window, cx))
        {
            cx.notify();
            return true;
        }
        let Some(left) = self.subs.active.take() else {
            return false;
        };
        window.blur(cx);
        self.show_page(Page::Git, window, cx);
        match left {
            Child::Sub(_) => {
                if self.has_subs() {
                    self.show_subs(window, cx);
                }
            }
            Child::Worktree(_) => self.show_worktrees(window, cx),
        }
        self.apply_visibility(cx);
        cx.notify();
        true
    }

    /// 不再留着某个工作树的页面（它被移除了）。正显示着它的话先回到这个仓库自己。
    pub(super) fn drop_child(&mut self, child: &Child, cx: &mut Context<Self>) {
        if self.subs.active.as_ref() == Some(child) {
            self.subs.active = None;
        }
        self.subs.views.retain(|(key, _)| key != child);
        self.apply_visibility(cx);
    }

    /// 进过的那个子仓库、工作树的页面；没进过没有。
    pub(super) fn child_view(&self, child: &Child) -> Option<Entity<RepoView>> {
        let mut views = self.subs.views.iter();
        views
            .find(|(key, _)| key == child)
            .map(|(_, view)| view.clone())
    }

    /// 子仓库页上的返回按钮点了：交给根仓库。等这一轮更新完再做，根仓库那边要反过来改这个页面。
    fn leave_to_root(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.sub_of.as_ref().map(|of| of.root.clone()) else {
            return;
        };
        window.defer(cx, move |window, cx| {
            root.update(cx, |root, cx| root.back_to_root(window, cx))
                .ok();
        });
    }

    /// 子仓库页侧栏顶上的「返回 <根仓库>」：整行的按钮，一眼看得到。根仓库自己的页面没有。
    pub(super) fn render_back(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let of = self.sub_of.as_ref()?;
        let accent = cx.theme().primary;
        let (name, tip, worktree) = (of.name.clone(), of.name.clone(), of.worktree);
        Some(
            div()
                .flex_none()
                .px(px(10.))
                .pt(px(8.))
                .child(
                    h_flex()
                        .id("back-to-root")
                        .h(px(30.))
                        .px(px(10.))
                        .gap(px(6.))
                        .rounded(px(ui::RADIUS))
                        .border_1()
                        .border_color(accent.opacity(0.45))
                        .bg(accent.opacity(0.12))
                        .text_color(accent)
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .cursor_pointer()
                        .hover(|s| s.bg(accent.opacity(0.22)))
                        .child(ui::icon(IconName::ArrowLeft).flex_none())
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(tf!("返回 {name}", name)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .font_weight(FontWeight::NORMAL)
                                .opacity(0.75)
                                .child("Alt+←"),
                        )
                        .tooltip(move |window, cx| {
                            let name = tip.clone();
                            let text = match worktree {
                                true => tf!("返回 {name} 的工作树总览（Alt+←）", name),
                                false => tf!("返回 {name} 的子仓库总览（Alt+←）", name),
                            };
                            ui::tip(text, window, cx)
                        })
                        .on_click(
                            cx.listener(|this, _, window, cx| this.leave_to_root(window, cx)),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 侧栏收成图标栏时的返回按钮，放在最上面仓库图标的位置。
    pub(super) fn render_rail_back(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let of = self.sub_of.as_ref()?;
        let accent = cx.theme().primary;
        let (name, worktree) = (of.name.clone(), of.worktree);
        Some(
            div()
                .id("rail-back")
                .size(px(30.))
                .rounded(px(7.))
                .border_1()
                .border_color(accent.opacity(0.45))
                .bg(accent.opacity(0.12))
                .text_color(accent)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.bg(accent.opacity(0.22)))
                .child(ui::icon(IconName::ArrowLeft))
                .tooltip(move |window, cx| {
                    let name = name.clone();
                    let text = match worktree {
                        true => tf!("返回 {name} 的工作树总览（Alt+←）", name),
                        false => tf!("返回 {name} 的子仓库总览（Alt+←）", name),
                    };
                    ui::tip(text, window, cx)
                })
                .on_click(cx.listener(|this, _, window, cx| this.leave_to_root(window, cx)))
                .into_any_element(),
        )
    }

    /// 全部获取、全部拉取：一个个子仓库做（同时几个），做完说有几个没成。拉取只做能快进的，
    /// 没设上游、不在分支上的跳过：要合并、变基的进到那个子仓库里做。
    fn bulk_subs(&mut self, pull: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        if self.subs.bulk.is_some() {
            return;
        }
        let label = if pull {
            tr("全部拉取")
        } else {
            tr("全部获取")
        };
        let args: &[&str] = match pull {
            true => &["pull", "--ff-only", "--no-edit"],
            false => &["fetch", "--all"],
        };
        // 拉取只做在分支上、设了上游的。状态还没读到（或者读失败）的照样拉：git 会说清楚为什么
        // 不行，不能都当成没设上游跳过
        let (mut no_upstream, mut detached) = (0, 0);
        let targets: Vec<String> = self
            .subs
            .paths
            .iter()
            .filter(|path| {
                if !pull {
                    return true;
                }
                match self.subs.summaries.get(*path) {
                    Some(summary) if summary.branch.is_none() => detached += 1,
                    Some(summary) if !summary.upstream => no_upstream += 1,
                    _ => return true,
                }
                false
            })
            .cloned()
            .collect();
        if targets.is_empty() {
            let text = tr("没有可拉取的子仓库：都未设置上游或不在分支上。");
            window.push_notification(Notification::info(text), cx);
            return;
        }
        // 先核对每个子仓库还是独立的仓库：目录被删了、换成了普通文件夹时，git 在里面会往上找到
        // 这个仓库，「全部拉取」就成了拉取这个仓库
        let outer = channel.git(&self.entry.path, toplevel_args());
        let server = self.entry.server.clone();
        // 每个作业轮到它时才发 git（`buffer_unordered` 拿到名额才调闭包）：同时最多 PARALLEL 个
        let jobs: Vec<_> = targets
            .iter()
            .map(|relative| {
                let (channel, path, relative) =
                    (channel.clone(), self.sub_path(relative), relative.clone());
                let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
                let key = super::ops::writing_key(server.as_deref(), &path);
                move |outer: Option<String>| async move {
                    if !own_repo(&channel, &path, outer.as_deref()).await {
                        let why = tr("不是独立的仓库（可能已删除或未初始化），已跳过。");
                        return (relative, Outcome::Skipped(why.to_string()));
                    }
                    // 这个子仓库正在别的页面里写：跳过，不和它撞
                    let _writing = match super::ops::Writing::take(key, label.into()) {
                        Ok(writing) => writing,
                        Err(running) => {
                            let why = tf!("正在另一个页面中执行「{running}」，已跳过。", running);
                            return (relative, Outcome::Skipped(why));
                        }
                    };
                    let outcome = match channel.git(&path, args).await.and_then(|out| out.ok()) {
                        Ok(_) => Outcome::Done,
                        Err(err) => Outcome::Failed(format!("{err:#}")),
                    };
                    (relative, outcome)
                }
            })
            .collect();
        self.subs.bulk = Some(Bulk {
            label,
            done: 0,
            total: targets.len(),
        });
        self.subs.bulk_task = Some(cx.spawn_in(window, async move |this, cx| {
            let outer = toplevel(outer.await);
            let jobs = jobs.into_iter().map(|job| job(outer.clone()));
            let mut results = futures::stream::iter(jobs).buffer_unordered(PARALLEL);
            let (mut failed, mut skipped) = (Vec::new(), Vec::new());
            while let Some((relative, outcome)) = results.next().await {
                match outcome {
                    Outcome::Done => {}
                    Outcome::Skipped(why) => skipped.push((relative, why)),
                    Outcome::Failed(error) => failed.push((relative, error)),
                }
                let updated = this.update(cx, |this, cx| {
                    if let Some(bulk) = &mut this.subs.bulk {
                        bulk.done += 1;
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
            this.update_in(cx, |this, window, cx| {
                let total = this.subs.bulk.take().map_or(0, |bulk| bulk.total);
                let done = total - failed.len() - skipped.len();
                let mut text = tf!("{n} 个子仓库已完成", n = done);
                if !failed.is_empty() {
                    text.push_str(&tf!("，{n} 个失败", n = failed.len()));
                }
                if !skipped.is_empty() {
                    text.push_str(&tf!("，{n} 个已跳过", n = skipped.len()));
                }
                if no_upstream > 0 {
                    text.push_str(&tf!("，{n} 个未设置上游，已跳过", n = no_upstream));
                }
                if detached > 0 {
                    text.push_str(&tf!("，{n} 个不在分支上，已跳过", n = detached));
                }
                if failed.is_empty() && skipped.is_empty() {
                    window.push_notification(Notification::success(text).title(label), cx);
                } else {
                    // 失败的、跳过的逐个写原因；有失败的才算警告
                    let mut listed: Vec<&(String, String)> =
                        failed.iter().chain(&skipped).collect();
                    listed.sort();
                    let detail: Vec<String> = listed
                        .iter()
                        .map(|(path, why)| format!("{path}\n{}\n", why.trim()))
                        .collect();
                    let note = match failed.is_empty() {
                        true => Notification::info(text),
                        false => Notification::warning(text),
                    };
                    let note = note.title(label).autohide(false);
                    let note = super::ops::with_detail(note, label, &detail.join("\n"));
                    window.push_notification(note, cx);
                }
                this.load_sub_summaries(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// 总览表里显示的子仓库：过了过滤框、「有改动」的筛选的。
    fn shown_subs(&self, cx: &App) -> Vec<String> {
        let needle = self.subs.filter.read(cx).value().trim().to_lowercase();
        self.subs
            .paths
            .iter()
            .filter(|path| needle.is_empty() || path.to_lowercase().contains(&needle))
            .filter(|path| {
                !self.subs.only_changed
                    || self
                        .subs
                        .summaries
                        .get(*path)
                        .is_some_and(|s| s.changes > 0)
            })
            .cloned()
            .collect()
    }

    /// 子仓库总览：上面一条工具，下面是表。
    pub(super) fn render_subs(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, bg) = (theme.border, theme.muted_foreground, theme.background);
        let total = self.subs.paths.len();
        let changed = self
            .subs
            .paths
            .iter()
            .filter(|path| {
                self.subs
                    .summaries
                    .get(*path)
                    .is_some_and(|s| s.changes > 0)
            })
            .count();
        let shown = self.shown_subs(cx);
        let busy = self.subs.bulk.is_some();
        let progress = self
            .subs
            .bulk
            .as_ref()
            .map(|bulk| {
                tf!(
                    "正在{label}：{done}/{total}",
                    label = bulk.label,
                    done = bulk.done,
                    total = bulk.total
                )
            })
            .or_else(|| {
                self.subs
                    .scanning
                    .then(|| tr("正在查找子仓库…").to_string())
            });
        let bar = h_flex()
            .flex_none()
            .h(px(44.))
            .px(px(12.))
            .gap(px(8.))
            .border_b_1()
            .border_color(border)
            .child(
                div().w(px(220.)).flex_shrink(1.).min_w(px(100.)).child(
                    Input::new(&self.subs.filter)
                        .small()
                        .cleanable(true)
                        .prefix(ui::icon(IconName::Search).text_color(muted)),
                ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(2.))
                    .child(
                        Button::new("subs-all")
                            .ghost()
                            .small()
                            .label(tf!("全部 {n}", n = total))
                            .selected(!self.subs.only_changed)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.subs.only_changed = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("subs-changed")
                            .ghost()
                            .small()
                            .label(tf!("有改动 {n}", n = changed))
                            .selected(self.subs.only_changed)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.subs.only_changed = true;
                                cx.notify();
                            })),
                    ),
            )
            .child(div().flex_1().min_w(px(4.)))
            .children(progress.map(|text| {
                ui::clipped("subs-progress", text)
                    .flex_shrink(1.)
                    .text_xs()
                    .text_color(muted)
            }))
            .child(
                Button::new("subs-fetch")
                    .small()
                    .flex_none()
                    .icon(ui::icon(IconName::ArrowDownToDot))
                    .label(tr("全部获取"))
                    .tooltip(tr("对每个子仓库执行 git fetch --all"))
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| this.bulk_subs(false, window, cx))),
            )
            .child(
                Button::new("subs-pull")
                    .small()
                    .flex_none()
                    .icon(ui::icon(IconName::ArrowDownToLine))
                    .label(tr("全部拉取"))
                    .tooltip(tr("对每个子仓库执行 git pull --ff-only：仅快进，不合并"))
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| this.bulk_subs(true, window, cx))),
            )
            .child(
                Button::new("subs-rescan")
                    .ghost()
                    .small()
                    .flex_none()
                    .icon(ui::icon(IconName::RotateCw))
                    .tooltip(tr("重新扫描子仓库"))
                    .disabled(self.subs.scanning)
                    .on_click(cx.listener(|this, _, _, cx| this.scan_subs(cx))),
            );
        let head = |label: &'static str| {
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(muted)
                .child(label)
        };
        let header = h_flex()
            .flex_none()
            .h(px(28.))
            .px(px(12.))
            .gap(px(12.))
            .border_b_1()
            .border_color(border)
            .child(head(tr("仓库")).flex_1().min_w(px(160.)))
            .child(head(tr("分支")).w(px(170.)).flex_none())
            .child(head(tr("改动")).w(px(48.)).flex_none())
            .child(head(tr("同步")).w(px(88.)).flex_none())
            .child(head(tr("最近提交")).flex_1().min_w(px(120.)))
            .child(div().w(px(56.)).flex_none());
        let body: AnyElement = if shown.is_empty() {
            let text = match (total, self.subs.scanning) {
                (0, true) => tr("正在查找子仓库…"),
                (0, false) => tr("未找到子仓库"),
                _ => tr("没有符合条件的子仓库"),
            };
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child(text)
                .into_any_element()
        } else {
            let rows: Vec<AnyElement> = shown
                .iter()
                .enumerate()
                .map(|(ix, path)| self.render_sub_row(ix, path, cx))
                .collect();
            v_flex()
                .id("subs-rows")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .children(rows)
                .when(self.subs.truncated, |list| {
                    list.child(
                        div()
                            .px(px(12.))
                            .py(px(8.))
                            .text_xs()
                            .text_color(muted)
                            .child(tr("目录过多，子仓库未全部列出。")),
                    )
                })
                .into_any_element()
        };
        v_flex()
            .size_full()
            .bg(bg)
            .child(bar)
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_sub_row(&self, ix: usize, path: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, hover, accent, warning, danger) = (
            theme.border,
            theme.muted_foreground,
            theme.list_hover,
            theme.primary,
            theme.warning,
            theme.danger,
        );
        let summary = self.subs.summaries.get(path);
        let dir = folder(path).to_string();
        let name_cell = v_flex()
            .flex_1()
            .min_w(px(160.))
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(
                        ui::icon(IconName::FolderGit2)
                            .flex_none()
                            .text_color(accent),
                    )
                    .child(
                        ui::clipped_with_tip(
                            ("sub-name", ix),
                            leaf(path).to_string(),
                            path.to_string(),
                        )
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM),
                    ),
            )
            // 和名字对齐（图标 14、空 6）。缩进放在外面一层：`clipped` 拿自己的宽度和文字比
            .when(!dir.is_empty(), |cell| {
                cell.child(
                    div().pl(px(20.)).min_w_0().child(
                        ui::clipped(("sub-dir", ix), dir)
                            .text_size(px(11.))
                            .text_color(muted),
                    ),
                )
            });
        let dash = || div().text_color(muted).child("—");
        let (branch, changes, sync, last): (AnyElement, AnyElement, AnyElement, AnyElement) =
            match summary {
                None => (
                    dash().into_any_element(),
                    div().into_any_element(),
                    div().into_any_element(),
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(tr("正在读取…"))
                        .into_any_element(),
                ),
                Some(Summary {
                    error: Some(error), ..
                }) => (
                    dash().into_any_element(),
                    div().into_any_element(),
                    div().into_any_element(),
                    ui::clipped(("sub-error", ix), error.clone())
                        .text_xs()
                        .text_color(danger)
                        .into_any_element(),
                ),
                Some(s) => {
                    let branch = match (&s.branch, &s.head) {
                        (Some(branch), _) => branch.clone(),
                        (None, Some(head)) => {
                            tf!("分离的 HEAD（{head}）", head = &head[..head.len().min(8)])
                        }
                        (None, None) => tr("暂无提交").to_string(),
                    };
                    let on_branch = s.branch.is_some();
                    let branch = h_flex()
                        .gap(px(5.))
                        .min_w_0()
                        .child(
                            ui::icon_small(IconName::GitBranch)
                                .flex_none()
                                .text_color(if on_branch { muted } else { warning }),
                        )
                        .child(ui::clipped(("sub-branch", ix), branch).text_xs())
                        .into_any_element();
                    let changes = match s.changes {
                        0 => dash().text_xs().into_any_element(),
                        // 包一层：直接放进定宽的格子里会被拉成和格子一样宽
                        n => h_flex()
                            .child(ui::count_pill(n, true, cx))
                            .into_any_element(),
                    };
                    let mut sync = String::new();
                    if s.ahead > 0 {
                        sync.push_str(&format!("↑{}", s.ahead));
                    }
                    if s.behind > 0 {
                        if !sync.is_empty() {
                            sync.push(' ');
                        }
                        sync.push_str(&format!("↓{}", s.behind));
                    }
                    let sync = match (sync.is_empty(), s.upstream) {
                        (false, _) => div().text_xs().child(sync).into_any_element(),
                        (true, true) => dash().text_xs().into_any_element(),
                        (true, false) => div()
                            .id(("sub-no-upstream", ix))
                            .text_xs()
                            .text_color(muted)
                            .child(tr("无上游"))
                            .into_any_element(),
                    };
                    let last = match s.time {
                        0 => dash().text_xs().into_any_element(),
                        time => v_flex()
                            .min_w_0()
                            .child(ui::clipped(("sub-subject", ix), s.subject.clone()).text_xs())
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(muted)
                                    .child(super::history::format_time(time)),
                            )
                            .into_any_element(),
                    };
                    (branch, changes, sync, last)
                }
            };
        let (a, b, c) = (path.to_string(), path.to_string(), path.to_string());
        let actions = h_flex()
            .w(px(56.))
            .flex_none()
            .gap(px(2.))
            .child(
                Button::new(("sub-terminal", ix))
                    .ghost()
                    .xsmall()
                    .icon(ui::icon(IconName::SquareTerminal).text_color(muted))
                    .tooltip(tr("在此新建终端"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.enter_sub(&a, Then::NewTerminal, window, cx);
                    })),
            )
            .child(
                Button::new(("sub-files", ix))
                    .ghost()
                    .xsmall()
                    .icon(ui::icon(IconName::Files).text_color(muted))
                    .tooltip(tr("在文件页中打开"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.enter_sub(&b, Then::Files, window, cx);
                    })),
            );
        h_flex()
            .id(("sub-row", ix))
            .flex_none()
            .h(px(ROW_HEIGHT))
            .px(px(12.))
            .gap(px(12.))
            .border_b_1()
            .border_color(border.opacity(0.5))
            .cursor_pointer()
            .hover(|s| s.bg(hover))
            .on_click(
                cx.listener(move |this, _, window, cx| this.enter_sub(&c, Then::Stay, window, cx)),
            )
            .child(name_cell)
            .child(div().w(px(170.)).flex_none().min_w_0().child(branch))
            .child(div().w(px(48.)).flex_none().child(changes))
            .child(div().w(px(88.)).flex_none().child(sync))
            .child(div().flex_1().min_w(px(120.)).child(last))
            .child(actions)
            .context_menu(self.sub_menu(path, cx))
            .into_any_element()
    }

    /// 总览表里一行的右键菜单。
    fn sub_menu(&self, path: &str, cx: &mut Context<Self>) -> super::menus::MenuFn {
        let view = cx.entity().downgrade();
        let entry = self.sub_entry(path);
        let path = path.to_string();
        let app = self.app.clone();
        Box::new(move |menu, _, cx| {
            let go = |label: &'static str, then: Then| {
                let (view, path) = (view.clone(), path.clone());
                PopupMenuItem::new(label).on_click(move |_, window, cx| {
                    view.update(cx, |this, cx| this.enter_sub(&path, then, window, cx))
                        .ok();
                })
            };
            let mapped = entry.clone();
            let (view, entry, full) = (view.clone(), entry.clone(), entry.path.clone());
            let menu = menu
                .item(go(tr("进入此仓库"), Then::Stay))
                .item(go(tr("在此新建终端"), Then::NewTerminal))
                .item(go(tr("在文件页中打开"), Then::Files))
                .separator()
                .item(
                    PopupMenuItem::new(tr("在新标签页中打开")).on_click(move |_, _, cx| {
                        view.update(cx, |_, cx| cx.emit(RepoEvent::Open(entry.clone())))
                            .ok();
                    }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(tr("复制路径")).on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(full.clone()));
                    }),
                );
            crate::mapping::entry_items(menu, &app, &mapped, cx)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{folder, leaf};

    #[test]
    fn 子仓库的名字和所在的目录() {
        assert_eq!(leaf("packages/plugins/@org/plugin-a"), "plugin-a");
        assert_eq!(
            folder("packages/plugins/@org/plugin-a"),
            "packages/plugins/@org"
        );
        assert_eq!(leaf("plugin-a"), "plugin-a");
        assert_eq!(folder("plugin-a"), "");
    }
}
