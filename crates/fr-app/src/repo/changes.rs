//! 本地修改：未暂存和已暂存的文件、选中文件的改动（按块、按行操作）、提交说明。
//!
//! 和 Fork 一样：单击文件看改动，双击暂存或取消暂存；改动里按块、按行暂存、取消暂存或丢弃；
//! 提交说明框里 Ctrl+Enter 提交。

use std::ops::Range;

use fr_core::patch::{self, LineKind};
use fr_core::status::{Entry, Kind, Status};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::stage_diff::{Row, StageDiff};
use super::{Mode, RepoView};

const LIST_WIDTH: f32 = 340.;
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
    unstaged_scroll: UniformListScrollHandle,
    staged_scroll: UniformListScrollHandle,
    diff: Option<StageDiff>,
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
            TextareaState::new(window, cx).placeholder("提交说明，第一行是标题。Ctrl+Enter 提交")
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
        let changes = Self {
            unstaged: Vec::new(),
            staged: Vec::new(),
            selected: None,
            selected_ix: 0,
            unstaged_scroll: UniformListScrollHandle::new(),
            staged_scroll: UniformListScrollHandle::new(),
            diff: None,
            diff_stale: false,
            diff_task: None,
            message,
            amend: false,
            amend_message: None,
            amend_task: None,
        };
        (changes, vec![subscription])
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

