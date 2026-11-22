//! 在分离的 HEAD 上提交过、要切到别处时先问一句：切过去以后这些提交不在任何分支上了
//! （原型见 `docs/design/reliability/` 的「离开分离的 HEAD」）。

use std::rc::Rc;

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::RepoView;
use super::dialogs::{Ask, ref_name_error};
use super::impact::{Action, Choice, Impact, Row, Tone, list_commits};
use super::ops::failure;

/// 新分支名最长几个字
const NAME_MAX: usize = 40;

/// 从提交说明里取一个分支名：字母数字以外的换成 `-`，小写，最多 [`NAME_MAX`] 个字；取不出来用
/// `detached-<提交号>`。
pub(super) fn branch_name_from(subject: &str, id: &str) -> String {
    let mut name = String::new();
    for ch in subject.chars() {
        let ch = match ch.is_alphanumeric() {
            true => ch.to_lowercase().next().unwrap_or(ch),
            false => '-',
        };
        if ch == '-' && (name.is_empty() || name.ends_with('-')) {
            continue;
        }
        if name.chars().count() >= NAME_MAX {
            break;
        }
        name.push(ch);
    }
    let name = name.trim_end_matches('-').to_string();
    match name.is_empty() {
        true => format!("detached-{}", &id[..id.len().min(7)]),
        false => name,
    }
}

impl RepoView {
    /// 现在在分离的 HEAD 上，而且 HEAD 上有不在任何分支、远程分支、标签上的提交时，先问：新建分支
    /// 留住它们再切换，或者直接切换；不是这样的直接做 `then`。`target` 是要切到哪（给人看的名字）。
    pub(super) fn leave_detached_then(
        &mut self,
        target: String,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let detached = self.in_progress.is_none()
            && self
                .status
                .as_ref()
                .is_some_and(|status| status.branch.is_none() && status.head.is_some());
        let Some(channel) = self.channel.clone().filter(|_| detached) else {
            return then(self, window, cx);
        };
        let range = ["HEAD", "--not", "--branches", "--remotes", "--tags"]
            .map(String::from)
            .to_vec();
        let orphans = list_commits(channel, self.entry.path.clone(), range, false, false);
        let then: Action = Rc::new(then);
        cx.spawn_in(window, async move |this, cx| {
            let orphans = orphans.await;
            this.update_in(cx, |this, window, cx| {
                if orphans.total == 0 {
                    return then(this, window, cx);
                }
                let name = orphans
                    .commits
                    .first()
                    .map(|commit| branch_name_from(&commit.subject, &commit.id))
                    .unwrap_or_default();
                let impact = Impact {
                    description: tr("你在分离的 HEAD 上提交过，切换后这些提交就不在任何分支上了。")
                        .into(),
                    rows: vec![
                        Row::new(
                            Tone::Warn,
                            IconName::GitCommitHorizontal,
                            "不在分支上",
                            orphans.count_text(),
                        )
                        .listed(&orphans),
                        Row::new(
                            Tone::Good,
                            IconName::ArchiveRestore,
                            "怎么找回",
                            tr("直接切换的话，提交可在侧栏的「找回提交」中找到，保留 30 天。"),
                        ),
                    ],
                };
                let direct = {
                    let then = then.clone();
                    Choice::new(
                        tr("直接切换"),
                        ButtonVariant::Default,
                        move |this, window, cx| then(this, window, cx),
                    )
                };
                let keep = {
                    let (then, target) = (then.clone(), target.clone());
                    Choice::new(
                        tr("新建分支并切换…"),
                        ButtonVariant::Primary,
                        move |this, window, cx| {
                            this.keep_detached_then(
                                name.clone(),
                                target.clone(),
                                then.clone(),
                                window,
                                cx,
                            )
                        },
                    )
                };
                let title = tf!("切换到 {target}？", target = &target);
                this.choose_impact(title, impact, vec![direct, keep], false, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// 问新分支的名字，在 HEAD 上建这个分支（不切过去），再做 `then`。
    fn keep_detached_then(
        &mut self,
        name: String,
        target: String,
        then: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ask = Ask {
            title: tr("新建分支，留住这些提交").into(),
            placeholder: tr("分支名称，例如 feature/login").into(),
            value: name,
            hint: tf!(
                "在当前的 HEAD 上新建分支，然后切换到 {target}。",
                target = &target
            )
            .into(),
            ok: tr("新建并切换"),
            validate: Some(Box::new(|this, answer| {
                ref_name_error(&answer.text).or_else(|| this.branch_exists_error(&answer.text))
            })),
            ..Default::default()
        };
        self.ask(ask, window, cx, move |this, answer, window, cx| {
            let branch = answer.text;
            let create = vec!["branch".into(), "--".into(), branch.clone(), "HEAD".into()];
            let then = then.clone();
            this.run_op_with(
                tr("新建分支"),
                create,
                window,
                cx,
                move |this, result, window, cx| match result {
                    Ok(_) => {
                        let text = tf!("已新建分支 {branch}，提交留在上面", branch = &branch);
                        window.push_notification(Notification::success(text), cx);
                        then(this, window, cx);
                    }
                    Err(text) => failure(window, cx, tr("新建分支"), text),
                },
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::branch_name_from;

    #[test]
    fn 从提交说明里取分支名() {
        assert_eq!(
            branch_name_from("test(usage): drain on shutdown", "f20c6d1aa"),
            "test-usage-drain-on-shutdown"
        );
        assert_eq!(branch_name_from("修复 登录超时", "abc"), "修复-登录超时");
        assert_eq!(branch_name_from("  ...  ", "f20c6d1aa"), "detached-f20c6d1");
        let long = branch_name_from(&"a".repeat(80), "x");
        assert_eq!(long.chars().count(), 40);
        // 截断后不以 - 结尾
        assert_eq!(
            branch_name_from(&format!("{} b", "a".repeat(39)), "x"),
            "a".repeat(39)
        );
    }
}
