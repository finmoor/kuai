//! 提交历史：提交图列表、提交详情、所选提交的改动。
//!
//! 上面有搜索框：按说明、作者、哈希找提交，不匹配的行变淡，回车跳到下一个匹配。
//! 搜索框左边可以改成按文件路径、按改动内容找：这两种交给 git（`log -- <路径>`、`log -S`），
//! 按回车才找，找到的提交单独列出来（[`History::found`]），清空搜索框回到全部提交。
//! 「文件历史」模式下只列出改过某个文件的提交，改动也只显示这个文件的。
//! 提交列表可以用键盘：上下键、PageUp、PageDown、Home、End 换选中的提交，Ctrl+C 复制它的哈希。
//!
//! 当前分支比上游多的提交（待推送）、少的提交（待拉取）在行的最前面标 ↑、↓，
//! 搜索框那一行右边写个数（[`RepoView::load_pending`]）。
//!
//! 第一次读历史时边读边显示：读到前 300 个提交就先画出来（布局和读完全部时的前一段一致，
//! 见 `fr_core::log::Commit::parents`），读完再整个换上。重读（比如提交之后）时等读完再换，
//! 免得滚动位置跳回顶上。

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fr_channel::StreamItem;
use fr_core::graph::Graph;
use fr_core::i18n::tr;
use fr_core::log::{self as gitlog, Commit};
use fr_core::show::{self, Details};
use fr_core::tf;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffFile, DiffFileStatus, DiffMode, DiffState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use super::compare::End;
use super::diff_search::Where;
use super::diff_tools;
use super::file_tree::{self, Row as ListRow};
use super::image_diff::{self, Blob, ImageDiff, Source};
use super::layout::Split;
use crate::{graph_paint, ui};

const ROW_HEIGHT: f32 = 26.;
/// 提交改的文件超过这么多个，文件清单上面才有过滤框
const FILTER_FROM: usize = 8;
const AUTHOR_WIDTH: f32 = 140.;
const DATE_WIDTH: f32 = 130.;
const SHA_WIDTH: f32 = 76.;
/// 行最前面标待推送、待拉取的那一列（有这样的提交时才有这一列）
const MARK_WIDTH: f32 = 14.;
/// 提交列表这一块比这窄时，待推送、待拉取的个数只写箭头和数字
const PENDING_WORDS_FROM: f32 = 860.;
/// 第一次读历史时，读到这么多个提交就先显示
const FIRST_PAGE: usize = 300;

/// 提交列表里列哪些分支上的提交。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum Scope {
    /// 所有分支、远程分支、标签（隐藏起来的除外）
    #[default]
    All,
    /// 当前分支，加上它的上游
    Current,
    /// 只看一个引用：完整的引用名，和显示的名字
    Only { full: String, name: String },
}

/// 搜索框搜什么。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum SearchBy {
    /// 说明、作者、哈希：在读到的提交里找，不匹配的变淡
    #[default]
    Text,
    /// 改动过路径里有这几个字的文件
    Path,
    /// 增加或删除的行里有这几个字
    Content,
    /// 这几个字出现的次数变了（`git log -S`）
    Occurrence,
}

impl SearchBy {
    const ALL: [SearchBy; 4] = [
        SearchBy::Text,
        SearchBy::Path,
        SearchBy::Content,
        SearchBy::Occurrence,
    ];

    fn label(self) -> &'static str {
        match self {
            SearchBy::Text => tr("说明、作者、哈希"),
            SearchBy::Path => tr("文件路径"),
            SearchBy::Content => tr("改动内容"),
            SearchBy::Occurrence => tr("出现次数变化"),
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            SearchBy::Text => tr("搜索提交…"),
            SearchBy::Path => tr("输入路径中的文字，按 Enter 搜索"),
            SearchBy::Content => tr("输入增加或删除的行中的文字，按 Enter 搜索"),
            SearchBy::Occurrence => tr("输入文字，查找其出现次数有变化的提交，按 Enter 搜索"),
        }
    }

    /// 要交给 git 找的是哪一种；在读到的提交里找的没有。
    fn find(self) -> Option<gitlog::Find> {
        match self {
            SearchBy::Text => None,
            SearchBy::Path => Some(gitlog::Find::Path),
            SearchBy::Content => Some(gitlog::Find::Content),
            SearchBy::Occurrence => Some(gitlog::Find::Occurrence),
        }
    }
}

pub(super) struct Loaded {
    commits: Arc<Vec<Commit>>,
    graph: Arc<Graph>,
    stats: SharedString,
}

pub(super) enum Load {
    /// 还没开始读
    Idle,
    /// 还没有任何提交
    Empty,
    Loading,
    Ready {
        data: Loaded,
        /// 只读了前一段，其余的还在读
        partial: bool,
    },
    Failed(SharedString),
}

enum Progress {
    Page(Loaded),
    Done(Result<Loaded, String>),
}

/// 一行要用到的主题颜色，每帧取一次。
#[derive(Clone, Copy)]
struct RowColors {
    selected: Hsla,
    hover: Hsla,
    muted: Hsla,
    border: Hsla,
    /// 引用标签的颜色：HEAD、本地分支、标签、远程分支
    head: Hsla,
    local: Hsla,
    tag: Hsla,
    remote: Hsla,
}

/// 选中的提交改了的一个文件。
/// 读回来的一个提交的详情和改动，在后台解析好的（见 [`parse_details`]）。
struct Parsed {
    details: Details,
    ws_note: Option<SharedString>,
    /// 补丁多大（字节）、解析花了多久：记日志用
    size: usize,
    took: Duration,
    text: String,
    files: Result<Vec<DiffFile>, SharedString>,
}

/// 解析读回来的提交。在后台调：整个文件、超长的行解析要好一会儿，原来在界面线程上做，大的改动
/// 一点开界面就卡住。`plain` 是忽略空白时另读的一份不忽略的，用来说藏了几行。
fn parse_details(bytes: Vec<u8>, plain: Option<Vec<u8>>) -> Parsed {
    let started = Instant::now();
    let (details, patch) = show::parse(&bytes);
    let ws_note = plain
        .as_deref()
        .map(|plain| diff_tools::ws_note(show::parse(plain).1, patch));
    let (text, files) = diff_tools::parse_patch(patch);
    Parsed {
        details,
        ws_note,
        size: patch.len(),
        took: started.elapsed(),
        text,
        files,
    }
}

struct ChangedFile {
    path: SharedString,
    /// 改名、复制前的路径
    orig: Option<SharedString>,
    letter: &'static str,
    additions: usize,
    deletions: usize,
}

pub(super) struct History {
    /// 提交列表的键盘焦点
    focus: FocusHandle,
    /// 等历史读完再定位的提交：从文件历史、逐行追溯跳过来时列表正在重读
    pending_reveal: Option<String>,
    /// 选中的提交改了哪些文件
    files: Vec<ChangedFile>,
    /// 改动的文件上面的过滤框（文件多的时候才显示）
    file_filter: Entity<InputState>,
    /// 改动的文件按树形显示时收起的目录
    folded: HashSet<String>,
    /// 下面的详情区收起来了
    collapsed: bool,
    /// 详情区的高度，按窗口高度算，每帧更新
    details_height: Pixels,
    /// 详情区左边一栏的宽度，按窗口宽度算
    details_width: Pixels,
    /// 窗口内容区的宽度：算一行里留给分支标签多宽、改动视图的文件标题多宽
    viewport_width: Pixels,
    /// 侧栏现在多宽
    side_width: f32,
    load: Load,
    load_task: Option<Task<()>>,
    /// 旧的历史还显示着，新的在后台读
    reloading: bool,
    selected: Option<usize>,
    /// 多选的提交（Ctrl、Shift 加点击，见 `multi_commit.rs`）：两个以上才算，按哈希记，列表重读了
    /// 还对得上
    pub(super) multi: Vec<String>,
    /// 点右键之前选中的提交：点右键会先选中那一行，菜单里「与选中的提交比较」说的是它
    marked: Option<String>,
    scroll: UniformListScrollHandle,
    /// 文件历史模式：只看改过这个文件的提交
    file: Option<String>,
    /// 列哪些分支上的提交
    scope: Scope,
    /// 改动里显示整个文件，不只是改动附近的几行
    whole_file: bool,
    search: Entity<InputState>,
    /// 搜索框搜什么（文件历史里一律按说明、作者、哈希）
    search_by: SearchBy,
    /// 交给 git 找过的：按什么找、找的字。有它的时候列表里只有找到的提交
    found: Option<(gitlog::Find, String)>,
    /// 当前分支比上游多的提交（待推送）
    unpushed: HashSet<Box<str>>,
    /// 上游比当前分支多的提交（待拉取）
    unpulled: HashSet<Box<str>>,
    /// 上面两份是按哪个 HEAD、哪个上游（名字和它指着的提交）算的：都没变就不重算
    pending_key: Option<(String, String, String)>,
    pending_task: Option<Task<()>>,
    /// 搜索的内容（小写）；空表示没在搜索
    needle: String,
    /// 匹配的提交的下标，从小到大
    matches: Vec<usize>,
    /// 现在跳到了第几个匹配
    match_pos: Option<usize>,
    details: Option<Details>,
    /// 详情里列的是这个提交时的全部文件（不是改动的文件），见 `commit_tree.rs`
    pub(super) all_files: bool,
    pub(super) tree: Option<super::commit_tree::CommitTree>,
    /// 全部文件里点开的文件：右边看它那时的内容
    pub(super) preview: Option<super::commit_tree::Preview>,
    diff: Entity<DiffState>,
    /// 现在显示的补丁（在改动中搜索用）
    diff_text: SharedString,
    diff_info: SharedString,
    /// 忽略空白时改动上面写的那句话（少显示了多少行）
    ws_note: Option<SharedString>,
    /// 点开了改动里的一张图片：哪个文件、它的图片对比。有它时右边显示它，不显示全部改动
    image: Option<(SharedString, Entity<ImageDiff>)>,
    diff_task: Option<Task<()>>,
    /// 最近一次构建可见行花的时间和行数，用来观察滚动时的开销
    last_rows_build: (Duration, usize),
    /// 设置 FR_M0_BENCH=1 时的自动滚动测速
    bench: Option<Bench>,
    hooks_applied: bool,
}

