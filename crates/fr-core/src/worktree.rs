//! 工作树（`git worktree`）：同一个仓库的另一份工作目录，可以同时检出不同的分支。
//! 这里是读清单和各条命令的参数，以及路径怎么比、怎么写短。

/// `git worktree list` 里的一个工作树。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Worktree {
    /// 工作树的目录，git 给的写法（Windows 上也是正斜杠）
    pub path: String,
    pub head: Option<String>,
    /// 检出的分支（去掉了 `refs/heads/`）；分离的 HEAD、裸仓库没有
    pub branch: Option<String>,
    pub bare: bool,
    /// 锁定了：清理时不会移除。值是锁定时写的原因，可以是空的
    pub locked: Option<String>,
    /// 目录已经不在了，`git worktree prune` 会清掉它的记录。值是 git 说的原因
    pub prunable: Option<String>,
    /// 清单里的第一个：主工作树
    pub main: bool,
}

impl Worktree {
    /// 目录名：工作树显示的名字。
    pub fn name(&self) -> &str {
        let path = self.path.trim_end_matches(['/', '\\']);
        path.rsplit(['/', '\\']).next().unwrap_or(path)
    }
}

pub fn list_args() -> Vec<String> {
    ["worktree", "list", "--porcelain"]
        .map(String::from)
        .to_vec()
}

/// 解析 `git worktree list --porcelain`：一个工作树一段，段和段之间空一行。
pub fn parse(out: &[u8]) -> Vec<Worktree> {
    let text = String::from_utf8_lossy(out);
    let mut list: Vec<Worktree> = Vec::new();
    for line in text.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key == "worktree" {
            list.push(Worktree {
                path: value.to_string(),
                main: list.is_empty(),
                ..Default::default()
            });
            continue;
        }
        let Some(tree) = list.last_mut() else {
            continue;
        };
        match key {
            "HEAD" => tree.head = Some(value.to_string()),
            "branch" => {
                let name = value.strip_prefix("refs/heads/").unwrap_or(value);
                tree.branch = Some(name.to_string());
            }
            "bare" => tree.bare = true,
            "locked" => tree.locked = Some(value.to_string()),
            "prunable" => tree.prunable = Some(value.to_string()),
            _ => {}
        }
    }
    list
}

/// 新建工作树时检出什么。
pub enum Checkout<'a> {
    /// 新建一个分支，从 `base`（分支、标签、提交）开始
    NewBranch { name: &'a str, base: &'a str },
    /// 检出已有的分支
    Branch(&'a str),
}

pub fn add_args(path: &str, checkout: Checkout) -> Vec<String> {
    let mut args = vec!["worktree".to_string(), "add".to_string()];
    match checkout {
        Checkout::NewBranch { name, base } => {
            args.extend(["-b".to_string(), name.to_string(), path.to_string()]);
            if !base.is_empty() {
                args.push(base.to_string());
            }
        }
        Checkout::Branch(name) => args.extend([path.to_string(), name.to_string()]),
    }
    args
}

/// 移除工作树。有没提交的改动、没跟踪的文件时 git 不肯移除，`force` 才行。
pub fn remove_args(path: &str, force: bool) -> Vec<String> {
    let mut args = vec!["worktree".to_string(), "remove".to_string()];
    if force {
        args.push("--force".into());
    }
    args.push(path.to_string());
    args
}

/// 清掉目录已经不在的工作树的记录。
pub fn prune_args() -> Vec<String> {
    ["worktree", "prune"].map(String::from).to_vec()
}

pub fn lock_args(path: &str, lock: bool) -> Vec<String> {
    let verb = if lock { "lock" } else { "unlock" };
    ["worktree", verb, path].map(String::from).to_vec()
}

/// 路径统一成正斜杠、去掉结尾的斜杠；Windows 的路径（带盘符）不分大小写，转成小写。
fn canonical(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path.trim_end_matches('/');
    let windows = path.as_bytes().get(1) == Some(&b':');
    match windows {
        true => path.to_lowercase(),
        false => path.to_string(),
    }
}

/// 两个路径是不是同一个目录（斜杠的方向、结尾的斜杠、Windows 上的大小写不算区别）。
pub fn same_path(a: &str, b: &str) -> bool {
    canonical(a) == canonical(b)
}

/// 新建工作树默认放在哪：仓库旁边的 `<仓库>.worktrees/<名字>`，名字是分支名把斜杠换成横线。
/// 分隔符跟着仓库路径的写法。
pub fn default_path(repo: &str, branch: &str) -> String {
    let root = repo.trim_end_matches(['/', '\\']);
    let sep = if root.contains('\\') { '\\' } else { '/' };
    let name: String = branch
        .trim()
        .chars()
        .map(|c| if matches!(c, '/' | '\\') { '-' } else { c })
        .collect();
    format!("{root}.worktrees{sep}{name}")
}

/// 工作树的位置写短一点：仓库里面的写相对路径，仓库旁边的写 `../…`，别处的原样。
pub fn short_path(path: &str, repo: &str) -> String {
    let (full, root) = (canonical(path), canonical(repo));
    // 截原来的写法（保留大小写），长度按统一以后的算：只换了斜杠和大小写，字节数不变
    let original = path.replace('\\', "/");
    let original = original.trim_end_matches('/');
    // 转小写改了字节数（个别非 ASCII 字母）：按位置截不了，原样给
    if full == root || full.len() != original.len() {
        return original.to_string();
    }
    if let Some(rest) = full.strip_prefix(&format!("{root}/")) {
        return original[original.len() - rest.len()..].to_string();
    }
    if let Some((parent, _)) = root.rsplit_once('/')
        && let Some(rest) = full.strip_prefix(&format!("{parent}/"))
    {
        return format!("../{}", &original[original.len() - rest.len()..]);
    }
    original.to_string()
}

