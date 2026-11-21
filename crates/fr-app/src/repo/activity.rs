//! 活动记录：这个仓库里执行过的每条会改东西的 git 命令（提交、暂存、切换、合并……）和联网的
//! 命令（获取、拉取、推送，包括自动获取），各自的结果、用了多久、git 说了什么。
//!
//! 只读的命令（读状态、读历史、读改动）不记：它们每次刷新都跑一遍，记下来就把有用的淹没了。
//! 记录只在内存里，关掉标签页就没了；最多留 [`MAX_ENTRIES`] 条。

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use fr_channel::GitOutput;
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use crate::ui;

const MAX_ENTRIES: usize = 200;
/// 一条记录里 git 的输出最多留这么多字节（留末尾）
const MAX_OUTPUT: usize = 16 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Outcome {
    Running,
    Done,
    Failed,
    Cancelled,
}

/// 一条记录：一次 git 命令。
#[derive(Clone)]
pub(super) struct Entry {
    id: u64,
    at: chrono::DateTime<chrono::Local>,
    /// 属于哪个操作：「拉取」「提交」「自动获取」……
    label: SharedString,
    /// 完整的命令行
    command: SharedString,
    outcome: Outcome,
    elapsed: Option<Duration>,
    /// git 的输出，标准错误在前、标准输出在后
    output: SharedString,
}

#[derive(Default)]
pub(super) struct Log {
    entries: VecDeque<Entry>,
    next_id: u64,
}

impl Log {
    /// 开始一条命令，返回它的编号。
    /// 开始执行一条命令；`command` 是给人看的命令行（`git …`）。
    pub(super) fn begin(&mut self, label: SharedString, command: String) -> u64 {
        self.next_id += 1;
        if self.entries.len() >= MAX_ENTRIES {
            self.entries.pop_back();
        }
        self.entries.push_front(Entry {
            id: self.next_id,
            at: chrono::Local::now(),
            label,
            // 带凭据的远程地址（添加、修改远程时）不留在记录里，复制出去的也没有
            command: fr_core::git::redact_urls(&command).into_owned().into(),
            outcome: Outcome::Running,
            elapsed: None,
            output: SharedString::default(),
        });
        self.next_id
    }

    /// 一条命令结束了。
    pub(super) fn end(&mut self, id: u64, result: &anyhow::Result<GitOutput>, cancelled: bool) {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return;
        };
        let started = entry.at;
        entry.elapsed = (chrono::Local::now() - started).to_std().ok();
        match result {
            _ if cancelled => entry.outcome = Outcome::Cancelled,
            Ok(out) => {
                entry.outcome = if out.success() {
                    Outcome::Done
                } else {
                    Outcome::Failed
                };
                entry.output = fr_core::git::redact_urls(&output_text(out))
                    .into_owned()
                    .into();
            }
            Err(err) => {
                entry.outcome = Outcome::Failed;
                entry.output = fr_core::git::redact_urls(&format!("{err:#}"))
                    .into_owned()
                    .into();
            }
        }
    }

    fn running(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.outcome == Outcome::Running)
            .count()
    }
}

/// 记录里放的输出：标准错误（去掉用回车刷新掉的进度）加标准输出，太长只留末尾。
fn output_text(out: &GitOutput) -> String {
    let mut text = String::new();
    for line in out.stderr.lines() {
        let line = line.trim_end_matches('\r');
        let line = line.rsplit('\r').next().unwrap_or(line);
        if !line.trim().is_empty() {
            text.push_str(line);
            text.push('\n');
        }
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    text.push_str(stdout.trim_end());
    let text = text.trim_end();
    if text.len() <= MAX_OUTPUT {
        return text.to_string();
    }
    let mut start = text.len() - MAX_OUTPUT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…\n{}", &text[start..])
}

fn elapsed_text(elapsed: Duration) -> String {
    if elapsed.as_secs() >= 10 {
        format!("{} s", elapsed.as_secs())
    } else if elapsed.as_millis() >= 1000 {
        format!("{:.1} s", elapsed.as_secs_f64())
    } else {
        format!("{} ms", elapsed.as_millis())
    }
}

impl RepoView {
    /// 有几条命令正在执行（工具栏上「活动」按钮的角标）。
    pub(super) fn activity_running(&self) -> usize {
        self.activity.running()
    }

    /// 打开活动记录。
    pub(super) fn show_activity(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repo = cx.entity();
        let panel = cx.new(|cx| ActivityPanel {
            _observe: cx.observe(&repo, |_, _, cx| cx.notify()),
            repo: repo.downgrade(),
            open: HashSet::new(),
        });
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(tr("活动记录"))
                .w(px(760.))
                .child(panel.clone())
        });
    }
}

