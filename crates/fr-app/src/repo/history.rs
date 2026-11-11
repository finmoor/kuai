//! 提交历史：提交图列表、提交详情、所选提交的改动。
//!
//! 上面有搜索框：按说明、作者、哈希找提交，不匹配的行变淡，回车跳到下一个匹配。
//! 「文件历史」模式下只列出改过某个文件的提交，改动也只显示这个文件的。
//! 提交列表可以用键盘：上下键、PageUp、PageDown、Home、End 换选中的提交。
//!
//! 第一次读历史时边读边显示：读到前 300 个提交就先画出来（布局和读完全部时的前一段一致，
//! 见 `fr_core::log::Commit::parents`），读完再整个换上。重读（比如提交之后）时等读完再换，
//! 免得滚动位置跳回顶上。

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
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffFile, DiffFileStatus, DiffState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use crate::{graph_paint, ui};

const ROW_HEIGHT: f32 = 26.;
const AUTHOR_WIDTH: f32 = 140.;
const DATE_WIDTH: f32 = 130.;
const SHA_WIDTH: f32 = 76.;
/// 提交详情左边一栏最宽这么宽；窗口窄的时候按比例缩，给右边的改动留地方
const DETAILS_WIDTH: f32 = 380.;
/// 第一次读历史时，读到这么多个提交就先显示
const FIRST_PAGE: usize = 300;

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
struct ChangedFile {
    path: SharedString,
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
    /// 下面的详情区收起来了
    collapsed: bool,
    /// 详情区的高度，按窗口高度算，每帧更新
    details_height: Pixels,
    /// 详情区左边一栏的宽度，按窗口宽度算
    details_width: Pixels,
    /// 窗口内容区的宽度：算一行里留给分支标签多宽、改动视图的文件标题多宽
    viewport_width: Pixels,
    load: Load,
    load_task: Option<Task<()>>,
    /// 旧的历史还显示着，新的在后台读
    reloading: bool,
    selected: Option<usize>,
    scroll: UniformListScrollHandle,
    /// 文件历史模式：只看改过这个文件的提交
    file: Option<String>,
    search: Entity<InputState>,
    /// 搜索的内容（小写）；空表示没在搜索
    needle: String,
    /// 匹配的提交的下标，从小到大
    matches: Vec<usize>,
    /// 现在跳到了第几个匹配
    match_pos: Option<usize>,
    details: Option<Details>,
    diff: Entity<DiffState>,
    diff_info: SharedString,
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
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("搜索提交：说明、作者、哈希")));
        let subscriptions = vec![cx.subscribe_in(
            &search,
            window,
            |this, _, event: &InputEvent, _, cx| match event {
                InputEvent::Change => this.update_search(true, cx),
                InputEvent::PressEnter { .. } => this.next_match(1, cx),
                _ => {}
            },
        )];
        let history = Self {
            focus: cx.focus_handle(),
            pending_reveal: None,
            files: Vec::new(),
            collapsed: false,
            details_height: px(340.),
            details_width: px(DETAILS_WIDTH),
            viewport_width: px(1400.),
            load: Load::Idle,
            load_task: None,
            reloading: false,
            selected: None,
            scroll: UniformListScrollHandle::new(),
            file: None,
            search,
            needle: String::new(),
            matches: Vec::new(),
            match_pos: None,
            details: None,
            diff: cx.new(|cx| DiffState::new(Vec::new(), cx)),
            diff_info: tr("点选一个提交查看改动").into(),
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

    /// 详情区占窗口高度的四成，夹在一个范围里，免得矮窗口里把提交列表挤没。
    pub(super) fn set_viewport(&mut self, viewport: Size<Pixels>) {
        self.details_height = (viewport.height * 0.4).clamp(px(150.), px(460.));
        self.details_width = (viewport.width * 0.3).clamp(px(250.), px(DETAILS_WIDTH));
        self.viewport_width = viewport.width;
    }

    /// 选中的提交。
    pub(super) fn selected_commit(&self) -> Option<&Commit> {
        let Load::Ready { data, .. } = &self.load else {
            return None;
        };
        self.selected.and_then(|ix| data.commits.get(ix))
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
        // 文件历史：只要改过这个文件的提交，跟着改名往回找
        let file_mode = self.history.file.is_some();
        let log_args = match &self.history.file {
            Some(path) => gitlog::file_log_args(path),
            None => gitlog::log_args(),
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
                    None => break Err(tr("读取中断").into()),
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

    fn clear_commit_selection(&mut self, cx: &mut Context<Self>) {
        self.history.selected = None;
        self.history.details = None;
        self.history.files.clear();
        self.history.diff_task = None;
        self.history.diff_info = tr("点选一个提交查看改动").into();
        self.history
            .diff
            .update(cx, |state, cx| state.set_files(Vec::new(), cx));
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
            None if complete => {
                self.history.pending_reveal = None;
                self.clear_commit_selection(cx);
                self.history.diff_info = not_found_text(id);
            }
            None => self.history.pending_reveal = Some(id.to_string()),
        }
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

    /// 把键盘焦点放到提交列表上（Ctrl+2）。
    pub fn show_history_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let needle = self.history.search.read(cx).value().trim().to_lowercase();
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

    pub(super) fn select_commit(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Load::Ready { data, .. } = &self.history.load else {
            return;
        };
        let Some(commit) = data.commits.get(ix) else {
            return;
        };
        self.history.selected = Some(ix);
        let id = commit.id.to_string();
        self.load_details(id, cx);
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
        if let Some(path) = &self.history.file {
            show_args.extend(["--".into(), path.clone()]);
        }
        // 换了要看的提交：旧的详情和改动马上撤掉，免得高亮的是这个提交、下面显示的还是上一个的
        self.history.details = None;
        self.history.files.clear();
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
            // 文件历史跟着改名往回找，可改名之前的提交按现在的路径取不到改动：
            // 那就取这个提交的全部改动，并说明原因，不显示成「没有改动」。
            // 改名的那个提交按路径取出来是「新增文件」，也换成全部改动，才看得出是从哪个名字改来的。
            let mut whole = "";
            if in_file && let Ok(bytes) = &out {
                let patch = show::parse(bytes).1;
                if patch.iter().all(u8::is_ascii_whitespace) {
                    whole = tr("这个提交里这个文件还叫别的名字，下面是这个提交的全部改动 · ");
                } else if has_older && is_new_file_patch(patch) {
                    whole =
                        tr("这个文件是在这个提交里改成现在的名字的，下面是这个提交的全部改动 · ");
                }
                if !whole.is_empty() {
                    out = channel
                        .git(&repo, show::args(&id))
                        .await
                        .and_then(|out| out.ok());
                }
            }
            let fetch_time = started.elapsed();
            this.update(cx, |this, cx| {
                match out {
                    Err(err) => {
                        this.history.diff_info =
                            tf!("读取改动失败：{err}", err = format!("{:#}", err)).into()
                    }
                    Ok(bytes) => {
                        let started = Instant::now();
                        let (details, patch) = show::parse(&bytes);
                        this.history.details = Some(details);
                        let text = crate::text::decode(patch);
                        match DiffFile::parse(&text) {
                            Err(err) => {
                                this.history.diff_info =
                                    tf!("diff 解析失败：{message}", message = err.message()).into()
                            }
                            Ok(files) => {
                                let parse_time = started.elapsed();
                                let additions: usize = files.iter().map(|f| f.additions()).sum();
                                let deletions: usize = files.iter().map(|f| f.deletions()).sum();
                                this.history.files = files
                                    .iter()
                                    .map(|f| ChangedFile {
                                        path: f.path().clone(),
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
                                    patch.len() as f64 / 1024.,
                                    parse_time.as_millis()
                                );
                                this.history
                                    .diff
                                    .update(cx, |state, cx| state.set_files(files, cx));
                            }
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// 换了界面语言：搜索框的提示文字、还没选提交时的那句话要重新设一遍。
    pub(super) fn retranslate_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.history.search.update(cx, |state, cx| {
            state.set_placeholder(tr("搜索提交：说明、作者、哈希"), window, cx)
        });
        if self.history.selected.is_none() {
            self.history.diff_info = tr("点选一个提交查看改动").into();
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
        let subject_width = f32::from(self.history.viewport_width)
            - super::sidebar::WIDTH
            - f32::from(graph_width)
            - AUTHOR_WIDTH
            - DATE_WIDTH
            - SHA_WIDTH
            - 40.;
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
        let rows: Vec<AnyElement> = range
            .clone()
            .filter(|ix| *ix < data.commits.len())
            .map(|ix| {
                let commit = &data.commits[ix];
                let row = data.graph.rows[ix];
                let selected = self.history.selected == Some(ix);
                // 搜索时不匹配的提交变淡，提交图照常画
                let dim = searching && self.history.matches.binary_search(&ix).is_err();
                let menu = self.commit_menu(ix, cx);
                let line = h_flex()
                    .id(ix)
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .pr_2()
                    .gap_2()
                    .text_sm()
                    .cursor_pointer()
                    .when(dim, |d| d.opacity(0.35))
                    .when(selected, |d| d.bg(colors.selected))
                    .when(!selected, |d| d.hover(|s| s.bg(colors.hover)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.history.focus.focus(window, cx);
                        this.select_commit(ix, cx)
                    }))
                    // 点右键先选中这一行，菜单里的操作都是对它做的
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, _, cx| {
                            if this.history.selected != Some(ix) {
                                this.select_commit(ix, cx);
                            }
                        }),
                    )
                    .child(graph_paint::graph_cell(
                        data.graph.segments(ix).to_vec(),
                        row.lane,
                        row.color,
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
                                badge_budget,
                                ix,
                                colors,
                            ))
                            .child(
                                div()
                                    .truncate()
                                    .child(SharedString::from(commit.subject.to_string())),
                            ),
                    )
                    .child(
                        h_flex()
                            .w(px(AUTHOR_WIDTH))
                            .flex_none()
                            .gap(px(6.))
                            .child(ui::avatar(&commit.author, 18., cx))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(colors.muted)
                                    .child(SharedString::from(commit.author.to_string())),
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
                            .font_family("Consolas")
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
        // 改了哪些文件：点一下右边的改动滚到它；右键可以看这个文件的历史和逐行追溯
        let files: Vec<AnyElement> = self
            .history
            .files
            .iter()
            .enumerate()
            .map(|(ix, file)| {
                let path = file.path.clone();
                let menu = self.tracked_file_menu(file.path.to_string(), cx);
                let color = match file.letter {
                    "A" => added,
                    "D" => removed,
                    _ => changed,
                };
                h_flex()
                    .id(("changed-file", ix))
                    .h(px(22.))
                    .px_1()
                    .gap_2()
                    .rounded_sm()
                    .text_xs()
                    .cursor_pointer()
                    .hover(|s| s.bg(colors.hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.history
                            .diff
                            .update(cx, |diff, cx| diff.scroll_to_file(&path, cx));
                    }))
                    .child(
                        div()
                            .w(px(10.))
                            .flex_none()
                            .font_weight(FontWeight::BOLD)
                            .text_color(color)
                            .child(file.letter),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(file.path.clone()))
                    .child(
                        div()
                            .flex_none()
                            .text_color(colors.muted)
                            .child(format!("+{} −{}", file.additions, file.deletions)),
                    )
                    .context_menu(menu)
                    .into_any_element()
            })
            .collect();
        let Some(d) = &self.history.details else {
            let text = if self.history.selected.is_some() {
                tr("正在读取…")
            } else {
                tr("没有选中提交")
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
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .truncate()
                            .child(d.author.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.muted)
                            .truncate()
                            .child(format!(
                                "{} · {}",
                                d.author_email,
                                format_time(d.author_time)
                            )),
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
            tr("没有（这是第一个提交）").to_string()
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
                    .when(!files.is_empty(), |panel| {
                        panel
                            .child(div().h(px(1.)).bg(colors.border))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.muted)
                                    .child(tf!("改动的文件（{files}）", files = files.len())),
                            )
                            .child(v_flex().children(files))
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
        let collapsed = self.history.collapsed;
        let details_height = self.history.details_height;
        let details_width = self.history.details_width;
        // 改动视图看得见的宽度，去掉文件标题左边的折叠按钮、两边的内边距和滚动条的位置
        let header_width =
            (self.history.viewport_width - px(super::sidebar::WIDTH) - details_width - px(64.))
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
            Load::Empty => (tr("还没有任何提交").into(), muted, div().into_any_element()),
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
                    tf!("先显示前 {n} 个提交，其余的还在读…", n = data.commits.len())
                } else {
                    tf!("{n} 个提交", n = data.commits.len())
                };
                if self.history.reloading && !partial {
                    text.push_str(tr(" · 正在重读…"));
                }
                if data.graph.max_lanes > graph_paint::MAX_LANES {
                    text = tf!(
                        "提交图同时有 {lanes} 条线，只画得下前 {max} 条，右边带 ··· 的行还有没画出来的 · {text}",
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
                tr("提交还没读完，读到的里面还没有匹配的").into()
            } else {
                tr("没有匹配的提交").into()
            }
        } else {
            match self.history.match_pos {
                Some(pos) => format!("{} / {}", pos + 1, self.history.matches.len()).into(),
                None => tf!(
                    "{matches} 个匹配，回车跳到第一个",
                    matches = self.history.matches.len()
                )
                .into(),
            }
        };
        let has_matches = !self.history.matches.is_empty();

        v_flex()
            .size_full()
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
                        .child(div().flex_1().min_w_0().truncate().child(path.clone()))
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
                                .label(tr("回到所有提交"))
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
                    .child(
                        div().w(px(320.)).flex_shrink(1.).min_w(px(140.)).child(
                            Input::new(&self.history.search)
                                .small()
                                .cleanable(true)
                                .prefix(ui::icon(IconName::Search).text_color(muted)),
                        ),
                    )
                    .child(
                        div()
                            .flex_shrink(1.)
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(muted)
                            .child(match_info),
                    )
                    .when(has_matches, |bar| {
                        bar.child(
                            Button::new("match-prev")
                                .xsmall()
                                .ghost()
                                .icon(ui::icon(IconName::ChevronUp))
                                .tooltip(tr("上一个匹配的提交（Shift+回车）"))
                                .on_click(cx.listener(|this, _, _, cx| this.next_match(-1, cx))),
                        )
                        .child(
                            Button::new("match-next")
                                .xsmall()
                                .ghost()
                                .icon(ui::icon(IconName::ChevronDown))
                                .tooltip(tr("下一个匹配的提交（回车）"))
                                .on_click(cx.listener(|this, _, _, cx| this.next_match(1, cx))),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("pick-file")
                            .small()
                            .ghost()
                            .icon(ui::icon(IconName::FileClock))
                            .label(tr("文件历史…"))
                            .tooltip(tr("选一个文件，看它的历史或逐行追溯"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.pick_file(window, cx)),
                            ),
                    ),
            )
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
                    .child(
                        div()
                            .id("history-status")
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(status)
                            .when_some(status_detail, |d, detail| {
                                d.tooltip(move |window, cx| {
                                    Tooltip::new(detail.clone()).build(window, cx)
                                })
                            }),
                    )
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
                            .tooltip(tr("收起或展开下面的提交详情和改动，给提交列表腾地方"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.history.collapsed = !this.history.collapsed;
                                cx.notify();
                            })),
                    ),
            )
            .when(!collapsed, |view| {
                view.child(
                    h_flex()
                        .h(details_height)
                        .flex_none()
                        .border_t_1()
                        .border_color(border)
                        .child(
                            div()
                                .w(details_width)
                                .flex_none()
                                .h_full()
                                .border_r_1()
                                .border_color(border)
                                .child(details),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .child(
                                    div()
                                        .h(px(26.))
                                        .flex_none()
                                        .px_3()
                                        .flex()
                                        .items_center()
                                        .border_b_1()
                                        .border_color(border)
                                        .text_xs()
                                        .text_color(muted)
                                        .child(self.history.diff_info.clone()),
                                )
                                .child(
                                    div().flex_1().min_h_0().child(
                                        Diff::new(&self.history.diff)
                                            .line_number(true)
                                            .syntax_highlight(true)
                                            .render_header(move |file, _, _| {
                                                diff_file_header(file, header_width, muted)
                                            })
                                            .size_full(),
                                    ),
                                ),
                        ),
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
            "先显示前 {commits} 个提交（{fetch_time} ms 读到），其余的还在读…",
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

fn format_time(ts: i64) -> SharedString {
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
        "提交历史里没有找到 {id}（它可能不在任何分支上）",
        id = &id[..id.len().min(10)]
    )
    .into()
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
    budget: f32,
    row: usize,
    colors: RowColors,
) -> Vec<AnyElement> {
    let labels = ref_labels(refs, is_remote);
    let widths: Vec<f32> = labels.iter().map(|l| badge_width(&l.name)).collect();
    let shown = badges_that_fit(&widths, budget);
    let mut visible: Vec<&RefLabel> = labels[..shown].iter().collect();
    // 两个都放得下时还是「HEAD」在前、分支名在后
    if visible.len() >= 2 && visible[1].kind == RefKind::Head {
        visible.swap(0, 1);
    }
    let mut badges: Vec<AnyElement> = visible
        .into_iter()
        .map(|label| {
            let color: Hsla = match label.kind {
                RefKind::Head => colors.head,
                RefKind::Current | RefKind::Local => colors.local,
                RefKind::Tag => colors.tag,
                RefKind::Remote => colors.remote,
            };
            // 当前分支加粗（和侧栏一样）：「HEAD」被收起来时也认得出
            badge(&label.name, color, label.kind == RefKind::Current)
        })
        .collect();
    if shown < labels.len() {
        // 收起来的那些：鼠标停在「+N」上能看到名字
        let hidden: SharedString = tf!(
            "还有：{join}",
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
                .tooltip(move |window, cx| Tooltip::new(hidden.clone()).build(window, cx))
                .into_any_element(),
        );
    }
    badges
}

fn badge(label: &str, color: Hsla, bold: bool) -> AnyElement {
    div()
        .flex_none()
        .max_w(px(BADGE_TEXT_MAX + 10.))
        .truncate()
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
        .child(SharedString::from(label.to_string()))
        .into_any_element()
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
        DiffFileStatus::Renamed => tr("已改名"),
        DiffFileStatus::Copied => tr("已复制"),
        DiffFileStatus::Conflicted => tr("有冲突"),
        _ => "",
    };
    h_flex()
        .w_full()
        .max_w(width)
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .child(name),
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
