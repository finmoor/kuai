//! 工作区页、服务器页上一个仓库和它的子仓库的那几行（两页共用，样子一样）。
//!
//! 仓库那一行：小箭头（有子仓库才有）、图标、名字和路径、收起时有几个子仓库、几个小标签（服务器页上
//! 写它在哪些工作区里）、已打开、终端个数、分支、改动数，最后是鼠标移上去才出来的按钮。子仓库挂在
//! 下面，左边一条竖线：图标、名字和相对目录、已打开、分支、改动数。「已打开」、终端个数、分支、
//! 改动数、悬停的按钮各占固定的宽度（没有的也留着位置，见 [`Columns`]），所以各行对齐成一张表。

use std::rc::Rc;

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu};
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::repo::Summary;
use crate::repo_scan::{Scanned, sub_entry, sub_name};
use crate::settings::RepoEntry;
use crate::term_board::TermState;
use crate::term_overview::TermOverview;
use crate::ui;

/// 分支那一列的宽度
const BRANCH_WIDTH: f32 = 130.;
/// 窗口窄时分支那一列的宽度
const BRANCH_WIDTH_COMPACT: f32 = 100.;
/// 改动数那一列的宽度
const CHANGES_WIDTH: f32 = 72.;
/// 悬停的一个按钮占的宽度
const ACTION_WIDTH: f32 = 26.;
/// 「已打开」那一列的宽度
const OPEN_WIDTH: f32 = 52.;
/// 终端个数那一列的宽度
const TERMS_WIDTH: f32 = 60.;
/// 名字和路径最窄缩到这么宽：再窄就连名字都看不见了，宁可让小标签先让位
const NAME_MIN: f32 = 120.;
/// 窗口窄于这个宽度时各行用窄的排法（和标题栏上三个页面只留图标是同一个宽度）
pub(crate) const COMPACT_BELOW: f32 = 1100.;

/// 一页上各行怎么分列（同一页的行一样，才对得齐）。
#[derive(Clone, Copy)]
pub(crate) struct Columns {
    /// 悬停的按钮留几个的宽度
    pub slots: usize,
    /// 留出终端个数那一列（这一页上有仓库开着终端时）
    pub terms: bool,
    /// 窗口窄：不写「已打开」（开着的图标是主色），分支那一列窄一点，不画小标签
    pub compact: bool,
}

impl Columns {
    /// `blocks` 里有开着终端的就留终端那一列；悬停的按钮按最多的那一行留。
    pub fn for_blocks(blocks: &[(Block, Vec<AnyElement>)], compact: bool) -> Self {
        Self {
            slots: blocks
                .iter()
                .map(|(_, actions)| actions.len())
                .max()
                .unwrap_or(0),
            terms: blocks.iter().any(|(block, _)| block.terms.is_some()),
            compact,
        }
    }
}

/// 点了一行里的什么：在哪个页面上由调用的地方决定做什么。
pub(crate) type OnEntry<T> = Rc<dyn Fn(&mut T, RepoEntry, &mut Window, &mut Context<T>)>;

