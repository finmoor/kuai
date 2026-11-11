//! 暂存用的 diff 视图的数据：每行显示什么、选中了哪些行、按选择生成部分补丁。
//!
//! 选行和 Fork 一样：单击选一行，Shift 加单击选一段，Ctrl 加单击增减一行，按住拖动选一段，
//! 双击选中整块。只有增加、删除的行能选，上下文行不能选。

use fr_core::i18n::tr;
use std::collections::HashSet;
use std::ops::Range;

use fr_core::patch::{self, FileDiff, LineKind};
use fr_core::status::Entry;
use gpui_kit::component::Rope;
use gpui_kit::component::diff::{DiffLineRange, DiffSide};
use gpui_kit::component::highlighter::{HighlightTheme, SyntaxHighlighter};
use gpui_kit::{HighlightStyle, SharedString, UniformListScrollHandle};

use super::changes::Area;
use super::inline_diff::{self, Marks};

/// 一行最多显示这么多字符，超长的行（比如压缩过的 js）后面截掉；生成补丁时用的是原样字节，不受影响
const MAX_CHARS: usize = 4000;

/// 代码行加起来超过这么多字节就不做语法高亮（高亮在界面线程上算）
const MAX_HIGHLIGHT_BYTES: usize = 400_000;

/// 最多给这么多对行算行内差异（也在界面线程上算）
const MAX_INLINE_PAIRS: usize = 2000;

/// 一行里的语法高亮：字节范围和样式
pub(super) type LineHighlights = Vec<(Range<usize>, HighlightStyle)>;

