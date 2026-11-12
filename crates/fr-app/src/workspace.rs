//! 主窗口：标题栏（工作区、标签页、设置）和下面的内容。
//!
//! 标题栏是自己画的，标签页直接放在里面，不另占一行。第一个标签页是仓库管理器，后面每个
//! 打开的仓库一个；设置页打开时也是一个标签页。标签页开多了先一起变窄，窄到头了横向滚动，
//! 右边多出一个列出全部标签页的按钮；设置和窗口的按钮一直留在原地。
//!
//! 工作区是一组标签页：切到另一个工作区，标签栏整个换成它的那一组。别的工作区里已经打开过
//! 的标签页留在内存里（正在做的操作不会被打断），只是不显示。
//!
//! 快捷键：F5 刷新、Ctrl+W 关标签页、Ctrl+Tab 切标签页、Ctrl+F 搜索提交、Ctrl+P 快速打开仓库、
//! Ctrl+1 本地修改、Ctrl+2 所有提交、Ctrl+, 设置、Ctrl+Shift+T 重新打开刚关掉的标签页、
//! Ctrl+` 打开、收起当前仓库底下的终端。
//!
//! 关标签页时：有写操作在做就先拦一下；提交说明框里没提交的内容记下来，再打开这个仓库时放回去。
//! 关掉的标签页每个工作区记最近 10 个（存在设置里），手滑关错了能找回来；
//! 设置里还可以把标签页上的关闭按钮藏起来。

use std::cell::Cell;
use std::rc::Rc;

use fr_core::i18n::{self, tr, trc};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, TitleBar, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::manager::{ManagerEvent, ManagerView};
use crate::prompt::{self, Prompts};
use crate::repo::{RepoEvent, RepoView};
use crate::settings::RepoEntry;
use crate::settings_page::SettingsPage;
use crate::tray::{Tray, TrayAction};
use crate::ui;
use crate::win::{self, Hwnd};

/// 标题栏的高度。标签页 28 高，上下各留 5。
const TITLE_HEIGHT: f32 = 38.;
/// 标签页最窄缩到这么宽（还看得见图标和名字的头几个字）；再放不下就横向滚动。
const TAB_MIN_WIDTH: f32 = 96.;

struct RepoTab {
    entry: RepoEntry,
    view: Entity<RepoView>,
    _observe: Subscription,
}

/// 一个工作区里已经建好的标签页和选中的那一个。
type Parked = (Vec<RepoTab>, usize);

/// 上一次应用到窗口上的外观设置；变了才重新应用。
#[derive(PartialEq, Clone)]
struct Applied {
    palette: &'static str,
    lang: i18n::Lang,
    tray: bool,
}

pub struct Workspace {
    app: Entity<AppState>,
    manager: Entity<ManagerView>,
    settings_page: Entity<SettingsPage>,
    /// 当前工作区的标签页
    tabs: Vec<RepoTab>,
    /// 0 是仓库管理器，i 是 `tabs[i - 1]`
    active: usize,
    /// 其他工作区里已经建好的标签页，下标和设置里的工作区对应；当前工作区的那一格是空的
    parked: Vec<Option<Parked>>,
    /// 设置页开着；它盖在当前标签页上面，关掉回到原来那一页
    showing_settings: bool,
    /// 标签栏的横向滚动位置
    tab_scroll: ScrollHandle,
    /// 标签页多到放不下了（上一帧量出来的）：多显示一个「所有标签页」的按钮
    tabs_overflow: bool,
    /// 关掉的标签页里没提交的说明，再打开同一个仓库时放回去（只在这次运行期间记着）
    drafts: Vec<(RepoEntry, String)>,
    tray: Option<Tray>,
    hwnd: Option<Hwnd>,
    applied: Option<Applied>,
    /// 托盘菜单里点了「退出」：这次关闭不再收进托盘
    quitting: Rc<Cell<bool>>,
    _subscriptions: Vec<Subscription>,
    _prompts: Task<()>,
}

