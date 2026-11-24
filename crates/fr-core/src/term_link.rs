//! 终端里的链接（按住 Ctrl 单击打开）：网址、文件和目录的路径。
//!
//! 这里只看屏幕上的字：认出鼠标底下的是不是网址、像不像路径，路径带没带行号；路径按终端开在哪个
//! 目录算出在仓库里的哪里（[`resolve`]）。文件到底在不在由界面去问（本机直接看，服务器上的问
//! kuaid），认得出、但不在的不算链接。

use std::ops::Range;

/// 认出来的一个链接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// 在那一行字里从第几个字到第几个字（按字符数，不含 `end`）：下划线画在这几个字下面
    pub span: Range<usize>,
    pub target: Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// 网址：http、https 的
    Url(String),
    /// 文件或目录的路径（原样写的，没去查），带着行号、列号（从 1 起）：`src/main.rs:12:5`、
    /// `Foo.cs(12,5)`。`file://` 的网址也算路径
    Path {
        path: String,
        line: Option<u32>,
        col: Option<u32>,
    },
}

/// `text`（一行字，写满折下去的几行连起来）里第 `at` 个字（按字符数）所在的链接。
pub fn link_at(text: &str, at: usize) -> Option<Link> {
    let chars: Vec<char> = text.chars().collect();
    if at >= chars.len() {
        return None;
    }
    url_at(&chars, at).or_else(|| path_at(&chars, at))
}

/// 网址开头的这几种
const SCHEMES: [&str; 3] = ["https://", "http://", "file://"];

/// 网址里能有的字：ASCII 里看得见的，除了引号、尖括号这些常拿来把网址括起来的。中文、全角标点
/// 紧挨着写在后面的（「见https://x.com说明」）不算网址
fn url_char(c: char) -> bool {
    c.is_ascii_graphic()
        && !matches!(
            c,
            '"' | '\'' | '`' | '<' | '>' | '|' | '{' | '}' | '\\' | '^'
        )
}

fn url_at(chars: &[char], at: usize) -> Option<Link> {
    if !url_char(chars[at]) {
        return None;
    }
    // 网址里没有空白：往左到空白为止，在这一截里找网址的开头
    let mut from = at;
    while from > 0 && url_char(chars[from - 1]) {
        from -= 1;
    }
    let starts_with = |i: usize, scheme: &str| {
        scheme.len() <= chars.len() - i
            && scheme
                .chars()
                .zip(&chars[i..])
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    };
    // 开头前面得是字母、数字以外的（`xhttps://` 不算）
    let start = (from..=at).rev().find(|&i| {
        SCHEMES.iter().any(|scheme| starts_with(i, scheme))
            && (i == 0 || !chars[i - 1].is_ascii_alphanumeric())
    })?;
    let scheme = SCHEMES.iter().find(|scheme| starts_with(start, scheme))?;
    let mut end = start + scheme.len();
    while end < chars.len() && url_char(chars[end]) {
        end += 1;
    }
    let end = trim_url_end(chars, start + scheme.len(), end);
    if at >= end || end == start + scheme.len() {
        return None;
    }
    let url: String = chars[start..end].iter().collect();
    let target = match *scheme {
        "file://" => Target::Path {
            path: file_url_path(&url[scheme.len()..]),
            line: None,
            col: None,
        },
        _ => Target::Url(url),
    };
    Some(Link {
        span: start..end,
        target,
    })
}

/// 网址末尾的标点不算（句号、逗号、配不上对的右括号）。
fn trim_url_end(chars: &[char], body: usize, mut end: usize) -> usize {
    while end > body {
        let count = |c: char| chars[body..end].iter().filter(|&&x| x == c).count();
        let drop = match chars[end - 1] {
            '.' | ',' | ';' | ':' | '!' | '?' => true,
            ')' => count('(') < count(')'),
            ']' => count('[') < count(']'),
            _ => false,
        };
        if !drop {
            break;
        }
        end -= 1;
    }
    end
}

