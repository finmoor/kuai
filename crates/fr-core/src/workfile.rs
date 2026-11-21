//! 读、写工作区里的一个文件（内置的合并编辑器用）。
//!
//! 通道只会「在仓库里执行 git」。和 [`crate::ignore`] 一样借 git 的 shell 别名：别名在仓库根目录里
//! 由 sh 执行，本机和服务器上都能用。文件按原样字节读写，换行符和编码都不动。

/// 先输出文件内容的散列（`git hash-object --no-filters`，按原样字节算），再输出内容。先算散列
/// 后读：两步之间文件变了，记下的是旧的散列，保存时核对不上，不会把别人的改动盖掉。
const READ: &str =
    r#"alias.fr-read=!f(){ git hash-object --no-filters -- "$1" && cat -- "$1"; }; f"#;
/// 写之前先核对文件的散列还是读的时候那个（第二个参数；空的表示文件原来不存在），对不上就把第三个
/// 参数（说明）写到标准错误、不写。编辑的这一会儿别的程序改了文件，原来照旧的内容写回去，别人的
/// 改动就没了。
///
/// 先写到旁边的临时文件再改名：写到一半出错，原来的文件还在。`cat >` 保留不了可执行位，
/// 所以用 `cp -p` 先照原文件做一个权限一样的临时文件，再把内容灌进去。
const WRITE: &str = r#"alias.fr-write=!f(){ h=$(git hash-object --no-filters -- "$1" 2>/dev/null); if [ "$h" != "$2" ]; then printf '%s\n' "$3" >&2; return 1; fi; t="$1.fr-tmp.$$"; if [ -e "$1" ]; then cp -p -- "$1" "$t" || return 1; fi; cat > "$t" && mv -f -- "$t" "$1" || { rm -f -- "$t"; return 1; }; }; f"#;

/// 读出 `path`（相对仓库根）的内容，从标准输出拿；用 [`split_read`] 拆成散列和内容。
pub fn read_args(path: &str) -> Vec<String> {
    vec!["-c".into(), READ.into(), "fr-read".into(), path.into()]
}

/// 拆开 [`read_args`] 的输出：第一行是散列（交给 [`write_args`] 核对），后面是原样的内容。
pub fn split_read(out: &[u8]) -> Option<(String, &[u8])> {
    let end = out.iter().position(|&byte| byte == b'\n')?;
    let hash = std::str::from_utf8(&out[..end]).ok()?.trim();
    let valid = !hash.is_empty() && hash.bytes().all(|byte| byte.is_ascii_hexdigit());
    valid.then(|| (hash.to_string(), &out[end + 1..]))
}

/// 把标准输入写进 `path`（相对仓库根）。`expected` 是读的时候的散列（[`split_read`] 给的；空的
/// 表示文件原来不存在），现在的文件对不上就不写，失败，标准错误里是 `changed`。
pub fn write_args(path: &str, expected: &str, changed: &str) -> Vec<String> {
    vec![
        "-c".into(),
        WRITE.into(),
        "fr-write".into(),
        path.into(),
        expected.into(),
        changed.into(),
    ]
}

