//! 新建终端的预设：开一个终端，等 shell 的提示符出来替你输入一条命令、回车，比如 Claude Code、Codex
//! 这些命令行工具，或者自己加的 `npm run dev`。程序退出后终端留在 shell 里，可以接着敲命令。
//!
//! - 预设存在设置里（[`Settings::terminal_presets`]；没改过是 None，用 [`builtins`]），「新终端」按钮、
//!   Ctrl+Shift+` 开哪个看 [`Settings::terminal_default`]。界面各处读全局的一份（[`sync`]）。
//! - 哪个仓库的第几个终端是从哪个预设开的，记在内存里（[`remember`]）：重启这个终端时再输入一遍，
//!   程序没设标题时终端叫预设的名字。接上已有的会话（重开 Kuai、在别处开着的）不再输入。
//! - 内置的四个用各家的图标（`assets/logos/`，单色，画的时候填颜色）；自己加的从一组线条图标里挑。
//! - 内置的命令都不用备用屏（`CSI ?1049h`）：用了的话程序的画面不进往回翻的记录，往回翻、消息刻度、
//!   固定输入区都用不上。所以 Codex 带 `--no-alt-screen`，OpenCode 用 `opencode mini`。
//!
//! 先开 shell、再输入命令（而不是让程序当终端里唯一的进程）：这些工具多半装在靠 `.bashrc` 加进 PATH
//! 的地方，只有交互式的 shell 找得到；程序退出后终端还在；也不用改 kuaid 和协议。

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;
use serde::{Deserialize, Serialize};

use crate::app_state::AppState;
use crate::settings::TermSlot;
use crate::ui;

/// 一个预设。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preset {
    /// 认它用的：内置的是 `claude` 这些，自己加的是 `custom-1` 这样
    pub id: String,
    pub name: String,
    /// 在 shell 里输入的命令
    pub command: String,
    /// 「继续上次的会话」输入的命令；空着是没有
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resume: String,
    /// 图标：[`LOGOS`] 或 [`GLYPHS`] 里的名字
    #[serde(default)]
    pub icon: String,
    /// 颜色：工作区那一组里的第几种（[`ui::hue`]）；None 是默认的（各家的颜色，或者文字的颜色）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hue: Option<usize>,
    /// 在新建终端的菜单里列出来
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub shown: bool,
}

fn yes() -> bool {
    true
}

fn is_yes(value: &bool) -> bool {
    *value
}

/// 内置的预设。命令核对过的版本和出处见 `docs/design/terminal-presets/product-facts.md`。
pub fn builtins() -> Vec<Preset> {
    let preset = |id: &str, name: &str, command: &str, resume: &str, icon: &str| Preset {
        id: id.to_string(),
        name: name.to_string(),
        command: command.to_string(),
        resume: resume.to_string(),
        icon: icon.to_string(),
        hue: None,
        shown: true,
    };
    vec![
        preset(
            "claude",
            "Claude Code",
            "claude",
            "claude -c",
            "claude-code",
        ),
        preset(
            "codex",
            "Codex",
            "codex --no-alt-screen",
            "codex resume --last --no-alt-screen",
            "openai",
        ),
        preset(
            "opencode",
            "OpenCode",
            "opencode mini",
            "opencode mini -c",
            "opencode",
        ),
        preset("agy", "Antigravity CLI", "agy", "agy -c", "antigravity"),
    ]
}

/// 新开的终端要替用户输入的命令：从预设开的，或者「运行其他命令」当场敲的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Start {
    /// 终端叫什么（程序自己没设标题时）
    pub name: String,
    /// 在 shell 里输入的命令
    pub command: String,
    pub icon: String,
    pub hue: Option<usize>,
}

impl Start {
    pub fn of(preset: &Preset) -> Start {
        Start {
            name: preset.name.clone(),
            command: preset.command.clone(),
            icon: preset.icon.clone(),
            hue: preset.hue,
        }
    }

    /// 继续上次的会话；这个预设没有这条命令是 None。
    pub fn resume_of(preset: &Preset) -> Option<Start> {
        (!preset.resume.trim().is_empty()).then(|| Start {
            command: preset.resume.clone(),
            ..Start::of(preset)
        })
    }

    /// 当场敲的一条命令：终端叫程序的名字。
    pub fn command(command: &str) -> Start {
        Start {
            name: program_of(command),
            command: command.to_string(),
            icon: "play".to_string(),
            hue: None,
        }
    }
}

/// 命令运行的程序：第一个词（带空格的路径加了引号的，是引号里的）去掉目录、`.exe`，小写。
/// `npm run dev` 是 `npm`。
fn program_of(command: &str) -> String {
    let command = command.trim_start();
    let word = match command.chars().next() {
        Some(quote @ ('"' | '\'')) => command[1..].split(quote).next().unwrap_or_default(),
        _ => command.split_whitespace().next().unwrap_or_default(),
    };
    let name = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_lowercase();
    match name.strip_suffix(".exe") {
        Some(stem) => stem.to_string(),
        None => name,
    }
}

// ---------- 全局的一份 ----------

/// 现在的预设和「默认运行」（设置里那一份的副本）。
struct Current {
    presets: Vec<Preset>,
    default: String,
}

impl Global for Current {}

