//! 比较两端：两个提交、两个分支，或者其中一端是工作区（本地修改）。
//!
//! 两端直接比（`git diff A B`），不是从合并基算起的那种（`A...B`）：看到的就是「把 A 变成 B
//! 要改什么」。两边各自多了哪些提交另外列（[`only_in_args`]、[`count_args`]）。
//!
//! 文件清单和每个文件的改动分开取：两个分支可能差几百个文件，一次全取出来又慢又占地方，
//! 点哪个文件取哪个。

use crate::patch;

/// 比较的一端。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// 一个提交：完整的引用名（`refs/heads/main`）、`HEAD` 或哈希
    Rev(String),
    /// 工作区现在的样子（包括已暂存和未暂存的改动）
    Worktree,
}

/// 两端之间有改动的一个文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub path: String,
    /// 改名、复制时原来的路径
    pub orig_path: Option<String>,
    /// `A` 新增、`D` 删除、`M` 修改、`R` 改名、`C` 复制、`T` 类型变了
    pub status: char,
    /// 增删的行数；二进制文件没有
    pub lines: Option<(u32, u32)>,
}

/// 两端怎么写进 `git diff` 的参数里。两端都是工作区时没有可比的。
/// 基准是工作区、对比是提交时用 `-R` 倒过来。
fn ends(base: &End, other: &End) -> Option<Vec<String>> {
    Some(match (base, other) {
        (End::Rev(a), End::Rev(b)) => vec![a.clone(), b.clone()],
        (End::Rev(a), End::Worktree) => vec![a.clone()],
        (End::Worktree, End::Rev(b)) => vec!["-R".into(), b.clone()],
        (End::Worktree, End::Worktree) => return None,
    })
}

/// 两端之间改了哪些文件：状态和增删行数一次取回来（[`parse_files`]）。
pub fn files_args(base: &End, other: &End) -> Option<Vec<String>> {
    let mut args: Vec<String> = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--raw",
        "--numstat",
        "-z",
        "-M",
    ]
    .map(String::from)
    .to_vec();
    args.extend(ends(base, other)?);
    args.push("--".into());
    Some(args)
}

/// 解析 [`files_args`] 的输出：前一半是 `--raw` 的记录（状态和路径），后一半是 `--numstat` 的
/// （增删行数和路径），都以 NUL 分隔；改名、复制的记录后面跟两个路径。
pub fn parse_files(out: &[u8]) -> Vec<File> {
    let text = String::from_utf8_lossy(out);
    let mut fields = text.split('\0');
    let mut files: Vec<File> = Vec::new();
    let mut stats: Vec<(String, Option<(u32, u32)>)> = Vec::new();
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        if let Some(raw) = field.strip_prefix(':') {
            // `:100644 100644 <旧> <新> R100`
            let status = raw
                .rsplit(' ')
                .next()
                .and_then(|status| status.chars().next())
                .unwrap_or('M');
            let Some(first) = fields.next() else {
                break;
            };
            let (path, orig_path) = match status {
                'R' | 'C' => match fields.next() {
                    Some(new) => (new.to_string(), Some(first.to_string())),
                    None => break,
                },
                _ => (first.to_string(), None),
            };
            files.push(File {
                path,
                orig_path,
                status,
                lines: None,
            });
            continue;
        }
        // `增\t删\t路径`；改名时路径那一栏是空的，后面跟旧、新两个路径；二进制文件是 `-\t-`
        let mut parts = field.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let path = match path.is_empty() {
            true => {
                let (Some(_old), Some(new)) = (fields.next(), fields.next()) else {
                    break;
                };
                new.to_string()
            }
            false => path.to_string(),
        };
        let lines = match (added.parse(), removed.parse()) {
            (Ok(added), Ok(removed)) => Some((added, removed)),
            _ => None,
        };
        stats.push((path, lines));
    }
    // 两半的顺序一样；万一对不上（很旧的 git）就按路径找
    for (ix, file) in files.iter_mut().enumerate() {
        let hit = match stats.get(ix) {
            Some((path, lines)) if *path == file.path => Some(*lines),
            _ => stats
                .iter()
                .find(|(path, _)| *path == file.path)
                .map(|(_, lines)| *lines),
        };
        file.lines = hit.flatten();
    }
    files
}

/// 一个文件在两端之间的改动。改名的把原路径也带上，才看得到改名前后的对比。
/// 前缀写死成 a/ b/，不受用户的 diff 配置影响。
pub fn patch_args(
    base: &End,
    other: &End,
    path: &str,
    orig_path: Option<&str>,
) -> Option<Vec<String>> {
    let mut args: Vec<String> = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "-M",
        "-U3",
    ]
    .map(String::from)
    .to_vec();
    args.extend(ends(base, other)?);
    args.push("--".into());
    args.push(path.to_string());
    args.extend(orig_path.map(String::from));
    Some(args)
}

/// 工作区里没跟踪的文件：对比是工作区时显示成新文件，基准是工作区时显示成删掉的。
/// 退出码 1 表示有差异，属于正常结果。
pub fn untracked_patch_args(path: &str, worktree_is_base: bool) -> Vec<String> {
    let mut args = patch::untracked_args(path);
    if worktree_is_base {
        // 放在 `diff` 后面
        args.insert(1, "-R".into());
    }
    args
}