impl History {
    pub(super) fn new(
        window: &mut Window,
        cx: &mut Context<RepoView>,
    ) -> (Self, Vec<Subscription>) {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(SearchBy::Text.placeholder()));
        let file_filter = cx.new(|cx| InputState::new(window, cx).placeholder(tr("过滤文件…")));
        let subscriptions = vec![
            cx.subscribe_in(
                &search,
                window,
                |this, _, event: &InputEvent, _, cx| match event {
                    InputEvent::Change => this.search_changed(cx),
                    InputEvent::PressEnter { .. } => this.search_enter(cx),
                    _ => {}
                },
            ),
            cx.subscribe(&file_filter, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        let history = Self {
            focus: cx.focus_handle(),
            pending_reveal: None,
            files: Vec::new(),
            file_filter,
            folded: HashSet::new(),
            collapsed: false,
            details_height: px(340.),
            details_width: px(380.),
            viewport_width: px(1400.),
            side_width: super::layout::SIDEBAR_DEFAULT,
            load: Load::Idle,
            load_task: None,
            reloading: false,
            selected: None,
            multi: Vec::new(),
            marked: None,
            scroll: UniformListScrollHandle::new(),
            file: None,
            scope: Scope::All,
            whole_file: false,
            search,
            search_by: SearchBy::Text,
            found: None,
            unpushed: HashSet::new(),
            unpulled: HashSet::new(),
            pending_key: None,
            pending_task: None,
            needle: String::new(),
            matches: Vec::new(),
            match_pos: None,
            details: None,
            all_files: false,
            tree: None,
            preview: None,
            diff: cx.new(|cx| DiffState::new(Vec::new(), cx)),
            diff_text: SharedString::default(),
            diff_info: tr("选择一个提交以查看改动").into(),
            ws_note: None,
            image: None,
            diff_task: None,
            last_rows_build: (Duration::ZERO, 0),
            bench: None,
            hooks_applied: false,
        };
        (history, subscriptions)
    }

    pub(super) fn commit_at(&self, ix: usize) -> Option<&Commit> {
        match &self.load {
            Load::Ready { data, .. } => data.commits.get(ix),
            _ => None,
        }
    }

    /// 每帧记下窗口的宽度和各块的大小（怎么算的见 `layout::sizes`）。
    pub(super) fn set_layout(&mut self, viewport: Size<Pixels>, sizes: super::layout::Sizes) {
        self.details_height = px(sizes.details_height);
        self.details_width = px(sizes.details_width);
        self.side_width = sizes.sidebar;
        self.viewport_width = viewport.width;
    }

    /// 详情里正看着的提交（或贮藏）的哈希。
    pub(super) fn details_id(&self) -> Option<String> {
        self.details.as_ref().map(|d| d.id.to_string())
    }

    /// 选中的提交在列表里的位置。
    pub(super) fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    /// 选中的提交。
    pub(super) fn selected_commit(&self) -> Option<&Commit> {
        let Load::Ready { data, .. } = &self.load else {
            return None;
        };
        self.selected.and_then(|ix| data.commits.get(ix))
    }

    /// 第 `ix` 行的菜单里「选中的提交」是哪个：这一行不是选中的，就是现在选中的那个；
    /// 这一行已经选中了（点右键选中的），就是在那之前选中的。
    pub(super) fn marked_for(&self, ix: usize) -> Option<String> {
        match self.selected == Some(ix) {
            true => self.marked.clone(),
            false => self.selected_commit().map(|commit| commit.id.to_string()),
        }
    }

    /// 搜索框现在按什么搜：文件历史里只有改过那个文件的提交，一律在里面按说明、作者、哈希找。
    fn search_by(&self) -> SearchBy {
        match self.file {
            Some(_) => SearchBy::Text,
            None => self.search_by,
        }
    }

    /// 行最前面要不要多一列标待推送、待拉取。
    fn has_marks(&self) -> bool {
        !self.unpushed.is_empty() || !self.unpulled.is_empty()
    }

    /// 提交在列表里的位置（越往后越早）。
    pub(super) fn position(&self, id: &str) -> Option<usize> {
        match &self.load {
            Load::Ready { data, .. } => data.commits.iter().position(|c| &*c.id == id),
            _ => None,
        }
    }
}

const BENCH_SCROLL_FRAMES: usize = 600;
const BENCH_JUMP_FRAMES: usize = 300;
const BENCH_ROWS_PER_FRAME: usize = 10;

/// 自动滚动测速：先每帧往下滚固定行数，再随机跳转（模拟拖动滚动条），记录每帧间隔。
#[derive(Default)]
struct Bench {
    frame: usize,
    pos: usize,
    seed: u64,
    last_frame: Option<Instant>,
    scroll_intervals: Vec<Duration>,
    jump_intervals: Vec<Duration>,
    row_builds: Vec<Duration>,
}

impl RepoView {
    /// 读提交历史。第一次读时先显示前一页；重读时等读完再换。
    pub(super) fn reload_history(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        self.load_pending(cx);
        if self.status.as_ref().is_some_and(|s| s.head.is_none()) {
            // 还没有任何提交：git log HEAD 会报错
            self.history.load = Load::Empty;
            self.history.load_task = None;
            self.history.reloading = false;
            return;
        }
        let showing = matches!(self.history.load, Load::Ready { .. });
        if !showing {
            self.history.load = Load::Loading;
        }
        self.history.reloading = showing;
        // 只看的那个引用已经不在了（分支删了）：回到所有分支
        if let Scope::Only { full, .. } = &self.history.scope
            && !self.ref_exists(full)
        {
            self.history.scope = Scope::All;
        }
        // 文件历史：只要改过这个文件的提交，跟着改名往回找
        // 交给 git 找出来的提交也一样：互相不一定是父子，连成一条线来画
        let file_mode = self.history.file.is_some() || self.history.found.is_some();
        let log_args = match (&self.history.file, &self.history.found) {
            (Some(path), _) => gitlog::file_log_args(path),
            (None, Some((find, needle))) => {
                gitlog::find_log_args(&self.log_scope(), &self.hidden_refs(cx), *find, needle)
            }
            (None, None) => gitlog::scoped_log_args(&self.log_scope(), &self.hidden_refs(cx)),
        };
        let mut stream = channel.git_stream(&self.entry.path, log_args);
        let (tx, mut rx) = mpsc::unbounded();
        // 收数据、解析和布局都在后台线程；界面线程只在第一页和读完时各更新一次
        let worker = cx.background_executor().spawn(async move {
            let started = Instant::now();
            let mut buf = Vec::with_capacity(1 << 20);
            let mut page_sent = showing;
            let outcome = loop {
                match stream.next().await {
                    Some(Ok(StreamItem::Chunk(chunk))) => {
                        buf.extend_from_slice(&chunk);
                        if !page_sent && let Some(end) = gitlog::nth_record_end(&buf, FIRST_PAGE) {
                            page_sent = true;
                            let page =
                                build_loaded(&buf[..end], started.elapsed(), None, file_mode);
                            let _ = tx.unbounded_send(Progress::Page(page));
                        }
                    }
                    Some(Ok(StreamItem::Progress(_))) => {}
                    Some(Ok(StreamItem::Done(out))) => {
                        break if out.success() {
                            Ok(build_loaded(
                                &buf,
                                started.elapsed(),
                                Some(out.elapsed),
                                file_mode,
                            ))
                        } else {
                            Err(out.stderr)
                        };
                    }
                    Some(Err(err)) => break Err(format!("{err:#}")),
                    None => break Err(tr("读取已中断").into()),
                }
            };
            let _ = tx.unbounded_send(Progress::Done(outcome));
        });
        self.history.load_task = Some(cx.spawn(async move |this, cx| {
            // 这个任务被丢弃（比如又开始重读）时，后台的读取跟着取消
            let _worker = worker;
            while let Some(progress) = rx.next().await {
                let applied = this.update(cx, |this, cx| this.apply_history_progress(progress, cx));
                if applied.is_err() {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn apply_history_progress(&mut self, progress: Progress, cx: &mut Context<Self>) {
        match progress {
            Progress::Page(data) => {
                log::info!("{}：{}", self.entry.path, data.stats);
                self.history.load = Load::Ready {
                    data,
                    partial: true,
                };
                // 先显示的这一页也要算搜索的匹配，不然搜索框里有字时会说「没有匹配」
                self.update_search(false, cx);
                self.try_pending_reveal(false, cx);
            }
            Progress::Done(Ok(data)) => {
                log::info!("{}：{}", self.entry.path, data.stats);
                // 按用户现在选中的提交恢复高亮：读的这段时间里他可能已经换了选中
                let current = self.history.selected_commit().map(|c| c.id.to_string());
                let found = current
                    .as_deref()
                    .and_then(|id| data.commits.iter().position(|c| &*c.id == id));
                self.history.load = Load::Ready {
                    data,
                    partial: false,
                };
                self.history.reloading = false;
                match (&current, found) {
                    (_, Some(ix)) => self.history.selected = Some(ix),
                    // 选中的提交没了（比如修改了上次提交）
                    (Some(_), None) => self.clear_commit_selection(cx),
                    // 什么都没选（刚打开仓库）：选中 HEAD，没有就第一个提交，下面的详情不空着。
                    // 正要定位到别的提交、正在看贮藏这类列表外的东西时不动
                    (None, None)
                        if self.history.pending_reveal.is_none()
                            && self.history.details.is_none() =>
                    {
                        let head = self.status.as_ref().and_then(|s| s.head.clone());
                        let first = match &self.history.load {
                            Load::Ready { data, .. } if !data.commits.is_empty() => Some(
                                head.and_then(|head| {
                                    data.commits.iter().position(|c| *c.id == *head)
                                })
                                .unwrap_or(0),
                            ),
                            _ => None,
                        };
                        if let Some(ix) = first {
                            self.select_commit(ix, cx);
                        }
                    }
                    (None, None) => {}
                }
                self.update_search(false, cx);
                self.try_pending_reveal(true, cx);
                self.apply_test_hooks(cx);
            }
            Progress::Done(Err(err)) => {
                self.history.reloading = false;
                self.history.load = Load::Failed(tf!("读取提交历史失败：{err}", err).into());
            }
        }
        cx.notify();
    }

    pub(super) fn clear_commit_selection(&mut self, cx: &mut Context<Self>) {
        self.history.selected = None;
        self.history.details = None;
        self.history.files.clear();
        self.history.diff_task = None;
        self.history.ws_note = None;
        self.history.image = None;
        self.history.diff_info = tr("选择一个提交以查看改动").into();
        self.history.diff_text = SharedString::default();
        self.history
            .diff
            .update(cx, |state, cx| state.set_files(Vec::new(), cx));
        self.update_diff_search(Where::History, false, cx);
    }

    pub(super) fn history_diff_state(&self) -> Entity<DiffState> {
        self.history.diff.clone()
    }

    pub(super) fn history_diff_text(&self) -> SharedString {
        self.history.diff_text.clone()
    }

    /// 键盘焦点是不是在提交的改动里（或者它上面的搜索条里）。
    pub(super) fn history_diff_focused(&self, window: &Window, cx: &App) -> bool {
        self.history
            .diff
            .focus_handle(cx)
            .contains_focused(window, cx)
            || self.diff_search.at(Where::History).is_focused(window, cx)
    }

    /// 收起点开的那张图片，回到全部改动。
    pub(super) fn close_commit_image(&mut self) {
        self.history.image = None;
    }

    /// 测试钩子：FR_M0_SELECT=<说明里的文字> 自动选中第一个匹配的提交；FR_M0_BENCH=1 开始滚动测速。
    fn apply_test_hooks(&mut self, cx: &mut Context<Self>) {
        if self.history.hooks_applied {
            return;
        }
        self.history.hooks_applied = true;
        let Load::Ready { data, .. } = &self.history.load else {
            return;
        };
        if let Ok(needle) = std::env::var("FR_M0_SELECT")
            && let Some(ix) = data
                .commits
                .iter()
                .position(|c| c.subject.contains(&needle))
        {
            self.history
                .scroll
                .scroll_to_item(ix, ScrollStrategy::Center);
            self.select_commit(ix, cx);
        }
        if std::env::var("FR_M0_BENCH").is_ok_and(|v| v == "1") {
            self.history.bench = Some(Bench {
                seed: 0x2545_f491_4f6c_dd1d,
                ..Default::default()
            });
        }
    }

    /// 在提交图里找到并选中一个提交（点侧栏的分支、标签时用）。
    pub(super) fn reveal_commit(&mut self, id: &str, cx: &mut Context<Self>) {
        // 列表还没读完（刚从文件历史回到全部提交、正在重读）：先记着，读到了再定位
        let (found, complete) = match &self.history.load {
            Load::Ready { data, partial } => (
                data.commits.iter().position(|c| &*c.id == id),
                !*partial && !self.history.reloading,
            ),
            _ => (None, false),
        };
        match found {
            Some(ix) => {
                self.history.pending_reveal = None;
                self.history
                    .scroll
                    .scroll_to_item(ix, ScrollStrategy::Center);
                self.select_commit(ix, cx);
            }
            // 列表里只有 git 找出来的提交，要看的不在里面：回到全部提交再定位
            None if self.history.found.is_some() => {
                self.history.found = None;
                self.restart_history(cx);
                self.history.pending_reveal = Some(id.to_string());
            }
            None if complete => {
                self.history.pending_reveal = None;
                self.clear_commit_selection(cx);
                self.history.diff_info = not_found_text(id);
            }
            None => self.history.pending_reveal = Some(id.to_string()),
        }
    }

    /// 读当前分支和上游各多出哪些提交（待推送、待拉取）。HEAD 和上游都没变就不读。
    fn load_pending(&mut self, cx: &mut Context<Self>) {
        let status = self.status.as_ref();
        let head = status.and_then(|s| s.head.clone());
        // 上游要是远程分支，而且还在：和「仅当前分支」带上游时一样
        let upstream = status
            .and_then(|s| s.upstream.as_deref())
            .and_then(|name| self.refs.remotes.iter().find(|b| b.name == name))
            .map(|branch| (branch.name.clone(), branch.target.clone()));
        let (Some(channel), Some(head), Some((upstream, target))) =
            (self.channel.clone(), head, upstream)
        else {
            self.history.pending_key = None;
            self.history.pending_task = None;
            self.history.unpushed.clear();
            self.history.unpulled.clear();
            return;
        };
        let key = (head, upstream.clone(), target);
        if self.history.pending_key.as_ref() == Some(&key) {
            return;
        }
        self.history.pending_key = Some(key);
        let read = channel.git(&self.entry.path, gitlog::pending_args(&upstream));
        self.history.pending_task = Some(cx.spawn(async move |this, cx| {
            let out = read.await.and_then(|out| out.ok());
            this.update(cx, |this, cx| {
                let (ahead, behind) = match out {
                    Ok(out) => gitlog::parse_pending(&out),
                    Err(err) => {
                        // 读不出来就不标，下次刷新再试
                        log::warn!("读取待推送、待拉取的提交失败：{err:#}");
                        this.history.pending_key = None;
                        Default::default()
                    }
                };
                this.history.unpushed = ahead.into_iter().collect();
                this.history.unpulled = behind.into_iter().collect();
                cx.notify();
            })
            .ok();
        }));
    }

    /// Ctrl+0：回到 HEAD 那个提交并选中，焦点放到提交列表上。
    pub fn go_to_head(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_history_view(window, cx);
        let Some(head) = self.status.as_ref().and_then(|s| s.head.clone()) else {
            return;
        };
        // 列表里只有 git 找出来的提交、HEAD 不在里面：连搜索框里的字一起清掉，回到全部提交
        if self.history.found.is_some() && self.history.position(&head).is_none() {
            self.clear_search(window, cx);
            self.restart_history(cx);
        }
        // 只看着别的分支时 HEAD 不在列表里：回到所有分支
        if matches!(self.history.scope, Scope::Only { .. })
            && self.history.found.is_none()
            && self.history.position(&head).is_none()
        {
            self.history.scope = Scope::All;
            self.restart_history(cx);
        }
        self.reveal_commit(&head, cx);
    }

    /// 历史读到一段或读完时，看看等着定位的提交到了没有。`last` 为真表示已经读完。
    fn try_pending_reveal(&mut self, last: bool, cx: &mut Context<Self>) {
        let Some(id) = self.history.pending_reveal.clone() else {
            return;
        };
        let Load::Ready { data, .. } = &self.history.load else {
            return;
        };
        match data.commits.iter().position(|c| *c.id == *id) {
            Some(ix) => {
                self.history.pending_reveal = None;
                self.history
                    .scroll
                    .scroll_to_item(ix, ScrollStrategy::Center);
                self.select_commit(ix, cx);
            }
            None if last => {
                self.history.pending_reveal = None;
                self.history.diff_info = not_found_text(&id);
            }
            None => {}
        }
    }

    /// 文件历史：提交列表只留改过这个文件的提交。
    pub(super) fn show_file_history(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.history.file = Some(path);
        self.blame = None;
        self.sidebar_select_history();
        self.clear_search(window, cx);
        self.restart_history(cx);
    }

    pub(super) fn close_file_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.history.file.take().is_some() {
            self.clear_search(window, cx);
            self.restart_history(cx);
        }
    }

    /// 离开文件历史和逐行追溯，回到全部提交。点侧栏的分支、标签、贮藏时先做这个，
    /// 不然要找的提交不在列表里，贮藏的改动也会被限定成一个文件。
    pub(super) fn leave_file_views(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.blame = None;
        self.close_file_history(window, cx);
    }

    /// 进出文件历史时清掉搜索：搜索词是给另一份列表用的，留着会把提交都变淡。
    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.history.search.read(cx).value().is_empty() {
            self.history
                .search
                .update(cx, |search, cx| search.set_value("", window, cx));
        }
        // 文件历史里只按说明、作者、哈希搜，提示跟着换
        let by = self.history.search_by();
        self.history.search.update(cx, |search, cx| {
            search.set_placeholder(by.placeholder(), window, cx)
        });
        self.history.found = None;
        self.history.needle.clear();
        self.history.matches.clear();
        self.history.match_pos = None;
    }

    /// 提交列表上的按键。
    fn history_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.history.focus.is_focused(window) {
            return;
        }
        let Load::Ready { data, .. } = &self.history.load else {
            return;
        };
        let count = data.commits.len();
        if count == 0 {
            return;
        }
        const PAGE: usize = 15;
        let current = self.history.selected;
        let modifiers = event.keystroke.modifiers;
        if event.keystroke.key == "escape" && self.history.multi.len() > 1 {
            cx.stop_propagation();
            self.clear_multi_commits();
            cx.notify();
            return;
        }
        if event.keystroke.key == "c"
            && (modifiers.control || modifiers.platform)
            && !modifiers.shift
            && !modifiers.alt
            && self.history.multi.len() > 1
        {
            cx.stop_propagation();
            let ids: Vec<String> = self.picked_commits().into_iter().map(|p| p.id).collect();
            let text = tf!("已复制 {n} 个提交的哈希", n = ids.len());
            cx.write_to_clipboard(ClipboardItem::new_string(ids.join("\n")));
            window.push_notification(Notification::success(text), cx);
            return;
        }
        if event.keystroke.key == "c"
            && (modifiers.control || modifiers.platform)
            && !modifiers.shift
            && !modifiers.alt
        {
            // Ctrl+C：复制选中提交的哈希
            if let Some(id) = self.history.selected_commit().map(|c| c.id.to_string()) {
                cx.stop_propagation();
                let short = id[..8.min(id.len())].to_string();
                cx.write_to_clipboard(ClipboardItem::new_string(id));
                window.push_notification(
                    Notification::success(tf!("已复制提交哈希 {id}", id = short)),
                    cx,
                );
            }
            return;
        }
        let next = match event.keystroke.key.as_str() {
            "down" => current.map_or(0, |ix| (ix + 1).min(count - 1)),
            "up" => current.map_or(0, |ix| ix.saturating_sub(1)),
            "pagedown" => current.map_or(0, |ix| (ix + PAGE).min(count - 1)),
            "pageup" => current.map_or(0, |ix| ix.saturating_sub(PAGE)),
            "home" => 0,
            "end" => count - 1,
            _ => return,
        };
        cx.stop_propagation();
        if Some(next) != current {
            self.history
                .scroll
                .scroll_to_item(next, ScrollStrategy::Nearest);
            self.select_commit(next, cx);
        }
    }

    /// 切到 Git 页的提交历史，把键盘焦点放到提交列表上（Ctrl+2）。
    pub fn show_history_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_page(super::Page::Git, window, cx);
        self.leave_file_views(window, cx);
        self.sidebar_select_history();
        self.history.focus.focus(window, cx);
        cx.notify();
    }

    /// 选一个已跟踪的文件，看它的历史或逐行追溯：没改动的文件只能从这里进。
    fn pick_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let list = channel.git(&self.entry.path, vec!["ls-files".into(), "-z".into()]);
        let picker = cx.new(|cx| super::file_picker::FilePicker::new(list, window, cx));
        cx.subscribe_in(
            &picker,
            window,
            |this, _, event: &super::file_picker::Picked, window, cx| {
                window.close_dialog(cx);
                if event.blame {
                    this.show_blame(event.path.clone(), cx);
                } else {
                    this.show_file_history(event.path.clone(), window, cx);
                }
            },
        )
        .detach();
        super::file_picker::open(picker, window, cx);
    }

    /// 这个完整的引用名现在还有没有。
    fn ref_exists(&self, full: &str) -> bool {
        let has = |list: &[fr_core::refs::Branch], name: &str| list.iter().any(|b| b.name == name);
        if let Some(name) = full.strip_prefix("refs/heads/") {
            has(&self.refs.branches, name)
        } else if let Some(name) = full.strip_prefix("refs/remotes/") {
            has(&self.refs.remotes, name)
        } else if let Some(name) = full.strip_prefix("refs/tags/") {
            self.refs.tags.iter().any(|t| t.name == name)
        } else {
            false
        }
    }

    /// 读历史时给 git 的范围。
    fn log_scope(&self) -> gitlog::Scope {
        match &self.history.scope {
            Scope::All => gitlog::Scope::All,
            Scope::Current => {
                let mut refs = vec!["HEAD".to_string()];
                // 带上上游：看得到还没拉下来的提交。上游已经被删掉的不带，不然 git 会报错
                let upstream = self.status.as_ref().and_then(|s| s.upstream.as_deref());
                if let Some(upstream) = upstream
                    && self.refs.remotes.iter().any(|b| b.name == upstream)
                {
                    refs.push(format!("refs/remotes/{upstream}"));
                }
                gitlog::Scope::Refs(refs)
            }
            Scope::Only { full, .. } => gitlog::Scope::Refs(vec![full.clone()]),
        }
    }

    /// 提交列表里隐藏了哪些引用（完整的名字）。
    pub(super) fn hidden_refs(&self, cx: &App) -> Vec<String> {
        self.app
            .read(cx)
            .settings
            .memo(&self.entry)
            .map(|memo| memo.hidden_refs.clone())
            .unwrap_or_default()
    }

    /// 换一个范围看提交历史。
    pub(super) fn set_scope(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_file_views(window, cx);
        self.sidebar_select_history();
        if self.history.scope != scope {
            self.history.scope = scope;
            self.restart_history(cx);
        }
        cx.notify();
    }

    /// 现在是不是只看这一个引用。
    pub(super) fn is_only(&self, full: &str) -> bool {
        matches!(&self.history.scope, Scope::Only { full: only, .. } if only == full)
    }

    /// 在提交列表里隐藏一个引用，或者取消隐藏。
    pub(super) fn toggle_hidden(&mut self, full: String, cx: &mut Context<Self>) {
        let entry = self.entry.clone();
        self.app.update(cx, |app, cx| {
            app.edit_memo(&entry, cx, |memo| {
                if memo.hidden_refs.contains(&full) {
                    memo.hidden_refs.retain(|hidden| *hidden != full);
                } else {
                    memo.hidden_refs.push(full);
                }
            })
        });
        // 隐藏只在列全部分支时起作用；这时才用得着重读
        if self.history.scope == Scope::All && self.history.file.is_none() {
            self.restart_history(cx);
        }
        cx.notify();
    }

    /// 一个都不隐藏了。
    fn show_all_hidden(&mut self, cx: &mut Context<Self>) {
        let entry = self.entry.clone();
        self.app.update(cx, |app, cx| {
            app.edit_memo(&entry, cx, |memo| memo.hidden_refs.clear())
        });
        if self.history.scope == Scope::All && self.history.file.is_none() {
            self.restart_history(cx);
        }
        cx.notify();
    }

    /// 提交列表上面「看哪些分支」的按钮和它的菜单。
    fn render_scope_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let scope = self.history.scope.clone();
        let hidden = self.hidden_refs(cx);
        let current = self.status.as_ref().and_then(|s| s.branch.clone());
        let mut label: String = match &scope {
            Scope::All => tr("所有分支").into(),
            Scope::Current => match &current {
                Some(branch) => tf!("仅显示 {branch}", branch),
                None => tr("仅当前 HEAD").into(),
            },
            Scope::Only { name, .. } => tf!("仅显示 {name}", name),
        };
        if scope == Scope::All && !hidden.is_empty() {
            label = tf!("{label}（已隐藏 {n} 个）", label, n = hidden.len());
        }
        let filtered = scope != Scope::All || !hidden.is_empty();
        let accent = cx.theme().primary;
        // 不加悬停提示：提示出现在按钮下面，会盖住弹出的菜单
        Button::new("history-scope")
            .small()
            .ghost()
            .icon(if filtered {
                ui::icon(IconName::ListFilter).text_color(accent)
            } else {
                ui::icon(IconName::ListFilter)
            })
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                let pick = |label: SharedString, to: Scope, on: bool| {
                    let view = view.clone();
                    PopupMenuItem::new(label)
                        .checked(on)
                        .on_click(move |_, window, cx| {
                            view.update(cx, |this, cx| this.set_scope(to.clone(), window, cx))
                                .ok();
                        })
                };
                menu = menu
                    .item(pick(tr("所有分支").into(), Scope::All, scope == Scope::All))
                    .item(pick(
                        tr("仅当前分支（及其上游）").into(),
                        Scope::Current,
                        scope == Scope::Current,
                    ));
                if let Scope::Only { name, .. } = &scope {
                    menu = menu.item(pick(tf!("仅显示 {name}", name).into(), scope.clone(), true));
                }
                if !hidden.is_empty() {
                    menu = menu
                        .separator()
                        .item(PopupMenuItem::label(tr("已隐藏（点击取消隐藏）")));
                    for full in hidden.iter().take(12) {
                        let (view, full) = (view.clone(), full.clone());
                        menu =
                            menu.item(PopupMenuItem::new(short_ref(&full).to_string()).on_click(
                                move |_, _, cx| {
                                    view.update(cx, |this, cx| {
                                        this.toggle_hidden(full.clone(), cx)
                                    })
                                    .ok();
                                },
                            ));
                    }
                    let view = view.clone();
                    menu = menu.item(PopupMenuItem::new(tr("全部取消隐藏")).on_click(
                        move |_, _, cx| {
                            view.update(cx, |this, cx| this.show_all_hidden(cx)).ok();
                        },
                    ));
                }
                menu
            })
    }

    /// 换了要看的范围（全部提交或某个文件）：清掉选中，从头读。
    fn restart_history(&mut self, cx: &mut Context<Self>) {
        self.clear_commit_selection(cx);
        self.history.load = Load::Idle;
        self.history.matches.clear();
        self.history.match_pos = None;
        self.history.scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.reload_history(cx);
        cx.notify();
    }

    /// 按搜索框里的内容重新找匹配的提交。`jump` 为真时滚到第一个匹配。
    fn update_search(&mut self, jump: bool, cx: &mut Context<Self>) {
        // 交给 git 找的时候列表里已经只有找到的提交，不再按搜索框里的字变淡
        let needle = match self.history.search_by() {
            SearchBy::Text => self.history.search.read(cx).value().trim().to_lowercase(),
            _ => String::new(),
        };
        self.history.matches = match &self.history.load {
            Load::Ready { data, .. } if !needle.is_empty() => data
                .commits
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    c.subject.to_lowercase().contains(&needle)
                        || c.author.to_lowercase().contains(&needle)
                        || c.id.starts_with(&needle)
                })
                .map(|(ix, _)| ix)
                .collect(),
            _ => Vec::new(),
        };
        self.history.needle = needle;
        if jump {
            self.history.match_pos = None;
            if let Some(&first) = self.history.matches.first() {
                self.history
                    .scroll
                    .scroll_to_item(first, ScrollStrategy::Center);
            }
        }
        cx.notify();
    }

    /// 搜索框里的字变了。
    fn search_changed(&mut self, cx: &mut Context<Self>) {
        if self.history.search_by() == SearchBy::Text {
            return self.update_search(true, cx);
        }
        // 交给 git 找的按回车才找；清空了搜索框就回到全部提交
        if self.history.found.is_some() && self.history.search.read(cx).value().trim().is_empty() {
            self.history.found = None;
            self.restart_history(cx);
        }
        cx.notify();
    }

    /// 在搜索框里按了回车。
    fn search_enter(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.history.search_by().find() else {
            return self.next_match(1, cx);
        };
        let needle = self.history.search.read(cx).value().trim().to_string();
        if needle.is_empty() {
            return;
        }
        let search = (find, needle);
        if self.history.found.as_ref() != Some(&search) {
            self.history.found = Some(search);
            return self.restart_history(cx);
        }
        // 已经找过这几个字了：再按回车往下选一个，到底绕回第一个
        let count = match &self.history.load {
            Load::Ready { data, .. } => data.commits.len(),
            _ => 0,
        };
        if count > 0 {
            let next = self.history.selected.map_or(0, |ix| (ix + 1) % count);
            self.history
                .scroll
                .scroll_to_item(next, ScrollStrategy::Nearest);
            self.select_commit(next, cx);
        }
    }

    /// 换一种搜法：搜索框里的字按新的搜法再搜一次。
    fn set_search_by(&mut self, by: SearchBy, window: &mut Window, cx: &mut Context<Self>) {
        self.history.search.focus_handle(cx).focus(window, cx);
        if self.history.search_by == by {
            return;
        }
        self.history.search_by = by;
        self.history.search.update(cx, |state, cx| {
            state.set_placeholder(by.placeholder(), window, cx)
        });
        let needle = self.history.search.read(cx).value().trim().to_string();
        let found = by
            .find()
            .filter(|_| !needle.is_empty())
            .map(|find| (find, needle));
        if self.history.found != found {
            self.history.found = found;
            self.restart_history(cx);
        }
        self.update_search(true, cx);
    }

    /// 搜索框左边选「搜什么」的按钮。
    fn render_search_by(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let current = self.history.search_by;
        Button::new("search-by")
            .small()
            .outline()
            .label(current.label())
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for by in SearchBy::ALL {
                    let view = view.clone();
                    menu = menu.item(
                        PopupMenuItem::new(by.label())
                            .checked(by == current)
                            .on_click(move |_, window, cx| {
                                view.update(cx, |this, cx| this.set_search_by(by, window, cx))
                                    .ok();
                            }),
                    );
                }
                menu
            })
    }

    /// 交给 git 找的时候，列表上面那一行写的话；没在这样找时没有这一行。
    fn find_info(&self, cx: &App) -> Option<SharedString> {
        let find = self.history.search_by().find()?;
        let typed = self.history.search.read(cx).value().trim().to_string();
        let Some((_, needle)) = self
            .history
            .found
            .as_ref()
            .filter(|(found, needle)| *found == find && *needle == typed)
        else {
            // 搜索框里的字还没找过
            return (!typed.is_empty()).then(|| tr("按 Enter 开始搜索").into());
        };
        let (count, reading) = match &self.history.load {
            Load::Idle | Load::Loading => (0, true),
            Load::Ready { data, partial } => (data.commits.len(), *partial),
            // 出错的话下面的状态行里写着
            Load::Empty | Load::Failed(_) => return None,
        };
        let text = match (find, reading || self.history.reloading, count) {
            (_, true, 0) => tr("正在查找…").into(),
            (_, true, n) => tf!("正在查找，已找到 {n} 个提交…", n),
            (gitlog::Find::Path, false, 0) => {
                tf!("没有改动过路径中含「{needle}」的文件的提交", needle)
            }
            (gitlog::Find::Path, false, n) => {
                tf!("改动过路径中含「{needle}」的文件的提交：{n} 个", needle, n)
            }
            (gitlog::Find::Content, false, 0) => {
                tf!("没有增删的行中含「{needle}」的提交", needle)
            }
            (gitlog::Find::Content, false, n) => {
                tf!("增删的行中含「{needle}」的提交：{n} 个", needle, n)
            }
            // 没找到时提一句另一种找法：只改了同一行别处的提交出现次数不变
            (gitlog::Find::Occurrence, false, 0) => {
                tf!(
                    "没有「{needle}」出现次数有变化的提交。只改动所在行其他部分的提交，可改用「改动内容」查找。",
                    needle
                )
            }
            (gitlog::Find::Occurrence, false, n) => {
                tf!("「{needle}」出现次数有变化的提交：{n} 个", needle, n)
            }
        };
        Some(text.into())
    }

    /// 搜索框那一行右边的「↑ 2 待推送　↓ 1 待拉取」。地方窄时只写箭头和数字，悬停看全。
    fn render_pending(&self, cx: &App) -> Option<AnyElement> {
        let (ahead, behind) = (self.history.unpushed.len(), self.history.unpulled.len());
        if ahead + behind == 0 {
            return None;
        }
        let theme = cx.theme();
        let (accent, muted) = (theme.blue, theme.muted_foreground);
        let room = f32::from(self.history.viewport_width) - self.history.side_width;
        let words = room >= PENDING_WORDS_FROM;
        let part = |id: &'static str, arrow: &'static str, n: usize, color: Hsla| {
            let (word, tip): (&str, String) = match arrow {
                "↑" => (tr("待推送"), tf!("{n} 个提交待推送：上游分支还没有", n)),
                _ => (tr("待拉取"), tf!("{n} 个提交待拉取：当前分支还没有", n)),
            };
            h_flex()
                .id(id)
                .flex_none()
                .gap(px(3.))
                .child(div().text_color(color).child(arrow))
                .child(div().child(n.to_string()))
                .when(words, |part| part.child(div().child(word)))
                .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
        };
        Some(
            h_flex()
                .flex_none()
                .gap(px(10.))
                .text_xs()
                .text_color(muted)
                .when(ahead > 0, |row| {
                    row.child(part("pending-push", "↑", ahead, accent))
                })
                .when(behind > 0, |row| {
                    row.child(part("pending-pull", "↓", behind, muted))
                })
                .into_any_element(),
        )
    }

    /// 跳到下一个（`step` 为 1）或上一个（-1）匹配的提交并选中。
    pub(super) fn next_match(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = self.history.matches.len() as isize;
        if count == 0 {
            return;
        }
        let pos = match self.history.match_pos {
            Some(pos) => (pos as isize + step).rem_euclid(count) as usize,
            None if step < 0 => (count - 1) as usize,
            None => 0,
        };
        self.history.match_pos = Some(pos);
        let ix = self.history.matches[pos];
        self.history
            .scroll
            .scroll_to_item(ix, ScrollStrategy::Center);
        self.select_commit(ix, cx);
    }

    /// 把键盘焦点放到搜索框（Ctrl+F）。
    pub(super) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_select_history();
        self.history.search.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// 提交图上分支、标签的小牌子：右键是侧栏里同一项的菜单；双击本地分支切换过去，双击远程分支
    /// 检出（建同名的本地分支跟踪它），和 Fork 一样。HEAD 那个牌子不接。
    fn wire_badge(
        &self,
        label: &RefLabel,
        badge: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let find = |list: &[fr_core::refs::Branch]| list.iter().position(|b| b.name == label.name);
        let side = match label.kind {
            RefKind::Current | RefKind::Local => {
                find(&self.refs.branches).map(super::sidebar::SideItem::Branch)
            }
            RefKind::Remote => find(&self.refs.remotes).map(super::sidebar::SideItem::Remote),
            RefKind::Tag => self
                .refs
                .tags
                .iter()
                .position(|t| t.name == label.name)
                .map(super::sidebar::SideItem::Tag),
            RefKind::Head => None,
        };
        let Some(side) = side else {
            return badge.into_any_element();
        };
        let (name, kind) = (label.name.clone(), label.kind);
        let badge = badge.on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            if event.click_count() != 2 {
                return;
            }
            match kind {
                RefKind::Local => {
                    cx.stop_propagation();
                    this.checkout_branch(&name, window, cx);
                }
                RefKind::Remote => {
                    cx.stop_propagation();
                    this.checkout_remote(&name, window, cx);
                }
                _ => {}
            }
        }));
        match self.sidebar_menu(side, cx) {
            Some(menu) => badge.context_menu(menu).into_any_element(),
            None => badge.into_any_element(),
        }
    }

    pub(super) fn select_commit(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Load::Ready { data, .. } = &self.history.load else {
            return;
        };
        let Some(commit) = data.commits.get(ix) else {
            return;
        };
        self.history.selected = Some(ix);
        self.history.marked = None;
        self.history.multi.clear();
        let id = commit.id.to_string();
        self.load_details(id, cx);
    }

    /// 比较两个提交：列表里靠后的（早的）当基准。
    pub(super) fn compare_commits(
        &mut self,
        a: String,
        b: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (base, other) = match (self.history.position(&a), self.history.position(&b)) {
            (Some(at_a), Some(at_b)) if at_a < at_b => (b, a),
            _ => (a, b),
        };
        self.compare_ends(End::Commit(base), End::Commit(other), window, cx);
    }

    /// 收起或展开下面的提交详情（提交历史、找回提交共用）。
    pub(super) fn toggle_history_details(&mut self, cx: &mut Context<Self>) {
        self.history.collapsed = !self.history.collapsed;
        cx.notify();
    }

    /// 显示不在提交列表里的东西（比如贮藏）的详情和改动；列表里不选中任何提交。
    pub(super) fn show_object(&mut self, id: String, cx: &mut Context<Self>) {
        self.history.selected = None;
        self.load_details(id, cx);
    }

    fn load_details(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let mut show_args = show::args(&id);
        let in_file = self.history.file.is_some();
        // 文件历史里它下面还有更早的提交：按路径取出来却是「新增文件」，那就是这个提交改的名
        let has_older = match (&self.history.load, self.history.selected) {
            (Load::Ready { data, .. }, Some(ix)) => ix + 1 < data.commits.len(),
            _ => false,
        };
        let whole_file = self.history.whole_file;
        if whole_file {
            fr_core::patch::set_context(&mut show_args, fr_core::patch::WHOLE_FILE);
        }
        if let Some(path) = &self.history.file {
            show_args.extend(["--".into(), path.clone()]);
        }
        let ws = self.ignore_ws;
        let plain = self.ws_plain(&channel, &mut show_args);
        // 换了要看的提交：旧的详情和改动马上撤掉，免得高亮的是这个提交、下面显示的还是上一个的
        self.history.details = None;
        self.history.ws_note = None;
        self.history.image = None;
        self.history.files.clear();
        self.history.diff_text = SharedString::default();
        self.history
            .diff
            .update(cx, |state, cx| state.set_files(Vec::new(), cx));
        self.history.diff_info = tf!("正在读取 {id} 的改动…", id = &id[..id.len().min(10)]).into();

        let started = Instant::now();
        let request = channel.git(&self.entry.path, show_args);
        let repo = self.entry.path.clone();
        // 换选中提交时旧任务随 Task 一起丢弃：本机的 git 进程被杀掉，远程的请求发取消
        self.history.diff_task = Some(cx.spawn(async move |this, cx| {
            let mut out = request.await.and_then(|out| out.ok());
            let mut plain = diff_tools::plain_out(plain).await;
            // 文件历史跟着改名往回找，可改名之前的提交按现在的路径取不到改动：
            // 那就取这个提交的全部改动，并说明原因，不显示成「没有改动」。
            // 改名的那个提交按路径取出来是「新增文件」，也换成全部改动，才看得出是从哪个名字改来的。
            let mut whole = "";
            if in_file && let Ok(bytes) = &out {
                let patch = show::parse(bytes).1;
                if patch.iter().all(u8::is_ascii_whitespace) {
                    whole = tr("该文件在此提交中使用其他名称，以下为全部改动 · ");
                } else if has_older && is_new_file_patch(patch) {
                    whole = tr("该文件在此提交中被重命名，以下为全部改动 · ");
                }
                if !whole.is_empty() {
                    let mut args = show::args(&id);
                    if whole_file {
                        fr_core::patch::set_context(&mut args, fr_core::patch::WHOLE_FILE);
                    }
                    if ws {
                        let request = channel.git(&repo, args.clone());
                        plain = request.await.ok().map(|out| out.stdout);
                        fr_core::patch::ignore_whitespace(&mut args);
                    }
                    out = channel.git(&repo, args).await.and_then(|out| out.ok());
                }
            }
            let fetch_time = started.elapsed();
            let parsed = match out {
                Ok(bytes) => Ok(cx
                    .background_spawn(async move { parse_details(bytes, plain) })
                    .await),
                Err(err) => Err(err),
            };
            this.update(cx, |this, cx| {
                match parsed {
                    Err(err) => {
                        this.history.diff_info =
                            tf!("读取改动失败：{err}", err = format!("{:#}", err)).into()
                    }
                    Ok(parsed) => {
                        this.history.details = Some(parsed.details);
                        this.history.ws_note = parsed.ws_note;
                        this.commit_tree_follow(cx);
                        match parsed.files {
                            Err(message) => this.history.diff_info = message,
                            Ok(files) => {
                                let additions: usize = files.iter().map(|f| f.additions()).sum();
                                let deletions: usize = files.iter().map(|f| f.deletions()).sum();
                                this.history.files = files
                                    .iter()
                                    .map(|f| ChangedFile {
                                        path: f.path().clone(),
                                        orig: f
                                            .original_path()
                                            .filter(|orig| *orig != f.path())
                                            .cloned(),
                                        letter: match f.status() {
                                            DiffFileStatus::Added => "A",
                                            DiffFileStatus::Deleted => "D",
                                            DiffFileStatus::Renamed => "R",
                                            DiffFileStatus::Copied => "C",
                                            _ => "M",
                                        },
                                        additions: f.additions(),
                                        deletions: f.deletions(),
                                    })
                                    .collect();
                                let note = whole;
                                this.history.diff_info = tf!(
                                    "{note}{files} 个文件 · +{additions} −{deletions}",
                                    note,
                                    files = files.len(),
                                    additions,
                                    deletions
                                )
                                .into();
                                // 读取、解析各花了多久只记日志，界面上不写
                                log::info!(
                                    "{}：{} · 读取 {} ms（{:.1} KB）· 解析 {} ms",
                                    this.entry.path,
                                    this.history.diff_info,
                                    fetch_time.as_millis(),
                                    parsed.size as f64 / 1024.,
                                    parsed.took.as_millis()
                                );
                                this.history
                                    .diff
                                    .update(cx, |state, cx| state.set_files(files, cx));
                                this.history.diff_text = parsed.text.into();
                            }
                        }
                    }
                }
                // 搜索条开着的话按新的改动再找一遍
                this.update_diff_search(Where::History, false, cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// 切换「显示整个文件」，重读正在看的那个提交的改动。
    fn toggle_commit_whole_file(&mut self, cx: &mut Context<Self>) {
        self.history.whole_file = !self.history.whole_file;
        let whole = self.history.whole_file;
        // 整个文件：补丁里带的上下文全都显示；否则只留改动附近三行
        self.history.diff.update(cx, |state, cx| {
            state.set_context_lines(if whole { None } else { Some(3) }, cx)
        });
        self.reload_details(cx);
        cx.notify();
    }

    /// 提交里改的一个文件的右键菜单：看历史、逐行追溯（原有的），加上用外部工具比较。
    fn commit_file_menu(
        &self,
        ix: usize,
        at: Option<(String, bool)>,
        cx: &mut Context<Self>,
    ) -> super::menus::MenuFn {
        let file = &self.history.files[ix];
        let base = self.tracked_file_menu(file.path.to_string(), at, cx);
        let label = self.ext_label(crate::ext_tools::Kind::Diff, cx);
        let details = self.history.details.as_ref();
        let commit = details.map(|d| d.id.clone()).unwrap_or_default();
        let parent = details.and_then(|d| d.parents.first().cloned());
        let (path, orig, letter) = (
            file.path.to_string(),
            file.orig.as_ref().map(|o| o.to_string()),
            file.letter,
        );
        let view = cx.entity().downgrade();
        Box::new(move |menu, window, cx| {
            let menu = base(menu, window, cx);
            let (view, commit, parent, path, orig) = (
                view.clone(),
                commit.clone(),
                parent.clone(),
                path.clone(),
                orig.clone(),
            );
            menu.separator().item(
                gpui_kit::component::menu::PopupMenuItem::new(label.clone()).on_click(
                    move |_, window, cx| {
                        let (commit, parent, path, orig) =
                            (commit.clone(), parent.clone(), path.clone(), orig.clone());
                        view.update(cx, |this, cx| {
                            this.external_diff_commit(
                                commit, parent, path, orig, letter, window, cx,
                            )
                        })
                        .ok();
                    },
                ),
            )
        })
    }

    /// 点了提交改动里的一个文件：图片就在右边打开图片对比，别的滚到它的改动。
    fn open_changed_file(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(file) = self.history.files.get(ix) else {
            return;
        };
        let path = file.path.clone();
        let (Some(channel), Some(details), true) = (
            self.channel.clone(),
            self.history.details.as_ref(),
            image_diff::is_image(&path),
        ) else {
            self.history.image = None;
            self.history
                .diff
                .update(cx, |diff, cx| diff.scroll_to_file(&path, cx));
            cx.notify();
            return;
        };
        // 旧的在第一个父提交里（合并提交的改动也是和它比的）；新增的没有旧的，删除的没有新的
        let old_path = file.orig.clone().unwrap_or_else(|| path.clone());
        let old = details
            .parents
            .first()
            .filter(|_| file.letter != "A")
            .map(|parent| Source::new(Blob::Rev(parent.clone()), old_path.to_string()));
        let new = (file.letter != "D")
            .then(|| Source::new(Blob::Rev(details.id.clone()), path.to_string()));
        let (app, repo) = (self.app.clone(), self.entry.path.clone());
        let view = cx.new(|cx| ImageDiff::new(app, channel, repo, old, new, cx));
        self.history.image = Some((path, view));
        cx.notify();
    }

    /// 重读正在看的那个提交（或贮藏）的改动：换了怎么看（整个文件、忽略空白）以后用。
    pub(super) fn reload_details(&mut self, cx: &mut Context<Self>) {
        let showing = self
            .history
            .details
            .as_ref()
            .map(|details| details.id.clone())
            .or_else(|| self.history.selected_commit().map(|c| c.id.to_string()));
        if let Some(id) = showing {
            self.load_details(id, cx);
        }
    }

    /// 换了界面语言：搜索框的提示文字、还没选提交时的那句话要重新设一遍。
    pub(super) fn retranslate_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let by = self.history.search_by();
        self.history.search.update(cx, |state, cx| {
            state.set_placeholder(by.placeholder(), window, cx)
        });
        self.history.file_filter.update(cx, |state, cx| {
            state.set_placeholder(tr("过滤文件…"), window, cx)
        });
        if self.history.selected.is_none() {
            self.history.diff_info = tr("选择一个提交以查看改动").into();
        }
    }

    fn render_commit_rows(
        &mut self,
        range: Range<usize>,
        colors: RowColors,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let started = Instant::now();
        let Load::Ready { data, .. } = &self.history.load else {
            return Vec::new();
        };
        let graph_width = graph_paint::column_width(data.graph.max_lanes);
        // 「说明」一栏的宽度：窗口宽度去掉侧栏、提交图、后面三栏、行的右边距和四个间隔
        let marks = self.history.has_marks();
        let subject_width = f32::from(self.history.viewport_width)
            - self.history.side_width
            - f32::from(graph_width)
            - AUTHOR_WIDTH
            - DATE_WIDTH
            - SHA_WIDTH
            - 40.
            - if marks { MARK_WIDTH + 8. } else { 0. };
        let badge_budget = badge_budget(subject_width);
        let searching = !self.history.needle.is_empty();
        // 哪些名字是远程分支：按读到的引用认，不按名字里有没有斜杠猜
        let remote_refs: std::collections::HashSet<&str> =
            self.refs.remotes.iter().map(|b| b.name.as_str()).collect();
        let remote_names = &self.remote_names;
        let is_remote = |name: &str| {
            remote_refs.contains(name)
                || name.split_once('/').is_some_and(|(remote, rest)| {
                    rest == "HEAD" && remote_names.iter().any(|n| n == remote)
                })
        };
        // 隐藏起来的分支、标签：它指着的提交要是别的引用也够得着，提交还在，但它的名字不显示
        // （只看某个分支时「隐藏」不起作用，名字也照常显示）
        let hidden = if self.history.scope == Scope::All {
            self.hidden_refs(cx)
        } else {
            Vec::new()
        };
        let rows: Vec<AnyElement> = range
            .clone()
            .filter(|ix| *ix < data.commits.len())
            .map(|ix| {
                let commit = &data.commits[ix];
                let row = data.graph.rows[ix];
                let in_multi = self.history.multi.len() > 1
                    && self.history.multi.iter().any(|id| **id == *commit.id);
                let selected = self.history.selected == Some(ix) || in_multi;
                // 搜索时不匹配的提交变淡，提交图照常画
                let dim = searching && self.history.matches.binary_search(&ix).is_err();
                // 待推送的标 ↑；待拉取的标 ↓，整行淡一些（还不在当前分支上）
                let unpushed = marks && self.history.unpushed.contains(&commit.id);
                let unpulled = marks && self.history.unpulled.contains(&commit.id);
                let menu = match in_multi {
                    true => Some(self.multi_commit_menu(cx)),
                    false => self.commit_menu(ix, cx),
                };
                let line = h_flex()
                    .id(ix)
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .pr_2()
                    .gap_2()
                    .text_sm()
                    .cursor_pointer()
                    .when(dim, |d| d.opacity(0.35))
                    .when(unpulled && !dim, |d| d.opacity(0.6))
                    .when(selected, |d| d.bg(colors.selected))
                    .when(!selected, |d| d.hover(|s| s.bg(colors.hover)))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.history.focus.focus(window, cx);
                        // Ctrl 加点击：加进多选或去掉；Shift 加点击：从选中的连到这一个
                        let modifiers = event.modifiers();
                        if modifiers.control || modifiers.platform {
                            this.toggle_multi_commit(ix, cx);
                            return;
                        }
                        if modifiers.shift {
                            this.range_multi_commit(ix, cx);
                            return;
                        }
                        this.select_commit(ix, cx)
                    }))
                    // 点右键先选中这一行，菜单里的操作都是对它做的；点在多选的提交上就保持多选
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, _, cx| {
                            if in_multi {
                                return;
                            }
                            if this.history.selected != Some(ix) || this.history.multi.len() > 1 {
                                let before =
                                    this.history.selected_commit().map(|c| c.id.to_string());
                                this.select_commit(ix, cx);
                                this.history.marked = before;
                            }
                        }),
                    )
                    .when(marks, |line| {
                        let (arrow, color, tip) = match (unpushed, unpulled) {
                            (true, _) => ("↑", colors.head, tr("待推送：上游分支还没有这个提交")),
                            (_, true) => ("↓", colors.muted, tr("待拉取：当前分支还没有这个提交")),
                            _ => ("", colors.muted, ""),
                        };
                        line.child(
                            div()
                                .id(("pending", ix))
                                .w(px(MARK_WIDTH))
                                .ml(px(4.))
                                .mr(px(-4.))
                                .flex_none()
                                .text_xs()
                                .text_center()
                                .text_color(color)
                                .child(arrow)
                                .when(!tip.is_empty(), |mark| {
                                    mark.tooltip(move |window, cx| ui::tip(tip, window, cx))
                                }),
                        )
                    })
                    .child(graph_paint::graph_cell(
                        data.graph.segments(ix).to_vec(),
                        row.lane,
                        row.color,
                        row.clipped,
                        graph_width,
                    ))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .overflow_hidden()
                            .children(ref_badges(
                                &commit.refs,
                                &is_remote,
                                &hidden,
                                badge_budget,
                                ix,
                                colors,
                                &mut |label, badge| self.wire_badge(label, badge, cx),
                            ))
                            .child(ui::clipped(
                                ("subject", ix),
                                SharedString::from(commit.subject.to_string()),
                            )),
                    )
                    .child(
                        h_flex()
                            .w(px(AUTHOR_WIDTH))
                            .flex_none()
                            .gap(px(6.))
                            .child(ui::avatar(&commit.author, 18., cx))
                            .child(
                                ui::clipped(
                                    ("author", ix),
                                    SharedString::from(commit.author.to_string()),
                                )
                                .text_color(colors.muted),
                            ),
                    )
                    .child(
                        div()
                            .w(px(DATE_WIDTH))
                            .flex_none()
                            .text_xs()
                            .text_color(colors.muted)
                            .child(format_time(commit.time)),
                    )
                    .child(
                        div()
                            .w(px(SHA_WIDTH))
                            .flex_none()
                            .text_xs()
                            .font_family(crate::fonts::mono())
                            .text_color(colors.muted)
                            .child(SharedString::from(
                                commit.id[..8.min(commit.id.len())].to_string(),
                            )),
                    );
                match menu {
                    Some(menu) => line.context_menu(menu).into_any_element(),
                    None => line.into_any_element(),
                }
            })
            .collect();
        let elapsed = started.elapsed();
        self.history.last_rows_build = (elapsed, range.len());
        if let Some(bench) = &mut self.history.bench {
            bench.row_builds.push(elapsed);
        }
        rows
    }

    /// 左下角的提交详情：说明、作者、提交者、哈希、父提交，下面是这个提交改了的文件。
    fn render_commit_details(&mut self, colors: RowColors, cx: &mut Context<Self>) -> AnyElement {
        let (added, removed, changed) = {
            let theme = cx.theme();
            (theme.success, theme.danger, theme.warning)
        };
        // 改了哪些文件：点一下右边的改动滚到它；右键可以看这个文件的历史和逐行追溯。
        // 树形还是平铺和本地修改、比较共用一个设置；文件多的时候上面有过滤框
        let tree = self.app.read(cx).settings.file_tree;
        let total = self.history.files.len();
        let all_files = self.history.all_files;
        // 全部文件一般很多：一直有过滤框
        let filterable = total > FILTER_FROM || all_files;
        let needle = match filterable {
            true => self
                .history
                .file_filter
                .read(cx)
                .value()
                .trim()
                .to_lowercase(),
            false => String::new(),
        };
        let shown: Vec<(&str, usize)> = self
            .history
            .files
            .iter()
            .enumerate()
            .filter(|(_, file)| file_tree::matches(&file.path, &needle))
            .map(|(ix, file)| (file.path.as_ref(), ix))
            .collect();
        let folded = &self.history.folded;
        let rows = file_tree::rows(&shown, tree, &|path| {
            needle.is_empty() && folded.contains(path)
        });
        let none_match = rows.is_empty() && total > 0 && !all_files;
        let tree_rows = match all_files {
            true => Some(self.render_commit_tree_rows(&needle, colors.hover, colors.selected, cx)),
            false => None,
        };
        let files: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .filter_map(|(row_ix, row)| {
                let indent = |depth: usize| px(4. + depth as f32 * file_tree::INDENT);
                let (ix, depth) = match row {
                    ListRow::File { index, depth } => (*index, *depth),
                    ListRow::Dir {
                        path,
                        label,
                        depth,
                        files,
                        open,
                    } => {
                        let key = path.clone();
                        let cells = file_tree::dir_cells(
                            ("changed-dir-name", row_ix),
                            label,
                            path,
                            *files,
                            *open,
                            cx,
                        );
                        return Some(
                            h_flex()
                                .id(("changed-dir", row_ix))
                                .h(px(22.))
                                .pl(indent(*depth))
                                .pr_1()
                                .gap(px(5.))
                                .rounded_sm()
                                .text_xs()
                                .cursor_pointer()
                                .hover(|s| s.bg(colors.hover))
                                .children(cells)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.history.folded.remove(&key) {
                                        this.history.folded.insert(key.clone());
                                    }
                                    cx.notify();
                                }))
                                .into_any_element(),
                        );
                    }
                };
                let file = self.history.files.get(ix)?;
                let opened = self
                    .history
                    .image
                    .as_ref()
                    .is_some_and(|(path, _)| *path == file.path);
                // 逐行追溯看这个提交里的版本；文件在这个提交里被删掉了，看删除前（第一个父提交）的
                let at = self
                    .history
                    .details
                    .as_ref()
                    .and_then(|d| match file.letter {
                        "D" => d.parents.first().map(|parent| (parent.clone(), true)),
                        _ => Some((d.id.clone(), false)),
                    });
                let menu = self.commit_file_menu(ix, at, cx);
                let color = match file.letter {
                    "A" => added,
                    "D" => removed,
                    _ => changed,
                };
                // 树形里目录在上面的行里，这里只写文件名
                let label: SharedString = match tree {
                    true => file_tree::split_name(&file.path).1.to_string().into(),
                    false => file.path.clone(),
                };
                // 行里写的就是全路径（平铺、根目录下的）：提示只在被省略时弹
                let bare = label == file.path;
                Some(
                    h_flex()
                        .id(("changed-file", row_ix))
                        .h(px(22.))
                        .pl(indent(depth))
                        .pr_1()
                        // 树形里：前面空出箭头那么宽，字母落在目录图标的中间、名字对着目录的名字
                        .gap(px(if tree { 7. } else { 8. }))
                        .rounded_sm()
                        .text_xs()
                        .cursor_pointer()
                        .when(opened, |row| row.bg(colors.selected))
                        .when(!opened, |row| row.hover(|s| s.bg(colors.hover)))
                        .on_click(cx.listener(move |this, _, _, cx| this.open_changed_file(ix, cx)))
                        .when(tree, |row| {
                            row.child(div().flex_none().w(px(file_tree::FILE_LEAD)))
                        })
                        .child(
                            div()
                                .w(px(10.))
                                .flex_none()
                                .font_weight(FontWeight::BOLD)
                                .text_color(color)
                                .child(file.letter),
                        )
                        .child(match bare {
                            true => ui::clipped(("changed-file-name", row_ix), label)
                                .flex_1()
                                .text_ellipsis_start()
                                .into_any_element(),
                            false => div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis_start()
                                .child(ui::one_line(label))
                                .into_any_element(),
                        })
                        .child(
                            div()
                                .flex_none()
                                .text_color(colors.muted)
                                .child(format!("+{} −{}", file.additions, file.deletions)),
                        )
                        .when(!bare, |row| {
                            let path = file.path.clone();
                            row.tooltip(move |window, cx| ui::tip(path.clone(), window, cx))
                        })
                        .context_menu(menu)
                        .into_any_element(),
                )
            })
            .collect();
        let (app, view) = (self.app.clone(), cx.entity().downgrade());
        let filter_input = filterable.then(|| {
            Input::new(&self.history.file_filter)
                .small()
                .cleanable(true)
                .prefix(ui::icon(IconName::Search).text_color(colors.muted))
        });
        let Some(d) = &self.history.details else {
            let text = if self.history.selected.is_some() {
                tr("正在读取…")
            } else {
                tr("未选中提交")
            };
            return div()
                .p_3()
                .text_sm()
                .text_color(colors.muted)
                .child(text)
                .into_any_element();
        };
        // 英文的「Committer」比两三个汉字宽
        let label_width = match fr_core::i18n::lang() {
            fr_core::i18n::Lang::Zh => 48.,
            fr_core::i18n::Lang::En => 68.,
        };
        let line = |label: &'static str, value: String| {
            h_flex()
                .gap_2()
                .text_xs()
                .child(
                    div()
                        .w(px(label_width))
                        .flex_none()
                        .text_color(colors.muted)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(value))
        };
        let short = |id: &String| id.chars().take(10).collect::<String>();
        // 作者：头像、名字，下面一行是邮箱和时间
        let author = h_flex()
            .gap(px(10.))
            .child(ui::avatar(&d.author, 30., cx))
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        ui::clipped("detail-author", d.author.clone())
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM),
                    )
                    .child(
                        ui::clipped(
                            "detail-email",
                            format!("{} · {}", d.author_email, format_time(d.author_time)),
                        )
                        .text_xs()
                        .text_color(colors.muted),
                    ),
            );
        let mut info = v_flex().gap_1();
        if d.committer != d.author || d.committer_time != d.author_time {
            info = info.child(line(
                tr("提交者"),
                format!(
                    "{} <{}> · {}",
                    d.committer,
                    d.committer_email,
                    format_time(d.committer_time)
                ),
            ));
        }
        let parents = if d.parents.is_empty() {
            tr("无（这是第一个提交）").to_string()
        } else {
            d.parents.iter().map(short).collect::<Vec<_>>().join("  ")
        };
        info = info
            .child(line(tr("哈希"), d.id.clone()))
            .child(line(tr("父提交"), parents));
        div()
            .id("details")
            .size_full()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .p_3()
                    .gap_3()
                    .child(author)
                    .child(div().text_sm().whitespace_normal().child(d.message.clone()))
                    .child(div().h(px(1.)).bg(colors.border))
                    .child(info)
                    .child(div().h(px(1.)).bg(colors.border))
                    .child(
                        h_flex()
                            .gap_2()
                            // 改动的文件｜这个提交时的全部文件
                            .child(
                                ButtonGroup::new("details-files-kind")
                                    .outline()
                                    .xsmall()
                                    .child(
                                        Button::new("details-files-changed")
                                            .label(tf!("改动的文件（{files}）", files = total))
                                            .selected(!all_files)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.set_all_files(false, cx)
                                            })),
                                    )
                                    .child(
                                        Button::new("details-files-all")
                                            .label(tr("全部文件"))
                                            .tooltip(tr("此提交时仓库里的全部文件：点击查看当时的内容，可另存"))
                                            .selected(all_files)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.set_all_files(true, cx)
                                            })),
                                    ),
                            )
                            .child(div().flex_1())
                            .when(!all_files, |row| {
                                row.child(file_tree::mode_toggle(
                                    "details-list-mode",
                                    tree,
                                    move |tree, _, cx| {
                                        app.update(cx, |app, cx| {
                                            app.update_settings(cx, |s| s.file_tree = tree)
                                        });
                                        view.update(cx, |_, cx| cx.notify()).ok();
                                    },
                                ))
                            }),
                    )
                    .children(filter_input)
                    .when(none_match, |panel| {
                        panel.child(
                            div()
                                .text_xs()
                                .text_color(colors.muted)
                                .child(tr("没有匹配的文件")),
                        )
                    })
                    .child(match tree_rows {
                        Some(rows) => v_flex().children(rows),
                        None => v_flex().children(files),
                    }),
            )
            .into_any_element()
    }

    pub(super) fn render_history(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, danger) = (theme.border, theme.muted_foreground, theme.danger);
        let colors = RowColors {
            selected: theme.list_active,
            hover: theme.list_hover,
            muted,
            border,
            head: theme.blue,
            local: theme.green,
            tag: theme.yellow,
            remote: theme.magenta,
        };
        let details = self.render_commit_details(colors, cx);
        // 并排还是上下排跟着设置走：别的标签页里改了，这里重画时跟上
        let split = self.app.read(cx).settings.diff_split;
        let mode = if split {
            DiffMode::Split
        } else {
            DiffMode::Unified
        };
        if self.history.diff.read(cx).mode() != mode {
            self.history
                .diff
                .update(cx, |state, cx| state.set_mode(mode, cx));
        }
        let whole_file = self.history.whole_file;
        let diff_state = self.history.diff.clone();
        let (prev_state, next_state) = (diff_state.clone(), diff_state.clone());
        let collapsed = self.history.collapsed;
        let details_height = self.history.details_height;
        let details_width = self.history.details_width;
        // 改动视图看得见的宽度，去掉文件标题左边的折叠按钮、两边的内边距和滚动条的位置
        let header_width =
            (self.history.viewport_width - px(self.history.side_width) - details_width - px(64.))
                .max(px(160.));
        let graph_width = match &self.history.load {
            Load::Ready { data, .. } => graph_paint::column_width(data.graph.max_lanes),
            _ => px(120.),
        };
        // 状态行上写的，和鼠标停在上面时显示的细节（读取、解析各花了多久）
        let mut status_detail: Option<SharedString> = None;
        let (status, status_color, list): (SharedString, Hsla, AnyElement) = match &self
            .history
            .load
        {
            Load::Idle | Load::Loading => (
                tr("正在读取提交历史…").into(),
                muted,
                div().into_any_element(),
            ),
            Load::Empty => (tr("暂无提交").into(), muted, div().into_any_element()),
            Load::Failed(err) => (err.clone(), danger, div().into_any_element()),
            Load::Ready { data, partial } => {
                let (build, rows) = self.history.last_rows_build;
                status_detail = Some(
                    tf!(
                        "{stats} · 构建 {rows} 行 {as_micros} µs",
                        stats = data.stats,
                        rows,
                        as_micros = build.as_micros()
                    )
                    .into(),
                );
                let mut text = if *partial {
                    tf!(
                        "已显示前 {n} 个提交，正在加载其余提交…",
                        n = data.commits.len()
                    )
                } else {
                    tf!("{n} 个提交", n = data.commits.len())
                };
                if self.history.reloading && !partial {
                    text.push_str(tr(" · 正在刷新…"));
                }
                if data.graph.max_lanes > graph_paint::MAX_LANES {
                    text = tf!(
                        "提交图共 {lanes} 条线，仅绘制前 {max} 条（带 ··· 的行不完整） · {text}",
                        lanes = data.graph.max_lanes,
                        max = graph_paint::MAX_LANES,
                        text = text
                    );
                }
                (
                    text.into(),
                    muted,
                    uniform_list(
                        "commits",
                        data.commits.len(),
                        cx.processor(move |this, range: Range<usize>, _window, cx| {
                            this.render_commit_rows(range, colors, cx)
                        }),
                    )
                    .track_scroll(&self.history.scroll)
                    .size_full()
                    .into_any_element(),
                )
            }
        };

        // 还没读完时不能说「没有匹配」：只是还没读到
        let still_reading = matches!(
            self.history.load,
            Load::Loading | Load::Ready { partial: true, .. }
        );
        let match_info: SharedString = if self.history.needle.is_empty() {
            "".into()
        } else if self.history.matches.is_empty() {
            if still_reading {
                tr("提交尚未加载完，已加载的提交中没有匹配项").into()
            } else {
                tr("没有匹配的提交").into()
            }
        } else {
            match self.history.match_pos {
                Some(pos) => format!("{} / {}", pos + 1, self.history.matches.len()).into(),
                None => tf!(
                    "{matches} 个匹配项，按 Enter 跳转到第一个",
                    matches = self.history.matches.len()
                )
                .into(),
            }
        };
        let has_matches = !self.history.matches.is_empty();
        let find_info = self.find_info(cx);
        // 点开了一张图片：右边显示它，上面一行写是哪个文件、怎么回去
        let image_pane = self.history.image.clone().map(|(path, view)| {
            v_flex()
                .flex_1()
                .min_h_0()
                .child(
                    h_flex()
                        .h(px(28.))
                        .flex_none()
                        .pl_3()
                        .pr_2()
                        .gap_2()
                        .border_b_1()
                        .border_color(border)
                        .text_xs()
                        .child(
                            ui::clipped("commit-image-path", path)
                                .flex_1()
                                .text_ellipsis_start(),
                        )
                        .child(
                            Button::new("commit-image-back")
                                .xsmall()
                                .label(tr("返回全部改动"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.history.image = None;
                                    cx.notify();
                                })),
                        ),
                )
                .child(div().flex_1().min_h_0().child(view))
                .into_any_element()
        });
        let ws_toggle = self.ws_toggle("history-ignore-ws", cx);
        let search_toggle = self.search_toggle(Where::History, cx);
        let search_bar = self.render_diff_search(Where::History, cx);
        let search_marks = self.search_decorations(Where::History, cx);
        let ws_strip = self
            .history
            .ws_note
            .clone()
            .filter(|_| self.ignore_ws)
            .map(|note| diff_tools::info_strip("history-ws-note", note, cx));
        let pending = self.render_pending(cx);
        let marks = self.history.has_marks();
        let search_by = match self.history.file {
            Some(_) => None,
            None => Some(self.render_search_by(cx).into_any_element()),
        };
        let info_bg = cx.theme().secondary;

        // 找回提交这一页换掉上半部分（列表），下面的详情和改动照用
        let top = if self.mode == super::Mode::Recover {
            self.render_recover(collapsed, cx)
        } else {
            v_flex()
                .flex_1()
                .min_h_0()
                .when_some(self.history.file.clone(), |view, path| {
                    view.child(
                        h_flex()
                            .h(px(30.))
                            .flex_none()
                            .px_3()
                            .gap_2()
                            .border_b_1()
                            .border_color(border)
                            .text_sm()
                            .child(div().text_color(muted).child(tr("文件历史")))
                            .child(ui::clipped("file-history-path", path.clone()).flex_1())
                            .child(
                                Button::new("file-blame")
                                    .xsmall()
                                    .label(tr("逐行追溯"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.show_blame(path.clone(), cx)
                                    })),
                            )
                            .child(
                                Button::new("close-file-history")
                                    .xsmall()
                                    .label(tr("返回所有提交"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_file_history(window, cx)
                                    })),
                            ),
                    )
                })
                .child(
                    h_flex()
                        .h(px(40.))
                        .flex_none()
                        .px(px(10.))
                        .gap_2()
                        .children(search_by)
                        .child(
                            div().w(px(320.)).flex_shrink(1.).min_w(px(140.)).child(
                                Input::new(&self.history.search)
                                    .small()
                                    .cleanable(true)
                                    .prefix(ui::icon(IconName::Search).text_color(muted)),
                            ),
                        )
                        .child(
                            ui::clipped("match-info", match_info)
                                .flex_shrink(1.)
                                .text_xs()
                                .text_color(muted),
                        )
                        .when(has_matches, |bar| {
                            bar.child(
                                Button::new("match-prev")
                                    .xsmall()
                                    .ghost()
                                    .icon(ui::icon(IconName::ChevronUp))
                                    .tooltip(tr("上一个匹配的提交（Shift+Enter）"))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.next_match(-1, cx)),
                                    ),
                            )
                            .child(
                                Button::new("match-next")
                                    .xsmall()
                                    .ghost()
                                    .icon(ui::icon(IconName::ChevronDown))
                                    .tooltip(tr("下一个匹配的提交（Enter）"))
                                    .on_click(cx.listener(|this, _, _, cx| this.next_match(1, cx))),
                            )
                        })
                        .child(div().flex_1())
                        .children(pending)
                        .when(self.history.file.is_none(), |bar| {
                            bar.child(self.render_scope_button(cx))
                        })
                        .child(
                            Button::new("pick-file")
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::FileClock))
                                .label(tr("文件历史…"))
                                .tooltip(tr("选择文件以查看其历史或逐行追溯"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.pick_file(window, cx)),
                                ),
                        ),
                )
                .when_some(find_info, |view, info| {
                    view.child(
                        h_flex()
                            .h(px(26.))
                            .flex_none()
                            .px_3()
                            .gap_2()
                            .bg(info_bg)
                            .text_xs()
                            .text_color(muted)
                            .child(ui::icon_small(IconName::Info).flex_none())
                            .child(ui::clipped("find-info", info).flex_1()),
                    )
                })
                .child(
                    h_flex()
                        .h(px(24.))
                        .flex_none()
                        .pr_2()
                        .gap_2()
                        .border_b_1()
                        .border_color(border)
                        .text_size(px(11.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(muted)
                        .when(marks, |header| {
                            header.child(div().w(px(MARK_WIDTH)).ml(px(4.)).mr(px(-4.)).flex_none())
                        })
                        // 提交图那一列不写标题：分支少时这一列很窄，放不下字（Fork 也不写）
                        .child(div().w(graph_width).flex_none())
                        .child(div().flex_1().child(tr("说明")))
                        .child(div().w(px(AUTHOR_WIDTH)).flex_none().child(tr("作者")))
                        .child(div().w(px(DATE_WIDTH)).flex_none().child(tr("时间")))
                        .child(div().w(px(SHA_WIDTH)).flex_none().child(tr("哈希"))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h(px(78.))
                        .overflow_hidden()
                        .track_focus(&self.history.focus)
                        .on_key_down(cx.listener(Self::history_key))
                        .child(list),
                )
                .child(
                    h_flex()
                        .h(px(26.))
                        .flex_none()
                        .px_3()
                        .gap_2()
                        .border_t_1()
                        .border_color(border)
                        .text_xs()
                        .text_color(status_color)
                        .child(match status_detail {
                            // 有详细说明的（读了多久、多大）：停上去看说明
                            Some(detail) => div()
                                .id("history-status")
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(status)
                                .tooltip(move |window, cx| {
                                    ui::full_text_tip(detail.clone(), window, cx)
                                }),
                            None => ui::clipped("history-status", status).flex_1(),
                        })
                        .child(
                            Button::new("toggle-details")
                                .xsmall()
                                .ghost()
                                .icon(ui::icon(if collapsed {
                                    IconName::PanelBottomOpen
                                } else {
                                    IconName::PanelBottomClose
                                }))
                                .label(if collapsed {
                                    tr("展开详情")
                                } else {
                                    tr("收起详情")
                                })
                                .tooltip(tr("收起或展开提交详情"))
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.toggle_history_details(cx)),
                                ),
                        ),
                )
                .into_any_element()
        };
        let multi_panel = self.render_multi_commits(cx);
        let multi = multi_panel.is_some();
        let tree_preview = self.render_tree_preview(cx);
        let previewing = tree_preview.is_some();
        v_flex()
            .size_full()
            .child(top)
            .when_some(multi_panel.filter(|_| !collapsed), |view, panel| {
                view.child(
                    div()
                        .h(details_height)
                        .flex_none()
                        .relative()
                        .child(panel)
                        .child(self.split_handle(Split::DetailsHeight, cx)),
                )
            })
            .when(!collapsed && !multi, |view| {
                view.child(
                    h_flex()
                        .h(details_height)
                        .flex_none()
                        .relative()
                        .child(
                            div()
                                .w(details_width)
                                .flex_none()
                                .h_full()
                                .relative()
                                .child(details)
                                .child(self.split_handle(Split::DetailsWidth, cx)),
                        )
                        // 全部文件里点开了一个文件：右边是它那时的内容，不是改动
                        .children(tree_preview)
                        .when(!previewing, |row| {
                            row.child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .child(
                                        h_flex()
                                            .h(px(28.))
                                            .flex_none()
                                            .pl_3()
                                            .pr_2()
                                            .gap_1()
                                            .border_b_1()
                                            .border_color(border)
                                            .text_xs()
                                            .text_color(muted)
                                            .child(
                                                ui::clipped(
                                                    "diff-info",
                                                    self.history.diff_info.clone(),
                                                )
                                                .flex_1(),
                                            )
                                            .child(
                                                Button::new("diff-prev-change")
                                                    .xsmall()
                                                    .ghost()
                                                    .icon(ui::icon(IconName::ArrowUp))
                                                    .tooltip(tr("上一处改动"))
                                                    .on_click(move |_, _, cx| {
                                                        prev_state.update(cx, |state, cx| {
                                                            state.previous_change(cx)
                                                        })
                                                    }),
                                            )
                                            .child(
                                                Button::new("diff-next-change")
                                                    .xsmall()
                                                    .ghost()
                                                    .icon(ui::icon(IconName::ArrowDown))
                                                    .tooltip(tr("下一处改动"))
                                                    .on_click(move |_, _, cx| {
                                                        next_state.update(cx, |state, cx| {
                                                            state.next_change(cx)
                                                        })
                                                    }),
                                            )
                                            .child(search_toggle)
                                            .child(ws_toggle)
                                            .child(
                                                Button::new("history-whole-file")
                                                    .xsmall()
                                                    .ghost()
                                                    .icon(ui::icon(IconName::UnfoldVertical))
                                                    .selected(whole_file)
                                                    .tooltip(tr("显示整个文件"))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.toggle_commit_whole_file(cx)
                                                    })),
                                            )
                                            .child(
                                                Button::new("history-split")
                                                    .xsmall()
                                                    .ghost()
                                                    .icon(ui::icon(IconName::Columns2))
                                                    .selected(split)
                                                    .tooltip(tr(
                                                        "并排显示：左侧为旧版本，右侧为新版本",
                                                    ))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.toggle_split(cx)
                                                    })),
                                            ),
                                    )
                                    .children(search_bar)
                                    .children(ws_strip)
                                    .child(match image_pane {
                                        Some(pane) => pane,
                                        None => div()
                                            .relative()
                                            .flex_1()
                                            .min_h_0()
                                            .child(
                                                Diff::new(&self.history.diff)
                                                    .line_number(true)
                                                    .syntax_highlight(true)
                                                    .decorations(search_marks)
                                                    .render_header(move |file, _, _| {
                                                        diff_file_header(file, header_width, muted)
                                                    })
                                                    .size_full(),
                                            )
                                            // 按住 Ctrl 滚滚轮调代码字号
                                            .child(crate::zoom::wheel_zoom(self.app.clone()))
                                            .into_any_element(),
                                    }),
                            )
                        })
                        // 放在最后：画在详情和改动的上面，整条上边都拖得动
                        .child(self.split_handle(Split::DetailsHeight, cx)),
                )
            })
            .into_any_element()
    }

    /// 每帧推进一次滚动测速，测完打印结果并退出。
    pub(super) fn tick_bench(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.step_bench(window) {
            self.finish_bench(window, cx);
        }
    }

    /// 返回 true 表示测完了。
    fn step_bench(&mut self, window: &mut Window) -> bool {
        let (Load::Ready { data, .. }, Some(bench)) = (&self.history.load, &mut self.history.bench)
        else {
            return false;
        };
        let count = data.commits.len();
        let now = Instant::now();
        if let Some(prev) = bench.last_frame.replace(now) {
            if bench.frame <= BENCH_SCROLL_FRAMES {
                bench.scroll_intervals.push(now - prev);
            } else {
                bench.jump_intervals.push(now - prev);
            }
        }
        bench.frame += 1;
        if bench.frame <= BENCH_SCROLL_FRAMES {
            bench.pos = (bench.pos + BENCH_ROWS_PER_FRAME).min(count - 1);
        } else if bench.frame <= BENCH_SCROLL_FRAMES + BENCH_JUMP_FRAMES {
            // 线性同余随机数，够用来打散跳转位置
            bench.seed = bench
                .seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            bench.pos = (bench.seed >> 33) as usize % count;
        } else {
            return true;
        }
        self.history
            .scroll
            .scroll_to_item(bench.pos, ScrollStrategy::Top);
        window.request_animation_frame();
        false
    }

    fn finish_bench(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut bench) = self.history.bench.take() else {
            return;
        };
        // GPUI 自己记的帧耗时：draw 是每帧在 CPU 上构建和绘制场景的时间，不含等待垂直同步
        let snapshot = window.frame_duration_snapshot();
        macro_rules! hist {
            ($name:expr, $h:expr) => {{
                let h = &$h;
                let ms = |ns: u64| ns as f64 / 1e6;
                tf!("{name}：{h} 帧，平均 {e6} ms，p50 {value_at_quantile} ms，p95 {value_at_quantile2} ms，p99 {value_at_quantile3} ms，最慢 {h2} ms", name = $name, h = h.len(), e6 = format!("{:.2}", h.mean() / 1e6), value_at_quantile = format!("{:.2}", ms(h.value_at_quantile(0.5))), value_at_quantile2 = format!("{:.2}", ms(h.value_at_quantile(0.95))), value_at_quantile3 = format!("{:.2}", ms(h.value_at_quantile(0.99))), h2 = format!("{:.2}", ms(h.max())))
            }};
        }
        let report = [
            summarize(
                &tf!(
                    "连续滚动（每帧 {BENCH_ROWS_PER_FRAME} 行）帧间隔",
                    BENCH_ROWS_PER_FRAME
                ),
                &mut bench.scroll_intervals,
            ),
            summarize(tr("随机跳转帧间隔"), &mut bench.jump_intervals),
            summarize(tr("每帧构建可见行"), &mut bench.row_builds),
            hist!(
                tr("GPUI Window::draw 耗时"),
                snapshot.draw_duration_histogram
            ),
            hist!(tr("GPUI 从变脏到呈现"), snapshot.dirty_to_present_histogram),
            hist!(tr("GPUI 呈现间隔"), snapshot.present_interval_histogram),
        ]
        .join("\n");
        eprintln!("[M0 滚动测速]\n{report}");
        cx.quit();
    }
}

