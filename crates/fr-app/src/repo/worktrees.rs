//! 工作树（`git worktree`）：同一个仓库的另一份工作目录，可以同时检出不同的分支。命令行工具
//! 干活时也会自己建（Claude Code 建在 `<仓库>/.claude/worktrees/` 下，Codex 建在
//! `~/.codex/worktrees/` 下）。
//!
//! - 清单跟着每次刷新读（[`fr_core::worktree`]）。侧栏「所有提交」下面有一项「工作树」，点它右边
//!   是总览表：每个工作树在哪、在哪个分支、有几个改动、和上游差几个提交、最近一次提交；上面可以
//!   新建、清理目录已经不在的。
//! - 点表里的一行进到那个工作树：和进子仓库是同一套（见 [`super::subs`]），整页换成它的，Git、
//!   终端、文件三页都是它自己的，Alt+← 回到总览表。工作树的页面里不再列工作树，都从这张表进。
//! - 分支已经在别的工作树里检出时，侧栏的分支后面有标记，检出它时改成问要不要进那个工作树。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use fr_core::i18n::{tr, trc};
use fr_core::tf;
use fr_core::worktree::{self, Checkout, Worktree};
use futures::StreamExt as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::radio::Radio;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::dialogs::ref_name_error;
use super::sidebar::SideItem;
use super::subs::{Child, PARALLEL, ROW_HEIGHT, Summary, Then, summarize};
use super::{Mode, RepoEvent, RepoView};
use crate::settings::RepoEntry;
use crate::ui;

/// 对话框里的一个动作（确定、跟着分支名改位置），按钮、回车、输入框的变化共用。
type Act = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Default)]
pub(super) struct Worktrees {
    /// `git worktree list` 的结果，第一个是主工作树。读不到（git 太旧）时是空的
    list: Vec<Worktree>,
    /// 各个工作树的情况，按清单里的路径记
    summaries: HashMap<String, Summary>,
    summary_task: Option<Task<()>>,
    /// 新建以后等清单读回来、进到新建的那个里面
    open_task: Option<Task<()>>,
}

/// 名字后面的小标签（「当前」「Claude Code」「已锁定」）。
fn tag(text: impl Into<SharedString>, strong: bool, cx: &App) -> Div {
    let theme = cx.theme();
    let (bg, fg) = match strong {
        true => (theme.primary, theme.primary_foreground),
        false => (theme.secondary, theme.muted_foreground),
    };
    div()
        .flex_none()
        .px(px(5.))
        .h(px(16.))
        .rounded(px(4.))
        .bg(bg)
        .text_color(fg)
        .text_size(px(10.5))
        .line_height(px(16.))
        .child(text.into())
}

impl RepoView {
    /// 刷新时读到了新的清单。
    pub(super) fn set_worktrees(&mut self, list: Vec<Worktree>, cx: &mut Context<Self>) {
        if list == self.worktrees.list {
            return;
        }
        self.worktrees.list = list;
        let list = &self.worktrees.list;
        self.worktrees
            .summaries
            .retain(|path, _| list.iter().any(|tree| tree.path == *path));
        self.drop_worktrees_from_subs();
        if self.mode == Mode::Worktrees {
            match self.has_worktrees_item() {
                true => self.load_worktree_summaries(cx),
                false => self.sidebar_select_history(),
            }
        }
        cx.notify();
    }

    /// 侧栏上有没有「工作树」这一项。从工作树总览进来的页面没有：都从那张表进。
    pub(super) fn has_worktrees_item(&self) -> bool {
        let entered = self.sub_of.as_ref().is_some_and(|of| of.worktree);
        !entered && !self.worktrees.list.is_empty()
    }

    /// 目录还在的工作树有几个，主工作树也算：侧栏和总览上的数字（和原型一样，两处一致）。
    pub(super) fn live_worktrees(&self) -> usize {
        let list = self.worktrees.list.iter();
        list.filter(|tree| tree.prunable.is_none()).count()
    }

    /// 所有工作树的目录。
    pub(super) fn worktree_paths(&self) -> Vec<String> {
        let list = self.worktrees.list.iter();
        list.map(|tree| tree.path.clone()).collect()
    }

    /// 工作树的目录按这边的写法：本机的 Windows 路径用反斜杠，和单独打开它时一样
    /// （终端、记住的东西按路径认）。git 给的都是正斜杠。
    fn tree_dir(&self, path: &str) -> String {
        let windows_local = self.entry.server.is_none() && self.entry.path.contains('\\');
        match windows_local {
            true => path.replace('/', "\\"),
            false => path.to_string(),
        }
    }

