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
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use crate::ui;

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

impl RepoView {
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
