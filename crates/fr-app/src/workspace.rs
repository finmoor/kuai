//! 主窗口：标题栏（三个页面、标签页、设置）和下面的内容。
//!
//! 标题栏是自己画的，标签页直接放在里面，不另占一行。最左边是三个页面：服务器、终端、工作区，
//! 长得一样，名字固定不变；后面是标签页：仓库（和子仓库）一个一个排（设计见
//! `design/server-workspace/`，第二版）。设置页打开时也是一个标签页。1.17 右边还有一段终端的标签页
//! （T3），2026-11-20 去掉了：终端在会话页里看，那里的「打开仓库」打开它所在的仓库。
//!
//! 标签页全局一份，像 VS Code 的编辑器标签：打开哪个就是哪个，和选了哪台服务器、哪个工作区都
//! 无关。从哪里打开（工作区页、服务器页、会话页、快速打开）都是往这一排里加，
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
use std::sync::Arc;

use fr_core::i18n::{self, tr, trc};
use fr_core::tf;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, TitleBar, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::history::History;
use crate::machines::{LOCAL, WorkspaceKey, machine_of};
use crate::manager::{ManagerEvent, ManagerView};
use crate::prompt::{self, Prompts};
use crate::repo::{RepoEvent, RepoView};
use crate::repo_scan::RepoScan;
use crate::settings::{Front, RepoEntry};
use crate::settings_page::SettingsPage;
use crate::tab_fit::{self, Segment};
use crate::term_board::{
    AwayUpdates, BoardHandle, Row as TermRow, Shown, TermBoard, TermId, Waiting,
};
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

/// 后退、前进记下的一步：在哪一页。仓库按它本身记（标签页的位置会变），工作区页记着列的是哪台
/// 电脑、选中的是哪个工作区（那台上还没有工作区时是 None）。设计见 `docs/design/back-and-workspace/`
/// （方向 B）。
#[derive(Clone, PartialEq, Eq, Debug)]
enum Nav {
    Servers,
    Terms,
    Workspaces {
        machine: String,
        key: Option<WorkspaceKey>,
    },
    Repo(RepoEntry),
}

/// 还回得去：仓库的标签页关了、工作区删了、主机移除了就回不去，后退、前进时跳过（提示里也不写）。
fn reachable(tabs: &[RepoTab], app: &AppState, nav: &Nav) -> bool {
    match nav {
        Nav::Repo(entry) => tabs.iter().any(|tab| tab.entry.same_repo(entry)),
        Nav::Workspaces { machine, key } => {
            let known = machine == LOCAL || app.settings.servers.iter().any(|s| s.id == *machine);
            known && key.as_ref().is_none_or(|key| app.workspace(key).is_some())
        }
        _ => true,
    }
}

/// 主窗口的工作区：别的页面要换页（打开工作区）时经它找过来。
pub(crate) struct Navigator(WeakEntity<Workspace>);

impl Global for Navigator {}

/// 到工作区页，选中工作区 `key`，`entry` 那一行标出来（会话页、仓库页、标签页右键的「打开工作区」，
/// 见 `ws_links`）。推迟到这一轮事件处理完再做：点的那个页面这时正被占用，换页要告诉它不显示了。
pub(crate) fn go_to_workspace(
    key: WorkspaceKey,
    entry: Option<RepoEntry>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(this) = cx.try_global::<Navigator>().map(|nav| nav.0.clone()) else {
        return;
    };
    window.defer(cx, move |window, cx| {
        this.update(cx, |this, cx| this.open_workspace(key, entry, window, cx))
            .ok();
    });
}

/// 开机自启时等多久问出有没有托盘（桌面刚登录，会话总线、托盘可能还没起来）。
const TRAY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// 退出、重启以前要问的：会结束几个本机终端里的程序，丢掉几个没保存的文件，映射的目录里别的
/// 程序开着的几个文件之后读写不了。
struct Losses {
    terminals: usize,
    unsaved: usize,
    mapped: usize,
}

impl Losses {
    fn none(&self) -> bool {
        self.terminals == 0 && self.unsaved == 0 && self.mapped == 0
    }

    /// 对话框里的说明。`restart`：是重启（升级），不是退出。
    fn describe(&self, restart: bool) -> String {
        let (n, m, k) = (self.unsaved, self.terminals, self.mapped);
        let mapped = match restart {
            false => tf!(
                "其他程序在映射的目录中打开了 {n} 个文件，退出后将无法继续读写。",
                n = k
            ),
            true => tf!(
                "其他程序在映射的目录中打开了 {n} 个文件，重启后将无法继续读写。",
                n = k
            ),
        };
        let main = match (m, n, restart) {
            (0, 0, _) => return mapped,
            (_, 0, false) => tf!(
                "有 {n} 个本机终端正在运行，退出将结束其中的程序。远程主机上的终端不受影响。",
                n = m
            ),
            (_, 0, true) => tf!(
                "有 {n} 个本机终端正在运行，重启将结束其中的程序。远程主机上的终端不受影响。",
                n = m
            ),
            (0, _, false) => tf!("有 {n} 个文件尚未保存，退出后修改将丢失。", n),
            (0, _, true) => tf!("有 {n} 个文件尚未保存，重启后修改将丢失。", n),
            (_, _, false) => tf!(
                "有 {n} 个文件尚未保存，退出后修改将丢失。另有 {m} 个本机终端正在运行，退出将结束其中的程序。",
                n,
                m
            ),
            (_, _, true) => tf!(
                "有 {n} 个文件尚未保存，重启后修改将丢失。另有 {m} 个本机终端正在运行，重启将结束其中的程序。",
                n,
                m
            ),
        };
        match k {
            0 => main,
            _ => format!("{main}\n{mapped}"),
        }
    }
}

