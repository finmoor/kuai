//! 提交和钩子：钩子没通过时说清楚是钩子的事、给「查看输出」和「跳过钩子并提交」；把已暂存的
//! 改动并入之前的某个提交。
//!
//! 提交是一条 `git commit -F -`。钩子（pre-commit、commit-msg）在它里面跑，输出边做边显示在底部
//! 的「输出」里（见 `ops.rs` 的 `Step::live`）。提交失败时 git 的报错跟着主机的语言变，不按文字认
//! 是不是钩子：看钩子目录里有没有装着这两个钩子，有、又不是没设作者，就按钩子没通过报。
//!
//! 并入之前的提交：`git commit --fixup=<提交>` 接一次 `git rebase -i --autosquash`（清单由 git
//! 自己排，不用改）。所选提交之后有合并提交时重放不了，不让选。

use std::collections::HashSet;

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::component::radio::Radio;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use super::impact::{Row as ImpactRow, Tone, rows_box};
use super::ops::{Step, args};
use crate::ui;

/// 一次提交要做的事（提交失败后「跳过钩子并提交」照它再来一遍）。
#[derive(Clone)]
pub(super) struct Commit {
    /// 提交成功后接着推送
    pub push: bool,
    /// 修改上次提交
    pub amend: bool,
    /// 这一次不运行 pre-commit、commit-msg 钩子
    pub no_verify: bool,
    /// 交出去时提交说明框里的原文：成功后内容没变才清空
    pub sent: String,
    pub message: String,
}

/// 「钩子未通过」那条通知的标识：同一时间只留一条。
struct HookNote;

/// 列出装着（存在、能执行）的钩子：参数是要问的钩子名，装着的各输出一行。钩子目录按
/// `core.hooksPath` 走（`rev-parse --git-path` 认得）。
const HOOKS: &str = r#"alias.fr-hooks=!f(){ for n in "$@"; do p=$(git rev-parse --git-path "hooks/$n"); [ -f "$p" ] && [ -x "$p" ] && echo "$n"; done; return 0; }; f"#;

/// 提交时会跑的钩子
const COMMIT_HOOKS: [&str; 2] = ["pre-commit", "commit-msg"];

/// 从最近多少个提交里选要并入的那个
const FIXUP_LISTED: usize = 60;

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// 并入之前的提交对话框里的一行。
#[derive(Clone)]
struct FixRow {
    id: String,
    subject: String,
    note: String,
    /// 待推送（当前分支有、上游还没有）
    unpushed: bool,
    /// 不能选的原因
    blocked: Option<&'static str>,
}

/// 「并入之前的提交」的对话框：选一个提交，下面写会怎样。
struct FixupPicker {
    repo: WeakEntity<RepoView>,
    rows: Vec<FixRow>,
    selected: Option<usize>,
    /// 已暂存了几个文件
    staged: usize,
}

impl FixupPicker {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected.and_then(|ix| self.rows.get(ix)).cloned() else {
            return;
        };
        // 所选提交是不是这条历史上最早的那个（没有父提交）：变基要用 --root
        window.close_dialog(cx);
        self.repo
            .update(cx, |repo, cx| {
                repo.run_fixup(row.id, row.subject, window, cx)
            })
            .ok();
    }
}

