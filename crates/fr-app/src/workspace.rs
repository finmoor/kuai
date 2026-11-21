//! 主窗口：标题栏（三个页面、标签页、设置）和下面的内容。
//!
//! 标题栏是自己画的，标签页直接放在里面，不另占一行。最左边是三个页面：服务器、终端、工作区，
//! 长得一样，名字固定不变；后面是标签页：仓库（和子仓库）一个一个排（设计见
//! `design/server-workspace/`，第二版）。设置页打开时也是一个标签页。1.17 右边还有一段终端的标签页
//! （T3），2026-11-20 去掉了：终端在终端页里看，那里的「打开仓库」打开它所在的仓库。
//!
//! 标签页全局一份，像 VS Code 的编辑器标签：打开哪个就是哪个，和选了哪台服务器、哪个工作区都
//! 无关。从哪里打开（工作区页、服务器页、终端页、快速打开）都是往这一排里加，
//! 已经开着的切过去。工作区只是「一组常一起看的仓库」，在工作区页里一次全部打开。开着哪些标签页
//! 只记在这台电脑上。
//!
//! 快捷键：F5 刷新、Ctrl+W 关标签页、Ctrl+Tab 切标签页、Ctrl+F 查找（在哪一页就找哪一页的）、Ctrl+P 快速打开仓库、
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
//! 关掉的标签页记最近 10 个（存在设置里），手滑关错了能找回来；
//! 设置里还可以把标签页上的关闭按钮藏起来。

use std::cell::{Cell, RefCell};
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
use crate::machines::LOCAL;
use crate::manager::{ManagerEvent, ManagerView};
use crate::prompt::{self, Prompts};
use crate::repo::{RepoEvent, RepoView};
use crate::repo_scan::RepoScan;
use crate::settings::{Front, RepoEntry};
use crate::settings_page::SettingsPage;
use crate::tab_fit::{self, Segment};
use crate::term_board::{BoardHandle, Row as TermRow, Shown, Stopped, TermBoard, TermId};
use crate::term_overview::{Go, TermOverview};
use crate::toast::Toasts;
use crate::tray::{Tray, TrayAction};
use crate::ui;
use crate::update::{self, UpdateEvent, Updater};
use crate::win::{self, Hwnd};
use crate::workspace_page::{PageEvent, WorkspacePage};

/// 标题栏的高度。标签页 28 高，上下各留 5。
const TITLE_HEIGHT: f32 = 38.;
/// 标题栏上标签之间的间隔
const TAB_GAP: f32 = 4.;
/// 「⌄ N」按钮的宽度（排标签页时先留出来）
const MORE_WIDTH: f32 = 44.;
/// 仓库标签页最宽这么宽，名字再长就省略
const REPO_TAB_MAX: f32 = 240.;
/// 标签上名字后面那一截淡淡的字（同名的说明）至少放得下这么宽（连前面的间隔）才画
const ASIDE_MIN: f32 = 6. + 24.;
/// 窗口比这窄时，标题栏上的三个页面只留图标
const NARROW_WIDTH: f32 = 1100.;

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

