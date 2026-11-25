//! 本地修改的单列表（`docs/design/narrow-and-changes/` 方向 ②）：未暂存、已暂存合成一个
//! 列表，按路径排、勾选时不跳动；行首的方框暂存（全部暂存是勾、部分暂存是「—」并在右边写「2/5」）。
//!
//! 文件列表上面的「分栏｜单列表」切换，按仓库记住；没切过的用设置里的默认（默认分栏，不改变老用户）。
//! 右边的改动照旧是「未暂存」或「已暂存」那一份：部分暂存的文件在改动上面多一个切换。

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::rc::Rc;

use fr_core::i18n::tr;
use fr_core::status::{Entry, Kind};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::super::RepoView;
use super::super::file_tree::{self, Row as ListRow};
use super::{Area, Colors, FILE_ROW_HEIGHT, kind_color};
use crate::ui;

/// 一个文件暂存了多少。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::repo) enum Staged {
    None,
    Part,
    All,
}

pub(in crate::repo) fn staged_of(entry: &Entry) -> Staged {
    match (entry.staged.is_some(), entry.unstaged.is_some()) {
        (true, false) => Staged::All,
        (true, true) => Staged::Part,
        _ => Staged::None,
    }
}

/// 拆 `git diff --numstat -z` 的输出：路径 →（加的行 + 删的行）。二进制文件（写的是 `-`）算 0。
fn parse_numstat(out: &[u8]) -> HashMap<String, u32> {
    let mut map = HashMap::new();
    let mut rest = out;
    let field = |rest: &mut &[u8], end: u8| -> Option<String> {
        let at = rest.iter().position(|b| *b == end)?;
        let text = String::from_utf8_lossy(&rest[..at]).into_owned();
        *rest = &rest[at + 1..];
        Some(text)
    };
    while !rest.is_empty() {
        let (Some(added), Some(removed)) = (field(&mut rest, b'\t'), field(&mut rest, b'\t'))
        else {
            break;
        };
        // 改名：数字后面紧跟 NUL，接着是旧路径、新路径
        let path = if rest.first() == Some(&0) {
            rest = &rest[1..];
            let _old = field(&mut rest, 0);
            field(&mut rest, 0)
        } else {
            field(&mut rest, 0)
        };
        let Some(path) = path else {
            break;
        };
        let n = |text: &str| text.parse::<u32>().unwrap_or(0);
        map.insert(path, n(&added) + n(&removed));
    }
    map
}

impl RepoView {
    /// 本地修改现在是不是单列表：这个仓库切过的照它，没切过的照设置里的默认。
    pub(in crate::repo) fn single_list_on(&self, cx: &App) -> bool {
        let settings = &self.app.read(cx).settings;
        settings
            .changes_views
            .get(&self.view_key())
            .copied()
            .unwrap_or(settings.changes_single)
    }

    /// 记「分栏｜单列表」用的键：哪台电脑上的哪个仓库。
    fn view_key(&self) -> String {
        super::super::ops::writing_key(self.entry.server.as_deref(), &self.entry.path)
            .replace('\n', "|")
    }

    /// 切换分栏、单列表（文件列表上面的开关），记在这个仓库上。
    pub(in crate::repo) fn set_single_list(&mut self, on: bool, cx: &mut Context<Self>) {
        let key = self.view_key();
        self.app.update(cx, |app, cx| {
            app.update_settings(cx, |s| {
                // 和默认一样就不单独记
                match on == s.changes_single {
                    true => s.changes_views.remove(&key),
                    false => s.changes_views.insert(key, on),
                };
            })
        });
        self.changes.multi.clear();
        if on {
            self.load_numstat(cx);
        }
        cx.notify();
    }

    /// 状态变了以后（`apply_status`）：单列表按路径重排，部分暂存的文件重读行数。
    pub(in crate::repo) fn sync_single(&mut self, cx: &mut Context<Self>) {
        let mut all: Vec<Entry> = self
            .status
            .as_ref()
            .map(|status| status.entries.clone())
            .unwrap_or_default();
        all.sort_by(|a, b| a.path.cmp(&b.path));
        self.changes.single = all;
        if self.single_list_on(cx) {
            self.load_numstat(cx);
        }
    }

