//! 读取并解析提交历史。

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// 一个提交，只含列表里要显示的字段；完整说明和改动在点开时再取。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    /// 完整哈希（十六进制）
    pub id: Box<str>,
    /// 父提交在列表里的下标；不在已读取范围内的父提交不记
    pub parents: Vec<u32>,
    pub author: Box<str>,
    /// 作者时间，Unix 秒
    pub time: i64,
    /// 指向这个提交的引用，git `%D` 的原样输出，例如 `HEAD -> main, origin/main, tag: v1`
    pub refs: Box<str>,
    /// 说明的第一行
    pub subject: Box<str>,
}

/// 字段之间用 0x1f 分隔，提交之间由 `-z` 用 NUL 分隔，说明里出现什么字符都不会串行。
const FORMAT: &str = "--format=%H%x1f%P%x1f%an%x1f%at%x1f%D%x1f%s";

/// `git log` 的参数：所有本地分支、远程分支、标签和 HEAD（不含 stash），按日期排序。
/// 日期排序保证子提交总在父提交前面，提交图布局依赖这一点。
pub const LOG_ARGS: &[&str] = &[
    "log",
    "--branches",
    "--remotes",
    "--tags",
    "HEAD",
    "--date-order",
    "-z",
    FORMAT,
];

/// 取一个提交改动的 `git show` 参数，后面再接提交哈希。
/// 合并提交只和第一个父提交比较，和 Fork 默认的显示一致。
pub const SHOW_ARGS: &[&str] = &[
    "show",
    "--format=",
    "--patch",
    "--no-ext-diff",
    "--diff-merges=first-parent",
    "-M",
    "-U3",
];

/// 在本机执行 `git log` 并解析。
pub fn load(repo: &Path) -> Result<Vec<Commit>> {
    let out = crate::git::run(repo, LOG_ARGS)?;
    Ok(parse(&out))
}

/// 解析 [`LOG_ARGS`] 产生的输出。
pub fn parse(out: &[u8]) -> Vec<Commit> {
    struct Raw<'a> {
        id: &'a str,
        parents: &'a str,
        author: &'a str,
        time: i64,
        refs: &'a str,
        subject: &'a str,
    }

    let text = String::from_utf8_lossy(out);
    let mut raws = Vec::new();
    for record in text.split('\0') {
        // 第一个之后的记录前面可能带一个换行
        let record = record.trim_start_matches('\n');
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(6, '\x1f');
        let (Some(id), Some(parents), Some(author), Some(time), Some(refs)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        raws.push(Raw {
            id,
            parents,
            author,
            time: time.parse().unwrap_or(0),
            refs,
            subject: fields.next().unwrap_or(""),
        });
    }

    let index: HashMap<&str, u32> = raws
        .iter()
        .enumerate()
        .map(|(i, raw)| (raw.id, i as u32))
        .collect();
    raws.iter()
        .map(|raw| Commit {
            id: raw.id.into(),
            parents: raw
                .parents
                .split_ascii_whitespace()
                .filter_map(|p| index.get(p).copied())
                .collect(),
            author: raw.author.into(),
            time: raw.time,
            refs: raw.refs.into(),
            subject: raw.subject.into(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析两条提交并把父提交换成下标() {
        let out = "bbb\x1faaa\x1f张伟\x1f1700000100\x1fHEAD -> main\x1f修复：空列表崩溃\0\
                   \naaa\x1f\x1fAlice\x1f1700000000\x1f\x1finit\0";
        let commits = parse(out.as_bytes());
        assert_eq!(commits.len(), 2);
        assert_eq!(&*commits[0].id, "bbb");
        assert_eq!(commits[0].parents, vec![1]);
        assert_eq!(&*commits[0].refs, "HEAD -> main");
        assert_eq!(&*commits[0].subject, "修复：空列表崩溃");
        assert!(commits[1].parents.is_empty());
        assert_eq!(commits[1].time, 1700000000);
    }

    #[test]
    fn 不在范围内的父提交被忽略() {
        let out = "bbb\x1fzzz\x1fA\x1f1\x1f\x1fx\0";
        assert!(parse(out.as_bytes())[0].parents.is_empty());
    }
}