fn summarize(name: &str, values: &mut [Duration]) -> String {
    if values.is_empty() {
        return tf!("{name}：没有数据", name);
    }
    values.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.;
    let pct = |p: f64| values[((values.len() - 1) as f64 * p).round() as usize];
    let avg = values.iter().sum::<Duration>() / values.len() as u32;
    tf!(
        "{name}：{values} 次，平均 {avg} ms，p50 {pct} ms，p95 {pct2} ms，p99 {pct3} ms，最慢 {values2} ms",
        name,
        values = values.len(),
        avg = format!("{:.2}", ms(avg)),
        pct = format!("{:.2}", ms(pct(0.5))),
        pct2 = format!("{:.2}", ms(pct(0.95))),
        pct3 = format!("{:.2}", ms(pct(0.99))),
        values2 = format!("{:.2}", ms(values[values.len() - 1]))
    )
}

/// 解析 git log 的输出并计算布局，在后台线程执行。`git_time` 为空表示这是先显示的前一页。
fn build_loaded(
    out: &[u8],
    fetch_time: Duration,
    git_time: Option<Duration>,
    linear: bool,
) -> Loaded {
    let t0 = Instant::now();
    let mut commits = gitlog::parse(out);
    if linear {
        // 文件历史里的提交互相不一定是父子：连成一条线来画
        let count = commits.len() as u32;
        for (ix, commit) in commits.iter_mut().enumerate() {
            let next = ix as u32 + 1;
            commit.parents = if next < count { vec![next] } else { Vec::new() };
        }
    }
    let t1 = Instant::now();
    let graph = Graph::build(&commits);
    let t2 = Instant::now();
    let stats = match git_time {
        Some(git) => tf!(
            "{commits} 个提交 · 读取 {fetch_time} s（git {git} s，{e6} MB）· 解析 {t0} ms · 布局 {t1} ms",
            commits = commits.len(),
            fetch_time = format!("{:.2}", fetch_time.as_secs_f64()),
            git = format!("{:.2}", git.as_secs_f64()),
            e6 = format!("{:.1}", out.len() as f64 / 1e6),
            t0 = (t1 - t0).as_millis(),
            t1 = (t2 - t1).as_millis()
        ),
        None => tf!(
            "已显示前 {commits} 个提交（{fetch_time} ms），正在加载其余提交…",
            commits = commits.len(),
            fetch_time = fetch_time.as_millis()
        ),
    };
    Loaded {
        commits: Arc::new(commits),
        graph: Arc::new(graph),
        stats: stats.into(),
    }
}

