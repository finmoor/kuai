//! 按键变成发给终端里程序的字节（xterm 的那一套转义序列）。
//!
//! 只管「不是直接打字」的键：回车、方向键、功能键、Ctrl 和 Alt 的组合。普通的字（包括输入法
//! 上屏的）不经过这里，由系统当文字送进来。

use alacritty_terminal::term::TermMode;
use gpui_kit::Keystroke;

/// 滚轮交给谁。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WheelTarget {
    /// 程序要了鼠标（Claude Code、开了鼠标的 vim、htop）：照实报给它，它自己翻
    Report,
    /// 全屏的程序没要鼠标（less、vim）：没有可以往回翻的输出，当上下键
    Arrows,
    /// 翻终端记着的输出
    Scrollback,
}

/// 按着 Shift 总是翻终端记着的输出，不交给程序（和 Windows 终端一样）。
pub fn wheel_target(mode: TermMode, shift: bool) -> WheelTarget {
    if shift {
        WheelTarget::Scrollback
    } else if mode.intersects(TermMode::MOUSE_MODE) {
        WheelTarget::Report
    } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        WheelTarget::Arrows
    } else {
        WheelTarget::Scrollback
    }
}

/// 这个按键要发的字节；`None` 表示不是这里管的（普通打字，或者只按了修饰键）。
/// `app_cursor`：程序把方向键切到了「应用模式」（vim、less 之类会切）。
pub fn encode(keystroke: &Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
    let mods = &keystroke.modifiers;
    if mods.platform {
        return None;
    }
    let (ctrl, alt, shift) = (mods.control, mods.alt, mods.shift);
    // xterm 的修饰键编号：1 加上 Shift 1、Alt 2、Ctrl 4
    let code = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let plain = code == 1;
    let text = |s: &str| Some(s.as_bytes().to_vec());
    // 前面带 ESC 就是 Alt
    let with_alt = |mut bytes: Vec<u8>| {
        if alt {
            bytes.insert(0, 0x1b);
        }
        Some(bytes)
    };

    let key = keystroke.key.as_str();
    match key {
        // Shift+回车、Alt+回车发 ESC 加回车：写提示词的程序（Claude Code 等）拿它当「换行不提交」
        "enter" if shift || alt => text("\x1b\r"),
        "enter" => text("\r"),
        "tab" if shift => text("\x1b[Z"),
        "tab" => with_alt(vec![b'\t']),
        "escape" => with_alt(vec![0x1b]),
        "backspace" if ctrl => with_alt(vec![0x08]),
        "backspace" => with_alt(vec![0x7f]),
        "space" if ctrl => with_alt(vec![0]),
        "space" if alt => text("\x1b "),
        "up" | "down" | "right" | "left" | "home" | "end" => {
            let letter = match key {
                "up" => 'A',
                "down" => 'B',
                "right" => 'C',
                "left" => 'D',
                "home" => 'H',
                _ => 'F',
            };
            if !plain {
                Some(format!("\x1b[1;{code}{letter}").into_bytes())
            } else if app_cursor {
                Some(format!("\x1bO{letter}").into_bytes())
            } else {
                Some(format!("\x1b[{letter}").into_bytes())
            }
        }
        "insert" | "delete" | "pageup" | "pagedown" => {
            let number = match key {
                "insert" => 2,
                "delete" => 3,
                "pageup" => 5,
                _ => 6,
            };
            tilde(number, code)
        }
        "f1" | "f2" | "f3" | "f4" => {
            let letter = (b'P' + key.as_bytes()[1] - b'1') as char;
            if plain {
                Some(format!("\x1bO{letter}").into_bytes())
            } else {
                Some(format!("\x1b[1;{code}{letter}").into_bytes())
            }
        }
        "f5" => tilde(15, code),
        "f6" => tilde(17, code),
        "f7" => tilde(18, code),
        "f8" => tilde(19, code),
        "f9" => tilde(20, code),
        "f10" => tilde(21, code),
        "f11" => tilde(23, code),
        "f12" => tilde(24, code),
        _ => {
            let mut chars = key.chars();
            let (Some(ch), None) = (chars.next(), chars.next()) else {
                return None;
            };
            if ctrl {
                // Ctrl+字母是 1 到 26 号控制字符，另外几个符号各有对应
                let byte = match ch.to_ascii_lowercase() {
                    c @ 'a'..='z' => c as u8 - b'a' + 1,
                    '@' | '2' => 0,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '/' | '-' | '7' => 0x1f,
                    '8' | '?' => 0x7f,
                    _ => return None,
                };
                with_alt(vec![byte])
            } else if alt {
                // Alt+字：ESC 加上这个字（按了 Shift 就是大写或上档的符号）
                let typed = keystroke
                    .key_char
                    .clone()
                    .filter(|typed| !typed.is_empty())
                    .unwrap_or_else(|| {
                        if shift {
                            ch.to_uppercase().to_string()
                        } else {
                            ch.to_string()
                        }
                    });
                Some(format!("\x1b{typed}").into_bytes())
            } else {
                None
            }
        }
    }
}

