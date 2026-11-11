//! 配色。每套配色只定十几个颜色（背景、面板、文字的三个层级、主色、六个标志色），
//! 组件库要的上百个颜色键从它们推出来，所以各套主题的明暗关系、层级是一致的。
//!
//! 定配色时守的几条：
//! - 深色主题不用纯黑底、纯白字：底色带一点灰，正文的对比度压到 11:1 左右，长时间看不刺眼。
//! - 文字分三级：正文（`fg`）、次要（`muted`，路径、时间、说明）、最弱（`subtle`，占位符、分隔）。
//! - 主按钮、选中、链接都用同一个主色（`accent`），不用和文字同色的黑白按钮。

use fr_core::i18n::{tr, tr_key};
use gpui_kit::component::ThemeConfig;
use serde_json::{Value, json};

pub struct Palette {
    /// 设置里存的名字
    pub id: &'static str,
    /// 显示的名字（专有名词不翻译，「默认」两套翻译）
    name: &'static str,
    pub dark: bool,
    /// 主背景：列表、改动内容
    pub bg: &'static str,
    /// 侧栏、标题栏、工具栏
    pub panel: &'static str,
    /// 浮起来的东西：菜单、对话框、通知
    pub raised: &'static str,
    pub border: &'static str,
    /// 正文
    pub fg: &'static str,
    /// 次要文字
    pub muted: &'static str,
    /// 悬停的行、次要按钮的底
    pub hover: &'static str,
    /// 选中的行
    pub select: &'static str,
    /// 主色
    pub accent: &'static str,
    /// 主色上面的文字
    pub accent_fg: &'static str,
    pub red: &'static str,
    pub green: &'static str,
    pub yellow: &'static str,
    pub blue: &'static str,
    pub magenta: &'static str,
    pub cyan: &'static str,
}

impl Palette {
    pub fn label(&self) -> &'static str {
        match self.id {
            "light" => tr("默认浅色"),
            "dark" => tr("默认深色"),
            _ => self.name,
        }
    }
}