/// 上一次应用到窗口上的外观设置；变了才重新应用。
#[derive(PartialEq, Clone)]
struct Applied {
    palette: &'static str,
    lang: i18n::Lang,
    tray: bool,
    /// 代码字号、Tab 宽度
    code: (u8, u8),
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
    /// 按 Ctrl+Shift+A 打开会话页之前在哪一页：再按一次回去
    before_terms: Page,
    /// 那一页是仓库标签页时是哪个仓库：回去时按它找。下标靠不住，在会话页上关了、固定了别的标签页
    /// 位置就变了，原来会回到别的仓库，或者一个没有标签页的空页
    before_terms_repo: Option<RepoEntry>,
    /// 后退、前进（标题栏最左边的两个按钮、Alt+←、Alt+→、鼠标侧键）：去过的页面
    history: History<Nav>,
    /// 正在后退、前进：这次换页不记
    traveling: bool,
    /// 上次离开会话页时焦点在终端里：退回去时放回去，接着打字
    terms_typing: bool,
    /// 主机移除了、因为有没保存的文件或正在做操作而暂时没关的标签页（已经说过一次，不再说）
    kept_on_removal: Vec<RepoEntry>,
    /// 设置页开着；它盖在当前标签页上面，关掉回到原来那一页
    showing_settings: bool,
    /// 所有工作区里的终端现在怎么样（见 `term_board`）
    board: Entity<TermBoard>,
    /// 会话页（标题栏上的「会话」，Ctrl+Shift+A）
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
    /// 窗口收进了托盘（藏起来了）：托盘下线时要拿出来
    tucked: Cell<bool>,
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
                ManagerEvent::RunInTerminal(entry, start) => {
                    this.new_terminal_for(entry.clone(), start.clone(), window, cx)
                }
            }),
            cx.subscribe_in(
                &workspaces,
                window,
                |this, _, event, window, cx| match event {
                    // 从哪个工作区打开的：仓库页的工作区按钮先写它
                    PageEvent::Open(entry) => {
                        let home = this.workspaces.read(cx).selected(cx);
                        this.open(entry.clone(), window, cx);
                        this.hint_home(home, cx);
                    }
                    PageEvent::OpenAll {
                        entries,
                        show,
                        home,
                    } => this.open_all(entries.clone(), *show, home.clone(), window, cx),
                    PageEvent::Rename(entry) => this.rename_repo(entry.clone(), window, cx),
                    PageEvent::Terminals(entry) => this.show_terminals_of(entry, window, cx),
                    PageEvent::RunInTerminal(entry, start) => {
                        this.new_terminal_for(entry.clone(), start.clone(), window, cx)
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
            // 窗口不在前台时，显示着的终端不算「正看着」：当场告诉 `board`，不等重画（见 `sync_shown`）
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.hide_if_minimized(cx);
                } else {
                    // 回到 Kuai 了：系统通知都收回，哪些停下了看标题栏上的个数
                    this.toast_job(ToastJob::Clear);
                    // 断网、睡眠后在等着重连的主机马上试一次，不用等到下一次（最多 30 秒）
                    this.app.update(cx, |app, cx| app.retry_now(cx));
                }
                this.sync_shown(window, cx);
                cx.notify();
            }),
            // 窗口藏起来、最小化了（没失去焦点也算，比如桌面上只剩它一个窗口）：同上
            cx.observe_window_visibility(window, |this, _, window, cx| {
                this.sync_shown(window, cx);
                cx.notify();
            }),
            // 标题栏上「会话」那一项的个数跟着变；会话页里换了选中的，也要重新告诉 `board`
            cx.observe(&board, |_, _, cx| cx.notify()),
            cx.observe(&overview, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &overview,
                window,
                |this, _, event: &Go, window, cx| match event {
                    Go::Repo(entry) => this.open_repo_git(entry, window, cx),
                    Go::File {
                        entry,
                        path,
                        at,
                        dir,
                    } => this.open_repo_path(entry, path.clone(), *at, *dir, window, cx),
                },
            ),
            cx.subscribe_in(&board, window, |this, _, event: &Waiting, window, cx| {
                this.terminal_waiting(&event.0, window, cx)
            }),
            cx.subscribe_in(&board, window, |this, _, event: &AwayUpdates, window, cx| {
                this.away_updates(event.0, window, cx)
            }),
            // 升级：小牌跟着检查、下载的进度变；点了「重启完成升级」先问一下再重启
            cx.observe(&updater, |_, _, cx| cx.notify()),
            cx.subscribe_in(&updater, window, |this, _, event, window, cx| match event {
                UpdateEvent::Restart => this.restart_for_update(window, cx),
                // .deb 的新版本在后台装好了：问过再重启。原来装好就直接退出，等认证的那几分钟里
                // 改了没保存的、新开的终端都没了
                UpdateEvent::Installed => this.restart_for_update(window, cx),
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
                // 端口转发的 ssh 都结束，不留在后台
                crate::port_forward::stop_all(cx);
                // 终端看过的时刻、各台主机最后一次连着的时刻存盘：下次启动认得出离开期间的新输出
                this.board.update(cx, |board, _| board.save_memory());
                async {}
            }),
            // 连上时用的是在弹窗里输入的密码、口令：问要不要存下来，以后连接不再问
            cx.subscribe_in(
                &app,
                window,
                |_, app, typed: &crate::app_state::SecretTyped, window, cx| {
                    let name = app.read(cx).machine_name(&typed.id).to_string();
                    let text = match typed.passphrase {
                        true => tf!("已连接 {name}。保存私钥口令后，以后连接不再询问。", name),
                        false => tf!("已连接 {name}。保存登录密码后，以后连接不再询问。", name),
                    };
                    let (app, id) = (app.clone(), typed.id.clone());
                    let note = Notification::info(text).action(move |_, _, cx| {
                        let (app, id, note) = (app.clone(), id.clone(), cx.entity().downgrade());
                        Button::new("save-typed-secret")
                            .small()
                            .primary()
                            .label(tr("保存"))
                            .on_click(move |_, window, cx| {
                                note.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                                let saving = app.update(cx, |app, cx| app.save_typed_secret(&id, cx));
                                window
                                    .spawn(cx, async move |cx| {
                                        let result = saving.await;
                                        cx.update(|window, cx| {
                                            let note = match result {
                                                Ok(()) => Notification::success(tr("已保存")),
                                                Err(err) => Notification::error(tf!(
                                                    "保存失败：{err}",
                                                    err
                                                ))
                                                .autohide(false),
                                            };
                                            window.push_notification(note, cx);
                                        })
                                        .ok();
                                    })
                                    .detach();
                            })
                    });
                    window.push_notification(note, cx);
                },
            ),
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
                        crate::mapping::Notice::NeedWinFsp if !cfg!(windows) => {
                            Notification::warning(tr(
                                "映射需要 fuse3，请先安装：sudo apt install fuse3（Fedora 为 sudo dnf install fuse3）。",
                            ))
                        }
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
                        crate::mapping::Notice::InstallFailed
                        | crate::mapping::Notice::NoWinget => {
                            let text = match notice {
                                crate::mapping::Notice::NoWinget => tr(
                                    "未找到 winget，无法自动安装 WinFsp。请从 WinFsp 官网下载安装，然后重新映射。",
                                ),
                                _ => tr("WinFsp 安装未完成。"),
                            };
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
        // 点了关闭（系统的标题栏、Alt+F4；Linux 上自己画的关闭按钮另走 `close_requested`）
        crate::tray::set_can_hide(win::hwnd(window).is_some());
        {
            let this = cx.entity().downgrade();
            window.on_window_should_close(cx, move |window, cx| {
                this.update(cx, |this, cx| this.close_requested(window, cx))
                    .unwrap_or(true)
            });
        }
        // Linux：注销桌面、kill、在终端里按 Ctrl+C 时照常退出（映射断开、托盘图标撤掉），见 signals.rs
        #[cfg(target_os = "linux")]
        if let Some(mut signals) = crate::signals::listen() {
            use futures::StreamExt as _;
            cx.spawn(async move |this, cx| {
                if let Some(signal) = signals.next().await {
                    log::info!("收到信号 {signal}，退出");
                    this.update(cx, |this, cx| {
                        this.quitting.set(true);
                        cx.quit();
                    })
                    .ok();
                }
            })
            .detach();
        }
        // 刚升级完：说一声，给个看更新说明的按钮
        if updater.read(cx).updated_from.is_some() {
            cx.defer_in(window, |_, window, cx| {
                let version = env!("CARGO_PKG_VERSION");
                let notification = Notification::success(tf!("已更新到 {version}", version))
                    .action(move |_, _, _| {
                        Button::new("update-notes")
                            .small()
                            .label(tr("查看更新内容"))
                            .on_click(move |_, window, cx| {
                                let page =
                                    format!("{}/releases/tag/v{version}", update::repo_url());
                                crate::shell::report(crate::shell::open_url(&page), window, cx);
                            })
                    });
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
            while let Some(click) = clicks.next().await {
                let alive = this.update_in(cx, |this, window, cx| {
                    this.toast_clicked(&click, window, cx);
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        // 还不知道有没有托盘（Linux 上启动时还在问）：问完了重画一遍，设置页里的托盘选项跟着出来
        if crate::tray::unknown() {
            cx.spawn(async move |_, cx| {
                let started = std::time::Instant::now();
                while crate::tray::unknown() && started.elapsed() < TRAY_WAIT {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(50))
                        .await;
                }
                cx.update(|cx| cx.refresh_windows());
            })
            .detach();
        }
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
            history: History::default(),
            traveling: false,
            terms_typing: false,
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
            tucked: Cell::new(false),
            applied: None,
            quitting,
            _subscriptions: subscriptions,
            _prompts: prompt::show_in(prompts, window, cx),
        };
        cx.set_global(Navigator(cx.weak_entity()));
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
            // Wayland 上藏不起来，「最小化到托盘」不显示，存着的值（X11 上开的）也不算：不然设置页
            // 上关不掉它，开机自启照样最小化
            tray: (settings.minimize_to_tray && crate::tray::can_hide()) || settings.close_to_tray,
            code: (settings.code_font_size, settings.tab_width),
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
        let palette_changed = before.as_ref().map(|a| a.palette) != Some(palette.id);
        if palette_changed {
            crate::appearance::apply_theme(&settings, window, cx);
        }
        // 代码字号写进主题（组件库的改动视图、编辑器都按它）；换主题时主题会重置，再写一次
        let code_changed = before.as_ref().map(|a| a.code) != Some(wanted.code);
        if code_changed || palette_changed {
            crate::zoom::apply(&settings, cx);
            // Tab 宽度变了：开着的改动按新的宽度重新排
            let tab_changed = before.as_ref().map(|a| a.code.1) != Some(wanted.code.1);
            if before.is_some() && tab_changed {
                let views: Vec<_> = self.tabs.iter().map(|tab| tab.view.clone()).collect();
                for view in views {
                    view.update(cx, |view, cx| view.tab_width_changed(cx));
                }
            }
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

    /// 「重启完成升级」，.deb 的新版本在后台装好了（[`UpdateEvent::Installed`]）也到这里：会丢掉
    /// 东西（没保存的文件、本机终端里在跑的程序、映射的目录里别的程序开着的文件）就先问一下，原来
    /// 只问终端。.deb 的还没装就先去装：装的时候不丢东西，装好了再来问。
    fn restart_for_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.updater.read(cx).installs_first() {
            self.updater.update(cx, |u, cx| u.install_and_relaunch(cx));
            return;
        }
        let losses = self.losses(cx);
        let (updater, quitting) = (self.updater.clone(), self.quitting.clone());
        let restart = Rc::new(move |cx: &mut App| {
            if updater.update(cx, |u, cx| u.install_and_relaunch(cx)) {
                quitting.set(true);
                cx.quit();
            }
        });
        if losses.none() {
            restart(cx);
            return;
        }
        let description = losses.describe(true);
        let (ok, variant) = match losses.unsaved {
            0 => (tr("重启"), ButtonVariant::Primary),
            _ => (tr("放弃修改并重启"), ButtonVariant::Danger),
        };
        window.open_alert_dialog(cx, move |alert, _, _| {
            let restart = restart.clone();
            alert
                .title(tr("重启 Kuai 以完成更新"))
                .description(description.clone())
                .on_ok(|_, _, _| false)
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .label(tr("取消"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(Button::new("ok").label(ok).with_variant(variant).on_click(
                            move |_, window, cx| {
                                window.close_dialog(cx);
                                restart(cx);
                            },
                        )),
                )
        });
    }

    /// 退出、重启会丢掉的（见 [`Losses`]）。
    fn losses(&self, cx: &App) -> Losses {
        Losses {
            terminals: self.live_terminals(cx),
            unsaved: self.unsaved_files(cx),
            mapped: self.app.read(cx).mapped_open_files(),
        }
    }

    /// 把藏到托盘、最小化或者被挡住的窗口叫到最前面。
    ///
    /// 放到这一轮更新之后再做：`ShowWindow` 会同步调用窗口过程，GPUI 在那里还要借用整个 App，
    /// 在更新里直接调会撞上正借着的 App（日志里的 `RefCell already borrowed`，这一帧可能不重画）。
    fn show_window(&self, cx: &mut Context<Self>) {
        self.tucked.set(false);
        if let Some(hwnd) = self.hwnd {
            cx.spawn(async move |_, _| win::show(hwnd)).detach();
        }
        // 常驻托盘时很少真的重启：再次启动、点托盘把窗口叫回来，就当作启动，顺便检查更新
        self.updater
            .update(cx, |updater, cx| updater.window_shown(cx));
    }

    /// 把窗口叫到前面（点了系统通知、托盘图标，又启动了一次 Kuai）。Windows 上 [`Self::show_window`]
    /// 会从托盘、最小化里还原；Linux 上请窗口管理器激活它（藏起来的、最小化的也还原），`token` 是
    /// 通知服务、托盘、启动器给的激活令牌（见 `win::activate`）。
    fn bring_to_front(&self, token: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if cfg!(windows) {
            self.show_window(cx);
            return;
        }
        self.tucked.set(false);
        self.updater
            .update(cx, |updater, cx| updater.window_shown(cx));
        win::activate(window, token.as_deref());
        // 在 GNOME 的通知面板里点的：面板还开着时窗口管理器不理激活请求，等它关了再请求一次
        cx.spawn_in(window, async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(400))
                .await;
            cx.update(|window, _| win::activate(window, token.as_deref()))
                .ok();
        })
        .detach();
    }

    /// 把窗口藏到托盘。和 [`Self::show_window`] 一样放到这一轮更新之后再做。Wayland 上藏不起来
    /// （没有窗口句柄），最小化。
    fn hide_window(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.hwnd {
            Some(hwnd) => {
                self.tucked.set(true);
                cx.spawn(async move |_, _| win::hide(hwnd)).detach();
                self.tray_tip_once(cx);
            }
            None => window.minimize_window(),
        }
    }

    /// 第一次收进托盘：发一条系统通知，说 Kuai 还在运行、怎么叫回来（只发一次，记在设置里）。
    /// Windows 默认把托盘图标收在任务栏右下角的「^」里，第一次用的人以为程序没了。
    fn tray_tip_once(&self, cx: &mut Context<Self>) {
        if self.app.read(cx).settings.tray_tip_shown {
            return;
        }
        self.app.update(cx, |app, cx| {
            app.update_settings(cx, |s| s.tray_tip_shown = true)
        });
        let body = match cfg!(windows) {
            true => tr("点击任务栏右下角的 Kuai 图标即可恢复窗口，图标可能收在「^」中。"),
            false => tr("点击系统托盘中的 Kuai 图标即可恢复窗口。"),
        };
        self.toast_job(ToastJob::Show {
            key: TRAY_TOAST.to_string(),
            title: tr("Kuai 已收进托盘，仍在运行").to_string(),
            body: body.to_string(),
        });
    }

    /// 点了关闭：返回要不要真的关（关了主窗口就退出）。设了关闭到托盘、托盘也在的，把窗口藏起来
    /// （Wayland 上藏不起来，最小化）；要退出而本机终端里还有程序在运行，先问一下（问了就先不关，
    /// 点「退出」时再退出）。
    fn close_requested(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.quitting.get() {
            return true;
        }
        if self.app.read(cx).settings.close_to_tray && self.in_tray() {
            self.hide_window(window, cx);
            return false;
        }
        if self.confirm_agents(window, cx) {
            return false;
        }
        !self.confirm_quit(window, cx)
    }

    /// 点了关闭、没设关闭到托盘，又有 AI 终端在跑：终端会接着在主机上跑，但退出以后它们
    /// 停下了不会再提醒，先问一句。可以最小化到托盘（顺便记成以后关闭都这样）、取消、仍要退出。
    /// 问了返回真（先不关，选了再说）。
    fn confirm_agents(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let agents = self.board.read(cx).agents();
        if agents.is_empty() {
            return false;
        }
        let app = self.app.read(cx);
        let lines: Vec<(SharedString, SharedString, Hsla)> = agents
            .iter()
            .take(4)
            .map(|row| {
                let place = tf!(
                    "{repo} · {state}",
                    repo = app.repo_name(&row.entry),
                    state = crate::term_overview::TermOverview::state_text(row)
                );
                let color = crate::term_overview::TermOverview::state_color(row, cx);
                (row.label.clone(), place.into(), color)
            })
            .collect();
        let more = agents.len().saturating_sub(lines.len());
        // 窗口藏得进托盘（Windows、X11 上有托盘）：给「最小化到托盘」，没开托盘的也给，点了再建托盘
        // 图标（见 `ensure_tray`）。藏不起来（Wayland）或者没有托盘：给「最小化窗口」，Kuai 不退出，
        // 终端停下了照样提醒（原来只剩「仍要退出」「取消」）
        let tray = crate::tray::available();
        let hide = tray && crate::tray::can_hide();
        let remember = Rc::new(Cell::new(false));
        let this = cx.entity().downgrade();
        let description = tf!(
            "{n} 个 AI 终端正在运行。终端将继续在主机上运行，但退出后不再提醒它们停下。",
            n = agents.len().to_string()
        );
        window.open_dialog(cx, move |dialog, _, cx| {
            let (muted, border) = (cx.theme().muted_foreground, cx.theme().border);
            let save = {
                let (this, remember) = (this.clone(), remember.clone());
                move |cx: &mut App| {
                    if remember.get() {
                        this.update(cx, |this, cx| {
                            this.app.update(cx, |app, cx| {
                                app.update_settings(cx, |s| s.close_to_tray = true)
                            })
                        })
                        .ok();
                    }
                }
            };
            let (quit_save, tray_save) = (save.clone(), save);
            let (quit_this, tray_this) = (this.clone(), this.clone());
            let list = v_flex()
                .gap(px(4.))
                .p(px(8.))
                .rounded(px(ui::RADIUS))
                .border_1()
                .border_color(border)
                .children(lines.iter().map(|(name, place, color)| {
                    h_flex()
                        .gap(px(8.))
                        .text_sm()
                        .child(div().flex_none().size(px(7.)).rounded_full().bg(*color))
                        .child(
                            div()
                                .flex_shrink(1.)
                                .min_w_0()
                                .truncate()
                                .child(name.clone()),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .flex_shrink(10.)
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(muted)
                                .child(place.clone()),
                        )
                }))
                .when(more > 0, |list| {
                    list.child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(tf!("另有 {n} 个", n = more.to_string())),
                    )
                });
            // 「以后也这样」记成关闭到托盘：要有托盘才管用（Wayland 上是关闭时最小化）
            let remember_box = tray.then(|| {
                let remember = remember.clone();
                Checkbox::new("close-remember")
                    .label(match hide {
                        true => tr("以后关闭窗口时最小化到托盘"),
                        false => tr("以后关闭窗口时最小化"),
                    })
                    .checked(remember.get())
                    .on_click(move |checked: &bool, window, _| {
                        remember.set(*checked);
                        window.refresh();
                    })
            });
            dialog
                .title(tr("退出 Kuai？"))
                .w(px(460.))
                .child(
                    v_flex()
                        .gap_3()
                        .child(div().text_sm().child(description.clone()))
                        .child(list)
                        .children(remember_box),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("quit")
                                .label(tr("仍要退出"))
                                .with_variant(ButtonVariant::Danger)
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    quit_save(cx);
                                    quit_this
                                        .update(cx, |this, cx| {
                                            if !this.confirm_quit(window, cx) {
                                                this.quitting.set(true);
                                                cx.quit();
                                            }
                                        })
                                        .ok();
                                }),
                        )
                        .child(
                            Button::new("cancel")
                                .label(tr("取消"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("tray")
                                .primary()
                                .label(match hide {
                                    true => tr("最小化到托盘"),
                                    false => tr("最小化窗口"),
                                })
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    tray_save(cx);
                                    tray_this
                                        .update(cx, |this, cx| {
                                            // 藏不起来的 hide_window 就是最小化
                                            if tray {
                                                this.ensure_tray(cx);
                                            }
                                            this.hide_window(window, cx)
                                        })
                                        .ok();
                                }),
                        ),
                )
        });
        true
    }

    /// 设置里没开托盘时也要收进托盘（关窗口的确认框里点了「最小化到托盘」）：先建托盘图标，之后照常
    /// 由设置管（改了托盘、语言时重建）。
    fn ensure_tray(&mut self, cx: &mut Context<Self>) {
        if self.tray.is_none() {
            self.tray = Tray::new(cx, |this, action, cx| this.on_tray(action, cx));
        }
    }

    /// 托盘图标在，窗口可以收进去（Linux 上托盘可能还没起来，或者中途下线了）。
    fn in_tray(&self) -> bool {
        self.tray.is_some() && crate::tray::available()
    }

    fn on_tray(&mut self, action: TrayAction, cx: &mut Context<Self>) {
        match action {
            TrayAction::Show { .. } if cfg!(windows) => self.show_window(cx),
            TrayAction::Show { token } => {
                let this = cx.entity().downgrade();
                crate::with_main_window(cx, move |window, cx| {
                    this.update(cx, |this, cx| this.bring_to_front(token, window, cx))
                        .ok();
                });
            }
            TrayAction::Quit => self.quit_from_tray(cx),
            // 托盘下线了（桌面重启了外壳、关了托盘扩展）：收在里面的窗口拿出来，不然任务栏上也
            // 没有它，找不回来
            TrayAction::Lost => {
                if self.tucked.get() {
                    self.show_window(cx);
                }
            }
        }
    }

    /// 托盘菜单的「退出」：本机终端里没有程序在运行就直接退出；有就把窗口叫出来，问一下。
    fn quit_from_tray(&mut self, cx: &mut Context<Self>) {
        if self.losses(cx).none() {
            self.quitting.set(true);
            cx.quit();
            return;
        }
        let this = cx.entity().downgrade();
        crate::with_main_window(cx, move |window, cx| {
            this.update(cx, |this, cx| {
                this.bring_to_front(None, window, cx);
                this.confirm_quit(window, cx)
            })
            .ok();
        });
    }

    /// 退出前问一下：本机终端里还有程序在运行，退出将结束它们（服务器上的终端不受影响）。
    /// 有托盘时多一个「最小化到托盘」，窗口藏起来，终端接着运行。返回问了没有：一个都没有时
    /// 不问，由调用的地方直接退出。
    fn confirm_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let losses = self.losses(cx);
        if losses.none() {
            return false;
        }
        let description = losses.describe(false);
        let ok = match (losses.terminals, losses.unsaved) {
            (0, 0) => tr("退出"),
            (0, _) => tr("放弃修改并退出"),
            _ => tr("结束并退出"),
        };
        let quitting = self.quitting.clone();
        let to_tray = self.in_tray().then(|| cx.entity().downgrade());
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
                                    this.update(cx, |this, cx| this.hide_window(window, cx))
                                        .ok();
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
    /// 点任务栏上的按钮最小化的不藏：留在任务栏上，再点一下就回来（Linux 上分不出来，都藏）。
    /// Wayland 上藏不起来，什么都不做（设置里也没有这一项）。
    fn hide_if_minimized(&mut self, cx: &mut Context<Self>) {
        let on = self.app.read(cx).settings.minimize_to_tray && self.in_tray();
        if let (true, Some(hwnd)) = (on, self.hwnd)
            && win::is_minimized(hwnd)
            && !win::cursor_on_taskbar()
        {
            self.tucked.set(true);
            win::hide(hwnd);
            self.tray_tip_once(cx);
        }
    }

    /// 开机自启带 `--minimized` 启动：有托盘就直接收进托盘（Wayland 上最小化）。Linux 上还不知道
    /// 有没有托盘（桌面刚起来，`tray::probe` 还在问）就在后台等它问完再定，不卡界面；一直问不到就
    /// 不收。
    pub fn start_in_tray(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !crate::tray::unknown() {
            self.tuck_away(window);
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let started = std::time::Instant::now();
            while crate::tray::unknown() && started.elapsed() < TRAY_WAIT {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
            }
            this.update_in(cx, |this, window, cx| {
                this.tuck_away(window);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 有托盘就把窗口收进去（Wayland 上最小化）。
    fn tuck_away(&mut self, window: &mut Window) {
        if !self.in_tray() {
            return;
        }
        match self.hwnd {
            Some(hwnd) => {
                self.tucked.set(true);
                win::hide(hwnd);
            }
            None => window.minimize_window(),
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
            self.bring_to_front(request.activation, window, cx);
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
        home: WorkspaceKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut first = None;
        for entry in entries {
            let ix = self.ensure_tab(entry, window, cx);
            // 和单个打开的一样：仓库页的工作区按钮先写是从哪个工作区开的
            let view = self.tabs[ix].view.clone();
            view.update(cx, |view, cx| view.set_home_hint(Some(home.clone()), cx));
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

    /// 工作区页、主机页上选了一个仓库的「新建终端」：到会话页，在这个仓库里新开一个，输入 `start`。
    fn new_terminal_for(
        &mut self,
        entry: RepoEntry,
        start: Option<crate::presets::Start>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_terms(window, cx);
        self.overview.update(cx, |overview, cx| {
            overview.new_terminal_with(entry, start, window, cx)
        });
    }

    /// 这个仓库的标签页在第几个；没开着就建一个加在最后。
    fn ensure_tab(
        &mut self,
        entry: RepoEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Windows 上同一个仓库的几种写法算一个（见 [`RepoEntry::same_repo`]），不然从会话页打开没登记
        // 过的子仓库会再开一个标签页
        if let Some(ix) = self.tabs.iter().position(|t| t.entry.same_repo(&entry)) {
            return ix;
        }
        let tab = self.build_tab(entry, window, cx);
        self.tabs.push(tab);
        self.tabs.len() - 1
    }

    /// 换到一页（标题栏上的三个页面、某个标签页）。设置页开着就关掉。换了就记一步，后退能回来。
    fn show_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.here(cx);
        self.show_page_from(from, page, window, cx);
    }

    /// 点了标题栏的「工作区」：从仓库标签页过来的，左边的主机跟着这个仓库所在的主机。
    /// 后退、前进回到工作区页的不跟（回到的是当时看的那台）。
    fn show_workspaces_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.active_repo().map(|tab| tab.entry.clone()) {
            let machine = machine_of(&entry).to_string();
            self.workspaces
                .update(cx, |page, cx| page.follow(machine, cx));
        }
        self.show_page(Page::Workspaces, window, cx);
    }

    /// 同 [`Self::show_page`]。`from` 是换页之前在哪：换页前已经改了要去的那一页的样子（选中了别的
    /// 工作区）时，调用的地方要在改之前记下。
    fn show_page_from(
        &mut self,
        from: Nav,
        page: Page,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 离开会话页时焦点在不在终端里：退回来时照样放回去
        if self.page == Page::Terms && page != Page::Terms && !self.showing_settings {
            self.terms_typing = self.overview.read(cx).typing(window, cx);
        }
        // 设置页盖着时，下面那一页不算显示着
        let was = (!self.showing_settings).then_some(self.page);
        self.showing_settings = false;
        self.page = page;
        self.page_changed(was, window, cx);
        self.persist(cx);
        if !self.traveling && self.here(cx) != from {
            self.history.remember(from);
        }
        cx.notify();
    }

    // ---- 后退、前进 ----

    /// 现在在哪一页（设置页盖着时是下面那一页）。
    fn here(&self, cx: &App) -> Nav {
        match self.page {
            Page::Servers => Nav::Servers,
            Page::Terms => Nav::Terms,
            Page::Workspaces => {
                let (machine, key) = self.workspaces.read(cx).selection(cx);
                Nav::Workspaces { machine, key }
            }
            Page::Repo(ix) => match self.tabs.get(ix) {
                Some(tab) => Nav::Repo(tab.entry.clone()),
                None => Nav::Workspaces {
                    machine: LOCAL.to_string(),
                    key: None,
                },
            },
        }
    }

    /// 按钮的提示里写的名字。
    fn nav_label(&self, nav: &Nav, cx: &App) -> SharedString {
        let app = self.app.read(cx);
        match nav {
            Nav::Servers => trc("页面", "主机").into(),
            Nav::Terms => trc("页面", "会话").into(),
            Nav::Workspaces { key: Some(key), .. } => {
                tf!("工作区「{name}」", name = app.workspace_name(key)).into()
            }
            Nav::Workspaces { key: None, .. } => trc("页面", "工作区").into(),
            Nav::Repo(entry) => app.repo_name(entry),
        }
    }

    /// 后退、前进会到哪（按钮的提示用）；没有是 None。后退时设置页开着是关掉它，在子仓库、工作树
    /// 里是回到上一层。
    fn travel_target(&self, back: bool, cx: &App) -> Option<SharedString> {
        let here = self.here(cx);
        if back {
            if self.showing_settings {
                return Some(self.nav_label(&here, cx));
            }
            // 回到的那一层：进了两层时是外面那个子仓库，不是标签页的仓库
            if let Some(tab) = self.active_repo()
                && let Some(target) = tab.view.read(cx).back_target(cx)
            {
                return Some(self.app.read(cx).repo_name(&target));
            }
        }
        let app = self.app.read(cx);
        self.history
            .peek(back, &here, |nav| reachable(&self.tabs, app, nav))
            .map(|nav| self.nav_label(nav, cx))
    }

    /// 后退（标题栏的 ←、Alt+←、鼠标侧键）：设置页开着就关掉它；在子仓库、工作树里先回到上一层；
    /// 不然回到上一页。
    pub fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.showing_settings {
            self.toggle_settings(window, cx);
            return;
        }
        if let Some(tab) = self.active_repo() {
            let view = tab.view.clone();
            if view.update(cx, |view, cx| view.back_to_root(window, cx)) {
                cx.notify();
                return;
            }
        }
        self.travel(true, window, cx);
    }

    /// 前进（标题栏的 →、Alt+→、鼠标侧键）。
    pub fn go_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.travel(false, window, cx);
    }

    fn travel(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let here = self.here(cx);
        let (tabs, app) = (&self.tabs, self.app.read(cx));
        let Some(nav) = self
            .history
            .step(back, here, |nav| reachable(tabs, app, nav))
        else {
            return;
        };
        self.traveling = true;
        self.visit(nav, window, cx);
        self.traveling = false;
    }

    /// 回到记下的那一页。
    fn visit(&mut self, nav: Nav, window: &mut Window, cx: &mut Context<Self>) {
        match nav {
            Nav::Servers => self.show_page(Page::Servers, window, cx),
            Nav::Terms => {
                self.show_terms(window, cx);
                if self.terms_typing {
                    self.overview
                        .update(cx, |overview, cx| overview.type_in(window, cx));
                }
            }
            Nav::Workspaces { machine, key } => {
                let known = |key: &WorkspaceKey| self.app.read(cx).workspace(key).is_some();
                match key.filter(known) {
                    Some(key) => self.app.update(cx, |app, cx| app.set_current(key, cx)),
                    // 那台上没有选中的工作区（还没有）：照旧列那台
                    None => self
                        .app
                        .update(cx, |app, _| app.menu_machine = Some(machine)),
                }
                self.show_page(Page::Workspaces, window, cx);
            }
            Nav::Repo(entry) => {
                let at = self.tabs.iter().position(|tab| tab.entry.same_repo(&entry));
                if let Some(ix) = at {
                    self.show_page(Page::Repo(ix), window, cx);
                }
            }
        }
    }

    /// 到工作区页，选中工作区 `key`，`entry` 那一行标出来（「打开工作区」，见 [`go_to_workspace`]）。
    fn open_workspace(
        &mut self,
        key: WorkspaceKey,
        entry: Option<RepoEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let from = self.here(cx);
        self.workspaces
            .update(cx, |page, cx| page.reveal(key, entry, cx));
        self.show_page_from(from, Page::Workspaces, window, cx);
    }

    /// 刚打开的仓库标签页是从哪个工作区来的（工作区页上打开的、会话页里列在哪个工作区下面）：它的
    /// 工作区按钮先写这个。
    fn hint_home(&mut self, home: Option<WorkspaceKey>, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.view.update(cx, |view, cx| view.set_home_hint(home, cx));
        }
    }

    /// 显示着的那一页换了（`was` 是原来显示着的，设置页盖着时是 None）：会话页、工作区页要知道
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
        self.app.update(cx, |app, cx| app.toggle_pinned(&entry, cx));
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

    /// 服务器页上点了一台服务器的「N 个工作区」：到工作区页看那台的。
    fn show_workspaces_of(&mut self, machine: String, window: &mut Window, cx: &mut Context<Self>) {
        self.app
            .update(cx, |app, _| app.menu_machine = Some(machine));
        self.show_page(Page::Workspaces, window, cx);
    }

    /// 工作区页上点了一个仓库的终端个数：到会话页，选中它。
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

    /// 打开设置的终端那一页（新建终端菜单里的「管理预设…」）。
    pub fn open_terminal_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_page
            .update(cx, |page, cx| page.show_terminal(cx));
        if !self.showing_settings {
            self.toggle_settings(window, cx);
        }
    }

    /// Ctrl+Shift+A：打开会话页；已经在上面就回到原来那一页。
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
            // 焦点还在看不见的会话页上：放回显示出来的终端里
            if let Some(tab) = self.active_repo() {
                tab.shown(cx)
                    .update(cx, |view, cx| view.focus_shown_terminal(window, cx));
            }
        } else {
            self.show_terms(window, cx);
        }
    }

    /// 到会话页（标题栏上的「会话」）。
    fn show_terms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page != Page::Terms {
            self.before_terms = self.page;
            self.before_terms_repo = self.front_repo();
        }
        self.show_page(Page::Terms, window, cx);
    }

    /// 提醒里的「前往」、点了系统通知：到会话页选中这个终端。
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

    /// 从会话页打开一个仓库（工具栏的「打开仓库」、右键菜单、Enter、双击）：打开它的标签页，显示
    /// Git 页。
    fn open_repo_git(&mut self, entry: &RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        let home = self.overview.read(cx).listed_under(entry, cx);
        self.open(entry.clone(), window, cx);
        self.hint_home(home, cx);
        // 这个标签页里之前进了子仓库、别的工作树：先退回这个仓库本身，不然显示的是里面那一层的
        // Git 页，和会话页里那个终端所在的仓库对不上（同 `open_repo_path`）
        if let Some(view) = self.active_repo().map(|tab| tab.view.clone()) {
            view.update(cx, |view, cx| {
                for _ in 0..4 {
                    if !view.back_to_root(window, cx) {
                        break;
                    }
                }
                view.show_git_page(window, cx);
            });
        }
    }

    /// 会话页里按住 Ctrl 单击了仓库里的文件、目录：打开这个仓库的标签页（进了子仓库的退回这个仓库），
    /// 在文件页打开它（见 [`RepoView::show_path`]）。
    fn open_repo_path(
        &mut self,
        entry: &RepoEntry,
        path: String,
        at: Option<(u32, u32)>,
        dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open(entry.clone(), window, cx);
        let Some(view) = self.active_repo().map(|tab| tab.view.clone()) else {
            return;
        };
        view.update(cx, |view, cx| {
            // 一次退一层（子仓库的工作树里又进了一层的也退得回来）
            for _ in 0..4 {
                if !view.back_to_root(window, cx) {
                    break;
                }
            }
            view.show_path(path, at, dir, window, cx);
        });
    }

    /// 点了系统通知：把窗口叫到前面，前往那个终端（它已经不在了就只叫窗口）。
    fn toast_clicked(
        &mut self,
        click: &crate::toast::Click,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tag = click.tag.as_str();
        log::info!("点了系统通知：{tag}（激活令牌：{}）", click.token.is_some());
        self.bring_to_front(click.token.clone(), window, cx);
        if tag == AWAY_TOAST {
            self.show_waiting(window, cx);
            return;
        }
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

    /// 一个不在眼前的 AI 终端静默久了（待查看）：右下角说一声，带「前往」；Kuai 不在前台时任务栏上的
    /// 按钮闪，再发一条系统通知（设置里可以关）。
    fn terminal_waiting(&mut self, row: &TermRow, window: &mut Window, cx: &mut Context<Self>) {
        if !self.app.read(cx).settings.notify_terminal_stopped {
            return;
        }
        let app = self.app.read(cx);
        let repo = app.repo_name(&row.entry);
        let wait = match row.kind {
            crate::term_board::Kind::Agent(agent) => agent.wait(&app.settings.term_alerts),
            _ => crate::term_board::QUIET,
        };
        let name = row.label.clone();
        let text = tf!(
            "{name} 待查看（{repo}）",
            name = name.clone(),
            repo = repo.clone()
        );
        let line = tf!(
            "{repo} · 静默超过 {span}，可能在等你确认",
            repo,
            span = crate::term_board::span(wait)
        );
        let body = match row.last.trim() {
            "" => line,
            last => format!("{line}\n{last}"),
        };
        // 系统通知：标题是谁待查看，正文是在哪、静默多久，和屏幕上最后一行（多半是程序在问什么）
        let (toast_key, toast_title, toast_body) =
            (stopped_note_key(&row.id), tf!("{name} 待查看", name), body);
        let path = row.entry.path.clone();
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
            "提醒终端待查看：{}，窗口{}",
            path,
            if in_front(window) {
                "在前台"
            } else {
                "不在前台"
            }
        );
        window.push_notification(note, cx);
        if in_front(window) {
            return;
        }
        // 闪任务栏上的按钮也会让系统当场给窗口发消息，同样放到这一轮更新之外（道理见 `toasts`）
        if let Some(hwnd) = self.hwnd {
            cx.spawn(async move |_, _| win::flash(hwnd)).detach();
        }
        let system = self.app.read(cx).settings.notify_terminal_system;
        // Linux：不发系统通知时请窗口管理器提醒（Dock、任务栏上的图标标出来）。发的话不用：GNOME 对
        // 「请求注意」的窗口另发一条「Kuai 已就绪」，和我们的通知重了
        if !cfg!(windows) && !system {
            window.request_attention();
        }
        if system {
            self.toast_job(ToastJob::Show {
                key: toast_key.to_string(),
                title: toast_title.to_string(),
                body: toast_body.to_string(),
            });
        }
    }

    /// 重开 Kuai、重新连上主机以后，离开期间有新输出的终端：合成一条提醒，带「查看」（到会话页只看
    /// 待查看的）；Kuai 不在前台时再发一条系统通知。
    fn away_updates(&mut self, n: usize, window: &mut Window, cx: &mut Context<Self>) {
        let settings = &self.app.read(cx).settings;
        if !settings.notify_terminal_stopped {
            return;
        }
        let system = settings.notify_terminal_system;
        let text = tf!("离开期间 {n} 个终端有新输出", n = n.to_string());
        let this = cx.entity().downgrade();
        let note = Notification::info(text.clone())
            .id::<AwayNote>()
            .action(move |_, _, _| {
                let this = this.clone();
                Button::new("away-go")
                    .small()
                    .primary()
                    .label(tr("查看"))
                    .on_click(move |_, window, cx| {
                        this.update(cx, |this, cx| this.show_waiting(window, cx))
                            .ok();
                    })
            })
            .autohide(true);
        window.push_notification(note, cx);
        if !in_front(window) && system {
            self.toast_job(ToastJob::Show {
                key: AWAY_TOAST.to_string(),
                title: text,
                body: tr("已在会话页标出，不逐个发送通知。").to_string(),
            });
        }
    }

    /// 到会话页、只看待查看的（离开期间有更新的那条提醒里点「查看」）。
    fn show_waiting(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_terms(window, cx);
        self.overview
            .update(cx, |overview, cx| overview.show_waiting(cx));
    }

    /// 交给发系统通知的那个任务去做（见 `toasts`）。
    fn toast_job(&self, job: ToastJob) {
        self.toast_jobs.unbounded_send(job).ok();
    }

    /// 正显示着哪个终端变了：告诉 `board`（显示着的算看过，停下了不提醒）。
    ///
    /// 平时在 `render` 里做；窗口失去焦点、藏起来、最小化时由窗口的回调当场做一次：最小化和藏进
    /// 托盘以后窗口不再重画（GPUI 在最小化时收起重画的回调，藏起来的窗口收不到 `WM_PAINT`），
    /// 只靠 `render` 的话会一直当作有人在看，停下了也不提醒。
    fn sync_shown(&mut self, window: &Window, cx: &mut Context<Self>) {
        let shown = self.shown_terminal(window, cx);
        if shown != self.told_shown {
            self.told_shown = shown.clone();
            let board = self.board.clone();
            cx.defer(move |cx| board.update(cx, |board, cx| board.set_shown(shown, cx)));
        }
    }

    /// 现在正显示着哪个终端：窗口在前台、看得见时，会话页里选中的（正往里输入的另算），或者当前
    /// 标签页的终端页上那个。
    fn shown_terminal(&self, window: &Window, cx: &App) -> Option<Shown> {
        if !in_front(window) || self.showing_settings {
            return None;
        }
        if self.page == Page::Terms {
            return self.overview.read(cx).shown_for_board(window, cx);
        }
        let view = self.active_repo()?.shown(cx);
        let view = view.read(cx);
        // 仓库终端页并排着：那几格都算显示着
        let split = view.split_terminals();
        if split.len() > 1 {
            let board = self.board.read(cx);
            let ids = split
                .iter()
                .filter_map(|(entry, number)| {
                    let rows = board.terms_of(entry)?;
                    let row = rows.into_iter().find(|row| row.number == *number)?;
                    Some(row.id)
                })
                .collect();
            return Some(Shown::Split(ids));
        }
        let (entry, number) = view.shown_terminal()?;
        Some(Shown::Repo(entry, number))
    }

    fn active_repo(&self) -> Option<&RepoTab> {
        match self.page {
            Page::Repo(ix) if !self.showing_settings => self.tabs.get(ix),
            _ => None,
        }
    }

    /// 应用状态（主机、设置）。
    pub fn app_entity(&self) -> Entity<AppState> {
        self.app.clone()
    }

    /// 获取、拉取、推送这些操作的快捷键：交给当前仓库（在子仓库里就是子仓库）。
    pub fn git_in_active(
        &mut self,
        op: crate::repo::GitShortcut,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.active_repo() {
            tab.shown(cx)
                .update(cx, |view, cx| view.git_shortcut(op, window, cx));
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

    /// Ctrl+Shift+V：当前仓库文件页上的 Markdown 在预览和源文件之间切换。
    pub fn preview_in_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_repo() {
            tab.shown(cx)
                .update(cx, |view, cx| view.toggle_markdown_preview(window, cx));
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

    /// Ctrl+Shift+`：当前仓库再开一个终端，切到它的终端页显示它（在会话页上时开在那里）。
    pub fn new_terminal_in_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let start = crate::presets::default_start(cx);
        self.start_terminal_in_active(start, window, cx);
    }

    /// 给预设设的快捷键（设置 → 快捷键）：新开一个终端运行它。`preset` 是预设的 id，空的是只开
    /// shell；预设已经删了就不开。
    pub fn run_preset_in_active(
        &mut self,
        preset: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let start = match preset.is_empty() {
            true => None,
            false => match crate::presets::all(cx).into_iter().find(|p| p.id == preset) {
                Some(found) => Some(crate::presets::Start::of(&found)),
                None => return,
            },
        };
        self.start_terminal_in_active(start, window, cx);
    }

    /// 新开一个终端，输入 `start`（None 是只开 shell）：会话页上开在那里选中的仓库里，别处开在
    /// 当前仓库里。
    fn start_terminal_in_active(
        &mut self,
        start: Option<crate::presets::Start>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.page == Page::Terms && !self.showing_settings {
            self.overview.update(cx, |overview, cx| {
                overview.new_terminal_here(start, window, cx)
            });
            return;
        }
        if let Some(tab) = self.active_repo() {
            tab.shown(cx)
                .update(cx, |view, cx| view.new_terminal_running(start, window, cx));
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
        let picker = cx.new(|cx| QuickOpen::new(app, PickFor::Open, window, cx));
        cx.subscribe_in(&picker, window, |this, _, event: &Picked, window, cx| {
            window.close_dialog(cx);
            this.open(event.0.clone(), window, cx);
        })
        .detach();
        show_picker(picker, tr("快速打开仓库").into(), window, cx);
    }

    /// Ctrl+Shift+N：选一个仓库，在里面新建终端，到会话页显示它。
    pub fn pick_new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_terms(window, cx);
        self.overview
            .update(cx, |overview, cx| overview.pick_new_terminal(window, cx));
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
            .context_menu(move |menu, window, cx| {
                tab_menu(
                    menu,
                    ix,
                    entry.clone(),
                    can_reopen,
                    &menu_app,
                    &menu_this,
                    window,
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
    /// 第一次用（`docs/design/first-run/` 方向 B）：没有远程主机、本机也没有仓库、没开
    /// 标签页，也没点过「暂时跳过」时，整个窗口是欢迎页。本机的仓库列表还没读到时不算（不然老用户
    /// 启动的那一下会闪出欢迎页）。添加了主机、打开了仓库，它就不再出现。
    fn welcome(&self, cx: &App) -> bool {
        let app = self.app.read(cx);
        !self.showing_settings
            && !app.settings.welcome_skipped
            && app.settings.servers.is_empty()
            && self.tabs.is_empty()
            && app
                .repos_on(crate::machines::LOCAL)
                .is_some_and(|repos| repos.is_empty())
    }

    /// 欢迎页：两张卡片，底下「暂时跳过」。「连接远程主机」整张可点；「在本机开始」里列三件事
    /// （打开仓库、新建项目、克隆），各点各的（`docs/design/sandbox-1126/` 第 3 题方向 A）。
    fn render_welcome(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (primary, border, muted, fg, bg, secondary) = (
            theme.primary,
            theme.border,
            theme.muted_foreground,
            theme.foreground,
            theme.background,
            theme.secondary,
        );
        // 卡片的壳：宽度、边框；推荐的那张带主色的边和「推荐」
        let shell = |main: bool| {
            div()
                .relative()
                .flex()
                .flex_col()
                .w(px(290.))
                .p(px(20.))
                .rounded(px(14.))
                .border_1()
                .border_color(if main { primary.opacity(0.55) } else { border })
                .bg(if main { primary.opacity(0.06) } else { bg })
                .when(main, |card| {
                    card.child(
                        div()
                            .absolute()
                            .top(px(14.))
                            .right(px(14.))
                            .px(px(8.))
                            .rounded(px(9.))
                            .bg(primary.opacity(0.14))
                            .text_size(px(11.))
                            .text_color(primary)
                            .child(tr("推荐")),
                    )
                })
        };
        // 卡片上面的三样：图标、标题、一句说明
        let head = |icon: IconName, title: &'static str, desc: &'static str, main: bool| {
            [
                div()
                    .size(px(42.))
                    .mb(px(14.))
                    .rounded(px(11.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .bg(if main { primary } else { secondary })
                    .text_color(if main { gpui_kit::white() } else { fg })
                    .child(Icon::new(icon).size(px(22.)))
                    .into_any_element(),
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .mb(px(4.))
                    .child(title)
                    .into_any_element(),
                div()
                    .text_size(px(12.5))
                    .text_color(muted)
                    .whitespace_normal()
                    .child(desc)
                    .into_any_element(),
            ]
        };
        // 「在本机开始」里的一行：点了做一件事
        let local_row = |id: &'static str, icon: IconName, label: &'static str, first: bool| {
            h_flex()
                .id(id)
                .h(px(32.))
                .px(px(8.))
                .gap(px(9.))
                .rounded(px(7.))
                .text_size(px(13.))
                .cursor_pointer()
                .hover(|s| s.bg(secondary))
                .when(first, |row| {
                    row.text_color(primary).font_weight(FontWeight::SEMIBOLD)
                })
                .child(Icon::new(icon).size(px(15.)))
                .child(div().flex_1().child(label))
                .child(
                    Icon::new(IconName::ChevronRight)
                        .size(px(14.))
                        .text_color(muted),
                )
        };
        v_flex()
            .id("welcome")
            .size_full()
            .items_center()
            .pt(px(56.))
            .px(px(16.))
            .overflow_y_scroll()
            // Kuai 的标志（和程序图标同一张图）；原来是「主机」图标，和下面第一张卡片的重复
            .child(img(kuai_logo()).size(px(68.)).mb(px(14.)))
            .child(
                div()
                    .text_size(px(24.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .mb(px(6.))
                    .child(tr("欢迎使用 Kuai")),
            )
            .child(
                div()
                    .text_size(px(13.5))
                    .text_color(muted)
                    .mb(px(26.))
                    .text_center()
                    .child(tr(
                        "在远程主机上用 Git、终端和文件，运行 Claude Code 等 AI 助手。",
                    )),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .justify_center()
                    // 两张一样高：短的那张把最后一行压到底
                    .items_stretch()
                    .gap(px(16.))
                    .child(
                        shell(true)
                            .id("welcome-host")
                            .cursor_pointer()
                            .hover(|s| s.border_color(primary))
                            .children(head(
                                IconName::Server,
                                tr("连接远程主机"),
                                tr("通过 SSH 连接 VPS 或开发机，仓库和终端都在主机上。"),
                                true,
                            ))
                            .child(div().flex_1().min_h(px(14.)))
                            .child(
                                h_flex()
                                    .gap(px(4.))
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(primary)
                                    .child(tr("添加主机…"))
                                    .child(Icon::new(IconName::ChevronRight).size(px(14.))),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_page(Page::Servers, window, cx);
                                this.manager
                                    .update(cx, |manager, cx| manager.add_host(window, cx));
                            })),
                    )
                    .child(
                        shell(false)
                            .children(head(
                                IconName::Monitor,
                                tr("在本机开始"),
                                tr("不连接主机，使用本机上的 Git 仓库。"),
                                false,
                            ))
                            .child(
                                v_flex()
                                    .mt(px(10.))
                                    .mx(px(-8.))
                                    .mb(px(-8.))
                                    .gap(px(1.))
                                    .child(
                                        local_row(
                                            "welcome-open",
                                            IconName::FolderGit2,
                                            tr("打开仓库…"),
                                            true,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.show_page(Page::Servers, window, cx);
                                                this.manager.update(cx, |manager, cx| {
                                                    manager.open_local(window, cx)
                                                });
                                            }),
                                        ),
                                    )
                                    .child(
                                        local_row(
                                            "welcome-new",
                                            IconName::FolderPlus,
                                            tr("新建项目…"),
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.show_page(Page::Servers, window, cx);
                                                this.manager.update(cx, |manager, cx| {
                                                    manager.new_local_project(window, cx)
                                                });
                                            }),
                                        ),
                                    )
                                    .child(
                                        local_row(
                                            "welcome-clone",
                                            IconName::Download,
                                            tr("克隆…"),
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.show_page(Page::Servers, window, cx);
                                                this.manager.update(cx, |manager, cx| {
                                                    manager.clone_local(window, cx)
                                                });
                                            }),
                                        ),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .id("welcome-skip")
                    .mt(px(22.))
                    .text_size(px(12.5))
                    .text_color(muted)
                    .cursor_pointer()
                    .hover(|s| s.text_color(fg).underline())
                    .child(tr("暂时跳过"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app.update(cx, |app, cx| {
                            app.update_settings(cx, |settings| settings.welcome_skipped = true)
                        });
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

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
                Page::Workspaces => this.show_workspaces_page(window, cx),
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
        let welcome = self.welcome(cx);
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
        // 所有电脑上的终端：待查看、离开期间有更新的有几个就标几个；断开了的主机另标「N 台未连接」
        let waiting = self.board.read(cx).attention();
        let offline: Vec<SharedString> = {
            let (board, app) = (self.board.read(cx), self.app.read(cx));
            board
                .offline_machines()
                .iter()
                .map(|key| app.machine_name(key))
                .collect()
        };
        let offline_tip: Option<String> = (!offline.is_empty()).then(|| {
            tf!(
                "{names} 未连接，其终端状态未知",
                names = offline
                    .iter()
                    .map(|name| name.to_string())
                    .collect::<Vec<_>>()
                    .join(tr("、"))
            )
        });
        let terms = self
            .render_page_entry(
                Page::Terms,
                "page-terms",
                IconName::SquareTerminal,
                trc("页面", "会话"),
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
            .when(!offline.is_empty(), |tab| {
                let label = match narrow {
                    true => offline.len().to_string(),
                    false => tf!("{n} 台未连接", n = offline.len().to_string()),
                };
                tab.child(
                    h_flex()
                        .flex_none()
                        .h(px(16.))
                        .px(px(5.))
                        .gap(px(3.))
                        .rounded_full()
                        .border_1()
                        .border_color(border)
                        .text_size(px(10.5))
                        .text_color(muted)
                        .child(Icon::new(IconName::Unplug).size(px(10.)))
                        .child(label),
                )
            })
            .tooltip(move |window, cx| {
                let text = match waiting {
                    0 => tr("会话（Ctrl+Shift+A）").to_string(),
                    n => tf!("会话（Ctrl+Shift+A）：{n} 个终端待查看", n = n.to_string()),
                };
                let text = crate::keymap::with_key(&text, "page.hub", cx);
                match &offline_tip {
                    Some(offline) => ui::two_line_tip(text, offline.clone().into(), window, cx),
                    None => ui::tip(text, window, cx),
                }
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
        // 最左边的后退、前进：提示里写会回到哪
        let travel = |back: bool, cx: &mut Context<Self>| {
            let target = self.travel_target(back, cx);
            let tip: SharedString = match (&target, back) {
                (Some(to), true) => {
                    let tip = tf!("返回：{to}（Alt+←）", to = to.clone());
                    crate::keymap::with_key(&tip, "back", cx)
                }
                (Some(to), false) => {
                    let tip = tf!("前进：{to}（Alt+→）", to = to.clone());
                    crate::keymap::with_key(&tip, "forward", cx)
                }
                (None, true) => tr("没有可返回的页面").into(),
                (None, false) => tr("没有可前进的页面").into(),
            };
            let (id, icon) = match back {
                true => ("nav-back", IconName::ArrowLeft),
                false => ("nav-forward", IconName::ArrowRight),
            };
            div().occlude().flex_none().child(
                Button::new(id)
                    .ghost()
                    .small()
                    .icon(ui::icon(icon))
                    .disabled(target.is_none())
                    .tooltip(tip)
                    .on_click(cx.listener(move |this, _, window, cx| match back {
                        true => this.go_back(window, cx),
                        false => this.go_forward(window, cx),
                    })),
            )
        };
        let back = travel(true, cx);
        let forward = travel(false, cx);
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
                        .tooltip(crate::keymap::with_key(
                            tr("打开仓库（Ctrl+P）"),
                            "quick",
                            cx,
                        ))
                        .on_click(cx.listener(|this, _, window, cx| this.quick_open(window, cx))),
                ),
            )
            // 这一段是空的：按住可以拖窗口
            .child(div().flex_1().min_w(px(24.)).h_full());
        // Linux 上标题栏是自己画的，关闭按钮默认直接关窗口，不经 `on_window_should_close`：
        // 关闭到托盘、退出前问一下都要走一遍
        let this = cx.entity().downgrade();
        let close = move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let close = this
                .update(cx, |this, cx| this.close_requested(window, cx))
                .unwrap_or(true);
            if close {
                window.remove_window();
            }
        };
        TitleBar::new()
            .h(px(TITLE_HEIGHT))
            .on_close_window(close)
            .child(
                h_flex()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .gap(px(4.))
                    // 欢迎页：只有「开始使用」
                    .when(welcome, |bar| {
                        bar.child(
                            tab_shell("page-welcome", true, cx)
                                .flex_none()
                                .ml(px(8.))
                                .child(ui::icon(IconName::Sparkles).flex_none())
                                .child(tr("开始使用")),
                        )
                    })
                    // 最前面是后退、前进，然后是三个页面，后面是标签页
                    .when(!welcome, |bar| {
                        bar.child(back)
                            .child(forward)
                            .child(div().flex_none().w(px(1.)).h(px(16.)).mx(px(2.)).bg(border))
                            .child(servers)
                            .child(terms)
                            .child(workspaces)
                            .child(div().flex_none().w(px(1.)).h(px(16.)).mx(px(4.)).bg(border))
                    })
                    // 标签页那一块画在这个占位的块里面（绝对定位）。标题栏的宽度按里面内容的
                    // 最小宽度算，直接把标签页排在这一行里的话，开多了会把整行撑宽，
                    // 右边的设置和窗口按钮被挤出窗口
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            // 欢迎页没有标签页，也不给「打开仓库」：这一块只是拖窗口的空白
                            .when(!welcome, |region| region.child(measure).child(strip)),
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
                                .tooltip(crate::keymap::with_key(
                                    tr("设置（Ctrl+,）"),
                                    "settings",
                                    cx,
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_settings(window, cx)
                                })),
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

/// 仓库标签页的右键菜单。
#[allow(clippy::too_many_arguments)]
fn tab_menu(
    menu: PopupMenu,
    ix: usize,
    entry: RepoEntry,
    can_reopen: bool,
    app: &Entity<AppState>,
    this: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let (close, others, right, all) = (this.clone(), this.clone(), this.clone(), this.clone());
    let (reopen, pin, rename) = (this.clone(), this.clone(), this.clone());
    let path = entry.path.clone();
    let pinned = app.read(cx).tabs().is_pinned(&entry);
    // 这个标签页的工作区按钮先写哪个（从哪个工作区打开的）
    let home = this
        .upgrade()
        .and_then(|this| this.read(cx).tabs.get(ix).map(|tab| tab.view.clone()))
        .and_then(|view| view.read(cx).home_hint());
    let menu = menu
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
            PopupMenuItem::new(crate::keymap::with_key(
                tr("重新打开已关闭的标签页（Ctrl+Shift+T）"),
                "tab.reopen",
                cx,
            ))
            .disabled(!can_reopen)
            .on_click(move |_, window, cx| {
                reopen
                    .update(cx, |this, cx| this.reopen_closed(window, cx))
                    .ok();
            }),
        )
        .separator();
    let add = crate::ws_links::plain_add(app);
    crate::ws_links::context_items(menu, app, &entry, home.as_ref(), true, &add, window, cx)
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
pub(crate) fn new_workspace_dialog(
    places: Vec<(String, SharedString)>,
    default: String,
    window: &mut Window,
    cx: &mut App,
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
                .font_family(crate::fonts::mono())
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

/// 「终端待查看」那条提醒的类别（同一个终端只留一条，前往以后去掉）。
struct TerminalStopped;

/// 「离开期间有新输出」那条提醒的类别（只留一条）。
struct AwayNote;

/// 「离开期间有新输出」那条系统通知的键（点了到会话页只看待查看的）。
const AWAY_TOAST: &str = "away";
/// 第一次收进托盘时那条说明的标记（点了只把窗口叫回来）。
const TRAY_TOAST: &str = "tray";

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

/// Kuai 的标志：和程序图标同一张图，只解一次。
fn kuai_logo() -> Arc<Image> {
    static LOGO: std::sync::OnceLock<Arc<Image>> = std::sync::OnceLock::new();
    LOGO.get_or_init(|| {
        Arc::new(Image::from_bytes(
            ImageFormat::Png,
            include_bytes!("../assets/icon.png").to_vec(),
        ))
    })
    .clone()
}

/// 窗口在用户眼前：激活着，而且看得见。最小化、藏进托盘的不算，哪怕还激活着（桌面上只剩它一个
/// 窗口时最小化不一定失去激活）。
fn in_front(window: &Window) -> bool {
    window.is_window_active() && window.visibility() != WindowVisibility::Hidden
}

fn stopped_note_key(id: &TermId) -> SharedString {
    format!("{}/{}", id.machine, id.term).into()
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title_bar = self.render_title_bar(window, cx).into_any_element();
        self.sync_shown(window, cx);
        let theme = cx.theme();
        let (bg, fg) = (theme.background, theme.foreground);
        let content = if self.welcome(cx) {
            self.render_welcome(cx)
        } else if self.showing_settings {
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
            // 鼠标侧键：后退、前进
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, window, cx| this.go_back(window, cx)),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _, window, cx| this.go_forward(window, cx)),
            )
            .child(ui::tooltip_guard())
            .child(title_bar)
            .child(div().flex_1().min_h_0().child(content))
    }
}

/// 到主机页（工作区页空状态的「前往主机页」）。
pub(crate) fn go_to_hosts(window: &mut Window, cx: &mut App) {
    let Some(this) = cx.try_global::<Navigator>().map(|nav| nav.0.clone()) else {
        return;
    };
    window.defer(cx, move |window, cx| {
        this.update(cx, |this, cx| this.show_page(Page::Servers, window, cx))
            .ok();
    });
}

/// 选仓库的框里选中了一个仓库。
pub(crate) struct Picked(pub RepoEntry);

/// 选仓库干什么。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickFor {
    /// 快速打开（Ctrl+P）：刚关掉的、开着的在前，行尾写着
    Open,
    /// 在里面新建终端（会话页的 +、Ctrl+Shift+N）：行尾写它在哪个工作区
    Terminal,
}

/// 选仓库的框的按键上下文：新建终端时 Tab、Shift+Tab 换要运行的程序（见 `main.rs`）。
pub(crate) const QUICK_OPEN_CONTEXT: &str = "QuickOpen";

/// 选仓库的框：一个输入框加仓库列表，输入的字按名字和路径筛选。快速打开和新建终端共用。
pub(crate) struct QuickOpen {
    app: Entity<AppState>,
    input: Entity<InputState>,
    purpose: PickFor,
    /// 高亮的（回车会选的）是哪个仓库：按仓库记，不按第几行（列表会变：别的主机连上了，列表里多了
    /// 仓库）。None 是第一个；画的时候记成画出来高亮的那个，之后列表变了它也不跟着换
    selected: Option<RepoEntry>,
    /// 新建终端：框上面那一排要运行的程序选的是第几个（0 是只开 shell，往后是菜单里列的预设，见
    /// [`Self::programs`]）。开始时是「默认运行」的那个，Tab、Shift+Tab 换
    program: usize,
    _subscription: Subscription,
}

impl EventEmitter<Picked> for QuickOpen {}

/// 把选仓库的框弹出来（选中以后的事由订阅 [`Picked`] 的那一方做，记得关掉对话框）。
pub(crate) fn show_picker(
    picker: Entity<QuickOpen>,
    title: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    // 点按钮（会话页标题行的 +、标题栏的 +）弹出来的：鼠标还停在按钮上，排着队的提示半秒后会盖在
    // 对话框的标题上
    ui::cancel_pending_tip(window, cx);
    let first_build = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        // 对话框打开的那一刻焦点会被它拿走，打开后再放到输入框
        if first_build.replace(false) {
            let input = picker.read(cx).input.clone();
            input.focus_handle(cx).focus(window, cx);
        }
        let on_enter = picker.clone();
        dialog
            .title(title.clone())
            .w(px(620.))
            // 回车选中高亮的那个
            .on_ok(move |_, _, cx| {
                on_enter.update(cx, |picker, cx| picker.pick_selected(cx));
                false
            })
            .child(picker.clone())
    });
}

impl QuickOpen {
    pub(crate) fn new(
        app: Entity<AppState>,
        purpose: PickFor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("仓库名称或路径中的关键字")));
        let subscription = cx.subscribe(&input, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                // 筛选的字变了：回到第一个
                this.selected = None;
                cx.notify();
            }
        });
        let default = crate::presets::default_preset(cx).map(|preset| preset.id);
        let program = Self::programs(cx)
            .iter()
            .position(|start| start.as_ref().map(|(id, _)| id) == default.as_ref())
            .unwrap_or(0);
        Self {
            app,
            input,
            purpose,
            selected: None,
            program,
            _subscription: subscription,
        }
    }

    /// 新建终端时可以选的程序：只开 shell（None），再是菜单里列的预设（id 和要输入的）。「默认运行」
    /// 的那个设成不在菜单里显示也列上：侧栏的「新终端」、Ctrl+Shift+` 照样运行它，这里原来却退回
    /// 只开 shell。
    fn programs(cx: &App) -> Vec<Option<(String, crate::presets::Start)>> {
        let default = crate::presets::default_preset(cx).map(|preset| preset.id);
        std::iter::once(None)
            .chain(
                crate::presets::all(cx)
                    .iter()
                    .filter(|preset| preset.shown || default.as_ref() == Some(&preset.id))
                    .map(|preset| Some((preset.id.clone(), crate::presets::Start::of(preset)))),
            )
            .collect()
    }

    /// 新建终端：上面那一排选的程序（None 是只开 shell）。
    pub(crate) fn chosen_start(&self, cx: &App) -> Option<crate::presets::Start> {
        Self::programs(cx)
            .into_iter()
            .nth(self.program)
            .flatten()
            .map(|(_, start)| start)
    }

    /// 匹配输入内容的仓库：名字、路径、服务器名里有这几个字（不分大小写）。
    /// 快速打开：先是刚关掉的（最近关的在最上），再是开着的，最后是各台电脑列表里的其余仓库（本机
    /// 的在前）；第三项是行尾标的小字：刚关掉、开着。新建终端：各台电脑列表里的仓库，行尾写它在
    /// 哪个工作区（不在的写「未加入工作区」）。
    fn matches(&self, cx: &App) -> Vec<(RepoEntry, SharedString, Option<SharedString>)> {
        let needle = self.input.read(cx).value().trim().to_lowercase();
        let app = self.app.read(cx);
        let wanted = |(entry, title, _): &(RepoEntry, SharedString, Option<SharedString>)| {
            needle.is_empty()
                || title.to_lowercase().contains(&needle)
                || entry.path.to_lowercase().contains(&needle)
        };
        if self.purpose == PickFor::Terminal {
            return app
                .all_repos()
                .into_iter()
                .map(|entry| {
                    let place = match app.workspace_of(&entry) {
                        Some(key) => app.workspace_name(&key),
                        None => tr("未加入工作区").into(),
                    };
                    let title = app.repo_title(&entry);
                    (entry, title, Some(place))
                })
                .filter(wanted)
                .collect();
        }
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
        let closed_mark: SharedString = tr("最近关闭").into();
        let open_mark: SharedString = tr("已打开").into();
        closed
            .iter()
            .map(|entry| (entry.clone(), Some(closed_mark.clone())))
            .chain(
                open.iter()
                    .map(|entry| (entry.clone(), Some(open_mark.clone()))),
            )
            .chain(rest.into_iter().map(|entry| (entry, None)))
            .map(|(entry, mark)| {
                let title = app.repo_title(&entry);
                (entry, title, mark)
            })
            .filter(wanted)
            .collect()
    }

    /// 回车：选高亮的那个。它已经不在列表里了（列表变了）就不选，换成高亮现在的第一个，再按一次。
    fn pick_selected(&mut self, cx: &mut Context<Self>) {
        let matches = self.matches(cx);
        let target = match &self.selected {
            Some(entry) => Some(entry.clone()),
            None => matches.first().map(|(entry, ..)| entry.clone()),
        };
        match target.filter(|target| matches.iter().any(|(entry, ..)| entry == target)) {
            Some(entry) => cx.emit(Picked(entry)),
            None => {
                self.selected = None;
                cx.notify();
            }
        }
    }

    /// 新建终端时 Tab、Shift+Tab：换要运行的程序，到头绕回去。快速打开时照旧切焦点。
    fn next_program(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.purpose != PickFor::Terminal {
            cx.propagate();
            return;
        }
        let count = Self::programs(cx).len().max(1);
        self.program = match forward {
            true => (self.program + 1) % count,
            false => (self.program + count - 1) % count,
        };
        cx.notify();
    }

    /// 上下键：在匹配结果里移动，到头绕回去。
    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let matches = self.matches(cx);
        let count = matches.len();
        if count == 0 {
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|selected| matches.iter().position(|(entry, ..)| entry == selected))
            .unwrap_or(0);
        let next = match event.keystroke.key.as_str() {
            "down" => (at + 1) % count,
            "up" => (at + count - 1) % count,
            _ => return,
        };
        self.selected = Some(matches[next].0.clone());
        cx.stop_propagation();
        cx.notify();
    }
}

