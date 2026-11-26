//! 丢弃改动可以撤销：丢弃之前先把要动的文件各存一份进对象库（`git hash-object -w`，
//! 按原样字节），丢弃以后通知里有「撤销」，本地修改里按 Ctrl+Z 也行；撤销前核对文件还是丢弃后的
//! 样子，期间又被改过的不放回。存的内容在对象库里没有引用，git 清理时（默认两周以后）才删。
//!
//! 有了撤销，确认框里可以勾「不再询问」（设置 → Git 里能改回来）。

use fr_core::i18n::tr;
use fr_core::tf;
use fr_core::workfile::{self, Snap};
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::component::{Sizable as _, WindowExt as _, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use super::ops::Step;

/// 上一次丢弃：撤销用。
#[derive(Clone)]
pub(super) struct Discarded {
    files: Vec<String>,
    before: Vec<Snap>,
    after: Vec<Snap>,
}

/// 「已丢弃」那条通知的标识：同一时间只留一条（撤销的总是最近一次）。
struct DiscardNote;

/// 看几个文件现在的样子（`write` 为真时内容存进对象库）。
async fn snap(
    channel: &fr_channel::Channel,
    repo: &str,
    files: &[String],
    write: bool,
) -> anyhow::Result<Vec<Snap>> {
    let out = channel
        .git_with_input(
            repo,
            workfile::snap_args(write),
            Some(workfile::snap_stdin(files)),
        )
        .await?
        .ok()?;
    workfile::parse_snap(files, &out).ok_or_else(|| anyhow::anyhow!(tr("无法识别读取结果")))
}

impl RepoView {
    /// 丢弃之前问一下；设置里关了「丢弃改动前确认」（确认框里勾过「不再询问」）就直接做。
    /// 回车不执行，要点红色的按钮。
    pub(super) fn confirm_discard(
        &mut self,
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if !self.app.read(cx).settings.confirm_discard {
            action(self, window, cx);
            return;
        }
        let (title, description) = (title.into(), description.into());
        let view = cx.entity().downgrade();
        let action = std::rc::Rc::new(action);
        let app = self.app.clone();
        let quiet = std::rc::Rc::new(std::cell::Cell::new(false));
        crate::ui::cancel_pending_tip(window, cx);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (view, action, app, quiet) =
                (view.clone(), action.clone(), app.clone(), quiet.clone());
            let checked = quiet.clone();
            let run = move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                window.close_dialog(cx);
                if quiet.get() {
                    app.update(cx, |app, cx| {
                        app.update_settings(cx, |s| s.confirm_discard = false)
                    });
                }
                view.update(cx, |this, cx| action(this, window, cx)).ok();
            };
            alert
                .title(title.clone())
                .description(description.clone())
                // 破坏性的操作按回车不执行
                .on_ok(|_, _, _| false)
                .footer(
                    DialogFooter::new()
                        .child(
                            h_flex().flex_1().child(
                                Checkbox::new("discard-quiet")
                                    .label(tr("不再询问"))
                                    .tooltip(tr("以后丢弃时不再确认，可在提示里撤销。在「设置 → Git」中可恢复确认。"))
                                    .checked(checked.get())
                                    .on_click(move |on, window, _| {
                                        checked.set(*on);
                                        window.refresh();
                                    }),
                            ),
                        )
                        .child(
                            Button::new("cancel")
                                .label(tr("取消"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("ok")
                                .label(tr("丢弃"))
                                .with_variant(ButtonVariant::Danger)
                                .on_click(run),
                        ),
                )
        });
    }

    /// 存一份再丢弃：`files` 是会被动到的文件（未跟踪的目录先展开成里面的文件），`steps` 是丢弃的
    /// 命令。做完弹「已丢弃 … 撤销」，`done` 是那句话。存不了就不丢弃。
    pub(super) fn discard_with_undo(
        &mut self,
        files: Vec<String>,
        steps: Vec<Step>,
        done: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        cx.spawn_in(window, async move |this, cx| {
            // 未跟踪的目录（状态里一项、以 / 结尾）：`git clean -d` 会删掉里面的全部文件
            let mut expanded = Vec::new();
            for file in files {
                if !file.ends_with('/') {
                    expanded.push(file);
                    continue;
                }
                let args = ["ls-files", "-o", "--exclude-standard", "-z", "--", &file]
                    .map(String::from)
                    .to_vec();
                let inside = channel
                    .git(&repo, args)
                    .await
                    .ok()
                    .filter(|out| out.success())
                    .map(|out| {
                        out.stdout
                            .split(|b| *b == 0)
                            .filter(|name| !name.is_empty())
                            .map(|name| String::from_utf8_lossy(name).into_owned())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                expanded.extend(inside);
            }
            let before = snap(&channel, &repo, &expanded, true).await;
            this.update_in(cx, |this, window, cx| {
                let before = match before {
                    Ok(before) => before,
                    Err(err) => {
                        let text = tf!(
                            "无法在丢弃前保存文件内容，未丢弃：{err}",
                            err = format!("{err:#}")
                        );
                        window.push_notification(Notification::error(text).autohide(false), cx);
                        return;
                    }
                };
                this.run_seq(
                    tr("丢弃"),
                    steps,
                    window,
                    cx,
                    move |this, _, window, cx| {
                        this.after_discard(expanded, before, done, window, cx);
                    },
                );
            })
            .ok();
        })
        .detach();
    }

    /// 丢弃做完了：记下丢弃后的样子（撤销前核对），弹「已丢弃 … 撤销」。
    fn after_discard(
        &mut self,
        files: Vec<String>,
        before: Vec<Snap>,
        done: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let after = snap(&channel, &repo, &files, false).await;
            this.update_in(cx, |this, window, cx| {
                let after = after.unwrap_or_else(|_| vec![Snap::Unknown; files.len()]);
                let lost = before
                    .iter()
                    .zip(&after)
                    .filter(|(b, a)| b != a && (**b == Snap::Unknown || **a == Snap::Unknown))
                    .count();
                this.discarded = Some(Discarded {
                    files,
                    before,
                    after,
                });
                let text = match lost {
                    0 => done.clone(),
                    lost => tf!(
                        "{done}（其中 {lost} 个是链接或目录，无法撤销）",
                        done = done.as_str(),
                        lost
                    ),
                };
                let view = cx.entity().downgrade();
                let note =
                    crate::ui::action_note(NotificationType::Success, text, move |_, _, cx| {
                        let (view, me) = (view.clone(), cx.entity().downgrade());
                        Button::new("undo-discard")
                            .small()
                            .label(tr("撤销"))
                            .tooltip(tr("放回丢弃前的内容（Ctrl+Z）"))
                            .on_click(move |_, window, cx| {
                                view.update(cx, |this, cx| this.undo_discard(window, cx))
                                    .ok();
                                me.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                            })
                    })
                    .id::<DiscardNote>();
                window.push_notification(note, cx);
            })
            .ok();
        })
        .detach();
    }

    /// 撤销上一次丢弃（通知里的「撤销」、本地修改里的 Ctrl+Z）：丢弃后又被改过的文件不放回。
    pub(super) fn undo_discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(discarded) = self.discarded.take() else {
            window.push_notification(Notification::info(tr("没有可撤销的丢弃")), cx);
            return;
        };
        window.remove_notification::<DiscardNote>(cx);
        let steps: Vec<Step> = discarded
            .files
            .iter()
            .zip(discarded.before.iter().zip(&discarded.after))
            .filter(|(_, (before, after))| {
                before != after && **before != Snap::Unknown && **after != Snap::Unknown
            })
            .map(|(file, (before, after))| {
                let changed = tf!("{file} 在丢弃后又被修改，未撤销。", file = file.as_str());
                Step::local(workfile::unsnap_args(file, before, after, &changed))
                    .shown(format!("restore {}", fr_core::git::sh_quote(file)))
            })
            .collect();
        if steps.is_empty() {
            window.push_notification(Notification::info(tr("丢弃的内容已经放回")), cx);
            return;
        }
        let count = steps.len();
        self.run_seq(
            tr("撤销丢弃"),
            steps,
            window,
            cx,
            move |_, _, window, cx| {
                let text = tf!("已撤销丢弃，放回了 {count} 个文件", count);
                window.push_notification(Notification::success(text), cx);
            },
        );
    }
}
