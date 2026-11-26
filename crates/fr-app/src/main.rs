// 发布版不带控制台窗口；日志要看的话把标准错误重定向到文件
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_state;
mod appearance;
mod autostart;
mod browser;
mod clone;
mod command_palette;
mod desktop;
mod diagnose;
mod ext_tools;
mod fonts;
mod graph_paint;
mod history;
mod instance;
mod keymap;
mod keys_page;
mod machines;
mod manager;
mod mapping;
mod new_project;
mod open_dir;
mod open_with;
mod packaging;
mod palette;
mod port_forward;
mod presets;
mod prompt;
mod redirect_guard;
mod repo;
mod repo_rows;
mod repo_scan;
mod secrets;
mod settings;
mod settings_page;
mod shell;
mod side_split;
#[cfg(target_os = "linux")]
mod signals;
mod tab_fit;
mod term_board;
mod term_end;
mod term_memory;
mod term_names;
mod term_overview;
mod term_split;
mod terminal;
mod text;
mod toast;
mod tray;
mod ui;
mod update;
mod update_view;
mod web_serve;
mod win;
mod workspace;
mod workspace_page;
mod ws_links;
mod zoom;

use std::path::PathBuf;

use futures::StreamExt as _;
use gpui_kit::component::input::{MoveToNextWord, MoveToPreviousWord};
use gpui_kit::*;

use crate::app_state::AppState;
use crate::workspace::Workspace;

gpui_kit::actions!(
    kuai,
    [
        ToggleFrameOverlay,
        Refresh,
        CloseTab,
        NextTab,
        PrevTab,
        FindCommit,
        QuickOpen,
        ShowChanges,
        ShowHistory,
        OpenSettings,
        ReopenTab,
        OpenTerminal,
        NewTerminal,
        ToggleSidebar,
        ShowFiles,
        SaveFile,
        TogglePreview,
        GoBack,
        GoForward,
        ShowTerminals,
        PickNewTerminal,
        GoToHead,
        NextProgram,
        PrevProgram,
        DiffLineUp,
        DiffLineDown,
        DiffExtendUp,
        DiffExtendDown,
        Fetch,
        Pull,
        Push,
        StashChanges,
        PopStash,
        NewBranch,
        FocusSwitch,
        FocusSwitchBack,
        RenameTerminal,
        OpenTerminalRepo
    ]
);

/// 焦点在终端里时，键位表里出现过的键除了终端里也可用的快捷键（[`keymap::terminal_keeps`]，再加上
/// Ctrl+Alt+Shift+F 帧耗时浮层）一律不当快捷键；设置页录快捷键时（[`keymap::RECORD_CONTEXT`]）全都不当。
///
/// GPUI 收到按键先查键位表，查不到才轮到终端自己的按键处理。所以别处绑过的键终端收不到：
/// 界面库在最外层绑了 Tab、Shift+Tab（切焦点）和 Ctrl+C（复制界面上选中的文字），补全、
/// Claude Code 切模式、中断程序都会被它拦下。这里不一个个列「哪些键要让给终端」——漏一个就是
/// 一个用不了的键，界面库升级多绑一个也发现不了——而是反过来只留终端里也可用的，其余的每一个
/// 都在终端这一层绑成「什么都不做」，盖住外层的绑定。之后在设置里改的键由 `keymap::sync` 跟着改。
///
/// 要在所有快捷键都登记完以后调用：同一层里后登记的优先。
/// 帧耗时浮层（排查卡顿用）的键。
const FRAME_OVERLAY_KEY: &str = "ctrl-alt-shift-f";

