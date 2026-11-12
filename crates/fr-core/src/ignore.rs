//! 把一个路径加到仓库根目录的 `.gitignore`。
//!
//! 通道只会「在仓库里执行 git」，没有写文件的操作。这里借 git 的 shell 别名来写：
//! `git -c alias.x='!…' x <规则>`，别名在仓库根目录里由 sh 执行（Git for Windows 自带 sh），
//! 本机和服务器上都能用，kuaid 不用为此加功能。

/// 追加一行的 shell 函数：`.gitignore` 末尾没有换行时先补一个。规则作为参数传进去，不拼进脚本。
const ALIAS: &str = r#"alias.fr-ignore=!f(){ if [ -s .gitignore ] && [ -n "$(tail -c1 .gitignore)" ]; then echo >> .gitignore; fi; printf '%s\n' "$1" >> .gitignore; }; f"#;

/// 把 `path`（相对仓库根，`git status` 给的那种）加到 `.gitignore` 的 git 参数。
pub fn args(path: &str) -> Vec<String> {
    vec!["-c".into(), ALIAS.into(), "fr-ignore".into(), pattern(path)]
}

/// 路径对应的忽略规则：开头加 `/` 表示从仓库根算起，通配符按字面转义。
pub fn pattern(path: &str) -> String {
    let mut rule = String::with_capacity(path.len() + 2);
    rule.push('/');
    for ch in path.chars() {
        if matches!(ch, '\\' | '*' | '?' | '[' | ']') {
            rule.push('\\');
        }
        rule.push(ch);
    }
    // 行尾的空格要转义，否则会被忽略
    if rule.ends_with(' ') {
        rule.insert(rule.len() - 1, '\\');
    }
    rule
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 规则从根算起并转义通配符() {
        assert_eq!(pattern("target/"), "/target/");
        assert_eq!(pattern("a [1].txt"), r"/a \[1\].txt");
        assert_eq!(pattern("笔记*.md"), r"/笔记\*.md");
        assert_eq!(pattern("x "), r"/x\ ");
    }

    #[test]
    fn 追加到gitignore并且生效() {
        let dir = std::env::temp_dir().join(format!("fr-ignore-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::git::run(&dir, &["init", "-q"]).unwrap();
        // 原来的 .gitignore 末尾没有换行
        std::fs::write(dir.join(".gitignore"), "*.log").unwrap();
        std::fs::write(dir.join("a [1].txt"), "x").unwrap();
        std::fs::write(dir.join("新文件.txt"), "x").unwrap();

        for path in ["a [1].txt", "新文件.txt"] {
            let args = args(path);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            crate::git::run(&dir, &args).unwrap();
        }
        let text = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(text, "*.log\n/a \\[1\\].txt\n/新文件.txt\n");
        let status = crate::git::run(&dir, &["status", "--porcelain"]).unwrap();
        assert_eq!(String::from_utf8_lossy(&status).trim(), "?? .gitignore");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
