//! 提交列表多选：Ctrl 加点击加减、Shift 加点击连选一段，选中两个以上时下面的详情换成
//! 批量操作：比较（正好两个时）、挑拣到当前分支、还原、压缩成一个、复制哈希、导出补丁。右键菜单
//! 里也是这几样。
//!
//! 挑拣按从旧到新、还原按从新到旧做（和 git 的习惯一致，后面的改动接得上前面的）；遇到冲突和单个
//! 提交一样停下来，横幅里继续或中止。压缩交给交互式变基（见 `rebase.rs`）：选中的要是当前分支上连着
//! 的一段，最旧的那个保留，其余的并进去，对话框里还能再看、再改。

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use super::menus::MenuFn;
use super::ops::args;
use crate::ui;

/// 选中的一个提交。
#[derive(Clone, Debug)]
pub(super) struct Picked {
    /// 在列表里的位置（新的在前）
    pub(super) ix: usize,
    pub(super) id: String,
    pub(super) subject: String,
    pub(super) merge: bool,
}

/// 能不能把选中的压缩成一个：要是当前分支上连着的一段（一个接一个的第一个父提交）、没有合并提交、
/// 最旧的那个有父提交。能的话返回变基的起点（最旧的那个的父提交）。
pub(super) fn squash_base(
    picked: &[Picked],
    parents: impl Fn(usize) -> Vec<u32>,
) -> Result<String, &'static str> {
    let (Some(newest), Some(oldest)) = (picked.first(), picked.last()) else {
        return Err(tr("请至少选中两个提交"));
    };
    if picked.len() < 2 {
        return Err(tr("请至少选中两个提交"));
    }
    if picked.iter().any(|p| p.merge) {
        return Err(tr("选中的提交里有合并提交，无法压缩"));
    }
    for pair in picked.windows(2) {
        let (newer, older) = (&pair[0], &pair[1]);
        let first = parents(newer.ix).first().copied();
        if older.ix != newer.ix + 1 || first != Some(older.ix as u32) {
            return Err(tr("只能压缩一个接一个的提交（中间不能隔着别的提交）"));
        }
    }
    if parents(oldest.ix).is_empty() {
        return Err(tr("最早的提交是第一个提交，无法压缩到它之前"));
    }
    let _ = newest;
    Ok(format!("{}^", oldest.id))
}

impl RepoView {
    /// 多选着的提交（两个以上才算），按列表的顺序（新的在前）。
    pub(super) fn picked_commits(&self) -> Vec<Picked> {
        if self.history.multi.len() < 2 {
            return Vec::new();
        }
        let mut picked: Vec<Picked> = self
            .history
            .multi
            .iter()
            .filter_map(|id| {
                let ix = self.history.position(id)?;
                let commit = self.history.commit_at(ix)?;
                Some(Picked {
                    ix,
                    id: commit.id.to_string(),
                    subject: commit.subject.to_string(),
                    merge: commit.parents.len() > 1,
                })
            })
            .collect();
        picked.sort_by_key(|p| p.ix);
        picked
    }

    /// 清掉多选（换了列表、点了别的）。
    pub(super) fn clear_multi_commits(&mut self) {
        self.history.multi.clear();
    }

    /// Ctrl 加点击第 `ix` 个提交：加进多选，已经在里面就去掉。原来只选了一个的，那一个也算进来。
    pub(super) fn toggle_multi_commit(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(id) = self.history.commit_at(ix).map(|c| c.id.to_string()) else {
            return;
        };
        if self.history.multi.len() < 2 {
            self.history.multi.clear();
            if let Some(current) = self.history.selected_commit().map(|c| c.id.to_string()) {
                self.history.multi.push(current);
            }
        }
        match self.history.multi.iter().position(|picked| *picked == id) {
            Some(at) => {
                self.history.multi.remove(at);
            }
            None => self.history.multi.push(id),
        }
        // 只剩一个：回到普通的单选，选它
        if self.history.multi.len() < 2 {
            let last = self.history.multi.pop();
            match last.and_then(|id| self.history.position(&id)) {
                Some(at) => self.select_commit(at, cx),
                None => self.select_commit(ix, cx),
            }
        }
        cx.notify();
    }

