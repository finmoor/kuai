//! 设置 → 快捷键：上面是键盘图（看全局、找空着的键），下面分组列出全部快捷键（看清楚、改）。选定的方向是
//! 「B 分组卡片 + C 键盘图」（`docs/design/terminal-launcher/`）。两边联动：鼠标停在下面一项上，键盘图
//! 换到它那一层、圈出它的键；点键盘上用了的键，下面滚到那一项；点空着的键，就地分给一个还没有快捷键的
//! 操作；在下面录新的键时，键盘图跟着亮出按下的键，撞了别的操作标红。
//!
//! 键盘图固定在上面、下面的列表单独滚：这一块的高度按它在窗口里的位置算到窗口底，设置页自己的列表就不用
//! 再滚。录键、按键搜索时焦点在一个看不见的元素上（[`keymap::RECORD_CONTEXT`]），按什么都送到这里。

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::keymap::{self, Check, Entry, Group, InTerminal};
use crate::ui;

/// 键盘图的一层：按着哪几个修饰键。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layer {
    Ctrl,
    CtrlShift,
    Alt,
    CtrlAlt,
    Plain,
}

impl Layer {
    const ALL: [Layer; 5] = [
        Layer::Ctrl,
        Layer::CtrlShift,
        Layer::Alt,
        Layer::CtrlAlt,
        Layer::Plain,
    ];

    fn prefix(self) -> &'static str {
        match self {
            Layer::Ctrl => "ctrl-",
            Layer::CtrlShift => "ctrl-shift-",
            Layer::Alt => "alt-",
            Layer::CtrlAlt => "ctrl-alt-",
            Layer::Plain => "",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Layer::Ctrl => "Ctrl +",
            Layer::CtrlShift => "Ctrl+Shift +",
            Layer::Alt => "Alt +",
            Layer::CtrlAlt => "Ctrl+Alt +",
            Layer::Plain => tr("无修饰键"),
        }
    }

    /// 这一层按着这个修饰键（键盘图上把它点亮）
    fn holds(self, cap: &str) -> bool {
        match cap {
            "Ctrl" => matches!(self, Layer::Ctrl | Layer::CtrlShift | Layer::CtrlAlt),
            "Shift" => self == Layer::CtrlShift,
            "Alt" => matches!(self, Layer::Alt | Layer::CtrlAlt),
            _ => false,
        }
    }

    /// 一个键在键盘图的哪一层；画不出来的（带 Win、Shift+Alt……）是 None
    fn of(keys: &str) -> Option<Layer> {
        let (mods, _) = keymap::split(keys);
        if mods.platform || mods.function {
            return None;
        }
        Some(match (mods.control, mods.shift, mods.alt) {
            (true, false, false) => Layer::Ctrl,
            (true, true, false) => Layer::CtrlShift,
            (false, false, true) => Layer::Alt,
            (true, false, true) => Layer::CtrlAlt,
            (false, false, false) => Layer::Plain,
            _ => return None,
        })
    }
}

/// 键盘图的一个键：键名（GPUI 的写法，空的是空位）、键上的字、几格宽。修饰键的键名是空的、字是
/// `Ctrl` 这些（见 [`MODIFIERS`]）。每行一共 15 格。
type KeyCap = (&'static str, &'static str, f32);

const MODIFIERS: [&str; 5] = ["Ctrl", "Shift", "Alt", "Win", "Caps"];

const ROWS: [&[KeyCap]; 6] = [
    &[
        ("escape", "Esc", 1.),
        ("", "", 1.),
        ("f1", "F1", 1.),
        ("f2", "F2", 1.),
        ("f3", "F3", 1.),
        ("f4", "F4", 1.),
        ("", "", 0.5),
        ("f5", "F5", 1.),
        ("f6", "F6", 1.),
        ("f7", "F7", 1.),
        ("f8", "F8", 1.),
        ("", "", 0.5),
        ("f9", "F9", 1.),
        ("f10", "F10", 1.),
        ("f11", "F11", 1.),
        ("f12", "F12", 1.),
    ],
    &[
        ("`", "`", 1.),
        ("1", "1", 1.),
        ("2", "2", 1.),
        ("3", "3", 1.),
        ("4", "4", 1.),
        ("5", "5", 1.),
        ("6", "6", 1.),
        ("7", "7", 1.),
        ("8", "8", 1.),
        ("9", "9", 1.),
        ("0", "0", 1.),
        ("-", "-", 1.),
        ("=", "=", 1.),
        ("backspace", "Backspace", 2.),
    ],
    &[
        ("tab", "Tab", 1.5),
        ("q", "Q", 1.),
        ("w", "W", 1.),
        ("e", "E", 1.),
        ("r", "R", 1.),
        ("t", "T", 1.),
        ("y", "Y", 1.),
        ("u", "U", 1.),
        ("i", "I", 1.),
        ("o", "O", 1.),
        ("p", "P", 1.),
        ("[", "[", 1.),
        ("]", "]", 1.),
        ("\\", "\\", 1.5),
    ],
    &[
        ("", "Caps", 1.75),
        ("a", "A", 1.),
        ("s", "S", 1.),
        ("d", "D", 1.),
        ("f", "F", 1.),
        ("g", "G", 1.),
        ("h", "H", 1.),
        ("j", "J", 1.),
        ("k", "K", 1.),
        ("l", "L", 1.),
        (";", ";", 1.),
        ("'", "'", 1.),
        ("enter", "Enter", 2.25),
    ],
    &[
        ("", "Shift", 2.25),
        ("z", "Z", 1.),
        ("x", "X", 1.),
        ("c", "C", 1.),
        ("v", "V", 1.),
        ("b", "B", 1.),
        ("n", "N", 1.),
        ("m", "M", 1.),
        (",", ",", 1.),
        (".", ".", 1.),
        ("/", "/", 1.),
        ("", "Shift", 2.75),
    ],
    &[
        ("", "Ctrl", 1.25),
        ("", "Win", 1.25),
        ("", "Alt", 1.25),
        ("space", "Space", 4.75),
        ("left", "←", 1.),
        ("up", "↑", 1.),
        ("down", "↓", 1.),
        ("right", "→", 1.),
        ("", "Alt", 1.25),
        ("", "Ctrl", 1.25),
    ],
];
const ROW_UNITS: f32 = 15.;

