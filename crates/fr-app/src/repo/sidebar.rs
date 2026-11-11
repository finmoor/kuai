//! 侧栏：仓库的名字和位置、过滤框，下面是本地修改、所有提交、分支、远程、标签、贮藏、子模块。
//!
//! 分支、远程分支、标签按名字里的 `/` 排成目录树；远程分支先按远程分，每个远程一个文件夹
//! （刚添加、还没获取到分支的远程也有，是空的）。文件夹和分组都可以收起。
//!
//! 单击分支、标签：在提交图里跳到它指向的提交；双击本地分支：切换过去；
//! 双击远程分支：切到同名本地分支，没有就新建一个跟踪它的。
//! 过滤框里输几个字，只留名字里有这几个字的分支、标签、贮藏，文件夹全部展开。

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Range;

use fr_core::i18n::{tr, trc};
use fr_core::refs::Refs;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{Mode, RepoView};
use crate::ui;

pub(super) const WIDTH: f32 = 240.;
const ROW_HEIGHT: f32 = 26.;
/// 目录树每深一层往右缩进多少。
const INDENT: f32 = 14.;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum SideSection {
    Branches,
    Remotes,
    Tags,
    Stashes,
    Submodules,
}

impl SideSection {
    fn title(self) -> &'static str {
        match self {
            // 分组的标题在英文里是复数（Branches），和按钮上的同一个词（Branch）分开翻
            SideSection::Branches => trc("分组", "分支"),
            SideSection::Remotes => trc("分组", "远程"),
            SideSection::Tags => trc("分组", "标签"),
            SideSection::Stashes => trc("分组", "贮藏"),
            SideSection::Submodules => trc("分组", "子模块"),
        }
    }

    fn icon(self) -> IconName {
        match self {
            SideSection::Branches => IconName::GitBranch,
            SideSection::Remotes => IconName::Cloud,
            SideSection::Tags => IconName::Tag,
            SideSection::Stashes => IconName::Archive,
            SideSection::Submodules => IconName::Package,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SideItem {
    Changes,
    History,
    Header(SideSection),
    /// 目录树里的文件夹；数字是它的键（`前缀/路径`）的哈希
    Folder(u64),
    /// 一个远程（远程分支树的第一层）；下标对着 [`RepoView::side_remotes`]
    RemoteRoot(usize),
    Branch(usize),
    Remote(usize),
    Tag(usize),
    Stash(usize),
    Submodule(usize),
}

/// 侧栏里的一行。
#[derive(Clone, Debug, PartialEq)]
struct SideRow {
    item: SideItem,
    /// 缩进几层
    depth: usize,
    /// 显示的字：文件夹和叶子是路径的最后一段
    label: String,
    /// 文件夹、远程的收起键
    key: Option<String>,
    /// 文件夹、分组是不是展开着
    open: bool,
    /// 文件夹、分组里有几项
    count: usize,
}

/// 目录树里的一行（还没对应到侧栏的具体项）。
#[derive(Clone, Debug, PartialEq)]
enum TreeRow {
    Folder {
        /// 从树根到这个文件夹的完整路径
        path: String,
        name: String,
        depth: usize,
        leaves: usize,
        open: bool,
    },
    Leaf {
        /// 调用的人给的下标（分支在 `refs.branches` 里的位置等）
        index: usize,
        name: String,
        depth: usize,
    },
}

#[derive(Default)]
struct TreeNode<'a> {
    folders: BTreeMap<&'a str, TreeNode<'a>>,
    leaves: Vec<(&'a str, usize)>,
}

impl TreeNode<'_> {
    fn count(&self) -> usize {
        self.leaves.len() + self.folders.values().map(TreeNode::count).sum::<usize>()
    }
}

/// 把带 `/` 的名字排成目录树：同一层里文件夹在前、叶子在后，各自按名字排。
/// `collapsed` 说哪些文件夹（按完整路径）收着，收着的不往下列。
fn tree_rows(names: &[(&str, usize)], collapsed: &dyn Fn(&str) -> bool) -> Vec<TreeRow> {
    let mut root = TreeNode::default();
    for (name, index) in names {
        let mut node = &mut root;
        let mut parts = name.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_some() {
                node = node.folders.entry(part).or_default();
            } else {
                node.leaves.push((part, *index));
            }
        }
    }
    fn walk(
        node: &TreeNode,
        prefix: &str,
        depth: usize,
        collapsed: &dyn Fn(&str) -> bool,
        out: &mut Vec<TreeRow>,
    ) {
        for (name, child) in &node.folders {
            let path = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            let open = !collapsed(&path);
            out.push(TreeRow::Folder {
                path: path.clone(),
                name: name.to_string(),
                depth,
                leaves: child.count(),
                open,
            });
            if open {
                walk(child, &path, depth + 1, collapsed, out);
            }
        }
        let mut leaves = node.leaves.clone();
        leaves.sort_by_key(|(name, _)| name.to_lowercase());
        for (name, index) in leaves {
            out.push(TreeRow::Leaf {
                index,
                name: name.to_string(),
                depth,
            });
        }
    }
    let mut out = Vec::new();
    walk(&root, "", 0, collapsed, &mut out);
    out
}

