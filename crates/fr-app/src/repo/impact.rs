//! 改写历史、丢改动之前的确认框里「会怎样」的那几行：分支怎么动、哪些提交受影响（标出已推送的）、
//! 哪些改动会丢、怎么找回（原型见 `docs/design/reliability/`）。

use std::collections::HashSet;
use std::rc::Rc;

use fr_channel::{Channel, GitOutput};
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use crate::ui;

/// 确认框里最多列几个提交，再多写「还有 N 个…」
pub(super) const MAX_LISTED: usize = 5;

/// 一行是好事、要留意还是会丢东西：决定图标的颜色。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Tone {
    Plain,
    Warn,
    Bad,
    Good,
}

/// 列出来的一个提交。
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Commit {
    pub id: String,
    pub subject: String,
    /// 在某个远程分支上（已经推送过）
    pub pushed: bool,
    /// 写在后面的灰字（作者、时间）
    pub note: Option<String>,
}

#[derive(Clone)]
pub(super) struct Row {
    pub tone: Tone,
    pub icon: IconName,
    pub key: SharedString,
    pub text: SharedString,
    pub commits: Vec<Commit>,
    /// 没列出来的还有几个
    pub more: usize,
}

impl Row {
    pub(super) fn new(
        tone: Tone,
        icon: IconName,
        key: &'static str,
        text: impl Into<SharedString>,
    ) -> Self {
        Self {
            tone,
            icon,
            key: tr(key).into(),
            text: text.into(),
            commits: Vec::new(),
            more: 0,
        }
    }

    /// 列出这一段提交的前几个。
    pub(super) fn listed(mut self, listed: &Listed) -> Self {
        self.commits = listed.commits.clone();
        self.more = listed.total.saturating_sub(self.commits.len());
        self
    }
}

/// 确认框的说明：一句话，下面几行「会怎样」。
#[derive(Clone)]
pub(super) struct Impact {
    pub description: SharedString,
    pub rows: Vec<Row>,
}

/// 「怎么找回」那一行常用的话：提交还在「找回提交」里。
pub(super) fn find_row() -> Row {
    Row::new(
        Tone::Good,
        IconName::ArchiveRestore,
        "怎么找回",
        tr("提交可在侧栏的「找回提交」中找到，保留 30 天。"),
    )
}

/// 一段提交（比如重置时离开分支的那些）：一共几个、其中几个已推送、前几个的样子。
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Listed {
    pub commits: Vec<Commit>,
    pub total: usize,
    pub pushed: usize,
}

impl Listed {
    /// 「N 个提交，其中 M 个已推送」。
    pub(super) fn count_text(&self) -> String {
        let (total, pushed) = (self.total, self.pushed);
        match pushed {
            0 => tf!("{total} 个提交", total),
            _ => tf!("{total} 个提交，其中 {pushed} 个已推送", total, pushed),
        }
    }
}

/// 未推送的最多读这么多个：没有远程的仓库里一段提交可能上千个，再多的当作没推送
const UNPUSHED_MAX: usize = 1000;

/// 读 `range`（`git rev-list` 的写法，比如 `["abc..HEAD"]`）里的提交，新的在前。`author` 时后面写上
/// 作者和时间（灰字）。`check_pushed` 时数出其中几个已推送（`range`
/// 里自己带了 `--not` 的不能用：后面再加的 `--not --remotes` 会把意思翻过来）。读不出来的算 0 个。
pub(super) async fn list_commits(
    channel: Channel,
    repo: String,
    range: Vec<String>,
    author: bool,
    check_pushed: bool,
) -> Listed {
    let run = |head: &[&str], tail: &[&str]| {
        let mut args: Vec<String> = head.iter().map(|arg| arg.to_string()).collect();
        args.extend(range.iter().cloned());
        args.extend(tail.iter().map(|arg| arg.to_string()));
        args.push("--".into());
        channel.git(&repo, args)
    };
    let text = |out: anyhow::Result<GitOutput>| -> String {
        out.ok()
            .filter(GitOutput::success)
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default()
    };
    let total: usize = text(run(&["rev-list", "--count"], &[]).await)
        .trim()
        .parse()
        .unwrap_or(0);
    if total == 0 {
        return Listed::default();
    }
    let max = format!("--max-count={UNPUSHED_MAX}");
    let unpushed = match check_pushed {
        true => parse_ids(&text(
            run(&["rev-list", &max], &["--not", "--remotes"]).await,
        )),
        false => HashSet::new(),
    };
    let format = match author {
        true => "--format=%H%x00%s%x00%an%x00%ct",
        false => "--format=%H%x00%s",
    };
    let listed = format!("--max-count={MAX_LISTED}");
    let log = text(run(&["log", "--no-color", format, &listed], &[]).await);
    let known = check_pushed && unpushed.len() < UNPUSHED_MAX;
    let commits = parse_commits(&log, |id| known && !unpushed.contains(id));
    let pushed = match known {
        true => total.saturating_sub(unpushed.len()),
        false => 0,
    };
    Listed {
        commits,
        total,
        pushed,
    }
}