/// 设置变了（或者程序刚启动）：换成这一份。没变就不动。
pub fn sync(settings: &crate::settings::Settings, cx: &mut App) {
    let presets = settings.terminal_presets.clone().unwrap_or_else(builtins);
    let default = settings.terminal_default.clone();
    if cx
        .try_global::<Current>()
        .is_some_and(|current| current.presets == presets && current.default == default)
    {
        return;
    }
    cx.set_global(Current { presets, default });
}

/// 全部预设，按设置里排的先后。
pub fn all(cx: &App) -> Vec<Preset> {
    cx.try_global::<Current>()
        .map_or_else(builtins, |current| current.presets.clone())
}

/// 「默认运行」的那个预设；是只开 shell（或者指着已经删掉的预设）是 None。
pub fn default_preset(cx: &App) -> Option<Preset> {
    let current = cx.try_global::<Current>()?;
    current
        .presets
        .iter()
        .find(|preset| preset.id == current.default)
        .cloned()
}

/// 「新终端」按钮、Ctrl+Shift+` 开的终端要输入什么。
pub fn default_start(cx: &App) -> Option<Start> {
    default_preset(cx).map(|preset| Start::of(&preset))
}

/// 存回设置用的：主窗口的 [`AppState`]。
struct Store(WeakEntity<AppState>);

impl Global for Store {}

/// 程序启动时调用一次。
pub fn init(app: &Entity<AppState>, cx: &mut App) {
    cx.set_global(Store(app.downgrade()));
}

/// 改预设、「默认运行」，存进设置。改完和内置的一样就不存列表（以后内置的有变化也跟着变）。
fn edit(cx: &mut App, f: impl FnOnce(&mut Vec<Preset>, &mut String) + 'static) {
    let Some(app) = cx.try_global::<Store>().and_then(|store| store.0.upgrade()) else {
        return;
    };
    app.update(cx, |app, cx| {
        app.update_settings(cx, |settings| {
            let mut list = settings.terminal_presets.take().unwrap_or_else(builtins);
            f(&mut list, &mut settings.terminal_default);
            if !list
                .iter()
                .any(|preset| preset.id == settings.terminal_default)
            {
                settings.terminal_default.clear();
            }
            settings.terminal_presets = (list != builtins()).then_some(list);
        })
    });
}

/// 加一个自己的预设，返回它的 id。
fn add(mut preset: Preset, cx: &mut App) {
    edit(cx, move |list, _| {
        preset.id = free_id(list);
        list.push(preset);
    });
}

/// 还没用过的 `custom-N`。
fn free_id(list: &[Preset]) -> String {
    (1..)
        .map(|n| format!("custom-{n}"))
        .find(|id| !list.iter().any(|preset| preset.id == *id))
        .unwrap_or_default()
}

// ---------- 哪个终端是从哪个预设开的 ----------

/// 仓库的第几个终端是从哪个预设开的（只在内存里，见模块说明）。
#[derive(Default)]
struct Started(HashMap<TermSlot, Start>);

impl Global for Started {}

/// 新开了一个终端：记下它要输入什么（只开 shell 的是 None，把这个编号以前记的去掉）。
pub fn remember(slot: TermSlot, start: Option<Start>, cx: &mut App) {
    let started = cx.default_global::<Started>();
    match start {
        Some(start) => {
            started.0.insert(slot, start);
        }
        None => {
            started.0.remove(&slot);
        }
    }
}

/// 这个终端是从哪个预设开的。
pub fn started(slot: &TermSlot, cx: &App) -> Option<Start> {
    cx.try_global::<Started>()?.0.get(slot).cloned()
}

/// 终端叫什么：从预设开的，标题还是 shell 自己的（见 [`shell_title`]）或者没有标题时叫预设的名字；
/// 程序自己设了标题（Claude Code 会写在做的事）就用标题。都没有是 None。
pub fn display_name(title: Option<&str>, start: Option<&Start>) -> Option<SharedString> {
    match (title, start) {
        (Some(title), Some(start)) if shell_title(title) => Some(start.name.clone().into()),
        (Some(title), _) => Some(crate::terminal::short_title(title)),
        (None, Some(start)) => Some(start.name.clone().into()),
        (None, None) => None,
    }
}

/// 这是 shell 自己设的标题：Windows 上刚启动时是它的程序路径（`C:\...\pwsh.exe`）；Linux 上常见的
/// 是「用户@主机: 目录」，或者就是 shell 的名字。npm 装的 Codex 这类程序不设标题，标题一直是 shell 的。
fn shell_title(title: &str) -> bool {
    const SHELLS: [&str; 9] = [
        "pwsh",
        "powershell",
        "cmd",
        "bash",
        "zsh",
        "fish",
        "sh",
        "nu",
        "dash",
    ];
    let title = title.trim();
    let program_path = title.to_lowercase().ends_with(".exe") && title.contains(['\\', '/']);
    let user_at_host = title
        .split_once(':')
        .is_some_and(|(head, _)| head.contains('@') && !head.contains(char::is_whitespace));
    program_path
        || user_at_host
        || SHELLS.contains(&program_of(title).as_str()) && !title.contains(' ')
}

// ---------- 图标 ----------