/// [`write_args`] 给人看的写法（活动记录、失败的详情）：等价的 shell 命令，不露出别名那段脚本。
pub fn write_shown(path: &str) -> String {
    format!("cat > {}", crate::git::sh_quote(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::path::Path;
    use std::process::{Output, Stdio};

    fn run(dir: &Path, args: &[String], stdin: Option<&[u8]>) -> Output {
        let mut child = crate::git::command(dir, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut pipe = child.stdin.take().unwrap();
        if let Some(bytes) = stdin {
            // 核对不上时别名不读标准输入就退出了，这里写不进去不算错
            let _ = pipe.write_all(bytes);
        }
        drop(pipe);
        child.wait_with_output().unwrap()
    }

    fn ok(out: Output) -> Vec<u8> {
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        out.stdout
    }

    /// 读出来，拆成散列和内容。
    fn read(dir: &Path, path: &str) -> (String, Vec<u8>) {
        let out = ok(run(dir, &read_args(path), None));
        let (hash, text) = split_read(&out).unwrap();
        (hash, text.to_vec())
    }

    fn repo(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::git::run(&dir, &["init", "-q"]).unwrap();
        dir
    }

    #[test]
    fn 按原样读写工作区的文件() {
        let dir = repo("workfile-test");
        std::fs::create_dir_all(dir.join("子 目录")).unwrap();
        // 路径里有空格、中文和引号；内容带 CRLF、不是 UTF-8 的字节、末尾没有换行
        let path = "子 目录/a 'b'.txt";
        let original: &[u8] = b"line1\r\n\xd6\xd0\xce\xc4\r\nlast";
        std::fs::write(dir.join(path), original).unwrap();
        let (hash, text) = read(&dir, path);
        assert_eq!(text, original);

        let changed: &[u8] = b"<<<<<<< x\r\nnew\xff\r\n";
        ok(run(
            &dir,
            &write_args(path, &hash, "changed"),
            Some(changed),
        ));
        assert_eq!(std::fs::read(dir.join(path)).unwrap(), changed);
        let (after, text) = read(&dir, path);
        assert_eq!(text, changed);
        assert_ne!(after, hash);
        // 没有留下临时文件
        let left: Vec<_> = std::fs::read_dir(dir.join("子 目录"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "{left:?}");

        // 原来没有的文件也能写出来（散列给空的）
        ok(run(&dir, &write_args("new.txt", "", "changed"), Some(b"x")));
        assert_eq!(std::fs::read(dir.join("new.txt")).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 读了以后文件被别的程序改过_不写() {
        let dir = repo("workfile-guard");
        std::fs::write(dir.join("a.txt"), b"old\n").unwrap();
        let (hash, _) = read(&dir, "a.txt");
        // 编辑器开着的时候别的程序改了它
        std::fs::write(dir.join("a.txt"), b"theirs\n").unwrap();
        let note = "已被修改 'x' $HOME";
        let out = run(&dir, &write_args("a.txt", &hash, note), Some(b"mine\n"));
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains(note));
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"theirs\n");

        // 原来没有、现在有了：也不写
        std::fs::write(dir.join("b.txt"), b"theirs\n").unwrap();
        let out = run(&dir, &write_args("b.txt", "", note), Some(b"mine\n"));
        assert!(!out.status.success());
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"theirs\n");

        // 被删掉了：也不写
        std::fs::remove_file(dir.join("a.txt")).unwrap();
        let out = run(&dir, &write_args("a.txt", &hash, note), Some(b"mine\n"));
        assert!(!out.status.success());
        assert!(!dir.join("a.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 文件名以减号开头也能读写() {
        // `[ -e "$1" ]` 只有两个参数，第一个是一元运算符，第二个不管是什么都当文件名
        let dir = repo("workfile-dash");
        std::fs::write(dir.join("-x.txt"), b"old\n").unwrap();
        let (hash, text) = read(&dir, "-x.txt");
        assert_eq!(text, b"old\n");
        ok(run(
            &dir,
            &write_args("-x.txt", &hash, "changed"),
            Some(b"new\n"),
        ));
        assert_eq!(std::fs::read(dir.join("-x.txt")).unwrap(), b"new\n");
        // 原来没有的也一样
        ok(run(&dir, &write_args("-e", "", "changed"), Some(b"y")));
        assert_eq!(std::fs::read(dir.join("-e")).unwrap(), b"y");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 拆开读出的散列和内容() {
        let (hash, text) = split_read(b"0123abcd\r\nline\n").unwrap();
        assert_eq!((hash.as_str(), text), ("0123abcd", &b"line\n"[..]));
        assert_eq!(split_read(b"abc\n").map(|(_, text)| text.len()), Some(0));
        assert!(split_read(b"not a hash\nx").is_none());
        assert!(split_read(b"\nx").is_none());
        assert!(split_read(b"abc").is_none());
    }
}
