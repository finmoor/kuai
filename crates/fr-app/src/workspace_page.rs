//! 工作区页（标题栏上的「工作区」）：左边选一台电脑，列出它上面的工作区；右边是选中的工作区里的
//! 仓库，每个仓库下面列出它的子仓库。点仓库、子仓库在标签页中打开（已经开着就切过去）。设计见
//! `design/server-workspace/`（第二版）。
//!
//! 工作区只是一组常一起看的仓库：没有「当前工作区」，选中一个也不动标题栏上的标签页（标签页和
//! 选了哪台服务器、哪个工作区都无关）。右上角「全部打开」把它的仓库连同子仓库一次开成标签页；
//! 子仓库还没找完的，找完了接着开。
//!
//! 仓库那一行写分支、改动数、已打开、终端个数（点了到会话页），鼠标移上去出「新建终端」「移出工作区」；
//! 子仓库默认展开，左边的小箭头收起（记在设置里，只在这台电脑上）。行的样子见 `repo_rows`。
//! 1.16.1 左上角的工作区菜单（带「切换服务器」的那一套）由这一页代替。

use std::cell::Cell;
use std::rc::Rc;

use fr_core::i18n::{tr, trc};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::{AppState, ConnStatus};
use crate::machines::{LOCAL, WorkspaceKey, machine_of};
use crate::repo_rows::{Block, Columns, Handlers, MenuFor, render_block};
use crate::repo_scan::{RepoScan, sub_entry};
use crate::settings::RepoEntry;
use crate::term_board::{TermBoard, TermState};
use crate::ui;

/// 侧栏顶上那一条的高度（和会话页、仓库页一样）
const HEAD_HEIGHT: f32 = 52.;

pub enum PageEvent {
    /// 在标签页中打开（已经开着就切过去）
    Open(RepoEntry),
    /// 一次打开几个。`show` 为真时显示第一个；假的是子仓库找完了接着开的，只加在后面
    /// `home`：从哪个工作区开的（仓库页的工作区按钮先写它）
    OpenAll {
        entries: Vec<RepoEntry>,
        show: bool,
        home: WorkspaceKey,
    },
    /// 给仓库改显示的名字（对话框由主窗口开）
    Rename(RepoEntry),
    /// 到会话页看这个仓库的终端
    Terminals(RepoEntry),
    /// 在这个仓库里新开一个终端，运行选的这个（None 是只开 shell）
    RunInTerminal(RepoEntry, Option<crate::presets::Start>),
}

/// 新建终端的菜单里选了：让主窗口在 `entry` 里新开一个终端（到会话页显示）。
fn run_in(page: WeakEntity<WorkspacePage>, entry: RepoEntry) -> crate::presets::OnStart {
    Rc::new(move |start, _, cx| {
        let entry = entry.clone();
        page.update(cx, |_, cx| cx.emit(PageEvent::RunInTerminal(entry, start)))
            .ok();
    })
}

pub struct WorkspacePage {
    app: Entity<AppState>,
    board: Entity<TermBoard>,
    scan: Entity<RepoScan>,
    /// 点「全部打开」时子仓库还没找完的仓库（和从哪个工作区开的）：找完了接着把它们的子仓库开出来
    open_later: Vec<(RepoEntry, WorkspaceKey)>,
    /// 这一页显示着
    visible: bool,
    /// 「打开工作区」过来时要标出来的那个仓库（或子仓库）：那一行带选中的底色，滚到看得见。离开这一页、
    /// 换了工作区就不标了
    hit: Option<RepoEntry>,
    /// 标出来的那一行还没滚到看得见
    scroll_to_hit: bool,
    /// 滚到的那一行就是要标的那个（不是子仓库还没找到、收着时代它标出来的仓库那一行）
    hit_exact: bool,
    /// 右边那一栏的滚动
    scroll: ScrollHandle,
    /// 标出来的那一行上一帧画在哪（滚到它用，见 [`crate::repo_rows::Block::hit_at`]）
    hit_bounds: crate::repo_rows::HitCell,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PageEvent> for WorkspacePage {}

/// 一个仓库的终端：几个还在的，和最要紧的那个的状态（已停下比运行中要紧，运行中比空闲要紧）。
pub(crate) fn terminal_summary(board: &TermBoard, entry: &RepoEntry) -> Option<(usize, TermState)> {
    let rows = board.terms_of(entry)?;
    let alive: Vec<_> = rows.iter().filter(|row| row.alive).collect();
    let rank = |state: TermState| match state {
        TermState::Stopped => 0,
        TermState::Running => 1,
        TermState::Idle => 2,
    };
    let worst = alive.iter().map(|row| row.state).min_by_key(|s| rank(*s))?;
    Some((alive.len(), worst))
}

impl WorkspacePage {
    pub fn new(
        app: Entity<AppState>,
        board: Entity<TermBoard>,
        scan: Entity<RepoScan>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            // 工作区、仓库变了，服务器连上了：重画，显示着的话把要看的仓库再交给 `scan`
            cx.observe(&app, |this, _, cx| {
                if this.visible {
                    this.want_scan(cx);
                }
                cx.notify();
            }),
            cx.observe(&scan, |this, _, cx| {
                this.open_found_subs(cx);
                cx.notify();
            }),
            cx.observe(&board, |_, _, cx| cx.notify()),
        ];
        Self {
            app,
            board,
            scan,
            open_later: Vec::new(),
            visible: false,
            hit: None,
            scroll_to_hit: false,
            hit_exact: false,
            scroll: ScrollHandle::new(),
            hit_bounds: Rc::new(Cell::new(None)),
            _subscriptions: subscriptions,
        }
    }