/// 读 `git log --format=%H%x00%s%x00%an%x00%ct` 的输出（后两段可以没有）：每行用 NUL 隔开的哈希、
/// 说明、作者、时间，作者和时间拼成灰字。`pushed` 说这个哈希在不在远程分支上。
fn parse_commits(out: &str, pushed: impl Fn(&str) -> bool) -> Vec<Commit> {
    out.lines()
        .filter_map(|line| {
            let mut parts = line.split('\0');
            let id = parts.next()?.trim();
            if id.is_empty() {
                return None;
            }
            let subject = parts.next().unwrap_or_default().to_string();
            let author = parts.next().filter(|author| !author.is_empty());
            let time = parts
                .next()
                .and_then(|time| time.trim().parse::<i64>().ok());
            let note = match (author, time) {
                (Some(author), Some(time)) => {
                    Some(format!("{author} · {}", super::history::format_time(time)))
                }
                (Some(author), None) => Some(author.to_string()),
                _ => None,
            };
            Some(Commit {
                pushed: pushed(id),
                id: id.to_string(),
                subject,
                note,
            })
        })
        .collect()
}

/// 一列哈希（每行一个）。
fn parse_ids(out: &str) -> HashSet<String> {
    out.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn render(impact: &Impact, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    v_flex()
        .gap(px(10.))
        .child(
            div()
                .text_sm()
                .text_color(muted)
                .whitespace_normal()
                .child(impact.description.clone()),
        )
        .child(rows_box(&impact.rows, cx))
        .into_any_element()
}

/// 「会怎样」那几行，带一圈边框（交互式变基的对话框里也用）。
pub(super) fn rows_box(rows: &[Row], cx: &App) -> Div {
    let theme = cx.theme();
    let (muted, border, fg) = (theme.muted_foreground, theme.border, theme.foreground);
    let color = |tone: Tone| match tone {
        Tone::Plain => theme.muted_foreground,
        Tone::Warn => theme.warning,
        Tone::Bad => theme.danger,
        Tone::Good => theme.success,
    };
    let warning = theme.warning;
    let rows = rows.iter().map(|row| {
        let commits = row.commits.iter().take(MAX_LISTED).map(|commit| {
            h_flex()
                .gap(px(6.))
                .min_w_0()
                .text_xs()
                .child(
                    div()
                        .flex_none()
                        .font_family("Consolas")
                        .text_color(muted)
                        .child(commit.id.chars().take(8).collect::<String>()),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_shrink_1()
                        .truncate()
                        .text_color(fg)
                        .child(commit.subject.clone()),
                )
                .when(commit.pushed, |line| {
                    line.child(
                        div()
                            .flex_none()
                            .px(px(5.))
                            .rounded(px(4.))
                            .border_1()
                            .border_color(warning.opacity(0.5))
                            .text_color(warning)
                            .child(tr("已推送")),
                    )
                })
                .when_some(commit.note.clone(), |line, note| {
                    line.child(
                        div()
                            .flex_none()
                            .ml_auto()
                            .pl(px(8.))
                            .text_color(muted)
                            .child(note),
                    )
                })
        });
        let more = row.more + row.commits.len().saturating_sub(MAX_LISTED);
        h_flex()
            .items_start()
            .gap(px(8.))
            .child(
                div()
                    .flex_none()
                    .pt(px(2.))
                    .child(ui::icon_small(row.icon).text_color(color(row.tone))),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(72.))
                    .text_sm()
                    .text_color(muted)
                    .child(row.key.clone()),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(3.))
                    .child(
                        div()
                            .text_sm()
                            .text_color(fg)
                            .whitespace_normal()
                            .child(row.text.clone()),
                    )
                    .children(commits)
                    .when(more > 0, |list| {
                        list.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(tf!("还有 {more} 个…", more)),
                        )
                    }),
            )
    });
    v_flex()
        .gap(px(8.))
        .p(px(10.))
        .rounded(px(6.))
        .border_1()
        .border_color(border)
        .children(rows)
}