/// 只要日期，逐行追溯里用。
pub(super) fn format_date(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

pub(super) fn format_time(ts: i64) -> SharedString {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
        .into()
}

fn not_found_text(id: &str) -> SharedString {
    tf!(
        "提交历史中未找到 {id}（可能不在任何分支上）",
        id = &id[..id.len().min(10)]
    )
    .into()
}

/// 完整的引用名去掉 `refs/heads/` 这类前缀，给人看。
pub(super) fn short_ref(full: &str) -> &str {
    ["refs/heads/", "refs/remotes/", "refs/tags/"]
        .iter()
        .find_map(|prefix| full.strip_prefix(prefix))
        .unwrap_or(full)
}

/// 补丁里有没有「新增文件」的那一行（`new file mode`）。
fn is_new_file_patch(patch: &[u8]) -> bool {
    patch
        .split(|b| *b == b'\n')
        .any(|line| line.starts_with(b"new file mode "))
}

/// 一个标签里的字最宽这么宽，再长用省略号
const BADGE_TEXT_MAX: f32 = 170.;
/// 标签之间的间隔
const BADGE_GAP: f32 = 4.;
/// 「+N」那个标签占的宽度
const BADGE_MORE: f32 = 28.;

/// 提交上的一个引用：显示的名字，和它是哪一类。
#[derive(Debug, PartialEq)]
struct RefLabel {
    name: String,
    kind: RefKind,
}

impl RefLabel {
    /// 完整的引用名（`refs/heads/…`），和「隐藏」记的名字对得上。HEAD 没有，返回空的。
    fn full_name(&self) -> String {
        match self.kind {
            RefKind::Current | RefKind::Local => format!("refs/heads/{}", self.name),
            RefKind::Remote => format!("refs/remotes/{}", self.name),
            RefKind::Tag => format!("refs/tags/{}", self.name),
            RefKind::Head => String::new(),
        }
    }
}

/// 引用的种类，也是放不下时留谁的先后：当前分支的名字比「HEAD」两个字有用，排最前。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RefKind {
    Current,
    Head,
    Local,
    Tag,
    Remote,
}

