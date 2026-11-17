//! 在改动中搜索：本地修改、提交详情、比较三处的改动上方各有一行搜索条。
//!
//! Ctrl+F（或点改动上方的「搜索」）打开，输入就找，写「第几处 / 共几处」，Enter 下一处、
//! Shift+Enter 上一处，可以区分大小写，Esc 关掉。只搜显示出来的补丁里的行：本地修改、比较
//! 是选中的那个文件，提交详情是这个提交的全部改动。
//!
//! 本地修改的上下排视图是我们自己画的，匹配的字加底色；并排视图、提交详情、比较用的是
//! 组件库的改动视图，它只能给整行加底色，所以那里标的是匹配所在的行。

use std::ops::Range;

use fr_core::i18n::tr;
use fr_core::patch::{self, LineKind};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{
    DiffDecoration, DiffLinePosition, DiffLineRange, DiffSide, DiffState,
};
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, h_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::stage_diff::{Row, StageDiff};
use super::{Mode, RepoView};
use crate::ui;

/// 最多记这么多处匹配：再多也看不过来，省得一个字母搜出几十万处。
const MAX_HITS: usize = 5000;

/// 哪一处的改动。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Where {
    Changes,
    History,
    Compare,
}

/// 一处匹配。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Hit {
    /// 补丁里的第几个文件（提交详情一次显示好几个文件）
    file: usize,
    /// 在旧的一边（删掉的行）还是新的一边
    old: bool,
    /// 那一边的行号
    line: u32,
    /// 我们自己画的视图里是第几行
    row: usize,
    /// 在那一行文字里的字节范围
    range: Range<usize>,
}

pub(super) struct DiffSearch {
    input: Entity<InputState>,
    open: bool,
    /// 区分大小写
    case: bool,
    hits: Vec<Hit>,
    /// 现在停在第几处
    pos: Option<usize>,
}

impl DiffSearch {
    fn new(at: Where, window: &mut Window, cx: &mut Context<RepoView>) -> (Self, Subscription) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr("在改动中搜索…")));
        let events =
            cx.subscribe_in(
                &input,
                window,
                move |this, _, event: &InputEvent, _, cx| match event {
                    InputEvent::Change => this.update_diff_search(at, true, cx),
                    InputEvent::PressEnter { .. } => this.step_diff_search(at, 1, cx),
                    _ => {}
                },
            );
        let search = Self {
            input,
            open: false,
            case: false,
            hits: Vec::new(),
            pos: None,
        };
        (search, events)
    }

    /// 搜索条开着，而且键盘焦点在它的输入框里。
    pub(super) fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.open && self.input.focus_handle(cx).contains_focused(window, cx)
    }

    /// 我们自己画的视图里，第 `row` 行有哪些匹配（字节范围），其中现在停着的是哪一个。
    pub(super) fn marks(&self, row: usize) -> (Vec<Range<usize>>, Option<Range<usize>>) {
        if !self.open || self.hits.is_empty() {
            return (Vec::new(), None);
        }
        let start = self.hits.partition_point(|hit| hit.row < row);
        let in_row = self.hits[start..].iter().take_while(|hit| hit.row == row);
        let ranges: Vec<Range<usize>> = in_row.map(|hit| hit.range.clone()).collect();
        let current = self
            .pos
            .and_then(|pos| self.hits.get(pos))
            .filter(|hit| hit.row == row)
            .map(|hit| hit.range.clone());
        (ranges, current)
    }
}

/// 三处各一个搜索条。
pub(super) struct DiffSearches([DiffSearch; 3]);

impl DiffSearches {
    pub(super) fn new(
        window: &mut Window,
        cx: &mut Context<RepoView>,
    ) -> (Self, Vec<Subscription>) {
        let (changes, a) = DiffSearch::new(Where::Changes, window, cx);
        let (history, b) = DiffSearch::new(Where::History, window, cx);
        let (compare, c) = DiffSearch::new(Where::Compare, window, cx);
        (Self([changes, history, compare]), vec![a, b, c])
    }

    pub(super) fn at(&self, at: Where) -> &DiffSearch {
        &self.0[at as usize]
    }