fn key_hash(key: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

pub(super) struct Sidebar {
    /// 收起的分组
    collapsed: HashSet<SideSection>,
    /// 收起的文件夹和远程，按键（`前缀/路径`）记
    folded: HashSet<String>,
    filter: Entity<InputState>,
    scroll: UniformListScrollHandle,
    selected: SideItem,
    /// 侧栏的键盘焦点：上下键换选中的项，回车等于双击
    focus: FocusHandle,
}

impl Sidebar {
    pub(super) fn new(window: &mut Window, cx: &mut Context<RepoView>) -> (Self, Subscription) {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(tr("过滤分支、标签…")));
        let subscription = cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let sidebar = Self {
            // 标签可能有几千个，默认收起
            collapsed: HashSet::from([SideSection::Tags]),
            folded: HashSet::new(),
            filter,
            scroll: UniformListScrollHandle::new(),
            selected: SideItem::History,
            focus: cx.focus_handle(),
        };
        (sidebar, subscription)
    }
}

impl RepoView {
    /// 换了界面语言：过滤框里的提示文字是建的时候定的，要重新设一遍。
    pub(super) fn retranslate_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar.filter.update(cx, |state, cx| {
            state.set_placeholder(tr("过滤分支、标签…"), window, cx)
        });
    }

    /// 侧栏「远程」下面第一层的远程名：配置里的远程，加上远程分支名字里出现的（按名字排）。
    pub(super) fn side_remotes(&self) -> Vec<String> {
        let mut names: Vec<String> = self.remote_names.clone();
        for remote in &self.refs.remotes {
            if let Some((first, _)) = remote.name.split_once('/')
                && !names.iter().any(|n| n == first)
            {
                names.push(first.to_string());
            }
        }
        names.sort();
        names
    }

    fn sidebar_rows(&self, cx: &App) -> Vec<SideRow> {
        let needle = self.sidebar.filter.read(cx).value().trim().to_lowercase();
        let filtering = !needle.is_empty();
        let matches = |name: &str| !filtering || name.to_lowercase().contains(&needle);
        let folded = &self.sidebar.folded;
        let mut rows = vec![
            SideRow {
                item: SideItem::Changes,
                depth: 0,
                label: String::new(),
                key: None,
                open: false,
                count: self.status.as_ref().map_or(0, |s| s.entries.len()),
            },
            SideRow {
                item: SideItem::History,
                depth: 0,
                label: String::new(),
                key: None,
                open: false,
                count: 0,
            },
        ];
        // 一棵目录树 → 侧栏的行。`prefix` 是收起键的前缀，`base` 是这棵树整体缩进几层
        let push_tree = |rows: &mut Vec<SideRow>,
                         names: &[(&str, usize)],
                         prefix: &str,
                         base: usize,
                         leaf: &dyn Fn(usize) -> SideItem| {
            let collapsed = |path: &str| !filtering && folded.contains(&format!("{prefix}/{path}"));
            for row in tree_rows(names, &collapsed) {
                rows.push(match row {
                    TreeRow::Folder {
                        path,
                        name,
                        depth,
                        leaves,
                        open,
                    } => {
                        let key = format!("{prefix}/{path}");
                        SideRow {
                            item: SideItem::Folder(key_hash(&key)),
                            depth: base + depth,
                            label: name,
                            key: Some(key),
                            open,
                            count: leaves,
                        }
                    }
                    TreeRow::Leaf { index, name, depth } => SideRow {
                        item: leaf(index),
                        depth: base + depth,
                        label: name,
                        key: None,
                        open: false,
                        count: 0,
                    },
                });
            }
        };
        let header = |rows: &mut Vec<SideRow>, section: SideSection, count: usize| -> bool {
            let open = filtering || !self.sidebar.collapsed.contains(&section);
            rows.push(SideRow {
                item: SideItem::Header(section),
                depth: 0,
                label: String::new(),
                key: None,
                open,
                count,
            });
            open
        };

        // 分支
        let branches: Vec<(&str, usize)> = self
            .refs
            .branches
            .iter()
            .enumerate()
            .filter(|(_, b)| matches(&b.name))
            .map(|(i, b)| (b.name.as_ref(), i))
            .collect();
        if header(&mut rows, SideSection::Branches, branches.len()) {
            push_tree(&mut rows, &branches, "b", 1, &SideItem::Branch);
        }

        // 远程：第一层是各个远程，下面是它的分支
        let remotes = self.side_remotes();
        let remote_branches: Vec<(&str, usize)> = self
            .refs
            .remotes
            .iter()
            .enumerate()
            .filter(|(_, b)| matches(&b.name))
            .map(|(i, b)| (b.name.as_ref(), i))
            .collect();
        if header(&mut rows, SideSection::Remotes, remote_branches.len()) {
            for (ri, remote) in remotes.iter().enumerate() {
                let under: Vec<(&str, usize)> = remote_branches
                    .iter()
                    .filter_map(|(name, i)| {
                        name.strip_prefix(remote.as_str())
                            .and_then(|rest| rest.strip_prefix('/'))
                            .map(|rest| (rest, *i))
                    })
                    .collect();
                // 过滤时，没有匹配分支的远程不列
                if filtering && under.is_empty() {
                    continue;
                }
                let key = format!("r/{remote}");
                let open = filtering || !folded.contains(&key);
                rows.push(SideRow {
                    item: SideItem::RemoteRoot(ri),
                    depth: 1,
                    label: remote.clone(),
                    key: Some(key.clone()),
                    open,
                    count: under.len(),
                });
                if open {
                    push_tree(&mut rows, &under, &key, 2, &SideItem::Remote);
                }
            }
        }

        // 标签
        let tags: Vec<(&str, usize)> = self
            .refs
            .tags
            .iter()
            .enumerate()
            .filter(|(_, t)| matches(&t.name))
            .map(|(i, t)| (t.name.as_ref(), i))
            .collect();
        if header(&mut rows, SideSection::Tags, tags.len()) {
            push_tree(&mut rows, &tags, "t", 1, &SideItem::Tag);
        }

        // 贮藏、子模块：不分文件夹
        let flat = |rows: &mut Vec<SideRow>,
                    section: SideSection,
                    names: Vec<(String, usize)>,
                    leaf: &dyn Fn(usize) -> SideItem| {
            if header(rows, section, names.len()) {
                rows.extend(names.into_iter().map(|(label, i)| SideRow {
                    item: leaf(i),
                    depth: 1,
                    label,
                    key: None,
                    open: false,
                    count: 0,
                }));
            }
        };
        let stashes = self
            .refs
            .stashes
            .iter()
            .enumerate()
            .map(|(i, s)| (stash_label(&s.message), i))
            .filter(|(label, _)| matches(label))
            .collect();
        flat(&mut rows, SideSection::Stashes, stashes, &SideItem::Stash);
        // 没有子模块的仓库不显示这一组
        if !self.submodules.is_empty() {
            let submodules: Vec<(String, usize)> = self
                .submodules
                .iter()
                .enumerate()
                .filter(|(_, path)| matches(path))
                .map(|(i, path)| (path.clone(), i))
                .collect();
            flat(
                &mut rows,
                SideSection::Submodules,
                submodules,
                &SideItem::Submodule,
            );
        }
        rows
    }

    /// 侧栏顶上：仓库的名字、在哪（和右边的工具栏一样高），下面是过滤框。
    fn render_side_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border, accent) = (theme.muted_foreground, theme.sidebar_border, theme.primary);
        let remote = self.entry.server.is_some();
        let place = self.place(cx);
        v_flex()
            .flex_none()
            .child(
                h_flex()
                    .h(px(super::TOOLBAR_HEIGHT))
                    .px(px(12.))
                    .gap(px(10.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .flex_none()
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
                            ),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(self.entry.name().to_string()),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(muted)
                                    .truncate()
                                    .child(format!("{place} · {}", self.entry.path)),
                            ),
                    ),
            )
            .child(
                div().px(px(10.)).pt(px(8.)).pb(px(4.)).child(
                    Input::new(&self.sidebar.filter)
                        .small()
                        .cleanable(true)
                        .prefix(ui::icon(IconName::Search).text_color(muted)),
                ),
            )
    }

    pub(super) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (bg, border) = (theme.sidebar, theme.sidebar_border);
        let count = self.sidebar_rows(cx).len();
        let actions = self.render_side_actions(cx);
        v_flex()
            .w(px(WIDTH))
            .flex_none()
            .h_full()
            .bg(bg)
            .border_r_1()
            .border_color(border)
            .child(self.render_side_header(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .py(px(4.))
                    .track_focus(&self.sidebar.focus)
                    .on_key_down(cx.listener(Self::sidebar_key))
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
                    ),
            )
            .children(actions)
            .into_any_element()
    }

    /// 侧栏底部的一条：选中的分支或贮藏的全名，和对它的常用操作。
    /// 按钮不放在那一行上：侧栏只有 230 宽，三个按钮会把名字挤得只剩七八个字。
    fn render_side_actions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.sidebar_border);
        let idle = self.op.is_none();
        let (kind, name, buttons): (&str, String, Vec<Button>) = match self.sidebar.selected {
            SideItem::Branch(i) => {
                let branch = self.refs.branches.get(i)?;
                if branch.current {
                    return None;
                }
                let (checkout, delete) = (branch.name.clone(), branch.name.clone());
                (
                    tr("分支"),
                    branch.name.to_string(),
                    vec![
                        Button::new("branch-checkout")
                            .label(tr("切换"))
                            .tooltip(tr("git checkout，双击这一行也可以"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.checkout_branch(&checkout, window, cx)
                            })),
                        Button::new("branch-delete")
                            .label(tr("删除…"))
                            .tooltip(tr("git branch -d：删除这个本地分支"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.delete_branch(delete.clone(), window, cx)
                            })),
                    ],
                )
            }
            SideItem::Stash(i) => {
                let stash = self.refs.stashes.get(i)?;
                let (apply, pop, drop) = (
                    stash.selector.clone(),
                    stash.selector.clone(),
                    (stash.selector.clone(), stash.message.clone()),
                );
                (
                    tr("贮藏"),
                    stash_label(&stash.message),
                    vec![
                        Button::new("stash-apply")
                            .label(tr("应用"))
                            .tooltip(tr("git stash apply：恢复改动，贮藏留着"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.stash_apply(apply.clone(), window, cx)
                            })),
                        Button::new("stash-pop")
                            .label(tr("弹出"))
                            .tooltip(tr("git stash pop：恢复改动并删掉这个贮藏"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.stash_pop(pop.clone(), window, cx)
                            })),
                        Button::new("stash-drop")
                            .label(tr("删除…"))
                            .tooltip(tr("git stash drop：删掉这个贮藏，不恢复改动"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.stash_drop(drop.0.clone(), drop.1.clone(), window, cx)
                            })),
                    ],
                )
            }
            _ => return None,
        };
        Some(
            v_flex()
                .flex_none()
                .px_2()
                .py_2()
                .gap_1()
                .border_t_1()
                .border_color(border)
                .child(
                    h_flex()
                        .gap_1()
                        .items_start()
                        .text_xs()
                        .child(div().flex_none().text_color(muted).child(kind))
                        // 名字长就折行，这里要能看到全名
                        .child(div().flex_1().min_w_0().whitespace_normal().child(name)),
                )
                .child(
                    h_flex().gap_1().children(
                        buttons
                            .into_iter()
                            .map(|button| button.xsmall().disabled(!idle)),
                    ),
                )
                .into_any_element(),
        )
    }

    fn render_sidebar_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let rows = self.sidebar_rows(cx);
        let theme = cx.theme();
        let (fg, muted, active, hover, danger, accent) = (
            theme.sidebar_foreground,
            theme.muted_foreground,
            theme.sidebar_accent,
            theme.list_hover,
            theme.danger,
            theme.primary,
        );
        range
            .filter_map(|ix| {
                let row = rows.get(ix)?.clone();
                let item = row.item;
                let selected = item == self.sidebar.selected;
                let is_header = matches!(item, SideItem::Header(_));
                // 行本身占满宽度（好点），里面一块圆角的底显示选中、悬停
                let inner = h_flex()
                    .h(px(ROW_HEIGHT - 2.))
                    .flex_1()
                    .min_w_0()
                    .mx(px(6.))
                    .pl(px(6. + INDENT * row.depth as f32))
                    .pr(px(8.))
                    .gap(px(6.))
                    .rounded(px(ui::RADIUS))
                    .text_size(px(13.))
                    .text_color(fg)
                    .when(selected, |d| d.bg(active))
                    .when(!selected && !is_header, |d| d.hover(|s| s.bg(hover)));
                let chevron = |open: bool| {
                    ui::icon_small(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .flex_none()
                    .text_color(muted)
                };
                let name = |text: String| div().flex_1().min_w_0().truncate().child(text);
                let count = |n: usize| {
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(n.to_string())
                };
                let inner = match item {
                    SideItem::Changes => inner
                        .child(ui::icon(IconName::FileDiff).flex_none().text_color(muted))
                        .child(name(tr("本地修改").to_string()))
                        .when(row.count > 0, |r| {
                            r.child(ui::count_pill(row.count, true, cx))
                        }),
                    SideItem::History => inner
                        .child(
                            ui::icon(IconName::GitCommitHorizontal)
                                .flex_none()
                                .text_color(muted),
                        )
                        .child(name(tr("所有提交").to_string())),
                    SideItem::Header(section) => inner
                        .child(chevron(row.open))
                        .child(ui::icon(section.icon()).flex_none().text_color(muted))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(muted)
                                .child(section.title()),
                        )
                        .child(count(row.count)),
                    SideItem::Folder(_) => inner
                        .child(chevron(row.open))
                        .child(
                            ui::icon(if row.open {
                                IconName::FolderOpen
                            } else {
                                IconName::Folder
                            })
                            .flex_none()
                            .text_color(muted),
                        )
                        .child(name(row.label))
                        .when(!row.open, |r| r.child(count(row.count))),
                    SideItem::RemoteRoot(_) => inner
                        .child(chevron(row.open))
                        .child(ui::icon(IconName::Cloud).flex_none().text_color(muted))
                        .child(name(row.label))
                        .when(!row.open, |r| r.child(count(row.count))),
                    SideItem::Branch(i) => {
                        let branch = self.refs.branches.get(i)?;
                        let mut detail = String::new();
                        if branch.ahead > 0 {
                            detail.push_str(&format!("↑{}", branch.ahead));
                        }
                        if branch.behind > 0 {
                            if !detail.is_empty() {
                                detail.push(' ');
                            }
                            detail.push_str(&format!("↓{}", branch.behind));
                        }
                        // 当前分支：打勾、加粗，颜色用主色
                        inner
                            .child(if branch.current {
                                ui::icon(IconName::Check).flex_none().text_color(accent)
                            } else {
                                ui::icon(IconName::GitBranch).flex_none().text_color(muted)
                            })
                            .child(
                                name(row.label)
                                    .when(branch.current, |d| d.font_weight(FontWeight::SEMIBOLD)),
                            )
                            .when(branch.gone, |r| {
                                r.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.))
                                        .text_color(danger)
                                        .child(tr("上游已删")),
                                )
                            })
                            .when(!detail.is_empty(), |r| {
                                r.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.))
                                        .text_color(muted)
                                        .child(detail),
                                )
                            })
                    }
                    SideItem::Remote(_) => inner
                        .child(ui::icon(IconName::GitBranch).flex_none().text_color(muted))
                        .child(name(row.label)),
                    SideItem::Tag(_) => inner
                        .child(ui::icon(IconName::Tag).flex_none().text_color(muted))
                        .child(name(row.label)),
                    SideItem::Submodule(_) => inner
                        .child(ui::icon(IconName::Package).flex_none().text_color(muted))
                        .child(name(row.label)),
                    SideItem::Stash(_) => inner
                        .child(ui::icon(IconName::Archive).flex_none().text_color(muted))
                        .child(name(row.label)),
                };
                let outer = h_flex()
                    .id(ix)
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.sidebar.focus.focus(window, cx);
                        this.click_sidebar(item, event.click_count(), window, cx)
                    }))
                    // 点右键先选中这一项，菜单里的操作都是对它做的
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, _, cx| {
                            if matches!(
                                item,
                                SideItem::Branch(_)
                                    | SideItem::Remote(_)
                                    | SideItem::RemoteRoot(_)
                                    | SideItem::Tag(_)
                                    | SideItem::Stash(_)
                                    | SideItem::Submodule(_)
                            ) {
                                this.sidebar.selected = item;
                                cx.notify();
                            }
                        }),
                    )
                    .child(inner);
                let menu = self.sidebar_menu(item, cx);
                Some(match menu {
                    Some(menu) => outer.context_menu(menu).into_any_element(),
                    None => outer.into_any_element(),
                })
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
            // 远程的下标对着远程名的列表，远程少了就不选它了
            SideItem::RemoteRoot(i) => {
                (i < self.side_remotes().len()).then_some(SideItem::RemoteRoot(i))
            }
            other => Some(other),
        };
        self.sidebar.selected = found.unwrap_or(match self.mode {
            Mode::Changes => SideItem::Changes,
            Mode::History | Mode::Blame => SideItem::History,
        });
    }

    /// 切到提交历史，侧栏选中「所有提交」。
    pub(super) fn sidebar_select_history(&mut self) {
        self.sidebar.selected = SideItem::History;
        self.mode = Mode::History;
    }

    /// 侧栏上的按键：上下键换选中的项（跳过分组标题），回车等于双击，
    /// 左右键收起、展开选中的文件夹。
    fn sidebar_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.sidebar.focus.is_focused(window) {
            return;
        }
        let rows = self.sidebar_rows(cx);
        let at = rows.iter().position(|r| r.item == self.sidebar.selected);
        let down = match event.keystroke.key.as_str() {
            "down" => true,
            "up" => false,
            "enter" => {
                cx.stop_propagation();
                self.click_sidebar(self.sidebar.selected, 2, window, cx);
                return;
            }
            key @ ("left" | "right") => {
                // 只对文件夹、远程有用：左收起、右展开
                if let Some(row) = at.and_then(|at| rows.get(at))
                    && let Some(folder) = &row.key
                    && row.open == (key == "left")
                {
                    cx.stop_propagation();
                    self.toggle_folder(folder.clone());
                    cx.notify();
                }
                return;
            }
            _ => return,
        };
        cx.stop_propagation();
        let Some(mut at) = at else {
            return;
        };
        loop {
            at = if down {
                at + 1
            } else {
                match at.checked_sub(1) {
                    Some(at) => at,
                    None => return,
                }
            };
            match rows.get(at) {
                None => return,
                Some(row) if matches!(row.item, SideItem::Header(_)) => continue,
                Some(row) => {
                    self.sidebar
                        .scroll
                        .scroll_to_item(at, ScrollStrategy::Nearest);
                    match row.item {
                        // 文件夹只是选中，不因为路过就收起、展开
                        SideItem::Folder(_) | SideItem::RemoteRoot(_) => {
                            self.sidebar.selected = row.item;
                            cx.notify();
                        }
                        item => self.click_sidebar(item, 1, window, cx),
                    }
                    return;
                }
            }
        }
    }

    fn toggle_folder(&mut self, key: String) {
        if !self.sidebar.folded.remove(&key) {
            self.sidebar.folded.insert(key);
        }
    }

    /// 切到本地修改，并把键盘焦点放到文件列表上（Ctrl+1）。
    pub fn show_changes_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.blame = None;
        self.show_changes(cx);
        self.focus_file_list(window, cx);
        cx.notify();
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
            SideItem::Folder(hash) => {
                // 文件夹：点一下收起或展开
                let key = self
                    .sidebar_rows(cx)
                    .into_iter()
                    .find(|row| row.item == SideItem::Folder(hash))
                    .and_then(|row| row.key);
                if let Some(key) = key {
                    self.sidebar.selected = item;
                    self.toggle_folder(key);
                }
            }
            SideItem::RemoteRoot(i) => {
                if let Some(remote) = self.side_remotes().get(i) {
                    self.sidebar.selected = item;
                    self.toggle_folder(format!("r/{remote}"));
                }
            }
            SideItem::Changes => self.show_changes(cx),
            SideItem::History => {
                self.sidebar.selected = item;
                self.mode = Mode::History;
                // 正在看文件历史时，点「所有提交」回到全部提交
                self.close_file_history(window, cx);
            }
            SideItem::Branch(i) => {
                let Some(branch) = self.refs.branches.get(i).cloned() else {
                    return;
                };
                self.sidebar.selected = item;
                self.mode = Mode::History;
                self.leave_file_views(window, cx);
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
                self.leave_file_views(window, cx);
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
                self.leave_file_views(window, cx);
                self.reveal_commit(&tag.target, cx);
            }
            SideItem::Submodule(i) => {
                self.sidebar.selected = item;
                // 双击：在新标签页里打开这个子模块
                if clicks >= 2 {
                    self.open_submodule(i, cx);
                }
            }
            SideItem::Stash(i) => {
                let Some(stash) = self.refs.stashes.get(i).cloned() else {
                    return;
                };
                // 在提交详情的位置显示贮藏里的改动
                self.sidebar.selected = item;
                self.mode = Mode::History;
                self.leave_file_views(window, cx);
                self.show_object(stash.target, cx);
            }
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
    use super::{TreeRow, stash_label, tree_rows};

    #[test]
    fn 贮藏说明去掉前缀() {
        assert_eq!(stash_label("On main: 试验"), "试验");
        assert_eq!(stash_label("WIP on feature/x: abc123 说明"), "abc123 说明");
        assert_eq!(stash_label("自定义"), "自定义");
    }

    /// 把树画成缩进的文字，好比对。
    fn draw(names: &[&str], collapsed: &[&str]) -> Vec<String> {
        let names: Vec<(&str, usize)> = names.iter().enumerate().map(|(i, n)| (*n, i)).collect();
        tree_rows(&names, &|path| collapsed.contains(&path))
            .into_iter()
            .map(|row| match row {
                TreeRow::Folder {
                    name,
                    depth,
                    leaves,
                    open,
                    ..
                } => format!(
                    "{}{} {name}/ ({leaves})",
                    "  ".repeat(depth),
                    if open { "v" } else { ">" }
                ),
                TreeRow::Leaf { index, name, depth } => {
                    format!("{}{name} #{index}", "  ".repeat(depth))
                }
            })
            .collect()
    }

    #[test]
    fn 分支按斜杠排成目录树() {
        let names = [
            "main",
            "feature/login",
            "feature/ui/dark",
            "fix/crash",
            "Develop",
            "feature/api",
        ];
        assert_eq!(
            draw(&names, &[]),
            [
                "v feature/ (3)",
                "  v ui/ (1)",
                "    dark #2",
                "  api #5",
                "  login #1",
                "v fix/ (1)",
                "  crash #3",
                // 同一层里文件夹在前、叶子在后；叶子不分大小写按名字排
                "Develop #4",
                "main #0",
            ]
        );
    }

    #[test]
    fn 收起的文件夹不往下列() {
        let names = ["feature/login", "feature/ui/dark", "main"];
        assert_eq!(draw(&names, &["feature"]), ["> feature/ (2)", "main #2"]);
        // 收起的是里面一层：外面一层照常展开
        assert_eq!(
            draw(&names, &["feature/ui"]),
            ["v feature/ (2)", "  > ui/ (1)", "  login #0", "main #2"]
        );
        assert!(draw(&[], &[]).is_empty());
    }
}