    /// 选中的工作区（后退时回到它、从这一页打开的仓库记着它）。
    pub fn selected(&self, cx: &App) -> Option<WorkspaceKey> {
        self.selection(cx).1
    }

    /// 「打开工作区」：选中工作区 `key`（列它所在的那台电脑），`entry` 那一行标出来、滚到看得见。
    pub fn reveal(&mut self, key: WorkspaceKey, entry: Option<RepoEntry>, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| app.set_current(key, cx));
        self.scroll_to_hit = entry.is_some();
        self.hit_exact = false;
        self.hit = entry;
        self.hit_bounds.set(None);
        if self.visible {
            self.want_scan(cx);
        }
        cx.notify();
    }

    /// 标出来的那一行不在看得见的地方：滚过去（按上一帧画的位置算）。
    fn scroll_hit_into_view(&mut self, cx: &mut Context<Self>) {
        let Some((hit, exact)) = self.hit_bounds.get() else {
            return;
        };
        self.scroll_to_hit = false;
        self.hit_exact = exact;
        let view = self.scroll.bounds();
        let margin = px(12.);
        let was = self.scroll.offset();
        let mut y = was.y;
        // 在下面看不见：往上滚到底边露出来
        if hit.bottom() + margin > view.bottom() {
            y -= hit.bottom() + margin - view.bottom();
        }
        // 在上面看不见（或者比看得见的地方还高）：顶边对齐
        let top = hit.top() + (y - was.y);
        if top - margin < view.top() {
            y += view.top() - (top - margin);
        }
        let y = y.min(px(0.));
        if y != was.y {
            self.scroll.set_offset(point(was.x, y));
            cx.notify();
        }
    }

    /// 这一页显示出来了：读一遍选中的工作区里各个仓库的子仓库、分支、改动数。
    pub fn shown(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.visible = true;
        self.want_scan(cx);
        cx.notify();
    }

    /// 这一页不显示了。
    pub fn hidden(&mut self) {
        self.visible = false;
        self.hit = None;
        self.scroll_to_hit = false;
    }

    /// 列的是哪台电脑，选中的是哪个工作区（那台上的工作区还没读到、一个都没有时是 None）。
    pub fn selection(&self, cx: &App) -> (String, Option<WorkspaceKey>) {
        let app = self.app.read(cx);
        let machine = app.menu_machine();
        let current = app.current();
        let key = app.workspaces_on(&machine).and_then(|list| {
            let chosen = list.iter().find(|(key, ..)| key == current);
            chosen.or(list.first()).map(|(key, ..)| key.clone())
        });
        (machine, key)
    }

    fn want_scan(&mut self, cx: &mut Context<Self>) {
        let (_, key) = self.selection(cx);
        let Some(key) = key else {
            return;
        };
        let members = self.app.read(cx).members(&key);
        self.scan.update(cx, |scan, cx| scan.want(&members, cx));
    }

    /// 「全部打开」时还没找完的子仓库，找完了的接着开出来（不换显示着的那一页）。
    fn open_found_subs(&mut self, cx: &mut Context<Self>) {
        if self.open_later.is_empty() {
            return;
        }
        let mut found: Vec<(WorkspaceKey, Vec<RepoEntry>)> = Vec::new();
        let scan = self.scan.read(cx);
        self.open_later.retain(|(root, home)| {
            let Some(subs) = scan.get(root).and_then(|scanned| scanned.subs.clone()) else {
                // 读不了（服务器断开了）就不等了
                return scan.pending(root);
            };
            let subs = subs.iter().map(|relative| sub_entry(root, relative));
            match found.iter_mut().find(|(key, _)| key == home) {
                Some((_, entries)) => entries.extend(subs),
                None => found.push((home.clone(), subs.collect())),
            }
            false
        });
        for (home, entries) in found {
            if !entries.is_empty() {
                cx.emit(PageEvent::OpenAll {
                    entries,
                    show: false,
                    home,
                });
            }
        }
    }

    /// 工作区页的下拉里换一台电脑看。
    fn pick_machine(&mut self, machine: String, cx: &mut Context<Self>) {
        self.hit = None;
        self.app
            .update(cx, |app, _| app.menu_machine = Some(machine));
        self.want_scan(cx);
        cx.notify();
    }

    /// 选中一个工作区：只是在这一页上看它，标签页不动。
    fn pick(&mut self, key: WorkspaceKey, cx: &mut Context<Self>) {
        self.hit = None;
        self.app.update(cx, |app, cx| app.set_current(key, cx));
        self.want_scan(cx);
        cx.notify();
    }

    /// 「全部打开」：工作区里的仓库连同子仓库，按列的先后各开一个标签页（已经开着的不重复开），
    /// 显示第一个。子仓库还没找完的，找完了接着开。
    fn open_all(&mut self, key: &WorkspaceKey, cx: &mut Context<Self>) {
        let members = self.app.read(cx).members(key);
        let scan = self.scan.read(cx);
        let mut entries: Vec<RepoEntry> = Vec::new();
        let mut later: Vec<RepoEntry> = Vec::new();
        for root in &members {
            entries.push(root.clone());
            match scan.get(root).and_then(|scanned| scanned.subs.clone()) {
                Some(subs) => {
                    entries.extend(subs.iter().map(|relative| sub_entry(root, relative)));
                }
                None if scan.pending(root) => later.push(root.clone()),
                None => {}
            }
        }
        // 接在还没补开完的后面（同一个不重复）：原来是换掉，连着对两个工作区「全部打开」时，
        // 前一个还没找完的子仓库就不开了
        for root in later {
            if !self.open_later.iter().any(|(other, _)| *other == root) {
                self.open_later.push((root, key.clone()));
            }
        }
        if !entries.is_empty() {
            cx.emit(PageEvent::OpenAll {
                entries,
                show: true,
                home: key.clone(),
            });
        }
    }

    fn toggle_fold(&mut self, entry: RepoEntry, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            app.update_settings(cx, |settings| match settings.folded_subs.contains(&entry) {
                true => settings.folded_subs.retain(|other| *other != entry),
                false => settings.folded_subs.push(entry),
            })
        });
    }

    fn connect(&mut self, machine: &str, cx: &mut Context<Self>) {
        let machine = machine.to_string();
        self.app
            .update(cx, |app, cx| drop(app.connect(&machine, cx)));
    }

    /// 那台电脑断开着：说一声，返回假。
    fn check_writable(&self, machine: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let app = self.app.read(cx);
        if app.writable(machine) {
            return true;
        }
        let text = tf!(
            "与 {name} 的连接已断开，暂时无法修改。",
            name = app.machine_name(machine)
        );
        window.push_notification(Notification::warning(text), cx);
        false
    }

    /// 新建工作区：填名称、选保存在哪台电脑上（默认是这一页正看着的那台，它没连着时默认本机）。
    /// 建好了选中它。
    fn new_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.read(cx);
        let shown = app.menu_machine();
        let default = match app.writable(&shown) {
            true => shown,
            false => LOCAL.to_string(),
        };
        let mut places: Vec<(String, SharedString)> = vec![(LOCAL.to_string(), tr("本机").into())];
        places.extend(
            app.settings
                .servers
                .iter()
                .map(|server| (server.id.clone(), server.name.clone().into())),
        );
        let this = cx.entity().downgrade();
        crate::workspace::new_workspace_dialog(
            places,
            default,
            window,
            cx,
            move |name, machine, _, cx| {
                this.update(cx, |this, cx| {
                    let key = this
                        .app
                        .update(cx, |app, cx| app.add_workspace(&machine, &name, cx));
                    if let Some(key) = key {
                        this.pick(key, cx);
                    }
                })
                .ok();
            },
        );
    }

    fn rename_workspace(&mut self, key: WorkspaceKey, window: &mut Window, cx: &mut Context<Self>) {
        if !self.check_writable(&key.machine, window, cx) {
            return;
        }
        let old = self.app.read(cx).workspace_name(&key).to_string();
        let app = self.app.clone();
        crate::workspace::ask_name(
            tr("重命名工作区"),
            tr("新名称"),
            old,
            tr("重命名"),
            false,
            None,
            window,
            cx,
            move |name, _, cx| app.update(cx, |app, cx| app.rename_workspace(&key, &name, cx)),
        );
    }

    /// 删掉一个工作区（先问一下）。每台电脑至少留一个。仓库还在列表里，开着的标签页不动。
    fn delete_workspace(&mut self, key: WorkspaceKey, window: &mut Window, cx: &mut Context<Self>) {
        if !self.check_writable(&key.machine, window, cx) {
            return;
        }
        let (name, machine, count) = {
            let app = self.app.read(cx);
            (
                app.workspace_name(&key),
                app.machine_name(&key.machine),
                app.workspace_count(&key.machine),
            )
        };
        if count <= 1 {
            let text = match key.is_local() {
                true => tr("这是最后一个工作区，无法删除。").to_string(),
                false => tf!("这是 {machine} 上的最后一个工作区，无法删除。", machine),
            };
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let mut description = String::new();
        // 服务器上的工作区：别的电脑上也跟着没了
        if !key.is_local() {
            description.push_str(&tf!(
                "此工作区保存在 {machine} 上，所有电脑将同步删除。",
                machine
            ));
        }
        description.push_str(tr("仓库仍保留在列表中，已打开的标签页不受影响。"));
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (this, key) = (this.clone(), key.clone());
            alert
                .title(tf!("删除工作区「{name}」？", name))
                .description(description.clone())
                .on_ok(|_, _, _| false)
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .label(tr("取消"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("ok")
                                .label(tr("删除工作区"))
                                .with_variant(ButtonVariant::Danger)
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let key = key.clone();
                                    this.update(cx, |this, cx| {
                                        this.app.update(cx, |app, cx| {
                                            app.remove_workspace(&key, cx);
                                            app.resolve_current(cx);
                                        });
                                    })
                                    .ok();
                                }),
                        ),
                )
        });
    }

    /// 把仓库移出工作区（先问一下）。仓库还在列表里，开着的标签页不关。
    fn remove_from_workspace(
        &mut self,
        key: WorkspaceKey,
        entry: RepoEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.check_writable(&key.machine, window, cx) {
            return;
        }
        let (name, workspace, machine) = {
            let app = self.app.read(cx);
            (
                app.repo_name(&entry),
                app.workspace_name(&key),
                (!key.is_local()).then(|| app.machine_name(&key.machine)),
            )
        };
        let mut description = String::new();
        // 服务器上的工作区：别的电脑上也跟着变
        if let Some(machine) = machine {
            description.push_str(&tf!(
                "此工作区保存在 {machine} 上，所有电脑将同步移除。",
                machine
            ));
        }
        description.push_str(tr("仓库仍保留在列表中，已打开的标签页不受影响。"));
        let app = self.app.clone();
        crate::manager::confirm(
            tf!("将 {name} 移出工作区「{workspace}」？", name, workspace),
            description,
            tr("移出"),
            false,
            window,
            cx,
            move |cx| {
                app.update(cx, |app, cx| app.remove_from_workspace(&key, &entry, cx));
            },
        );
    }

    /// 「添加仓库…」：列出能放进这个工作区的仓库（这台电脑的，和它配置的服务器上的），按电脑分组，
    /// 可以查找，勾上就放进来、去掉勾就移出去。
    fn add_repos(&mut self, key: WorkspaceKey, window: &mut Window, cx: &mut Context<Self>) {
        if !self.check_writable(&key.machine, window, cx) {
            return;
        }
        let title = tf!(
            "添加仓库到「{name}」",
            name = self.app.read(cx).workspace_name(&key)
        );
        let picker = cx.new(|cx| AddRepos::new(self.app.clone(), key, window, cx));
        let first_build = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                let input = picker.read(cx).input.clone();
                input.focus_handle(cx).focus(window, cx);
            }
            dialog
                .title(title.clone())
                .w(px(560.))
                .on_ok(|_, _, _| false)
                .child(picker.clone())
                .footer(
                    DialogFooter::new().child(
                        Button::new("done")
                            .primary()
                            .label(tr("完成"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
        });
    }

    // ---- 画 ----

    /// 左边的侧栏：顶上一条、电脑的下拉、工作区的列表。
    fn render_side(
        &self,
        machine: &str,
        selected: Option<&WorkspaceKey>,
        width: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (panel, border, muted, accent) = (
            theme.sidebar,
            theme.sidebar_border,
            theme.muted_foreground,
            theme.primary,
        );
        let (selected_bg, hover, fg) = (
            theme.sidebar_accent,
            theme.list_hover,
            theme.sidebar_foreground,
        );
        let app = self.app.read(cx);
        let list = app.workspaces_on(machine);
        let hues = app.workspace_hues();
        let summary: SharedString = match &list {
            Some(list) => tf!("{n} 个工作区", n = list.len()).into(),
            None => crate::workspace::machine_status(app, machine, cx).0,
        };
        let rows: Vec<AnyElement> = list
            .clone()
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(ix, (key, name, count))| {
                let on = selected == Some(&key);
                let hue = hues.get(&key).map_or(muted, |slot| ui::hue(*slot, cx));
                let (pick, menu_key) = (key.clone(), key.clone());
                let this = cx.entity().downgrade();
                h_flex()
                    .id(("ws-row", ix))
                    .h(px(30.))
                    .mx(px(6.))
                    .px(px(8.))
                    .gap(px(8.))
                    .rounded(px(ui::RADIUS))
                    .cursor_pointer()
                    .text_sm()
                    .text_color(fg)
                    .when(on, |row| row.bg(selected_bg))
                    .when(!on, |row| row.hover(|s| s.bg(hover)))
                    .on_click(cx.listener(move |this, _, _, cx| this.pick(pick.clone(), cx)))
                    // 工作区的色块：和会话页侧栏的分节标题同一个颜色
                    .child(
                        h_flex()
                            .flex_none()
                            .w(px(12.))
                            .justify_center()
                            .child(ui::hue_mark(hue, 8.)),
                    )
                    .child(ui::clipped(("ws-name", ix), name).flex_1())
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(muted)
                            .child(count.to_string()),
                    )
                    .context_menu(move |menu, _, _| {
                        let (rename, delete) = (this.clone(), this.clone());
                        let (rename_key, delete_key) = (menu_key.clone(), menu_key.clone());
                        menu.item(PopupMenuItem::new(tr("重命名…")).on_click(
                            move |_, window, cx| {
                                rename
                                    .update(cx, |this, cx| {
                                        this.rename_workspace(rename_key.clone(), window, cx)
                                    })
                                    .ok();
                            },
                        ))
                        .item(
                            PopupMenuItem::new(tr("删除…")).on_click(move |_, window, cx| {
                                delete
                                    .update(cx, |this, cx| {
                                        this.delete_workspace(delete_key.clone(), window, cx)
                                    })
                                    .ok();
                            }),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        let empty: Option<AnyElement> = match &list {
            None => Some(self.render_connect(machine, cx)),
            Some(list) if list.is_empty() => Some(
                div()
                    .px(px(14.))
                    .py(px(24.))
                    .text_xs()
                    .text_color(muted)
                    .text_center()
                    .child(tr("这台主机上没有工作区。"))
                    .into_any_element(),
            ),
            Some(_) => None,
        };
        let switcher = self.render_machine_picker(machine, cx);
        v_flex()
            .w(px(width))
            .flex_none()
            .relative()
            .h_full()
            .bg(panel)
            .border_r_1()
            .border_color(border)
            .child(
                h_flex()
                    .h(px(HEAD_HEIGHT))
                    .flex_none()
                    .px(px(12.))
                    .gap(px(10.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .flex_none()
                            .size(px(28.))
                            .rounded(px(7.))
                            .bg(accent.opacity(0.14))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(ui::icon(IconName::Layers).text_color(accent)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                ui::clipped("ws-title", trc("页面", "工作区"))
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                ui::clipped("ws-summary", summary)
                                    .text_size(px(11.))
                                    .text_color(muted),
                            ),
                    )
                    .child(
                        Button::new("ws-new")
                            .ghost()
                            .xsmall()
                            .icon(ui::icon(IconName::Plus))
                            .tooltip(tr("新建工作区…"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.new_workspace(window, cx)),
                            ),
                    ),
            )
            .children(switcher)
            .child(
                v_flex()
                    .id("ws-rows")
                    .flex_1()
                    .min_h_0()
                    .pt(px(6.))
                    .pb(px(10.))
                    .gap(px(1.))
                    .overflow_y_scroll()
                    .children(rows)
                    .children(empty),
            )
            .child(crate::side_split::handle("ws-side-split", &self.app))
    }

    /// 侧栏上的电脑下拉：本机和各台服务器，右边写有几个工作区或者连没连上。没有配置服务器时没有它。
    fn render_machine_picker(&self, machine: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let app = self.app.read(cx);
        if app.settings.servers.is_empty() {
            return None;
        }
        let muted = cx.theme().muted_foreground;
        let note = |id: &str| -> SharedString {
            match app.workspaces_on(id) {
                Some(list) => tf!("{n} 个工作区", n = list.len()).into(),
                None => crate::workspace::machine_status(app, id, cx).0,
            }
        };
        let mut choices: Vec<(String, IconName, SharedString, SharedString)> = vec![(
            LOCAL.to_string(),
            IconName::Monitor,
            app.machine_name(LOCAL),
            note(LOCAL),
        )];
        for server in &app.settings.servers {
            choices.push((
                server.id.clone(),
                IconName::Server,
                app.machine_name(&server.id),
                note(&server.id),
            ));
        }
        let dot = (machine != LOCAL).then(|| crate::workspace::machine_status(app, machine, cx).1);
        let (icon, name) = choices
            .iter()
            .find(|choice| choice.0 == machine)
            .map(|choice| (choice.1, choice.2.clone()))?;
        let now = machine.to_string();
        let this = cx.entity().downgrade();
        let button = Button::new("ws-machine")
            .outline()
            .small()
            .w_full()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(6.))
                    .child(ui::icon_small(icon).flex_none().text_color(muted))
                    .child(ui::clipped("ws-machine-name", name).flex_shrink(1.))
                    .children(dot.map(|dot| div().flex_none().size(px(6.)).rounded_full().bg(dot))),
            )
            .dropdown_caret(true)
            .tooltip(tr("列出所选主机上的工作区"))
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.min_w(px(232.));
                for (id, icon, name, note) in &choices {
                    let (this, to) = (this.clone(), id.clone());
                    let (icon, name, note) = (*icon, name.clone(), note.clone());
                    menu = menu.item(
                        PopupMenuItem::element(move |_, _| {
                            h_flex()
                                .w_full()
                                .gap(px(8.))
                                .child(ui::icon_small(icon).flex_none())
                                .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(note.clone()),
                                )
                        })
                        .checked(*id == now)
                        .on_click(move |_, _, cx| {
                            let to = to.clone();
                            this.update(cx, |this, cx| this.pick_machine(to, cx)).ok();
                        }),
                    );
                }
                menu
            });
        Some(
            div()
                .flex_none()
                .mx(px(10.))
                .mt(px(8.))
                .child(button)
                .into_any_element(),
        )
    }

    /// 选的那台服务器还没连上：说一声，给一个「连接」；刚才没连上的把原因写出来。
    fn render_connect(&self, machine: &str, cx: &mut Context<Self>) -> AnyElement {
        let (muted, danger) = (cx.theme().muted_foreground, cx.theme().danger);
        let app = self.app.read(cx);
        let status = app.status(machine);
        let connecting = matches!(status, ConnStatus::Connecting);
        let failed = match status {
            ConnStatus::Failed(err) => Some(err),
            _ => None,
        };
        let label = tf!("连接 {name}", name = app.machine_name(machine));
        let id = machine.to_string();
        v_flex()
            .px(px(14.))
            .py(px(24.))
            .gap(px(12.))
            .items_center()
            .text_xs()
            .text_color(muted)
            .child(tr("尚未连接此主机，连接后显示其中的工作区。"))
            .children(failed.map(|err| {
                v_flex()
                    .w_full()
                    .gap(px(2.))
                    .text_color(danger)
                    .child(tr("连接失败"))
                    .child(div().w_full().text_size(px(11.)).child(err))
            }))
            .child(
                Button::new("ws-connect")
                    .outline()
                    .small()
                    .icon(ui::icon(IconName::Plug))
                    .label(label)
                    .loading(connecting)
                    .disabled(connecting)
                    .on_click(cx.listener(move |this, _, _, cx| this.connect(&id, cx))),
            )
            .into_any_element()
    }

    /// 右边：选中的工作区的名字、个数、按钮，下面是它的仓库。
    fn render_main(
        &self,
        machine: &str,
        key: Option<&WorkspaceKey>,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, panel, warning) = (
            theme.muted_foreground,
            theme.border,
            theme.sidebar,
            theme.warning,
        );
        let app = self.app.read(cx);
        let Some(key) = key else {
            let (icon, text) = match app.workspaces_on(machine) {
                None => (IconName::Plug, tr("尚未连接此主机")),
                Some(_) => (IconName::Layers, tr("这台主机上没有工作区")),
            };
            return blank(icon, text, cx).into_any_element();
        };
        let key = key.clone();
        let members = app.members(&key);
        let open = Rc::new(app.tabs().repos.clone());
        let scan = self.scan.read(cx);
        let subs: usize = members
            .iter()
            .filter_map(|entry| scan.get(entry).and_then(|s| s.subs.as_ref()).map(Vec::len))
            .sum();
        let opened = open
            .iter()
            .filter(|tab| {
                members
                    .iter()
                    .any(|member| member == *tab || member.contains(tab))
            })
            .count();
        let mut line = tf!("{n} 个仓库", n = members.len());
        if subs > 0 {
            line.push_str(&tf!("、{n} 个子仓库", n = subs));
        }
        if opened > 0 {
            line.push_str(&tf!(" · {n} 个已打开", n = opened));
        }
        line.push_str(" · ");
        line.push_str(&match key.is_local() {
            true => tr("保存在本机").to_string(),
            false => tf!("保存在 {name}", name = app.machine_name(&key.machine)),
        });
        let writable = app.writable(&key.machine);
        let name = app.workspace_name(&key);
        // 工作区的颜色（和侧栏、会话页的一样）
        let hue = ui::hue(app.workspace_hue(&key), cx);
        // 断开着：只能看、能打开，不能改
        let banner = (!writable).then(|| {
            let id = key.machine.clone();
            h_flex()
                .px(px(14.))
                .py(px(8.))
                .gap(px(10.))
                .rounded(px(9.))
                .border_1()
                .border_color(warning.opacity(0.35))
                .bg(warning.opacity(0.08))
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .size_4()
                        .flex_none()
                        .text_color(warning),
                )
                .child(div().flex_1().min_w_0().text_sm().child(tf!(
                    "与 {name} 的连接已断开，暂时无法修改。",
                    name = app.machine_name(&key.machine)
                )))
                .child(
                    Button::new("ws-reconnect")
                        .small()
                        .icon(ui::icon(IconName::Plug))
                        .label(tr("重新连接"))
                        .on_click(cx.listener(move |this, _, _, cx| this.connect(&id, cx))),
                )
        });
        let (all_key, add_key, more_key) = (key.clone(), key.clone(), key.clone());
        let this = cx.entity().downgrade();
        let actions = h_flex()
            .flex_none()
            .gap(px(6.))
            .when(!members.is_empty(), |row| {
                row.child(
                    Button::new("ws-open-all")
                        .small()
                        .icon(ui::icon(IconName::SquareStack))
                        .label(tr("全部打开"))
                        .tooltip(tr("把此工作区中的仓库和子仓库都在标签页中打开"))
                        .on_click(cx.listener(move |this, _, _, cx| this.open_all(&all_key, cx))),
                )
            })
            .child(
                Button::new("ws-add")
                    .small()
                    .icon(ui::icon(IconName::Plus))
                    .label(tr("添加仓库…"))
                    .disabled(!writable)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.add_repos(add_key.clone(), window, cx)
                    })),
            )
            .child(
                Button::new("ws-more")
                    .ghost()
                    .small()
                    .icon(ui::icon(IconName::Ellipsis))
                    .tooltip(tr("重命名、删除"))
                    .dropdown_menu(move |menu, _, _| {
                        let (rename, delete) = (this.clone(), this.clone());
                        let (rename_key, delete_key) = (more_key.clone(), more_key.clone());
                        menu.item(PopupMenuItem::new(tr("重命名…")).on_click(
                            move |_, window, cx| {
                                rename
                                    .update(cx, |this, cx| {
                                        this.rename_workspace(rename_key.clone(), window, cx)
                                    })
                                    .ok();
                            },
                        ))
                        .item(
                            PopupMenuItem::new(tr("删除…")).on_click(move |_, window, cx| {
                                delete
                                    .update(cx, |this, cx| {
                                        this.delete_workspace(delete_key.clone(), window, cx)
                                    })
                                    .ok();
                            }),
                        )
                    }),
            );
        let head = h_flex()
            .items_start()
            .gap(px(12.))
            .child(
                div()
                    .flex_none()
                    .size(px(40.))
                    .rounded(px(9.))
                    .bg(hue.opacity(0.14))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(IconName::Layers).size_4().text_color(hue)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(4.))
                    .child(
                        ui::clipped("ws-title", name)
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(ui::clipped("ws-line", line).text_xs().text_color(muted)),
            )
            .child(actions);
        let blocks = self.render_blocks(&key, &members, open, writable, compact, cx);
        let add_key = key.clone();
        let card = v_flex()
            .p(px(4.))
            .border_1()
            .border_color(border)
            .rounded(px(10.))
            .bg(panel)
            .when(blocks.is_empty(), |card| {
                card.child(
                    v_flex()
                        .items_center()
                        .gap(px(10.))
                        .py(px(48.))
                        .text_color(muted)
                        .child(Icon::new(IconName::FolderGit2).size_6())
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(cx.theme().foreground)
                                .child(tr("此工作区中暂无仓库")),
                        )
                        .child(
                            Button::new("ws-add-empty")
                                .small()
                                .icon(ui::icon(IconName::Plus))
                                .label(tr("添加仓库…"))
                                .disabled(!writable)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.add_repos(add_key.clone(), window, cx)
                                })),
                        ),
                )
            })
            .children(blocks);
        div()
            .id("ws-main")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(960.))
                    .mx_auto()
                    .px(px(28.))
                    .py(px(24.))
                    .gap(px(14.))
                    .child(head)
                    .children(banner)
                    .child(card),
            )
            .into_any_element()
    }

    /// 工作区里的各个仓库和它们的子仓库。
    fn render_blocks(
        &self,
        key: &WorkspaceKey,
        members: &[RepoEntry],
        open: Rc<Vec<RepoEntry>>,
        writable: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let menu_key = key.clone();
        let this = cx.entity().downgrade();
        let menu: MenuFor = Rc::new(
            move |menu: PopupMenu, entry: RepoEntry, sub: bool, window, cx| {
                let at = |this: &WeakEntity<WorkspacePage>| (this.clone(), entry.clone());
                let (open, term, leave, rename) = (at(&this), at(&this), at(&this), at(&this));
                let path = entry.path.clone();
                let menu = menu.item(PopupMenuItem::new(tr("在标签页中打开")).on_click(
                    move |_, _, cx| {
                        open.0
                            .update(cx, |_, cx| cx.emit(PageEvent::Open(open.1.clone())))
                            .ok();
                    },
                ));
                // 新建终端：子仓库的行也有（开在子仓库里）
                let place = term
                    .0
                    .upgrade()
                    .map(|page| page.read(cx).app.read(cx).repo_name(&term.1))
                    .unwrap_or_default();
                let menu = menu.submenu(
                    tr("新建终端"),
                    window,
                    cx,
                    crate::presets::menu_builder(place, run_in(term.0, term.1)),
                );
                let menu = match sub {
                    true => menu,
                    false => {
                        let leave_key = menu_key.clone();
                        menu.separator().item(
                            PopupMenuItem::new(tr("移出工作区"))
                                .disabled(!writable)
                                .on_click(move |_, window, cx| {
                                    let key = leave_key.clone();
                                    leave
                                        .0
                                        .update(cx, |this, cx| {
                                            this.remove_from_workspace(
                                                key,
                                                leave.1.clone(),
                                                window,
                                                cx,
                                            )
                                        })
                                        .ok();
                                }),
                        )
                    }
                };
                menu.separator()
                    .item(PopupMenuItem::new(tr("重命名…")).on_click(move |_, _, cx| {
                        rename
                            .0
                            .update(cx, |_, cx| cx.emit(PageEvent::Rename(rename.1.clone())))
                            .ok();
                    }))
                    .item(
                        PopupMenuItem::new(tr("复制仓库路径")).on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                        }),
                    )
            },
        );
        let handlers: Handlers<Self> = Handlers {
            open: Rc::new(|_, entry, _, cx| cx.emit(PageEvent::Open(entry))),
            fold: Rc::new(|this, entry, _, cx| this.toggle_fold(entry, cx)),
            terms: Some(Rc::new(|_, entry, _, cx| {
                cx.emit(PageEvent::Terminals(entry))
            })),
            menu: Some(menu),
        };
        let mut blocks = Vec::new();
        for (ix, entry) in members.iter().enumerate() {
            let block = {
                let (app, scan, board) =
                    (self.app.read(cx), self.scan.read(cx), self.board.read(cx));
                let machine = machine_of(entry);
                let offline =
                    entry.server.is_some() && !matches!(app.status(machine), ConnStatus::Ready(_));
                let scanned = scan.get(entry).cloned();
                Block {
                    entry: entry.clone(),
                    name: app.repo_name(entry),
                    aside: entry.server.as_ref().map(|_| app.machine_name(machine)),
                    sub_names: crate::repo_rows::sub_names(app, entry, scanned.as_ref()),
                    scanned,
                    loading: scan.pending(entry),
                    offline,
                    folded: app.settings.folded_subs.contains(entry),
                    open: open.clone(),
                    terms: terminal_summary(board, entry),
                    chips: Vec::new(),
                    hit: self
                        .hit
                        .clone()
                        .filter(|hit| entry.same_repo(hit) || entry.contains(hit)),
                    hit_at: Some(self.hit_bounds.clone()),
                }
            };
            let (term_entry, leave_entry, leave_key) = (entry.clone(), entry.clone(), key.clone());
            let place = self.app.read(cx).repo_name(entry);
            let actions = vec![
                // 只有图标：点了弹新建终端的菜单
                Button::new(("ws-term", ix))
                    .ghost()
                    .xsmall()
                    .icon(ui::icon(IconName::SquareTerminal))
                    .tooltip(tr("新建终端"))
                    // 不当作点了这一行
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .dropdown_menu(crate::presets::menu_builder(
                        place,
                        run_in(cx.entity().downgrade(), term_entry),
                    ))
                    .into_any_element(),
                Button::new(("ws-leave", ix))
                    .ghost()
                    .xsmall()
                    .disabled(!writable)
                    .icon(ui::icon(IconName::CircleMinus))
                    .tooltip(tr("移出工作区"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.remove_from_workspace(
                            leave_key.clone(),
                            leave_entry.clone(),
                            window,
                            cx,
                        );
                    }))
                    .into_any_element(),
            ];
            blocks.push((block, actions));
        }
        let columns = Columns::for_blocks(&blocks, compact);
        blocks
            .into_iter()
            .enumerate()
            .map(|(ix, (block, actions))| render_block(ix, block, actions, columns, &handlers, cx))
            .collect()
    }
}

