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
    /// 父提交在列表里的下标。不在列表里的父提交给一个 >= 提交数的占位下标（同一个父提交用同一个），
    /// 这样只读了前一段时，泳道照样等着它，前一段的布局和读完全部时一致
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

/// [`LOG_ARGS`] 的 `String` 版，给通道接口用。
pub fn log_args() -> Vec<String> {
    LOG_ARGS.iter().map(|s| s.to_string()).collect()
}

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

    let mut index: HashMap<&str, u32> = raws
        .iter()
        .enumerate()
        .map(|(i, raw)| (raw.id, i as u32))
        .collect();
    let mut next_missing = raws.len() as u32;
    raws.iter()
        .map(|raw| Commit {
            id: raw.id.into(),
            parents: raw
                .parents
                .split_ascii_whitespace()
                .map(|p| {
                    *index.entry(p).or_insert_with(|| {
                        next_missing += 1;
                        next_missing - 1
                    })
                })
                .collect(),
            author: raw.author.into(),
            time: raw.time,
            refs: raw.refs.into(),
            subject: raw.subject.into(),
        })
        .collect()
}

/// 第 `n` 条记录结束（第 `n` 个 NUL 之后）的位置；不够 `n` 条时返回 None。读历史时先显示前一页用。
pub fn nth_record_end(out: &[u8], n: usize) -> Option<usize> {
    if n == 0 {
        return Some(0);
    }
    out.iter()
        .enumerate()
        .filter(|(_, b)| **b == 0)
        .nth(n - 1)
        .map(|(i, _)| i + 1)
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
    fn 不在列表里的父提交用同一个占位下标() {
        let out = "bbb\x1fzzz\x1fA\x1f1\x1f\x1fx\0ccc\x1fzzz yyy\x1fA\x1f1\x1f\x1fy\0";
        let commits = parse(out.as_bytes());
        assert_eq!(commits[0].parents, vec![2]);
        assert_eq!(commits[1].parents, vec![2, 3]);
    }

    #[test]
    fn 只读前一段时布局和读完全部时一致() {
        // 两条分支来回合并，前一段里有父提交在后面
        let history = [
            ("m4", "m3 f3"),
            ("f3", "f2"),
            ("m3", "m2"),
            ("g1", "m2"),
            ("f2", "f1 m2"),
            ("m2", "m1"),
            ("f1", "m1"),
            ("m1", "m0"),
            ("m0", ""),
        ];
        let out: String = history
            .iter()
            .map(|(id, parents)| format!("{id}\x1f{parents}\x1fA\x1f1\x1f\x1fs\0"))
            .collect();
        let full = crate::graph::Graph::build(&parse(out.as_bytes()));
        for n in 1..history.len() {
            let end = nth_record_end(out.as_bytes(), n).unwrap();
            let prefix = crate::graph::Graph::build(&parse(&out.as_bytes()[..end]));
            for row in 0..n {
                assert_eq!(
                    prefix.rows[row].lane, full.rows[row].lane,
                    "前 {n} 条，第 {row} 行"
                );
                assert_eq!(prefix.rows[row].color, full.rows[row].color);
                assert_eq!(
                    prefix.segments(row),
                    full.segments(row),
                    "前 {n} 条，第 {row} 行"
                );
            }
        }
        assert_eq!(nth_record_end(out.as_bytes(), 100), None);
    }
}