/// `file:///C:/a%20b/c.txt`、`file:///home/me/x` 里的路径：去掉 Windows 盘符前面的 `/`，`%XX` 换回字。
fn file_url_path(rest: &str) -> String {
    // `file://主机/路径`：主机一般是空的或者 localhost
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let mut bytes = Vec::with_capacity(rest.len());
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (
            raw[i],
            raw.get(i + 1).copied().and_then(hex),
            raw.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(hi), Some(lo)) => {
                bytes.push((hi * 16 + lo) as u8);
                i += 3;
            }
            (b, _, _) => {
                bytes.push(b);
                i += 1;
            }
        }
    }
    let path = String::from_utf8_lossy(&bytes).into_owned();
    let drive = path.as_bytes();
    match drive.len() >= 3 && drive[0] == b'/' && drive[1].is_ascii_alphabetic() && drive[2] == b':'
    {
        true => path[1..].to_string(),
        false => path,
    }
}

/// 路径里能有的字：空白、引号、括号、逗号这些常拿来把路径括起来、隔开的不算（带空格的路径认不了）；
/// 中文、全角标点里只有标点、制表线不算，`docs/架构.md` 这样的照样认。
fn path_char(c: char) -> bool {
    !c.is_whitespace()
        && !matches!(
            c,
            '"' | '\''
                | '`'
                | '<'
                | '>'
                | '|'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | ','
                | ';'
                | '*'
                | '?'
        )
        && !fullwidth_punct(c)
        && !('\u{2500}'..='\u{259f}').contains(&c)
}

fn fullwidth_punct(c: char) -> bool {
    matches!(
        c,
        '，' | '。'
            | '、'
            | '；'
            | '：'
            | '！'
            | '？'
            | '（'
            | '）'
            | '【'
            | '】'
            | '「'
            | '」'
            | '《'
            | '》'
            | '“'
            | '”'
            | '‘'
            | '’'
            | '…'
    )
}

fn path_at(chars: &[char], at: usize) -> Option<Link> {
    if !path_char(chars[at]) {
        return None;
    }
    let mut start = at;
    while start > 0 && path_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = at + 1;
    while end < chars.len() && path_char(chars[end]) {
        end += 1;
    }
    // Claude Code 里提到文件写成 `@src/x.rs`
    while start < end && chars[start] == '@' {
        start += 1;
    }
    // 句末的标点、`路径:` 后面的冒号不算
    while end > start && matches!(chars[end - 1], '.' | ':' | '!') {
        end -= 1;
    }
    if at < start || at >= end {
        return None;
    }
    let token: String = chars[start..end].iter().collect();
    let (path, mut line, mut col) = split_position(&token);
    // `Foo.cs(12,5)`、`Foo.cs(12)`：行号写在后面的括号里
    let mut span_end = start + path.chars().count();
    if line.is_none() && span_end == end {
        if let Some((n, (l, c))) = paren_position(&chars[end..]) {
            line = Some(l);
            col = c;
            span_end = end + n;
        }
    } else {
        span_end = end;
    }
    if !looks_like_path(path) {
        return None;
    }
    Some(Link {
        span: start..span_end,
        target: Target::Path {
            path: path.to_string(),
            line,
            col,
        },
    })
}

/// `src/x.rs:12:5` 分成路径、行号、列号。Windows 盘符后面的冒号不算（`C:\x`）。
fn split_position(token: &str) -> (&str, Option<u32>, Option<u32>) {
    let mut numbers: Vec<u32> = Vec::new();
    let mut rest = token;
    while numbers.len() < 2 {
        let Some((head, tail)) = rest.rsplit_once(':') else {
            break;
        };
        match tail.parse::<u32>() {
            Ok(n) if !head.is_empty() && !tail.starts_with('+') => {
                numbers.insert(0, n);
                rest = head;
            }
            _ => break,
        }
    }
    (rest, numbers.first().copied(), numbers.get(1).copied())
}