/// 拆 `git log` 的 `%D`，按当前分支、HEAD、本地分支、标签、远程分支排。
/// 本地分支和它的远程分支常常在同一个提交上，放不下时先收起远程的那个。
fn ref_labels(refs: &str, is_remote: &dyn Fn(&str) -> bool) -> Vec<RefLabel> {
    if refs.is_empty() {
        return Vec::new();
    }
    let mut labels = Vec::new();
    for item in refs.split(", ") {
        let (name, kind) = if let Some(branch) = item.strip_prefix("HEAD -> ") {
            labels.push(RefLabel {
                name: "HEAD".into(),
                kind: RefKind::Head,
            });
            (branch, RefKind::Current)
        } else if item == "HEAD" {
            (item, RefKind::Head)
        } else if let Some(tag) = item.strip_prefix("tag: ") {
            (tag, RefKind::Tag)
        } else if is_remote(item) {
            (item, RefKind::Remote)
        } else {
            (item, RefKind::Local)
        };
        labels.push(RefLabel {
            name: name.to_string(),
            kind,
        });
    }
    labels.sort_by_key(|label| label.kind);
    labels
}

/// 估一个标签的宽度：字号 12，大写字母宽一些，汉字一个算 12；加左右内边距和边框。
fn badge_width(name: &str) -> f32 {
    let text: f32 = name
        .chars()
        .map(|c| match c {
            c if !c.is_ascii() => 12.,
            c if c.is_ascii_uppercase() => 8.,
            _ => 6.,
        })
        .sum();
    text.min(BADGE_TEXT_MAX) + 10.
}

