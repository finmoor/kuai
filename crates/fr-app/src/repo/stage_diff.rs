//! 暂存用的 diff 视图的数据：每行显示什么、选中了哪些行、按选择生成部分补丁。
//!
//! 选行和 Fork 一样：单击选一行，Shift 加单击选一段，Ctrl 加单击增减一行，按住拖动选一段，
//! 双击选中整块。只有增加、删除的行能选，上下文行不能选。

use std::collections::HashSet;

use fr_core::patch::{self, FileDiff, LineKind};
use fr_core::status::Entry;
use gpui_kit::{SharedString, UniformListScrollHandle};

use super::changes::Area;

/// 一行最多显示这么多字符，超长的行（比如压缩过的 js）后面截掉；生成补丁时用的是原样字节，不受影响
const MAX_CHARS: usize = 4000;

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
    pub(super) selected: Vec<bool>,
    anchor: Option<usize>,
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
            Some("这个文件有冲突：在编辑器里解决后，点「标记为已解决」暂存它".into())
        } else if file.is_binary() {
            Some("二进制文件，只能整个文件操作".into())
        } else if file.hunks.is_empty() {
            Some(if out.is_empty() {
                "没有内容改动".into()
            } else {
                "没有文本改动（比如只改了权限），只能整个文件操作".into()
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
        Self {
            area,
            entry,
            file,
            rows,
            texts,
            selected,
            anchor: None,
            dragging: false,
            last_drag: None,
            widest,
            scroll,
            notice,
            error: None,
            source: out.to_vec(),
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
            selected: Vec::new(),
            anchor: None,
            dragging: false,
            last_drag: None,
            widest: None,
            scroll,
            notice: None,
            error: Some(error),
            source: Vec::new(),
        }
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

/// 显示用的文字：不是 UTF-8 的字节显示成替换字符，Tab 换成 4 个空格，去掉行尾的 `\r`。
fn display(bytes: &[u8]) -> SharedString {
    let text = String::from_utf8_lossy(bytes);
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
