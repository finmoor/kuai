//! 对话框：新建分支、贮藏，以及通用的「填一个名字」对话框。

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

/// 对话框里「确定」要做的事，按钮和回车共用。
type Submit = Rc<dyn Fn(&mut Window, &mut App)>;

/// 通用对话框的内容：一个输入框，下面可以有几个勾选项和一行说明。
pub(super) struct Ask {
    pub title: SharedString,
    pub placeholder: SharedString,
    /// 输入框里预先填好的内容
    pub value: String,
    pub hint: SharedString,
    pub ok: &'static str,
    /// 勾选项：文字和默认是否勾上
    pub checks: Vec<(&'static str, bool)>,
}

impl RepoView {
    /// 弹出通用对话框。点确定（或在输入框里按回车）并且填了内容时调用 `on_ok`，
    /// 参数是填的内容（去掉首尾空白）和各勾选项的状态。
    pub(super) fn ask(
        &mut self,
        ask: Ask,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_ok: impl Fn(&mut Self, String, &[bool], &mut Window, &mut Context<Self>) + 'static,
    ) {
        let Ask {
            title,
            placeholder,
            value,
            hint,
            ok,
            checks,
        } = ask;
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        if !value.is_empty() {
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        let states: Rc<Vec<Cell<bool>>> =
            Rc::new(checks.iter().map(|(_, on)| Cell::new(*on)).collect());
        let view = cx.entity().downgrade();
        let submit: Submit = {
            let (input, states) = (input.clone(), states.clone());
            Rc::new(move |window, cx| {
                let text = input.read(cx).value().trim().to_string();
                if text.is_empty() {
                    window.push_notification(Notification::warning("还没有填内容"), cx);
                    return;
                }
                let checked: Vec<bool> = states.iter().map(Cell::get).collect();
                window.close_dialog(cx);
                view.update(cx, |this, cx| on_ok(this, text, &checked, window, cx))
                    .ok();
            })
        };
        let first_build = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                input.focus_handle(cx).focus(window, cx);
            }
            let muted = cx.theme().muted_foreground;
            let mut body = v_flex().gap_3().child(Input::new(&input));
            for (ix, (label, _)) in checks.iter().enumerate() {
                let states = states.clone();
                body = body.child(
                    Checkbox::new(("ask-check", ix))
                        .label(*label)
                        .checked(states[ix].get())
                        .on_click(move |checked, window, _| {
                            states[ix].set(*checked);
                            window.refresh();
                        }),
                );
            }
            if !hint.is_empty() {
                body = body.child(div().text_xs().text_color(muted).child(hint.clone()));
            }
            let (submit, on_enter) = (submit.clone(), submit.clone());
            dialog
                .title(title.clone())
                .w(px(520.))
                // 回车等于点确定；关不关对话框由 submit 自己决定
                .on_ok(move |_, window, cx| {
                    on_enter(window, cx);
                    false
                })
                .child(body)
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("ok")
                                .primary()
                                .label(ok)
                                .on_click(move |_, window, cx| submit(window, cx)),
                        ),
                )
        });
    }

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
                // 回车等于点确定；关不关对话框由 submit 自己决定
                .on_ok({
                    let submit = submit.clone();
                    move |event, window, cx| {
                        submit(event, window, cx);
                        false
                    }
                })
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
                // 回车等于点确定；关不关对话框由 submit 自己决定
                .on_ok({
                    let submit = submit.clone();
                    move |event, window, cx| {
                        submit(event, window, cx);
                        false
                    }
                })
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