/// 正在给哪一项录新的键、按下了什么（还没按是 None）。
struct Recording {
    id: String,
    keys: Option<String>,
}

/// 下面列表里的一行：组的标题，或者第几项。
#[derive(Clone, PartialEq)]
enum Item {
    Head(Group),
    Entry(usize),
}

pub struct KeysPage {
    app: Entity<AppState>,
    search: Entity<InputState>,
    /// 录键、按键搜索时接按键（见模块说明）
    focus: FocusHandle,
    list: ScrollHandle,
    /// 这一块的上边在窗口里的位置（画的时候量），用来算高度
    top: Rc<Cell<f32>>,
    /// 按键搜索：直接按组合键，查它被谁占了
    key_search: bool,
    key_query: Option<String>,
    only_changed: bool,
    layer: Layer,
    folded: bool,
    /// 鼠标停在下面哪一项上（键盘图圈出它的键）
    hover: Option<String>,
    /// 键盘图上点了的空键：就地分给一个操作
    free: Option<String>,
    recording: Option<Recording>,
    /// 刚跳到、刚分到键的那一项：闪一下
    flash: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl KeysPage {
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr("搜索操作或按键")));
        let focus = cx.focus_handle();
        let subscriptions = vec![
            cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.observe(&app, |_, _, cx| cx.notify()),
        ];
        Self {
            app,
            search,
            focus,
            list: ScrollHandle::new(),
            top: Rc::new(Cell::new(150.)),
            key_search: false,
            key_query: None,
            only_changed: false,
            layer: Layer::Ctrl,
            folded: false,
            hover: None,
            free: None,
            recording: None,
            flash: None,
            _subscriptions: subscriptions,
        }
    }

    fn keymap(&self, cx: &App) -> BTreeMap<String, Vec<String>> {
        self.app.read(cx).settings.keymap.clone()
    }

    /// 下面列表里显示哪些：按组排，组里没有符合条件的就不列这一组。
    fn items(
        &self,
        entries: &[Entry],
        keymap: &BTreeMap<String, Vec<String>>,
        cx: &App,
    ) -> Vec<Item> {
        let query = self.search.read(cx).value().trim().to_lowercase();
        let visible = |entry: &Entry| {
            let keys = keymap::keys_of(entry, keymap);
            if self.only_changed && !keymap::changed(entry, keymap) {
                return false;
            }
            if self.key_search {
                return self
                    .key_query
                    .as_ref()
                    .is_none_or(|wanted| keys.iter().any(|keys| keys == wanted));
            }
            query.is_empty()
                || entry.name.to_lowercase().contains(&query)
                || entry.desc.to_lowercase().contains(&query)
                || keys
                    .iter()
                    .any(|keys| keymap::display(keys).to_lowercase().contains(&query))
        };
        let mut items = Vec::new();
        for group in Group::ALL {
            let shown: Vec<usize> = entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.group == group && visible(entry))
                .map(|(index, _)| index)
                .collect();
            if shown.is_empty() {
                continue;
            }
            items.push(Item::Head(group));
            items.extend(shown.into_iter().map(Item::Entry));
        }
        items
    }

    // ---------- 操作 ----------

    fn start_recording(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.free = None;
        self.recording = Some(Recording { id, keys: None });
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn stop_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.recording = None;
        // 焦点留在录制的地方，全局的快捷键都被它盖住了
        window.blur(cx);
        cx.notify();
    }

    /// 把录到的键用上。`replace`：别的操作在用的，从那一项去掉。
    fn commit(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Recording {
            id,
            keys: Some(keys),
        }) = self.recording.take()
        else {
            return;
        };
        let entries = keymap::entries(cx);
        let current = self.keymap(cx);
        let check = keymap::check(&id, &keys, &entries, &current);
        let taken = match &check {
            Check::Refused(_) | Check::Fixed(_) => {
                self.recording = Some(Recording {
                    id,
                    keys: Some(keys),
                });
                return;
            }
            Check::Taken(_) if !replace => {
                self.recording = Some(Recording {
                    id,
                    keys: Some(keys),
                });
                return;
            }
            Check::Taken(other) => Some(other.clone()),
            Check::Ok | Check::TerminalUses(_) => None,
        };
        let (me, other) = (
            entries.iter().find(|entry| entry.id == id).cloned(),
            taken.and_then(|other| entries.iter().find(|entry| entry.id == other).cloned()),
        );
        let Some(me) = me else {
            return;
        };
        let new_keys = keys.clone();
        self.app.update(cx, |app, cx| {
            app.update_settings(cx, |settings| {
                if let Some(other) = &other {
                    let rest: Vec<String> = keymap::keys_of(other, &settings.keymap)
                        .into_iter()
                        .filter(|k| *k != new_keys)
                        .collect();
                    keymap::set(&mut settings.keymap, other, rest);
                }
                keymap::set(&mut settings.keymap, &me, vec![new_keys]);
            })
        });
        self.flash_entry(id, cx);
        self.stop_recording(window, cx);
    }

    /// 这一项改成没有键（录键时按 Backspace、点「清除」）。
    fn clear(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let entries = keymap::entries(cx);
        if let Some(entry) = entries.iter().find(|entry| entry.id == id).cloned() {
            self.app.update(cx, |app, cx| {
                app.update_settings(cx, |settings| {
                    keymap::set(&mut settings.keymap, &entry, Vec::new())
                })
            });
        }
        self.stop_recording(window, cx);
    }

    fn reset(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let id = id.to_string();
        self.app.update(cx, |app, cx| {
            app.update_settings(cx, |settings| {
                settings.keymap.remove(&id);
            })
        });
        self.stop_recording(window, cx);
    }

    /// 全部恢复默认：改过的键一下子都没了、没法撤销，先问一下（红按钮，回车不执行）。
    fn reset_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.keymap(cx);
        let count = keymap::entries(cx)
            .iter()
            .filter(|entry| keymap::changed(entry, &current))
            .count();
        let (app, page) = (self.app.clone(), cx.entity().downgrade());
        crate::manager::confirm_in(
            tf!("将 {count} 项快捷键恢复为默认？", count),
            tr("修改过的快捷键将全部恢复为默认设置，此操作无法撤销。").to_string(),
            tr("全部恢复默认"),
            true,
            window,
            cx,
            move |window, cx| {
                app.update(cx, |app, cx| {
                    app.update_settings(cx, |settings| settings.keymap.clear())
                });
                page.update(cx, |page, cx| {
                    page.free = None;
                    page.stop_recording(window, cx);
                })
                .ok();
            },
        );
    }

    /// 键盘图上点的空键分给 `id` 这一项。
    fn assign(&mut self, id: String, keys: String, window: &mut Window, cx: &mut Context<Self>) {
        let entries = keymap::entries(cx);
        let Some(entry) = entries.iter().find(|entry| entry.id == id).cloned() else {
            return;
        };
        self.app.update(cx, |app, cx| {
            app.update_settings(cx, |settings| {
                keymap::set(&mut settings.keymap, &entry, vec![keys])
            })
        });
        self.free = None;
        self.jump(id, window, cx);
    }

    /// 下面的列表滚到这一项、闪一下。
    fn jump(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let entries = keymap::entries(cx);
        let current = self.keymap(cx);
        let position = |page: &Self, cx: &App| {
            page.items(&entries, &current, cx)
                .iter()
                .position(|item| matches!(item, Item::Entry(index) if entries[*index].id == id))
        };
        // 搜索、按键搜索、只看已改的把它滤掉了：先放开
        let filtered = position(self, cx).is_none();
        if filtered {
            self.only_changed = false;
            self.key_search = false;
            self.key_query = None;
            self.search
                .update(cx, |search, cx| search.set_value("", window, cx));
        }
        if let Some(position) = position(self, cx) {
            // 整行已经看得见就不滚，只闪一下；不然滚到列表上方（只滚到刚好露出会停在最底边，
            // 像没找到），是组里第一项时连组标题一起露出来。刚放开过滤时上一帧的位置对不上，直接滚
            let shown = !filtered
                && self.list.bounds_for_item(position).is_some_and(|row| {
                    let (view, offset) = (self.list.bounds(), self.list.offset());
                    row.top() + offset.y >= view.top() && row.bottom() + offset.y <= view.bottom()
                });
            if !shown {
                let items = self.items(&entries, &current, cx);
                let top = match position.checked_sub(1).and_then(|before| items.get(before)) {
                    Some(Item::Head(_)) => position - 1,
                    _ => position,
                };
                self.list.scroll_to_top_of_item(top);
            }
        }
        self.flash_entry(id, cx);
    }

    fn flash_entry(&mut self, id: String, cx: &mut Context<Self>) {
        self.flash = Some(id.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1600))
                .await;
            this.update(cx, |this, cx| {
                if this.flash.as_deref() == Some(id.as_str()) {
                    this.flash = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// 录键、按键搜索时按下的键。
    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let mods = keystroke.modifiers;
        let plain = !mods.control && !mods.alt && !mods.shift && !mods.platform;
        if let Some(recording) = &mut self.recording {
            cx.stop_propagation();
            match keystroke.key.as_str() {
                "escape" if plain => self.stop_recording(window, cx),
                "enter" if plain && recording.keys.is_some() => self.commit(false, window, cx),
                "backspace" | "delete" if plain => {
                    let id = recording.id.clone();
                    self.clear(&id, window, cx);
                }
                _ => {
                    // 换成统一的写法：Windows 上 Ctrl+Shift+1 收到的是 ctrl-!，要和表里的比
                    if let Some(keys) = keymap::from_keystroke(keystroke) {
                        recording.keys = Some(keymap::canonical(&keys, cx));
                        cx.notify();
                    }
                }
            }
            return;
        }
        if self.key_search {
            cx.stop_propagation();
            if plain && keystroke.key == "escape" {
                self.key_search = false;
                self.key_query = None;
                window.blur(cx);
            } else if let Some(keys) = keymap::from_keystroke(keystroke) {
                self.key_query = Some(keymap::canonical(&keys, cx));
            }
            cx.notify();
        }
    }
}

// ---------- 画 ----------

/// 一个键的几个键帽（`Ctrl` `Shift` `` ` ``）；`big` 是录键时的大号。
/// 这个键谁在用（不能改的那些也算）。
fn owner_of<'a>(
    entries: &'a [Entry],
    keymap: &BTreeMap<String, Vec<String>>,
    keys: &str,
) -> Option<&'a Entry> {
    entries
        .iter()
        .find(|entry| keymap::keys_of(entry, keymap).iter().any(|k| k == keys))
}