/// 各家的图标：名字、文件、菜单和设置里的颜色（None 是文字的颜色：这家的标志是黑白的）。
/// 出处见 `assets/logos/README.md`。
const LOGOS: [(&str, &[u8], Option<u32>); 4] = [
    (
        "claude-code",
        include_bytes!("../assets/logos/claude-code.svg"),
        Some(0xD97757),
    ),
    ("openai", include_bytes!("../assets/logos/openai.svg"), None),
    (
        "opencode",
        include_bytes!("../assets/logos/opencode.svg"),
        None,
    ),
    (
        "antigravity",
        include_bytes!("../assets/logos/antigravity.svg"),
        Some(0x3186FF),
    ),
];

/// 资源里的路径前缀（[`Assets`]）。
const LOGO_DIR: &str = "kuai/logos/";

/// 自己加的预设可以挑的线条图标。
pub const GLYPHS: [&str; 12] = [
    "play", "terminal", "sparkles", "code", "rocket", "globe", "wrench", "package", "server",
    "database", "bot", "zap",
];

fn glyph(name: &str) -> Option<IconName> {
    Some(match name {
        "play" => IconName::Play,
        "terminal" => IconName::SquareTerminal,
        "sparkles" => IconName::Sparkles,
        "code" => IconName::CodeXml,
        "rocket" => IconName::Rocket,
        "globe" => IconName::Globe,
        "wrench" => IconName::Wrench,
        "package" => IconName::Package,
        "server" => IconName::Server,
        "database" => IconName::Database,
        "bot" => IconName::Bot,
        "zap" => IconName::Zap,
        _ => return None,
    })
}

fn logo(name: &str) -> Option<&'static (&'static str, &'static [u8], Option<u32>)> {
    LOGOS.iter().find(|(logo, ..)| *logo == name)
}

/// 预设的图标（没设颜色、大小）。认不出的名字画成三角。
pub fn icon(name: &str) -> Icon {
    match logo(name) {
        Some((logo, ..)) => Icon::empty().path(format!("{LOGO_DIR}{logo}.svg")),
        None => Icon::new(glyph(name).unwrap_or(IconName::Play)),
    }
}

/// 菜单、设置里预设图标的颜色：挑了颜色用挑的，没挑的各家用自己的颜色，线条图标用次要文字的灰。
pub fn color(icon: &str, hue: Option<usize>, cx: &App) -> Hsla {
    if let Some(hue) = hue {
        return ui::hue(hue, cx);
    }
    match logo(icon) {
        Some((_, _, Some(hex))) => rgb(*hex).into(),
        Some((_, _, None)) => cx.theme().foreground,
        None => cx.theme().muted_foreground,
    }
}

/// 内置的那几个程序的进程名和图标：内置的预设删了、手敲的 `claude` 也认得出。
const PROGRAMS: [(&str, &str); 4] = [
    ("claude", "claude-code"),
    ("codex", "openai"),
    ("opencode", "opencode"),
    ("agy", "antigravity"),
];

/// 终端里在跑的程序画哪个图标（会话页、仓库终端页侧栏的行）：先按预设的命令认（第一个词是这个
/// 程序），再按内置的那几个的进程名认。认不出是 None。
pub fn program_icon(programs: &[String], cx: &App) -> Option<String> {
    let presets = all(cx);
    programs.iter().find_map(|name| {
        let name = program_of(name);
        if name.is_empty() {
            return None;
        }
        presets
            .iter()
            .find(|preset| program_of(&preset.command) == name)
            .map(|preset| preset.icon.clone())
            .or_else(|| {
                PROGRAMS
                    .iter()
                    .find(|(program, _)| *program == name)
                    .map(|(_, icon)| icon.to_string())
            })
    })
}

/// 界面库的整套图标，再加上各家的图标（[`LOGO_DIR`] 下面）。
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let wanted = path
            .strip_prefix(LOGO_DIR)
            .and_then(|file| file.strip_suffix(".svg"));
        if let Some((_, bytes, _)) = wanted.and_then(logo) {
            return Ok(Some(Cow::Borrowed(*bytes)));
        }
        gpui_kit::assets::AllAssets.load(path)
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut found = gpui_kit::assets::AllAssets.list(path)?;
        found.extend(
            LOGOS
                .iter()
                .map(|(name, ..)| format!("{LOGO_DIR}{name}.svg"))
                .filter(|file| file.starts_with(path))
                .map(SharedString::from),
        );
        Ok(found)
    }
}

// ---------- 菜单 ----------

/// 对话框里「确定」要做的事，按钮和回车共用。
type Submit = Rc<dyn Fn(&mut Window, &mut App)>;

/// 选好了：开一个终端，替用户输入这个（None 是只开 shell）。
pub type OnStart = Rc<dyn Fn(Option<Start>, &mut Window, &mut App)>;

