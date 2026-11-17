//! 本地修改：未暂存和已暂存的文件、选中文件的改动（按块、按行操作）、提交说明。
//!
//! 和 Fork 一样：单击文件看改动，双击暂存或取消暂存；改动里按块、按行暂存、取消暂存或丢弃；
//! 提交说明框里 Ctrl+Enter 提交。
//!
//! 文件列表可以多选（Ctrl、Shift 加点击）和用键盘操作：上下键换文件，空格或回车暂存、取消暂存，
//! Delete 丢弃，Ctrl+A 全选这一栏，Ctrl+Enter 提交。
//!
//! 文件列表上面一行：过滤框（只留路径里有这几个字的文件）、树形和平铺的切换（见
//! [`super::file_tree`]，和提交详情、比较共用一个设置）、筛选按钮（隐藏未跟踪的文件）。
//! 选择、上下键、Shift 连选都按列表里显示的顺序走；过滤掉的、藏起来的文件不参与。

use fr_core::i18n::tr;
use fr_core::tf;
use std::collections::{BTreeSet, HashSet};
use std::ops::Range;
use std::rc::Rc;

use fr_core::patch::{self, LineKind};
use fr_core::status::{Entry, Kind, Status};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _, DropdownButton};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::diff::{Diff, DiffEvent, DiffFile, DiffLineRange, DiffMode, DiffState};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::diff_search::Where;
use super::diff_tools;
use super::file_tree::{self, Row as ListRow};
use super::image_diff::{self, Blob, ImageDiff, Source};
use super::layout::Split;
use super::ops::Step;
use super::stage_diff::{Row, StageDiff};
use super::{Mode, RepoView};
use crate::ui;

const FILE_ROW_HEIGHT: f32 = 24.;
/// diff 里每一行（包括块标题）的高度。uniform_list 按量出来的一行给所有行排版，
/// 行高必须一样；块标题里的 xsmall 按钮高 20px，正好放得下
const DIFF_ROW_HEIGHT: f32 = 22.;
const MONO: &str = "Consolas";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Area {
    Unstaged,
    Staged,
}

pub(super) struct Changes {
    unstaged: Vec<Entry>,
    staged: Vec<Entry>,
    /// 选中的文件；同一个文件可能同时在两栏
    selected: Option<(Area, String)>,
    /// 选中文件在它那一栏的位置；文件从这一栏消失后选同一位置的下一个
    selected_ix: usize,
    /// 多选时选中的全部文件（都在 `selected` 那一栏，也包括它）；只选了一个时是空的
    multi: BTreeSet<String>,
    /// Shift 连选的起点
    anchor: usize,
    /// 文件列表的键盘焦点
    focus: FocusHandle,
    /// 右边改动内容的键盘焦点：上下键在改动行之间走，空格暂存选中的行
    diff_focus: FocusHandle,
    unstaged_scroll: UniformListScrollHandle,
    staged_scroll: UniformListScrollHandle,
    /// 过滤框：只留路径里有这几个字的文件，两栏一起
    filter: Entity<InputState>,
    /// 树形里收起的目录：哪一栏加目录的完整路径（`u:src/repo`、`s:docs`）
    folded: HashSet<String>,
    diff: Option<StageDiff>,
    /// 并排着看时用组件库的改动视图，这是它的状态；内容和 `diff` 是同一份补丁
    split: Entity<DiffState>,
    /// 并排视图里选中的行
    split_selection: Option<DiffLineRange>,
    /// 刚在选行那一条上按下了鼠标：接下来组件库清选中的行那一下不算数
    hold_selection: bool,
    /// 显示整个文件，不只是改动附近的几行
    whole_file: bool,
    /// 忽略空白时改动上面写的那句话（少显示了多少行）
    ws_note: Option<SharedString>,
    /// 选中的是图片时的图片对比
    image: Option<Entity<ImageDiff>>,
    /// 显示的改动可能已经过时（刚做完操作、正在重读），这时不能按块、按行操作
    diff_stale: bool,
    diff_task: Option<Task<()>>,
    message: Entity<TextareaState>,
    amend: bool,
    /// 勾选「修改上次提交」时自动填进去的说明；取消勾选时如果没改过就清空
    amend_message: Option<String>,
    amend_task: Option<Task<()>>,
}

impl Changes {
    pub(super) fn new(
        window: &mut Window,
        cx: &mut Context<RepoView>,
    ) -> (Self, Vec<Subscription>) {
        let message = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(tr("提交说明（首行为标题），按 Ctrl+Enter 提交"))
        });
        let subscription = cx.subscribe_in(
            &message,
            window,
            |this: &mut RepoView, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter {
                    secondary: true, ..
                } = event
                {
                    this.commit(window, cx);
                }
            },
        );
        // 并排视图里在行号上拖动选行：把选中的范围记到 `diff` 上，暂存、丢弃都认它
        let split = cx.new(|cx| DiffState::new(Vec::new(), cx).with_mode(DiffMode::Split));
        let split_events = cx.subscribe(
            &split,
            |this: &mut RepoView, state, event: &DiffEvent, cx| {
                let range = match event {
                    DiffEvent::SelectionStarted(range) | DiffEvent::SelectionEnded(range) => {
                        Some(range.clone())
                    }
                    DiffEvent::SelectionChanged(range) => range.clone(),
                    _ => return,
                };
                // 在窗口里任何地方按下鼠标，组件库都会把选中的行清掉。按的是选行那一条上的按钮时
                // 不能清：按钮是跟着选中的行出来的，清了它就没了，等不到松手。把高亮放回去，照旧。
                if range.is_none() && std::mem::take(&mut this.changes.hold_selection) {
                    let keep = this.changes.split_selection.clone();
                    state.update(cx, |state, cx| state.set_selected_lines(keep, cx));
                    return;
                }
                this.changes.hold_selection = false;
                this.changes.split_selection = range.clone();
                if let Some(diff) = &mut this.changes.diff {
                    diff.select_lines(range.as_ref());
                    cx.notify();
                }
            },
        );
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(tr("过滤文件…")));
        let filter_events =
            cx.subscribe(&filter, |this: &mut RepoView, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    for area in [Area::Unstaged, Area::Staged] {
                        let scroll = this.changes.scroll(area);
                        scroll.scroll_to_item(0, ScrollStrategy::Top);
                    }
                    cx.notify();
                }
            });
        let changes = Self {
            unstaged: Vec::new(),
            staged: Vec::new(),
            selected: None,
            selected_ix: 0,
            multi: BTreeSet::new(),
            anchor: 0,
            focus: cx.focus_handle(),
            diff_focus: cx.focus_handle(),
            unstaged_scroll: UniformListScrollHandle::new(),
            staged_scroll: UniformListScrollHandle::new(),
            filter,
            folded: HashSet::new(),
            diff: None,
            split,
            split_selection: None,
            hold_selection: false,
            whole_file: false,
            ws_note: None,
            image: None,
            diff_stale: false,
            diff_task: None,
            message,
            amend: false,
            amend_message: None,
            amend_task: None,
        };
        (changes, vec![subscription, split_events, filter_events])
    }

    fn list(&self, area: Area) -> &Vec<Entry> {
        match area {
            Area::Unstaged => &self.unstaged,
            Area::Staged => &self.staged,
        }
    }

    /// 刚做完写操作：显示的改动先当作过时，等刷新完再允许按块、按行操作。
    pub(super) fn mark_stale(&mut self) {
        if self.diff.is_some() {
            self.diff_stale = true;
        }
    }

    /// 刷新失败时解除过时标记，免得按钮一直不能点。
    pub(super) fn settle(&mut self) {
        self.diff_stale = false;
    }

    /// 提交说明框里现在的内容（去掉首尾空白）。
    pub(super) fn message_text(&self, cx: &App) -> String {
        self.message.read(cx).value().trim().to_string()
    }

    pub(super) fn clear_message(&mut self, window: &mut Window, cx: &mut Context<RepoView>) {
        self.message.update(cx, |m, cx| m.set_value("", window, cx));
        self.amend = false;
        self.amend_message = None;
    }

    fn scroll(&self, area: Area) -> &UniformListScrollHandle {
        match area {
            Area::Unstaged => &self.unstaged_scroll,
            Area::Staged => &self.staged_scroll,
        }
    }

    /// 现在选中的文件：多选时是全部，否则是那一个。
    fn selected_entries(&self) -> Option<(Area, Vec<Entry>)> {
        let (area, path) = self.selected.as_ref()?;
        let entries: Vec<Entry> = self
            .list(*area)
            .iter()
            .filter(|e| {
                if self.multi.is_empty() {
                    e.path == *path
                } else {
                    self.multi.contains(&e.path)
                }
            })
            .cloned()
            .collect();
        (!entries.is_empty()).then_some((*area, entries))
    }

    /// 这一栏里多选了几个文件；没有多选时是 0。
    fn multi_count(&self, area: Area) -> usize {
        match &self.selected {
            Some((a, _)) if *a == area => self.multi.len(),
            _ => 0,
        }
    }
}

/// 一串路径接成 `--pathspec-from-file=- --pathspec-file-nul` 要的标准输入。
fn nul_paths<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<u8> {
    let mut bytes = Vec::new();
    for path in paths {
        bytes.extend_from_slice(path.as_bytes());
        bytes.push(0);
    }
    bytes
}

/// 确认框里列文件：最多列 8 个，多的写「等 N 个」。
fn name_list(entries: &[Entry]) -> String {
    let mut names: Vec<&str> = entries.iter().take(8).map(|e| e.path.as_str()).collect();
    let more = entries.len().saturating_sub(names.len());
    let tail = tf!("……共 {entries} 个文件", entries = entries.len());
    if more > 0 {
        names.push(&tail);
    }
    names.join("\n")
}

