//! 行内差异：一行被改成另一行时，标出具体改了哪几个词，不只是整行涂一种颜色。
//!
//! 一块改动里，连着的几行「删除」后面紧跟几行「增加」，就按顺序一一配对（第一行删除对第一行增加……），
//! 每一对按词比较。两行差得太多（共同的部分不到四分之一）就不标：到处都是标记等于没标。
//! 提交详情里的改动由组件库自己做这件事；这里是给本地修改里那个能按行暂存的视图用的。

use std::ops::Range;

use gpui_kit::{HighlightStyle, Hsla};
use similar::{Algorithm, DiffOp, capture_diff_slices};

/// 一行里要着重标出来的字节范围，从左到右、互不重叠。
pub(super) type Marks = Vec<Range<usize>>;

/// 一行超过这么多字节就不比了（压缩过的 js 之类）
const MAX_LINE: usize = 1000;
/// 两行共同的部分占不到这个比例就不标
const MIN_SIMILARITY: f32 = 0.25;

/// 比较配对的两行，返回旧的一行、新的一行里各自改了的范围。不值得标的时候两边都是空的。
pub(super) fn pair(old: &str, new: &str) -> (Marks, Marks) {
    if old == new || old.len() > MAX_LINE || new.len() > MAX_LINE {
        return (Marks::new(), Marks::new());
    }
    let (old_tokens, new_tokens) = (tokens(old), tokens(new));
    let old_words: Vec<&str> = old_tokens.iter().map(|range| &old[range.clone()]).collect();
    let new_words: Vec<&str> = new_tokens.iter().map(|range| &new[range.clone()]).collect();
    let ops = capture_diff_slices(Algorithm::Myers, &old_words, &new_words);

    let (mut old_marks, mut new_marks) = (Marks::new(), Marks::new());
    let mut same = 0usize;
    let span = |tokens: &[Range<usize>], index: usize, len: usize| -> Option<Range<usize>> {
        (len > 0).then(|| tokens[index].start..tokens[index + len - 1].end)
    };
    for op in ops {
        match op {
            DiffOp::Equal { old_index, len, .. } => {
                same += old_words[old_index..old_index + len]
                    .iter()
                    .filter(|word| !word.trim().is_empty())
                    .map(|word| word.chars().count())
                    .sum::<usize>();
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => old_marks.extend(span(&old_tokens, old_index, old_len)),
            DiffOp::Insert {
                new_index, new_len, ..
            } => new_marks.extend(span(&new_tokens, new_index, new_len)),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                old_marks.extend(span(&old_tokens, old_index, old_len));
                new_marks.extend(span(&new_tokens, new_index, new_len));
            }
        }
    }
    // 按字数算，不按字节：汉字一个占三个字节，按字节算会显得差别特别大
    let solid = |text: &str| text.chars().filter(|c| !c.is_whitespace()).count();
    let longest = solid(old).max(solid(new));
    if longest == 0 || (same as f32) < longest as f32 * MIN_SIMILARITY {
        return (Marks::new(), Marks::new());
    }
    (join(old, old_marks), join(new, new_marks))
}

/// 拆成词：一串字母数字下划线算一个词，一串空白算一个，别的字符（标点、汉字）一个字符一个。
fn tokens(text: &str) -> Vec<Range<usize>> {
    #[derive(PartialEq, Clone, Copy)]
    enum Kind {
        Word,
        Space,
        Other,
    }
    let kind_of = |c: char| {
        if c.is_ascii_alphanumeric() || c == '_' {
            Kind::Word
        } else if c.is_whitespace() {
            Kind::Space
        } else {
            Kind::Other
        }
    };
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut last: Option<Kind> = None;
    for (at, c) in text.char_indices() {
        let kind = kind_of(c);
        let end = at + c.len_utf8();
        match (last, out.last_mut()) {
            (Some(prev), Some(range)) if prev == kind && kind != Kind::Other => range.end = end,
            _ => out.push(at..end),
        }
        last = Some(kind);
    }
    out
}

/// 去掉只含空白的标记（缩进变了不值得着重标），再把挨着的、中间只隔着空白的标记连成一段。
fn join(text: &str, marks: Marks) -> Marks {
    let mut out: Marks = Vec::new();
    for mark in marks {
        // 两头的空白不算在标记里
        let part = &text[mark.clone()];
        let inner = part.trim();
        if inner.is_empty() {
            continue;
        }
        let start = mark.start + (part.len() - part.trim_start().len());
        let mark = start..start + inner.len();
        match out.last_mut() {
            Some(prev) if text[prev.end..mark.start].trim().is_empty() => prev.end = mark.end,
            _ => out.push(mark),
        }
    }
    out
}

