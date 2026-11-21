//! 文件清单的排法：平铺，或者按目录排成树。本地修改、提交详情、比较三处共用同一套，
//! 树形还是平铺记在设置里（`Settings::file_tree`），三处一起变。
//!
//! 树形里一串只有一个子目录、自己没有文件的目录并成一行（`crates/fr-app/src/repo`），
//! 目录行右边是里面的文件数；同一层里目录在前、文件在后，都不分大小写按名字排。

use std::collections::BTreeMap;

use fr_core::i18n::tr;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::ui;

/// 树形里每深一层往右缩进多少。
pub(super) const INDENT: f32 = 14.;
/// 树形里文件那一行最前面空出来的宽度：和目录那一行的箭头一样宽（见 [`dir_cells`]）。
pub(super) const FILE_LEAD: f32 = 12.;

/// 清单里的一行。
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Row {
    Dir {
        /// 从仓库根到这个目录的完整路径：收起、展开按它记
        path: String,
        /// 显示的字：并成一行的几层目录用 `/` 接着
        label: String,
        depth: usize,
        /// 里面（含子目录）有几个文件
        files: usize,
        open: bool,
    },
    File {
        /// 调用的人给的下标（文件在它那份清单里的位置）
        index: usize,
        depth: usize,
    },
}

#[derive(Default)]
struct Node<'a> {
    dirs: BTreeMap<(String, &'a str), Node<'a>>,
    files: Vec<(&'a str, usize)>,
}

impl Node<'_> {
    fn count(&self) -> usize {
        self.files.len() + self.dirs.values().map(Node::count).sum::<usize>()
    }
}

/// 把文件排成要显示的行。`files` 是（路径，下标）；`tree` 为假时原样平铺。
/// `folded` 说哪些目录（按完整路径）收着，收着的不往下列。
pub(super) fn rows(files: &[(&str, usize)], tree: bool, folded: &dyn Fn(&str) -> bool) -> Vec<Row> {
    if !tree {
        let flat = files.iter();
        return flat
            .map(|(_, index)| Row::File {
                index: *index,
                depth: 0,
            })
            .collect();
    }
    let mut root = Node::default();
    for (path, index) in files {
        let mut node = &mut root;
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_some() {
                node = node.dirs.entry((part.to_lowercase(), part)).or_default();
            } else {
                node.files.push((part, *index));
            }
        }
    }
    fn walk(
        node: &Node,
        prefix: &str,
        depth: usize,
        folded: &dyn Fn(&str) -> bool,
        out: &mut Vec<Row>,
    ) {
        for ((_, name), child) in &node.dirs {
            // 一串只有一个子目录的并成一行
            let (mut label, mut child) = (name.to_string(), child);
            while child.files.is_empty() && child.dirs.len() == 1 {
                let Some(((_, next), only)) = child.dirs.iter().next() else {
                    break;
                };
                label.push('/');
                label.push_str(next);
                child = only;
            }
            let path = match prefix.is_empty() {
                true => label.clone(),
                false => format!("{prefix}/{label}"),
            };
            let open = !folded(&path);
            out.push(Row::Dir {
                path: path.clone(),
                label,
                depth,
                files: child.count(),
                open,
            });
            if open {
                walk(child, &path, depth + 1, folded, out);
            }
        }
        let mut files = node.files.clone();
        files.sort_by_key(|(name, _)| name.to_lowercase());
        for (_, index) in files {
            out.push(Row::File { index, depth });
        }
    }
    let mut out = Vec::new();
    walk(&root, "", 0, folded, &mut out);
    out
}

/// 路径里有没有过滤框里的字（不分大小写；`needle` 要先转成小写）。
pub(super) fn matches(path: &str, needle: &str) -> bool {
    needle.is_empty() || path.to_lowercase().contains(needle)
}

/// 路径拆成目录和文件名：平铺时目录写淡一些。
pub(super) fn split_name(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(at) => (&path[..=at], &path[at + 1..]),
        None => ("", path),
    }
}

/// 改动的种类：一个带底色的小方块，里面一个字母，颜色分新增、修改、删除。
pub(super) fn kind_badge(letter: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .size(px(16.))
        .flex_none()
        .rounded(px(4.))
        .bg(color.opacity(0.16))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(10.5))
        .line_height(px(16.))
        .font_weight(FontWeight::BOLD)
        .text_color(color)
        .child(letter.into())
}

/// 目录那一行里面的东西：箭头、文件夹图标、名字、文件数。外面的行（高度、点击）调用的人自己包。
///
/// 树形里文件那一行最前面也留出箭头那么宽（[`FILE_LEAD`]），改动的小方块对着文件夹图标、名字对着
/// 文件夹的名字：同一层的文件和目录对齐，下一层整个往右缩一格，看得出谁在谁里面。
pub(super) fn dir_cells(
    id: impl Into<ElementId>,
    label: &str,
    path: &str,
    files: usize,
    open: bool,
    cx: &App,
) -> Vec<AnyElement> {
    let muted = cx.theme().muted_foreground;
    vec![
        ui::icon_small(if open {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        })
        .flex_none()
        .text_color(muted)
        .into_any_element(),
        ui::icon(if open {
            IconName::FolderOpen
        } else {
            IconName::Folder
        })
        .flex_none()
        .text_color(ui::folder_color(cx))
        .into_any_element(),
        ui::clipped_with_tip(id, label.to_string(), path.to_string())
            .flex_1()
            .text_ellipsis_start()
            .into_any_element(),
        div()
            .flex_none()
            .text_size(px(11.))
            .text_color(muted)
            .child(files.to_string())
            .into_any_element(),
    ]
}

