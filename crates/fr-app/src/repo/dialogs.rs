//! 对话框：新建分支、贮藏。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{Mode, RepoView};

impl RepoView {
    /// 新建分支：在提交历史里选了提交就从它创建，否则从当前 HEAD 创建。
    pub(super) fn new_branch_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let start = (self.mode == Mode::History)
            .then(|| self.history.selected_commit())
            .flatten()
            .map(|c| (c.id.to_string(), c.subject.to_string()));
        let name =
            cx.new(|cx| InputState::new(window, cx).placeholder("分支名，比如 feature/login"));
        let switch = Rc::new(Cell::new(true));
        let view = cx.entity().downgrade();
        let first_build = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                name.focus_handle(cx).focus(window, cx);
            }
            let muted = cx.theme().muted_foreground;
            let from: SharedString = match &start {
                Some((id, subject)) => {
                    format!("从选中的提交 {} {subject} 创建", &id[..id.len().min(8)]).into()
                }
                None => "从当前 HEAD 创建".into(),
            };
            let submit = {
                let (name, switch, view, start) =
                    (name.clone(), switch.clone(), view.clone(), start.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let branch = name.read(cx).value().trim().to_string();
                    if branch.is_empty() {
                        window.push_notification(Notification::warning("填写分支名"), cx);
                        return;
                    }
                    let mut args: Vec<String> = if switch.get() {
                        vec!["checkout".into(), "-q".into(), "-b".into()]
                    } else {
                        vec!["branch".into()]
                    };
                    args.push(branch.clone());
                    args.extend(start.as_ref().map(|(id, _)| id.clone()));
                    window.close_dialog(cx);
                    view.update(cx, |this, cx| {
                        this.run_op(
                            "新建分支",
                            args,
                            None,
                            window,
                            cx,
                            move |_, _, window, cx| {
                                window.push_notification(
                                    Notification::success(format!("已创建分支 {branch}")),
                                    cx,
                                );
                            },
                        );
                    })
                    .ok();
                }
            };
            dialog
                .title("新建分支")
                .w(px(520.))
                .child(
                    v_flex()
                        .gap_3()
                        .child(Input::new(&name))
                        .child(
                            Checkbox::new("switch")
                                .label("创建后切换过去")
                                .checked(switch.get())
                                .on_click({
                                    let switch = switch.clone();
                                    move |checked, window, _| {
                                        switch.set(*checked);
                                        window.refresh();
                                    }
                                }),
                        )
                        .child(div().text_xs().text_color(muted).child(from)),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(Button::new("ok").primary().label("创建").on_click(submit)),
                )
        });
    }

    /// 贮藏当前的改动。
    pub(super) fn stash_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = cx.new(|cx| InputState::new(window, cx).placeholder("说明，可以不填"));
        let untracked = Rc::new(Cell::new(false));
        let view = cx.entity().downgrade();
        let first_build = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                message.focus_handle(cx).focus(window, cx);
            }
            let muted = cx.theme().muted_foreground;
            let submit = {
                let (message, untracked, view) = (message.clone(), untracked.clone(), view.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let text = message.read(cx).value().trim().to_string();
                    let mut args: Vec<String> = vec!["stash".into(), "push".into()];
                    if untracked.get() {
                        args.push("--include-untracked".into());
                    }
                    if !text.is_empty() {
                        args.push("-m".into());
                        args.push(text);
                    }
                    window.close_dialog(cx);
                    view.update(cx, |this, cx| {
                        this.run_op("贮藏", args, None, window, cx, |_, _, _, _| {});
                    })
                    .ok();
                }
            };
            dialog
                .title("贮藏改动")
                .w(px(520.))
                .child(
                    v_flex()
                        .gap_3()
                        .child(Input::new(&message))
                        .child(
                            Checkbox::new("untracked")
                                .label("包括未跟踪的文件")
                                .checked(untracked.get())
                                .on_click({
                                    let untracked = untracked.clone();
                                    move |checked, window, _| {
                                        untracked.set(*checked);
                                        window.refresh();
                                    }
                                }),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child("工作区和暂存区的改动收起来，恢复成干净的状态；以后从侧栏的「贮藏」里恢复。"),
                        ),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(Button::new("ok").primary().label("贮藏").on_click(submit)),
                )
        });
    }
}
