//! M0 主界面：提交图列表、diff 视图、提交信息输入框。

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fr_core::graph::Graph;
use fr_core::log::Commit;
use gpui_kit::component::diff::{Diff, DiffFile, DiffState};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::graph_paint;

const ROW_HEIGHT: f32 = 26.;
const AUTHOR_WIDTH: f32 = 130.;
const DATE_WIDTH: f32 = 130.;
const SHA_WIDTH: f32 = 76.;

struct Loaded {
    commits: Arc<Vec<Commit>>,
    graph: Arc<Graph>,
    stats: SharedString,
}

enum Load {
    Loading,
    Ready(Loaded),
    Failed(SharedString),
}

/// 一行要用到的主题颜色，每帧取一次。
#[derive(Clone, Copy)]
struct RowColors {
    selected: Hsla,
    hover: Hsla,
    muted: Hsla,
    border: Hsla,
}

pub struct RepoView {
    repo: PathBuf,
    load: Load,
    selected: Option<usize>,
    scroll: UniformListScrollHandle,
    diff: Entity<DiffState>,
    diff_info: SharedString,
    diff_task: Option<Task<()>>,
    message: Entity<TextareaState>,
    /// 最近一次构建可见行花的时间和行数，用来观察滚动时的开销
    last_rows_build: (Duration, usize),
    /// 设置 FR_M0_BENCH=1 时的自动滚动测速
    bench: Option<Bench>,
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
    pub fn new(repo: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let diff = cx.new(|cx| DiffState::new(Vec::new(), cx));
        let message = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("提交信息：用中文输入法在这里打字，看候选框位置对不对、能不能正常上屏")
        });

        let path = repo.clone();
        let task = cx
            .background_executor()
            .spawn(async move { load_repo(&path) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.load = match result {
                    Ok(loaded) => Load::Ready(loaded),
                    Err(err) => Load::Failed(format!("读取失败：{err:#}").into()),
                };
                this.apply_test_hooks(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();

        Self {
            repo,
            load: Load::Loading,
            selected: None,
            scroll: UniformListScrollHandle::new(),
            diff,
            diff_info: "点选一个提交查看改动".into(),
            diff_task: None,
            message,
            last_rows_build: (Duration::ZERO, 0),
            bench: None,
        }
    }

    /// 测试钩子：FR_M0_SELECT=<说明里的文字> 自动选中第一个匹配的提交；FR_M0_BENCH=1 开始滚动测速。
    fn apply_test_hooks(&mut self, cx: &mut Context<Self>) {
        let Load::Ready(loaded) = &self.load else {
            return;
        };
        if let Ok(needle) = std::env::var("FR_M0_SELECT")
            && let Some(ix) = loaded
                .commits
                .iter()
                .position(|c| c.subject.contains(&needle))
        {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
            self.select(ix, cx);
        }
        if std::env::var("FR_M0_BENCH").is_ok_and(|v| v == "1") {
            self.bench = Some(Bench {
                seed: 0x2545_f491_4f6c_dd1d,
                ..Default::default()
            });
        }
    }

    /// 每帧推进一次测速；返回 true 表示测完了。
    fn step_bench(&mut self, window: &mut Window) -> bool {
        let (Load::Ready(loaded), Some(bench)) = (&self.load, &mut self.bench) else {
            return false;
        };
        let count = loaded.commits.len();
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
        self.scroll.scroll_to_item(bench.pos, ScrollStrategy::Top);
        window.request_animation_frame();
        false
    }

    fn finish_bench(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut bench) = self.bench.take() else {
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

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Load::Ready(loaded) = &self.load else {
            return;
        };
        self.selected = Some(ix);
        let id = loaded.commits[ix].id.to_string();
        let repo = self.repo.clone();
        self.diff_info = format!("正在读取 {} 的改动…", &id[..10]).into();

        let task = cx.background_executor().spawn(async move {
            let started = Instant::now();
            let mut args = fr_core::log::SHOW_ARGS.to_vec();
            args.push(&id);
            let out = fr_core::git::run(&repo, &args);
            (out, started.elapsed())
        });
        // 换选中提交时旧任务随 Task 一起丢弃，不会把旧结果写回来
        self.diff_task = Some(cx.spawn(async move |this, cx| {
            let (out, git_time) = task.await;
            this.update(cx, |this, cx| {
                match out {
                    Err(err) => this.diff_info = format!("git show 失败：{err:#}").into(),
                    Ok(bytes) => {
                        let started = Instant::now();
                        let text = String::from_utf8_lossy(&bytes);
                        match DiffFile::parse(&text) {
                            Err(err) => {
                                this.diff_info = format!("diff 解析失败：{}", err.message()).into()
                            }
                            Ok(files) => {
                                let parse_time = started.elapsed();
                                let additions: usize = files.iter().map(|f| f.additions()).sum();
                                let deletions: usize = files.iter().map(|f| f.deletions()).sum();
                                this.diff_info = format!(
                                    "{} 个文件 · +{} −{} · git show {} ms（{:.1} KB）· 解析 {} ms",
                                    files.len(),
                                    additions,
                                    deletions,
                                    git_time.as_millis(),
                                    bytes.len() as f64 / 1024.,
                                    parse_time.as_millis(),
                                )
                                .into();
                                this.diff.update(cx, |state, cx| state.set_files(files, cx));
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

    fn render_rows(
        &mut self,
        range: Range<usize>,
        colors: RowColors,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let started = Instant::now();
        let Load::Ready(loaded) = &self.load else {
            return Vec::new();
        };
        let graph_width = graph_paint::column_width(loaded.graph.max_lanes);
        let rows: Vec<AnyElement> = range
            .clone()
            .map(|ix| {
                let commit = &loaded.commits[ix];
                let row = loaded.graph.rows[ix];
                let selected = self.selected == Some(ix);
                h_flex()
                    .id(ix)
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .pr_2()
                    .gap_2()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(colors.selected))
                    .when(!selected, |d| d.hover(|s| s.bg(colors.hover)))
                    .on_click(cx.listener(move |this, _, _, cx| this.select(ix, cx)))
                    .child(graph_paint::graph_cell(
                        loaded.graph.segments(ix).to_vec(),
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
                            .child(SharedString::from(commit.id[..8].to_string())),
                    )
                    .into_any_element()
            })
            .collect();
        let elapsed = started.elapsed();
        self.last_rows_build = (elapsed, range.len());
        if let Some(bench) = &mut self.bench {
            bench.row_builds.push(elapsed);
        }
        rows
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

impl Render for RepoView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.step_bench(window) {
            self.finish_bench(window, cx);
        }
        let graph_width = match &self.load {
            Load::Ready(loaded) => graph_paint::column_width(loaded.graph.max_lanes),
            _ => px(120.),
        };
        let theme = cx.theme();
        let (bg, fg, border, muted, title_bar) = (
            theme.background,
            theme.foreground,
            theme.border,
            theme.muted_foreground,
            theme.title_bar,
        );
        let colors = RowColors {
            selected: theme.list_active,
            hover: theme.list_hover,
            muted,
            border,
        };

        let (status, list): (SharedString, AnyElement) = match &self.load {
            Load::Loading => (
                format!("正在读取 {} 的提交历史…", self.repo.display()).into(),
                div().into_any_element(),
            ),
            Load::Failed(err) => (err.clone(), div().into_any_element()),
            Load::Ready(loaded) => {
                let (build, rows) = self.last_rows_build;
                (
                    format!(
                        "{} · 最近一次构建 {} 行用时 {} µs",
                        loaded.stats,
                        rows,
                        build.as_micros()
                    )
                    .into(),
                    uniform_list(
                        "commits",
                        loaded.commits.len(),
                        cx.processor(move |this, range: Range<usize>, _window, cx| {
                            this.render_rows(range, colors, cx)
                        }),
                    )
                    .track_scroll(&self.scroll)
                    .size_full()
                    .into_any_element(),
                )
            }
        };

        v_flex()
            .size_full()
            .bg(bg)
            .text_color(fg)
            .child(
                h_flex()
                    .h(px(30.))
                    .px_3()
                    .flex_none()
                    .bg(title_bar)
                    .border_b_1()
                    .border_color(border)
                    .text_xs()
                    .child(status),
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
                    .child(div().w(graph_width).flex_none().pl_2().child("提交图"))
                    .child(div().flex_1().child("说明"))
                    .child(div().w(px(AUTHOR_WIDTH)).child("作者"))
                    .child(div().w(px(DATE_WIDTH)).child("时间"))
                    .child(div().w(px(SHA_WIDTH)).child("哈希")),
            )
            .child(div().flex_1().min_h_0().overflow_hidden().child(list))
            .child(
                h_flex()
                    .h(px(380.))
                    .flex_none()
                    .border_t_1()
                    .border_color(border)
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
                                    .child(self.diff_info.clone()),
                            )
                            .child(
                                div().flex_1().min_h_0().child(
                                    Diff::new(&self.diff)
                                        .line_number(true)
                                        .syntax_highlight(true)
                                        .size_full(),
                                ),
                            ),
                    )
                    .child(
                        v_flex()
                            .w(px(380.))
                            .flex_none()
                            .h_full()
                            .p_3()
                            .gap_2()
                            .border_l_1()
                            .border_color(border)
                            .child(div().text_sm().child("提交信息（测试中文输入法）"))
                            .child(Textarea::new(&self.message).h(px(160.)))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("请切换到微软拼音输入几句中文：看候选框是否跟着光标、能否正常上屏、退格和方向键是否正常。"),
                            ),
                    ),
            )
    }
}

/// 读取提交历史并计算布局，在后台线程执行。
fn load_repo(repo: &Path) -> anyhow::Result<Loaded> {
    let t0 = Instant::now();
    let out = fr_core::git::run(repo, fr_core::log::LOG_ARGS)?;
    let t1 = Instant::now();
    let commits = fr_core::log::parse(&out);
    let t2 = Instant::now();
    let graph = Graph::build(&commits);
    let t3 = Instant::now();
    let stats = format!(
        "{} 个提交 · git log {:.2} s（{:.1} MB）· 解析 {} ms · 布局 {} ms · 最多 {} 条泳道",
        commits.len(),
        (t1 - t0).as_secs_f64(),
        out.len() as f64 / 1e6,
        (t2 - t1).as_millis(),
        (t3 - t2).as_millis(),
        graph.max_lanes,
    );
    Ok(Loaded {
        commits: Arc::new(commits),
        graph: Arc::new(graph),
        stats: stats.into(),
    })
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
