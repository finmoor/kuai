//! 侧栏收起后的图标栏：48 宽，从上到下是仓库的图标、Git、终端、文件三页的切换、这一页的几项，
//! 最下面是展开侧栏的按钮。
//!
//! - Git 页：本地修改（带改动的文件数）、所有提交，和分支、远程、标签、贮藏、子模块。后几个
//!   点一下在右边弹出列表，点列表里的一项和在侧栏里单击那一行一样（在提交图里跳过去；贮藏是
//!   显示里面的改动；子模块是在新标签页里打开）。切换分支、删除这些操作要展开侧栏。
//! - 终端页：各终端的序号，点一下切过去，右键和侧栏里那一行一样；最后是「新终端」。
//! - 文件页：展开侧栏看目录树、新建文件、上传。目录树要展开侧栏才看得到。
//!
//! 收起、展开见 [`super::layout`]：所有标签页、两页共用一个状态，存在设置里。

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::layout::{RAIL_WIDTH, Split};
use super::sidebar::{SideItem, SideSection, stash_label};
use super::{Page, RepoView, TOOLBAR_HEIGHT};
use crate::ui;

/// 图标栏上按钮的边长
const BUTTON: f32 = 34.;
/// 弹出的列表最多列这么多项，再多的展开侧栏看（标签可能有几千个）
const MENU_MAX: usize = 100;

/// 图标栏上的一个按钮：只有图标，选中时有底色。
pub(super) fn rail_button(id: impl Into<ElementId>, icon: IconName, selected: bool) -> Button {
    Button::new(id)
        .ghost()
        .w(px(BUTTON))
        .h(px(BUTTON))
        .icon(Icon::new(icon).size_4())
        .selected(selected)
}

/// 按钮右上角的小数字（有几个改动、几个终端在跑）。
fn badge(count: usize, strong: bool, cx: &App) -> impl IntoElement {
    div()
        .absolute()
        .top(px(-3.))
        .right(px(-5.))
        .child(ui::count_pill(count, strong, cx))
}

fn separator(cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .w(px(24.))
        .h(px(1.))
        .my(px(2.))
        .bg(cx.theme().sidebar_border)
}

/// 选中的这一项在哪一组里（图标栏上那一组的图标显示成选中）。
fn section_of(item: SideItem) -> Option<SideSection> {
    match item {
        SideItem::Branch(_) => Some(SideSection::Branches),
        SideItem::Remote(_) | SideItem::RemoteRoot(_) => Some(SideSection::Remotes),
        SideItem::Tag(_) => Some(SideSection::Tags),
        SideItem::Stash(_) => Some(SideSection::Stashes),
        SideItem::Submodule(_) => Some(SideSection::Submodules),
        SideItem::Header(section) => Some(section),
        SideItem::Changes
        | SideItem::History
        | SideItem::SubRepos
        | SideItem::Worktrees
        | SideItem::Compare
        | SideItem::Recover
        | SideItem::Folder(_) => None,
    }
}

/// 一组里没有东西时，弹出的列表里写的话。
fn empty_text(section: SideSection) -> &'static str {
    match section {
        SideSection::Branches => tr("暂无分支"),
        SideSection::Remotes => tr("暂无远程分支"),
        SideSection::Tags => tr("暂无标签"),
        SideSection::Stashes => tr("暂无贮藏"),
        SideSection::Submodules => tr("暂无子模块"),
    }
}