/// 头部里值得显示的行
const SHOWN_HEADERS: [&[u8]; 10] = [
    b"old mode",
    b"new mode",
    b"new file mode",
    b"deleted file mode",
    b"rename from",
    b"rename to",
    b"copy from",
    b"copy to",
    b"similarity index",
    b"Binary files",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Row {
    /// 头部信息（改名、权限、二进制）
    Meta,
    Hunk(usize),
    /// 块下标、行下标
    Line(usize, usize),
}

pub(super) struct StageDiff {
    pub(super) area: Area,
    pub(super) entry: Entry,
    pub(super) file: FileDiff,
    pub(super) rows: Vec<Row>,
    /// 每行显示的文字
    pub(super) texts: Vec<SharedString>,
    /// 每行的语法高亮；没有高亮时是空的
    pub(super) highlights: Vec<LineHighlights>,
    /// 高亮是按深色还是浅色主题算的；还没算过时为空
    pub(super) highlighted_dark: Option<bool>,
    /// 每行里相对配对的那一行改了的部分（行内差异）；没有时是空的
    pub(super) inline: Vec<Marks>,
    pub(super) selected: Vec<bool>,
    anchor: Option<usize>,
    /// 键盘上下键现在停在哪一行
    cursor: Option<usize>,
    pub(super) dragging: bool,
    last_drag: Option<usize>,
    /// 最长的一行，列表按它定宽度，长行可以横向滚动
    pub(super) widest: Option<usize>,
    pub(super) scroll: UniformListScrollHandle,
    /// 不能按块、按行操作的原因
    pub(super) notice: Option<SharedString>,
    pub(super) error: Option<SharedString>,
    /// git 给的原始 diff，重读时拿来比较有没有变
    source: Vec<u8>,
}

impl StageDiff {
    pub(super) fn new(
        area: Area,
        entry: Entry,
        out: &[u8],
        scroll: UniformListScrollHandle,
    ) -> Self {
        let mut files = patch::parse(out);
        let file = if files.is_empty() {
            FileDiff::default()
        } else {
            files.swap_remove(0)
        };
        let mut rows = Vec::new();
        let mut texts = Vec::new();
        if file.is_combined() {
            // 冲突时的组合 diff 不能按行操作，原样显示
            for line in out.split(|&b| b == b'\n').take(20_000) {
                rows.push(Row::Meta);
                texts.push(display(line));
            }
        } else {
            for line in &file.header {
                if SHOWN_HEADERS.iter().any(|p| line.starts_with(p)) {
                    rows.push(Row::Meta);
                    texts.push(display(line));
                }
            }
            for (hi, hunk) in file.hunks.iter().enumerate() {
                rows.push(Row::Hunk(hi));
                texts.push(hunk.title().into());
                for (li, line) in hunk.lines.iter().enumerate() {
                    rows.push(Row::Line(hi, li));
                    texts.push(display(&line.content));
                }
            }
        }
        let notice: Option<SharedString> = if file.is_combined() {
            Some(tr("有冲突：点「解决冲突…」逐处选；在别处改好的，点「标记为已解决」").into())
        } else if file.is_binary() {
            Some(tr("二进制文件，只能整个文件操作").into())
        } else if file.hunks.is_empty() {
            Some(if out.is_empty() {
                tr("没有内容改动").into()
            } else {
                tr("没有文本改动（比如只改了权限），只能整个文件操作").into()
            })
        } else {
            None
        };
        let widest = texts
            .iter()
            .enumerate()
            .max_by_key(|(_, t)| t.len())
            .map(|(i, _)| i);
        let selected = vec![false; rows.len()];
        let inline = if file.is_combined() {
            Vec::new()
        } else {
            inline_marks(&file, &rows, &texts)
        };
        Self {
            area,
            entry,
            file,
            rows,
            texts,
            highlights: Vec::new(),
            highlighted_dark: None,
            inline,
            selected,
            anchor: None,
            cursor: None,
            dragging: false,
            last_drag: None,
            widest,
            scroll,
            notice,
            error: None,
            source: out.to_vec(),
        }
    }

    /// git 给的原始 diff（并排着看时交给组件库再解析一遍）。
    pub(super) fn source(&self) -> &[u8] {
        &self.source
    }

    /// 并排着看时，组件库那边选中了一段行：把这段里的改动行标成选中，后面暂存、丢弃用的还是这份标记。
    /// 同一边的范围只算那一边的行（左边是删除的行，右边是增加的行）；从一边拖到另一边的，
    /// 按改动在补丁里的先后算中间的全部改动行。
    pub(super) fn select_lines(&mut self, range: Option<&DiffLineRange>) {
        self.clear();
        self.anchor = None;
        self.cursor = None;
        let Some(range) = range else {
            return;
        };
        let number = |row: &Row, side: DiffSide| match row {
            Row::Line(h, l) => {
                let line = &self.file.hunks[*h].lines[*l];
                match side {
                    DiffSide::Original => line.old_no,
                    DiffSide::Modified => line.new_no,
                }
            }
            _ => None,
        };
        if range.side() == range.end_side() {
            let side = range.side();
            let (lo, hi) = (
                range.start().min(range.end()) as u32,
                range.start().max(range.end()) as u32,
            );
            let wanted = match side {
                DiffSide::Original => LineKind::Removed,
                DiffSide::Modified => LineKind::Added,
            };
            for ix in 0..self.rows.len() {
                let Row::Line(h, l) = self.rows[ix] else {
                    continue;
                };
                let hit = number(&self.rows[ix], side).is_some_and(|n| (lo..=hi).contains(&n));
                if hit && self.file.hunks[h].lines[l].kind == wanted {
                    self.selected[ix] = true;
                }
            }
            return;
        }
        let find = |side: DiffSide, line: usize| {
            self.rows
                .iter()
                .position(|row| number(row, side) == Some(line as u32))
        };
        if let (Some(a), Some(b)) = (
            find(range.side(), range.start()),
            find(range.end_side(), range.end()),
        ) {
            self.select_range(a, b);
        }
    }

    /// 这个 diff 是不是就是 `out` 解析出来的。
    pub(super) fn same_source(&self, area: Area, path: &str, out: &[u8]) -> bool {
        self.error.is_none() && self.area == area && self.entry.path == path && self.source == out
    }

    pub(super) fn failed(
        area: Area,
        entry: Entry,
        error: SharedString,
        scroll: UniformListScrollHandle,
    ) -> Self {
        Self {
            area,
            entry,
            file: FileDiff::default(),
            rows: Vec::new(),
            texts: Vec::new(),
            highlights: Vec::new(),
            highlighted_dark: None,
            inline: Vec::new(),
            selected: Vec::new(),
            anchor: None,
            cursor: None,
            dragging: false,
            last_drag: None,
            widest: None,
            scroll,
            notice: None,
            error: Some(error),
            source: Vec::new(),
        }
    }

    /// 给代码行加语法高亮。按文件扩展名认语言，不认识的不高亮。
    ///
    /// 把所有代码行（新旧两边混在一起）接成一段文字交给高亮器，再按行拆开。
    /// diff 只是文件的片段，语法树不完整，但 tree-sitter 能容错，关键字、字符串、注释都认得出来。
    pub(super) fn highlight(&mut self, theme: &HighlightTheme, dark: bool) {
        self.highlighted_dark = Some(dark);
        let Some(language) = language_for(&self.entry.path) else {
            return;
        };
        if !self.editable() {
            return;
        }
        let mut text = String::new();
        let mut spans: Vec<(usize, Range<usize>)> = Vec::new();
        for (ix, row) in self.rows.iter().enumerate() {
            if let Row::Line(..) = row {
                let start = text.len();
                text.push_str(&self.texts[ix]);
                spans.push((ix, start..text.len()));
                text.push('\n');
            }
        }
        if text.len() > MAX_HIGHLIGHT_BYTES {
            return;
        }
        let mut highlighter = SyntaxHighlighter::new(language);
        highlighter.update(None, &Rope::from_str(&text), None);
        let styles = highlighter.styles(&(0..text.len()), theme);

        let mut highlights = vec![LineHighlights::new(); self.rows.len()];
        let mut first = 0;
        for (row, span) in spans {
            while first < styles.len() && styles[first].0.end <= span.start {
                first += 1;
            }
            let line = &self.texts[row];
            let mut covered = 0;
            for (range, style) in styles[first..]
                .iter()
                .take_while(|(range, _)| range.start < span.end)
            {
                let start = range.start.max(span.start) - span.start;
                let end = range.end.min(span.end) - span.start;
                // 范围要一个接一个、落在字符边界上，不然画文字时会出错
                if start >= covered
                    && start < end
                    && line.is_char_boundary(start)
                    && line.is_char_boundary(end)
                {
                    highlights[row].push((start..end, *style));
                    covered = end;
                }
            }
        }
        self.highlights = highlights;
    }

    /// 能不能按块、按行操作。
    pub(super) fn editable(&self) -> bool {
        !self.file.hunks.is_empty() && !self.file.is_binary() && !self.file.is_combined()
    }

    fn selectable(&self, row: usize) -> bool {
        match self.rows.get(row) {
            Some(Row::Line(h, l)) => self.file.hunks[*h].lines[*l].kind != LineKind::Context,
            _ => false,
        }
    }

    /// 鼠标按下：单击选一行，Shift 选一段，Ctrl 增减一行，双击选整块。
    pub(super) fn press(&mut self, row: usize, shift: bool, ctrl: bool, clicks: usize) {
        if !self.editable() || row >= self.rows.len() {
            return;
        }
        if clicks >= 2 {
            if let Row::Line(h, _) | Row::Hunk(h) = self.rows[row] {
                self.select_hunk(h);
            }
            self.dragging = false;
            return;
        }
        match self.anchor {
            Some(anchor) if shift => {
                if !ctrl {
                    self.clear();
                }
                self.select_range(anchor, row);
            }
            _ if ctrl => {
                if self.selectable(row) {
                    self.selected[row] = !self.selected[row];
                }
                self.anchor = Some(row);
            }
            _ => {
                self.clear();
                if self.selectable(row) {
                    self.selected[row] = true;
                }
                self.anchor = Some(row);
            }
        }
        self.dragging = true;
        self.last_drag = Some(row);
        self.cursor = Some(row);
    }

    /// 键盘上下键：挪到上一个、下一个改动行并选中它；`extend`（按着 Shift）时从起点连选到这里。
    /// 返回挪到的行，用来滚动；已经到头返回 None。
    pub(super) fn step(&mut self, down: bool, extend: bool) -> Option<usize> {
        if !self.editable() {
            return None;
        }
        let rows = self.rows.len();
        let next = match (self.cursor.or(self.anchor), down) {
            (None, true) => (0..rows).find(|row| self.selectable(*row)),
            (None, false) => (0..rows).rev().find(|row| self.selectable(*row)),
            (Some(at), true) => (at + 1..rows).find(|row| self.selectable(*row)),
            (Some(at), false) => (0..at.min(rows)).rev().find(|row| self.selectable(*row)),
        }?;
        self.clear();
        match self.anchor {
            Some(anchor) if extend => self.select_range(anchor, next),
            _ => {
                self.selected[next] = true;
                self.anchor = Some(next);
            }
        }
        self.cursor = Some(next);
        Some(next)
    }

    /// 按住拖到某一行：选中从起点到这一行。选择变了返回 true。
    pub(super) fn drag_to(&mut self, row: usize) -> bool {
        if self.last_drag == Some(row) || row >= self.rows.len() {
            return false;
        }
        self.last_drag = Some(row);
        let Some(anchor) = self.anchor else {
            return false;
        };
        self.clear();
        self.select_range(anchor, row);
        true
    }

    fn select_range(&mut self, a: usize, b: usize) {
        for row in a.min(b)..=a.max(b) {
            if self.selectable(row) {
                self.selected[row] = true;
            }
        }
    }

    fn select_hunk(&mut self, hunk: usize) {
        self.clear();
        for row in 0..self.rows.len() {
            if matches!(self.rows[row], Row::Line(h, _) if h == hunk) && self.selectable(row) {
                self.selected[row] = true;
            }
        }
    }

    fn clear(&mut self) {
        self.selected.fill(false);
    }

    pub(super) fn selected_count(&self) -> usize {
        self.selected.iter().filter(|s| **s).count()
    }

    pub(super) fn hunk_changed_count(&self, hunk: usize) -> usize {
        self.file.hunks.get(hunk).map_or(0, |h| {
            h.lines
                .iter()
                .filter(|l| l.kind != LineKind::Context)
                .count()
        })
    }

    /// 按选择生成补丁：给了 `hunk` 就是整块，否则是选中的行。没选中改动时返回 None。
    pub(super) fn patch(&self, reverse: bool, hunk: Option<usize>) -> Option<Vec<u8>> {
        match hunk {
            Some(h) => self.file.partial(reverse, |hi, _| hi == h),
            None => {
                let picked: HashSet<(usize, usize)> = self
                    .rows
                    .iter()
                    .zip(&self.selected)
                    .filter_map(|(row, selected)| match (row, selected) {
                        (Row::Line(h, l), true) => Some((*h, *l)),
                        _ => None,
                    })
                    .collect();
                self.file.partial(reverse, |h, l| picked.contains(&(h, l)))
            }
        }
    }
}

/// 给每一行算行内差异。一块里连着的删除行后面紧跟着增加行，就按顺序一一配对来比。
fn inline_marks(file: &FileDiff, rows: &[Row], texts: &[SharedString]) -> Vec<Marks> {
    let mut marks = vec![Marks::new(); rows.len()];
    // 块标题在第几行：块里第 l 行就在它后面第 l + 1 行
    let mut starts = vec![0usize; file.hunks.len()];
    for (ix, row) in rows.iter().enumerate() {
        if let Row::Hunk(h) = row {
            starts[*h] = ix;
        }
    }
    let mut budget = MAX_INLINE_PAIRS;
    for (h, hunk) in file.hunks.iter().enumerate() {
        let lines = &hunk.lines;
        let mut at = 0;
        while at < lines.len() {
            if lines[at].kind != LineKind::Removed {
                at += 1;
                continue;
            }
            let removed = lines[at..]
                .iter()
                .take_while(|line| line.kind == LineKind::Removed)
                .count();
            let added = lines[at + removed..]
                .iter()
                .take_while(|line| line.kind == LineKind::Added)
                .count();
            for k in 0..removed.min(added) {
                if budget == 0 {
                    return marks;
                }
                budget -= 1;
                let (old_row, new_row) = (starts[h] + 1 + at + k, starts[h] + 1 + at + removed + k);
                let (old, new) = inline_diff::pair(&texts[old_row], &texts[new_row]);
                marks[old_row] = old;
                marks[new_row] = new;
            }
            at += removed + added;
        }
    }
    marks
}

/// 按文件扩展名（或文件名）认语言，返回高亮器认识的名字。
fn language_for(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let by_name = match name.as_str() {
        "makefile" | "gnumakefile" => Some("make"),
        "cmakelists.txt" => Some("cmake"),
        "dockerfile" | ".bashrc" | ".zshrc" | ".profile" => Some("bash"),
        "cargo.lock" => Some("toml"),
        _ => None,
    };
    if by_name.is_some() {
        return by_name;
    }
    let (_, extension) = name.rsplit_once('.')?;
    Some(match extension {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "cs" => "csharp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "scala" => "scala",
        "lua" => "lua",
        "zig" => "zig",
        "ex" | "exs" => "elixir",
        "sh" | "bash" | "zsh" => "bash",
        "sql" => "sql",
        "json" | "jsonc" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "md" | "mdx" | "markdown" => "markdown",
        "html" | "htm" | "vue" => "html",
        "css" | "scss" => "css",
        "svelte" => "svelte",
        "proto" => "proto",
        "graphql" | "gql" => "graphql",
        "cmake" => "cmake",
        "mk" => "make",
        _ => return None,
    })
}

/// 显示用的文字：不是 UTF-8 的字节显示成替换字符，Tab 换成 4 个空格，去掉行尾的 `\r`。
fn display(bytes: &[u8]) -> SharedString {
    let text = crate::text::decode(bytes);
    let text = text.strip_suffix('\r').unwrap_or(&text);
    let mut out = String::with_capacity(text.len());
    for (i, ch) in text.chars().enumerate() {
        if i >= MAX_CHARS {
            out.push('…');
            break;
        }
        if ch == '\t' {
            out.push_str("    ");
        } else {
            out.push(ch);
        }
    }
    out.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,3 @@
 1
-2
+two
 3
@@ -10,2 +10,3 @@
 10
+x
 11
";

    fn view() -> StageDiff {
        let entry = Entry {
            path: "a.txt".into(),
            orig_path: None,
            staged: None,
            unstaged: Some(fr_core::status::Kind::Modified),
            submodule: false,
        };
        StageDiff::new(
            Area::Unstaged,
            entry,
            DIFF.as_bytes(),
            UniformListScrollHandle::new(),
        )
    }

    #[test]
    fn 行的排列和可选的行() {
        let v = view();
        // 块标题 + 4 行，块标题 + 3 行
        assert_eq!(v.rows.len(), 9);
        assert_eq!(v.rows[0], Row::Hunk(0));
        assert_eq!(&*v.texts[2], "2");
        assert!(!v.selectable(1));
        assert!(v.selectable(2) && v.selectable(3));
        assert!(v.editable());
    }

    #[test]
    fn 单击shift和ctrl选行() {
        let mut v = view();
        v.press(2, false, false, 1);
        assert_eq!(v.selected_count(), 1);
        // Shift 选到第二块的 +x：中间的上下文行和块标题不算
        v.press(7, true, false, 1);
        assert_eq!(v.selected_count(), 3);
        // Ctrl 去掉 +two
        v.press(3, false, true, 1);
        assert_eq!(v.selected_count(), 2);
        let patch = String::from_utf8(v.patch(false, None).unwrap()).unwrap();
        // 暂存时没选中的 +two 去掉（暂存区里本来没有它），不是变成上下文
        assert!(patch.contains(" 1\n-2\n 3\n"), "{patch}");
        assert!(patch.contains("+x\n"));
    }

    #[test]
    fn 上下键在改动行之间走() {
        let mut diff = view();
        let first = diff.step(true, false).unwrap();
        assert!(diff.selectable(first));
        assert_eq!(diff.selected_count(), 1);
        let second = diff.step(true, false).unwrap();
        assert!(second > first && diff.selected[second] && !diff.selected[first]);
        // 按着 Shift 往回走：从起点连选
        let back = diff.step(false, true).unwrap();
        assert_eq!(back, first);
        assert_eq!(diff.selected_count(), 2);
        // 到头了不动
        assert_eq!(diff.step(false, false), None);
    }

    #[test]
    fn 配对的行有行内差异() {
        let v = view();
        // 第 2 行是 -2，第 3 行是 +two：差得太多，不标
        assert!(v.inline[2].is_empty() && v.inline[3].is_empty());
        let diff = "diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,4 +1,4 @@
 fn main() {
-    let count = items.len();
-    print(count);
+    let total = items.len();
+    print(total);
 }
";
        let entry = Entry {
            path: "a.rs".into(),
            orig_path: None,
            staged: None,
            unstaged: Some(fr_core::status::Kind::Modified),
            submodule: false,
        };
        let v = StageDiff::new(
            Area::Unstaged,
            entry,
            diff.as_bytes(),
            UniformListScrollHandle::new(),
        );
        // 行：块标题、上下文、两行删除、两行增加、上下文
        let marked = |row: usize| -> Vec<&str> {
            v.inline[row]
                .iter()
                .map(|range| &v.texts[row][range.clone()])
                .collect()
        };
        assert_eq!(marked(2), ["count"]);
        assert_eq!(marked(3), ["count"]);
        assert_eq!(marked(4), ["total"]);
        assert_eq!(marked(5), ["total"]);
        assert!(v.inline[1].is_empty() && v.inline[6].is_empty());
    }

    #[test]
    fn 并排时按行号选中改动行() {
        let mut v = view();
        // 左边（旧的）第 1 到 3 行：只有删除的那一行（-2）算
        v.select_lines(Some(&DiffLineRange::new("a.txt", DiffSide::Original, 1, 3)));
        assert_eq!(v.selected_count(), 1);
        assert!(v.selected[2]);
        // 右边（新的）第 11 到 2 行，倒着拖也行：+two 和 +x
        v.select_lines(Some(&DiffLineRange::new(
            "a.txt",
            DiffSide::Modified,
            11,
            2,
        )));
        assert_eq!(v.selected_count(), 2);
        assert!(v.selected[3] && v.selected[7]);
        // 从左边第 2 行拖到右边第 2 行：-2 和 +two
        let across =
            DiffLineRange::new("a.txt", DiffSide::Original, 2, 2).with_end_side(DiffSide::Modified);
        v.select_lines(Some(&across));
        assert_eq!(v.selected_count(), 2);
        assert!(v.selected[2] && v.selected[3]);
        v.select_lines(None);
        assert_eq!(v.selected_count(), 0);
    }

    #[test]
    fn 拖动和双击() {
        let mut v = view();
        v.press(2, false, false, 1);
        assert!(v.drag_to(3));
        assert!(!v.drag_to(3));
        assert_eq!(v.selected_count(), 2);
        v.press(7, false, false, 2);
        assert_eq!(v.selected_count(), 1);
        assert!(!v.dragging);
    }
}
