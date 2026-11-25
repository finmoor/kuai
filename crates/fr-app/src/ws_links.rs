//! 「打开工作区」：从会话页、仓库页、标签页的右键菜单到工作区页，选中包含这个仓库的工作区，这个
//! 仓库那一行标出来（设计见 `docs/design/back-and-workspace/`）。
//!
//! 仓库在几个工作区里时列出来选（会话页里它列在哪个工作区下面、从哪个工作区打开的，排第一），下面
//! 「添加到其他工作区」列还没放它的；一个都不在时是「添加到工作区」。只在一个里时按钮直接去。

use std::rc::Rc;

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::h_flex;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::machines::WorkspaceKey;
use crate::settings::RepoEntry;
use crate::ui;

/// 把仓库放进工作区。会话页的要顺带展开那一节、选中它（见 `TermOverview::add_to_workspace`），
/// 别处直接放（[`plain_add`]）。
pub(crate) type AddTo = Rc<dyn Fn(WorkspaceKey, RepoEntry, &mut Window, &mut App)>;

/// 直接放进去，别的不动。
pub(crate) fn plain_add(app: &Entity<AppState>) -> AddTo {
    let app = app.downgrade();
    Rc::new(move |key, entry, _, cx| {
        app.update(cx, |app, cx| {
            app.add_to_workspace(&key, &entry, cx);
        })
        .ok();
    })
}

/// 工作区 `key` 里有没有这个仓库（子仓库算它所在的仓库的）。
fn holds(app: &AppState, key: &WorkspaceKey, entry: &RepoEntry) -> bool {
    app.members(key)
        .iter()
        .any(|member| member.same_repo(entry) || member.contains(entry))
}

/// 包含这个仓库的工作区（读到了的电脑上；子仓库算它所在的仓库的）。`first` 排第一，其余按电脑的
/// 先后（本机在前，各台主机按主机页的先后）、各自工作区的先后。
pub(crate) fn homes(
    app: &AppState,
    entry: &RepoEntry,
    first: Option<&WorkspaceKey>,
) -> Vec<WorkspaceKey> {
    let mut keys: Vec<WorkspaceKey> = app
        .machine_keys()
        .iter()
        .flat_map(|machine| app.workspaces_on(machine).unwrap_or_default())
        .map(|(key, _, _)| key)
        .filter(|key| holds(app, key, entry))
        .collect();
    if let Some(at) = first.and_then(|first| keys.iter().position(|key| key == first)) {
        let key = keys.remove(at);
        keys.insert(0, key);
    }
    keys
}

/// 还能放进去的工作区：那台电脑连着、访问得到这个仓库（本机的工作区能放各台主机上的，主机上的
/// 工作区能放它自己和它配置的主机上的），还没放它的。子仓库不放：工作区页在仓库下面列出它的子仓库。
pub(crate) fn addable(app: &AppState, entry: &RepoEntry) -> Vec<WorkspaceKey> {
    if app.root_of(entry).is_some() {
        return Vec::new();
    }
    let ids = app.ids();
    let mut keys = Vec::new();
    for machine in app.machine_keys() {
        if !app.writable(&machine) || ids.member_ref(&machine, entry).is_none() {
            continue;
        }
        for (key, _, _) in app.workspaces_on(&machine).unwrap_or_default() {
            if !holds(app, &key, entry) {
                keys.push(key);
            }
        }
    }
    keys
}

/// 到工作区页，选中工作区 `key`，这个仓库那一行标出来。
pub(crate) fn open(key: WorkspaceKey, entry: RepoEntry, window: &mut Window, cx: &mut App) {
    crate::workspace::go_to_workspace(key, Some(entry), window, cx);
}

/// 菜单里一个工作区那一行要画的：色块、名字，行尾淡淡写在哪台（和几个仓库）。
#[derive(Clone)]
struct Row {
    key: WorkspaceKey,
    color: Hsla,
    name: SharedString,
    aside: SharedString,
}

fn rows(app: &AppState, keys: Vec<WorkspaceKey>, count: bool, cx: &App) -> Vec<Row> {
    keys.into_iter()
        .map(|key| {
            let place: SharedString = match key.is_local() {
                true => tr("本机").into(),
                false => app.machine_name(&key.machine),
            };
            let aside = match count {
                true => tf!("{place} · {n} 个仓库", place, n = app.members(&key).len()).into(),
                false => place,
            };
            Row {
                color: ui::hue(app.workspace_hue(&key), cx),
                name: app.workspace_name(&key),
                aside,
                key,
            }
        })
        .collect()
}