impl Workspace {
    pub fn new(
        app: Entity<AppState>,
        prompts: Prompts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let manager = cx.new(|cx| ManagerView::new(app.clone(), cx));
        let settings_page = cx.new(|cx| SettingsPage::new(app.clone(), cx));
        let quitting = Rc::new(Cell::new(false));
        let subscriptions = vec![
            cx.subscribe_in(&manager, window, |this, _, event, window, cx| match event {
                ManagerEvent::Open(entry) => this.open(entry.clone(), window, cx),
                ManagerEvent::Rename(entry) => this.rename_repo(entry.clone(), window, cx),
                ManagerEvent::RemoveFromWorkspace(entry) => {
                    this.remove_from_workspace(entry.clone(), window, cx)
                }
                ManagerEvent::QuickOpen => this.quick_open(window, cx),
            }),
            // 设置变了：服务器改名要刷新标签上的名字；服务器被移除了，它上面的仓库的标签页跟着关掉；
            // 主题、语言、托盘的选项变了就应用到窗口上
            cx.observe_in(&app, window, |this, _, window, cx| {
                this.drop_tabs_of_removed_servers(cx);
                this.apply_settings(window, cx);
                cx.notify();
            }),
            // 记住窗口的位置和大小
            cx.observe_window_bounds(window, |this, window, cx| {
                let state = crate::appearance::window_state(window);
                this.app
                    .update(cx, |app, cx| app.set_window_state(state, cx));
            }),
            // 主题设成「跟随系统」时，Windows 切换深浅色就跟着换
            cx.observe_window_appearance(window, |this, window, cx| {
                this.apply_settings(window, cx);
            }),
            // 最小化到托盘：窗口失去焦点时看一眼是不是最小化了
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.hide_if_minimized(cx);
                }
            }),
        ];
        // 关闭到托盘：点关闭按钮时不退出，把窗口藏起来
        {
            let (app, quitting) = (app.clone(), quitting.clone());
            let this = cx.entity().downgrade();
            window.on_window_should_close(cx, move |window, cx| {
                let to_tray = app.read(cx).settings.close_to_tray
                    && !quitting.get()
                    && this
                        .read_with(cx, |this, _| this.tray.is_some())
                        .unwrap_or(false);
                if let (true, Some(hwnd)) = (to_tray, win::hwnd(window)) {
                    win::hide(hwnd);
                    return false;
                }
                // 真要退出：终端里还有程序在跑就先问一下
                let terminals = this
                    .read_with(cx, |this, cx| this.live_terminals(cx))
                    .unwrap_or(0);
                if terminals == 0 || quitting.get() {
                    return true;
                }
                let quitting = quitting.clone();
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let quitting = quitting.clone();
                    alert
                        .title(tr("退出？"))
                        .description(tf!(
                            "还有 {n} 个终端开着，退出会结束里面的程序。",
                            n = terminals
                        ))
                        .on_ok(|_, _, _| false)
                        .footer(
                            DialogFooter::new()
                                .child(
                                    Button::new("cancel")
                                        .label(tr("不退出"))
                                        .on_click(|_, window, cx| window.close_dialog(cx)),
                                )
                                .child(
                                    Button::new("ok")
                                        .label(tr("结束并退出"))
                                        .with_variant(ButtonVariant::Danger)
                                        .on_click(move |_, window, cx| {
                                            window.close_dialog(cx);
                                            quitting.set(true);
                                            cx.quit();
                                        }),
                                ),
                        )
                });
                false
            });
        }
        let count = app.read(cx).settings.workspaces.len();
        let mut this = Self {
            app: app.clone(),
            manager,
            settings_page,
            tabs: Vec::new(),
            active: 0,
            parked: (0..count).map(|_| None).collect(),
            showing_settings: false,
            tab_scroll: ScrollHandle::new(),
            tabs_overflow: false,
            drafts: Vec::new(),
            tray: None,
            hwnd: win::hwnd(window),
            applied: None,
            quitting,
            _subscriptions: subscriptions,
            _prompts: prompt::show_in(prompts, window, cx),
        };
        // 恢复上次开着的标签页
        let (open, active) = {
            let group = app.read(cx).settings.workspace();
            (group.tabs.clone(), group.active)
        };
        for entry in open {
            let tab = this.build_tab(entry, window, cx);
            this.tabs.push(tab);
        }
        this.active = active.min(this.tabs.len());
        this.update_visibility(cx);
        this.apply_settings(window, cx);
        this
    }

    /// 把设置里的主题、语言、托盘应用到窗口上；和上次一样就不动。
    fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.app.read(cx).settings.clone();
        let palette = crate::appearance::current_palette(&settings, window);
        let lang = settings.language.resolve();
        let wanted = Applied {
            palette: palette.id,
            lang,
            tray: settings.minimize_to_tray || settings.close_to_tray,
        };
        let before = self.applied.clone();
        if before.as_ref() == Some(&wanted) {
            return;
        }
        if before.as_ref().map(|a| a.lang) != Some(lang) {
            i18n::set_lang(lang);
            gpui_kit::component::set_locale(lang.code());
            // 启动时不用：标签页都是按当前语言建的。中途换了语言，开着的标签页要跟着换
            if before.is_some() {
                let views: Vec<_> = self
                    .tabs
                    .iter()
                    .chain(
                        self.parked
                            .iter()
                            .flatten()
                            .flat_map(|(tabs, _)| tabs.iter()),
                    )
                    .map(|tab| tab.view.clone())
                    .collect();
                for view in views {
                    view.update(cx, |view, cx| view.language_changed(window, cx));
                }
            }
        }
        if before.as_ref().map(|a| a.palette) != Some(palette.id) {
            crate::appearance::apply_theme(&settings, window, cx);
        }
        // 托盘：开关变了，或者语言变了（菜单上的字要换）就重建
        let rebuild_tray = before.as_ref().map(|a| (a.tray, a.lang)) != Some((wanted.tray, lang));
        if rebuild_tray {
            self.tray = None;
            if wanted.tray {
                self.tray = Tray::new(cx, |this, action, cx| this.on_tray(action, cx));
            }
        }
        self.applied = Some(wanted);
        window.refresh();
    }

    fn on_tray(&mut self, action: TrayAction, cx: &mut Context<Self>) {
        match action {
            TrayAction::Show => {
                if let Some(hwnd) = self.hwnd {
                    win::show(hwnd);
                }
            }
            TrayAction::Quit => {
                self.quitting.set(true);
                cx.quit();
            }
        }
    }

    /// 最小化到托盘：窗口最小化了、设置里开着、托盘图标也在，就把窗口藏起来。
    fn hide_if_minimized(&mut self, cx: &mut Context<Self>) {
        let on = self.app.read(cx).settings.minimize_to_tray && self.tray.is_some();
        if let (true, Some(hwnd)) = (on, self.hwnd)
            && win::is_minimized(hwnd)
        {
            win::hide(hwnd);
        }
    }

    /// 开机自启带 `--minimized` 启动：有托盘就直接收进托盘。返回收没收进去。
    pub fn start_in_tray(&mut self) -> bool {
        match (self.tray.is_some(), self.hwnd) {
            (true, Some(hwnd)) => {
                win::hide(hwnd);
                true
            }
            _ => false,
        }
    }

    /// 各个工作区里一共有几个终端还跑着程序。
    fn live_terminals(&self, cx: &App) -> usize {
        self.tabs
            .iter()
            .chain(
                self.parked
                    .iter()
                    .flatten()
                    .flat_map(|(tabs, _)| tabs.iter()),
            )
            .filter(|tab| tab.view.read(cx).terminal_alive(cx))
            .count()
    }

    /// 服务器被移除了：它上面的仓库的标签页跟着关掉（各个工作区里的都关）。
    fn drop_tabs_of_removed_servers(&mut self, cx: &mut Context<Self>) {
        let app = self.app.read(cx);
        let gone = |entry: &RepoEntry| {
            entry
                .server
                .as_deref()
                .is_some_and(|id| app.server(id).is_none())
        };
        for parked in self.parked.iter_mut().flatten() {
            let current = parked
                .1
                .checked_sub(1)
                .and_then(|ix| parked.0.get(ix))
                .map(|tab| tab.entry.clone());
            parked.0.retain(|tab| !gone(&tab.entry));
            parked.1 = current
                .and_then(|entry| parked.0.iter().position(|t| t.entry == entry))
                .map_or(0, |ix| ix + 1);
        }
        let before = self.tabs.len();
        let current = self
            .active
            .checked_sub(1)
            .and_then(|ix| self.tabs.get(ix))
            .map(|tab| tab.entry.clone());
        self.tabs.retain(|tab| !gone(&tab.entry));
        if self.tabs.len() != before {
            // 原来显示的标签页还在就留在它上面，不在了回到仓库页
            self.active = current
                .and_then(|entry| self.tabs.iter().position(|t| t.entry == entry))
                .map_or(0, |ix| ix + 1);
            self.persist(cx);
        }
    }

    /// 告诉每个仓库标签页它是不是当前显示的：切到哪一页，哪一页就刷新。
    fn update_visibility(&self, cx: &mut Context<Self>) {
        for (ix, tab) in self.tabs.iter().enumerate() {
            let visible = self.active == ix + 1 && !self.showing_settings;
            tab.view
                .update(cx, |view, cx| view.set_visible(visible, cx));
        }
    }

    /// 打开仓库：已经开着就切过去。
    pub fn open(&mut self, entry: RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        match self.tabs.iter().position(|t| t.entry == entry) {
            Some(ix) => self.active = ix + 1,
            None => {
                let tab = self.build_tab(entry, window, cx);
                self.tabs.push(tab);
                self.active = self.tabs.len();
            }
        }
        self.showing_settings = false;
        self.reveal_active_tab();
        self.persist(cx);
        cx.notify();
    }

    /// 标签页多到要滚动时，把选中的那一页滚到看得见的地方。
    fn reveal_active_tab(&self) {
        if let Some(ix) = self.active.checked_sub(1) {
            self.tab_scroll.scroll_to_item(ix);
        }
    }

    /// 在标签页上滚滚轮：标签页多到放不下时左右滚。
    fn scroll_tabs(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let max = self.tab_scroll.max_offset().x;
        if max <= px(0.) {
            return;
        }
        let delta = event.delta.pixel_delta(window.line_height());
        let step = if delta.x.is_zero() { delta.y } else { delta.x };
        let mut offset = self.tab_scroll.offset();
        offset.x = (offset.x + step).clamp(-max, px(0.));
        self.tab_scroll.set_offset(offset);
        cx.notify();
    }

    /// 给仓库改名：只改这里显示的名字，目录不动。留空恢复成目录名。
    fn rename_repo(&mut self, entry: RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let current = app.read(cx).repo_name(&entry).to_string();
        ask_name(
            tr("仓库改名"),
            tr("显示的名字，留空用目录名"),
            current,
            tr("改名"),
            true,
            window,
            cx,
            move |name, _, cx| app.update(cx, |app, cx| app.rename_repo(&entry, &name, cx)),
        );
    }

    fn build_tab(
        &mut self,
        entry: RepoEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RepoTab {
        let app = self.app.clone();
        let view = cx.new(|cx| RepoView::new(entry.clone(), app, window, cx));
        // 仓库页要求打开另一个仓库（子模块）
        cx.subscribe_in(&view, window, |this, _, event, window, cx| match event {
            RepoEvent::Open(entry) => {
                this.app
                    .update(cx, |app, cx| app.add_repo(entry.clone(), cx));
                this.open(entry.clone(), window, cx);
            }
            RepoEvent::Rename(entry) => this.rename_repo(entry.clone(), window, cx),
        })
        .detach();
        // 上次关这个仓库的标签页时说明框里还有没提交的内容：放回去
        if let Some(at) = self.drafts.iter().position(|(e, _)| *e == entry) {
            let (_, draft) = self.drafts.remove(at);
            view.update(cx, |view, cx| view.set_draft(draft, window, cx));
        }
        // 标签上的「有改动」「正在做」跟着仓库的状态变
        let observe = cx.observe(&view, |_, _, cx| cx.notify());
        RepoTab {
            entry,
            view,
            _observe: observe,
        }
    }

    /// 关标签页。固定着的不关。这一页有写操作在做时不直接关：联网的操作问一下要不要取消，
    /// 改工作区、暂存区的操作（提交、变基）等它做完，中途杀掉会把仓库留在半截状态。
    fn close(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        if self.is_pinned(&tab.entry, cx) {
            let text = tr("这个标签页固定着，取消固定才能关。");
            window.push_notification(Notification::info(text), cx);
            return;
        }
        let (busy, cancellable, terminal) = {
            let view = tab.view.read(cx);
            (view.busy(), view.cancellable(), view.terminal_alive(cx))
        };
        let entry = tab.entry.clone();
        let Some(op) = busy else {
            if terminal {
                let text = tr("终端里的程序还在跑，关掉标签页会结束它。").to_string();
                self.confirm_close(entry, text, tr("结束并关闭"), window, cx);
            } else {
                self.close_now(ix, cx);
            }
            return;
        };
        if !cancellable {
            let text = tf!("这个仓库正在{op}，等它做完再关。", op);
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let text = tf!("这个仓库正在{op}，关掉标签页会取消它。", op);
        self.confirm_close(entry, text, tr("取消操作并关闭"), window, cx);
    }

    /// 关标签页前问一下：`description` 说关掉会怎样，`ok` 是确认按钮上的字。
    fn confirm_close(
        &mut self,
        entry: RepoEntry,
        description: String,
        ok: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (workspace, entry) = (workspace.clone(), entry.clone());
            alert
                .title(tr("关掉这个标签页？"))
                .description(description.clone())
                // 回车不执行，要明确点按钮
                .on_ok(|_, _, _| false)
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .label(tr("不关"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("ok")
                                .label(ok)
                                .with_variant(ButtonVariant::Danger)
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    workspace
                                        .update(cx, |this, cx| {
                                            let at =
                                                this.tabs.iter().position(|t| t.entry == entry);
                                            if let Some(at) = at {
                                                this.close_now(at, cx);
                                            }
                                        })
                                        .ok();
                                }),
                        ),
                )
        });
    }

    fn close_now(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(ix);
        let draft = tab.view.read(cx).draft(cx);
        self.drafts.retain(|(e, _)| *e != tab.entry);
        if !draft.trim().is_empty() {
            self.drafts.push((tab.entry.clone(), draft));
        }
        let entry = tab.entry.clone();
        self.app.update(cx, |app, _| app.note_closed_tab(entry));
        // 关掉的是当前页：留在同一个位置（原来右边那一页），右边没有了才往左；
        // 关掉的是它前面的页，选中位置跟着前移
        if self.active > ix + 1 {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len());
        self.persist(cx);
        cx.notify();
    }

    /// 固定、取消固定第 `ix` 个标签页。固定的排到最前面，选中的还是原来那一页。
    fn toggle_pin(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.tabs.get(ix).map(|tab| tab.entry.clone()) else {
            return;
        };
        let current = self
            .active
            .checked_sub(1)
            .and_then(|ix| self.tabs.get(ix))
            .map(|tab| tab.entry.clone());
        self.app
            .update(cx, |app, cx| app.toggle_pinned(&entry, cx));
        let pinned = self.app.read(cx).settings.workspace().pinned.clone();
        self.tabs.sort_by_key(|tab| !pinned.contains(&tab.entry));
        if let Some(current) = current {
            self.active = self
                .tabs
                .iter()
                .position(|tab| tab.entry == current)
                .map_or(0, |ix| ix + 1);
        }
        self.reveal_active_tab();
        self.persist(cx);
        cx.notify();
    }

    fn is_pinned(&self, entry: &RepoEntry, cx: &App) -> bool {
        self.app.read(cx).settings.workspace().is_pinned(entry)
    }

    /// 把仓库从当前工作区移除（先问一下）：开着的标签页跟着关，仓库还在「全部仓库」里。
    fn remove_from_workspace(
        &mut self,
        entry: RepoEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab = self.tabs.iter().find(|tab| tab.entry == entry);
        if let Some(tab) = tab {
            let view = tab.view.read(cx);
            if view.busy().is_some() || view.terminal_alive(cx) {
                let text = tr("这个仓库正在做操作或终端还开着，先处理完再移除。");
                window.push_notification(Notification::warning(text), cx);
                return;
            }
        }
        let open = tab.is_some();
        let (name, workspace) = {
            let app = self.app.read(cx);
            (
                app.repo_name(&entry),
                app.settings.workspace().name.clone(),
            )
        };
        let this = cx.entity().downgrade();
        crate::manager::confirm(
            tf!("把 {name} 从工作区「{workspace}」移除？", name, workspace),
            if open {
                tr("开着的标签页会关掉。仓库还在「全部仓库」里。").to_string()
            } else {
                tr("仓库还在「全部仓库」里。").to_string()
            },
            tr("移除"),
            false,
            window,
            cx,
            move |cx| {
                this.update(cx, |this, cx| {
                    if let Some(at) = this.tabs.iter().position(|tab| tab.entry == entry) {
                        this.close_now(at, cx);
                    }
                    this.app
                        .update(cx, |app, cx| app.remove_from_workspace(&entry, cx));
                })
                .ok();
            },
        );
    }

    /// 关掉除了第 `keep` 个以外的标签页（`None` 是全关）。固定的不关；
    /// 正在做操作的、终端里还跑着程序的也留着，并说一声。
    fn close_others(&mut self, keep: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let keep_entry = keep
            .and_then(|ix| self.tabs.get(ix))
            .map(|tab| tab.entry.clone());
        let pinned = self.app.read(cx).settings.workspace().pinned.clone();
        let mut skipped = 0;
        let targets: Vec<RepoEntry> = self
            .tabs
            .iter()
            .filter(|tab| Some(&tab.entry) != keep_entry.as_ref())
            .filter(|tab| !pinned.contains(&tab.entry))
            .filter(|tab| {
                let view = tab.view.read(cx);
                let busy = view.busy().is_some() || view.terminal_alive(cx);
                skipped += busy as usize;
                !busy
            })
            .map(|tab| tab.entry.clone())
            .collect();
        for entry in targets {
            if let Some(at) = self.tabs.iter().position(|t| t.entry == entry) {
                self.close_now(at, cx);
            }
        }
        if let Some(entry) = keep_entry {
            self.active = self
                .tabs
                .iter()
                .position(|t| t.entry == entry)
                .map_or(0, |ix| ix + 1);
            self.persist(cx);
        }
        if skipped > 0 {
            let text = tf!(
                "有 {n} 个标签页正在做操作或终端还开着，留着没关。",
                n = skipped
            );
            window.push_notification(Notification::warning(text), cx);
        }
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.active = ix.min(self.tabs.len());
        self.showing_settings = false;
        self.reveal_active_tab();
        self.persist(cx);
        cx.notify();
    }

    /// 打开设置页（Ctrl+,）；已经开着就关掉。
    pub fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.showing_settings = !self.showing_settings;
        self.update_visibility(cx);
        cx.notify();
    }

    fn active_repo(&self) -> Option<&RepoTab> {
        if self.showing_settings {
            return None;
        }
        self.active.checked_sub(1).and_then(|ix| self.tabs.get(ix))
    }

    /// F5：重读当前仓库的状态和提交历史。
    pub fn refresh_active(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.view.update(cx, |view, cx| view.refresh_now(cx));
        }
    }

    /// Ctrl+W：关掉当前的标签页（设置页开着就关设置页）。
    pub fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.showing_settings {
            self.toggle_settings(cx);
        } else if let Some(ix) = self.active.checked_sub(1) {
            self.close(ix, window, cx);
        }
    }

    /// Ctrl+Shift+T：重新打开这个工作区里最近关掉的标签页。
    pub fn reopen_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.app.read(cx).closed_tabs().into_iter().next() {
            Some(entry) => self.open(entry, window, cx),
            None => {
                let text = tr("这个工作区里没有刚关掉的标签页。");
                window.push_notification(Notification::info(text), cx);
            }
        }
    }

    /// Ctrl+`：打开、收起当前仓库底下的终端。
    pub fn terminal_in_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.view
                .update(cx, |view, cx| view.toggle_terminal(window, cx));
        }
    }

    /// Ctrl+1、Ctrl+2：当前仓库切到本地修改、所有提交。
    pub fn show_in_active(&mut self, changes: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.view.update(cx, |view, cx| {
                if changes {
                    view.show_changes_view(window, cx);
                } else {
                    view.show_history_view(window, cx);
                }
            });
        }
    }

    /// Ctrl+Tab、Ctrl+Shift+Tab：切到下一个、上一个标签页，到头绕回去。
    pub fn cycle_tab(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = self.tabs.len() as isize + 1;
        let next = (self.active as isize + step).rem_euclid(count);
        self.select(next as usize, cx);
    }

    /// Ctrl+F：在当前仓库里搜索提交。
    pub fn find_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.view.update(cx, |view, cx| view.find_commit(window, cx));
        }
    }

    /// Ctrl+P：快速打开仓库。输入几个字筛选，上下键选，回车打开选中的，也可以点。
    pub fn quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let picker = cx.new(|cx| QuickOpen::new(app, window, cx));
        cx.subscribe_in(&picker, window, |this, _, event: &Picked, window, cx| {
            window.close_dialog(cx);
            this.open(event.0.clone(), window, cx);
        })
        .detach();
        let first_build = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            // 对话框打开的那一刻焦点会被它拿走，打开后再放到输入框
            if first_build.replace(false) {
                let input = picker.read(cx).input.clone();
                input.focus_handle(cx).focus(window, cx);
            }
            let on_enter = picker.clone();
            dialog
                .title(tr("快速打开仓库"))
                .w(px(620.))
                // 回车打开选中的那个
                .on_ok(move |_, _, cx| {
                    on_enter.update(cx, |picker, cx| picker.pick_selected(cx));
                    false
                })
                .child(picker.clone())
        });
    }

    fn persist(&self, cx: &mut Context<Self>) {
        self.update_visibility(cx);
        let tabs = self.tabs.iter().map(|t| t.entry.clone()).collect();
        let active = self.active;
        self.app
            .update(cx, |app, _| app.set_open_tabs(tabs, active));
    }

    // ---- 工作区 ----

    fn current_workspace(&self, cx: &App) -> usize {
        self.app.read(cx).settings.active_workspace
    }

    /// 切到第 `ix` 个工作区：当前的标签页收起来（留在内存里），换上那一组。
    fn switch_workspace(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.current_workspace(cx);
        let count = self.app.read(cx).settings.workspaces.len();
        self.parked.resize_with(count, || None);
        if ix == current || ix >= count {
            return;
        }
        // 收起来之前先让它们知道自己不显示了
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| view.set_visible(false, cx));
        }
        let tabs = std::mem::take(&mut self.tabs);
        self.parked[current] = Some((tabs, self.active));
        self.app
            .update(cx, |app, cx| app.set_active_workspace(ix, cx));
        match self.parked[ix].take() {
            Some((tabs, active)) => {
                self.tabs = tabs;
                self.active = active.min(self.tabs.len());
            }
            None => {
                let (open, active) = {
                    let group = self.app.read(cx).settings.workspace();
                    (group.tabs.clone(), group.active)
                };
                for entry in open {
                    let tab = self.build_tab(entry, window, cx);
                    self.tabs.push(tab);
                }
                self.active = active.min(self.tabs.len());
            }
        }
        self.showing_settings = false;
        self.reveal_active_tab();
        self.persist(cx);
        cx.notify();
    }

    fn new_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        ask_name(
            tr("新建工作区"),
            tr("工作区的名字，比如「公司」「个人」"),
            String::new(),
            tr("新建"),
            false,
            window,
            cx,
            move |name, window, cx| {
                this.update(cx, |this, cx| {
                    let ix = this.app.update(cx, |app, cx| app.add_workspace(&name, cx));
                    this.parked.push(None);
                    this.switch_workspace(ix, window, cx);
                })
                .ok();
            },
        );
    }

    fn rename_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.current_workspace(cx);
        let old = self.app.read(cx).settings.workspace().name.clone();
        let app = self.app.clone();
        ask_name(
            tr("给工作区改名"),
            tr("新的名字"),
            old,
            tr("改名"),
            false,
            window,
            cx,
            move |name, _, cx| app.update(cx, |app, cx| app.rename_workspace(ix, &name, cx)),
        );
    }

    /// 删掉当前工作区（先问一下）。只剩一个时不能删；里面有标签页正在做操作时不删。
    fn delete_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.current_workspace(cx);
        let (name, count) = {
            let settings = &self.app.read(cx).settings;
            (settings.workspace().name.clone(), settings.workspaces.len())
        };
        if count <= 1 {
            let text = tr("只剩这一个工作区了，不能删。");
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        if self.tabs.iter().any(|tab| {
            let view = tab.view.read(cx);
            view.busy().is_some() || view.terminal_alive(cx)
        }) {
            let text = tr("这个工作区里有标签页正在做操作或终端还开着，先处理完再删。");
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let tabs = self.tabs.len();
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            alert
                .title(tf!("删除工作区「{name}」？", name))
                .description(if tabs == 0 {
                    tr("这个工作区里没有开着的标签页。").to_string()
                } else {
                    tf!("里面的 {tabs} 个标签页会关掉，仓库还在列表里。", tabs)
                })
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
                                    this.update(cx, |this, cx| {
                                        this.delete_workspace_now(ix, window, cx)
                                    })
                                    .ok();
                                }),
                        ),
                )
        });
    }

    fn delete_workspace_now(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.current_workspace(cx) != ix {
            return;
        }
        // 先切到旁边的一个，再把这个删掉
        let next = if ix == 0 { 1 } else { ix - 1 };
        self.switch_workspace(next, window, cx);
        if ix < self.parked.len() {
            self.parked.remove(ix);
        }
        self.app.update(cx, |app, cx| app.remove_workspace(ix, cx));
        cx.notify();
    }

    /// 把一个标签页移到另一个工作区：仓库也跟着换，不再是这个工作区的。
    fn move_tab(&mut self, ix: usize, to: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        if let Some(op) = tab.view.read(cx).busy() {
            let text = tf!("这个仓库正在{op}，等它做完再移。", op);
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        // 移过去是在那边重新打开，终端带不过去
        if tab.view.read(cx).terminal_alive(cx) {
            let text = tr("这个仓库的终端还开着，先结束终端再移。");
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let entry = tab.entry.clone();
        self.close_now(ix, cx);
        self.app.update(cx, |app, cx| {
            app.remove_from_workspace(&entry, cx);
            app.push_tab_to_workspace(to, entry.clone(), cx)
        });
        // 那个工作区的标签页已经建好了（切过去过）：给它补上这一页
        let loaded = self.parked.get(to).is_some_and(|p| p.is_some());
        if loaded {
            let tab = self.build_tab(entry.clone(), window, cx);
            if let Some(Some((tabs, _))) = self.parked.get_mut(to)
                && !tabs.iter().any(|t| t.entry == entry)
            {
                tabs.push(tab);
            }
        }
    }

    // ---- 标题栏 ----

    fn render_workspace_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = &self.app.read(cx).settings;
        let name: SharedString = settings.workspace().name.clone().into();
        let this = cx.entity().downgrade();
        let app = self.app.clone();
        div().occlude().child(
            Button::new("workspace")
                .ghost()
                .small()
                .icon(ui::icon(IconName::Layers))
                .label(name)
                .dropdown_menu(move |menu, _, cx| workspace_menu(menu, &app, &this, cx)),
        )
    }

    /// 一个标签页：图标、名字、关闭按钮（固定着的是一个图钉）。`ix` 是 `tabs` 里的下标。
    fn render_repo_tab(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let tab = &self.tabs[ix];
        let app = self.app.read(cx);
        let title = app.repo_title(&tab.entry);
        let selected = self.active == ix + 1 && !self.showing_settings;
        let (busy, changes) = {
            let view = tab.view.read(cx);
            (view.busy().is_some(), view.change_count())
        };
        let glyph = if busy {
            IconName::LoaderCircle
        } else if tab.entry.server.is_some() {
            IconName::Server
        } else {
            IconName::FolderGit2
        };
        let (accent, muted) = (cx.theme().primary, cx.theme().muted_foreground);
        let pinned = app.settings.workspace().is_pinned(&tab.entry);
        let show_close = app.settings.tab_close_button && !pinned;
        let can_reopen = !app.settings.workspace().closed.is_empty();
        let close = tab_close(("close-tab", ix), selected, cx).on_click(cx.listener(
            move |this, _, window, cx| {
                cx.stop_propagation();
                this.close(ix, window, cx);
            },
        ));
        let (menu_this, menu_app, entry) =
            (cx.entity().downgrade(), self.app.clone(), tab.entry.clone());
        // 标签页窄了名字会被截掉：停上去显示完整的名字和路径
        let (tip_title, tip_path) = (title.clone(), SharedString::from(tab.entry.path.clone()));
        tab_shell(("tab", ix), selected, cx)
            .flex_shrink(1.)
            .min_w(px(TAB_MIN_WIDTH))
            .max_w(px(210.))
            .tooltip(move |window, cx| {
                ui::two_line_tip(tip_title.clone(), tip_path.clone(), window, cx)
            })
            // 标签页自己挡住了鼠标，滚轮到不了外面那层：在这里接住，滚标签栏
            .on_scroll(cx.listener(Self::scroll_tabs))
            .on_click(cx.listener(move |this, _, _, cx| this.select(ix + 1, cx)))
            // 中键关标签页
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _, window, cx| this.close(ix, window, cx)),
            )
            .child(ui::icon(glyph).flex_none())
            .child(div().min_w_0().truncate().child(title))
            // 有没提交的改动：名字后面一个小点
            .when(changes > 0, |tab| {
                tab.child(div().flex_none().size(px(6.)).rounded_full().bg(accent))
            })
            // 固定着的没有关闭按钮，那个位置是一个图钉
            .when(pinned, |tab| {
                tab.child(
                    div()
                        .flex_none()
                        .size(px(16.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(muted)
                        .child(ui::icon_small(IconName::Pin)),
                )
            })
            .when(show_close, |tab| tab.child(close))
            .context_menu(move |menu, _, cx| {
                tab_menu(
                    menu,
                    ix,
                    entry.clone(),
                    can_reopen,
                    &menu_app,
                    &menu_this,
                    cx,
                )
            })
            .into_any_element()
    }

    /// 「所有标签页」的按钮：标签页多到要滚动时才有，点开列出全部，选一个就切过去。
    fn render_tab_list_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let app = self.app.read(cx);
        let titles: Vec<SharedString> = self
            .tabs
            .iter()
            .map(|tab| app.repo_title(&tab.entry))
            .collect();
        let active = (!self.showing_settings)
            .then(|| self.active.checked_sub(1))
            .flatten();
        let this = cx.entity().downgrade();
        div()
            .occlude()
            .flex_none()
            .child(
                Button::new("tab-list")
                    .ghost()
                    .xsmall()
                    .icon(ui::icon(IconName::ChevronDown))
                    .tooltip(tr("所有标签页"))
                    .dropdown_menu(move |mut menu, _, _| {
                        for (ix, title) in titles.iter().enumerate() {
                            let this = this.clone();
                            menu = menu.item(
                                PopupMenuItem::new(title.clone())
                                    .checked(active == Some(ix))
                                    .on_click(move |_, _, cx| {
                                        this.update(cx, |this, cx| this.select(ix + 1, cx)).ok();
                                    }),
                            );
                        }
                        menu
                    }),
            )
            .into_any_element()
    }

    fn render_title_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 标签页放不放得下要等排完版才知道：下一帧看一眼滚动范围，变了再画一次
        {
            let handle = self.tab_scroll.clone();
            cx.on_next_frame(window, move |this, _, cx| {
                let overflow = handle.max_offset().x > px(0.5);
                if overflow != this.tabs_overflow {
                    this.tabs_overflow = overflow;
                    cx.notify();
                }
            });
        }
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        let home_selected = self.active == 0 && !self.showing_settings;
        let home = tab_shell("tab-home", home_selected, cx)
            .flex_none()
            .on_click(cx.listener(|this, _, _, cx| this.select(0, cx)))
            .child(ui::icon(IconName::LayoutGrid).flex_none())
            .child(trc("页面", "仓库"));
        let repo_tabs: Vec<AnyElement> = (0..self.tabs.len())
            .map(|ix| self.render_repo_tab(ix, cx))
            .collect();
        let settings_tab = self.showing_settings.then(|| {
            tab_shell("tab-settings", true, cx)
                .flex_none()
                .child(ui::icon(IconName::Settings).flex_none())
                .child(tr("设置"))
                .child(
                    tab_close("close-settings", true, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_settings(cx))),
                )
        });
        let list_button = self.tabs_overflow.then(|| self.render_tab_list_button(cx));
        // 标签页这一段：放不下时在自己的宽度里滚动
        let strip = h_flex()
            .absolute()
            .inset_0()
            .gap(px(4.))
            .child(
                h_flex()
                    .id("tab-strip")
                    .flex_shrink(1.)
                    .min_w_0()
                    .h_full()
                    .gap(px(4.))
                    .overflow_x_scroll()
                    .track_scroll(&self.tab_scroll)
                    .children(repo_tabs)
                    .children(settings_tab),
            )
            .child(
                div().occlude().flex_none().child(
                    Button::new("tab-add")
                        .ghost()
                        .xsmall()
                        .icon(ui::icon(IconName::Plus))
                        .tooltip(tr("打开仓库（Ctrl+P）"))
                        .on_click(cx.listener(|this, _, window, cx| this.quick_open(window, cx))),
                ),
            )
            .children(list_button)
            // 这一段是空的：按住可以拖窗口
            .child(div().flex_1().min_w(px(24.)).h_full());
        TitleBar::new().h(px(TITLE_HEIGHT)).child(
            h_flex()
                .h_full()
                .flex_1()
                .min_w_0()
                .gap(px(4.))
                .child(self.render_workspace_button(cx))
                .child(div().flex_none().w(px(1.)).h(px(16.)).mx(px(4.)).bg(border))
                .child(home)
                // 标签页那一段画在这个占位的块里面（绝对定位）。标题栏的宽度按里面内容的
                // 最小宽度算，直接把标签页排在这一行里的话，开多了会把整行撑宽，
                // 右边的设置和窗口按钮被挤出窗口
                .child(div().relative().flex_1().min_w_0().h_full().child(strip))
                .child(
                    div().occlude().mr(px(4.)).text_color(muted).child(
                        Button::new("open-settings")
                            .ghost()
                            .small()
                            .icon(ui::icon(IconName::Settings))
                            .tooltip(tr("设置（Ctrl+,）"))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_settings(cx))),
                    ),
                ),
        )
    }
}

