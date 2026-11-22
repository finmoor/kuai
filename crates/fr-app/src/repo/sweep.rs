//! 清理上游已删除的分支：把上游没了的本地分支列出来，勾选后一起删掉。
//!
//! 上游没了，多半是合并请求合进去以后远程分支被删了，本地这个也就没用了；但也可能是别人删错了、
//! 本地还有没推上去的提交。所以先逐个看它的提交在不在某个远程分支里：在的默认勾上，不在的
//! 默认不勾、标出有几个提交会丢。当前分支不列（git 不让删）。删用 `-D`：合并请求合进去时
//! 常常是压成一个提交的，`-d` 认不出来，会一个都删不掉。

use std::cell::Cell;
use std::rc::Rc;

use fr_channel::Channel;
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;

/// 一个上游已删除的分支，和它的提交在不在远程分支里。
struct Gone {
    name: String,
    /// 读列表时它指向的提交：删之前核对还是不是它
    tip: String,
    /// 它的提交都在某个远程分支里（删了不丢东西）
    merged: bool,
    /// 写在名字下面的一句：并入了哪个远程分支，或者有几个提交没并入
    detail: String,
}

/// 含有这个分支的远程分支里说哪一个，要让人一看就知道「合进去了」：先看当前分支的上游
/// （`current`），再看主干（main、master、develop），再看发布分支（`release/` 开头），都没有
/// 才说第一个。「远程名/HEAD」这样的指针不算。
fn first_remote_branch<'a>(output: &'a str, current: Option<&str>) -> Option<&'a str> {
    let branches = || {
        output
            .lines()
            .map(str::trim)
            .filter(|line| line.contains('/') && !line.ends_with("/HEAD"))
    };
    let name = |line: &str| {
        line.split_once('/')
            .map_or("", |(_, name)| name)
            .to_string()
    };
    let trunk = |line: &&str| ["main", "master", "develop", "dev"].contains(&name(line).as_str());
    branches()
        .find(|line| Some(*line) == current)
        .or_else(|| branches().find(trunk))
        .or_else(|| branches().find(|line| name(line).starts_with("release/")))
        .or_else(|| branches().next())
}

async fn inspect(
    channel: &Channel,
    repo: &str,
    (name, tip): (String, String),
    current: Option<&str>,
) -> Gone {
    let full = format!("refs/heads/{name}");
    let count = ["rev-list", "--count", &full, "--not", "--remotes"];
    let count = channel
        .git(repo, count.map(String::from).to_vec())
        .await
        .and_then(|out| out.ok())
        .ok()
        .and_then(|out| String::from_utf8_lossy(&out).trim().parse::<usize>().ok());
    let (merged, detail) = match count {
        Some(0) => {
            let contains = [
                "for-each-ref",
                "--count=300",
                "--contains",
                &full,
                "--format=%(refname:short)",
                "refs/remotes",
            ];
            let found = channel
                .git(repo, contains.map(String::from).to_vec())
                .await
                .and_then(|out| out.ok())
                .ok()
                .map(|out| String::from_utf8_lossy(&out).into_owned());
            let detail = match found
                .as_deref()
                .and_then(|out| first_remote_branch(out, current))
            {
                Some(remote) => tf!("已并入 {remote}", remote),
                None => tr("已并入远程分支").to_string(),
            };
            (true, detail)
        }
        Some(n) => (false, tf!("{n} 个提交未并入任何远程分支", n)),
        None => (false, tr("无法确认是否已并入远程分支").to_string()),
    };
    Gone {
        name,
        tip,
        merged,
        detail,
    }
}

impl RepoView {
    /// 上游已删除的本地分支有几个（不算当前分支）。
    pub(super) fn gone_count(&self) -> usize {
        self.refs
            .branches
            .iter()
            .filter(|branch| branch.gone && !branch.current)
            .count()
    }

    /// 侧栏「分支」标题上的「清理 N」：先看每个分支的提交在不在远程分支里，再弹出对话框。
    pub(super) fn sweep_gone_branches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let names: Vec<(String, String)> = self
            .refs
            .branches
            .iter()
            .filter(|branch| branch.gone && !branch.current)
            .map(|branch| (branch.name.to_string(), branch.target.clone()))
            .collect();
        if names.is_empty() {
            let text = tr("没有上游已删除的分支。");
            window.push_notification(Notification::info(text), cx);
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        let upstream = self.status.as_ref().and_then(|s| s.upstream.clone());
        cx.spawn_in(window, async move |this, cx| {
            let checks = names
                .into_iter()
                .map(|name| inspect(&channel, &repo, name, upstream.as_deref()));
            let list = futures::future::join_all(checks).await;
            this.update_in(cx, |this, window, cx| this.sweep_dialog(list, window, cx))
                .ok();
        })
        .detach();
    }

