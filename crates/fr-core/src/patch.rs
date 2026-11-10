//! 解析 `git diff` 的输出，生成只含选中行的补丁，用来按块、按行暂存、取消暂存和丢弃。
//!
//! 补丁内容按原样字节保存：文件不是 UTF-8（比如 GBK）或带 CRLF 时，生成的补丁照样能应用。
//!
//! 生成规则和 git gui、`git add -p` 一样。补丁要应用到「目标」一侧，目标里本来就有的行改成上下文：
//! - 暂存（正向应用到暂存区，目标是旧的一侧）：没选中的 `-` 行改成上下文，没选中的 `+` 行去掉；
//! - 取消暂存、丢弃（用 `--reverse` 应用到暂存区或工作区，目标是新的一侧）：
//!   没选中的 `+` 行改成上下文，没选中的 `-` 行去掉。

use std::io::Write as _;

/// 取工作区相对暂存区的改动（未暂存的）。前缀写死成 a/ b/，不受用户的 diff 配置影响。
pub fn unstaged_args(path: &str) -> Vec<String> {
    diff_args(&[], &[path])
}

/// 取暂存区相对 HEAD 的改动（已暂存的）。改名时把原路径也带上，才能看到改名前后的对比。
pub fn staged_args(path: &str, orig_path: Option<&str>) -> Vec<String> {
    let mut paths = vec![path];
    paths.extend(orig_path);
    diff_args(&["--cached", "-M"], &paths)
}

/// 取未跟踪文件的内容，显示成新文件。退出码 1 表示有差异，属于正常结果。
pub fn untracked_args(path: &str) -> Vec<String> {
    diff_args(&["--no-index"], &["/dev/null", path])
}

fn diff_args(extra: &[&str], paths: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "-U3",
    ]
    .map(String::from)
    .to_vec();
    args.extend(extra.iter().map(|s| s.to_string()));
    args.push("--".into());
    args.extend(paths.iter().map(|s| s.to_string()));
    args
}

/// 把补丁应用到暂存区：暂存（`reverse=false`）或取消暂存（`reverse=true`）。补丁从标准输入给。
pub fn apply_cached_args(reverse: bool) -> Vec<String> {
    let mut args = apply_args(reverse);
    args.insert(1, "--cached".into());
    args
}

/// 把补丁反向应用到工作区：丢弃选中的改动。补丁从标准输入给。
pub fn discard_args() -> Vec<String> {
    apply_args(true)
}

fn apply_args(reverse: bool) -> Vec<String> {
    let mut args: Vec<String> = ["apply", "--recount", "--whitespace=nowarn"]
        .map(String::from)
        .to_vec();
    if reverse {
        args.push("--reverse".into());
    }
    args.push("-".into());
    args
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    /// 去掉开头的 `+`、`-` 或空格后的原样字节，不含换行
    pub content: Vec<u8>,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    /// 紧跟着的 `\ No newline at end of file` 行，原样保存
    pub no_newline: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    /// `@@ … @@` 后面 git 给出的所在函数
    pub section: String,
    pub lines: Vec<Line>,
}

impl Hunk {
    /// 块标题，比如 `@@ -10,7 +10,8 @@ fn main()`
    pub fn title(&self) -> String {
        let mut title = format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_count, self.new_start, self.new_count
        );
        if !self.section.is_empty() {
            title.push(' ');
            title.push_str(&self.section);
        }
        title
    }
}

