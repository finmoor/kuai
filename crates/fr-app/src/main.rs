mod graph_paint;
mod repo_view;

use std::path::PathBuf;

use gpui_kit::*;

gpui_kit::actions!(fr_m0, [ToggleFrameOverlay]);

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
    if std::env::var_os("FR_LOG").is_some() {
        let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info));
    }

    // 第一个参数是仓库路径；不给就打开 tools/gen-test-repo.py 生成的测试仓库
    let repo = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".testdata/big-repo"));

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        // Ctrl+Shift+F：切换 GPUI 的帧耗时浮层（隐藏 → 简要 → 详细）
        cx.bind_keys([KeyBinding::new("ctrl-shift-f", ToggleFrameOverlay, None)]);
        cx.on_action(|_: &ToggleFrameOverlay, cx: &mut App| {
            for handle in cx.windows() {
                handle
                    .update(cx, |_, window, _| window.cycle_debug_frame_overlay_mode())
                    .ok();
            }
        });

        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("fork-remote · M0 技术验证".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            if std::env::var("FR_M0_OVERLAY").is_ok_and(|v| v == "1") {
                window.set_debug_frame_overlay_mode(DebugFrameOverlayMode::Minimal);
            }
            cx.new(|cx| repo_view::RepoView::new(repo, window, cx))
        })
        .expect("打开窗口失败");
        cx.activate(true);
    });
}