/// 右边没东西可列时的样子：一个大图标、一句话。
fn blank(icon: IconName, text: &'static str, cx: &App) -> Div {
    let (muted, fg) = (cx.theme().muted_foreground, cx.theme().foreground);
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap(px(10.))
        .text_color(muted)
        .child(Icon::new(icon).size_8())
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(fg)
                .child(text),
        )
}

impl Render for WorkspacePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size().width.as_f32();
        let width = crate::repo::sidebar_open(&self.app.read(cx).settings.layout, viewport);
        let (machine, key) = self.selection(cx);
        let background = cx.theme().background;
        // 「打开工作区」过来的：这一帧画完知道那一行在哪，下一帧滚过去
        if self.scroll_to_hit {
            cx.on_next_frame(window, |this, _, cx| this.scroll_hit_into_view(cx));
        } else if self.hit.is_some() && !self.hit_exact {
            // 先滚到了仓库那一行（子仓库还没找到）：找到了、标在子仓库那一行上以后再滚一次
            cx.on_next_frame(window, |this, _, cx| {
                if this.hit_bounds.get().is_some_and(|(_, exact)| exact) {
                    this.scroll_hit_into_view(cx);
                }
            });
        }
        h_flex()
            .size_full()
            .bg(background)
            // 拖左边那一栏的分隔条，见 side_split
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<crate::side_split::DragSide>, window, cx| {
                    crate::side_split::drag(&this.app, event, window, cx);
                    cx.notify();
                },
            ))
            .child(self.render_side(&machine, key.as_ref(), width, cx))
            .child(self.render_main(
                &machine,
                key.as_ref(),
                viewport < crate::repo_rows::COMPACT_BELOW,
                cx,
            ))
    }
}

