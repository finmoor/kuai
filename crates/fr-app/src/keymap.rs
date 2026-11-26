//! 快捷键：能改的操作都登记在 [`entries`]（名字、分组、默认的键、终端里还归不归 Kuai），设置里只存
//! 改过的（[`Settings::keymap`]）。启动时和设置变了以后按表绑（[`sync`]）；界面上写快捷键的地方都从
//! 这里取（[`label`]、[`with_key`]），改了键提示跟着变。设置页的「快捷键」一页见 `keys_page.rs`。
//!
//! 改键不清空键位表（那样界面库自己绑的 Tab、Ctrl+C 这些也没了），靠「同一层后绑的优先」：新的键绑到
//! 操作上，不再用的键只解开原来的操作（`Unbind`）。终端那一层（见 `main.rs` 的
//! `release_keys_to_terminal`）跟着改：终端里也归 Kuai 的键在终端那一层再绑一遍，交给程序的绑成
//! 「什么都不做」。录快捷键时（[`RECORD_CONTEXT`]）所有用过的键都绑成「什么都不做」，按什么都送到
//! 录制框。细节见 [`sync`]。
//!
//! 键在设置里、键位表里用 GPUI 的写法（`ctrl-shift-\``），界面上写成 `Ctrl+Shift+\``（[`display`]）。

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use fr_core::i18n::{tr, tr_key};
use gpui_kit::*;
use serde::Deserialize;

use crate::settings::Settings;

/// 设置页录快捷键时的按键上下文：这里所有用过的键都绑成「什么都不做」，按什么都送到录制框。
pub const RECORD_CONTEXT: &str = "KeyRecorder";

/// 在当前仓库新开一个终端，运行这个预设（`preset` 是预设的 id，空的是只开 shell）。
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = kuai, no_json)]
pub struct RunPreset {
    pub preset: String,
}

/// 快捷键分组（设置页里一组一张卡片）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    Window,
    Page,
    NewTerminal,
    Hub,
    Git,
    File,
    Terminal,
}

impl Group {
    pub const ALL: [Group; 7] = [
        Group::Window,
        Group::Page,
        Group::NewTerminal,
        Group::Hub,
        Group::Git,
        Group::File,
        Group::Terminal,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Group::Window => tr("标签页与窗口"),
            Group::Page => tr("页面"),
            Group::NewTerminal => tr("新建终端"),
            Group::Hub => tr("会话页"),
            Group::Git => "Git",
            Group::File => tr("文件"),
            Group::Terminal => tr("终端中"),
        }
    }

    /// 写在组标题后面的一句（没有是 None）
    pub fn lead(self) -> Option<&'static str> {
        match self {
            Group::NewTerminal => Some(tr("在当前仓库新建终端并运行对应程序")),
            Group::Terminal => Some(tr("由终端处理，不可修改")),
            _ => None,
        }
    }
}

/// 焦点在终端里时这个键归谁。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InTerminal {
    /// 仍由 Kuai 处理：终端里的程序收不到
    Keep,
    /// 交给终端里的程序
    Pass,
    /// 不能改：终端、输入框自己处理的键。列出来是为了查得到，也不让别的操作占用
    Fixed,
}

/// 一个快捷键。
#[derive(Clone)]
pub struct Entry {
    /// 认它用的（设置里按它存）：`tab.close`、`new.claude` 这样
    pub id: String,
    pub group: Group,
    /// 名字（已经翻译好）
    pub name: SharedString,
    /// 一句说明（已经翻译好，可以空着）
    pub desc: SharedString,
    /// 默认的键（GPUI 的写法）；空的是默认没有
    pub defaults: Vec<String>,
    pub scope: InTerminal,
    /// 新建终端运行哪个预设（`Some("")` 是只开 shell）；不是新建终端的是 None
    pub preset: Option<String>,
    /// 按下以后做的事；不能改的是 None
    action: Option<fn() -> Box<dyn Action>>,
}

impl Entry {
    pub fn editable(&self) -> bool {
        self.scope != InTerminal::Fixed
    }

    /// 按下以后做的事（命令面板里选中这一条时也是做它）；只列出来看的没有。
    pub fn make_action(&self) -> Option<Box<dyn Action>> {
        if let Some(preset) = &self.preset {
            return Some(Box::new(RunPreset {
                preset: preset.clone(),
            }));
        }
        self.action.map(|make| make())
    }
}

/// 能改的、只列出来看的快捷键：(id, 分组, 名字, 说明, 默认的键, 终端里, 按下做的事)。
///
/// 终端里也可用（[`InTerminal::Keep`]）的都是终端里的程序本来就收不到、或者收到了也和不按 Shift 时没有区别
/// 的组合键；F5、Ctrl+W、Ctrl+F、Ctrl+P、Ctrl+S 交给程序（删一个词、往前搜、上一条命令……）。
type Row = (
    &'static str,
    Group,
    &'static str,
    &'static str,
    &'static [&'static str],
    InTerminal,
    Option<fn() -> Box<dyn Action>>,
);