/// 弹出的列表：这一组的各项，点一下和在侧栏里单击那一行一样。
fn section_menu(
    view: &WeakEntity<RepoView>,
    section: SideSection,
    menu: PopupMenu,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let Some(entity) = view.upgrade() else {
        return menu;
    };
    let entries = entity.read(cx).rail_entries(section);
    let gone = match section {
        SideSection::Branches => entity.read(cx).gone_count(),
        _ => 0,
    };
    let mut menu = menu.max_h(px(420.)).scrollable(true);
    // 有上游已删除的分支：最上面一项是清理它们（放在下面的话，分支多了要滚到底才看得到）
    if gone > 0 {
        let view = view.clone();
        let label = tf!("清理 {n} 个上游已删除的分支…", n = gone);
        menu = menu
            .item(
                PopupMenuItem::new(label)
                    .icon(IconName::BrushCleaning)
                    .on_click(move |_, window, cx| {
                        view.update(cx, |this, cx| this.sweep_gone_branches(window, cx))
                            .ok();
                    }),
            )
            .separator();
    }
    if entries.is_empty() {
        return menu.item(PopupMenuItem::new(empty_text(section)).disabled(true));
    }
    let total = entries.len();
    for (item, label, key, checked) in entries.into_iter().take(MENU_MAX) {
        let view = view.clone();
        menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(
            move |_, window, cx| {
                view.update(cx, |this, cx| this.click_rail_entry(item, &key, window, cx))
                    .ok();
            },
        ));
    }
    if total > MENU_MAX {
        let view = view.clone();
        menu = menu.separator().item(
            PopupMenuItem::new(tf!("展开侧栏以查看全部 {n} 项", n = total)).on_click(
                move |_, window, cx| {
                    view.update(cx, |this, cx| {
                        this.toggle_sidebar(window, cx);
                        this.reveal_section(section, window, cx);
                    })
                    .ok();
                },
            ),
        );
    }
    menu
}