/// 一个文件的改动。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileDiff {
    /// 从 `diff --git` 到第一个 `@@` 之前的行，原样字节
    pub header: Vec<Vec<u8>>,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    pub fn is_new(&self) -> bool {
        self.has_header(b"new file mode")
    }

    pub fn is_deleted(&self) -> bool {
        self.has_header(b"deleted file mode")
    }

    pub fn is_binary(&self) -> bool {
        self.has_header(b"Binary files ") || self.has_header(b"GIT binary patch")
    }

    /// 合并冲突时的组合 diff（`diff --cc`），不能按行操作
    pub fn is_combined(&self) -> bool {
        self.has_header(b"diff --cc ") || self.has_header(b"diff --combined ")
    }

    fn has_header(&self, prefix: &[u8]) -> bool {
        self.header.iter().any(|l| l.starts_with(prefix))
    }

    /// 增加和删除的行数。
    pub fn changed_lines(&self) -> usize {
        self.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind != LineKind::Context)
            .count()
    }

    /// 生成只含选中行的补丁，`selected(块下标, 行下标)` 说明哪些改动行选中了。
    /// `reverse` 见模块说明。没有选中任何改动行时返回 None。
    pub fn partial(
        &self,
        reverse: bool,
        selected: impl Fn(usize, usize) -> bool,
    ) -> Option<Vec<u8>> {
        let mut body = Vec::new();
        let mut all = true;
        let mut any = false;
        // 前面已经写进补丁的块让另一侧的行号偏移了多少
        let mut delta: i64 = 0;
        for (hi, hunk) in self.hunks.iter().enumerate() {
            let mut lines = Vec::new();
            let (mut old_n, mut new_n) = (0u32, 0u32);
            let mut changed = false;
            for (li, line) in hunk.lines.iter().enumerate() {
                let picked = line.kind != LineKind::Context && selected(hi, li);
                if line.kind != LineKind::Context && !picked {
                    all = false;
                }
                let prefix = match (line.kind, picked, reverse) {
                    (LineKind::Context, _, _) => b' ',
                    (LineKind::Added, true, _) => b'+',
                    (LineKind::Removed, true, _) => b'-',
                    // 目标里有这一行，留作上下文
                    (LineKind::Added, false, true) | (LineKind::Removed, false, false) => b' ',
                    // 目标里没有这一行，去掉
                    (LineKind::Added, false, false) | (LineKind::Removed, false, true) => continue,
                };
                match prefix {
                    b'+' => new_n += 1,
                    b'-' => old_n += 1,
                    _ => {
                        old_n += 1;
                        new_n += 1;
                    }
                }
                changed |= prefix != b' ';
                lines.push(prefix);
                lines.extend_from_slice(&line.content);
                lines.push(b'\n');
                if let Some(marker) = &line.no_newline {
                    lines.extend_from_slice(marker);
                    lines.push(b'\n');
                }
            }
            if !changed {
                continue;
            }
            any = true;
            // 目标一侧的起始行号不变，另一侧按前面的块偏移；`git apply` 以目标一侧为准找位置
            let (old_start, new_start) = if reverse {
                let new_start = i64::from(hunk.new_start);
                (side_start(new_start - delta, old_n), new_start)
            } else {
                let old_start = i64::from(hunk.old_start);
                (old_start, side_start(old_start + delta, new_n))
            };
            delta += i64::from(new_n) - i64::from(old_n);
            let _ = write!(body, "@@ -{old_start},{old_n} +{new_start},{new_n} @@");
            if !hunk.section.is_empty() {
                body.push(b' ');
                body.extend_from_slice(hunk.section.as_bytes());
            }
            body.push(b'\n');
            body.extend_from_slice(&lines);
        }
        if !any {
            return None;
        }

        // 只选了一部分时：暂存删除文件的部分行、取消暂存新文件的部分行，结果是修改而不是删除或新建，
        // 头部要改成修改，否则 git apply 会报「删除补丁留下了文件内容」之类的错
        let rewrite = !all && ((!reverse && self.is_deleted()) || (reverse && self.is_new()));
        let mut patch = Vec::new();
        for line in &self.header {
            if rewrite {
                if line.starts_with(b"new file mode")
                    || line.starts_with(b"deleted file mode")
                    || line.starts_with(b"index ")
                {
                    continue;
                }
                if line.as_slice() == b"--- /dev/null"
                    && let Some(plus) = self.header.iter().find(|l| l.starts_with(b"+++ "))
                {
                    patch.extend(swap_side(plus, b"--- ", b'b', b'a'));
                    patch.push(b'\n');
                    continue;
                }
                if line.as_slice() == b"+++ /dev/null"
                    && let Some(minus) = self.header.iter().find(|l| l.starts_with(b"--- "))
                {
                    patch.extend(swap_side(minus, b"+++ ", b'a', b'b'));
                    patch.push(b'\n');
                    continue;
                }
            }
            patch.extend_from_slice(line);
            patch.push(b'\n');
        }
        patch.extend(body);
        Some(patch)
    }
}

/// 行数为 0 时，统一 diff 格式的起始行号写的是前一行。
fn side_start(start: i64, count: u32) -> i64 {
    if count == 0 {
        (start - 1).max(0)
    } else {
        start.max(1)
    }
}

