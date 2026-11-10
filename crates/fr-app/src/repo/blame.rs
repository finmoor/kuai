//! 逐行追溯（blame）：左边是每一行最后改它的提交，右边是文件内容。点一行跳到那个提交。

use std::ops::Range;

use fr_core::blame::{self, Blame};
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{Mode, RepoView};

const ROW_HEIGHT: f32 = 22.;
/// 一行最多显示这么多字符
const MAX_CHARS: usize = 2000;

pub(super) struct BlameView {
    path: String,
    data: Option<Blame>,
    /// 每一行显示的文字
    texts: Vec<SharedString>,
    error: Option<SharedString>,
    scroll: UniformListScrollHandle,
    /// 关掉以后回到哪个视图
    back: Mode,
    _task: Task<()>,
}

impl RepoView {
    /// 打开逐行追溯。
    pub(super) fn show_blame(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let request = channel.git(&self.entry.path, blame::args(&path));
        let task = cx.spawn(async move |this, cx| {
            let result = request.await.and_then(|out| out.ok());
            this.update(cx, |this, cx| {
                let Some(view) = &mut this.blame else {
                    return;
                };
                match result {
                    Ok(out) => {
                        let data = blame::parse(&out);
                        view.texts = data
                            .lines
                            .iter()
                            .map(|line| {
                                let text = crate::text::decode(&line.content);
                                let shown: String = text
                                    .chars()
                                    .take(MAX_CHARS)
                                    .map(|c| if c == '\t' { ' ' } else { c })
                                    .collect();
                                shown.into()
                            })
                            .collect();
                        view.data = Some(data);
                    }
                    Err(err) => view.error = Some(format!("读取失败：{err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        let back = match self.mode {
            Mode::Blame => self.blame.as_ref().map_or(Mode::History, |b| b.back),
            other => other,
        };
        self.blame = Some(BlameView {
            path,
            data: None,
            texts: Vec::new(),
            error: None,
            scroll: UniformListScrollHandle::new(),
            back,
            _task: task,
        });
        self.mode = Mode::Blame;
        cx.notify();
    }

    pub(super) fn close_blame(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.blame.take() {
            self.mode = view.back;
        }
        cx.notify();
    }

    /// 点一行：回到提交历史，选中改这一行的提交。
    fn blame_jump(&mut self, line: usize, cx: &mut Context<Self>) {
        let Some(commit) = self.blame.as_ref().and_then(|view| {
            let data = view.data.as_ref()?;
            data.commits.get(data.lines.get(line)?.commit)
        }) else {
            return;
        };
        if commit.uncommitted() {
            return;
        }
        let id = commit.id.clone();
        self.blame = None;
        self.sidebar_select_history();
        // 正在看文件历史时列表里不一定有这个提交：先回到全部提交
        self.close_file_history(cx);
        self.reveal_commit(&id, cx);
        cx.notify();
    }

    pub(super) fn render_blame(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, danger) = (theme.border, theme.muted_foreground, theme.danger);
        let Some(view) = &self.blame else {
            return div().into_any_element();
        };
        let body: AnyElement = match (&view.data, &view.error) {
            (_, Some(err)) => div()
                .p_3()
                .text_sm()
                .text_color(danger)
                .child(err.clone())
                .into_any_element(),
            (None, None) => div()
                .p_3()
                .text_sm()
                .text_color(muted)
                .child("正在读取…")
                .into_any_element(),
            (Some(data), None) => uniform_list(
                "blame",
                data.lines.len(),
                cx.processor(|this, range: Range<usize>, _, cx| this.render_blame_rows(range, cx)),
            )
            .track_scroll(&view.scroll)
            .size_full()
            .into_any_element(),
        };
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h(px(30.))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .text_sm()
                    .child(div().text_color(muted).child("逐行追溯"))
                    .child(div().flex_1().min_w_0().truncate().child(view.path.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("点一行跳到改它的提交"),
                    )
                    .child(
                        Button::new("close-blame")
                            .xsmall()
                            .label("关闭")
                            .on_click(cx.listener(|this, _, _, cx| this.close_blame(cx))),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn render_blame_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.theme();
        let (border, muted, hover) = (theme.border, theme.muted_foreground, theme.list_hover);
        let Some(view) = &self.blame else {
            return Vec::new();
        };
        let Some(data) = &view.data else {
            return Vec::new();
        };
        range
            .filter_map(|ix| {
                let line = data.lines.get(ix)?;
                let commit = data.commits.get(line.commit)?;
                // 提交信息只在一段的第一行显示
                let (who, what): (SharedString, SharedString) = if !line.first {
                    ("".into(), "".into())
                } else if commit.uncommitted() {
                    ("".into(), "还没提交".into())
                } else {
                    (
                        format!(
                            "{} {}",
                            &commit.id[..8],
                            super::history::format_date(commit.time)
                        )
                        .into(),
                        format!("{} · {}", commit.author, commit.summary).into(),
                    )
                };
                Some(
                    h_flex()
                        .id(ix)
                        .h(px(ROW_HEIGHT))
                        .w_full()
                        .text_xs()
                        .cursor_pointer()
                        .hover(|s| s.bg(hover))
                        .when(line.first && ix > 0, |row| {
                            row.border_t_1().border_color(border)
                        })
                        .on_click(cx.listener(move |this, _, _, cx| this.blame_jump(ix, cx)))
                        .child(
                            div()
                                .w(px(150.))
                                .flex_none()
                                .px_2()
                                .font_family("Consolas")
                                .text_color(muted)
                                .child(who),
                        )
                        .child(
                            div()
                                .w(px(260.))
                                .flex_none()
                                .pr_2()
                                .truncate()
                                .text_color(muted)
                                .child(what),
                        )
                        .child(
                            div()
                                .w(px(44.))
                                .flex_none()
                                .pr_2()
                                .text_right()
                                .text_color(muted)
                                .border_l_1()
                                .border_color(border)
                                .child((ix + 1).to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .font_family("Consolas")
                                .text_sm()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .child(view.texts.get(ix).cloned().unwrap_or_default()),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }
}
