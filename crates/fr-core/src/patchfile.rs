//! 补丁文件：认出是哪种、预检能不能应用、应用。
//!
//! 补丁在本机读，内容经标准输入交给主机上的 git，不要求先把文件传上去。两种：
//! - 差异补丁（`git diff` 的输出）：只有改动，用 `git apply`。
//! - 提交补丁（`git format-patch` 的输出，一封封邮件）：带作者、时间、说明，可以用 `git am` 原样
//!   变回提交，也可以只取里面的改动。
//!
//! 预检要按文件说清楚能不能应用，只能读 git 的报错。那些话跟着主机的语言变，所以预检借 shell 别名
//! 把语言固定成英文（[`check_args`]）；真正应用时不改语言。

/// 补丁太大就不读了：界面要逐个文件列出来，主机上的 git 也要整份读进内存。
pub const MAX_BYTES: u64 = 50 * 1024 * 1024;

/// 补丁里的一个提交（提交补丁才有）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailCommit {
    pub id: String,
    pub author: String,
    pub subject: String,
}

/// 认出提交补丁里的各个提交；一个都没有就是差异补丁。按 `From <40 位哈希> ` 开头的行分封。
pub fn mail_commits(bytes: &[u8]) -> Vec<MailCommit> {
    let text = String::from_utf8_lossy(bytes);
    let mut commits: Vec<MailCommit> = Vec::new();
    // 在信头里（`From ` 行之后、第一个空行之前）
    let mut in_head = false;
    let mut continuing = false;
    for line in text.lines() {
        if let Some(id) = mail_start(line) {
            commits.push(MailCommit {
                id: id.to_string(),
                author: String::new(),
                subject: String::new(),
            });
            in_head = true;
            continuing = false;
            continue;
        }
        let Some(commit) = commits.last_mut().filter(|_| in_head) else {
            continue;
        };
        if line.is_empty() {
            in_head = false;
        } else if let Some(rest) = line.strip_prefix("From: ") {
            commit.author = decode_words(rest.split('<').next().unwrap_or(rest).trim());
            continuing = false;
        } else if let Some(rest) = line.strip_prefix("Subject: ") {
            commit.subject = strip_patch_tag(&decode_words(rest.trim()));
            continuing = true;
        } else if continuing && line.starts_with(' ') {
            // 折成几行的标题：编码的两段之间的空白不算，别的各留一个空格
            let more = decode_words(line.trim());
            if !(line.trim_start().starts_with("=?") && commit.subject_was_encoded()) {
                commit.subject.push(' ');
            }
            commit.subject.push_str(&more);
        } else {
            continuing = false;
        }
    }
    commits
}

impl MailCommit {
    /// 标题里有非 ASCII 字符：多半是编码过的，折行处不补空格。
    fn subject_was_encoded(&self) -> bool {
        !self.subject.is_ascii()
    }
}

/// `From 0123…（40 位） Mon Sep 17 00:00:00 2001`
fn mail_start(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("From ")?;
    let (id, tail) = rest.split_once(' ')?;
    let hex = id.len() >= 40 && id.bytes().all(|b| b.is_ascii_hexdigit());
    (hex && tail.contains("00:00:00 2001")).then_some(id)
}

/// 去掉标题前面的 `[PATCH 1/2]`。
fn strip_patch_tag(subject: &str) -> String {
    let subject = subject.trim();
    match subject
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
    {
        Some((tag, rest)) if tag.starts_with("PATCH") => rest.trim().to_string(),
        _ => subject.to_string(),
    }
}