/// 标签页的外形。选中的是一块浮起来的底，没选中的只有字。
fn tab_shell(id: impl Into<ElementId>, selected: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let (bg, fg, muted, hover, border) = (
        theme.background,
        theme.foreground,
        theme.muted_foreground,
        theme.accent,
        theme.border,
    );
    h_flex()
        .id(id)
        .group("tab")
        // 标题栏整条是「拖动窗口」的区域，上面的控件要自己接住鼠标。
        // 不能用「挡鼠标但放过滚轮」的那种：系统问这一点是不是标题栏时，gpui 把被它挡住的
        // 拖动区也算上，真实鼠标点标签页就成了拖窗口
        .occlude()
        .relative()
        .h(px(28.))
        .px(px(10.))
        .gap(px(6.))
        .rounded(px(ui::RADIUS))
        .border_1()
        .text_size(px(13.))
        .cursor_pointer()
        .when(selected, |tab| {
            tab.bg(bg).border_color(border).text_color(fg)
        })
        .when(!selected, |tab| {
            tab.border_color(gpui_kit::transparent_black())
                .text_color(muted)
                .hover(|s| s.bg(hover).text_color(fg))
        })
}

/// 标签页上的关闭按钮：选中的一直显示；没选中的鼠标移上去才显示，浮在标签页右边
/// （不占名字的地方，标签页窄的时候名字能多露几个字）。
fn tab_close(id: impl Into<ElementId>, selected: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let (muted, hover, tab_hover) = (theme.muted_foreground, theme.secondary, theme.accent);
    div()
        .id(id)
        .flex_none()
        .size(px(16.))
        .rounded(px(4.))
        .flex()
        .items_center()
        .justify_center()
        .text_color(muted)
        .hover(|s| s.bg(hover))
        .when(!selected, |close| {
            close
                .absolute()
                .right(px(6.))
                .top(px(5.))
                // 底色和悬停时的标签页一样，盖住下面的字
                .bg(tab_hover)
                .invisible()
                .group_hover("tab", |s| s.visible())
        })
        .child(ui::icon_small(IconName::X))
}