pub const PALETTES: &[Palette] = &[
    Palette {
        id: "light",
        name: tr_key("默认浅色"),
        dark: false,
        bg: "#ffffff",
        panel: "#f4f5f7",
        raised: "#ffffff",
        border: "#e1e4e8",
        fg: "#1f2328",
        muted: "#656d76",
        hover: "#eaedf0",
        select: "#dce8fd",
        accent: "#2563eb",
        accent_fg: "#ffffff",
        red: "#cf222e",
        green: "#1a7f37",
        yellow: "#9a6700",
        blue: "#0969da",
        magenta: "#8250df",
        cyan: "#1b7c83",
    },
    Palette {
        id: "dark",
        name: tr_key("默认深色"),
        dark: true,
        bg: "#1e1f22",
        panel: "#26282c",
        raised: "#2b2d31",
        border: "#393b40",
        fg: "#d5d8de",
        muted: "#8e939c",
        hover: "#2f3136",
        select: "#2c4470",
        accent: "#3b7bf6",
        accent_fg: "#ffffff",
        red: "#f0635c",
        green: "#5db563",
        yellow: "#d4a23c",
        blue: "#5aa2f7",
        magenta: "#b68af2",
        cyan: "#41bfc9",
    },
    Palette {
        id: "github-light",
        name: "GitHub Light",
        dark: false,
        bg: "#ffffff",
        panel: "#f6f8fa",
        raised: "#ffffff",
        border: "#d1d9e0",
        fg: "#1f2328",
        muted: "#59636e",
        hover: "#eaeef2",
        select: "#ddf4ff",
        accent: "#1f883d",
        accent_fg: "#ffffff",
        red: "#d1242f",
        green: "#1a7f37",
        yellow: "#9a6700",
        blue: "#0969da",
        magenta: "#8250df",
        cyan: "#1b7c83",
    },
    Palette {
        id: "solarized-light",
        name: "Solarized Light",
        dark: false,
        bg: "#fdf6e3",
        panel: "#f3ecd8",
        raised: "#fdf6e3",
        border: "#ddd6c1",
        fg: "#4f666d",
        muted: "#71878c",
        hover: "#eee8d5",
        select: "#e7e0ca",
        accent: "#268bd2",
        accent_fg: "#fdf6e3",
        red: "#dc322f",
        green: "#859900",
        yellow: "#b58900",
        blue: "#268bd2",
        magenta: "#d33682",
        cyan: "#2aa198",
    },
    Palette {
        id: "catppuccin-latte",
        name: "Catppuccin Latte",
        dark: false,
        bg: "#eff1f5",
        panel: "#e6e9ef",
        raised: "#eff1f5",
        border: "#ccd0da",
        fg: "#4c4f69",
        muted: "#6c6f85",
        hover: "#dce0e8",
        select: "#c5cdf0",
        accent: "#1e66f5",
        accent_fg: "#eff1f5",
        red: "#d20f39",
        green: "#40a02b",
        yellow: "#df8e1d",
        blue: "#1e66f5",
        magenta: "#8839ef",
        cyan: "#179299",
    },
    Palette {
        id: "github-dimmed",
        name: "GitHub Dark Dimmed",
        dark: true,
        bg: "#22272e",
        panel: "#1c2128",
        raised: "#2d333b",
        border: "#444c56",
        fg: "#adbac7",
        muted: "#768390",
        hover: "#2d333b",
        select: "#2b4571",
        accent: "#347d39",
        accent_fg: "#ffffff",
        red: "#f47067",
        green: "#57ab5a",
        yellow: "#c69026",
        blue: "#539bf5",
        magenta: "#b083f0",
        cyan: "#39c5cf",
    },
    Palette {
        id: "one-dark",
        name: "One Dark",
        dark: true,
        bg: "#282c34",
        panel: "#21252b",
        raised: "#2c313a",
        border: "#3b4048",
        fg: "#abb2bf",
        muted: "#7f848e",
        hover: "#2c313a",
        select: "#3a4a66",
        accent: "#4d78cc",
        accent_fg: "#ffffff",
        red: "#e06c75",
        green: "#98c379",
        yellow: "#e5c07b",
        blue: "#61afef",
        magenta: "#c678dd",
        cyan: "#56b6c2",
    },
    Palette {
        id: "dracula",
        name: "Dracula",
        dark: true,
        bg: "#282a36",
        panel: "#21222c",
        raised: "#343746",
        border: "#44475a",
        fg: "#e6e6e0",
        muted: "#9aa0c3",
        hover: "#343746",
        select: "#4a4d6a",
        accent: "#bd93f9",
        accent_fg: "#21222c",
        red: "#ff5555",
        green: "#50fa7b",
        yellow: "#f1fa8c",
        blue: "#8be9fd",
        magenta: "#ff79c6",
        cyan: "#8be9fd",
    },
    Palette {
        id: "nord",
        name: "Nord",
        dark: true,
        bg: "#2e3440",
        panel: "#272c36",
        raised: "#3b4252",
        border: "#434c5e",
        fg: "#d8dee9",
        muted: "#9aa5b8",
        hover: "#3b4252",
        select: "#46546c",
        accent: "#88c0d0",
        accent_fg: "#2e3440",
        red: "#bf616a",
        green: "#a3be8c",
        yellow: "#ebcb8b",
        blue: "#81a1c1",
        magenta: "#b48ead",
        cyan: "#8fbcbb",
    },
    Palette {
        id: "tokyo-night",
        name: "Tokyo Night",
        dark: true,
        bg: "#1a1b26",
        panel: "#16161e",
        raised: "#24283b",
        border: "#2f3549",
        fg: "#a9b1d6",
        muted: "#737aa2",
        hover: "#24283b",
        select: "#2f3d66",
        accent: "#7aa2f7",
        accent_fg: "#16161e",
        red: "#f7768e",
        green: "#9ece6a",
        yellow: "#e0af68",
        blue: "#7aa2f7",
        magenta: "#bb9af7",
        cyan: "#7dcfff",
    },
    Palette {
        id: "catppuccin-mocha",
        name: "Catppuccin Mocha",
        dark: true,
        bg: "#1e1e2e",
        panel: "#181825",
        raised: "#313244",
        border: "#3b3d52",
        fg: "#cdd6f4",
        muted: "#9399b2",
        hover: "#313244",
        select: "#45475a",
        accent: "#89b4fa",
        accent_fg: "#1e1e2e",
        red: "#f38ba8",
        green: "#a6e3a1",
        yellow: "#f9e2af",
        blue: "#89b4fa",
        magenta: "#cba6f7",
        cyan: "#94e2d5",
    },
    Palette {
        id: "gruvbox-dark",
        name: "Gruvbox Dark",
        dark: true,
        bg: "#282828",
        panel: "#1d2021",
        raised: "#3c3836",
        border: "#504945",
        fg: "#ebdbb2",
        muted: "#a89984",
        hover: "#3c3836",
        select: "#504945",
        accent: "#d79921",
        accent_fg: "#1d2021",
        red: "#fb4934",
        green: "#b8bb26",
        yellow: "#fabd2f",
        blue: "#83a598",
        magenta: "#d3869b",
        cyan: "#8ec07c",
    },
    Palette {
        id: "solarized-dark",
        name: "Solarized Dark",
        dark: true,
        bg: "#002b36",
        panel: "#00252e",
        raised: "#073642",
        border: "#0e4753",
        fg: "#9aa8a8",
        muted: "#6c8188",
        hover: "#073642",
        select: "#0b4250",
        accent: "#268bd2",
        accent_fg: "#fdf6e3",
        red: "#dc322f",
        green: "#859900",
        yellow: "#b58900",
        blue: "#268bd2",
        magenta: "#d33682",
        cyan: "#2aa198",
    },
];

