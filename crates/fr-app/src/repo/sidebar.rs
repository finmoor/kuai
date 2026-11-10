//! 侧栏：本地修改、所有提交、分支、远程分支、标签、贮藏。
//!
//! 单击分支、标签：在提交图里跳到它指向的提交；双击本地分支：切换过去；
//! 双击远程分支：切到同名本地分支，没有就新建一个跟踪它的。

use std::collections::HashSet;
use std::ops::Range;

use fr_core::refs::Refs;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{Mode, RepoView};

const WIDTH: f32 = 230.;
const ROW_HEIGHT: f32 = 26.;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum SideSection {
    Branches,
    Remotes,
    Tags,
    Stashes,
}

impl SideSection {
    fn title(self) -> &'static str {
        match self {
            SideSection::Branches => "分支",
            SideSection::Remotes => "远程",
            SideSection::Tags => "标签",
            SideSection::Stashes => "贮藏",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SideItem {
    Changes,
    History,
    Header(SideSection),
    Branch(usize),
    Remote(usize),
    Tag(usize),
    Stash(usize),
}

pub(super) struct Sidebar {
    collapsed: HashSet<SideSection>,
    scroll: UniformListScrollHandle,
    selected: SideItem,
}

impl Default for Sidebar {
    fn default() -> Self {
        Self {
            // 标签可能有几千个，默认收起
            collapsed: HashSet::from([SideSection::Tags]),
            scroll: UniformListScrollHandle::new(),
            selected: SideItem::History,
        }
    }
}

impl RepoView {
    fn sidebar_items(&self) -> Vec<SideItem> {
        let mut items = vec![SideItem::Changes, SideItem::History];
        let sections = [
            (SideSection::Branches, self.refs.branches.len()),
            (SideSection::Remotes, self.refs.remotes.len()),
            (SideSection::Tags, self.refs.tags.len()),
            (SideSection::Stashes, self.refs.stashes.len()),
        ];
        for (section, count) in sections {
            items.push(SideItem::Header(section));
            if self.sidebar.collapsed.contains(&section) {
                continue;
            }
            items.extend((0..count).map(|i| match section {
                SideSection::Branches => SideItem::Branch(i),
                SideSection::Remotes => SideItem::Remote(i),
                SideSection::Tags => SideItem::Tag(i),
                SideSection::Stashes => SideItem::Stash(i),
            }));
        }
        items
    }

    pub(super) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (bg, border) = (theme.sidebar, theme.sidebar_border);
        let count = self.sidebar_items().len();
        div()
            .w(px(WIDTH))
            .flex_none()
            .h_full()
            .bg(bg)
            .border_r_1()
            .border_color(border)
            .child(
                uniform_list(
                    "sidebar",
                    count,
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        this.render_sidebar_rows(range, cx)
                    }),
                )
                .track_scroll(&self.sidebar.scroll)
                .size_full(),
            )
            .into_any_element()
    }

    fn render_sidebar_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let items = self.sidebar_items();
        let theme = cx.theme();
        let (muted, active, hover, danger) = (
            theme.muted_foreground,
            theme.sidebar_accent,
            theme.list_hover,
            theme.danger,
        );
        let idle = self.op.is_none();
        let changed = self.status.as_ref().map_or(0, |s| s.entries.len());
        range
            .filter_map(|ix| {
                let item = *items.get(ix)?;
                let selected = item == self.sidebar.selected;
                let row = h_flex()
                    .id(ix)
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .px_2()
                    .gap_1()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(active))
                    .when(!selected, |d| d.hover(|s| s.bg(hover)))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.click_sidebar(item, event.click_count(), window, cx)
                    }));
                let row = match item {
                    SideItem::Changes => row
                        .child(div().flex_1().child("本地修改"))
                        .when(changed > 0, |r| {
                            r.child(div().text_xs().text_color(muted).child(changed.to_string()))
                        }),
                    SideItem::History => row.child("所有提交"),
                    SideItem::Header(section) => {
                        let count = match section {
                            SideSection::Branches => self.refs.branches.len(),
                            SideSection::Remotes => self.refs.remotes.len(),
                            SideSection::Tags => self.refs.tags.len(),
                            SideSection::Stashes => self.refs.stashes.len(),
                        };
                        let arrow = if self.sidebar.collapsed.contains(&section) {
                            "▸"
                        } else {
                            "▾"
                        };
                        // uniform_list 里每行高度固定，用内边距往下挪，外边距会压到下一行
                        row.pt_1()
                            .text_xs()
                            .text_color(muted)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(div().w(px(12.)).child(arrow))
                            .child(div().flex_1().child(section.title()))
                            .child(count.to_string())
                    }
                    SideItem::Branch(i) => {
                        let branch = self.refs.branches.get(i)?;
                        let mut detail = String::new();
                        if branch.ahead > 0 {
                            detail.push_str(&format!("↑{} ", branch.ahead));
                        }
                        if branch.behind > 0 {
                            detail.push_str(&format!("↓{}", branch.behind));
                        }
                        let row = row
                            .pl(px(20.))
                            .child(div().w(px(12.)).flex_none().child(if branch.current {
                                "●"
                            } else {
                                ""
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .when(branch.current, |d| d.font_weight(FontWeight::SEMIBOLD))
                                    .child(branch.name.clone()),
                            )
                            .when(branch.gone, |r| {
                                r.child(div().text_xs().text_color(danger).child("上游已删"))
                            })
                            .when(!detail.is_empty(), |r| {
                                r.child(div().text_xs().text_color(muted).child(detail))
                            });
                        // 选中的不是当前分支：这一行出现「切换」「删除」
                        if selected && !branch.current {
                            let (checkout, delete) = (branch.name.clone(), branch.name.clone());
                            row.child(
                                Button::new(("branch-checkout", i))
                                    .xsmall()
                                    .ghost()
                                    .label("切换")
                                    .tooltip("git checkout，双击这一行也可以")
                                    .disabled(!idle)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.checkout_branch(&checkout, window, cx)
                                    })),
                            )
                            .child(
                                Button::new(("branch-delete", i))
                                    .xsmall()
                                    .ghost()
                                    .label("删除")
                                    .tooltip("git branch -d：删除这个本地分支")
                                    .disabled(!idle)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.delete_branch(delete.clone(), window, cx)
                                    })),
                            )
                        } else {
                            row
                        }
                    }
                    SideItem::Remote(i) => {
                        let remote = self.refs.remotes.get(i)?;
                        row.pl(px(32.)).child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(remote.name.clone()),
                        )
                    }
                    SideItem::Tag(i) => {
                        let tag = self.refs.tags.get(i)?;
                        row.pl(px(32.))
                            .child(div().flex_1().min_w_0().truncate().child(tag.name.clone()))
                    }
                    SideItem::Stash(i) => {
                        let stash = self.refs.stashes.get(i)?;
                        let row = row.pl(px(32.)).child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(stash_label(&stash.message)),
                        );
                        if selected {
                            let (apply, pop, drop) = (
                                stash.selector.clone(),
                                stash.selector.clone(),
                                (stash.selector.clone(), stash.message.clone()),
                            );
                            row.child(
                                Button::new(("stash-apply", i))
                                    .xsmall()
                                    .ghost()
                                    .label("应用")
                                    .tooltip("git stash apply：恢复改动，贮藏留着")
                                    .disabled(!idle)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.stash_apply(apply.clone(), window, cx)
                                    })),
                            )
                            .child(
                                Button::new(("stash-pop", i))
                                    .xsmall()
                                    .ghost()
                                    .label("弹出")
                                    .tooltip("git stash pop：恢复改动并删掉这个贮藏")
                                    .disabled(!idle)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.stash_pop(Some(pop.clone()), window, cx)
                                    })),
                            )
                            .child(
                                Button::new(("stash-drop", i))
                                    .xsmall()
                                    .ghost()
                                    .label("删除")
                                    .disabled(!idle)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.stash_drop(drop.0.clone(), drop.1.clone(), window, cx)
                                    })),
                            )
                        } else {
                            row
                        }
                    }
                };
                Some(row.into_any_element())
            })
            .collect()
    }

    /// 引用刷新后找回原来选中的那一项：分支、远程分支、标签按名字，贮藏按提交。
    /// 侧栏按下标选中，不找回的话，比如弹出一个贮藏后，选中的会变成下一个贮藏，
    /// 那一行的「弹出」「删除」按钮就对着别的贮藏了。找不到就退回到当前视图对应的那一项。
    pub(super) fn remap_sidebar_selection(&mut self, old: &Refs) {
        let new = &self.refs;
        let found = match self.sidebar.selected {
            SideItem::Branch(i) => old
                .branches
                .get(i)
                .and_then(|b| new.branches.iter().position(|n| n.name == b.name))
                .map(SideItem::Branch),
            SideItem::Remote(i) => old
                .remotes
                .get(i)
                .and_then(|b| new.remotes.iter().position(|n| n.name == b.name))
                .map(SideItem::Remote),
            SideItem::Tag(i) => old
                .tags
                .get(i)
                .and_then(|t| new.tags.iter().position(|n| n.name == t.name))
                .map(SideItem::Tag),
            SideItem::Stash(i) => old
                .stashes
                .get(i)
                .and_then(|s| new.stashes.iter().position(|n| n.target == s.target))
                .map(SideItem::Stash),
            other => Some(other),
        };
        self.sidebar.selected = found.unwrap_or(match self.mode {
            Mode::Changes => SideItem::Changes,
            Mode::History => SideItem::History,
        });
    }

    /// 切到本地修改；还没选文件就选第一个。
    pub(super) fn show_changes(&mut self, cx: &mut Context<Self>) {
        self.sidebar.selected = SideItem::Changes;
        self.mode = Mode::Changes;
        if self.changes_selection_empty() {
            self.select_first_file(cx);
        }
    }

    fn click_sidebar(
        &mut self,
        item: SideItem,
        clicks: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match item {
            SideItem::Header(section) => {
                if !self.sidebar.collapsed.remove(&section) {
                    self.sidebar.collapsed.insert(section);
                }
            }
            SideItem::Changes => self.show_changes(cx),
            SideItem::History => {
                self.sidebar.selected = item;
                self.mode = Mode::History;
            }
            SideItem::Branch(i) => {
                let Some(branch) = self.refs.branches.get(i).cloned() else {
                    return;
                };
                self.sidebar.selected = item;
                self.mode = Mode::History;
                if clicks >= 2 {
                    self.checkout_branch(&branch.name, window, cx);
                } else {
                    self.reveal_commit(&branch.target, cx);
                }
            }
            SideItem::Remote(i) => {
                let Some(remote) = self.refs.remotes.get(i).cloned() else {
                    return;
                };
                self.sidebar.selected = item;
                self.mode = Mode::History;
                if clicks >= 2 {
                    self.checkout_remote(&remote.name, window, cx);
                } else {
                    self.reveal_commit(&remote.target, cx);
                }
            }
            SideItem::Tag(i) => {
                let Some(tag) = self.refs.tags.get(i).cloned() else {
                    return;
                };
                self.sidebar.selected = item;
                self.mode = Mode::History;
                self.reveal_commit(&tag.target, cx);
            }
            SideItem::Stash(_) => self.sidebar.selected = item,
        }
        cx.notify();
    }
}

/// 贮藏的说明去掉 git 加的前缀：`On main: 说明` → `说明`，`WIP on main: abc 说明` → `abc 说明`。
fn stash_label(message: &str) -> String {
    match message.split_once(": ") {
        Some((prefix, rest)) if prefix.starts_with("On ") || prefix.starts_with("WIP on ") => {
            rest.to_string()
        }
        _ => message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::stash_label;

    #[test]
    fn 贮藏说明去掉前缀() {
        assert_eq!(stash_label("On main: 试验"), "试验");
        assert_eq!(stash_label("WIP on feature/x: abc123 说明"), "abc123 说明");
        assert_eq!(stash_label("自定义"), "自定义");
    }
}