impl QuickOpen {
    /// 新建终端的框上面那一排：要运行的程序（只开 shell、各个预设），选中的那个描边；行尾写怎么换、
    /// 「管理预设…」。
    fn render_programs(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, primary, hover, active) = (
            theme.muted_foreground,
            theme.border,
            theme.primary,
            theme.list_hover,
            theme.list_active,
        );
        let programs: Vec<(gpui_kit::component::Icon, SharedString)> = Self::programs(cx)
            .iter()
            .map(|program| match program {
                Some((_, start)) => (
                    crate::presets::icon(&start.icon)
                        .size_3p5()
                        .text_color(crate::presets::color(&start.icon, start.hue, cx)),
                    SharedString::from(start.name.clone()),
                ),
                None => (
                    ui::icon(IconName::SquareTerminal).text_color(muted),
                    SharedString::from(tr("终端")),
                ),
            })
            .collect();
        let chosen = self.program.min(programs.len().saturating_sub(1));
        let chips: Vec<AnyElement> = programs
            .into_iter()
            .enumerate()
            .map(|(ix, (icon, name))| {
                h_flex()
                    .id(("pick-program", ix))
                    .h(px(26.))
                    .px_2()
                    .gap_1p5()
                    .rounded(px(ui::RADIUS))
                    .border_1()
                    .border_color(if ix == chosen { primary } else { border })
                    .when(ix == chosen, |chip| chip.bg(active))
                    .when(ix != chosen, |chip| chip.hover(|s| s.bg(hover)))
                    .cursor_pointer()
                    .text_sm()
                    .child(icon)
                    .child(name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.program = ix;
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        v_flex()
            .gap_1()
            .child(h_flex().flex_wrap().gap_1().children(chips))
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(muted)
                    .child(div().child(tr("Tab 切换要运行的程序")))
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("pick-manage")
                            .cursor_pointer()
                            .hover(|s| s.text_color(primary))
                            .child(tr("管理预设…"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                                crate::with_main_workspace(cx, |workspace, window, cx| {
                                    workspace.open_terminal_settings(window, cx)
                                });
                            }),
                    ),
            )
            .into_any_element()
    }
}

impl Render for QuickOpen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list_height = (window.viewport_size().height - px(240.)).clamp(px(120.), px(360.));
        let theme = cx.theme();
        let (muted, hover, active) = (theme.muted_foreground, theme.list_hover, theme.list_active);
        let matches = self.matches(cx);
        // 高亮的那个不在列表里了（还没选过、列表变了）：高亮第一个，记下它
        let selected = match self
            .selected
            .as_ref()
            .and_then(|selected| matches.iter().position(|(entry, ..)| entry == selected))
        {
            Some(at) => at,
            None => {
                self.selected = matches.first().map(|(entry, ..)| entry.clone());
                0
            }
        };
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
                        row.child(div().flex_1())
                            .child(div().flex_none().text_xs().text_color(muted).child(mark))
                    })
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        let programs = (self.purpose == PickFor::Terminal).then(|| self.render_programs(cx));
        v_flex()
            .gap_2()
            .key_context(QUICK_OPEN_CONTEXT)
            .capture_key_down(cx.listener(Self::on_key))
            .on_action(
                cx.listener(|this, _: &crate::NextProgram, _, cx| this.next_program(true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &crate::PrevProgram, _, cx| this.next_program(false, cx)),
            )
            .children(programs)
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
