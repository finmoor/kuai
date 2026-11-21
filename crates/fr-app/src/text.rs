//! 把字节变成能显示的文字：文件内容解码，字节数写成人看的大小。
//!
//! 仓库里的文件不一定是 UTF-8：国内的老项目常见 GBK。是合法的 UTF-8 就按 UTF-8，
//! 不是就按 GB18030（GBK 的超集）解，再不行的字节显示成替换符号。只影响显示，
//! 生成补丁用的始终是原始字节。

use std::borrow::Cow;

/// 解码一段字节。
pub fn decode(bytes: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => {
            let (text, _, _) = encoding_rs::GB18030.decode(bytes);
            text
        }
    }
}

/// 字节数写成人看的大小：`812 B`、`3.4 KB`、`12 MB`、`340 KB`。不到 100 的留一位小数
/// （是 0 就不写），100 以上取整。界面上所有的大小都用它，写法一致。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.;
    let mut unit = 0;
    // 取整以后是 1024 的（1023.5 起）写成下一级的 1
    while value >= 1023.5 && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    let number = match value < 100. {
        true => {
            let text = format!("{value:.1}");
            match text.strip_suffix(".0") {
                Some(whole) => whole.to_string(),
                None => text,
            }
        }
        false => format!("{value:.0}"),
    };
    format!("{number} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::{decode, human_size};

    #[test]
    fn 大小写成人看的() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(812), "812 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1 KB");
        assert_eq!(human_size(3482), "3.4 KB");
        assert_eq!(human_size(340 * 1024), "340 KB");
        assert_eq!(human_size(5 * 1024 * 1024 + 300 * 1024), "5.3 MB");
        assert_eq!(human_size(12 << 20), "12 MB");
        assert_eq!(human_size(20 << 20), "20 MB");
        assert_eq!(human_size((18 << 20) + (410 << 10)), "18.4 MB");
        assert_eq!(human_size(3 << 30), "3 GB");
        // 差一点到 1 MB 的不写成 1024 KB
        assert_eq!(human_size((1 << 20) - 1), "1 MB");
    }

    #[test]
    fn utf8原样_gbk转过来() {
        assert_eq!(decode("中文 abc".as_bytes()), "中文 abc");
        // “中文”的 GBK 编码
        assert_eq!(decode(&[0xD6, 0xD0, 0xCE, 0xC4, b' ', b'a']), "中文 a");
        assert_eq!(decode(b""), "");
    }
}