/// 右键菜单。参数是那一行的仓库（子仓库那一行是子仓库）和它是不是子仓库。
pub(crate) type MenuFor =
    Rc<dyn Fn(PopupMenu, RepoEntry, bool, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// 一个仓库要画的东西。
pub(crate) struct Block {
    pub entry: RepoEntry,
    pub name: SharedString,
    /// 名字后面淡淡写的那一截（工作区页上写服务器上的仓库在哪台）
    pub aside: Option<SharedString>,
    /// 读到的子仓库、分支、改动数
    pub scanned: Option<Scanned>,
    /// 子仓库显示的名字（和 `scanned` 里的子仓库一一对应，起过名字的用起的名字），见 [`sub_names`]
    pub sub_names: Vec<SharedString>,
    /// 还在读
    pub loading: bool,
    /// 那台服务器没连着：读不了
    pub offline: bool,
    /// 子仓库收起了
    pub folded: bool,
    /// 开着的标签页
    pub open: Rc<Vec<RepoEntry>>,
    /// 这个仓库的终端：几个、最要紧的那个的状态（已停下、运行中、空闲）
    pub terms: Option<(usize, TermState)>,
    /// 名字和已打开之间的小标签（服务器页：它在哪些工作区里）
    pub chips: Vec<AnyElement>,
}

/// 一行里能点的几样。
pub(crate) struct Handlers<T> {
    pub open: OnEntry<T>,
    pub fold: OnEntry<T>,
    /// 点终端个数；没有就只显示
    pub terms: Option<OnEntry<T>>,
    pub menu: Option<MenuFor>,
}

impl<T> Clone for Handlers<T> {
    fn clone(&self) -> Self {
        Self {
            open: self.open.clone(),
            fold: self.fold.clone(),
            terms: self.terms.clone(),
            menu: self.menu.clone(),
        }
    }
}

/// 画一个仓库和它下面的子仓库。`key` 让每一行的元素 id 不重；`actions` 是仓库那一行悬停时出来的
/// 按钮；`columns` 见 [`Columns`]。
pub(crate) fn render_block<T: 'static>(
    key: usize,
    block: Block,
    actions: Vec<AnyElement>,
    columns: Columns,
    handlers: &Handlers<T>,
    cx: &mut Context<T>,
) -> AnyElement {
    let slots = columns.slots;
    let theme = cx.theme();
    let (muted, border, accent) = (theme.muted_foreground, theme.border, theme.primary);
    let (secondary, list_hover) = (theme.secondary, theme.list_hover);
    let strong = theme.foreground.opacity(0.8);
    let subs: Vec<String> = block
        .scanned
        .as_ref()
        .and_then(|scanned| scanned.subs.clone())
        .unwrap_or_default();
    let has_subs = !subs.is_empty();
    let unfolded = has_subs && !block.folded;
    let own = block.scanned.as_ref().and_then(Scanned::own).cloned();
    let entry = block.entry.clone();
    let caret = div()
        .id(("block-caret", key))
        .flex_none()
        .size(px(18.))
        .rounded(px(4.))
        .flex()
        .items_center()
        .justify_center()
        .text_color(muted)
        .when(has_subs, |caret| {
            let fold = handlers.fold.clone();
            let entry = entry.clone();
            caret
                .cursor_pointer()
                .hover(move |s| s.bg(secondary))
                .child(ui::icon_small(if unfolded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                }))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    fold(this, entry.clone(), window, cx)
                }))
        });
    let is_open = block.open.contains(&block.entry);
    let glyph = match block.entry.server {
        Some(_) => IconName::Server,
        None => IconName::FolderGit2,
    };
    // 没找全（子仓库太多）：写「至少」，不当成全部
    let truncated = block
        .scanned
        .as_ref()
        .is_some_and(|scanned| scanned.truncated);
    let folded_note = (has_subs && !unfolded).then(|| {
        let n = subs.len();
        div()
            .flex_shrink(2.)
            .min_w_0()
            .truncate()
            .text_xs()
            .text_color(muted)
            .child(match truncated {
                true => tf!("至少 {n} 个子仓库", n),
                false => tf!("{n} 个子仓库", n),
            })
    });
    let terms = block.terms.map(|(count, state)| {
        let chip = h_flex()
            .id(("block-terms", key))
            .flex_none()
            .h(px(22.))
            .px(px(8.))
            .gap(px(5.))
            .rounded_full()
            .border_1()
            .border_color(border)
            .text_xs()
            .text_color(strong)
            .child(TermOverview::dot(state, 7., cx))
            .child(Icon::new(IconName::SquareTerminal).size_3())
            .child(count.to_string());
        match handlers.terms.clone() {
            Some(go) => {
                let entry = entry.clone();
                chip.cursor_pointer()
                    .hover(move |s| s.bg(list_hover))
                    .tooltip(|window, cx| ui::tip(tr("在终端页中查看"), window, cx))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        go(this, entry.clone(), window, cx)
                    }))
                    .into_any_element()
            }
            None => chip.into_any_element(),
        }
    });
    let head = row_shell(("block-row", key), &block.entry, handlers, false, cx)
        .min_h(px(46.))
        .pl(px(6.))
        .child(caret)
        .child(
            ui::icon(glyph)
                .flex_none()
                .text_color(if is_open { accent } else { muted }),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w(px(NAME_MIN))
                .child(
                    h_flex()
                        .min_w_0()
                        .child(
                            ui::clipped(("block-name", key), block.name.clone())
                                .flex_shrink(1.)
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM),
                        )
                        .children(block.aside.clone().map(|aside| {
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(muted)
                                .child(format!(" · {aside}"))
                        })),
                )
                .child(
                    ui::clipped(("block-path", key), block.entry.path.clone())
                        .text_xs()
                        .font_family(crate::fonts::mono())
                        .text_color(muted),
                ),
        )
        .children(folded_note)
        .when(!columns.compact, |row| row.children(block.chips))
        .children(open_cell(is_open, columns, cx))
        .when(columns.terms, |row| {
            row.child(div().flex_none().w(px(TERMS_WIDTH)).children(terms))
        })
        .child(state_cells(
            own.as_ref(),
            block.loading,
            block.offline,
            columns.compact,
            cx,
        ))
        .child(
            h_flex()
                .flex_none()
                .w(px(ACTION_WIDTH * slots as f32 + 4.))
                .justify_end()
                .gap(px(2.))
                .children(actions.into_iter().map(|action| {
                    div()
                        .invisible()
                        .group_hover(group_name(&block.entry, false), |s| s.visible())
                        .child(action)
                })),
        );
    let head = with_menu(head, &block.entry, false, handlers);
    if !unfolded {
        return head;
    }
    let rows: Vec<AnyElement> =
        subs.iter()
            .enumerate()
            .map(|(at, relative)| {
                let sub = sub_entry(&block.entry, relative);
                let state = block
                    .scanned
                    .as_ref()
                    .and_then(|scanned| scanned.states.get(relative))
                    .cloned();
                let is_open = block.open.contains(&sub);
                let id = key * 10_000 + at;
                let row =
                    row_shell(("sub-row", id), &sub, handlers, true, cx)
                        .min_h(px(38.))
                        .pl(px(8.))
                        .child(
                            ui::icon(IconName::FolderTree)
                                .flex_none()
                                .text_color(if is_open { accent } else { muted }),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w(px(NAME_MIN))
                                .child(
                                    ui::clipped(
                                        ("sub-name", id),
                                        block.sub_names.get(at).cloned().unwrap_or_else(|| {
                                            sub_name(relative).to_string().into()
                                        }),
                                    )
                                    .text_sm(),
                                )
                                .child(
                                    ui::clipped(("sub-path", id), relative.clone())
                                        .text_xs()
                                        .font_family(crate::fonts::mono())
                                        .text_color(muted),
                                ),
                        )
                        .children(open_cell(is_open, columns, cx))
                        .when(columns.terms, |row| {
                            row.child(div().flex_none().w(px(TERMS_WIDTH)))
                        })
                        .child(state_cells(
                            state.as_ref(),
                            block.loading,
                            block.offline,
                            columns.compact,
                            cx,
                        ));
                with_menu(
                    row.child(div().flex_none().w(px(ACTION_WIDTH * slots as f32 + 4.))),
                    &sub,
                    true,
                    handlers,
                )
            })
            .collect();
    v_flex()
        .child(head)
        .child(
            v_flex()
                .ml(px(33.))
                .mb(px(4.))
                .pl(px(8.))
                .border_l_1()
                .border_color(border)
                .children(rows),
        )
        .into_any_element()
}