/// 这个界面用到的颜色，每帧取一次。
#[derive(Clone, Copy)]
struct Colors {
    border: Hsla,
    muted: Hsla,
    selected: Hsla,
    hover: Hsla,
    added: Hsla,
    removed: Hsla,
    picked: Hsla,
    /// 行内差异：一行里具体改了的那几个词的底色
    added_mark: Hsla,
    removed_mark: Hsla,
    hunk: Hsla,
    success: Hsla,
    warning: Hsla,
    danger: Hsla,
    info: Hsla,
}

fn colors(cx: &App) -> Colors {
    let t = cx.theme();
    Colors {
        border: t.border,
        muted: t.muted_foreground,
        selected: t.list_active,
        hover: t.list_hover,
        added: hsla(135. / 360., 0.55, 0.45, 0.16),
        removed: hsla(0., 0.7, 0.55, 0.15),
        picked: hsla(212. / 360., 0.9, 0.55, 0.32),
        added_mark: hsla(135. / 360., 0.6, 0.45, 0.34),
        removed_mark: hsla(0., 0.75, 0.55, 0.32),
        hunk: t.muted,
        success: t.success,
        warning: t.warning,
        danger: t.danger,
        info: t.info,
    }
}

fn kind_color(kind: Kind, c: Colors) -> Hsla {
    match kind {
        Kind::Added | Kind::Untracked => c.success,
        Kind::Modified | Kind::TypeChanged => c.warning,
        Kind::Deleted | Kind::Conflicted => c.danger,
        Kind::Renamed | Kind::Copied => c.info,
    }
}

/// 收起的目录在 [`Changes::folded`] 里的键。
/// 选中的文件要是图片：旧的、新的两张各从哪取。不是图片（或者有冲突、是子模块）时没有。
fn image_sources(area: Area, entry: &Entry) -> Option<(Option<Source>, Option<Source>)> {
    if entry.submodule || !image_diff::is_image(&entry.path) {
        return None;
    }
    let path = entry.path.as_str();
    let at = |blob: Blob, path: &str| Some(Source::new(blob, path));
    Some(match area {
        // 工作区相对暂存区
        Area::Unstaged => match entry.unstaged? {
            Kind::Conflicted => return None,
            Kind::Untracked | Kind::Added => (None, at(Blob::Worktree, path)),
            Kind::Deleted => (at(Blob::Index, path), None),
            _ => (at(Blob::Index, path), at(Blob::Worktree, path)),
        },
        // 暂存区相对 HEAD；改了名的旧图在原来的路径上
        Area::Staged => {
            let orig = entry.orig_path.as_deref().unwrap_or(path);
            match entry.staged? {
                Kind::Added => (None, at(Blob::Index, path)),
                Kind::Deleted => (at(Blob::Rev("HEAD".into()), path), None),
                _ => (at(Blob::Rev("HEAD".into()), orig), at(Blob::Index, path)),
            }
        }
    })
}

fn fold_key(area: Area, path: &str) -> String {
    match area {
        Area::Unstaged => format!("u:{path}"),
        Area::Staged => format!("s:{path}"),
    }
}

impl RepoView {
    /// 本地修改里是不是不列未跟踪的文件（所有标签页共用的设置）。
    fn hide_untracked(&self, cx: &App) -> bool {
        self.app.read(cx).settings.hide_untracked
    }

    /// 藏起来了几个未跟踪的文件。
    fn hidden_untracked(&self, cx: &App) -> usize {
        match self.hide_untracked(cx) {
            true => {
                let untracked = |e: &&Entry| e.unstaged == Some(Kind::Untracked);
                self.changes.unstaged.iter().filter(untracked).count()
            }
            false => 0,
        }
    }

    /// 这一栏里列出来的文件（下标对着这一栏的清单）：过滤框里的字、隐藏未跟踪都算进去了。
    fn shown_entries(&self, area: Area, cx: &App) -> Vec<usize> {
        let needle = self.changes.filter.read(cx).value().trim().to_lowercase();
        let hide = area == Area::Unstaged && self.hide_untracked(cx);
        let list = self.changes.list(area).iter().enumerate();
        list.filter(|(_, e)| !(hide && e.unstaged == Some(Kind::Untracked)))
            .filter(|(_, e)| file_tree::matches(&e.path, &needle))
            .map(|(ix, _)| ix)
            .collect()
    }

    /// 这一栏现在显示的行：树形时有目录行，收起的目录不往下列。
    fn changes_rows(&self, area: Area, cx: &App) -> Vec<ListRow> {
        let list = self.changes.list(area);
        let shown = self.shown_entries(area, cx);
        let files: Vec<(&str, usize)> = shown
            .iter()
            .map(|ix| (list[*ix].path.as_str(), *ix))
            .collect();
        let tree = self.app.read(cx).settings.file_tree;
        // 过滤时目录全部展开
        let filtering = !self.changes.filter.read(cx).value().trim().is_empty();
        let folded = &self.changes.folded;
        file_tree::rows(&files, tree, &|path| {
            !filtering && folded.contains(&fold_key(area, path))
        })
    }

    /// 这一栏里看得见的文件，按显示的顺序（下标对着这一栏的清单）。
    fn file_order(&self, area: Area, cx: &App) -> Vec<usize> {
        let rows = self.changes_rows(area, cx).into_iter();
        rows.filter_map(|row| match row {
            ListRow::File { index, .. } => Some(index),
            ListRow::Dir { .. } => None,
        })
        .collect()
    }

    /// 这个文件显示在这一栏的第几行；过滤掉了、在收起的目录里就没有。
    fn row_of(&self, area: Area, ix: usize, cx: &App) -> Option<usize> {
        let rows = self.changes_rows(area, cx);
        rows.iter()
            .position(|row| matches!(row, ListRow::File { index, .. } if *index == ix))
    }

    fn scroll_to_file(&self, area: Area, ix: usize, cx: &App) {
        if let Some(row) = self.row_of(area, ix, cx) {
            self.changes
                .scroll(area)
                .scroll_to_item(row, ScrollStrategy::Nearest);
        }
    }