/// 工作区按钮的下拉菜单：各个工作区（当前的打勾），下面是新建、改名、删除。
fn workspace_menu(
    mut menu: PopupMenu,
    app: &Entity<AppState>,
    this: &WeakEntity<Workspace>,
    cx: &App,
) -> PopupMenu {
    let settings = &app.read(cx).settings;
    let current = settings.active_workspace;
    for (ix, group) in settings.workspaces.iter().enumerate() {
        let this = this.clone();
        // 括号里是这个工作区有几个仓库，不是开着几个标签页：全关了它也不是空的
        let label = tf!("{name}（{n}）", name = group.name, n = group.repos.len());
        menu = menu.item(PopupMenuItem::new(label).checked(ix == current).on_click(
            move |_, window, cx| {
                this.update(cx, |this, cx| this.switch_workspace(ix, window, cx))
                    .ok();
            },
        ));
    }
    let (new, rename, delete) = (this.clone(), this.clone(), this.clone());
    menu.separator()
        .item(
            PopupMenuItem::new(tr("新建工作区…")).on_click(move |_, window, cx| {
                new.update(cx, |this, cx| this.new_workspace(window, cx))
                    .ok();
            }),
        )
        .item(
            PopupMenuItem::new(tr("给当前工作区改名…")).on_click(move |_, window, cx| {
                rename
                    .update(cx, |this, cx| this.rename_workspace(window, cx))
                    .ok();
            }),
        )
        .item(
            PopupMenuItem::new(tr("删除当前工作区…")).on_click(move |_, window, cx| {
                delete
                    .update(cx, |this, cx| this.delete_workspace(window, cx))
                    .ok();
            }),
        )
}