/// 新建终端的菜单（分体按钮的 ⌄、右键菜单里「新建终端」的子菜单）：只开 shell、各个预设、继续上次
/// 的会话、运行其他命令、管理预设。`place` 是仓库名，写在「运行其他命令」对话框的标题里。
pub fn menu(
    mut menu: PopupMenu,
    place: SharedString,
    on_start: OnStart,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let presets = all(cx);
    let default = default_preset(cx).map(|preset| preset.id);
    menu = menu.item(menu_row(None, default.is_none(), &on_start, cx));
    for preset in presets.iter().filter(|preset| preset.shown) {
        let is_default = default.as_deref() == Some(preset.id.as_str());
        menu = menu.item(menu_row(Some(Start::of(preset)), is_default, &on_start, cx));
    }
    let resumable: Vec<Start> = presets
        .iter()
        .filter(|preset| preset.shown)
        .filter_map(Start::resume_of)
        .collect();
    if !resumable.is_empty() {
        let on = on_start.clone();
        menu = menu.separator().submenu_with_icon(
            Some(Icon::new(IconName::Undo2)),
            tr("继续上次的会话"),
            window,
            cx,
            move |mut sub, _, cx| {
                for start in &resumable {
                    sub = sub.item(menu_row(Some(start.clone()), false, &on, cx));
                }
                sub
            },
        );
    }
    let run = on_start.clone();
    menu.separator()
        .item(
            PopupMenuItem::new(tr("运行其他命令…"))
                .on_click(move |_, window, cx| run_dialog(place.clone(), run.clone(), window, cx)),
        )
        .item(
            PopupMenuItem::new(tr("管理预设…")).on_click(|_, window, cx| open_settings(window, cx)),
        )
}

/// 菜单里的一行：图标、名字、命令（次要文字），「默认运行」的那个行尾写快捷键。
fn menu_row(start: Option<Start>, default: bool, on_start: &OnStart, cx: &App) -> PopupMenuItem {
    let theme = cx.theme();
    let (name, command, icon): (SharedString, SharedString, Icon) = match &start {
        Some(start) => (
            start.name.clone().into(),
            start.command.clone().into(),
            icon(&start.icon).text_color(color(&start.icon, start.hue, cx)),
        ),
        None => (
            tr("终端").into(),
            SharedString::default(),
            Icon::new(IconName::SquareTerminal).text_color(theme.muted_foreground),
        ),
    };
    let on = on_start.clone();
    PopupMenuItem::element(move |_, cx| {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let hint: SharedString = match command.is_empty() {
            true => tr("只开 shell").into(),
            false => command.clone(),
        };
        h_flex()
            .w_full()
            .min_w(px(260.))
            .gap_3()
            .child(div().flex_none().child(name.clone()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(muted)
                    .when(!command.is_empty(), |text| {
                        text.font_family(theme.mono_font_family.clone())
                    })
                    .child(hint),
            )
            .when(default, |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(muted)
                        .child("Ctrl+Shift+`"),
                )
            })
    })
    .icon(icon)
    .on_click(move |_, window, cx| on(start.clone(), window, cx))
}

/// 下拉菜单的内容：在 `place` 这个仓库里新建终端，选了交给 `on_start`。
pub fn menu_builder(
    place: SharedString,
    on_start: OnStart,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |popup, window, cx| menu(popup, place.clone(), on_start.clone(), window, cx)
}

/// 一个终端都没有时的按钮（仓库终端页、会话页）：「打开终端」（默认运行的是预设时是「打开」它）
/// 加 ⌄，下面一行「或者直接运行」列出其余的预设。
pub fn first_buttons(place: SharedString, on_start: OnStart, cx: &App) -> impl IntoElement {
    let muted = cx.theme().muted_foreground;
    let default = default_preset(cx);
    let (main_icon, label): (Icon, SharedString) = match &default {
        Some(preset) => (
            icon(&preset.icon),
            tf!("打开 {name}", name = preset.name.clone()).into(),
        ),
        None => (ui::icon(IconName::Plus), tr("打开终端").into()),
    };
    let default_id = default.as_ref().map(|preset| preset.id.clone());
    // 「或者直接运行」：默认运行的那个已经在按钮上了，不再列；默认的是预设时列上只开 shell
    let mut others: Vec<(Option<Start>, Icon, SharedString, SharedString)> = Vec::new();
    if default.is_some() {
        others.push((
            None,
            ui::icon(IconName::SquareTerminal).text_color(muted),
            tr("只开 shell").into(),
            tr("新建终端，不运行程序").into(),
        ));
    }
    for preset in all(cx)
        .iter()
        .filter(|preset| preset.shown && Some(&preset.id) != default_id.as_ref())
    {
        others.push((
            Some(Start::of(preset)),
            icon(&preset.icon)
                .size_3p5()
                .text_color(color(&preset.icon, preset.hue, cx)),
            preset.name.clone().into(),
            preset.command.clone().into(),
        ));
    }
    let links: Vec<Button> = others
        .into_iter()
        .enumerate()
        .map(|(ix, (start, glyph, name, tip))| {
            let on = on_start.clone();
            Button::new(("first-run", ix))
                .ghost()
                .xsmall()
                .icon(glyph)
                .label(name)
                .tooltip(tip)
                .on_click(move |_, window, cx| on(start.clone(), window, cx))
        })
        .collect();
    let (on_main, start) = (on_start.clone(), default.as_ref().map(Start::of));
    v_flex()
        .items_center()
        .gap_3()
        .child(
            h_flex()
                .gap(px(1.))
                .child(
                    Button::new("first-open")
                        .primary()
                        .small()
                        .icon(main_icon)
                        .label(label)
                        .on_click(move |_, window, cx| on_main(start.clone(), window, cx)),
                )
                .child(
                    Button::new("first-menu")
                        .primary()
                        .small()
                        .icon(ui::icon_small(IconName::ChevronDown))
                        .tooltip(tr("选择要运行的程序"))
                        .dropdown_menu(menu_builder(place, on_start)),
                ),
        )
        .when(!links.is_empty(), |col| {
            col.child(
                h_flex()
                    .flex_wrap()
                    .justify_center()
                    .gap_1()
                    .text_xs()
                    .text_color(muted)
                    .child(div().child(tr("或者直接运行：")))
                    .children(links),
            )
        })
}

