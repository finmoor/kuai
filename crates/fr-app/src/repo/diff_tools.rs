//! 看改动时三处（本地修改、提交详情、比较）共用的东西：忽略空白。
//!
//! 「忽略空白」是每个仓库标签页自己的开关，不存盘：它会让一部分改动不显示，
//! 下次打开时默认看到的应该是全部改动。开着的时候改动是用 `git diff -w` 取的，
//! 另外再取一份不带 `-w` 的，只拿来数少显示了多少行，写在改动上面。
//! `-w` 取出来的补丁和文件对不上，所以开着时不能按块、按行暂存。

use fr_channel::{Channel, GitOutput};
use fr_core::i18n::tr;
use fr_core::{patch, tf};
use futures::future::BoxFuture;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::DiffFile;
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use crate::ui;

/// 解码、解析一份补丁。在后台调：整个文件、超长的行解析要好一会儿，不能占着界面线程。返回全文
/// 和解析出来的文件，解析失败的是给用户看的原因。
pub(super) fn parse_patch(bytes: &[u8]) -> (String, Result<Vec<DiffFile>, SharedString>) {
    let text = crate::text::expand_patch_tabs(&crate::text::decode(bytes)).into_owned();
    let files = DiffFile::parse(&text)
        .map_err(|err| tf!("diff 解析失败：{message}", message = err.message()).into());
    (text, files)
}

/// 不忽略空白时的那份补丁的请求；没开「忽略空白」时没有。
pub(super) type Plain = Option<BoxFuture<'static, anyhow::Result<GitOutput>>>;

/// 忽略空白时不能按块、按行操作，按钮上的提示。
pub(super) fn ws_blocked() -> &'static str {
    tr("已忽略空白，无法按块或按行操作")
}

/// 等不忽略空白的那份读回来。读不出来就当没有：只是少写一句话。
pub(super) async fn plain_out(plain: Plain) -> Option<Vec<u8>> {
    match plain {
        Some(request) => request.await.ok().map(|out| out.stdout),
        None => None,
    }
}

/// 改动上面写的那句话：忽略空白以后少显示了多少行。`plain` 是不忽略时的补丁。
pub(super) fn ws_note(plain: &[u8], ignored: &[u8]) -> SharedString {
    let (added, removed) = hidden_lines(plain, ignored);
    match added + removed {
        0 => tr("没有仅空白不同的改动").into(),
        _ => tf!(
            "已忽略仅空白不同的改动（+{added} −{removed}）",
            added,
            removed
        )
        .into(),
    }
}

/// 忽略空白以后少显示的增加、删除行数。
fn hidden_lines(plain: &[u8], ignored: &[u8]) -> (usize, usize) {
    let (all_added, all_removed) = patch::count_changed(plain);
    let (added, removed) = patch::count_changed(ignored);
    (
        all_added.saturating_sub(added),
        all_removed.saturating_sub(removed),
    )
}

/// 改动上面的一行说明：一个图标一句话。
pub(super) fn info_strip(id: &'static str, text: SharedString, cx: &App) -> AnyElement {
    let theme = cx.theme();
    h_flex()
        .h(px(26.))
        .flex_none()
        .px_3()
        .gap_2()
        .bg(theme.secondary)
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(ui::icon_small(IconName::Info).flex_none())
        .child(ui::clipped(id, text).flex_1())
        .into_any_element()
}

/// 折行的并排对比在哪一处（各记各上一次画出来多宽，见 [`RepoView::rewrap_guard`]）。
#[derive(Clone, Copy)]
pub(super) enum SplitAt {
    Changes = 0,
    History = 1,
    Compare = 2,
}

impl RepoView {
    /// 并排的设置开着时，这么宽的改动区真的并排（true），还是暂以上下排显示（false）：两侧各占一半，
    /// 不足 [`super::layout::SPLIT_MIN`] 时每侧放不下几个字。点过「仍然并排」的照并排。
    pub(super) fn split_fits(&self, width: f32) -> bool {
        self.split_forced || width >= super::layout::SPLIT_MIN
    }