/// 提交框里的一条提醒（黄色）：一句话，后面可以跟一个按钮。
pub(super) fn note(text: SharedString, action: Option<AnyElement>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    h_flex()
        .items_start()
        .gap(px(6.))
        .p(px(8.))
        .rounded(px(6.))
        .border_1()
        .border_color(theme.warning.opacity(0.45))
        .bg(theme.warning.opacity(0.08))
        .child(
            div()
                .flex_none()
                .pt(px(1.))
                .child(ui::icon_small(IconName::TriangleAlert).text_color(theme.warning)),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(6.))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.foreground)
                        .whitespace_normal()
                        .child(text),
                )
                .children(action),
        )
        .into_any_element()
}

/// 点了按钮以后在仓库视图上做的事。
pub(super) type Action = Rc<dyn Fn(&mut RepoView, &mut Window, &mut Context<RepoView>)>;

/// 确认框底下的一个按钮（「取消」另外加）。
pub(super) struct Choice {
    pub label: SharedString,
    pub variant: ButtonVariant,
    pub action: Action,
}

impl Choice {
    pub(super) fn new(
        label: impl Into<SharedString>,
        variant: ButtonVariant,
        action: impl Fn(&mut RepoView, &mut Window, &mut Context<RepoView>) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            variant,
            action: Rc::new(action),
        }
    }
}

impl RepoView {
    /// 带「会怎样」那几行的确认框。`danger` 时按钮是红的、回车不执行（同 `confirm`），否则回车等于
    /// 点确定（同 `confirm_plain`）。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn confirm_impact(
        &mut self,
        title: impl Into<SharedString>,
        impact: Impact,
        ok: &'static str,
        danger: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let variant = match danger {
            true => ButtonVariant::Danger,
            false => ButtonVariant::Primary,
        };
        let choice = Choice::new(ok, variant, action);
        self.choose_impact(title, impact, vec![choice], !danger, window, cx);
    }

    /// 带「会怎样」那几行、几个按钮的确认框：`choices` 按顺序排在「取消」后面。`enter` 时回车等于点
    /// 最后一个按钮。
    pub(super) fn choose_impact(
        &mut self,
        title: impl Into<SharedString>,
        impact: Impact,
        choices: Vec<Choice>,
        enter: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = title.into();
        let view = cx.entity().downgrade();
        let choices = Rc::new(choices);
        ui::cancel_pending_tip(window, cx);
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let click = |index: usize| {
                let (view, choices) = (view.clone(), choices.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    window.close_dialog(cx);
                    let action = choices[index].action.clone();
                    view.update(cx, |this, cx| action(this, window, cx)).ok();
                }
            };
            let last = choices.len().saturating_sub(1);
            let on_enter = click(last);
            let mut footer = DialogFooter::new().child(
                Button::new("cancel")
                    .label(tr("取消"))
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            );
            for (index, choice) in choices.iter().enumerate() {
                footer = footer.child(
                    Button::new(("choice", index))
                        .label(choice.label.clone())
                        .with_variant(choice.variant)
                        .on_click(click(index)),
                );
            }
            alert
                .width(px(600.))
                .title(title.clone())
                .description(render(&impact, cx))
                .on_ok(move |event, window, cx| {
                    if enter {
                        on_enter(event, window, cx);
                    }
                    false
                })
                .footer(footer)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{Commit, Listed, parse_commits, parse_ids};

    #[test]
    fn 读提交列表_标出已推送的() {
        let unpushed = parse_ids("aaa\n\n");
        let commits = parse_commits("aaa\0first\nbbb\0second\0Mia\0not a time\n", |id| {
            !unpushed.contains(id)
        });
        assert_eq!(
            commits,
            [
                Commit {
                    id: "aaa".into(),
                    subject: "first".into(),
                    pushed: false,
                    note: None
                },
                Commit {
                    id: "bbb".into(),
                    subject: "second".into(),
                    pushed: true,
                    note: Some("Mia".into())
                },
            ]
        );
        let listed = Listed {
            commits,
            total: 4,
            pushed: 1,
        };
        assert_eq!(listed.count_text(), "4 个提交，其中 1 个已推送");
        let none = Listed {
            total: 1,
            ..Listed::default()
        };
        assert_eq!(none.count_text(), "1 个提交");
    }
}