/// 「添加仓库…」里的列表：查找框，下面按电脑分组的仓库，每个前面一个勾选框。
struct AddRepos {
    app: Entity<AppState>,
    key: WorkspaceKey,
    input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl AddRepos {
    fn new(
        app: Entity<AppState>,
        key: WorkspaceKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("按名称或路径查找仓库")));
        let subscriptions = vec![
            cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            // 勾了以后工作区变了：勾选框跟着变
            cx.observe(&app, |_, _, cx| cx.notify()),
        ];
        Self {
            app,
            key,
            input,
            _subscriptions: subscriptions,
        }
    }

    fn toggle(&mut self, entry: RepoEntry, checked: bool, cx: &mut Context<Self>) {
        let key = self.key.clone();
        self.app.update(cx, |app, cx| match checked {
            true => drop(app.add_to_workspace(&key, &entry, cx)),
            false => drop(app.remove_from_workspace(&key, &entry, cx)),
        });
    }
}

impl Render for AddRepos {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list_height = (window.viewport_size().height - px(260.)).clamp(px(160.), px(420.));
        let theme = cx.theme();
        let (muted, hover) = (theme.muted_foreground, theme.list_hover);
        let needle = self.input.read(cx).value().trim().to_lowercase();
        let app = self.app.read(cx);
        let members = app.members(&self.key);
        // 这个工作区所在的电脑看得到的仓库：它自己的、它配置的服务器上的（读到了的）
        let repos = app.perspective_repos(&self.key.machine);
        let mut groups: Vec<(String, Vec<RepoEntry>)> = Vec::new();
        for entry in repos {
            let name = app.repo_name(&entry).to_lowercase();
            if !needle.is_empty()
                && !name.contains(&needle)
                && !entry.path.to_lowercase().contains(&needle)
            {
                continue;
            }
            let machine = machine_of(&entry).to_string();
            match groups.iter_mut().find(|(key, _)| *key == machine) {
                Some((_, list)) => list.push(entry),
                None => groups.push((machine, vec![entry])),
            }
        }
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut ix: usize = 0;
        for (machine, list) in groups {
            let icon = match machine == LOCAL {
                true => IconName::Monitor,
                false => IconName::Server,
            };
            rows.push(
                h_flex()
                    .h(px(26.))
                    .px(px(6.))
                    .gap(px(6.))
                    .text_xs()
                    .text_color(muted)
                    .child(ui::icon_small(icon))
                    .child(app.machine_name(&machine))
                    .into_any_element(),
            );
            for entry in list {
                let checked = members.contains(&entry);
                let (toggle, name) = (entry.clone(), app.repo_name(&entry));
                rows.push(
                    h_flex()
                        .id(("add-repo", ix))
                        .px(px(6.))
                        .py(px(4.))
                        .gap(px(10.))
                        .rounded(px(ui::RADIUS))
                        .hover(|s| s.bg(hover))
                        .child(Checkbox::new(("add-check", ix)).checked(checked).on_click(
                            cx.listener(move |this, checked: &bool, _, cx| {
                                this.toggle(toggle.clone(), *checked, cx)
                            }),
                        ))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(ui::clipped(("add-name", ix), name).text_sm())
                                .child(
                                    ui::clipped(("add-path", ix), entry.path.clone())
                                        .text_xs()
                                        .font_family(crate::fonts::mono())
                                        .text_color(muted),
                                ),
                        )
                        .into_any_element(),
                );
                ix += 1;
            }
        }
        let empty = rows.is_empty();
        v_flex()
            .gap_2()
            .child(Input::new(&self.input))
            .child(
                div()
                    .id("add-repos-list")
                    .max_h(list_height)
                    .overflow_y_scroll()
                    .child(v_flex().gap(px(1.)).children(rows))
                    .when(empty, |list| {
                        list.child(
                            div()
                                .p_2()
                                .text_sm()
                                .text_color(muted)
                                .child(tr("没有匹配的仓库")),
                        )
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(tr("勾选即加入此工作区；新仓库请先在主机页中添加。")),
            )
    }
}
