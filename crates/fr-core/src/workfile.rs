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

/// 冲突的文件按暂存区里的三份重新合并一遍，结果写到标准输出，不碰工作区：`:1:` 是共同祖先（两边
/// 都新加的文件没有，按空的算）、`:2:` 当前、`:3:` 传入。第二个参数是冲突标记的样式：`merge`（和
/// git 合并时默认的一样）或 `diff3`、`zdiff3`（带共同祖先）；后面三个是标记上写的名字。
/// 合并编辑器先拿 `merge` 那份核对工作区里的文件冲突以后没被动过，再换成带共同祖先的那份。
/// `git merge-file` 的退出码是冲突的处数，128 以上才是出错。
const BASE: &str = r#"alias.fr-base=!f(){ d=$(mktemp -d) || return 1; git show ":2:$1" > "$d/o" 2>/dev/null && git show ":3:$1" > "$d/t" 2>/dev/null || { rm -rf "$d"; return 1; }; git show ":1:$1" > "$d/b" 2>/dev/null || : > "$d/b"; s=""; [ "$2" = merge ] || s="--$2"; git merge-file -p $s -L "$3" -L "$4" -L "$5" "$d/o" "$d/b" "$d/t"; r=$?; rm -rf "$d"; [ "$r" -lt 128 ]; }; f"#;

/// 按暂存区里的三份重新合并 `path`（见 [`BASE`]），从标准输出拿结果。`style` 是 `merge`、`diff3`
/// 或 `zdiff3`；`labels` 是当前、共同祖先、传入三边标记上写的名字。
pub fn base_args(path: &str, style: &str, labels: [&str; 3]) -> Vec<String> {
    let mut args = vec![
        "-c".into(),
        BASE.into(),
        "fr-base".into(),
        path.into(),
        style.into(),
    ];
    args.extend(labels.iter().map(|label| label.to_string()));
    args
}

/// 丢弃改动前给几个文件各存一份（丢弃以后能撤销）：路径从标准输入来，一行一个。先逐个输出它现在
/// 是什么样（`f` 普通文件、`-` 不存在、`?` 符号链接、目录这类存不了的），一行 `===` 以后是那些普通
/// 文件的散列，顺序一样。第一个参数是 `w` 时内容写进对象库（`hash-object -w`，按原样字节、不过
/// 过滤器），否则只算散列（丢弃以后记下来，撤销前核对）。散列一次算完，文件多也只起一个 git。
const SNAP: &str = r#"alias.fr-snap=!f(){ w=; [ "$1" = w ] && w=-w; t=$(mktemp) || return 1; while IFS= read -r p; do if [ -L "$p" ] || [ -d "$p" ]; then echo '?'; elif [ -f "$p" ]; then echo f; printf '%s\n' "$p" >> "$t"; elif [ -e "$p" ]; then echo '?'; else echo -; fi; done; echo ===; git hash-object --no-filters $w --stdin-paths < "$t"; r=$?; rm -f "$t"; return $r; }; f"#;

/// 撤销丢弃时放回一个文件：先核对它现在还是丢弃后的样子（第三个参数：散列，或 `-` 表示不存在），
/// 不是就把第四个参数（说明）写到标准错误、不动；是就放回丢弃前的样子（第二个参数：对象库里的
/// 内容，或 `-` 表示原来没有这个文件，删掉）。
const UNSNAP: &str = r#"alias.fr-unsnap=!f(){ c=-; if [ -L "$1" ] || [ -d "$1" ]; then c='?'; elif [ -f "$1" ]; then c=$(git hash-object --no-filters -- "$1") || c='?'; elif [ -e "$1" ]; then c='?'; fi; if [ "$c" != "$3" ]; then printf '%s\n' "$4" >&2; return 1; fi; if [ "$2" = - ]; then rm -f -- "$1"; else d=$(dirname -- "$1"); mkdir -p -- "$d" && git cat-file blob "$2" > "$1"; fi; }; f"#;

/// 一个文件在某一刻的样子（见 [`snap_args`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Snap {
    /// 普通文件，内容的散列
    Blob(String),
    /// 不存在
    Absent,
    /// 存不了（符号链接、目录、名字里有换行）
    Unknown,
}

impl Snap {
    /// 交给 [`unsnap_args`] 的写法。
    pub fn arg(&self) -> &str {
        match self {
            Snap::Blob(hash) => hash,
            Snap::Absent => "-",
            Snap::Unknown => "?",
        }
    }
}

/// 存（`write` 为真）或只看几个文件现在的样子，见 [`SNAP`]。路径用 [`snap_stdin`] 喂进标准输入，
/// 输出交给 [`parse_snap`]。
pub fn snap_args(write: bool) -> Vec<String> {
    vec![
        "-c".into(),
        SNAP.into(),
        "fr-snap".into(),
        if write { "w" } else { "r" }.into(),
    ]
}

