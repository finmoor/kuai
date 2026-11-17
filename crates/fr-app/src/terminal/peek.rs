//! 只看一个开在 kuaid 里的终端：终端总览页右边的预览。
//!
//! 和 [`super::TerminalView`] 的区别：用「只看」的接法接上（`term_attach` 带 `watch`），不改那个终端的
//! 大小，按它自己的行列数画，地方不够就把字缩小；不收键盘鼠标，也不往里写任何东西——仿真器要回答
//! 程序的话（问光标位置、终端型号）一律丢掉，那些由正经接着它的那一头或者 kuaid 回答。
//! 画法是同一套（[`super::snapshot`]、[`super::paint_frame`]）。

use std::sync::Arc;

use alacritty_terminal::event::Event as TermEvent;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use fr_channel::Connection;
use fr_core::i18n::tr;
use fr_core::tf;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::colors::Palette;
use super::{Feed, Frame, Grid, Proxy, paint_frame, prefs, pump, snapshot};

/// 字和面板边缘之间留的空
const PADDING: f32 = 12.;
/// 字最小缩到这么大，再小看不清；还放不下就只显示左上角那一块
const MIN_FONT: f32 = 8.;

/// 接得怎么样了。
enum State {
    /// 在看
    Live,
    /// 里面的程序结束了：退出码
    Exited(Option<i32>),
    /// 和那台电脑的连接断了
    Lost(String),
}

pub struct TermPeek {
    term: Arc<FairMutex<Term<Proxy>>>,
    grid: Grid,
    state: State,
    _pump: Task<()>,
    _events: Task<()>,
}

impl TermPeek {
    /// 看 `conn` 那台电脑上编号是 `term` 的终端，它现在是 `cols` 列 `rows` 行。
    pub fn new(
        conn: &Arc<Connection>,
        term: &str,
        cols: u16,
        rows: u16,
        cx: &mut Context<Self>,
    ) -> Self {
        let grid = Grid {
            cols: usize::from(cols).max(2),
            rows: usize::from(rows).max(1),
        };
        let (tx, mut rx) = mpsc::unbounded();
        let emulator = Term::new(prefs::config(&prefs::get(cx)), &grid, Proxy(tx.clone()));
        let emulator = Arc::new(FairMutex::new(emulator));
        let stream = conn.term_view(term);
        let executor = cx.background_executor().clone();
        let pump = cx
            .background_executor()
            .spawn(pump(stream, emulator.clone(), tx, executor));
        let events = cx.spawn(async move |this, cx| {
            while let Some(feed) = rx.next().await {
                if this.update(cx, |this, cx| this.on_feed(feed, cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            term: emulator,
            grid,
            state: State::Live,
            _pump: pump,
            _events: events,
        }
    }

    /// 这个预览是按几列几行建的（那个终端改了大小就得重建）。
    pub fn size(&self) -> (u16, u16) {
        (self.grid.cols as u16, self.grid.rows as u16)
    }

    fn on_feed(&mut self, feed: Feed, cx: &mut Context<Self>) {
        match feed {
            // 有新内容：重画。仿真器别的事（要回答程序、改标题、动剪贴板）都不管
            Feed::Term(TermEvent::Wakeup) => {}
            Feed::Term(_) | Feed::Opened(_) => return,
            Feed::Exited(code) => self.state = State::Exited(code),
            Feed::Lost(reason) => self.state = State::Lost(reason),
        }
        cx.notify();
    }

    /// 底下那一行说明：平时没有，程序结束、连接断开时说一声。
    fn notice(&self) -> Option<SharedString> {
        match &self.state {
            State::Live => None,
            State::Exited(Some(code)) => {
                Some(tf!("程序已结束（退出码 {code}）", code = code.to_string()).into())
            }
            State::Exited(None) => Some(tr("程序已结束").into()),
            State::Lost(reason) if reason.is_empty() => Some(tr("连接已断开").into()),
            State::Lost(reason) => Some(tf!("连接已断开：{reason}", reason).into()),
        }
    }

    fn frame(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &App) -> Option<Frame> {
        let prefs = prefs::get(cx);
        let base_font = prefs::base_font(&prefs);
        let font_id = window.text_system().resolve_font(&base_font);
        let advance = |size: Pixels| {
            window
                .text_system()
                .advance(font_id, size, 'm')
                .map(|advance| advance.width)
                .unwrap_or(size * 0.55)
        };
        // 按设置里的字号排一遍，放不下就按比例把字缩小
        let full = size(advance(px(prefs.font_size)), px(prefs.cell_height()));
        let (cols, rows) = (self.grid.cols as f32, self.grid.rows as f32);
        let fit = (bounds.size.width / (full.width * cols))
            .min(bounds.size.height / (full.height * rows))
            .min(1.);
        let font_size = px((prefs.font_size * fit).max(MIN_FONT));
        let scale = font_size / px(prefs.font_size);
        let cell = size(advance(font_size), px(prefs.cell_height() * scale));
        if bounds.size.width < cell.width * 4. || bounds.size.height < cell.height {
            return None;
        }
        let theme = cx.theme();
        let palette = Palette::new(theme.foreground, theme.background, theme.is_dark());
        let (rects, shapes, runs, cursor) = snapshot(&self.term.lock(), &palette, theme.selection);
        Some(Frame {
            origin: bounds.origin,
            cell,
            rects,
            shapes,
            runs,
            cursor: cursor.filter(|_| matches!(self.state, State::Live)),
            foreground: palette.foreground,
            background: palette.background,
            // 没有焦点：光标画成空心的
            focused: false,
            marked: None,
            font: base_font,
            font_size,
        })
    }
}

impl Render for TermPeek {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (background, muted, bar) = (theme.background, theme.muted_foreground, theme.secondary);
        let view = cx.entity();
        div()
            .relative()
            .size_full()
            .p(px(PADDING))
            .bg(background)
            .overflow_hidden()
            .child(
                canvas(
                    move |bounds, window, cx| view.read(cx).frame(bounds, window, cx),
                    |bounds, frame, window, cx| {
                        if let Some(frame) = frame {
                            paint_frame(&frame, bounds, window, cx);
                        }
                    },
                )
                .size_full(),
            )
            .when_some(self.notice(), |this, notice| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .px(px(PADDING))
                        .py_1()
                        .bg(bar)
                        .text_xs()
                        .text_color(muted)
                        .child(notice),
                )
            })
    }
}