/// 「管理预设…」：到设置的终端页。
fn open_settings(_: &mut Window, cx: &mut App) {
    crate::with_main_workspace(cx, |workspace, window, cx| {
        workspace.open_terminal_settings(window, cx)
    });
}

/// 「运行其他命令…」：问一条命令，开一个终端输入它；勾了「同时存为预设」的另外存一个预设。
pub fn run_dialog(place: SharedString, on_start: OnStart, window: &mut Window, cx: &mut App) {
    ui::cancel_pending_tip(window, cx);
    let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr("例如 npm run dev")));
    let save = Rc::new(Cell::new(false));
    let submit: Submit = {
        let (input, save) = (input.clone(), save.clone());
        Rc::new(move |window, cx| {
            let command = input.read(cx).value().trim().to_string();
            if command.is_empty() {
                return;
            }
            window.close_dialog(cx);
            let start = Start::command(&command);
            if save.get() {
                add(
                    Preset {
                        id: String::new(),
                        name: start.name.clone(),
                        command: command.clone(),
                        resume: String::new(),
                        icon: start.icon.clone(),
                        hue: None,
                        shown: true,
                    },
                    cx,
                );
            }
            on_start(Some(start), window, cx);
        })
    };
    let first_build = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_build.replace(false) {
            input.focus_handle(cx).focus(window, cx);
        }
        let muted = cx.theme().muted_foreground;
        let (ok, enter, toggle) = (submit.clone(), submit.clone(), save.clone());
        dialog
            .title(tf!("在 {repo} 中运行命令", repo = place.clone()))
            .w(px(520.))
            .on_ok(move |_, window, cx| {
                enter(window, cx);
                false
            })
            .child(
                v_flex()
                    .gap_3()
                    .child(Input::new(&input))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(tr("新开一个终端，在仓库目录的 shell 里输入这条命令。")),
                    )
                    .child(
                        Checkbox::new("run-save")
                            .label(tr("同时存为预设"))
                            .checked(save.get())
                            .on_click(move |checked, window, _| {
                                toggle.set(*checked);
                                window.refresh();
                            }),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("cancel")
                            .label(tr("取消"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("ok")
                            .primary()
                            .label(tr("运行"))
                            .on_click(move |_, window, cx| ok(window, cx)),
                    ),
            )
    });
}

// ---------- 设置 → 终端 → 新建终端 ----------

/// 设置页终端那一页的「新建终端」一组：默认运行、预设表、添加和恢复。
pub fn settings_group(app: &Entity<AppState>, cx: &App) -> SettingGroup {
    let presets = all(cx);
    let mut options: Vec<(SharedString, SharedString)> =
        vec![("shell".into(), tr("终端（只开 shell）").into())];
    options.extend(
        presets
            .iter()
            .map(|preset| (preset.id.clone().into(), preset.name.clone().into())),
    );
    let get = app.clone();
    let default = SettingItem::new(
        tr("默认运行"),
        SettingField::dropdown(
            options,
            move |cx| {
                let settings = &get.read(cx).settings;
                let known = all(cx)
                    .iter()
                    .any(|preset| preset.id == settings.terminal_default);
                match known {
                    true => settings.terminal_default.clone().into(),
                    false => "shell".into(),
                }
            },
            move |value: SharedString, cx| {
                let id = match value.as_ref() {
                    "shell" => String::new(),
                    id => id.to_string(),
                };
                edit(cx, move |_, default| *default = id);
            },
        ),
    )
    .description(tr(
        "侧栏的「新终端」、会话页仓库行上的 +、Ctrl+Shift+` 开的是它。",
    ));
    SettingGroup::new()
        .title(tr("新建终端"))
        .item(default)
        .item(SettingItem::render(|_, _, cx| table(cx)))
}

/// 拖着排序时跟着鼠标的那一块：预设在第几个、叫什么。
#[derive(Clone)]
struct DragPreset {
    index: usize,
    name: SharedString,
}

impl Render for DragPreset {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2()
            .py_1()
            .rounded(px(ui::RADIUS))
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_sm()
            .child(self.name.clone())
    }
}

