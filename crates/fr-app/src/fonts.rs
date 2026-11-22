//! 等宽字体：界面上要对齐的地方（提交号、差异、路径、终端）都用 [`mono`]。
//!
//! Windows 上就是 Consolas（每台都有）。Linux 上没有 Consolas，GPUI 找不到字体时退回比例字体，
//! 对不齐；启动时问 fontconfig 系统设的等宽字体（`fc-match monospace`），GPUI 认得就用它，不认得再
//! 从常见的几种里挑一个装着的。终端设置里写的字体这台电脑上没装时也退回它（[`usable`]）。

use std::collections::HashSet;
use std::sync::OnceLock;

use gpui_kit::App;

/// 没初始化、什么都没找到时用的。
const DEFAULT: &str = if cfg!(windows) {
    "Consolas"
} else {
    "DejaVu Sans Mono"
};

/// Linux 上 fontconfig 给的那个 GPUI 不认得时，按先后试这些。
#[cfg_attr(windows, allow(dead_code))]
const CANDIDATES: [&str; 10] = [
    "DejaVu Sans Mono",
    "Noto Sans Mono",
    "Liberation Mono",
    "Ubuntu Sans Mono",
    "Ubuntu Mono",
    "Adwaita Mono",
    "Cascadia Mono",
    "Source Code Pro",
    "JetBrains Mono",
    "Hack",
];

static MONO: OnceLock<String> = OnceLock::new();
/// 装着的字体（只在 Linux 上记）
static INSTALLED: OnceLock<HashSet<String>> = OnceLock::new();

/// 启动时、开窗口之前调一次。
pub fn init(cx: &App) {
    if cfg!(windows) {
        return;
    }
    let installed: HashSet<String> = cx.text_system().all_font_names().into_iter().collect();
    let mono = system_monospace()
        .into_iter()
        .chain(CANDIDATES.iter().map(|name| name.to_string()))
        .find(|name| installed.contains(name))
        .unwrap_or_else(|| DEFAULT.to_string());
    log::info!("等宽字体：{mono}");
    let _ = MONO.set(mono);
    let _ = INSTALLED.set(installed);
}

/// fontconfig 里 `monospace` 指的字体。
fn system_monospace() -> Option<String> {
    let output = std::process::Command::new("fc-match")
        .args(["-f", "%{family[0]}", "monospace"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let name = String::from_utf8(output.stdout).ok()?;
    let name = name.trim();
    (output.status.success() && !name.is_empty()).then(|| name.to_string())
}

/// 界面上用的等宽字体。
pub fn mono() -> &'static str {
    MONO.get().map(String::as_str).unwrap_or(DEFAULT)
}

/// 设置里选的字体：这台电脑上没装就换成 [`mono`]（只在 Linux 上查；Windows 上照旧交给 GPUI）。
pub fn usable(name: &str) -> &str {
    match INSTALLED.get() {
        Some(installed) if !installed.contains(name) => mono(),
        _ => name,
    }
}