#[rustfmt::skip]
fn rows() -> Vec<Row> {
    use Group::{File, Git, Hub, NewTerminal, Page, Terminal, Window};
    use InTerminal::{Fixed, Keep, Pass};
    vec![
        ("tab.close", Window, tr_key("关闭标签页"), tr_key("有操作正在执行时会先提示"), &["ctrl-w"], Pass, Some(|| Box::new(crate::CloseTab))),
        ("tab.next", Window, tr_key("下一个标签页"), "", &["ctrl-tab"], Keep, Some(|| Box::new(crate::NextTab))),
        ("tab.prev", Window, tr_key("上一个标签页"), "", &["ctrl-shift-tab"], Keep, Some(|| Box::new(crate::PrevTab))),
        ("tab.reopen", Window, tr_key("重新打开关闭的标签页"), "", &["ctrl-shift-t"], Keep, Some(|| Box::new(crate::ReopenTab))),
        ("quick", Window, tr_key("快速打开"), tr_key("搜索仓库、命令、分支和文件"), &["ctrl-p"], Pass, Some(|| Box::new(crate::QuickOpen))),
        ("settings", Window, tr_key("打开设置"), "", &["ctrl-,"], Keep, Some(|| Box::new(crate::OpenSettings))),
        // 不用 Ctrl+B：焦点在终端里时它归里面的程序（Claude Code 把命令转到后台、tmux 的前缀键）
        ("sidebar", Window, tr_key("收起或展开侧栏"), "", &["ctrl-shift-b"], Keep, Some(|| Box::new(crate::ToggleSidebar))),
        // 标题栏最左边的两个按钮；在子仓库、工作树里时后退先回到上一层。焦点在终端、输入框里时归它们
        // （往回、往后一个词），输入框那边见 main.rs
        ("back", Window, tr_key("后退"), "", &["alt-left"], Pass, Some(|| Box::new(crate::GoBack))),
        ("forward", Window, tr_key("前进"), "", &["alt-right"], Pass, Some(|| Box::new(crate::GoForward))),
        ("refresh", Window, tr_key("刷新"), tr_key("刷新当前仓库的状态和提交历史"), &["f5"], Pass, Some(|| Box::new(crate::Refresh))),
        ("page.changes", Page, tr_key("本地修改"), tr_key("切换到 Git 页的本地修改"), &["ctrl-1"], Keep, Some(|| Box::new(crate::ShowChanges))),
        ("page.history", Page, tr_key("所有提交"), tr_key("切换到 Git 页的所有提交"), &["ctrl-2"], Keep, Some(|| Box::new(crate::ShowHistory))),
        ("page.head", Page, tr_key("回到 HEAD"), "", &["ctrl-0"], Keep, Some(|| Box::new(crate::GoToHead))),
        // 和 VS Code 的资源管理器一样
        ("page.files", Page, tr_key("文件页"), "", &["ctrl-shift-e"], Keep, Some(|| Box::new(crate::ShowFiles))),
        ("page.terminal", Page, tr_key("终端页"), tr_key("在终端页和之前的页面之间切换"), &["ctrl-`"], Keep, Some(|| Box::new(crate::OpenTerminal))),
        // 终端里的程序分不出 Ctrl+Shift+A 和 Ctrl+A，留给 Kuai；Ctrl+Shift+N 同理
        ("page.hub", Page, tr_key("会话页"), "", &["ctrl-shift-a"], Keep, Some(|| Box::new(crate::ShowTerminals))),
        ("new.default", NewTerminal, tr_key("新建终端"), tr_key("运行设为默认的预设"), &["ctrl-shift-`"], Keep, Some(|| Box::new(crate::NewTerminal))),
        ("new.pick", NewTerminal, tr_key("选择仓库新建终端"), tr_key("在会话页中打开"), &["ctrl-shift-n"], Keep, Some(|| Box::new(crate::PickNewTerminal))),
        ("hub.prev", Hub, tr_key("上一个终端"), tr_key("焦点在终端中时也可用"), &["ctrl-shift-up"], Fixed, None),
        ("hub.next", Hub, tr_key("下一个终端"), tr_key("焦点在终端中时也可用"), &["ctrl-shift-down"], Fixed, None),
        // 会话页、仓库终端页：F6 是 Windows 上在窗格之间切换的通行键；Esc 留给
        // Claude Code 这些程序。这几个动作只有会话页、仓库终端页接，别处按了照常交给原来的处理
        ("hub.focus", Hub, tr_key("在侧栏和终端之间切换"), tr_key("会话页、仓库终端页；焦点在终端中时也可用"), &["f6"], Keep, Some(|| Box::new(crate::FocusSwitch))),
        ("hub.focus_back", Hub, tr_key("反向切换焦点"), tr_key("焦点在终端中时也可用"), &["shift-f6"], Keep, Some(|| Box::new(crate::FocusSwitchBack))),
        ("hub.input", Hub, tr_key("进入输入"), tr_key("焦点在会话页侧栏中时"), &["enter"], Fixed, None),
        ("hub.open_repo", Hub, tr_key("打开所在仓库"), tr_key("焦点在会话页侧栏中时"), &["ctrl-enter"], Pass, Some(|| Box::new(crate::OpenTerminalRepo))),
        ("hub.rename", Hub, tr_key("重命名终端"), tr_key("焦点在侧栏中时；终端中的 F2 交给程序"), &["f2"], Pass, Some(|| Box::new(crate::RenameTerminal))),
        ("hub.cell", Hub, tr_key("并排查看时换格子"), tr_key("焦点在终端中时也可用"), &["ctrl-alt-left", "ctrl-alt-right"], Fixed, None),
        ("git.commit", Git, tr_key("提交"), tr_key("焦点在提交说明、文件列表或改动中时"), &["ctrl-enter"], Fixed, None),
        // 工具栏上的几个操作，默认的键和 Fork 一样。焦点在终端里时交给程序（它们收到的是 Ctrl+F、
        // Ctrl+L 这些）：联网、改仓库的操作不该在终端里打字时被误触
        ("git.fetch", Git, tr_key("获取"), tr_key("从远程下载新的提交和分支，不改动当前分支"), &["ctrl-shift-f"], Pass, Some(|| Box::new(crate::Fetch))),
        ("git.pull", Git, tr_key("拉取"), tr_key("获取远程的新提交，并合并到当前分支"), &["ctrl-shift-l"], Pass, Some(|| Box::new(crate::Pull))),
        ("git.push", Git, tr_key("推送"), tr_key("将当前分支的新提交上传到远程"), &["ctrl-shift-p"], Pass, Some(|| Box::new(crate::Push))),
        ("git.stash", Git, tr_key("贮藏"), tr_key("将未提交的改动暂时收起"), &["ctrl-shift-s"], Pass, Some(|| Box::new(crate::StashChanges))),
        ("git.pop", Git, tr_key("弹出贮藏"), tr_key("恢复最近一次贮藏的改动，并删除该贮藏"), &[], Pass, Some(|| Box::new(crate::PopStash))),
        ("git.branch", Git, tr_key("新建分支"), "", &[], Pass, Some(|| Box::new(crate::NewBranch))),
        ("git.find", Git, tr_key("查找"), tr_key("Git 页中搜索提交，文件页中查找打开的文件"), &["ctrl-f"], Pass, Some(|| Box::new(crate::FindCommit))),
        ("file.save", File, tr_key("保存文件"), "", &["ctrl-s"], Pass, Some(|| Box::new(crate::SaveFile))),
        // 和 VS Code 一样；焦点在终端里时归终端（粘贴）
        ("file.preview", File, tr_key("切换 Markdown 预览"), "", &["ctrl-shift-v"], Pass, Some(|| Box::new(crate::TogglePreview))),
        ("term.interrupt", Terminal, tr_key("中断"), tr_key("选中文字时为复制"), &["ctrl-c"], Fixed, None),
        ("term.copy", Terminal, tr_key("复制"), "", &["ctrl-shift-c"], Fixed, None),
        ("term.paste", Terminal, tr_key("粘贴"), tr_key("剪贴板中为图片或文件时粘贴其路径"), &["ctrl-v", "shift-insert"], Fixed, None),
        ("term.scroll", Terminal, tr_key("向上、向下翻页"), "", &["shift-pageup", "shift-pagedown"], Fixed, None),
    ]
}

