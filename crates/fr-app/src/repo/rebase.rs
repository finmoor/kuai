//! 交互式变基：把当前分支上某个提交之后的那些提交整理一遍——调顺序、改说明、几个并成一个、丢掉。
//!
//! 在提交列表里一个提交上点右键进来，那个提交是起点（它自己不动），它之后到 HEAD 的提交列在
//! 对话框里，新的在上，和提交列表一个顺序。排好以后交给 `git rebase -i`，清单怎么送进去见
//! [`fr_core::rebase`]。中途遇到冲突和普通的变基一样：停下来，工具栏下面出横幅，解决后继续。

use std::collections::HashSet;

use fr_core::i18n::{tr, tr_key, trc};
use fr_core::rebase::{self, Action, Candidate, Item, Problem};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use super::impact::{Row as ImpactRow, Tone, rows_box};
use super::ops::Step;
use crate::ui;

/// 一次最多整理这么多个提交：再多对话框里也看不过来
const MAX_COMMITS: usize = 200;

/// 对一个提交做什么（界面上的选项；改说明的新内容在那一行自己的输入框里）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Pick,
    Reword,
    Squash,
    Fixup,
    Drop,
}

impl Kind {
    const ALL: [Kind; 5] = [
        Kind::Pick,
        Kind::Reword,
        Kind::Squash,
        Kind::Fixup,
        Kind::Drop,
    ];

    fn label(self) -> &'static str {
        match self {
            Kind::Pick => tr("保留"),
            Kind::Reword => tr("修改说明"),
            Kind::Squash => tr("并入下方提交"),
            Kind::Fixup => tr("并入下方提交并丢弃说明"),
            // 英文里别处的「丢弃」是 Discard（丢掉没提交的改动），变基的这一项叫 Drop
            Kind::Drop => trc("变基", "丢弃"),
        }
    }

    /// 菜单里的说明。
    fn detail(self) -> &'static str {
        tr(match self {
            Kind::Pick => tr_key("保留（pick）：保持原样"),
            Kind::Reword => tr_key("修改说明（reword）：改动不变，仅修改提交说明"),
            Kind::Squash => tr_key("并入下方提交（squash）：保留两者的说明"),
            Kind::Fixup => tr_key("并入下方提交并丢弃说明（fixup）：仅保留下方提交的说明"),
            Kind::Drop => tr_key("丢弃（drop）：删除此提交及其改动"),
        })
    }
}

struct Row {
    commit: Candidate,
    kind: Kind,
    /// 改说明时的输入框
    message: Option<Entity<TextareaState>>,
}

/// 交互式变基的对话框。
pub(super) struct RebaseEditor {
    repo: WeakEntity<RepoView>,
    /// 起点：它之后的提交才动
    base: String,
    /// 要整理的提交，新的在前
    rows: Vec<Row>,
    /// 打开时的顺序，用来看是不是什么都没改
    original: Vec<String>,
    problem: Option<SharedString>,
    /// 已推送的提交（在某个远程分支上）：标出来，整理后推送要强推
    pushed: HashSet<String>,
}

impl RebaseEditor {
    fn set_kind(&mut self, ix: usize, kind: Kind, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get_mut(ix) else {
            return;
        };
        row.kind = kind;
        if kind == Kind::Reword && row.message.is_none() {
            let text = row.commit.message.clone();
            let state = cx.new(|cx| TextareaState::new(window, cx));
            state.update(cx, |state, cx| state.set_value(text, window, cx));
            state.focus_handle(cx).focus(window, cx);
            row.message = Some(state);
        }
        self.problem = None;
        cx.notify();
    }

    /// 往上（更新）或往下（更早）挪一行。
    fn move_row(&mut self, ix: usize, up: bool, cx: &mut Context<Self>) {
        let to = if up {
            ix.checked_sub(1)
        } else {
            (ix + 1 < self.rows.len()).then_some(ix + 1)
        };
        if let Some(to) = to {
            self.rows.swap(ix, to);
            self.problem = None;
            cx.notify();
        }
    }

