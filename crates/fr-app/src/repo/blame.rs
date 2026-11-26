//! 逐行追溯（blame）：左边是每一行最后改它的提交，右边是文件内容。点一行跳到那个提交。
//! 键盘：上下键、PageUp、PageDown、Home、End 换行，回车跳到改这一行的提交，Esc 关闭。
//!
//! 一路往前追：页头的「更早的版本」、行的右键菜单「追溯此行更早的修改」换成改这一行的那个提交
//! 之前的版本（文件那时叫别的名字也跟得上），「较新的版本」退回来。

use fr_core::i18n::tr;
use fr_core::tf;
use std::ops::Range;

use fr_core::blame::{self, Blame};
use gpui_kit::component::button::Button;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{Mode, RepoView};

const ROW_HEIGHT: f32 = 22.;
/// 一行最多显示这么多字符
const MAX_CHARS: usize = 2000;

pub(super) struct BlameView {
    path: String,
    /// 追溯哪个提交里的版本；None 是工作区现在的样子
    rev: Option<String>,
    data: Option<Blame>,
    /// 每一行显示的文字
    texts: Vec<SharedString>,
    error: Option<SharedString>,
    scroll: UniformListScrollHandle,
    /// 键盘选中的行
    selected: Option<usize>,
    focus: FocusHandle,
    /// 刚打开，还没把键盘焦点拿过来
    grab_focus: bool,
    /// 关掉以后回到哪个视图
    back: Mode,
    /// 往更早的版本追过来的路：（路径，版本），最后一个是上一步看的
    trail: Vec<(String, Option<String>)>,
    /// 读完以后选中、滚到第几行（往前追时落在差不多的位置）
    land: Option<usize>,
    _task: Task<()>,
}

impl RepoView {
    /// 打开逐行追溯：工作区现在的版本。
    pub(super) fn show_blame(&mut self, path: String, cx: &mut Context<Self>) {
        self.show_blame_at(path, None, cx);
    }

    /// 打开逐行追溯：`rev` 给了就是那个提交里的版本（从提交详情、比较里打开时），页头写明是哪个提交。
    pub(super) fn show_blame_at(
        &mut self,
        path: String,
        rev: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.open_blame(path, rev, Vec::new(), None, cx);
    }