/// 全部快捷键，按设置页里的先后：「新建终端」一组在「选择仓库新建终端」后面接「终端」和每个预设。
pub fn entries(cx: &App) -> Vec<Entry> {
    let mut out = Vec::new();
    for (id, group, name, desc, keys, scope, action) in rows() {
        out.push(Entry {
            id: id.to_string(),
            group,
            name: tr(name).into(),
            desc: match desc.is_empty() {
                true => SharedString::default(),
                false => tr(desc).into(),
            },
            defaults: keys.iter().map(|key| key.to_string()).collect(),
            scope,
            preset: None,
            action,
        });
        if id == "new.pick" {
            out.push(Entry {
                id: "new.shell".to_string(),
                group: Group::NewTerminal,
                name: tr("终端").into(),
                desc: tr("只开 shell").into(),
                defaults: Vec::new(),
                scope: InTerminal::Keep,
                preset: Some(String::new()),
                action: None,
            });
            for preset in crate::presets::all(cx) {
                out.push(Entry {
                    id: preset_id(&preset.id),
                    group: Group::NewTerminal,
                    name: preset.name.clone().into(),
                    desc: SharedString::default(),
                    defaults: Vec::new(),
                    scope: InTerminal::Keep,
                    preset: Some(preset.id.clone()),
                    action: None,
                });
            }
        }
    }
    out
}

/// 预设 `preset` 的快捷键的 id。
pub fn preset_id(preset: &str) -> String {
    format!("new.{preset}")
}

/// 这一项现在的键：设置里改过的，没改过是默认的。
pub fn keys_of(entry: &Entry, keymap: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    match keymap.get(&entry.id) {
        Some(keys) if entry.editable() => keys.clone(),
        _ => entry.defaults.clone(),
    }
}

/// 设置里改过这一项（和默认的不一样）。
pub fn changed(entry: &Entry, keymap: &BTreeMap<String, Vec<String>>) -> bool {
    entry.editable()
        && keymap
            .get(&entry.id)
            .is_some_and(|keys| *keys != entry.defaults)
}

// ---------- Linux 上 Shift 加符号键 ----------