/// 显示着的是哪一页。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Servers,
    Terms,
    Workspaces,
    /// `tabs` 里的第几个
    Repo(usize),
}

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
    /// 服务器页（标题栏上的「服务器」）
    manager: Entity<ManagerView>,
    /// 工作区页（标题栏上的「工作区」）
    workspaces: Entity<WorkspacePage>,
    settings_page: Entity<SettingsPage>,
    /// 仓库（和子仓库）的标签页，全局一份
    tabs: Vec<RepoTab>,
    /// 显示着的那一页
    page: Page,
    /// 按 Ctrl+Shift+A 打开终端页之前在哪一页：再按一次回去
    before_terms: Page,
    /// 那一页是仓库标签页时是哪个仓库：回去时按它找。下标靠不住，在终端页上关了、固定了别的标签页
    /// 位置就变了，原来会回到别的仓库，或者一个没有标签页的空页
    before_terms_repo: Option<RepoEntry>,
    /// 主机移除了、因为有没保存的文件或正在做操作而暂时没关的标签页（已经说过一次，不再说）
    kept_on_removal: Vec<RepoEntry>,
    /// 设置页开着；它盖在当前标签页上面，关掉回到原来那一页
    showing_settings: bool,
    /// 所有工作区里的终端现在怎么样（见 `term_board`）
    board: Entity<TermBoard>,
    /// 终端页（标题栏上的「终端」，Ctrl+Shift+A）
    overview: Entity<TermOverview>,
    /// 上一次告诉 `board` 的「正显示着哪个终端」
    told_shown: Option<Shown>,
    /// Kuai 不在前台时发的系统通知（终端停下了）
    /// 系统通知。发、收回都要跨进程调用系统，等的时候系统会接着给窗口派消息，窗口要画，所以不能在
    /// 处理界面更新的半路上做（做了日志里就是几条 `RefCell already borrowed`，少画一帧；1.15 起
    /// 就有，2026-11-18 查到）：都经 `toast_jobs` 交给主线程上的一个任务，一件一件做。这里留着的
    /// 这一份只在退出时用
    toasts: Rc<RefCell<Toasts>>,
    toast_jobs: mpsc::UnboundedSender<ToastJob>,
    /// 标题栏上标签页那一块有多宽（画的时候量，下一帧排标签页用）
    tab_region: Rc<Cell<f32>>,
    /// 这一帧排标签页用的是多宽：和量到的不一样就再排一次
    tab_region_used: f32,
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
        let board = cx.new(|cx| TermBoard::new(app.clone(), cx));
        cx.set_global(BoardHandle(board.clone()));
        let scan = cx.new(|cx| RepoScan::new(app.clone(), cx));
        let manager = cx.new(|cx| ManagerView::new(app.clone(), scan.clone(), board.clone(), cx));
        let workspaces =
            cx.new(|cx| WorkspacePage::new(app.clone(), board.clone(), scan.clone(), window, cx));
        let settings_page = cx.new(|cx| SettingsPage::new(app.clone(), updater.clone(), cx));
        let overview = cx.new(|cx| TermOverview::new(app.clone(), board.clone(), window, cx));
        let quitting = Rc::new(Cell::new(false));
        let subscriptions = vec![
            cx.subscribe_in(&manager, window, |this, _, event, window, cx| match event {
                ManagerEvent::Open(entry) => this.open(entry.clone(), window, cx),
                ManagerEvent::Rename(entry) => this.rename_repo(entry.clone(), window, cx),
                ManagerEvent::ShowWorkspaces(machine) => {
                    this.show_workspaces_of(machine.clone(), window, cx)
                }
                ManagerEvent::Terminals(entry) => this.show_terminals_of(entry, window, cx),
            }),
            cx.subscribe_in(
                &workspaces,
                window,
                |this, _, event, window, cx| match event {
                    PageEvent::Open(entry) => this.open(entry.clone(), window, cx),
                    PageEvent::OpenAll { entries, show } => {
                        this.open_all(entries.clone(), *show, window, cx)
                    }
                    PageEvent::Rename(entry) => this.rename_repo(entry.clone(), window, cx),
                    PageEvent::Terminals(entry) => this.show_terminals_of(entry, window, cx),
                    PageEvent::NewTerminal(entry) => {
                        this.new_terminal_for(entry.clone(), window, cx)
                    }
                },
            ),
            // 设置变了：服务器改名要刷新标签上的名字；服务器被移除了，它上面的仓库的标签页跟着关掉；
            // 经别的电脑连的那台认出来是哪一台了，它上面的标签页换过去；
            // 主题、语言、托盘的选项变了就应用到窗口上
            cx.observe_in(&app, window, |this, _, window, cx| {
                this.drop_tabs_of_removed_servers(window, cx);
                this.remap_duplicate_tabs(window, cx);
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
                    Go::Repo(entry) => this.open_repo_git(entry, window, cx),
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
            workspaces,
            settings_page,
            tabs: Vec::new(),
            page: Page::Workspaces,
            before_terms: Page::Workspaces,
            before_terms_repo: None,
            kept_on_removal: Vec::new(),
            showing_settings: false,
            board,
            overview,
            told_shown: None,
            toasts,
            toast_jobs,
            tab_region: Rc::new(Cell::new(0.)),
            tab_region_used: 0.,
            drafts: Vec::new(),
            tray: None,
            hwnd: win::hwnd(window),
            applied: None,
            quitting,
            _subscriptions: subscriptions,
            _prompts: prompt::show_in(prompts, window, cx),
        };
        // 恢复上次开着的标签页和显示着的那一页
        let saved = app.read(cx).tabs().clone();
        for entry in saved.repos {
            let tab = this.build_tab(entry, window, cx);
            this.tabs.push(tab);
        }
        this.page = match &saved.front {
            Front::Servers => Page::Servers,
            Front::Workspaces | Front::Terms => Page::Workspaces,
            Front::Repo(entry) => this
                .tabs
                .iter()
                .position(|tab| tab.entry == *entry)
                .map_or(Page::Workspaces, Page::Repo),
        };
        this.update_visibility(cx);
        this.apply_settings(window, cx);
        // 一开始显示的那一页也要知道自己显示了（工作区页、服务器页要读子仓库、分支、改动数）
        cx.defer_in(window, |this, window, cx| {
            this.page_changed(None, window, cx)
        });
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
                let views: Vec<_> = self.tabs.iter().map(|tab| tab.view.clone()).collect();
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
                    "有 {n} 个本机终端正在运行，重启将结束其中的程序。远程主机上的终端不受影响。",
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
                "有 {n} 个本机终端正在运行，退出将结束其中的程序。远程主机上的终端不受影响。",
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

    /// 打开本机的目录（命令行给的、再次启动时交过来的）：在仓库中就打开仓库（也加进本机的列表），
    /// 不是仓库先问是否初始化。
    pub fn open_local_dir(&mut self, dir: String, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.clone();
        crate::open_dir::open_dir(
            &app,
            None,
            dir,
            window,
            cx,
            |this: &mut Self, entry, window, cx| {
                this.app
                    .update(cx, |app, cx| app.add_repo(entry.clone(), cx));
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

    /// 打开的标签页中，一共有几个文件没保存（退出 Kuai 前要问）。
    fn unsaved_files(&self, cx: &App) -> usize {
        self.tabs
            .iter()
            .map(|tab| tab.view.read(cx).unsaved_with_subs(cx))
            .sum()
    }

    /// 一共有几个本机终端的程序正在运行（退出 Kuai 时将结束）：打开的标签页中的，加上关掉的标签页
    /// 留下的。
    fn live_terminals(&self, cx: &App) -> usize {
        let open: usize = self
            .tabs
            .iter()
            .map(|tab| tab.view.read(cx).local_terminals_with_subs(cx))
            .sum();
        open + crate::repo::parked_terminals_running(cx)
    }

    /// 显示着的仓库标签页（设置页盖着时也算）。
    fn front_repo(&self) -> Option<RepoEntry> {
        match self.page {
            Page::Repo(ix) => self.tabs.get(ix).map(|tab| tab.entry.clone()),
            _ => None,
        }
    }

    /// 标签页变了以后，回到原来显示的那个仓库标签页上；它不在了就是 `fallback`。
    fn refocus(&mut self, front: Option<RepoEntry>, fallback: Page) {
        let Some(entry) = front else {
            return;
        };
        self.page = self
            .tabs
            .iter()
            .position(|tab| tab.entry == entry)
            .map_or(fallback, Page::Repo);
    }

    /// 这个标签页现在关不得：有没保存的文件，或者正在做操作。不经确认就关掉它的地方（移除主机、
    /// 认出重复的主机、批量关闭）先跳过它。
    fn unsafe_to_drop(tab: &RepoTab, cx: &App) -> bool {
        let view = tab.view.read(cx);
        view.unsaved_with_subs(cx) > 0 || view.busy_with_subs(cx).is_some()
    }

    /// 服务器被移除了：它上面的仓库的标签页跟着关掉，经它连的也关。有没保存的文件、正在做操作的
    /// 先留着（它手上的连接还在，还能存），说一声；存好、做完以后再关（原来直接关，修改就丢了）。
    fn drop_tabs_of_removed_servers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.read(cx);
        let gone = |entry: &RepoEntry| {
            entry.server.as_deref().is_some_and(|id| {
                let direct = crate::machines::split_relay(id).map_or(id, |(via, _)| via);
                app.server(direct).is_none()
            })
        };
        let (kept, closing): (Vec<&RepoTab>, Vec<&RepoTab>) = self
            .tabs
            .iter()
            .filter(|tab| gone(&tab.entry))
            .partition(|tab| Self::unsafe_to_drop(tab, cx));
        let kept: Vec<RepoEntry> = kept.into_iter().map(|tab| tab.entry.clone()).collect();
        let closing: Vec<RepoEntry> = closing.into_iter().map(|tab| tab.entry.clone()).collect();
        if !closing.is_empty() {
            let was = (!self.showing_settings).then_some(self.page);
            let front = self.front_repo();
            self.tabs.retain(|tab| !closing.contains(&tab.entry));
            // 原来显示的标签页还在就留在它上面，不在了回到服务器页
            self.refocus(front, Page::Servers);
            self.page_changed(was, window, cx);
            self.persist(cx);
        }
        let fresh = kept
            .iter()
            .any(|entry| !self.kept_on_removal.contains(entry));
        if fresh {
            let text = tf!(
                "主机已移除，其中 {n} 个标签页有未保存的文件或正在执行操作，暂未关闭。处理完后再关闭。",
                n = kept.len()
            );
            window.push_notification(Notification::warning(text).autohide(false), cx);
        }
        self.kept_on_removal = kept;
    }

    /// 经别的电脑连的那台认出来就是本机、就是它经由的那台，或者就是本机直接配置的一台（见
    /// `machines::Identities`）：它上面开着的仓库标签页换成那一台的，不再绕路连；那一台的同一个
    /// 已经开着就只留那一个。显示着的跟着换过去，固定着的照样固定。
    fn remap_duplicate_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repos: Vec<(usize, RepoEntry)> = {
            let app = self.app.read(cx);
            let ids = app.ids();
            self.tabs
                .iter()
                .enumerate()
                // 有没保存的文件、正在做操作的先不换（换就是丢掉这个视图）：处理完以后下一次再换
                .filter(|(_, tab)| !Self::unsafe_to_drop(tab, cx))
                .filter_map(|(ix, tab)| {
                    let to = ids.entry(tab.entry.clone());
                    (to != tab.entry).then_some((ix, to))
                })
                .collect()
        };
        if repos.is_empty() {
            return;
        }
        let was = (!self.showing_settings).then_some(self.page);
        // 显示着的那一个换过以后怎么写
        let front = match self.page {
            Page::Repo(ix) => self.tabs.get(ix).map(|tab| {
                repos
                    .iter()
                    .find(|(at, _)| *at == ix)
                    .map_or_else(|| tab.entry.clone(), |(_, to)| to.clone())
            }),
            _ => None,
        };
        // 从后往前换：去掉一个不影响前面那些的位置
        for (ix, to) in repos.into_iter().rev() {
            let from = self.tabs[ix].entry.clone();
            log::info!("标签页换成同一台电脑的写法：{from:?} → {to:?}");
            self.app.update(cx, |app, _| app.repin(&from, &to));
            if self.tabs.iter().any(|tab| tab.entry == to) {
                self.tabs.remove(ix);
            } else {
                let tab = self.build_tab(to, window, cx);
                self.tabs[ix] = tab;
            }
        }
        if let Some(entry) = front {
            self.page = self
                .tabs
                .iter()
                .position(|tab| tab.entry == entry)
                .map_or(Page::Servers, Page::Repo);
        }
        self.page_changed(was, window, cx);
        self.persist(cx);
        cx.notify();
    }

    /// 告诉每个标签页它是不是当前显示的：切到哪一页，哪一页就刷新。
    fn update_visibility(&self, cx: &mut Context<Self>) {
        for (ix, tab) in self.tabs.iter().enumerate() {
            let visible = self.page == Page::Repo(ix) && !self.showing_settings;
            tab.view
                .update(cx, |view, cx| view.set_visible(visible, cx));
        }
    }

    /// 打开仓库（或子仓库）：已经开着就切过去，没开着就在最后加一个标签页。只是开标签页，不放进
    /// 哪个工作区，也不进列表（新加仓库的地方自己先加进列表）。
    pub fn open(&mut self, entry: RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.ensure_tab(entry, window, cx);
        self.show_page(Page::Repo(ix), window, cx);
    }

    /// 一次打开几个（工作区页的「全部打开」）：按给的先后，没开着的加在最后；`show` 时显示第一个。
    fn open_all(
        &mut self,
        entries: Vec<RepoEntry>,
        show: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut first = None;
        for entry in entries {
            let ix = self.ensure_tab(entry, window, cx);
            first.get_or_insert(ix);
        }
        match first.filter(|_| show) {
            Some(ix) => self.show_page(Page::Repo(ix), window, cx),
            None => {
                self.persist(cx);
                cx.notify();
            }
        }
    }

    /// 工作区页上点了一个仓库的「新建终端」：到终端页，在这个仓库里新开一个。
    fn new_terminal_for(&mut self, entry: RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.show_terms(window, cx);
        self.overview
            .update(cx, |overview, cx| overview.new_terminal(entry, window, cx));
    }

    /// 这个仓库的标签页在第几个；没开着就建一个加在最后。
    fn ensure_tab(
        &mut self,
        entry: RepoEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Windows 上同一个仓库的几种写法算一个（见 [`RepoEntry::same_repo`]），不然从终端页打开没登记
        // 过的子仓库会再开一个标签页
        if let Some(ix) = self.tabs.iter().position(|t| t.entry.same_repo(&entry)) {
            return ix;
        }
        let tab = self.build_tab(entry, window, cx);
        self.tabs.push(tab);
        self.tabs.len() - 1
    }

    /// 换到一页（标题栏上的三个页面、某个标签页）。设置页开着就关掉。
    fn show_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        // 设置页盖着时，下面那一页不算显示着
        let was = (!self.showing_settings).then_some(self.page);
        self.showing_settings = false;
        self.page = page;
        self.page_changed(was, window, cx);
        self.persist(cx);
        cx.notify();
    }

    /// 显示着的那一页换了（`was` 是原来显示着的，设置页盖着时是 None）：终端页、工作区页要知道
    /// 自己显示了、不显示了。
    fn page_changed(&mut self, was: Option<Page>, window: &mut Window, cx: &mut Context<Self>) {
        let now = (!self.showing_settings).then_some(self.page);
        if was == now {
            return;
        }
        if was == Some(Page::Terms) {
            self.overview.update(cx, |overview, cx| overview.hidden(cx));
        }
        if was == Some(Page::Workspaces) {
            self.workspaces.update(cx, |page, _| page.hidden());
        }
        if was == Some(Page::Servers) {
            self.manager.update(cx, |page, _| page.hidden());
        }
        if now == Some(Page::Terms) {
            self.overview
                .update(cx, |overview, cx| overview.shown(window, cx));
        }
        if now == Some(Page::Workspaces) {
            self.workspaces
                .update(cx, |page, cx| page.shown(window, cx));
        }
        if now == Some(Page::Servers) {
            self.manager.update(cx, |page, cx| page.shown(window, cx));
        }
    }

    // 1.16 以前标签页放不下时横着滚（reveal_active_tab、scroll_tabs）；第二版改成先缩、再收进
    // 「⌄ N」，当前的总在外面（见 `tab_fit`），不再滚

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
            self.close_now(ix, window, cx);
            return;
        };
        if !cancellable {
            let text = tf!("此仓库正在{op}，请等待完成后再关闭。", op);
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        // 同时有没保存的文件：一起写出来，原来只说要取消操作，确认以后修改也没了
        let text = match unsaved {
            0 => tf!("此仓库正在{op}，关闭标签页将取消该操作。", op),
            n => tf!(
                "此仓库正在{op}，关闭标签页将取消该操作；另有 {n} 个文件尚未保存，修改将丢失。",
                op,
                n
            ),
        };
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
                                                this.close_now(at, window, cx);
                                            }
                                        })
                                        .ok();
                                }),
                        ),
                )
        });
    }

    fn close_now(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
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
        // 关掉的是当前页：留在同一个位置（原来右边那一页），右边没有了才往左，一个都不剩了回到
        // 工作区页；关掉的是它前面的页，选中位置跟着前移
        let was = (!self.showing_settings).then_some(self.page);
        match self.page {
            Page::Repo(at) if at == ix => {
                self.page = match self.tabs.len() {
                    0 => Page::Workspaces,
                    n => Page::Repo(ix.min(n - 1)),
                };
            }
            Page::Repo(at) if at > ix => self.page = Page::Repo(at - 1),
            _ => {}
        }
        self.page_changed(was, window, cx);
        self.persist(cx);
        cx.notify();
    }

    /// 固定、取消固定第 `ix` 个标签页。固定的排到最前面，选中的还是原来那一页。
    fn toggle_pin(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.tabs.get(ix).map(|tab| tab.entry.clone()) else {
            return;
        };
        let front = self.front_repo();
        self.app
            .update(cx, |app, cx| app.toggle_pinned(&entry, cx));
        let pinned = self.app.read(cx).tabs().pinned.clone();
        self.tabs.sort_by_key(|tab| !pinned.contains(&tab.entry));
        self.refocus(front, Page::Workspaces);
        self.persist(cx);
        cx.notify();
    }

    fn is_pinned(&self, entry: &RepoEntry, cx: &App) -> bool {
        self.app.read(cx).tabs().is_pinned(entry)
    }

    /// 关掉一批标签页：`which` 挑出要关的（参数是下标和仓库）。固定的不关；正在做操作的、有没保存
    /// 的文件的也留着（原来没保存的照关，修改就丢了），并说一声。显示着的那一页没被关掉就还留在它上面。
    fn close_many(
        &mut self,
        which: impl Fn(usize, &RepoEntry) -> bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pinned = self.app.read(cx).tabs().pinned.clone();
        let mut skipped = 0;
        let targets: Vec<RepoEntry> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(ix, tab)| which(*ix, &tab.entry) && !pinned.contains(&tab.entry))
            .filter(|(_, tab)| {
                let kept = Self::unsafe_to_drop(tab, cx);
                skipped += kept as usize;
                !kept
            })
            .map(|(_, tab)| tab.entry.clone())
            .collect();
        for entry in targets {
            if let Some(at) = self.tabs.iter().position(|t| t.entry == entry) {
                self.close_now(at, window, cx);
            }
        }
        if skipped > 0 {
            let text = tf!(
                "{n} 个标签页有未保存的文件或正在执行操作，未关闭。",
                n = skipped
            );
            window.push_notification(Notification::warning(text), cx);
        }
        cx.notify();
    }

    /// 关掉除了第 `keep` 个以外的标签页（`None` 是全关），关完显示留下的那一个。
    fn close_others(&mut self, keep: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let keep_entry = keep
            .and_then(|ix| self.tabs.get(ix))
            .map(|tab| tab.entry.clone());
        self.close_many(|_, entry| Some(entry) != keep_entry.as_ref(), window, cx);
        if let Some(ix) =
            keep_entry.and_then(|entry| self.tabs.iter().position(|t| t.entry == entry))
        {
            self.show_page(Page::Repo(ix), window, cx);
        }
    }

    /// 关掉第 `ix` 个右边的标签页。
    fn close_right(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.close_many(|at, _| at > ix, window, cx);
    }

    /// 标签页右键的「在工作区页中显示」：到工作区页，选中包含它的那个工作区。
    fn reveal_in_workspaces(
        &mut self,
        entry: &RepoEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.app.read(cx).workspace_of(entry) else {
            return;
        };
        self.app.update(cx, |app, cx| app.set_current(key, cx));
        self.show_page(Page::Workspaces, window, cx);
    }

    /// 服务器页上点了一台服务器的「N 个工作区」：到工作区页看那台的。
    fn show_workspaces_of(&mut self, machine: String, window: &mut Window, cx: &mut Context<Self>) {
        self.app
            .update(cx, |app, _| app.menu_machine = Some(machine));
        self.show_page(Page::Workspaces, window, cx);
    }

    /// 工作区页上点了一个仓库的终端个数：到终端页，选中它。
    fn show_terminals_of(
        &mut self,
        entry: &RepoEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_page(Page::Terms, window, cx);
        self.overview
            .update(cx, |overview, cx| overview.reveal_repo(entry, window, cx));
    }

    /// 打开设置页（Ctrl+,）；已经开着就关掉。
    pub fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let was = (!self.showing_settings).then_some(self.page);
        self.showing_settings = !self.showing_settings;
        self.page_changed(was, window, cx);
        self.update_visibility(cx);
        cx.notify();
    }

    /// Ctrl+Shift+A：打开终端页；已经在上面就回到原来那一页。
    pub fn toggle_terms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page == Page::Terms && !self.showing_settings {
            let back = match self.before_terms {
                Page::Repo(_) => self
                    .before_terms_repo
                    .as_ref()
                    .and_then(|entry| self.tabs.iter().position(|tab| tab.entry == *entry))
                    .map_or(Page::Workspaces, Page::Repo),
                page => page,
            };
            self.show_page(back, window, cx);
            // 焦点还在看不见的终端页上：放回显示出来的终端里
            if let Some(tab) = self.active_repo() {
                tab.shown(cx)
                    .update(cx, |view, cx| view.focus_shown_terminal(window, cx));
            }
        } else {
            self.show_terms(window, cx);
        }
    }

    /// 到终端页（标题栏上的「终端」）。
    fn show_terms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page != Page::Terms {
            self.before_terms = self.page;
            self.before_terms_repo = self.front_repo();
        }
        self.show_page(Page::Terms, window, cx);
    }

    /// 提醒里的「前往」、点了系统通知：到终端页选中这个终端。
    fn go_to_terminal(&mut self, row: &TermRow, window: &mut Window, cx: &mut Context<Self>) {
        log::info!("前往终端：{} 的第 {} 个", row.entry.path, row.number);
        self.show_terms(window, cx);
        let slot = row.slot();
        self.overview
            .update(cx, |overview, cx| overview.reveal_term(&slot, window, cx));
        self.seen_terminal(row, window, cx);
    }

    /// 看过这个终端了：不再算「已停下、没看过」，说它停下了的那条提醒用不着了。
    fn seen_terminal(&mut self, row: &TermRow, window: &mut Window, cx: &mut Context<Self>) {
        self.board
            .update(cx, |board, cx| board.mark_seen(&row.id, cx));
        let key = stopped_note_key(&row.id);
        self.toast_job(ToastJob::Remove(key.to_string()));
        window.remove_notification1::<TerminalStopped>(key, cx);
    }

    /// 从终端页打开一个仓库（工具栏的「打开仓库」、右键菜单、Enter、双击）：打开它的标签页，显示
    /// Git 页。
    fn open_repo_git(&mut self, entry: &RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.open(entry.clone(), window, cx);
        if let Some(tab) = self.active_repo() {
            tab.shown(cx)
                .update(cx, |view, cx| view.show_git_page(window, cx));
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
            self.board.read(cx).row(&id)
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
            Some(title) => {
                let title = crate::terminal::short_title(title);
                tf!("{repo} 的终端已停下：{title}", repo, title)
            }
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
        if !window.is_window_active() || self.showing_settings {
            return None;
        }
        if self.page == Page::Terms {
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
        match self.page {
            Page::Repo(ix) if !self.showing_settings => self.tabs.get(ix),
            _ => None,
        }
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
            self.toggle_settings(window, cx);
        } else if self.page == Page::Terms {
            self.toggle_terms(window, cx);
        } else if let Page::Repo(ix) = self.page {
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

    /// Ctrl+Shift+T：重新打开最近关掉的标签页。
    pub fn reopen_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.app.read(cx).closed_tabs().into_iter().next() {
            Some(entry) => self.open(entry, window, cx),
            None => {
                let text = tr("没有最近关闭的标签页。");
                window.push_notification(Notification::info(text), cx);
            }
        }
    }

    /// Ctrl+Shift+`：当前仓库再开一个终端，切到终端页显示它。
    pub fn new_terminal_in_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 终端页上：开在那里选中的仓库里
        if self.page == Page::Terms && !self.showing_settings {
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

    /// Ctrl+Tab、Ctrl+Shift+Tab：切到下一个、上一个标签页，到头绕回去。在三个页面上时从第一个
    /// （往回是最后一个）开始。
    pub fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let order: Vec<Page> = (0..self.tabs.len()).map(Page::Repo).collect();
        let count = order.len() as isize;
        if count == 0 {
            return;
        }
        let next = match order.iter().position(|page| *page == self.page) {
            Some(at) => (at as isize + step).rem_euclid(count),
            None if step > 0 => 0,
            None => count - 1,
        };
        self.show_page(order[next as usize], window, cx);
    }

    /// Ctrl+F：在当前仓库页正看着的东西里找（文件、改动、提交……见 `RepoView::find`）。
    pub fn find_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.shown(cx).update(cx, |view, cx| view.find(window, cx));
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

    /// 告诉各个标签页显不显示，把开着的标签页和显示着的那一页记进设置。
    fn persist(&self, cx: &mut Context<Self>) {
        self.update_visibility(cx);
        let repos: Vec<RepoEntry> = self.tabs.iter().map(|t| t.entry.clone()).collect();
        let front = match self.page {
            Page::Servers => Front::Servers,
            Page::Terms => Front::Terms,
            Page::Workspaces => Front::Workspaces,
            Page::Repo(ix) => repos
                .get(ix)
                .cloned()
                .map_or(Front::Workspaces, Front::Repo),
        };
        self.app.update(cx, |app, _| app.set_tabs(repos, front));
    }

    // ---- 标题栏 ----

    /// 每个仓库标签页名字后面要不要带一截说明：开着的标签页里有同名的才带（像 VS Code 的同名文件），
    /// 子仓库带它所在的仓库的名字，别的带是哪台电脑。
    fn tab_hints(&self, cx: &App) -> Vec<Option<SharedString>> {
        let app = self.app.read(cx);
        let names: Vec<SharedString> = self
            .tabs
            .iter()
            .map(|tab| app.repo_name(&tab.entry))
            .collect();
        self.tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let twin = names
                    .iter()
                    .enumerate()
                    .any(|(other, name)| other != ix && *name == names[ix]);
                twin.then(|| match app.root_of(&tab.entry) {
                    Some(root) => app.repo_name(&root),
                    None => app.machine_name(crate::machines::machine_of(&tab.entry)),
                })
            })
            .collect()
    }

    /// 第 `ix` 个仓库标签页上要画的东西。`hint` 见 [`Self::tab_hints`]。
    fn repo_tab_look(&self, ix: usize, hint: Option<SharedString>, cx: &App) -> RepoTabLook {
        let tab = &self.tabs[ix];
        let app = self.app.read(cx);
        let name = app.repo_name(&tab.entry);
        let view = tab.view.read(cx);
        let inner = view.sub_title(cx);
        // 在这个标签页里进到了子仓库、别的工作树：名字后面带上它的名字
        let title: SharedString = match &inner {
            Some(sub) => format!("{name} / {sub}").into(),
            None => name,
        };
        let glyph = if view.busy_with_subs(cx).is_some() {
            IconName::LoaderCircle
        } else if app.root_of(&tab.entry).is_some() {
            IconName::FolderTree
        } else if tab.entry.server.is_some() {
            IconName::Server
        } else {
            IconName::FolderGit2
        };
        let pinned = app.tabs().is_pinned(&tab.entry);
        // 停上去显示完整的名字、哪台电脑和路径；经服务器连的写明经由哪台
        let tip_title: SharedString = match &inner {
            Some(sub) => format!("{} / {sub}", app.repo_tip(&tab.entry)).into(),
            None => app.repo_tip(&tab.entry),
        };
        RepoTabLook {
            title,
            hint,
            glyph,
            changes: view.change_count_with_subs() > 0,
            pinned,
            show_close: app.settings.tab_close_button && !pinned,
            selected: self.page == Page::Repo(ix) && !self.showing_settings,
            tip_title,
        }
    }

    /// 一个仓库标签页：图标、名字、关闭按钮（固定着的是一个图钉），`width` 宽（见 `tab_fit`）。
    ///
    /// 标签上只写名字（子仓库也只写子仓库的名字，靠图标区分），全名、哪台电脑、路径在悬停提示里。
    /// 1.16 以前写「名字 · 服务器」，子仓库写「根 / 子」，几个一起开时前半截都一样，挤一点就分不清。
    fn render_repo_tab(
        &self,
        ix: usize,
        look: &RepoTabLook,
        width: f32,
        with_hint: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tab = &self.tabs[ix];
        let (accent, muted) = (cx.theme().primary, cx.theme().muted_foreground);
        let app = self.app.read(cx);
        let can_reopen = !app.closed_tabs().is_empty();
        let in_workspace = app.workspace_of(&tab.entry).is_some();
        let close = tab_close(("close-tab", ix), look.selected, cx).on_click(cx.listener(
            move |this, _, window, cx| {
                cx.stop_propagation();
                this.close(ix, window, cx);
            },
        ));
        let (menu_this, menu_app, entry) =
            (cx.entity().downgrade(), self.app.clone(), tab.entry.clone());
        let tip_title = look.tip_title.clone();
        let tip_path = SharedString::from(tab.entry.path.clone());
        tab_shell(("tab", ix), look.selected, cx)
            .flex_none()
            .w(px(width))
            .tooltip(move |window, cx| {
                ui::two_line_tip(tip_title.clone(), tip_path.clone(), window, cx)
            })
            .on_click(
                cx.listener(move |this, _, window, cx| this.show_page(Page::Repo(ix), window, cx)),
            )
            // 中键关标签页
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _, window, cx| this.close(ix, window, cx)),
            )
            .child(ui::icon(look.glyph).flex_none())
            .child(
                div()
                    .flex_shrink(1.)
                    .min_w_0()
                    .truncate()
                    .child(look.title.clone()),
            )
            .when_some(look.hint.clone().filter(|_| with_hint), |tab, hint| {
                tab.child(
                    div()
                        .flex_shrink(4.)
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(muted)
                        .child(hint),
                )
            })
            // 有没提交的改动：名字后面一个小点
            .when(look.changes, |tab| {
                tab.child(div().flex_none().size(px(6.)).rounded_full().bg(accent))
            })
            // 固定着的没有关闭按钮，那个位置是一个图钉
            .when(look.pinned, |tab| {
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
            .when(look.show_close, |tab| tab.child(close))
            .context_menu(move |menu, _, cx| {
                tab_menu(
                    menu,
                    ix,
                    entry.clone(),
                    can_reopen,
                    in_workspace,
                    &menu_app,
                    &menu_this,
                    cx,
                )
            })
            .into_any_element()
    }

    /// 「⌄ N」：放不下收起来了几个。点开列出全部开着的标签页，后面淡淡写在哪台（子仓库写它所在的
    /// 仓库），当前的打勾；点了切过去。
    fn render_more_button(&self, hidden: usize, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let app = self.app.read(cx);
        let repos: Vec<(IconName, SharedString, SharedString, bool)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let root = app.root_of(&tab.entry);
                let glyph = match (&root, &tab.entry.server) {
                    (Some(_), _) => IconName::FolderTree,
                    (None, Some(_)) => IconName::Server,
                    (None, None) => IconName::FolderGit2,
                };
                let aside = match &root {
                    Some(root) => app.repo_name(root),
                    None => app.machine_name(crate::machines::machine_of(&tab.entry)),
                };
                let on = self.page == Page::Repo(ix) && !self.showing_settings;
                (glyph, app.repo_name(&tab.entry), aside, on)
            })
            .collect();
        let this = cx.entity().downgrade();
        div()
            .occlude()
            .flex_none()
            .child(
                Button::new("tab-more")
                    .ghost()
                    .xsmall()
                    .icon(ui::icon(IconName::ChevronDown))
                    .label(hidden.to_string())
                    .tooltip(tf!("另有 {n} 个标签页未显示，点击列出全部", n = hidden))
                    .dropdown_menu(move |mut menu, _, _| {
                        menu = menu.min_w(px(300.)).max_h(px(520.)).scrollable(true);
                        for (ix, (glyph, name, aside, on)) in repos.iter().enumerate() {
                            let this = this.clone();
                            let (glyph, name, aside) = (*glyph, name.clone(), aside.clone());
                            menu = menu.item(
                                PopupMenuItem::element(move |_, _| {
                                    h_flex()
                                        .w_full()
                                        .gap(px(8.))
                                        .child(ui::icon(glyph).flex_none())
                                        .child(
                                            div()
                                                .flex_shrink(1.)
                                                .min_w_0()
                                                .truncate()
                                                .child(name.clone()),
                                        )
                                        .child(
                                            div()
                                                .flex_shrink(4.)
                                                .min_w_0()
                                                .truncate()
                                                .text_xs()
                                                .text_color(muted)
                                                .child(aside.clone()),
                                        )
                                })
                                .checked(*on)
                                .on_click(move |_, window, cx| {
                                    this.update(cx, |this, cx| {
                                        this.show_page(Page::Repo(ix), window, cx)
                                    })
                                    .ok();
                                }),
                            );
                        }
                        menu
                    }),
            )
            .into_any_element()
    }

    /// 标题栏最左边的一个页面入口（服务器、终端、工作区）：长得和标签页一样，名字固定，不跟着选了
    /// 哪台服务器、哪个工作区变。窗口窄时
    /// 只留图标，悬停看名字。
    fn render_page_entry(
        &self,
        page: Page,
        id: &'static str,
        icon: IconName,
        label: &'static str,
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let selected = self.page == page && !self.showing_settings;
        tab_shell(id, selected, cx)
            .flex_none()
            .when(narrow, |tab| tab.px(px(8.)))
            .on_click(cx.listener(move |this, _, window, cx| match page {
                Page::Terms => this.show_terms(window, cx),
                _ => this.show_page(page, window, cx),
            }))
            .child(ui::icon(icon).flex_none())
            .when(!narrow, |tab| tab.child(label))
    }

    fn render_title_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 标签页那一块有多宽要等排完版才知道：按上一帧量到的排，宽度变了（拉窗口、设置页开关、
        // 升级的小牌出来）下一帧再排一次
        let viewport = window.viewport_size().width.as_f32();
        let measured = self.tab_region.get();
        let region = if measured > 0. {
            measured
        } else {
            (viewport - 480.).max(200.)
        };
        self.tab_region_used = region;
        {
            let cell = self.tab_region.clone();
            cx.on_next_frame(window, move |this, _, cx| {
                if (cell.get() - this.tab_region_used).abs() > 0.5 {
                    cx.notify();
                }
            });
        }
        let theme = cx.theme();
        let (border, muted, warning) = (theme.border, theme.muted_foreground, theme.warning);
        // 窗口窄的时候三个页面只留图标，给标签页腾地方
        let narrow = viewport < NARROW_WIDTH;
        let servers_label = trc("页面", "主机");
        let servers = self
            .render_page_entry(
                Page::Servers,
                "page-servers",
                IconName::Server,
                servers_label,
                narrow,
                cx,
            )
            // 写着字时提示只是把字再说一遍：只留图标时才要
            .when(narrow, |tab| {
                tab.tooltip(move |window, cx| ui::tip(servers_label, window, cx))
            });
        // 所有电脑上的终端：已停下、还没看过的有几个就标几个
        let waiting = self.board.read(cx).attention();
        let terms = self
            .render_page_entry(
                Page::Terms,
                "page-terms",
                IconName::SquareTerminal,
                trc("页面", "终端"),
                narrow,
                cx,
            )
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
                    0 => tr("所有终端（Ctrl+Shift+A）").to_string(),
                    n => tf!(
                        "所有终端（Ctrl+Shift+A）：{n} 个已停下，尚未查看",
                        n = n.to_string()
                    ),
                };
                ui::tip(text, window, cx)
            });
        let workspaces_label = trc("页面", "工作区");
        let workspaces = self
            .render_page_entry(
                Page::Workspaces,
                "page-workspaces",
                IconName::Layers,
                workspaces_label,
                narrow,
                cx,
            )
            .when(narrow, |tab| {
                tab.tooltip(move |window, cx| ui::tip(workspaces_label, window, cx))
            });
        // 先量出每个标签不挤时多宽，再决定怎么缩、哪些收进「⌄」（见 `tab_fit`）
        let hints = self.tab_hints(cx);
        let looks: Vec<RepoTabLook> = hints
            .into_iter()
            .enumerate()
            .map(|(ix, hint)| self.repo_tab_look(ix, hint, cx))
            .collect();
        let naturals: Vec<f32> = looks
            .iter()
            .map(|look| repo_tab_natural(look, window))
            .collect();
        let bases: Vec<f32> = looks
            .iter()
            .map(|look| repo_tab_base(look, window))
            .collect();
        // 标签页那一块里除了标签还有：「+」、拖窗口的空白、设置页开着时它的标签，和它们之间的间隔
        let settings_width = match self.showing_settings {
            true => {
                let label = ui::text_width(window, tr("设置"), 13., None, FontWeight::NORMAL);
                22. + 14. + 6. + label + 6. + 16. + TAB_GAP
            }
            false => 0.,
        };
        let room = region - (20. + TAB_GAP) - (24. + TAB_GAP) - settings_width;
        let active =
            (0..self.tabs.len()).find(|ix| !self.showing_settings && self.page == Page::Repo(*ix));
        let fit = tab_fit::fit(
            &Segment {
                natural: &naturals,
                min: tab_fit::REPO_MIN,
                active,
            },
            room,
            TAB_GAP,
            MORE_WIDTH + TAB_GAP,
        );
        let hidden = fit.hidden(naturals.len());
        // 名字后面那一截淡淡的字（同名的说明）压得只剩一点时不画，免得只剩一个省略号
        let repo_tabs: Vec<AnyElement> = fit
            .shown
            .iter()
            .map(|(ix, width)| {
                let aside = *width >= bases[*ix] + ASIDE_MIN;
                self.render_repo_tab(*ix, &looks[*ix], *width, aside, cx)
            })
            .collect();
        let more = fit.overflow.then(|| self.render_more_button(hidden, cx));
        let settings_tab =
            self.showing_settings.then(|| {
                tab_shell("tab-settings", true, cx)
                    .flex_none()
                    .child(ui::icon(IconName::Settings).flex_none())
                    .child(tr("设置"))
                    .child(tab_close("close-settings", true, cx).on_click(
                        cx.listener(|this, _, window, cx| this.toggle_settings(window, cx)),
                    ))
            });
        let measure = {
            let cell = self.tab_region.clone();
            canvas(
                move |bounds, _, _| cell.set(bounds.size.width.as_f32()),
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0()
        };
        // 标签页这一块：仓库标签、「⌄ N」、「+」、拖窗口的空白
        let strip = h_flex()
            .absolute()
            .inset_0()
            .gap(px(TAB_GAP))
            .children(repo_tabs)
            .children(settings_tab)
            .children(more)
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
            // 这一段是空的：按住可以拖窗口
            .child(div().flex_1().min_w(px(24.)).h_full());
        TitleBar::new().h(px(TITLE_HEIGHT)).child(
            h_flex()
                .h_full()
                .flex_1()
                .min_w_0()
                .gap(px(4.))
                // 三个页面在最前，后面是标签页
                .child(servers)
                .child(terms)
                .child(workspaces)
                .child(div().flex_none().w(px(1.)).h(px(16.)).mx(px(4.)).bg(border))
                // 标签页那一块画在这个占位的块里面（绝对定位）。标题栏的宽度按里面内容的
                // 最小宽度算，直接把标签页排在这一行里的话，开多了会把整行撑宽，
                // 右边的设置和窗口按钮被挤出窗口
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(measure)
                        .child(strip),
                )
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
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_settings(window, cx)),
                            ),
                    ),
                ),
        )
    }
}

