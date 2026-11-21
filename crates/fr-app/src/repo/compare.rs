//! 比较：两个提交、两个分支，或者提交、分支和本地修改之间差在哪。
//!
//! 侧栏「所有提交」下面有一项「比较」，整个主区域用来比较（单独的比较视图，原型在
//! `docs/design/review-changes/`）：
//! - 顶上一条：基准、对比两端各是一个下拉，可以换成本地修改、HEAD、任意分支、标签，或者填一个
//!   提交的哈希；中间的按钮交换两边；右边写对比比基准领先、落后几个提交，一共改了几个文件。
//! - 左边两栏：「文件」是两端之间有改动的文件（树形或平铺、可过滤，见 [`super::file_tree`]），
//!   「提交」列出两边各自独有的提交。
//! - 右边是选中的那个文件的改动。
//!
//! 两端直接比（`git diff 基准 对比`），看到的是「把基准变成对比要改什么」。一端是本地修改时
//! 没跟踪的文件也算进去（git 自己不列它们，从工作区状态里补）。
//!
//! 进来的路：侧栏点「比较」（两端还没定时挑一对常用的）；提交列表里选中一个提交、按住 Ctrl 点
//! 另一个；提交、分支、标签的右键菜单。两端记着，去别的页面再回来还在；点「关闭」才清掉。

use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;

use fr_channel::Channel;
use fr_core::compare::{self, File};
use fr_core::i18n::{tr, trc};
use fr_core::log::{self as gitlog, Commit};
use fr_core::patch;
use fr_core::status::Kind;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffMode, DiffState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::dialogs::Ask;
use super::diff_search::Where;
use super::diff_tools;
use super::file_tree::{self, Row};
use super::image_diff::{self, Blob, ImageDiff, Source};
use super::layout::Split;
use super::sidebar::SideItem;
use super::{Mode, RepoView};
use crate::ui;

const ROW_HEIGHT: f32 = 24.;
const MONO: &str = "Consolas";
/// 「提交」一栏里每边最多列这么多个
const COMMITS_MAX: usize = 300;
/// 下拉里分支、标签各最多列这么多个，再多的填名字
const MENU_MAX: usize = 200;

/// 比较的一端。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum End {
    /// 本地修改：工作区现在的样子
    Worktree,
    Head,
    /// 本地分支的名字
    Branch(String),
    /// 远程分支，比如 `origin/main`
    Remote(String),
    Tag(String),
    /// 提交的完整哈希
    Commit(String),
}

impl End {
    fn rev(&self) -> compare::End {
        compare::End::Rev(match self {
            End::Worktree => return compare::End::Worktree,
            End::Head => "HEAD".to_string(),
            End::Branch(name) => format!("refs/heads/{name}"),
            End::Remote(name) => format!("refs/remotes/{name}"),
            End::Tag(name) => format!("refs/tags/{name}"),
            End::Commit(id) => id.clone(),
        })
    }

    fn icon(&self) -> IconName {
        match self {
            End::Worktree => IconName::FileDiff,
            End::Head | End::Commit(_) => IconName::GitCommitHorizontal,
            End::Branch(_) | End::Remote(_) => IconName::GitBranch,
            End::Tag(_) => IconName::Tag,
        }
    }
}

/// 两端里的哪一端。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Base,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Files,
    Commits,
}

enum Load {
    Loading,
    Ready,
    Failed(SharedString),
}

/// 清单里的一个文件。
#[derive(Clone)]
struct Item {
    file: File,
    origin: Origin,
}

/// 清单里这一条是怎么来的、改动怎么取。
#[derive(Clone, PartialEq)]
enum Origin {
    /// git 比较两端得出的
    Git,
    /// 工作区里没跟踪的文件：git 的比较里没有它，是从工作区状态里补的
    Untracked,
    /// 提交里有、工作区里取消了跟踪但文件还在（`git rm --cached`）：git 的比较说它删了、工作区状态
    /// 说它没跟踪，合成一条「修改」，比提交里的版本和磁盘上的文件。磁盘上的那份存成了 blob
    /// （[`compare::store_args`]），这里是它的哈希
    Disk(String),
}

/// 文件清单：git 比较出来的（`out`），一端是工作区时加上工作区状态里没跟踪的（`untracked`）。
/// `commit` 是另一端的提交。没跟踪的那个路径 git 的比较里也有时合成一条（见 [`Origin::Disk`]）。
async fn build_items(
    channel: &Channel,
    repo: &str,
    out: &[u8],
    untracked: Vec<String>,
    worktree_is_base: bool,
    commit: Option<String>,
) -> Vec<Item> {
    let mut items: Vec<Item> = compare::parse_files(out)
        .into_iter()
        .map(|file| Item {
            file,
            origin: Origin::Git,
        })
        .collect();
    let (detached, untracked): (Vec<String>, Vec<String>) = untracked
        .into_iter()
        .partition(|path| items.iter().any(|item| item.file.path == *path));
    if let (false, Some(commit)) = (detached.is_empty(), commit) {
        let stored = channel.git(repo, compare::store_args(&detached)).await;
        let blobs: Vec<String> = stored
            .ok()
            .filter(|out| out.success())
            .map(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        // 存不下来（磁盘上的读不了）：只留 git 比较出来的那一条
        if blobs.len() == detached.len() {
            for (path, blob) in detached.iter().zip(blobs) {
                let old = format!("{commit}:{path}");
                let (a, b) = match worktree_is_base {
                    true => (blob.as_str(), old.as_str()),
                    false => (old.as_str(), blob.as_str()),
                };
                let numstat = channel.git(repo, compare::blob_numstat_args(a, b)).await;
                let lines = numstat
                    .ok()
                    .filter(|out| out.success())
                    .and_then(|out| compare::parse_numstat(&out.stdout));
                if let Some(item) = items.iter_mut().find(|item| item.file.path == *path) {
                    item.file.status = 'M';
                    item.file.orig_path = None;
                    item.file.lines = lines;
                    item.origin = Origin::Disk(blob);
                }
            }
        }
    }
    items.extend(untracked.into_iter().map(|path| Item {
        file: File {
            path,
            orig_path: None,
            status: if worktree_is_base { 'D' } else { 'A' },
            lines: None,
        },
        origin: Origin::Untracked,
    }));
    items.sort_by(|a, b| a.file.path.cmp(&b.file.path));
    items
}

/// 「提交」一栏里的一行。
#[derive(Clone)]
enum CommitRow {
    /// 一组的标题：只在哪一端上有，一共几个
    Title(SharedString, usize),
    /// 哪一组（真是对比独有的）、组里的第几个
    Commit(bool, usize),
    /// 这一组是空的
    None,
    /// 没列完
    More(usize),
}

pub(super) struct Compare {
    /// 基准、对比；没在比较时是空的
    ends: Option<(End, End)>,
    tab: Tab,
    load: Load,
    files: Vec<Item>,
    /// 只在基准上的、只在对比上的提交
    behind: Vec<Commit>,
    ahead: Vec<Commit>,
    /// 两边各多了几个提交（落后、领先）；`behind`、`ahead` 没列全时以它为准
    counts: Option<(usize, usize)>,
    /// 选中的文件
    selected: Option<String>,
    /// 「提交」一栏里选中的提交
    picked: Option<String>,
    filter: Entity<InputState>,
    /// 树形里收起的目录
    folded: HashSet<String>,
    scroll: UniformListScrollHandle,
    commit_scroll: UniformListScrollHandle,
    /// 文件清单的键盘焦点
    focus: FocusHandle,
    diff: Entity<DiffState>,
    /// 现在显示的补丁：刷新时内容没变就不动它，滚动位置留着
    diff_text: SharedString,
    /// 没有改动可显示时写的话（正在读取、读取失败、没有差异）
    diff_note: Option<SharedString>,
    /// 忽略空白时改动上面写的那句话（少显示了多少行）
    ws_note: Option<SharedString>,
    /// 选中的是图片时的图片对比
    image: Option<Entity<ImageDiff>>,
    whole_file: bool,
    /// 上次读的时候两端各指着哪个提交；变了才重读
    loaded: Option<(Option<String>, Option<String>)>,
    /// 不在这一页时仓库变了：回来时重读
    dirty: bool,
    load_task: Option<Task<()>>,
    diff_task: Option<Task<()>>,
    resolve_task: Option<Task<()>>,
}

impl Compare {
    pub(super) fn new(window: &mut Window, cx: &mut Context<RepoView>) -> (Self, Subscription) {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(tr("过滤文件…")));
        let subscription = cx.subscribe(&filter, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.compare.scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            }
        });
        let compare = Self {
            ends: None,
            tab: Tab::Files,
            load: Load::Ready,
            files: Vec::new(),
            behind: Vec::new(),
            ahead: Vec::new(),
            counts: None,
            selected: None,
            picked: None,
            filter,
            folded: HashSet::new(),
            scroll: UniformListScrollHandle::new(),
            commit_scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            diff: cx.new(|cx| DiffState::new(Vec::new(), cx)),
            diff_text: SharedString::default(),
            diff_note: None,
            ws_note: None,
            image: None,
            whole_file: false,
            loaded: None,
            dirty: false,
            load_task: None,
            diff_task: None,
            resolve_task: None,
        };
        (compare, subscription)
    }

    fn item(&self, path: &str) -> Option<&Item> {
        self.files.iter().find(|item| item.file.path == path)
    }

    /// 改动的种类用哪个颜色、增删的行数加起来。
    fn totals(&self) -> (usize, usize) {
        let lines = self.files.iter().filter_map(|item| item.file.lines);
        lines.fold((0, 0), |(added, removed), (a, d)| {
            (added + a as usize, removed + d as usize)
        })
    }
}