/// 键盘图上一个键的悬停提示：谁在用，或者终端程序常用、未使用。
fn key_tip(entries: &[Entry], keymap: &BTreeMap<String, Vec<String>>, keys: &str) -> SharedString {
    let shown = keymap::display(keys);
    match owner_of(entries, keymap, keys) {
        Some(entry) => format!("{shown}：{}", entry.name).into(),
        None => match keymap::terminal_use(keys) {
            Some(what) => tf!("{keys}：终端程序常用（{what}）", keys = shown, what).into(),
            None => tf!("{keys}：未使用", keys = shown).into(),
        },
    }
}

fn caps(keys: &str, big: bool, cx: &App) -> Div {
    let theme = cx.theme();
    let (edge, bg, fg) = (theme.input, theme.background, theme.foreground);
    h_flex()
        .gap(px(3.))
        .children(keymap::display(keys).split('+').map(|part| {
            div()
                .flex_none()
                .min_w(px(if big { 30. } else { 20. }))
                .h(px(if big { 28. } else { 20. }))
                .px(px(if big { 9. } else { 5. }))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(if big { 6. } else { 4. }))
                .border_1()
                .border_b_2()
                .border_color(edge)
                .bg(bg)
                .text_size(px(if big { 13. } else { 11. }))
                .text_color(fg)
                .child(part.to_string())
        }))
}

