//! 工作区状态：`git status --porcelain=v2 -z` 的参数和解析。需要 git 2.11 以上。

/// 读工作区状态的参数：带分支信息，未跟踪的文件逐个列出（和 Fork 一样）。
pub fn args() -> Vec<String> {
    [
        "status",
        "--porcelain=v2",
        "-z",
        "--branch",
        "--untracked-files=all",
    ]
    .map(String::from)
    .to_vec()
}

/// 一个文件的改动类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

impl Kind {
    /// 列表里显示的字母
    pub fn letter(self) -> &'static str {
        match self {
            Kind::Added => "A",
            Kind::Modified => "M",
            Kind::Deleted => "D",
            Kind::Renamed => "R",
            Kind::Copied => "C",
            Kind::TypeChanged => "T",
            Kind::Untracked => "?",
            Kind::Conflicted => "U",
        }
    }

    fn from_code(code: u8) -> Option<Kind> {
        Some(match code {
            b'A' => Kind::Added,
            b'M' => Kind::Modified,
            b'D' => Kind::Deleted,
            b'R' => Kind::Renamed,
            b'C' => Kind::Copied,
            b'T' => Kind::TypeChanged,
            b'U' => Kind::Conflicted,
            _ => return None,
        })
    }
}

/// 一个有改动的文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    /// 改名或复制前的路径
    pub orig_path: Option<String>,
    /// 暂存区相对 HEAD 的改动
    pub staged: Option<Kind>,
    /// 工作区相对暂存区的改动；未跟踪和有冲突的文件也算在这边
    pub unstaged: Option<Kind>,
    pub submodule: bool,
    /// 暂存区里这个文件的模式和对象号（porcelain v2 的 `mI hI`）。暂存区里的内容变了（又暂存了一些、
    /// 在别处暂存），改动类型可能不变，这里会变；冲突、未跟踪的文件没有
    pub index: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// 当前分支；分离头指针时为空
    pub branch: Option<String>,
    /// HEAD 指向的提交；还没有任何提交时为空
    pub head: Option<String>,
    pub upstream: Option<String>,
    /// 比上游多几个提交
    pub ahead: u32,
    /// 比上游少几个提交
    pub behind: u32,
    pub entries: Vec<Entry>,
}

impl Status {
    pub fn unstaged(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.unstaged.is_some())
    }

    pub fn staged(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.staged.is_some())
    }
}

/// 解析 [`args`] 产生的输出。
pub fn parse(out: &[u8]) -> Status {
    let text = String::from_utf8_lossy(out);
    let mut status = Status::default();
    let mut records = text.split('\0');
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if let Some(header) = record.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" => status.head = (value != "(initial)").then(|| value.to_string()),
                "branch.head" => status.branch = (value != "(detached)").then(|| value.to_string()),
                "branch.upstream" => status.upstream = Some(value.to_string()),
                "branch.ab" => {
                    for part in value.split_whitespace() {
                        if let Some(n) = part.strip_prefix('+') {
                            status.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            status.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        match record.as_bytes()[0] {
            // 普通改动：1 XY sub mH mI mW hH hI path
            // 改名或复制：2 XY sub mH mI mW hH hI Xscore path，原路径是下一个 NUL 分隔的字段
            kind @ (b'1' | b'2') => {
                let n = if kind == b'1' { 9 } else { 10 };
                let fields: Vec<&str> = record.splitn(n, ' ').collect();
                if fields.len() < n {
                    continue;
                }
                let xy = fields[1].as_bytes();
                let orig_path = if kind == b'2' {
                    records.next().map(String::from)
                } else {
                    None
                };
                status.entries.push(Entry {
                    path: fields[n - 1].to_string(),
                    orig_path,
                    staged: xy.first().and_then(|&c| Kind::from_code(c)),
                    unstaged: xy.get(1).and_then(|&c| Kind::from_code(c)),
                    submodule: fields[2].starts_with('S'),
                    index: Some(format!("{} {}", fields[4], fields[7])),
                });
            }
            // 有冲突：u XY sub m1 m2 m3 mW h1 h2 h3 path
            b'u' => {
                let fields: Vec<&str> = record.splitn(11, ' ').collect();
                if fields.len() < 11 {
                    continue;
                }
                status.entries.push(Entry {
                    path: fields[10].to_string(),
                    orig_path: None,
                    staged: None,
                    unstaged: Some(Kind::Conflicted),
                    submodule: fields[2].starts_with('S'),
                    index: None,
                });
            }
            b'?' => {
                if let Some(path) = record.strip_prefix("? ") {
                    status.entries.push(Entry {
                        path: path.to_string(),
                        orig_path: None,
                        staged: None,
                        unstaged: Some(Kind::Untracked),
                        submodule: false,
                        index: None,
                    });
                }
            }
            _ => {}
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析分支信息和各类改动() {
        let out = [
            "# branch.oid 1234abcd",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -3",
            "1 .M N... 100644 100644 100644 aaa aaa src/有 空格.rs",
            "1 A. N... 000000 100644 100644 000 bbb new.txt",
            "1 MD N... 100644 100644 000000 ccc ddd both.txt",
            "2 R. N... 100644 100644 100644 eee eee R100 新名字.md",
            "旧名字.md",
            "u UU N... 100644 100644 100644 100644 f1 f2 f3 conflict.txt",
            "? 未跟踪/文件.txt",
            "",
        ]
        .join("\0");
        let status = parse(out.as_bytes());
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.head.as_deref(), Some("1234abcd"));
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.ahead, status.behind), (2, 3));
        assert_eq!(status.entries.len(), 6);

        let e = &status.entries[0];
        assert_eq!(e.path, "src/有 空格.rs");
        assert_eq!((e.staged, e.unstaged), (None, Some(Kind::Modified)));
        let e = &status.entries[2];
        assert_eq!(
            (e.staged, e.unstaged),
            (Some(Kind::Modified), Some(Kind::Deleted))
        );
        assert_eq!(e.index.as_deref(), Some("100644 ddd"));
        let e = &status.entries[3];
        assert_eq!(e.path, "新名字.md");
        assert_eq!(e.orig_path.as_deref(), Some("旧名字.md"));
        assert_eq!(e.staged, Some(Kind::Renamed));
        assert_eq!(status.entries[4].unstaged, Some(Kind::Conflicted));
        assert_eq!(status.entries[5].unstaged, Some(Kind::Untracked));
        assert_eq!(status.unstaged().count(), 4);
        assert_eq!(status.staged().count(), 3);
    }

    #[test]
    fn 还没有提交和分离头指针() {
        let status = parse(b"# branch.oid (initial)\0# branch.head (detached)\0");
        assert_eq!(status.head, None);
        assert_eq!(status.branch, None);
    }
}