    /// 读部分暂存的文件暂存了几行、一共改了几行（「2/5」）。没有部分暂存的文件不读。
    fn load_numstat(&mut self, cx: &mut Context<Self>) {
        let partial = self
            .changes
            .single
            .iter()
            .any(|entry| staged_of(entry) == Staged::Part);
        if !partial {
            self.changes.numstat.clear();
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        let args = |cached: bool| {
            let mut args: Vec<String> = vec!["diff".into(), "--numstat".into(), "-z".into()];
            if cached {
                args.push("--cached".into());
                args.push("--find-renames".into());
            }
            args
        };
        let staged = channel.git(&repo, args(true));
        let unstaged = channel.git(&repo, args(false));
        self.changes.numstat_task = Some(cx.spawn(async move |this, cx| {
            let (staged, unstaged) = futures::join!(staged, unstaged);
            let read = |out: anyhow::Result<fr_channel::GitOutput>| {
                out.ok()
                    .filter(|out| out.success())
                    .map(|out| parse_numstat(&out.stdout))
                    .unwrap_or_default()
            };
            let (staged, unstaged) = (read(staged), read(unstaged));
            this.update(cx, |this, cx| {
                let mut map = HashMap::new();
                for (path, lines) in &staged {
                    let rest = unstaged.get(path).copied().unwrap_or(0);
                    map.insert(path.clone(), (*lines, rest));
                }
                this.changes.numstat = map;
                cx.notify();
            })
            .ok();
        }));
    }

    /// 单列表里列出来的文件（下标对着 `changes.single`）：过滤框里的字、隐藏未跟踪都算进去了。
    fn single_shown(&self, cx: &App) -> Vec<usize> {
        let needle = self.changes.filter.read(cx).value().trim().to_lowercase();
        let hide = self.hide_untracked(cx);
        self.changes
            .single
            .iter()
            .enumerate()
            .filter(|(_, e)| !(hide && e.unstaged == Some(Kind::Untracked)))
            .filter(|(_, e)| file_tree::matches(&e.path, &needle))
            .map(|(ix, _)| ix)
            .collect()
    }

    /// 单列表现在显示的行：树形时有目录行。
    fn single_rows(&self, cx: &App) -> Vec<ListRow> {
        let list = &self.changes.single;
        let files: Vec<(&str, usize)> = self
            .single_shown(cx)
            .into_iter()
            .map(|ix| (list[ix].path.as_str(), ix))
            .collect();
        let tree = self.app.read(cx).settings.file_tree;
        let filtering = !self.changes.filter.read(cx).value().trim().is_empty();
        let folded = &self.changes.folded;
        file_tree::rows(&files, tree, &|path| {
            !filtering && folded.contains(&format!("a:{path}"))
        })
    }

    /// 单列表里看得见的文件，按显示的顺序。
    pub(in crate::repo) fn single_order(&self, cx: &App) -> Vec<usize> {
        self.single_rows(cx)
            .into_iter()
            .filter_map(|row| match row {
                ListRow::File { index, .. } => Some(index),
                ListRow::Dir { .. } => None,
            })
            .collect()
    }

    /// 选中单列表里的第 `ix` 个文件：右边显示它未暂存的那一份，全部暂存了的显示已暂存的。
    /// 已经选着它、正看着另一份（部分暂存的文件切过）就不换。
    pub(in crate::repo) fn select_single(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.changes.single.get(ix).cloned() else {
            return;
        };
        let keep = match &self.changes.selected {
            Some((area, path)) if *path == entry.path => match area {
                Area::Unstaged => entry.unstaged.is_some().then_some(Area::Unstaged),
                Area::Staged => entry.staged.is_some().then_some(Area::Staged),
            },
            _ => None,
        };
        let area = keep.unwrap_or(match entry.unstaged.is_some() {
            true => Area::Unstaged,
            false => Area::Staged,
        });
        let at = self
            .changes
            .list(area)
            .iter()
            .position(|e| e.path == entry.path)
            .unwrap_or(0);
        self.changes.single_anchor_path = Some(entry.path.clone());
        let order = self.single_order(cx);
        self.changes.single_pos = order.iter().position(|shown| *shown == ix).unwrap_or(0);
        self.select_file(area, entry.path, at, cx);
    }

    /// 单列表里点了一个文件（方框以外的地方）：普通点击只选它；Ctrl 加点击加进多选或去掉；Shift
    /// 加点击连选一段。
    fn click_single(
        &mut self,
        ix: usize,
        modifiers: Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.changes.focus.focus(window, cx);
        let Some(path) = self.changes.single.get(ix).map(|e| e.path.clone()) else {
            return;
        };
        if modifiers.shift {
            let from = self.single_anchor(cx).unwrap_or(ix);
            self.changes.multi = self.single_between(from, ix, cx);
            let anchor = self.changes.single_anchor_path.clone();
            self.select_single(ix, cx);
            self.changes.single_anchor_path = anchor;
        } else if modifiers.control || modifiers.platform {
            if self.changes.multi.is_empty()
                && let Some((_, current)) = &self.changes.selected
            {
                self.changes.multi.insert(current.clone());
            }
            if self.changes.multi.contains(&path) && self.changes.multi.len() > 1 {
                self.changes.multi.remove(&path);
                let shown = matches!(&self.changes.selected, Some((_, p)) if *p == path);
                let next = self.changes.multi.iter().next().cloned();
                if shown && let Some(next) = next {
                    let at = self.changes.single.iter().position(|e| e.path == next);
                    if let Some(at) = at {
                        self.select_single(at, cx);
                    }
                }
            } else {
                self.changes.multi.insert(path);
                self.select_single(ix, cx);
            }
        } else {
            self.changes.multi.clear();
            self.select_single(ix, cx);
        }
        if self.changes.multi.len() <= 1 {
            self.changes.multi.clear();
        }
        cx.notify();
    }

    /// Shift 连选的起点在单列表里的位置。
    fn single_anchor(&self, _cx: &App) -> Option<usize> {
        let path = self.changes.single_anchor_path.as_ref().or(self
            .changes
            .selected
            .as_ref()
            .map(|(_, path)| path))?;
        self.changes.single.iter().position(|e| e.path == *path)
    }

    /// 显示的顺序里从 `from` 到 `to`（两头都算）的文件。
    fn single_between(&self, from: usize, to: usize, cx: &App) -> BTreeSet<String> {
        let order = self.single_order(cx);
        let at = |ix: usize| order.iter().position(|shown| *shown == ix);
        let picked: Vec<usize> = match (at(from), at(to)) {
            (Some(a), Some(b)) => order[a.min(b)..=a.max(b)].to_vec(),
            _ => vec![to],
        };
        picked
            .into_iter()
            .filter_map(|ix| self.changes.single.get(ix))
            .map(|e| e.path.clone())
            .collect()
    }

    /// 上下键换文件；按着 Shift 连选。
    pub(in crate::repo) fn move_single(
        &mut self,
        down: bool,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let order = self.single_order(cx);
        let current = self.changes.selected.as_ref().and_then(|(_, path)| {
            let ix = self.changes.single.iter().position(|e| e.path == *path)?;
            order.iter().position(|shown| *shown == ix)
        });
        let next = match (current, down) {
            (Some(at), true) => order.get(at + 1).copied(),
            (Some(at), false) => at.checked_sub(1).map(|at| order[at]),
            (None, _) => order.first().copied(),
        };
        let Some(ix) = next else {
            return;
        };
        if extend {
            let from = self.single_anchor(cx).unwrap_or(ix);
            self.changes.multi = self.single_between(from, ix, cx);
            if self.changes.multi.len() <= 1 {
                self.changes.multi.clear();
            }
            let anchor = self.changes.single_anchor_path.clone();
            self.select_single(ix, cx);
            self.changes.single_anchor_path = anchor;
        } else {
            self.changes.multi.clear();
            self.select_single(ix, cx);
        }
        if let Some(row) = self
            .single_rows(cx)
            .iter()
            .position(|row| matches!(row, ListRow::File { index, .. } if *index == ix))
        {
            self.changes
                .single_scroll
                .scroll_to_item(row, ScrollStrategy::Nearest);
        }
    }

    /// 单列表里选中的文件：多选时是全部，否则是右边显示的那一个。
    fn single_picked(&self) -> Vec<Entry> {
        let picked = |path: &str| match self.changes.multi.is_empty() {
            true => matches!(&self.changes.selected, Some((_, p)) if p == path),
            false => self.changes.multi.contains(path),
        };
        self.changes
            .single
            .iter()
            .filter(|e| picked(&e.path))
            .cloned()
            .collect()
    }

    /// 点方框（或者空格）：有没暂存完的就把它们整个暂存，都已暂存了就取消暂存。
    fn toggle_single(&mut self, entries: Vec<Entry>, window: &mut Window, cx: &mut Context<Self>) {
        if entries.is_empty() {
            return;
        }
        let all_staged = entries.iter().all(|e| staged_of(e) == Staged::All);
        match all_staged {
            true => self.unstage_files(entries, window, cx),
            false => {
                let rest: Vec<Entry> = entries
                    .into_iter()
                    .filter(|e| staged_of(e) != Staged::All)
                    .collect();
                self.stage_files(rest, window, cx)
            }
        }
    }

    /// 空格、回车：选中的文件暂存或取消暂存。
    pub(in crate::repo) fn toggle_single_picked(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picked = self.single_picked();
        self.toggle_single(picked, window, cx);
    }

    /// Delete：丢弃选中的文件未暂存的改动（已暂存的部分不动）。
    pub(in crate::repo) fn discard_single_picked(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picked: Vec<Entry> = self
            .single_picked()
            .into_iter()
            .filter(|e| e.unstaged.is_some())
            .collect();
        if !picked.is_empty() {
            self.discard_files(picked, window, cx);
        }
    }

    /// Ctrl+A：全选列出来的文件。
    pub(in crate::repo) fn select_all_single(&mut self, cx: &mut Context<Self>) {
        let all: BTreeSet<String> = self
            .single_shown(cx)
            .into_iter()
            .map(|ix| self.changes.single[ix].path.clone())
            .collect();
        self.changes.multi = if all.len() > 1 { all } else { BTreeSet::new() };
        cx.notify();
    }

    /// 状态变了以后选中的文件还在单列表里：换到它现在有的那一份（整个暂存了就看已暂存的）。
    /// 返回是不是处理了；不在了交给原来的逻辑选下一个。
    pub(in crate::repo) fn keep_single_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((area, path)) = self.changes.selected.clone() else {
            return false;
        };
        let Some(ix) = self.changes.single.iter().position(|e| e.path == path) else {
            // 不在了（丢弃了、提交了）：选同一位置的下一个
            let order = self.single_order(cx);
            let at = self.changes.single_pos.min(order.len().saturating_sub(1));
            return match order.get(at).copied() {
                Some(next) => {
                    self.select_single(next, cx);
                    true
                }
                None => false,
            };
        };
        let entry = &self.changes.single[ix];
        let still = match area {
            Area::Unstaged => entry.unstaged.is_some(),
            Area::Staged => entry.staged.is_some(),
        };
        if still {
            return false;
        }
        self.select_single(ix, cx);
        true
    }