/// 一段界面文字大概多宽：汉字、全角标点按字号算，字母、数字、间隔号这些按一半多一点
/// （估得偏宽）。只用来定顶上一条放不放得下。
fn text_width(text: &str, size: f32) -> f32 {
    let wide = text.chars().filter(|c| *c >= '\u{2e80}').count() as f32;
    let narrow = text.chars().count() as f32 - wide;
    wide * size + narrow * size * 0.48
}

/// 菜单里的名字太长就省掉中间一段：菜单不会自己省略，长名字会冲到菜单外面。
fn menu_name(name: &str) -> String {
    const KEEP: usize = 26;
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= KEEP * 2 + 1 {
        return name.to_string();
    }
    let head: String = chars[..KEEP].iter().collect();
    let tail: String = chars[chars.len() - KEEP..].iter().collect();
    format!("{head}…{tail}")
}

/// 下拉最宽、最窄给多少。窗口窄的时候它们自己缩，名字放不下就省略。
const END_MAX: f32 = 260.;
const END_MIN: f32 = 96.;

/// 顶上一条里放不放右边那句话。`room` 是去掉固定的东西以后剩下的宽度，`summary` 是那句话要的
/// 宽度，`needs` 是两个下拉把名字写全各要多宽。放不下就不放（左边两栏的标题上有文件数、
/// 提交数），地方留给两个下拉。
fn summary_fits(room: f32, summary: f32, needs: (f32, f32)) -> bool {
    needs.0.min(END_MAX) + needs.1.min(END_MAX) + summary <= room
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// 改动种类的颜色：新增绿、删除红、改名和复制蓝，其余黄。
fn status_color(status: char, cx: &App) -> Hsla {
    let theme = cx.theme();
    match status {
        'A' => theme.success,
        'D' => theme.danger,
        'R' | 'C' => theme.info,
        _ => theme.warning,
    }
}

impl RepoView {
    /// 换了界面语言：过滤框里的提示文字是建的时候定的，要重新设一遍。
    pub(super) fn retranslate_compare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.compare.filter.update(cx, |state, cx| {
            state.set_placeholder(tr("过滤文件…"), window, cx)
        });
    }

    /// 这一端显示的名字。
    fn end_name(&self, end: &End) -> String {
        match end {
            End::Worktree => tr("本地修改").to_string(),
            End::Head => "HEAD".to_string(),
            End::Branch(name) | End::Remote(name) | End::Tag(name) => name.clone(),
            End::Commit(id) => short(id).to_string(),
        }
    }

    /// 这一端现在指着哪个提交；本地修改没有，已经不在了的分支、标签也没有。
    fn end_sha(&self, end: &End) -> Option<String> {
        let branch = |list: &[fr_core::refs::Branch], name: &str| {
            let mut list = list.iter();
            list.find(|b| b.name == name).map(|b| b.target.clone())
        };
        match end {
            End::Worktree => None,
            End::Head => self.status.as_ref().and_then(|s| s.head.clone()),
            End::Branch(name) => branch(&self.refs.branches, name),
            End::Remote(name) => branch(&self.refs.remotes, name),
            End::Tag(name) => {
                let mut tags = self.refs.tags.iter();
                tags.find(|t| t.name == *name).map(|t| t.target.clone())
            }
            End::Commit(id) => Some(id.clone()),
        }
    }

    /// 填的名字正好是哪个分支、标签：按它算，显示名字不显示哈希。
    fn named_end(&self, name: &str) -> Option<End> {
        let has = |list: &[fr_core::refs::Branch]| list.iter().any(|b| b.name == name);
        if name == "HEAD" {
            Some(End::Head)
        } else if has(&self.refs.branches) {
            Some(End::Branch(name.to_string()))
        } else if has(&self.refs.remotes) {
            Some(End::Remote(name.to_string()))
        } else if self.refs.tags.iter().any(|t| t.name == name) {
            Some(End::Tag(name.to_string()))
        } else {
            None
        }
    }

    /// 「提交」一栏里这一端叫什么：本地修改按它所在的提交（HEAD）算。
    fn end_commit_name(&self, end: &End) -> String {
        match end {
            End::Worktree => "HEAD".to_string(),
            other => self.end_name(other),
        }
    }

    /// 当前分支这一端；没在分支上（分离的 HEAD）就是 HEAD。
    pub(super) fn current_end(&self) -> End {
        match self.status.as_ref().and_then(|s| s.branch.clone()) {
            Some(branch) => End::Branch(branch),
            None => End::Head,
        }
    }

    /// 还没定两端时挑一对：当前分支不是主分支就和主分支比；是的话，和上游有出入就和上游比；
    /// 都不是就看本地修改相对 HEAD 改了什么。
    fn default_ends(&self) -> (End, End) {
        let current = self.refs.branches.iter().find(|b| b.current);
        let current_name = current.map(|b| b.name.as_str());
        for main in ["main", "master"] {
            let exists = self.refs.branches.iter().any(|b| b.name == main);
            if exists && current_name != Some(main) {
                return (End::Branch(main.to_string()), self.current_end());
            }
        }
        if let Some(branch) = current
            && let Some(upstream) = &branch.upstream
            && !branch.gone
            && (branch.ahead > 0 || branch.behind > 0)
        {
            return (
                End::Remote(upstream.clone()),
                End::Branch(branch.name.clone()),
            );
        }
        (End::Head, End::Worktree)
    }

    /// 侧栏点了「比较」。
    pub(super) fn show_compare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_file_views(window, cx);
        self.sidebar.selected = SideItem::Compare;
        self.mode = Mode::Compare;
        if self.compare.ends.is_none() {
            let (base, other) = self.default_ends();
            self.compare.ends = Some((base, other));
            self.load_compare(true, cx);
        } else if std::mem::take(&mut self.compare.dirty) {
            self.load_compare(false, cx);
        }
        cx.notify();
    }

    /// 比较这两端（从右键菜单、Ctrl 加点击来）。
    pub(super) fn compare_ends(
        &mut self,
        base: End,
        other: End,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.leave_file_views(window, cx);
        self.sidebar.selected = SideItem::Compare;
        self.mode = Mode::Compare;
        self.compare.ends = Some((base, other));
        self.compare.tab = Tab::Files;
        self.load_compare(true, cx);
        cx.notify();
    }

    fn set_compare_end(&mut self, side: Side, end: End, cx: &mut Context<Self>) {
        let Some((base, other)) = &mut self.compare.ends else {
            return;
        };
        let slot = match side {
            Side::Base => base,
            Side::Other => other,
        };
        if *slot == end {
            return;
        }
        *slot = end;
        self.load_compare(true, cx);
        cx.notify();
    }

    fn swap_compare(&mut self, cx: &mut Context<Self>) {
        let Some((base, other)) = self.compare.ends.take() else {
            return;
        };
        self.compare.ends = Some((other, base));
        // 两边独有的提交对调就行，文件的增删要重读
        self.load_compare(true, cx);
        cx.notify();
    }

    /// 「关闭」：两端清掉，回到提交历史。
    fn close_compare(&mut self, cx: &mut Context<Self>) {
        self.compare.ends = None;
        self.compare.files.clear();
        self.compare.behind.clear();
        self.compare.ahead.clear();
        self.compare.counts = None;
        self.compare.selected = None;
        self.compare.loaded = None;
        self.compare.load_task = None;
        self.compare.diff_task = None;
        self.clear_compare_diff(None, cx);
        self.sidebar_select_history();
        cx.notify();
    }

    /// 刷新之后：两端指着的提交变了（分支往前走了），或者有一端是本地修改，就重读。
    pub(super) fn compare_refreshed(&mut self, cx: &mut Context<Self>) {
        let Some((base, other)) = &self.compare.ends else {
            return;
        };
        let worktree = matches!(base, End::Worktree) || matches!(other, End::Worktree);
        let now = (self.end_sha(base), self.end_sha(other));
        if !worktree && self.compare.loaded.as_ref() == Some(&now) {
            return;
        }
        if self.mode == Mode::Compare {
            self.load_compare(false, cx);
        } else {
            self.compare.dirty = true;
        }
    }

    fn clear_compare_diff(&mut self, note: Option<SharedString>, cx: &mut Context<Self>) {
        self.compare.diff_text = SharedString::default();
        self.compare.diff_note = note;
        self.compare.ws_note = None;
        self.compare
            .diff
            .update(cx, |state, cx| state.set_files(Vec::new(), cx));
        self.update_diff_search(Where::Compare, false, cx);
    }

    pub(super) fn compare_diff_state(&self) -> Entity<DiffState> {
        self.compare.diff.clone()
    }

    pub(super) fn compare_diff_text(&self) -> SharedString {
        self.compare.diff_text.clone()
    }

    /// 读两端之间的文件清单和两边各自独有的提交。`fresh` 是换了两端：旧的马上撤掉；
    /// 否则是仓库变了重读，旧的先留着，读完再换，选中的文件不变。
    fn load_compare(&mut self, fresh: bool, cx: &mut Context<Self>) {
        let (Some(channel), Some((base, other))) =
            (self.channel.clone(), self.compare.ends.clone())
        else {
            return;
        };
        self.compare.dirty = false;
        self.compare.loaded = Some((self.end_sha(&base), self.end_sha(&other)));
        if fresh {
            self.compare.files.clear();
            self.compare.behind.clear();
            self.compare.ahead.clear();
            self.compare.counts = None;
            self.compare.selected = None;
            self.compare.picked = None;
            self.compare.load = Load::Loading;
            self.compare.diff_task = None;
            self.compare.scroll.scroll_to_item(0, ScrollStrategy::Top);
            self.clear_compare_diff(None, cx);
        }
        let (a, b) = (base.rev(), other.rev());
        let Some(args) = compare::files_args(&a, &b) else {
            // 两端都是本地修改
            self.compare.load = Load::Ready;
            self.compare.load_task = None;
            return;
        };
        let repo = self.entry.path.clone();
        let (ra, rb) = (
            compare::rev_of(&a).to_string(),
            compare::rev_of(&b).to_string(),
        );
        let read_files = channel.git(&repo, args);
        let read_count = channel.git(&repo, compare::count_args(&ra, &rb));
        let read_ahead = channel.git(&repo, compare::only_in_args(&rb, &ra, COMMITS_MAX));
        let read_behind = channel.git(&repo, compare::only_in_args(&ra, &rb, COMMITS_MAX));
        // 没跟踪的文件 git 的比较里不列：一端是本地修改时从工作区状态里补
        let worktree_is_base = a == compare::End::Worktree;
        let untracked: Vec<String> = match worktree_is_base || b == compare::End::Worktree {
            true => self
                .status
                .iter()
                .flat_map(|status| status.entries.iter())
                .filter(|entry| entry.unstaged == Some(Kind::Untracked) && !entry.submodule)
                .map(|entry| entry.path.clone())
                .collect(),
            false => Vec::new(),
        };
        // 另一端的提交：取消了跟踪、还在磁盘上的文件拿它里面的版本比
        let commit = match (&a, &b) {
            (compare::End::Worktree, compare::End::Rev(rev))
            | (compare::End::Rev(rev), compare::End::Worktree) => Some(rev.clone()),
            _ => None,
        };
        // 换两端、再刷新时旧任务随 Task 一起丢弃
        self.compare.load_task = Some(cx.spawn(async move |this, cx| {
            let (files, count, ahead, behind) =
                futures::join!(read_files, read_count, read_ahead, read_behind);
            let items = match files.and_then(|out| out.ok()) {
                Ok(out) => {
                    Ok(
                        build_items(&channel, &repo, &out, untracked, worktree_is_base, commit)
                            .await,
                    )
                }
                Err(err) => Err(err),
            };
            this.update(cx, |this, cx| {
                match items {
                    Err(err) => {
                        let text = tf!("读取失败：{err}", err = format!("{:#}", err));
                        this.compare.load = Load::Failed(text.into());
                        this.compare.files.clear();
                    }
                    Ok(items) => {
                        this.compare.files = items;
                        this.compare.load = Load::Ready;
                    }
                }
                let commits = |out: anyhow::Result<fr_channel::GitOutput>| {
                    out.ok()
                        .filter(|out| out.success())
                        .map(|out| gitlog::parse(&out.stdout))
                        .unwrap_or_default()
                };
                this.compare.ahead = commits(ahead);
                this.compare.behind = commits(behind);
                this.compare.counts = count
                    .ok()
                    .filter(|out| out.success())
                    .and_then(|out| compare::parse_count(&out.stdout));
                // 选中的文件还在就重读它的改动，不在了选第一个
                let keep = this.compare.selected.clone();
                let keep = keep.filter(|path| this.compare.item(path).is_some());
                match keep {
                    Some(_) => this.load_compare_diff(cx),
                    None => match this.first_compare_file(cx) {
                        Some(first) => {
                            this.compare.selected = None;
                            this.select_compare_file(first, cx);
                        }
                        None => {
                            this.compare.selected = None;
                            this.compare.diff_task = None;
                            this.clear_compare_diff(None, cx);
                        }
                    },
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn select_compare_file(&mut self, path: String, cx: &mut Context<Self>) {
        if self.compare.selected.as_deref() == Some(path.as_str()) {
            return;
        }
        self.compare.selected = Some(path);
        self.clear_compare_diff(Some(tr("正在读取…").into()), cx);
        self.load_compare_diff(cx);
        cx.notify();
    }

    /// 读选中的那个文件在两端之间的改动。
    pub(super) fn load_compare_diff(&mut self, cx: &mut Context<Self>) {
        let (Some(channel), Some((base, other)), Some(path)) = (
            self.channel.clone(),
            self.compare.ends.clone(),
            self.compare.selected.clone(),
        ) else {
            return;
        };
        let Some(item) = self.compare.item(&path).cloned() else {
            return;
        };
        let (a, b) = (base.rev(), other.rev());
        // 图片：另外建一个图片对比（同一个文件重读时只刷新，不重建）。
        // 新增的没有旧的，删除的没有新的；改了名的旧图在原来的路径上
        let blob = |end: &compare::End| match end {
            compare::End::Worktree => Blob::Worktree,
            compare::End::Rev(rev) => Blob::Rev(rev.clone()),
        };
        match image_diff::is_image(&path) {
            false => self.compare.image = None,
            true => {
                let old_path = item.file.orig_path.as_deref().unwrap_or(&path);
                let old = (item.file.status != 'A').then(|| Source::new(blob(&a), old_path));
                let new = (item.file.status != 'D').then(|| Source::new(blob(&b), path.as_str()));
                match &self.compare.image {
                    Some(view) if view.read(cx).shows(&old, &new) => {
                        view.update(cx, |view, cx| view.reload(cx))
                    }
                    _ => {
                        let (app, repo) = (self.app.clone(), self.entry.path.clone());
                        let channel = channel.clone();
                        self.compare.image =
                            Some(cx.new(|cx| ImageDiff::new(app, channel, repo, old, new, cx)));
                    }
                }
            }
        }
        let (args, ok): (Option<Vec<String>>, &'static [i32]) = match &item.origin {
            Origin::Untracked => {
                let reverse = a == compare::End::Worktree;
                (Some(compare::untracked_patch_args(&path, reverse)), &[0, 1])
            }
            Origin::Git => (
                compare::patch_args(&a, &b, &path, item.file.orig_path.as_deref()),
                &[0],
            ),
            Origin::Disk(blob) => {
                let commit = match (&a, &b) {
                    (compare::End::Rev(rev), _) | (_, compare::End::Rev(rev)) => rev.clone(),
                    _ => return,
                };
                let old = format!("{commit}:{path}");
                let args = match a == compare::End::Worktree {
                    true => compare::blob_patch_args(blob, &old),
                    false => compare::blob_patch_args(&old, blob),
                };
                (Some(args), &[0])
            }
        };
        // 和磁盘上存下来的那份比的：补丁里 blob 的名字换成路径
        let named = match &item.origin {
            Origin::Disk(blob) => Some((blob.clone(), path.clone())),
            _ => None,
        };
        let Some(mut args) = args else {
            return;
        };
        if self.compare.whole_file {
            patch::set_context(&mut args, patch::WHOLE_FILE);
        }
        let plain = self.ws_plain(&channel, &mut args);
        let request = channel.git(&self.entry.path, args);
        // 换选中的文件时旧任务随 Task 一起丢弃
        self.compare.diff_task = Some(cx.spawn(async move |this, cx| {
            let mut result = request.await;
            let mut plain = diff_tools::plain_out(plain).await;
            if let Some((blob, path)) = &named {
                if let Ok(out) = &mut result {
                    out.stdout = compare::name_blob(&out.stdout, blob, path);
                }
                if let Some(plain) = &mut plain {
                    *plain = compare::name_blob(plain, blob, path);
                }
            }
            // 解码、解析放到后台：整个文件、超长的行解析要好一会儿，不能占着界面线程
            let result = match result {
                Ok(out) if out.code.is_some_and(|code| ok.contains(&code)) => {
                    let parsed = cx.background_spawn(async move {
                        let ws_note = plain
                            .as_deref()
                            .map(|plain| diff_tools::ws_note(plain, &out.stdout));
                        let (text, files) = diff_tools::parse_patch(&out.stdout);
                        (text, files, ws_note)
                    });
                    Ok(parsed.await)
                }
                Ok(out) => Err(out.stderr),
                Err(err) => Err(format!("{err:#}")),
            };
            this.update(cx, |this, cx| {
                let failed =
                    |err: String| -> SharedString { tf!("读取改动失败：{err}", err).into() };
                match result {
                    Ok((text, files, ws_note)) => {
                        // 内容没变（别的文件触发的刷新）：不动，滚动位置留着
                        if this.compare.diff_note.is_none() && *text == *this.compare.diff_text {
                            this.compare.ws_note = ws_note;
                            cx.notify();
                            return;
                        }
                        match files {
                            Err(note) => this.clear_compare_diff(Some(note), cx),
                            Ok(files) if files.is_empty() => {
                                let note = match ws_note {
                                    Some(_) => tr("忽略空白后，此文件在两端没有差异"),
                                    None => tr("此文件在两端没有差异"),
                                };
                                this.clear_compare_diff(Some(note.into()), cx);
                            }
                            Ok(files) => {
                                this.compare.diff_note = None;
                                this.compare.ws_note = ws_note;
                                this.compare.diff_text = text.into();
                                this.compare
                                    .diff
                                    .update(cx, |state, cx| state.set_files(files, cx));
                                this.update_diff_search(Where::Compare, false, cx);
                            }
                        }
                    }
                    Err(err) => this.clear_compare_diff(Some(failed(err)), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// 切换「显示整个文件」，重读正在看的文件。
    fn toggle_compare_whole_file(&mut self, cx: &mut Context<Self>) {
        self.compare.whole_file = !self.compare.whole_file;
        let whole = self.compare.whole_file;
        self.compare.diff.update(cx, |state, cx| {
            state.set_context_lines(if whole { None } else { Some(3) }, cx)
        });
        self.load_compare_diff(cx);
        cx.notify();
    }

    /// 文件清单现在显示的行：过滤、树形还是平铺、收起的目录都算进去了。
    fn compare_rows(&self, cx: &App) -> Vec<Row> {
        let needle = self.compare.filter.read(cx).value().trim().to_lowercase();
        let shown: Vec<(&str, usize)> = self
            .compare
            .files
            .iter()
            .enumerate()
            .filter(|(_, item)| file_tree::matches(&item.file.path, &needle))
            .map(|(ix, item)| (item.file.path.as_str(), ix))
            .collect();
        let tree = self.app.read(cx).settings.file_tree;
        // 过滤时目录全部展开
        let folded = &self.compare.folded;
        file_tree::rows(&shown, tree, &|path| {
            needle.is_empty() && folded.contains(path)
        })
    }

    /// 清单里显示的第一个文件（树形里目录在前，不一定是路径最靠前的那个）。
    fn first_compare_file(&self, cx: &App) -> Option<String> {
        let rows = self.compare_rows(cx);
        let first = rows.iter().find_map(|row| match row {
            Row::File { index, .. } => self.compare.files.get(*index),
            Row::Dir { .. } => None,
        });
        first.map(|item| item.file.path.clone())
    }

    /// 文件清单上的按键：上下键换文件（跳过目录）。
    fn compare_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let down = match event.keystroke.key.as_str() {
            "down" => true,
            "up" => false,
            _ => return,
        };
        if self.compare.tab != Tab::Files {
            return;
        }
        cx.stop_propagation();
        let rows = self.compare_rows(cx);
        let path_at = |row: &Row| match row {
            Row::File { index, .. } => {
                let item = self.compare.files.get(*index);
                item.map(|item| item.file.path.clone())
            }
            Row::Dir { .. } => None,
        };
        let at = rows
            .iter()
            .position(|row| path_at(row).is_some() && path_at(row) == self.compare.selected);
        let mut order: Vec<usize> = match (at, down) {
            (Some(at), true) => (at + 1..rows.len()).collect(),
            (Some(at), false) => (0..at).rev().collect(),
            (None, _) => (0..rows.len()).collect(),
        };
        order.retain(|ix| matches!(rows[*ix], Row::File { .. }));
        let Some(next) = order.first().copied() else {
            return;
        };
        if let Some(path) = path_at(&rows[next]) {
            self.compare
                .scroll
                .scroll_to_item(next, ScrollStrategy::Nearest);
            self.select_compare_file(path, cx);
        }
    }

    /// 到提交历史里看这个提交。
    fn reveal_from_compare(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_file_views(window, cx);
        self.sidebar_select_history();
        self.reveal_commit(&id, cx);
        cx.notify();
    }

    /// 填一个哈希或者引用名当一端：先让 git 认一下是不是提交。
    fn ask_compare_rev(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let ask = Ask {
            title: tr("输入提交的哈希").into(),
            placeholder: tr("提交的哈希，或分支、标签的名称").into(),
            ok: tr("确定"),
            validate: Some(Box::new(|_, answer| {
                answer
                    .text
                    .starts_with('-')
                    .then(|| tr("不能以 - 开头").to_string())
            })),
            ..Ask::default()
        };
        self.ask(ask, window, cx, move |this, answer, window, cx| {
            let Some(channel) = this.channel.clone() else {
                return;
            };
            let typed = answer.text;
            let request = channel.git(&this.entry.path, compare::resolve_args(&typed));
            this.compare.resolve_task = Some(cx.spawn_in(window, async move |this, cx| {
                let id = request
                    .await
                    .and_then(|out| out.ok())
                    .map(|out| String::from_utf8_lossy(&out).trim().to_string())
                    .ok()
                    .filter(|id| !id.is_empty());
                this.update_in(cx, |this, window, cx| match id {
                    Some(id) => {
                        let end = this.named_end(&typed).unwrap_or(End::Commit(id));
                        this.set_compare_end(side, end, cx)
                    }
                    None => {
                        let text = tf!("找不到提交 {typed}", typed);
                        window.push_notification(Notification::warning(text), cx);
                    }
                })
                .ok();
            }));
        });
    }

    // ---------- 画 ----------

    pub(super) fn render_compare(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let (border, bg) = (cx.theme().border, cx.theme().background);
        let Some((base, other)) = self.compare.ends.clone() else {
            return div().size_full().bg(bg).into_any_element();
        };
        // 并排还是上下排跟着设置走：别处改了，这里重画时跟上
        let split = self.app.read(cx).settings.diff_split;
        let mode = if split {
            DiffMode::Split
        } else {
            DiffMode::Unified
        };
        if self.compare.diff.read(cx).mode() != mode {
            self.compare
                .diff
                .update(cx, |state, cx| state.set_mode(mode, cx));
        }
        let left_width = px(self.sizes(cx).details_width);
        let bar = self.render_compare_bar(&base, &other, cx);
        let left = self.render_compare_left(&base, &other, cx);
        let right = self.render_compare_diff(split, cx);
        v_flex()
            .size_full()
            .bg(bg)
            .child(bar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .w(left_width)
                            .flex_none()
                            .h_full()
                            .relative()
                            .border_r_1()
                            .border_color(border)
                            .child(left)
                            .child(self.split_handle(Split::DetailsWidth, cx)),
                    )
                    .child(right),
            )
            .into_any_element()
    }

    /// 顶上一条：两端、交换、差多少、关闭。
    fn render_compare_bar(&self, base: &End, other: &End, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, accent, danger) = (
            theme.border,
            theme.muted_foreground,
            theme.primary,
            theme.danger,
        );
        let (added_color, removed_color) = (theme.success, theme.danger);
        let label = |text: &'static str| {
            div()
                .flex_none()
                .text_xs()
                .text_color(muted)
                .child(text)
                .into_any_element()
        };
        let both_worktree = matches!((base, other), (End::Worktree, End::Worktree));
        // 右边那句话：差几个提交、几个文件、增删多少行
        let (text, totals): (SharedString, Option<(usize, usize)>) = match &self.compare.load {
            Load::Loading => (tr("正在读取…").into(), None),
            Load::Failed(_) => (SharedString::default(), None),
            Load::Ready if both_worktree => (SharedString::default(), None),
            Load::Ready => {
                let files = tf!("{n} 个文件", n = self.compare.files.len());
                let text = match self.compare.counts {
                    Some((behind, ahead)) => tf!(
                        "领先 {ahead} · 落后 {behind} · {files}",
                        ahead,
                        behind,
                        files
                    ),
                    None => files,
                };
                (text.into(), Some(self.compare.totals()))
            }
        };
        let numbers = totals.map(|(added, removed)| (format!("+{added}"), format!("−{removed}")));
        // 放不放得下是估的：这一条的宽度去掉固定的那些，剩下的够不够两个下拉加那句话
        let (title, base_label, other_label, close) =
            (tr("比较"), tr("基准"), tr("对比"), tr("关闭"));
        let fixed = 24.
            + 14.
            + text_width(title, 14.)
            + text_width(base_label, 12.)
            + text_width(other_label, 12.)
            + 24.
            + (30. + text_width(close, 14.))
            + 8. * 9.;
        let summary_width = text_width(&text, 12.)
            + numbers.as_ref().map_or(0., |(added, removed)| {
                text_width(added, 12.) + text_width(removed, 12.) + 16.
            });
        let room = f32::from(self.viewport.width) - self.sizes(cx).sidebar - fixed;
        // 下拉里：图标、名字、哈希、箭头，加两边的内边距
        let need = |end: &End| {
            let sha = match end {
                End::Commit(_) | End::Worktree => 0.,
                _ => 6. + text_width("00000000", 12.),
            };
            26. + 14. + 6. + text_width(&self.end_name(end), 15.) + sha + 6. + 16.
        };
        let show_summary = summary_fits(room, summary_width, (need(base), need(other)));
        let summary: AnyElement = match &self.compare.load {
            // 读取失败的原因总要写出来，占剩下的地方
            Load::Failed(err) => ui::clipped("compare-error", err.clone())
                .flex_1()
                .text_xs()
                .text_color(danger)
                .into_any_element(),
            _ if !show_summary || text.is_empty() => div().into_any_element(),
            _ => {
                let tip = tf!(
                    "{other} 相对 {base}：领先、落后的是提交数",
                    other = self.end_commit_name(other),
                    base = self.end_commit_name(base)
                );
                h_flex()
                    .flex_none()
                    .gap(px(8.))
                    .text_xs()
                    .child(
                        div()
                            .id("compare-summary")
                            .text_color(muted)
                            .child(text.clone())
                            .when(totals.is_some(), |summary| {
                                summary.tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
                            }),
                    )
                    .children(numbers.map(|(added, removed)| {
                        h_flex()
                            .gap(px(8.))
                            .child(div().text_color(added_color).child(added))
                            .child(div().text_color(removed_color).child(removed))
                    }))
                    .into_any_element()
            }
        };
        h_flex()
            .flex_none()
            .h(px(44.))
            .px(px(12.))
            .gap(px(8.))
            .border_b_1()
            .border_color(border)
            .child(
                ui::icon(IconName::GitCompare)
                    .flex_none()
                    .text_color(accent),
            )
            .child(
                div()
                    .flex_none()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(label(base_label))
            .child(self.end_button(Side::Base, base, cx))
            .child(
                Button::new("compare-swap")
                    .ghost()
                    .xsmall()
                    .flex_none()
                    .icon(ui::icon(IconName::ArrowLeftRight))
                    .tooltip(tr("交换两边"))
                    .on_click(cx.listener(|this, _, _, cx| this.swap_compare(cx))),
            )
            .child(label(other_label))
            .child(self.end_button(Side::Other, other, cx))
            .child(summary)
            .child(div().flex_1())
            .child(
                Button::new("compare-close")
                    .ghost()
                    .small()
                    .flex_none()
                    .icon(ui::icon(IconName::X))
                    .label(close)
                    .tooltip(tr("结束比较，回到所有提交"))
                    .on_click(cx.listener(|this, _, _, cx| this.close_compare(cx))),
            )
            .into_any_element()
    }

    /// 一端的下拉：现在是什么，点开换成别的。
    fn end_button(&self, side: Side, end: &End, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let name = self.end_name(end);
        let sha = match end {
            // 名字已经是哈希了
            End::Commit(_) => None,
            _ => self.end_sha(end),
        };
        // 已经不在了的分支、标签：下面读取会失败，这里先标出来
        let gone = !matches!(end, End::Worktree) && self.end_sha(end).is_none();
        let view = cx.entity().downgrade();
        let current = end.clone();
        let head = self.status.as_ref().and_then(|s| s.head.clone());
        let names = |list: &[fr_core::refs::Branch]| -> Vec<String> {
            list.iter().map(|b| b.name.clone()).collect()
        };
        let (branches, remotes) = (names(&self.refs.branches), names(&self.refs.remotes));
        // 标签按名字排的，新的多半在后面：倒过来列
        let tags: Vec<String> = self
            .refs
            .tags
            .iter()
            .rev()
            .map(|t| t.name.clone())
            .collect();
        let id = match side {
            Side::Base => "compare-base",
            Side::Other => "compare-other",
        };
        Button::new(id)
            .outline()
            .small()
            .flex_shrink(1.)
            .min_w(px(END_MIN))
            .max_w(px(END_MAX))
            .child(
                h_flex()
                    .min_w_0()
                    .gap(px(6.))
                    .child(ui::icon(end.icon()).flex_none().text_color(muted))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .when(gone, |name| name.text_color(muted).line_through())
                            .child(name),
                    )
                    .children(sha.map(|sha| {
                        div()
                            .flex_none()
                            .text_xs()
                            .font_family(MONO)
                            .text_color(muted)
                            .child(short(&sha).to_string())
                    })),
            )
            .dropdown_caret(true)
            .dropdown_menu(move |menu, _, _| {
                let pick = |end: End, label: String| {
                    let (view, checked) = (view.clone(), end == current);
                    PopupMenuItem::new(label)
                        .checked(checked)
                        .on_click(move |_, _, cx| {
                            let end = end.clone();
                            view.update(cx, |this, cx| this.set_compare_end(side, end, cx))
                                .ok();
                        })
                };
                let many = branches.len() + remotes.len() + tags.len() > 12;
                let mut menu = menu.min_w(px(260.));
                if many {
                    menu = menu.max_h(px(440.)).scrollable(true);
                }
                let mut menu = menu
                    .label(tr("本地"))
                    .item(pick(End::Worktree, tr("本地修改（工作区）").to_string()));
                if let Some(head) = &head {
                    menu = menu.item(pick(End::Head, format!("HEAD  {}", short(head))));
                }
                if !branches.is_empty() || !remotes.is_empty() {
                    menu = menu.label(trc("分组", "分支"));
                }
                for name in branches.iter().take(MENU_MAX) {
                    menu = menu.item(pick(End::Branch(name.clone()), menu_name(name)));
                }
                for name in remotes.iter().take(MENU_MAX) {
                    menu = menu.item(pick(End::Remote(name.clone()), menu_name(name)));
                }
                if !tags.is_empty() {
                    menu = menu.label(trc("分组", "标签"));
                }
                for name in tags.iter().take(MENU_MAX) {
                    menu = menu.item(pick(End::Tag(name.clone()), menu_name(name)));
                }
                let hidden = branches.len().saturating_sub(MENU_MAX)
                    + remotes.len().saturating_sub(MENU_MAX)
                    + tags.len().saturating_sub(MENU_MAX);
                let view = view.clone();
                let label = match hidden {
                    0 => tr("输入提交的哈希…").to_string(),
                    n => tf!("输入提交的哈希或名称（另有 {n} 项未列出）…", n),
                };
                menu.separator()
                    .item(PopupMenuItem::new(label).icon(IconName::Search).on_click(
                        move |_, window, cx| {
                            view.update(cx, |this, cx| this.ask_compare_rev(side, window, cx))
                                .ok();
                        },
                    ))
            })
            .into_any_element()
    }

    /// 左边：「文件」「提交」两栏。
    fn render_compare_left(
        &mut self,
        base: &End,
        other: &End,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, accent, fg) = (
            theme.border,
            theme.muted_foreground,
            theme.primary,
            theme.foreground,
        );
        let now = self.compare.tab;
        let commits = match self.compare.counts {
            Some((behind, ahead)) => behind + ahead,
            None => self.compare.behind.len() + self.compare.ahead.len(),
        };
        let tab = |id: &'static str, which: Tab, title: &'static str, count: usize| {
            let on = now == which;
            h_flex()
                .id(id)
                .h_full()
                .px(px(4.))
                .gap(px(6.))
                .border_b_2()
                .border_color(if on {
                    accent
                } else {
                    gpui_kit::transparent_black()
                })
                .text_sm()
                .when(on, |tab| tab.font_weight(FontWeight::SEMIBOLD))
                .when(!on, |tab| {
                    tab.text_color(muted)
                        .cursor_pointer()
                        .hover(|s| s.text_color(fg))
                })
                .child(title)
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(FontWeight::NORMAL)
                        .text_color(muted)
                        .child(count.to_string()),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.compare.tab = which;
                    cx.notify();
                }))
        };
        let tabs = h_flex()
            .flex_none()
            .h(px(32.))
            .px(px(10.))
            .gap(px(12.))
            .border_b_1()
            .border_color(border)
            .child(tab(
                "compare-tab-files",
                Tab::Files,
                tr("文件"),
                self.compare.files.len(),
            ))
            .child(tab(
                "compare-tab-commits",
                Tab::Commits,
                trc("分栏", "提交"),
                commits,
            ));
        let body = match now {
            Tab::Files => self.render_compare_files(base, other, cx),
            Tab::Commits => self.render_compare_commits(base, other, cx),
        };
        v_flex()
            .size_full()
            .child(tabs)
            .child(body)
            .into_any_element()
    }

    /// 「文件」一栏：过滤框、树形和平铺的切换，下面是清单。
    fn render_compare_files(
        &mut self,
        base: &End,
        other: &End,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (muted, danger) = (cx.theme().muted_foreground, cx.theme().danger);
        let tree = self.app.read(cx).settings.file_tree;
        let (app, view) = (self.app.clone(), cx.entity().downgrade());
        let head = h_flex()
            .flex_none()
            .px(px(8.))
            .pt(px(8.))
            .pb(px(4.))
            .gap(px(6.))
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.compare.filter)
                        .small()
                        .cleanable(true)
                        .prefix(ui::icon(IconName::Search).text_color(muted)),
                ),
            )
            .child(file_tree::mode_toggle(
                "compare-list-mode",
                tree,
                move |tree, _, cx| {
                    app.update(cx, |app, cx| {
                        app.update_settings(cx, |s| s.file_tree = tree)
                    });
                    view.update(cx, |_, cx| cx.notify()).ok();
                },
            ));
        let note = |text: SharedString, color: Hsla| {
            div()
                .p_3()
                .text_sm()
                .text_color(color)
                .whitespace_normal()
                .child(text)
                .into_any_element()
        };
        let rows = Rc::new(self.compare_rows(cx));
        let both_worktree = matches!((base, other), (End::Worktree, End::Worktree));
        let list: AnyElement = match &self.compare.load {
            Load::Failed(err) => note(err.clone(), danger),
            Load::Loading if self.compare.files.is_empty() => note(tr("正在读取…").into(), muted),
            _ if both_worktree => note(tr("两端均为本地修改，没有可比较的内容。").into(), muted),
            _ if self.compare.files.is_empty() => note(tr("两端没有差异。").into(), muted),
            _ if rows.is_empty() => note(tr("没有匹配的文件").into(), muted),
            _ => {
                let rows = rows.clone();
                uniform_list(
                    "compare-files",
                    rows.len(),
                    cx.processor(move |this, range: Range<usize>, _window, cx| {
                        this.render_compare_rows(&rows, range, tree, cx)
                    }),
                )
                .track_scroll(&self.compare.scroll)
                .size_full()
                .into_any_element()
            }
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .child(head)
            .child(
                div()
                    .id("compare-file-list")
                    .flex_1()
                    .min_h_0()
                    .track_focus(&self.compare.focus)
                    .on_key_down(cx.listener(Self::compare_key))
                    .child(list),
            )
            .into_any_element()
    }

    fn render_compare_rows(
        &mut self,
        rows: &[Row],
        range: Range<usize>,
        tree: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.theme();
        let (muted, selected_bg, hover, info) = (
            theme.muted_foreground,
            theme.list_active,
            theme.list_hover,
            theme.info,
        );
        let (added_color, removed_color) = (theme.success, theme.danger);
        range
            .filter_map(|ix| {
                let indent = |depth: usize| px(8. + depth as f32 * file_tree::INDENT);
                match rows.get(ix)? {
                    Row::Dir {
                        path,
                        label,
                        depth,
                        files,
                        open,
                    } => {
                        let key = path.clone();
                        Some(
                            h_flex()
                                .id(("compare-dir", ix))
                                .h(px(ROW_HEIGHT))
                                .w_full()
                                .pl(indent(*depth))
                                .pr(px(10.))
                                .gap(px(5.))
                                .text_sm()
                                .cursor_pointer()
                                .hover(|s| s.bg(hover))
                                .children(file_tree::dir_cells(
                                    ("compare-dir-name", ix),
                                    label,
                                    path,
                                    *files,
                                    *open,
                                    cx,
                                ))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.compare.focus.focus(window, cx);
                                    if !this.compare.folded.remove(&key) {
                                        this.compare.folded.insert(key.clone());
                                    }
                                    cx.notify();
                                }))
                                .into_any_element(),
                        )
                    }
                    Row::File { index, depth } => {
                        let item = self.compare.files.get(*index)?;
                        let path = item.file.path.clone();
                        let (dir, name) = file_tree::split_name(&item.file.path);
                        let (dir, name) = (dir.trim_end_matches('/').to_string(), name.to_string());
                        let is_selected = self.compare.selected.as_deref() == Some(path.as_str());
                        let color = status_color(item.file.status, cx);
                        let stats: AnyElement = match (&item.origin, item.file.lines) {
                            (Origin::Untracked, _) => div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(muted)
                                .child(tr("未跟踪"))
                                .into_any_element(),
                            (_, None) => div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(muted)
                                .child(tr("二进制"))
                                .into_any_element(),
                            (_, Some((added, removed))) => h_flex()
                                .flex_none()
                                .gap(px(4.))
                                .text_size(px(11.))
                                .child(div().text_color(added_color).child(format!("+{added}")))
                                .child(div().text_color(removed_color).child(format!("−{removed}")))
                                .into_any_element(),
                        };
                        let full: SharedString = match &item.file.orig_path {
                            Some(orig) => format!("{orig} → {path}").into(),
                            None => path.clone().into(),
                        };
                        // 根目录下、没改名的：提示里的全名就是行里的名字，只在名字被省略时弹
                        let bare = dir.is_empty() && item.file.orig_path.is_none();
                        let row = h_flex()
                            .id(("compare-file", ix))
                            .h(px(ROW_HEIGHT))
                            .w_full()
                            .pl(indent(*depth))
                            .pr(px(10.))
                            // 树形里和本地修改一样：前面空出箭头那么宽，名字对着目录的名字
                            .gap(px(if tree { 3. } else { 8. }))
                            .text_sm()
                            .cursor_pointer()
                            .when(is_selected, |row| row.bg(selected_bg))
                            .when(!is_selected, |row| row.hover(|s| s.bg(hover)))
                            // 右边显示的是哪个文件：左边一条色带，和本地修改里一样
                            .border_l_2()
                            .border_color(if is_selected {
                                info
                            } else {
                                gpui_kit::transparent_black()
                            })
                            .on_click(cx.listener({
                                let path = path.clone();
                                move |this, _, window, cx| {
                                    this.compare.focus.focus(window, cx);
                                    this.select_compare_file(path.clone(), cx);
                                }
                            }))
                            .when(tree, |row| {
                                row.child(div().flex_none().w(px(file_tree::FILE_LEAD)))
                            })
                            .child(file_tree::kind_badge(item.file.status.to_string(), color))
                            // 名字先占地方，放不下时省略中间；平铺时后面跟着目录
                            .child(match bare {
                                true => ui::clipped(("compare-name", ix), name)
                                    .flex_shrink(1.)
                                    .text_ellipsis_middle()
                                    .into_any_element(),
                                false => div()
                                    .flex_shrink(1.)
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis_middle()
                                    .child(ui::one_line(name))
                                    .into_any_element(),
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(if tree || dir.is_empty() { 0. } else { 36. }))
                                    .truncate()
                                    .text_xs()
                                    .text_color(muted)
                                    .when(!tree, |cell| cell.child(ui::one_line(dir))),
                            )
                            .child(stats)
                            .when(!bare, |row| {
                                row.tooltip(move |window, cx| ui::tip(full.clone(), window, cx))
                            });
                        // 没跟踪的文件没有历史可看
                        Some(match item.origin == Origin::Untracked {
                            true => row.into_any_element(),
                            false => row
                                .context_menu(self.tracked_file_menu(path, cx))
                                .into_any_element(),
                        })
                    }
                }
            })
            .collect()
    }

    /// 「提交」一栏：两边各自独有的提交。
    fn render_compare_commits(
        &mut self,
        base: &End,
        other: &End,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        if matches!(self.compare.load, Load::Loading) && self.compare.counts.is_none() {
            return div()
                .p_3()
                .text_sm()
                .text_color(muted)
                .child(tr("正在读取…"))
                .into_any_element();
        }
        let (behind_total, ahead_total) = self
            .compare
            .counts
            .unwrap_or((self.compare.behind.len(), self.compare.ahead.len()));
        let mut rows: Vec<CommitRow> = Vec::new();
        let mut group = |ahead: bool, end: &End, shown: usize, total: usize| {
            let title = tf!("仅 {name} 有", name = self.end_commit_name(end));
            rows.push(CommitRow::Title(title.into(), total.max(shown)));
            match shown {
                0 => rows.push(CommitRow::None),
                n => rows.extend((0..n).map(|ix| CommitRow::Commit(ahead, ix))),
            }
            if total > shown && shown > 0 {
                rows.push(CommitRow::More(shown));
            }
        };
        group(true, other, self.compare.ahead.len(), ahead_total);
        group(false, base, self.compare.behind.len(), behind_total);
        let rows = Rc::new(rows);
        let count = rows.len();
        div()
            .flex_1()
            .min_h_0()
            .child(
                uniform_list(
                    "compare-commits",
                    count,
                    cx.processor(move |this, range: Range<usize>, _window, cx| {
                        this.render_compare_commit_rows(&rows, range, cx)
                    }),
                )
                .track_scroll(&self.compare.commit_scroll)
                .size_full(),
            )
            .into_any_element()
    }

    fn render_compare_commit_rows(
        &mut self,
        rows: &[CommitRow],
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.theme();
        let (muted, selected_bg, hover, accent) = (
            theme.muted_foreground,
            theme.list_active,
            theme.list_hover,
            theme.primary,
        );
        let view = cx.entity().downgrade();
        range
            .filter_map(|ix| {
                let line = || h_flex().h(px(ROW_HEIGHT)).w_full().px(px(12.)).gap(px(8.));
                Some(match rows.get(ix)? {
                    CommitRow::Title(title, count) => line()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(ui::clipped(("compare-group", ix), title.clone()).flex_shrink(1.))
                        .child(
                            div()
                                .flex_none()
                                .font_weight(FontWeight::NORMAL)
                                .text_color(muted)
                                .child(count.to_string()),
                        )
                        .into_any_element(),
                    CommitRow::None => line()
                        .justify_center()
                        .text_xs()
                        .text_color(muted)
                        .child(tr("无"))
                        .into_any_element(),
                    CommitRow::More(shown) => line()
                        .text_xs()
                        .text_color(muted)
                        .child(tf!("仅列出最近 {n} 个", n = shown))
                        .into_any_element(),
                    CommitRow::Commit(ahead, at) => {
                        let list = match ahead {
                            true => &self.compare.ahead,
                            false => &self.compare.behind,
                        };
                        let commit = list.get(*at)?;
                        let id = commit.id.to_string();
                        let is_picked = self.compare.picked.as_deref() == Some(id.as_str());
                        let tip: SharedString = format!(
                            "{}\n{} · {}",
                            commit.subject,
                            commit.author,
                            super::history::format_time(commit.time)
                        )
                        .into();
                        let (click_id, menu_id, copy_id) = (id.clone(), id.clone(), id.clone());
                        let menu_view = view.clone();
                        line()
                            .id(("compare-commit", ix))
                            .text_sm()
                            .cursor_pointer()
                            .when(is_picked, |row| row.bg(selected_bg))
                            .when(!is_picked, |row| row.hover(|s| s.bg(hover)))
                            .child(
                                div()
                                    .flex_none()
                                    .size(px(9.))
                                    .rounded_full()
                                    .border_2()
                                    .border_color(accent),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(SharedString::from(commit.subject.to_string())),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .font_family(MONO)
                                    .text_color(muted)
                                    .child(short(&id).to_string()),
                            )
                            .tooltip(move |window, cx| ui::full_text_tip(tip.clone(), window, cx))
                            // 单击选中，双击到提交历史里看它
                            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                this.compare.picked = Some(click_id.clone());
                                if event.click_count() >= 2 {
                                    this.reveal_from_compare(click_id.clone(), window, cx);
                                }
                                cx.notify();
                            }))
                            .context_menu(move |menu, _, _| {
                                let (view, id, copy) =
                                    (menu_view.clone(), menu_id.clone(), copy_id.clone());
                                menu.item(PopupMenuItem::new(tr("在所有提交中显示")).on_click(
                                    move |_, window, cx| {
                                        let id = id.clone();
                                        view.update(cx, |this, cx| {
                                            this.reveal_from_compare(id, window, cx)
                                        })
                                        .ok();
                                    },
                                ))
                                .separator()
                                .item(
                                    PopupMenuItem::new(tr("复制哈希")).on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy.clone(),
                                        ));
                                    }),
                                )
                            })
                            .into_any_element()
                    }
                })
            })
            .collect()
    }

    /// 右边：选中的文件的改动。
    fn render_compare_diff(&mut self, split: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        let (added_color, removed_color) = (theme.success, theme.danger);
        let item = self.compare.selected.as_deref();
        let item = item.and_then(|path| self.compare.item(path));
        let title: AnyElement = match item {
            None => div().flex_1().into_any_element(),
            Some(item) => {
                let (dir, name) = file_tree::split_name(&item.file.path);
                let old = item.file.orig_path.clone();
                let stats = item.file.lines.map(|(added, removed)| {
                    h_flex()
                        .flex_none()
                        .gap(px(4.))
                        .text_xs()
                        .child(div().text_color(added_color).child(format!("+{added}")))
                        .child(div().text_color(removed_color).child(format!("−{removed}")))
                });
                let path: AnyElement = match old {
                    // 改名的：原来的路径和现在的写成一句，放不下时省略前面
                    Some(old) => ui::clipped(
                        "compare-renamed-path",
                        format!("{old} → {}", item.file.path),
                    )
                    .flex_shrink(1.)
                    .text_ellipsis_start()
                    .text_sm()
                    .into_any_element(),
                    None => h_flex()
                        .flex_shrink(1.)
                        .min_w_0()
                        .text_sm()
                        .child(
                            ui::clipped("compare-dir-path", dir.to_string())
                                .flex_shrink(1.)
                                .text_ellipsis_start()
                                .text_color(muted),
                        )
                        .child(
                            div()
                                .flex_none()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(ui::one_line(name.to_string())),
                        )
                        .into_any_element(),
                };
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(8.))
                    .child(ui::icon(IconName::File).flex_none().text_color(muted))
                    .child(path)
                    .children(stats)
                    .child(div().flex_1())
                    .into_any_element()
            }
        };
        let whole_file = self.compare.whole_file;
        let ws_toggle = self.ws_toggle("compare-ignore-ws", cx);
        let search_toggle = self.search_toggle(Where::Compare, cx);
        let search_bar = self.render_diff_search(Where::Compare, cx);
        let search_marks = self.search_decorations(Where::Compare, cx);
        let ws_strip = self
            .compare
            .ws_note
            .clone()
            .filter(|_| self.ignore_ws)
            .map(|note| diff_tools::info_strip("compare-ws-note", note, cx));
        let item = self.compare.selected.as_deref();
        let item = item.and_then(|path| self.compare.item(path));
        let (prev_state, next_state) = (self.compare.diff.clone(), self.compare.diff.clone());
        let has_diff = self.compare.diff_note.is_none() && item.is_some();
        let bar = h_flex()
            .h(px(32.))
            .flex_none()
            .pl(px(12.))
            .pr(px(8.))
            .gap(px(2.))
            .border_b_1()
            .border_color(border)
            .child(title)
            .when(has_diff, |bar| {
                bar.child(
                    Button::new("compare-prev-change")
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::ArrowUp))
                        .tooltip(tr("上一处改动"))
                        .on_click(move |_, _, cx| {
                            prev_state.update(cx, |state, cx| state.previous_change(cx))
                        }),
                )
                .child(
                    Button::new("compare-next-change")
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::ArrowDown))
                        .tooltip(tr("下一处改动"))
                        .on_click(move |_, _, cx| {
                            next_state.update(cx, |state, cx| state.next_change(cx))
                        }),
                )
            })
            .child(search_toggle)
            .child(ws_toggle)
            .child(
                Button::new("compare-whole-file")
                    .xsmall()
                    .ghost()
                    .icon(ui::icon(IconName::UnfoldVertical))
                    .selected(whole_file)
                    .tooltip(tr("显示整个文件"))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_compare_whole_file(cx))),
            )
            .child(
                Button::new("compare-split")
                    .xsmall()
                    .ghost()
                    .icon(ui::icon(IconName::Columns2))
                    .selected(split)
                    .tooltip(tr("并排显示：左侧为旧版本，右侧为新版本"))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_split(cx))),
            );
        let note = match (&self.compare.diff_note, item.is_some()) {
            (Some(note), _) => Some(note.clone()),
            (None, false) if matches!(self.compare.load, Load::Ready) => {
                Some(match self.compare.files.is_empty() {
                    true => SharedString::default(),
                    false => tr("选择一个文件以查看改动").into(),
                })
            }
            (None, false) => Some(SharedString::default()),
            (None, true) => None,
        };
        // 图片不显示「二进制文件有差异」，显示两张图（读没读到它自己会写）
        let image = self.compare.image.clone().filter(|_| item.is_some());
        let body: AnyElement = match note {
            _ if image.is_some() => div().size_full().children(image).into_any_element(),
            Some(note) => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_sm()
                .text_color(muted)
                .whitespace_normal()
                .child(note)
                .into_any_element(),
            None => Diff::new(&self.compare.diff)
                .line_number(true)
                .syntax_highlight(true)
                .decorations(search_marks)
                .header_visible(false)
                .size_full()
                .into_any_element(),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(bar)
            .children(search_bar)
            .children(ws_strip)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{END_MAX, END_MIN, menu_name, summary_fits, text_width};

    #[test]
    fn 顶上一条放不放那句话() {
        assert!(summary_fits(700., 200., (120., 180.)));
        assert!(!summary_fits(400., 200., (120., 180.)));
        // 名字太长的按上限算
        assert!(summary_fits(120. + END_MAX + 200., 200., (120., 400.)));
        const { assert!(END_MIN < END_MAX) };
    }

    #[test]
    fn 估文字的宽度() {
        assert_eq!(text_width("比较", 14.), 28.);
        // 间隔号、减号是窄的
        assert_eq!(text_width("·−", 10.), text_width("ab", 10.));
        assert!((text_width("main", 10.) - 19.2).abs() < 0.01);
        assert_eq!(text_width("", 14.), 0.);
    }

    #[test]
    fn 菜单里的长名字省掉中间() {
        assert_eq!(menu_name("feat/compare"), "feat/compare");
        let long = format!("{}{}", "a".repeat(40), "b".repeat(40));
        let short = menu_name(&long);
        assert_eq!(short.chars().count(), 53);
        assert!(short.starts_with("aaa") && short.ends_with("bbb") && short.contains('…'));
    }
}