/// 这个工作树是哪个命令行工具建的（按它们放工作树的目录认）；认不出来没有。
pub fn made_by(path: &str) -> Option<&'static str> {
    let path = path.replace('\\', "/");
    if path.contains("/.claude/worktrees/") {
        Some("Claude Code")
    } else if path.contains("/.codex/worktrees/") {
        Some("Codex")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析清单() {
        let out = "worktree /data/ops-bridge\n\
                   HEAD a41c9e0000000000000000000000000000000000\n\
                   branch refs/heads/main\n\
                   \n\
                   worktree /data/ops-bridge.worktrees/feat-login\n\
                   HEAD 9c2e1a7000000000000000000000000000000000\n\
                   branch refs/heads/feat/login\n\
                   locked 正在评审\n\
                   \n\
                   worktree /home/feng/.codex/worktrees/a41c/ops-bridge\n\
                   HEAD 3e7e7e6000000000000000000000000000000000\n\
                   detached\n\
                   \n\
                   worktree /tmp/review-0412\n\
                   HEAD 77d0b15000000000000000000000000000000000\n\
                   branch refs/heads/review/0412\n\
                   prunable gitdir file points to non-existent location\n\
                   \n\
                   worktree /data/locked-no-reason\n\
                   HEAD 77d0b15000000000000000000000000000000000\n\
                   detached\n\
                   locked\n";
        let list = parse(out.as_bytes());
        assert_eq!(list.len(), 5);
        assert!(list[0].main && !list[1].main);
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert_eq!(list[1].branch.as_deref(), Some("feat/login"));
        assert_eq!(list[1].locked.as_deref(), Some("正在评审"));
        assert_eq!(list[1].name(), "feat-login");
        assert_eq!(list[2].branch, None);
        assert_eq!(
            list[2].head.as_deref(),
            Some("3e7e7e6000000000000000000000000000000000")
        );
        assert!(list[3].prunable.is_some() && list[3].locked.is_none());
        assert_eq!(list[4].locked.as_deref(), Some(""));
        assert!(parse(b"").is_empty());
    }

    #[test]
    fn 裸仓库() {
        let list = parse(
            b"worktree /srv/repo.git\nbare\n\nworktree /srv/wt\nHEAD abc\nbranch refs/heads/x\n",
        );
        assert!(list[0].bare && list[0].main && list[0].head.is_none());
        assert_eq!(list[1].branch.as_deref(), Some("x"));
    }

    #[test]
    fn 命令的参数() {
        let new = Checkout::NewBranch {
            name: "feat/x",
            base: "main",
        };
        assert_eq!(
            add_args("/w/x", new),
            ["worktree", "add", "-b", "feat/x", "/w/x", "main"]
        );
        let from_head = Checkout::NewBranch {
            name: "feat/x",
            base: "",
        };
        assert_eq!(
            add_args("/w/x", from_head),
            ["worktree", "add", "-b", "feat/x", "/w/x"]
        );
        assert_eq!(
            add_args("/w/x", Checkout::Branch("fix/y")),
            ["worktree", "add", "/w/x", "fix/y"]
        );
        assert_eq!(remove_args("/w/x", false), ["worktree", "remove", "/w/x"]);
        assert_eq!(
            remove_args("/w/x", true),
            ["worktree", "remove", "--force", "/w/x"]
        );
        assert_eq!(lock_args("/w/x", true), ["worktree", "lock", "/w/x"]);
        assert_eq!(lock_args("/w/x", false), ["worktree", "unlock", "/w/x"]);
    }

    #[test]
    fn 路径怎么比() {
        assert!(same_path("C:/Users/Me/repo", r"c:\users\me\repo\"));
        assert!(same_path("/data/repo/", "/data/repo"));
        // Linux 上大小写不同是两个目录
        assert!(!same_path("/data/Repo", "/data/repo"));
    }

    #[test]
    fn 默认放在仓库旁边() {
        assert_eq!(
            default_path("/data/ops-bridge/", "feat/login"),
            "/data/ops-bridge.worktrees/feat-login"
        );
        assert_eq!(
            default_path(r"C:\src\kuai", "fix/a/b"),
            r"C:\src\kuai.worktrees\fix-a-b"
        );
    }

    #[test]
    fn 位置写短() {
        let repo = "/data/platform/ops-bridge";
        assert_eq!(short_path(repo, repo), repo);
        assert_eq!(
            short_path("/data/platform/ops-bridge/.claude/worktrees/fix", repo),
            ".claude/worktrees/fix"
        );
        assert_eq!(
            short_path("/data/platform/ops-bridge.worktrees/feat-login", repo),
            "../ops-bridge.worktrees/feat-login"
        );
        assert_eq!(short_path("/tmp/review", repo), "/tmp/review");
        // Windows：斜杠方向、大小写不一样也认得出来，显示的保留原来的大小写
        assert_eq!(
            short_path("C:/Src/Kuai.worktrees/Feat-X", r"c:\src\kuai"),
            "../Kuai.worktrees/Feat-X"
        );
    }

    #[test]
    fn 哪个工具建的() {
        assert_eq!(
            made_by("/data/repo/.claude/worktrees/fix-timeout"),
            Some("Claude Code")
        );
        assert_eq!(
            made_by(r"C:\Users\Me\.codex\worktrees\a41c\repo"),
            Some("Codex")
        );
        assert_eq!(made_by("/data/repo.worktrees/x"), None);
    }
}