/// 一项现在的键：键帽，几个键之间写「或」；没有键写「未设置」。
fn keys_view(keys: &[String], cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    if keys.is_empty() {
        return div()
            .text_xs()
            .text_color(muted)
            .child(tr("未设置"))
            .into_any_element();
    }
    let mut row = h_flex().gap(px(4.));
    for (index, keys) in keys.iter().enumerate() {
        if index > 0 {
            row = row.child(div().text_xs().text_color(muted).child(tr("或")));
        }
        row = row.child(caps(keys, false, cx));
    }
    row.into_any_element()
}

/// 每一组在键盘图上的颜色：工作区那一组颜色里挑（终端里的那些用次要文字的灰）。
fn group_color(group: Group, cx: &App) -> Hsla {
    let slot = match group {
        Group::Window => 0,
        Group::Page => 1,
        Group::NewTerminal => 4,
        Group::Hub => 2,
        Group::Git => 3,
        Group::File => 5,
        Group::Terminal => return cx.theme().muted_foreground,
    };
    ui::hue(slot, cx)
}

impl KeysPage {
    fn render_toolbar(&self, any_changed: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, accent, bg) = (theme.muted_foreground, theme.primary, theme.background);
        let search: AnyElement = match self.key_search {
            false => Input::new(&self.search)
                .small()
                .cleanable(true)
                .prefix(ui::icon_small(IconName::Search).text_color(muted))
                .into_any_element(),
            // 和小号输入框一样高（24），切换按键搜索时下面的键盘图不跳
            true => h_flex()
                .h(px(24.))
                .px(px(8.))
                .gap(px(8.))
                .rounded(px(ui::RADIUS))
                .border_1()
                .border_color(accent)
                .bg(bg)
                .text_xs()
                .text_color(muted)
                .child(ui::icon_small(IconName::Keyboard).text_color(accent))
                .child(match &self.key_query {
                    Some(keys) => h_flex()
                        .gap(px(6.))
                        .child(caps(keys, false, cx))
                        .child(tr("Esc 退出按键搜索"))
                        .into_any_element(),
                    None => div().child(tr("按下要查询的组合键…")).into_any_element(),
                })
                .into_any_element(),
        };
        h_flex()
            .gap(px(8.))
            .child(div().flex_1().min_w(px(200.)).child(search))
            .child(
                Button::new("keys-by-key")
                    .small()
                    .icon(ui::icon_small(IconName::Keyboard))
                    .label(tr("按键搜索"))
                    .selected(self.key_search)
                    .tooltip(tr("按下组合键，查看其用途"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.key_search = !this.key_search;
                        this.key_query = None;
                        this.recording = None;
                        match this.key_search {
                            true => this.focus.focus(window, cx),
                            false => window.blur(cx),
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("keys-changed")
                    .small()
                    .label(tr("仅显示已修改"))
                    .selected(self.only_changed)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.only_changed = !this.only_changed;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("keys-reset-all")
                    .small()
                    .label(tr("全部恢复默认"))
                    .disabled(!any_changed)
                    .on_click(cx.listener(|this, _, window, cx| this.reset_all(window, cx))),
            )
    }

    /// 键盘图上要看的那一层：正在录的键、点了的空键、鼠标停着的那一项的键在哪一层就看哪一层。
    fn shown_layer(&self, cx: &App) -> Layer {
        let focus = self
            .recording
            .as_ref()
            .and_then(|recording| recording.keys.clone())
            .or_else(|| self.free.clone())
            .or_else(|| {
                let id = self.hover.as_ref()?;
                let entries = keymap::entries(cx);
                let entry = entries.iter().find(|entry| entry.id == *id)?;
                keymap::keys_of(entry, &self.keymap(cx)).into_iter().next()
            });
        focus.as_deref().and_then(Layer::of).unwrap_or(self.layer)
    }

    fn render_keyboard(
        &self,
        entries: &[Entry],
        keymap: &BTreeMap<String, Vec<String>>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, muted, border, panel, bg, accent, danger, warning) = (
            theme.foreground,
            theme.muted_foreground,
            theme.border,
            theme.sidebar,
            theme.background,
            theme.primary,
            theme.danger,
            theme.warning,
        );
        let layer = self.shown_layer(cx);
        let owner = |keys: &str| owner_of(entries, keymap, keys);
        let hovered_keys: Option<String> = self.hover.as_ref().and_then(|id| {
            let entry = entries.iter().find(|entry| entry.id == *id)?;
            keymap::keys_of(entry, keymap).into_iter().next()
        });
        let recording = self.recording.as_ref().and_then(|recording| {
            let keys = recording.keys.clone()?;
            // 和录键那一行的字一个颜色：不让用的红，能替换的黄
            let tone = match keymap::check(&recording.id, &keys, entries, keymap) {
                Check::Ok | Check::TerminalUses(_) => accent,
                Check::Taken(_) => warning,
                Check::Fixed(_) | Check::Refused(_) => danger,
            };
            Some((keys, tone))
        });
        let tabs = h_flex()
            .gap(px(2.))
            .children(Layer::ALL.into_iter().enumerate().map(|(ix, each)| {
                Button::new(("keys-layer", ix))
                    .xsmall()
                    .ghost()
                    .label(each.label())
                    .selected(each == layer)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.layer = each;
                        this.free = None;
                        cx.notify();
                    }))
            }));
        let swatch = |color: Hsla, fill: Hsla| {
            div()
                .flex_none()
                .size(px(10.))
                .rounded(px(3.))
                .border_1()
                .border_color(color)
                .bg(fill)
        };
        let legend = h_flex()
            // 靠右；窗口窄（比如英文、900 宽）放不下时整块换到下一行，不被挤出卡片
            .ml_auto()
            .flex_wrap()
            .gap(px(12.))
            .text_xs()
            .text_color(muted)
            .child(
                h_flex()
                    .gap(px(5.))
                    // 用了的键按分组着色（下面每组标题前的圆点是它的颜色），这里用中性的灰
                    .child(swatch(fg.opacity(0.35), fg.opacity(0.1)))
                    .child(tr("已使用")),
            )
            .child(
                h_flex()
                    .gap(px(5.))
                    .child(swatch(warning.opacity(0.5), warning.opacity(0.16)))
                    .child(tr("终端程序常用")),
            )
            .child(
                h_flex()
                    .gap(px(5.))
                    .child(swatch(border, bg))
                    .child(tr("未使用")),
            );
        let fold = h_flex()
            .id("keys-fold")
            .gap(px(4.))
            .cursor_pointer()
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(fg)
            .child(
                ui::icon_small(if self.folded {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .text_color(muted),
            )
            .child(tr("键盘图"))
            .when(self.folded, |row| {
                row.child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::NORMAL)
                        .text_color(muted)
                        .child(tr("展开查看未使用的键")),
                )
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.folded = !this.folded;
                this.free = None;
                cx.notify();
            }));
        let mut panel_view = v_flex()
            .flex_none()
            .gap(px(8.))
            .p(px(10.))
            .rounded(px(10.))
            .border_1()
            .border_color(border)
            .bg(panel)
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_x(px(10.))
                    .gap_y(px(4.))
                    .child(fold)
                    .when(!self.folded, |row| row.child(tabs).child(legend)),
            );
        if self.folded {
            return panel_view;
        }
        let mut key_ix = 0usize;
        let page = cx.entity();
        let rows = ROWS.iter().enumerate().map(|(row_ix, row)| {
            // 第二行写占用它的操作：两行字的高度要算准（界面库默认的行高放不下第二行）
            let height = if row_ix == 0 { 24. } else { 36. };
            h_flex()
                .gap(px(4.))
                .children(row.iter().map(|&(name, cap, units)| {
                    key_ix += 1;
                    let width = relative(units / ROW_UNITS);
                    let base = div()
                        .min_w_0()
                        .w(width)
                        .h(px(height))
                        .rounded(px(6.))
                        .overflow_hidden();
                    if name.is_empty() && cap.is_empty() {
                        return base.into_any_element();
                    }
                    if name.is_empty() && MODIFIERS.contains(&cap) {
                        let lit = layer.holds(cap);
                        return base
                            .border_1()
                            .border_b_2()
                            .border_color(if lit { accent } else { border })
                            .bg(if lit { accent } else { bg.opacity(0.6) })
                            .px(px(5.))
                            .py(px(3.))
                            .text_size(px(11.))
                            .text_color(if lit { gpui_kit::white() } else { muted })
                            .child(cap)
                            .into_any_element();
                    }
                    let keys = format!("{}{name}", layer.prefix());
                    let user = owner(&keys);
                    let pass = user.is_none() && keymap::terminal_use(&keys).is_some();
                    let color = user.map(|entry| group_color(entry.group, cx));
                    let mut key = base
                        // 按位置起名、不按组合键：换层时按下和松开还算同一个键，点击不会丢
                        .id(("keys-key", key_ix))
                        .flex()
                        .flex_col()
                        .justify_between()
                        .px(px(5.))
                        .py(px(3.))
                        .border_1()
                        .border_b_2()
                        .cursor_pointer()
                        .border_color(match color {
                            Some(color) => color.opacity(0.55),
                            None if pass => warning.opacity(0.45),
                            None => border,
                        })
                        .bg(match color {
                            Some(color) => color.opacity(0.16),
                            None if pass => warning.opacity(0.14),
                            None => bg,
                        })
                        .child(
                            div()
                                .text_size(px(11.))
                                .line_height(px(14.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(color.unwrap_or(fg))
                                .child(cap),
                        )
                        .when_some(user.filter(|_| row_ix > 0), |key, entry| {
                            key.child(
                                div()
                                    .text_size(px(10.))
                                    .line_height(px(12.))
                                    .text_color(fg)
                                    .truncate()
                                    .child(entry.name.clone()),
                            )
                        });
                    let highlighted = hovered_keys.as_deref() == Some(keys.as_str())
                        || self.free.as_deref() == Some(keys.as_str());
                    if highlighted {
                        key = key.border_2().border_color(accent);
                    }
                    if let Some((recorded, tone)) = &recording
                        && *recorded == keys
                    {
                        key = key.border_2().border_color(*tone).bg(tone.opacity(0.25));
                    }
                    let target = user.map(|entry| entry.id.clone());
                    let page = page.clone();
                    key.tooltip(move |window, cx| {
                        // 要显示时才按当时那一层写：鼠标从列表的一项移过来时键盘图可能刚换层，
                        // 提早写好的字会是上一层的
                        let page = page.read(cx);
                        let keys = format!("{}{name}", page.shown_layer(cx).prefix());
                        let tip = key_tip(&keymap::entries(cx), &page.keymap(cx), &keys);
                        ui::tip(tip, window, cx)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| match &target {
                        Some(id) => {
                            this.free = None;
                            this.hover = None;
                            this.jump(id.clone(), window, cx);
                        }
                        None => {
                            this.recording = None;
                            this.free = match this.free.as_deref() == Some(keys.as_str()) {
                                true => None,
                                false => Some(keys.clone()),
                            };
                            cx.notify();
                        }
                    }))
                    .into_any_element()
                }))
        });
        panel_view = panel_view
            .child(v_flex().gap(px(4.)).children(rows.collect::<Vec<_>>()))
            .child(self.render_status(entries, keymap, cx));
        panel_view
    }

    /// 键盘图下面那一行：录键时写按下的键怎么样，鼠标停在一项上时写它的键，点了空键时列能分给谁。
    fn render_status(
        &self,
        entries: &[Entry],
        keymap: &BTreeMap<String, Vec<String>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, warning, danger) = (
            theme.foreground,
            theme.muted_foreground,
            theme.warning,
            theme.danger,
        );
        // 只占一行、放不下就省略：鼠标扫过名字很长的预设时，下面的列表不跟着上下跳
        let line = |text: SharedString, color: Hsla| {
            div()
                .h(px(20.))
                .flex()
                .items_center()
                .min_w_0()
                .text_xs()
                .text_color(color)
                .child(div().min_w_0().truncate().child(text))
                .into_any_element()
        };
        if let Some(free) = &self.free {
            let candidates: Vec<&Entry> = entries
                .iter()
                .filter(|entry| entry.editable() && keymap::keys_of(entry, keymap).is_empty())
                .collect();
            let (note, tone): (SharedString, Hsla) = match keymap::terminal_use(free) {
                Some(what) => (
                    tf!(
                        "终端程序会用到此键（{what}），分配后在终端中也会触发该操作。",
                        what
                    )
                    .into(),
                    warning,
                ),
                None => (tr("分配给尚无快捷键的操作：").into(), muted),
            };
            let chips = candidates.into_iter().enumerate().map(|(ix, entry)| {
                let (id, keys) = (entry.id.clone(), free.clone());
                let icon = entry.preset.as_ref().map(|preset| {
                    match crate::presets::all(cx)
                        .into_iter()
                        .find(|p| p.id == *preset)
                    {
                        Some(p) => crate::presets::icon(&p.icon)
                            .text_color(crate::presets::color(&p.icon, p.hue, cx)),
                        None => ui::icon_small(IconName::SquareTerminal).text_color(muted),
                    }
                });
                // 名字太长的写一截，悬停看全名
                let full = entry.name.clone();
                let short: SharedString = match full.chars().count() > 20 {
                    true => format!("{}…", full.chars().take(18).collect::<String>()).into(),
                    false => full.clone(),
                };
                Button::new(("keys-assign", ix))
                    .xsmall()
                    .when_some(icon, |button, icon| button.icon(icon))
                    .when(short != full, |button| button.tooltip(full.clone()))
                    .label(short)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.assign(id.clone(), keys.clone(), window, cx)
                    }))
            });
            return v_flex()
                .gap(px(6.))
                .child(
                    h_flex()
                        .gap(px(8.))
                        .text_xs()
                        .text_color(fg)
                        .child(caps(free, false, cx))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(tr("未使用")))
                        .child(div().text_color(tone).child(note)),
                )
                .child(
                    h_flex().flex_wrap().gap(px(6.)).children(chips).child(
                        Button::new("keys-assign-cancel")
                            .xsmall()
                            .ghost()
                            .label(tr("取消"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.free = None;
                                cx.notify();
                            })),
                    ),
                )
                .into_any_element();
        }
        if let Some(Recording {
            id,
            keys: Some(keys),
        }) = &self.recording
        {
            // 录到的键怎么样：不能用的标红，能替换的、终端程序也用的标黄（和录键那一行一致）
            let shown = keymap::display(keys);
            let check = keymap::check(id, keys, entries, keymap);
            let (text, tone): (SharedString, Hsla) = match &check {
                Check::Taken(other) | Check::Fixed(other) => {
                    let name = entries
                        .iter()
                        .find(|entry| entry.id == *other)
                        .map(|entry| entry.name.clone())
                        .unwrap_or_default();
                    let tone = match check {
                        Check::Taken(_) => warning,
                        _ => danger,
                    };
                    (
                        tf!("{keys} · 已用于「{name}」", keys = shown, name).into(),
                        tone,
                    )
                }
                Check::Refused(reason) => (format!("{shown} · {reason}").into(), danger),
                Check::TerminalUses(what) => (
                    tf!("{keys}：终端程序常用（{what}）", keys = shown, what).into(),
                    warning,
                ),
                Check::Ok => (tf!("{keys} · 未使用", keys = shown).into(), muted),
            };
            return line(text, tone);
        }
        if let Some(id) = &self.hover
            && let Some(entry) = entries.iter().find(|entry| entry.id == *id)
            && let Some(keys) = keymap::keys_of(entry, keymap).first()
        {
            let off_map = Layer::of(keys).is_none()
                || !ROWS.iter().any(|row| {
                    row.iter()
                        .any(|cap| !cap.0.is_empty() && keymap::split(keys).1 == cap.0)
                });
            let text = match off_map {
                true => tf!(
                    "{keys} · {name}（键盘图中未显示此键）",
                    keys = keymap::display(keys),
                    name = entry.name.clone()
                ),
                false => format!("{} · {}", keymap::display(keys), entry.name),
            };
            return line(text.into(), muted);
        }
        line(
            tr("单击已使用的键跳到对应操作，单击未使用的键可直接分配。").into(),
            muted,
        )
    }

    fn render_row(
        &self,
        position: usize,
        index: usize,
        (first, last): (bool, bool),
        entries: &[Entry],
        keymap: &BTreeMap<String, Vec<String>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entry = &entries[index];
        let theme = cx.theme();
        let (fg, muted, border, bg, accent, hover_bg, danger, warning, success) = (
            theme.foreground,
            theme.muted_foreground,
            theme.border,
            theme.background,
            theme.primary,
            theme.list_hover,
            theme.danger,
            theme.warning,
            theme.success,
        );
        let keys = keymap::keys_of(entry, keymap);
        let changed = keymap::changed(entry, keymap);
        let recording = self
            .recording
            .as_ref()
            .filter(|recording| recording.id == entry.id);
        // 一组都是终端里可用的：写在组标题后面，这里不再每行写
        let all_keep = entries
            .iter()
            .filter(|other| other.group == entry.group)
            .all(|other| other.scope == InTerminal::Keep);
        let scope = match (entry.scope, all_keep) {
            (_, true) => "",
            (InTerminal::Keep, false) => tr("终端中可用"),
            (InTerminal::Pass, false) => tr("终端中由程序处理"),
            (InTerminal::Fixed, false) => "",
        };
        let desc: Vec<SharedString> = [entry.desc.clone(), scope.into()]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect();
        let desc = desc.join(" · ");
        let icon = entry.preset.as_ref().map(|preset| {
            match crate::presets::all(cx)
                .into_iter()
                .find(|p| p.id == *preset)
            {
                Some(p) => crate::presets::icon(&p.icon)
                    .size_4()
                    .text_color(crate::presets::color(&p.icon, p.hue, cx)),
                None => ui::icon(IconName::SquareTerminal).text_color(muted),
            }
        });
        let id = entry.id.clone();
        let chip: AnyElement = match (entry.editable(), recording) {
            (false, _) => div()
                .id(("keys-chip", position))
                .flex_none()
                .min_h(px(30.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(px(7.))
                .border_1()
                .border_dashed()
                .border_color(border)
                .child(keys_view(&keys, cx))
                .tooltip(|window, cx| ui::tip(tr("由终端或输入框处理，不可修改"), window, cx))
                .into_any_element(),
            (true, Some(recording)) => h_flex()
                .flex_none()
                .min_h(px(30.))
                .px(px(8.))
                .gap(px(6.))
                .rounded(px(7.))
                .border_1()
                .border_color(accent)
                .bg(accent.opacity(0.08))
                .child(div().size(px(7.)).rounded_full().bg(accent))
                .child(match &recording.keys {
                    Some(keys) => caps(keys, false, cx).into_any_element(),
                    None => div()
                        .text_xs()
                        .text_color(accent)
                        .child(tr("按下组合键…"))
                        .into_any_element(),
                })
                .into_any_element(),
            (true, None) => {
                let id = id.clone();
                div()
                    .id(("keys-chip", position))
                    .flex_none()
                    .min_h(px(30.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(px(7.))
                    .border_1()
                    .border_color(border)
                    .bg(bg)
                    .cursor_pointer()
                    .hover(|style| style.border_color(accent.opacity(0.6)))
                    .child(keys_view(&keys, cx))
                    .tooltip(|window, cx| ui::tip(tr("点击修改"), window, cx))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_recording(id.clone(), window, cx)
                    }))
                    .into_any_element()
            }
        };
        let reset = (changed && recording.is_none()).then(|| {
            let id = id.clone();
            let default = match entry.defaults.first() {
                Some(keys) => keymap::display(keys),
                None => tr("未设置").to_string(),
            };
            Button::new(("keys-reset", position))
                .xsmall()
                .ghost()
                .icon(ui::icon_small(IconName::Undo2))
                .tooltip(tf!("恢复默认（{keys}）", keys = default))
                .on_click(cx.listener(move |this, _, window, cx| this.reset(&id, window, cx)))
        });
        let flashing = self.flash.as_deref() == Some(entry.id.as_str());
        let lit = self.hover.as_deref() == Some(entry.id.as_str());
        let hover_id = entry.id.clone();
        let row = h_flex()
            .id(("keys-row", position))
            .px(px(14.))
            .py(px(9.))
            .gap(px(16.))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        h_flex()
                            .gap(px(7.))
                            .text_sm()
                            .text_color(fg)
                            .children(icon)
                            // 预设的名字可能很长：放不下就省略，悬停看全文
                            .child(ui::clipped(("keys-name", position), entry.name.clone()))
                            .when(changed, |row| {
                                row.child(div().flex_none().size(px(6.)).rounded_full().bg(accent))
                            }),
                    )
                    .when(!desc.is_empty(), |col| {
                        col.child(div().text_xs().text_color(muted).child(desc))
                    }),
            )
            .child(h_flex().flex_none().gap(px(4.)).children(reset).child(chip))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if this.recording.is_some() || this.free.is_some() {
                    return;
                }
                match *hovered {
                    true => this.hover = Some(hover_id.clone()),
                    false if this.hover.as_deref() == Some(hover_id.as_str()) => this.hover = None,
                    false => return,
                }
                cx.notify();
            }));
        let message = recording.map(|recording| {
            self.render_recorder(
                entry,
                recording,
                entries,
                keymap,
                changed,
                (danger, warning, success, muted),
                cx,
            )
        });
        v_flex()
            .id(("keys-item", position))
            .border_x_1()
            .border_color(border)
            .bg(if flashing {
                accent.opacity(0.12)
            } else if lit {
                hover_bg
            } else {
                bg
            })
            .when(first, |item| item.border_t_1().rounded_t(px(10.)))
            .when(!first, |item| item.border_t_1())
            .when(last, |item| item.border_b_1().rounded_b(px(10.)))
            .child(row)
            .children(message)
            .into_any_element()
    }

    /// 录键那一行下面：按下的键能不能用、撞了谁，确定、替换、清除、恢复默认、取消。
    #[allow(clippy::too_many_arguments)]
    fn render_recorder(
        &self,
        entry: &Entry,
        recording: &Recording,
        entries: &[Entry],
        keymap: &BTreeMap<String, Vec<String>>,
        changed: bool,
        (danger, warning, success, muted): (Hsla, Hsla, Hsla, Hsla),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (text, tone, primary): (SharedString, Hsla, Option<(&'static str, bool)>) =
            match &recording.keys {
                None => (
                    tr("请按下新的组合键：Enter 确定，Esc 取消，Backspace 清除。").into(),
                    muted,
                    None,
                ),
                Some(keys) => match keymap::check(&entry.id, keys, entries, keymap) {
                    Check::Ok => (
                        tr("按 Enter 确定，Esc 取消。").into(),
                        success,
                        Some((tr("确定"), false)),
                    ),
                    Check::TerminalUses(what) => (
                        tf!(
                            "终端程序会用到 {keys}（{what}），改用后在终端中也会触发此操作。",
                            keys = keymap::display(keys),
                            what
                        )
                        .into(),
                        warning,
                        Some((tr("确定"), false)),
                    ),
                    Check::Taken(other) => {
                        let name = entries
                            .iter()
                            .find(|e| e.id == other)
                            .map(|e| e.name.clone())
                            .unwrap_or_default();
                        // 能替换，所以是黄字；红字只给不让用的
                        (
                            tf!("此键已用于「{name}」，替换后将从该操作中移除。", name).into(),
                            warning,
                            Some((tr("替换"), true)),
                        )
                    }
                    Check::Fixed(other) => {
                        let name = entries
                            .iter()
                            .find(|e| e.id == other)
                            .map(|e| e.name.clone())
                            .unwrap_or_default();
                        (
                            tf!("此键已用于「{name}」，不可占用。", name).into(),
                            danger,
                            None,
                        )
                    }
                    Check::Refused(reason) => (reason.into(), danger, None),
                },
            };
        let id = entry.id.clone();
        h_flex()
            .px(px(14.))
            .pb(px(10.))
            .gap(px(8.))
            .flex_wrap()
            .child(
                div()
                    .flex_1()
                    .min_w(px(200.))
                    .text_xs()
                    .text_color(tone)
                    .child(text),
            )
            .child(
                Button::new("keys-rec-clear")
                    .xsmall()
                    .label(tr("清除"))
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _, window, cx| this.clear(&id, window, cx)
                    })),
            )
            .when(changed, |row| {
                let id = id.clone();
                row.child(
                    Button::new("keys-rec-reset")
                        .xsmall()
                        .label(tr("恢复默认"))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.reset(&id, window, cx)),
                        ),
                )
            })
            .child(
                Button::new("keys-rec-cancel")
                    .xsmall()
                    .label(tr("取消"))
                    .on_click(cx.listener(|this, _, window, cx| this.stop_recording(window, cx))),
            )
            .when_some(primary, |row, (label, replace)| {
                row.child(
                    Button::new("keys-rec-ok")
                        .xsmall()
                        .primary()
                        .label(label)
                        .on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.commit(replace, window, cx)
                            }),
                        ),
                )
            })
            .into_any_element()
    }
}