    fn at_mut(&mut self, at: Where) -> &mut DiffSearch {
        &mut self.0[at as usize]
    }
}

/// 一行里所有的匹配（字节范围，不重叠）。不区分大小写时两边都按小写比。
fn occurrences(hay: &str, needle: &str, case: bool) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if needle.is_empty() || hay.len() < needle.len() {
        return out;
    }
    if case {
        out.extend(
            hay.match_indices(needle)
                .map(|(at, found)| at..at + found.len()),
        );
        return out;
    }
    let (lower_hay, lower_needle) = (hay.to_lowercase(), needle.to_lowercase());
    if lower_hay.len() == hay.len() {
        // 转小写以后长度没变（绝大多数文字都是），位置直接对得上
        out.extend(
            lower_hay
                .match_indices(&lower_needle)
                .filter(|(at, found)| {
                    hay.is_char_boundary(*at) && hay.is_char_boundary(at + found.len())
                })
                .map(|(at, found)| at..at + found.len()),
        );
        return out;
    }
    // 少数字母转小写后字节数会变：退一步，只把英文字母不分大小写
    let (hay_bytes, needle_bytes) = (hay.as_bytes(), needle.as_bytes());
    let mut at = 0;
    while at + needle_bytes.len() <= hay_bytes.len() {
        let end = at + needle_bytes.len();
        if hay.is_char_boundary(at)
            && hay.is_char_boundary(end)
            && hay_bytes[at..end].eq_ignore_ascii_case(needle_bytes)
        {
            out.push(at..end);
            at = end;
        } else {
            at += 1;
        }
    }
    out
}

/// 在一份补丁里找（提交详情、比较、并排视图）：返回匹配和补丁里的文件数。
fn patch_hits(text: &str, needle: &str, case: bool) -> (Vec<Hit>, usize) {
    let files = patch::parse(text.as_bytes());
    let mut hits = Vec::new();
    'files: for (file, diff) in files.iter().enumerate() {
        for line in diff.hunks.iter().flat_map(|hunk| &hunk.lines) {
            let old = line.kind == LineKind::Removed;
            let Some(number) = (if old { line.old_no } else { line.new_no }) else {
                continue;
            };
            let content = String::from_utf8_lossy(&line.content);
            for range in occurrences(&content, needle, case) {
                hits.push(Hit {
                    file,
                    old,
                    line: number,
                    row: 0,
                    range,
                });
                if hits.len() >= MAX_HITS {
                    break 'files;
                }
            }
        }
    }
    (hits, files.len())
}

/// 在本地修改的改动里找：按显示出来的文字找（制表符已经换成空格），字的位置才对得上。
fn stage_hits(diff: &StageDiff, needle: &str, case: bool) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (row, kind) in diff.rows.iter().enumerate() {
        let Row::Line(hunk, line) = *kind else {
            continue;
        };
        let (Some(line), Some(text)) = (
            diff.file
                .hunks
                .get(hunk)
                .and_then(|hunk| hunk.lines.get(line)),
            diff.texts.get(row),
        ) else {
            continue;
        };
        let old = line.kind == LineKind::Removed;
        let number = if old { line.old_no } else { line.new_no }.unwrap_or(0);
        for range in occurrences(text, needle, case) {
            hits.push(Hit {
                file: 0,
                old,
                line: number,
                row,
                range,
            });
            if hits.len() >= MAX_HITS {
                return hits;
            }
        }
    }
    hits
}