/// 信头里的 `=?UTF-8?q?…?=`、`=?UTF-8?b?…?=` 解出来；别的原样。
fn decode_words(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    let mut last_encoded = false;
    while let Some(start) = rest.find("=?") {
        let (before, after) = rest.split_at(start);
        let decoded = decode_word(after);
        // 两个编码段之间只有空白：不算
        if !(last_encoded && decoded.is_some() && before.trim().is_empty()) {
            out.push_str(before);
        }
        match decoded {
            Some((word, len)) => {
                out.push_str(&word);
                rest = &after[len..];
                last_encoded = true;
            }
            None => {
                out.push_str("=?");
                rest = &after[2..];
                last_encoded = false;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 解一个编码段，返回（内容，这一段占了多少字节）。只认 UTF-8。
fn decode_word(text: &str) -> Option<(String, usize)> {
    let body = text.strip_prefix("=?")?;
    let mut parts = body.splitn(3, '?');
    let charset = parts.next()?;
    let kind = parts.next()?;
    let tail = parts.next()?;
    let end = tail.find("?=")?;
    let data = &tail[..end];
    if !charset.eq_ignore_ascii_case("utf-8") {
        return None;
    }
    let bytes = match kind {
        "q" | "Q" => {
            let mut bytes = Vec::new();
            let raw = data.as_bytes();
            let mut i = 0;
            while i < raw.len() {
                match raw[i] {
                    b'_' => bytes.push(b' '),
                    b'=' if i + 3 <= raw.len() => {
                        let hex = std::str::from_utf8(raw.get(i + 1..i + 3)?).ok()?;
                        bytes.push(u8::from_str_radix(hex, 16).ok()?);
                        i += 2;
                    }
                    other => bytes.push(other),
                }
                i += 1;
            }
            bytes
        }
        "b" | "B" => base64(data)?,
        _ => return None,
    };
    let len = 2 + charset.len() + 1 + kind.len() + 1 + end + 2;
    Some((String::from_utf8_lossy(&bytes).into_owned(), len))
}

fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// 补丁里的一个文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchFile {
    pub path: String,
    /// 改名前的路径
    pub orig: Option<String>,
    /// 增、删的行数；二进制文件没有
    pub lines: Option<(u32, u32)>,
    /// 补丁要新建这个文件
    pub created: bool,
    /// 补丁要删掉这个文件
    pub deleted: bool,
}

/// 列出补丁改了哪些文件（不看能不能应用）：`git apply --numstat --summary -z`，补丁从标准输入给。
pub fn list_args() -> Vec<String> {
    ["apply", "--numstat", "--summary", "-z", "-"]
        .map(String::from)
        .to_vec()
}

/// 解析 [`list_args`] 的输出。前面是 `增\t删\t路径\0`（改名的是 `增\t删\t\0旧\0新\0`），后面是
/// ` create mode 100644 路径` 这样的几行。同一个文件出现几次（提交补丁里几个提交都改了它）的合成一条。
pub fn parse_list(out: &[u8]) -> Vec<PatchFile> {
    let mut files: Vec<PatchFile> = Vec::new();
    let mut add = |path: String, orig: Option<String>, lines: Option<(u32, u32)>| match files
        .iter_mut()
        .find(|file| file.path == path)
    {
        Some(file) => {
            file.lines = match (file.lines, lines) {
                (Some((a, d)), Some((a2, d2))) => Some((a + a2, d + d2)),
                _ => None,
            }
        }
        None => files.push(PatchFile {
            path,
            orig,
            lines,
            created: false,
            deleted: false,
        }),
    };
    let mut fields = out.split(|b| *b == 0).peekable();
    let mut summary: Vec<u8> = Vec::new();
    while let Some(field) = fields.next() {
        let text = String::from_utf8_lossy(field);
        // 摘要那几行前面可能粘着上一条的换行
        let record = text.trim_start_matches('\n');
        let mut parts = record.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            summary.extend_from_slice(field);
            summary.push(b'\n');
            continue;
        };
        let count = |text: &str| text.parse::<u32>().ok();
        let numeric = |text: &str| text == "-" || count(text).is_some();
        if !numeric(added) || !numeric(deleted) {
            summary.extend_from_slice(field);
            summary.push(b'\n');
            continue;
        }
        let lines = count(added).zip(count(deleted));
        if path.is_empty() {
            let orig = fields
                .next()
                .map(|f| String::from_utf8_lossy(f).into_owned());
            let new = fields
                .next()
                .map(|f| String::from_utf8_lossy(f).into_owned());
            if let Some(new) = new {
                add(new, orig, lines);
            }
        } else {
            add(path.to_string(), None, lines);
        }
    }
    for line in String::from_utf8_lossy(&summary).lines() {
        let line = line.trim();
        let created = line.strip_prefix("create mode ");
        let deleted = line.strip_prefix("delete mode ");
        let Some(rest) = created.or(deleted) else {
            continue;
        };
        // `100644 路径`
        let Some((_, path)) = rest.split_once(' ') else {
            continue;
        };
        if let Some(file) = files.iter_mut().find(|file| file.path == path) {
            file.created |= created.is_some();
            file.deleted |= deleted.is_some();
        }
    }
    files
}

/// 一个文件预检的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// 可以直接应用
    Clean,
    /// 和现在的内容对不上；三方合并能应用，冲突留在文件里
    Mismatch,
    /// 要改的文件不存在
    Missing,
    /// 要新建的文件已经存在
    Exists,
    /// 别的原因应用不了
    Failed,
}

impl Fit {
    /// 三方合并也救不了。
    pub fn hard(self) -> bool {
        matches!(self, Fit::Missing | Fit::Failed)
    }
}

/// 预检：`git apply --check --verbose`，语言固定成英文好认（见模块说明）。补丁从标准输入给。
/// `three_way` 时带 `--3way`：对不上的文件改用三方合并，看还有没有应用不了的。
const CHECK: &str = r#"alias.fr-apply-check=!f(){ LC_ALL=C LANGUAGE=C LANG=C git apply --check --verbose "$@" -; }; f"#;

pub fn check_args(three_way: bool) -> Vec<String> {
    let mut args = vec!["-c".to_string(), CHECK.into(), "fr-apply-check".into()];
    if three_way {
        args.push("--3way".into());
    }
    args
}

/// 解析 [`check_args`] 的输出（标准错误）：每个文件一段，`Checking patch 路径...` 开头，后面是它的
/// 报错。返回（路径，结果）；改名的按新路径记。
pub fn parse_check(stderr: &[u8]) -> Vec<(String, Fit)> {
    let text = String::from_utf8_lossy(stderr);
    let mut out: Vec<(String, Fit)> = Vec::new();
    // 正在看哪个文件的报错
    let mut current: Option<usize> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Checking patch ") {
            let name = rest.strip_suffix("...").unwrap_or(rest);
            let name = match name.split_once(" => ") {
                Some((_, new)) => new,
                None => name,
            };
            let path = unquote(name);
            // 同一个文件检查几次（提交补丁里几个提交都改了它）：合成一条，已经记了不好的就留着
            current = Some(match out.iter().position(|(seen, _)| *seen == path) {
                Some(at) => at,
                None => {
                    out.push((path, Fit::Clean));
                    out.len() - 1
                }
            });
            continue;
        }
        let Some((_, fit)) = current.and_then(|at| out.get_mut(at)) else {
            continue;
        };
        let worse = if line.contains("with conflicts") {
            Some(Fit::Mismatch)
        } else if !line.starts_with("error:") {
            None
        } else if line.contains("No such file or directory")
            || line.contains("does not exist in index")
        {
            Some(Fit::Missing)
        } else if line.contains("already exists in") {
            Some(Fit::Exists)
        } else if line.contains("patch does not apply") || line.contains("patch failed") {
            Some(Fit::Mismatch)
        } else if line.starts_with("error: while searching for") {
            None
        } else {
            Some(Fit::Failed)
        };
        if let Some(worse) = worse {
            // 一个文件报几句：留最要紧的那句（不存在、失败压过对不上）
            let rank = |fit: Fit| match fit {
                Fit::Clean => 0,
                Fit::Mismatch => 1,
                Fit::Exists => 2,
                Fit::Missing => 3,
                Fit::Failed => 4,
            };
            if rank(worse) > rank(*fit) {
                *fit = worse;
            }
        }
    }
    out
}