    fn sweep_dialog(&mut self, list: Vec<Gone>, window: &mut Window, cx: &mut Context<Self>) {
        let list = Rc::new(list);
        let states: Rc<Vec<Cell<bool>>> =
            Rc::new(list.iter().map(|gone| Cell::new(gone.merged)).collect());
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let theme = cx.theme();
            let (muted, warning, border) = (theme.muted_foreground, theme.warning, theme.border);
            let mut rows = v_flex()
                .id("sweep-list")
                .max_h(px(320.))
                .overflow_y_scroll()
                .border_1()
                .border_color(border)
                .rounded(px(crate::ui::RADIUS))
                .p_2()
                .gap_2();
            for (ix, gone) in list.iter().enumerate() {
                let states = states.clone();
                rows = rows.child(
                    v_flex()
                        .gap(px(2.))
                        .child(
                            Checkbox::new(("sweep-check", ix))
                                .label(gone.name.clone())
                                .checked(states[ix].get())
                                .on_click(move |checked, window, _| {
                                    states[ix].set(*checked);
                                    window.refresh();
                                }),
                        )
                        .child(
                            div()
                                .pl(px(24.))
                                .text_xs()
                                .text_color(if gone.merged { muted } else { warning })
                                .child(gone.detail.clone()),
                        ),
                );
            }
            let picked = states.iter().filter(|state| state.get()).count();
            let (view, list, states) = (view.clone(), list.clone(), states.clone());
            dialog
                .title(tr("清理上游已删除的分支"))
                .w(px(520.))
                // 回车不执行，要明确点按钮
                .on_ok(|_, _, _| false)
                .child(
                    v_flex().gap_3().child(rows).child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .whitespace_normal()
                            .child(tr("仅删除本地分支。未并入远程分支的提交，删除后难以找回。")),
                    ),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .label(tr("取消"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("ok")
                                .with_variant(ButtonVariant::Danger)
                                .label(tf!("删除 {n} 个分支", n = picked))
                                .disabled(picked == 0)
                                .on_click(move |_, window, cx| {
                                    let names: Vec<(String, String)> = list
                                        .iter()
                                        .zip(states.iter())
                                        .filter(|(_, state)| state.get())
                                        .map(|(gone, _)| (gone.name.clone(), gone.tip.clone()))
                                        .collect();
                                    window.close_dialog(cx);
                                    view.update(cx, |this, cx| this.run_sweep(names, window, cx))
                                        .ok();
                                }),
                        ),
                )
        });
    }

    /// 删掉勾选的分支（`names` 是名字和读列表时它指向的提交）。删之前核对每个分支还指着那个提交：
    /// 对话框开着时又往上提交了，新的提交没有并入远程，`branch -D` 会连它们一起丢掉，这时一个都不删。
    fn run_sweep(
        &mut self,
        names: Vec<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if names.is_empty() {
            return;
        }
        let count = names.len();
        let checks: Vec<super::ops::Check> = names
            .iter()
            .map(|(name, tip)| {
                let full = format!("refs/heads/{name}");
                let args = vec!["rev-parse".into(), "-q".into(), "--verify".into(), full];
                (args, Some(tip.clone()))
            })
            .collect();
        let mut args: Vec<String> = vec!["branch".into(), "-D".into(), "--".into()];
        args.extend(names.iter().map(|(name, _)| name.clone()));
        self.verify_then(checks, window, cx, move |this, window, cx| {
            this.run_op(
                tr("清理分支"),
                args,
                None,
                window,
                cx,
                move |this, _, window, cx| {
                    // 记进「找回提交」，按原名能恢复
                    this.remember_deleted(names.clone(), cx);
                    let text = tf!("已删除 {n} 个分支", n = count);
                    window.push_notification(Notification::success(text), cx);
                },
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::first_remote_branch;

    #[test]
    fn 并入了哪个远程分支_跳过指针() {
        assert_eq!(
            first_remote_branch("origin\norigin/HEAD\norigin/main\n", None),
            Some("origin/main")
        );
        assert_eq!(first_remote_branch("origin/HEAD\n", None), None);
        assert_eq!(
            first_remote_branch("origin/feat/alive\norigin/master\n", None),
            Some("origin/master")
        );
        assert_eq!(
            first_remote_branch("origin/feat/a\norigin/feat/main\n", None),
            Some("origin/feat/a")
        );
        // 主干不叫 main 的仓库：先说当前分支的上游，再说发布分支
        let out = "origin/20250411\norigin/feat/x\norigin/release/1.7.x\norigin/release/2.0\n";
        assert_eq!(
            first_remote_branch(out, Some("origin/release/2.0")),
            Some("origin/release/2.0")
        );
        assert_eq!(
            first_remote_branch(out, Some("origin/other")),
            Some("origin/release/1.7.x")
        );
    }
}
