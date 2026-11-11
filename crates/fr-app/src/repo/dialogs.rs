//! 对话框：通用的「填一两个名字、勾几个选项」对话框，以及用它做的新建分支、贮藏。
//!
//! 点确定时先校验，有问题就把原因写在对话框里，对话框不关、填的内容还在；校验过了才关掉去执行。

use fr_core::i18n::tr;
use fr_core::tf;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use fr_core::status::Kind;
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

/// 点确定时的校验：返回要显示给用户的问题；返回 `None` 才算通过。
pub(super) type Validate = Box<dyn Fn(&RepoView, &Answer) -> Option<String>>;

/// 通用对话框的内容：一个或两个输入框，下面可以有几个勾选项和一行说明。
#[derive(Default)]
pub(super) struct Ask {
    pub title: SharedString,
    pub placeholder: SharedString,
    /// 输入框里预先填好的内容
    pub value: String,
    /// 第二个输入框的占位文字；`None` 时只有一个输入框
    pub second: Option<SharedString>,
    pub hint: SharedString,
    pub ok: &'static str,
    /// 勾选项：文字和默认是否勾上
    pub checks: Vec<(&'static str, bool)>,
    /// 第一个输入框可以不填
    pub allow_empty: bool,
    pub validate: Option<Validate>,
}

/// 用户在对话框里填的内容（去掉了首尾空白）和各勾选项的状态。
pub(super) struct Answer {
    pub text: String,
    pub second: String,
    pub checks: Vec<bool>,
}

/// 分支名、标签名合不合 git 的规则；不合时返回给用户看的说明。
pub(super) fn ref_name_error(name: &str) -> Option<String> {
    let bad_char = name
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c));
    let bad_shape = name.starts_with(['-', '/', '.'])
        || name.ends_with(['/', '.'])
        || name.ends_with(".lock")
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || name == "@";
    (bad_char || bad_shape).then(|| {
        tr("名字不合规则：不能有空格和 ~ ^ : ? * [ \\，不能有连着的两个点或两个斜杠，不能以 - / . 开头，也不能以 / 或 . 结尾。").to_string()
    })
}

impl RepoView {
    /// 已经有同名的本地分支时返回说明。
    pub(super) fn branch_exists_error(&self, name: &str) -> Option<String> {
        self.refs
            .branches
            .iter()
            .any(|b| b.name == name)
            .then(|| tf!("已经有叫 {name} 的分支了", name))
    }