impl RepoView {
    pub(super) fn apply_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.changes.unstaged = status.unstaged().cloned().collect();
        self.changes.staged = status.staged().cloned().collect();
        self.status = Some(status);
        self.sync_file_selection(cx);
    }

    /// 状态变了以后：选中的文件还在就重读它的改动；不在了就选同一位置的下一个文件。
    fn sync_file_selection(&mut self, cx: &mut Context<Self>) {
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
        let list = self.changes.list(area);
        if !list.is_empty() {
            let ix = self.changes.selected_ix.min(list.len() - 1);
            let next = list[ix].path.clone();
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

    /// 切到本地修改时还没选文件：选第一个。
    pub(super) fn select_first_file(&mut self, cx: &mut Context<Self>) {
        let first = self
            .changes
            .unstaged
            .first()
            .map(|e| (Area::Unstaged, e.path.clone()))
            .or_else(|| {
                self.changes
                    .staged
                    .first()
                    .map(|e| (Area::Staged, e.path.clone()))
            });
        match first {
            Some((area, path)) => self.select_file(area, path, 0, cx),
            None => {
                self.changes.selected = None;
                self.changes.diff = None;
                self.changes.diff_stale = false;
            }
        }
    }

    fn select_file(&mut self, area: Area, path: String, ix: usize, cx: &mut Context<Self>) {
        let same = self.changes.selected.as_ref() == Some(&(area, path.clone()));
        self.changes.selected = Some((area, path));
        self.changes.selected_ix = ix;
        if !same || self.changes.diff.is_none() {
            self.load_file_diff(cx);
        }
        cx.notify();
    }

    fn load_file_diff(&mut self, cx: &mut Context<Self>) {
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
        let (args, ok): (Vec<String>, &'static [i32]) = match area {
            Area::Unstaged if entry.unstaged == Some(Kind::Untracked) => {
                (patch::untracked_args(&path), &[0, 1])
            }
            Area::Unstaged => (patch::unstaged_args(&path), &[0]),
            Area::Staged => (patch::staged_args(&path, entry.orig_path.as_deref()), &[0]),
        };
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
            this.update(cx, |this, cx| {
                // 选中的文件已经换了：这个结果作废
                if this.changes.selected.as_ref() != Some(&(area, entry.path.clone())) {
                    return;
                }
                this.changes.diff_stale = false;
                // 内容没变（比如别的文件触发的刷新）：沿用原来的，正在选的行不清掉
                if let (Ok(out), Some(old)) = (&result, &mut this.changes.diff)
                    && out.code.is_some_and(|c| ok.contains(&c))
                    && old.same_source(area, &entry.path, &out.stdout)
                {
                    old.entry = entry;
                    cx.notify();
                    return;
                }
                this.changes.diff = Some(match result {
                    Ok(out) if out.code.is_some_and(|c| ok.contains(&c)) => {
                        StageDiff::new(area, entry, &out.stdout, scroll)
                    }
                    Ok(out) => StageDiff::failed(area, entry, out.stderr.into(), scroll),
                    Err(err) => StageDiff::failed(area, entry, format!("{err:#}").into(), scroll),
                });
                cx.notify();
            })
            .ok();
        }));
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

    fn stage_file(&mut self, entry: &Entry, window: &mut Window, cx: &mut Context<Self>) {
        let args = vec!["add".into(), "--".into(), entry.path.clone()];
        self.run_op("暂存", args, None, window, cx, |_, _, _, _| {});
    }

    fn unstage_file(&mut self, entry: &Entry, window: &mut Window, cx: &mut Context<Self>) {
        let mut args: Vec<String> =
            vec!["reset".into(), "-q".into(), "--".into(), entry.path.clone()];
        // 改名的文件：原路径也要恢复
        args.extend(entry.orig_path.clone());
        self.run_op("取消暂存", args, None, window, cx, |_, _, _, _| {});
    }

    fn discard_file(&mut self, entry: &Entry, window: &mut Window, cx: &mut Context<Self>) {
        let untracked = entry.unstaged == Some(Kind::Untracked);
        let base: &[&str] = if untracked {
            &["clean", "-f", "-q", "--"]
        } else {
            &["checkout", "-q", "--"]
        };
        let mut args: Vec<String> = base.iter().map(|s| s.to_string()).collect();
        args.push(entry.path.clone());
        let description = if untracked {
            format!("未跟踪的文件 {} 会被删除，不能撤销。", entry.path)
        } else {
            format!("{} 在工作区里没暂存的改动会被丢弃，不能撤销。", entry.path)
        };
        self.confirm(
            "丢弃改动？",
            description,
            "丢弃",
            window,
            cx,
            move |this, window, cx| {
                this.run_op("丢弃", args.clone(), None, window, cx, |_, _, _, _| {});
            },
        );
    }

    fn stage_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let args = vec!["add".into(), "-A".into()];
        self.run_op("暂存", args, None, window, cx, |_, _, _, _| {});
    }

    fn unstage_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let args = vec!["reset".into(), "-q".into()];
        self.run_op("取消暂存", args, None, window, cx, |_, _, _, _| {});
    }

    /// 暂存（未暂存一栏）或取消暂存（已暂存一栏）选中的行；给了 `hunk` 就是整块。
    fn apply_selection(
        &mut self,
        hunk: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.changes.diff_stale {
            return;
        }
        let Some(diff) = &self.changes.diff else {
            return;
        };
        let reverse = diff.area == Area::Staged;
        let Some(bytes) = diff.patch(reverse, hunk) else {
            return;
        };
        let label = if reverse { "取消暂存" } else { "暂存" };
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
        if self.changes.diff_stale {
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
        let description = format!(
            "{} 里的 {count} 行改动会从工作区丢弃，不能撤销。",
            diff.entry.path
        );
        self.confirm(
            "丢弃改动？",
            description,
            "丢弃",
            window,
            cx,
            move |this, window, cx| {
                this.run_op(
                    "丢弃",
                    patch::discard_args(),
                    Some(bytes.clone()),
                    window,
                    cx,
                    |_, _, _, _| {},
                );
            },
        );
    }

    fn press_diff_row(&mut self, row: usize, event: &MouseDownEvent, cx: &mut Context<Self>) {
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
        if self.op.is_some() {
            return;
        }
        let message = self.changes.message.read(cx).value().trim().to_string();
        if message.is_empty() {
            window.push_notification(Notification::warning("先写提交说明"), cx);
            return;
        }
        let amend = self.changes.amend;
        if !amend && self.changes.staged.is_empty() {
            window.push_notification(Notification::warning("没有已暂存的改动"), cx);
            return;
        }
        let mut args: Vec<String> = vec!["commit".into(), "-F".into(), "-".into()];
        if amend {
            args.push("--amend".into());
        }
        let label = if amend { "修改提交" } else { "提交" };
        self.run_op(
            label,
            args,
            Some(message.into_bytes()),
            window,
            cx,
            |this, _, window, cx| {
                this.changes
                    .message
                    .update(cx, |m, cx| m.set_value("", window, cx));
                this.changes.amend = false;
                this.changes.amend_message = None;
            },
        );
    }

    pub(super) fn render_changes(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let c = colors(cx);
        h_flex()
            .size_full()
            .child(
                v_flex()
                    .w(px(LIST_WIDTH))
                    .flex_none()
                    .h_full()
                    .border_r_1()
                    .border_color(c.border)
                    .child(self.render_file_section(Area::Unstaged, c, cx))
                    .child(self.render_file_section(Area::Staged, c, cx))
                    .child(self.render_commit_box(c, cx)),
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

    fn render_file_section(&mut self, area: Area, c: Colors, cx: &mut Context<Self>) -> AnyElement {
        let count = self.changes.list(area).len();
        let busy = self.op.is_some();
        let (title, action, empty, id) = match area {
            Area::Unstaged => ("未暂存", "全部暂存", "工作区没有改动", "unstaged"),
            Area::Staged => ("已暂存", "全部取消暂存", "还没有暂存的改动", "staged"),
        };
        let scroll = match area {
            Area::Unstaged => self.changes.unstaged_scroll.clone(),
            Area::Staged => self.changes.staged_scroll.clone(),
        };
        let body = if count == 0 {
            div()
                .p_3()
                .text_sm()
                .text_color(c.muted)
                .child(empty)
                .into_any_element()
        } else {
            uniform_list(
                id,
                count,
                cx.processor(move |this, range: Range<usize>, _window, cx| {
                    this.render_file_rows(area, range, c, cx)
                }),
            )
            .track_scroll(&scroll)
            .size_full()
            .into_any_element()
        };
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
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("{title}（{count}）")),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new(SharedString::from(format!("{id}-all")))
                            .xsmall()
                            .ghost()
                            .label(action)
                            .disabled(busy || count == 0)
                            .on_click(cx.listener(move |this, _, window, cx| match area {
                                Area::Unstaged => this.stage_all(window, cx),
                                Area::Staged => this.unstage_all(window, cx),
                            })),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn render_file_rows(
        &mut self,
        area: Area,
        range: Range<usize>,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let list = self.changes.list(area);
        let selected = self.changes.selected.clone();
        range
            .filter_map(|ix| {
                let entry = list.get(ix)?;
                let kind = match area {
                    Area::Unstaged => entry.unstaged,
                    Area::Staged => entry.staged,
                }?;
                let is_selected = selected
                    .as_ref()
                    .is_some_and(|(a, p)| *a == area && *p == entry.path);
                let (dir, name) = match entry.path.rsplit_once('/') {
                    Some((dir, name)) => (dir.to_string(), name.to_string()),
                    None => (String::new(), entry.path.clone()),
                };
                let path = entry.path.clone();
                Some(
                    h_flex()
                        .id(ix)
                        .h(px(FILE_ROW_HEIGHT))
                        .w_full()
                        .px_2()
                        .gap_2()
                        .text_sm()
                        .cursor_pointer()
                        .when(is_selected, |d| d.bg(c.selected))
                        .when(!is_selected, |d| d.hover(|s| s.bg(c.hover)))
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            if event.click_count() >= 2 {
                                this.toggle_file(area, &path, window, cx);
                            } else {
                                this.select_file(area, path.clone(), ix, cx);
                            }
                        }))
                        .child(
                            div()
                                .w(px(12.))
                                .flex_none()
                                .font_weight(FontWeight::BOLD)
                                .text_color(kind_color(kind, c))
                                .child(kind.letter()),
                        )
                        .child(div().flex_none().max_w(px(220.)).truncate().child(name))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(c.muted)
                                .child(dir),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_commit_box(&mut self, c: Colors, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.op.is_some();
        let amend = self.changes.amend;
        let staged = self.changes.staged.len();
        let committing = matches!(self.op.as_deref(), Some("提交" | "修改提交"));
        let view = cx.entity().downgrade();
        let label: SharedString = if amend {
            "修改上次提交".into()
        } else if staged > 0 {
            format!("提交（{staged} 个文件）").into()
        } else {
            "提交".into()
        };
        v_flex()
            .flex_none()
            .p_2()
            .gap_2()
            .border_t_1()
            .border_color(c.border)
            .child(Textarea::new(&self.changes.message).h(px(110.)))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Checkbox::new("amend")
                            .label("修改上次提交")
                            .checked(amend)
                            .on_click(move |checked, window, cx| {
                                view.update(cx, |this, cx| this.set_amend(*checked, window, cx))
                                    .ok();
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("commit")
                            .primary()
                            .small()
                            .label(label)
                            .loading(committing)
                            // 和 Fork 一样，没暂存东西时按钮变灰；Ctrl+Enter 仍会提示原因
                            .disabled(busy || (!amend && staged == 0))
                            .on_click(cx.listener(|this, _, window, cx| this.commit(window, cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_stage_diff(&mut self, c: Colors, cx: &mut Context<Self>) -> AnyElement {
        let Some(diff) = &self.changes.diff else {
            let text = if self.changes.unstaged.is_empty() && self.changes.staged.is_empty() {
                "工作区和暂存区都没有改动"
            } else if self.changes.selected.is_some() {
                "正在读取改动…"
            } else {
                "选一个文件查看改动"
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
        };
        let idle = self.op.is_none();
        let line_ops = idle && !self.changes.diff_stale;
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

        let mut bar = h_flex()
            .h(px(36.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(c.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            );
        if count > 0 {
            bar = match area {
                Area::Unstaged => bar
                    .child(
                        Button::new("stage-lines")
                            .small()
                            .label(format!("暂存选中的 {count} 行"))
                            .disabled(!line_ops)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.apply_selection(None, window, cx)
                            })),
                    )
                    .child(
                        Button::new("discard-lines")
                            .small()
                            .ghost()
                            .label(format!("丢弃选中的 {count} 行…"))
                            .disabled(!line_ops)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.discard_selection(None, window, cx)
                            })),
                    ),
                Area::Staged => bar.child(
                    Button::new("unstage-lines")
                        .small()
                        .label(format!("取消暂存选中的 {count} 行"))
                        .disabled(!line_ops)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.apply_selection(None, window, cx)
                        })),
                ),
            };
        }
        bar = match area {
            Area::Unstaged => {
                let conflicted = entry.unstaged == Some(Kind::Conflicted);
                let (stage, discard) = (entry.clone(), entry.clone());
                bar.child(
                    Button::new("stage-file")
                        .small()
                        .primary()
                        .label(if conflicted {
                            "标记为已解决"
                        } else {
                            "暂存文件"
                        })
                        .disabled(!idle)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.stage_file(&stage, window, cx)
                        })),
                )
                .when(!conflicted, |bar| {
                    bar.child(
                        Button::new("discard-file")
                            .small()
                            .ghost()
                            .label("丢弃文件…")
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
                    .label("取消暂存文件")
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
            .child(bar)
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
        let line_ops = self.op.is_none() && !self.changes.diff_stale;
        let Some(diff) = &self.changes.diff else {
            return Vec::new();
        };
        let editable = diff.editable();
        let area = diff.area;
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
                        let mut row = h_flex()
                            .id(ix)
                            .h(px(DIFF_ROW_HEIGHT))
                            .w_full()
                            .px_3()
                            .gap_2()
                            .bg(c.hunk)
                            .border_t_1()
                            .border_color(c.border)
                            .child(
                                div()
                                    .flex_none()
                                    .font_family(MONO)
                                    .text_size(px(12.))
                                    .text_color(c.muted)
                                    .child(text),
                            );
                        if editable {
                            row = match area {
                                Area::Unstaged => row
                                    .child(
                                        Button::new(("stage-hunk", h))
                                            .xsmall()
                                            .label("暂存此块")
                                            .disabled(!line_ops)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.apply_selection(Some(h), window, cx)
                                            })),
                                    )
                                    .child(
                                        Button::new(("discard-hunk", h))
                                            .xsmall()
                                            .ghost()
                                            .label("丢弃此块…")
                                            .disabled(!line_ops)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.discard_selection(Some(h), window, cx)
                                            })),
                                    ),
                                Area::Staged => row.child(
                                    Button::new(("unstage-hunk", h))
                                        .xsmall()
                                        .label("取消暂存此块")
                                        .disabled(!line_ops)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.apply_selection(Some(h), window, cx)
                                        })),
                                ),
                            };
                        }
                        row.into_any_element()
                    }
                    Row::Line(h, l) => {
                        let line = &diff.file.hunks[h].lines[l];
                        let (sign, bg) = match line.kind {
                            LineKind::Added => ("+", Some(c.added)),
                            LineKind::Removed => ("-", Some(c.removed)),
                            LineKind::Context => ("", None),
                        };
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
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    this.press_diff_row(ix, event, cx)
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
                            .child(div().flex_none().whitespace_nowrap().child(text))
                            .into_any_element()
                    }
                })
            })
            .collect()
    }
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