fn item(
    row: Row,
    muted: Hsla,
    on_click: impl Fn(WorkspaceKey, &mut Window, &mut App) + 'static,
) -> PopupMenuItem {
    let Row {
        key,
        color,
        name,
        aside,
    } = row;
    PopupMenuItem::element(move |_, _| {
        h_flex()
            .w_full()
            .gap(px(8.))
            .child(ui::hue_mark(color, 8.))
            .child(
                div()
                    .flex_shrink(1.)
                    .min_w_0()
                    .truncate()
                    .child(name.clone()),
            )
            .child(div().flex_1().min_w(px(16.)))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(muted)
                    .child(aside.clone()),
            )
    })
    .on_click(move |_, window, cx| on_click(key.clone(), window, cx))
}

/// 一组「添加到…」的项：点了放进去。下面是「新建工作区并添加…」（`app` 给了才有）和一句工作区是
/// 什么。
fn add_items(
    menu: PopupMenu,
    app: &Entity<AppState>,
    rows: Vec<Row>,
    entry: &RepoEntry,
    add: &AddTo,
    muted: Hsla,
) -> PopupMenu {
    let menu = match rows.is_empty() {
        true => menu.label(tr("没有可添加到的工作区")),
        false => rows.into_iter().fold(menu, |menu, row| {
            let (entry, add) = (entry.clone(), add.clone());
            menu.item(item(row, muted, move |key, window, cx| {
                add(key, entry.clone(), window, cx)
            }))
        }),
    };
    let (app, entry, add) = (app.clone(), entry.clone(), add.clone());
    menu.separator()
        .item(
            PopupMenuItem::new(tr("新建工作区并添加…"))
                .on_click(move |_, window, cx| new_and_add(&app, &entry, &add, window, cx)),
        )
        .label(tr("工作区是可选的分组，不影响已打开的标签页"))
}

/// 「新建工作区并添加…」：起名字、选保存在哪（本机，或仓库所在的那台主机），建好放进去。
fn new_and_add(
    app: &Entity<AppState>,
    entry: &RepoEntry,
    add: &AddTo,
    window: &mut Window,
    cx: &mut App,
) {
    let app = app.clone();
    let state = app.read(cx);
    let machine = crate::machines::machine_of(entry).to_string();
    let mut places: Vec<(String, SharedString)> =
        vec![(crate::machines::LOCAL.to_string(), tr("本机").into())];
    if machine != crate::machines::LOCAL && state.writable(&machine) {
        places.push((machine.clone(), state.machine_name(&machine)));
    }
    let default = match places.len() {
        2 => machine,
        _ => crate::machines::LOCAL.to_string(),
    };
    let (entry, add) = (entry.clone(), add.clone());
    crate::workspace::new_workspace_dialog(
        places,
        default,
        window,
        cx,
        move |name, machine, window, cx| {
            let key = app.update(cx, |app, cx| app.add_workspace(&machine, &name, cx));
            if let Some(key) = key {
                add(key, entry.clone(), window, cx);
            }
        },
    );
}

/// 「打开工作区」按钮点开的菜单：在几个工作区里时一行一个、下面「添加到其他工作区 ▸」；一个都不在
/// 时是能放进去的那几个。只在一个里时按钮直接去（见 [`button`]），不弹。
fn dropdown(
    menu: PopupMenu,
    app: &Entity<AppState>,
    entry: &RepoEntry,
    first: Option<&WorkspaceKey>,
    add: &AddTo,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let muted = cx.theme().muted_foreground;
    let (homes, more) = {
        let state = app.read(cx);
        let homes = homes(state, entry, first);
        let more = addable(state, entry);
        (rows(state, homes, true, cx), rows(state, more, false, cx))
    };
    if homes.is_empty() {
        return add_items(menu.label(tr("添加到工作区")), app, more, entry, add, muted);
    }
    let menu = homes.into_iter().fold(
        menu.label(tr("选择要打开的工作区")),
        |menu, row| {
            let entry = entry.clone();
            menu.item(item(row, muted, move |key, window, cx| {
                open(key, entry.clone(), window, cx)
            }))
        },
    );
    if more.is_empty() {
        return menu;
    }
    let (app, entry, add) = (app.clone(), entry.clone(), add.clone());
    menu.separator().submenu(
        tr("添加到其他工作区"),
        window,
        cx,
        move |sub, _, _| add_items(sub, &app, more.clone(), &entry, &add, muted),
    )
}