/// 路径后面紧跟着的 `(12,5)`、`(12)`：用了几个字、行号、列号。
fn paren_position(after: &[char]) -> Option<(usize, (u32, Option<u32>))> {
    if after.first() != Some(&'(') {
        return None;
    }
    let close = after.iter().position(|&c| c == ')')?;
    let inner: String = after[1..close].iter().collect();
    let mut parts = inner.split(',').map(str::trim);
    let line = parts.next()?.parse().ok()?;
    let col = match parts.next() {
        Some(col) => Some(col.parse().ok()?),
        None => None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((close + 1, (line, col)))
}

/// 看着像路径：绝对路径、带斜杠的相对路径，或者带扩展名的文件名（`README.md`）。网址、纯数字、
/// 版本号（`1.18.0`）不算。认得宽一点没关系：不在的文件不会当成链接。
fn looks_like_path(path: &str) -> bool {
    if path.is_empty() || path.contains("://") || path.chars().count() > 1024 {
        return false;
    }
    if path.contains('/') || path.contains('\\') {
        // 至少有一个字母、数字或者中文：`/`、`..`、`-->` 不算
        return path.chars().any(|c| c.is_alphanumeric());
    }
    match path.rsplit_once('.') {
        Some((name, ext)) => {
            !name.is_empty()
                && !ext.is_empty()
                && ext.len() <= 16
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && ext.chars().any(|c| c.is_ascii_alphabetic())
        }
        None => false,
    }
}

/// 一个路径在哪。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// 在仓库里：相对仓库根，`/` 分隔（空的是仓库根自己）
    Repo(String),
    /// 在仓库外：绝对路径
    Outside(String),
}

/// 终端里写的路径 `path` 在哪。相对路径从终端开在的目录（仓库根下的 `sub`，`/` 分隔，空的是仓库根）
/// 算起，`..` 能跳出仓库。`root` 是仓库根（本机、Windows 服务器上是 `C:\…` 或 `C:/…`，Linux 服务器
/// 上是 `/…`）：Windows 的不分大小写，两种斜杠都认，也认 `/C:/…`。认不了的是 `None`：`~/…`，
/// Windows 上没有盘符、以单个斜杠开头的（Git Bash 的 `/c/…`）。
pub fn resolve(root: &str, sub: &str, path: &str) -> Option<Place> {
    let windows = is_windows_path(root);
    let path = if windows {
        strip_drive_slash(path)
    } else {
        path
    };
    let split = |text: &str| -> Vec<String> {
        text.split(|c| c == '/' || (windows && c == '\\'))
            .filter(|part| !part.is_empty())
            .map(String::from)
            .collect()
    };
    let unc = |text: &str| windows && (text.starts_with("\\\\") || text.starts_with("//"));
    let absolute = match windows {
        true if drive_absolute(path) || unc(path) => true,
        true if path.starts_with(['/', '\\', '~']) => return None,
        false if path.starts_with('/') => true,
        false if path.starts_with('~') => return None,
        _ => false,
    };
    let base = match absolute {
        true => Vec::new(),
        false => split(root).into_iter().chain(split(sub)).collect(),
    };
    let mut parts = base;
    for part in split(path) {
        match part.as_str() {
            "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let root_parts = split(root);
    let same = |a: &String, b: &String| match windows {
        true => a.to_lowercase() == b.to_lowercase(),
        false => a == b,
    };
    let unc_matches = unc(root) == (if absolute { unc(path) } else { unc(root) });
    if unc_matches
        && parts.len() >= root_parts.len()
        && root_parts.iter().zip(&parts).all(|(a, b)| same(a, b))
    {
        return Some(Place::Repo(parts[root_parts.len()..].join("/")));
    }
    let prefix = match (absolute && unc(path)) || (!absolute && unc(root)) {
        true => "\\\\",
        false => "",
    };
    Some(Place::Outside(match windows {
        true => format!("{prefix}{}", parts.join("\\")),
        false => format!("/{}", parts.join("/")),
    }))
}

/// 同 [`resolve`]，相对路径从 `dir` 算起：`dir` 是终端里那一段输出当时所在的目录（提示符里写的，
/// 见 [`crate::term_text::prompt_dir`]），绝对路径，可以在仓库外。`~` 开头的（`dir` 或者 `path`）换成
/// `home`（那台电脑上 shell 的主目录），不知道就认不了。
pub fn resolve_at(root: &str, dir: &str, home: Option<&str>, path: &str) -> Option<Place> {
    let expand = |text: &str| -> Option<Option<String>> {
        match text.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
                Some(Some(format!("{}{rest}", home?)))
            }
            Some(_) => None,
            None => Some(None),
        }
    };
    let dir = expand(dir)?.unwrap_or_else(|| dir.to_string());
    let path = expand(path)?.unwrap_or_else(|| path.to_string());
    let windows = is_windows_path(root);
    let absolute = match windows {
        true => {
            drive_absolute(strip_drive_slash(&path))
                || path.starts_with("\\\\")
                || path.starts_with("//")
        }
        false => path.starts_with('/'),
    };
    if absolute {
        return resolve(root, "", &path);
    }
    let sep = if windows { '\\' } else { '/' };
    let joined = format!("{}{sep}{path}", dir.trim_end_matches(['/', '\\']));
    resolve(root, "", &joined)
}