/// 标签页的右键菜单。
fn tab_menu(
    menu: PopupMenu,
    ix: usize,
    entry: RepoEntry,
    can_reopen: bool,
    app: &Entity<AppState>,
    this: &WeakEntity<Workspace>,
    cx: &App,
) -> PopupMenu {
    let (close, others, all, rename) = (this.clone(), this.clone(), this.clone(), this.clone());
    let (reopen, pin, leave) = (this.clone(), this.clone(), this.clone());
    let leave_entry = entry.clone();
    let path = entry.path.clone();
    let pinned = app.read(cx).settings.workspace().is_pinned(&entry);
    let mut menu = menu
        .item(
            PopupMenuItem::new(if pinned {
                tr("取消固定")
            } else {
                tr("固定标签页")
            })
            .on_click(move |_, _, cx| {
                pin.update(cx, |this, cx| this.toggle_pin(ix, cx)).ok();
            }),
        )
        .separator()
        .item(
            PopupMenuItem::new(tr("关闭"))
                .disabled(pinned)
                .on_click(move |_, window, cx| {
                    close.update(cx, |this, cx| this.close(ix, window, cx)).ok();
                }),
        )
        .item(
            PopupMenuItem::new(tr("关闭其他标签页")).on_click(move |_, window, cx| {
                others
                    .update(cx, |this, cx| this.close_others(Some(ix), window, cx))
                    .ok();
            }),
        )
        .item(
            PopupMenuItem::new(tr("关闭全部标签页")).on_click(move |_, window, cx| {
                all.update(cx, |this, cx| this.close_others(None, window, cx))
                    .ok();
            }),
        )
        .item(
            PopupMenuItem::new(tr("重新打开刚关掉的（Ctrl+Shift+T）"))
                .disabled(!can_reopen)
                .on_click(move |_, window, cx| {
                    reopen
                        .update(cx, |this, cx| this.reopen_closed(window, cx))
                        .ok();
                }),
        )
        .separator();
    // 移到别的工作区：有别的工作区才列出来
    let settings = &app.read(cx).settings;
    let current = settings.active_workspace;
    for (to, group) in settings.workspaces.iter().enumerate() {
        if to == current {
            continue;
        }
        let this = this.clone();
        menu = menu.item(
            PopupMenuItem::new(tf!("移到工作区「{name}」", name = group.name)).on_click(
                move |_, window, cx| {
                    this.update(cx, |this, cx| this.move_tab(ix, to, window, cx))
                        .ok();
                },
            ),
        );
    }
    menu.item(
        PopupMenuItem::new(tr("从工作区移除")).on_click(move |_, window, cx| {
            leave
                .update(cx, |this, cx| {
                    this.remove_from_workspace(leave_entry.clone(), window, cx)
                })
                .ok();
        }),
    )
    .separator()
    .item(
        PopupMenuItem::new(tr("改名…")).on_click(move |_, window, cx| {
            rename
                .update(cx, |this, cx| this.rename_repo(entry.clone(), window, cx))
                .ok();
        }),
    )
    .item(
        PopupMenuItem::new(tr("复制仓库路径")).on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
        }),
    )
}