impl Render for FixupPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list_height = (window.viewport_size().height - px(380.)).clamp(px(120.), px(300.));
        let theme = cx.theme();
        let (muted, border, warning, panel, active, hover) = (
            theme.muted_foreground,
            theme.border,
            theme.warning,
            theme.muted,
            theme.list_active,
            theme.list_hover,
        );
        let selected = self.selected;
        let rows: Vec<AnyElement> = self
            .rows
            .iter()
            .enumerate()
            .map(|(ix, row)| {
                let blocked = row.blocked;
                h_flex()
                    .id(("fixup-row", ix))
                    .h(px(30.))
                    .px_2()
                    .gap_2()
                    .when(ix > 0, |d| d.border_t_1().border_color(border))
                    .when(selected == Some(ix), |d| d.bg(active))
                    .when(selected != Some(ix) && blocked.is_none(), |d| {
                        d.hover(|s| s.bg(hover)).cursor_pointer()
                    })
                    .when(blocked.is_none(), |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| {
                            this.selected = Some(ix);
                            cx.notify();
                        }))
                    })
                    .child(
                        Radio::new(("fixup-pick", ix))
                            .checked(selected == Some(ix))
                            .disabled(blocked.is_some()),
                    )
                    .child(
                        div()
                            .w(px(64.))
                            .flex_none()
                            .font_family(crate::fonts::mono())
                            .text_xs()
                            .text_color(muted)
                            .child(short(&row.id).to_string()),
                    )
                    .when(!row.unpushed && blocked.is_none(), |line| {
                        line.child(
                            div()
                                .flex_none()
                                .px(px(5.))
                                .rounded(px(4.))
                                .border_1()
                                .border_color(warning.opacity(0.5))
                                .text_xs()
                                .text_color(warning)
                                .child(tr("已推送")),
                        )
                    })
                    .child(
                        ui::clipped(("fixup-subject", ix), row.subject.clone())
                            .flex_1()
                            .text_sm()
                            .when(blocked.is_some(), |d| d.text_color(muted)),
                    )
                    .child(
                        ui::clipped(
                            ("fixup-note", ix),
                            match blocked {
                                Some(why) => why.to_string(),
                                None => row.note.clone(),
                            },
                        )
                        .flex_none()
                        .max_w(px(220.))
                        .text_xs()
                        .text_color(muted),
                    )
                    .into_any_element()
            })
            .collect();
        // 会怎样：所选提交和它之后的提交都要重新生成
        let mut impact = Vec::new();
        if let Some(ix) = selected {
            let rewritten = ix + 1;
            let pushed = self.rows[..=ix].iter().filter(|row| !row.unpushed).count();
            impact.push(ImpactRow::new(
                Tone::Plain,
                IconName::GitCommitHorizontal,
                "重新生成",
                tf!(
                    "{id} 及其之后共 {rewritten} 个提交将重新生成，不新增提交。",
                    id = short(&self.rows[ix].id),
                    rewritten
                ),
            ));
            if pushed > 0 {
                impact.push(ImpactRow::new(
                    Tone::Warn,
                    IconName::ArrowUp,
                    "已推送",
                    tf!("其中 {pushed} 个已推送，完成后推送要强制推送。", pushed),
                ));
            }
            impact.push(ImpactRow::new(
                Tone::Good,
                IconName::ArchiveRestore,
                "怎么找回",
                tr("完成后的通知里可撤销；之前的位置也记在侧栏的「找回提交」里。"),
            ));
        }
        v_flex()
            .gap_3()
            .child(div().text_sm().whitespace_normal().child(tf!(
                "把已暂存的 {staged} 个文件的改动补进所选的提交。",
                staged = self.staged
            )))
            .child(
                div()
                    .id("fixup-list")
                    .max_h(list_height)
                    .overflow_y_scroll()
                    .rounded(px(ui::RADIUS))
                    .border_1()
                    .border_color(border)
                    .bg(panel.opacity(0.35))
                    .child(v_flex().children(rows)),
            )
            .when(!impact.is_empty(), |d| d.child(rows_box(&impact, cx)))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(muted)
                            .child(tr("未暂存的改动不受影响（先贮藏，完成后恢复）。")),
                    )
                    .child(
                        Button::new("fixup-cancel")
                            .label(tr("取消"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("fixup-ok")
                            .primary()
                            .label(tr("并入"))
                            .disabled(selected.is_none())
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

impl RepoView {
    /// 装着哪几个提交时会跑的钩子。读不到按没有算。
    pub(super) fn commit_hooks(&self, cx: &mut Context<Self>) -> Task<Vec<&'static str>> {
        self.hooks_present(&COMMIT_HOOKS, cx)
    }

    /// `names` 里的钩子哪几个装着（存在、能执行）。读不到按没有算。
    fn hooks_present(
        &self,
        names: &'static [&'static str],
        cx: &mut Context<Self>,
    ) -> Task<Vec<&'static str>> {
        let Some(channel) = self.channel.clone() else {
            return Task::ready(Vec::new());
        };
        let mut list = vec!["-c".to_string(), HOOKS.into(), "fr-hooks".into()];
        list.extend(names.iter().map(|name| name.to_string()));
        let read = channel.git(&self.entry.path, list);
        cx.background_spawn(async move {
            let Some(out) = read.await.ok().filter(|out| out.success()) else {
                return Vec::new();
            };
            let text = String::from_utf8_lossy(&out.stdout);
            names
                .iter()
                .copied()
                .filter(|name| text.lines().any(|line| line.trim() == *name))
                .collect()
        })
    }

    /// 推送没成：仓库里装着 pre-push 钩子、又不像是远程拒绝的，按钩子未通过报，给「查看输出」和
    /// 「跳过钩子并推送」；别的照常报错。`push` 是这次推送的参数，跳过钩子时带上 `--no-verify` 再推；
    /// `pushed` 是推成以后通知里要的（文字、远程上的分支、远程），见 [`RepoView::notify_pushed`]。
    pub(super) fn push_failed(
        &mut self,
        detail: String,
        push: Vec<String>,
        pushed: (String, String, String),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !looks_like_hook(&detail) {
            return self.failure(tr("推送"), detail, window, cx);
        }
        let hooks = self.hooks_present(&["pre-push"], cx);
        cx.spawn_in(window, async move |this, cx| {
            let hooked = !hooks.await.is_empty();
            this.update_in(cx, |this, window, cx| {
                if !hooked {
                    return this.failure(tr("推送"), detail, window, cx);
                }
                let open = this.output_opener(cx);
                let view = cx.entity().downgrade();
                let note = ui::actions_note(
                    NotificationType::Warning,
                    Some(tr("推送未完成：pre-push 钩子未通过").into()),
                    tr("提交尚未推送。可查看钩子的输出，修正后重新推送。"),
                    move |_, _, cx| {
                        let me = cx.entity().downgrade();
                        let mut buttons = Vec::new();
                        if let Some(open) = open.clone() {
                            buttons.push(
                                Button::new("hook-output")
                                    .label(tr("查看输出"))
                                    .on_click(move |_, window, cx| open(window, cx)),
                            );
                        }
                        let (view, push, pushed) = (view.clone(), push.clone(), pushed.clone());
                        buttons.push(
                            Button::new("hook-skip-push")
                                .label(tr("跳过钩子并推送…"))
                                .on_click(move |_, window, cx| {
                                    me.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                                    let (push, pushed) = (push.clone(), pushed.clone());
                                    view.update(cx, |this, cx| {
                                        this.confirm_skip_push_hook(push, pushed, window, cx)
                                    })
                                    .ok();
                                }),
                        );
                        buttons
                    },
                )
                .id::<HookNote>();
                window.push_notification(note, cx);
            })
            .ok();
        })
        .detach();
    }

    fn confirm_skip_push_hook(
        &mut self,
        push: Vec<String>,
        pushed: (String, String, String),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let description =
            tr("本次推送不运行 pre-push 钩子（--no-verify）。仅此一次，之后的推送照常运行钩子。");
        self.confirm_plain(
            tr("跳过钩子并推送？"),
            description,
            tr("跳过钩子并推送"),
            window,
            cx,
            move |this, window, cx| {
                let mut push = push.clone();
                push.push("--no-verify".into());
                let (text, branch, remote) = pushed.clone();
                this.run_net_op(
                    tr("推送"),
                    push,
                    window,
                    cx,
                    move |this, _, window, cx| {
                        this.notify_pushed(text, branch, remote, window, cx);
                    },
                );
            },
        );
    }

    /// 提交没成，仓库里装着钩子：按钩子未通过报。提交说明还在框里；钩子（格式化工具）可能已经改了
    /// 文件，提醒去看。「查看输出」看钩子说了什么，「跳过钩子并提交」确认后带 `--no-verify` 再提交。
    pub(super) fn hook_failed(
        &mut self,
        hooks: Vec<&'static str>,
        commit: Commit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let names = hooks.join(tr("、"));
        let title = tf!("提交未完成：{names} 钩子未通过", names = &names);
        let text = tr("提交说明已保留。钩子可能已改动文件，请查看「本地修改」。");
        let open = self.output_opener(cx);
        let view = cx.entity().downgrade();
        let note = ui::actions_note(
            NotificationType::Warning,
            Some(title.into()),
            text,
            move |_, _, cx| {
                let me = cx.entity().downgrade();
                let mut buttons = Vec::new();
                if let Some(open) = open.clone() {
                    buttons.push(
                        Button::new("hook-output")
                            .label(tr("查看输出"))
                            .on_click(move |_, window, cx| open(window, cx)),
                    );
                }
                let (view, names, commit) = (view.clone(), names.clone(), commit.clone());
                buttons.push(
                    Button::new("hook-skip")
                        .label(tr("跳过钩子并提交…"))
                        .on_click(move |_, window, cx| {
                            me.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                            let (names, commit) = (names.clone(), commit.clone());
                            view.update(cx, |this, cx| {
                                this.confirm_skip_hooks(names, commit, window, cx)
                            })
                            .ok();
                        }),
                );
                buttons
            },
        )
        .id::<HookNote>();
        window.push_notification(note, cx);
    }

    /// 跳过钩子要确认：说清只这一次。确认后照现在提交框里的说明、暂存区再提交一遍。
    fn confirm_skip_hooks(
        &mut self,
        names: String,
        commit: Commit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let description = tf!(
            "本次提交不运行 {names} 钩子（--no-verify）。仅此一次，之后的提交照常运行钩子。",
            names
        );
        self.confirm_plain(
            tr("跳过钩子并提交？"),
            description,
            tr("跳过钩子并提交"),
            window,
            cx,
            move |this, window, cx| this.commit_then(commit.push, true, window, cx),
        );
    }

    /// 「并入之前的提交…」：把已暂存的改动补进更早的某个提交。`preset` 是先选中的提交（提交的右键
    /// 菜单进来时）。
    pub(super) fn fixup_dialog(
        &mut self,
        preset: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(running) = &self.op {
            window.push_notification(
                Notification::warning(tf!("请等待「{running}」完成后重试", running)),
                cx,
            );
            return;
        }
        if let Some(state) = self.in_progress {
            let text = tf!(
                "{name}尚未完成，请先完成或中止。",
                name = self.op_name(state)
            );
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let staged = self.status.as_ref().map_or(0, |s| {
            s.entries.iter().filter(|e| e.staged.is_some()).count()
        });
        if staged == 0 {
            let text = tr("没有已暂存的改动。请先暂存要并入的改动。");
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let count = format!("-{FIXUP_LISTED}");
        let list = channel.git(
            &self.entry.path,
            args(&[
                "log",
                &count,
                "--first-parent",
                "-z",
                "--format=%H%x1f%P%x1f%an%x1f%at%x1f%s",
                "HEAD",
                "--",
            ]),
        );
        // 没推送过的：其余的已推送
        let unpushed = channel.git(
            &self.entry.path,
            args(&["rev-list", &count, "HEAD", "--not", "--remotes", "--"]),
        );
        let view = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let (list, unpushed) = futures::join!(list, unpushed);
            let unpushed: HashSet<String> = unpushed
                .and_then(|out| out.ok())
                .map(|out| {
                    String::from_utf8_lossy(&out)
                        .lines()
                        .map(|line| line.trim().to_string())
                        .collect()
                })
                .unwrap_or_default();
            let rows = list
                .and_then(|out| out.ok())
                .map(|out| parse_fix_rows(&out, &unpushed))
                .unwrap_or_default();
            cx.update(|window, cx| {
                if rows.is_empty() {
                    let text = tr("读取提交失败，无法选择要并入的提交。");
                    window.push_notification(Notification::warning(text), cx);
                    return;
                }
                let selected = preset
                    .as_ref()
                    .and_then(|id| rows.iter().position(|row| row.id == *id))
                    .filter(|ix| rows[*ix].blocked.is_none());
                if preset.is_some() && selected.is_none() {
                    let text = tr("此提交不在当前分支最近的提交中，或其后有合并提交，无法并入。");
                    window.push_notification(Notification::warning(text).autohide(false), cx);
                    return;
                }
                let picker = cx.new(|_| FixupPicker {
                    repo: view.clone(),
                    rows,
                    selected,
                    staged,
                });
                window.open_dialog(cx, move |dialog, _, _| {
                    dialog
                        .title(tr("并入之前的提交"))
                        .w(px(720.))
                        .child(picker.clone())
                });
            })
            .ok();
        })
        .detach();
    }

    /// 并入：先做一个 fixup 提交，再让 git 按 `--autosquash` 把它并进去。未暂存的改动由
    /// `--autostash` 先收起来、做完放回。
    fn run_fixup(
        &mut self,
        id: String,
        subject: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        // 所选提交没有父提交（这条历史上最早的那个）：变基从头来
        let parent = format!("{id}^");
        let has_parent = channel.git(
            &self.entry.path,
            args(&["rev-parse", "-q", "--verify", &parent]),
        );
        let checks = self.head_checks();
        let point = self.undo_point().map(|point| point.soft());
        cx.spawn_in(window, async move |this, cx| {
            let has_parent = has_parent.await.is_ok_and(|out| out.success());
            this.update_in(cx, |this, window, cx| {
                let fixup = format!("--fixup={id}");
                let commit = Step::local(args(&["commit", "-q", &fixup])).live();
                let mut rebase = args(&[
                    "-c",
                    "sequence.editor=true",
                    "rebase",
                    "-i",
                    "--autosquash",
                    "--autostash",
                ]);
                let shown = match has_parent {
                    true => {
                        rebase.push(parent.clone());
                        format!("git rebase -i --autosquash --autostash {parent}")
                    }
                    false => {
                        rebase.push("--root".into());
                        "git rebase -i --autosquash --autostash --root".to_string()
                    }
                };
                let steps = vec![commit, Step::local(rebase).shown(shown)];
                this.verify_then(checks, window, cx, move |this, window, cx| {
                    this.run_seq(
                        tr("并入之前的提交"),
                        steps,
                        window,
                        cx,
                        move |this, _, window, cx| {
                            let text = tf!(
                                "已并入 {id}「{subject}」",
                                id = short(&id),
                                subject = subject.as_str()
                            );
                            this.notify_undoable(text, point, window, cx);
                        },
                    );
                });
            })
            .ok();
        })
        .detach();
    }
}

