//! 把文件内容的字节变成能显示的文字。
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

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn utf8原样_gbk转过来() {
        assert_eq!(decode("中文 abc".as_bytes()), "中文 abc");
        // “中文”的 GBK 编码
        assert_eq!(decode(&[0xD6, 0xD0, 0xCE, 0xC4, b' ', b'a']), "中文 a");
        assert_eq!(decode(b""), "");
    }
}
