//! 新建终端的预设：开一个终端，等 shell 的提示符出来替你输入一条命令、回车，比如 Claude Code、Codex
//! 这些命令行工具，或者自己加的 `npm run dev`。程序退出后终端留在 shell 里，可以接着敲命令。
//!
//! - 预设存在设置里（[`Settings::terminal_presets`]；没改过是 None，用 [`builtins`]），「新终端」按钮、
//!   Ctrl+Shift+` 开哪个看 [`Settings::terminal_default`]。界面各处读全局的一份（[`sync`]）。
//! - 哪个仓库的第几个终端是从哪个预设开的，记下来（[`remember`]），开好的存进设置：重启这个终端时
//!   再输入一遍，程序没设标题时终端叫预设的名字，重开 Kuai 以后接回来的也是（存下的要和终端列表
//!   对上才算，见 [`Started`]）。接上已有的会话（重开 Kuai、在别处开着的）不再输入。
//! - 内置的四个用各家的图标（`assets/logos/`，单色，画的时候填颜色）；自己加的从一组线条图标里挑。
//! - 内置的命令都不用备用屏（`CSI ?1049h`）：用了的话程序的画面不进往回翻的记录，往回翻、消息刻度、
//!   固定输入区都用不上。所以 Codex 带 `--no-alt-screen`。OpenCode 例外，用完整的界面：不占满屏幕的
//!   `opencode mini` 进去以后换不了模型（2026-11-25 改回来的，旧设置里存着的见 [`upgrade_builtins`]）。
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
use fr_channel::TermInfo;

/// 一个预设。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preset {
    /// 认它用的：内置的是 `claude` 这些，自己加的是 `custom-1` 这样
    pub id: String,
    pub name: String,
    /// 在 shell 里输入的命令
    pub command: String,
    /// 「继续上次的对话」输入的命令；空着是没有
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
    /// 还出现在哪些右键菜单里（提交、分支、文件）：命令里可以用 `$sha`、`$branch`、`$file`、`$repo`、
    /// `$remote`，点的时候换成点的那个对象（见 [`expand`]）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub places: Vec<Place>,
    /// 从右键菜单运行时不开终端，在后台运行、输出显示在底部的「输出」里
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
}

/// 一条命令还出现在哪种对象的右键菜单里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Place {
    #[serde(rename = "commit")]
    Commit,
    #[serde(rename = "branch")]
    Branch,
    #[serde(rename = "file")]
    File,
}

impl Place {
    pub const ALL: [Place; 3] = [Place::Commit, Place::Branch, Place::File];

    pub fn label(self) -> &'static str {
        match self {
            Place::Commit => tr("提交"),
            Place::Branch => tr("分支"),
            Place::File => tr("文件"),
        }
    }
}

/// 命令里的变量换成什么。没有的（比如在提交上点的没有 `$file`）换成空的。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vars {
    pub sha: String,
    pub branch: String,
    pub file: String,
    pub repo: String,
    pub remote: String,
}

/// 把命令里的 `$sha`、`$branch`、`$file`、`$repo`、`$remote` 换成值。值里有空格、引号这些字符时
/// 用双引号括起来（里面的 `"`、`\`、`$`、反引号加反斜杠）；已经写在引号里的不再加引号。认不得的
/// `$名字`（shell 自己的变量）原样留着。
pub fn expand(command: &str, vars: &Vars) -> String {
    let safe = |value: &str| {
        !value.is_empty()
            && value
                .chars()
                .all(|c| c.is_alphanumeric() || "_./@:+-=,%~".contains(c))
    };
    let escaped = |value: &str| {
        let mut out = String::with_capacity(value.len() + 2);
        for c in value.chars() {
            if matches!(c, '"' | '\\' | '$' | '`') {
                out.push('\\');
            }
            out.push(c);
        }
        out
    };
    let mut out = String::with_capacity(command.len() + 32);
    // 现在在哪种引号里：值要不要自己加引号看它
    let mut quote: Option<char> = None;
    let mut rest = command;
    while let Some(c) = rest.chars().next() {
        if c != '$' {
            match (quote, c) {
                (None, '"' | '\'') => quote = Some(c),
                (Some(open), c) if c == open => quote = None,
                _ => {}
            }
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let name: String = rest[1..]
            .chars()
            .take_while(|c| c.is_ascii_lowercase())
            .collect();
        let value = match name.as_str() {
            "sha" => Some(&vars.sha),
            "branch" => Some(&vars.branch),
            "file" => Some(&vars.file),
            "repo" => Some(&vars.repo),
            "remote" => Some(&vars.remote),
            _ => None,
        };
        match value {
            Some(value) => {
                match quote {
                    // 单引号里什么都不转义
                    Some('\'') => out.push_str(value),
                    Some(_) => out.push_str(&escaped(value)),
                    None if safe(value) => out.push_str(value),
                    None => {
                        out.push('"');
                        out.push_str(&escaped(value));
                        out.push('"');
                    }
                }
                rest = &rest[1 + name.len()..];
            }
            None => {
                out.push('$');
                rest = &rest[1..];
            }
        }
    }
    out
}

/// 出现在 `place` 这种对象的右键菜单里的命令。
pub fn for_place(place: Place, cx: &App) -> Vec<Preset> {
    all(cx)
        .into_iter()
        .filter(|preset| preset.places.contains(&place))
        .collect()
}

/// 现成的例子（设置里「添加示例」）：把右键点的对象交给终端里的 AI。
fn examples() -> Vec<Preset> {
    let example = |name: &str, command: &str, place: Place| Preset {
        id: String::new(),
        name: name.to_string(),
        command: command.to_string(),
        resume: String::new(),
        icon: "claude-code".to_string(),
        hue: None,
        shown: false,
        places: vec![place],
        background: false,
    };
    vec![
        example(
            tr("让 Claude Code 评审此提交"),
            "claude \"评审提交 $sha，指出有风险的改动\"",
            Place::Commit,
        ),
        example(
            tr("让 Claude Code 讲解此文件"),
            "claude \"讲解 $file 的结构和关键逻辑\"",
            Place::File,
        ),
    ]
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
        places: Vec::new(),
        background: false,
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
        // 完整的界面（用备用屏）：`opencode mini` 不占满屏幕，可是进去以后换不了模型
        preset(
            "opencode",
            "OpenCode",
            "opencode",
            "opencode -c",
            "opencode",
        ),
        preset("agy", "Antigravity CLI", "agy", "agy -c", "antigravity"),
    ]
}