/// [`snap_args`] 的标准输入：一行一个路径。名字里有换行的放不进去，调用方先挑出去（见
/// [`parse_snap`]）。
pub fn snap_stdin(paths: &[String]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for path in paths.iter().filter(|path| !path.contains('\n')) {
        bytes.extend_from_slice(path.as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

/// 拆开 [`snap_args`] 的输出，按 `paths` 的顺序一个一个对上；名字里有换行的是 [`Snap::Unknown`]。
/// 输出对不上（行数不够、散列不像）时 None。
pub fn parse_snap(paths: &[String], out: &[u8]) -> Option<Vec<Snap>> {
    let text = std::str::from_utf8(out).ok()?;
    let mut lines = text.lines();
    let mut kinds = Vec::new();
    for line in lines.by_ref() {
        if line == "===" {
            break;
        }
        kinds.push(line.to_string());
    }
    let mut hashes = lines.filter(|line| !line.is_empty());
    let mut kinds = kinds.into_iter();
    let mut snaps = Vec::with_capacity(paths.len());
    for path in paths {
        if path.contains('\n') {
            snaps.push(Snap::Unknown);
            continue;
        }
        let snap = match kinds.next()?.as_str() {
            "f" => {
                let hash = hashes.next()?;
                if hash.is_empty() || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return None;
                }
                Snap::Blob(hash.to_string())
            }
            "-" => Snap::Absent,
            _ => Snap::Unknown,
        };
        snaps.push(snap);
    }
    Some(snaps)
}

/// 撤销丢弃：把 `path` 放回 `before`，前提是它现在还是 `after`（见 [`UNSNAP`]）；不是的话失败，
/// 标准错误里是 `changed`。
pub fn unsnap_args(path: &str, before: &Snap, after: &Snap, changed: &str) -> Vec<String> {
    vec![
        "-c".into(),
        UNSNAP.into(),
        "fr-unsnap".into(),
        path.into(),
        before.arg().into(),
        after.arg().into(),
        changed.into(),
    ]
}

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
    fn 按暂存区里的三份重新合并() {
        let dir = repo("workfile-base");
        let git = |args: &[&str]| crate::git::run(&dir, args);
        git(&["config", "user.name", "t"]).unwrap();
        git(&["config", "user.email", "t@example.com"]).unwrap();
        git(&["config", "core.autocrlf", "false"]).unwrap();
        git(&["checkout", "-q", "-b", "main"]).unwrap();
        std::fs::write(
            dir.join("a.txt"),
            "head
base
tail
",
        )
        .unwrap();
        git(&["add", "a.txt"]).unwrap();
        git(&["commit", "-q", "-m", "base"]).unwrap();
        git(&["checkout", "-q", "-b", "feature"]).unwrap();
        std::fs::write(
            dir.join("a.txt"),
            "head
theirs
tail
",
        )
        .unwrap();
        git(&["commit", "-q", "-am", "theirs"]).unwrap();
        git(&["checkout", "-q", "main"]).unwrap();
        std::fs::write(
            dir.join("a.txt"),
            "head
ours
tail
",
        )
        .unwrap();
        git(&["commit", "-q", "-am", "ours"]).unwrap();
        assert!(git(&["merge", "feature"]).is_err(), "应该冲突");
        let worktree = std::fs::read(dir.join("a.txt")).unwrap();
        // 和 git 合并时写进工作区的一样
        let merge = ok(run(
            &dir,
            &base_args("a.txt", "merge", ["HEAD", "base", "feature"]),
            None,
        ));
        assert_eq!(merge, worktree);
        // 带共同祖先的那份
        let diff3 = ok(run(
            &dir,
            &base_args("a.txt", "diff3", ["HEAD", "base", "feature"]),
            None,
        ));
        let diff3 = String::from_utf8(diff3).unwrap();
        assert!(diff3.contains("||||||| base\nbase\n"), "{diff3}");
        // 没有冲突的文件：暂存区里没有那几份，失败
        let out = run(&dir, &base_args("nope.txt", "merge", ["a", "b", "c"]), None);
        assert!(!out.status.success());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 丢弃前存一份_撤销时放回() {
        let dir = repo("workfile-snap");
        std::fs::create_dir_all(dir.join("d")).unwrap();
        std::fs::write(dir.join("a.txt"), b"before\r\n\xff").unwrap();
        std::fs::write(dir.join("d/b.txt"), b"untracked\n").unwrap();
        let paths: Vec<String> = ["a.txt", "d/b.txt", "gone.txt", "d", "x\ny"]
            .map(String::from)
            .to_vec();
        let out = ok(run(&dir, &snap_args(true), Some(&snap_stdin(&paths))));
        let before = parse_snap(&paths, &out).unwrap();
        assert!(matches!(&before[0], Snap::Blob(_)));
        assert!(matches!(&before[1], Snap::Blob(_)));
        assert_eq!(before[2], Snap::Absent);
        assert_eq!(before[3], Snap::Unknown);
        assert_eq!(before[4], Snap::Unknown);
        // 像丢弃那样：a.txt 改回别的内容，未跟踪的 d/b.txt 删掉，gone.txt 冒出来
        std::fs::write(dir.join("a.txt"), b"after\n").unwrap();
        std::fs::remove_file(dir.join("d/b.txt")).unwrap();
        std::fs::remove_dir(dir.join("d")).unwrap();
        std::fs::write(dir.join("gone.txt"), b"restored\n").unwrap();
        let out = ok(run(&dir, &snap_args(false), Some(&snap_stdin(&paths))));
        let after = parse_snap(&paths, &out).unwrap();
        assert_eq!(after[1], Snap::Absent);
        for ix in 0..3 {
            ok(run(
                &dir,
                &unsnap_args(&paths[ix], &before[ix], &after[ix], "changed"),
                None,
            ));
        }
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"before\r\n\xff");
        assert_eq!(std::fs::read(dir.join("d/b.txt")).unwrap(), b"untracked\n");
        assert!(!dir.join("gone.txt").exists());
        // 丢弃以后又被改过：不放回
        std::fs::write(dir.join("a.txt"), b"edited again\n").unwrap();
        let out = run(
            &dir,
            &unsnap_args("a.txt", &before[0], &after[0], "已改"),
            None,
        );
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("已改"));
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"edited again\n");
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