/// 活动记录的对话框。
struct ActivityPanel {
    repo: WeakEntity<RepoView>,
    /// 展开着看输出的那几条
    open: HashSet<u64>,
    _observe: Subscription,
}

impl Render for ActivityPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list_height = (window.viewport_size().height - px(260.)).clamp(px(160.), px(520.));
        let theme = cx.theme();
        let (muted, hover, border, success, danger, warning, panel) = (
            theme.muted_foreground,
            theme.list_hover,
            theme.border,
            theme.success,
            theme.danger,
            theme.warning,
            theme.muted,
        );
        let entries: Vec<Entry> = self
            .repo
            .upgrade()
            .map(|repo| repo.read(cx).activity.entries.iter().cloned().collect())
            .unwrap_or_default();
        if entries.is_empty() {
            return v_flex()
                .gap_2()
                .child(div().py_6().text_sm().text_color(muted).child(tr(
                    "暂无记录。提交、切换分支、获取、推送等操作将记录在此处。",
                )));
        }
        let rows: Vec<AnyElement> = entries
            .into_iter()
            .map(|entry| {
                let id = entry.id;
                let open = self.open.contains(&id);
                let glyph = match entry.outcome {
                    Outcome::Running => Spinner::new()
                        .icon(Icon::new(IconName::LoaderCircle))
                        .with_size(px(14.))
                        .into_any_element(),
                    Outcome::Done => ui::icon(IconName::CircleCheck)
                        .text_color(success)
                        .into_any_element(),
                    Outcome::Failed => ui::icon(IconName::CircleX)
                        .text_color(danger)
                        .into_any_element(),
                    Outcome::Cancelled => ui::icon(IconName::CircleSlash)
                        .text_color(warning)
                        .into_any_element(),
                };
                let state: SharedString = match (entry.outcome, entry.elapsed) {
                    (Outcome::Running, _) => tr("正在执行…").into(),
                    (Outcome::Cancelled, _) => tr("已取消").into(),
                    (Outcome::Failed, Some(elapsed)) => {
                        tf!("失败 · {elapsed}", elapsed = elapsed_text(elapsed)).into()
                    }
                    (_, Some(elapsed)) => elapsed_text(elapsed).into(),
                    (Outcome::Failed, None) => tr("失败").into(),
                    (Outcome::Done, None) => "".into(),
                };
                let copy_text = format!("{}\n{}", entry.command, entry.output);
                let head = h_flex()
                    .id(("activity", id as usize))
                    .h(px(30.))
                    .px_2()
                    .gap_2()
                    .rounded(px(ui::RADIUS))
                    .cursor_pointer()
                    .hover(|s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.open.remove(&id) {
                            this.open.insert(id);
                        }
                        cx.notify();
                    }))
                    .child(
                        ui::icon_small(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .flex_none()
                        .text_color(muted),
                    )
                    .child(div().flex_none().size(px(14.)).child(glyph))
                    .child(
                        div()
                            .flex_none()
                            .w(px(58.))
                            .text_xs()
                            .text_color(muted)
                            .child(entry.at.format("%H:%M:%S").to_string()),
                    )
                    .child(
                        ui::clipped("label", entry.label.clone())
                            .flex_none()
                            .max_w(px(150.))
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM),
                    )
                    .child(
                        ui::clipped("command", entry.command.clone())
                            .flex_1()
                            .font_family("Consolas")
                            .text_xs()
                            .text_color(muted),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(if entry.outcome == Outcome::Failed {
                                danger
                            } else {
                                muted
                            })
                            .child(state),
                    );
                let detail = open.then(|| {
                    v_flex()
                        .ml(px(22.))
                        .mb_1()
                        .p_2()
                        .gap_2()
                        .rounded(px(ui::RADIUS))
                        .bg(panel)
                        .font_family("Consolas")
                        .text_xs()
                        .child(div().whitespace_normal().child(entry.command.clone()))
                        .child(
                            div()
                                .id(("activity-output", id as usize))
                                .max_h(px(200.))
                                .overflow_y_scroll()
                                .text_color(muted)
                                .child(if entry.output.is_empty() {
                                    SharedString::from(tr("（无输出）"))
                                } else {
                                    entry.output.clone()
                                }),
                        )
                        .child(
                            h_flex().child(
                                Button::new(("activity-copy", id as usize))
                                    .xsmall()
                                    .label(tr("复制命令和输出"))
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_text.clone(),
                                        ));
                                    }),
                            ),
                        )
                });
                v_flex().child(head).children(detail).into_any_element()
            })
            .collect();
        v_flex()
            .gap_2()
            .child(
                div()
                    .id("activity-list")
                    .max_h(list_height)
                    .overflow_y_scroll()
                    .child(v_flex().children(rows)),
            )
            .child(
                h_flex()
                    .pt_2()
                    .gap_2()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(muted)
                            .child(tr(
                                "点击一行查看 git 输出。仅记录修改仓库或访问网络的命令，关闭标签页后清空。",
                            )),
                    )
                    .child(
                        Button::new("activity-close")
                            .small()
                            .ghost()
                            .label(tr("关闭"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{Log, MAX_ENTRIES, MAX_OUTPUT, Outcome, output_text};
    use fr_channel::GitOutput;

    #[test]
    fn 记录的开始和结束() {
        let mut log = Log::default();
        let a = log.begin("提交".into(), "git commit -F -".into());
        assert_eq!(log.running(), 1);
        assert_eq!(&*log.entries[0].command, "git commit -F -");
        log.end(
            a,
            &Ok(GitOutput {
                code: Some(1),
                stdout: b"nothing to commit".to_vec(),
                stderr: "Writing:  50%\rWriting: 100%\nerror: x\n".into(),
                ..Default::default()
            }),
            false,
        );
        assert_eq!(log.running(), 0);
        assert_eq!(log.entries[0].outcome, Outcome::Failed);
        assert_eq!(
            &*log.entries[0].output,
            "Writing: 100%\nerror: x\nnothing to commit"
        );
        let b = log.begin("获取".into(), "git fetch".into());
        log.end(b, &Err(anyhow::anyhow!("已取消")), true);
        // 新的在前
        assert_eq!(log.entries[0].outcome, Outcome::Cancelled);
        assert_eq!(&*log.entries[1].label, "提交");
    }

    #[test]
    fn 记录多了去掉最旧的() {
        let mut log = Log::default();
        for n in 0..MAX_ENTRIES + 5 {
            log.begin("x".into(), format!("git {n}"));
        }
        assert_eq!(log.entries.len(), MAX_ENTRIES);
        assert_eq!(&*log.entries[0].command, format!("git {}", MAX_ENTRIES + 4));
    }

    #[test]
    fn 输出太长只留末尾() {
        let out = GitOutput {
            code: Some(0),
            stdout: "汉".repeat(MAX_OUTPUT).into_bytes(),
            ..Default::default()
        };
        let text = output_text(&out);
        assert!(text.starts_with("…\n汉"));
        assert!(text.len() <= MAX_OUTPUT + 8);
    }
}