impl RepoView {
    /// Ctrl+F 按下时如果正在看改动，就在改动里搜；返回假表示不归这里管（去搜提交）。
    pub(super) fn find_in_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let at = match self.mode {
            Mode::Changes if self.changes_showing_diff() => Where::Changes,
            Mode::Compare => Where::Compare,
            // 提交历史里 Ctrl+F 平常是搜提交；焦点在改动里（点过它）时才是在改动里搜
            Mode::History if self.history_diff_focused(window, cx) => Where::History,
            _ => return false,
        };
        self.open_diff_search(at, window, cx);
        true
    }

    /// 打开搜索条，焦点放进输入框。
    pub(super) fn open_diff_search(
        &mut self,
        at: Where,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let search = self.diff_search.at_mut(at);
        search.open = true;
        search.input.focus_handle(cx).focus(window, cx);
        self.update_diff_search(at, false, cx);
        cx.notify();
    }

    /// 关掉搜索条，焦点还给改动。
    pub(super) fn close_diff_search(
        &mut self,
        at: Where,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let search = self.diff_search.at_mut(at);
        search.open = false;
        search.hits.clear();
        search.pos = None;
        self.focus_diff(at, window, cx);
        cx.notify();
    }

    /// 改动上方「搜索」按钮：开着就关，关着就开。
    pub(super) fn toggle_diff_search(
        &mut self,
        at: Where,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.diff_search.at(at).open {
            true => self.close_diff_search(at, window, cx),
            false => self.open_diff_search(at, window, cx),
        }
    }

    /// 按搜索框里的字重新找一遍（字变了、换了大小写、改动重读了）。`jump` 为真时跳到第一处。
    pub(super) fn update_diff_search(&mut self, at: Where, jump: bool, cx: &mut Context<Self>) {
        let search = self.diff_search.at(at);
        if !search.open {
            return;
        }
        let (needle, case) = (search.input.read(cx).value().to_string(), search.case);
        let hits = match (needle.is_empty(), at) {
            (true, _) => Vec::new(),
            (false, Where::Changes) => match self.changes_diff() {
                Some(diff) => stage_hits(diff, &needle, case),
                None => Vec::new(),
            },
            (false, Where::History | Where::Compare) => {
                let (hits, files) = patch_hits(self.diff_text(at), &needle, case);
                // 我们解析出来的文件数要和改动视图里的对得上，行才定位得到
                match files == self.diff_state(at).read(cx).files().len() {
                    true => hits,
                    false => Vec::new(),
                }
            }
        };
        let search = self.diff_search.at_mut(at);
        search.hits = hits;
        search.pos = match (jump, search.pos) {
            _ if search.hits.is_empty() => None,
            (true, _) => Some(0),
            (false, pos) => pos.map(|pos| pos.min(search.hits.len() - 1)),
        };
        if jump {
            self.reveal_hit(at, cx);
        }
        cx.notify();
    }

    /// 下一处（`step` 为 1）或上一处（-1），到头绕回去。
    pub(super) fn step_diff_search(&mut self, at: Where, step: isize, cx: &mut Context<Self>) {
        let search = self.diff_search.at_mut(at);
        let count = search.hits.len() as isize;
        if count == 0 {
            return;
        }
        search.pos = Some(match search.pos {
            Some(pos) => (pos as isize + step).rem_euclid(count) as usize,
            None if step < 0 => (count - 1) as usize,
            None => 0,
        });
        self.reveal_hit(at, cx);
        cx.notify();
    }

    /// 把现在停着的那一处滚到看得见的地方。
    fn reveal_hit(&mut self, at: Where, cx: &mut Context<Self>) {
        let search = self.diff_search.at(at);
        let Some(hit) = search.pos.and_then(|pos| search.hits.get(pos)).cloned() else {
            return;
        };
        if at == Where::Changes && !self.changes_split_showing(cx) {
            if let Some(diff) = self.changes_diff() {
                diff.scroll.scroll_to_item(hit.row, ScrollStrategy::Center);
            }
            return;
        }
        if at == Where::History {
            // 正开着一张图片：回到全部改动才看得到这一行
            self.close_commit_image();
        }
        let state = self.diff_state(at);
        let Some(position) =
            hit_position(&hit, state.read(cx).files().get(hit.file).map(|f| f.path()))
        else {
            return;
        };
        state.update(cx, |state, cx| state.scroll_to_line(position, cx));
    }

    /// 组件库的改动视图里给匹配所在的行加的底色。
    pub(super) fn search_decorations(&self, at: Where, cx: &App) -> Vec<DiffDecoration> {
        let search = self.diff_search.at(at);
        if !search.open || search.hits.is_empty() {
            return Vec::new();
        }
        let warning = cx.theme().warning;
        let state = self.diff_state(at);
        let files = state.read(cx).files();
        let mark = |hit: &Hit, strength: f32| {
            let path = files.get(hit.file)?.path().clone();
            let side = match hit.old {
                true => DiffSide::Original,
                false => DiffSide::Modified,
            };
            let line = hit.line as usize;
            Some(
                DiffDecoration::new(DiffLineRange::new(path, side, line, line))
                    .with_background(warning.opacity(strength)),
            )
        };
        let mut out: Vec<DiffDecoration> = Vec::new();
        let mut last: Option<(usize, bool, u32)> = None;
        // 同一行里有几处的只加一次
        for hit in &search.hits {
            let key = (hit.file, hit.old, hit.line);
            if last != Some(key) {
                last = Some(key);
                out.extend(mark(hit, 0.2));
            }
        }
        // 现在停着的那一行放在最后（后给的盖住先给的），颜色深一些
        let current = search.pos.and_then(|pos| search.hits.get(pos));
        out.extend(current.and_then(|hit| mark(hit, 0.45)));
        out
    }

    /// 那一处的改动在组件库的改动视图里用的是哪个状态。
    fn diff_state(&self, at: Where) -> Entity<DiffState> {
        match at {
            Where::Changes => self.changes_split_state(),
            Where::History => self.history_diff_state(),
            Where::Compare => self.compare_diff_state(),
        }
    }

    /// 那一处现在显示的补丁（本地修改的不走这里）。
    fn diff_text(&self, at: Where) -> &str {
        match at {
            Where::Changes => "",
            Where::History => self.history_diff_text(),
            Where::Compare => self.compare_diff_text(),
        }
    }

    fn focus_diff(&mut self, at: Where, window: &mut Window, cx: &mut Context<Self>) {
        match at {
            Where::Changes => self.focus_changes_diff(window, cx),
            Where::History | Where::Compare => {
                self.diff_state(at).focus_handle(cx).focus(window, cx)
            }
        }
    }

    /// 改动上方的搜索条；没打开时没有。
    pub(super) fn render_diff_search(
        &self,
        at: Where,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let search = self.diff_search.at(at);
        if !search.open {
            return None;
        }
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        let typed = !search.input.read(cx).value().is_empty();
        let count: SharedString = match (search.hits.len(), search.pos) {
            (0, _) if typed => tr("无匹配项").into(),
            (0, _) => SharedString::default(),
            (n, _) if n >= MAX_HITS => tf!("超过 {n} 处", n).into(),
            (n, Some(pos)) => format!("{} / {n}", pos + 1).into(),
            (n, None) => tf!("{n} 处", n).into(),
        };
        let scope = match at {
            Where::History => tr("搜索此提交的全部改动"),
            _ => tr("仅搜索当前显示的文件"),
        };
        let has_hits = !search.hits.is_empty();
        Some(
            h_flex()
                .h(px(34.))
                .flex_none()
                .pl(px(10.))
                .pr(px(8.))
                .gap(px(4.))
                .border_b_1()
                .border_color(border)
                // Esc 关掉，Shift+Enter 上一处：在输入框收到之前先拦下来
                .capture_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    let key = &event.keystroke;
                    match key.key.as_str() {
                        "escape" => this.close_diff_search(at, window, cx),
                        "enter" if key.modifiers.shift => this.step_diff_search(at, -1, cx),
                        _ => return,
                    }
                    cx.stop_propagation();
                }))
                .on_action(cx.listener(move |this, _: &Escape, window, cx| {
                    this.close_diff_search(at, window, cx)
                }))
                .child(
                    div().w(px(260.)).flex_shrink(1.).min_w(px(120.)).child(
                        Input::new(&search.input)
                            .small()
                            .cleanable(true)
                            .prefix(ui::icon(IconName::Search).text_color(muted)),
                    ),
                )
                .child(
                    div()
                        .flex_none()
                        .min_w(px(52.))
                        .px(px(6.))
                        .text_xs()
                        .text_color(muted)
                        .child(count),
                )
                .child(
                    Button::new(("diff-search-prev", at as usize))
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::ChevronUp))
                        .tooltip(tr("上一处（Shift+Enter）"))
                        .disabled(!has_hits)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.step_diff_search(at, -1, cx)),
                        ),
                )
                .child(
                    Button::new(("diff-search-next", at as usize))
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::ChevronDown))
                        .tooltip(tr("下一处（Enter）"))
                        .disabled(!has_hits)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.step_diff_search(at, 1, cx)),
                        ),
                )
                .child(
                    Button::new(("diff-search-case", at as usize))
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::CaseSensitive))
                        .selected(search.case)
                        .tooltip(tr("区分大小写"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let search = this.diff_search.at_mut(at);
                            search.case = !search.case;
                            this.update_diff_search(at, true, cx);
                        })),
                )
                .child(
                    ui::clipped(("diff-search-scope", at as usize), scope)
                        .flex_1()
                        .px(px(6.))
                        .text_xs()
                        .text_color(muted)
                        .text_right(),
                )
                .child(
                    Button::new(("diff-search-close", at as usize))
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::X))
                        .tooltip(tr("关闭（Esc）"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.close_diff_search(at, window, cx)
                        })),
                )
                .into_any_element(),
        )
    }

    /// 改动上方打开搜索条的按钮。
    pub(super) fn search_toggle(&self, at: Where, cx: &mut Context<Self>) -> Button {
        let tip = match at {
            // 提交历史里 Ctrl+F 是搜提交
            Where::History => tr("在改动中搜索"),
            _ => tr("在改动中搜索（Ctrl+F）"),
        };
        Button::new(("diff-search-toggle", at as usize))
            .xsmall()
            .ghost()
            .icon(ui::icon(IconName::Search))
            .selected(self.diff_search.at(at).open)
            .tooltip(tip)
            .on_click(
                cx.listener(move |this, _, window, cx| this.toggle_diff_search(at, window, cx)),
            )
    }
}