/// 把语法高亮和行内差异的标记合成一份：标记范围里的字加上底色，语法的颜色照旧。
/// 两边都是从左到右、互不重叠的；结果也是。
pub(super) fn merge(
    syntax: &[(Range<usize>, HighlightStyle)],
    marks: &[Range<usize>],
    background: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if marks.is_empty() {
        return syntax.to_vec();
    }
    let mut cuts: Vec<usize> = syntax
        .iter()
        .flat_map(|(range, _)| [range.start, range.end])
        .chain(marks.iter().flat_map(|range| [range.start, range.end]))
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    let (mut s, mut m) = (0, 0);
    let mut out: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for pair in cuts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        while s < syntax.len() && syntax[s].0.end <= start {
            s += 1;
        }
        while m < marks.len() && marks[m].end <= start {
            m += 1;
        }
        let style = syntax
            .get(s)
            .filter(|(range, _)| range.start <= start)
            .map(|(_, style)| *style);
        let marked = marks.get(m).is_some_and(|range| range.start <= start);
        if style.is_none() && !marked {
            continue;
        }
        let mut style = style.unwrap_or_default();
        if marked {
            style.background_color = Some(background);
        }
        // 样式一样又挨着的并成一段，少画几段
        match out.last_mut() {
            Some((prev, prev_style)) if prev.end == start && *prev_style == style => prev.end = end,
            _ => out.push((start..end, style)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked<'a>(text: &'a str, marks: &Marks) -> Vec<&'a str> {
        marks.iter().map(|range| &text[range.clone()]).collect()
    }

    #[test]
    fn 标出改了的词() {
        let (old, new) = (
            "    let count = items.len();",
            "    let total = items.len() + 1;",
        );
        let (a, b) = pair(old, new);
        assert_eq!(marked(old, &a), ["count"]);
        assert_eq!(marked(new, &b), ["total", "+ 1"]);
        // 汉字一个字一个字地比
        let (old, new) = ("// 读取配置文件", "// 读取设置文件");
        let (a, b) = pair(old, new);
        assert_eq!(marked(old, &a), ["配"]);
        assert_eq!(marked(new, &b), ["设"]);
        // 行尾加了一段带汉字的注释：相似度按字数算，不会因为汉字占的字节多就不标
        let (old, new) = ("    a + b", "    a + b + 0 // 加法");
        let (a, b) = pair(old, new);
        assert!(a.is_empty());
        assert_eq!(marked(new, &b), ["+ 0 // 加法"]);
    }

    #[test]
    fn 不值得标的不标() {
        // 一样的、差得太多的、只动了缩进的
        assert_eq!(pair("same", "same"), (vec![], vec![]));
        assert_eq!(
            pair("fn alpha() {}", "struct Totally { different: u32 }"),
            (vec![], vec![])
        );
        assert_eq!(pair("  x = 1", "      x = 1"), (vec![], vec![]));
        // 太长的不比
        let long = "a ".repeat(MAX_LINE);
        assert_eq!(pair(&long, "a b"), (vec![], vec![]));
    }

    #[test]
    fn 挨着的标记连成一段() {
        let (old, new) = ("return a + b;", "return left * right;");
        let (a, b) = pair(old, new);
        assert_eq!(marked(old, &a), ["a + b"]);
        assert_eq!(marked(new, &b), ["left * right"]);
    }

    #[test]
    fn 语法高亮和标记合在一起() {
        let color = |h: f32| HighlightStyle {
            color: Some(gpui_kit::hsla(h, 1., 0.5, 1.)),
            ..Default::default()
        };
        let bg = gpui_kit::hsla(0.3, 1., 0.5, 0.4);
        // 文字：0..10；语法：2..6 一种颜色，8..10 另一种；标记：4..9
        let syntax = vec![(2..6, color(0.1)), (8..10, color(0.6))];
        let merged = merge(&syntax, std::slice::from_ref(&(4..9)), bg);
        let ranges: Vec<Range<usize>> = merged.iter().map(|(range, _)| range.clone()).collect();
        assert_eq!(ranges, [2..4, 4..6, 6..8, 8..9, 9..10]);
        let has_bg: Vec<bool> = merged
            .iter()
            .map(|(_, style)| style.background_color == Some(bg))
            .collect();
        assert_eq!(has_bg, [false, true, true, true, false]);
        // 语法的颜色还在
        assert_eq!(merged[1].1.color, color(0.1).color);
        assert_eq!(merged[2].1.color, None);
        assert_eq!(merged[3].1.color, color(0.6).color);
        // 没有标记时原样返回
        assert_eq!(merge(&syntax, &[], bg), syntax);
        // 没有语法高亮时只有标记
        assert_eq!(merge(&[], std::slice::from_ref(&(1..3)), bg).len(), 1);
    }
}