/// 「说明」一栏里最多拿多宽放标签：说明至少留 200，栏窄的时候至少留四成半。
fn badge_budget(subject_width: f32) -> f32 {
    (subject_width - 200.).max(subject_width * 0.55)
}

/// 这么宽里放得下前几个标签。至少放一个；有放不下的，要先给「+N」留出位置。
fn badges_that_fit(widths: &[f32], budget: f32) -> usize {
    let total: f32 = widths.iter().map(|w| w + BADGE_GAP).sum();
    if total <= budget {
        return widths.len();
    }
    let mut used = BADGE_MORE + BADGE_GAP;
    let mut count = 0;
    for width in widths {
        if count > 0 && used + width + BADGE_GAP > budget {
            break;
        }
        used += width + BADGE_GAP;
        count += 1;
    }
    count
}

fn ref_badges(
    refs: &str,
    is_remote: &dyn Fn(&str) -> bool,
    hidden: &[String],
    budget: f32,
    row: usize,
    colors: RowColors,
    wire: &mut dyn FnMut(&RefLabel, Stateful<Div>) -> AnyElement,
) -> Vec<AnyElement> {
    let mut labels = ref_labels(refs, is_remote);
    if !hidden.is_empty() {
        labels.retain(|label| !hidden.contains(&label.full_name()));
    }
    let widths: Vec<f32> = labels.iter().map(|l| badge_width(&l.name)).collect();
    let shown = badges_that_fit(&widths, budget);
    let mut visible: Vec<&RefLabel> = labels[..shown].iter().collect();
    // 两个都放得下时还是「HEAD」在前、分支名在后
    if visible.len() >= 2 && visible[1].kind == RefKind::Head {
        visible.swap(0, 1);
    }
    let mut badges: Vec<AnyElement> = visible
        .into_iter()
        .enumerate()
        .map(|(at, label)| {
            let color: Hsla = match label.kind {
                RefKind::Head => colors.head,
                RefKind::Current | RefKind::Local => colors.local,
                RefKind::Tag => colors.tag,
                RefKind::Remote => colors.remote,
            };
            // 当前分支加粗（和侧栏一样）：「HEAD」被收起来时也认得出
            let bold = label.kind == RefKind::Current;
            wire(label, badge(row, at, &label.name, color, bold))
        })
        .collect();
    if shown < labels.len() {
        // 收起来的那些：鼠标停在「+N」上能看到名字
        let hidden: SharedString = tf!(
            "另有：{join}",
            join = labels[shown..]
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>()
                .join(tr("、"))
        )
        .into();
        badges.push(
            div()
                .id(("more-refs", row))
                .flex_none()
                .h(px(18.))
                .px(px(5.))
                .rounded(px(4.))
                .bg(colors.muted.opacity(0.14))
                .text_size(px(11.))
                .line_height(px(18.))
                .text_color(colors.muted)
                .child(format!("+{}", labels.len() - shown))
                .tooltip(move |window, cx| ui::tip(hidden.clone(), window, cx))
                .into_any_element(),
        );
    }
    badges
}