/// `ESC [ 数字 ~` 这一类键（Insert、Delete、翻页、F5 以后的功能键）。
fn tilde(number: u8, code: u8) -> Option<Vec<u8>> {
    Some(if code == 1 {
        format!("\x1b[{number}~").into_bytes()
    } else {
        format!("\x1b[{number};{code}~").into_bytes()
    })
}

/// 滚一格滚轮报给要了鼠标的程序的字节（xterm 的写法）。`col`、`row` 从 0 起；`extra` 是
/// 修饰键（Alt 8、Ctrl 16）。程序开了 SGR（`sgr`）就用它，开了 UTF-8 坐标（`utf8`）次之，
/// 都没开是最早的写法；后两种表示不了的坐标（最早的写法超过 223）不报。
pub fn wheel_report(
    up: bool,
    col: usize,
    row: usize,
    extra: u8,
    sgr: bool,
    utf8: bool,
) -> Option<Vec<u8>> {
    let button = if up { 64 } else { 65 } + extra;
    if sgr {
        return Some(format!("\x1b[<{button};{};{}M", col + 1, row + 1).into_bytes());
    }
    let mut out = b"\x1b[M".to_vec();
    out.push(32 + button);
    for value in [col + 1, row + 1] {
        let code = u32::try_from(32 + value).ok()?;
        match utf8 {
            true if code <= 0x7ff => {
                let mut buf = [0; 4];
                out.extend_from_slice(char::from_u32(code)?.encode_utf8(&mut buf).as_bytes());
            }
            false if code <= 0xff => out.push(code as u8),
            _ => return None,
        }
    }
    Some(out)
}