/// 这一端按提交算是哪个：工作区按 HEAD 算（数两边各多了哪些提交时用）。
pub fn rev_of(end: &End) -> &str {
    match end {
        End::Rev(rev) => rev,
        End::Worktree => "HEAD",
    }
}

/// 只在 `have` 上、不在 `lack` 上的提交，最多 `limit` 个。格式和提交历史一样，用
/// [`crate::log::parse`] 解析。
pub fn only_in_args(have: &str, lack: &str, limit: usize) -> Vec<String> {
    let scope = crate::log::Scope::Refs(vec![have.to_string(), format!("^{lack}")]);
    let mut args = crate::log::scoped_log_args(&scope, &[]);
    args.insert(1, format!("--max-count={limit}"));
    args
}

/// 两边各多了几个提交：输出是 `<只在 base 上的>\t<只在 other 上的>`（[`parse_count`]）。
pub fn count_args(base: &str, other: &str) -> Vec<String> {
    vec![
        "rev-list".into(),
        "--left-right".into(),
        "--count".into(),
        format!("{base}...{other}"),
        "--".into(),
    ]
}

/// 解析 [`count_args`] 的输出：（只在基准上的，只在对比上的）。
pub fn parse_count(out: &[u8]) -> Option<(usize, usize)> {
    let text = String::from_utf8_lossy(out);
    let mut parts = text.split_whitespace();
    let behind = parts.next()?.parse().ok()?;
    let ahead = parts.next()?.parse().ok()?;
    Some((behind, ahead))
}

/// 把用户填的哈希、引用名解析成提交的完整哈希（`^{commit}`：标签解开，不是提交的报错）。
/// 以 `-` 开头的会被当成选项，调用的地方先挡掉（旧版 git 没有 `--end-of-options`）。
pub fn resolve_args(rev: &str) -> Vec<String> {
    vec![
        "rev-parse".into(),
        "--verify".into(),
        "--quiet".into(),
        format!("{rev}^{{commit}}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rev(name: &str) -> End {
        End::Rev(name.to_string())
    }

    #[test]
    fn 两端写进参数里() {
        let tail = |args: Option<Vec<String>>| {
            let args = args.unwrap();
            let from = args.iter().position(|arg| arg == "-M").unwrap();
            args[from + 1..].join(" ")
        };
        assert_eq!(tail(files_args(&rev("a"), &rev("b"))), "a b --");
        assert_eq!(tail(files_args(&rev("a"), &End::Worktree)), "a --");
        // 基准是工作区：倒过来比
        assert_eq!(tail(files_args(&End::Worktree, &rev("b"))), "-R b --");
        assert!(files_args(&End::Worktree, &End::Worktree).is_none());
        let patch = patch_args(&rev("a"), &rev("b"), "new.rs", Some("old.rs")).unwrap();
        assert_eq!(patch[patch.len() - 5..].join(" "), "a b -- new.rs old.rs");
        assert!(patch.contains(&"-U3".to_string()));
        assert_eq!(untracked_patch_args("x", true)[..2].join(" "), "diff -R");
        assert_eq!(untracked_patch_args("x", false)[0], "diff");
    }

    #[test]
    fn 解析文件清单() {
        let out = concat!(
            ":100644 100644 1111111 2222222 M\0src/a.rs\0",
            ":000000 100644 0000000 3333333 A\0docs/新 文件.md\0",
            ":100644 100644 4444444 5555555 R087\0old/name.rs\0new/name.rs\0",
            ":100644 000000 6666666 0000000 D\0gone.txt\0",
            ":100644 100644 7777777 8888888 M\0logo.png\0",
            "13\t6\tsrc/a.rs\0",
            "40\t0\tdocs/新 文件.md\0",
            "2\t1\t\0old/name.rs\0new/name.rs\0",
            "0\t9\tgone.txt\0",
            "-\t-\tlogo.png\0",
        );
        let files = parse_files(out.as_bytes());
        let brief: Vec<_> = files
            .iter()
            .map(|file| (file.status, file.path.as_str(), file.lines))
            .collect();
        assert_eq!(
            brief,
            [
                ('M', "src/a.rs", Some((13, 6))),
                ('A', "docs/新 文件.md", Some((40, 0))),
                ('R', "new/name.rs", Some((2, 1))),
                ('D', "gone.txt", Some((0, 9))),
                // 二进制文件没有行数
                ('M', "logo.png", None),
            ]
        );
        assert_eq!(files[2].orig_path.as_deref(), Some("old/name.rs"));
        assert!(files[0].orig_path.is_none());
        assert!(parse_files(b"").is_empty());
    }

    #[test]
    fn 两边各多了哪些提交() {
        let args = only_in_args("refs/heads/feat", "refs/heads/main", 300);
        assert_eq!(args[0], "log");
        assert_eq!(
            args[1..4].join(" "),
            "--max-count=300 refs/heads/feat ^refs/heads/main"
        );
        assert_eq!(args.last().unwrap(), "--");
        assert_eq!(
            count_args("main", "feat")[3],
            "main...feat",
            "左边是基准，右边是对比"
        );
        assert_eq!(parse_count(b"2\t5\n"), Some((2, 5)));
        assert_eq!(parse_count(b""), None);
        assert_eq!(rev_of(&End::Worktree), "HEAD");
        assert_eq!(resolve_args("v1.0").last().unwrap(), "v1.0^{commit}");
    }
}