fn release_keys_to_terminal(cx: &mut App) {
    let mut keeps = keymap::terminal_keeps(cx);
    keeps.push(keymap::canonical(FRAME_OVERLAY_KEY, cx));
    let mut taken: Vec<String> = {
        let bindings = cx.key_bindings();
        let bindings = bindings.borrow();
        bindings
            .bindings()
            .map(|binding| keymap::binding_keys(binding, cx))
            .collect()
    };
    taken.sort();
    taken.dedup();
    let context = |name: &str| {
        Some(std::rc::Rc::new(
            KeyBindingContextPredicate::parse(name).expect("合法的上下文名"),
        ))
    };
    let (terminal, record) = (
        context(terminal::KEY_CONTEXT),
        context(keymap::RECORD_CONTEXT),
    );
    let mut masks: Vec<KeyBinding> = Vec::new();
    for keys in &taken {
        // Linux 上 Shift 加符号键按下时收到的是另一种写法，两种都让（`keymap::forms`）
        for form in keymap::forms(keys) {
            let mask = |context: &Option<std::rc::Rc<KeyBindingContextPredicate>>| {
                KeyBinding::load(&form, Box::new(NoAction), context.clone(), false, None, cx)
                    .inspect_err(|error| eprintln!("让不出这个键 {form}：{error}"))
                    .ok()
            };
            if !keeps.contains(keys) {
                masks.extend(mask(&terminal));
            }
            masks.extend(mask(&record));
        }
    }
    cx.bind_keys(masks);
}

/// 主窗口里的工作区，全局的快捷键靠它找到要操作的对象。
struct MainWorkspace(WeakEntity<Workspace>, AnyWindowHandle);

impl Global for MainWorkspace {}

/// 登记一个全局快捷键动作：在工作区上执行 `f`。
/// 动作是在窗口处理按键的过程中触发的，这时窗口正被占用，所以推迟到这一轮处理完再执行。
fn workspace_action<A: Action>(
    cx: &mut App,
    f: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + Copy + 'static,
) {
    cx.on_action(move |_: &A, cx: &mut App| {
        cx.defer(move |cx| {
            let Some(MainWorkspace(workspace, handle)) = cx.try_global::<MainWorkspace>() else {
                return;
            };
            let (workspace, handle) = (workspace.clone(), *handle);
            handle
                .update(cx, |_, window, cx| {
                    workspace.update(cx, |ws, cx| f(ws, window, cx)).ok();
                })
                .ok();
        });
    });
}

/// 一个 Git 操作的快捷键：交给当前仓库，和点工具栏上的按钮一样。
fn git_action<A: Action>(cx: &mut App, op: repo::GitShortcut) {
    cx.on_action(move |_: &A, cx: &mut App| {
        cx.defer(move |cx| {
            let Some(MainWorkspace(workspace, handle)) = cx.try_global::<MainWorkspace>() else {
                return;
            };
            let (workspace, handle) = (workspace.clone(), *handle);
            handle
                .update(cx, |_, window, cx| {
                    workspace
                        .update(cx, |ws, cx| ws.git_in_active(op, window, cx))
                        .ok();
                })
                .ok();
        });
    });
}

/// 设置文件的问题在主窗口上说：启动时读它出了问题（见 [`settings::LoadIssue`]）、锁不上它
/// （`unlocked`），之后存不了。这几种通知都不自己消失，用户得看到。
fn tell_settings_problems(
    load_issue: Option<settings::LoadIssue>,
    unlocked: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::notification::Notification;
    if let Some(issue) = load_issue {
        window.push_notification(Notification::error(issue.text()).autohide(false), cx);
    }
    if let Some(err) = unlocked {
        let text = fr_core::tf!(
            "无法锁定设置文件（{err}），无法确定是否有另一个 Kuai 在使用它。这次运行中的改动不会保存，以免互相覆盖。",
            err
        );
        window.push_notification(Notification::error(text).autohide(false), cx);
    }
    let mut failures = app_state::save_failures();
    window
        .spawn(cx, async move |cx| {
            while let Some(err) = failures.next().await {
                let text = fr_core::tf!("设置未能保存：{err}", err);
                let told = cx.update(|window, cx| {
                    window.push_notification(Notification::error(text).autohide(false), cx)
                });
                if told.is_err() {
                    break;
                }
            }
        })
        .detach();
}

