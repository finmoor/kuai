//! 拉取的第二步：获取完以后怎么并进来。
//!
//! 原来获取完再执行 `git pull`，它自己又获取一次：第二次联网不能取消，拿到的也可能和第一次的
//! 不一样。现在只获取一次（[`fetch_args`]），第二步照 `git pull` 的规矩选合并还是变基，用不联网的
//! 命令做（[`decide`]、[`finish_args`]）：
//! - 命令行的 `--rebase`（界面上「以变基代替合并」）最优先，盖过 `pull.ff=only`；
//! - `pull.ff=only`：只快进；
//! - `branch.<分支>.rebase`，没写再看 `pull.rebase`：true、interactive 变基，merges 变基并保留合并；
//! - 都没写就合并，`pull.ff` 定快进的方式。
//!
//! 合并的是获取写下的 FETCH_HEAD，和 `git pull` 一样，合并说明也一样（「Merge branch 'main' of
//! …」）。变基不带起点，git 用上游加 fork-point，也和 `git pull --rebase` 一样。和 `git pull` 不同的
//! 一处：没指定合并方式、两边又各有新提交时，`git pull`（2.33 起）报错要求先配置，这里照界面上说的
//! 合并。

/// 第二步怎么做。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// 合并；带着 `--ff` 或 `--no-ff`（`pull.ff` 写了的话）
    Merge(Option<&'static str>),
    /// 只快进，不能快进就失败
    FfOnly,
    /// 变基；`merges` 为真时保留合并（`--rebase-merges`）
    Rebase { merges: bool },
}

/// 读配置的参数：输出交给 [`decide`]。什么都没配时 git 以 1 退出，按空的算。
pub fn config_args() -> Vec<String> {
    ["config", "-z", "--get-regexp", r"^(pull|branch)\."]
        .map(String::from)
        .to_vec()
}

/// 第一步，获取（联网，可以取消）。不论 `fetch.writeFetchHEAD` 怎么配都写 FETCH_HEAD：第二步要合并它。
pub fn fetch_args() -> Vec<String> {
    ["-c", "fetch.writeFetchHEAD=true", "fetch", "--progress"]
        .map(String::from)
        .to_vec()
}

/// 合并 FETCH_HEAD 之前先看里面有没有要合并的那一行（上游分支）：上游在远程上被删了时一行都没有，
/// `git merge FETCH_HEAD` 会说「Already up to date」当作成功；这时把第一个参数写到标准错误、失败。
/// 后面的参数原样交给 `git merge`。
const MERGE: &str = "alias.fr-pull-merge=!f(){ m=\"$1\"; shift; \
     h=$(git rev-parse --git-path FETCH_HEAD) && grep -qv \"\tnot-for-merge\t\" \"$h\" 2>/dev/null \
     || { printf '%s\\n' \"$m\" >&2; return 1; }; git merge \"$@\" FETCH_HEAD; }; f";

/// 照配置定第二步。`config` 是 [`config_args`] 的输出，`branch` 是当前分支，`rebase` 是界面上
/// 选了「以变基代替合并」。
pub fn decide(config: &[u8], branch: &str, rebase: bool) -> Finish {
    let entries = parse(config);
    // 同一项写了几次以最后一次为准，和 git 一样
    let get = |key: &str| {
        entries
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_deref())
    };
    if rebase {
        return Finish::Rebase { merges: false };
    }
    let ff = get("pull.ff");
    if ff
        .flatten()
        .is_some_and(|value| value.eq_ignore_ascii_case("only"))
    {
        return Finish::FfOnly;
    }
    let setting = get(&format!("branch.{branch}.rebase"))
        .and_then(rebase_value)
        .or_else(|| get("pull.rebase").and_then(rebase_value));
    match setting {
        Some(Some(merges)) => Finish::Rebase { merges },
        _ => Finish::Merge(match ff.and_then(bool_value) {
            Some(true) => Some("--ff"),
            Some(false) => Some("--no-ff"),
            None => None,
        }),
    }
}

/// 第二步的参数。`missing` 是 FETCH_HEAD 里没有上游分支时报的话（见 [`MERGE`]）。
pub fn finish_args(finish: Finish, autostash: bool, missing: &str) -> Vec<String> {
    let mut args: Vec<String> = match finish {
        Finish::Rebase { merges } => {
            let mut args = vec!["rebase".to_string()];
            if merges {
                args.push("--rebase-merges".into());
            }
            args
        }
        Finish::Merge(_) | Finish::FfOnly => {
            // --no-edit：要合并时直接用默认的合并说明，不弹编辑器
            let mut args = ["-c", MERGE, "fr-pull-merge", missing, "--no-edit"]
                .map(String::from)
                .to_vec();
            match finish {
                Finish::FfOnly => args.push("--ff-only".into()),
                Finish::Merge(Some(flag)) => args.push(flag.into()),
                _ => {}
            }
            args
        }
    };
    if autostash {
        args.push("--autostash".into());
    }
    args
}