impl RepoView {
    /// 收成图标栏的侧栏。右边那条线也是分隔条：往右拖开、双击都会展开。
    pub(super) fn render_rail(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let (bg, border) = (cx.theme().sidebar, cx.theme().sidebar_border);
        let items = match self.page {
            Page::Git => self.render_git_rail(cx),
            Page::Terminal => self.render_terminal_rail(cx),
            Page::Files => self.render_files_rail(cx),
        };
        v_flex()
            .w(px(RAIL_WIDTH))
            .flex_none()
            .h_full()
            .relative()
            .bg(bg)
            .child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .h(px(TOOLBAR_HEIGHT))
                    .justify_center()
                    .border_b_1()
                    .border_color(border)
                    // 子仓库的页面：这个位置是返回按钮
                    .child(match self.render_rail_back(cx) {
                        Some(back) => back,
                        None => self.render_rail_repo(cx).into_any_element(),
                    }),
            )
            .child(
                v_flex()
                    .id("rail-items")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .items_center()
                    .pt(px(8.))
                    .pb(px(4.))
                    .gap(px(4.))
                    .overflow_y_scroll()
                    .child(self.render_rail_pages(cx))
                    .child(separator(cx))
                    .children(items),
            )
            .child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .py(px(8.))
                    .justify_center()
                    .child(
                        rail_button("rail-expand", IconName::PanelLeftOpen, false)
                            .tooltip(tr("展开侧栏（Ctrl+Shift+B）"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_sidebar(window, cx)),
                            ),
                    ),
            )
            .child(self.split_handle(Split::Sidebar, cx))
            .into_any_element()
    }

    /// 最上面的仓库图标，和侧栏顶上的一样；停上去看名字和位置，右键和侧栏顶上一样。
    fn render_rail_repo(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = cx.theme().primary;
        let remote = self.entry.server.is_some();
        let name = self.app.read(cx).repo_name(&self.entry);
        let tip = format!("{name}\n{} · {}", self.place_label(cx), self.entry.path);
        let (view, entry) = (cx.entity().downgrade(), self.entry.clone());
        div()
            .id("rail-repo")
            .size(px(28.))
            .rounded(px(7.))
            .bg(accent.opacity(0.14))
            .flex()
            .items_center()
            .justify_center()
            .child(
                ui::icon(if remote {
                    IconName::Server
                } else {
                    IconName::FolderGit2
                })
                .text_color(accent),
            )
            .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
            .context_menu(move |menu, _, cx| {
                let app = view.upgrade().map(|this| this.read(cx).app.clone());
                let mapped = entry.clone();
                let (view, entry) = (view.clone(), entry.clone());
                let path = entry.path.clone();
                let menu = menu
                    .item(PopupMenuItem::new(tr("重命名…")).on_click(move |_, _, cx| {
                        view.update(cx, |_, cx| cx.emit(super::RepoEvent::Rename(entry.clone())))
                            .ok();
                    }))
                    .item(
                        PopupMenuItem::new(tr("复制仓库路径")).on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                        }),
                    );
                match app {
                    Some(app) => crate::mapping::entry_items(menu, &app, &mapped, cx),
                    None => menu,
                }
            })
    }

    /// Git、终端、文件三页的切换，竖着放，样子和侧栏里横着的那个一样；终端上标出有几个在跑，
    /// 文件上标出有几个没保存。
    fn render_rail_pages(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (track, fg, muted) = (theme.secondary, theme.foreground, theme.muted_foreground);
        let dark = theme.is_dark();
        let on_bg = if dark {
            track.blend(fg.opacity(0.12))
        } else {
            theme.background
        };
        let running = self.terminals_running(cx);
        let unsaved = self.unsaved_files();
        let half = |id: &'static str, page: Page, icon: IconName, tip: &'static str| {
            let on = self.page == page;
            div()
                .id(id)
                .relative()
                .size(px(30.))
                .rounded(px(5.))
                .flex()
                .items_center()
                .justify_center()
                .text_color(if on { fg } else { muted })
                .when(on, |half| {
                    half.bg(on_bg).when(!dark, |half| half.shadow_xs())
                })
                .when(!on, |half| {
                    half.cursor_pointer().hover(|s| s.text_color(fg))
                })
                .child(Icon::new(icon).size_4())
                .tooltip(move |window, cx| ui::tip(tip, window, cx))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if this.page != page {
                        this.show_page(page, window, cx);
                    }
                }))
        };
        v_flex()
            .flex_none()
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(7.))
            .bg(track)
            .child(half("rail-git", Page::Git, IconName::GitBranch, "Git"))
            .child(
                half(
                    "rail-terminal",
                    Page::Terminal,
                    IconName::SquareTerminal,
                    tr("终端（Ctrl+`）"),
                )
                .when(running > 0, |half| half.child(badge(running, false, cx))),
            )
            .child(
                half(
                    "rail-files",
                    Page::Files,
                    IconName::Files,
                    tr("文件（Ctrl+Shift+E）"),
                )
                .when(unsaved > 0, |half| half.child(badge(unsaved, true, cx))),
            )
    }

    /// Git 页的那几项：本地修改、所有提交，和分支、远程、标签、贮藏、子模块的弹出列表。
    fn render_git_rail(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let selected = self.sidebar.selected;
        let changes = self.change_count();
        let changes_button = rail_button(
            "rail-changes",
            IconName::FileDiff,
            selected == SideItem::Changes,
        )
        .tooltip(tr("本地修改（Ctrl+1）"))
        .on_click(
            cx.listener(|this, _, window, cx| this.click_sidebar(SideItem::Changes, 1, window, cx)),
        );
        let mut items = vec![
            // 数字放在按钮外面：放进按钮里的话，是挨着图标排的，贴不到按钮的右上角
            div()
                .relative()
                .flex_none()
                .child(changes_button)
                .when(changes > 0, |d| d.child(badge(changes, true, cx)))
                .into_any_element(),
            rail_button(
                "rail-history",
                IconName::GitCommitHorizontal,
                selected == SideItem::History,
            )
            .tooltip(tr("所有提交（Ctrl+2）"))
            .on_click(cx.listener(|this, _, window, cx| {
                this.click_sidebar(SideItem::History, 1, window, cx)
            }))
            .into_any_element(),
            rail_button(
                "rail-compare",
                IconName::GitCompare,
                selected == SideItem::Compare,
            )
            .tooltip(tr("比较"))
            .on_click(cx.listener(|this, _, window, cx| {
                this.click_sidebar(SideItem::Compare, 1, window, cx)
            }))
            .into_any_element(),
        ];
        if self.has_subs() {
            items.push(
                rail_button(
                    "rail-subs",
                    IconName::FolderTree,
                    selected == SideItem::SubRepos,
                )
                .tooltip(tr("子仓库"))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.click_sidebar(SideItem::SubRepos, 1, window, cx)
                }))
                .into_any_element(),
            );
        }
        if self.has_worktrees_item() {
            items.push(
                rail_button(
                    "rail-worktrees",
                    IconName::Layers,
                    selected == SideItem::Worktrees,
                )
                .tooltip(tr("工作树"))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.click_sidebar(SideItem::Worktrees, 1, window, cx)
                }))
                .into_any_element(),
            );
        }
        items.push(
            rail_button(
                "rail-recover",
                IconName::ArchiveRestore,
                selected == SideItem::Recover,
            )
            .tooltip(tr("找回提交"))
            .on_click(cx.listener(|this, _, window, cx| {
                this.click_sidebar(SideItem::Recover, 1, window, cx)
            }))
            .into_any_element(),
        );
        items.push(separator(cx).into_any_element());
        let mut sections = vec![
            SideSection::Branches,
            SideSection::Remotes,
            SideSection::Tags,
            SideSection::Stashes,
        ];
        // 和侧栏一样：没有子模块的仓库不显示这一组
        if !self.submodules.is_empty() {
            sections.push(SideSection::Submodules);
        }
        let view = cx.entity().downgrade();
        for section in sections {
            let view = view.clone();
            items.push(
                rail_button(
                    ("rail-section", section as usize),
                    section.icon(),
                    section_of(selected) == Some(section),
                )
                .tooltip(section.title())
                .dropdown_menu_with_anchor(Anchor::LeftCenter, move |menu, _, cx| {
                    section_menu(&view, section, menu, cx)
                })
                .into_any_element(),
            );
        }
        items
    }

    /// 弹出列表里的各项：在侧栏里对应的那一项、显示的名字、认它用的名字（贮藏是提交，见
    /// [`Self::rail_item_now`]）、打不打勾（当前分支）。
    fn rail_entries(&self, section: SideSection) -> Vec<(SideItem, String, String, bool)> {
        let refs = &self.refs;
        match section {
            SideSection::Branches => refs
                .branches
                .iter()
                .enumerate()
                .map(|(i, branch)| {
                    let mut label = branch.name.to_string();
                    if branch.ahead > 0 {
                        label.push_str(&format!("  ↑{}", branch.ahead));
                    }
                    if branch.behind > 0 {
                        label.push_str(&format!("  ↓{}", branch.behind));
                    }
                    (
                        SideItem::Branch(i),
                        label,
                        branch.name.clone(),
                        branch.current,
                    )
                })
                .collect(),
            SideSection::Remotes => refs
                .remotes
                .iter()
                .enumerate()
                .map(|(i, branch)| {
                    let name = branch.name.to_string();
                    (SideItem::Remote(i), name.clone(), name, false)
                })
                .collect(),
            SideSection::Tags => refs
                .tags
                .iter()
                .enumerate()
                .map(|(i, tag)| {
                    let name = tag.name.to_string();
                    (SideItem::Tag(i), name.clone(), name, false)
                })
                .collect(),
            SideSection::Stashes => refs
                .stashes
                .iter()
                .enumerate()
                .map(|(i, stash)| {
                    let label = stash_label(&stash.message);
                    (SideItem::Stash(i), label, stash.target.clone(), false)
                })
                .collect(),
            SideSection::Submodules => self
                .submodules
                .iter()
                .enumerate()
                .map(|(i, path)| (SideItem::Submodule(i), path.clone(), path.clone(), false))
                .collect(),
        }
    }

    /// 弹出列表里那一项现在在第几个：按名字（贮藏按提交、子模块按路径）在现在的列表里找，没了是
    /// None。菜单开着时引用可能已经重读了（自动获取、文件监听），列表多一个少一个，按原来的下标取
    /// 就成了别的那个。
    fn rail_item_now(&self, item: SideItem, key: &str) -> Option<SideItem> {
        let refs = &self.refs;
        match item {
            SideItem::Branch(_) => refs
                .branches
                .iter()
                .position(|b| b.name == key)
                .map(SideItem::Branch),
            SideItem::Remote(_) => refs
                .remotes
                .iter()
                .position(|b| b.name == key)
                .map(SideItem::Remote),
            SideItem::Tag(_) => refs
                .tags
                .iter()
                .position(|t| t.name == key)
                .map(SideItem::Tag),
            SideItem::Stash(_) => refs
                .stashes
                .iter()
                .position(|s| s.target == key)
                .map(SideItem::Stash),
            SideItem::Submodule(_) => self
                .submodules
                .iter()
                .position(|path| path == key)
                .map(SideItem::Submodule),
            other => Some(other),
        }
    }

    /// 点了弹出列表里的一项：和在侧栏里单击那一行一样；子模块单击就打开（侧栏里要双击）。
    fn click_rail_entry(
        &mut self,
        item: SideItem,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 已经没了（删掉了、被别处改了名）就不做
        let Some(item) = self.rail_item_now(item, key) else {
            return;
        };
        let clicks = match item {
            SideItem::Submodule(_) => 2,
            _ => 1,
        };
        self.click_sidebar(item, clicks, window, cx);
    }
}