    /// Shift 加点击第 `ix` 个提交：从现在选中的那个连到它，中间的都选上。
    pub(super) fn range_multi_commit(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(from) = self.history.selected_index() else {
            self.select_commit(ix, cx);
            return;
        };
        let (lo, hi) = (from.min(ix), from.max(ix));
        let ids: Vec<String> = (lo..=hi)
            .filter_map(|at| self.history.commit_at(at).map(|c| c.id.to_string()))
            .collect();
        if ids.len() < 2 {
            self.select_commit(ix, cx);
            return;
        }
        self.history.multi = ids;
        cx.notify();
    }

    /// 压缩选中的提交能不能做；能的话是变基的起点。
    fn squash_check(&self, picked: &[Picked]) -> Result<String, &'static str> {
        squash_base(picked, |ix| {
            self.history
                .commit_at(ix)
                .map(|c| c.parents.clone())
                .unwrap_or_default()
        })
    }

    /// 挑拣选中的提交到当前分支（从旧到新）。
    fn cherry_pick_picked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = self.picked_commits();
        if picked.is_empty() {
            return;
        }
        let mut pick = args(&["cherry-pick"]);
        // 合并提交要说明相对哪个父提交：取第一个，和界面上显示的改动一致
        if picked.iter().any(|p| p.merge) {
            pick.extend(args(&["-m", "1"]));
        }
        pick.extend(picked.iter().rev().map(|p| p.id.clone()));
        let count = picked.len();
        self.run_op(
            tr("挑拣"),
            pick,
            None,
            window,
            cx,
            move |this, _, w, cx| {
                this.clear_multi_commits();
                let text = tf!("已挑拣 {count} 个提交到当前分支", count);
                w.push_notification(Notification::success(text), cx);
            },
        );
    }

    /// 还原选中的提交（从新到旧，各生成一个还原提交）。
    fn revert_picked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = self.picked_commits();
        if picked.is_empty() {
            return;
        }
        let mut revert = args(&["revert", "--no-edit"]);
        if picked.iter().any(|p| p.merge) {
            revert.extend(args(&["-m", "1"]));
        }
        revert.extend(picked.iter().map(|p| p.id.clone()));
        let count = picked.len();
        let description = tf!(
            "将按从新到旧的顺序为这 {count} 个提交各新建一个还原提交，撤销它们的改动。",
            count
        );
        self.confirm_plain(
            tf!("还原 {count} 个提交？", count),
            description,
            tr("还原"),
            window,
            cx,
            move |this, window, cx| {
                this.run_op(
                    tr("还原"),
                    revert.clone(),
                    None,
                    window,
                    cx,
                    move |this, _, w, cx| {
                        this.clear_multi_commits();
                        let text = tf!("已新建 {count} 个提交以还原所选提交", count);
                        w.push_notification(Notification::success(text), cx);
                    },
                );
            },
        );
    }

    /// 压缩选中的提交：打开交互式变基，最旧的保留，其余的预先设成并入下方提交。
    fn squash_picked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = self.picked_commits();
        match self.squash_check(&picked) {
            Ok(base) => {
                // 最旧的保留，其余的并进去
                let squash = picked[..picked.len() - 1]
                    .iter()
                    .map(|p| p.id.clone())
                    .collect();
                self.interactive_rebase_with(base, squash, window, cx);
            }
            Err(reason) => window.push_notification(Notification::warning(reason), cx),
        }
    }

    /// 复制选中的提交的哈希，一行一个（新的在前）。
    fn copy_picked_hashes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = self.picked_commits();
        let text: Vec<String> = picked.iter().map(|p| p.id.clone()).collect();
        cx.write_to_clipboard(ClipboardItem::new_string(text.join("\n")));
        let note = tf!("已复制 {n} 个提交的哈希", n = picked.len());
        window.push_notification(Notification::success(note), cx);
    }

    /// 选中的提交当补丁的来源（新的在前，`format-patch` 出来就是旧的在前）。
    pub(super) fn picked_patch_source(&self) -> super::patch_io::Source {
        let ids = self.picked_commits().into_iter().map(|p| p.id).collect();
        super::patch_io::Source::Commits(ids)
    }

    /// 把选中的提交导出成一个补丁文件（`git format-patch`，从旧到新），存到本机选的位置。
    fn export_picked_patches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picked_commits().is_empty() {
            return;
        }
        let source = self.picked_patch_source();
        self.export_patch(source, window, cx);
    }

    /// 多选着提交时的右键菜单。
    pub(super) fn multi_commit_menu(&self, cx: &mut Context<Self>) -> MenuFn {
        let view = cx.entity().downgrade();
        let picked = self.picked_commits();
        let count = picked.len();
        let pair = (count == 2).then(|| (picked[0].id.clone(), picked[1].id.clone()));
        let squash = self.squash_check(&picked);
        let detached = self.status.as_ref().is_some_and(|s| s.branch.is_none());
        Box::new(move |menu, _, _| {
            let item =
                |label: SharedString, f: fn(&mut RepoView, &mut Window, &mut Context<RepoView>)| {
                    let view = view.clone();
                    PopupMenuItem::new(label).on_click(move |_, window, cx| {
                        view.update(cx, |this, cx| f(this, window, cx)).ok();
                    })
                };
            let mut menu = menu;
            if let Some((a, b)) = pair.clone() {
                let view = view.clone();
                menu = menu
                    .item(PopupMenuItem::new(tr("比较这两个提交")).on_click(
                        move |_, window, cx| {
                            let (a, b) = (a.clone(), b.clone());
                            view.update(cx, |this, cx| this.compare_commits(a, b, window, cx))
                                .ok();
                        },
                    ))
                    .separator();
            }
            menu.item(
                item(
                    tf!("挑拣 {count} 个提交到当前分支", count).into(),
                    |this, w, cx| this.cherry_pick_picked(w, cx),
                )
                .disabled(detached),
            )
            .item(item(
                tf!("还原 {count} 个提交…", count).into(),
                |this, w, cx| this.revert_picked(w, cx),
            ))
            .item(
                item(
                    tf!("压缩 {count} 个提交为一个…", count).into(),
                    |this, w, cx| this.squash_picked(w, cx),
                )
                .disabled(squash.is_err()),
            )
            .separator()
            .item(item(tr("复制哈希").into(), |this, w, cx| {
                this.copy_picked_hashes(w, cx)
            }))
            .item(item(tr("导出为补丁…").into(), |this, w, cx| {
                this.export_picked_patches(w, cx)
            }))
            .item(item(tr("复制为补丁").into(), |this, w, cx| {
                let source = this.picked_patch_source();
                this.copy_patch(source, w, cx)
            }))
        })
    }

    /// 多选着提交时，下面的详情区换成这一块：选了哪些、能做什么。没多选时没有。
    pub(super) fn render_multi_commits(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picked = self.picked_commits();
        if picked.len() < 2 {
            return None;
        }
        let theme = cx.theme();
        let (muted, border, panel) = (theme.muted_foreground, theme.border, theme.muted);
        let count = picked.len();
        let busy = self.op.is_some();
        let squash = self.squash_check(&picked);
        let detached = self.status.as_ref().is_some_and(|s| s.branch.is_none());
        let pair = (count == 2).then(|| (picked[0].id.clone(), picked[1].id.clone()));
        let rows: Vec<AnyElement> = picked
            .iter()
            .take(12)
            .map(|p| {
                h_flex()
                    .h(px(22.))
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .flex_none()
                            .font_family(crate::fonts::mono())
                            .text_xs()
                            .text_color(muted)
                            .child(p.id[..p.id.len().min(8)].to_string()),
                    )
                    .child(div().min_w_0().truncate().child(p.subject.clone()))
                    .into_any_element()
            })
            .collect();
        let more = count.saturating_sub(12);
        let squash_tip: SharedString = match &squash {
            Ok(_) => tr("进入「整理提交」：最早的提交保留，其余并入它，开始前还可以调整").into(),
            Err(reason) => (*reason).into(),
        };
        Some(
            v_flex()
                .size_full()
                .p_3()
                .gap_3()
                .border_t_1()
                .border_color(border)
                .child(
                    h_flex()
                        .gap_2()
                        .child(ui::icon(IconName::ListChecks).text_color(muted))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(tf!("已选中 {count} 个提交", count)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(tr("Ctrl 加点击增减，Shift 加点击连选，Esc 取消多选")),
                        ),
                )
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_2()
                        .when_some(pair, |row, (a, b)| {
                            row.child(
                                Button::new("multi-compare")
                                    .small()
                                    .primary()
                                    .icon(ui::icon(IconName::GitCompare))
                                    .label(tr("比较这两个提交"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.compare_commits(a.clone(), b.clone(), window, cx)
                                    })),
                            )
                        })
                        .child(
                            Button::new("multi-pick")
                                .small()
                                .label(tf!("挑拣到当前分支（{count}）", count))
                                .tooltip(tr("按从旧到新的顺序挑拣到当前分支"))
                                .disabled(busy || detached)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.cherry_pick_picked(window, cx)
                                })),
                        )
                        .child(
                            Button::new("multi-revert")
                                .small()
                                .label(tr("还原…"))
                                .tooltip(tr("按从新到旧的顺序各新建一个还原提交"))
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.revert_picked(window, cx)
                                })),
                        )
                        .child(
                            div().id("multi-squash-wrap").child(
                                Button::new("multi-squash")
                                    .small()
                                    .label(tr("压缩为一个…"))
                                    .tooltip(squash_tip)
                                    .disabled(busy || squash.is_err())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.squash_picked(window, cx)
                                    })),
                            ),
                        )
                        .child(
                            Button::new("multi-copy")
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::Copy))
                                .label(tr("复制哈希"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.copy_picked_hashes(window, cx)
                                })),
                        )
                        .child(
                            Button::new("multi-export")
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::FileDown))
                                .label(tr("导出为补丁文件…"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.export_picked_patches(window, cx)
                                })),
                        ),
                )
                .child(
                    v_flex()
                        .id("multi-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_2()
                        .rounded(px(ui::RADIUS))
                        .bg(panel.opacity(0.4))
                        .children(rows)
                        .when(more > 0, |list| {
                            list.child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(tf!("……另有 {more} 个", more)),
                            )
                        }),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Picked, squash_base};

    fn picked(ix: usize, merge: bool) -> Picked {
        Picked {
            ix,
            id: format!("c{ix}"),
            subject: String::new(),
            merge,
        }
    }

    #[test]
    fn 能不能压缩() {
        // 0 → 1 → 2 → 3 一条线（parents 是列表里的下标）
        let line = |ix: usize| vec![(ix + 1) as u32];
        assert_eq!(
            squash_base(&[picked(1, false), picked(2, false)], line),
            Ok("c2^".into())
        );
        // 中间隔着一个
        assert!(squash_base(&[picked(0, false), picked(2, false)], line).is_err());
        // 有合并提交
        assert!(squash_base(&[picked(0, true), picked(1, false)], line).is_err());
        // 第一个父提交不是下一个（另一条分支上的）
        let other = |ix: usize| {
            if ix == 0 {
                vec![5]
            } else {
                vec![(ix + 1) as u32]
            }
        };
        assert!(squash_base(&[picked(0, false), picked(1, false)], other).is_err());
        // 最早的是第一个提交
        let root = |ix: usize| {
            if ix == 1 {
                Vec::new()
            } else {
                vec![(ix + 1) as u32]
            }
        };
        assert!(squash_base(&[picked(0, false), picked(1, false)], root).is_err());
        assert!(squash_base(&[picked(0, false)], line).is_err());
    }
}