/// 第二步给人看的写法（活动记录、失败的详情）：合并写成等价的 `git merge --no-edit … FETCH_HEAD`，
/// 不露出 [`MERGE`] 那段别名。
pub fn finish_shown(finish: Finish, autostash: bool) -> String {
    let mut args = finish_args(finish, autostash, "");
    if let Some(at) = args.iter().position(|arg| arg == "fr-pull-merge") {
        // 「-c 别名 fr-pull-merge 报的话」换成 merge，合并的对象写在最后
        args.splice(..at + 2, ["merge".to_string()]);
        args.push("FETCH_HEAD".into());
    }
    format!("git {}", args.join(" "))
}

/// 拆开 `git config -z --get-regexp` 的输出：每项是「名字\n值」，以 NUL 结尾；只写了名字没写值的
/// 没有换行，值是 None。名字里的节和变量 git 已经转成小写，分支名（子节）原样。
fn parse(out: &[u8]) -> Vec<(String, Option<String>)> {
    String::from_utf8_lossy(out)
        .split('\0')
        .filter(|item| !item.is_empty())
        .map(|item| match item.split_once('\n') {
            Some((name, value)) => (name.to_string(), Some(value.to_string())),
            None => (item.to_string(), None),
        })
        .collect()
}

/// git 的布尔值。只写了名字没写值算 true；不认识的值是 None。
fn bool_value(value: Option<&str>) -> Option<bool> {
    let Some(value) = value else {
        return Some(true);
    };
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

/// 变基的设置：Some(None) 是不变基，Some(Some(merges)) 是变基；不认识的值当没写（None）。
/// interactive 要人在编辑器里改清单，这里做不了，按普通的变基；preserve 是老写法，git 已经不认了。
fn rebase_value(value: Option<&str>) -> Option<Option<bool>> {
    match value.map(str::to_ascii_lowercase).as_deref() {
        Some("merges" | "m") => Some(Some(true)),
        Some("interactive" | "i" | "preserve" | "p") => Some(Some(false)),
        _ => bool_value(value).map(|on| on.then_some(false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(items: &[&str]) -> Vec<u8> {
        items
            .iter()
            .flat_map(|item| [item.as_bytes(), b"\0"].concat())
            .collect()
    }

    #[test]
    fn 第二步给人看的写法() {
        assert_eq!(
            finish_shown(Finish::FfOnly, true),
            "git merge --no-edit --ff-only --autostash FETCH_HEAD"
        );
        assert_eq!(
            finish_shown(Finish::Merge(None), false),
            "git merge --no-edit FETCH_HEAD"
        );
        assert_eq!(
            finish_shown(Finish::Merge(Some("--no-ff")), false),
            "git merge --no-edit --no-ff FETCH_HEAD"
        );
        assert_eq!(
            finish_shown(Finish::Rebase { merges: true }, true),
            "git rebase --rebase-merges --autostash"
        );
    }

    #[test]
    fn 照配置选合并还是变基() {
        let none = config(&[]);
        assert_eq!(decide(&none, "main", false), Finish::Merge(None));
        // 界面上选了变基，盖过 pull.ff=only
        let ff_only = config(&["pull.ff\nonly"]);
        assert_eq!(
            decide(&ff_only, "main", true),
            Finish::Rebase { merges: false }
        );
        assert_eq!(decide(&ff_only, "main", false), Finish::FfOnly);
        // pull.ff=only 也盖过配置里的变基
        let both = config(&["pull.rebase\ntrue", "pull.ff\nOnly"]);
        assert_eq!(decide(&both, "main", false), Finish::FfOnly);
        // 分支自己的设置优先；子节（分支名）分大小写
        let branch = config(&[
            "branch.main.remote\norigin",
            "pull.rebase\ntrue",
            "branch.main.rebase\nfalse",
            "branch.Feat.rebase\nmerges",
        ]);
        assert_eq!(decide(&branch, "main", false), Finish::Merge(None));
        assert_eq!(
            decide(&branch, "Feat", false),
            Finish::Rebase { merges: true }
        );
        assert_eq!(
            decide(&branch, "feat", false),
            Finish::Rebase { merges: false }
        );
        // 只写了名字算 true；同一项以最后一次为准；不认识的值当没写
        let bare = config(&["pull.rebase"]);
        assert_eq!(
            decide(&bare, "main", false),
            Finish::Rebase { merges: false }
        );
        let twice = config(&["pull.rebase\ntrue", "pull.rebase\nno"]);
        assert_eq!(decide(&twice, "main", false), Finish::Merge(None));
        let odd = config(&["pull.rebase\nsometimes"]);
        assert_eq!(decide(&odd, "main", false), Finish::Merge(None));
        // pull.ff 定合并时快进的方式
        let no_ff = config(&["pull.ff\nfalse"]);
        assert_eq!(
            decide(&no_ff, "main", false),
            Finish::Merge(Some("--no-ff"))
        );
        let ff = config(&["pull.ff\ntrue"]);
        assert_eq!(decide(&ff, "main", false), Finish::Merge(Some("--ff")));
    }

    #[test]
    fn 第二步的参数() {
        let rebase = finish_args(Finish::Rebase { merges: true }, true, "x");
        assert_eq!(rebase, ["rebase", "--rebase-merges", "--autostash"]);
        let merge = finish_args(Finish::Merge(Some("--no-ff")), false, "没有上游");
        assert_eq!(merge[..2], ["-c".to_string(), MERGE.to_string()]);
        assert_eq!(
            merge[2..],
            ["fr-pull-merge", "没有上游", "--no-edit", "--no-ff"]
        );
        let ff_only = finish_args(Finish::FfOnly, true, "x");
        assert_eq!(ff_only[4..], ["--no-edit", "--ff-only", "--autostash"]);
    }

    /// 真的 git：获取一次以后合并 FETCH_HEAD，合并说明和 `git pull` 的一样；上游在远程上没了时失败。
    #[test]
    fn 获取一次再合并() {
        let root = std::env::temp_dir().join(format!("fr-pull-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let git = |dir: &str, args: &[&str]| {
            let out = crate::git::command(&root.join(dir), args).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (
                out.status.success(),
                text,
                String::from_utf8_lossy(&out.stderr).to_string(),
            )
        };
        let ok = |dir: &str, args: &[&str]| {
            let (success, text, err) = git(dir, args);
            assert!(success, "{args:?}: {err}");
            text
        };
        ok("", &["init", "-q", "--bare", "-b", "main", "remote.git"]);
        ok("", &["init", "-q", "-b", "main", "up"]);
        ok("", &["init", "-q", "-b", "main", "down"]);
        for dir in ["up", "down"] {
            ok(dir, &["config", "user.name", "t"]);
            ok(dir, &["config", "user.email", "t@example.com"]);
            ok(dir, &["remote", "add", "origin", "../remote.git"]);
        }
        ok("up", &["commit", "-q", "--allow-empty", "-m", "base"]);
        ok("up", &["push", "-q", "origin", "main"]);
        ok("down", &["fetch", "-q"]);
        ok(
            "down",
            &["checkout", "-q", "-b", "main", "--track", "origin/main"],
        );
        ok("down", &["commit", "-q", "--allow-empty", "-m", "local"]);
        ok("up", &["commit", "-q", "--allow-empty", "-m", "upstream"]);
        ok("up", &["push", "-q", "origin", "main"]);

        let fetch = fetch_args();
        let fetch: Vec<&str> = fetch.iter().map(String::as_str).collect();
        // fetch.writeFetchHEAD=false 也照样写
        ok("down", &["config", "fetch.writeFetchHEAD", "false"]);
        ok("down", &fetch);
        let config = crate::git::command(&root.join("down"), &config_args())
            .output()
            .unwrap();
        let finish = decide(&config.stdout, "main", false);
        assert_eq!(finish, Finish::Merge(None));
        let merge = finish_args(finish, false, "missing upstream");
        let merge: Vec<&str> = merge.iter().map(String::as_str).collect();
        ok("down", &merge);
        let subject = ok("down", &["log", "-1", "--format=%s"]);
        assert!(subject.starts_with("Merge branch 'main' of "), "{subject}");

        // 上游在远程上被删了：FETCH_HEAD 里没有要合并的那一行，失败，什么都不动
        let head = ok("down", &["rev-parse", "HEAD"]);
        ok("down", &["config", "branch.main.merge", "refs/heads/gone"]);
        ok("down", &fetch);
        let (success, _, err) = git("down", &merge);
        assert!(!success);
        assert!(err.contains("missing upstream"), "{err}");
        assert_eq!(ok("down", &["rev-parse", "HEAD"]), head);
        let _ = std::fs::remove_dir_all(&root);
    }
}
