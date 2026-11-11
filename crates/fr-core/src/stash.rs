//! 贮藏（`git stash`）的参数。

/// `git stash push` 的参数；`message` 是贮藏的说明，`include_untracked` 是连未跟踪的文件一起收。
///
/// 带上未跟踪的文件时，要给这一条命令关掉「路径按字面理解」（[`crate::git::command`] 给所有 git
/// 进程都设了 `GIT_LITERAL_PATHSPECS=1`）。git 把未跟踪的文件收进贮藏以后，自己会用带魔法前缀的
/// 路径（`:/`）去清工作区；路径按字面理解的话，这一步什么也清不掉：文件进了贮藏，工作区里却还留着
/// 一份，之后弹出时撞上「已经存在」，贮藏就弹不出来了。这条命令我们自己不给路径，关掉没有别的影响。
pub fn push_args(message: Option<&str>, include_untracked: bool) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if include_untracked {
        args.push("--no-literal-pathspecs".into());
    }
    args.extend(["stash".to_string(), "push".to_string()]);
    if include_untracked {
        args.push("--include-untracked".into());
    }
    if let Some(message) = message.filter(|m| !m.is_empty()) {
        args.push("-m".into());
        args.push(message.to_string());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::push_args;

    #[test]
    fn 参数() {
        assert_eq!(push_args(None, false), ["stash", "push"]);
        assert_eq!(push_args(Some(""), false), ["stash", "push"]);
        assert_eq!(
            push_args(Some("说明"), false),
            ["stash", "push", "-m", "说明"]
        );
        assert_eq!(
            push_args(Some("说明"), true),
            [
                "--no-literal-pathspecs",
                "stash",
                "push",
                "--include-untracked",
                "-m",
                "说明"
            ]
        );
    }

    #[test]
    fn 带未跟踪文件的贮藏收得走也放得回() {
        let dir = std::env::temp_dir().join(format!("fr-stash-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| crate::git::run(&dir, args).unwrap();
        git(&["init", "-q"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "core.autocrlf", "false"]);
        std::fs::write(dir.join("a.txt"), "1\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "c1"]);
        // 一个改了的文件、一个未跟踪的文件
        std::fs::write(dir.join("a.txt"), "2\n").unwrap();
        std::fs::write(dir.join("新 文件.txt"), "new\n").unwrap();

        let push = push_args(Some("m"), true);
        let push: Vec<&str> = push.iter().map(String::as_str).collect();
        git(&push);
        // 工作区回到干净：未跟踪的文件也收走了（没关掉字面路径时它会留在原地）
        assert!(!dir.join("新 文件.txt").exists());
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "1\n");

        git(&["stash", "pop", "stash@{0}"]);
        assert_eq!(
            std::fs::read_to_string(dir.join("新 文件.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "2\n");
        assert!(git(&["stash", "list"]).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
