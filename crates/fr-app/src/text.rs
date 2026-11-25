//! 把字节变成能显示的文字：文件内容解码，字节数写成人看的大小。
//!
//! 仓库里的文件不一定是 UTF-8：国内的老项目常见 GBK。是合法的 UTF-8 就按 UTF-8，
//! 不是就按 GB18030（GBK 的超集）解，再不行的字节显示成替换符号。只影响显示，
//! 生成补丁用的始终是原始字节。

use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 显示代码时一个 Tab 占几格（设置里的「Tab 宽度」，见 `workspace.rs` 的 `apply_settings`）
static TAB_WIDTH: AtomicUsize = AtomicUsize::new(4);

/// 现在的 Tab 宽度。
pub fn tab_width() -> usize {
    TAB_WIDTH.load(Ordering::Relaxed).max(1)
}

/// 换 Tab 宽度（设置变了时）。
pub fn set_tab_width(width: usize) {
    TAB_WIDTH.store(width.clamp(1, 16), Ordering::Relaxed);
}

/// 一行里的 Tab 换成空格，对齐到 `width` 的整数倍（照列算，不是一律换几个）。没有 Tab 原样返回。
pub fn expand_tabs(line: &str, width: usize) -> Cow<'_, str> {
    if !line.contains('\t') {
        return Cow::Borrowed(line);
    }
    let width = width.max(1);
    let mut out = String::with_capacity(line.len() + width * 2);
    let mut column = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let pad = width - column % width;
            out.extend(std::iter::repeat_n(' ', pad));
            column += pad;
        } else {
            out.push(ch);
            column += 1;
        }
    }
    Cow::Owned(out)
}

/// 补丁里代码行的 Tab 换成空格（只换块里的内容，行首的 `+`、`-`、空格不算一列），只用来显示：
/// 组件库的改动视图照字显示 Tab，宽窄不由我们定。没有 Tab 原样返回。
pub fn expand_patch_tabs(patch: &str) -> Cow<'_, str> {
    if !patch.contains('\t') {
        return Cow::Borrowed(patch);
    }
    let width = tab_width();
    let mut out = String::with_capacity(patch.len() + patch.len() / 8);
    let mut in_hunk = false;
    for line in patch.split_inclusive('\n') {
        if line.starts_with("diff ") {
            in_hunk = false;
        } else if line.starts_with("@@") {
            in_hunk = true;
        }
        let content = in_hunk
            && line
                .chars()
                .next()
                .is_some_and(|first| matches!(first, '+' | '-' | ' '));
        match content {
            true => {
                out.push_str(&line[..1]);
                out.push_str(&expand_tabs(&line[1..], width));
            }
            false => out.push_str(line),
        }
    }
    Cow::Owned(out)
}

/// 解码一段字节。
pub fn decode(bytes: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => {
            let (text, _, _) = encoding_rs::GB18030.decode(bytes);
            text
        }
    }
}

/// 字节数写成人看的大小：`812 B`、`3.4 KB`、`12 MB`、`340 KB`。不到 100 的留一位小数
/// （是 0 就不写），100 以上取整。界面上所有的大小都用它，写法一致。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.;
    let mut unit = 0;
    // 取整以后是 1024 的（1023.5 起）写成下一级的 1
    while value >= 1023.5 && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    let number = match value < 100. {
        true => {
            let text = format!("{value:.1}");
            match text.strip_suffix(".0") {
                Some(whole) => whole.to_string(),
                None => text,
            }
        }
        false => format!("{value:.0}"),
    };
    format!("{number} {}", UNITS[unit])
}

#[cfg(test)]
mod tab_tests {
    use super::{expand_patch_tabs, expand_tabs};

    #[test]
    fn tab_对齐到整数倍() {
        assert_eq!(expand_tabs("a\tb", 4), "a   b");
        assert_eq!(expand_tabs("\tx", 4), "    x");
        assert_eq!(expand_tabs("abcd\tx", 4), "abcd    x");
        assert_eq!(expand_tabs("ab\t\tx", 2), "ab    x");
        assert_eq!(expand_tabs("none", 4), "none");
    }

    #[test]
    fn 补丁里只换代码行() {
        let patch = "diff --git a/x\tb b/x\tb\n--- a/x\tb\n+++ b/x\tb\n@@ -1 +1 @@\n-\told\n+\tnew\n \tsame\n";
        let out = expand_patch_tabs(patch);
        assert!(out.contains("-    old\n+    new\n     same\n"), "{out}");
        // 头上的路径不动
        assert!(out.starts_with("diff --git a/x\tb b/x\tb\n--- a/x\tb\n+++ b/x\tb\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::{decode, human_size};

    #[test]
    fn 大小写成人看的() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(812), "812 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1 KB");
        assert_eq!(human_size(3482), "3.4 KB");
        assert_eq!(human_size(340 * 1024), "340 KB");
        assert_eq!(human_size(5 * 1024 * 1024 + 300 * 1024), "5.3 MB");
        assert_eq!(human_size(12 << 20), "12 MB");
        assert_eq!(human_size(20 << 20), "20 MB");
        assert_eq!(human_size((18 << 20) + (410 << 10)), "18.4 MB");
        assert_eq!(human_size(3 << 30), "3 GB");
        // 差一点到 1 MB 的不写成 1024 KB
        assert_eq!(human_size((1 << 20) - 1), "1 MB");
    }

    #[test]
    fn utf8原样_gbk转过来() {
        assert_eq!(decode("中文 abc".as_bytes()), "中文 abc");
        // “中文”的 GBK 编码
        assert_eq!(decode(&[0xD6, 0xD0, 0xCE, 0xC4, b' ', b'a']), "中文 a");
        assert_eq!(decode(b""), "");
    }
}