    /// 打开逐行追溯。`trail` 是往前追过来的路（新打开的是空的），`land` 是读完落在第几行。
    fn open_blame(
        &mut self,
        path: String,
        rev: Option<String>,
        trail: Vec<(String, Option<String>)>,
        land: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let request = channel.git(&self.entry.path, blame::args_at(rev.as_deref(), &path));
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
                        // 往前追过来的：落在原来那一行附近
                        if let Some(line) = view.land.take().filter(|_| !data.lines.is_empty()) {
                            let line = line.min(data.lines.len() - 1);
                            view.selected = Some(line);
                            view.scroll.scroll_to_item(line, ScrollStrategy::Center);
                        }
                        view.data = Some(data);
                    }
                    Err(err) => {
                        view.error = Some(tf!("读取失败：{err}", err = format!("{:#}", err)).into())
                    }
                }
                cx.notify();
            })
            .ok();
        });
        let back = match self.mode {
            Mode::Blame => self.blame.as_ref().map_or(Mode::History, |b| b.back),
            Mode::Merge => Mode::Changes,
            other => other,
        };
        self.blame = Some(BlameView {
            path,
            rev,
            data: None,
            texts: Vec::new(),
            error: None,
            scroll: UniformListScrollHandle::new(),
            selected: None,
            focus: cx.focus_handle(),
            grab_focus: true,
            back,
            trail,
            land,
            _task: task,
        });
        self.mode = Mode::Blame;
        cx.notify();
    }

    /// 往更早的版本追：`line` 给了就追那一行（改它的那个提交之前的版本），没给按选中的行，再没有
    /// 就按这个版本里最近改过文件的那个提交。追不动（文件就是那个提交加进来的）时说明原因。
    fn blame_older(&mut self, line: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.blame else {
            return;
        };
        let Some(data) = &view.data else {
            return;
        };
        let line = line.or(view.selected);
        let picked = match line {
            Some(line) => data
                .lines
                .get(line)
                .and_then(|l| data.commits.get(l.commit).map(|c| (c, l.orig_line))),
            None => data
                .commits
                .iter()
                .filter(|commit| !commit.uncommitted())
                .max_by_key(|commit| commit.time)
                .map(|commit| (commit, 0)),
        };
        let Some((commit, orig_line)) = picked else {
            return;
        };
        // 还没提交的行：更早的版本就是 HEAD 里的
        let target = match commit.uncommitted() {
            true => Some(("HEAD".to_string(), view.path.clone())),
            false => commit.previous.clone(),
        };
        let Some((rev, path)) = target else {
            let text = tf!(
                "没有更早的版本：此文件由 {id}「{summary}」添加。",
                id = &commit.id[..commit.id.len().min(8)],
                summary = commit.summary.as_str()
            );
            window.push_notification(
                gpui_kit::component::notification::Notification::info(text),
                cx,
            );
            return;
        };
        let mut trail = view.trail.clone();
        trail.push((view.path.clone(), view.rev.clone()));
        // 那一行在更早的版本里大约还在原来的位置
        let land = match (line, orig_line) {
            (Some(_), n) if n > 0 => Some(n as usize - 1),
            (Some(line), _) => Some(line),
            (None, _) => None,
        };
        let back = view.back;
        self.open_blame(path, Some(rev), trail, land, cx);
        if let Some(view) = &mut self.blame {
            view.back = back;
        }
    }

    /// 退回上一步看的版本（较新的那个）。
    fn blame_newer(&mut self, cx: &mut Context<Self>) {
        let Some(view) = &self.blame else {
            return;
        };
        let mut trail = view.trail.clone();
        let Some((path, rev)) = trail.pop() else {
            return;
        };
        let (land, back) = (view.selected, view.back);
        self.open_blame(path, rev, trail, land, cx);
        if let Some(view) = &mut self.blame {
            view.back = back;
        }
    }

    pub(super) fn close_blame(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.blame.take() {
            self.mode = view.back;
        }
        cx.notify();
    }

    /// 刚打开逐行追溯时把键盘焦点放上去，上下键马上能用。
    pub(super) fn focus_blame_if_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = &mut self.blame
            && std::mem::take(&mut view.grab_focus)
        {
            view.focus.focus(window, cx);
        }
    }

    fn blame_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &mut self.blame else {
            return;
        };
        if !view.focus.is_focused(window) {
            return;
        }
        let count = view.data.as_ref().map_or(0, |d| d.lines.len());
        const PAGE: usize = 20;
        let current = view.selected;
        let next = match event.keystroke.key.as_str() {
            "escape" => {
                cx.stop_propagation();
                self.close_blame(cx);
                return;
            }
            "enter" => {
                cx.stop_propagation();
                if let Some(line) = current {
                    self.blame_jump(line, window, cx);
                }
                return;
            }
            _ if count == 0 => return,
            "down" => current.map_or(0, |ix| (ix + 1).min(count - 1)),
            "up" => current.map_or(0, |ix| ix.saturating_sub(1)),
            "pagedown" => current.map_or(0, |ix| (ix + PAGE).min(count - 1)),
            "pageup" => current.map_or(0, |ix| ix.saturating_sub(PAGE)),
            "home" => 0,
            "end" => count - 1,
            _ => return,
        };
        cx.stop_propagation();
        view.selected = Some(next);
        view.scroll.scroll_to_item(next, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// 点一行：回到提交历史，选中改这一行的提交。
    fn blame_jump(&mut self, line: usize, window: &mut Window, cx: &mut Context<Self>) {
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
        // 正在看文件历史时列表里不一定有这个提交：先回到全部提交。
        // 列表要重读，reveal_commit 会先记着，读到了再定位
        self.close_file_history(window, cx);
        self.reveal_commit(&id, cx);
        cx.notify();
    }

    pub(super) fn render_blame(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, danger) = (theme.border, theme.muted_foreground, theme.danger);
        let Some(view) = &self.blame else {
            return div().into_any_element();
        };
        let steps = view.trail.len();
        let loaded = view.data.is_some();
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
                .child(tr("正在读取…"))
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
                    .child(div().text_color(muted).child(tr("逐行追溯")))
                    .child(crate::ui::clipped("blame-path", view.path.clone()).flex_1())
                    // 追溯的是哪个版本：提交里的写短哈希，工作区写「当前版本」
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(muted)
                            .child(match &view.rev {
                                Some(rev) => SharedString::from(tf!(
                                    "提交 {rev} 中的版本",
                                    rev = &rev[..rev.len().min(8)]
                                )),
                                None => tr("当前版本").into(),
                            }),
                    )
                    .when(steps > 0, |bar| {
                        bar.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(muted)
                                .child(tf!("已往前追 {steps} 步", steps)),
                        )
                    })
                    .child(
                        Button::new("blame-older")
                            .xsmall()
                            .label(tr("更早的版本"))
                            .tooltip(tr(
                                "追溯选中行更早的修改；未选中行时，回到最近一次修改此文件之前的版本",
                            ))
                            .disabled(!loaded)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.blame_older(None, window, cx)
                            })),
                    )
                    .child(
                        Button::new("blame-newer")
                            .xsmall()
                            .label(tr("较新的版本"))
                            .tooltip(tr("回到上一步查看的版本"))
                            .disabled(steps == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.blame_newer(cx))),
                    )
                    .child(
                        Button::new("close-blame")
                            .xsmall()
                            .label(tr("关闭"))
                            .on_click(cx.listener(|this, _, _, cx| this.close_blame(cx))),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
            .track_focus(&view.focus)
            .on_key_down(cx.listener(Self::blame_key))
            .into_any_element()
    }

    fn render_blame_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.theme();
        let (border, muted, hover, active) = (
            theme.border,
            theme.muted_foreground,
            theme.list_hover,
            theme.list_active,
        );
        let Some(view) = &self.blame else {
            return Vec::new();
        };
        let selected = view.selected;
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
                    ("".into(), tr("未提交").into())
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
                        .when(selected == Some(ix), |row| row.bg(active))
                        .hover(|s| s.bg(hover))
                        .when(line.first && ix > 0, |row| {
                            row.border_t_1().border_color(border)
                        })
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.blame_jump(ix, window, cx)),
                        )
                        .context_menu({
                            let view = cx.entity().downgrade();
                            let (id, uncommitted) = (commit.id.clone(), commit.uncommitted());
                            move |menu, _, _| {
                                let (older, jump) = (view.clone(), view.clone());
                                let menu = menu.item(
                                    PopupMenuItem::new(tr("追溯此行更早的修改")).on_click(
                                        move |_, window, cx| {
                                            older
                                                .update(cx, |this, cx| {
                                                    this.blame_older(Some(ix), window, cx)
                                                })
                                                .ok();
                                        },
                                    ),
                                );
                                if uncommitted {
                                    return menu;
                                }
                                let id = id.clone();
                                menu.item(
                                    PopupMenuItem::new(tr("在提交历史中查看此提交")).on_click(
                                        move |_, window, cx| {
                                            jump.update(cx, |this, cx| {
                                                this.blame_jump(ix, window, cx)
                                            })
                                            .ok();
                                        },
                                    ),
                                )
                                .separator()
                                .item(
                                    PopupMenuItem::new(tr("复制哈希")).on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            id.clone(),
                                        ));
                                    }),
                                )
                            }
                        })
                        .child(
                            div()
                                .w(px(150.))
                                .flex_none()
                                .px_2()
                                .font_family(crate::fonts::mono())
                                .text_color(muted)
                                .child(who),
                        )
                        .child(
                            div()
                                .w(px(260.))
                                .flex_none()
                                .pr_2()
                                .text_color(muted)
                                .child(crate::ui::clipped("what", what)),
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
                                .font_family(crate::fonts::mono())
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