/// GPUI 在 Linux 上不按键盘布局换算：Shift 加符号键、数字键报成符号本身、去掉 Shift（按
/// Ctrl+Shift+\` 收到的是 `ctrl-~`，Ctrl+Shift+1 是 `ctrl-!`），键盘布局映射也是空的，写成
/// `ctrl-shift-\`` 的绑定按不出来。Windows 上它按真的键盘布局换算，不用管。
const SHIFT_IN_SYMBOL: bool = cfg!(not(any(target_os = "windows", target_os = "macos")));

/// 美式键盘上 Shift 加这个键出来的符号。别的布局对不上时只是显示的键名不同，按下去照样能用：
/// 绑的就是按下时收到的写法（[`typed`]）。
const SHIFTED: [(&str, &str); 21] = [
    ("`", "~"),
    ("1", "!"),
    ("2", "@"),
    ("3", "#"),
    ("4", "$"),
    ("5", "%"),
    ("6", "^"),
    ("7", "&"),
    ("8", "*"),
    ("9", "("),
    ("0", ")"),
    ("-", "_"),
    ("=", "+"),
    ("[", "{"),
    ("]", "}"),
    ("\\", "|"),
    (";", ":"),
    ("'", "\""),
    (",", "<"),
    (".", ">"),
    ("/", "?"),
];

/// 一个键按 GPUI 的写法拼回去（修饰键的顺序和它一样：fn、ctrl、alt、super、shift）。
fn join(mods: Modifiers, key: &str) -> String {
    let mut out = String::new();
    for (on, name) in [
        (mods.function, "fn-"),
        (mods.control, "ctrl-"),
        (mods.alt, "alt-"),
        (mods.platform, "super-"),
        (mods.shift, "shift-"),
    ] {
        if on {
            out.push_str(name);
        }
    }
    out.push_str(key);
    out
}

/// 一组键里每一个都过一遍 `each`，有变化才返回。
fn map_each(keys: &str, each: impl Fn(Modifiers, &str) -> Option<String>) -> Option<String> {
    let mut changed = false;
    let parts: Vec<String> = keys
        .split_whitespace()
        .map(|one| {
            let (mods, key) = split(one);
            match each(mods, &key) {
                Some(new) => {
                    changed = true;
                    new
                }
                None => one.to_string(),
            }
        })
        .collect();
    changed.then(|| parts.join(" "))
}

/// 并进符号里的 Shift 拆出来：`ctrl-~` → `ctrl-shift-\``。
fn shift_out(keys: &str) -> Option<String> {
    map_each(keys, |mut mods, key| {
        let (base, _) = SHIFTED.iter().find(|(_, shifted)| *shifted == key)?;
        (!mods.shift).then(|| {
            mods.shift = true;
            join(mods, base)
        })
    })
}

/// 反过来，Shift 加符号键写成按下时收到的符号：`ctrl-shift-\`` → `ctrl-~`。
fn shift_in(keys: &str) -> Option<String> {
    map_each(keys, |mut mods, key| {
        let (_, shifted) = SHIFTED.iter().find(|(base, _)| *base == key)?;
        mods.shift.then(|| {
            mods.shift = false;
            join(mods, shifted)
        })
    })
}

/// 按下 `keys`（统一的写法）时 GPUI 实际收到的写法，和统一的写法不一样时才有（只在 Linux 上，
/// 见 [`SHIFT_IN_SYMBOL`]）。绑键、让键时两种都要：统一的那种按不出来。
pub fn typed(keys: &str) -> Option<String> {
    SHIFT_IN_SYMBOL.then(|| shift_in(keys)).flatten()
}

/// 绑一个键时要绑的几种写法：统一的写法，加上 Linux 上按下时实际收到的（[`typed`]）。
pub fn forms(keys: &str) -> Vec<String> {
    std::iter::once(keys.to_string())
        .chain(typed(keys))
        .collect()
}

// ---------- 绑键 ----------

/// 上一次绑上的：(键, 哪一项)。只重绑变了的。
#[derive(Default)]
struct Bound {
    keys: Vec<(String, String)>,
    /// 绑上的每一项按下以后做的事叫什么：去掉键时只解开它（见 [`sync`]）
    actions: HashMap<String, &'static str>,
    /// 现在每一项的键（界面上显示用）
    effective: HashMap<String, Vec<String>>,
}

impl Global for Bound {}

