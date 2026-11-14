//! 终端的设置（字体、光标、往回翻多少行……）怎么到各个终端手里。
//!
//! 设置存在 [`crate::settings::TerminalSettings`] 里，所有终端共用。终端的视图不认识全局状态，
//! 所以放一份在界面库的全局变量里（[`set_prefs`]）：设置一变就换掉，开着的终端看到变化各自重画。
//! 反过来，在终端里按 Ctrl+加号、减号调字号，经 [`set_prefs_writer`] 登记的函数存回设置。

use std::rc::Rc;
use std::sync::OnceLock;

use alacritty_terminal::term::Config;
use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle};
use gpui_kit::*;

use crate::settings::{TerminalCursor, TerminalSettings};

/// 现在生效的终端设置。
pub(super) struct Prefs(TerminalSettings);

impl Global for Prefs {}

/// 把改过的终端设置存回去的函数。
struct Writer(Rc<dyn Fn(TerminalSettings, &mut App)>);

impl Global for Writer {}

/// 设置变了（或者程序刚启动）：换成这一份。没变就不动，免得所有终端白白重画。
pub fn set_prefs(settings: &TerminalSettings, cx: &mut App) {
    if cx
        .try_global::<Prefs>()
        .is_some_and(|prefs| prefs.0 == *settings)
    {
        return;
    }
    cx.set_global(Prefs(settings.clone()));
}

/// 登记「把终端设置存回去」的函数（程序启动时调用一次）。
pub fn set_prefs_writer(cx: &mut App, write: impl Fn(TerminalSettings, &mut App) + 'static) {
    cx.set_global(Writer(Rc::new(write)));
}

pub(super) fn get(cx: &App) -> TerminalSettings {
    cx.try_global::<Prefs>()
        .map(|prefs| prefs.0.clone())
        .unwrap_or_default()
}

/// 存回去；没登记过（测试里）就只换全局的那一份。
pub(super) fn write(prefs: TerminalSettings, cx: &mut App) {
    match cx.try_global::<Writer>().map(|writer| writer.0.clone()) {
        Some(write) => write(prefs, cx),
        None => set_prefs(&prefs, cx),
    }
}

/// 交给终端仿真器的那几项：往回翻多少行、光标默认的样子。
pub(super) fn config(prefs: &TerminalSettings) -> Config {
    Config {
        scrolling_history: prefs.scrollback,
        default_cursor_style: CursorStyle {
            shape: match prefs.cursor {
                TerminalCursor::Block => CursorShape::Block,
                TerminalCursor::Beam => CursorShape::Beam,
                TerminalCursor::Underline => CursorShape::Underline,
            },
            blinking: prefs.cursor_blink,
        },
        ..Config::default()
    }
}

/// 画字用的字体：选的等宽字体，里面没有的字（中文）先找选的中文字体，再由系统挑。
pub(super) fn base_font(prefs: &TerminalSettings) -> Font {
    let mut font = font(SharedString::from(prefs.font.clone()));
    if !prefs.cjk_font.is_empty() {
        font.fallbacks = Some(FontFallbacks::from_fonts(vec![prefs.cjk_font.clone()]));
    }
    font
}

/// 系统里装着的字体里能给终端用的：等宽的，和带中文的。
pub struct FontChoices {
    /// 等宽字体（i 和 M 一样宽）
    pub monospace: Vec<SharedString>,
    /// 有中文字形的字体
    pub cjk: Vec<SharedString>,
}

/// 挑一遍系统字体（几百个，第一次要几十到几百毫秒），之后用记下来的。
pub fn font_choices(cx: &App) -> &'static FontChoices {
    static CHOICES: OnceLock<FontChoices> = OnceLock::new();
    CHOICES.get_or_init(|| {
        let text = cx.text_system();
        let size = px(16.);
        let (mut monospace, mut cjk) = (Vec::new(), Vec::new());
        for name in text.all_font_names() {
            // @ 开头的是竖排用的同一个字体
            if name.starts_with('@') {
                continue;
            }
            let id = text.resolve_font(&font(SharedString::from(name.clone())));
            let width = |ch: char| text.advance(id, size, ch).ok().map(|advance| advance.width);
            // 图标字体（Segoe MDL2 Assets 之类）也是每个字形一样宽，但宽度是一整个字号，
            // 字母位置上是图标：等宽的西文字体，字母只有半个字号多一点宽
            if let (Some(narrow), Some(wide)) = (width('i'), width('M'))
                && narrow > px(0.)
                && narrow < size * 0.75
                && (narrow - wide).abs() < px(0.01)
            {
                monospace.push(name.clone().into());
            }
            if width('中').is_some() && width('字').is_some() {
                cjk.push(name.into());
            }
        }
        FontChoices { monospace, cjk }
    })
}
