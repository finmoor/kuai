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
use super::ops::Step;

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
            this.update_in(cx, |this, window, cx| {
                let dialog = Sweep {
                    title: tr("清理上游已删除的分支").into(),
                    note: tr("仅删除本地分支。未并入远程分支的提交，删除后难以找回。").into(),
                    label: tr("清理分支"),
                };
                this.sweep_dialog_with(list, dialog, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// 列出要删的分支，逐个标出提交在不在远程分支里，勾选后一起删（清理上游已删除的分支、侧栏里
    /// 多选后删除都用它）。
    fn sweep_dialog_with(
        &mut self,
        list: Vec<Gone>,
        dialog_text: Sweep,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Sweep { title, note, label } = dialog_text;
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
                .title(title.clone())
                .w(px(520.))
                // 回车不执行，要明确点按钮
                .on_ok(|_, _, _| false)
                .child(
                    v_flex().gap_3().child(rows).child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .whitespace_normal()
                            .child(note.clone()),
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
                                    view.update(cx, |this, cx| {
                                        this.run_sweep(names, label, window, cx)
                                    })
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
        label: &'static str,
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
            this.run_op(label, args, None, window, cx, move |this, _, window, cx| {
                // 记进「找回提交」，按原名能恢复
                this.remember_deleted(names.clone(), cx);
                this.sidebar.multi.clear();
                let text = tf!("已删除 {n} 个分支", n = count);
                window.push_notification(Notification::success(text), cx);
            });
        });
    }

    // ---------- 侧栏多选：批量删除、推送 ----------

    /// 多选着分支或标签时的右键菜单：对选中的全部操作。`picked` 是完整的引用名，都是同一类。
    pub(super) fn side_multi_menu(
        &self,
        picked: Vec<String>,
        cx: &mut Context<Self>,
    ) -> super::menus::MenuFn {
        let view = cx.entity().downgrade();
        let tags = picked
            .first()
            .is_some_and(|full| full.starts_with("refs/tags/"));
        let names: Vec<String> = picked
            .iter()
            .filter_map(|full| full.splitn(3, '/').nth(2))
            .map(str::to_string)
            .collect();
        let count = names.len();
        let remote = self.default_remote();
        Box::new(move |menu, _, _| {
            let item = |label: String,
                        run: fn(
                &mut RepoView,
                Vec<String>,
                &mut Window,
                &mut Context<RepoView>,
            )| {
                let (view, names) = (view.clone(), names.clone());
                gpui_kit::component::menu::PopupMenuItem::new(label).on_click(
                    move |_, window, cx| {
                        let names = names.clone();
                        view.update(cx, |this, cx| run(this, names, window, cx))
                            .ok();
                    },
                )
            };
            let copy = names.join("\n");
            let menu = match tags {
                true => menu
                    .item(item(
                        tf!("推送选中的 {count} 个标签到 {remote}", count, remote),
                        |this, names, w, cx| this.push_tags(names, w, cx),
                    ))
                    .item(item(
                        tf!("删除选中的 {count} 个本地标签…", count),
                        |this, names, w, cx| this.delete_tags(names, w, cx),
                    )),
                false => menu
                    .item(item(
                        tf!("推送选中的 {count} 个分支…", count),
                        |this, names, w, cx| this.push_branches(names, w, cx),
                    ))
                    .item(item(
                        tf!("删除选中的 {count} 个分支…", count),
                        |this, names, w, cx| this.delete_branches(names, w, cx),
                    )),
            };
            menu.separator().item(
                gpui_kit::component::menu::PopupMenuItem::new(tr("复制名称")).on_click(
                    move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                    },
                ),
            )
        })
    }

    /// 删除选中的几个本地分支：和清理上游已删除的分支用同一个对话框，逐个标出提交在不在远程分支
    /// 里，不在的默认不勾。当前分支删不了，不列。
    pub(super) fn delete_branches(
        &mut self,
        names: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picked: Vec<(String, String)> = self
            .refs
            .branches
            .iter()
            .filter(|branch| !branch.current && names.contains(&branch.name))
            .map(|branch| (branch.name.clone(), branch.target.clone()))
            .collect();
        if picked.is_empty() {
            let text = tr("选中的是当前分支，无法删除。请先切换到其他分支。");
            window.push_notification(Notification::info(text), cx);
            return;
        }
        let skipped = names.len() - picked.len();
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        let upstream = self.status.as_ref().and_then(|s| s.upstream.clone());
        cx.spawn_in(window, async move |this, cx| {
            let checks = picked
                .into_iter()
                .map(|name| inspect(&channel, &repo, name, upstream.as_deref()));
            let list = futures::future::join_all(checks).await;
            this.update_in(cx, |this, window, cx| {
                let note = match skipped {
                    0 => tr("仅删除本地分支。未并入远程分支的提交，删除后难以找回。").to_string(),
                    _ => tr("仅删除本地分支；当前分支不能删除，未列出。未并入远程分支的提交，删除后难以找回。")
                        .to_string(),
                };
                let dialog = Sweep {
                    title: tr("删除分支").into(),
                    note: note.into(),
                    label: tr("删除分支"),
                };
                this.sweep_dialog_with(list, dialog, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// 推送选中的几个分支：有上游的推到上游，没有的推到默认远程的同名分支并设为上游。先说清各推到
    /// 哪里，确认了再推。
    fn push_branches(&mut self, names: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        use std::collections::BTreeMap;
        if self.remote_names.is_empty() {
            let text = tr("此仓库尚无远程。请先添加远程。");
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        // 远程 → （已有上游的引用规格，新分支的引用规格）
        let mut plan: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
        let mut lines: Vec<String> = Vec::new();
        for branch in self
            .refs
            .branches
            .iter()
            .filter(|b| names.contains(&b.name))
        {
            let known = branch
                .upstream
                .as_deref()
                .filter(|_| !branch.gone)
                .and_then(|upstream| self.split_remote_ref(upstream));
            match known {
                Some((remote, on_remote)) => {
                    lines.push(format!("{} → {remote}/{on_remote}", branch.name));
                    let spec = format!("refs/heads/{}:refs/heads/{on_remote}", branch.name);
                    plan.entry(remote).or_default().0.push(spec);
                }
                None => {
                    let remote = self.default_remote();
                    lines.push(tf!(
                        "{name} → {remote}/{name}（新分支，设为上游）",
                        name = branch.name.as_str(),
                        remote = remote.as_str()
                    ));
                    let spec = format!("refs/heads/{0}:refs/heads/{0}", branch.name);
                    plan.entry(remote).or_default().1.push(spec);
                }
            }
        }
        let count = lines.len();
        if count == 0 {
            return;
        }
        let mut steps: Vec<Step> = Vec::new();
        for (remote, (known, fresh)) in plan {
            if !known.is_empty() {
                let mut push = vec!["push".to_string(), "--progress".into(), remote.clone()];
                push.extend(known);
                steps.push(Step::net(push));
            }
            if !fresh.is_empty() {
                let mut push = vec!["push".to_string(), "--progress".into(), "-u".into(), remote];
                push.extend(fresh);
                steps.push(Step::net(push));
            }
        }
        let description = lines.join("\n");
        self.confirm_plain(
            tf!("推送 {count} 个分支？", count),
            description,
            tr("推送"),
            window,
            cx,
            move |this, window, cx| {
                this.run_seq(
                    tr("推送"),
                    steps.clone(),
                    window,
                    cx,
                    move |this, _, window, cx| {
                        let text = tf!("已推送 {count} 个分支", count);
                        this.notify_net(Notification::success(text), window, cx);
                    },
                );
            },
        );
    }

    /// 删除选中的几个本地标签。
    fn delete_tags(&mut self, names: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let count = names.len();
        let shown = names.join(tr("、"));
        let description = tf!(
            "删除 {count} 个本地标签：{shown}。远程上的同名标签不受影响。",
            count,
            shown
        );
        self.confirm(
            tf!("删除 {count} 个标签？", count),
            description,
            tr("删除"),
            window,
            cx,
            move |this, window, cx| {
                let mut delete: Vec<String> = vec!["tag".into(), "-d".into(), "--".into()];
                delete.extend(names.iter().cloned());
                this.run_op(
                    tr("删除标签"),
                    delete,
                    None,
                    window,
                    cx,
                    move |this, _, window, cx| {
                        this.sidebar.multi.clear();
                        let text = tf!("已删除 {count} 个本地标签", count);
                        window.push_notification(Notification::success(text), cx);
                    },
                );
            },
        );
    }

    /// 把选中的几个标签推送到默认远程。
    fn push_tags(&mut self, names: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.remote_names.is_empty() {
            let text = tr("此仓库尚无远程。请先添加远程。");
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let (count, remote) = (names.len(), self.default_remote());
        let mut push = vec!["push".to_string(), "--progress".into(), remote.clone()];
        push.extend(names.iter().map(|name| format!("refs/tags/{name}")));
        self.run_net_op(
            tr("推送"),
            push,
            window,
            cx,
            move |this, _, window, cx| {
                let text = tf!("已推送 {count} 个标签到 {remote}", count, remote);
                this.notify_net(Notification::success(text), window, cx);
            },
        );
    }
}

/// 批量删除分支的对话框上写什么。
struct Sweep {
    title: SharedString,
    /// 列表下面那句说明
    note: SharedString,
    /// 操作的名字（「清理分支」「删除分支」）
    label: &'static str,
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