    fn tree_entry(&self, path: &str) -> RepoEntry {
        RepoEntry {
            server: self.entry.server.clone(),
            path: self.tree_dir(path),
        }
    }

    /// 这个页面自己是不是这个工作树。
    fn is_own_tree(&self, tree: &Worktree) -> bool {
        worktree::same_path(&tree.path, &self.entry.path)
    }

    /// 这个分支在别的哪个工作树里检出着。
    pub(super) fn tree_holding(&self, branch: &str) -> Option<&Worktree> {
        let mut others = self.worktrees.list.iter();
        others.find(|tree| tree.branch.as_deref() == Some(branch) && !self.is_own_tree(tree))
    }

    /// 侧栏点了「工作树」：右边显示总览表。
    pub(super) fn show_worktrees(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_worktrees_item() {
            return;
        }
        self.leave_file_views(window, cx);
        self.sidebar.selected = SideItem::Worktrees;
        self.mode = Mode::Worktrees;
        self.load_worktree_summaries(cx);
        cx.notify();
    }

    /// 读每个工作树的改动数、和上游的差距、最近提交。目录已经不在的不读。
    pub(super) fn load_worktree_summaries(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let jobs: Vec<_> = self
            .worktrees
            .list
            .iter()
            .filter(|tree| tree.prunable.is_none() && !tree.bare)
            .map(|tree| {
                let (channel, key) = (channel.clone(), tree.path.clone());
                let dir = self.tree_dir(&tree.path);
                async move { (key, summarize(channel, dir).await) }
            })
            .collect();
        self.worktrees.summary_task = Some(cx.spawn(async move |this, cx| {
            let mut results = futures::stream::iter(jobs).buffer_unordered(PARALLEL);
            while let Some((key, summary)) = results.next().await {
                let updated = this.update(cx, |this, cx| {
                    this.worktrees.summaries.insert(key, summary);
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
        }));
    }

    /// 进到一个工作树：页面整个换成它的。点的是这个页面自己那一行时，去看它的提交历史。
    pub(super) fn enter_worktree(
        &mut self,
        path: &str,
        then: Then,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tree) = self.worktrees.list.iter().find(|tree| tree.path == path) else {
            return;
        };
        if tree.prunable.is_some() {
            let text = tr("此工作树的目录已不存在，可点击「清理…」移除记录。");
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        if self.is_own_tree(tree) {
            match then {
                Then::Stay => self.click_sidebar(SideItem::History, 1, window, cx),
                Then::NewTerminal => self.new_terminal(window, cx),
                Then::Files => self.show_files(window, cx),
            }
            return;
        }
        let entry = self.tree_entry(path);
        self.enter_child(Child::Worktree(path.to_string()), entry, then, window, cx);
    }

    /// 工作树总览：上面一条（有几个、清理、新建），下面是表。
    pub(super) fn render_worktrees(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, bg) = (theme.border, theme.muted_foreground, theme.background);
        let stale = self.stale_worktrees().len();
        let idle = self.op.is_none();
        let bar = h_flex()
            .flex_none()
            .h(px(44.))
            .px(px(12.))
            .gap(px(8.))
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("工作树")),
            )
            .child(ui::count_pill(self.live_worktrees(), false, cx))
            .child(
                ui::clipped(
                    "worktrees-what",
                    tr("同一仓库的另一份工作目录，可同时检出不同的分支。"),
                )
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(muted),
            )
            .when(stale > 0, |bar| {
                bar.child(
                    Button::new("worktrees-prune")
                        .small()
                        .flex_none()
                        .icon(ui::icon(IconName::BrushCleaning))
                        .label(tr("清理…"))
                        .tooltip(tr("移除目录已不存在的工作树的记录（git worktree prune）"))
                        .disabled(!idle)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.prune_worktrees_dialog(window, cx)
                        })),
                )
            })
            .child(
                Button::new("worktrees-new")
                    .small()
                    .primary()
                    .flex_none()
                    .icon(ui::icon(IconName::Plus))
                    .label(tr("新建工作树…"))
                    .disabled(!idle)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.new_worktree_dialog(None, window, cx)
                    })),
            );
        let head = |label: &'static str| {
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(muted)
                .child(label)
        };
        let header = h_flex()
            .flex_none()
            .h(px(28.))
            .px(px(12.))
            .gap(px(12.))
            .border_b_1()
            .border_color(border)
            // 列名是单数（Worktree），页面标题是复数（Worktrees）
            .child(head(trc("列", "工作树")).flex_1().min_w(px(160.)))
            .child(head(tr("分支")).w(px(170.)).flex_none())
            .child(head(tr("改动")).w(px(48.)).flex_none())
            .child(head(tr("同步")).w(px(88.)).flex_none())
            .child(head(tr("最近提交")).flex_1().min_w(px(120.)))
            .child(div().w(px(56.)).flex_none());
        let trees = self.worktrees.list.clone();
        let rows: Vec<AnyElement> = trees
            .iter()
            .enumerate()
            .map(|(ix, tree)| self.render_worktree_row(ix, tree, cx))
            .collect();
        v_flex()
            .size_full()
            .bg(bg)
            .child(bar)
            .child(header)
            .child(
                v_flex()
                    .id("worktree-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .into_any_element()
    }

    fn render_worktree_row(
        &self,
        ix: usize,
        tree: &Worktree,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, hover, accent, warning, danger) = (
            theme.border,
            theme.muted_foreground,
            theme.list_hover,
            theme.primary,
            theme.warning,
            theme.danger,
        );
        let gone = tree.prunable.is_some();
        let own = self.is_own_tree(tree);
        let name = match tree.main {
            true => tr("主工作树").to_string(),
            false => tree.name().to_string(),
        };
        // 路径按这边的写法（本机的 Windows 路径用反斜杠），git 给的是正斜杠
        let place = self.tree_dir(&worktree::short_path(&tree.path, &self.entry.path));
        let full = self.tree_dir(&tree.path);
        let name_cell = v_flex()
            .flex_1()
            .min_w(px(160.))
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(
                        ui::icon(IconName::FolderGit2)
                            .flex_none()
                            .text_color(if gone { muted } else { accent }),
                    )
                    .child(
                        ui::clipped_with_tip(("tree-name", ix), name, full.clone())
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .when(gone, |name| name.text_color(muted).line_through()),
                    )
                    .children(worktree::made_by(&tree.path).map(|by| tag(by, false, cx)))
                    .when(own, |row| row.child(tag(tr("当前"), true, cx)))
                    .when_some(tree.locked.clone(), |row, reason| {
                        let tip: SharedString = match reason.is_empty() {
                            true => tr("已锁定：清理时不会移除").into(),
                            false => tf!("已锁定：{reason}", reason).into(),
                        };
                        row.child(
                            div()
                                .id(("tree-locked", ix))
                                .flex_none()
                                .child(ui::icon_small(IconName::Lock).text_color(muted))
                                .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx)),
                        )
                    }),
            )
            // 省略前面：结尾才认得出是哪个目录。和名字对齐（图标 14、空 6）；缩进放在外面一层：
            // `clipped` 拿自己的宽度和文字比
            .child(
                div().pl(px(20.)).min_w_0().child(
                    ui::clipped_with_tip(("tree-dir", ix), place, full)
                        .text_ellipsis_start()
                        .text_size(px(11.))
                        .text_color(muted),
                ),
            );
        let dash = || div().text_color(muted).child("—");
        let summary = self.worktrees.summaries.get(&tree.path);
        // 分支用清单里的：读各个工作树的情况要一会儿，分支先显示出来
        let branch_text = match (&tree.branch, &tree.head) {
            _ if gone => tr("目录已不存在").to_string(),
            (Some(branch), _) => branch.clone(),
            (None, Some(head)) => tf!("分离的 HEAD（{head}）", head = &head[..head.len().min(8)]),
            (None, None) => tr("暂无提交").to_string(),
        };
        let on_branch = tree.branch.is_some();
        let branch = h_flex()
            .gap(px(5.))
            .min_w_0()
            .when(!gone, |row| {
                row.child(
                    ui::icon_small(IconName::GitBranch)
                        .flex_none()
                        .text_color(if on_branch { muted } else { warning }),
                )
            })
            .child(
                ui::clipped(("tree-branch", ix), branch_text)
                    .text_xs()
                    .when(gone, |text| text.text_color(muted)),
            )
            .into_any_element();
        let (changes, sync, last): (AnyElement, AnyElement, AnyElement) = match summary {
            _ if gone => (
                dash().text_xs().into_any_element(),
                dash().text_xs().into_any_element(),
                dash().text_xs().into_any_element(),
            ),
            None => (
                div().into_any_element(),
                div().into_any_element(),
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(tr("正在读取…"))
                    .into_any_element(),
            ),
            Some(Summary {
                error: Some(error), ..
            }) => (
                div().into_any_element(),
                div().into_any_element(),
                ui::clipped(("tree-error", ix), error.clone())
                    .text_xs()
                    .text_color(danger)
                    .into_any_element(),
            ),
            Some(s) => {
                let changes = match s.changes {
                    0 => dash().text_xs().into_any_element(),
                    // 包一层：直接放进定宽的格子里会被拉成和格子一样宽
                    n => h_flex()
                        .child(ui::count_pill(n, true, cx))
                        .into_any_element(),
                };
                let mut sync = String::new();
                if s.ahead > 0 {
                    sync.push_str(&format!("↑{}", s.ahead));
                }
                if s.behind > 0 {
                    if !sync.is_empty() {
                        sync.push(' ');
                    }
                    sync.push_str(&format!("↓{}", s.behind));
                }
                let sync = match (sync.is_empty(), s.upstream) {
                    (false, _) => div().text_xs().child(sync).into_any_element(),
                    (true, true) => dash().text_xs().into_any_element(),
                    (true, false) => div()
                        .text_xs()
                        .text_color(muted)
                        .child(tr("无上游"))
                        .into_any_element(),
                };
                let last = match s.time {
                    0 => dash().text_xs().into_any_element(),
                    time => v_flex()
                        .min_w_0()
                        .child(ui::clipped(("tree-subject", ix), s.subject.clone()).text_xs())
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(muted)
                                .child(super::history::format_time(time)),
                        )
                        .into_any_element(),
                };
                (changes, sync, last)
            }
        };
        let (a, b, c) = (tree.path.clone(), tree.path.clone(), tree.path.clone());
        let actions = h_flex()
            .w(px(56.))
            .flex_none()
            .gap(px(2.))
            .when(!gone, |actions| {
                actions
                    .child(
                        Button::new(("tree-terminal", ix))
                            .ghost()
                            .xsmall()
                            .icon(ui::icon(IconName::SquareTerminal).text_color(muted))
                            .tooltip(tr("在此新建终端"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.enter_worktree(&a, Then::NewTerminal, window, cx);
                            })),
                    )
                    .child(
                        Button::new(("tree-files", ix))
                            .ghost()
                            .xsmall()
                            .icon(ui::icon(IconName::Files).text_color(muted))
                            .tooltip(tr("在文件页中打开"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.enter_worktree(&b, Then::Files, window, cx);
                            })),
                    )
            });
        h_flex()
            .id(("tree-row", ix))
            .flex_none()
            .h(px(ROW_HEIGHT))
            .px(px(12.))
            .gap(px(12.))
            .border_b_1()
            .border_color(border.opacity(0.5))
            .cursor_pointer()
            .when(own, |row| row.bg(accent.opacity(0.07)))
            .hover(|s| s.bg(hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.enter_worktree(&c, Then::Stay, window, cx)
            }))
            .child(name_cell)
            .child(div().w(px(170.)).flex_none().min_w_0().child(branch))
            .child(div().w(px(48.)).flex_none().child(changes))
            .child(div().w(px(88.)).flex_none().child(sync))
            .child(div().flex_1().min_w(px(120.)).child(last))
            .child(actions)
            .context_menu(self.worktree_menu(tree, cx))
            .into_any_element()
    }

    /// 总览表里一行的右键菜单。
    fn worktree_menu(&self, tree: &Worktree, cx: &mut Context<Self>) -> super::menus::MenuFn {
        let view = cx.entity().downgrade();
        let entry = self.tree_entry(&tree.path);
        let (path, own, main) = (tree.path.clone(), self.is_own_tree(tree), tree.main);
        let (gone, locked) = (tree.prunable.is_some(), tree.locked.is_some());
        let app = self.app.clone();
        Box::new(move |menu, _, cx| {
            let copy = {
                let full = entry.path.clone();
                PopupMenuItem::new(tr("复制路径")).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(full.clone()));
                })
            };
            if gone {
                let view = view.clone();
                return menu
                    .item(
                        PopupMenuItem::new(tr("移除记录…")).on_click(move |_, window, cx| {
                            view.update(cx, |this, cx| this.prune_worktrees_dialog(window, cx))
                                .ok();
                        }),
                    )
                    .separator()
                    .item(copy);
            }
            let go = |label: &'static str, then: Then| {
                let (view, path) = (view.clone(), path.clone());
                PopupMenuItem::new(label).on_click(move |_, window, cx| {
                    view.update(cx, |this, cx| this.enter_worktree(&path, then, window, cx))
                        .ok();
                })
            };
            let menu = match own {
                true => menu,
                false => menu.item(go(tr("进入此工作树"), Then::Stay)),
            };
            let menu = menu
                .item(go(tr("在此新建终端"), Then::NewTerminal))
                .item(go(tr("在文件页中打开"), Then::Files));
            let menu = match own {
                true => menu,
                false => {
                    let (view, entry) = (view.clone(), entry.clone());
                    menu.separator()
                        .item(PopupMenuItem::new(tr("在新标签页中打开")).on_click(
                            move |_, _, cx| {
                                view.update(cx, |_, cx| cx.emit(RepoEvent::Open(entry.clone())))
                                    .ok();
                            },
                        ))
                }
            };
            // 服务器上的工作树：映射到本机了就有本机路径
            let menu = menu.separator().item(copy);
            let menu = crate::mapping::entry_items(menu, &app, &entry, cx);
            if main {
                return menu;
            }
            let lock = {
                let (view, path) = (view.clone(), path.clone());
                let label = if locked {
                    tr("解除锁定")
                } else {
                    tr("锁定")
                };
                PopupMenuItem::new(label).on_click(move |_, window, cx| {
                    view.update(cx, |this, cx| {
                        this.lock_worktree(&path, !locked, window, cx)
                    })
                    .ok();
                })
            };
            let remove = {
                let (view, path) = (view.clone(), path.clone());
                PopupMenuItem::new(tr("移除…")).on_click(move |_, window, cx| {
                    view.update(cx, |this, cx| {
                        this.remove_worktree_dialog(&path, window, cx)
                    })
                    .ok();
                })
            };
            menu.separator().item(lock).item(remove)
        })
    }

    /// 目录已经不在、又没锁定的工作树：清理会移除它们的记录。
    fn stale_worktrees(&self) -> Vec<String> {
        let list = self.worktrees.list.iter();
        list.filter(|tree| tree.prunable.is_some() && tree.locked.is_none())
            .map(|tree| tree.path.clone())
            .collect()
    }

    fn prune_worktrees_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stale = self.stale_worktrees();
        if stale.is_empty() {
            let text = tr("没有可清理的工作树：目录已不存在的均已锁定。");
            window.push_notification(Notification::info(text), cx);
            return;
        }
        let count = stale.len();
        let paths: Vec<String> = stale.iter().map(|path| self.tree_dir(path)).collect();
        let text = tf!(
            "以下工作树的目录已不存在，将移除其记录：\n{paths}",
            paths = paths.join("\n")
        );
        self.confirm_plain(
            tr("清理工作树"),
            text,
            tr("清理"),
            window,
            cx,
            move |this, window, cx| {
                this.run_op(
                    tr("清理工作树"),
                    worktree::prune_args(),
                    None,
                    window,
                    cx,
                    move |_, _, window, cx| {
                        let text = tf!("已清理 {n} 条记录", n = count);
                        window.push_notification(Notification::success(text), cx);
                    },
                );
            },
        );
    }

    fn lock_worktree(
        &mut self,
        path: &str,
        lock: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = if lock {
            tr("锁定工作树")
        } else {
            tr("解除锁定")
        };
        let dir = self.tree_dir(path);
        self.run_op(
            label,
            worktree::lock_args(&dir, lock),
            None,
            window,
            cx,
            move |_, _, window, cx| {
                let text = match lock {
                    true => tr("已锁定：清理时不会移除此工作树"),
                    false => tr("已解除锁定"),
                };
                window.push_notification(Notification::success(text), cx);
            },
        );
    }

    /// 移除一个工作树：目录会被删掉，所以先说清楚；里面有没提交的改动时要勾「仍然移除」。
    fn remove_worktree_dialog(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tree) = self.worktrees.list.iter().find(|tree| tree.path == path) else {
            return;
        };
        if tree.locked.is_some() {
            let text = tr("此工作树已锁定，请先解除锁定。");
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let child = Child::Worktree(path.to_string());
        // 进过它：里面的终端还在跑、文件没保存，先处理完
        if let Some(view) = self.child_view(&child) {
            let view = view.read(cx);
            let problem = if view.terminals_running(cx) > 0 {
                Some(tr("此工作树中还有终端正在运行，请先结束。"))
            } else if view.unsaved_with_subs(cx) > 0 {
                Some(tr("此工作树中有未保存的文件，请先保存或放弃修改。"))
            } else {
                None
            };
            if let Some(problem) = problem {
                window.push_notification(Notification::warning(problem), cx);
                return;
            }
        }
        let changes = self
            .worktrees
            .summaries
            .get(path)
            .map_or(0, |summary| summary.changes);
        let (name, dir) = (tree.name().to_string(), self.tree_dir(path));
        let branch = tree.branch.clone();
        let view = cx.entity().downgrade();
        let force = Rc::new(Cell::new(false));
        window.open_dialog(cx, move |dialog, _, cx| {
            let danger = cx.theme().danger;
            let mut text = tf!("目录 {dir} 将被删除。", dir = &dir);
            if let Some(branch) = &branch {
                text.push_str(&tf!("分支 {branch} 保留。", branch));
            }
            let mut body = v_flex()
                .gap_3()
                .child(div().text_sm().whitespace_normal().child(text));
            if changes > 0 {
                let tick = force.clone();
                body = body.child(
                    v_flex()
                        .gap_2()
                        .p_3()
                        .rounded(px(ui::RADIUS))
                        .bg(danger.opacity(0.1))
                        .child(div().text_sm().whitespace_normal().child(tf!(
                            "其中有 {n} 个未提交的改动，移除后无法恢复。",
                            n = changes
                        )))
                        .child(
                            Checkbox::new("remove-tree-force")
                                .label(tr("仍然移除"))
                                .checked(force.get())
                                .on_click(move |checked, window, _| {
                                    tick.set(*checked);
                                    window.refresh();
                                }),
                        ),
                );
            }
            let run = {
                let (view, child, dir, name) =
                    (view.clone(), child.clone(), dir.clone(), name.clone());
                let force = force.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    window.close_dialog(cx);
                    let (child, dir, name) = (child.clone(), dir.clone(), name.clone());
                    let force = force.get();
                    view.update(cx, |this, cx| {
                        this.remove_worktree(child, dir, name, force, window, cx)
                    })
                    .ok();
                }
            };
            dialog
                .title(tf!("移除工作树 {name}？", name = &name))
                .w(px(520.))
                // 删目录不能撤销：按回车不执行，要明确点那个按钮
                .on_ok(|_, _, _| false)
                .child(body)
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .label(tr("取消"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("ok")
                                .label(tr("移除"))
                                .with_variant(ButtonVariant::Danger)
                                .disabled(changes > 0 && !force.get())
                                .on_click(run),
                        ),
                )
        });
    }

    fn remove_worktree(
        &mut self,
        child: Child,
        dir: String,
        name: String,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_op(
            tr("移除工作树"),
            worktree::remove_args(&dir, force),
            None,
            window,
            cx,
            move |this, _, window, cx| {
                this.drop_child(&child, cx);
                let text = tf!("已移除工作树 {name}", name);
                window.push_notification(Notification::success(text), cx);
            },
        );
    }

    /// 新建工作树：新建一个分支，或者检出一个已有的分支（已经在别的工作树里检出的选不了）。
    /// `branch` 是预先选好的已有分支（从分支的右键菜单来）。
    pub(super) fn new_worktree_dialog(
        &mut self,
        branch: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 能选的已有分支：没在任何工作树里检出的本地分支
        let held = |name: &str| {
            let mut trees = self.worktrees.list.iter();
            trees.any(|tree| tree.branch.as_deref() == Some(name))
        };
        let free: Rc<Vec<String>> = Rc::new(
            self.refs
                .branches
                .iter()
                .map(|branch| branch.name.clone())
                .filter(|name| !held(name))
                .collect(),
        );
        let preset = branch.filter(|name| free.contains(name));
        let existing = Rc::new(Cell::new(preset.is_some()));
        let picked: Rc<RefCell<Option<String>>> =
            Rc::new(RefCell::new(preset.or_else(|| free.first().cloned())));
        let current = self
            .status
            .as_ref()
            .and_then(|status| status.branch.clone());
        let repo = self.entry.path.clone();
        let name = cx
            .new(|cx| InputState::new(window, cx).placeholder(tr("分支名称，例如 feature/login")));
        let base = cx.new(|cx| InputState::new(window, cx).placeholder("HEAD"));
        if let Some(current) = current {
            base.update(cx, |state, cx| state.set_value(current, window, cx));
        }
        let place = cx.new(|cx| InputState::new(window, cx).placeholder(tr("工作树的目录")));
        // 位置跟着分支名变，直到用户自己改过它
        let auto: Rc<RefCell<String>> = Rc::default();
        let sync_place: Act = {
            let (name, place, auto) = (name.clone(), place.clone(), auto.clone());
            let (existing, picked, repo) = (existing.clone(), picked.clone(), repo.clone());
            Rc::new(move |window, cx| {
                if place.read(cx).value().as_ref() != auto.borrow().as_str() {
                    return;
                }
                let branch = match existing.get() {
                    true => picked.borrow().clone().unwrap_or_default(),
                    false => name.read(cx).value().to_string(),
                };
                let wanted = worktree::default_path(&repo, &branch);
                *auto.borrow_mut() = wanted.clone();
                place.update(cx, |state, cx| state.set_value(wanted, window, cx));
                // 位置长，要紧的是结尾的目录名
                ui::show_input_end(&place, window);
            })
        };
        sync_place(window, cx);
        let name_changed = {
            let sync_place = sync_place.clone();
            cx.subscribe_in(
                &name,
                window,
                move |_, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        sync_place(window, cx);
                    }
                },
            )
        };
        let open_after = Rc::new(Cell::new(true));
        let problem: Rc<RefCell<Option<SharedString>>> = Rc::default();
        let view = cx.entity().downgrade();
        let submit: Act = {
            let (name, base, place) = (name.clone(), base.clone(), place.clone());
            let (existing, picked) = (existing.clone(), picked.clone());
            let (open_after, problem, view) = (open_after.clone(), problem.clone(), view.clone());
            Rc::new(move |window, cx| {
                let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
                let (new_name, base, dir) = (read(&name), read(&base), read(&place));
                let branch = match existing.get() {
                    true => picked.borrow().clone(),
                    false => Some(new_name).filter(|name| !name.is_empty()),
                };
                let found = view.upgrade().and_then(|view| {
                    let this = view.read(cx);
                    let Some(branch) = &branch else {
                        return Some(match existing.get() {
                            true => tr("请选择分支").to_string(),
                            false => tr("请填写分支名称").to_string(),
                        });
                    };
                    if !existing.get()
                        && let Some(problem) =
                            ref_name_error(branch).or_else(|| this.branch_exists_error(branch))
                    {
                        return Some(problem);
                    }
                    if dir.is_empty() {
                        return Some(tr("请填写工作树的位置").to_string());
                    }
                    let taken = this.worktree_paths();
                    taken
                        .iter()
                        .any(|path| worktree::same_path(path, &dir))
                        .then(|| tr("此位置已有工作树").to_string())
                });
                if let Some(found) = found {
                    *problem.borrow_mut() = Some(found.into());
                    window.refresh();
                    return;
                }
                let Some(branch) = branch else {
                    return;
                };
                window.close_dialog(cx);
                let (new, open) = (!existing.get(), open_after.get());
                view.update(cx, |this, cx| {
                    this.add_worktree(dir, branch, new.then_some(base), open, window, cx)
                })
                .ok();
            })
        };
        let first_build = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            // 订阅跟着对话框：对话框关了就不再跟着分支名改位置
            let _keep = &name_changed;
            if first_build.replace(false) && !existing.get() {
                name.focus_handle(cx).focus(window, cx);
            }
            let (muted, danger) = (cx.theme().muted_foreground, cx.theme().danger);
            let use_existing = existing.get();
            // 两种做法二选一：没选的那一组禁用。选了新建分支时直接可以打名字
            let pick_mode = |to: bool| {
                let (existing, sync_place, name) =
                    (existing.clone(), sync_place.clone(), name.clone());
                move |_: &bool, window: &mut Window, cx: &mut App| {
                    existing.set(to);
                    sync_place(window, cx);
                    if !to {
                        name.focus_handle(cx).focus(window, cx);
                    }
                    window.refresh();
                }
            };
            let new_group = v_flex()
                .gap_2()
                .child(
                    Radio::new("tree-new-branch")
                        .label(tr("新建分支"))
                        .checked(!use_existing)
                        .on_click(pick_mode(false)),
                )
                .child(
                    h_flex()
                        .pl(px(24.))
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&name).disabled(use_existing)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(muted)
                                .child(tr("基于")),
                        )
                        .child(
                            div()
                                .w(px(150.))
                                .flex_none()
                                .child(Input::new(&base).disabled(use_existing)),
                        ),
                );
            let chosen = picked.borrow().clone();
            let branch_button: AnyElement = match chosen {
                None => div()
                    .text_xs()
                    .text_color(muted)
                    .child(tr("没有可选的分支：本地分支均已在工作树中检出。"))
                    .into_any_element(),
                Some(chosen) => {
                    let (free, picked) = (free.clone(), picked.clone());
                    let (existing, sync_place) = (existing.clone(), sync_place.clone());
                    let now = chosen.clone();
                    Button::new("tree-branch-pick")
                        .outline()
                        .w_full()
                        .disabled(!use_existing)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_left()
                                .child(chosen),
                        )
                        .dropdown_caret(true)
                        .dropdown_menu(move |mut menu, _, _| {
                            menu = menu.min_w(px(360.)).max_h(px(320.)).scrollable(true);
                            for branch in free.iter() {
                                let (picked, existing) = (picked.clone(), existing.clone());
                                let sync_place = sync_place.clone();
                                let to = branch.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(branch.clone())
                                        .checked(*branch == now)
                                        .on_click(move |_, window, cx| {
                                            *picked.borrow_mut() = Some(to.clone());
                                            existing.set(true);
                                            sync_place(window, cx);
                                            window.refresh();
                                        }),
                                );
                            }
                            menu
                        })
                        .into_any_element()
                }
            };
            // 没有能选的分支：这一项选不了，下面只说为什么，不再重复「不可选」的说明
            let none_free = free.is_empty();
            let existing_group = v_flex()
                .gap_2()
                .child(
                    Radio::new("tree-existing-branch")
                        .label(tr("已有分支"))
                        .checked(use_existing)
                        .disabled(none_free)
                        .on_click(pick_mode(true)),
                )
                .child(
                    v_flex()
                        .pl(px(24.))
                        .gap_1()
                        .child(branch_button)
                        .when(!none_free, |group| {
                            group.child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(tr("已在其他工作树中检出的分支不可选。")),
                            )
                        }),
                );
            let tick = open_after.clone();
            let mut body = v_flex()
                .gap_3()
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(tr("同一仓库的另一份工作目录，可同时检出不同的分支。")),
                )
                .child(new_group)
                .child(existing_group)
                .child(
                    v_flex()
                        .gap_1()
                        .child(div().text_sm().child(tr("位置")))
                        .child(Input::new(&place)),
                )
                .child(
                    Checkbox::new("tree-open-after")
                        .label(tr("创建后打开"))
                        .checked(open_after.get())
                        .on_click(move |checked, window, _| {
                            tick.set(*checked);
                            window.refresh();
                        }),
                );
            if let Some(found) = problem.borrow().clone() {
                body = body.child(div().text_sm().text_color(danger).child(found));
            }
            let (on_click, on_enter) = (submit.clone(), submit.clone());
            dialog
                .title(tr("新建工作树"))
                .w(px(540.))
                .on_ok(move |_, window, cx| {
                    on_enter(window, cx);
                    false
                })
                .child(body)
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .label(tr("取消"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("ok")
                                .primary()
                                .label(tr("创建"))
                                .on_click(move |_, window, cx| on_click(window, cx)),
                        ),
                )
        });
    }

    /// 执行 `git worktree add`。`base` 有值是新建分支（从它开始），没有是检出已有的分支。
    fn add_worktree(
        &mut self,
        dir: String,
        branch: String,
        base: Option<String>,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let checkout = match &base {
            Some(base) => Checkout::NewBranch {
                name: &branch,
                base,
            },
            None => Checkout::Branch(&branch),
        };
        let args = worktree::add_args(&dir, checkout);
        self.run_op(
            tr("新建工作树"),
            args,
            None,
            window,
            cx,
            move |this, _, window, cx| {
                let text = tf!("已创建工作树，检出 {branch}", branch = &branch);
                window.push_notification(Notification::success(text), cx);
                if open {
                    this.open_new_worktree(branch, window, cx);
                }
            },
        );
    }

    /// 新建完：重读清单，进到检出了 `branch` 的那个工作树里。位置用清单里的写法，不用对话框里填的
    /// （填的可能是相对路径）。
    fn open_new_worktree(&mut self, branch: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let nul = worktree::has_nul(channel.git_version());
        let read = channel.git(&self.entry.path, worktree::list_args(nul));
        self.worktrees.open_task = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(out) = read.await.and_then(|out| out.ok()) else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.set_worktrees(worktree::parse(&out, nul), cx);
                let made = this
                    .worktrees
                    .list
                    .iter()
                    .find(|tree| tree.branch.as_deref() == Some(branch.as_str()))
                    .map(|tree| tree.path.clone());
                if let Some(path) = made {
                    this.enter_worktree(&path, Then::Stay, window, cx);
                }
            })
            .ok();
        }));
    }

    /// 要检出的分支已经在别的工作树里检出了（git 不让同一个分支检出两处）：问要不要进那个工作树。
    /// 没有这回事返回假，调用的地方照常检出。
    pub(super) fn offer_tree_holding(
        &mut self,
        branch: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tree) = self.tree_holding(branch) else {
            return false;
        };
        let (path, name) = (tree.path.clone(), tree.name().to_string());
        let text = match tree.main {
            true => tf!("分支 {branch} 已在主工作树中检出，无法在此检出。", branch),
            false => tf!(
                "分支 {branch} 已在工作树 {name} 中检出，无法在此检出。",
                branch,
                name
            ),
        };
        // 从工作树总览进来的页面进不了别的工作树（那要先退回去）：只说明
        if !self.has_worktrees_item() {
            window.push_notification(Notification::warning(text), cx);
            return true;
        }
        self.confirm_plain(
            tr("分支已在其他工作树中检出"),
            text,
            tr("进入该工作树"),
            window,
            cx,
            move |this, window, cx| this.enter_worktree(&path, Then::Stay, window, cx),
        );
        true
    }
}
