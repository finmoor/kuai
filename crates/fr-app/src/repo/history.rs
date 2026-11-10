//! 提交历史：提交图列表、提交详情、所选提交的改动。
//!
//! 上面有搜索框：按说明、作者、哈希找提交，不匹配的行变淡，回车跳到下一个匹配。
//! 「文件历史」模式下只列出改过某个文件的提交，改动也只显示这个文件的。
//!
//! 第一次读历史时边读边显示：读到前 300 个提交就先画出来（布局和读完全部时的前一段一致，
//! 见 `fr_core::log::Commit::parents`），读完再整个换上。重读（比如提交之后）时等读完再换，
//! 免得滚动位置跳回顶上。

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fr_channel::StreamItem;
use fr_core::graph::Graph;
use fr_core::log::{self as gitlog, Commit};
use fr_core::show::{self, Details};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffFile, DiffState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use crate::graph_paint;

const ROW_HEIGHT: f32 = 26.;
const AUTHOR_WIDTH: f32 = 130.;
const DATE_WIDTH: f32 = 130.;
const SHA_WIDTH: f32 = 76.;
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
}

pub(super) struct History {
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
            cx.new(|cx| InputState::new(window, cx).placeholder("搜索提交：说明、作者、哈希"));
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
            diff_info: "点选一个提交查看改动".into(),
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
        let keep = self.history.selected_commit().map(|c| c.id.to_string());
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
                    None => break Err("读取中断".into()),
                }
            };
            let _ = tx.unbounded_send(Progress::Done(outcome));
        });
        self.history.load_task = Some(cx.spawn(async move |this, cx| {
            // 这个任务被丢弃（比如又开始重读）时，后台的读取跟着取消
            let _worker = worker;
            while let Some(progress) = rx.next().await {
                let applied = this.update(cx, |this, cx| {
                    this.apply_history_progress(progress, keep.as_deref(), cx)
                });
                if applied.is_err() {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn apply_history_progress(
        &mut self,
        progress: Progress,
        keep: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        match progress {
            Progress::Page(data) => {
                log::info!("{}：{}", self.entry.path, data.stats);
                self.history.load = Load::Ready {
                    data,
                    partial: true,
                };
            }
            Progress::Done(Ok(data)) => {
                log::info!("{}：{}", self.entry.path, data.stats);
                let found = keep.and_then(|id| data.commits.iter().position(|c| &*c.id == id));
                self.history.load = Load::Ready {
                    data,
                    partial: false,
                };
                self.history.reloading = false;
                match (keep, found) {
                    (_, Some(ix)) => self.history.selected = Some(ix),
                    // 之前选中的提交没了（比如修改了上次提交）
                    (Some(_), None) => self.clear_commit_selection(cx),
                    (None, None) => {}
                }
                self.update_search(false, cx);
                self.apply_test_hooks(cx);
            }
            Progress::Done(Err(err)) => {
                self.history.reloading = false;
                self.history.load = Load::Failed(format!("读取提交历史失败：{err}").into());
            }
        }
        cx.notify();
    }

    fn clear_commit_selection(&mut self, cx: &mut Context<Self>) {
        self.history.selected = None;
        self.history.details = None;
        self.history.diff_task = None;
        self.history.diff_info = "点选一个提交查看改动".into();
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
        let Load::Ready { data, .. } = &self.history.load else {
            return;
        };
        if let Some(ix) = data.commits.iter().position(|c| &*c.id == id) {
            self.history
                .scroll
                .scroll_to_item(ix, ScrollStrategy::Center);
            self.select_commit(ix, cx);
        }
    }

    /// 文件历史：提交列表只留改过这个文件的提交。
    pub(super) fn show_file_history(&mut self, path: String, cx: &mut Context<Self>) {
        self.history.file = Some(path);
        self.sidebar_select_history();
        self.restart_history(cx);
    }

    pub(super) fn close_file_history(&mut self, cx: &mut Context<Self>) {
        if self.history.file.take().is_some() {
            self.restart_history(cx);
        }
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
        if let Some(path) = &self.history.file {
            show_args.extend(["--".into(), path.clone()]);
        }
        self.history.diff_info = format!("正在读取 {} 的改动…", &id[..id.len().min(10)]).into();

        let started = Instant::now();
        let request = channel.git(&self.entry.path, show_args);
        // 换选中提交时旧任务随 Task 一起丢弃：本机的 git 进程被杀掉，远程的请求发取消
        self.history.diff_task = Some(cx.spawn(async move |this, cx| {
            let out = request.await.and_then(|out| out.ok());
            let fetch_time = started.elapsed();
            this.update(cx, |this, cx| {
                match out {
                    Err(err) => this.history.diff_info = format!("读取改动失败：{err:#}").into(),
                    Ok(bytes) => {
                        let started = Instant::now();
                        let (details, patch) = show::parse(&bytes);
                        this.history.details = Some(details);
                        let text = crate::text::decode(patch);
                        match DiffFile::parse(&text) {
                            Err(err) => {
                                this.history.diff_info =
                                    format!("diff 解析失败：{}", err.message()).into()
                            }
                            Ok(files) => {
                                let parse_time = started.elapsed();
                                let additions: usize = files.iter().map(|f| f.additions()).sum();
                                let deletions: usize = files.iter().map(|f| f.deletions()).sum();
                                this.history.diff_info = format!(
                                    "{} 个文件 · +{} −{} · 读取 {} ms（{:.1} KB）· 解析 {} ms",
                                    files.len(),
                                    additions,
                                    deletions,
                                    fetch_time.as_millis(),
                                    patch.len() as f64 / 1024.,
                                    parse_time.as_millis(),
                                )
                                .into();
                                log::info!("{}：{}", this.entry.path, this.history.diff_info);
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
        let searching = !self.history.needle.is_empty();
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
                    .on_click(cx.listener(move |this, _, _, cx| this.select_commit(ix, cx)))
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
                            .children(ref_badges(&commit.refs, colors))
                            .child(
                                div()
                                    .truncate()
                                    .child(SharedString::from(commit.subject.to_string())),
                            ),
                    )
                    .child(
                        div()
                            .w(px(AUTHOR_WIDTH))
                            .truncate()
                            .text_color(colors.muted)
                            .child(SharedString::from(commit.author.to_string())),
                    )
                    .child(
                        div()
                            .w(px(DATE_WIDTH))
                            .text_color(colors.muted)
                            .child(format_time(commit.time)),
                    )
                    .child(
                        div()
                            .w(px(SHA_WIDTH))
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

    /// 左下角的提交详情：说明、作者、提交者、哈希、父提交。
    fn render_commit_details(&self, colors: RowColors) -> AnyElement {
        let Some(d) = &self.history.details else {
            return div()
                .p_3()
                .text_sm()
                .text_color(colors.muted)
                .child("没有选中提交")
                .into_any_element();
        };
        let line = |label: &'static str, value: String| {
            h_flex()
                .gap_2()
                .text_xs()
                .child(
                    div()
                        .w(px(48.))
                        .flex_none()
                        .text_color(colors.muted)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(value))
        };
        let short = |id: &String| id.chars().take(10).collect::<String>();
        let mut info = v_flex().gap_1().child(line(
            "作者",
            format!(
                "{} <{}> · {}",
                d.author,
                d.author_email,
                format_time(d.author_time)
            ),
        ));
        if d.committer != d.author || d.committer_time != d.author_time {
            info = info.child(line(
                "提交者",
                format!(
                    "{} <{}> · {}",
                    d.committer,
                    d.committer_email,
                    format_time(d.committer_time)
                ),
            ));
        }
        info = info.child(line("哈希", d.id.clone())).child(line(
            "父提交",
            d.parents.iter().map(short).collect::<Vec<_>>().join("  "),
        ));
        div()
            .id("details")
            .size_full()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .p_3()
                    .gap_3()
                    .child(div().text_sm().whitespace_normal().child(d.message.clone()))
                    .child(div().h(px(1.)).bg(colors.border))
                    .child(info),
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
        };
        let graph_width = match &self.history.load {
            Load::Ready { data, .. } => graph_paint::column_width(data.graph.max_lanes),
            _ => px(120.),
        };
        let (status, status_color, list): (SharedString, Hsla, AnyElement) =
            match &self.history.load {
                Load::Idle | Load::Loading => {
                    ("正在读取提交历史…".into(), muted, div().into_any_element())
                }
                Load::Empty => ("还没有任何提交".into(), muted, div().into_any_element()),
                Load::Failed(err) => (err.clone(), danger, div().into_any_element()),
                Load::Ready { data, partial } => {
                    let (build, rows) = self.history.last_rows_build;
                    let mut text =
                        format!("{} · 构建 {} 行 {} µs", data.stats, rows, build.as_micros());
                    if self.history.reloading && !partial {
                        text.push_str(" · 正在重读…");
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

        let match_info: SharedString = if self.history.needle.is_empty() {
            "".into()
        } else if self.history.matches.is_empty() {
            "没有匹配的提交".into()
        } else {
            match self.history.match_pos {
                Some(pos) => format!("{} / {}", pos + 1, self.history.matches.len()).into(),
                None => format!("{} 个匹配，回车跳到第一个", self.history.matches.len()).into(),
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
                        .child(div().text_color(muted).child("文件历史"))
                        .child(div().flex_1().min_w_0().truncate().child(path.clone()))
                        .child(
                            Button::new("file-blame")
                                .xsmall()
                                .label("逐行追溯")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.show_blame(path.clone(), cx)
                                })),
                        )
                        .child(
                            Button::new("close-file-history")
                                .xsmall()
                                .label("回到所有提交")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.close_file_history(cx)),
                                ),
                        ),
                )
            })
            .child(
                h_flex()
                    .h(px(34.))
                    .flex_none()
                    .px_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .w(px(360.))
                            .child(Input::new(&self.history.search).small().cleanable(true)),
                    )
                    .child(div().text_xs().text_color(muted).child(match_info))
                    .when(has_matches, |bar| {
                        bar.child(
                            Button::new("match-prev")
                                .xsmall()
                                .ghost()
                                .label("上一个")
                                .on_click(cx.listener(|this, _, _, cx| this.next_match(-1, cx))),
                        )
                        .child(
                            Button::new("match-next")
                                .xsmall()
                                .ghost()
                                .label("下一个")
                                .on_click(cx.listener(|this, _, _, cx| this.next_match(1, cx))),
                        )
                    }),
            )
            .child(
                h_flex()
                    .h(px(24.))
                    .flex_none()
                    .pr_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .text_xs()
                    .text_color(muted)
                    // 提交图那一列不写标题：分支少时这一列很窄，放不下字（Fork 也不写）
                    .child(div().w(graph_width).flex_none())
                    .child(div().flex_1().child("说明"))
                    .child(div().w(px(AUTHOR_WIDTH)).child("作者"))
                    .child(div().w(px(DATE_WIDTH)).child("时间"))
                    .child(div().w(px(SHA_WIDTH)).child("哈希")),
            )
            .child(div().flex_1().min_h_0().overflow_hidden().child(list))
            .child(
                h_flex()
                    .h(px(22.))
                    .flex_none()
                    .px_3()
                    .border_t_1()
                    .border_color(border)
                    .text_xs()
                    .text_color(status_color)
                    .child(div().truncate().child(status)),
            )
            .child(
                h_flex()
                    .h(px(340.))
                    .flex_none()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        div()
                            .w(px(DETAILS_WIDTH))
                            .flex_none()
                            .h_full()
                            .border_r_1()
                            .border_color(border)
                            .child(self.render_commit_details(colors)),
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
                                        .size_full(),
                                ),
                            ),
                    ),
            )
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
                format!(
                    "{}：{} 帧，平均 {:.2} ms，p50 {:.2} ms，p95 {:.2} ms，p99 {:.2} ms，最慢 {:.2} ms",
                    $name,
                    h.len(),
                    h.mean() / 1e6,
                    ms(h.value_at_quantile(0.5)),
                    ms(h.value_at_quantile(0.95)),
                    ms(h.value_at_quantile(0.99)),
                    ms(h.max()),
                )
            }};
        }
        let report = [
            summarize(
                &format!("连续滚动（每帧 {BENCH_ROWS_PER_FRAME} 行）帧间隔"),
                &mut bench.scroll_intervals,
            ),
            summarize("随机跳转帧间隔", &mut bench.jump_intervals),
            summarize("每帧构建可见行", &mut bench.row_builds),
            hist!("GPUI Window::draw 耗时", snapshot.draw_duration_histogram),
            hist!("GPUI 从变脏到呈现", snapshot.dirty_to_present_histogram),
            hist!("GPUI 呈现间隔", snapshot.present_interval_histogram),
        ]
        .join("\n");
        eprintln!("[M0 滚动测速]\n{report}");
        cx.quit();
    }
}

fn summarize(name: &str, values: &mut [Duration]) -> String {
    if values.is_empty() {
        return format!("{name}：没有数据");
    }
    values.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.;
    let pct = |p: f64| values[((values.len() - 1) as f64 * p).round() as usize];
    let avg = values.iter().sum::<Duration>() / values.len() as u32;
    format!(
        "{name}：{} 次，平均 {:.2} ms，p50 {:.2} ms，p95 {:.2} ms，p99 {:.2} ms，最慢 {:.2} ms",
        values.len(),
        ms(avg),
        ms(pct(0.5)),
        ms(pct(0.95)),
        ms(pct(0.99)),
        ms(values[values.len() - 1]),
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
        Some(git) => format!(
            "{} 个提交 · 读取 {:.2} s（git {:.2} s，{:.1} MB）· 解析 {} ms · 布局 {} ms",
            commits.len(),
            fetch_time.as_secs_f64(),
            git.as_secs_f64(),
            out.len() as f64 / 1e6,
            (t1 - t0).as_millis(),
            (t2 - t1).as_millis(),
        ),
        None => format!(
            "先显示前 {} 个提交（{} ms 读到），其余的还在读…",
            commits.len(),
            fetch_time.as_millis()
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

/// 把 `%D` 的输出拆成小标签：本地分支、远程分支、标签、HEAD。
fn ref_badges(refs: &str, colors: RowColors) -> Vec<AnyElement> {
    if refs.is_empty() {
        return Vec::new();
    }
    let mut badges = Vec::new();
    for item in refs.split(", ") {
        let (label, color): (&str, Hsla) = if let Some(branch) = item.strip_prefix("HEAD -> ") {
            badges.push(badge("HEAD", rgb(0x2f80ed).into(), colors));
            (branch, rgb(0x27ae60).into())
        } else if let Some(tag) = item.strip_prefix("tag: ") {
            (tag, rgb(0xb7950b).into())
        } else if item.contains('/') && !item.starts_with("feature/") {
            (item, rgb(0x9b51e0).into())
        } else {
            (item, rgb(0x27ae60).into())
        };
        badges.push(badge(label, color, colors));
    }
    badges
}

fn badge(label: &str, color: Hsla, colors: RowColors) -> AnyElement {
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(color)
        .text_xs()
        .text_color(color)
        .bg(colors.border.opacity(0.15))
        .child(SharedString::from(label.to_string()))
        .into_any_element()
}
