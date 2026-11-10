//! 取一个提交的详情和改动。

/// 详情字段用 0x1f 分隔、以 NUL 结尾，后面紧跟补丁。
const FORMAT: &str = "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%cn%x1f%ce%x1f%ct%x1f%B%x00";

/// `git show` 的参数。合并提交只和第一个父提交比较，和 Fork 默认的显示一致。
pub fn args(rev: &str) -> Vec<String> {
    [
        "show",
        FORMAT,
        "--patch",
        "--no-ext-diff",
        "--diff-merges=first-parent",
        "-M",
        "-U3",
        rev,
    ]
    .map(String::from)
    .to_vec()
}

/// 提交详情。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Details {
    pub id: String,
    pub parents: Vec<String>,
    pub author: String,
    pub author_email: String,
    /// Unix 秒
    pub author_time: i64,
    pub committer: String,
    pub committer_email: String,
    pub committer_time: i64,
    /// 完整说明
    pub message: String,
}

/// 拆开 [`args`] 产生的输出：详情和补丁。
pub fn parse(out: &[u8]) -> (Details, &[u8]) {
    let (head, patch) = match out.iter().position(|&b| b == 0) {
        Some(i) => (&out[..i], &out[i + 1..]),
        None => (&out[..0], out),
    };
    let head = String::from_utf8_lossy(head);
    let mut fields = head.trim_start_matches('\n').splitn(9, '\x1f');
    let mut next = || fields.next().unwrap_or("").to_string();
    let id = next();
    let parents = next().split_whitespace().map(String::from).collect();
    let author = next();
    let author_email = next();
    let author_time = next().parse().unwrap_or(0);
    let committer = next();
    let committer_email = next();
    let committer_time = next().parse().unwrap_or(0);
    let message = next().trim_end().to_string();
    // 补丁前面的空行去掉
    let skip = patch.iter().take_while(|&&b| b == b'\n').count();
    (
        Details {
            id,
            parents,
            author,
            author_email,
            author_time,
            committer,
            committer_email,
            committer_time,
            message,
        },
        &patch[skip..],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 拆出详情和补丁() {
        let out = "abc\x1fp1 p2\x1f张伟\x1fzw@example.com\x1f1700000000\x1fBot\x1fbot@example.com\x1f1700000100\x1f修复：空列表崩溃\n\n详细说明\n\0\n\ndiff --git a/x b/x\n";
        let (details, patch) = parse(out.as_bytes());
        assert_eq!(details.id, "abc");
        assert_eq!(details.parents, ["p1", "p2"]);
        assert_eq!(details.author, "张伟");
        assert_eq!(details.author_time, 1700000000);
        assert_eq!(details.committer_time, 1700000100);
        assert_eq!(details.message, "修复：空列表崩溃\n\n详细说明");
        assert_eq!(patch, b"diff --git a/x b/x\n");
    }

    #[test]
    fn 没有补丁也能解析() {
        let (details, patch) =
            parse("abc\x1f\x1fA\x1fa@x\x1f1\x1fA\x1fa@x\x1f1\x1finit\n\0\n".as_bytes());
        assert_eq!(details.message, "init");
        assert!(details.parents.is_empty());
        assert!(patch.is_empty());
    }
}