    /// 部分暂存的文件：改动上面「未暂存｜已暂存」切换看哪一份。别的文件没有。
    pub(in crate::repo) fn single_area_switch(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.single_list_on(cx) {
            return None;
        }
        let (area, path) = self.changes.selected.clone()?;
        let entry = self.changes.single.iter().find(|e| e.path == path)?;
        if staged_of(entry) != Staged::Part {
            return None;
        }
        let (a, b) = (path.clone(), path);
        Some(
            ButtonGroup::new("single-area")
                .outline()
                .xsmall()
                .child(
                    Button::new("single-area-unstaged")
                        .label(tr("未暂存"))
                        .selected(area == Area::Unstaged)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let at = this.changes.unstaged.iter().position(|e| e.path == a);
                            if let Some(at) = at {
                                this.select_file(Area::Unstaged, a.clone(), at, cx);
                            }
                        })),
                )
                .child(
                    Button::new("single-area-staged")
                        .label(tr("已暂存"))
                        .selected(area == Area::Staged)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let at = this.changes.staged.iter().position(|e| e.path == b);
                            if let Some(at) = at {
                                this.select_file(Area::Staged, b.clone(), at, cx);
                            }
                        })),
                )
                .into_any_element(),
        )
    }

    /// 单列表：上面一行全选方框、「更改 n」「已暂存 n 个文件」，下面是文件。
    pub(super) fn render_single_section(
        &mut self,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shown = self.single_shown(cx);
        let total = self.changes.single.len();
        let hidden = self.hidden_untracked(cx);
        let staged_files = self
            .changes
            .single
            .iter()
            .filter(|e| e.staged.is_some())
            .count();
        let shown_entries: Vec<Entry> = shown
            .iter()
            .map(|ix| self.changes.single[*ix].clone())
            .collect();
        // 全选方框：列出来的全部暂存了是勾，一个都没暂存是空，其余是「—」
        let all_state = if shown_entries.is_empty() {
            Staged::None
        } else if shown_entries.iter().all(|e| staged_of(e) == Staged::All) {
            Staged::All
        } else if shown_entries.iter().all(|e| staged_of(e) == Staged::None) {
            Staged::None
        } else {
            Staged::Part
        };
        let busy = self.op.is_some();
        let picked = self.changes.multi.len();
        let filtering = !self.changes.filter.read(cx).value().trim().is_empty();
        let rows = Rc::new(self.single_rows(cx));
        let scroll = self.changes.single_scroll.clone();
        let body = if shown.is_empty() {
            let text = match (total, total == hidden) {
                (0, _) => tr("工作区和暂存区均无改动"),
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
                "single-list",
                rows.len(),
                cx.processor(move |this, range: Range<usize>, _window, cx| {
                    this.render_single_rows(&rows, range, c, cx)
                }),
            )
            .track_scroll(&scroll)
            .size_full()
            .into_any_element()
        };
        let all_tip: SharedString = match (all_state, filtering) {
            (Staged::All, false) => tr("取消暂存全部文件").into(),
            (Staged::All, true) => tr("取消暂存匹配的文件").into(),
            (_, false) => tr("暂存全部文件").into(),
            (_, true) => tr("暂存匹配的文件").into(),
        };
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
                        "single-hidden-untracked",
                        tf!("已隐藏 {n} 个未跟踪的文件", n = hidden),
                    )
                    .flex_shrink(1.),
                )
                .child(
                    div()
                        .id("single-show-untracked")
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
            .child(
                h_flex()
                    .h(px(30.))
                    .flex_none()
                    .px_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(c.border)
                    .child(
                        tri_check(
                            "single-all",
                            all_state,
                            !busy && !shown_entries.is_empty(),
                            cx,
                        )
                        .tooltip(move |window, cx| ui::tip(all_tip.clone(), window, cx))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                cx.stop_propagation();
                                let entries = shown_entries.clone();
                                this.toggle_single(entries, window, cx);
                            },
                        )),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(c.muted)
                            .child(tr("更改")),
                    )
                    .child(crate::ui::count_pill(shown.len(), false, cx))
                    .child(div().flex_1().min_w(px(4.)))
                    .when(picked > 1, |bar| {
                        bar.child(
                            Button::new("single-picked-discard")
                                .xsmall()
                                .ghost()
                                .label(tr("丢弃…"))
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.discard_single_picked(window, cx)
                                })),
                        )
                    })
                    .when(picked <= 1, |bar| {
                        bar.child(
                            ui::clipped(
                                "single-staged-count",
                                tf!("已暂存 {n} 个文件", n = staged_files),
                            )
                            .flex_shrink(1.)
                            .text_xs()
                            .text_color(c.muted),
                        )
                    }),
            )
            .child(div().flex_1().min_h_0().child(body))
            .children(hidden_note)
            .into_any_element()
    }

    fn render_single_rows(
        &mut self,
        rows: &[ListRow],
        range: Range<usize>,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let selected = self.changes.selected.clone();
        let multi = self.changes.multi.len() > 1;
        let tree = self.app.read(cx).settings.file_tree;
        let busy = self.op.is_some();
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
                        let key = format!("a:{path}");
                        let cells = file_tree::dir_cells(
                            ("single-dir-name", row_ix),
                            label,
                            path,
                            *files,
                            *open,
                            cx,
                        );
                        return Some(
                            h_flex()
                                .id(("single-dir", row_ix))
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
                let entry = self.changes.single.get(ix)?.clone();
                let state = staged_of(&entry);
                // 写哪个字母：已暂存的那一份（相对 HEAD 是什么改动），没暂存的看工作区
                let kind = entry.staged.or(entry.unstaged)?;
                let is_primary = selected.as_ref().is_some_and(|(_, p)| *p == entry.path);
                let in_multi = multi && self.changes.multi.contains(&entry.path);
                let is_selected = is_primary || in_multi;
                let (dir, name) = match entry.path.rsplit_once('/') {
                    Some((dir, name)) => (dir.to_string(), name.to_string()),
                    None => (String::new(), entry.path.clone()),
                };
                let full: SharedString = match &entry.orig_path {
                    Some(orig) => format!("{orig} → {}", entry.path).into(),
                    None => entry.path.clone().into(),
                };
                let bare = dir.is_empty() && entry.orig_path.is_none();
                // 右键菜单：照右边看的那一份（部分暂存的按未暂存）
                let area = match entry.unstaged.is_some() {
                    true => Area::Unstaged,
                    false => Area::Staged,
                };
                let menu = self.file_menu(area, &entry, cx);
                let part = (state == Staged::Part)
                    .then(|| self.changes.numstat.get(&entry.path).copied())
                    .map(|counts| match counts {
                        Some((staged, rest)) if staged + rest > 0 => {
                            let total = staged + rest;
                            (
                                format!("{staged}/{total}"),
                                tf!(
                                    "已暂存 {staged} / {total} 行改动，点击复选框暂存其余的改动",
                                    staged,
                                    total
                                ),
                            )
                        }
                        _ => (
                            tr("部分").to_string(),
                            tr("部分改动已暂存，点击复选框暂存其余的改动").to_string(),
                        ),
                    });
                let check_tip: SharedString = match state {
                    Staged::All => tr("取消暂存此文件").into(),
                    Staged::Part => tr("暂存其余的改动").into(),
                    Staged::None if entry.unstaged == Some(Kind::Conflicted) => {
                        tr("标记为已解决").into()
                    }
                    Staged::None => tr("暂存此文件").into(),
                };
                let toggle = entry.clone();
                Some(
                    h_flex()
                        .id(("single-file", row_ix))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, _, window, cx| {
                                if !in_multi {
                                    this.click_single(ix, Modifiers::default(), window, cx);
                                }
                            }),
                        )
                        .h(px(FILE_ROW_HEIGHT))
                        .w_full()
                        .pl(indent(depth))
                        .pr_2()
                        .gap(px(if tree { 4. } else { 7. }))
                        .text_sm()
                        .cursor_pointer()
                        .when(is_selected, |d| d.bg(c.selected))
                        .when(!is_selected, |d| d.hover(|s| s.bg(c.hover)))
                        .border_l_2()
                        .border_color(if is_primary {
                            c.info
                        } else {
                            gpui_kit::transparent_black()
                        })
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            this.click_single(ix, event.modifiers(), window, cx);
                        }))
                        .child(
                            tri_check(("single-check", row_ix), state, !busy, cx)
                                .tooltip(move |window, cx| ui::tip(check_tip.clone(), window, cx))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    // 点在多选里的文件上：一起暂存；否则只管这一个
                                    let entries = match in_multi {
                                        true => this.single_picked(),
                                        false => vec![toggle.clone()],
                                    };
                                    this.toggle_single(entries, window, cx);
                                })),
                        )
                        .child(file_tree::kind_badge(kind.letter(), kind_color(kind, c)))
                        .child(match bare {
                            true => ui::clipped(("single-name", row_ix), name)
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
                                .min_w(px(if tree || dir.is_empty() { 0. } else { 30. }))
                                .truncate()
                                .text_xs()
                                .text_color(c.muted)
                                .when(!tree, |cell| cell.child(ui::one_line(dir))),
                        )
                        .when_some(part, |row, (text, tip)| {
                            let tip: SharedString = tip.into();
                            row.child(
                                div()
                                    .id(("single-part", row_ix))
                                    .flex_none()
                                    .px(px(5.))
                                    .rounded(px(7.))
                                    .bg(c.hunk)
                                    .text_size(px(10.5))
                                    .text_color(c.muted)
                                    .child(text)
                                    .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx)),
                            )
                        })
                        .when(!bare, |row| {
                            row.tooltip(move |window, cx| ui::tip(full.clone(), window, cx))
                        })
                        .context_menu(menu)
                        .into_any_element(),
                )
            })
            .collect()
    }

    /// 文件列表上面那一行的「分栏｜单列表」切换。列表窄时只有图标。
    pub(in crate::repo) fn render_view_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let single = self.single_list_on(cx);
        let narrow = self.sizes(cx).changes_list < 300. && !self.stacked_changes();
        let button = |id: &'static str, icon: IconName, label: &'static str, on: bool| {
            let button = Button::new(id).selected(on);
            match narrow {
                true => button.icon(ui::icon(icon)),
                false => button.label(label),
            }
        };
        ButtonGroup::new("changes-view")
            .outline()
            .xsmall()
            .child(
                button("changes-view-split", IconName::Rows2, tr("分栏"), !single)
                    .tooltip(tr("分栏：未暂存和已暂存的文件分开列出"))
                    .on_click(cx.listener(|this, _, _, cx| this.set_single_list(false, cx))),
            )
            .child(
                button(
                    "changes-view-single",
                    IconName::ListChecks,
                    tr("单列表"),
                    single,
                )
                .tooltip(tr("单列表：所有改动列在一起，勾选方框暂存"))
                .on_click(cx.listener(|this, _, _, cx| this.set_single_list(true, cx))),
            )
            .into_any_element()
    }
}