/// 按 id 找配色；找不到（设置文件里是旧名字、手改错了）就用这个明暗下的默认配色。
pub fn find(id: &str, dark: bool) -> &'static Palette {
    PALETTES
        .iter()
        .find(|p| p.id == id && p.dark == dark)
        .unwrap_or_else(|| default_for(dark))
}

pub fn default_for(dark: bool) -> &'static Palette {
    PALETTES
        .iter()
        .find(|p| p.dark == dark)
        .expect("深色、浅色各至少有一套配色")
}

/// `#rrggbb` → (r, g, b)
fn rgb(hex: &str) -> (u8, u8, u8) {
    let v = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
    ((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// 两个颜色按比例混：`t` 是 `b` 占的份量。
pub fn mix(a: &str, b: &str, t: f32) -> String {
    let (a, b) = (rgb(a), rgb(b));
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        ch(a.0, b.0),
        ch(a.1, b.1),
        ch(a.2, b.2)
    )
}

/// 相对亮度（WCAG），算对比度用。
#[cfg(test)]
fn luminance(hex: &str) -> f32 {
    let (r, g, b) = rgb(hex);
    let lin = |c: u8| {
        let c = c as f32 / 255.;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// 两个颜色的对比度（1 到 21）。配色的测试用它查文字看不看得清。
#[cfg(test)]
pub fn contrast(a: &str, b: &str) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

impl Palette {
    /// 推出组件库要的主题配置。
    pub fn config(&self) -> ThemeConfig {
        let p = self;
        // 主色的悬停、按下：浅色主题往暗里走，深色主题往亮里走
        let toward = if p.dark { "#ffffff" } else { "#000000" };
        let accent_hover = mix(p.accent, toward, 0.10);
        let accent_active = mix(p.accent, toward, 0.20);
        // 次要按钮：比悬停的底再重一点，看得出是个按钮
        let secondary = mix(p.hover, p.border, 0.35);
        let secondary_hover = mix(p.hover, p.border, 0.75);
        // 标志色上面的字：浅色主题用白的，深色主题用底色（标志色都比较亮）
        let on_color = if p.dark { p.bg } else { "#ffffff" };
        let overlay = if p.dark { "#00000080" } else { "#00000033" };
        let colors: Vec<(&str, String)> = vec![
            ("background", p.bg.to_string()),
            ("foreground", p.fg.to_string()),
            ("border", p.border.to_string()),
            ("accent.background", p.hover.to_string()),
            ("accent.foreground", p.fg.to_string()),
            ("muted.background", p.hover.to_string()),
            ("muted.foreground", p.muted.to_string()),
            ("primary.background", p.accent.to_string()),
            ("primary.foreground", p.accent_fg.to_string()),
            ("primary.hover.background", accent_hover.to_string()),
            ("primary.active.background", accent_active.to_string()),
            ("secondary.background", secondary.to_string()),
            ("secondary.foreground", p.fg.to_string()),
            ("secondary.hover.background", secondary_hover.to_string()),
            ("secondary.active.background", p.border.to_string()),
            ("popover.background", p.raised.to_string()),
            ("popover.foreground", p.fg.to_string()),
            ("accordion.background", p.bg.to_string()),
            ("group_box.background", p.panel.to_string()),
            ("group_box.foreground", p.fg.to_string()),
            ("description_list.label.background", p.panel.to_string()),
            ("description_list.label.foreground", p.muted.to_string()),
            ("caret", p.fg.to_string()),
            ("ring", p.accent.to_string()),
            ("input.border", mix(p.border, p.muted, 0.18)),
            ("selection.background", format!("{}66", p.accent)),
            ("link", p.blue.to_string()),
            ("link.hover", mix(p.blue, toward, 0.15)),
            ("link.active", mix(p.blue, toward, 0.25)),
            ("list.background", p.bg.to_string()),
            ("list.hover.background", p.hover.to_string()),
            ("list.active.background", p.select.to_string()),
            ("list.active.border", p.accent.to_string()),
            ("list.even.background", p.bg.to_string()),
            ("list.head.background", p.panel.to_string()),
            ("table.background", p.bg.to_string()),
            ("table.hover.background", p.hover.to_string()),
            ("table.active.background", p.select.to_string()),
            ("table.active.border", p.accent.to_string()),
            ("table.even.background", p.bg.to_string()),
            ("table.head.background", p.panel.to_string()),
            ("table.head.foreground", p.muted.to_string()),
            ("table.row.border", p.border.to_string()),
            ("sidebar.background", p.panel.to_string()),
            ("sidebar.foreground", p.fg.to_string()),
            ("sidebar.border", p.border.to_string()),
            ("sidebar.accent.background", p.select.to_string()),
            ("sidebar.accent.foreground", p.fg.to_string()),
            ("sidebar.primary.background", p.accent.to_string()),
            ("sidebar.primary.foreground", p.accent_fg.to_string()),
            ("title_bar.background", p.panel.to_string()),
            ("title_bar.border", p.border.to_string()),
            ("status_bar.background", p.panel.to_string()),
            ("status_bar.border", p.border.to_string()),
            ("tab_bar.background", p.panel.to_string()),
            ("tab_bar.segmented.background", p.hover.to_string()),
            ("tab.background", "#00000000".to_string()),
            ("tab.foreground", p.muted.to_string()),
            ("tab.active.background", p.bg.to_string()),
            ("tab.active.foreground", p.fg.to_string()),
            ("scrollbar.background", format!("{}00", p.bg)),
            ("scrollbar.thumb.background", format!("{}66", p.muted)),
            ("scrollbar.thumb.hover.background", format!("{}aa", p.muted)),
            ("skeleton.background", p.hover.to_string()),
            ("switch.background", mix(p.border, p.muted, 0.35)),
            ("slider.background", p.accent.to_string()),
            ("slider.thumb.background", p.raised.to_string()),
            ("progress.bar.background", p.accent.to_string()),
            ("danger.background", p.red.to_string()),
            ("danger.foreground", on_color.to_string()),
            ("success.background", p.green.to_string()),
            ("success.foreground", on_color.to_string()),
            ("warning.background", p.yellow.to_string()),
            ("warning.foreground", on_color.to_string()),
            ("info.background", p.blue.to_string()),
            ("info.foreground", on_color.to_string()),
            ("overlay", overlay.to_string()),
            ("window.border", p.border.to_string()),
            ("drag.border", p.accent.to_string()),
            ("drop_target.background", format!("{}33", p.accent)),
            ("chart.grid", p.border.to_string()),
            ("base.red", p.red.to_string()),
            ("base.red.light", p.red.to_string()),
            ("base.green", p.green.to_string()),
            ("base.green.light", p.green.to_string()),
            ("base.blue", p.blue.to_string()),
            ("base.blue.light", p.blue.to_string()),
            ("base.yellow", p.yellow.to_string()),
            ("base.yellow.light", p.yellow.to_string()),
            ("base.magenta", p.magenta.to_string()),
            ("base.magenta.light", p.magenta.to_string()),
            ("base.cyan", p.cyan.to_string()),
            ("base.cyan.light", p.cyan.to_string()),
        ];
        let colors: Value = Value::Object(
            colors
                .into_iter()
                .map(|(key, value)| (key.to_string(), Value::String(value)))
                .collect(),
        );
        let config = json!({
            "name": p.id,
            "mode": if p.dark { "dark" } else { "light" },
            "radius": 6,
            "radius.lg": 10,
            "shadow": true,
            "colors": colors,
        });
        serde_json::from_value(config).expect("配色推出来的主题配置应该合法")
    }

    /// 最弱的文字颜色：占位说明、分隔用的小字。
    pub fn subtle(&self) -> String {
        mix(self.muted, self.bg, 0.35)
    }
}

#[cfg(test)]
mod tests {
    use super::{PALETTES, contrast, default_for, find, mix};

    #[test]
    fn 每套配色都能推出主题配置() {
        for palette in PALETTES {
            let config = palette.config();
            assert_eq!(config.mode.is_dark(), palette.dark, "{}", palette.id);
        }
        // id 不重复
        let mut ids: Vec<_> = PALETTES.iter().map(|p| p.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), PALETTES.len());
    }

    #[test]
    fn 文字看得清又不刺眼() {
        for p in PALETTES {
            // 正文对背景至少 4.5（WCAG AA）；次要文字至少 3
            assert!(
                contrast(p.fg, p.bg) >= 4.5,
                "{} 正文 {}",
                p.id,
                contrast(p.fg, p.bg)
            );
            assert!(contrast(p.fg, p.panel) >= 4.5, "{} 面板上的正文", p.id);
            assert!(
                contrast(p.muted, p.bg) >= 3.0,
                "{} 次要文字 {}",
                p.id,
                contrast(p.muted, p.bg)
            );
            // 主按钮上的字看得清
            assert!(
                contrast(p.accent_fg, p.accent) >= 3.0,
                "{} 主按钮 {}",
                p.id,
                contrast(p.accent_fg, p.accent)
            );
            // 选中的行上正文还看得清
            assert!(
                contrast(p.fg, p.select) >= 4.0,
                "{} 选中行 {}",
                p.id,
                contrast(p.fg, p.select)
            );
            if p.dark {
                // 深色主题：不用纯黑底纯白字，对比度不超过 15，主按钮不是白的
                assert!(
                    contrast(p.fg, p.bg) <= 15.0,
                    "{} 太刺眼 {}",
                    p.id,
                    contrast(p.fg, p.bg)
                );
                assert!(
                    contrast(p.accent, "#ffffff") >= 1.5,
                    "{} 主按钮接近纯白",
                    p.id
                );
            }
        }
    }

    #[test]
    fn 找不到的名字退回默认配色() {
        assert_eq!(find("nord", true).id, "nord");
        assert_eq!(find("没有这个", true).id, "dark");
        // 名字对但明暗不对（浅色的位置填了深色配色）也退回去
        assert_eq!(find("nord", false).id, "light");
        assert_eq!(default_for(false).id, "light");
    }

    #[test]
    fn 颜色混合() {
        assert_eq!(mix("#000000", "#ffffff", 0.5), "#808080");
        assert_eq!(mix("#102030", "#ffffff", 0.0), "#102030");
    }
}
