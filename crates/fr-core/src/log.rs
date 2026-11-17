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

/// 提交列表里列哪些分支上的提交。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Scope {
    /// 所有本地分支、远程分支、标签和 HEAD
    #[default]
    All,
    /// 只列从这几个引用（完整的引用名、`HEAD` 或哈希）够得着的提交
    Refs(Vec<String>),
}

/// 按范围读提交历史的参数。`hidden` 是隐藏起来的引用（完整的名字，比如 `refs/heads/wip`、
/// `refs/remotes/origin/old`、`refs/tags/v0.1`），只在列全部分支时起作用：这些引用不当起点，
/// 只在它们上面的提交就不出现了。HEAD 总是列出来的，所以隐藏不了当前分支。
pub fn scoped_log_args(scope: &Scope, hidden: &[String]) -> Vec<String> {
    if *scope == Scope::All && hidden.is_empty() {
        return log_args();
    }
    let mut args: Vec<String> = vec!["log".into()];
    args.extend(scope_refs(scope, hidden));
    args.extend(["--date-order", "-z", FORMAT].map(String::from));
    // 引用名和后面可能出现的路径分开：分支叫 `docs`、仓库里又有 `docs` 目录时不会有歧义
    args.push("--".into());
    args
}

/// 范围写成 `git log` 的起点：从哪些引用往回列。
fn scope_refs(scope: &Scope, hidden: &[String]) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    match scope {
        Scope::All => {
            // `--exclude` 只管它后面最近的一个 `--branches`、`--remotes` 或 `--tags`，
            // 而且写的是去掉 `refs/heads/` 这类前缀以后的名字
            for (prefix, flag) in [
                ("refs/heads/", "--branches"),
                ("refs/remotes/", "--remotes"),
                ("refs/tags/", "--tags"),
            ] {
                for name in hidden.iter().filter_map(|full| full.strip_prefix(prefix)) {
                    args.push(format!("--exclude={name}"));
                }
                args.push(flag.into());
            }
            args.push("HEAD".into());
        }
        Scope::Refs(refs) => args.extend(refs.iter().cloned()),
    }
    args
}

/// 让 git 去找提交时按什么找（按说明、作者、哈希找的不用 git，列表里现成的字段就够）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Find {
    /// 改动过路径里有这几个字的文件
    Path,
    /// 改动里增加或删除过这几个字（`git log -S`）
    Content,
}

/// 在范围里找提交的参数，输出和 [`scoped_log_args`] 一样，用 [`parse`] 解析。都不分大小写。
/// 找到的提交互相不一定是父子，画的时候连成一条线。
pub fn find_log_args(scope: &Scope, hidden: &[String], find: Find, needle: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if find == Find::Path {
        // 我们给 git 设了路径一律按字面理解，这里要通配，得先关掉
        args.push("--no-literal-pathspecs".into());
    }
    args.push("log".into());
    args.extend(scope_refs(scope, hidden));
    args.extend(["--date-order", "-z", FORMAT].map(String::from));
    match find {
        Find::Path => {
            // 不带 glob 的通配里 `*` 也匹配 `/`，所以前后各一个 `*` 就是「路径里有这几个字」；
            // 要找的字里的通配符按字面算。Windows 上习惯写的反斜杠当成斜杠
            let mut pattern = String::from(":(icase)*");
            for ch in needle.chars() {
                match ch {
                    '\\' => pattern.push('/'),
                    // 不用反斜杠转义：Windows 上的 git 会先把路径里的反斜杠换成斜杠
                    '*' | '?' | '[' => {
                        pattern.push('[');
                        pattern.push(ch);
                        pattern.push(']');
                    }
                    _ => pattern.push(ch),
                }
            }
            pattern.push('*');
            args.extend(["--".into(), pattern]);
        }
        Find::Content => {
            args.extend(["-i".into(), format!("-S{needle}"), "--".into()]);
        }
    }
    args
}

/// 当前分支和它的上游各多出哪些提交。`upstream` 是远程分支的名字（`origin/main`）。
pub fn pending_args(upstream: &str) -> Vec<String> {
    vec![
        "rev-list".into(),
        "--left-right".into(),
        format!("HEAD...refs/remotes/{upstream}"),
        "--".into(),
    ]
}

/// 解析 [`pending_args`] 的输出：（本地有、上游还没有的，上游有、本地还没有的）。
pub fn parse_pending(out: &[u8]) -> (Vec<Box<str>>, Vec<Box<str>>) {
    let (mut ahead, mut behind) = (Vec::new(), Vec::new());
    for line in String::from_utf8_lossy(out).lines() {
        if let Some(id) = line.strip_prefix('<') {
            ahead.push(id.trim().into());
        } else if let Some(id) = line.strip_prefix('>') {
            behind.push(id.trim().into());
        }
    }
    (ahead, behind)
}