impl Render for KeysPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = keymap::entries(cx);
        let current = self.keymap(cx);
        let any_changed = entries.iter().any(|entry| keymap::changed(entry, &current));
        let items = self.items(&entries, &current, cx);
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        // 高度：从这一块的上边到窗口底（设置页的列表下边还留着 16 的空）
        let viewport = f32::from(window.viewport_size().height);
        let height = (viewport - self.top.get() - 20.).max(360.);
        let mut rows: Vec<AnyElement> = Vec::new();
        for (position, item) in items.iter().enumerate() {
            match item {
                Item::Head(group) => {
                    let all_keep = entries
                        .iter()
                        .filter(|entry| entry.group == *group)
                        .all(|entry| entry.scope == InTerminal::Keep);
                    let lead: Vec<&str> = [group.lead(), all_keep.then(|| tr("终端中也可用"))]
                        .into_iter()
                        .flatten()
                        .collect();
                    rows.push(
                        h_flex()
                            .pt(px(if position == 0 { 2. } else { 16. }))
                            .pb(px(6.))
                            .gap(px(8.))
                            // 这一组的键在键盘图上的颜色
                            .child(
                                div()
                                    .flex_none()
                                    .size(px(8.))
                                    .rounded_full()
                                    .bg(group_color(*group, cx)),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(fg)
                                    .child(group.title()),
                            )
                            .when(!lead.is_empty(), |row| {
                                row.child(div().text_xs().text_color(muted).child(lead.join(" · ")))
                            })
                            .into_any_element(),
                    );
                }
                Item::Entry(index) => {
                    let first = matches!(
                        items.get(position.wrapping_sub(1)),
                        Some(Item::Head(_)) | None
                    );
                    let last = !matches!(items.get(position + 1), Some(Item::Entry(_)));
                    rows.push(self.render_row(
                        position,
                        *index,
                        (first, last),
                        &entries,
                        &current,
                        cx,
                    ));
                }
            }
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .py(px(24.))
                    .text_sm()
                    .text_color(muted)
                    .child(tr("没有符合条件的快捷键"))
                    .into_any_element(),
            );
        }
        let top = self.top.clone();
        v_flex()
            .relative()
            .w_full()
            .h(px(height))
            .gap(px(12.))
            .child(
                // 量一下这一块在窗口里的上边：高度按它算到窗口底
                canvas(
                    move |bounds, window, _| {
                        let y = f32::from(bounds.origin.y);
                        if (y - top.get()).abs() > 0.5 {
                            top.set(y);
                            // 画的时候不能直接重画：下一帧按量到的位置重算高度
                            window.request_animation_frame();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                // 录键、按键搜索时接按键的地方
                div()
                    .absolute()
                    .size_0()
                    .track_focus(&self.focus)
                    .key_context(keymap::RECORD_CONTEXT)
                    .on_key_down(cx.listener(Self::on_key)),
            )
            .child(self.render_toolbar(any_changed, cx))
            .child(self.render_keyboard(&entries, &current, cx))
            .child(
                div()
                    .id("keys-list")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .track_scroll(&self.list)
                    .pr(px(4.))
                    // 每一行直接放在滚动的这一层里：滚到第几项（`scroll_to_item`）按这一层的子元素数
                    .children(rows),
            )
    }
}