/// git 用 C 的写法括起来的路径（带引号、反斜杠、控制字符的才这样）还原；没括的原样。
pub fn unquote(name: &str) -> String {
    let Some(inner) = name
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return name.to_string();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let raw = inner.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] != b'\\' || i + 1 >= raw.len() {
            bytes.push(raw[i]);
            i += 1;
            continue;
        }
        i += 1;
        match raw[i] {
            b'n' => bytes.push(b'\n'),
            b't' => bytes.push(b'\t'),
            b'r' => bytes.push(b'\r'),
            b'a' => bytes.push(7),
            b'b' => bytes.push(8),
            b'f' => bytes.push(12),
            b'v' => bytes.push(11),
            b'0'..=b'7' => {
                let mut value = 0u32;
                let mut used = 0;
                while used < 3 && i < raw.len() && (b'0'..=b'7').contains(&raw[i]) {
                    value = value * 8 + u32::from(raw[i] - b'0');
                    i += 1;
                    used += 1;
                }
                bytes.push(value as u8);
                continue;
            }
            other => bytes.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 把补丁的改动应用到工作区（`index` 时一并暂存）；`three_way` 时对不上的文件做三方合并，冲突
/// 留在文件里（那时暂存区里也会留下冲突的三份）。补丁从标准输入给。
pub fn apply_args(index: bool, three_way: bool) -> Vec<String> {
    let mut args = vec!["apply".to_string()];
    if three_way {
        args.push("--3way".into());
    } else if index {
        args.push("--index".into());
    }
    args.push("-".into());
    args
}

/// 把提交补丁原样变回提交（保留作者、时间、说明）。失败时停在一半，用 [`am_abort_args`] 退回去。
pub fn am_args(three_way: bool) -> Vec<String> {
    let mut args = vec!["am".to_string(), "--keep-cr".into()];
    if three_way {
        args.push("--3way".into());
    }
    args.push("-".into());
    args
}

pub fn am_abort_args() -> Vec<String> {
    vec!["am".into(), "--abort".into()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 认出提交补丁里的提交() {
        let mbox = "From d4cb9a42b78a162fc57be8bb10847fa7c19f0270 Mon Sep 17 00:00:00 2001\n\
From: =?UTF-8?q?=E5=BC=A0=E4=BC=9F?= <t@example.com>\n\
Date: Thu, 26 Nov 2026 10:00:00 +0800\n\
Subject: [PATCH 1/2] =?UTF-8?q?work=201=EF=BC=9A=E6=94=B9=20a=E3=80=81b?=\n\
\x20=?UTF-8?q?=EF=BC=8C=E5=8A=A0=20new?=\n\
\n\
---\n a.txt | 2 +-\n\
From: not a header\n\
diff --git a/a.txt b/a.txt\n\
From 29b882c7494e2cca9dc284f3de8f6bda7f6e295a Mon Sep 17 00:00:00 2001\n\
From: Alice <a@example.com>\n\
Subject: [PATCH 2/2] plain subject that is long enough to be folded\n\
\x20onto a second line\n\
\n";
        let commits = mail_commits(mbox.as_bytes());
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].id, "d4cb9a42b78a162fc57be8bb10847fa7c19f0270");
        assert_eq!(commits[0].author, "张伟");
        assert_eq!(commits[0].subject, "work 1：改 a、b，加 new");
        assert_eq!(commits[1].author, "Alice");
        assert_eq!(
            commits[1].subject,
            "plain subject that is long enough to be folded onto a second line"
        );
        // 差异补丁里没有提交；正文里碰巧有 From 开头的行不算
        assert!(mail_commits(b"diff --git a/a b/a\n+From me to you\n").is_empty());
        assert!(mail_commits(b"From someone who is not a hash\n").is_empty());
    }

    #[test]
    fn 编码的信头() {
        assert_eq!(decode_words("=?UTF-8?b?5L2g5aW9?= <a@b>"), "你好 <a@b>");
        assert_eq!(decode_words("=?utf-8?Q?a_b=3Dc?="), "a b=c");
        assert_eq!(decode_words("plain =? not encoded"), "plain =? not encoded");
        assert_eq!(decode_words("=?GBK?q?x?="), "=?GBK?q?x?=");
    }

    #[test]
    fn 列出补丁里的文件() {
        let out = b"2\t2\ta.txt\x001\t0\tb.txt\x000\t1\tkeep.txt\x001\t0\tnew.txt\x00-\t-\timg.png\x003\t1\t\x00old name.rs\x00new name.rs\x002\t0\ta.txt\x00 delete mode 100644 keep.txt\n create mode 100644 new.txt\n";
        let files = parse_list(out);
        let names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            names,
            [
                "a.txt",
                "b.txt",
                "keep.txt",
                "new.txt",
                "img.png",
                "new name.rs"
            ]
        );
        // a.txt 出现两次，行数加起来
        assert_eq!(files[0].lines, Some((4, 2)));
        assert!(files[2].deleted && !files[2].created);
        assert!(files[3].created);
        assert_eq!(files[4].lines, None);
        assert_eq!(files[5].orig.as_deref(), Some("old name.rs"));
        assert!(parse_list(b"").is_empty());
    }

    #[test]
    fn 预检的结果按文件分() {
        let plain = "Checking patch a.txt...\nerror: while searching for:\nl1\nl2\n\nerror: patch failed: a.txt:1\nerror: a.txt: patch does not apply\nChecking patch b.txt...\nerror: b.txt: No such file or directory\nChecking patch keep.txt...\nChecking patch new.txt...\nerror: new.txt: already exists in working directory\nChecking patch \"\\344\\270\\255 \\\"x\\\".txt\"...\nChecking patch old.rs => new.rs...\n";
        assert_eq!(
            parse_check(plain.as_bytes()),
            [
                ("a.txt".to_string(), Fit::Mismatch),
                ("b.txt".to_string(), Fit::Missing),
                ("keep.txt".to_string(), Fit::Clean),
                ("new.txt".to_string(), Fit::Exists),
                ("中 \"x\".txt".to_string(), Fit::Clean),
                ("new.rs".to_string(), Fit::Clean),
            ]
        );
        let three = "Checking patch a.txt...\nApplied patch to 'a.txt' with conflicts.\nChecking patch b.txt...\nerror: b.txt: does not exist in index\nChecking patch keep.txt...\nFalling back to direct application...\nChecking patch new.txt...\nPerforming three-way merge...\nApplied patch to 'new.txt' with conflicts.\n";
        let fits = parse_check(three.as_bytes());
        assert_eq!(fits[0].1, Fit::Mismatch);
        assert_eq!(fits[1].1, Fit::Missing);
        assert_eq!(fits[2].1, Fit::Clean);
        assert_eq!(fits[3].1, Fit::Mismatch);
        assert!(Fit::Missing.hard() && !Fit::Mismatch.hard() && !Fit::Exists.hard());
    }

    #[test]
    fn 参数() {
        assert_eq!(apply_args(false, false), ["apply", "-"]);
        assert_eq!(apply_args(true, false), ["apply", "--index", "-"]);
        assert_eq!(apply_args(true, true), ["apply", "--3way", "-"]);
        assert_eq!(am_args(true), ["am", "--keep-cr", "--3way", "-"]);
        assert_eq!(check_args(true).last().unwrap(), "--3way");
    }

    /// 真的跑一遍：对得上的、对不上的、不存在的、已存在的各一个。
    #[test]
    fn 对着真仓库预检和应用() {
        let dir = std::env::temp_dir().join(format!("fr-patchfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| crate::git::run(&dir, args).unwrap();
        let write = |name: &str, text: &str| std::fs::write(dir.join(name), text).unwrap();
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "core.autocrlf", "false"]);
        write("a.txt", "l1\nl2\nl3\n");
        write("b.txt", "x1\n");
        write("中文 名.txt", "k\n");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "work"]);
        write("a.txt", "l1\nl2 changed\nl3\n");
        write("b.txt", "x1\nx2\n");
        write("new.txt", "n\n");
        std::fs::remove_file(dir.join("中文 名.txt")).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "改动：一"]);
        let diff = git(&["diff", "main", "work"]);
        let mbox = git(&["format-patch", "--stdout", "main..work"]);
        git(&["checkout", "-q", "main"]);

        let run = |args: Vec<String>, stdin: &[u8]| {
            let (running, _) = crate::exec::spawn(&dir, &args, Some(stdin.to_vec())).unwrap();
            let (out, finished) = running.collect().unwrap();
            (out, finished)
        };
        // 列文件
        let (out, finished) = run(list_args(), &diff);
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        let files = parse_list(&out);
        let names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(names, ["a.txt", "b.txt", "new.txt", "中文 名.txt"]);
        assert!(files[2].created && files[3].deleted);
        // 干净时都能应用
        let (_, finished) = run(check_args(false), &diff);
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        let fits = parse_check(finished.stderr.as_bytes());
        assert_eq!(fits.len(), 4, "{}", finished.stderr);
        assert!(fits.iter().all(|(_, fit)| *fit == Fit::Clean));
        assert_eq!(fits[3].0, "中文 名.txt");
        // 提交补丁认得出提交，当差异预检也行
        let commits = mail_commits(&mbox);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].subject, "改动：一");
        let (_, finished) = run(check_args(false), &mbox);
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);

        // 把 a 的同一行改了、b 删了、new 先建出来
        write("a.txt", "l1\nl2 mine\nl3\n");
        std::fs::remove_file(dir.join("b.txt")).unwrap();
        write("new.txt", "mine\n");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "main 上的改动"]);
        let (_, finished) = run(check_args(false), &diff);
        assert_ne!(finished.code, Some(0));
        let fits = parse_check(finished.stderr.as_bytes());
        let fit = |name: &str| fits.iter().find(|(path, _)| path == name).unwrap().1;
        assert_eq!(fit("a.txt"), Fit::Mismatch, "{}", finished.stderr);
        assert_eq!(fit("b.txt"), Fit::Missing);
        assert_eq!(fit("new.txt"), Fit::Exists);
        assert_eq!(fit("中文 名.txt"), Fit::Clean);
        // 三方合并：a、new 能带着冲突应用，b 不存在还是不行
        let (_, finished) = run(check_args(true), &diff);
        let fits = parse_check(finished.stderr.as_bytes());
        let fit = |name: &str| fits.iter().find(|(path, _)| path == name).unwrap().1;
        assert_eq!(fit("a.txt"), Fit::Mismatch, "{}", finished.stderr);
        assert_eq!(fit("b.txt"), Fit::Missing);

        // 回到干净的时候：应用并暂存；提交补丁变回提交
        git(&["reset", "-q", "--hard", "HEAD~1"]);
        let (_, finished) = run(apply_args(true, false), &diff);
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        let status = String::from_utf8(git(&["status", "--porcelain"])).unwrap();
        assert!(
            status.contains("M  a.txt") && status.contains("A  new.txt"),
            "{status}"
        );
        git(&["reset", "-q", "--hard"]);
        let (_, finished) = run(am_args(false), &mbox);
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        let log = String::from_utf8(git(&["log", "-1", "--format=%s"])).unwrap();
        assert_eq!(log.trim(), "改动：一");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
