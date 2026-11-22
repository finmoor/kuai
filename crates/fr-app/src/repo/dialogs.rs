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
use gpui_kit::component::radio::Radio;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::ops::Step;
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
    pub hint: Hint,
    pub ok: &'static str,
    /// 勾选项：文字和默认是否勾上
    pub checks: Vec<(SharedString, bool)>,
    /// 第一个输入框可以不填
    pub allow_empty: bool,
    pub validate: Option<Validate>,
}

/// 对话框最下面那行说明：固定的一句，或者随勾选项变（见 [`ByChecks`]）。
pub(super) enum Hint {
    Text(SharedString),
    ByChecks(ByChecks),
}

/// 按各勾选项现在勾没勾（参数）算出那行说明。
pub(super) type ByChecks = Box<dyn Fn(&[bool]) -> SharedString>;

impl Default for Hint {
    fn default() -> Self {
        Self::Text(SharedString::default())
    }
}

impl From<SharedString> for Hint {
    fn from(text: SharedString) -> Self {
        Self::Text(text)
    }
}

impl From<&'static str> for Hint {
    fn from(text: &'static str) -> Self {
        Self::Text(text.into())
    }
}

impl From<String> for Hint {
    fn from(text: String) -> Self {
        Self::Text(text.into())
    }
}

/// 「几样里选一样」对话框的内容。
#[derive(Default)]
pub(super) struct Choose {
    pub title: SharedString,
    pub description: SharedString,
    /// 单选项：文字，和它下面的一行小字（可以空着）
    pub options: Vec<(SharedString, SharedString)>,
    /// 默认选第几项
    pub selected: usize,
    /// 勾选项：文字和默认是否勾上
    pub checks: Vec<(&'static str, bool)>,
    pub ok: &'static str,
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
        tr("名称无效：不能包含空格、~ ^ : ? * [ \\、连续的两个点或斜杠，不能以 - / . 开头，也不能以 / . 结尾。").to_string()
    })
}

impl RepoView {
    /// 已经有同名的本地分支时返回说明。
    pub(super) fn branch_exists_error(&self, name: &str) -> Option<String> {
        self.refs
            .branches
            .iter()
            .any(|b| b.name == name)
            .then(|| tf!("分支 {name} 已存在", name))
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
                    Some(tr("请填写内容").to_string())
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
                        .label(label.clone())
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
            let hint = match &hint {
                Hint::Text(text) => text.clone(),
                Hint::ByChecks(text) => text(&states.iter().map(Cell::get).collect::<Vec<_>>()),
            };
            if !hint.is_empty() {
                body = body.child(div().text_xs().text_color(muted).child(hint));
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

    /// 弹出「几样里选一样」的对话框：上面一句说明，中间几个单选项（各带一行小字），下面可以有
    /// 几个勾选项。点确定（或按回车）时关掉对话框，把选了第几项、各勾选项的状态交给 `on_ok`。
    pub(super) fn choose(
        &mut self,
        choose: Choose,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_ok: impl Fn(&mut Self, usize, Vec<bool>, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let Choose {
            title,
            description,
            options,
            selected,
            checks,
            ok,
        } = choose;
        let selected = Rc::new(Cell::new(selected.min(options.len().saturating_sub(1))));
        let states: Rc<Vec<Cell<bool>>> =
            Rc::new(checks.iter().map(|(_, on)| Cell::new(*on)).collect());
        let view = cx.entity().downgrade();
        let submit: Submit = {
            let (selected, states) = (selected.clone(), states.clone());
            Rc::new(move |window, cx| {
                window.close_dialog(cx);
                let checks: Vec<bool> = states.iter().map(Cell::get).collect();
                view.update(cx, |this, cx| {
                    on_ok(this, selected.get(), checks, window, cx)
                })
                .ok();
            })
        };
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            // 没有说明时第一个单选项紧贴着标题，留一点空
            let mut body = v_flex()
                .gap_3()
                .when(description.is_empty(), |body| body.pt_1());
            if !description.is_empty() {
                body = body.child(
                    div()
                        .text_sm()
                        .whitespace_normal()
                        .child(description.clone()),
                );
            }
            for (ix, (label, detail)) in options.iter().enumerate() {
                let pick = selected.clone();
                body = body.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Radio::new(("choose-option", ix))
                                .label(label.clone())
                                .checked(selected.get() == ix)
                                .on_click(move |_, window, _| {
                                    pick.set(ix);
                                    window.refresh();
                                }),
                        )
                        .when(!detail.is_empty(), |row| {
                            row.child(
                                div()
                                    .pl(px(24.))
                                    .text_xs()
                                    .text_color(muted)
                                    .whitespace_normal()
                                    .child(detail.clone()),
                            )
                        }),
                );
            }
            for (ix, (label, _)) in checks.iter().enumerate() {
                let states = states.clone();
                body = body.child(
                    Checkbox::new(("choose-check", ix))
                        .label(*label)
                        .checked(states[ix].get())
                        .on_click(move |checked, window, _| {
                            states[ix].set(*checked);
                            window.refresh();
                        }),
                );
            }
            let (submit, on_enter) = (submit.clone(), submit.clone());
            dialog
                .title(title.clone())
                .w(px(540.))
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
        self.new_branch_at(start, String::new(), true, window, cx);
    }

    /// 新建分支的对话框：`start` 是从哪个提交（哈希、说明）建，没有就从 HEAD；`value` 是预先填的
    /// 名字，`switch` 是「创建后切换」默认勾不勾。
    pub(super) fn new_branch_at(
        &mut self,
        start: Option<(String, String)>,
        value: String,
        switch: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
            placeholder: tr("分支名称，例如 feature/login").into(),
            value,
            hint: hint.into(),
            ok: tr("创建"),
            checks: vec![(tr("创建后切换").into(), switch)],
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
            let what = tf!("切换到 {branch}", branch = &branch);
            let create =
                move |this: &mut Self, stash: bool, window: &mut Window, cx: &mut Context<Self>| {
                    let branch = branch.clone();
                    this.run_stashed(
                        tr("新建分支"),
                        vec![Step::local(args.clone())],
                        stash,
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
                };
            // 从别的提交新建并切换过去，工作区要换内容：有改动时先问怎么处理。
            // 从当前 HEAD 新建不用问，改动原样留着
            if switch && start.is_some() {
                this.with_local_changes(what, tr("保留改动并切换"), window, cx, create);
            } else {
                create(this, false, window, cx);
            }
        });
    }