    /// 提交详情、比较页右边的改动区多宽（左边是侧栏和文件清单）。
    pub(super) fn details_diff_width(&self, cx: &App) -> f32 {
        let sizes = self.sizes(cx);
        f32::from(self.viewport.width) - sizes.sidebar - sizes.details_width
    }

    /// 垫在折行的并排对比上的一层，只量宽度：和上一次画出来的不一样就再画一帧。
    ///
    /// 组件按它上一次画出来的宽度折行，宽度要到画的时候才知道；它在画的过程中发的重画通知不起作用
    /// （gpui 在画的过程中不安排下一帧）。平时拖窗口、动鼠标会接着画，看不出来；从上下排换回并排
    /// （点「仍然并排」、点「并排」）只画一帧，不补的话这一帧是按旧宽度折的，一侧被挤出画面，
    /// 要等鼠标动一下才对。
    pub(super) fn rewrap_guard(&self, at: SplitAt) -> AnyElement {
        let seen = self.split_seen[at as usize].clone();
        canvas(
            move |bounds, window, _| {
                let width = f32::from(bounds.size.width);
                if (seen.replace(width) - width).abs() >= 1. {
                    window.request_animation_frame();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
        .into_any_element()
    }

    /// 并排开着、改动区却不够宽时改动上面的那一行：说明为什么是上下排，给「仍然并排」。
    pub(super) fn narrow_split_strip(
        &self,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        h_flex()
            .h(px(28.))
            .flex_none()
            .px_3()
            .gap_2()
            .bg(theme.warning.opacity(0.1))
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .child(
                ui::icon_small(IconName::Info)
                    .flex_none()
                    .text_color(theme.warning),
            )
            .child(
                ui::clipped(id, tr("改动区不够宽，暂以上下排显示；加宽窗口后恢复并排。")).flex_1(),
            )
            .child(
                Button::new((id, 1usize))
                    .xsmall()
                    .label(tr("仍然并排"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.split_forced = true;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// 「忽略空白」开着时把参数改成带 `-w` 的，并另外发一个不带的请求（数少显示了多少行用）。
    pub(super) fn ws_plain(&self, channel: &Channel, args: &mut Vec<String>) -> Plain {
        if !self.ignore_ws {
            return None;
        }
        let plain = channel.git(&self.entry.path, args.clone());
        patch::ignore_whitespace(args);
        Some(plain)
    }

    /// 切换「忽略空白」，三处正在看的改动都重读。
    pub(super) fn toggle_ignore_ws(&mut self, cx: &mut Context<Self>) {
        self.ignore_ws = !self.ignore_ws;
        self.load_file_diff(cx);
        self.reload_details(cx);
        self.load_compare_diff(cx);
        cx.notify();
    }

    /// 改动上方「忽略空白」的开关。
    pub(super) fn ws_toggle(&self, id: &'static str, cx: &mut Context<Self>) -> Button {
        Button::new(id)
            .xsmall()
            .ghost()
            .icon(ui::icon(IconName::Pilcrow))
            .selected(self.ignore_ws)
            .tooltip(tr("忽略空白：不显示仅缩进或行尾空格不同的改动"))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_ignore_ws(cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::hidden_lines;

    #[test]
    fn 数忽略空白以后少显示的行() {
        let plain = b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,4 @@\n-fn a() {\n-  x\n+fn a()  {\n+    x\n+    y\n }\n";
        let ignored =
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,4 @@\n fn a() {\n   x\n+    y\n }\n";
        assert_eq!(hidden_lines(plain, ignored), (2, 2));
        assert_eq!(hidden_lines(plain, plain), (0, 0));
        // 忽略以后一处改动都不剩
        assert_eq!(hidden_lines(plain, b""), (3, 2));
    }
}
