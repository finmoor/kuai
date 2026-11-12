//! 内置的合并编辑器：解决一个文件里的冲突，不用再开外部编辑器。
//!
//! 两种用法，可以来回切：
//! - 逐处选择：文件按冲突标记拆开，每一处冲突选用当前的、传入的，或者两边都要；
//! - 手动编辑：整个文件放进编辑框里随便改（已经选好的那些处先替换好，没选的还带着冲突标记）。
//!
//! 保存时文件里不能再有冲突标记；写回工作区后 `git add`，这个文件就算解决了。读写文件借 git 的
//! shell 别名（见 [`fr_core::workfile`]），本机和服务器上的仓库都能用。
//!
//! 文件按 UTF-8 处理（别的编码放进编辑框再写回去会坏，所以不接）。CRLF 的文件编辑时按 LF，
//! 保存时换回 CRLF。

use std::ops::Range;

use fr_core::i18n::tr;
use fr_core::tf;
use fr_core::workfile;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffConflict, DiffConflictResolution, DiffFile, DiffState};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::ops::Step;
use super::{Mode, RepoView};
use crate::ui;

pub(super) struct MergeView {
    path: String,
    /// 关掉以后回到哪个视图
    back: Mode,
    /// 文件的内容（换行统一成 LF）：逐处选择是在它上面选的
    text: String,
    /// 原来是 CRLF 的文件，保存时换回去
    crlf: bool,
    /// 逐处选择用的视图
    picker: Entity<DiffState>,
    /// 手动编辑用的编辑框
    editor: Entity<EditorState>,
    manual: bool,
    loading: bool,
    error: Option<SharedString>,
    _task: Task<()>,
    _observe: Subscription,
}

/// 按每一处的选择拼出文件的内容。没选的那几处原样留着（带冲突标记）。
/// `text` 是解析出 `conflicts` 的那份内容。
fn assemble(
    text: &str,
    conflicts: &[DiffConflict],
    resolution: impl Fn(usize) -> Option<DiffConflictResolution>,
) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    // 组件库给的行号从 1 起、不含末尾
    let take = |out: &mut String, range: Range<usize>| {
        let (start, end) = (range.start.saturating_sub(1), range.end.saturating_sub(1));
        for line in lines.get(start..end.min(lines.len())).unwrap_or_default() {
            out.push_str(line);
        }
    };
    let mut out = String::with_capacity(text.len());
    let mut next = 1;
    for (ix, conflict) in conflicts.iter().enumerate() {
        let all = conflict.lines();
        take(&mut out, next..all.start);
        match resolution(ix) {
            Some(DiffConflictResolution::Current) => take(&mut out, conflict.current_lines()),
            Some(DiffConflictResolution::Incoming) => take(&mut out, conflict.incoming_lines()),
            Some(DiffConflictResolution::Both) => {
                take(&mut out, conflict.current_lines());
                take(&mut out, conflict.incoming_lines());
            }
            // 没选，或者以后组件库多出来的选法：原样留着
            _ => take(&mut out, all.clone()),
        }
        next = all.end;
    }
    take(&mut out, next..lines.len() + 1);
    out
}

/// 文件里有几处冲突；冲突标记不完整（只有开头没有结尾之类）时返回 None。
fn count_conflicts(path: &str, text: &str) -> Option<usize> {
    DiffFile::parse_conflicts(path.to_string(), text)
        .ok()
        .map(|file| file.conflicts().len())
}

