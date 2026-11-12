//! 界面语言。
//!
//! 源码里直接写中文，中文原文就是查表的键：`tr("提交")` 在中文界面下原样返回，在英文界面下
//! 去 `locales/en.json` 里查译文，查不到仍然返回中文（不会出现空白或键名）。带变量的句子用
//! [`tf!`](crate::tf)：`tf!("移除服务器 {name}？", name = server.name)`，占位符按名字填，
//! 译文里可以调换占位符的先后。
//!
//! 语言是进程级的：切换以后重画界面，各处的 `tr` 就返回新语言的文字。已经显示出来的通知、
//! 存进状态里的字符串不会跟着变。

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// 设置里选的语言。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    /// 跟着系统的显示语言：中文系统用中文，其他用英文
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "zh-CN")]
    Chinese,
    #[serde(rename = "en")]
    English,
}

impl Language {
    pub const ALL: [Language; 3] = [Language::System, Language::Chinese, Language::English];

    /// 语言自己的名字，不翻译：不认识当前界面语言的人也要能找到自己的语言。
    pub fn label(self) -> &'static str {
        match self {
            Language::System => tr("跟随系统"),
            Language::Chinese => "简体中文",
            Language::English => "English",
        }
    }

    /// 实际用哪种：`System` 按系统的显示语言定。
    pub fn resolve(self) -> Lang {
        match self {
            Language::Chinese => Lang::Zh,
            Language::English => Lang::En,
            Language::System => {
                if system_is_chinese() {
                    Lang::Zh
                } else {
                    Lang::En
                }
            }
        }
    }
}

/// 实际生效的语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    /// 组件库（gpui-component）认的语言代码。
    pub fn code(self) -> &'static str {
        match self {
            Lang::Zh => "zh-CN",
            Lang::En => "en",
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn set_lang(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

pub fn lang() -> Lang {
    if CURRENT.load(Ordering::Relaxed) == Lang::En as u8 {
        Lang::En
    } else {
        Lang::Zh
    }
}

/// 中文原文 → 英文。文件在编译时嵌进程序。
fn english() -> &'static HashMap<&'static str, &'static str> {
    static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let table: HashMap<String, String> =
            serde_json::from_str(include_str!("../locales/en.json")).unwrap_or_default();
        // 表在整个运行期间都要用，泄漏掉换成 'static 的引用，查表不用再复制字符串
        table
            .into_iter()
            .filter(|(_, en)| !en.is_empty())
            .map(|(zh, en)| (&*zh.leak(), &*en.leak()))
            .collect()
    })
}

/// 按当前语言给出这句话。
pub fn tr(text: &'static str) -> &'static str {
    match lang() {
        Lang::Zh => text,
        Lang::En => english().get(text).copied().unwrap_or(text),
    }
}

/// 同一个词在不同的地方要翻成不同的英文时用：`trc("分组", "分支")` 在中文界面下还是「分支」，
/// 英文界面下查「分支|分组」这一条（侧栏分组的标题是 Branches，工具栏按钮上的是 Branch）；
/// 没有这一条就退回 `tr("分支")`。
pub fn trc(context: &'static str, text: &'static str) -> &'static str {
    match lang() {
        Lang::Zh => text,
        Lang::En => {
            let key = format!("{text}|{context}");
            english()
                .get(key.as_str())
                .copied()
                .unwrap_or_else(|| tr(text))
        }
    }
}