/// `+++ b/x` 换成 `--- a/x`（或反过来），带引号的路径也处理。
fn swap_side(line: &[u8], to_prefix: &[u8], from_dir: u8, to_dir: u8) -> Vec<u8> {
    let rest = &line[4.min(line.len())..];
    let mut out = to_prefix.to_vec();
    if rest.len() >= 2 && rest[0] == from_dir && rest[1] == b'/' {
        out.push(to_dir);
        out.extend_from_slice(&rest[1..]);
    } else if rest.len() >= 3 && rest[0] == b'"' && rest[1] == from_dir && rest[2] == b'/' {
        out.push(b'"');
        out.push(to_dir);
        out.extend_from_slice(&rest[2..]);
    } else {
        out.extend_from_slice(rest);
    }
    out
}

/// 解析 `git diff` 的输出。
pub fn parse(out: &[u8]) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    // 当前块还差几行旧的、几行新的
    let (mut old_left, mut new_left) = (0u32, 0u32);
    let (mut old_no, mut new_no) = (0u32, 0u32);
    for raw in out.split(|&b| b == b'\n') {
        if old_left > 0 || new_left > 0 {
            let hunk = files
                .last_mut()
                .and_then(|f| f.hunks.last_mut())
                .expect("块内必有当前块");
            let (kind, content) = match raw.first() {
                // `diff.suppressBlankEmpty` 打开时，空的上下文行没有前面的空格
                None => (LineKind::Context, &raw[..0]),
                Some(b' ') => (LineKind::Context, &raw[1..]),
                Some(b'+') => (LineKind::Added, &raw[1..]),
                Some(b'-') => (LineKind::Removed, &raw[1..]),
                Some(b'\\') => {
                    if let Some(last) = hunk.lines.last_mut() {
                        last.no_newline = Some(raw.to_vec());
                    }
                    continue;
                }
                // 行数对不上：当作这一块结束，按头部处理
                Some(_) => {
                    old_left = 0;
                    new_left = 0;
                    files.last_mut().unwrap().header.push(raw.to_vec());
                    continue;
                }
            };
            let (o, n) = match kind {
                LineKind::Context => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                    old_no += 1;
                    new_no += 1;
                    (Some(old_no - 1), Some(new_no - 1))
                }
                LineKind::Added => {
                    new_left = new_left.saturating_sub(1);
                    new_no += 1;
                    (None, Some(new_no - 1))
                }
                LineKind::Removed => {
                    old_left = old_left.saturating_sub(1);
                    old_no += 1;
                    (Some(old_no - 1), None)
                }
            };
            hunk.lines.push(Line {
                kind,
                content: content.to_vec(),
                old_no: o,
                new_no: n,
                no_newline: None,
            });
            continue;
        }
        if raw.starts_with(b"\\") {
            if let Some(line) = files
                .last_mut()
                .and_then(|f| f.hunks.last_mut())
                .and_then(|h| h.lines.last_mut())
            {
                line.no_newline = Some(raw.to_vec());
            }
            continue;
        }
        if raw.starts_with(b"diff ") {
            files.push(FileDiff {
                header: vec![raw.to_vec()],
                hunks: Vec::new(),
            });
            continue;
        }
        if raw.starts_with(b"@@ ")
            && let Some(hunk) = parse_hunk_header(raw)
        {
            if files.is_empty() {
                files.push(FileDiff::default());
            }
            old_left = hunk.old_count;
            new_left = hunk.new_count;
            old_no = hunk.old_start;
            new_no = hunk.new_start;
            files.last_mut().unwrap().hunks.push(hunk);
            continue;
        }
        if let Some(file) = files.last_mut()
            && file.hunks.is_empty()
            && !raw.is_empty()
        {
            file.header.push(raw.to_vec());
        }
    }
    files
}