impl RepoView {
    /// 打开合并编辑器，解决 `path` 里的冲突。
    pub(super) fn open_merge_editor(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let request = channel.git(&self.entry.path, workfile::read_args(&path));
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = request.await.and_then(|out| out.ok());
            this.update_in(cx, |this, window, cx| {
                let Some(view) = &mut this.merge else {
                    return;
                };
                view.loading = false;
                let text = match result.map(String::from_utf8) {
                    Ok(Ok(text)) => text,
                    Ok(Err(_)) => {
                        view.error = Some(
                            tr("这个文件不是 UTF-8 编码，这里改不了。在外部编辑器里解决后，回「本地修改」点「标记为已解决」。")
                                .into(),
                        );
                        cx.notify();
                        return;
                    }
                    Err(err) => {
                        view.error =
                            Some(tf!("读取文件失败：{err}", err = format!("{:#}", err)).into());
                        cx.notify();
                        return;
                    }
                };
                view.crlf = text.contains("\r\n");
                view.text = text.replace("\r\n", "\n");
                let file = DiffFile::parse_conflicts(view.path.clone(), &view.text);
                match file {
                    Ok(file) if !file.conflicts().is_empty() => {
                        view.picker.update(cx, |state, cx| state.set_files([file], cx));
                    }
                    // 没有冲突标记，或者标记不完整：只能手动编辑
                    _ => {
                        let text = view.text.clone();
                        view.editor
                            .update(cx, |state, cx| state.set_value(text, window, cx));
                        view.manual = true;
                    }
                }
                cx.notify();
            })
            .ok();
        });
        let picker = cx.new(|cx| DiffState::new(Vec::new(), cx));
        let editor = cx.new(|cx| EditorState::new(window, cx).line_number(true));
        let observe = cx.observe(&picker, |_, _, cx| cx.notify());
        let back = match self.mode {
            Mode::Merge => self.merge.as_ref().map_or(Mode::Changes, |view| view.back),
            Mode::Blame => Mode::Changes,
            other => other,
        };
        self.merge = Some(MergeView {
            path,
            back,
            text: String::new(),
            crlf: false,
            picker,
            editor,
            manual: false,
            loading: true,
            error: None,
            _task: task,
            _observe: observe,
        });
        self.blame = None;
        self.mode = Mode::Merge;
        cx.notify();
    }

    pub(super) fn close_merge_editor(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.merge.take() {
            self.mode = view.back;
        }
        cx.notify();
    }

    /// 逐处选择里现在的结果：选好的替换掉，没选的还带着标记。
    fn merge_picked_text(&self, cx: &App) -> Option<String> {
        let view = self.merge.as_ref()?;
        let state = view.picker.read(cx);
        let file = state.files().first()?;
        Some(assemble(&view.text, file.conflicts(), |ix| {
            state.conflict_resolution(&view.path, ix)
        }))
    }

    /// 每一处冲突都取同一边。
    fn merge_pick_all(&mut self, resolution: DiffConflictResolution, cx: &mut Context<Self>) {
        let Some(view) = &self.merge else {
            return;
        };
        let path = view.path.clone();
        view.picker.update(cx, |state, cx| {
            let count = state
                .files()
                .first()
                .map_or(0, |file| file.conflicts().len());
            for ix in 0..count {
                state.resolve_conflict(&path, ix, Some(resolution), cx);
            }
        });
        cx.notify();
    }

    /// 在「逐处选择」和「手动编辑」之间切换，内容跟着带过去。
    fn merge_set_manual(&mut self, manual: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.merge else {
            return;
        };
        if view.manual == manual || view.loading || view.error.is_some() {
            return;
        }
        if manual {
            let text = self.merge_picked_text(cx).unwrap_or_default();
            let Some(view) = &mut self.merge else {
                return;
            };
            view.editor
                .update(cx, |state, cx| state.set_value(text, window, cx));
            view.editor.focus_handle(cx).focus(window, cx);
            view.manual = true;
        } else {
            // 回到逐处选择：编辑框里现在的内容重新按冲突标记拆
            let text = view.editor.read(cx).value().to_string();
            match DiffFile::parse_conflicts(view.path.clone(), &text) {
                Ok(file) if !file.conflicts().is_empty() => {
                    // 换一个新的状态：原来记的「第几处选了哪边」对不上新的内容了
                    let picker = cx.new(|cx| DiffState::new([file], cx));
                    let observe = cx.observe(&picker, |_, _, cx| cx.notify());
                    let Some(view) = &mut self.merge else {
                        return;
                    };
                    view.text = text;
                    view.picker = picker;
                    view._observe = observe;
                    view.manual = false;
                }
                Ok(_) => {
                    let note = tr("已经没有冲突标记了。改好就点「保存并标记为已解决」。");
                    window.push_notification(Notification::info(note), cx);
                }
                Err(_) => {
                    let note =
                        tr("冲突标记不完整（<<<<<<<、=======、>>>>>>> 要成套）。补全或者删掉。");
                    window.push_notification(Notification::warning(note), cx);
                }
            }
        }
        cx.notify();
    }

    /// 保存并标记为已解决：文件里不能再有冲突标记。
    fn merge_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.merge else {
            return;
        };
        let text = if view.manual {
            view.editor.read(cx).value().to_string()
        } else {
            self.merge_picked_text(cx).unwrap_or_default()
        };
        let Some(view) = &self.merge else {
            return;
        };
        match count_conflicts(&view.path, &text) {
            Some(0) => {}
            Some(left) => {
                let note = tf!(
                    "还有 {left} 处冲突没解决：每处选一边，或在「手动编辑」里改好再保存。",
                    left
                );
                window.push_notification(Notification::warning(note), cx);
                return;
            }
            None => {
                let note = tr(
                    "还有不完整的冲突标记（<<<<<<<、=======、>>>>>>>）。在「手动编辑」里删干净再保存。",
                );
                window.push_notification(Notification::warning(note), cx);
                return;
            }
        }
        let path = view.path.clone();
        let bytes = if view.crlf {
            text.replace('\n', "\r\n").into_bytes()
        } else {
            text.into_bytes()
        };
        let steps = vec![
            Step {
                args: workfile::write_args(&path),
                stdin: Some(bytes),
                net: false,
            },
            Step::local(vec!["add".into(), "--".into(), path.clone()]),
        ];
        self.run_seq(
            tr("解决冲突"),
            steps,
            window,
            cx,
            move |this, _, w, cx| {
                this.close_merge_editor(cx);
                this.show_changes(cx);
                let text = tf!("{path} 已保存，并标记为已解决", path);
                w.push_notification(Notification::success(text), cx);
            },
        );
    }

    pub(super) fn render_merge(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, danger, accent) = (
            theme.border,
            theme.muted_foreground,
            theme.danger,
            theme.primary,
        );
        let (ours, theirs) = self.side_labels();
        let busy = self.op.is_some();
        let Some(view) = &self.merge else {
            return div().into_any_element();
        };
        let path: SharedString = view.path.clone().into();
        let manual = view.manual;
        let ready = !view.loading && view.error.is_none();
        // 一共几处、选了几处
        let (total, picked) = {
            let state = view.picker.read(cx);
            let total = state
                .files()
                .first()
                .map_or(0, |file| file.conflicts().len());
            let picked = (0..total)
                .filter(|ix| state.conflict_resolution(&view.path, *ix).is_some())
                .count();
            (total, picked)
        };
        let status: SharedString = if view.loading {
            tr("正在读取文件…").into()
        } else if manual {
            tr("手动编辑：直接改文件的内容，冲突标记（<<<<<<<、=======、>>>>>>>）要删干净。").into()
        } else {
            tf!(
                "{total} 处冲突，已选 {picked} 处。用每处右上角的按钮选一边，或两边都留。",
                total,
                picked
            )
            .into()
        };
        let body = if let Some(error) = view.error.clone() {
            div()
                .p_4()
                .text_sm()
                .text_color(danger)
                .whitespace_normal()
                .child(error)
                .into_any_element()
        } else if view.loading {
            div().into_any_element()
        } else if manual {
            Editor::new(&view.editor).h_full().into_any_element()
        } else {
            Diff::new(&view.picker)
                .line_number(true)
                .syntax_highlight(true)
                .header_visible(false)
                .size_full()
                .into_any_element()
        };
        let tip = path.clone();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h(px(38.))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .child(ui::icon(IconName::GitMerge).flex_none().text_color(accent))
                    .child(
                        div()
                            .flex_none()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr("解决冲突")),
                    )
                    .child(
                        div()
                            .id("merge-path")
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_start()
                            .text_sm()
                            .text_color(muted)
                            .child(path)
                            .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx)),
                    )
                    .child(
                        Button::new("merge-save")
                            .small()
                            .primary()
                            .label(tr("保存并标记为已解决"))
                            .disabled(!ready || busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.merge_save(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("merge-close")
                            .small()
                            .ghost()
                            .label(tr("关闭"))
                            .tooltip(tr("不保存，回到本地修改"))
                            .on_click(cx.listener(|this, _, _, cx| this.close_merge_editor(cx))),
                    ),
            )
            .child(
                h_flex()
                    .min_h(px(32.))
                    .flex_none()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(muted)
                            .whitespace_normal()
                            .child(status),
                    )
                    .when(ready && !manual, |bar| {
                        bar.child(
                            Button::new("merge-all-ours")
                                .xsmall()
                                .label(tf!("全部用{ours}", ours))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.merge_pick_all(DiffConflictResolution::Current, cx)
                                })),
                        )
                        .child(
                            Button::new("merge-all-theirs")
                                .xsmall()
                                .label(tf!("全部用{theirs}", theirs))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.merge_pick_all(DiffConflictResolution::Incoming, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("merge-mode-pick")
                            .xsmall()
                            .ghost()
                            .label(tr("逐处选择"))
                            .selected(!manual)
                            .disabled(!ready)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.merge_set_manual(false, window, cx)
                            })),
                    )
                    .child(
                        Button::new("merge-mode-manual")
                            .xsmall()
                            .ghost()
                            .label(tr("手动编辑"))
                            .selected(manual)
                            .disabled(!ready)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.merge_set_manual(true, window, cx)
                            })),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{assemble, count_conflicts};
    use gpui_kit::component::diff::{DiffConflictResolution, DiffFile};

    const TEXT: &str = "head