/// 预设表：图标、名字、命令、继续上次的会话、在菜单里显不显示、编辑、删除；拖着行排序。下面是
/// 「添加预设」「恢复内置预设」。
fn table(cx: &mut App) -> AnyElement {
    let presets = all(cx);
    let builtin_ids: Vec<String> = builtins().into_iter().map(|preset| preset.id).collect();
    let theme = cx.theme();
    let (muted, border, hover, mono) = (
        theme.muted_foreground,
        theme.border,
        theme.list_hover,
        theme.mono_font_family.clone(),
    );
    // 每一列的宽度：名字固定，两列命令平分剩下的
    let name_col = |el: Div| el.w(px(170.)).flex_none();
    let cmd_col = |el: Div| el.flex_1().min_w_0();
    let header = h_flex()
        .px_2()
        .pb_1()
        .gap_3()
        .text_xs()
        .text_color(muted)
        .child(div().w(px(14.)).flex_none())
        .child(name_col(div()).child(tr("预设")))
        .child(cmd_col(div()).child(tr("命令")))
        .child(cmd_col(div()).child(tr("继续上次的会话")))
        .child(div().w(px(36.)).flex_none().child(tr("显示")))
        .child(div().w(px(52.)).flex_none());
    let rows = presets.iter().enumerate().map(|(index, preset)| {
        let builtin = builtin_ids.contains(&preset.id);
        let drag = DragPreset {
            index,
            name: preset.name.clone().into(),
        };
        let (shown_id, edit_one, delete_id) =
            (preset.id.clone(), preset.clone(), preset.id.clone());
        h_flex()
            .id(("preset-row", index))
            .px_2()
            .py_1()
            .gap_3()
            .rounded(px(ui::RADIUS))
            .hover(|style| style.bg(hover))
            .cursor_grab()
            .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
            .drag_over::<DragPreset>(move |style, _, _, cx| {
                style.border_t_2().border_color(cx.theme().primary)
            })
            .on_drop(move |drag: &DragPreset, _, cx| {
                let from = drag.index;
                if from == index {
                    return;
                }
                edit(cx, move |list, _| {
                    if from < list.len() {
                        let preset = list.remove(from);
                        list.insert(index.min(list.len()), preset);
                    }
                });
            })
            .child(
                ui::icon(IconName::GripVertical)
                    .flex_none()
                    .text_color(muted.opacity(0.6)),
            )
            .child(
                name_col(h_flex().gap_2())
                    .child(icon(&preset.icon).flex_none().size_4().text_color(color(
                        &preset.icon,
                        preset.hue,
                        cx,
                    )))
                    .child(ui::clipped(("preset-name", index), preset.name.clone()).text_sm())
                    .when(builtin, |name| {
                        name.child(
                            div()
                                .flex_none()
                                .px_1()
                                .rounded(px(3.))
                                .border_1()
                                .border_color(border)
                                .text_xs()
                                .text_color(muted)
                                .child(tr("内置")),
                        )
                    }),
            )
            .child(
                cmd_col(div()).child(
                    ui::clipped(("preset-command", index), preset.command.clone())
                        .text_xs()
                        .font_family(mono.clone()),
                ),
            )
            .child(
                cmd_col(div()).child(match preset.resume.is_empty() {
                    true => div()
                        .text_xs()
                        .text_color(muted)
                        .child("—")
                        .into_any_element(),
                    false => ui::clipped(("preset-resume", index), preset.resume.clone())
                        .text_xs()
                        .font_family(mono.clone())
                        .into_any_element(),
                }),
            )
            .child(
                div().w(px(36.)).flex_none().child(
                    Switch::new(("preset-shown", index))
                        .small()
                        .checked(preset.shown)
                        .tooltip(tr("在新建终端的菜单里显示"))
                        .on_click(move |on, _, cx| {
                            let (id, on) = (shown_id.clone(), *on);
                            edit(cx, move |list, _| {
                                if let Some(preset) = list.iter_mut().find(|p| p.id == id) {
                                    preset.shown = on;
                                }
                            });
                        }),
                ),
            )
            .child(
                h_flex()
                    .w(px(52.))
                    .flex_none()
                    .gap_1()
                    .child(
                        Button::new(("preset-edit", index))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Pencil)
                            .tooltip(tr("编辑"))
                            .on_click(move |_, window, cx| {
                                edit_dialog(Some(edit_one.clone()), window, cx)
                            }),
                    )
                    .child(
                        Button::new(("preset-delete", index))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Trash)
                            .tooltip(tr("删除"))
                            .on_click(move |_, _, cx| {
                                let id = delete_id.clone();
                                edit(cx, move |list, _| list.retain(|p| p.id != id));
                            }),
                    ),
            )
    });
    let missing = builtins()
        .into_iter()
        .any(|builtin| !presets.iter().any(|preset| preset.id == builtin.id));
    let footer = h_flex()
        .pt_2()
        .gap_2()
        .flex_wrap()
        .child(
            Button::new("preset-add")
                .small()
                .icon(IconName::Plus)
                .label(tr("添加预设…"))
                .on_click(|_, window, cx| edit_dialog(None, window, cx)),
        )
        .child(
            Button::new("preset-restore")
                .small()
                .label(tr("恢复内置预设"))
                .disabled(!missing)
                .tooltip(tr("把删掉的内置预设加回来"))
                .on_click(|_, _, cx| edit(cx, restore_builtins)),
        );
    v_flex()
        .w_full()
        .child(header)
        .children(rows)
        .child(footer)
        .child(
            div()
                .pt_2()
                .text_xs()
                .text_color(muted)
                .child(tr(
                    "在仓库目录里打开 shell 后输入这条命令；程序退出后终端留在 shell。远程主机上的仓库在那台主机上运行，命令以那台主机上装的版本为准。",
                )),
        )
        .into_any_element()
}

