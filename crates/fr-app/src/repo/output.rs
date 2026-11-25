//! 底部的「输出」：获取、拉取、推送这类操作的完整输出，包括 `remote:` 开头的远程输出
//! （服务器上 hook 的说明、创建合并请求的链接），执行中实时更新。
//!
//! 打开的地方：工具栏上的进度（执行中点它）、通知里的「查看输出」、推送菜单的「查看上次推送的
//! 输出」。显示的是活动记录里那个操作的几条（见 `activity.rs`），这里只记是哪几条。

use std::collections::VecDeque;

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, WindowExt as _,
    h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use super::activity::{Entry, Outcome};
use crate::ui;

/// 做完的操作留几个可以再打开
const RECENT: usize = 20;

#[derive(Default)]
pub(super) struct Output {
    /// 底部的抽屉开着
    pub(super) open: bool,
    /// 指定看哪个操作；没指定看最近的（正在执行的，没有就是刚做完的）
    pinned: Option<Op>,
    /// 正在执行的操作
    current: Option<Op>,
    /// 做完的操作，新的在前
    recent: VecDeque<Op>,
    /// 只看远程的输出
    remote_only: bool,
    scroll: ScrollHandle,
}

/// 一个操作：名字和它每一步在活动记录里的编号。
#[derive(Clone, PartialEq, Debug)]
pub(super) struct Op {
    label: SharedString,
    ids: Vec<u64>,
    /// 有联网的一步（获取、拉取、推送……）
    net: bool,
    /// 是推送（「查看上次推送的输出」找它）
    push: bool,
}

impl Output {
    /// 开始一个操作。开着的抽屉跟着看它。
    pub(super) fn start(&mut self, label: SharedString, net: bool, push: bool) {
        self.current = Some(Op {
            label,
            ids: Vec::new(),
            net,
            push,
        });
        self.pinned = None;
    }

    /// 操作的一步开始了，`id` 是它在活动记录里的编号。
    pub(super) fn step(&mut self, id: u64) {
        if let Some(op) = &mut self.current {
            op.ids.push(id);
        }
    }

    /// 操作做完了：留在最近的里面（一步都没开始的不留）。
    pub(super) fn finish(&mut self) {
        let Some(op) = self.current.take() else {
            return;
        };
        if !op.ids.is_empty() {
            self.recent.push_front(op);
            self.recent.truncate(RECENT);
        }
    }

    fn shown(&self) -> Option<&Op> {
        self.pinned
            .as_ref()
            .or(self.current.as_ref())
            .or(self.recent.front())
    }

    /// 最近一次推送。
    pub(super) fn last_push(&self) -> Option<Op> {
        self.recent.iter().find(|op| op.push).cloned()
    }
}

/// 远程说的话：`remote:` 开头的行。
fn remote_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .filter(|line| line.trim_start().starts_with("remote:"))
}

/// git 和常见的服务器每次都说的进度（GitHub 的 Resolving deltas、Gitea 的 Processing），不算
/// 远程「说了话」。
const ROUTINE: [&str; 8] = [
    "Enumerating objects",
    "Counting objects",
    "Compressing objects",
    "Total ",
    "Resolving deltas",
    ". Processing",
    "Processing ",
    "Processed ",
];

/// 远程有没有说进度以外的话（hook 的说明、合并请求的链接）：有就在成功的通知里给「查看输出」。
fn remote_said(text: &str) -> bool {
    remote_lines(text).any(|line| {
        let said = line.trim_start().trim_start_matches("remote:").trim();
        !said.is_empty() && !ROUTINE.iter().any(|routine| said.starts_with(routine))
    })
}

impl RepoView {
    /// 操作开始时（`run_steps`）调。
    pub(super) fn output_start(&mut self, label: SharedString, net: bool, push: bool) {
        self.output.start(label, net, push);
        self.output.scroll.scroll_to_bottom();
    }