<<<<<<< HEAD
ours 1
ours 2
=======
theirs 1
>>>>>>> feature
middle
<<<<<<< HEAD
=======
only theirs
>>>>>>> feature
tail
";

    fn resolved(choices: [Option<DiffConflictResolution>; 2]) -> String {
        let file = DiffFile::parse_conflicts("a.txt", TEXT).unwrap();
        assert_eq!(file.conflicts().len(), 2);
        assemble(TEXT, file.conflicts(), |ix| choices[ix])
    }

    #[test]
    fn 按每一处的选择拼出内容() {
        use DiffConflictResolution::{Both, Current, Incoming};
        // 都没选：原样
        assert_eq!(resolved([None, None]), TEXT);
        assert_eq!(
            resolved([Some(Current), Some(Incoming)]),
            "head\nours 1\nours 2\nmiddle\nonly theirs\ntail\n"
        );
        assert_eq!(
            resolved([Some(Incoming), Some(Current)]),
            "head\ntheirs 1\nmiddle\ntail\n"
        );
        assert_eq!(
            resolved([Some(Both), Some(Both)]),
            "head\nours 1\nours 2\ntheirs 1\nmiddle\nonly theirs\ntail\n"
        );
        // 只选了一处：另一处还带着标记
        let half = resolved([Some(Current), None]);
        assert!(half.starts_with("head\nours 1\nours 2\nmiddle\n<<<<<<< HEAD\n"));
        assert_eq!(count_conflicts("a.txt", &half), Some(1));
        assert_eq!(
            count_conflicts("a.txt", &resolved([Some(Current), Some(Current)])),
            Some(0)
        );
    }

    #[test]
    fn 带共同祖先的冲突也认得() {
        let text = "a\n<<<<<<< HEAD\nours\n||||||| base\norig\n=======\ntheirs\n>>>>>>> x\nz";
        let file = DiffFile::parse_conflicts("a.txt", text).unwrap();
        let out = assemble(text, file.conflicts(), |_| {
            Some(DiffConflictResolution::Incoming)
        });
        // 最后一行没有换行符，照原样
        assert_eq!(out, "a\ntheirs\nz");
        let out = assemble(text, file.conflicts(), |_| {
            Some(DiffConflictResolution::Both)
        });
        assert_eq!(out, "a\nours\ntheirs\nz");
    }
}
