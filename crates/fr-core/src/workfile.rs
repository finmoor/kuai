//! 读、写工作区里的一个文件（内置的合并编辑器用）。
//!
//! 通道只会「在仓库里执行 git」。和 [`crate::ignore`] 一样借 git 的 shell 别名：别名在仓库根目录里
//! 由 sh 执行，本机和服务器上都能用。文件按原样字节读写，换行符和编码都不动。

const READ: &str = r#"alias.fr-read=!f(){ cat -- "$1"; }; f"#;
/// 先写到旁边的临时文件再改名：写到一半出错，原来的文件还在。`cat >` 保留不了可执行位，
/// 所以用 `cp -p` 先照原文件做一个权限一样的临时文件，再把内容灌进去。
const WRITE: &str = r#"alias.fr-write=!f(){ t="$1.fr-tmp.$$"; if [ -e "$1" ]; then cp -p -- "$1" "$t" || return 1; fi; cat > "$t" && mv -f -- "$t" "$1" || { rm -f -- "$t"; return 1; }; }; f"#;

/// 读出 `path`（相对仓库根）的内容，从标准输出拿。
pub fn read_args(path: &str) -> Vec<String> {
    vec!["-c".into(), READ.into(), "fr-read".into(), path.into()]
}

/// 把标准输入写进 `path`（相对仓库根）。
pub fn write_args(path: &str) -> Vec<String> {
    vec!["-c".into(), WRITE.into(), "fr-write".into(), path.into()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::process::Stdio;

    fn run(dir: &std::path::Path, args: &[String], stdin: Option<&[u8]>) -> Vec<u8> {
        let mut child = crate::git::command(dir, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut pipe = child.stdin.take().unwrap();
        if let Some(bytes) = stdin {
            pipe.write_all(bytes).unwrap();
        }
        drop(pipe);
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    #[test]
    fn 按原样读写工作区的文件() {
        let dir = std::env::temp_dir().join(format!("fr-workfile-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("子 目录")).unwrap();
        crate::git::run(&dir, &["init", "-q"]).unwrap();
        // 路径里有空格、中文和引号；内容带 CRLF、不是 UTF-8 的字节、末尾没有换行
        let path = "子 目录/a 'b'.txt";
        let original: &[u8] = b"line1\r\n\xd6\xd0\xce\xc4\r\nlast";
        std::fs::write(dir.join(path), original).unwrap();
        assert_eq!(run(&dir, &read_args(path), None), original);

        let changed: &[u8] = b"<<<<<<< x\r\nnew\xff\r\n";
        run(&dir, &write_args(path), Some(changed));
        assert_eq!(std::fs::read(dir.join(path)).unwrap(), changed);
        assert_eq!(run(&dir, &read_args(path), None), changed);
        // 没有留下临时文件
        let left: Vec<_> = std::fs::read_dir(dir.join("子 目录"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "{left:?}");

        // 原来没有的文件也能写出来
        run(&dir, &write_args("new.txt"), Some(b"x"));
        assert_eq!(std::fs::read(dir.join("new.txt")).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