/// 标记一句以后要拿去 [`tr`] 的话，本身什么也不做。用在常量里（那里不能调 `tr`）：
/// 写成 `tr_key("默认浅色")`，显示时再 `tr(名字)`。有了这个标记，检查译文全不全的测试才找得到它。
pub const fn tr_key(text: &'static str) -> &'static str {
    text
}

/// 这句话有没有英文译文（测试用）。
pub fn has_english(text: &str) -> bool {
    english().contains_key(text)
}

/// 把 `{名字}` 换成对应的值。模板里没有的名字忽略，没给值的占位符原样留着。
///
/// 英文分单复数：译文里写 `{n} {n|file|files}`，n 是 1 时用前一个词，否则用后一个。
pub fn fill(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let key = &after[..close];
                let mut parts = key.splitn(3, '|');
                let wanted = parts.next().unwrap_or_default();
                let forms = parts.next().zip(parts.next());
                match args.iter().find(|(name, _)| *name == wanted) {
                    Some((_, value)) => match forms {
                        Some((one, other)) => {
                            out.push_str(if value.to_string() == "1" { one } else { other })
                        }
                        None => out.push_str(&value.to_string()),
                    },
                    None => {
                        out.push('{');
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 带变量的句子：`tf!("{op}失败", op)`、`tf!("还有 {n} 个文件有冲突", n = conflicts)`。
/// 变量和占位符同名时可以只写名字。
#[macro_export]
macro_rules! tf {
    ($tpl:literal $(, $key:ident $(= $val:expr)?)* $(,)?) => {
        $crate::i18n::fill(
            $crate::i18n::tr($tpl),
            &[$((stringify!($key), &$crate::tf!(@val $key $(, $val)?) as &dyn ::std::fmt::Display)),*],
        )
    };
    (@val $key:ident) => { $key };
    (@val $key:ident, $val:expr) => { $val };
}

#[cfg(windows)]
fn system_is_chinese() -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    // SAFETY：没有参数，只读系统设置
    let id = unsafe { GetUserDefaultUILanguage() };
    // 语言标识的低 10 位是主语言，0x04 是中文
    id & 0x3ff == 0x04
}

#[cfg(not(windows))]
fn system_is_chinese() -> bool {
    std::env::var("LANG").is_ok_and(|lang| lang.starts_with("zh"))
}

#[cfg(test)]
mod tests {
    use super::fill;

    #[test]
    fn 占位符按名字填() {
        let n = 3;
        assert_eq!(
            fill("还有 {n} 个文件有冲突", &[("n", &n)]),
            "还有 3 个文件有冲突"
        );
        // 译文里换了先后也行
        assert_eq!(
            fill("{b} before {a}", &[("a", &"x"), ("b", &"y")]),
            "y before x"
        );
        // 没给值的占位符、落单的花括号原样留着
        assert_eq!(fill("{a} {zz} {", &[("a", &1)]), "1 {zz} {");
        // 英文的单复数
        let plural = "{n} {n|file|files}";
        assert_eq!(fill(plural, &[("n", &1)]), "1 file");
        assert_eq!(fill(plural, &[("n", &0)]), "0 files");
        assert_eq!(fill(plural, &[("n", &12)]), "12 files");
    }

    /// 一段源码里拿去翻译的话：`tr("…")`、`tf!("…"`、`tr_key("…")` 的第一个参数。
    /// 注释行和测试模块不算。
    fn keys_in(source: &str) -> Vec<String> {
        let source = source.split("#[cfg(test)]").next().unwrap_or_default();
        let code: String = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut keys = Vec::new();
        for marker in ["tr(", "tf!(", "tr_key("] {
            let mut rest = code.as_str();
            while let Some(at) = rest.find(marker) {
                let before = rest[..at].chars().next_back();
                let after = rest[at + marker.len()..].trim_start();
                rest = &rest[at + marker.len()..];
                // 前面还连着字母的是别的函数（`str(`、`attr(`）
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                if let Some(literal) = after.strip_prefix('"') {
                    keys.push(unescape(literal));
                }
            }
        }
        // trc("场合", "词")：查的是「词|场合」
        let mut rest = code.as_str();
        while let Some(at) = rest.find("trc(") {
            let before = rest[..at].chars().next_back();
            let after = rest[at + 4..].trim_start();
            rest = &rest[at + 4..];
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let Some(literal) = after.strip_prefix('"') else {
                continue;
            };
            let context = unescape(literal);
            // 跳过第一个字面量，找第二个
            let Some(end) = literal.find('"') else {
                continue;
            };
            let second = literal[end + 1..]
                .trim_start()
                .trim_start_matches(',')
                .trim_start();
            if let Some(literal) = second.strip_prefix('"') {
                keys.push(format!("{}|{context}", unescape(literal)));
            }
        }
        keys
    }

    /// 读一个 Rust 字符串字面量（开头的引号已经去掉），读到结尾的引号为止。
    fn unescape(literal: &str) -> String {
        let mut out = String::new();
        let mut chars = literal.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    // 行尾的反斜杠：这一行到下一行开头的空白都不算
                    Some('\n') | Some('\r') => {
                        while chars.peek().is_some_and(|c| c.is_whitespace()) {
                            chars.next();
                        }
                    }
                    Some(other) => out.push(other),
                    None => break,
                },
                c => out.push(c),
            }
        }
        out
    }

    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// 一句话里的占位符，排好序。
    fn placeholders(text: &str) -> Vec<&str> {
        let mut found: Vec<&str> = text
            .split('{')
            .skip(1)
            .filter_map(|part| part.split_once('}').map(|(name, _)| name))
            // `{n|file|files}` 算 n
            .map(|name| name.split('|').next().unwrap_or(name))
            .collect();
        found.sort_unstable();
        found.dedup();
        found
    }

    #[test]
    fn 认得出源码里拿去翻译的话() {
        let source = r#"
            // 注释里的 tr("不算")
            let a = tr("提交");
            let b = tf!("还有 {n} 个", n = 1);
            let c = attr("也不算");
            const D: &str = tr_key("默认浅色");
            let e = tr(
                "折了行的\
                 一句话",
            );
            let f = tr(name);
            let g = trc("分组", "分支");
            #[cfg(test)]
            mod tests { fn x() { tr("测试里的不算"); } }
        "#;
        let mut keys = keys_in(source);
        keys.sort();
        assert_eq!(
            keys,
            [
                "分支|分组",
                "折了行的一句话",
                "提交",
                "还有 {n} 个",
                "默认浅色"
            ]
        );
    }

    /// 界面上的每一句话都要有英文：扫客户端各个 crate 的源码，一句一句查。
    /// 新写了 `tr("…")` 没补英文，这个测试会把缺的列出来。
    #[test]
    fn 界面上的每句话都有英文() {
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let (mut total, mut missing) = (0, Vec::new());
        // kuaid 跑在服务器上，说的话不跟客户端的语言
        for name in ["fr-app", "fr-core", "fr-channel"] {
            let mut files = Vec::new();
            rust_files(&crates.join(name).join("src"), &mut files);
            for file in files {
                let text = std::fs::read_to_string(&file).unwrap();
                for key in keys_in(&text) {
                    // 只有英文字母、符号的话不用翻
                    if key.is_ascii() {
                        continue;
                    }
                    total += 1;
                    if !super::has_english(&key) {
                        missing.push(format!("{}：{key:?}", file.display()));
                    }
                }
            }
        }
        assert!(total > 300, "只扫到 {total} 句，扫描的办法是不是失效了");
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "这 {} 句还没有英文，补到 crates/fr-core/locales/en.json：\n{}",
            missing.len(),
            missing.join("\n")
        );
    }

    #[test]
    fn 译文里的占位符和原文一样() {
        for (zh, en) in super::english() {
            assert_eq!(placeholders(zh), placeholders(en), "原文：{zh}\n译文：{en}");
            assert!(
                !en.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "译文里还有汉字：{en}"
            );
        }
    }

    #[test]
    fn 宏里同名的变量可以省掉等号() {
        let op = "拉取";
        assert_eq!(crate::tf!("{op}失败", op), "拉取失败");
        assert_eq!(crate::tf!("{op}失败：{n}", op, n = 1 + 1), "拉取失败：2");
        assert_eq!(crate::tf!("没有变量"), "没有变量");
    }
}