/// 一个分支、标签的小牌子。名字太长会截掉，停上去看全名。`at` 是这一行里的第几个。
fn badge(row: usize, at: usize, label: &str, color: Hsla, bold: bool) -> Stateful<Div> {
    div()
        .id(SharedString::from(format!("ref-badge-{row}-{at}")))
        .flex_none()
        .max_w(px(BADGE_TEXT_MAX + 10.))
        .when(bold, |d| d.font_weight(FontWeight::SEMIBOLD))
        .h(px(18.))
        .px(px(5.))
        .rounded(px(4.))
        .border_1()
        .border_color(color.opacity(0.45))
        .bg(color.opacity(0.12))
        .text_size(px(11.))
        .line_height(px(16.))
        .text_color(color)
        .child(ui::clipped(
            ("badge", at),
            SharedString::from(label.to_string()),
        ))
}

/// 改动视图里每个文件的标题：路径、增删行数、文件是新增还是删除。
/// 组件库自带的标题和最长的一行代码一样宽，行一长，右边的行数就在看不见的地方；
/// 这里把宽度限制在看得见的范围里。
fn diff_file_header(file: &DiffFile, width: Pixels, muted: Hsla) -> AnyElement {
    let name = match (file.original_path(), file.modified_path()) {
        (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
        _ => file.path().to_string(),
    };
    let status = match file.status() {
        DiffFileStatus::Added => tr("新增"),
        DiffFileStatus::Deleted => tr("已删除"),
        DiffFileStatus::Renamed => tr("已重命名"),
        DiffFileStatus::Copied => tr("已复制"),
        DiffFileStatus::Conflicted => tr("有冲突"),
        _ => "",
    };
    h_flex()
        .w_full()
        .max_w(width)
        .gap_2()
        .child(
            ui::clipped(ElementId::Name(format!("diff-file:{name}").into()), name)
                .flex_1()
                // 放不下时省略前面的目录，文件名留着
                .text_ellipsis_start()
                .font_weight(FontWeight::MEDIUM),
        )
        .child(div().flex_none().text_xs().text_color(muted).child(format!(
            "+{} −{}",
            file.additions(),
            file.deletions()
        )))
        .when(!status.is_empty(), |header| {
            header.child(div().flex_none().text_xs().text_color(muted).child(status))
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{
        RefKind, badge_budget, badge_width, badges_that_fit, is_new_file_patch, ref_labels,
    };

    #[test]
    fn 认得出新增文件的补丁() {
        assert!(is_new_file_patch(
            b"diff --git a/n.txt b/n.txt\nnew file mode 100644\nindex 0000000..e69de29\n"
        ));
        assert!(!is_new_file_patch(
            b"diff --git a/n.txt b/n.txt\nindex 1..2 100644\n+new file mode 100644\n"
        ));
    }

    fn names(refs: &str) -> Vec<(String, RefKind)> {
        ref_labels(refs, &|name| name.starts_with("origin/"))
            .into_iter()
            .map(|label| (label.name, label.kind))
            .collect()
    }

    #[test]
    fn 引用按本地在前远程在后排() {
        assert_eq!(
            names("HEAD -> main, tag: v1, origin/main, origin/HEAD, feat"),
            vec![
                ("main".to_string(), RefKind::Current),
                ("HEAD".to_string(), RefKind::Head),
                ("feat".to_string(), RefKind::Local),
                ("v1".to_string(), RefKind::Tag),
                ("origin/main".to_string(), RefKind::Remote),
                ("origin/HEAD".to_string(), RefKind::Remote),
            ]
        );
        assert!(names("").is_empty());
    }

    #[test]
    fn 标签放不下时折起后面的() {
        // 都放得下
        assert_eq!(badges_that_fit(&[40., 40.], 100.), 2);
        // 放不下：给「+N」留 32，只够第一个
        assert_eq!(badges_that_fit(&[40., 40., 40.], 100.), 1);
        assert_eq!(badges_that_fit(&[40., 40., 40.], 125.), 2);
        // 再窄也放一个
        assert_eq!(badges_that_fit(&[90., 130.], 50.), 1);
        assert_eq!(badges_that_fit(&[], 50.), 0);
    }

    #[test]
    fn 窄窗口里说明至少留四成半() {
        assert_eq!(badge_budget(600.), 400.);
        assert!((badge_budget(260.) - 143.).abs() < 0.01);
        // 很长的名字按上限算
        assert_eq!(badge_width(&"x".repeat(100)), 180.);
        assert_eq!(badge_width("HEAD"), 42.);
    }
}