/// 设置 FR_LOG=1 时把日志打到标准错误，比如 GPUI 选用了哪块显卡。zbus、ksni 这些经 D-Bus 的库
/// 每条消息记一行，只要它们的警告和错误。
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        let chatty = ["zbus", "ksni", "tracing", "fuser"]
            .iter()
            .any(|name| metadata.target().starts_with(name));
        match chatty {
            true => metadata.level() <= log::Level::Warn,
            false => metadata.level() <= log::Level::Info,
        }
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!(
                "[{}] {}: {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

/// 在主窗口上做一件事（设置页里出了错要弹通知之类）。推迟到这一轮处理完再做。
pub(crate) fn with_main_window(cx: &mut App, f: impl FnOnce(&mut Window, &mut App) + 'static) {
    cx.defer(move |cx| {
        let Some(MainWorkspace(_, handle)) = cx.try_global::<MainWorkspace>() else {
            return;
        };
        let handle = *handle;
        handle.update(cx, |_, window, cx| f(window, cx)).ok();
    });
}

/// 在主窗口的工作区上做一件事（菜单里点了「管理预设…」要打开设置页之类）。推迟到这一轮处理完再做。
pub(crate) fn with_main_workspace(
    cx: &mut App,
    f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    cx.defer(move |cx| {
        let Some(MainWorkspace(workspace, handle)) = cx.try_global::<MainWorkspace>() else {
            return;
        };
        let (workspace, handle) = (workspace.clone(), *handle);
        handle
            .update(cx, |_, window, cx| {
                workspace.update(cx, |ws, cx| f(ws, window, cx)).ok();
            })
            .ok();
    });
}

/// 安装版的卸载程序在删文件之前调用（`tools/installer/kuai.iss`），不开窗口：
/// - `--has-saved-passwords`：凭据管理器里存着 Kuai 的密码、口令时退出码 0，没有是 1；
/// - `--has-local-kuaid`：这台电脑上有本机 kuaid 的目录（`~/.kuai`）时退出码 0，没有是 1；
/// - `--uninstall`：开机自启指向这个程序的话删掉（指向别处的，比如另一份解压版，不动）；
///   再带 `--forget-passwords` 时把存的密码、口令也删掉；再带 `--remove-local-kuaid` 时停掉本机的
///   kuaid、删掉 `~/.kuai`（见 [`remove_local_kuaid`]）；发系统通知用的标识和图标也删掉。
fn uninstall_command() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    if has("--has-saved-passwords") {
        return Some(if secrets::saved_count() > 0 { 0 } else { 1 });
    }
    if has("--has-local-kuaid") {
        let root = fr_channel::local_kuai_root();
        let found = std::fs::read_dir(&root).is_ok_and(|mut entries| entries.next().is_some());
        return Some(if found { 0 } else { 1 });
    }
    if !has("--uninstall") {
        return None;
    }
    if autostart::enabled() {
        let _ = autostart::set(false);
    }
    toast::unregister();
    if has("--forget-passwords") {
        secrets::forget_all();
    }
    if has("--remove-local-kuaid") {
        remove_local_kuaid();
    }
    Some(0)
}