    /// 排好的清单，旧的在前（git 要的顺序）。
    fn items(&self, cx: &App) -> Vec<Item> {
        self.rows
            .iter()
            .rev()
            .map(|row| Item {
                id: row.commit.id.clone(),
                action: match row.kind {
                    Kind::Pick => Action::Pick,
                    Kind::Squash => Action::Squash,
                    Kind::Fixup => Action::Fixup,
                    Kind::Drop => Action::Drop,
                    Kind::Reword => Action::Reword(
                        row.message
                            .as_ref()
                            .map(|state| state.read(cx).value().to_string())
                            .unwrap_or_else(|| row.commit.message.clone()),
                    ),
                },
            })
            .collect()
    }

    /// 什么都没改：全是「保留」，顺序也没动。
    fn untouched(&self) -> bool {
        self.rows.iter().all(|row| row.kind == Kind::Pick)
            && self
                .rows
                .iter()
                .map(|row| &row.commit.id)
                .eq(self.original.iter())
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 什么都没改就点开始：变基一遍也是原样，不做了
        if self.untouched() {
            window.close_dialog(cx);
            let note = tr("顺序和操作均未更改，未执行变基。");
            window.push_notification(Notification::info(note), cx);
            return;
        }
        let items = self.items(cx);
        if let Err(problem) = rebase::check(&items) {
            self.problem = Some(
                match problem {
                    Problem::NothingLeft => tr("不能丢弃全部提交，至少保留一个。"),
                    Problem::NothingToMeldInto => {
                        tr("最早的提交无法「并入下方提交」：下方没有提交。")
                    }
                    Problem::EmptyMessage => tr("有提交选择了「修改说明」，但说明为空。"),
                }
                .into(),
            );
            cx.notify();
            return;
        }
        window.close_dialog(cx);
        let (base, original) = (self.base.clone(), self.original.clone());
        self.repo
            .update(cx, |repo, cx| {
                repo.run_interactive_rebase(base, items, original, window, cx)
            })
            .ok();
    }
}