    /// 弹出通用对话框。点确定（或在输入框里按回车）并且通过校验时关掉对话框、调用 `on_ok`。
    pub(super) fn ask(
        &mut self,
        ask: Ask,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_ok: impl Fn(&mut Self, Answer, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let Ask {
            title,
            placeholder,
            value,
            second,
            hint,
            ok,
            checks,
            allow_empty,
            validate,
        } = ask;
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        if !value.is_empty() {
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        let second = second
            .map(|placeholder| cx.new(|cx| InputState::new(window, cx).placeholder(placeholder)));
        let states: Rc<Vec<Cell<bool>>> =
            Rc::new(checks.iter().map(|(_, on)| Cell::new(*on)).collect());
        // 校验没过的原因，显示在对话框里
        let problem: Rc<RefCell<Option<SharedString>>> = Rc::default();
        let view = cx.entity().downgrade();
        let submit: Submit = {
            let (input, second, states, problem) = (
                input.clone(),
                second.clone(),
                states.clone(),
                problem.clone(),
            );
            Rc::new(move |window, cx| {
                let answer = Answer {
                    text: input.read(cx).value().trim().to_string(),
                    second: second
                        .as_ref()
                        .map(|s| s.read(cx).value().trim().to_string())
                        .unwrap_or_default(),
                    checks: states.iter().map(Cell::get).collect(),
                };
                let found = if answer.text.is_empty() && !allow_empty {
                    Some(tr("还没有填内容").to_string())
                } else {
                    validate.as_ref().and_then(|validate| {
                        view.upgrade()
                            .and_then(|view| validate(view.read(cx), &answer))
                    })
                };
                if let Some(found) = found {
                    *problem.borrow_mut() = Some(found.into());
                    window.refresh();
                    return;
                }
                window.close_dialog(cx);
                view.update(cx, |this, cx| on_ok(this, answer, window, cx))
                    .ok();
            })
        };
        let first_build = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                input.focus_handle(cx).focus(window, cx);
            }
            let (muted, danger) = (cx.theme().muted_foreground, cx.theme().danger);
            let mut body = v_flex().gap_3().child(Input::new(&input));
            if let Some(second) = &second {
                body = body.child(Input::new(second));
            }
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
            if let Some(found) = problem.borrow().clone() {
                body = body.child(div().text_sm().text_color(danger).child(found));
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
                                .label(tr("取消"))
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
        let hint = match &start {
            Some((id, subject)) => {
                tf!(
                    "从选中的提交 {id} {subject} 创建",
                    id = &id[..id.len().min(8)],
                    subject
                )
            }
            None => tr("从当前 HEAD 创建").into(),
        };
        let ask = Ask {
            title: tr("新建分支").into(),
            placeholder: tr("分支名，比如 feature/login").into(),
            hint: hint.into(),
            ok: tr("创建"),
            checks: vec![(tr("创建后切换过去"), true)],
            validate: Some(Box::new(|this, answer| {
                ref_name_error(&answer.text).or_else(|| this.branch_exists_error(&answer.text))
            })),
            ..Default::default()
        };
        self.ask(ask, window, cx, move |this, answer, window, cx| {
            let branch = answer.text;
            let switch = answer.checks.first().copied().unwrap_or(true);
            let mut args: Vec<String> = if switch {
                vec!["checkout".into(), "-q".into(), "-b".into()]
            } else {
                vec!["branch".into()]
            };
            args.push(branch.clone());
            args.extend(start.as_ref().map(|(id, _)| id.clone()));
            this.run_op(
                tr("新建分支"),
                args,
                None,
                window,
                cx,
                move |_, _, window, cx| {
                    let text = if switch {
                        tf!("已创建分支 {branch}，并切换过去", branch)
                    } else {
                        tf!("已创建分支 {branch}", branch)
                    };
                    window.push_notification(Notification::success(text), cx);
                },
            );
        });
    }

    /// 贮藏当前的改动。
    pub(super) fn stash_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ask = Ask {
            title: tr("贮藏改动").into(),
            placeholder: tr("说明，可以不填").into(),
            hint: tr("工作区和暂存区的改动收起来，恢复成干净的状态；以后从侧栏的「贮藏」里恢复。")
                .into(),
            ok: tr("贮藏"),
            checks: vec![(tr("包括未跟踪的文件"), false)],
            allow_empty: true,
            validate: Some(Box::new(|this, answer| {
                let entries = this.status.as_ref().map(|s| s.entries.as_slice())?;
                let untracked = answer.checks.first().copied().unwrap_or(false);
                if entries.is_empty() {
                    Some(tr("工作区是干净的，没有可以贮藏的改动").into())
                } else if !untracked
                    && entries
                        .iter()
                        .all(|e| e.unstaged == Some(Kind::Untracked) && e.staged.is_none())
                {
                    Some(tr("现在只有未跟踪的文件：勾上「包括未跟踪的文件」才能贮藏它们").into())
                } else {
                    None
                }
            })),
            ..Default::default()
        };
        self.ask(ask, window, cx, |this, answer, window, cx| {
            let mut args: Vec<String> = vec!["stash".into(), "push".into()];
            if answer.checks.first().copied().unwrap_or(false) {
                args.push("--include-untracked".into());
            }
            if !answer.text.is_empty() {
                args.push("-m".into());
                args.push(answer.text);
            }
            this.run_op(
                tr("贮藏"),
                args,
                None,
                window,
                cx,
                |_, out, window, cx| {
                    // 没有改动时 git 也是成功退出，只在输出里说一句
                    let said = String::from_utf8_lossy(&out.stdout);
                    let note = if said.contains("No local changes") {
                        Notification::warning(tr("没有可以贮藏的改动"))
                    } else {
                        Notification::success(tr("改动已经贮藏，工作区恢复干净了"))
                    };
                    window.push_notification(note, cx);
                },
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::ref_name_error;

    #[test]
    fn 分支名的规则() {
        for good in ["main", "feature/login", "fix-1.2", "发布/一月"] {
            assert!(ref_name_error(good).is_none(), "{good}");
        }
        for bad in [
            "a b", "a..b", "-x", "/x", "x/", "x.", "x.lock", "a//b", "a~1", "a^", "a:b", "a?",
            "a*", "a[", "a\\b", "@", "a@{1}", ".hidden",
        ] {
            assert!(ref_name_error(bad).is_some(), "{bad}");
        }
    }
}