/// 对话框里「确定」要做的事：回车和点按钮共用。
type Submit = Rc<dyn Fn(&mut Window, &mut App)>;

/// 问一个名字的小对话框：一个输入框，回车或点确定时关掉并调用 `on_ok`。
/// `allow_empty` 为假时名字是空的就不关。
#[allow(clippy::too_many_arguments)]
fn ask_name<T: 'static>(
    title: &'static str,
    placeholder: &'static str,
    value: String,
    ok: &'static str,
    allow_empty: bool,
    window: &mut Window,
    cx: &mut Context<T>,
    on_ok: impl Fn(String, &mut Window, &mut App) + 'static,
) {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
    if !value.is_empty() {
        input.update(cx, |state, cx| state.set_value(value, window, cx));
    }
    let submit: Submit = {
        let input = input.clone();
        Rc::new(move |window, cx| {
            let name = input.read(cx).value().trim().to_string();
            if name.is_empty() && !allow_empty {
                return;
            }
            window.close_dialog(cx);
            on_ok(name, window, cx);
        })
    };
    let first_build = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_build.replace(false) {
            input.focus_handle(cx).focus(window, cx);
        }
        let (on_enter, on_click) = (submit.clone(), submit.clone());
        dialog
            .title(title)
            .w(px(420.))
            .on_ok(move |_, window, cx| {
                on_enter(window, cx);
                false
            })
            .child(Input::new(&input))
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
                            .label(ok)
                            .on_click(move |_, window, cx| on_click(window, cx)),
                    ),
            )
    });
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title_bar = self.render_title_bar(window, cx).into_any_element();
        let theme = cx.theme();
        let (bg, fg) = (theme.background, theme.foreground);
        let content = if self.showing_settings {
            self.settings_page.clone().into_any_element()
        } else {
            match self.active {
                0 => self.manager.clone().into_any_element(),
                ix => self.tabs[ix - 1].view.clone().into_any_element(),
            }
        };
        v_flex()
            .size_full()
            .bg(bg)
            .text_color(fg)
            .child(ui::tooltip_guard())
            .child(title_bar)
            .child(div().flex_1().min_h_0().child(content))
    }
}