/// 一个仓库下面各个子仓库显示的名字：给子仓库起过名字的（标签页右键「重命名…」）用起的名字，
/// 没起过的是目录名。
pub(crate) fn sub_names(
    app: &AppState,
    root: &RepoEntry,
    scanned: Option<&Scanned>,
) -> Vec<SharedString> {
    scanned
        .and_then(|scanned| scanned.subs.as_ref())
        .map(|subs| {
            subs.iter()
                .map(|relative| app.repo_name(&sub_entry(root, relative)))
                .collect()
        })
        .unwrap_or_default()
}

/// 悬停的按钮跟着哪一行显示。
fn group_name(entry: &RepoEntry, sub: bool) -> SharedString {
    format!(
        "{}:{}:{}",
        if sub { "sub" } else { "repo" },
        entry.server.as_deref().unwrap_or(""),
        entry.path
    )
    .into()
}

/// 一行的外壳：点了打开，右键是菜单，鼠标移上去变底色。
fn row_shell<T: 'static>(
    id: impl Into<ElementId>,
    entry: &RepoEntry,
    handlers: &Handlers<T>,
    sub: bool,
    cx: &mut Context<T>,
) -> Stateful<Div> {
    let hover = cx.theme().list_hover;
    let (open, open_entry) = (handlers.open.clone(), entry.clone());
    h_flex()
        .id(id)
        .group(group_name(entry, sub))
        .pr(px(8.))
        .gap(px(10.))
        .rounded(px(ui::RADIUS))
        .cursor_pointer()
        .hover(|s| s.bg(hover))
        .on_click(
            cx.listener(move |this, _, window, cx| open(this, open_entry.clone(), window, cx)),
        )
}