/// 粘贴进去的文字：程序开了「括号粘贴」就包上标记（它知道这是粘贴的，不会一行行当命令执行），
/// 没开就把换行换成回车。文字里的 ESC 去掉，免得粘贴的内容冒充按键。
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace('\x1b', "");
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::{WheelTarget, encode, paste, wheel_report, wheel_target};
    use alacritty_terminal::term::TermMode;
    use gpui_kit::Keystroke;

    #[test]
    fn 滚轮交给谁() {
        // 「滚轮当上下键」终端一开始就开着；Claude Code 切到备用屏，又要了鼠标（SGR 写法）。
        // 以前只看备用屏，滚轮成了上下键，Claude Code 提示「Scroll wheel is sending arrow keys」
        let claude = TermMode::ALT_SCREEN
            | TermMode::ALTERNATE_SCROLL
            | TermMode::MOUSE_REPORT_CLICK
            | TermMode::MOUSE_DRAG
            | TermMode::SGR_MOUSE;
        assert_eq!(wheel_target(claude, false), WheelTarget::Report);
        // 按着 Shift：翻终端记着的输出
        assert_eq!(wheel_target(claude, true), WheelTarget::Scrollback);
        // less、没开鼠标的 vim：上下键
        let less = TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL;
        assert_eq!(wheel_target(less, false), WheelTarget::Arrows);
        // 主屏上的程序要了鼠标（只跟踪移动的也算）
        assert_eq!(
            wheel_target(TermMode::MOUSE_MOTION, false),
            WheelTarget::Report
        );
        // 普通的命令行
        assert_eq!(
            wheel_target(TermMode::ALTERNATE_SCROLL, false),
            WheelTarget::Scrollback
        );
    }

    #[test]
    fn 滚轮报给要了鼠标的程序() {
        // SGR（Claude Code、vim 用的）：上 64、下 65，坐标从 1 起
        let sgr = |up, col, row, extra| wheel_report(up, col, row, extra, true, false).unwrap();
        assert_eq!(sgr(true, 0, 0, 0), b"\x1b[<64;1;1M");
        assert_eq!(sgr(false, 79, 23, 0), b"\x1b[<65;80;24M");
        assert_eq!(sgr(true, 4, 2, 16), b"\x1b[<80;5;3M");
        // 最早的写法：每个数加 32 成一个字节，超过 223 表示不了
        assert_eq!(
            wheel_report(true, 0, 0, 0, false, false).unwrap(),
            b"\x1b[M\x60\x21\x21"
        );
        assert_eq!(
            wheel_report(false, 222, 0, 0, false, false).unwrap()[4],
            255
        );
        assert_eq!(wheel_report(false, 223, 0, 0, false, false), None);
        // UTF-8 坐标：大的坐标用两个字节
        assert_eq!(
            wheel_report(false, 299, 0, 0, false, true).unwrap(),
            b"\x1b[M\x61\xc5\x8c\x21"
        );
    }

    fn send(source: &'static str) -> Option<String> {
        let keystroke = Keystroke::parse(source).unwrap();
        encode(&keystroke, false).map(|bytes| String::from_utf8(bytes).unwrap())
    }

    #[test]
    fn 常用的键() {
        assert_eq!(send("enter").as_deref(), Some("\r"));
        assert_eq!(send("shift-enter").as_deref(), Some("\x1b\r"));
        assert_eq!(send("tab").as_deref(), Some("\t"));
        assert_eq!(send("shift-tab").as_deref(), Some("\x1b[Z"));
        assert_eq!(send("backspace").as_deref(), Some("\x7f"));
        assert_eq!(send("escape").as_deref(), Some("\x1b"));
        assert_eq!(send("up").as_deref(), Some("\x1b[A"));
        assert_eq!(send("ctrl-right").as_deref(), Some("\x1b[1;5C"));
        assert_eq!(send("shift-home").as_deref(), Some("\x1b[1;2H"));
        assert_eq!(send("delete").as_deref(), Some("\x1b[3~"));
        assert_eq!(send("pageup").as_deref(), Some("\x1b[5~"));
        assert_eq!(send("f1").as_deref(), Some("\x1bOP"));
        assert_eq!(send("f5").as_deref(), Some("\x1b[15~"));
        assert_eq!(send("shift-f5").as_deref(), Some("\x1b[15;2~"));
    }

    #[test]
    fn 方向键的应用模式() {
        let up = Keystroke::parse("up").unwrap();
        assert_eq!(encode(&up, true).unwrap(), b"\x1bOA");
        // 带修饰键时不分模式
        let ctrl_up = Keystroke::parse("ctrl-up").unwrap();
        assert_eq!(encode(&ctrl_up, true).unwrap(), b"\x1b[1;5A");
    }

    #[test]
    fn ctrl和alt的组合() {
        assert_eq!(send("ctrl-c").as_deref(), Some("\x03"));
        assert_eq!(send("ctrl-w").as_deref(), Some("\x17"));
        assert_eq!(send("ctrl-[").as_deref(), Some("\x1b"));
        assert_eq!(send("ctrl-space").as_deref(), Some("\0"));
        assert_eq!(send("alt-b").as_deref(), Some("\x1bb"));
        assert_eq!(send("alt-backspace").as_deref(), Some("\x1b\x7f"));
        assert_eq!(send("ctrl-alt-c").as_deref(), Some("\x1b\x03"));
    }

    #[test]
    fn 普通打字不归这里管() {
        assert_eq!(send("a"), None);
        assert_eq!(send("shift-a"), None);
        assert_eq!(send("space"), None);
        assert_eq!(send("1"), None);
    }

    #[test]
    fn 粘贴() {
        assert_eq!(paste("a\r\nb\nc", false), b"a\rb\rc");
        assert_eq!(paste("a\nb", true), b"\x1b[200~a\nb\x1b[201~");
        // 里面的 ESC 去掉，冒充不了「粘贴结束」
        assert_eq!(paste("x\x1b[201~y", true), b"\x1b[200~x[201~y\x1b[201~");
    }
}
