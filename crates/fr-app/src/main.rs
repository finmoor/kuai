// 发布版不带控制台窗口；日志要看的话把标准错误重定向到文件
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_state;
mod appearance;
mod browser;
mod clone;
mod graph_paint;
mod manager;
mod prompt;
mod repo;
mod settings;
mod shell;
mod text;
mod workspace;

use std::path::PathBuf;

use gpui_kit::*;

use crate::app_state::AppState;
use crate::settings::RepoEntry;
use crate::workspace::Workspace;

gpui_kit::actions!(
    fork_remote,
    [
        ToggleFrameOverlay,
        Refresh,
        CloseTab,
        NextTab,
        PrevTab,
        FindCommit,
        QuickOpen
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

fn main() {
    // 被 git 或 ssh 当作 askpass 启动时：去问已经开着的主程序，打出回答就退出
    if let Some(code) = fr_core::askpass::run_helper() {
        std::process::exit(code);
    }
    if std::env::var_os("FR_LOG").is_some() {
        let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info));
    }

    let mut settings = settings::load();
    // 命令行给了仓库路径：加进本机仓库列表并打开，和 Fork 的 `fork <路径>` 一样
    if let Some(arg) = std::env::args_os().nth(1) {
        let path = PathBuf::from(arg);
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let path = path.to_string_lossy();
        let entry = RepoEntry {
            server: None,
            path: path.strip_prefix(r"\\?\").unwrap_or(&path).to_string(),
        };
        if !settings.repos.contains(&entry) {
            settings.repos.push(entry.clone());
        }
        let ix = match settings.open_tabs.iter().position(|e| *e == entry) {
            Some(ix) => ix,
            None => {
                settings.open_tabs.push(entry);
                settings.open_tabs.len() - 1
            }
        };
        settings.active_tab = ix + 1;
    }

    // git、ssh 要密码时交给界面弹窗
    let prompts = prompt::init();

    // 组件库的图标（勾选框的勾、通知的图标和关闭按钮等）要注册了资源才显示
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            // Ctrl+Shift+F：切换 GPUI 的帧耗时浮层（隐藏 → 简要 → 详细）
            cx.bind_keys([
                KeyBinding::new("ctrl-shift-f", ToggleFrameOverlay, None),
                KeyBinding::new("f5", Refresh, None),
                KeyBinding::new("ctrl-w", CloseTab, None),
                KeyBinding::new("ctrl-tab", NextTab, None),
                KeyBinding::new("ctrl-shift-tab", PrevTab, None),
                KeyBinding::new("ctrl-f", FindCommit, None),
                KeyBinding::new("ctrl-p", QuickOpen, None),
            ]);
            workspace_action::<Refresh>(cx, |ws, _, cx| ws.refresh_active(cx));
            workspace_action::<CloseTab>(cx, |ws, _, cx| ws.close_active(cx));
            workspace_action::<NextTab>(cx, |ws, _, cx| ws.cycle_tab(1, cx));
            workspace_action::<PrevTab>(cx, |ws, _, cx| ws.cycle_tab(-1, cx));
            workspace_action::<FindCommit>(cx, |ws, window, cx| ws.find_commit(window, cx));
            workspace_action::<QuickOpen>(cx, |ws, window, cx| ws.quick_open(window, cx));
            cx.on_action(|_: &ToggleFrameOverlay, cx: &mut App| {
                for handle in cx.windows() {
                    handle
                        .update(cx, |_, window, _| window.cycle_debug_frame_overlay_mode())
                        .ok();
                }
            });

            let (theme, saved_window) = (settings.theme, settings.window);
            let app = cx.new(|_| AppState::new(settings));
            let options = WindowOptions {
                window_bounds: Some(appearance::initial_bounds(saved_window, cx)),
                titlebar: Some(TitlebarOptions {
                    title: Some("fork-remote".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                if std::env::var("FR_M0_OVERLAY").is_ok_and(|v| v == "1") {
                    window.set_debug_frame_overlay_mode(DebugFrameOverlayMode::Minimal);
                }
                appearance::apply_theme(theme, window, cx);
                let workspace = cx.new(|cx| Workspace::new(app, prompts, window, cx));
                cx.set_global(MainWorkspace(workspace.downgrade(), window.window_handle()));
                workspace
            })
            .expect("打开窗口失败");
            cx.activate(true);
        });
}