/// 一处匹配在组件库的改动视图里的位置。
fn hit_position(hit: &Hit, path: Option<&SharedString>) -> Option<DiffLinePosition> {
    let side = match hit.old {
        true => DiffSide::Original,
        false => DiffSide::Modified,
    };
    Some(DiffLinePosition::new(
        path?.clone(),
        side,
        hit.line as usize,
    ))
}

#[cfg(test)]
mod tests {
    use super::{occurrences, patch_hits};

    #[test]
    fn 一行里找出所有匹配() {
        assert_eq!(
            occurrences("let old = Old::new(old);", "old", true),
            [4..7, 19..22]
        );
        assert_eq!(
            occurrences("let old = Old::new(old);", "old", false),
            [4..7, 10..13, 19..22]
        );
        // 中文、和字节数不变的大小写
        assert_eq!(occurrences("忽略空白 Ignore", "空白", false), [6..12]);
        assert_eq!(occurrences("ÀB àb", "àb", false), [0..3, 4..7]);
        // 不重叠
        assert_eq!(occurrences("aaaa", "aa", true), [0..2, 2..4]);
        assert!(occurrences("abc", "", true).is_empty());
        assert!(occurrences("ab", "abc", false).is_empty());
        // 转小写后字节数会变的字（İ）在这一行里：英文照样不分大小写，位置不乱
        assert_eq!(
            occurrences("İstanbul OLD old", "old", false),
            [10..13, 14..17]
        );
    }

    #[test]
    fn 在补丁里找_记下在哪一边哪一行() {
        let patch = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -3,3 +3,3 @@\n let x = 1;\n-let old = 2;\n+let new = old;\n }\ndiff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -10,2 +10,3 @@\n fn b() {\n+    old();\n }\n";
        let (hits, files) = patch_hits(patch, "old", true);
        assert_eq!(files, 2);
        let at: Vec<(usize, bool, u32)> = hits.iter().map(|h| (h.file, h.old, h.line)).collect();
        // 删掉的行按旧的一边的行号，增加的、没变的按新的一边
        assert_eq!(at, [(0, true, 4), (0, false, 4), (1, false, 11)]);
        assert_eq!(hits[1].range, 10..13);
        assert!(patch_hits(patch, "nothing", false).0.is_empty());
    }
}