/// 按设置绑快捷键（启动时、设置变了以后调用）。只动变了的键：
///
/// - 不再用的键：绑一个 `Unbind`，只解开原来那个操作。不绑「什么都不做」：那样同一个键在输入框里
///   自己的用处也一起没了。
/// - 新用的键：全局绑到操作上；终端那一层，终端里可用的再绑一遍，交给程序的绑成「什么都不做」；录键那
///   一层绑成「什么都不做」。
/// - 最里层自己绑了同一个键的（输入框这些）：自己选的新键原来归它们，重新登记一遍放到后面，焦点在
///   它们里面时还归它们（没有上下文的绑定和最里层一样深，一样深的后登记的优先）。默认的键原来就是全局
///   的优先（Ctrl+Shift+A 这些，见 `main.rs`），不动。
pub fn sync(settings: &Settings, cx: &mut App) {
    let entries = entries(cx);
    let mut wanted: Vec<(String, String)> = Vec::new();
    let mut effective = HashMap::new();
    for entry in &entries {
        let keys: Vec<String> = keys_of(entry, &settings.keymap)
            .iter()
            .map(|keys| canonical(keys, cx))
            .collect();
        if entry.editable() {
            for keys in &keys {
                // 同一个键写给了两项（手改设置文件改出来的）：前面的算
                if !wanted.iter().any(|(bound, _)| bound == keys) {
                    wanted.push((keys.clone(), entry.id.clone()));
                }
            }
        }
        effective.insert(entry.id.clone(), keys);
    }
    let bound = cx.default_global::<Bound>();
    let (before, old_actions) = (bound.keys.clone(), bound.actions.clone());
    if before == wanted {
        cx.global_mut::<Bound>().effective = effective;
        return;
    }
    let action_of = |id: &str| {
        entries
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.make_action())
    };
    let added: Vec<(String, String)> = wanted
        .iter()
        .filter(|pair| !before.contains(pair))
        .cloned()
        .collect();
    let terminal = predicate(crate::terminal::KEY_CONTEXT);
    let record = predicate(RECORD_CONTEXT);
    let ours: Vec<&'static str> = entries
        .iter()
        .filter_map(|entry| entry.make_action())
        .map(|action| action.name())
        .collect();
    // 要在新绑以前看：这些键原来归谁
    let contextual = contextual_owners(&added, &entries, &ours, &[&terminal, &record], cx);
    let load = |keys: &str,
                action: Box<dyn Action>,
                context: &Option<Rc<KeyBindingContextPredicate>>,
                cx: &App| {
        KeyBinding::load(keys, action, context.clone(), false, None, cx)
            .inspect_err(|error| log::warn!("快捷键 {keys} 绑不上：{error}"))
            .ok()
    };
    // 每个键按 `forms` 绑：Linux 上 Shift 加符号键另绑按下时实际收到的写法
    let mut bindings: Vec<KeyBinding> = Vec::new();
    for (keys, id) in before.iter().filter(|pair| !wanted.contains(pair)) {
        if let Some(name) = old_actions.get(id) {
            for keys in forms(keys) {
                bindings.extend(load(&keys, Box::new(Unbind((*name).into())), &None, cx));
            }
        }
    }
    for (keys, id) in &added {
        let Some(action) = action_of(id) else {
            continue;
        };
        let keep = entries
            .iter()
            .any(|entry| entry.id == *id && entry.scope == InTerminal::Keep);
        for keys in forms(keys) {
            bindings.extend(load(&keys, action.boxed_clone(), &None, cx));
            let in_terminal: Box<dyn Action> = match keep {
                true => action.boxed_clone(),
                false => Box::new(NoAction),
            };
            bindings.extend(load(&keys, in_terminal, &terminal, cx));
            bindings.extend(load(&keys, Box::new(NoAction), &record, cx));
        }
    }
    bindings.extend(contextual);
    cx.bind_keys(bindings);
    let actions: HashMap<String, &'static str> = wanted
        .iter()
        .filter_map(|(_, id)| Some((id.clone(), action_of(id)?.name())))
        .collect();
    let bound = cx.global_mut::<Bound>();
    bound.keys = wanted;
    bound.actions = actions;
    bound.effective = effective;
}