/// 「打开工作区」按钮：在一个工作区里直接去，几个、一个都没有时点开菜单（[`dropdown`]），`caret`
/// 时按钮上带一个向下的小箭头。`button` 是调用的地方做好样子的按钮（字、图标、大小），`anchor` 是
/// 菜单对齐按钮的哪个角。
#[allow(clippy::too_many_arguments)]
pub(crate) fn button(
    button: Button,
    app: &Entity<AppState>,
    entry: &RepoEntry,
    first: Option<WorkspaceKey>,
    add: AddTo,
    anchor: Anchor,
    caret: bool,
    cx: &App,
) -> AnyElement {
    let homes = homes(app.read(cx), entry, first.as_ref());
    if let [key] = homes.as_slice() {
        let (key, entry) = (key.clone(), entry.clone());
        return button
            .on_click(move |_, window, cx| open(key.clone(), entry.clone(), window, cx))
            .into_any_element();
    }
    let (app, entry) = (app.clone(), entry.clone());
    button
        .dropdown_caret(caret)
        .dropdown_menu_with_anchor(anchor, move |menu, window, cx| {
            dropdown(menu, &app, &entry, first.as_ref(), &add, window, cx)
        })
        .into_any_element()
}

/// 按钮上的悬停提示：去哪个工作区、在几个里、还没加入。
pub(crate) fn tip(app: &AppState, entry: &RepoEntry, first: Option<&WorkspaceKey>) -> String {
    let homes = homes(app, entry, first);
    match homes.as_slice() {
        [] => {
            tr("未加入工作区。工作区是可选的分组，用来把常用仓库放在一起，不影响已打开的标签页。")
                .to_string()
        }
        [key] => tf!("在工作区页中打开「{name}」", name = app.workspace_name(key)),
        _ => {
            let names: Vec<String> = homes
                .iter()
                .map(|key| app.workspace_name(key).to_string())
                .collect();
            tf!(
                "在 {n} 个工作区中：{names}",
                n = homes.len(),
                names = names.join(tr("、"))
            )
        }
    }
}

/// 右键菜单里的那一两项：只在一个工作区里是「打开工作区」，在几个里是「打开工作区 ▸」；`more` 时
/// 后面跟「添加到其他工作区 ▸」。一个都不在时是「添加到工作区 ▸」。
#[allow(clippy::too_many_arguments)]
pub(crate) fn context_items(
    menu: PopupMenu,
    app: &Entity<AppState>,
    entry: &RepoEntry,
    first: Option<&WorkspaceKey>,
    more: bool,
    add: &AddTo,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let muted = cx.theme().muted_foreground;
    let (homes, others) = {
        let state = app.read(cx);
        let homes = homes(state, entry, first);
        let others = addable(state, entry);
        (
            rows(state, homes, false, cx),
            rows(state, others, false, cx),
        )
    };
    // 子仓库不放进工作区（工作区页在仓库下面列出它的子仓库）：不出这一项。别的没有能加的工作区时
    // 照样有，里面是「新建工作区并添加…」
    let sub_repo = app.read(cx).root_of(entry).is_some();
    let add_sub = |menu: PopupMenu, label: &'static str, window: &mut Window, cx| {
        if sub_repo {
            return menu;
        }
        let (app, entry, add, others) = (app.clone(), entry.clone(), add.clone(), others.clone());
        menu.submenu(label, window, cx, move |sub, _, _| {
            add_items(sub, &app, others.clone(), &entry, &add, muted)
        })
    };
    let menu = match homes.len() {
        0 => return add_sub(menu, tr("添加到工作区"), window, cx),
        1 => {
            let key = homes[0].key.clone();
            let entry = entry.clone();
            menu.item(
                PopupMenuItem::new(tr("打开工作区"))
                    .on_click(move |_, window, cx| open(key.clone(), entry.clone(), window, cx)),
            )
        }
        _ => {
            let entry = entry.clone();
            menu.submenu(tr("打开工作区"), window, cx, move |sub, _, _| {
                homes.clone().into_iter().fold(sub, |sub, row| {
                    let entry = entry.clone();
                    sub.item(item(row, muted, move |key, window, cx| {
                        open(key, entry.clone(), window, cx)
                    }))
                })
            })
        }
    };
    match more {
        true => add_sub(menu, tr("添加到其他工作区"), window, cx),
        false => menu,
    }
}
