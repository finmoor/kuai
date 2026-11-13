// 发布版不带控制台窗口；日志要看的话把标准错误重定向到文件
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_state;
mod appearance;
mod autostart;
mod browser;
mod clone;
mod graph_paint;
mod instance;
mod manager;
mod open_dir;
mod palette;
mod prompt;
mod repo;
mod secrets;
mod settings;
mod settings_page;
mod shell;
mod terminal;
mod text;
mod tray;
mod ui;
mod update;
mod update_view;
mod win;
mod workspace;

use std::path::PathBuf;

use futures::StreamExt as _;
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
        ToggleSidebar
    ]
);

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

/// 设置 FR_LOG=1 时把日志打到标准错误，比如 GPUI 选用了哪块显卡。
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
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

/// 安装版的卸载程序在删文件之前调用（`tools/installer/kuai.iss`），不开窗口：
/// - `--has-saved-passwords`：凭据管理器里存着 Kuai 的密码、口令时退出码 0，没有是 1；
/// - `--uninstall`：开机自启指向这个程序的话删掉（指向别处的，比如另一份解压版，不动）；
///   再带 `--forget-passwords` 时把存的密码、口令也删掉。
fn uninstall_command() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    if has("--has-saved-passwords") {
        return Some(if secrets::saved_count() > 0 { 0 } else { 1 });
    }
    if !has("--uninstall") {
        return None;
    }
    if autostart::enabled() {
        let _ = autostart::set(false);
    }
    if has("--forget-passwords") {
        secrets::forget_all();
    }
    Some(0)
}

fn main() {
    // 被 git 或 ssh 当作 askpass 启动时：去问已经开着的主程序，打出回答就退出
    if let Some(code) = fr_core::askpass::run_helper() {
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
    };
    let (_instance, incoming) = match instance::claim(&request) {
        instance::Claim::First(guard, incoming) => (guard, incoming),
        instance::Claim::Forwarded => return,
    };
    // 上次下好了新版本、没点重启就退出了：先换上，拉起新版本，这个进程就不开窗口了
    if update::apply_staged_at_startup() {
        return;
    }

    let settings = settings::load();
    // 语言在画任何界面之前就定下来
    fr_core::i18n::set_lang(settings.language.resolve());

    // git、ssh 要密码时交给界面弹窗
    let prompts = prompt::init();

    // 图标要注册了资源才显示。用整套 Lucide 图标（程序大 1 MB 左右），界面上用到哪个不用再登记
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            // 组件库自带的文字（输入框的右键菜单、对话框的关闭按钮等）跟着界面语言
            gpui_kit::component::set_locale(fr_core::i18n::lang().code());
            // Ctrl+Shift+F：切换 GPUI 的帧耗时浮层（隐藏 → 简要 → 详细）
            cx.bind_keys([
                KeyBinding::new("ctrl-shift-f", ToggleFrameOverlay, None),
                KeyBinding::new("f5", Refresh, None),
                KeyBinding::new("ctrl-w", CloseTab, None),
                KeyBinding::new("ctrl-tab", NextTab, None),
                KeyBinding::new("ctrl-shift-tab", PrevTab, None),
                KeyBinding::new("ctrl-f", FindCommit, None),
                KeyBinding::new("ctrl-p", QuickOpen, None),
                KeyBinding::new("ctrl-1", ShowChanges, None),
                KeyBinding::new("ctrl-2", ShowHistory, None),
                KeyBinding::new("ctrl-,", OpenSettings, None),
                KeyBinding::new("ctrl-shift-t", ReopenTab, None),
                KeyBinding::new("ctrl-`", OpenTerminal, None),
                KeyBinding::new("ctrl-shift-`", NewTerminal, None),
                KeyBinding::new("ctrl-~", NewTerminal, None),
                // 不用 Ctrl+B：焦点在终端里时它归里面的程序（Claude Code 把命令转到后台、tmux 的前缀键）
                KeyBinding::new("ctrl-shift-b", ToggleSidebar, None),
            ]);
            // 焦点在终端里时，这几个键是 shell 和里面的程序要用的（删一个词、往前搜、上一条命令……），
            // 不当成上面的快捷键。切标签页、设置、Ctrl+` 这些终端用不到的照常
            cx.bind_keys(
                ["f5", "ctrl-w", "ctrl-f", "ctrl-p"]
                    .map(|key| KeyBinding::new(key, NoAction, Some("Terminal"))),
            );
            workspace_action::<OpenSettings>(cx, |ws, _, cx| ws.toggle_settings(cx));
            workspace_action::<Refresh>(cx, |ws, _, cx| ws.refresh_active(cx));
            workspace_action::<CloseTab>(cx, |ws, window, cx| ws.close_active(window, cx));
            workspace_action::<ShowChanges>(cx, |ws, window, cx| {
                ws.show_in_active(true, window, cx)
            });
            workspace_action::<ShowHistory>(cx, |ws, window, cx| {
                ws.show_in_active(false, window, cx)
            });
            workspace_action::<NextTab>(cx, |ws, _, cx| ws.cycle_tab(1, cx));
            workspace_action::<PrevTab>(cx, |ws, _, cx| ws.cycle_tab(-1, cx));
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
            cx.on_action(|_: &ToggleFrameOverlay, cx: &mut App| {
                for handle in cx.windows() {
                    handle
                        .update(cx, |_, window, _| window.cycle_debug_frame_overlay_mode())
                        .ok();
                }
            });

            let saved_window = settings.window;
            let app = cx.new(|_| AppState::new(settings));
            let updater = cx.new(|cx| update::Updater::new(app.clone(), cx));
            // 标题栏自己画（标签页放在里面），所以用组件库配套的窗口选项
            let options = WindowOptions {
                window_bounds: Some(appearance::initial_bounds(saved_window, cx)),
                window_min_size: Some(size(px(appearance::MIN_SIZE.0), px(appearance::MIN_SIZE.1))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Kuai".into()),
                    ..gpui_kit::component::TitleBar::title_bar_options()
                }),
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
                    workspace.update(cx, |ws, _| ws.start_in_tray());
                }
                if let Some(dir) = open_arg {
                    workspace.update(cx, |ws, cx| ws.open_local_dir(dir, window, cx));
                }
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
                                workspace.update(cx, |ws, cx| ws.on_launch_again(request, window, cx))
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