/// 清单标题行右边的两个按钮：树形、平铺。`pick` 收到的是「要不要树形」。
pub(super) fn mode_toggle(
    id: &'static str,
    tree: bool,
    pick: impl Fn(bool, &mut Window, &mut App) + Clone + 'static,
) -> impl IntoElement {
    let (to_tree, to_flat) = (pick.clone(), pick);
    h_flex()
        .flex_none()
        .gap(px(2.))
        .child(
            Button::new((id, 0usize))
                .xsmall()
                .ghost()
                .icon(ui::icon(IconName::ListTree))
                .selected(tree)
                .tooltip(tr("按目录显示"))
                .on_click(move |_, window, cx| to_tree(true, window, cx)),
        )
        .child(
            Button::new((id, 1usize))
                .xsmall()
                .ghost()
                .icon(ui::icon(IconName::List))
                .selected(!tree)
                .tooltip(tr("平铺显示"))
                .on_click(move |_, window, cx| to_flat(false, window, cx)),
        )
}

#[cfg(test)]
mod tests {
    use super::{Row, matches, rows, split_name};

    /// 把清单画成缩进的文字，好比对。
    fn draw(paths: &[&str], tree: bool, folded: &[&str]) -> Vec<String> {
        let files: Vec<(&str, usize)> = paths.iter().enumerate().map(|(i, p)| (*p, i)).collect();
        rows(&files, tree, &|path| folded.contains(&path))
            .into_iter()
            .map(|row| match row {
                Row::Dir {
                    label,
                    depth,
                    files,
                    open,
                    ..
                } => format!(
                    "{}{} {label}/ ({files})",
                    "  ".repeat(depth),
                    if open { "v" } else { ">" }
                ),
                Row::File { index, depth } => {
                    format!("{}{}", "  ".repeat(depth), split_name(paths[index]).1)
                }
            })
            .collect()
    }

    const PATHS: [&str; 8] = [
        "crates/fr-app/assets/compare-empty.png",
        "crates/fr-app/src/repo/compare.rs",
        "crates/fr-app/src/repo/menus.rs",
        "crates/fr-app/src/repo/mod.rs",
        "crates/fr-core/src/diff.rs",
        "crates/fr-core/src/protocol.rs",
        "docs/架构.md",
        "README.md",
    ];

    #[test]
    fn 按目录排成树_只有一个子目录的并成一行() {
        assert_eq!(
            draw(&PATHS, true, &[]),
            [
                "v crates/ (6)",
                "  v fr-app/ (4)",
                "    v assets/ (1)",
                "      compare-empty.png",
                "    v src/repo/ (3)",
                "      compare.rs",
                "      menus.rs",
                "      mod.rs",
                "  v fr-core/src/ (2)",
                "    diff.rs",
                "    protocol.rs",
                "v docs/ (1)",
                "  架构.md",
                // 同一层里目录在前、文件在后
                "README.md",
            ]
        );
        // 只有一条路的：从最上面一层就并起来
        assert_eq!(draw(&["a/b/c/x.rs"], true, &[]), ["v a/b/c/ (1)", "  x.rs"]);
        assert!(draw(&[], true, &[]).is_empty());
    }

    #[test]
    fn 收起的目录不往下列() {
        assert_eq!(
            draw(&PATHS, true, &["crates/fr-app", "docs"]),
            [
                "v crates/ (6)",
                "  > fr-app/ (4)",
                "  v fr-core/src/ (2)",
                "    diff.rs",
                "    protocol.rs",
                "> docs/ (1)",
                "README.md",
            ]
        );
        // 并成一行的目录按最里面一层的完整路径记
        assert_eq!(
            draw(&PATHS, true, &["crates/fr-core/src"])[8],
            "  > fr-core/src/ (2)"
        );
    }

    #[test]
    fn 平铺时照原来的顺序() {
        let flat = draw(&PATHS, false, &["crates"]);
        assert_eq!(flat.len(), PATHS.len());
        assert_eq!(flat[0], "compare-empty.png");
        assert_eq!(flat[7], "README.md");
    }

    #[test]
    fn 不分大小写_过滤和拆路径() {
        assert_eq!(
            draw(&["b/x", "A/y", "a/z", "Z", "c"], true, &[]),
            [
                "v A/ (1)", "  y", "v a/ (1)", "  z", "v b/ (1)", "  x", "c", "Z"
            ]
        );
        assert!(matches("crates/fr-core/src/Diff.rs", "diff"));
        assert!(matches("x", ""));
        assert!(!matches("src/a.rs", "diff"));
        assert_eq!(split_name("src/repo/mod.rs"), ("src/repo/", "mod.rs"));
        assert_eq!(split_name("README.md"), ("", "README.md"));
    }
}