    /// 贮藏当前的改动。正在看本地修改、又多选了文件时，可以只贮藏选中的那几个（默认就是）。
    pub(super) fn stash_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked: Vec<fr_core::status::Entry> = match self.mode {
            Mode::Changes => self
                .changes_selected_entries()
                .map(|(_, entries)| entries)
                .filter(|entries| entries.len() > 1)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        self.stash_entries_dialog(picked, window, cx);
    }

    /// 贮藏的对话框；`picked` 不空时多一个勾选项「仅贮藏选中的 N 个文件」，默认勾上。
    pub(super) fn stash_entries_dialog(
        &mut self,
        picked: Vec<fr_core::status::Entry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut checks = vec![(tr("包含未跟踪的文件").into(), false)];
        if !picked.is_empty() {
            let label = tf!("仅贮藏选中的 {n} 个文件", n = picked.len());
            checks.push((label.into(), true));
        }
        let only_picked = !picked.is_empty();
        let ask = Ask {
            title: tr("贮藏改动").into(),
            placeholder: tr("说明（可选）").into(),
            // 第二个勾选项「仅贮藏选中的 N 个文件」勾着时说明换成只贮藏一部分的
            hint: Hint::ByChecks(Box::new(move |checks| {
                match only_picked && checks.get(1).copied().unwrap_or(false) {
                    true => {
                        tr("仅贮藏选中的文件时，其中未跟踪的文件会一并贮藏；其余改动保留在工作区。")
                    }
                    false => tr("贮藏改动，使工作区恢复干净；可在侧栏的「贮藏」中恢复。"),
                }
                .into()
            })),
            ok: tr("贮藏"),
            checks,
            allow_empty: true,
            validate: Some(Box::new(move |this, answer| {
                // 只贮藏选中的：选中的总是有改动的
                if only_picked && answer.checks.get(1).copied().unwrap_or(false) {
                    return None;
                }
                let entries = this.status.as_ref().map(|s| s.entries.as_slice())?;
                let untracked = answer.checks.first().copied().unwrap_or(false);
                if entries.is_empty() {
                    Some(tr("没有可贮藏的改动").into())
                } else if !untracked
                    && entries
                        .iter()
                        .all(|e| e.unstaged == Some(Kind::Untracked) && e.staged.is_none())
                {
                    Some(tr("只有未跟踪的文件，需勾选「包含未跟踪的文件」才能贮藏").into())
                } else {
                    None
                }
            })),
            ..Default::default()
        };
        self.ask(ask, window, cx, move |this, answer, window, cx| {
            let untracked = answer.checks.first().copied().unwrap_or(false);
            let partial = only_picked && answer.checks.get(1).copied().unwrap_or(false);
            let args = match partial {
                true => {
                    // 改名的文件：原路径也要带上；选中的里有未跟踪的文件就连它们一起收
                    let paths: Vec<&str> = picked
                        .iter()
                        .flat_map(|e| {
                            std::iter::once(e.path.as_str()).chain(e.orig_path.as_deref())
                        })
                        .collect();
                    let has_untracked = picked.iter().any(|e| e.unstaged == Some(Kind::Untracked));
                    fr_core::stash::push_paths_args(Some(&answer.text), has_untracked, &paths)
                }
                false => fr_core::stash::push_args(Some(&answer.text), untracked),
            };
            let count = picked.len();
            this.run_op(
                tr("贮藏"),
                args,
                None,
                window,
                cx,
                move |_, out, window, cx| {
                    // 没有改动时 git 也是成功退出，只在输出里说一句
                    let said = String::from_utf8_lossy(&out.stdout);
                    let note = if said.contains("No local changes") {
                        Notification::warning(tr("没有可贮藏的改动"))
                    } else if partial {
                        Notification::success(tf!("已贮藏选中的 {count} 个文件", count))
                    } else {
                        Notification::success(tr("改动已贮藏，工作区已恢复干净"))
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
