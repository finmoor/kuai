//! 主窗口：标题栏（工作区、标签页、设置）和下面的内容。
//!
//! 标题栏是自己画的，标签页直接放在里面，不另占一行。第一个标签页是仓库管理器，后面每个
//! 打开的仓库一个；设置页打开时也是一个标签页。标签页开多了先一起变窄，窄到头了横向滚动，
//! 右边多出一个列出全部标签页的按钮；设置和窗口的按钮一直留在原地。
//!
//! 工作区是一组标签页：切到另一个工作区，标签栏整个换成它的那一组。别的工作区里已经打开过
//! 的标签页留在内存里（正在做的操作不会被打断），只是不显示。工作区保存在它所在的电脑上
//! （本机或服务器，见 `machines`），菜单里按电脑分组；开着哪些标签页各台电脑各记各的。
//!
//! 快捷键：F5 刷新、Ctrl+W 关标签页、Ctrl+Tab 切标签页、Ctrl+F 搜索提交、Ctrl+P 快速打开仓库、
//! Ctrl+1 本地修改、Ctrl+2 所有提交、Ctrl+, 设置、Ctrl+Shift+T 重新打开刚关掉的标签页、
//! Ctrl+` 当前仓库在 Git、终端两页之间切换、Ctrl+Shift+B 收起、展开侧栏。
//!
//! 有新版本时标题栏上齿轮左边多一个小牌（`update_view.rs`），点「重启完成升级」后这里先问一下
//! 本机终端，再换上新版本、拉起新的 Kuai、自己退出。
//!
//! 本机终端活在 Kuai 进程里，关标签页不结束（留到再打开这个仓库时接回，见 `repo/terminals.rs`），
//! 退出 Kuai 才结束：关窗口真要退出、托盘菜单的「退出」、重启以完成更新，都先数一下还有几个
//! 本机终端在运行，有就问一下；有托盘时可以改为最小化到托盘，终端接着运行。
//!
//! 关标签页时：有写操作在做就先拦一下；提交说明框里没提交的内容记下来，再打开这个仓库时放回去。
//! 关掉的标签页每个工作区记最近 10 个（存在设置里），手滑关错了能找回来；
//! 设置里还可以把标签页上的关闭按钮藏起来。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use fr_core::i18n::{self, tr, trc};
use fr_core::tf;
use futures::StreamExt as _;
use futures::channel::mpsc;
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
use crate::machines::{LOCAL, WorkspaceKey};
use crate::manager::{ManagerEvent, ManagerView};
use crate::prompt::{self, Prompts};
use crate::repo::{RepoEvent, RepoView};
use crate::settings::RepoEntry;
use crate::settings_page::SettingsPage;
use crate::term_board::{BoardHandle, Row as TermRow, Shown, Stopped, TermBoard, TermId};
use crate::term_overview::{Go, TermOverview};
use crate::toast::Toasts;
use crate::tray::{Tray, TrayAction};
use crate::ui;
use crate::update::{self, UpdateEvent, Updater};
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