/// 是 Windows 的路径：`C:` 开头，或者 `\\主机\共享`。
fn is_windows_path(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') || path.starts_with("\\\\")
}

/// `C:\…`、`C:/…`
fn drive_absolute(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// Codex 在 Windows 上也会把绝对路径显示成 `/C:/…`。
fn strip_drive_slash(path: &str) -> &str {
    path.strip_prefix('/')
        .filter(|rest| drive_absolute(rest))
        .unwrap_or(path)
}

/// 照原样找不到时再试的写法：git 的改动里文件写成 `a/src/x.rs`、`b/src/x.rs`。
pub fn alternatives(path: &str) -> Vec<&str> {
    let mut all = vec![path];
    if let Some(rest) = path.strip_prefix("a/").or_else(|| path.strip_prefix("b/"))
        && !rest.is_empty()
    {
        all.push(rest);
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 鼠标在 `needle` 的第一个字上时认出来的链接：链接那几个字、链接指向哪。
    fn at(text: &str, needle: &str) -> Option<(String, Target)> {
        let byte = text.find(needle).expect("needle");
        let at = text[..byte].chars().count();
        link_at(text, at).map(|link| {
            let shown: String = text
                .chars()
                .skip(link.span.start)
                .take(link.span.len())
                .collect();
            (shown, link.target)
        })
    }

    fn url(text: &str) -> Target {
        Target::Url(text.to_string())
    }

    fn path(text: &str, line: Option<u32>, col: Option<u32>) -> Target {
        Target::Path {
            path: text.to_string(),
            line,
            col,
        }
    }

    #[test]
    fn 网址() {
        let text = "详见 https://example.com/a?b=1&c=2#x。";
        assert_eq!(
            at(text, "example"),
            Some((
                "https://example.com/a?b=1&c=2#x".into(),
                url("https://example.com/a?b=1&c=2#x")
            ))
        );
        // 括起来的、句末的标点不算；配得上对的括号算
        assert_eq!(
            at("(see http://localhost:3000/x).", "local").map(|l| l.1),
            Some(url("http://localhost:3000/x"))
        );
        assert_eq!(
            at("https://en.wikipedia.org/wiki/Rust_(language), ok", "wiki").map(|l| l.1),
            Some(url("https://en.wikipedia.org/wiki/Rust_(language)"))
        );
        // 鼠标在 `https` 上也算；中文紧挨着结束
        assert_eq!(
            at("打开https://x.cn看看", "https").map(|l| l.1),
            Some(url("https://x.cn"))
        );
        // 不是网址开头的：xhttps://
        assert_eq!(at("xhttps://a.b", "a.b").map(|l| l.1), None);
    }

    #[test]
    fn file_网址当路径() {
        assert_eq!(
            at("open file:///C:/Users/me/a%20b.txt now", "Users").map(|l| l.1),
            Some(path("C:/Users/me/a b.txt", None, None))
        );
        assert_eq!(
            at("file:///home/me/x.log", "home").map(|l| l.1),
            Some(path("/home/me/x.log", None, None))
        );
    }

    #[test]
    fn 路径和行号() {
        // rustc
        assert_eq!(
            at("  --> crates/fr-core/src/lib.rs:12:5", "crates"),
            Some((
                "crates/fr-core/src/lib.rs:12:5".into(),
                path("crates/fr-core/src/lib.rs", Some(12), Some(5))
            ))
        );
        // PowerShell 的报错
        assert_eq!(
            at(r"At C:\repo\build.ps1:12 char:5", "repo").map(|l| l.1),
            Some(path(r"C:\repo\build.ps1", Some(12), None))
        );
        // MSBuild、C#：行号在括号里
        assert_eq!(
            at(r"C:\src\Foo.cs(12,5): error CS1002", "Foo"),
            Some((
                r"C:\src\Foo.cs(12,5)".into(),
                path(r"C:\src\Foo.cs", Some(12), Some(5))
            ))
        );
        // Claude Code 的工具调用、提到文件
        assert_eq!(
            at("⏺ Update(crates/fr-app/src/terminal/mod.rs)", "fr-app").map(|l| l.1),
            Some(path("crates/fr-app/src/terminal/mod.rs", None, None))
        );
        assert_eq!(
            at("看一下 @docs/架构.md 里的", "docs").map(|l| l.1),
            Some(path("docs/架构.md", None, None))
        );
        // git status、Python 的报错、句末
        assert_eq!(
            at("\tmodified:   src/main.rs", "src").map(|l| l.1),
            Some(path("src/main.rs", None, None))
        );
        assert_eq!(
            at(r#"  File "C:\app\run.py", line 3"#, "app").map(|l| l.1),
            Some(path(r"C:\app\run.py", None, None))
        );
        assert_eq!(
            at("改好了，见 README.md。", "README").map(|l| l.1),
            Some(path("README.md", None, None))
        );
        // 路径后面的冒号
        assert_eq!(
            at("src/x.rs: warning", "src").map(|l| l.1),
            Some(path("src/x.rs", None, None))
        );
    }

    #[test]
    fn 不像路径的不认() {
        for (text, needle) in [
            ("version 1.18.0 released", "1.18"),
            ("ok -> done", "->"),
            ("plain word", "plain"),
            ("a // b", "//"),
            ("│ box │", "box"),
        ] {
            assert_eq!(at(text, needle), None, "{text}");
        }
    }

    #[test]
    fn 按仓库根和终端开在的目录找() {
        let root = r"C:\Users\me\repo";
        assert_eq!(
            resolve(root, "", "src/x.rs"),
            Some(Place::Repo("src/x.rs".into()))
        );
        // 开在子目录里的终端；`..` 回到仓库里别处、跳出仓库
        assert_eq!(
            resolve(root, "crates/app", r"..\core\lib.rs"),
            Some(Place::Repo("crates/core/lib.rs".into()))
        );
        assert_eq!(
            resolve(root, "", "../other/x.rs"),
            Some(Place::Outside(r"C:\Users\me\other\x.rs".into()))
        );
        // 绝对路径：大小写、斜杠不一样也认得是仓库里的
        assert_eq!(
            resolve(root, "", "c:/users/ME/repo/Docs/a.md"),
            Some(Place::Repo("Docs/a.md".into()))
        );
        assert_eq!(resolve(root, "", root), Some(Place::Repo(String::new())));
        assert_eq!(
            resolve(root, "", r"D:\tmp\a.txt"),
            Some(Place::Outside(r"D:\tmp\a.txt".into()))
        );
        // Windows 上没有盘符的 `/` 路径和 `~` 开头的认不了
        assert_eq!(resolve(root, "", "/c/Users/me/repo/x"), None);
        assert_eq!(resolve(root, "", "~/x"), None);
        // Linux 服务器：分大小写
        let root = "/data/repo";
        assert_eq!(
            resolve(root, "", "/data/repo/src/x.rs"),
            Some(Place::Repo("src/x.rs".into()))
        );
        assert_eq!(
            resolve(root, "", "/data/Repo/x"),
            Some(Place::Outside("/data/Repo/x".into()))
        );
        assert_eq!(
            resolve(root, "", "./a/../b.txt"),
            Some(Place::Repo("b.txt".into()))
        );
        // Windows 服务器的仓库根写成正斜杠
        assert_eq!(
            resolve("C:/Users/bigno/proj", "", r"C:\Users\bigno\proj\a.txt"),
            Some(Place::Repo("a.txt".into()))
        );
    }

    #[test]
    fn git_改动里的_a_b_前缀() {
        assert_eq!(alternatives("a/src/x.rs"), ["a/src/x.rs", "src/x.rs"]);
        assert_eq!(alternatives("src/x.rs"), ["src/x.rs"]);
    }

    #[test]
    fn windows_绝对路径盘符前有斜杠() {
        let root = r"C:\Users\me\repo";
        let raw = "/c:/Users/me/repo/src/main.rs";
        assert_eq!(
            at(&format!("文件位置 ({raw}:494:2)"), "main.rs"),
            Some((format!("{raw}:494:2"), path(raw, Some(494), Some(2))))
        );
        let expected = Some(Place::Repo("src/main.rs".into()));
        assert_eq!(resolve(root, "docs", raw), expected);
        assert_eq!(super::resolve_at(root, r"D:\other", None, raw), expected);
        assert_eq!(
            resolve(root, "", "/D:/docs/"),
            Some(Place::Outside(r"D:\docs".into()))
        );
        assert_eq!(
            resolve("/c:/Users/me/repo", "", raw),
            Some(Place::Repo("src/main.rs".into()))
        );
    }

    #[test]
    fn 按提示符里的目录找() {
        use super::resolve_at;
        let root = "C:\\Users\\me\\repo";
        // 在仓库的子目录里
        assert_eq!(
            resolve_at(root, "C:\\Users\\me\\repo\\src\\deep", None, "util.rs"),
            Some(Place::Repo("src/deep/util.rs".into()))
        );
        assert_eq!(
            resolve_at(root, "c:\\users\\me\\REPO\\src\\deep", None, "../main.rs"),
            Some(Place::Repo("src/main.rs".into()))
        );
        // 在仓库外
        assert_eq!(
            resolve_at(root, "C:\\Users\\me\\other", None, "a.txt"),
            Some(Place::Outside("C:\\Users\\me\\other\\a.txt".into()))
        );
        // 盘符根目录（提示符是 `PS C:\>`）
        assert_eq!(
            resolve_at(root, "C:\\", None, "Users\\me\\repo\\x.rs"),
            Some(Place::Repo("x.rs".into()))
        );
        // 绝对路径不看目录
        assert_eq!(
            resolve_at(root, "D:\\x", None, "C:\\Users\\me\\repo\\y.rs"),
            Some(Place::Repo("y.rs".into()))
        );
        // Linux：`~` 换成主目录，不知道主目录就认不了
        let root = "/home/me/code/repo";
        assert_eq!(
            resolve_at(root, "~/code/repo/src", Some("/home/me"), "lib.rs"),
            Some(Place::Repo("src/lib.rs".into()))
        );
        assert_eq!(resolve_at(root, "~/code/repo/src", None, "lib.rs"), None);
        assert_eq!(
            resolve_at(root, "/etc", Some("/home/me"), "hosts"),
            Some(Place::Outside("/etc/hosts".into()))
        );
        assert_eq!(
            resolve_at(root, "/tmp", Some("/home/me"), "~/code/repo/README.md"),
            Some(Place::Repo("README.md".into()))
        );
    }
}