/// 三态的方框：全部暂存是勾，部分暂存是「—」，没暂存是空的。
fn tri_check(id: impl Into<ElementId>, state: Staged, enabled: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let (primary, border, fg) = (theme.primary, theme.input, theme.primary_foreground);
    let filled = state != Staged::None;
    div()
        .id(id)
        .flex_none()
        .size(px(14.))
        .rounded(px(3.))
        .border_1()
        .border_color(if filled { primary } else { border })
        .when(filled, |d| d.bg(primary))
        .flex()
        .items_center()
        .justify_center()
        .text_color(fg)
        .when(enabled, |d| {
            d.cursor_pointer().hover(|s| s.border_color(primary))
        })
        .when(!enabled, |d| d.opacity(0.5))
        .child(match state {
            Staged::All => Icon::new(IconName::Check).size(px(11.)).into_any_element(),
            Staged::Part => Icon::new(IconName::Minus).size(px(11.)).into_any_element(),
            Staged::None => div().into_any_element(),
        })
}

#[cfg(test)]
mod tests {
    use super::{Staged, parse_numstat, staged_of};
    use fr_core::status::{Entry, Kind};

    #[test]
    fn 已推送到哪些远程分支_不算默认分支的别名() {
        // `git branch -r --contains HEAD --format=%(refname:lstrip=2)` 的输出
        let out = b"origin/HEAD\norigin/main\nupstream/dev\n";
        assert_eq!(
            super::super::remote_names(out),
            vec!["origin/main".to_string(), "upstream/dev".to_string()]
        );
    }

    #[test]
    fn 读行数() {
        let out = b"3\t1\tsrc/a.rs\x00-\t-\timg.png\x002\t0\t\x00old.txt\x00new.txt\x00";
        let map = parse_numstat(out);
        assert_eq!(map.get("src/a.rs"), Some(&4));
        assert_eq!(map.get("img.png"), Some(&0));
        assert_eq!(map.get("new.txt"), Some(&2));
        assert_eq!(map.len(), 3);
        assert!(parse_numstat(b"").is_empty());
    }

    #[test]
    fn 暂存了多少() {
        let entry = |staged, unstaged| Entry {
            path: "a".into(),
            orig_path: None,
            staged,
            unstaged,
            submodule: false,
            index: None,
        };
        assert_eq!(staged_of(&entry(Some(Kind::Modified), None)), Staged::All);
        assert_eq!(
            staged_of(&entry(Some(Kind::Modified), Some(Kind::Modified))),
            Staged::Part
        );
        assert_eq!(staged_of(&entry(None, Some(Kind::Untracked))), Staged::None);
    }
}