/// 内置预设换过的命令：(id, 以前的命令, 以前的继续命令, 现在的命令, 现在的继续命令)。
const RETIRED: [(&str, &str, &str, &str, &str); 1] = [(
    "opencode",
    "opencode mini",
    "opencode mini -c",
    "opencode",
    "opencode -c",
)];

/// 改过预设列表的设置里还存着内置预设以前的命令（[`RETIRED`]）：还是原样的换成现在的，自己改过的
/// 不动。读设置时调用。
pub fn upgrade_builtins(list: &mut [Preset]) {
    for preset in list {
        for (id, command, resume, new_command, new_resume) in RETIRED {
            if preset.id != id {
                continue;
            }
            if preset.command == command {
                preset.command = new_command.to_string();
            }
            if preset.resume == resume {
                preset.resume = new_resume.to_string();
            }
        }
    }
}

/// 新开的终端要替用户输入的命令：从预设开的，或者「运行命令」当场敲的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    /// 终端叫什么（程序自己没设标题时）
    pub name: String,
    /// 在 shell 里输入的命令
    pub command: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

    /// 继续上次的对话；这个预设没有这条命令是 None。
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
pub fn program_of(command: &str) -> String {
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

/// 程序启动时调用一次：上次存下的从预设开的终端先读进来（对上终端列表以前不算，见 [`Started`]）。
pub fn init(app: &Entity<AppState>, cx: &mut App) {
    let started = Started::load(&app.read(cx).settings.started_terms);
    cx.set_global(started);
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

/// 拖着的第 `from` 个预设放到第 `index` 个上（排到它前面；`index` 是列表长度时排到最后）。
fn move_preset(from: usize, index: usize, cx: &mut App) {
    if from == index {
        return;
    }
    edit(cx, move |list, _| {
        if from < list.len() {
            let preset = list.remove(from);
            let to = drop_index(from, index).min(list.len());
            list.insert(to, preset);
        }
    });
}

/// 第 `from` 个放到第 `index` 个前面以后在第几个：往下拖时先拿了出来，后面的都往前挪了一位。原来
/// 照 `index` 插，往下拖的落到了指示线下面那一行的后面。
fn drop_index(from: usize, index: usize) -> usize {
    match from < index {
        true => index - 1,
        false => index,
    }
}

/// 还没用过的 `custom-N`。
fn free_id(list: &[Preset]) -> String {
    (1..)
        .map(|n| format!("custom-{n}"))
        .find(|id| !list.iter().any(|preset| preset.id == *id))
        .unwrap_or_default()
}

// ---------- 哪个终端是从哪个预设开的 ----------

/// 仓库的第几个终端是从哪个预设开的。按序号记，再记下开出来的是 kuaid 里的哪个会话：序号会被重用
/// （这个结束了，别的 Kuai 在同一个仓库又开了一个，拿到同一个序号），别人的那个不按这个预设显示，
/// 重启它时也不输入这条命令。
///
/// 开好了的存进设置（[`StartedRecord`]）：重开 Kuai 以后接回来的终端还叫预设的名字、画它的图标，
/// 重启时还输入它的命令。存下的先不算，等那台电脑的终端列表到了，会话编号和开出来的时刻都对上了
/// 才算（[`reconcile`]、[`confirm`]）：kuaid 重新启动以后编号从 t1 重新数，光看编号会认错。
#[derive(Default)]
struct Started(HashMap<TermSlot, Launched>);

struct Launched {
    start: Start,
    /// 开出来的会话（kuaid 给的编号）；刚开、还没回话，或者正在重启，是 None
    term: Option<String>,
    /// 会话开出来的时刻（这台电脑的 Unix 秒）；还没开好是 0
    born: u64,
    /// 上次运行时存下的，还没和终端列表对上：对上以前不算
    pending: bool,
}

impl Global for Started {}

/// 存在设置里的一条：哪个仓库的第几个终端，是 kuaid 里的哪个会话、什么时候开的，从哪个预设开的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartedRecord {
    pub slot: TermSlot,
    pub term: String,
    pub born: u64,
    pub start: Start,
}

/// 存下的开出来的时刻和终端列表里倒推的差这么多秒以内算同一个会话：列表里的「开了多久」按整秒
/// 给，两台电脑的钟也可能差一点。编号重用的是 kuaid 重新启动以后新开的，差得远
const BORN_SLACK: u64 = 30;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Started {
    fn load(records: &[StartedRecord]) -> Started {
        Started(
            records
                .iter()
                .map(|record| {
                    let launched = Launched {
                        start: record.start.clone(),
                        term: Some(record.term.clone()),
                        born: record.born,
                        pending: true,
                    };
                    (record.slot.clone(), launched)
                })
                .collect(),
        )
    }

    /// 要存的：开好了的（还没对上的也留着，那台电脑可能还没连上）。按仓库、序号排，设置文件不乱跳。
    fn records(&self) -> Vec<StartedRecord> {
        let mut records: Vec<StartedRecord> = self
            .0
            .iter()
            .filter_map(|(slot, found)| {
                Some(StartedRecord {
                    slot: slot.clone(),
                    term: found.term.clone()?,
                    born: found.born,
                    start: found.start.clone(),
                })
            })
            .collect();
        records.sort_by(|a, b| {
            (&a.slot.entry.server, &a.slot.entry.path, a.slot.number).cmp(&(
                &b.slot.entry.server,
                &b.slot.entry.path,
                b.slot.number,
            ))
        });
        records
    }

    fn remember(&mut self, slot: TermSlot, start: Option<Start>) {
        match start {
            Some(start) => {
                let launched = Launched {
                    start,
                    term: None,
                    born: 0,
                    pending: false,
                };
                self.0.insert(slot, launched);
            }
            None => {
                self.0.remove(&slot);
            }
        }
    }

    fn get(&self, slot: &TermSlot, term: Option<&str>) -> Option<Start> {
        let found = self.0.get(slot).filter(|found| !found.pending)?;
        match (&found.term, term) {
            (Some(bound), Some(term)) if bound != term => None,
            _ => Some(found.start.clone()),
        }
    }

    fn opened(&mut self, slot: &TermSlot, term: &str, created: bool) {
        let Some(found) = self.0.get_mut(slot) else {
            return;
        };
        match (&found.term, created) {
            (None, true) => {
                found.term = Some(term.to_string());
                found.born = now_secs();
            }
            (None, false) => {
                self.0.remove(slot);
            }
            // 断线后接回自己的那个；别的会话不动它
            (Some(_), _) => {}
        }
    }

    /// 开了 `age` 秒的会话和存下的这一条开出来的时刻差不多。
    fn same_birth(found: &Launched, age: u64, now: u64) -> bool {
        now.saturating_sub(age).abs_diff(found.born) <= BORN_SLACK
    }

    /// 存下的这一条和列表里的会话 `info` 对得上：同一个仓库的同一个序号、开出来的时刻差不多。
    fn matches(slot: &TermSlot, found: &Launched, info: &TermInfo, now: u64) -> bool {
        crate::terminal::session_number(&slot.entry.path, &info.key) == Some(slot.number)
            && Self::same_birth(found, info.age, now)
    }

    /// 一台电脑的完整终端列表到了：这台电脑上还没对上的，对上了算数，编号对上了别的对不上、列表里
    /// 没有了的去掉。列表是空的不动：kuaid 刚起来时可能先推一份空的。返回去掉了没有。
    fn reconcile(&mut self, machine: &str, list: &[TermInfo], now: u64) -> bool {
        let before = self.0.len();
        self.0.retain(|slot, found| {
            if !found.pending || crate::machines::machine_of(&slot.entry) != machine {
                return true;
            }
            let Some(term) = found.term.clone() else {
                return false;
            };
            match list.iter().find(|info| info.term == term) {
                Some(info) if Self::matches(slot, found, info, now) => {
                    found.pending = false;
                    true
                }
                Some(_) => false,
                None => list.is_empty(),
            }
        });
        self.0.len() != before
    }

    /// 仓库终端页问到了这个仓库第几个终端是会话 `term`、开了 `age` 秒：存下的那一条对得上就算数
    /// （对不上的留给完整的列表去掉）。
    fn confirm(&mut self, slot: &TermSlot, term: &str, age: u64, now: u64) {
        if let Some(found) = self.0.get_mut(slot)
            && found.pending
            && found.term.as_deref() == Some(term)
            && Self::same_birth(found, age, now)
        {
            found.pending = false;
        }
    }

    /// 记着的是 `term` 的（或者还没开好）。
    fn owns(&self, slot: &TermSlot, term: &str) -> Option<bool> {
        let found = self.0.get(slot)?;
        Some(found.term.as_deref().is_none_or(|bound| bound == term))
    }

    fn restarting(&mut self, slot: &TermSlot, term: &str) {
        match self.owns(slot, term) {
            Some(true) => {
                if let Some(found) = self.0.get_mut(slot) {
                    found.term = None;
                }
            }
            Some(false) => {
                self.0.remove(slot);
            }
            None => {}
        }
    }

    fn ended(&mut self, slot: &TermSlot, term: &str) {
        if self.owns(slot, term) == Some(true) {
            self.0.remove(slot);
        }
    }
}

/// 存下的从预设开的终端有变化：写回设置。推迟到这一轮处理完再写，调用的地方可能正在改别的实体。
fn save_started(cx: &mut App) {
    let Some(app) = cx.try_global::<Store>().and_then(|store| store.0.upgrade()) else {
        return;
    };
    let records = cx.default_global::<Started>().records();
    cx.defer(move |cx| {
        app.update(cx, |app, cx| {
            if app.settings.started_terms != records {
                app.update_settings(cx, |settings| settings.started_terms = records);
            }
        });
    });
}

/// 新开了一个终端：记下它要输入什么（只开 shell 的是 None，把这个编号以前记的去掉）。
pub fn remember(slot: TermSlot, start: Option<Start>, cx: &mut App) {
    cx.default_global::<Started>().remember(slot, start);
    save_started(cx);
}

/// 一台电脑（`machine`，见 [`crate::machines::machine_of`]）推来了完整的终端列表：上次运行时存下的
/// 这台电脑上的从预设开的终端，对上了算数，对不上、已经不在了的去掉。列表一到就调（「开了多久」
/// 是推来的那一刻的）。
pub fn reconcile(machine: &str, list: &[TermInfo], cx: &mut App) {
    if cx
        .default_global::<Started>()
        .reconcile(machine, list, now_secs())
    {
        save_started(cx);
    }
}

/// 仓库终端页问到 `slot` 是会话 `term`、开了 `age` 秒：上次运行时存下的这一条对得上就算数。
pub fn confirm(slot: &TermSlot, term: &str, age: u64, cx: &mut App) {
    cx.default_global::<Started>()
        .confirm(slot, term, age, now_secs());
}

/// 这个终端是从哪个预设开的。`term`：问的是 kuaid 里的哪个会话，不知道是 None；记着的是别的会话的
/// 就不算。
pub fn started(slot: &TermSlot, term: Option<&str>, cx: &App) -> Option<Start> {
    cx.try_global::<Started>()?.get(slot, term)
}

/// 从预设开的终端开好了（kuaid 回了话，会话是 `term`）。`created`：是新开的，记下是哪个会话；接上的
/// 是已有的（不是这次开的，比如别处刚用这个序号开了一个）就不算这个预设的。
pub fn opened(slot: &TermSlot, term: &str, created: bool, cx: &mut App) {
    cx.default_global::<Started>().opened(slot, term, created);
    save_started(cx);
}

/// 要重启 kuaid 里的 `term` 这个终端：记着的是它的，重启以后新开的接着算这个预设的（再输入一遍）；
/// 不是它的就去掉。
pub fn restarting(slot: &TermSlot, term: &str, cx: &mut App) {
    cx.default_global::<Started>().restarting(slot, term);
    save_started(cx);
}

/// kuaid 里的 `term` 这个终端结束了：记着的是它的就去掉。
pub fn ended(slot: &TermSlot, term: &str, cx: &mut App) {
    cx.default_global::<Started>().ended(slot, term);
    save_started(cx);
}

/// 终端叫什么：从预设开的，标题还是 shell 自己的（见 [`shell_title`]）或者没有标题时叫预设的名字；
/// 程序自己设了标题（Claude Code 会写在做的事）就用标题。都没有是 None。
pub fn display_name(title: Option<&str>, start: Option<&Start>) -> Option<SharedString> {
    match (title, start) {
        (Some(title), Some(start)) if shell_title(title) => Some(start.name.clone().into()),
        // shell 提示符设的「用户@主机: 目录」：同一个仓库的终端用户、主机都一样，只留目录。不然名字很长，
        // 同一个目录开两个时区分用的序号（见 `term_board.rs` 的 `refresh_meta`）常被截掉
        (Some(title), None) => match prompt_dir(title) {
            Some("") => None,
            Some(dir) => Some(dir.to_string().into()),
            None => Some(crate::terminal::short_title(title)),
        },
        (Some(title), _) => Some(crate::terminal::short_title(title)),
        (None, Some(start)) => Some(start.name.clone().into()),
        (None, None) => None,
    }
}

/// 「用户@主机: 目录」这种标题里的目录；不是这种写法的是 None。
fn prompt_dir(title: &str) -> Option<&str> {
    let (head, rest) = title.trim().split_once(':')?;
    (head.contains('@') && !head.contains(char::is_whitespace)).then(|| rest.trim())
}

/// 这是 shell 自己设的标题：Windows 上刚启动时是它的程序路径（`C:\...\pwsh.exe`）；Linux 上常见的
/// 是「用户@主机: 目录」，或者就是 shell 的名字。npm 装的 Codex 这类程序不设标题，标题一直是 shell 的。
pub(crate) fn shell_title(title: &str) -> bool {
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

/// 新建终端的菜单，各处都是这一个：分体按钮的 ⌄、只有图标的「新建终端」按钮（会话页仓库行的 +、
/// 工作区页和子仓库、工作树总览的行尾、文件页工具栏、图标栏的 +，点了就弹它）、右键菜单里「新建终端」
/// 的子菜单。列只开 shell、各个预设（只写名字）、继续上次的对话、运行命令、管理预设。`place` 是开在
/// 哪（仓库名或者仓库里的目录），写在「运行命令」对话框的标题里。
pub fn menu(
    mut menu: PopupMenu,
    place: SharedString,
    on_start: OnStart,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let presets = all(cx);
    let default = default_preset(cx).map(|preset| preset.id);
    // 行尾写快捷键：「默认运行」的那个写新建终端的键，别的写给它设的键（设置 → 快捷键）
    let key = |own: String, is_default: bool, cx: &App| {
        let default_key = is_default
            .then(|| crate::keymap::label("new.default", cx))
            .flatten();
        default_key.or_else(|| crate::keymap::label(&own, cx))
    };
    let shell_key = key("new.shell".to_string(), default.is_none(), cx);
    menu = menu.item(menu_row(
        ("menu-run", 0usize),
        None,
        shell_key,
        &on_start,
        cx,
    ));
    for (ix, preset) in presets.iter().filter(|preset| preset.shown).enumerate() {
        let is_default = default.as_deref() == Some(preset.id.as_str());
        let shortcut = key(crate::keymap::preset_id(&preset.id), is_default, cx);
        let row = menu_row(
            ("menu-run", ix + 1),
            Some(Start::of(preset)),
            shortcut,
            &on_start,
            cx,
        );
        menu = menu.item(row);
    }
    let resumable: Vec<Start> = presets
        .iter()
        .filter(|preset| preset.shown)
        .filter_map(Start::resume_of)
        .collect();
    if !resumable.is_empty() {
        let on = on_start.clone();
        // 叫「对话」：和标题栏的「会话」页、接回仍在运行的终端都不是一回事，这里是新开一个终端，
        // 让程序接着上次的对话（`claude --continue` 这类）
        menu = menu.separator().submenu_with_icon(
            Some(Icon::new(IconName::Undo2)),
            tr("在新终端中继续上次的对话"),
            window,
            cx,
            move |mut sub, _, cx| {
                for (ix, start) in resumable.iter().enumerate() {
                    let row = menu_row(("menu-resume", ix), Some(start.clone()), None, &on, cx);
                    sub = sub.item(row);
                }
                sub
            },
        );
    }
    let run = on_start.clone();
    menu.separator()
        .item(
            PopupMenuItem::new(tr("运行命令…"))
                .on_click(move |_, window, cx| run_dialog(place.clone(), run.clone(), window, cx)),
        )
        .item(
            PopupMenuItem::new(tr("管理预设…")).on_click(|_, window, cx| open_settings(window, cx)),
        )
}

/// 菜单里预设的名字最宽这么多，再长省略（全称在设置的预设表里看，那里悬停显示）。界面库的菜单项外层
/// 不会收窄，只给省略不够：不换行的字最窄也是全长，会把菜单撑出去。弹出菜单里出不了悬停提示，所以给
/// 宽一点，正常的名字都写得下。
const MENU_NAME_MAX: f32 = 360.;

/// 菜单里的一行：图标、名字（只写名字，命令在设置里看），有快捷键的行尾写上（界面上的写法）。
fn menu_row(
    id: impl Into<ElementId>,
    start: Option<Start>,
    shortcut: Option<String>,
    on_start: &OnStart,
    cx: &App,
) -> PopupMenuItem {
    let theme = cx.theme();
    let (name, icon): (SharedString, Icon) = match &start {
        Some(start) => (
            start.name.clone().into(),
            icon(&start.icon).text_color(color(&start.icon, start.hue, cx)),
        ),
        None => (
            tr("终端").into(),
            Icon::new(IconName::SquareTerminal).text_color(theme.muted_foreground),
        ),
    };
    let (id, on) = (id.into(), on_start.clone());
    PopupMenuItem::element(move |_, cx| {
        let muted = cx.theme().muted_foreground;
        h_flex()
            .w_full()
            .min_w(px(180.))
            .gap_3()
            .child(
                div()
                    .id(id.clone())
                    .min_w_0()
                    .max_w(px(MENU_NAME_MAX))
                    .truncate()
                    .child(name.clone()),
            )
            .when_some(shortcut.clone(), |row, shortcut| {
                row.child(
                    div()
                        .flex_none()
                        .ml_auto()
                        .text_xs()
                        .text_color(muted)
                        .child(shortcut),
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
    // 「或者直接运行」：默认运行的那个已经在按钮上了，不再列；默认的是预设时列上只开 shell。
    // 只写名字，不提示命令（命令在设置里看）
    let mut others: Vec<(Option<Start>, Icon, SharedString, Option<SharedString>)> = Vec::new();
    if default.is_some() {
        others.push((
            None,
            ui::icon(IconName::SquareTerminal).text_color(muted),
            tr("只开 shell").into(),
            Some(tr("新建终端，不运行程序").into()),
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
            None,
        ));
    }
    let links: Vec<Button> = others
        .into_iter()
        .enumerate()
        .map(|(ix, (start, glyph, name, tip))| {
            let on = on_start.clone();
            // 带边框：原来是没有底色、没有边框的按钮，和旁边的字连在一起，看不出能点
            Button::new(("first-run", ix))
                .outline()
                .xsmall()
                .icon(glyph)
                .label(name)
                .when_some(tip, |button, tip| button.tooltip(tip))
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

/// 「运行命令…」：问一条命令，开一个终端输入它；勾了「同时存为预设」的另外存一个预设。
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
                        places: Vec::new(),
                        background: false,
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
    .description(crate::keymap::with_key(
        tr("侧栏的「新终端」和会话页标签栏的 + 开的是它（Ctrl+Shift+`）。"),
        "new.default",
        cx,
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

/// 预设表：图标、名字、命令、继续上次的对话、在菜单里显不显示、编辑、删除；拖着行排序。下面是
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
        .child(cmd_col(div()).child(tr("继续上次的对话")))
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
            .on_drop(move |drag: &DragPreset, _, cx| move_preset(drag.index, index, cx))
            .child(
                ui::icon(IconName::GripVertical)
                    .flex_none()
                    .text_color(muted.opacity(0.6)),
            )
            .child(
                name_col(v_flex())
                    .child(
                        h_flex()
                            .gap_2()
                            .child(icon(&preset.icon).flex_none().size_4().text_color(color(
                                &preset.icon,
                                preset.hue,
                                cx,
                            )))
                            .child(
                                ui::clipped(("preset-name", index), preset.name.clone()).text_sm(),
                            )
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
                    // 还出现在哪些右键菜单里：「显示」只管新建终端的菜单，关着也照样在右键菜单里
                    .when(!preset.places.is_empty(), |name| {
                        let places: Vec<&str> =
                            preset.places.iter().map(|place| place.label()).collect();
                        name.child(
                            ui::clipped(
                                ("preset-places", index),
                                tf!("右键菜单：{places}", places = places.join(tr("、"))),
                            )
                            .pl(px(24.))
                            .text_xs()
                            .text_color(muted),
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
    // 拖到底下这一行：排到最后（每一行的指示线都画在上边，拖到一行上是排到它前面）
    let count = presets.len();
    let footer = h_flex()
        .id("preset-footer")
        .pt_2()
        .gap_2()
        .flex_wrap()
        .drag_over::<DragPreset>(|style, _, _, cx| {
            style.border_t_2().border_color(cx.theme().primary)
        })
        .on_drop(move |drag: &DragPreset, _, cx| move_preset(drag.index, count, cx))
        .child(
            Button::new("preset-add")
                .small()
                .icon(IconName::Plus)
                .label(tr("添加预设…"))
                .on_click(|_, window, cx| edit_dialog(None, window, cx)),
        )
        // 不加悬停提示：提示出现在按钮下面，会盖住弹出的菜单
        .child(
            Button::new("preset-example")
                .small()
                .label(tr("添加示例"))
                .dropdown_caret(true)
                .dropdown_menu(|mut menu, _, _| {
                    for example in examples() {
                        let name = example.name.clone();
                        menu = menu.item(
                            PopupMenuItem::new(name)
                                .on_click(move |_, _, cx| add(example.clone(), cx)),
                        );
                    }
                    menu.separator().label(tr(
                        "示例出现在提交、文件的右键菜单里，命令中的 $sha、$file 换成所点的对象。",
                    ))
                }),
        )
        .child(
            Button::new("preset-restore")
                .small()
                .label(tr("恢复内置预设"))
                .disabled(!missing)
                .tooltip(tr("恢复已删除的内置预设"))
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

/// 添加、编辑预设的对话框：名称、命令、继续上次的对话、图标和颜色。
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
        places: Vec::new(),
        background: false,
    });
    let name = field(&base.name, tr("例如 开发服务器"), window, cx);
    let command = field(&base.command, tr("例如 npm run dev"), window, cx);
    let resume = field(&base.resume, tr("例如 claude -c"), window, cx);
    let icon_choice = Rc::new(RefCell::new(base.icon.clone()));
    let hue_choice = Rc::new(Cell::new(base.hue));
    // 出现在哪：新建终端的菜单，提交、分支、文件的右键菜单
    let shown_choice = Rc::new(Cell::new(base.shown));
    let places_choice = Rc::new(RefCell::new(base.places.clone()));
    let background_choice = Rc::new(Cell::new(base.background));
    let problem: Rc<RefCell<Option<SharedString>>> = Rc::default();
    let submit: Submit = {
        let (name, command, resume) = (name.clone(), command.clone(), resume.clone());
        let (icon_choice, hue_choice, problem) =
            (icon_choice.clone(), hue_choice.clone(), problem.clone());
        let (shown_choice, places_choice, background_choice) = (
            shown_choice.clone(),
            places_choice.clone(),
            background_choice.clone(),
        );
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
                shown: shown_choice.get(),
                places: places_choice.borrow().clone(),
                background: background_choice.get(),
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
                    .child(label(tr("继续上次的对话（可选）")))
                    .child(Input::new(&resume)),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(label(tr("图标和颜色")))
                    .child(h_flex().gap_1().flex_wrap().children(icons))
                    .child(h_flex().gap_1().children(colors)),
            )
            .child({
                let shown = shown_choice.clone();
                let chosen = places_choice.borrow().clone();
                let places = Place::ALL.into_iter().enumerate().map(|(ix, place)| {
                    let pick = places_choice.clone();
                    Checkbox::new(("preset-place", ix))
                        .label(place.label())
                        .checked(chosen.contains(&place))
                        .on_click(move |on, window, _| {
                            let mut list = pick.borrow_mut();
                            list.retain(|have| *have != place);
                            if *on {
                                list.push(place);
                            }
                            window.refresh();
                        })
                });
                let background = background_choice.clone();
                v_flex()
                    .gap_2()
                    .child(label(tr("出现在")))
                    .child(
                        h_flex()
                            .gap_4()
                            .flex_wrap()
                            .child(
                                Checkbox::new("preset-place-new")
                                    .label(tr("新建终端"))
                                    .checked(shown.get())
                                    .on_click(move |on, window, _| {
                                        shown.set(*on);
                                        window.refresh();
                                    }),
                            )
                            .children(places),
                    )
                    .child(div().text_xs().text_color(muted).child(tr(
                        "勾选提交、分支或文件后，命令出现在它们的右键菜单里，可用 $sha、$branch、$file、$repo、$remote 代表所点的对象。",
                    )))
                    .when(!chosen.is_empty(), |d| {
                        d.child(
                            Checkbox::new("preset-background")
                                .label(tr("从右键菜单运行时不开终端，在后台运行并显示输出"))
                                .checked(background.get())
                                .on_click(move |on, window, _| {
                                    background.set(*on);
                                    window.refresh();
                                }),
                        )
                    })
            });
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
        Preset, Start, Started, builtins, display_name, drop_index, free_id, logo, program_of,
        restore_builtins, upgrade_builtins,
    };

    #[test]
    fn 拖到一行上是排到它前面() {
        let moved = |from: usize, index: usize| {
            let mut list = vec!['A', 'B', 'C'];
            let item = list.remove(from);
            list.insert(drop_index(from, index).min(list.len()), item);
            list.into_iter().collect::<String>()
        };
        // 往下拖：A 拖到 C 上，排在 C 前面
        assert_eq!(moved(0, 2), "BAC");
        // 往上拖：C 拖到 A 上
        assert_eq!(moved(2, 0), "CAB");
        // 拖到底下那一行：排到最后
        assert_eq!(moved(0, 3), "BCA");
        assert_eq!(moved(1, 3), "ACB");
    }

    #[test]
    fn 预设按会话记_序号被别人重用了不算() {
        let slot = crate::settings::TermSlot {
            entry: crate::settings::RepoEntry {
                server: None,
                path: "/work".into(),
            },
            number: 1,
        };
        let start = Start::command("npm run dev");
        let mut started = Started::default();
        started.remember(slot.clone(), Some(start.clone()));
        // 还没开好：谁问都算
        assert!(started.get(&slot, Some("t1")).is_some());
        started.opened(&slot, "t1", true);
        assert!(started.get(&slot, Some("t1")).is_some());
        // 别处用同一个序号开的另一个会话
        assert!(started.get(&slot, Some("t2")).is_none());
        // 重启别人的那个：不输入这条命令
        started.restarting(&slot, "t2");
        assert!(started.get(&slot, None).is_none());
        // 自己的重启：新开的接着算
        started.remember(slot.clone(), Some(start.clone()));
        started.opened(&slot, "t1", true);
        started.restarting(&slot, "t1");
        assert!(started.get(&slot, None).is_some());
        started.opened(&slot, "t3", true);
        assert!(started.get(&slot, Some("t3")).is_some());
        // 结束别人的不动，结束自己的去掉
        started.ended(&slot, "t9");
        assert!(started.get(&slot, Some("t3")).is_some());
        started.ended(&slot, "t3");
        assert!(started.get(&slot, None).is_none());
        // 想新开、接上的却是已有的：不算这个预设的
        started.remember(slot.clone(), Some(start));
        started.opened(&slot, "t4", false);
        assert!(started.get(&slot, None).is_none());
    }

    #[test]
    fn 重开以后_存下的从预设开的终端对上列表才算() {
        use fr_channel::TermInfo;
        let slot = |path: &str, number: u32| crate::settings::TermSlot {
            entry: crate::settings::RepoEntry {
                server: None,
                path: path.into(),
            },
            number,
        };
        let info = |term: &str, path: &str, number: u32, age: u64| TermInfo {
            term: term.into(),
            key: crate::terminal::session_key(path, number),
            age,
            ..TermInfo::default()
        };
        let now = 10_000;
        // 开好了才存；存的按仓库、序号排
        let mut started = Started::default();
        started.remember(slot("/b", 1), Some(Start::command("codex")));
        started.remember(slot("/a", 2), Some(Start::command("claude")));
        started.remember(slot("/a", 3), Some(Start::command("agy")));
        assert!(started.records().is_empty());
        started.opened(&slot("/b", 1), "t1", true);
        started.opened(&slot("/a", 2), "t2", true);
        started.opened(&slot("/a", 3), "t3", true);
        let mut records = started.records();
        assert_eq!(
            records.iter().map(|r| r.term.as_str()).collect::<Vec<_>>(),
            ["t2", "t3", "t1"]
        );
        for record in &mut records {
            record.born = now - 600;
        }
        // 重开 Kuai：读进来的先不算
        let mut loaded = Started::load(&records);
        assert!(loaded.get(&slot("/a", 2), Some("t2")).is_none());
        // 空的列表不动
        assert!(!loaded.reconcile("", &[], now));
        // 同一个编号、开出来的时刻对得上：算；编号对上但是新开的（kuaid 重新启动过）：去掉；
        // 列表里没有了：去掉
        let list = [info("t2", "/a", 2, 600), info("t1", "/b", 1, 5)];
        assert!(loaded.reconcile("", &list, now));
        assert_eq!(
            loaded.get(&slot("/a", 2), Some("t2")).map(|s| s.command),
            Some("claude".into())
        );
        assert!(!loaded.0.contains_key(&slot("/b", 1)));
        assert!(!loaded.0.contains_key(&slot("/a", 3)));
        // 别的电脑上的不动
        let mut remote = records.clone();
        remote[0].slot.entry.server = Some("s1".into());
        let mut loaded = Started::load(&remote);
        loaded.reconcile("", &list, now);
        assert!(
            loaded
                .0
                .get(&remote[0].slot)
                .is_some_and(|found| found.pending)
        );
        // 仓库终端页问到的：对得上的算，对不上的不动
        let mut loaded = Started::load(&records);
        loaded.confirm(&slot("/b", 1), "t1", 5, now);
        assert!(loaded.get(&slot("/b", 1), Some("t1")).is_none());
        loaded.confirm(&slot("/a", 2), "t2", 590, now);
        assert!(loaded.get(&slot("/a", 2), Some("t2")).is_some());
    }

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
        // 提示符设的「用户@主机: 目录」只留目录
        assert_eq!(
            display_name(Some("dev@box: ~/src/kuai"), None).map(|n| n.to_string()),
            Some("~/src/kuai".to_string())
        );
        assert_eq!(
            display_name(Some("me@devbox:~/proj"), None).map(|n| n.to_string()),
            Some("~/proj".to_string())
        );
        assert_eq!(display_name(Some("dev@box:"), None), None);
        // 带空格的不算（「a@b 的会话: x」是程序自己设的）
        assert_eq!(
            display_name(Some("fix bug @ login: done"), None).map(|n| n.to_string()),
            Some("fix bug @ login: done".to_string())
        );
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
        // OpenCode 例外：`opencode mini` 换不了模型，用完整的界面
        assert_eq!(
            command("opencode"),
            ("opencode".into(), "opencode -c".into())
        );
        // 内置的 id 不重复，图标都是各家的
        let mut ids: Vec<&str> = all.iter().map(|p| p.id.as_str()).collect();
        ids.dedup();
        assert_eq!(ids.len(), 4);
        assert!(all.iter().all(|p| logo(&p.icon).is_some()));
    }

    #[test]
    fn 存着的内置预设旧命令_原样的换成现在的() {
        let old = |command: &str, resume: &str| Preset {
            command: command.into(),
            resume: resume.into(),
            ..builtins().into_iter().find(|p| p.id == "opencode").unwrap()
        };
        let mut list = vec![old("opencode mini", "opencode mini -c")];
        upgrade_builtins(&mut list);
        assert_eq!(
            (list[0].command.as_str(), list[0].resume.as_str()),
            ("opencode", "opencode -c")
        );
        // 自己改过的不动；别的预设的同样命令也不动
        let mut list = vec![old("opencode mini --model x/y", "opencode mini -c")];
        upgrade_builtins(&mut list);
        assert_eq!(
            (list[0].command.as_str(), list[0].resume.as_str()),
            ("opencode mini --model x/y", "opencode -c")
        );
        let mut mine = old("opencode mini", "");
        mine.id = "custom-1".into();
        let mut list = vec![mine];
        upgrade_builtins(&mut list);
        assert_eq!(list[0].command, "opencode mini");
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
                places: Vec::new(),
                background: false,
            }
        }
    }
}