/// 卸载时选了「同时停止本机的 kuaid」：让常驻进程结束所有终端、退出（`kuaid --stop`），再删掉
/// `~/.kuai`（各版本的 kuaid 和它运行时的文件；仓库列表、工作区在 `%APPDATA%\kuai`，和设置一起留着）。
/// 这台电脑的 kuaid 也给连到这台电脑的别的电脑用，它们在这里开的终端一起结束，所以卸载程序先问。
/// 程序退出、文件解开要一会儿：删不掉的隔一会儿再试，最多等 15 秒，还删不掉的留着。
fn remove_local_kuaid() {
    use std::time::{Duration, Instant};
    let root = fr_channel::local_kuai_root();
    let current = std::fs::read_to_string(root.join("run").join("current")).unwrap_or_default();
    let current = current.trim().to_string();
    // 常驻进程正在用的那一版放最前面；哪一版的 `--stop` 都能叫它退出，前面的不成再试下一个
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(|c: char| c.is_ascii_digit())
                })
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    versions.sort_by_key(|dir| {
        dir.file_name()
            .is_none_or(|name| name.to_string_lossy() != current)
    });
    for dir in versions {
        let exe = dir.join("kuaid.exe");
        if !exe.is_file() {
            continue;
        }
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--stop")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        fr_core::ssh::no_window(&mut cmd);
        let Ok(mut child) = cmd.spawn() else {
            continue;
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        let stopped = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.success(),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(200));
                }
                _ => {
                    let _ = child.kill();
                    break false;
                }
            }
        };
        if stopped {
            break;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while root.exists() && std::fs::remove_dir_all(&root).is_err() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// 测试实例（设了 `FR_SETTINGS`）没另设 `FR_KUAID_HOME` 时，给它在设置文件旁边单独一套本机
/// kuaid 的状态（`kuaid-home`）。不然本机的 kuaid、工作区、主机列表和日常在用的是同一份：测试设置
/// 里的主机列表一交过去，日常的就被换掉，失去主机的仓库也从工作区里删掉。要和日常的共用，把
/// `FR_KUAID_HOME` 明确设成那一套。
fn isolate_test_instance() {
    let Some(settings) = std::env::var_os("FR_SETTINGS").filter(|path| !path.is_empty()) else {
        return;
    };
    if std::env::var_os("FR_KUAID_HOME").is_some_and(|dir| !dir.is_empty()) {
        return;
    }
    let home = PathBuf::from(settings)
        .parent()
        .map_or_else(|| PathBuf::from("kuaid-home"), |dir| dir.join("kuaid-home"));
    // SAFETY: 启动的第一件事，这时还没有别的线程在读环境变量
    unsafe { std::env::set_var("FR_KUAID_HOME", &home) };
}

/// Linux：桌面启动这个进程时给的激活令牌（Wayland 的 `XDG_ACTIVATION_TOKEN`、X11 的
/// `DESKTOP_STARTUP_ID`）。已经有 Kuai 在运行时交给它，它拿令牌才能把窗口叫到前面。读完从环境变量里
/// 去掉，免得终端里启动的程序拿到这个用过的令牌（GTK 也这么做）。
fn take_activation_token() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let mut token = None;
    for name in ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"] {
        if let Some(value) = std::env::var(name).ok().filter(|value| !value.is_empty()) {
            token.get_or_insert(value);
        }
        // SAFETY：启动的头几件事之一，这时还没有别的线程在读环境变量
        unsafe { std::env::remove_var(name) };
    }
    token
}