    /// 打开底部的输出：`op` 是要看的操作，None 看最近的。
    pub(super) fn open_output(&mut self, op: Option<Op>, cx: &mut Context<Self>) {
        self.output.pinned = op;
        self.output.open = true;
        self.output.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 工具栏上的进度点了：开着就关，关着就打开看正在执行的。
    pub(super) fn toggle_output(&mut self, cx: &mut Context<Self>) {
        match self.output.open {
            true => {
                self.output.open = false;
                cx.notify();
            }
            false => self.open_output(None, cx),
        }
    }

    /// 底部的输出占多高（开着时）；关着是 0。只在 Git 页上有。
    pub(super) fn output_height(&self) -> f32 {
        if !self.output.open || self.page != super::Page::Git {
            return 0.;
        }
        (f32::from(self.viewport.height) * 0.3).clamp(140., 300.)
    }

    /// 刚做完的那个操作联过网的话，打开底部输出看它的回调（失败的通知里「查看输出」用）。
    pub(super) fn output_opener(&self, cx: &Context<Self>) -> Option<super::ops::OpenOutput> {
        let op = self.output.recent.front().filter(|op| op.net).cloned()?;
        let view = cx.entity().downgrade();
        Some(std::rc::Rc::new(move |_: &mut Window, cx: &mut App| {
            view.update(cx, |this, cx| this.open_output(Some(op.clone()), cx))
                .ok();
        }))
    }

    /// 联网的操作成功了：弹通知；远程说了话（hook 的说明、合并请求的链接）时带「查看输出」，点了
    /// 打开底部的输出、收起这条通知。
    pub(super) fn notify_net(
        &self,
        note: Notification,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let said = self
            .output
            .recent
            .front()
            .filter(|op| op.net)
            .and_then(|op| self.op_text(op))
            .is_some_and(|text| remote_said(&text));
        let note = match (said, self.output_opener(cx)) {
            (true, Some(open)) => note.action(move |_, _, cx| {
                let (open, me) = (open.clone(), cx.entity().downgrade());
                Button::new("view-output")
                    .small()
                    .label(tr("查看输出"))
                    .on_click(move |_, window, cx| {
                        open(window, cx);
                        me.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                    })
            }),
            _ => note,
        };
        window.push_notification(note, cx);
    }

    /// 操作的全部输出（几步连在一起）；记录都被挤掉了是 None。
    fn op_text(&self, op: &Op) -> Option<String> {
        let entries: Vec<&Entry> = op
            .ids
            .iter()
            .filter_map(|id| self.activity.get(*id))
            .collect();
        (!entries.is_empty()).then(|| {
            entries
                .iter()
                .map(|entry| entry.output.as_ref())
                .collect::<Vec<_>>()
                .join("\n")
        })
    }

    /// 底部的输出；关着时没有。
    pub(super) fn render_output(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.output.open {
            return None;
        }
        let height = self.output_height();
        let theme = cx.theme();
        let (border, muted, bg, panel, success, danger, warning, info) = (
            theme.border,
            theme.muted_foreground,
            theme.background,
            theme.muted,
            theme.success,
            theme.danger,
            theme.warning,
            theme.info,
        );
        let op = self.output.shown().cloned();
        let entries: Vec<Entry> = op
            .iter()
            .flat_map(|op| op.ids.iter())
            .filter_map(|id| self.activity.get(*id).cloned())
            .collect();
        let running = entries.iter().any(|e| e.outcome == Outcome::Running);
        let outcome = if running {
            Outcome::Running
        } else if entries.iter().any(|e| e.outcome == Outcome::Failed) {
            Outcome::Failed
        } else if entries.iter().any(|e| e.outcome == Outcome::Cancelled) {
            Outcome::Cancelled
        } else {
            Outcome::Done
        };
        // 执行中、看着最底下时跟着滚（往上翻了就不动）
        if running {
            let handle = &self.output.scroll;
            let at_bottom = handle.offset().y <= -handle.max_offset().y + px(24.);
            if at_bottom {
                handle.scroll_to_bottom();
            }
        }
        let remote_only = self.output.remote_only;
        let mut copy = String::new();
        let mut blocks: Vec<AnyElement> = Vec::new();
        for entry in &entries {
            let lines: Vec<&str> = match remote_only {
                true => remote_lines(&entry.output).collect(),
                false => entry.output.lines().collect(),
            };
            if remote_only && lines.is_empty() {
                continue;
            }
            copy.push_str(&format!("$ {}\n", entry.command));
            for line in &lines {
                copy.push_str(line);
                copy.push('\n');
            }
            // 远程说的话换个颜色，一眼能从 git 自己的进度里认出来
            let rows = lines.iter().map(|line| {
                let remote = line.trim_start().starts_with("remote:");
                div()
                    .whitespace_normal()
                    .when(remote, |row| row.text_color(info))
                    .child(line.to_string())
            });
            blocks.push(
                v_flex()
                    .child(
                        div()
                            .mb(px(2.))
                            .text_color(muted)
                            .whitespace_normal()
                            .child(format!("$ {}", entry.command)),
                    )
                    .children(rows)
                    .into_any_element(),
            );
        }
        let empty: Option<SharedString> = match (&op, blocks.is_empty()) {
            (None, _) => Some(tr("暂无输出。获取、拉取、推送时的输出将显示在此处。").into()),
            (Some(_), true) if remote_only => Some(tr("远程没有输出。").into()),
            (Some(_), true) => Some(tr("（无输出）").into()),
            _ => None,
        };
        let state: Option<(AnyElement, SharedString)> = op.as_ref().map(|_| match outcome {
            Outcome::Running => (
                Spinner::new()
                    .icon(Icon::new(IconName::LoaderCircle))
                    .with_size(px(13.))
                    .into_any_element(),
                tr("正在执行…").into(),
            ),
            Outcome::Done => (
                ui::icon_small(IconName::CircleCheck)
                    .text_color(success)
                    .into_any_element(),
                tr("完成").into(),
            ),
            Outcome::Failed => (
                ui::icon_small(IconName::CircleX)
                    .text_color(danger)
                    .into_any_element(),
                tr("失败").into(),
            ),
            Outcome::Cancelled => (
                ui::icon_small(IconName::CircleSlash)
                    .text_color(warning)
                    .into_any_element(),
                tr("已取消").into(),
            ),
        });
        let title: SharedString = match &op {
            Some(op) => tf!("输出 · {label}", label = op.label.as_ref()).into(),
            None => tr("输出").into(),
        };
        Some(
            v_flex()
                .id("output-drawer")
                .flex_none()
                .h(px(height))
                .bg(bg)
                .border_t_1()
                .border_color(border)
                .child(
                    h_flex()
                        .flex_none()
                        .h(px(32.))
                        .pl(px(12.))
                        .pr(px(6.))
                        .gap(px(8.))
                        .border_b_1()
                        .border_color(border)
                        .child(
                            ui::icon_small(IconName::SquareTerminal)
                                .flex_none()
                                .text_color(muted),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(12.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title),
                        )
                        .when_some(state, |row, (glyph, text)| {
                            row.child(
                                h_flex()
                                    .flex_none()
                                    .gap(px(4.))
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .child(glyph)
                                    .child(text),
                            )
                        })
                        .child(div().flex_none().w(px(8.)))
                        .child(
                            ButtonGroup::new("output-filter")
                                .outline()
                                .xsmall()
                                .child(
                                    Button::new("output-all")
                                        .label(tr("全部"))
                                        .selected(!remote_only)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.output.remote_only = false;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("output-remote")
                                        .label(tr("仅远程输出"))
                                        .selected(remote_only)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.output.remote_only = true;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            Button::new("output-copy")
                                .xsmall()
                                .ghost()
                                .icon(ui::icon_small(IconName::Copy))
                                .tooltip(tr("复制输出"))
                                .disabled(copy.is_empty())
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                                }),
                        )
                        .child(div().flex_1().min_w(px(8.)))
                        .child(
                            Button::new("output-close")
                                .xsmall()
                                .ghost()
                                .icon(ui::icon_small(IconName::X))
                                .tooltip(tr("关闭"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.output.open = false;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .id("output-text")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.output.scroll)
                        .px(px(12.))
                        .py(px(8.))
                        .bg(panel.opacity(0.35))
                        .font_family(crate::fonts::mono())
                        .text_size(px(12.))
                        .line_height(px(18.))
                        .child(match empty {
                            Some(text) => div().text_color(muted).child(text).into_any_element(),
                            None => v_flex().gap(px(10.)).children(blocks).into_any_element(),
                        }),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Output, remote_lines, remote_said};

    #[test]
    fn 远程说的话() {
        let github = "Enumerating objects: 5, done.\nremote: Resolving deltas: 100% (2/2), completed with 2 local objects.\nTo github.com:a/b.git\n   1234..5678  main -> main";
        assert!(!remote_said(github));
        let new_branch = "remote: \nremote: Create a pull request for 'x' on GitHub by visiting:\nremote:      https://github.com/a/b/pull/new/x\nremote: ";
        assert!(remote_said(new_branch));
        assert_eq!(remote_lines(new_branch).count(), 4);
        let gitea = "remote: . Processing 1 references\nremote: Processed 1 references in total";
        assert!(!remote_said(gitea));
        let hook = "remote: error: GH006: Protected branch update failed for refs/heads/main.";
        assert!(remote_said(hook));
    }

    #[test]
    fn 操作的记录() {
        let mut output = Output::default();
        output.start("推送".into(), true, true);
        output.step(3);
        assert_eq!(output.shown().map(|op| op.ids.clone()), Some(vec![3]));
        output.finish();
        output.start("提交".into(), false, false);
        output.finish();
        // 一步都没开始的不留
        output.start("x".into(), false, false);
        output.finish();
        assert_eq!(output.recent.len(), 1);
        assert_eq!(output.last_push().map(|op| op.ids), Some(vec![3]));
    }
}