/// 推送失败的输出像不像是本机的钩子拦下的：远程拒绝（`[rejected]`、`[remote rejected]`）、连不上、
/// 没权限这些（git 自己说的 `fatal:`）不算。这几个词不随语言变。
fn looks_like_hook(detail: &str) -> bool {
    let rejected = detail.contains("[rejected]") || detail.contains("[remote rejected]");
    let fatal = detail
        .lines()
        .any(|line| line.trim_start().starts_with("fatal:"));
    !rejected && !fatal
}

/// 解析候选的提交（新的在前）。遇到合并提交起，它和它之前的都不能选：并入要把所选提交之后的提交
/// 重放一遍，重放不了合并提交。
fn parse_fix_rows(out: &[u8], unpushed: &HashSet<String>) -> Vec<FixRow> {
    let mut blocked = None;
    String::from_utf8_lossy(out)
        .split('\0')
        .filter_map(|record| {
            let record = record.trim_start_matches('\n');
            let mut fields = record.splitn(5, '\x1f');
            let id = fields.next().filter(|id| !id.is_empty())?.to_string();
            let parents = fields.next()?.split_ascii_whitespace().count();
            let author = fields.next()?;
            let time: i64 = fields.next()?.parse().unwrap_or(0);
            let subject = fields.next().unwrap_or("").to_string();
            if parents > 1 && blocked.is_none() {
                blocked = Some(tr("合并提交之前的提交无法并入"));
            }
            Some(FixRow {
                unpushed: unpushed.contains(&id),
                note: format!("{author} · {}", super::history::format_date(time)),
                blocked: match parents > 1 {
                    true => Some(tr("合并提交")),
                    false => blocked,
                },
                id,
                subject,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{looks_like_hook, parse_fix_rows};
    use std::collections::HashSet;

    #[test]
    fn 远程拒绝和连不上不算钩子拦下的() {
        assert!(looks_like_hook(
            "lint failed\nerror: failed to push some refs to 'origin'"
        ));
        assert!(!looks_like_hook(
            " ! [rejected]        main -> main (non-fast-forward)\nerror: failed to push some refs"
        ));
        assert!(!looks_like_hook(
            " ! [remote rejected] main -> main (pre-receive hook declined)"
        ));
        assert!(!looks_like_hook(
            "fatal: unable to access 'https://h/': Could not resolve host: h"
        ));
    }

    #[test]
    fn 合并提交和它之前的不能选() {
        let out = "c3\x1fb2\x1f张伟\x1f1700000200\x1f第三个\0\
                   b2\x1fa1 x9\x1fAlice\x1f1700000100\x1f合并 x\0\
                   a1\x1f\x1fAlice\x1f1700000000\x1f最早的\0";
        let unpushed: HashSet<String> = ["c3".to_string()].into();
        let rows = parse_fix_rows(out.as_bytes(), &unpushed);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].blocked.is_none() && rows[0].unpushed);
        assert_eq!(rows[1].blocked, Some("合并提交"));
        assert_eq!(rows[2].blocked, Some("合并提交之前的提交无法并入"));
        assert!(!rows[2].unpushed);
        assert_eq!(rows[2].subject, "最早的");
    }
}