/// 删掉的内置预设加回原来的位置（按内置的先后，插在前一个内置预设后面）。
fn restore_builtins(list: &mut Vec<Preset>, _: &mut String) {
    for (order, builtin) in builtins().into_iter().enumerate() {
        if list.iter().any(|preset| preset.id == builtin.id) {
            continue;
        }
        let earlier: Vec<String> = builtins()
            .into_iter()
            .take(order)
            .map(|preset| preset.id)
            .collect();
        let at = list
            .iter()
            .rposition(|preset| earlier.contains(&preset.id))
            .map_or(0, |ix| ix + 1);
        list.insert(at, builtin);
    }
}

/// 添加、编辑预设的对话框：名称、命令、继续上次的会话、图标和颜色。
fn edit_dialog(existing: Option<Preset>, window: &mut Window, cx: &mut App) {
    ui::cancel_pending_tip(window, cx);
    let field = |value: &str, placeholder: &'static str, window: &mut Window, cx: &mut App| {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        if !value.is_empty() {
            let value = value.to_string();
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        input
    };
    let base = existing.clone().unwrap_or(Preset {
        id: String::new(),
        name: String::new(),
        command: String::new(),
        resume: String::new(),
        icon: "play".to_string(),
        hue: None,
        shown: true,
    });
    let name = field(&base.name, tr("例如 开发服务器"), window, cx);
    let command = field(&base.command, tr("例如 npm run dev"), window, cx);
    let resume = field(&base.resume, tr("例如 claude -c"), window, cx);
    let icon_choice = Rc::new(RefCell::new(base.icon.clone()));
    let hue_choice = Rc::new(Cell::new(base.hue));
    let problem: Rc<RefCell<Option<SharedString>>> = Rc::default();
    let submit: Submit = {
        let (name, command, resume) = (name.clone(), command.clone(), resume.clone());
        let (icon_choice, hue_choice, problem) =
            (icon_choice.clone(), hue_choice.clone(), problem.clone());
        Rc::new(move |window, cx| {
            let text =
                |input: &Entity<InputState>, cx: &App| input.read(cx).value().trim().to_string();
            let preset = Preset {
                id: base.id.clone(),
                name: text(&name, cx),
                command: text(&command, cx),
                resume: text(&resume, cx),
                icon: icon_choice.borrow().clone(),
                hue: hue_choice.get(),
                shown: base.shown,
            };
            let found = if preset.name.is_empty() {
                Some(tr("请填写名称"))
            } else if preset.command.is_empty() {
                Some(tr("请填写命令"))
            } else {
                None
            };
            if let Some(found) = found {
                *problem.borrow_mut() = Some(found.into());
                window.refresh();
                return;
            }
            window.close_dialog(cx);
            if preset.id.is_empty() {
                add(preset, cx);
            } else {
                edit(cx, move |list, _| {
                    if let Some(slot) = list.iter_mut().find(|p| p.id == preset.id) {
                        *slot = preset;
                    }
                });
            }
        })
    };
    let title: SharedString = match existing {
        Some(_) => tr("编辑预设").into(),
        None => tr("添加预设").into(),
    };
    let first_build = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_build.replace(false) {
            name.focus_handle(cx).focus(window, cx);
        }
        let theme = cx.theme();
        let (muted, danger, primary, border) = (
            theme.muted_foreground,
            theme.danger,
            theme.primary,
            theme.border,
        );
        let label = |text: &'static str| div().text_sm().child(text);
        let chosen_icon = icon_choice.borrow().clone();
        let chosen_hue = hue_choice.get();
        let icons = LOGOS
            .iter()
            .map(|(logo, ..)| *logo)
            .chain(GLYPHS)
            .enumerate()
            .map(|(ix, choice)| {
                let on = choice == chosen_icon;
                let pick = icon_choice.clone();
                div()
                    .id(("preset-icon", ix))
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(ui::RADIUS))
                    .border_1()
                    .border_color(if on { primary } else { border })
                    .cursor_pointer()
                    .child(
                        icon(choice)
                            .size_4()
                            .text_color(color(choice, chosen_hue, cx)),
                    )
                    .on_click(move |_, window, _| {
                        *pick.borrow_mut() = choice.to_string();
                        window.refresh();
                    })
            });
        let colors = std::iter::once(None)
            .chain((0..ui::HUES).map(Some))
            .map(|hue| {
                let on = hue == chosen_hue;
                let pick = hue_choice.clone();
                let swatch = match hue {
                    Some(hue) => ui::hue(hue, cx),
                    None => color(&chosen_icon, None, cx),
                };
                div()
                    .id(("preset-hue", hue.map_or(0, |hue| hue + 1)))
                    .size(px(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .border_2()
                    .border_color(if on { primary } else { transparent_black() })
                    .cursor_pointer()
                    .child(div().size(px(14.)).rounded_full().bg(swatch))
                    .when(hue.is_none(), |el| el.tooltip(|window, cx| ui::tip(tr("默认颜色"), window, cx)))
                    .on_click(move |_, window, _| {
                        pick.set(hue);
                        window.refresh();
                    })
            });
        let mut body = v_flex()
            .gap_3()
            .child(v_flex().gap_1().child(label(tr("名称"))).child(Input::new(&name)))
            .child(
                v_flex()
                    .gap_1()
                    .child(label(tr("命令")))
                    .child(Input::new(&command))
                    .child(div().text_xs().text_color(muted).child(tr(
                        "在仓库目录里的 shell 中输入这条命令、回车。可以带参数，例如 claude --model opus。",
                    ))),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(label(tr("继续上次的会话（可选）")))
                    .child(Input::new(&resume)),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(label(tr("图标和颜色")))
                    .child(h_flex().gap_1().flex_wrap().children(icons))
                    .child(h_flex().gap_1().children(colors)),
            );
        if let Some(found) = problem.borrow().clone() {
            body = body.child(div().text_sm().text_color(danger).child(found));
        }
        let (ok, enter) = (submit.clone(), submit.clone());
        dialog
            .title(title.clone())
            .w(px(520.))
            .on_ok(move |_, window, cx| {
                enter(window, cx);
                false
            })
            .child(body)
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("cancel")
                            .label(tr("取消"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("ok")
                            .primary()
                            .label(tr("保存"))
                            .on_click(move |_, window, cx| ok(window, cx)),
                    ),
            )
    });
}