    pub(super) fn apply_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.changes.unstaged = status.unstaged().cloned().collect();
        self.changes.staged = status.staged().cloned().collect();
        self.status = Some(status);
        self.sync_file_selection(cx);
    }

    /// 状态变了以后：选中的文件还在就重读它的改动；不在了就选同一位置的下一个文件。
    fn sync_file_selection(&mut self, cx: &mut Context<Self>) {
        if let Some((area, _)) = &self.changes.selected {
            let area = *area;
            let still: BTreeSet<String> = self
                .changes
                .list(area)
                .iter()
                .filter(|e| self.changes.multi.contains(&e.path))
                .map(|e| e.path.clone())
                .collect();
            self.changes.multi = if still.len() > 1 {
                still
            } else {
                BTreeSet::new()
            };
        }
        let Some((area, path)) = self.changes.selected.clone() else {
            if self.mode == Mode::Changes {
                self.select_first_file(cx);
            } else {
                self.changes.diff = None;
                self.changes.diff_stale = false;
            }
            return;
        };
        if self.changes.list(area).iter().any(|e| e.path == path) {
            self.load_file_diff(cx);
            return;
        }
        // 同一位置的下一个：只在列出来的文件里挑
        let shown = self.shown_entries(area, cx);
        let at = self.changes.selected_ix;
        let next = shown.iter().copied().find(|ix| *ix >= at);
        if let Some(ix) = next.or(shown.last().copied()) {
            let next = self.changes.list(area)[ix].path.clone();
            self.changes.selected = Some((area, next));
            self.changes.selected_ix = ix;
            self.load_file_diff(cx);
            return;
        }
        let other = match area {
            Area::Unstaged => Area::Staged,
            Area::Staged => Area::Unstaged,
        };
        match self.changes.list(other).iter().position(|e| e.path == path) {
            Some(ix) => {
                self.changes.selected = Some((other, path));
                self.changes.selected_ix = ix;
                self.load_file_diff(cx);
            }
            None => {
                self.changes.selected = None;
                self.changes.diff = None;
                self.changes.diff_stale = false;
            }
        }
    }

    pub(super) fn changes_selection_empty(&self) -> bool {
        self.changes.selected.is_none()
    }

    pub(super) fn focus_file_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.changes.focus.focus(window, cx);
    }

    pub(super) fn changes_selected_entries(&self) -> Option<(Area, Vec<Entry>)> {
        self.changes.selected_entries()
    }

    /// 切到本地修改时还没选文件：选显示出来的第一个。
    pub(super) fn select_first_file(&mut self, cx: &mut Context<Self>) {
        let first = [Area::Unstaged, Area::Staged].into_iter().find_map(|area| {
            let ix = self.file_order(area, cx).first().copied()?;
            Some((area, self.changes.list(area)[ix].path.clone(), ix))
        });
        match first {
            Some((area, path, ix)) => self.select_file(area, path, ix, cx),
            None => {
                self.changes.selected = None;
                self.changes.diff = None;
                self.changes.diff_stale = false;
            }
        }
    }

    /// 选中本地修改里的这个文件（从文件页的「改动」过来）：先找未暂存的，再找已暂存的。
    pub(super) fn select_change(&mut self, path: &str, cx: &mut Context<Self>) {
        let at = |area: Area| {
            let list = self.changes.list(area);
            Some((area, list.iter().position(|entry| entry.path == path)?))
        };
        if let Some((area, ix)) = at(Area::Unstaged).or_else(|| at(Area::Staged)) {
            self.scroll_to_file(area, ix, cx);
            self.select_file(area, path.to_string(), ix, cx);
        }
    }

    fn select_file(&mut self, area: Area, path: String, ix: usize, cx: &mut Context<Self>) {
        let same = self.changes.selected.as_ref() == Some(&(area, path.clone()));
        self.changes.selected = Some((area, path));
        self.changes.selected_ix = ix;
        if !same {
            self.changes.diff = None;
        }
        if !same || self.changes.diff.is_none() {
            self.load_file_diff(cx);
        }
        cx.notify();
    }

    /// 点了一个文件：普通点击只选它；Ctrl 加点击把它加进多选或从多选里去掉；Shift 加点击连选一段。
    fn click_file(
        &mut self,
        area: Area,
        path: String,
        ix: usize,
        modifiers: Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.changes.focus.focus(window, cx);
        let same_area = matches!(&self.changes.selected, Some((a, _)) if *a == area);
        if modifiers.shift && same_area {
            self.changes.multi = self.paths_between(area, self.changes.anchor, ix, cx);
            self.select_file(area, path, ix, cx);
        } else if (modifiers.control || modifiers.platform) && same_area {
            if self.changes.multi.is_empty()
                && let Some((_, current)) = &self.changes.selected
            {
                self.changes.multi.insert(current.clone());
            }
            if self.changes.multi.contains(&path) && self.changes.multi.len() > 1 {
                self.changes.multi.remove(&path);
                // 去掉的正好是右边显示的那个：换成多选里的第一个
                if matches!(&self.changes.selected, Some((_, p)) if *p == path) {
                    let next = self
                        .changes
                        .multi
                        .iter()
                        .next()
                        .cloned()
                        .unwrap_or_default();
                    let at = self
                        .changes
                        .list(area)
                        .iter()
                        .position(|e| e.path == next)
                        .unwrap_or(0);
                    self.select_file(area, next, at, cx);
                }
            } else {
                self.changes.multi.insert(path.clone());
                self.select_file(area, path, ix, cx);
            }
            self.changes.anchor = ix;
        } else {
            self.changes.multi.clear();
            self.changes.anchor = ix;
            self.select_file(area, path, ix, cx);
        }
        if self.changes.multi.len() <= 1 {
            self.changes.multi.clear();
        }
        cx.notify();
    }

    /// 显示的顺序里从 `from` 到 `to`（两头都算）的文件；有一头没显示出来就只有 `to`。
    fn paths_between(&self, area: Area, from: usize, to: usize, cx: &App) -> BTreeSet<String> {
        let order = self.file_order(area, cx);
        let at = |ix: usize| order.iter().position(|shown| *shown == ix);
        let list = self.changes.list(area);
        let picked: Vec<usize> = match (at(from), at(to)) {
            (Some(a), Some(b)) => order[a.min(b)..=a.max(b)].to_vec(),
            _ => vec![to],
        };
        let paths = picked.into_iter().filter_map(|ix| list.get(ix));
        paths.map(|e| e.path.clone()).collect()
    }

    /// 文件列表上的按键。
    fn changes_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.changes.focus.is_focused(window) {
            return;
        }
        let (key, m) = (event.keystroke.key.as_str(), event.keystroke.modifiers);
        match key {
            "up" => self.move_file_selection(false, m.shift, cx),
            "down" => self.move_file_selection(true, m.shift, cx),
            "enter" if m.control || m.platform => self.commit(window, cx),
            "space" | "enter" => self.toggle_selected_files(window, cx),
            "delete" => self.discard_selected_files(window, cx),
            // 右键：到右边的改动里按行操作
            "right" => {
                self.changes.diff_focus.focus(window, cx);
                self.step_diff_row(true, false, cx);
            }
            "a" if m.control || m.platform => {
                if let Some((area, _)) = &self.changes.selected {
                    // 全选这一栏里列出来的（过滤掉的、藏起来的不算）
                    let list = self.changes.list(*area);
                    let shown = self.shown_entries(*area, cx);
                    let all: BTreeSet<String> =
                        shown.iter().map(|ix| list[*ix].path.clone()).collect();
                    self.changes.multi = if all.len() > 1 { all } else { BTreeSet::new() };
                    cx.notify();
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// 右边改动内容上的按键：上下键在改动行之间走（Shift 连选），空格、回车暂存或取消暂存选中的行，
    /// Delete 丢弃选中的行（会先问），左键回到文件列表。
    fn diff_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.changes.diff_focus.contains_focused(window, cx) {
            return;
        }
        // 搜索条也在这一块里：在它的输入框里打空格、按回车、Delete 是在输入，不是暂存、丢弃选中的行
        if self.diff_search.at(Where::Changes).is_focused(window, cx) {
            return;
        }
        let (key, m) = (event.keystroke.key.as_str(), event.keystroke.modifiers);
        // 并排视图自己管上下键（滚动、连选），这里不抢
        let split = self.split_mode(cx);
        match key {
            "up" | "down" if split => return,
            "up" => self.step_diff_row(false, m.shift, cx),
            "down" => self.step_diff_row(true, m.shift, cx),
            "enter" if m.control || m.platform => self.commit(window, cx),
            "space" | "enter" => self.apply_selection(None, window, cx),
            "delete" => {
                if self
                    .changes
                    .diff
                    .as_ref()
                    .is_some_and(|d| d.area == Area::Unstaged && d.selected_count() > 0)
                {
                    self.discard_selection(None, window, cx);
                }
            }
            "left" => self.changes.focus.focus(window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn step_diff_row(&mut self, down: bool, extend: bool, cx: &mut Context<Self>) {
        let Some(diff) = &mut self.changes.diff else {
            return;
        };
        if let Some(row) = diff.step(down, extend) {
            diff.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    /// 上下键换文件：到一栏的头尾就跨到另一栏。`extend`（按着 Shift）时在这一栏里连选。
    fn move_file_selection(&mut self, down: bool, extend: bool, cx: &mut Context<Self>) {
        let Some((area, _)) = self.changes.selected.clone() else {
            self.select_first_file(cx);
            return;
        };
        // 按显示的顺序走；选中的文件没显示出来（过滤掉了、目录收起了）就从这一栏的头上开始
        let order = self.file_order(area, cx);
        let at = order.iter().position(|ix| *ix == self.changes.selected_ix);
        let step = match (at, down) {
            (Some(at), true) => order.get(at + 1).copied(),
            (Some(at), false) => at.checked_sub(1).map(|at| order[at]),
            (None, _) => order.first().copied(),
        };
        let other = match area {
            Area::Unstaged => Area::Staged,
            Area::Staged => Area::Unstaged,
        };
        let target = match step {
            Some(ix) => Some((area, ix)),
            None if extend => None,
            // 到头了：往下从未暂存跨到已暂存的第一个，往上从已暂存跨到未暂存的最后一个
            None if down == (area == Area::Unstaged) => {
                let order = self.file_order(other, cx);
                let ix = match down {
                    true => order.first().copied(),
                    false => order.last().copied(),
                };
                ix.map(|ix| (other, ix))
            }
            None => None,
        };
        let Some((area, ix)) = target else {
            return;
        };
        let path = self.changes.list(area)[ix].path.clone();
        if extend {
            self.changes.multi = self.paths_between(area, self.changes.anchor, ix, cx);
            if self.changes.multi.len() <= 1 {
                self.changes.multi.clear();
            }
        } else {
            self.changes.multi.clear();
            self.changes.anchor = ix;
        }
        self.scroll_to_file(area, ix, cx);
        self.select_file(area, path, ix, cx);
    }

    /// 空格、回车：暂存或取消暂存选中的文件。
    fn toggle_selected_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((area, entries)) = self.changes.selected_entries() else {
            return;
        };
        match area {
            Area::Unstaged => self.stage_files(entries, window, cx),
            Area::Staged => self.unstage_files(entries, window, cx),
        }
    }

    fn discard_selected_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((Area::Unstaged, entries)) = self.changes.selected_entries() {
            self.discard_files(entries, window, cx);
        }
    }

    /// 暂存几个文件。有冲突的不在里面：暂存它等于标记为已解决，要一个一个来。
    pub(super) fn stage_files(
        &mut self,
        entries: Vec<Entry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let [entry] = entries.as_slice() {
            self.stage_file(entry, window, cx);
            return;
        }
        let (conflicted, rest): (Vec<Entry>, Vec<Entry>) = entries
            .into_iter()
            .partition(|e| e.unstaged == Some(Kind::Conflicted));
        let skipped = conflicted.len();
        let note = move |window: &mut Window, cx: &mut App| {
            if skipped > 0 {
                let text = tf!(
                    "{skipped} 个文件存在冲突，未暂存。解决后请右键选择「标记为已解决」。",
                    skipped
                );
                window.push_notification(Notification::warning(text), cx);
            }
        };
        if rest.is_empty() {
            note(window, cx);
            return;
        }
        let args = vec![
            "add".into(),
            "-A".into(),
            "--pathspec-from-file=-".into(),
            "--pathspec-file-nul".into(),
        ];
        let stdin = nul_paths(rest.iter().map(|e| e.path.as_str()));
        self.run_op(
            tr("暂存"),
            args,
            Some(stdin),
            window,
            cx,
            move |_, _, window, cx| note(window, cx),
        );
    }

    pub(super) fn unstage_files(
        &mut self,
        entries: Vec<Entry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let [entry] = entries.as_slice() {
            self.unstage_file(entry, window, cx);
            return;
        }
        let args = vec![
            "reset".into(),
            "-q".into(),
            "--pathspec-from-file=-".into(),
            "--pathspec-file-nul".into(),
        ];
        // 改名的文件：原路径也要恢复
        let paths = entries
            .iter()
            .flat_map(|e| std::iter::once(e.path.as_str()).chain(e.orig_path.as_deref()));
        let stdin = nul_paths(paths);
        self.run_op(
            tr("取消暂存"),
            args,
            Some(stdin),
            window,
            cx,
            |_, _, _, _| {},
        );
    }

    /// 丢弃几个文件在工作区里的改动（未跟踪的是删除）。有冲突的丢弃不了，跳过。
    pub(super) fn discard_files(
        &mut self,
        entries: Vec<Entry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entries: Vec<Entry> = entries
            .into_iter()
            .filter(|e| e.unstaged != Some(Kind::Conflicted))
            .collect();
        if let [entry] = entries.as_slice() {
            self.discard_file(entry, window, cx);
            return;
        }
        if entries.is_empty() {
            return;
        }
        let (untracked, tracked): (Vec<Entry>, Vec<Entry>) = entries
            .iter()
            .cloned()
            .partition(|e| e.unstaged == Some(Kind::Untracked));
        let description = tf!(
            "{entries} 个文件未暂存的改动将被丢弃，其中 {untracked} 个未跟踪的文件将被删除。此操作无法撤销。\n\n{entries2}",
            entries = entries.len(),
            untracked = untracked.len(),
            entries2 = name_list(&entries)
        );
        self.confirm(
            tf!("丢弃 {entries} 个文件的改动？", entries = entries.len()),
            description,
            tr("丢弃"),
            window,
            cx,
            move |this, window, cx| {
                let mut steps = Vec::new();
                if !tracked.is_empty() {
                    steps.push(Step {
                        args: vec![
                            "checkout".into(),
                            "-q".into(),
                            "--pathspec-from-file=-".into(),
                            "--pathspec-file-nul".into(),
                        ],
                        stdin: Some(nul_paths(tracked.iter().map(|e| e.path.as_str()))),
                        net: false,
                    });
                }
                if !untracked.is_empty() {
                    // -d：未跟踪的目录在状态里是一项，要连目录一起删
                    let mut clean: Vec<String> =
                        ["clean", "-f", "-d", "-q", "--"].map(String::from).to_vec();
                    clean.extend(untracked.iter().map(|e| e.path.clone()));
                    steps.push(Step::local(clean));
                }
                let count = tracked.len() + untracked.len();
                this.run_seq(tr("丢弃"), steps, window, cx, move |_, _, window, cx| {
                    let text = tf!("已丢弃 {count} 个文件的改动", count);
                    window.push_notification(Notification::success(text), cx);
                });
            },
        );
    }

    pub(super) fn load_file_diff(&mut self, cx: &mut Context<Self>) {
        let Some((area, path)) = self.changes.selected.clone() else {
            return;
        };
        let Some(entry) = self
            .changes
            .list(area)
            .iter()
            .find(|e| e.path == path)
            .cloned()
        else {
            return;
        };
        let Some(channel) = self.channel.clone() else {
            return;
        };
        // 图片：另外建一个图片对比（同一个文件重读时只刷新，不重建）
        match image_sources(area, &entry) {
            None => self.changes.image = None,
            Some((old, new)) => match &self.changes.image {
                Some(view) if view.read(cx).shows(&old, &new) => {
                    view.update(cx, |view, cx| view.reload(cx))
                }
                _ => {
                    let (app, repo) = (self.app.clone(), self.entry.path.clone());
                    let channel = channel.clone();
                    self.changes.image =
                        Some(cx.new(|cx| ImageDiff::new(app, channel, repo, old, new, cx)));
                }
            },
        }
        let (mut args, ok): (Vec<String>, &'static [i32]) = match area {
            Area::Unstaged if entry.unstaged == Some(Kind::Untracked) => {
                (patch::untracked_args(&path), &[0, 1])
            }
            Area::Unstaged => (patch::unstaged_args(&path), &[0]),
            Area::Staged => (patch::staged_args(&path, entry.orig_path.as_deref()), &[0]),
        };
        if self.changes.whole_file {
            patch::set_context(&mut args, patch::WHOLE_FILE);
        }
        let plain = self.ws_plain(&channel, &mut args);
        // 同一个文件重读（比如暂存了几行）时沿用滚动位置
        let scroll = self
            .changes
            .diff
            .as_ref()
            .filter(|d| d.area == area && d.entry.path == path)
            .map(|d| d.scroll.clone())
            .unwrap_or_default();
        let request = channel.git(&self.entry.path, args);
        self.changes.diff_stale = true;
        self.changes.diff_task = Some(cx.spawn(async move |this, cx| {
            let result = request.await;
            let plain = diff_tools::plain_out(plain).await;
            this.update(cx, |this, cx| {
                // 选中的文件已经换了：这个结果作废
                if this.changes.selected.as_ref() != Some(&(area, entry.path.clone())) {
                    return;
                }
                this.changes.diff_stale = false;
                this.changes.ws_note = match (&plain, &result) {
                    (Some(plain), Ok(out)) => Some(diff_tools::ws_note(plain, &out.stdout)),
                    _ => None,
                };
                // 内容没变（比如别的文件触发的刷新）：沿用原来的，正在选的行不清掉
                if let (Ok(out), Some(old)) = (&result, &mut this.changes.diff)
                    && out.code.is_some_and(|c| ok.contains(&c))
                    && old.same_source(area, &entry.path, &out.stdout)
                {
                    old.entry = entry;
                    cx.notify();
                    return;
                }
                // 下面换了一份改动：搜索条开着的话按新的再找一遍（放在最后）
                let research = cx.entity().downgrade();
                cx.defer(move |cx| {
                    research
                        .update(cx, |this, cx| {
                            this.update_diff_search(Where::Changes, false, cx)
                        })
                        .ok();
                });
                this.changes.diff = Some(match result {
                    Ok(out) if out.code.is_some_and(|c| ok.contains(&c)) => {
                        StageDiff::new(area, entry, &out.stdout, scroll)
                    }
                    Ok(out) => StageDiff::failed(area, entry, out.stderr.into(), scroll),
                    Err(err) => StageDiff::failed(area, entry, format!("{err:#}").into(), scroll),
                });
                this.sync_split_diff(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// 右边是不是显示着一个文件的改动。
    pub(super) fn changes_showing_diff(&self) -> bool {
        self.changes.diff.is_some()
    }

    pub(super) fn changes_diff(&self) -> Option<&StageDiff> {
        self.changes.diff.as_ref()
    }

    /// 现在是不是用组件库的并排视图在显示。
    pub(super) fn changes_split_showing(&self, cx: &App) -> bool {
        self.split_mode(cx)
            && self
                .changes
                .diff
                .as_ref()
                .is_some_and(|diff| diff.editable())
    }

    pub(super) fn changes_split_state(&self) -> Entity<DiffState> {
        self.changes.split.clone()
    }

    pub(super) fn focus_changes_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.changes.diff_focus.focus(window, cx);
    }

    /// 改动是不是并排着看（所有标签页共用的设置）。
    fn split_mode(&self, cx: &App) -> bool {
        self.app.read(cx).settings.diff_split
    }

    /// 并排视图显示的内容跟上 `diff`：同一份补丁交给组件库再解析一遍。不能按行操作的
    /// （二进制、有冲突的）不走并排视图。
    fn sync_split_diff(&mut self, cx: &mut Context<Self>) {
        let files = match &self.changes.diff {
            Some(diff) if diff.editable() && self.split_mode(cx) => {
                DiffFile::parse(&crate::text::decode(diff.source())).unwrap_or_default()
            }
            _ => Vec::new(),
        };
        let whole = self.changes.whole_file;
        self.changes.split.update(cx, |state, cx| {
            // 整个文件：补丁里带的上下文全都显示；否则只留改动附近三行，别的折起来
            state.set_context_lines(if whole { None } else { Some(3) }, cx);
            state.set_files(files, cx);
            state.set_selected_lines(None, cx);
        });
        self.changes.split_selection = None;
        self.changes.hold_selection = false;
    }

    /// 切换「显示整个文件」，重读选中文件的改动。
    fn toggle_whole_file(&mut self, cx: &mut Context<Self>) {
        self.changes.whole_file = !self.changes.whole_file;
        self.load_file_diff(cx);
        cx.notify();
    }

    /// 切换并排和上下排（所有标签页共用的设置；提交详情里的改动视图在重画时跟上）。
    pub(super) fn toggle_split(&mut self, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            app.update_settings(cx, |s| s.diff_split = !s.diff_split)
        });
        if let Some(diff) = &mut self.changes.diff {
            diff.select_lines(None);
        }
        self.sync_split_diff(cx);
        cx.notify();
    }

    /// 双击文件：暂存或取消暂存。
    fn toggle_file(&mut self, area: Area, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self
            .changes
            .list(area)
            .iter()
            .find(|e| e.path == path)
            .cloned()
        else {
            return;
        };
        match area {
            Area::Unstaged => self.stage_file(&entry, window, cx),
            Area::Staged => self.unstage_file(&entry, window, cx),
        }
    }

    pub(super) fn stage_file(
        &mut self,
        entry: &Entry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let args = vec!["add".into(), "--".into(), entry.path.clone()];
        self.run_op(tr("暂存"), args, None, window, cx, |_, _, _, _| {});
    }

    pub(super) fn unstage_file(
        &mut self,
        entry: &Entry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut args: Vec<String> =
            vec!["reset".into(), "-q".into(), "--".into(), entry.path.clone()];
        // 改名的文件：原路径也要恢复
        args.extend(entry.orig_path.clone());
        self.run_op(tr("取消暂存"), args, None, window, cx, |_, _, _, _| {});
    }

    pub(super) fn discard_file(
        &mut self,
        entry: &Entry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let untracked = entry.unstaged == Some(Kind::Untracked);
        let base: &[&str] = if untracked {
            // -d：未跟踪的目录在状态里是一项，要连目录一起删
            &["clean", "-f", "-d", "-q", "--"]
        } else {
            &["checkout", "-q", "--"]
        };
        let mut args: Vec<String> = base.iter().map(|s| s.to_string()).collect();
        args.push(entry.path.clone());
        let description = if untracked {
            tf!(
                "未跟踪的文件 {path} 将被删除，此操作无法撤销。",
                path = entry.path
            )
        } else {
            tf!(
                "{path} 在工作区中未暂存的改动将被丢弃，此操作无法撤销。",
                path = entry.path
            )
        };
        self.confirm(
            tr("丢弃改动？"),
            description,
            tr("丢弃"),
            window,
            cx,
            move |this, window, cx| {
                this.run_op(
                    tr("丢弃"),
                    args.clone(),
                    None,
                    window,
                    cx,
                    |_, _, window, cx| {
                        window.push_notification(Notification::success(tr("已丢弃")), cx);
                    },
                );
            },
        );
    }

    fn stage_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 有冲突的文件不跟着暂存：暂存它等于说「已经解决了」，带着冲突标记提交进去就坏了
        let conflicts = self
            .changes
            .unstaged
            .iter()
            .any(|e| e.unstaged == Some(Kind::Conflicted));
        // 藏起来的未跟踪文件也不跟着暂存：看不见的东西不该被加进去
        let hide = self.hidden_untracked(cx) > 0;
        if conflicts {
            let mut entries = self.changes.unstaged.clone();
            entries.retain(|e| !(hide && e.unstaged == Some(Kind::Untracked)));
            self.stage_files(entries, window, cx);
            return;
        }
        // `-u`：只暂存已跟踪的文件
        let args = vec!["add".into(), if hide { "-u" } else { "-A" }.into()];
        self.run_op(tr("暂存"), args, None, window, cx, |_, _, _, _| {});
    }

    fn unstage_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let args = vec!["reset".into(), "-q".into()];
        self.run_op(tr("取消暂存"), args, None, window, cx, |_, _, _, _| {});
    }

    /// 暂存（未暂存一栏）或取消暂存（已暂存一栏）选中的行；给了 `hunk` 就是整块。
    fn apply_selection(
        &mut self,
        hunk: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 忽略空白时显示的补丁和文件对不上，不能拿去暂存
        if self.changes.diff_stale || self.ignore_ws {
            return;
        }
        let Some(diff) = &self.changes.diff else {
            return;
        };
        let reverse = diff.area == Area::Staged;
        let Some(bytes) = diff.patch(reverse, hunk) else {
            return;
        };
        let label = if reverse {
            tr("取消暂存")
        } else {
            tr("暂存")
        };
        self.run_op(
            label,
            patch::apply_cached_args(reverse),
            Some(bytes),
            window,
            cx,
            |_, _, _, _| {},
        );
    }

    /// 丢弃工作区里选中的行；给了 `hunk` 就是整块。先确认。
    fn discard_selection(
        &mut self,
        hunk: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.changes.diff_stale || self.ignore_ws {
            return;
        }
        let Some(diff) = &self.changes.diff else {
            return;
        };
        if diff.area != Area::Unstaged {
            return;
        }
        let Some(bytes) = diff.patch(true, hunk) else {
            return;
        };
        let count = match hunk {
            Some(h) => diff.hunk_changed_count(h),
            None => diff.selected_count(),
        };
        let description = tf!(
            "{path} 中的 {count} 行改动将从工作区丢弃，此操作无法撤销。",
            path = diff.entry.path,
            count
        );
        self.confirm(
            tr("丢弃改动？"),
            description,
            tr("丢弃"),
            window,
            cx,
            move |this, window, cx| {
                this.run_op(
                    tr("丢弃"),
                    patch::discard_args(),
                    Some(bytes.clone()),
                    window,
                    cx,
                    |_, _, window, cx| {
                        window.push_notification(Notification::success(tr("已丢弃")), cx);
                    },
                );
            },
        );
    }

    fn press_diff_row(
        &mut self,
        row: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.changes.diff_focus.focus(window, cx);
        let Some(diff) = &mut self.changes.diff else {
            return;
        };
        diff.press(
            row,
            event.modifiers.shift,
            event.modifiers.control,
            event.click_count,
        );
        cx.notify();
    }

    fn drag_diff_row(&mut self, row: usize, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(diff) = &mut self.changes.diff else {
            return;
        };
        if !diff.dragging {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            diff.dragging = false;
            return;
        }
        if diff.drag_to(row) {
            cx.notify();
        }
    }

    fn end_diff_drag(&mut self) {
        if let Some(diff) = &mut self.changes.diff {
            diff.dragging = false;
        }
    }

    /// 勾选「修改上次提交」：说明是空的就填上上次提交的说明。
    fn set_amend(&mut self, checked: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.changes.amend = checked;
        let current = self.changes.message.read(cx).value().to_string();
        if checked {
            if current.trim().is_empty()
                && let Some(channel) = self.channel.clone()
            {
                let args = vec!["log".into(), "-1".into(), "--format=%B".into()];
                let request = channel.git(&self.entry.path, args);
                self.changes.amend_task = Some(cx.spawn_in(window, async move |this, cx| {
                    let Ok(out) = request.await.and_then(|out| out.ok()) else {
                        return;
                    };
                    let text = String::from_utf8_lossy(&out).trim_end().to_string();
                    this.update_in(cx, |this, window, cx| {
                        let empty = this.changes.message.read(cx).value().trim().is_empty();
                        if this.changes.amend && empty {
                            this.changes
                                .message
                                .update(cx, |m, cx| m.set_value(text.clone(), window, cx));
                            this.changes.amend_message = Some(text);
                        }
                    })
                    .ok();
                }));
            }
        } else if self.changes.amend_message.as_deref() == Some(current.as_str()) {
            self.changes
                .message
                .update(cx, |m, cx| m.set_value("", window, cx));
            self.changes.amend_message = None;
        }
        cx.notify();
    }

    pub(super) fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_then(false, window, cx);
    }

    /// 提交；`push` 为真时提交成功后接着推送。
    fn commit_then(&mut self, push: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.op.is_some() {
            return;
        }
        // 提交期间用户可能接着打字：记下交出去的原文，成功后内容没变才清空
        let sent = self.changes.message.read(cx).value().to_string();
        let message = sent.trim().to_string();
        if message.is_empty() {
            window.push_notification(Notification::warning(tr("请先填写提交说明")), cx);
            return;
        }
        let amend = self.changes.amend;
        if !amend && self.changes.staged.is_empty() {
            window.push_notification(Notification::warning(tr("没有已暂存的改动")), cx);
            return;
        }
        let mut args: Vec<String> = vec!["commit".into(), "-F".into(), "-".into()];
        if amend {
            args.push("--amend".into());
        }
        let label = if amend {
            tr("修改提交")
        } else {
            tr("提交")
        };
        let used = message.clone();
        self.run_op(
            label,
            args,
            Some(message.into_bytes()),
            window,
            cx,
            move |this, _, window, cx| {
                // 记下这条说明，下次可以从「最近用过的」里再拿
                let entry = this.entry.clone();
                this.app.update(cx, |app, cx| {
                    app.edit_memo(&entry, cx, |memo| memo.remember_message(&used))
                });
                let unchanged = this.changes.message.read(cx).value().as_ref() == sent.as_str();
                if unchanged {
                    this.changes.clear_message(window, cx);
                } else {
                    // 说明在提交期间又改过：留着，只把「修改上次提交」取消掉
                    this.changes.amend = false;
                    this.changes.amend_message = None;
                    let text = tr("已提交。提交期间说明已被修改，因此未清空。");
                    window.push_notification(Notification::info(text), cx);
                }
                // 到这里提交这个操作已经结束了，可以接着做下一个
                if push {
                    this.push(window, cx);
                }
            },
        );
    }

    /// 最近用过的提交说明（新的在前）。
    fn recent_messages(&self, cx: &App) -> Vec<String> {
        self.app
            .read(cx)
            .settings
            .memo(&self.entry)
            .map(|memo| memo.recent_messages.clone())
            .unwrap_or_default()
    }

    /// 把一条用过的说明放进说明框。
    fn use_message(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.changes
            .message
            .update(cx, |m, cx| m.set_value(text, window, cx));
        self.changes.amend_message = None;
        cx.notify();
    }

    /// 提交说明的草稿，关标签页时由工作区取走，再打开这个仓库时放回来。
    pub fn draft(&self, cx: &App) -> String {
        self.changes.message.read(cx).value().to_string()
    }

    pub fn set_draft(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.changes
            .message
            .update(cx, |m, cx| m.set_value(text, window, cx));
    }

    /// 换了界面语言：提交说明框里的提示文字要重新设一遍。
    pub(super) fn retranslate_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.changes.message.update(cx, |state, cx| {
            state.set_placeholder(tr("提交说明（首行为标题），按 Ctrl+Enter 提交"), window, cx)
        });
        self.changes.filter.update(cx, |state, cx| {
            state.set_placeholder(tr("过滤文件…"), window, cx)
        });
    }

    pub(super) fn render_changes(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let c = colors(cx);
        let list_width = self.sizes(cx).changes_list;
        h_flex()
            .size_full()
            .track_focus(&self.changes.focus)
            .on_key_down(cx.listener(Self::changes_key))
            .child(
                v_flex()
                    .w(px(list_width))
                    .flex_none()
                    .h_full()
                    .relative()
                    .child(self.render_list_bar(c, cx))
                    .child(self.render_file_section(Area::Unstaged, c, cx))
                    .child(self.render_file_section(Area::Staged, c, cx))
                    .child(self.render_commit_box(c, cx))
                    // 右边那条线就是分隔条：文件名太长看不全时把列表拉宽
                    .child(self.split_handle(Split::ChangesList, cx)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.render_stage_diff(c, cx)),
            )
            .into_any_element()
    }

    /// 文件列表上面一行：过滤框、树形和平铺的切换、筛选（隐藏未跟踪的文件）。
    fn render_list_bar(&mut self, c: Colors, cx: &mut Context<Self>) -> AnyElement {
        let tree = self.app.read(cx).settings.file_tree;
        let hide = self.hide_untracked(cx);
        let (app, view) = (self.app.clone(), cx.entity().downgrade());
        let (menu_app, menu_view) = (app.clone(), view.clone());
        h_flex()
            .flex_none()
            .h(px(38.))
            .px_2()
            .gap(px(6.))
            .border_b_1()
            .border_color(c.border)
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.changes.filter)
                        .small()
                        .cleanable(true)
                        .prefix(ui::icon(IconName::Search).text_color(c.muted)),
                ),
            )
            .child(file_tree::mode_toggle(
                "changes-list-mode",
                tree,
                move |tree, _, cx| {
                    app.update(cx, |app, cx| {
                        app.update_settings(cx, |s| s.file_tree = tree)
                    });
                    view.update(cx, |_, cx| cx.notify()).ok();
                },
            ))
            // 不加悬停提示：提示也出现在按钮下面，会把弹出的菜单盖住
            .child(
                Button::new("changes-list-filter")
                    .xsmall()
                    .ghost()
                    .icon(ui::icon(IconName::Funnel))
                    .selected(hide)
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                        let (app, view) = (menu_app.clone(), menu_view.clone());
                        menu.item(
                            PopupMenuItem::new(tr("隐藏未跟踪的文件"))
                                .checked(hide)
                                .on_click(move |_, _, cx| {
                                    app.update(cx, |app, cx| {
                                        app.update_settings(cx, |s| s.hide_untracked = !hide)
                                    });
                                    view.update(cx, |this, cx| this.hide_untracked_changed(cx))
                                        .ok();
                                }),
                        )
                    }),
            )
            .into_any_element()
    }

    /// 切换了「隐藏未跟踪的文件」：选中的文件要是被藏起来了，换一个。
    fn hide_untracked_changed(&mut self, cx: &mut Context<Self>) {
        self.changes.multi.clear();
        let gone = match &self.changes.selected {
            Some((area, _)) => !self
                .shown_entries(*area, cx)
                .contains(&self.changes.selected_ix),
            None => true,
        };
        if gone && self.hide_untracked(cx) {
            self.changes.selected = None;
            self.changes.diff = None;
            self.select_first_file(cx);
        }
        cx.notify();
    }

    fn render_file_section(&mut self, area: Area, c: Colors, cx: &mut Context<Self>) -> AnyElement {
        let total = self.changes.list(area).len();
        let count = self.shown_entries(area, cx).len();
        let hidden = match area {
            Area::Unstaged => self.hidden_untracked(cx),
            Area::Staged => 0,
        };
        let rows = Rc::new(self.changes_rows(area, cx));
        let picked = self.changes.multi_count(area);
        let busy = self.op.is_some();
        let (title, action, empty, id) = match area {
            Area::Unstaged => (
                tr("未暂存"),
                tr("全部暂存"),
                tr("工作区无改动"),
                "unstaged",
            ),
            Area::Staged => (
                tr("已暂存"),
                tr("全部取消暂存"),
                tr("暂无已暂存的改动"),
                "staged",
            ),
        };
        let scroll = match area {
            Area::Unstaged => self.changes.unstaged_scroll.clone(),
            Area::Staged => self.changes.staged_scroll.clone(),
        };
        let body = if count == 0 {
            // 有文件、只是都没列出来：过滤掉了，或者全是藏起来的未跟踪文件（下面另有一行说明）
            let text = match (total, total == hidden) {
                (0, _) => empty,
                (_, true) => "",
                (_, false) => tr("没有匹配的文件"),
            };
            div()
                .p_3()
                .text_sm()
                .text_color(c.muted)
                .child(text)
                .into_any_element()
        } else {
            let rows = rows.clone();
            uniform_list(
                id,
                rows.len(),
                cx.processor(move |this, range: Range<usize>, _window, cx| {
                    this.render_file_rows(area, &rows, range, c, cx)
                }),
            )
            .track_scroll(&scroll)
            .size_full()
            .into_any_element()
        };
        // 藏起来了未跟踪的文件：列表下面说一声，旁边可以直接显示回来
        let hidden_note = (hidden > 0).then(|| {
            h_flex()
                .flex_none()
                .h(px(26.))
                .px_2()
                .gap_2()
                .text_xs()
                .text_color(c.muted)
                .child(
                    ui::clipped(
                        "hidden-untracked",
                        tf!("已隐藏 {n} 个未跟踪的文件", n = hidden),
                    )
                    .flex_shrink(1.),
                )
                .child(
                    div()
                        .id("show-untracked")
                        .flex_none()
                        .text_color(c.info)
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .child(tr("显示"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.app.update(cx, |app, cx| {
                                app.update_settings(cx, |s| s.hide_untracked = false)
                            });
                            this.hide_untracked_changed(cx);
                        })),
                )
        });
        v_flex()
            .flex_1()
            .min_h_0()
            .when(area == Area::Staged, |d| {
                d.border_t_1().border_color(c.border)
            })
            .child(
                h_flex()
                    .h(px(30.))
                    .flex_none()
                    .px_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(c.border)
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(c.muted)
                            .child(title),
                    )
                    .child(crate::ui::count_pill(count, false, cx))
                    .child(div().flex_1())
                    .when(picked > 1, |bar| {
                        let label = match area {
                            Area::Unstaged => tf!("暂存选中的 {picked} 项", picked),
                            Area::Staged => tf!("取消暂存选中的 {picked} 项", picked),
                        };
                        bar.child(
                            Button::new(SharedString::from(format!("{id}-picked")))
                                .xsmall()
                                .label(label)
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_selected_files(window, cx)
                                })),
                        )
                        .when(area == Area::Unstaged, |bar| {
                            bar.child(
                                Button::new("unstaged-picked-discard")
                                    .xsmall()
                                    .ghost()
                                    .label(tr("丢弃…"))
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.discard_selected_files(window, cx)
                                    })),
                            )
                        })
                    })
                    .when(picked <= 1, |bar| {
                        bar.child(
                            Button::new(SharedString::from(format!("{id}-all")))
                                .xsmall()
                                .ghost()
                                .label(action)
                                .disabled(busy || count == 0)
                                .on_click(cx.listener(move |this, _, window, cx| match area {
                                    Area::Unstaged => this.stage_all(window, cx),
                                    Area::Staged => this.unstage_all(window, cx),
                                })),
                        )
                    }),
            )
            .child(div().flex_1().min_h_0().child(body))
            .children(hidden_note)
            .into_any_element()
    }

    fn render_file_rows(
        &mut self,
        area: Area,
        rows: &[ListRow],
        range: Range<usize>,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let list = self.changes.list(area);
        let selected = self.changes.selected.clone();
        let multi = self.changes.multi_count(area) > 1;
        let tree = self.app.read(cx).settings.file_tree;
        range
            .filter_map(|row_ix| {
                let indent = |depth: usize| px(8. + depth as f32 * file_tree::INDENT);
                let (ix, depth) = match rows.get(row_ix)? {
                    ListRow::File { index, depth } => (*index, *depth),
                    ListRow::Dir {
                        path,
                        label,
                        depth,
                        files,
                        open,
                    } => {
                        let key = fold_key(area, path);
                        let cells = file_tree::dir_cells(
                            ("change-dir-name", row_ix),
                            label,
                            path,
                            *files,
                            *open,
                            cx,
                        );
                        return Some(
                            h_flex()
                                .id(("change-dir", row_ix))
                                .h(px(FILE_ROW_HEIGHT))
                                .w_full()
                                .pl(indent(*depth))
                                .pr_2()
                                .gap(px(5.))
                                .text_sm()
                                .cursor_pointer()
                                .hover(|s| s.bg(c.hover))
                                .children(cells)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.changes.focus.focus(window, cx);
                                    if !this.changes.folded.remove(&key) {
                                        this.changes.folded.insert(key.clone());
                                    }
                                    cx.notify();
                                }))
                                .into_any_element(),
                        );
                    }
                };
                let entry = list.get(ix)?;
                let kind = match area {
                    Area::Unstaged => entry.unstaged,
                    Area::Staged => entry.staged,
                }?;
                let is_primary = selected
                    .as_ref()
                    .is_some_and(|(a, p)| *a == area && *p == entry.path);
                let in_multi = multi && self.changes.multi.contains(&entry.path);
                let is_selected = is_primary || in_multi;
                let (dir, name) = match entry.path.rsplit_once('/') {
                    Some((dir, name)) => (dir.to_string(), name.to_string()),
                    None => (String::new(), entry.path.clone()),
                };
                let path = entry.path.clone();
                let menu = if in_multi {
                    self.multi_file_menu(area, cx)
                } else {
                    self.file_menu(area, entry, cx)
                };
                let menu_path = entry.path.clone();
                Some(
                    h_flex()
                        .id(("change-file", row_ix))
                        // 点右键先选中这个文件；点在多选的文件上就保持多选
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, _, window, cx| {
                                if !in_multi {
                                    this.click_file(
                                        area,
                                        menu_path.clone(),
                                        ix,
                                        Modifiers::default(),
                                        window,
                                        cx,
                                    );
                                }
                            }),
                        )
                        .h(px(FILE_ROW_HEIGHT))
                        .w_full()
                        .pl(indent(depth))
                        .pr_2()
                        .gap_2()
                        .text_sm()
                        .cursor_pointer()
                        .when(is_selected, |d| d.bg(c.selected))
                        .when(!is_selected, |d| d.hover(|s| s.bg(c.hover)))
                        // 右边显示的是哪个文件：左边一条色带，多选时也分得出来
                        .border_l_2()
                        .border_color(if is_primary {
                            c.info
                        } else {
                            gpui_kit::transparent_black()
                        })
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            if event.click_count() >= 2 {
                                this.toggle_file(area, &path, window, cx);
                            } else {
                                this.click_file(
                                    area,
                                    path.clone(),
                                    ix,
                                    event.modifiers(),
                                    window,
                                    cx,
                                );
                            }
                        }))
                        .child(file_tree::kind_badge(kind.letter(), kind_color(kind, c)))
                        // 名字先占地方，放不下时省略中间：开头、结尾和扩展名都看得到。
                        // 目录用剩下的，至少留一点让人知道还有目录
                        .child(
                            div()
                                .flex_shrink(1.)
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis_middle()
                                .child(name),
                        )
                        // 树形里目录在上面的行里，这里不再写
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(if tree || dir.is_empty() { 0. } else { 36. }))
                                .truncate()
                                .text_xs()
                                .text_color(c.muted)
                                .when(!tree, |cell| cell.child(dir)),
                        )
                        .tooltip({
                            let full: SharedString = match &entry.orig_path {
                                Some(orig) => format!("{orig} → {}", entry.path).into(),
                                None => entry.path.clone().into(),
                            };
                            move |window, cx| ui::tip(full.clone(), window, cx)
                        })
                        .context_menu(menu)
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_commit_box(&mut self, c: Colors, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.op.is_some();
        let amend = self.changes.amend;
        let staged = self.changes.staged.len();
        let committing = self
            .op
            .as_deref()
            .is_some_and(|op| op == tr("提交") || op == tr("修改提交"));
        let view = cx.entity().downgrade();
        let label: SharedString = if amend {
            tr("修改上次提交").into()
        } else if staged > 0 {
            tf!("提交（{staged} 个文件）", staged).into()
        } else {
            tr("提交").into()
        };
        let recent = self.recent_messages(cx);
        let recent_view = view.clone();
        let push_view = view.clone();
        let can_commit = !busy && (amend || staged > 0);
        v_flex()
            .flex_none()
            .p_2()
            .gap_2()
            .border_t_1()
            .border_color(c.border)
            .child(
                h_flex()
                    .h(px(20.))
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(c.muted)
                            .child(tr("提交说明")),
                    )
                    // 不加悬停提示：提示出现在按钮下面，会盖住弹出的菜单
                    .child(
                        Button::new("recent-messages")
                            .xsmall()
                            .ghost()
                            .icon(crate::ui::icon_small(IconName::Clock))
                            .label(tr("最近使用"))
                            .dropdown_menu(move |mut menu, _, _| {
                                if recent.is_empty() {
                                    return menu.item(
                                        PopupMenuItem::new(tr("暂无；提交后，说明将记录在此处"))
                                            .disabled(true),
                                    );
                                }
                                for message in &recent {
                                    let (view, text) = (recent_view.clone(), message.clone());
                                    menu = menu.item(
                                        PopupMenuItem::new(message_preview(message)).on_click(
                                            move |_, window, cx| {
                                                view.update(cx, |this, cx| {
                                                    this.use_message(text.clone(), window, cx)
                                                })
                                                .ok();
                                            },
                                        ),
                                    );
                                }
                                menu
                            }),
                    ),
            )
            .child(Textarea::new(&self.changes.message).h(px(110.)))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Checkbox::new("amend")
                            .label(tr("修改上次提交"))
                            .checked(amend)
                            .disabled(busy)
                            .on_click(move |checked, window, cx| {
                                view.update(cx, |this, cx| this.set_amend(*checked, window, cx))
                                    .ok();
                            }),
                    )
                    .child(div().flex_1())
                    // 左半边是「提交」，右半边的小箭头里有「提交并推送」
                    .child(
                        DropdownButton::new("commit-split")
                            .primary()
                            .small()
                            // 和 Fork 一样，没暂存东西时按钮变灰；Ctrl+Enter 仍会提示原因
                            .disabled(!can_commit)
                            .button(
                                Button::new("commit")
                                    .label(label)
                                    .loading(committing)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.commit(window, cx)),
                                    ),
                            )
                            // 按钮在窗口最底下，菜单往上开，不盖住按钮自己
                            .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _, _| {
                                let view = push_view.clone();
                                menu.item(
                                    PopupMenuItem::new(if amend {
                                        tr("修改上次提交并推送")
                                    } else {
                                        tr("提交并推送")
                                    })
                                    .on_click(
                                        move |_, window, cx| {
                                            view.update(cx, |this, cx| {
                                                this.commit_then(true, window, cx)
                                            })
                                            .ok();
                                        },
                                    ),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_stage_diff(&mut self, c: Colors, cx: &mut Context<Self>) -> AnyElement {
        // 语法高亮的颜色跟着主题走：第一次显示、换了深浅色主题时（重新）算一遍
        let dark = cx.theme().is_dark();
        if let Some(diff) = &mut self.changes.diff
            && diff.highlighted_dark != Some(dark)
        {
            diff.highlight(&cx.theme().highlight_theme, dark);
        }
        if self.changes.diff.is_none() {
            let text = if self.changes.unstaged.is_empty() && self.changes.staged.is_empty() {
                tr("工作区和暂存区均无改动")
            } else if self.changes.selected.is_some() {
                tr("正在读取改动…")
            } else {
                tr("选择文件以查看改动")
            };
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(c.muted)
                .child(text)
                .into_any_element();
        }
        let idle = self.op.is_none();
        let ws = self.ignore_ws;
        let line_ops = idle && !self.changes.diff_stale && !ws;
        let ws_strip = self
            .changes
            .ws_note
            .clone()
            .filter(|_| ws)
            .map(|note| diff_tools::info_strip("changes-ws-note", note, cx));
        let ws_toggle = self.ws_toggle("diff-ignore-ws", cx);
        let ws_toggle_alone = self.ws_toggle("diff-ignore-ws-alone", cx);
        let search_toggle = self.search_toggle(Where::Changes, cx);
        let search_bar = self.render_diff_search(Where::Changes, cx);
        let search_marks = self.search_decorations(Where::Changes, cx);
        let Some(diff) = &self.changes.diff else {
            return div().into_any_element();
        };
        let area = diff.area;
        let count = if diff.editable() {
            diff.selected_count()
        } else {
            0
        };
        let title = match &diff.entry.orig_path {
            Some(orig) => format!("{orig} → {}", diff.entry.path),
            None => diff.entry.path.clone(),
        };
        let entry = diff.entry.clone();
        let rows = diff.rows.len();
        let widest = diff.widest;
        let scroll = diff.scroll.clone();
        let notice = diff.notice.clone();
        let error = diff.error.clone();
        let split = self.split_mode(cx) && diff.editable();
        let split_state = self.changes.split.clone();
        // 图片不显示「二进制文件有差异」那一行，显示两张图
        let image = self.changes.image.clone().filter(|_| !diff.editable());
        let notice = notice.filter(|_| image.is_none());
        // 只有空白不同的文件，忽略空白以后什么都不剩：说清楚是因为忽略了空白，并留着那个开关
        let ws_empty = ws && !diff.editable() && image.is_none() && rows == 0 && error.is_none();
        let notice = match notice {
            Some(text) if ws_empty && text == tr("没有内容改动") => {
                Some(tr("忽略空白后，此文件没有差异").into())
            }
            other => other,
        };
        // 怎么看改动的两个开关，放在选行那一行的右边
        let toggles = h_flex()
            .flex_none()
            .gap_1()
            .child(search_toggle)
            .child(ws_toggle)
            .child(
                Button::new("diff-whole-file")
                    .xsmall()
                    .ghost()
                    .icon(crate::ui::icon(IconName::UnfoldVertical))
                    .selected(self.changes.whole_file)
                    .tooltip(tr("显示整个文件"))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_whole_file(cx))),
            )
            .child(
                Button::new("diff-split")
                    .xsmall()
                    .ghost()
                    .icon(crate::ui::icon(IconName::Columns2))
                    .selected(self.split_mode(cx))
                    .tooltip(tr("并排显示：左侧为旧版本，右侧为新版本"))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_split(cx))),
            );

        let mut bar = h_flex()
            .h(px(36.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(c.border)
            .child(
                div()
                    .id("stage-diff-title")
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_start()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title.clone())
                    .tooltip({
                        let title: SharedString = title.into();
                        move |window, cx| ui::tip(title.clone(), window, cx)
                    }),
            );
        // 选中行以后的按钮单独占一行，不和文件名、「暂存文件」挤在一起：窗口窄的时候那一行放不下。
        // 这一行一直在（没选中时写怎么选），选中、取消选中时下面的改动不会上下跳。
        let select_bar = diff.editable().then(|| {
            let bar = h_flex()
                .h(px(30.))
                .flex_none()
                .px_3()
                .gap_2()
                .border_b_1()
                .border_color(c.border)
                .text_xs()
                .text_color(c.muted)
                .when(split, |bar| {
                    bar.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, _| {
                            this.changes.hold_selection = this.changes.split_selection.is_some();
                        }),
                    )
                });
            if count == 0 {
                let hint = match (area, split) {
                    _ if ws => tr("已忽略空白：只能对整个文件操作，不能按块或按行操作"),
                    (Area::Unstaged, false) => {
                        tr("点击选择行，按住 Shift 连选；空格暂存，Delete 丢弃")
                    }
                    (Area::Staged, false) => tr("点击选择行，按住 Shift 连选；空格取消暂存"),
                    (Area::Unstaged, true) => tr("在行号上拖动选择多行；空格暂存，Delete 丢弃"),
                    (Area::Staged, true) => tr("在行号上拖动选择多行；空格取消暂存"),
                };
                return bar
                    .child(crate::ui::clipped("select-hint", hint).flex_1())
                    .child(toggles);
            }
            let bar = bar.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(tf!("已选中 {count} 行", count)),
            );
            let bar = match area {
                Area::Unstaged => bar
                    .child(
                        Button::new("stage-lines")
                            .xsmall()
                            .label(tr("暂存所选行"))
                            .when(ws, |b| b.tooltip(diff_tools::ws_blocked()))
                            .disabled(!line_ops)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.apply_selection(None, window, cx)
                            })),
                    )
                    .child(
                        Button::new("discard-lines")
                            .xsmall()
                            .ghost()
                            .label(tr("丢弃所选行…"))
                            .when(ws, |b| b.tooltip(diff_tools::ws_blocked()))
                            .disabled(!line_ops)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.discard_selection(None, window, cx)
                            })),
                    ),
                Area::Staged => bar.child(
                    Button::new("unstage-lines")
                        .xsmall()
                        .label(tr("取消暂存所选行"))
                        .when(ws, |b| b.tooltip(diff_tools::ws_blocked()))
                        .disabled(!line_ops)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.apply_selection(None, window, cx)
                        })),
                ),
            };
            bar.child(toggles)
        });
        let select_bar = select_bar.or_else(|| {
            ws_empty.then(|| {
                h_flex()
                    .h(px(30.))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(c.border)
                    .child(div().flex_1())
                    .child(ws_toggle_alone)
            })
        });
        bar = match area {
            Area::Unstaged => {
                let conflicted = entry.unstaged == Some(Kind::Conflicted);
                let (stage, discard) = (entry.clone(), entry.clone());
                let merge_path = entry.path.clone();
                bar.when(conflicted, |bar| {
                    bar.child(
                        Button::new("merge-editor")
                            .small()
                            .primary()
                            .label(tr("解决冲突…"))
                            .disabled(!idle)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_merge_editor(merge_path.clone(), window, cx)
                            })),
                    )
                })
                .child(
                    // 有冲突时主按钮是「解决冲突…」，这个退到普通样式
                    if conflicted {
                        Button::new("stage-file").small()
                    } else {
                        Button::new("stage-file").small().primary()
                    }
                    .label(if conflicted {
                        tr("标记为已解决")
                    } else {
                        tr("暂存文件")
                    })
                    .disabled(!idle)
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.stage_file(&stage, window, cx)),
                    ),
                )
                .when(!conflicted, |bar| {
                    bar.child(
                        Button::new("discard-file")
                            .small()
                            .ghost()
                            .label(tr("丢弃文件…"))
                            .disabled(!idle)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.discard_file(&discard, window, cx)
                            })),
                    )
                })
            }
            Area::Staged => bar.child(
                Button::new("unstage-file")
                    .small()
                    .label(tr("取消暂存文件"))
                    .disabled(!idle)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.unstage_file(&entry, window, cx)
                    })),
            ),
        };

        let body = match error {
            Some(err) => div()
                .p_3()
                .text_sm()
                .text_color(c.danger)
                .child(err)
                .into_any_element(),
            None if image.is_some() => div().size_full().children(image).into_any_element(),
            // 并排：组件库的改动视图，文件名那一条我们自己已经有了
            None if split => Diff::new(&split_state)
                .line_number(true)
                .syntax_highlight(true)
                .decorations(search_marks)
                .header_visible(false)
                .size_full()
                .into_any_element(),
            None => uniform_list(
                "stage-diff",
                rows,
                cx.processor(move |this, range: Range<usize>, _window, cx| {
                    this.render_diff_rows(range, c, cx)
                }),
            )
            .track_scroll(&scroll)
            .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
            .with_width_from_item(widest)
            .size_full()
            .into_any_element(),
        };
        v_flex()
            .size_full()
            .track_focus(&self.changes.diff_focus)
            .on_key_down(cx.listener(Self::diff_key))
            .child(bar)
            .children(select_bar)
            .children(search_bar)
            .children(ws_strip)
            .when_some(notice, |d, notice| {
                d.child(
                    div()
                        .px_3()
                        .py_1()
                        .flex_none()
                        .text_xs()
                        .text_color(c.muted)
                        .border_b_1()
                        .border_color(c.border)
                        .child(notice),
                )
            })
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn render_diff_rows(
        &mut self,
        range: Range<usize>,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let ws = self.ignore_ws;
        let line_ops = self.op.is_none() && !self.changes.diff_stale && !ws;
        let Some(diff) = &self.changes.diff else {
            return Vec::new();
        };
        let editable = diff.editable();
        let area = diff.area;
        // 搜索到的字的底色，现在停着的那一处深一些
        let warning = cx.theme().warning;
        let (found_bg, current_bg) = (warning.opacity(0.35), warning.opacity(0.8));
        let search = self.diff_search.at(Where::Changes);
        range
            .filter_map(|ix| {
                let text = diff.texts.get(ix)?.clone();
                Some(match diff.rows[ix] {
                    Row::Meta => h_flex()
                        .id(ix)
                        .h(px(DIFF_ROW_HEIGHT))
                        .w_full()
                        .px_3()
                        .font_family(MONO)
                        .text_size(px(12.))
                        .text_color(c.muted)
                        .child(text)
                        .into_any_element(),
                    Row::Hunk(h) => {
                        // 按钮在前、标题在后：窗口窄、标题长的时候按钮不会被挤到右边看不见的地方
                        let title = div()
                            .flex_none()
                            .font_family(MONO)
                            .text_size(px(12.))
                            .text_color(c.muted)
                            .child(text);
                        let mut row = h_flex()
                            .id(ix)
                            .h(px(DIFF_ROW_HEIGHT))
                            .w_full()
                            .px_3()
                            .gap_2()
                            .bg(c.hunk)
                            .border_t_1()
                            .border_color(c.border);
                        if editable {
                            row = match area {
                                Area::Unstaged => row
                                    .child(
                                        Button::new(("stage-hunk", h))
                                            .xsmall()
                                            .label(tr("暂存此块"))
                                            .when(ws, |b| b.tooltip(diff_tools::ws_blocked()))
                                            .disabled(!line_ops)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.apply_selection(Some(h), window, cx)
                                            })),
                                    )
                                    .child(
                                        Button::new(("discard-hunk", h))
                                            .xsmall()
                                            .ghost()
                                            .label(tr("丢弃此块…"))
                                            .when(ws, |b| b.tooltip(diff_tools::ws_blocked()))
                                            .disabled(!line_ops)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.discard_selection(Some(h), window, cx)
                                            })),
                                    ),
                                Area::Staged => row.child(
                                    Button::new(("unstage-hunk", h))
                                        .xsmall()
                                        .label(tr("取消暂存此块"))
                                        .when(ws, |b| b.tooltip(diff_tools::ws_blocked()))
                                        .disabled(!line_ops)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.apply_selection(Some(h), window, cx)
                                        })),
                                ),
                            };
                        }
                        row.child(title).into_any_element()
                    }
                    Row::Line(h, l) => {
                        let line = &diff.file.hunks[h].lines[l];
                        let (sign, bg, mark) = match line.kind {
                            LineKind::Added => ("+", Some(c.added), c.added_mark),
                            LineKind::Removed => ("-", Some(c.removed), c.removed_mark),
                            LineKind::Context => ("", None, c.added_mark),
                        };
                        // 语法高亮，加上这一行里具体改了的那几个词的底色
                        let marks: &[Range<usize>] =
                            diff.inline.get(ix).map_or(&[], |marks| marks.as_slice());
                        let syntax: &[(Range<usize>, HighlightStyle)] =
                            diff.highlights.get(ix).map_or(&[], |h| h.as_slice());
                        let mut styles = super::inline_diff::merge(syntax, marks, mark);
                        let (found, current) = search.marks(ix);
                        if !found.is_empty() {
                            styles = super::inline_diff::merge(&styles, &found, found_bg);
                        }
                        if let Some(current) = current {
                            styles = super::inline_diff::merge(&styles, &[current], current_bg);
                        }
                        let bg = if diff.selected[ix] {
                            Some(c.picked)
                        } else {
                            bg
                        };
                        h_flex()
                            .id(ix)
                            .h(px(DIFF_ROW_HEIGHT))
                            .w_full()
                            .font_family(MONO)
                            .text_size(px(13.))
                            .when_some(bg, |d, bg| d.bg(bg))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                    this.press_diff_row(ix, event, window, cx)
                                }),
                            )
                            .on_mouse_move(cx.listener(
                                move |this, event: &MouseMoveEvent, _, cx| {
                                    this.drag_diff_row(ix, event, cx)
                                },
                            ))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseUpEvent, _, _| this.end_diff_drag()),
                            )
                            .child(line_number(line.old_no, c))
                            .child(line_number(line.new_no, c))
                            .child(div().w(px(16.)).flex_none().text_color(c.muted).child(sign))
                            .child(div().flex_none().whitespace_nowrap().child(
                                if styles.is_empty() {
                                    text.into_any_element()
                                } else {
                                    StyledText::new(text)
                                        .with_highlights(styles)
                                        .into_any_element()
                                },
                            ))
                            .into_any_element()
                    }
                })
            })
            .collect()
    }
}

/// 菜单里显示的一条提交说明：只要第一行，太长截断；后面还有内容的加省略号。
fn message_preview(message: &str) -> String {
    const MAX: usize = 46;
    let first = message.lines().next().unwrap_or("").trim();
    let mut shown: String = first.chars().take(MAX).collect();
    if first.chars().count() > MAX || message.trim().lines().count() > 1 {
        shown.push('…');
    }
    shown
}

fn line_number(n: Option<u32>, c: Colors) -> Div {
    h_flex()
        .w(px(48.))
        .flex_none()
        .justify_end()
        .pr_2()
        .text_size(px(12.))
        .text_color(c.muted)
        .child(n.map(|n| n.to_string()).unwrap_or_default())
}