impl Render for RebaseEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list_height = (window.viewport_size().height - px(300.)).clamp(px(140.), px(520.));
        let dirty = self
            .repo
            .upgrade()
            .is_some_and(|repo| repo.read(cx).has_tracked_changes());
        let theme = cx.theme();
        let warning = theme.warning;
        let (muted, border, danger, panel) = (
            theme.muted_foreground,
            theme.border,
            theme.danger,
            theme.muted,
        );
        let count = self.rows.len();
        let rows: Vec<AnyElement> = self
            .rows
            .iter()
            .enumerate()
            .map(|(ix, row)| {
                let kind = row.kind;
                let last = ix + 1 == count;
                let view = cx.entity().downgrade();
                let dropped = kind == Kind::Drop;
                let melded = matches!(kind, Kind::Squash | Kind::Fixup);
                let head = h_flex()
                    .h(px(32.))
                    .gap_2()
                    .child(
                        Button::new(("rebase-up", ix))
                            .xsmall()
                            .ghost()
                            .icon(ui::icon_small(IconName::ArrowUp))
                            .tooltip(tr("上移：移到较新的位置"))
                            .disabled(ix == 0)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.move_row(ix, true, cx)),
                            ),
                    )
                    .child(
                        Button::new(("rebase-down", ix))
                            .xsmall()
                            .ghost()
                            .icon(ui::icon_small(IconName::ArrowDown))
                            .tooltip(tr("下移：移到较早的位置"))
                            .disabled(last)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.move_row(ix, false, cx)),
                            ),
                    )
                    // 不加悬停提示：提示出现在按钮下面，会盖住弹出的菜单
                    .child(
                        div().w(px(176.)).flex_none().child(
                            Button::new(("rebase-kind", ix))
                                .small()
                                .outline()
                                .w_full()
                                .label(kind.label())
                                .dropdown_caret(true)
                                .dropdown_menu(move |mut menu, _, _| {
                                    for choice in Kind::ALL {
                                        let view = view.clone();
                                        menu = menu.item(
                                            PopupMenuItem::new(choice.detail())
                                                .checked(choice == kind)
                                                .on_click(move |_, window, cx| {
                                                    view.update(cx, |this, cx| {
                                                        this.set_kind(ix, choice, window, cx)
                                                    })
                                                    .ok();
                                                }),
                                        );
                                    }
                                    menu
                                }),
                        ),
                    )
                    .child(
                        div()
                            .w(px(64.))
                            .flex_none()
                            .font_family(crate::fonts::mono())
                            .text_xs()
                            .text_color(muted)
                            .child(row.commit.id.chars().take(8).collect::<String>()),
                    )
                    .when(self.pushed.contains(&row.commit.id), |line| {
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
                    .when(melded, |line| {
                        line.child(ui::icon_small(IconName::CornerRightDown).text_color(muted))
                    })
                    .child(
                        ui::clipped(("subject", ix), row.commit.subject().to_string())
                            .flex_1()
                            .text_sm()
                            .when(dropped, |d| d.line_through().text_color(muted)),
                    )
                    .child(
                        ui::clipped(
                            ("author", ix),
                            format!(
                                "{} · {}",
                                row.commit.author,
                                super::history::format_date(row.commit.time)
                            ),
                        )
                        .flex_none()
                        .max_w(px(150.))
                        .text_xs()
                        .text_color(muted),
                    );
                let editor = row
                    .message
                    .as_ref()
                    .filter(|_| kind == Kind::Reword)
                    .map(|state| {
                        div()
                            .pl(px(60.))
                            .pb_2()
                            .child(Textarea::new(state).h(px(84.)))
                    });
                v_flex()
                    .px_2()
                    .when(ix > 0, |d| d.border_t_1().border_color(border))
                    .child(head)
                    .children(editor)
                    .into_any_element()
            })
            .collect();
        let dropped = self.rows.iter().filter(|r| r.kind == Kind::Drop).count();
        let melded = self
            .rows
            .iter()
            .filter(|r| matches!(r.kind, Kind::Squash | Kind::Fixup))
            .count();
        let reworded = self.rows.iter().filter(|r| r.kind == Kind::Reword).count();
        let summary = tf!(
            "{count} 个提交：并入 {melded} 个，修改说明 {reworded} 个，丢弃 {dropped} 个；完成后剩余 {left} 个。",
            count,
            melded,
            reworded,
            dropped,
            left = count - melded - dropped
        );
        let pushed = self.pushed.len();
        let mut impact = Vec::new();
        if pushed > 0 {
            impact.push(ImpactRow::new(
                Tone::Warn,
                IconName::ArrowUp,
                "已推送",
                tf!("{pushed} 个已推送，完成后推送要强制推送。", pushed),
            ));
        }
        impact.push(ImpactRow::new(
            Tone::Good,
            IconName::ArchiveRestore,
            "怎么找回",
            tr("整理前的位置记在侧栏的「找回提交」里。"),
        ));
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .whitespace_normal()
                    .child(tr(
                        "最新的提交在最上方。用箭头调整顺序，在下拉列表中选择操作。",
                    )),
            )
            // 按现在的状态说：对话框开着时工作区可能变了
            .when(dirty, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .whitespace_normal()
                        .child(tr("未提交的改动将先贮藏，完成后自动恢复（--autostash）。")),
                )
            })
            .child(
                div()
                    .id("rebase-list")
                    .max_h(list_height)
                    .overflow_y_scroll()
                    .rounded(px(ui::RADIUS))
                    .border_1()
                    .border_color(border)
                    .bg(panel.opacity(0.35))
                    .child(v_flex().children(rows)),
            )
            .child(rows_box(&impact, cx))
            .children(
                self.problem
                    .clone()
                    .map(|problem| div().text_sm().text_color(danger).child(problem)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(muted)
                            .child(summary),
                    )
                    .child(
                        Button::new("rebase-cancel")
                            .label(tr("取消"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("rebase-start")
                            .primary()
                            .label(tr("开始变基"))
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

impl RepoView {
    /// 打开交互式变基：整理 `base` 之后到 HEAD 的提交。先确认 `base` 在当前分支的历史上。
    pub(super) fn interactive_rebase(
        &mut self,
        base: String,
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
            let text = tf!("{name}尚未完成，请先完成或中止。", name = state.name());
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        let is_ancestor = channel.git(
            &repo,
            vec![
                "merge-base".into(),
                "--is-ancestor".into(),
                base.clone(),
                "HEAD".into(),
            ],
        );
        let list = channel.git(&repo, rebase::list_args(&base));
        // 没推送过的：其余的已推送
        let unpushed = channel.git(
            &repo,
            vec![
                "rev-list".into(),
                format!("{base}..HEAD"),
                "--not".into(),
                "--remotes".into(),
                "--".into(),
            ],
        );
        let view = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let (is_ancestor, list, unpushed) = futures::join!(is_ancestor, list, unpushed);
            let unpushed: Option<HashSet<String>> = unpushed.and_then(|out| out.ok()).ok().map(|out| {
                String::from_utf8_lossy(&out)
                    .lines()
                    .map(|line| line.trim().to_string())
                    .collect()
            });
            cx.update(|window, cx| {
                let on_branch = is_ancestor.is_ok_and(|out| out.success());
                let commits = list
                    .and_then(|out| out.ok())
                    .map(|out| rebase::parse_list(&out))
                    .unwrap_or_default();
                // 清单里的 pick 重放不了合并提交（见 fr_core::rebase）
                let merges = commits.iter().filter(|c| c.parents > 1).count();
                let problem = if !on_branch {
                    Some(tr("此提交不在当前分支上。请先切换到包含该提交的分支。").to_string())
                } else if commits.is_empty() {
                    Some(tr("此提交之后没有其他提交，无需整理。").to_string())
                } else if merges > 0 {
                    Some(tf!(
                        "此提交之后有 {merges} 个合并提交，整理时无法保留合并关系。请选择最后一次合并之后的提交。",
                        merges
                    ))
                } else if commits.len() > MAX_COMMITS {
                    Some(tf!(
                        "此提交之后有 {n} 个提交，一次最多整理 {max} 个。请选择更近的提交。",
                        n = commits.len(),
                        max = MAX_COMMITS
                    ))
                } else {
                    None
                };
                if let Some(problem) = problem {
                    window.push_notification(Notification::warning(problem).autohide(false), cx);
                    return;
                }
                let original: Vec<String> = commits.iter().map(|c| c.id.clone()).collect();
                // 读不出来就当都没推送（不标）
                let pushed: HashSet<String> = match &unpushed {
                    Some(unpushed) => original
                        .iter()
                        .filter(|id| !unpushed.contains(*id))
                        .cloned()
                        .collect(),
                    None => HashSet::new(),
                };
                let short: String = base.chars().take(8).collect();
                let editor = cx.new(|_| RebaseEditor {
                    repo: view.clone(),
                    base: base.clone(),
                    rows: commits
                        .into_iter()
                        .map(|commit| Row {
                            commit,
                            kind: Kind::Pick,
                            message: None,
                        })
                        .collect(),
                    original,
                    problem: None,
                    pushed,
                });
                window.open_dialog(cx, move |dialog, _, _| {
                    dialog
                        .title(tf!("整理 {short} 之后的提交", short = &short))
                        .w(px(860.))
                        .child(editor.clone())
                });
            })
            .ok();
        })
        .detach();
    }

    /// `original` 是打开对话框时读到的那几个提交（新的在前，和 `git rev-list` 一样）：开始之前核对
    /// 现在还是这几个。对话框开着时别处又提交了，清单里没有新的那个，原来照旧清单变基，它就从分支上
    /// 掉了（reflog 里还找得到）。
    fn run_interactive_rebase(
        &mut self,
        base: String,
        items: Vec<Item>,
        original: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 参数里总带着 --autostash：不按打开对话框时的状态决定，开着的这一会儿工作区可能变了
        let step =
            Step::input(rebase::args(&base), rebase::input(&items)).shown(rebase::shown(&base));
        let kept = items
            .iter()
            .filter(|item| matches!(item.action, Action::Pick | Action::Reword(_)))
            .count();
        let listed = vec!["rev-list".into(), format!("{base}..HEAD"), "--".into()];
        let checks = vec![(listed, Some(original.join("\n")))];
        self.verify_then(checks, window, cx, move |this, window, cx| {
            this.run_seq(tr("变基"), vec![step], window, cx, move |_, _, w, cx| {
                let text = tf!("整理完成，该范围现有 {kept} 个提交", kept);
                w.push_notification(Notification::success(text), cx);
            });
        });
    }
}