#[cfg(test)]
mod tests {
    // 不用 `super::*`：那样会把界面库的 `test` 宏也带进来，盖掉标准的 `#[test]`
    use super::{
        Preset, Start, builtins, display_name, free_id, logo, program_of, restore_builtins,
    };

    #[test]
    fn 程序名取命令的第一个词() {
        assert_eq!(program_of("npm run dev"), "npm");
        assert_eq!(program_of("codex --no-alt-screen"), "codex");
        assert_eq!(program_of("C:\\Tools\\Claude.exe -c"), "claude");
        assert_eq!(program_of("\"/opt/x y/agy\""), "agy");
        assert_eq!(program_of("  "), "");
    }

    #[test]
    fn 从预设开的终端_标题是_shell_的就叫预设的名字() {
        let start = Start::command("codex --no-alt-screen");
        let name = |title: Option<&str>| display_name(title, Some(&start)).map(|n| n.to_string());
        assert_eq!(name(None).as_deref(), Some("codex"));
        assert_eq!(
            name(Some("C:\\Program Files\\PowerShell\\7\\pwsh.exe")).as_deref(),
            Some("codex")
        );
        assert_eq!(name(Some("dev@box: ~/src/kuai")).as_deref(), Some("codex"));
        assert_eq!(name(Some("bash")).as_deref(), Some("codex"));
        // 程序自己设的标题照用
        assert_eq!(name(Some("✳ 改设置页")).as_deref(), Some("✳ 改设置页"));
        assert_eq!(
            name(Some("bash 脚本说明")).as_deref(),
            Some("bash 脚本说明")
        );
        // 不是从预设开的：照原来的写法
        assert_eq!(
            display_name(Some("C:\\Windows\\pwsh.exe"), None).map(|n| n.to_string()),
            Some("pwsh".to_string())
        );
        assert_eq!(display_name(None, None), None);
    }

    #[test]
    fn 内置的命令不用备用屏() {
        let all = builtins();
        let command = |id: &str| {
            let preset = all.iter().find(|p| p.id == id).unwrap();
            (preset.command.clone(), preset.resume.clone())
        };
        let (codex, codex_resume) = command("codex");
        assert!(codex.contains("--no-alt-screen") && codex_resume.contains("--no-alt-screen"));
        assert_eq!(command("opencode").0, "opencode mini");
        // 内置的 id 不重复，图标都是各家的
        let mut ids: Vec<&str> = all.iter().map(|p| p.id.as_str()).collect();
        ids.dedup();
        assert_eq!(ids.len(), 4);
        assert!(all.iter().all(|p| logo(&p.icon).is_some()));
    }

    #[test]
    fn 恢复内置预设_加回原来的位置() {
        let mut list = builtins();
        list.retain(|p| p.id != "codex" && p.id != "claude");
        list.push(Preset {
            id: "custom-1".into(),
            ..Start::command("npm run dev").into_preset()
        });
        restore_builtins(&mut list, &mut String::new());
        let ids: Vec<&str> = list.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["claude", "codex", "opencode", "agy", "custom-1"]);
    }

    #[test]
    fn 存的预设_没写的项取默认() {
        let preset: Preset =
            serde_json::from_str(r#"{"id":"custom-1","name":"dev","command":"npm run dev"}"#)
                .unwrap();
        assert!(preset.shown && preset.resume.is_empty() && preset.hue.is_none());
        // 显示、没有颜色的不写出来
        let text = serde_json::to_string(&preset).unwrap();
        assert!(!text.contains("shown") && !text.contains("hue") && !text.contains("resume"));
    }

    #[test]
    fn 新的预设编号不重复() {
        let mut list = builtins();
        assert_eq!(free_id(&list), "custom-1");
        list.push(Preset {
            id: "custom-1".into(),
            ..builtins().remove(0)
        });
        assert_eq!(free_id(&list), "custom-2");
    }

    impl Start {
        fn into_preset(self) -> Preset {
            Preset {
                id: String::new(),
                name: self.name,
                command: self.command,
                resume: String::new(),
                icon: self.icon,
                hue: self.hue,
                shown: true,
            }
        }
    }
}
