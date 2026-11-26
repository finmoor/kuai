//! 逐行追溯（blame）：文件的每一行最后是哪个提交改的。

use std::collections::HashMap;

/// `git blame` 的参数。`--porcelain`：每个提交的信息只在它第一次出现时给一次。
pub fn args(path: &str) -> Vec<String> {
    args_at(None, path)
}

/// 同 [`args`]，`rev` 给了就追溯那个提交里的版本（不看工作区）。
pub fn args_at(rev: Option<&str>, path: &str) -> Vec<String> {
    let mut list: Vec<String> = ["blame", "--porcelain"].map(String::from).to_vec();
    list.extend(rev.map(str::to_string));
    list.extend(["--".to_string(), path.to_string()]);
    list
}

/// 改过这个文件的一个提交。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlameCommit {
    /// 完整哈希；还没提交的改动是全 0
    pub id: String,
    pub author: String,
    /// 作者时间，Unix 秒
    pub time: i64,
    pub summary: String,
    /// 这个提交改之前，这些行在哪个提交、哪个文件里（文件改过名时路径不一样）。这个文件就是它
    /// 加进来的，或者行还没提交时没有
    pub previous: Option<(String, String)>,
}

impl BlameCommit {
    /// 还没提交的改动（工作区里改了但没提交的行）。
    pub fn uncommitted(&self) -> bool {
        self.id.bytes().all(|b| b == b'0')
    }
}

/// 文件里的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    /// [`Blame::commits`] 的下标
    pub commit: usize,
    /// 这一行是不是一段连续同提交的行里的第一行（界面上只在第一行显示提交信息）
    pub first: bool,
    /// 这一行在改它的那个提交里是第几行（从 1 数）：往更早的版本追时落在差不多的位置
    pub orig_line: u32,
    /// 行的内容，原始字节（文件不一定是 UTF-8）
    pub content: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Blame {
    pub commits: Vec<BlameCommit>,
    pub lines: Vec<BlameLine>,
}

/// 解析 `git blame --porcelain` 的输出。
///
/// 每一行内容前面有一个头：`<哈希> <原行号> <现行号> [<行数>]`，提交第一次出现时后面跟着
/// `author`、`author-time`、`summary` 等若干行，然后是以制表符开头的内容行。
pub fn parse(out: &[u8]) -> Blame {
    let mut blame = Blame::default();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut current: Option<usize> = None;
    let mut previous: Option<usize> = None;
    let mut orig_line = 0;
    for line in out.split(|&b| b == b'\n') {
        if let Some(content) = line.strip_prefix(b"\t") {
            let Some(commit) = current else {
                continue;
            };
            blame.lines.push(BlameLine {
                commit,
                first: previous != Some(commit),
                orig_line,
                content: content.strip_suffix(b"\r").unwrap_or(content).to_vec(),
            });
            previous = Some(commit);
            continue;
        }
        let text = String::from_utf8_lossy(line);
        let (key, value) = text.split_once(' ').unwrap_or((&text, ""));
        // 提交头：40 位（SHA-1）或 64 位（SHA-256 的仓库）十六进制
        if matches!(key.len(), 40 | 64) && key.bytes().all(|b| b.is_ascii_hexdigit()) {
            let next = blame.commits.len();
            let ix = *index.entry(key.to_string()).or_insert(next);
            if ix == next {
                blame.commits.push(BlameCommit {
                    id: key.to_string(),
                    ..Default::default()
                });
            }
            current = Some(ix);
            orig_line = value
                .split_ascii_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            continue;
        }
        let Some(commit) = current.and_then(|ix| blame.commits.get_mut(ix)) else {
            continue;
        };
        match key {
            "author" => commit.author = value.to_string(),
            "author-time" => commit.time = value.parse().unwrap_or(0),
            "summary" => commit.summary = value.to_string(),
            "previous" => {
                commit.previous = value
                    .split_once(' ')
                    .map(|(id, path)| (id.to_string(), crate::patchfile::unquote(path)))
            }
            _ => {}
        }
    }
    blame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析真仓库的输出() {
        let dir = std::env::temp_dir().join(format!("fr-blame-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| crate::git::run(&dir, args).unwrap();
        git(&["init", "-q"]);
        git(&["config", "user.name", "甲"]);
        git(&["config", "user.email", "a@example.com"]);
        git(&["config", "core.autocrlf", "false"]);
        std::fs::write(dir.join("f.txt"), "一\n二\n三\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "第一次"]);
        std::fs::write(dir.join("f.txt"), "一\n改过\n三\n四\n").unwrap();
        git(&["commit", "-qam", "第二次"]);
        std::fs::write(dir.join("f.txt"), "一\n改过\n三\n四\n没提交\n").unwrap();

        let args = args("f.txt");
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let blame = parse(&git(&args));
        let summary = |line: usize| blame.commits[blame.lines[line].commit].summary.as_str();
        assert_eq!(blame.lines.len(), 5);
        assert_eq!(summary(0), "第一次");
        assert_eq!(summary(1), "第二次");
        assert_eq!(summary(2), "第一次");
        assert_eq!(summary(3), "第二次");
        assert!(blame.commits[blame.lines[4].commit].uncommitted());
        assert_eq!(blame.lines[1].content, "改过".as_bytes());
        assert_eq!(blame.commits[blame.lines[0].commit].author, "甲");
        // 第 3、4 行是不同的提交，各自是一段的第一行
        assert!(blame.lines.iter().all(|l| l.first));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sha256_的仓库也认得提交头() {
        let dir = std::env::temp_dir().join(format!("fr-blame-sha256-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 太旧的 git 建不了 SHA-256 的仓库：不测
        if crate::git::run(&dir, &["init", "-q", "--object-format=sha256"]).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let git = |args: &[&str]| crate::git::run(&dir, args).unwrap();
        git(&["config", "user.name", "甲"]);
        git(&["config", "user.email", "a@example.com"]);
        std::fs::write(dir.join("f.txt"), "一\n二\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "第一次"]);
        std::fs::write(dir.join("f.txt"), "一\n二\n没提交\n").unwrap();
        let args = args("f.txt");
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let blame = parse(&git(&args));
        assert_eq!(blame.lines.len(), 3);
        assert_eq!(blame.commits[blame.lines[0].commit].id.len(), 64);
        assert_eq!(blame.commits[blame.lines[0].commit].summary, "第一次");
        assert!(blame.commits[blame.lines[2].commit].uncommitted());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 连续的行只有第一行算一段的开头() {
        let id = "a".repeat(40);
        let out =
            format!("{id} 1 1 2\nauthor 乙\nauthor-time 100\nsummary 说明\n\tx\n{id} 2 2\n\ty\n");
        let blame = parse(out.as_bytes());
        assert_eq!(blame.commits.len(), 1);
        assert_eq!(blame.commits[0].time, 100);
        assert!(blame.lines[0].first);
        assert!(!blame.lines[1].first);
    }
}
