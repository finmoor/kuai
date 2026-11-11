//! 各处共用的小部件和尺寸：图标、头像、计数的小圆角块、分组标题。
//!
//! 界面的层级靠三样东西：文字的三级颜色（正文、次要、最弱）、主色只用在「现在选中的」和
//! 「主要操作」上、同一类东西用同一个行高和圆角。这里把它们集中定下来，各个页面不各写各的。

use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _};
use gpui_kit::prelude::*;
use gpui_kit::*;

/// 行和小块的圆角。
pub const RADIUS: f32 = 6.;

/// 一个图标，按 14 像素画（和 13、14 号字的 x 高度配得上）。
pub fn icon(name: IconName) -> Icon {
    Icon::new(name).size_3p5()
}

/// 小一号的图标：行尾的关闭、展开箭头。
pub fn icon_small(name: IconName) -> Icon {
    Icon::new(name).xsmall()
}

/// 计数的小块：侧栏「本地修改」后面的数字、分组里有几项。
pub fn count_pill(count: usize, strong: bool, cx: &App) -> Div {
    let theme = cx.theme();
    let (bg, fg) = if strong {
        (theme.primary, theme.primary_foreground)
    } else {
        (theme.muted, theme.muted_foreground)
    };
    div()
        .flex_none()
        .px(px(6.))
        .h(px(16.))
        .min_w(px(18.))
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_size(px(10.5))
        .line_height(px(16.))
        .font_weight(FontWeight::MEDIUM)
        .text_center()
        .child(count.to_string())
}

/// 按名字生成的头像：一个带颜色的圆，里面是名字的首字。同一个名字颜色不变。
pub fn avatar(name: &str, size: f32, cx: &App) -> Div {
    let theme = cx.theme();
    let colors = [
        theme.blue,
        theme.green,
        theme.magenta,
        theme.cyan,
        theme.yellow,
        theme.red,
    ];
    let color = colors[avatar_slot(name, colors.len())];
    div()
        .flex_none()
        .size(px(size))
        .rounded_full()
        .bg(color.opacity(0.22))
        .text_color(color)
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(size * 0.5))
        .line_height(px(size))
        .font_weight(FontWeight::SEMIBOLD)
        .child(initial(name))
}

/// 名字落在第几个颜色上。
fn avatar_slot(name: &str, slots: usize) -> usize {
    // FNV-1a：同一个名字每次、每台电脑上算出来都一样
    let mut hash: u32 = 0x811c9dc5;
    for byte in name.trim().to_lowercase().bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash as usize % slots.max(1)
}

/// 头像里的字：英文名取首字母（大写），中文名取第一个字。
fn initial(name: &str) -> String {
    name.trim()
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".into())
}

#[cfg(test)]
mod tests {
    use super::{avatar_slot, initial};

    #[test]
    fn 头像取名字的首字() {
        assert_eq!(initial("feng"), "F");
        assert_eq!(initial("  张三"), "张");
        assert_eq!(initial("<bot>"), "B");
        assert_eq!(initial(""), "?");
    }

    #[test]
    fn 同一个名字颜色不变() {
        assert_eq!(avatar_slot("Feng", 6), avatar_slot("feng ", 6));
        assert!(avatar_slot("别人", 6) < 6);
        assert_eq!(avatar_slot("x", 0), 0);
    }
}