/// 给一行接上右键菜单（有的话）。
fn with_menu<T>(
    row: Stateful<Div>,
    entry: &RepoEntry,
    sub: bool,
    handlers: &Handlers<T>,
) -> AnyElement {
    match handlers.menu.clone() {
        Some(menu) => {
            let entry = entry.clone();
            row.context_menu(move |popup, window, cx| menu(popup, entry.clone(), sub, window, cx))
                .into_any_element()
        }
        None => row.into_any_element(),
    }
}

/// 「已打开」那一列：开着的写一个小圆点加字（主色），没开的空着占位。窗口窄时没有这一列。
fn open_cell(is_open: bool, columns: Columns, cx: &App) -> Option<Div> {
    if columns.compact {
        return None;
    }
    let accent = cx.theme().primary;
    Some(
        h_flex()
            .flex_none()
            .w(px(OPEN_WIDTH))
            .gap(px(5.))
            .text_xs()
            .text_color(accent)
            .when(is_open, |cell| {
                cell.child(div().flex_none().size(px(6.)).rounded_full().bg(accent))
                    .child(tr("已打开"))
            }),
    )
}

/// 分支和改动数两列。还没读到的空着（在读时写「…」）；那台服务器没连着写「未连接」。
fn state_cells(
    state: Option<&Summary>,
    loading: bool,
    offline: bool,
    compact: bool,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    let (muted, warning) = (theme.muted_foreground, theme.warning);
    let strong = theme.foreground.opacity(0.8);
    let branch: Option<SharedString> = match state {
        Some(state) if state.error.is_some() => Some(tr("无法读取").into()),
        Some(state) => Some(match (&state.branch, &state.head) {
            (Some(branch), _) => branch.clone().into(),
            // 分离的 HEAD：写提交号的前几位
            (None, Some(head)) => head.chars().take(8).collect::<String>().into(),
            (None, None) => tr("尚无提交").into(),
        }),
        None if offline => Some(tr("未连接").into()),
        None if loading => Some("…".into()),
        None => None,
    };
    let changes = state
        .filter(|state| state.error.is_none())
        .map(|state| match state.changes {
            0 => div().text_color(muted).child(tr("无改动")),
            n => div().text_color(warning).child(tf!("{n} 个改动", n)),
        });
    h_flex()
        .flex_none()
        .gap(px(10.))
        .child(
            h_flex()
                .w(px(if compact {
                    BRANCH_WIDTH_COMPACT
                } else {
                    BRANCH_WIDTH
                }))
                .gap(px(4.))
                .text_xs()
                .when_some(branch, |cell, branch| {
                    cell.when(state.is_some(), |cell| {
                        cell.child(
                            Icon::new(IconName::GitBranch)
                                .size_3()
                                .flex_none()
                                .text_color(muted),
                        )
                    })
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(crate::fonts::mono())
                            .text_color(if state.is_some() { strong } else { muted })
                            .child(branch),
                    )
                }),
        )
        .child(
            h_flex()
                .w(px(CHANGES_WIDTH))
                .justify_end()
                .text_xs()
                .children(changes),
        )
}
