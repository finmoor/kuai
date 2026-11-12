//! 终端里的颜色：16 个基本色跟着界面的深浅各一套，256 色和真彩色按标准算。
//! 默认的前景、背景用界面主题的，终端和周围是一个底色。

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use gpui_kit::{Hsla, Rgba};

/// 深色界面用的 16 色（黑、红、绿、黄、蓝、紫、青、白，再加亮的八个）。
const DARK: [u32; 16] = [
    0x3f4451, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xd7dae0, 0x5a6374,
    0xff7b86, 0xb1e18b, 0xefcb85, 0x74baff, 0xdd8cf2, 0x67d3e0, 0xffffff,
];

/// 浅色界面用的 16 色。
const LIGHT: [u32; 16] = [
    0x383a42, 0xd43d30, 0x3d8a3c, 0xa87000, 0x0b6fb8, 0xa626a4, 0x08809a, 0xb8bcc4, 0x6a6e7a,
    0xe45649, 0x50a14f, 0xc18401, 0x2f8be0, 0xc050c8, 0x0997b3, 0x8a8f99,
];

/// 把终端里的颜色换成界面能画的颜色。
pub struct Palette {
    pub foreground: Hsla,
    pub background: Hsla,
    base: &'static [u32; 16],
}

impl Palette {
    pub fn new(foreground: Hsla, background: Hsla, dark: bool) -> Self {
        Self {
            foreground,
            background,
            base: if dark { &DARK } else { &LIGHT },
        }
    }

    /// `overrides` 是程序自己改过的颜色（OSC 4、10、11），改过的以它为准。
    pub fn resolve(&self, color: Color, overrides: &Colors) -> Hsla {
        match color {
            Color::Spec(rgb) => from_rgb(rgb),
            Color::Indexed(index) => match overrides[index as usize] {
                Some(rgb) => from_rgb(rgb),
                None => self.indexed(index),
            },
            Color::Named(name) => match overrides[name] {
                Some(rgb) => from_rgb(rgb),
                None => self.named(name),
            },
        }
    }

    fn named(&self, name: NamedColor) -> Hsla {
        use NamedColor::*;
        match name {
            Foreground | BrightForeground => self.foreground,
            Background => self.background,
            Cursor => self.foreground,
            DimForeground => dim(self.foreground),
            DimBlack | DimRed | DimGreen | DimYellow | DimBlue | DimMagenta | DimCyan
            | DimWhite => dim(from_hex(self.base[name as usize - DimBlack as usize])),
            other => from_hex(self.base[other as usize]),
        }
    }

    /// 256 色表：前 16 个是基本色，接着 6×6×6 的色块，最后 24 级灰。
    fn indexed(&self, index: u8) -> Hsla {
        match index {
            0..=15 => from_hex(self.base[index as usize]),
            16..=231 => {
                let index = index - 16;
                let level = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
                from_rgb(Rgb {
                    r: level(index / 36),
                    g: level(index / 6 % 6),
                    b: level(index % 6),
                })
            }
            _ => {
                let gray = 8 + 10 * (index - 232);
                from_rgb(Rgb {
                    r: gray,
                    g: gray,
                    b: gray,
                })
            }
        }
    }
}

/// 暗一档（SGR 2）：淡掉三分之一。
pub fn dim(mut color: Hsla) -> Hsla {
    color.a *= 0.66;
    color
}

/// 界面的颜色换回终端协议里的 RGB（程序问「背景是什么颜色」时回答用）。
pub fn to_rgb(color: Hsla) -> Rgb {
    let rgba: Rgba = color.into();
    let byte = |value: f32| (value * 255.).round().clamp(0., 255.) as u8;
    Rgb {
        r: byte(rgba.r),
        g: byte(rgba.g),
        b: byte(rgba.b),
    }
}

fn from_rgb(rgb: Rgb) -> Hsla {
    Rgba {
        r: rgb.r as f32 / 255.,
        g: rgb.g as f32 / 255.,
        b: rgb.b as f32 / 255.,
        a: 1.,
    }
    .into()
}

fn from_hex(hex: u32) -> Hsla {
    gpui_kit::rgb(hex).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_of(color: Hsla) -> (u8, u8, u8) {
        let rgba: Rgba = color.into();
        let byte = |v: f32| (v * 255.).round() as u8;
        (byte(rgba.r), byte(rgba.g), byte(rgba.b))
    }

    #[test]
    fn 颜色表的几个点() {
        let palette = Palette::new(from_hex(0xeeeeee), from_hex(0x111111), true);
        let none = Colors::default();
        // 基本色
        assert_eq!(
            rgb_of(palette.resolve(Color::Named(NamedColor::Red), &none)),
            (0xe0, 0x6c, 0x75)
        );
        assert_eq!(
            rgb_of(palette.resolve(Color::Indexed(1), &none)),
            (0xe0, 0x6c, 0x75)
        );
        // 色块的两头和灰阶的两头
        assert_eq!(
            rgb_of(palette.resolve(Color::Indexed(16), &none)),
            (0, 0, 0)
        );
        assert_eq!(
            rgb_of(palette.resolve(Color::Indexed(231), &none)),
            (255, 255, 255)
        );
        assert_eq!(
            rgb_of(palette.resolve(Color::Indexed(232), &none)),
            (8, 8, 8)
        );
        assert_eq!(
            rgb_of(palette.resolve(Color::Indexed(255), &none)),
            (238, 238, 238)
        );
        // 默认的前景、背景跟界面走
        assert_eq!(
            rgb_of(palette.resolve(Color::Named(NamedColor::Background), &none)),
            (0x11, 0x11, 0x11)
        );
        // 程序改过的颜色优先
        let mut changed = Colors::default();
        changed[1] = Some(Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(
            rgb_of(palette.resolve(Color::Indexed(1), &changed)),
            (1, 2, 3)
        );
    }
}