fn main() {
    // 甩掉 Redirection Guard 后重新启动的：先把带过来的 FR_* 环境变量设上，后面的都照它们来
    redirect_guard::restore_env();
    isolate_test_instance();
    let activation = take_activation_token();
    // Linux：继承来的文件描述符不再往下传；从 AppImage 启动的话记下那个文件，去掉它的环境变量
    // （别传给终端里的程序）
    packaging::take_env();
    // 被 git 或 ssh 当作 askpass 启动时：去问已经开着的主程序，打出回答就退出
    if let Some(code) = fr_core::askpass::run_helper() {
        std::process::exit(code);
    }
    // 被 ssh 当作 ProxyCommand 启动时：把连接转到跳板机上，直到连接结束
    if let Some(code) = fr_channel::run_ssh_hop() {
        std::process::exit(code);
    }
    if let Some(code) = uninstall_command() {
        std::process::exit(code);
    }
    if std::env::var_os("FR_LOG").is_some() {
        let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info));
    }
    // 开机自启带的参数：有托盘就直接收进托盘
    let start_minimized = std::env::args().any(|arg| arg == autostart::MINIMIZED_FLAG);
    // 命令行给了目录：打开它（不是仓库先问是否初始化），和 Fork 的 `fork <路径>` 一样。
    // 相对路径按这个进程的当前目录算，交给已经运行的 Kuai 之前先换成绝对路径
    let open_arg = std::env::args_os()
        .skip(1)
        .find(|arg| !arg.to_string_lossy().starts_with("--"))
        .map(|arg| {
            let path = PathBuf::from(arg);
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            let path = path.to_string_lossy();
            path.strip_prefix(r"\\?\").unwrap_or(&path).to_string()
        });
    // 只运行一个 Kuai：已经有一个在运行，就把显示窗口、打开目录交给它，这个进程退出
    let request = instance::Request {
        open: open_arg.clone(),
        show: !start_minimized,
        activation,
        ..Default::default()
    };
    let (single, incoming) = match instance::claim(&request) {
        instance::Claim::First(guard, incoming) => (guard, incoming),
        instance::Claim::Forwarded => return,
    };
    // 带着 Windows 的 Redirection Guard 启动的（旧版本的安装程序拉起的、从带着它的终端里启动的）：
    // 它会传给 kuaid 和终端里的所有程序，目录联接、符号链接都进不去。请资源管理器重新启动一遍，
    // 这个进程退出（见 redirect_guard.rs）
    if redirect_guard::relaunch_if_inherited(open_arg.as_deref()) {
        return;
    }
    // 上次下好了新版本、没点重启就退出了：先换上，拉起新版本，这个进程就不开窗口了
    if update::apply_staged_at_startup() {
        return;
    }
    // Linux：在后台问一下这个桌面有没有托盘，建托盘、设置页显示托盘选项时要用
    tray::probe();
    // Linux：应用菜单里的 Kuai 和图标（解压即用，没有安装程序）
    desktop::integrate();

    // 本机 git 的版本号先在后台问好（读工作树清单时要看），界面线程上不等 git 启动
    std::thread::spawn(fr_core::git::local_version);

    let (settings, load_issue) = settings::load();
    // 锁不上设置文件：这次运行不写它（见 `instance::Guard::unlocked`）
    let unlocked = single.unlocked().map(str::to_string);
    if let Some(err) = &unlocked {
        settings::hold_writes(err);
    }
    // 语言在画任何界面之前就定下来
    fr_core::i18n::set_lang(settings.language.resolve());

    // git、ssh 要密码时交给界面弹窗
    let prompts = prompt::init();

    // 图标要注册了资源才显示。用整套 Lucide 图标（程序大 1 MB 左右），界面上用到哪个不用再登记；
    // 再加上新建终端的预设里各家的图标
    gpui_kit::application()
        .with_assets(presets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            // Linux 上挑等宽字体（没有 Consolas）
            fonts::init(cx);
            // 组件库自带的文字（输入框的右键菜单、对话框的关闭按钮等）跟着界面语言
            gpui_kit::component::set_locale(fr_core::i18n::lang().code());
            cx.bind_keys([
                // 切换 GPUI 的帧耗时浮层（隐藏 → 简要 → 详细）。原来是 Ctrl+Shift+F，让给了「获取」
                // （和 Fork 一样）
                KeyBinding::new(FRAME_OVERLAY_KEY, ToggleFrameOverlay, None),
                // 新建终端的选仓库框里换要运行的程序。快捷键表先于按键监听，界面库在最外层把 Tab 绑成了
                // 切焦点，所以要在框自己的上下文里绑
                KeyBinding::new("tab", NextProgram, Some(workspace::QUICK_OPEN_CONTEXT)),
                KeyBinding::new(
                    "shift-tab",
                    PrevProgram,
                    Some(workspace::QUICK_OPEN_CONTEXT),
                ),
                // 本地修改的并排视图：焦点在组件库的改动正文里时，上下键原本是它的滚动。同一个上下文里
                // 后绑的先试：本地修改那边接了就是选行，没接（别处的改动视图、没有能选的行）
                // 照样交给组件库滚动
                KeyBinding::new("up", DiffLineUp, Some("Diff")),
                KeyBinding::new("down", DiffLineDown, Some("Diff")),
                KeyBinding::new("shift-up", DiffExtendUp, Some("Diff")),
                KeyBinding::new("shift-down", DiffExtendDown, Some("Diff")),
            ]);
            // 能在设置里改的快捷键（关标签页、切页面、新建终端……）按设置绑，见 keymap.rs。给预设设的
            // 快捷键跟着预设走，所以先把预设定下来
            presets::sync(&settings, cx);
            keymap::sync(&settings, cx);
            // 后退、前进默认是 Alt+←、Alt+→，焦点在输入框里时归输入框（往回、往后一个词）：没有上下文的
            // 绑定和输入框自己的一样深，后登记的在前，所以在快捷键后面再绑一遍。界面库的输入框和默认的
            // 快捷键撞的一共五个：Alt+←、Alt+→（让给输入框）、Ctrl+F（全局的接住以后按页面分派，文件页
            // 交给编辑器的查找，见 `RepoView::find`）、Ctrl+Shift+A 和 Ctrl+Shift+E（界面库拿来选到行首、
            // 行尾，Windows 上没人这么按，留给切页面）。界面库升级后重新对一遍。在设置里改了的键怎么和
            // 输入框分，见 `keymap::sync`
            cx.bind_keys([
                KeyBinding::new("alt-left", MoveToPreviousWord, Some("Input")),
                KeyBinding::new("alt-right", MoveToNextWord, Some("Input")),
            ]);
            release_keys_to_terminal(cx);
            workspace_action::<OpenSettings>(cx, |ws, window, cx| ws.toggle_settings(window, cx));
            workspace_action::<ShowTerminals>(cx, |ws, window, cx| ws.toggle_terms(window, cx));
            workspace_action::<PickNewTerminal>(cx, |ws, window, cx| {
                ws.pick_new_terminal(window, cx)
            });
            workspace_action::<Refresh>(cx, |ws, _, cx| ws.refresh_active(cx));
            // 工具栏上的几个 Git 操作（设置 → 快捷键 → Git）
            for (op, register) in [
                (
                    repo::GitShortcut::Fetch,
                    git_action::<Fetch> as fn(&mut App, repo::GitShortcut),
                ),
                (repo::GitShortcut::Pull, git_action::<Pull>),
                (repo::GitShortcut::Push, git_action::<Push>),
                (repo::GitShortcut::Stash, git_action::<StashChanges>),
                (repo::GitShortcut::Pop, git_action::<PopStash>),
                (repo::GitShortcut::Branch, git_action::<NewBranch>),
            ] {
                register(cx, op);
            }
            workspace_action::<CloseTab>(cx, |ws, window, cx| ws.close_active(window, cx));
            workspace_action::<ShowChanges>(cx, |ws, window, cx| {
                ws.show_in_active(true, window, cx)
            });
            workspace_action::<ShowHistory>(cx, |ws, window, cx| {
                ws.show_in_active(false, window, cx)
            });
            workspace_action::<GoToHead>(cx, |ws, window, cx| ws.go_to_head(window, cx));
            workspace_action::<NextTab>(cx, |ws, window, cx| ws.cycle_tab(1, window, cx));
            workspace_action::<PrevTab>(cx, |ws, window, cx| ws.cycle_tab(-1, window, cx));
            workspace_action::<FindCommit>(cx, |ws, window, cx| ws.find_commit(window, cx));
            workspace_action::<QuickOpen>(cx, |ws, window, cx| ws.quick_open(window, cx));
            workspace_action::<ReopenTab>(cx, |ws, window, cx| ws.reopen_closed(window, cx));
            workspace_action::<OpenTerminal>(cx, |ws, window, cx| {
                ws.terminal_in_active(window, cx)
            });
            workspace_action::<NewTerminal>(cx, |ws, window, cx| {
                ws.new_terminal_in_active(window, cx)
            });
            workspace_action::<ToggleSidebar>(cx, |ws, window, cx| {
                ws.toggle_sidebar_in_active(window, cx)
            });
            workspace_action::<ShowFiles>(cx, |ws, window, cx| ws.files_in_active(window, cx));
            workspace_action::<SaveFile>(cx, |ws, window, cx| ws.save_in_active(window, cx));
            workspace_action::<TogglePreview>(cx, |ws, window, cx| {
                ws.preview_in_active(window, cx)
            });
            workspace_action::<GoBack>(cx, |ws, window, cx| ws.go_back(window, cx));
            workspace_action::<GoForward>(cx, |ws, window, cx| ws.go_forward(window, cx));
            // 新建终端并运行某个预设（设置 → 快捷键里给预设设的键）。和上面一样推迟到这一轮处理完
            cx.on_action(|action: &keymap::RunPreset, cx: &mut App| {
                let preset = action.preset.clone();
                cx.defer(move |cx| {
                    let Some(MainWorkspace(workspace, handle)) = cx.try_global::<MainWorkspace>()
                    else {
                        return;
                    };
                    let (workspace, handle) = (workspace.clone(), *handle);
                    handle
                        .update(cx, |_, window, cx| {
                            workspace
                                .update(cx, |ws, cx| ws.run_preset_in_active(&preset, window, cx))
                                .ok();
                        })
                        .ok();
                });
            });
            cx.on_action(|_: &ToggleFrameOverlay, cx: &mut App| {
                for handle in cx.windows() {
                    handle
                        .update(cx, |_, window, _| window.cycle_debug_frame_overlay_mode())
                        .ok();
                }
            });

            let saved_window = settings.window;
            terminal::set_prefs(&settings.terminal, cx);
            let app = cx.new(|cx| AppState::new(settings, load_issue.clone(), cx));
            presets::init(&app, cx);
            // 在终端里按 Ctrl+加号、减号调了字号：存进设置
            terminal::set_prefs_writer(cx, {
                let app = app.clone();
                move |prefs, cx| {
                    app.update(cx, |app, cx| {
                        app.update_settings(cx, |s| s.terminal = prefs)
                    });
                }
            });
            let updater = cx.new(|cx| update::Updater::new(app.clone(), cx));
            // 标题栏自己画（标签页放在里面），所以用组件库配套的窗口选项
            let options = WindowOptions {
                window_bounds: Some(appearance::initial_bounds(saved_window, cx)),
                window_min_size: Some(size(px(appearance::MIN_SIZE.0), px(appearance::MIN_SIZE.1))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Kuai".into()),
                    ..gpui_kit::component::TitleBar::title_bar_options()
                }),
                // Linux 上也自己画标题栏和最小化、最大化、关闭（阴影、拖边改大小由组件库的窗口边框
                // 来做）；不要的话窗口管理器在上面再加一条自己的标题栏。桌面不支持时（没有合成器的
                // X11）窗口管理器照样加，组件库那时不画按钮。
                window_decorations: cfg!(target_os = "linux").then_some(WindowDecorations::Client),
                // Linux 上窗口的 WM_CLASS / app_id：桌面靠它对上 kuai.desktop，任务栏才显示 Kuai 的图标
                app_id: cfg!(target_os = "linux").then(|| "kuai".to_string()),
                ..gpui_kit::component::TitleBar::window_options()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                if std::env::var("FR_M0_OVERLAY").is_ok_and(|v| v == "1") {
                    window.set_debug_frame_overlay_mode(DebugFrameOverlayMode::Minimal);
                }
                // 主题、语言、托盘都由工作区按设置应用
                let workspace = cx.new(|cx| Workspace::new(app, updater, prompts, window, cx));
                cx.set_global(MainWorkspace(workspace.downgrade(), window.window_handle()));
                if start_minimized {
                    workspace.update(cx, |ws, cx| ws.start_in_tray(window, cx));
                }
                // 通知、对话框要等组件库给窗口装上它那一层（open_window 在这个闭包返回以后才装），
                // 在这里直接推会 panic：原来设置文件读不出来、命令行给的目录不存在时一启动就崩
                let opener = workspace.downgrade();
                window.defer(cx, move |window, cx| {
                    if let Some(dir) = open_arg {
                        opener
                            .update(cx, |ws, cx| ws.open_local_dir(dir, window, cx))
                            .ok();
                    }
                    tell_settings_problems(load_issue, unlocked, window, cx);
                });
                workspace
            })
            .expect("打开窗口失败");
            cx.activate(true);
            // 再次启动 Kuai 时交过来的请求：显示窗口、打开目录
            let mut incoming = incoming;
            cx.spawn(async move |cx| {
                while let Some(instance::Incoming { request, done }) = incoming.next().await {
                    let handled = cx.update(|cx| {
                        let Some(MainWorkspace(workspace, handle)) =
                            cx.try_global::<MainWorkspace>()
                        else {
                            return false;
                        };
                        let (workspace, handle) = (workspace.clone(), *handle);
                        handle
                            .update(cx, |_, window, cx| {
                                workspace
                                    .update(cx, |ws, cx| ws.on_launch_again(request, window, cx))
                            })
                            .is_ok_and(|updated| updated.is_ok())
                    });
                    if handled {
                        let _ = done.send(());
                    }
                }
            })
            .detach();
        });
}
