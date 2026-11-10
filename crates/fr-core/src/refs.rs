//! 分支、远程分支、标签和贮藏（stash）。

/// 列出本地分支、远程分支和标签。字段之间用 0x1f 分隔；附注标签取它指向的提交（`*objectname`）。
pub fn args() -> Vec<String> {
    vec![
        "for-each-ref".into(),
        "--format=%(refname)%1f%(objectname)%1f%(*objectname)%1f%(HEAD)%1f%(upstream:short)%1f%(upstream:track,nobracket)".into(),
        "refs/heads".into(),
        "refs/remotes".into(),
        "refs/tags".into(),
    ]
}

/// 列出贮藏。
pub fn stash_args() -> Vec<String> {
    ["stash", "list", "-z", "--format=%gd%x1f%H%x1f%ct%x1f%gs"]
        .map(String::from)
        .to_vec()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refs {
    pub branches: Vec<Branch>,
    /// 远程分支，不含 `origin/HEAD` 这种符号引用
    pub remotes: Vec<Branch>,
    pub tags: Vec<Tag>,
    pub stashes: Vec<Stash>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// 短名，比如 `main`、`origin/main`
    pub name: String,
    /// 指向的提交
    pub target: String,
    /// 当前检出的分支
    pub current: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// 上游分支已经不在了
    pub gone: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    /// 指向的提交（附注标签已经解开）
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    /// `stash@{0}`
    pub selector: String,
    pub target: String,
    pub time: i64,
    /// 比如 `WIP on main: 1234abc 说明` 或 `On main: 自己写的说明`
    pub message: String,
}

/// 解析 [`args`] 和 [`stash_args`] 的输出。
pub fn parse(refs_out: &[u8], stash_out: &[u8]) -> Refs {
    let mut refs = Refs::default();
    for line in String::from_utf8_lossy(refs_out).lines() {
        let f: Vec<&str> = line.split('\x1f').collect();
        if f.len() < 6 {
            continue;
        }
        let (refname, object, peeled, head, upstream, track) = (f[0], f[1], f[2], f[3], f[4], f[5]);
        let target = if peeled.is_empty() { object } else { peeled }.to_string();
        if let Some(name) = refname.strip_prefix("refs/heads/") {
            let (ahead, behind, gone) = parse_track(track);
            refs.branches.push(Branch {
                name: name.to_string(),
                target,
                current: head == "*",
                upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
                ahead,
                behind,
                gone,
            });
        } else if let Some(name) = refname.strip_prefix("refs/remotes/") {
            if name.ends_with("/HEAD") {
                continue;
            }
            refs.remotes.push(Branch {
                name: name.to_string(),
                target,
                current: false,
                upstream: None,
                ahead: 0,
                behind: 0,
                gone: false,
            });
        } else if let Some(name) = refname.strip_prefix("refs/tags/") {
            refs.tags.push(Tag {
                name: name.to_string(),
                target,
            });
        }
    }
    for record in String::from_utf8_lossy(stash_out).split('\0') {
        let record = record.trim_start_matches('\n');
        let f: Vec<&str> = record.splitn(4, '\x1f').collect();
        if f.len() < 4 {
            continue;
        }
        refs.stashes.push(Stash {
            selector: f[0].to_string(),
            target: f[1].to_string(),
            time: f[2].parse().unwrap_or(0),
            message: f[3].to_string(),
        });
    }
    refs
}

/// `ahead 1, behind 2`、`gone` 或空。
fn parse_track(track: &str) -> (u32, u32, bool) {
    let (mut ahead, mut behind) = (0, 0);
    for part in track.split(", ") {
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.parse().unwrap_or(0);
        }
    }
    (ahead, behind, track == "gone")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析分支远程标签和贮藏() {
        let refs_out = [
            "refs/heads/main\x1faaa\x1f\x1f*\x1forigin/main\x1fahead 1, behind 2",
            "refs/heads/feature/x\x1fbbb\x1f\x1f \x1forigin/feature/x\x1fgone",
            "refs/remotes/origin/HEAD\x1faaa\x1f\x1f \x1f\x1f",
            "refs/remotes/origin/main\x1fccc\x1f\x1f \x1f\x1f",
            "refs/tags/v1\x1ftagobj\x1fcommit1\x1f \x1f\x1f",
            "refs/tags/v2\x1fcommit2\x1f\x1f \x1f\x1f",
        ]
        .join("\n");
        let stash_out = "stash@{0}\x1fsss\x1f1700000000\x1fOn main: 试验\0\nstash@{1}\x1fttt\x1f1600000000\x1fWIP on main: abc 说明\0";
        let refs = parse(refs_out.as_bytes(), stash_out.as_bytes());
        assert_eq!(refs.branches.len(), 2);
        let main = &refs.branches[0];
        assert!(main.current);
        assert_eq!(main.upstream.as_deref(), Some("origin/main"));
        assert_eq!((main.ahead, main.behind), (1, 2));
        assert!(refs.branches[1].gone);
        assert_eq!(refs.remotes.len(), 1);
        assert_eq!(refs.remotes[0].name, "origin/main");
        assert_eq!(refs.tags[0].target, "commit1");
        assert_eq!(refs.tags[1].target, "commit2");
        assert_eq!(refs.stashes.len(), 2);
        assert_eq!(refs.stashes[1].selector, "stash@{1}");
        assert_eq!(refs.stashes[0].message, "On main: 试验");
    }
}