/// 快速打开里选中了一个仓库。
struct Picked(RepoEntry);

/// 快速打开：一个输入框加仓库列表，输入的字按名字和路径筛选。
struct QuickOpen {
    app: Entity<AppState>,
    input: Entity<InputState>,
    /// 上下键选中的是匹配结果里的第几个
    selected: usize,
    _subscription: Subscription,
}

impl EventEmitter<Picked> for QuickOpen {}

impl QuickOpen {
    fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("仓库名或路径里的几个字")));
        let subscription = cx.subscribe(&input, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                // 筛选的字变了：回到第一个
                this.selected = 0;
                cx.notify();
            }
        });
        Self {
            app,
            input,
            selected: 0,
            _subscription: subscription,
        }
    }

    /// 匹配输入内容的仓库：名字、路径、服务器名里有这几个字（不分大小写）。
    /// 先是这个工作区里刚关掉的（最近关的在最上），再是这个工作区别的仓库，最后是其余的。
    /// 第三项是行尾标的小字：刚关掉、开着。
    fn matches(&self, cx: &App) -> Vec<(RepoEntry, SharedString, Option<&'static str>)> {
        let needle = self.input.read(cx).value().trim().to_lowercase();
        let app = self.app.read(cx);
        let group = app.settings.workspace();
        let closed = app.closed_tabs();
        let members = group
            .repos
            .iter()
            .filter(|entry| !closed.contains(entry))
            .map(|entry| (entry.clone(), group.tabs.contains(entry).then(|| tr("开着"))));
        let rest = app
            .settings
            .repos
            .iter()
            .filter(|entry| !group.repos.contains(entry))
            .map(|entry| (entry.clone(), None));
        closed
            .iter()
            .map(|entry| (entry.clone(), Some(tr("刚关掉"))))
            .chain(members)
            .chain(rest)
            .map(|(entry, mark)| {
                let title = app.repo_title(&entry);
                (entry, title, mark)
            })
            .filter(|(entry, title, _)| {
                needle.is_empty()
                    || title.to_lowercase().contains(&needle)
                    || entry.path.to_lowercase().contains(&needle)
            })
            .collect()
    }

    fn pick_selected(&mut self, cx: &mut Context<Self>) {
        if let Some((entry, ..)) = self.matches(cx).into_iter().nth(self.selected) {
            cx.emit(Picked(entry));
        }
    }

    /// 上下键：在匹配结果里移动，到头绕回去。
    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let count = self.matches(cx).len();
        if count == 0 {
            return;
        }
        match event.keystroke.key.as_str() {
            "down" => self.selected = (self.selected + 1) % count,
            "up" => self.selected = (self.selected + count - 1) % count,
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Render for QuickOpen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list_height = (window.viewport_size().height - px(240.)).clamp(px(120.), px(360.));
        let theme = cx.theme();
        let (muted, hover, active) = (theme.muted_foreground, theme.list_hover, theme.list_active);
        let matches = self.matches(cx);
        let selected = self.selected.min(matches.len().saturating_sub(1));
        let rows: Vec<AnyElement> = matches
            .into_iter()
            .enumerate()
            .map(|(ix, (entry, title, mark))| {
                let path = entry.path.clone();
                let glyph = if entry.server.is_some() {
                    IconName::Server
                } else {
                    IconName::FolderGit2
                };
                h_flex()
                    .id(ix)
                    .px_2()
                    .py_1()
                    .gap_2()
                    .rounded(px(ui::RADIUS))
                    .cursor_pointer()
                    // 高亮的是回车会打开的
                    .when(ix == selected, |row| row.bg(active))
                    .when(ix != selected, |row| row.hover(|s| s.bg(hover)))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(Picked(entry.clone()))))
                    .child(ui::icon(glyph).flex_none().text_color(muted))
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(ui::clipped(("quick-title", ix), title).text_sm())
                            .child(
                                ui::clipped(("quick-path", ix), path)
                                    .text_xs()
                                    .text_color(muted),
                            ),
                    )
                    .when_some(mark, |row, mark| {
                        row.child(div().flex_1()).child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(muted)
                                .child(mark),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        v_flex()
            .gap_2()
            .capture_key_down(cx.listener(Self::on_key))
            .child(Input::new(&self.input))
            .child(
                div()
                    .id("quick-open-list")
                    .max_h(list_height)
                    .overflow_y_scroll()
                    .child(v_flex().gap_1().children(rows))
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
    }
}