/// 一个仓库标签页上要画的东西：量宽度和画的时候用同一份（见 [`repo_tab_natural`]）。
struct RepoTabLook {
    title: SharedString,
    /// 开着的标签页里有同名的时候，名字后面淡淡写的那一截
    hint: Option<SharedString>,
    glyph: IconName,
    /// 有没提交的改动：名字后面一个小点
    changes: bool,
    pinned: bool,
    /// 有关闭按钮（设置里没藏起来、没固定）。选中的排在名字后面；没选中的鼠标移上去才浮出来，不占地方
    show_close: bool,
    selected: bool,
    tip_title: SharedString,
}

/// 仓库标签页不挤时多宽：和 `render_repo_tab` 画的一样算（边框、内边距、图标、名字、说明、改动的
/// 小点、图钉、选中时排在后面的关闭按钮），最宽 [`REPO_TAB_MAX`]。
fn repo_tab_natural(look: &RepoTabLook, window: &Window) -> f32 {
    let mut width = repo_tab_base(look, window);
    if let Some(hint) = &look.hint {
        width += 6. + ui::text_width(window, hint, 12., None, FontWeight::NORMAL);
    }
    width.ceil().min(REPO_TAB_MAX)
}

/// 仓库标签页不算同名说明那一截时多宽。
fn repo_tab_base(look: &RepoTabLook, window: &Window) -> f32 {
    let mut width =
        2. + 20. + 14. + 6. + ui::text_width(window, &look.title, 13., None, FontWeight::NORMAL);
    if look.changes {
        width += 6. + 6.;
    }
    if look.pinned {
        width += 6. + 16.;
    }
    if look.selected && look.show_close {
        width += 6. + 16.;
    }
    width
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

/// 服务器连没连上：一句话和圆点的颜色。
pub(crate) fn machine_status(state: &AppState, machine: &str, cx: &App) -> (SharedString, Hsla) {
    let theme = cx.theme();
    match state.machine(machine) {
        Some(loaded) if loaded.live => (tr("已连接").into(), theme.success),
        Some(_) => (tr("已断开").into(), theme.muted_foreground),
        None => match state.status(machine) {
            crate::app_state::ConnStatus::Connecting => (tr("正在连接…").into(), theme.primary),
            // 和服务器页一样写「连接失败」（原因在服务器页上）
            crate::app_state::ConnStatus::Failed(_) => (tr("连接失败").into(), theme.danger),
            _ => (tr("未连接").into(), theme.muted_foreground),
        },
    }
}

/// 仓库标签页的右键菜单。`in_workspace`：有工作区包含它（「在工作区页中显示」才有用）。
#[allow(clippy::too_many_arguments)]
fn tab_menu(
    menu: PopupMenu,
    ix: usize,
    entry: RepoEntry,
    can_reopen: bool,
    in_workspace: bool,
    app: &Entity<AppState>,
    this: &WeakEntity<Workspace>,
    cx: &App,
) -> PopupMenu {
    let (close, others, right, all) = (this.clone(), this.clone(), this.clone(), this.clone());
    let (reopen, pin, reveal, rename) = (this.clone(), this.clone(), this.clone(), this.clone());
    let reveal_entry = entry.clone();
    let path = entry.path.clone();
    let pinned = app.read(cx).tabs().is_pinned(&entry);
    menu.item(
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
        PopupMenuItem::new(tr("关闭右侧标签页")).on_click(move |_, window, cx| {
            right
                .update(cx, |this, cx| this.close_right(ix, window, cx))
                .ok();
        }),
    )
    .item(
        PopupMenuItem::new(tr("全部关闭")).on_click(move |_, window, cx| {
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
    .separator()
    .item(
        PopupMenuItem::new(tr("在工作区页中显示"))
            .disabled(!in_workspace)
            .on_click(move |_, window, cx| {
                reveal
                    .update(cx, |this, cx| {
                        this.reveal_in_workspaces(&reveal_entry, window, cx)
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
pub(crate) fn ask_name<T: 'static>(
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
pub(crate) fn new_workspace_dialog<T: 'static>(
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
        } else {
            match self.page {
                Page::Servers => self.manager.clone().into_any_element(),
                Page::Terms => self.overview.clone().into_any_element(),
                Page::Workspaces => self.workspaces.clone().into_any_element(),
                Page::Repo(ix) => match self.tabs.get(ix) {
                    Some(tab) => tab.view.clone().into_any_element(),
                    None => self.workspaces.clone().into_any_element(),
                },
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
    /// 先是刚关掉的（最近关的在最上），再是开着的，最后是各台电脑列表里的其余仓库（本机的在前）。
    /// 第三项是行尾标的小字：刚关掉、开着。
    fn matches(&self, cx: &App) -> Vec<(RepoEntry, SharedString, Option<&'static str>)> {
        let needle = self.input.read(cx).value().trim().to_lowercase();
        let app = self.app.read(cx);
        let open = app.tabs().repos.clone();
        let closed: Vec<RepoEntry> = app
            .closed_tabs()
            .into_iter()
            .filter(|entry| !open.contains(entry))
            .collect();
        let rest: Vec<RepoEntry> = app
            .all_repos()
            .into_iter()
            .filter(|entry| !open.contains(entry) && !closed.contains(entry))
            .collect();
        closed
            .iter()
            .map(|entry| (entry.clone(), Some(tr("最近关闭"))))
            .chain(open.iter().map(|entry| (entry.clone(), Some(tr("已打开")))))
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