/// 文件历史的参数：从 HEAD 往回，只要改过这个文件的提交，跟着改名往回找。
pub fn file_log_args(path: &str) -> Vec<String> {
    ["log", "--follow", "-z", FORMAT, "HEAD", "--"]
        .iter()
        .map(|s| s.to_string())
        .chain([path.to_string()])
        .collect()
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
    fn 按范围读历史的参数() {
        let args = |scope: &Scope, hidden: &[&str]| {
            let hidden: Vec<String> = hidden.iter().map(|s| s.to_string()).collect();
            scoped_log_args(scope, &hidden).join(" ")
        };
        // 什么都不限：和原来的一样
        assert_eq!(args(&Scope::All, &[]), LOG_ARGS.join(" "));
        // 隐藏的引用按种类排在各自的开关前面
        let hidden = args(
            &Scope::All,
            &[
                "refs/heads/wip/a",
                "refs/tags/v0.1",
                "refs/remotes/origin/old",
            ],
        );
        assert!(
            hidden.starts_with(
                "log --exclude=wip/a --branches --exclude=origin/old --remotes --exclude=v0.1 --tags HEAD --date-order"
            ),
            "{hidden}"
        );
        // 只看几个引用
        let only = args(
            &Scope::Refs(vec!["HEAD".into(), "refs/remotes/origin/main".into()]),
            &["refs/heads/wip/a"],
        );
        assert!(
            only.starts_with("log HEAD refs/remotes/origin/main --date-order"),
            "{only}"
        );
        assert!(only.ends_with(" --"));
    }

    #[test]
    fn 隐藏的分支上独有的提交不列出来() {
        let dir = std::env::temp_dir().join(format!("fr-log-scope-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| crate::git::run(&dir, args).unwrap();
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        git(&["checkout", "-q", "-b", "wip/a"]);
        git(&["commit", "-q", "--allow-empty", "-m", "only on wip"]);
        git(&["tag", "v0.1"]);
        git(&["checkout", "-q", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "on main"]);

        let subjects = |scope: &Scope, hidden: &[&str]| -> Vec<String> {
            let hidden: Vec<String> = hidden.iter().map(|s| s.to_string()).collect();
            let args = scoped_log_args(scope, &hidden);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = crate::git::run(&dir, &args).unwrap();
            let mut subjects: Vec<String> =
                parse(&out).iter().map(|c| c.subject.to_string()).collect();
            subjects.sort();
            subjects
        };
        assert_eq!(
            subjects(&Scope::All, &[]),
            ["base", "on main", "only on wip"]
        );
        // 分支藏了，标签还指着那个提交：还看得到
        assert_eq!(
            subjects(&Scope::All, &["refs/heads/wip/a"]),
            ["base", "on main", "only on wip"]
        );
        assert_eq!(
            subjects(&Scope::All, &["refs/heads/wip/a", "refs/tags/v0.1"]),
            ["base", "on main"]
        );
        assert_eq!(
            subjects(&Scope::Refs(vec!["HEAD".into()]), &[]),
            ["base", "on main"]
        );
        assert_eq!(
            subjects(&Scope::Refs(vec!["refs/heads/wip/a".into()]), &[]),
            ["base", "only on wip"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 按路径和改动内容找提交_标出两边各多的提交() {
        let dir = std::env::temp_dir().join(format!("fr-log-find-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/Repo")).unwrap();
        let git = |args: &[&str]| crate::git::run(&dir, args).unwrap();
        let commit = |path: &str, text: &str, subject: &str| {
            std::fs::write(dir.join(path), text).unwrap();
            git(&["add", "-A"]);
            git(&["commit", "-q", "-m", subject]);
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "user.email", "t@example.com"]);
        commit("README.md", "hello\n", "readme");
        commit("src/Repo/Diff.rs", "fn a() {}\n", "add diff");
        commit(
            "src/Repo/Diff.rs",
            "fn a() {}\nlet ignore_whitespace = 1;\n",
            "ignore blanks",
        );
        commit("src/new[1].txt", "x\n", "bracket");
        commit("README.md", "hello\nIGNORE_WHITESPACE\n", "mention it");
        git(&["checkout", "-q", "-b", "side", "HEAD~2"]);
        commit("side.txt", "s\n", "on side");
        git(&["checkout", "-q", "main"]);

        let found = |scope: &Scope, find: Find, needle: &str| -> Vec<String> {
            let args = find_log_args(scope, &[], find, needle);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = crate::git::run(&dir, &args).unwrap();
            parse(&out).iter().map(|c| c.subject.to_string()).collect()
        };
        // 路径：不分大小写，隔着目录也算，反斜杠当斜杠
        assert_eq!(
            found(&Scope::All, Find::Path, "diff.rs"),
            ["ignore blanks", "add diff"]
        );
        assert_eq!(
            found(&Scope::All, Find::Path, "repo\\diff"),
            ["ignore blanks", "add diff"]
        );
        // 通配符按字面算：`[1]` 不是「一个 1」，`*` 不是「随便什么」
        assert_eq!(found(&Scope::All, Find::Path, "new[1]"), ["bracket"]);
        assert!(found(&Scope::All, Find::Path, "new1").is_empty());
        assert!(found(&Scope::All, Find::Path, "*").is_empty());
        // 范围照样管用
        assert_eq!(found(&Scope::All, Find::Path, "side"), ["on side"]);
        assert!(found(&Scope::Refs(vec!["HEAD".into()]), Find::Path, "side").is_empty());
        // 改动内容：增加或删除过这几个字的提交，不分大小写
        assert_eq!(
            found(&Scope::All, Find::Content, "ignore_whitespace"),
            ["mention it", "ignore blanks"]
        );
        assert!(found(&Scope::All, Find::Content, "没有这几个字").is_empty());

        // main 比 side 多 2 个、少 1 个
        git(&["update-ref", "refs/remotes/origin/side", "refs/heads/side"]);
        let args = pending_args("origin/side");
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (ahead, behind) = parse_pending(&crate::git::run(&dir, &args).unwrap());
        assert_eq!((ahead.len(), behind.len()), (2, 1));
        let head = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
        assert!(ahead.iter().any(|id| **id == *head.trim()));
        let _ = std::fs::remove_dir_all(&dir);
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