impl RepoTab {
    /// 这个标签页上显示着的仓库页：进到了子仓库、别的工作树里就是最里面的那一个，快捷键对它生效。
    fn shown(&self, cx: &App) -> Entity<RepoView> {
        let mut shown = self.view.clone();
        while let Some(inner) = shown.read(cx).active_sub() {
            shown = inner;
        }
        shown
    }
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
    updater: Entity<Updater>,
    manager: Entity<ManagerView>,
    settings_page: Entity<SettingsPage>,
    /// 当前工作区的标签页
    tabs: Vec<RepoTab>,
    /// 0 是仓库管理器，i 是 `tabs[i - 1]`
    active: usize,
    /// `tabs` 是哪个工作区的。和设置里现在用的不一样了（别的电脑删了这个工作区、连不上服务器换回了
    /// 本机的），就换成那一组
    shown: WorkspaceKey,
    /// 其他工作区里已经建好的标签页
    parked: HashMap<WorkspaceKey, Parked>,
    /// 设置页开着；它盖在当前标签页上面，关掉回到原来那一页
    showing_settings: bool,
    /// 所有工作区里的终端现在怎么样（见 `term_board`）
    board: Entity<TermBoard>,
    /// 终端总览页（标题栏上「仓库」右边的「终端」，Ctrl+Shift+A）
    overview: Entity<TermOverview>,
    /// 终端总览页开着；和设置页一样盖在当前标签页上面，再按一次回到原来那一页
    showing_terms: bool,
    /// 上一次告诉 `board` 的「正显示着哪个终端」
    told_shown: Option<Shown>,
    /// Kuai 不在前台时发的系统通知（终端停下了）
    /// 系统通知。发、收回都要跨进程调用系统，等的时候系统会接着给窗口派消息，窗口要画，所以不能在
    /// 处理界面更新的半路上做（做了日志里就是几条 `RefCell already borrowed`，少画一帧；1.15 起
    /// 就有，2026-11-18 查到）：都经 `toast_jobs` 交给主线程上的一个任务，一件一件做。这里留着的
    /// 这一份只在退出时用
    toasts: Rc<RefCell<Toasts>>,
    toast_jobs: mpsc::UnboundedSender<ToastJob>,
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
        updater: Entity<Updater>,
        prompts: Prompts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let manager = cx.new(|cx| ManagerView::new(app.clone(), cx));
        let settings_page = cx.new(|cx| SettingsPage::new(app.clone(), updater.clone(), cx));
        let board = cx.new(|cx| TermBoard::new(app.clone(), cx));
        cx.set_global(BoardHandle(board.clone()));
        let overview = cx.new(|cx| TermOverview::new(app.clone(), board.clone(), window, cx));
        let quitting = Rc::new(Cell::new(false));
        let subscriptions = vec![
            cx.subscribe_in(&manager, window, |this, _, event, window, cx| match event {
                ManagerEvent::Open(entry) => this.open(entry.clone(), window, cx),
                ManagerEvent::Rename(entry) => this.rename_repo(entry.clone(), window, cx),
                ManagerEvent::RemoveFromWorkspace(entry) => {
                    this.remove_from_workspace(entry.clone(), window, cx)
                }
                ManagerEvent::QuickOpen => this.quick_open(window, cx),
                ManagerEvent::SwitchTo(key) => this.switch_workspace(key.clone(), window, cx),
            }),
            // 设置变了：服务器改名要刷新标签上的名字；服务器被移除了，它上面的仓库的标签页跟着关掉；
            // 现在用的工作区换了（不在了、连不上回到本机）就换上那一组；
            // 主题、语言、托盘的选项变了就应用到窗口上
            cx.observe_in(&app, window, |this, _, window, cx| {
                this.drop_tabs_of_removed_servers(cx);
                this.follow_current(window, cx);
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
            // 最小化到托盘：窗口失去焦点时看一眼是不是最小化了。
            // 窗口不在前台时，显示着的终端不算「正看着」：重画一遍，把这个告诉 `board`
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.hide_if_minimized(cx);
                } else {
                    // 回到 Kuai 了：系统通知都收回，哪些停下了看标题栏上的个数
                    this.toast_job(ToastJob::Clear);
                }
                cx.notify();
            }),
            // 标题栏上「终端」那一项的个数跟着变；总览页里换了选中的，也要重新告诉 `board`
            cx.observe(&board, |_, _, cx| cx.notify()),
            cx.observe(&overview, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &overview,
                window,
                |this, _, event: &Go, window, cx| match event {
                    Go::Term(row) => this.go_to_terminal(row, window, cx),
                    Go::Repo(entry) => this.go_to_repo_terminals(entry, window, cx),
                },
            ),
            cx.subscribe_in(&board, window, |this, _, event: &Stopped, window, cx| {
                this.terminal_stopped(&event.0, window, cx)
            }),
            // 升级：小牌跟着检查、下载的进度变；点了「重启完成升级」先问一下再重启
            cx.observe(&updater, |_, _, cx| cx.notify()),
            cx.subscribe_in(&updater, window, |this, _, event, window, cx| match event {
                UpdateEvent::Restart => this.restart_for_update(window, cx),
            }),
            // 退出时先撤掉托盘图标：进程退出前没撤的话，托盘里会留一个点不开的图标，鼠标移过去才消失
            cx.on_app_quit(|this, cx| {
                this.tray = None;
                // 退出以后系统通知点不动了，都收回。排队的任务这时不会再跑了，当场做；它正做到一半
                // 就借不到，那一条留着也无妨
                if let Ok(mut toasts) = this.toasts.try_borrow_mut() {
                    toasts.clear();
                }
                // 映射到本机的目录都断开（等在路上的写入落到服务器）
                this.app.update(cx, |app, _| app.unmount_all());
                async {}
            }),
            // 映射那边要说的话（映射上了、没成、写入落不到服务器）
            cx.subscribe_in(
                &app,
                window,
                |_, app, notice: &crate::mapping::Notice, window, cx| {
                    let note = match notice {
                        crate::mapping::Notice::Info(text) => Notification::info(text.clone()),
                        crate::mapping::Notice::Warning(text) => {
                            Notification::warning(text.clone())
                        }
                        crate::mapping::Notice::Error(text) => Notification::error(text.clone()),
                        crate::mapping::Notice::NeedWinFsp => {
                            let app = app.clone();
                            let text = tr("映射需要先安装 WinFsp。");
                            Notification::warning(text).action(move |_, _, cx| {
                                let (app, note) = (app.clone(), cx.entity().downgrade());
                                Button::new("install-winfsp")
                                    .small()
                                    .primary()
                                    .label(tr("安装 WinFsp"))
                                    .on_click(move |_, window, cx| {
                                        app.update(cx, |app, cx| app.install_winfsp(cx));
                                        // 装的进度和结果另有提示，这一条收起来
                                        note.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                                    })
                            })
                        }
                        crate::mapping::Notice::InstallFailed => {
                            let text = tr("WinFsp 安装未完成。");
                            Notification::error(text).action(move |_, _, cx| {
                                let note = cx.entity().downgrade();
                                Button::new("winfsp-download")
                                    .small()
                                    .label(tr("打开下载页面"))
                                    .on_click(move |_, window, cx| {
                                        let result = crate::mapping::open_winfsp_download();
                                        crate::shell::report(result, window, cx);
                                        note.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                                    })
                            })
                        }
                    };
                    window.push_notification(note, cx);
                },
            ),
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
                // 真要退出：本机终端里还有程序在运行就先问一下
                if quitting.get() {
                    return true;
                }
                let asked = this
                    .update(cx, |this, cx| this.confirm_quit(window, cx))
                    .unwrap_or(false);
                !asked
            });
        }
        // 刚升级完：说一声，给个看更新说明的按钮
        if updater.read(cx).updated_from.is_some() {
            cx.defer_in(window, |_, window, cx| {
                let version = env!("CARGO_PKG_VERSION");
                let notification = Notification::success(tf!("已更新到 {version}", version)).action(
                    move |_, _, _| {
                        Button::new("update-notes")
                            .small()
                            .label(tr("查看更新内容"))
                            .on_click(move |_, window, cx| {
                                let page =
                                    format!("{}/releases/tag/v{version}", update::repo_url());
                                crate::shell::report(crate::shell::open_url(&page), window, cx);
                            })
                    },
                );
                window.push_notification(notification, cx);
            });
        }
        // 点了系统通知：回到 Kuai，前往那个终端
        let (toasts, mut clicks) = Toasts::new();
        let toasts = Rc::new(RefCell::new(toasts));
        let (toast_jobs, mut jobs) = mpsc::unbounded();
        cx.spawn({
            let toasts = toasts.clone();
            async move |_, _| {
                while let Some(job) = jobs.next().await {
                    let Ok(mut toasts) = toasts.try_borrow_mut() else {
                        continue;
                    };
                    match job {
                        ToastJob::Show { key, title, body } => toasts.show(&key, &title, &body),
                        ToastJob::Remove(key) => toasts.remove(&key),
                        ToastJob::Clear => toasts.clear(),
                    }
                }
            }
        })
        .detach();
        cx.spawn_in(window, async move |this, cx| {
            while let Some(tag) = clicks.next().await {
                let alive = this.update_in(cx, |this, window, cx| {
                    this.toast_clicked(&tag, window, cx);
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        let mut this = Self {
            app: app.clone(),
            updater,
            manager,
            settings_page,
            tabs: Vec::new(),
            active: 0,
            shown: app.read(cx).current().clone(),
            parked: HashMap::new(),
            showing_settings: false,
            board,
            overview,
            showing_terms: false,
            told_shown: None,
            toasts,
            toast_jobs,
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
        let view = app.read(cx).view();
        for entry in view.tabs {
            let tab = this.build_tab(entry, window, cx);
            this.tabs.push(tab);
        }
        this.active = view.active.min(this.tabs.len());
        this.update_visibility(cx);
        this.apply_settings(window, cx);
        // 上次用的是服务器上的工作区：连上它；连不上就回到本机的工作区
        let machine = this.shown.machine.clone();
        if machine != LOCAL {
            cx.defer_in(window, move |this, window, cx| {
                this.ensure_connected(machine, window, cx)
            });
        }
        this
    }

    /// 用的是服务器 `machine` 上的工作区：连上它（会读到它的工作区）。连不上就回到本机最近用的
    /// 工作区，说一声；那个工作区开着的标签页本机记着，连上以后切回去还在。
    fn ensure_connected(&mut self, machine: String, window: &mut Window, cx: &mut Context<Self>) {
        let connecting = self.app.update(cx, |app, cx| app.connect(&machine, cx));
        cx.spawn_in(window, async move |this, cx| {
            let Err(err) = connecting.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                // 已经换到别处了就不管
                if this.app.read(cx).current().machine != machine {
                    return;
                }
                log::warn!("连不上 {machine}，回到本机的工作区：{err}");
                let key = this.app.read(cx).recent_local();
                this.switch_workspace(key, window, cx);
                let (name, workspace) = {
                    let app = this.app.read(cx);
                    (
                        app.machine_name(&machine),
                        app.workspace_name(app.current()),
                    )
                };
                let text = tf!(
                    "无法连接 {name}，已切换到本机的工作区「{workspace}」",
                    name,
                    workspace
                );
                window.push_notification(Notification::warning(text), cx);
            })
            .ok();
        })
        .detach();
    }

    /// 设置里现在用的工作区和显示着的不一样了（别的电脑删了这个工作区，或者连不上回到了本机的）：
    /// 换成现在用的那一组。
    fn follow_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.app.read(cx).current().clone();
        if current != self.shown && !current.id.is_empty() {
            self.load_workspace(current, window, cx);
        }
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
                    .chain(self.parked.values().flat_map(|(tabs, _)| tabs.iter()))
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

    /// 点了「重启完成升级」：本机终端里还有程序在跑就先问一下（服务器上的终端开在 kuaid 里，
    /// 不受影响），再换上新版本、拉起新的 Kuai、自己退出。换没成的话，小牌上显示原因。
    fn restart_for_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let terminals = self.live_terminals(cx);
        let (updater, quitting) = (self.updater.clone(), self.quitting.clone());
        let restart = Rc::new(move |cx: &mut App| {
            if updater.update(cx, |u, cx| u.install_and_relaunch(cx)) {
                quitting.set(true);
                cx.quit();
            }
        });
        if terminals == 0 {
            restart(cx);
            return;
        }
        window.open_alert_dialog(cx, move |alert, _, _| {
            let restart = restart.clone();
            alert
                .title(tr("重启 Kuai 以完成更新"))
                .description(tf!(
                    "有 {n} 个本机终端正在运行，重启将结束其中的程序。服务器上的终端不受影响。",
                    n = terminals
                ))
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
                                .label(tr("重启"))
                                .with_variant(ButtonVariant::Primary)
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    restart(cx);
                                }),
                        ),
                )
        });
    }

    /// 把藏到托盘、最小化或者被挡住的窗口叫到最前面。
    ///
    /// 放到这一轮更新之后再做：`ShowWindow` 会同步调用窗口过程，GPUI 在那里还要借用整个 App，
    /// 在更新里直接调会撞上正借着的 App（日志里的 `RefCell already borrowed`，这一帧可能不重画）。
    fn show_window(&self, cx: &mut Context<Self>) {
        if let Some(hwnd) = self.hwnd {
            cx.spawn(async move |_, _| win::show(hwnd)).detach();
        }
        // 常驻托盘时很少真的重启：再次启动、点托盘把窗口叫回来，就当作启动，顺便检查更新
        self.updater
            .update(cx, |updater, cx| updater.window_shown(cx));
    }

    /// 把窗口藏到托盘。和 [`Self::show_window`] 一样放到这一轮更新之后再做。
    fn hide_window(&self, cx: &mut Context<Self>) {
        if let Some(hwnd) = self.hwnd {
            cx.spawn(async move |_, _| win::hide(hwnd)).detach();
        }
    }

    fn on_tray(&mut self, action: TrayAction, cx: &mut Context<Self>) {
        match action {
            TrayAction::Show => self.show_window(cx),
            TrayAction::Quit => self.quit_from_tray(cx),
        }
    }

    /// 托盘菜单的「退出」：本机终端里没有程序在运行就直接退出；有就把窗口叫出来，问一下。
    fn quit_from_tray(&mut self, cx: &mut Context<Self>) {
        let mapped = self.app.read(cx).mapped_open_files();
        if self.live_terminals(cx) == 0 && self.unsaved_files(cx) == 0 && mapped == 0 {
            self.quitting.set(true);
            cx.quit();
            return;
        }
        self.show_window(cx);
        let this = cx.entity().downgrade();
        crate::with_main_window(cx, move |window, cx| {
            this.update(cx, |this, cx| this.confirm_quit(window, cx))
                .ok();
        });
    }

    /// 退出前问一下：本机终端里还有程序在运行，退出将结束它们（服务器上的终端不受影响）。
    /// 有托盘时多一个「最小化到托盘」，窗口藏起来，终端接着运行。返回问了没有：一个都没有时
    /// 不问，由调用的地方直接退出。
    fn confirm_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let terminals = self.live_terminals(cx);
        let unsaved = self.unsaved_files(cx);
        // 映射到本机的目录里，别的程序还开着的文件：退出以后映射就没了
        let mapped = self.app.read(cx).mapped_open_files();
        if terminals == 0 && unsaved == 0 && mapped == 0 {
            return false;
        }
        let mapped_note = tf!(
            "其他程序在映射的目录中打开了 {n} 个文件，退出后将无法继续读写。",
            n = mapped
        );
        let description = match (terminals, unsaved) {
            (0, 0) => mapped_note.clone(),
            (_, 0) => tf!(
                "有 {n} 个本机终端正在运行，退出将结束其中的程序。服务器上的终端不受影响。",
                n = terminals
            ),
            (0, _) => tf!("有 {n} 个文件尚未保存，退出后修改将丢失。", n = unsaved),
            _ => tf!(
                "有 {n} 个文件尚未保存，退出后修改将丢失。另有 {m} 个本机终端正在运行，退出将结束其中的程序。",
                n = unsaved,
                m = terminals
            ),
        };
        let description = match mapped {
            0 => description,
            _ if terminals == 0 && unsaved == 0 => description,
            _ => format!("{description}\n{mapped_note}"),
        };
        let ok = match (terminals, unsaved) {
            (0, 0) => tr("退出"),
            (0, _) => tr("放弃修改并退出"),
            _ => tr("结束并退出"),
        };
        let quitting = self.quitting.clone();
        let to_tray = self.tray.is_some().then(|| cx.entity().downgrade());
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (quitting, to_tray) = (quitting.clone(), to_tray.clone());
            alert
                .title(tr("退出 Kuai？"))
                .description(description.clone())
                .on_ok(|_, _, _| false)
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .label(tr("取消"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .when_some(to_tray, |footer, this| {
                            footer.child(Button::new("tray").label(tr("最小化到托盘")).on_click(
                                move |_, window, cx| {
                                    window.close_dialog(cx);
                                    this.update(cx, |this, cx| this.hide_window(cx)).ok();
                                },
                            ))
                        })
                        .child(
                            Button::new("ok")
                                .label(ok)
                                .with_variant(ButtonVariant::Danger)
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    quitting.set(true);
                                    cx.quit();
                                }),
                        ),
                )
        });
        true
    }

    /// 最小化到托盘：窗口最小化了、设置里开着、托盘图标也在，就把窗口藏起来。
    /// 点任务栏上的按钮最小化的不藏：留在任务栏上，再点一下就回来。
    fn hide_if_minimized(&mut self, cx: &mut Context<Self>) {
        let on = self.app.read(cx).settings.minimize_to_tray && self.tray.is_some();
        if let (true, Some(hwnd)) = (on, self.hwnd)
            && win::is_minimized(hwnd)
            && !win::cursor_on_taskbar()
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

    /// 打开本机的目录（命令行给的、再次启动时交过来的）：在仓库中就打开仓库，不是仓库先问是否初始化。
    /// 现在用的是服务器上的工作区（放不进本机的仓库）：先回到本机最近用的工作区。
    pub fn open_local_dir(&mut self, dir: String, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.clone();
        crate::open_dir::open_dir(
            &app,
            None,
            dir,
            window,
            cx,
            |this: &mut Self, entry, window, cx| {
                if !this.app.read(cx).current().is_local() {
                    let key = this.app.read(cx).recent_local();
                    this.switch_workspace(key, window, cx);
                }
                this.open(entry, window, cx);
            },
        );
    }

    /// 已经运行时又启动了一次 Kuai（开始菜单、命令行带目录）：显示窗口，带了目录就打开。
    pub fn on_launch_again(
        &mut self,
        request: crate::instance::Request,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if request.show {
            self.show_window(cx);
        }
        if let Some(dir) = request.open {
            self.open_local_dir(dir, window, cx);
        }
    }

    /// 各个工作区里打开的标签页中，一共有几个文件没保存（退出 Kuai 前要问）。
    fn unsaved_files(&self, cx: &App) -> usize {
        self.tabs
            .iter()
            .chain(self.parked.values().flat_map(|(tabs, _)| tabs.iter()))
            .map(|tab| tab.view.read(cx).unsaved_with_subs(cx))
            .sum()
    }

    /// 一共有几个本机终端的程序正在运行（退出 Kuai 时将结束）：各个工作区里打开的标签页中的，
    /// 加上关掉的标签页留下的。
    fn live_terminals(&self, cx: &App) -> usize {
        let open: usize = self
            .tabs
            .iter()
            .chain(self.parked.values().flat_map(|(tabs, _)| tabs.iter()))
            .map(|tab| tab.view.read(cx).local_terminals_with_subs(cx))
            .sum();
        open + crate::repo::parked_terminals_running(cx)
    }

    /// 服务器被移除了：它上面的仓库的标签页跟着关掉（各个工作区里的都关），经它连的也关。
    fn drop_tabs_of_removed_servers(&mut self, cx: &mut Context<Self>) {
        let app = self.app.read(cx);
        let gone = |entry: &RepoEntry| {
            entry.server.as_deref().is_some_and(|id| {
                let direct = crate::machines::split_relay(id).map_or(id, |(via, _)| via);
                app.server(direct).is_none()
            })
        };
        // 服务器上的工作区整个不在了（服务器移除了）
        self.parked
            .retain(|key, _| key.is_local() || app.server(&key.machine).is_some());
        for parked in self.parked.values_mut() {
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
            let visible = self.active == ix + 1 && !self.showing_settings && !self.showing_terms;
            tab.view
                .update(cx, |view, cx| view.set_visible(visible, cx));
        }
    }

    /// 打开仓库：已经开着就切过去。新开的成了当前工作区的仓库，也进它所在那台电脑的列表。
    pub fn open(&mut self, entry: RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        match self.tabs.iter().position(|t| t.entry == entry) {
            Some(ix) => self.active = ix + 1,
            None => {
                self.app.update(cx, |app, cx| app.adopt(&entry, cx));
                let tab = self.build_tab(entry, window, cx);
                self.tabs.push(tab);
                self.active = self.tabs.len();
            }
        }
        self.showing_settings = false;
        self.leave_terms(cx);
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
    /// 对话框里写着仓库的路径，可以复制。
    fn rename_repo(&mut self, entry: RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let current = app.read(cx).repo_name(&entry).to_string();
        ask_name(
            tr("重命名仓库"),
            tr("显示名称，留空则使用目录名"),
            current,
            tr("重命名"),
            true,
            Some(entry.path.clone().into()),
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
            RepoEvent::Open(entry) => this.open(entry.clone(), window, cx),
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
    /// 终端里的程序不用管：关掉以后接着运行，再打开这个仓库时接回来。
    fn close(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        if self.is_pinned(&tab.entry, cx) {
            let text = tr("此标签页已固定，取消固定后才能关闭。");
            window.push_notification(Notification::info(text), cx);
            return;
        }
        let (busy, cancellable) = {
            let view = tab.view.read(cx);
            (view.busy_with_subs(cx), view.cancellable_with_subs(cx))
        };
        let entry = tab.entry.clone();
        let unsaved = tab.view.read(cx).unsaved_with_subs(cx);
        if busy.is_none() && unsaved > 0 {
            let text = tf!("有 {n} 个文件尚未保存，关闭后修改将丢失。", n = unsaved);
            self.confirm_close(entry, text, tr("放弃修改并关闭"), window, cx);
            return;
        }
        let Some(op) = busy else {
            self.close_now(ix, cx);
            return;
        };
        if !cancellable {
            let text = tf!("此仓库正在{op}，请等待完成后再关闭。", op);
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let text = tf!("此仓库正在{op}，关闭标签页将取消该操作。", op);
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
                .title(tr("关闭此标签页？"))
                .description(description.clone())
                // 回车不执行，要明确点按钮
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
        let pinned = self.app.read(cx).view().pinned;
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
        self.app.read(cx).view().is_pinned(entry)
    }

    /// 把仓库从当前工作区移除（先问一下）：开着的标签页跟着关，仓库还在「全部仓库」里。
    fn remove_from_workspace(
        &mut self,
        entry: RepoEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab = self.tabs.iter().find(|tab| tab.entry == entry);
        if let Some(op) = tab.and_then(|tab| tab.view.read(cx).busy_with_subs(cx)) {
            let text = tf!("此仓库正在{op}，请等待完成后再移除。", op);
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let open = tab.is_some();
        let (name, workspace, machine) = {
            let app = self.app.read(cx);
            let current = app.current().clone();
            if !app.writable(&current.machine) {
                let text = tf!(
                    "与 {name} 的连接已断开，暂时无法修改。",
                    name = app.machine_name(&current.machine)
                );
                window.push_notification(Notification::warning(text), cx);
                return;
            }
            (
                app.repo_name(&entry),
                app.workspace_name(&current),
                (!current.is_local()).then(|| app.machine_name(&current.machine)),
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
        description.push_str(if open {
            tr("已打开的标签页将关闭，仓库仍保留在「全部仓库」中。")
        } else {
            tr("仓库仍保留在「全部仓库」中。")
        });
        let this = cx.entity().downgrade();
        crate::manager::confirm(
            tf!("将 {name} 从工作区「{workspace}」移除？", name, workspace),
            description,
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
    /// 正在做操作的也留着，并说一声。
    fn close_others(&mut self, keep: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let keep_entry = keep
            .and_then(|ix| self.tabs.get(ix))
            .map(|tab| tab.entry.clone());
        let pinned = self.app.read(cx).view().pinned;
        let mut skipped = 0;
        let targets: Vec<RepoEntry> = self
            .tabs
            .iter()
            .filter(|tab| Some(&tab.entry) != keep_entry.as_ref())
            .filter(|tab| !pinned.contains(&tab.entry))
            .filter(|tab| {
                let busy = tab.view.read(cx).busy_with_subs(cx).is_some();
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
            let text = tf!("{n} 个标签页正在执行操作，未关闭。", n = skipped);
            window.push_notification(Notification::warning(text), cx);
        }
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.active = ix.min(self.tabs.len());
        self.showing_settings = false;
        self.leave_terms(cx);
        self.reveal_active_tab();
        self.persist(cx);
        cx.notify();
    }

    /// 打开设置页（Ctrl+,）；已经开着就关掉。
    pub fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.showing_settings = !self.showing_settings;
        self.leave_terms(cx);
        self.update_visibility(cx);
        cx.notify();
    }

    /// 打开终端总览页（Ctrl+Shift+A，标题栏上的「终端」）；已经开着就关掉，回到原来那一页。
    pub fn toggle_terms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.showing_terms {
            self.leave_terms(cx);
            // 焦点还在看不见的总览页上：放回显示出来的终端里
            if let Some(tab) = self.active_repo() {
                tab.shown(cx)
                    .update(cx, |view, cx| view.focus_shown_terminal(window, cx));
            }
        } else {
            self.showing_terms = true;
            self.showing_settings = false;
            self.overview
                .update(cx, |overview, cx| overview.shown(window, cx));
        }
        self.update_visibility(cx);
        cx.notify();
    }

    /// 不在终端总览页上了。
    fn leave_terms(&mut self, cx: &mut Context<Self>) {
        if self.showing_terms {
            self.showing_terms = false;
            self.overview.update(cx, |overview, cx| overview.hidden(cx));
        }
    }

    /// 从终端总览页前往一个终端：切到它所在的工作区，打开那个仓库的标签页，到终端页选中它。
    fn go_to_terminal(&mut self, row: &TermRow, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self
            .board
            .read(cx)
            .groups(cx)
            .into_iter()
            .find(|group| group.rows.iter().any(|other| other.id == row.id))
            .and_then(|group| group.workspace);
        let moved = workspace.as_ref().is_some_and(|key| *key != self.shown);
        log::info!(
            "前往终端：{} 的第 {} 个，{}",
            row.entry.path,
            row.number,
            if moved {
                "要换工作区"
            } else {
                "不换工作区"
            }
        );
        if let Some(key) = workspace.filter(|_| moved) {
            self.switch_workspace(key, window, cx);
        }
        self.open(row.entry.clone(), window, cx);
        if let Some(tab) = self.active.checked_sub(1).and_then(|ix| self.tabs.get(ix)) {
            let number = row.number;
            tab.view
                .update(cx, |view, cx| view.show_terminal_number(number, window, cx));
        }
        self.board
            .update(cx, |board, cx| board.mark_seen(&row.id, cx));
        // 说它停下了的那条提醒用不着了
        let key = stopped_note_key(&row.id);
        self.toast_job(ToastJob::Remove(key.to_string()));
        window.remove_notification1::<TerminalStopped>(key, cx);
        if moved {
            let name = {
                let app = self.app.read(cx);
                app.workspace_name(app.current())
            };
            let text = tf!("已切换到工作区「{name}」", name);
            window.push_notification(Notification::info(text), cx);
        }
    }

    /// 从终端总览页前往一个仓库的终端页：切到它所在的工作区，打开它的标签页。
    fn go_to_repo_terminals(
        &mut self,
        entry: &RepoEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self
            .board
            .read(cx)
            .repos(cx)
            .into_iter()
            .find(|group| group.repos.iter().any(|(other, _)| other == entry))
            .and_then(|group| group.workspace);
        if let Some(key) = workspace.filter(|key| *key != self.shown) {
            self.switch_workspace(key, window, cx);
        }
        self.open(entry.clone(), window, cx);
        if let Some(tab) = self.active.checked_sub(1).and_then(|ix| self.tabs.get(ix)) {
            tab.view
                .update(cx, |view, cx| view.show_terminal_page(window, cx));
        }
    }

    /// 点了系统通知：把窗口叫到前面，前往那个终端（它已经不在了就只叫窗口）。
    fn toast_clicked(&mut self, tag: &str, window: &mut Window, cx: &mut Context<Self>) {
        log::info!("点了系统通知：{tag}");
        self.show_window(cx);
        let row = tag.rsplit_once('/').and_then(|(machine, term)| {
            let id = TermId {
                machine: machine.to_string(),
                term: term.to_string(),
            };
            self.board.read(cx).row(&id, cx)
        });
        if let Some(row) = row {
            self.go_to_terminal(&row, window, cx);
        }
    }

    /// 一个不在眼前的终端停下了：右下角说一声，带「前往」；Kuai 不在前台时任务栏上的按钮闪，
    /// 再发一条系统通知（设置里可以关）。
    fn terminal_stopped(&mut self, row: &TermRow, window: &mut Window, cx: &mut Context<Self>) {
        if !self.app.read(cx).settings.notify_terminal_stopped {
            return;
        }
        let repo = self.app.read(cx).repo_name(&row.entry);
        let text = match &row.title {
            Some(title) => tf!("{repo} 的终端已停下：{title}", repo, title),
            None => tf!("{repo} 的终端已停下", repo),
        };
        // 系统通知：标题同右下角那条，正文是屏幕上最后一行（多半是程序在问什么）
        let (toast_key, toast_title, toast_body) =
            (stopped_note_key(&row.id), text.clone(), row.last.clone());
        let (this, key, row) = (
            cx.entity().downgrade(),
            stopped_note_key(&row.id),
            row.clone(),
        );
        // 同一个终端只留一条；带按钮的通知默认不会自己收起，这里要它几秒后收起（标题栏上的个数还在）
        let note = Notification::info(text)
            .id1::<TerminalStopped>(key)
            .action(move |_, _, _| {
                let (this, row) = (this.clone(), row.clone());
                Button::new("terminal-go")
                    .small()
                    .primary()
                    .label(tr("前往"))
                    .on_click(move |_, window, cx| {
                        this.update(cx, |this, cx| this.go_to_terminal(&row, window, cx))
                            .ok();
                    })
            })
            .autohide(true);
        log::info!(
            "提醒终端已停下：{repo}，窗口{}",
            if window.is_window_active() {
                "在前台"
            } else {
                "不在前台"
            }
        );
        window.push_notification(note, cx);
        if window.is_window_active() {
            return;
        }
        // 闪任务栏上的按钮也会让系统当场给窗口发消息，同样放到这一轮更新之外（道理见 `toasts`）
        if let Some(hwnd) = self.hwnd {
            cx.spawn(async move |_, _| win::flash(hwnd)).detach();
        }
        if self.app.read(cx).settings.notify_terminal_system {
            self.toast_job(ToastJob::Show {
                key: toast_key.to_string(),
                title: toast_title.to_string(),
                body: toast_body.to_string(),
            });
        }
    }

    /// 交给发系统通知的那个任务去做（见 `toasts`）。
    fn toast_job(&self, job: ToastJob) {
        self.toast_jobs.unbounded_send(job).ok();
    }

    /// 现在正显示着哪个终端：窗口在前台时，终端总览页里选中的（正往里输入的另算），或者当前标签页的
    /// 终端页上那个。
    fn shown_terminal(&self, window: &Window, cx: &App) -> Option<Shown> {
        if !window.is_window_active() {
            return None;
        }
        if self.showing_terms {
            let overview = self.overview.read(cx);
            let shown = match overview.typing(window, cx) {
                true => Shown::Typing,
                false => Shown::Term,
            };
            return overview.shown_id(cx).map(shown);
        }
        let (entry, number) = self.active_repo()?.shown(cx).read(cx).shown_terminal()?;
        Some(Shown::Repo(entry, number))
    }

    fn active_repo(&self) -> Option<&RepoTab> {
        if self.showing_settings || self.showing_terms {
            return None;
        }
        self.active.checked_sub(1).and_then(|ix| self.tabs.get(ix))
    }

    /// F5：重读当前仓库的状态和提交历史。
    pub fn refresh_active(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.shown(cx).update(cx, |view, cx| view.refresh_now(cx));
        }
    }

    /// Ctrl+W：关掉当前的标签页（设置页开着就关设置页）。
    pub fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.showing_settings {
            self.toggle_settings(cx);
        } else if self.showing_terms {
            self.toggle_terms(window, cx);
        } else if let Some(ix) = self.active.checked_sub(1) {
            // 在文件页上、有打开的文件：Ctrl+W 关的是那个文件，不是整个仓库标签页
            let closed_file = self.tabs.get(ix).is_some_and(|tab| {
                tab.shown(cx)
                    .update(cx, |view, cx| view.close_active_file(window, cx))
            });
            if !closed_file {
                self.close(ix, window, cx);
            }
        }
    }

    /// Alt+←：在子仓库、别的工作树里时回到上一层的总览表。
    pub fn back_to_root_in_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.view
                .update(cx, |view, cx| view.back_to_root(window, cx));
        }
    }

    /// Ctrl+Shift+E：当前仓库切到文件页。
    pub fn files_in_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.shown(cx)
                .update(cx, |view, cx| view.show_files(window, cx));
        }
    }

    /// Ctrl+S：保存当前仓库文件页上显示着的文件。
    pub fn save_in_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.shown(cx)
                .update(cx, |view, cx| view.save_active_file(window, cx));
        }
    }

    /// Ctrl+Shift+T：重新打开这个工作区里最近关掉的标签页。
    pub fn reopen_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.app.read(cx).closed_tabs().into_iter().next() {
            Some(entry) => self.open(entry, window, cx),
            None => {
                let text = tr("此工作区中没有最近关闭的标签页。");
                window.push_notification(Notification::info(text), cx);
            }
        }
    }

    /// Ctrl+Shift+`：当前仓库再开一个终端，切到终端页显示它。
    pub fn new_terminal_in_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 终端总览页上：开在那里选中的仓库里
        if self.showing_terms {
            self.overview
                .update(cx, |overview, cx| overview.new_terminal_here(window, cx));
            return;
        }
        if let Some(tab) = self.active_repo() {
            tab.shown(cx)
                .update(cx, |view, cx| view.new_terminal(window, cx));
        }
    }

    /// Ctrl+Shift+B：收起、展开仓库页的侧栏（所有标签页一起变）。
    pub fn toggle_sidebar_in_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.shown(cx)
                .update(cx, |view, cx| view.toggle_sidebar(window, cx));
        }
    }

    /// Ctrl+`：当前仓库在 Git、终端两页之间来回切。
    pub fn terminal_in_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.shown(cx)
                .update(cx, |view, cx| view.toggle_terminal(window, cx));
        }
    }

    /// Ctrl+1、Ctrl+2：当前仓库切到本地修改、所有提交。
    pub fn show_in_active(&mut self, changes: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.shown(cx).update(cx, |view, cx| {
                if changes {
                    view.show_changes_view(window, cx);
                } else {
                    view.show_history_view(window, cx);
                }
            });
        }
    }

    /// Ctrl+0：在当前仓库的提交列表里回到 HEAD。
    pub fn go_to_head(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.shown(cx)
                .update(cx, |view, cx| view.go_to_head(window, cx));
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
            tab.shown(cx)
                .update(cx, |view, cx| view.find_commit(window, cx));
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

    /// 切到另一个工作区：当前的标签页收起来（留在内存里），换上那一组。
    fn switch_workspace(&mut self, key: WorkspaceKey, window: &mut Window, cx: &mut Context<Self>) {
        if key == self.shown || key.id.is_empty() {
            return;
        }
        self.app
            .update(cx, |app, cx| app.set_current(key.clone(), cx));
        self.load_workspace(key, window, cx);
    }

    /// 把 `key` 那一组标签页换上来。服务器上的工作区：那台没连着就连上（连不上回到本机的工作区）。
    fn load_workspace(&mut self, key: WorkspaceKey, window: &mut Window, cx: &mut Context<Self>) {
        // 收起来之前先让它们知道自己不显示了
        for tab in &self.tabs {
            tab.view.update(cx, |view, cx| view.set_visible(false, cx));
        }
        let tabs = std::mem::take(&mut self.tabs);
        let old = std::mem::replace(&mut self.shown, key.clone());
        self.parked.insert(old, (tabs, self.active));
        match self.parked.remove(&key) {
            Some((tabs, active)) => {
                self.tabs = tabs;
                self.active = active.min(self.tabs.len());
            }
            None => {
                let view = self.app.read(cx).view();
                for entry in view.tabs {
                    let tab = self.build_tab(entry, window, cx);
                    self.tabs.push(tab);
                }
                self.active = view.active.min(self.tabs.len());
            }
        }
        self.showing_settings = false;
        self.leave_terms(cx);
        self.reveal_active_tab();
        self.persist(cx);
        cx.notify();
        if !key.is_local() && !self.app.read(cx).writable(&key.machine) {
            self.ensure_connected(key.machine.clone(), window, cx);
        }
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

    /// 新建工作区：填名称、选保存在哪台电脑上（默认是当前工作区所在的那台，它没连着时默认本机）。
    /// 建好了切过去。
    fn new_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.read(cx);
        // 菜单里正看着哪台电脑，就默认建在哪台
        let current = app.menu_machine();
        let default = match app.writable(&current) {
            true => current,
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
        new_workspace_dialog(
            places,
            default,
            window,
            cx,
            move |name, machine, window, cx| {
                this.update(cx, |this, cx| {
                    let key = this
                        .app
                        .update(cx, |app, cx| app.add_workspace(&machine, &name, cx));
                    if let Some(key) = key {
                        this.switch_workspace(key, window, cx);
                    }
                })
                .ok();
            },
        );
    }

    fn rename_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.shown.clone();
        if !self.check_writable(&key.machine, window, cx) {
            return;
        }
        let old = self.app.read(cx).workspace_name(&key).to_string();
        let app = self.app.clone();
        ask_name(
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

    /// 删掉当前工作区（先问一下）。每台电脑至少留一个；里面有标签页正在做操作时不删。
    fn delete_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.shown.clone();
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
        if self
            .tabs
            .iter()
            .any(|tab| tab.view.read(cx).busy_with_subs(cx).is_some())
        {
            let text = tr("此工作区中有标签页正在执行操作，请等待完成后再删除。");
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let tabs = self.tabs.len();
        let mut description = String::new();
        // 服务器上的工作区：别的电脑上也跟着没了
        if !key.is_local() {
            description.push_str(&tf!(
                "此工作区保存在 {machine} 上，所有电脑将同步删除。",
                machine
            ));
        }
        description.push_str(&if tabs == 0 {
            tr("此工作区中没有打开的标签页。").to_string()
        } else {
            tf!("其中的 {tabs} 个标签页将关闭，仓库仍保留在列表中。", tabs)
        });
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
                                        this.delete_workspace_now(key, window, cx)
                                    })
                                    .ok();
                                }),
                        ),
                )
        });
    }

    fn delete_workspace_now(
        &mut self,
        key: WorkspaceKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shown != key {
            return;
        }
        // 先切到同一台电脑上的另一个，再把这个删掉
        let next = self
            .app
            .read(cx)
            .workspaces_on(&key.machine)
            .and_then(|list| list.into_iter().map(|(k, ..)| k).find(|k| *k != key));
        let Some(next) = next else {
            return;
        };
        self.switch_workspace(next, window, cx);
        self.parked.remove(&key);
        self.app
            .update(cx, |app, cx| app.remove_workspace(&key, cx));
        cx.notify();
    }

    /// 把一个标签页移到同一台电脑上的另一个工作区：仓库也跟着换，不再是这个工作区的。
    fn move_tab(
        &mut self,
        ix: usize,
        to: WorkspaceKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        if let Some(op) = tab.view.read(cx).busy_with_subs(cx) {
            let text = tf!("此仓库正在{op}，请等待完成后再移动。", op);
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        if !self.check_writable(&to.machine, window, cx) {
            return;
        }
        // 移过去是在那边重新打开；本机终端跟着关标签页留下来，到那边的终端页时接回
        let entry = tab.entry.clone();
        self.close_now(ix, cx);
        self.app.update(cx, |app, cx| {
            app.remove_from_workspace(&entry, cx);
            app.push_tab_to_workspace(&to, entry.clone(), cx)
        });
        // 那个工作区的标签页已经建好了（切过去过）：给它补上这一页
        if self.parked.contains_key(&to) {
            let tab = self.build_tab(entry.clone(), window, cx);
            if let Some((tabs, _)) = self.parked.get_mut(&to)
                && !tabs.iter().any(|t| t.entry == entry)
            {
                tabs.push(tab);
            }
        }
    }

    // ---- 标题栏 ----

    /// 工作区按钮：工作区的名字，服务器上的后面跟一个服务器名的小标签。
    fn render_workspace_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.read(cx);
        let key = app.current().clone();
        let name = app.workspace_name(&key);
        let machine = (!key.is_local()).then(|| app.machine_name(&key.machine));
        let (chip, accent) = (cx.theme().primary.opacity(0.16), cx.theme().primary);
        let this = cx.entity().downgrade();
        let app = self.app.clone();
        div().occlude().child(
            Button::new("workspace")
                .ghost()
                .small()
                .icon(ui::icon(IconName::Layers))
                .label(name)
                .when_some(machine, |button, machine| {
                    button.child(
                        div()
                            .flex_none()
                            .px(px(6.))
                            .rounded_full()
                            .bg(chip)
                            .text_xs()
                            .text_color(accent)
                            .child(machine),
                    )
                })
                .dropdown_menu(move |menu, _, cx| {
                    workspace_menu(menu, MenuPage::Workspaces, &app, &this, cx)
                }),
        )
    }

    /// 一个标签页：图标、名字、关闭按钮（固定着的是一个图钉）。`ix` 是 `tabs` 里的下标。
    fn render_repo_tab(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let tab = &self.tabs[ix];
        let app = self.app.read(cx);
        // 进到了子仓库、别的工作树里：标题后面带上它的名字
        let title: SharedString = match tab.view.read(cx).sub_title(cx) {
            Some(sub) => format!("{} / {sub}", app.repo_title(&tab.entry)).into(),
            None => app.repo_title(&tab.entry),
        };
        let selected = self.active == ix + 1 && !self.showing_settings && !self.showing_terms;
        let (busy, changes) = {
            let view = tab.view.read(cx);
            (
                view.busy_with_subs(cx).is_some(),
                view.change_count_with_subs(),
            )
        };
        let glyph = if busy {
            IconName::LoaderCircle
        } else if tab.entry.server.is_some() {
            IconName::Server
        } else {
            IconName::FolderGit2
        };
        let (accent, muted) = (cx.theme().primary, cx.theme().muted_foreground);
        let pinned = app.view().is_pinned(&tab.entry);
        let show_close = app.settings.tab_close_button && !pinned;
        let can_reopen = !app.closed_tabs().is_empty();
        let close = tab_close(("close-tab", ix), selected, cx).on_click(cx.listener(
            move |this, _, window, cx| {
                cx.stop_propagation();
                this.close(ix, window, cx);
            },
        ));
        let (menu_this, menu_app, entry) =
            (cx.entity().downgrade(), self.app.clone(), tab.entry.clone());
        // 标签页窄了名字会被截掉：停上去显示完整的名字和路径；经服务器连的写明经由哪台
        let tip_title: SharedString = match tab.view.read(cx).sub_title(cx) {
            Some(sub) => format!("{} / {sub}", app.repo_tip(&tab.entry)).into(),
            None => app.repo_tip(&tab.entry),
        };
        let tip_path = SharedString::from(tab.entry.path.clone());
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
        let active = (!self.showing_settings && !self.showing_terms)
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
        let (border, muted, warning) = (theme.border, theme.muted_foreground, theme.warning);
        let home_selected = self.active == 0 && !self.showing_settings && !self.showing_terms;
        let home = tab_shell("tab-home", home_selected, cx)
            .flex_none()
            .on_click(cx.listener(|this, _, _, cx| this.select(0, cx)))
            .child(ui::icon(IconName::LayoutGrid).flex_none())
            .child(trc("页面", "仓库"));
        // 所有工作区里的终端：已停下、还没看过的有几个就标几个
        let waiting = self.board.read(cx).attention();
        let terms = tab_shell("tab-terms", self.showing_terms, cx)
            .flex_none()
            .on_click(cx.listener(|this, _, window, cx| this.toggle_terms(window, cx)))
            .child(ui::icon(IconName::SquareTerminal).flex_none())
            .child(trc("页面", "终端"))
            .when(waiting > 0, |tab| {
                tab.child(
                    div()
                        .flex_none()
                        .px(px(5.))
                        .h(px(16.))
                        .min_w(px(16.))
                        .rounded_full()
                        .bg(warning)
                        .text_color(gpui_kit::black())
                        .text_size(px(10.5))
                        .line_height(px(16.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_center()
                        .child(waiting.to_string()),
                )
            })
            .tooltip(move |window, cx| {
                let text = match waiting {
                    0 => tr("所有工作区中的终端（Ctrl+Shift+A）").to_string(),
                    n => tf!(
                        "所有工作区中的终端（Ctrl+Shift+A）：{n} 个已停下，尚未查看",
                        n = n.to_string()
                    ),
                };
                ui::tip(text, window, cx)
            });
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
                // 仓库、终端这两页是所有工作区共用的，排在前面；工作区按钮挨着它管的那一排标签页
                .child(home)
                .child(terms)
                .child(div().flex_none().w(px(1.)).h(px(16.)).mx(px(4.)).bg(border))
                .child(self.render_workspace_button(cx))
                // 标签页那一段画在这个占位的块里面（绝对定位）。标题栏的宽度按里面内容的
                // 最小宽度算，直接把标签页排在这一行里的话，开多了会把整行撑宽，
                // 右边的设置和窗口按钮被挤出窗口
                .child(div().relative().flex_1().min_w_0().h_full().child(strip))
                // 有新版本、正在下、下好了：齿轮左边一个小牌，点开看说明、升级
                .children(
                    crate::update_view::title_pill(&self.updater, cx)
                        .map(|pill| div().occlude().flex_none().child(pill)),
                )
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

/// 工作区菜单里画着哪一页。
#[derive(Clone, Copy, PartialEq)]
enum MenuPage {
    /// 一台电脑上的工作区
    Workspaces,
    /// 挑一台电脑
    Servers,
}

/// 把开着的工作区菜单换成另一页，菜单不关。
fn show_menu_page(
    menu: &WeakEntity<PopupMenu>,
    page: MenuPage,
    app: &Entity<AppState>,
    this: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let (app, this) = (app.clone(), this.clone());
    menu.update(cx, |menu, cx| {
        menu.rebuild(window, cx, move |menu, _, cx| {
            workspace_menu(menu, page, &app, &this, cx)
        });
    })
    .ok();
}

/// 菜单里点了不关菜单的一行。界面库的条目一点就关菜单，这一行算作不能点的条目，点击自己接；
/// 代价是方向键选不到它。
fn stay_item(
    id: impl Into<ElementId>,
    checked: bool,
    content: impl Fn(&App) -> AnyElement + 'static,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> PopupMenuItem {
    let (id, on_click) = (id.into(), Rc::new(on_click));
    PopupMenuItem::element(move |_, cx| {
        let on_click = on_click.clone();
        let theme = cx.theme();
        let (fg, hover) = (theme.popover_foreground, theme.accent);
        h_flex()
            .id(id.clone())
            .w_full()
            .h(px(26.))
            .px(px(6.))
            .gap(px(8.))
            .rounded(px(4.))
            .text_color(fg)
            .cursor_pointer()
            .hover(|s| s.bg(hover))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_, window, cx| on_click(window, cx))
            .child(content(cx))
    })
    .checked(checked)
    .disabled(true)
}

/// 工作区按钮的下拉菜单。只列一台电脑上的工作区（默认是当前工作区所在的那台），当前的打勾；
/// 配置了服务器时最上面一行写着是哪台，点它换一台（菜单不关，原处换成电脑的列表）。这次还没
/// 连上的服务器只有「连接」。下面是新建、改名、删除。
fn workspace_menu(
    mut menu: PopupMenu,
    page: MenuPage,
    app: &Entity<AppState>,
    this: &WeakEntity<Workspace>,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let popup = cx.entity().downgrade();
    let state = app.read(cx);
    let current = state.current().clone();
    let machine = state.menu_machine();
    let muted = cx.theme().muted_foreground;
    menu = menu.min_w(px(248.));
    if page == MenuPage::Servers {
        let mut machines = vec![LOCAL.to_string()];
        machines.extend(
            state
                .settings
                .servers
                .iter()
                .map(|server| server.id.clone()),
        );
        menu = menu.label(tr("切换服务器"));
        for (ix, id) in machines.into_iter().enumerate() {
            let name = state.machine_name(&id);
            let note: SharedString = match state.workspaces_on(&id) {
                Some(list) => tf!("{n} 个工作区", n = list.len()).into(),
                None => machine_status(state, &id, cx).0,
            };
            let icon = match id == LOCAL {
                true => IconName::Monitor,
                false => IconName::Server,
            };
            let (popup, app, this) = (popup.clone(), app.clone(), this.clone());
            menu = menu.item(stay_item(
                ("menu-server", ix),
                id == machine,
                move |_| {
                    h_flex()
                        .w_full()
                        .gap(px(8.))
                        .child(ui::icon_small(icon.clone()).flex_none())
                        .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(muted)
                                .child(note.clone()),
                        )
                        .into_any_element()
                },
                move |window, cx| {
                    app.update(cx, |app, _| app.menu_machine = Some(id.clone()));
                    show_menu_page(&popup, MenuPage::Workspaces, &app, &this, window, cx);
                },
            ));
        }
        return menu;
    }
    if state.settings.servers.is_empty() {
        menu = menu.item(machine_header(state, &machine, cx));
    } else {
        let name = state.machine_name(&machine);
        let status = (machine != LOCAL).then(|| machine_status(state, &machine, cx));
        let icon = match machine == LOCAL {
            true => IconName::Monitor,
            false => IconName::Server,
        };
        let (popup, app, this) = (popup.clone(), app.clone(), this.clone());
        menu = menu
            .item(stay_item(
                "menu-switch-server",
                false,
                move |_| {
                    h_flex()
                        .w_full()
                        .gap(px(8.))
                        .child(ui::icon_small(icon.clone()).flex_none())
                        .child(
                            div()
                                .flex_shrink(1.)
                                .min_w_0()
                                .truncate()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(name.clone()),
                        )
                        .children(
                            status.clone().map(|(_, dot)| {
                                div().flex_none().size(px(6.)).rounded_full().bg(dot)
                            }),
                        )
                        .child(div().flex_1())
                        .child(
                            h_flex()
                                .flex_none()
                                .gap(px(4.))
                                .text_xs()
                                .text_color(muted)
                                .child(tr("切换服务器"))
                                .child(ui::icon_small(IconName::ChevronRight)),
                        )
                        .into_any_element()
                },
                move |window, cx| {
                    show_menu_page(&popup, MenuPage::Servers, &app, &this, window, cx);
                },
            ))
            .separator();
    }
    match state.workspaces_on(&machine) {
        None => {
            let (app, machine) = (app.clone(), machine.clone());
            let label = tf!("连接 {name}", name = state.machine_name(&machine));
            menu = menu.item(
                PopupMenuItem::new(label)
                    .icon(ui::icon(IconName::Plug))
                    .on_click(move |_, _, cx| {
                        app.update(cx, |app, cx| drop(app.connect(&machine, cx)));
                    }),
            );
        }
        Some(list) => {
            for (key, name, count) in list {
                let this = this.clone();
                let checked = key == current;
                // 括号里是这个工作区有几个仓库，不是开着几个标签页：全关了它也不是空的
                let label = tf!("{name}（{n}）", name, n = count);
                menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(
                    move |_, window, cx| {
                        let key = key.clone();
                        this.update(cx, |this, cx| this.switch_workspace(key, window, cx))
                            .ok();
                    },
                ));
            }
        }
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
            PopupMenuItem::new(tr("重命名当前工作区…")).on_click(move |_, window, cx| {
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

/// 工作区菜单里一组的标题：电脑的名字，服务器的右边写着连没连上。
fn machine_header(state: &AppState, machine: &str, cx: &App) -> PopupMenuItem {
    let name = state.machine_name(machine);
    if machine == LOCAL {
        return PopupMenuItem::label(name);
    }
    let (status, dot) = machine_status(state, machine, cx);
    let muted = cx.theme().muted_foreground;
    PopupMenuItem::element(move |_, _| {
        h_flex()
            .w_full()
            .gap_2()
            .text_xs()
            .text_color(muted)
            .child(div().flex_1().child(name.clone()))
            .child(div().flex_none().size(px(6.)).rounded_full().bg(dot))
            .child(status.clone())
    })
    .disabled(true)
}

/// 服务器连没连上：一句话和圆点的颜色。
fn machine_status(state: &AppState, machine: &str, cx: &App) -> (SharedString, Hsla) {
    let theme = cx.theme();
    match state.machine(machine) {
        Some(loaded) if loaded.live => (tr("已连接").into(), theme.success),
        Some(_) => (tr("已断开").into(), theme.muted_foreground),
        None => match state.status(machine) {
            crate::app_state::ConnStatus::Connecting => (tr("正在连接…").into(), theme.primary),
            _ => (tr("未连接").into(), theme.muted_foreground),
        },
    }
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
    let pinned = app.read(cx).view().is_pinned(&entry);
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
            PopupMenuItem::new(tr("重新打开已关闭的标签页（Ctrl+Shift+T）"))
                .disabled(!can_reopen)
                .on_click(move |_, window, cx| {
                    reopen
                        .update(cx, |this, cx| this.reopen_closed(window, cx))
                        .ok();
                }),
        )
        .separator();
    // 移到同一台电脑上的别的工作区（别的电脑上的工作区放不了这台的仓库）：有才列出来
    let state = app.read(cx);
    let current = state.current().clone();
    let others = state.workspaces_on(&current.machine).unwrap_or_default();
    for (to, name, _) in others {
        if to == current {
            continue;
        }
        let this = this.clone();
        menu = menu.item(
            PopupMenuItem::new(tf!("移动到工作区「{name}」", name)).on_click(
                move |_, window, cx| {
                    let to = to.clone();
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
        PopupMenuItem::new(tr("重命名…")).on_click(move |_, window, cx| {
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
/// `allow_empty` 为假时名字是空的就不关。`path` 是给仓库改名时它的路径：写在输入框下面，可以复制。
#[allow(clippy::too_many_arguments)]
fn ask_name<T: 'static>(
    title: &'static str,
    placeholder: &'static str,
    value: String,
    ok: &'static str,
    allow_empty: bool,
    path: Option<SharedString>,
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
            .child(
                v_flex()
                    .gap_3()
                    .child(Input::new(&input))
                    .children(path.clone().map(|path| path_row(path, cx))),
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
                            .label(ok)
                            .on_click(move |_, window, cx| on_click(window, cx)),
                    ),
            )
    });
}

/// 新建工作区的对话框：名称，和保存在哪台电脑上（`places` 是本机和各台服务器：键和名字，
/// `default` 是一开始选中的）。点「新建」或回车时关掉并调用 `on_ok(名称, 电脑)`。
fn new_workspace_dialog<T: 'static>(
    places: Vec<(String, SharedString)>,
    default: String,
    window: &mut Window,
    cx: &mut Context<T>,
    on_ok: impl Fn(String, String, &mut Window, &mut App) + 'static,
) {
    let input = cx
        .new(|cx| InputState::new(window, cx).placeholder(tr("工作区名称，例如「公司」「个人」")));
    let selected = Rc::new(RefCell::new(default));
    let submit: Submit = {
        let (input, selected) = (input.clone(), selected.clone());
        Rc::new(move |window, cx| {
            let name = input.read(cx).value().trim().to_string();
            if name.is_empty() {
                return;
            }
            window.close_dialog(cx);
            on_ok(name, selected.borrow().clone(), window, cx);
        })
    };
    let first_build = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_build.replace(false) {
            input.focus_handle(cx).focus(window, cx);
        }
        let (on_enter, on_click) = (submit.clone(), submit.clone());
        let current = selected.borrow().clone();
        let current_name = places
            .iter()
            .find(|(key, _)| *key == current)
            .map_or_else(|| tr("本机").into(), |(_, name)| name.clone());
        let hint: SharedString = match current == LOCAL {
            true => tr("只保存在本机。").into(),
            false => tf!(
                "保存在 {name} 上，连接它的电脑均可查看和修改。",
                name = &current_name
            )
            .into(),
        };
        let (menu_places, menu_selected) = (places.clone(), selected.clone());
        let muted = cx.theme().muted_foreground;
        let label = |text: &'static str| div().text_sm().child(text);
        dialog
            .title(tr("新建工作区"))
            .w(px(420.))
            .on_ok(move |_, window, cx| {
                on_enter(window, cx);
                false
            })
            .child(
                v_flex()
                    .gap_2()
                    .child(label(tr("名称")))
                    .child(Input::new(&input))
                    .child(div().h(px(4.)))
                    .child(label(tr("保存位置")))
                    .child(
                        h_flex().child(
                            Button::new("place")
                                .outline()
                                .small()
                                .icon(ui::icon(if current == LOCAL {
                                    IconName::Monitor
                                } else {
                                    IconName::Server
                                }))
                                .label(current_name)
                                .dropdown_caret(true)
                                .dropdown_menu(move |mut menu, _, _| {
                                    let chosen = menu_selected.borrow().clone();
                                    for (key, name) in &menu_places {
                                        let (key, selected) = (key.clone(), menu_selected.clone());
                                        let icon = match key == LOCAL {
                                            true => IconName::Monitor,
                                            false => IconName::Server,
                                        };
                                        menu = menu.item(
                                            PopupMenuItem::new(name.clone())
                                                .icon(ui::icon(icon))
                                                .checked(key == chosen)
                                                .on_click(move |_, window, _| {
                                                    *selected.borrow_mut() = key.clone();
                                                    window.refresh();
                                                }),
                                        );
                                    }
                                    menu
                                }),
                        ),
                    )
                    .child(div().text_xs().text_color(muted).child(hint)),
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
                            .label(tr("新建"))
                            .on_click(move |_, window, cx| on_click(window, cx)),
                    ),
            )
    });
}

/// 重命名仓库的对话框里写路径的一行：放不下省略开头（悬停看全），右边是复制按钮。
fn path_row(path: SharedString, cx: &App) -> impl IntoElement {
    let muted = cx.theme().muted_foreground;
    let copy = path.clone();
    h_flex()
        .gap_2()
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(muted)
                .child(tr("路径")),
        )
        .child(
            ui::clipped("repo-path", path)
                .flex_1()
                .text_ellipsis_start()
                .text_xs()
                .font_family("Consolas")
                .text_color(muted),
        )
        .child(
            Button::new("copy-path")
                .ghost()
                .xsmall()
                .icon(ui::icon(IconName::Copy))
                .tooltip(tr("复制路径"))
                .on_click(move |_, window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.to_string()));
                    window.push_notification(Notification::success(tr("已复制路径")), cx);
                }),
        )
}