/// `added` 里的键原来归最里层自己绑了它的那几条（输入框这些）：新的全局绑定登记以后，把它们重新
/// 登记一遍。原来全局绑过这个键的，只算登记在那以后的（它们原来就比全局的优先）；原来没绑过、又是
/// 这一项默认的键（程序刚启动），全局的优先，不算。
fn contextual_owners(
    added: &[(String, String)],
    entries: &[Entry],
    ours: &[&'static str],
    skip: &[&Option<Rc<KeyBindingContextPredicate>>],
    cx: &App,
) -> Vec<KeyBinding> {
    if added.is_empty() {
        return Vec::new();
    }
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    let all: Vec<(usize, &KeyBinding, String)> = keymap
        .bindings()
        .enumerate()
        .map(|(ix, binding)| (ix, binding, binding_keys(binding, cx)))
        .collect();
    let skipped = |predicate: &KeyBindingContextPredicate| {
        skip.iter()
            .any(|skip| skip.as_deref().is_some_and(|skip| *skip == *predicate))
    };
    let mut out = Vec::new();
    for (keys, id) in added {
        let mine = all
            .iter()
            .filter(|(_, binding, bound)| {
                bound == keys
                    && binding.predicate().is_none()
                    && ours.contains(&binding.action().name())
            })
            .map(|(ix, ..)| *ix)
            .max();
        let default = entries
            .iter()
            .any(|entry| entry.id == *id && entry.defaults.contains(keys));
        if mine.is_none() && default {
            continue;
        }
        out.extend(
            all.iter()
                .filter(|(ix, binding, bound)| {
                    bound == keys
                        && mine.is_none_or(|mine| *ix > mine)
                        && binding
                            .predicate()
                            .is_some_and(|predicate| !skipped(&predicate))
                })
                .map(|(_, binding, _)| (*binding).clone()),
        );
    }
    out
}

fn predicate(context: &str) -> Option<Rc<KeyBindingContextPredicate>> {
    KeyBindingContextPredicate::parse(context).ok().map(Rc::new)
}

/// 键换成统一的写法：Windows 上按 Ctrl+Shift+` 收到的是 `ctrl-~`、数字键加 Shift 收到的是 `ctrl-!`
/// 这样，按键盘布局换回 `ctrl-shift-\``；Linux 上 GPUI 不换，按美式键盘换（[`SHIFT_IN_SYMBOL`]）。
/// 修饰键按 ctrl、alt、win、shift 排。认不出的原样返回。
pub fn canonical(keys: &str, cx: &App) -> String {
    let os = cx.platform_info().os;
    let Ok(binding) = KeyBinding::load(keys, Box::new(NoAction), None, false, None, cx) else {
        return keys.to_string();
    };
    let unparsed = binding
        .keystrokes()
        .iter()
        .map(|keystroke| keystroke.unparse(os))
        .collect::<Vec<_>>()
        .join(" ");
    // 拆出来以后没有符号了，再走一遍就停
    match SHIFT_IN_SYMBOL.then(|| shift_out(&unparsed)).flatten() {
        Some(split) => canonical(&split, cx),
        None => unparsed,
    }
}

/// 键位表里一条绑定的键，统一的写法（[`canonical`]）。
pub fn binding_keys(binding: &KeyBinding, cx: &App) -> String {
    let os = cx.platform_info().os;
    let raw: Vec<String> = binding
        .keystrokes()
        .iter()
        .map(|keystroke| keystroke.inner().unparse(os))
        .collect();
    canonical(&raw.join(" "), cx)
}

/// 焦点在终端里时仍归 Kuai 的键（统一的写法）：终端里也可用的操作现在的键。启动时让键给终端用。
pub fn terminal_keeps(cx: &App) -> Vec<String> {
    let Some(bound) = cx.try_global::<Bound>() else {
        return Vec::new();
    };
    let entries = entries(cx);
    bound
        .keys
        .iter()
        .filter(|(_, id)| {
            entries
                .iter()
                .any(|entry| entry.id == *id && entry.scope == InTerminal::Keep)
        })
        .map(|(keys, _)| keys.clone())
        .collect()
}

// ---------- 界面上的写法 ----------

/// 这一项现在的第一个键，按界面上的写法（`Ctrl+Shift+\``）；没有键是 None。
pub fn label(id: &str, cx: &App) -> Option<String> {
    let keys = cx
        .try_global::<Bound>()?
        .effective
        .get(id)?
        .first()?
        .clone();
    Some(display(&keys))
}

/// 写着默认快捷键的一句提示（`新建终端（Ctrl+Shift+\`）`），换成这一项现在的键；改成没有键时去掉
/// 它（连同括号或者顿号、逗号）。
pub fn with_key(text: &str, id: &str, cx: &App) -> SharedString {
    let Some(entry) = rows().into_iter().find(|row| row.0 == id) else {
        return text.to_string().into();
    };
    let Some(default) = entry.4.first().map(|keys| display(keys)) else {
        return text.to_string().into();
    };
    match label(id, cx) {
        Some(now) if now == default => text.to_string().into(),
        Some(now) => text.replace(&default, &now).into(),
        None => without(text, &default).into(),
    }
}

/// 一句提示里写着几个默认快捷键（`提交历史和本地修改（Ctrl+1、Ctrl+2）`）：逐个换成现在的。
pub fn with_keys(text: &str, ids: &[&str], cx: &App) -> SharedString {
    ids.iter()
        .fold(SharedString::from(text.to_string()), |text, id| {
            with_key(&text, id, cx)
        })
}

/// 从提示里去掉一个键：先找整个括号，再找带顿号、逗号的，最后只去掉键本身。
fn without(text: &str, keys: &str) -> String {
    let patterns = [
        format!("（{keys}）"),
        format!(" ({keys})"),
        format!("{keys}、"),
        format!("、{keys}"),
        format!("{keys}, "),
        format!(", {keys}"),
    ];
    match patterns
        .iter()
        .find(|pattern| text.contains(pattern.as_str()))
    {
        Some(pattern) => text.replacen(pattern.as_str(), "", 1),
        None => text.replacen(keys, "", 1),
    }
}

/// GPUI 的写法换成界面上的：`ctrl-shift-\`` → `Ctrl+Shift+\``，`alt-left` → `Alt+←`。几个键一组的
/// （`ctrl-k ctrl-s`）用空格分开。
pub fn display(keys: &str) -> String {
    keys.split_whitespace()
        .map(|one| {
            let (mods, key) = split(one);
            let mut parts: Vec<String> = Vec::new();
            for (flag, name) in [
                (mods.control, "Ctrl"),
                (mods.shift, "Shift"),
                (mods.alt, "Alt"),
                (mods.platform, "Win"),
            ] {
                if flag {
                    parts.push(name.to_string());
                }
            }
            parts.push(key_name(&key));
            parts.join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 一个键的修饰键和键名（GPUI 的写法里最后一段是键；减号键写成 `ctrl--`）。
pub fn split(keys: &str) -> (Modifiers, String) {
    let mut mods = Modifiers::none();
    let mut rest = keys;
    while let Some((head, tail)) = rest.split_once('-') {
        if tail.is_empty() {
            break;
        }
        match head {
            "ctrl" => mods.control = true,
            "shift" => mods.shift = true,
            "alt" => mods.alt = true,
            "cmd" | "win" | "super" | "platform" => mods.platform = true,
            "fn" => mods.function = true,
            _ => break,
        }
        rest = tail;
    }
    (mods, rest.to_string())
}

fn key_name(key: &str) -> String {
    let named = match key {
        "left" => "←",
        "right" => "→",
        "up" => "↑",
        "down" => "↓",
        "enter" => "Enter",
        "escape" => "Esc",
        "space" => "Space",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        _ => "",
    };
    if !named.is_empty() {
        return named.to_string();
    }
    if key.len() > 1 && key.starts_with('f') && key[1..].parse::<u8>().is_ok() {
        return key.to_uppercase();
    }
    let mut chars = key.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// 设置页录到的一个键：按下的修饰键加键，换成 GPUI 的写法。只按了修饰键是 None。
pub fn from_keystroke(keystroke: &Keystroke) -> Option<String> {
    let key = keystroke.key.as_str();
    if key.is_empty()
        || matches!(
            key,
            "shift" | "control" | "alt" | "platform" | "function" | "capslock"
        )
    {
        return None;
    }
    let mods = keystroke.modifiers;
    let mut parts: Vec<&str> = Vec::new();
    for (flag, name) in [
        (mods.control, "ctrl"),
        (mods.alt, "alt"),
        (mods.shift, "shift"),
        (mods.platform, "cmd"),
    ] {
        if flag {
            parts.push(name);
        }
    }
    parts.push(key);
    Some(parts.join("-"))
}

// ---------- 改键时的检查 ----------

/// 设置页录到一个键以后，能不能用它。
#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    /// 能用
    Ok,
    /// 不能用：原因
    Refused(&'static str),
    /// 不能用：这个键是不能改的那一项（终端里的复制、粘贴……）在用
    Fixed(String),
    /// 别的操作在用：可以替换（从那一项去掉）
    Taken(String),
    /// 能用，但终端里的程序也用这个键：它做什么
    TerminalUses(&'static str),
}

/// `id` 这一项想用 `keys`（GPUI 的写法）：能不能用。`keymap` 是现在的设置。
pub fn check(
    id: &str,
    keys: &str,
    entries: &[Entry],
    keymap: &BTreeMap<String, Vec<String>>,
) -> Check {
    let (mods, key) = split(keys);
    let function_key = key.len() > 1 && key.starts_with('f') && key[1..].parse::<u8>().is_ok();
    if !mods.control && !mods.alt && !mods.platform && !function_key {
        return Check::Refused(tr("需包含 Ctrl 或 Alt（F1–F12 除外）"));
    }
    if let Some(other) = entries
        .iter()
        .find(|entry| entry.id != id && keys_of(entry, keymap).iter().any(|k| k == keys))
    {
        return match other.editable() {
            true => Check::Taken(other.id.clone()),
            false => Check::Fixed(other.id.clone()),
        };
    }
    let keeps = entries
        .iter()
        .find(|entry| entry.id == id)
        .is_some_and(|entry| entry.scope == InTerminal::Keep);
    match terminal_use(keys) {
        Some(what) if keeps => Check::TerminalUses(what),
        _ => Check::Ok,
    }
}

/// 终端里的程序常用这个键做什么（shell 的行编辑、程序的按键）；不常用是 None。键盘图里标出来。
pub fn terminal_use(keys: &str) -> Option<&'static str> {
    let (mods, key) = split(keys);
    let letter = key.len() == 1 && key.chars().all(|c| c.is_ascii_alphabetic());
    if mods.alt && !mods.control && !mods.shift && (letter || key == ".") {
        return Some(tr("按词移动、删除"));
    }
    if !(mods.control && !mods.alt && !mods.shift && (letter || key == "\\")) {
        return None;
    }
    Some(match key.as_str() {
        "a" => tr("移到行首"),
        "b" => tr("后退一个字符，也是 tmux 的前缀键"),
        "c" => tr("中断"),
        "d" => tr("退出"),
        "e" => tr("移到行尾"),
        "f" => tr("前进一个字符"),
        "g" => tr("取消当前输入"),
        "h" => tr("退格"),
        "j" => tr("换行"),
        "k" => tr("删除到行尾"),
        "l" => tr("清屏"),
        "n" => tr("下一条命令"),
        "p" => tr("上一条命令"),
        "q" => tr("恢复输出"),
        "r" => tr("搜索历史"),
        "s" => tr("暂停输出"),
        "t" => tr("交换字符"),
        "u" => tr("删除到行首"),
        "w" => tr("删除一个词"),
        "x" => tr("Emacs 的前缀键、nano 的退出键"),
        "y" => tr("粘贴删除的内容"),
        "z" => tr("挂起"),
        "\\" => tr("强制退出程序"),
        _ => return None,
    })
}

/// 把 `id` 这一项改成 `keys`（空的是没有键），存进设置的写法：和默认的一样就不存。
pub fn set(keymap: &mut BTreeMap<String, Vec<String>>, entry: &Entry, keys: Vec<String>) {
    if keys == entry.defaults {
        keymap.remove(&entry.id);
    } else {
        keymap.insert(entry.id.clone(), keys);
    }
}

#[cfg(test)]
mod tests {
    use super::{display, from_keystroke, shift_in, shift_out, split, terminal_use, without};
    use gpui_kit::{Keystroke, Modifiers};

    #[test]
    fn linux_上并进符号的_shift_拆出来再并回去() {
        // 按下时收到的 → 统一的写法
        assert_eq!(shift_out("ctrl-~").as_deref(), Some("ctrl-shift-`"));
        assert_eq!(shift_out("ctrl-!").as_deref(), Some("ctrl-shift-1"));
        assert_eq!(shift_out("ctrl-alt-_").as_deref(), Some("ctrl-alt-shift--"));
        assert_eq!(shift_out("ctrl-|").as_deref(), Some("ctrl-shift-\\"));
        assert_eq!(
            shift_out("ctrl-k ctrl-~").as_deref(),
            Some("ctrl-k ctrl-shift-`")
        );
        // 已经是统一的写法、字母、本来没有 Shift 的符号：不动
        assert_eq!(shift_out("ctrl-shift-`"), None);
        assert_eq!(shift_out("ctrl-shift-a"), None);
        assert_eq!(shift_out("ctrl-`"), None);
        assert_eq!(shift_out("ctrl--"), None);
        // 统一的写法 → 按下时收到的
        assert_eq!(shift_in("ctrl-shift-`").as_deref(), Some("ctrl-~"));
        assert_eq!(shift_in("ctrl-shift-1").as_deref(), Some("ctrl-!"));
        assert_eq!(shift_in("ctrl-alt-shift--").as_deref(), Some("ctrl-alt-_"));
        assert_eq!(shift_in("ctrl-shift-a"), None);
        assert_eq!(shift_in("ctrl-shift-tab"), None);
        assert_eq!(shift_in("ctrl-`"), None);
        // 来回一趟还是原样
        for keys in [
            "ctrl-shift-`",
            "ctrl-shift-0",
            "alt-shift-/",
            "ctrl-shift-=",
        ] {
            assert_eq!(shift_out(&shift_in(keys).unwrap()).as_deref(), Some(keys));
        }
    }

    #[test]
    fn 键的界面写法() {
        assert_eq!(display("ctrl-shift-`"), "Ctrl+Shift+`");
        assert_eq!(display("alt-left"), "Alt+←");
        assert_eq!(display("f5"), "F5");
        assert_eq!(display("ctrl-,"), "Ctrl+,");
        assert_eq!(display("ctrl--"), "Ctrl+-");
        assert_eq!(display("ctrl-alt-1"), "Ctrl+Alt+1");
        assert_eq!(display("ctrl-shift-pageup"), "Ctrl+Shift+PageUp");
        assert_eq!(display("ctrl-k ctrl-s"), "Ctrl+K Ctrl+S");
        assert_eq!(split("shift-insert").1, "insert");
    }

    #[test]
    fn 录到的键换成键位表的写法() {
        let keystroke = |control, alt, shift, key: &str| Keystroke {
            modifiers: Modifiers {
                control,
                alt,
                shift,
                ..Modifiers::none()
            },
            key: key.to_string(),
            key_char: None,
        };
        assert_eq!(
            from_keystroke(&keystroke(true, true, false, "1")).as_deref(),
            Some("ctrl-alt-1")
        );
        assert_eq!(
            from_keystroke(&keystroke(true, false, true, "`")).as_deref(),
            Some("ctrl-shift-`")
        );
        // 只按了修饰键
        assert_eq!(
            from_keystroke(&keystroke(true, false, false, "control")),
            None
        );
    }

    #[test]
    fn 去掉了的快捷键从提示里拿掉() {
        assert_eq!(
            without("新建终端（Ctrl+Shift+`）", "Ctrl+Shift+`"),
            "新建终端"
        );
        assert_eq!(
            without("New terminal (Ctrl+Shift+`)", "Ctrl+Shift+`"),
            "New terminal"
        );
        assert_eq!(
            without("提交历史和本地修改（Ctrl+1、Ctrl+2）", "Ctrl+1"),
            "提交历史和本地修改（Ctrl+2）"
        );
        assert_eq!(
            without("提交历史和本地修改（Ctrl+2）", "Ctrl+2"),
            "提交历史和本地修改"
        );
        assert_eq!(
            without("History and changes (Ctrl+1, Ctrl+2)", "Ctrl+2"),
            "History and changes (Ctrl+1)"
        );
    }

    #[test]
    fn 终端里的程序常用的键() {
        assert!(terminal_use("ctrl-r").is_some());
        assert!(terminal_use("alt-b").is_some());
        assert_eq!(terminal_use("ctrl-shift-r"), None);
        assert_eq!(terminal_use("ctrl-alt-1"), None);
        assert!(terminal_use("ctrl-g").is_some());
        assert!(terminal_use("ctrl-\\").is_some());
        assert_eq!(terminal_use("ctrl-m"), None);
    }
}