fn parse_hunk_header(line: &[u8]) -> Option<Hunk> {
    let text = String::from_utf8_lossy(line);
    let rest = text.strip_prefix("@@ -")?;
    let (ranges, section) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |r: &str| -> Option<(u32, u32)> {
        match r.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = range(old)?;
    let (new_start, new_count) = range(new)?;
    Some(Hunk {
        old_start,
        old_count,
        new_start,
        new_count,
        section: section.trim().to_string(),
        lines: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/a.txt b/a.txt
index 1111111..2222222 100644
--- a/a.txt
+++ b/a.txt
@@ -1,4 +1,4 @@ fn main()
 1
-2
+two
 3
 4
@@ -8,3 +8,4 @@
 8
 9
 10
+11
\\ No newline at end of file
";

    #[test]
    fn 解析头部块和行号() {
        let files = parse(DIFF.as_bytes());
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.header.len(), 4);
        assert_eq!(f.hunks.len(), 2);
        let h = &f.hunks[0];
        assert_eq!(
            (h.old_start, h.old_count, h.new_start, h.new_count),
            (1, 4, 1, 4)
        );
        assert_eq!(h.section, "fn main()");
        assert_eq!(h.lines.len(), 5);
        assert_eq!(h.lines[1].kind, LineKind::Removed);
        assert_eq!((h.lines[1].old_no, h.lines[1].new_no), (Some(2), None));
        assert_eq!((h.lines[2].old_no, h.lines[2].new_no), (None, Some(2)));
        assert_eq!((h.lines[3].old_no, h.lines[3].new_no), (Some(3), Some(3)));
        let last = f.hunks[1].lines.last().unwrap();
        assert_eq!(last.content, b"11");
        assert!(last.no_newline.is_some());
        assert_eq!(f.changed_lines(), 3);
    }

    #[test]
    fn 暂存时没选中的删除行变上下文没选中的增加行去掉() {
        let f = &parse(DIFF.as_bytes())[0];
        // 只选第一块里的 +two
        let patch = f.partial(false, |h, l| h == 0 && l == 2).unwrap();
        let text = String::from_utf8(patch).unwrap();
        assert!(text.contains("@@ -1,4 +1,5 @@ fn main()\n 1\n 2\n+two\n 3\n 4\n"));
        assert!(!text.contains("+11"));
    }

    #[test]
    fn 取消暂存时没选中的增加行变上下文没选中的删除行去掉() {
        let f = &parse(DIFF.as_bytes())[0];
        let patch = f.partial(true, |h, l| h == 0 && l == 1).unwrap();
        let text = String::from_utf8(patch).unwrap();
        // 暂存区里有 two、没有 2：取消暂存「删掉 2」就是把 2 加回去，two 留作上下文
        assert!(
            text.contains("@@ -1,5 +1,4 @@ fn main()\n 1\n-2\n two\n 3\n 4\n"),
            "{text}"
        );
    }

    #[test]
    fn 第二块的另一侧行号按前面的块偏移且保留无换行标记() {
        let f = &parse(DIFF.as_bytes())[0];
        let patch = f.partial(false, |_, _| true).unwrap();
        let text = String::from_utf8(patch).unwrap();
        assert!(text.contains("@@ -8,3 +8,4 @@\n 8\n 9\n 10\n+11\n\\ No newline at end of file\n"));
    }

    #[test]
    fn 没选中改动行时不生成补丁() {
        let f = &parse(DIFF.as_bytes())[0];
        assert!(f.partial(false, |_, _| false).is_none());
    }

    #[test]
    fn 部分暂存删除的文件时改成修改() {
        let diff = "diff --git a/c.txt b/c.txt
deleted file mode 100644
index 3333333..0000000
--- a/c.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-x
-y
";
        let f = &parse(diff.as_bytes())[0];
        assert!(f.is_deleted());
        let text = String::from_utf8(f.partial(false, |_, l| l == 0).unwrap()).unwrap();
        assert!(!text.contains("deleted file mode"));
        assert!(text.contains("--- a/c.txt\n+++ b/c.txt\n"), "{text}");
        assert!(text.contains("@@ -1,2 +1,1 @@\n-x\n y\n"), "{text}");
        // 全选时保持删除
        let text = String::from_utf8(f.partial(false, |_, _| true).unwrap()).unwrap();
        assert!(text.contains("deleted file mode"));
        assert!(text.contains("@@ -1,2 +0,0 @@"));
    }

    #[test]
    fn 二进制文件没有块() {
        let diff = "diff --git a/x.png b/x.png\nindex 1..2 100644\nBinary files a/x.png and b/x.png differ\n";
        let f = &parse(diff.as_bytes())[0];
        assert!(f.is_binary());
        assert!(f.hunks.is_empty());
    }
}