/// 「终端已停下」那条提醒的类别（同一个终端只留一条，前往以后去掉）。
struct TerminalStopped;

/// 交给发系统通知的那个任务做的事（见 `Workspace::toasts`）。
enum ToastJob {
    Show {
        key: String,
        title: String,
        body: String,
    },
    /// 收回一条：已经去看过那个终端了
    Remove(String),
    Clear,
}

fn stopped_note_key(id: &TermId) -> SharedString {
    format!("{}/{}", id.machine, id.term).into()
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title_bar = self.render_title_bar(window, cx).into_any_element();
        // 正显示着哪个终端变了：告诉 `board`（显示着的算看过，停下了不提醒）
        let shown = self.shown_terminal(window, cx);
        if shown != self.told_shown {
            self.told_shown = shown.clone();
            let board = self.board.clone();
            cx.defer(move |cx| board.update(cx, |board, cx| board.set_shown(shown, cx)));
        }
        let theme = cx.theme();
        let (bg, fg) = (theme.background, theme.foreground);
        let content = if self.showing_settings {
            self.settings_page.clone().into_any_element()
        } else if self.showing_terms {
            self.overview.clone().into_any_element()
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
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("仓库名称或路径中的关键字")));
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
    /// 先是这个工作区里刚关掉的（最近关的在最上），再是这个工作区别的仓库，最后是这个工作区所在的
    /// 电脑看得到的其余仓库（它自己的、它配置的服务器上的）。第三项是行尾标的小字：刚关掉、开着。
    fn matches(&self, cx: &App) -> Vec<(RepoEntry, SharedString, Option<&'static str>)> {
        let needle = self.input.read(cx).value().trim().to_lowercase();
        let app = self.app.read(cx);
        let current = app.current().clone();
        let view = app.view();
        let closed = app.closed_tabs();
        let mut members = app.members(&current);
        // 开着、但已经不在工作区里的（别的电脑上移除了）也列着
        for tab in &view.tabs {
            if !members.contains(tab) {
                members.push(tab.clone());
            }
        }
        let rest: Vec<RepoEntry> = app
            .perspective_repos(&current.machine)
            .into_iter()
            .filter(|entry| !members.contains(entry))
            .collect();
        let members: Vec<(RepoEntry, Option<&'static str>)> = members
            .iter()
            .filter(|entry| !closed.contains(entry))
            .map(|entry| {
                (
                    entry.clone(),
                    view.tabs.contains(entry).then(|| tr("已打开")),
                )
            })
            .collect();
        closed
            .iter()
            .map(|entry| (entry.clone(), Some(tr("最近关闭"))))
            .chain(members)
            .chain(rest.into_iter().map(|entry| (entry, None)))
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
