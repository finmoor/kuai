//! 提交详情里看那个提交时的全部文件：「改动的文件｜全部文件」切换，全部文件按目录排、
//! 目录先收着；点一个文件在右边看它那时的内容（只读），可以另存到本机。
//!
//! 文件清单是 `git ls-tree -r`，内容是 `git show 提交:路径`，本机和服务器上的仓库一样。

use std::collections::HashSet;

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use super::file_tree::{self, Row as ListRow};
use crate::ui;

/// 全部文件最多列这么多行（目录展开得太多时）：再多一下子画不过来，用过滤框找
const MAX_ROWS: usize = 1500;
/// 预览最多读这么大
const MAX_PREVIEW: usize = 4 * 1024 * 1024;

/// 一个提交时的全部文件。
pub(super) struct CommitTree {
    commit: String,
    paths: Vec<String>,
    loading: bool,
    error: Option<SharedString>,
    /// 展开了的目录
    open: HashSet<String>,
    _task: Option<Task<()>>,
}

/// 右边预览的一个文件。
pub(super) struct Preview {
    commit: String,
    path: String,
    editor: Option<Entity<EditorState>>,
    /// 显示不了时的说明（二进制、太大、读不出来）
    note: Option<SharedString>,
    loading: bool,
    _task: Option<Task<()>>,
}

impl RepoView {
    /// 「改动的文件｜全部文件」切换。
    pub(super) fn set_all_files(&mut self, on: bool, cx: &mut Context<Self>) {
        self.history.all_files = on;
        if !on {
            self.history.preview = None;
        } else {
            self.ensure_commit_tree(cx);
        }
        cx.notify();
    }

    /// 看着的提交换了：全部文件开着的话重读清单，预览收起。
    pub(super) fn commit_tree_follow(&mut self, cx: &mut Context<Self>) {
        self.history.preview = None;
        if self.history.all_files {
            self.ensure_commit_tree(cx);
        }
    }

    /// 读看着的那个提交的全部文件（读过的不再读）。
    fn ensure_commit_tree(&mut self, cx: &mut Context<Self>) {
        let Some(commit) = self.history.details_id() else {
            return;
        };
        if self
            .history
            .tree
            .as_ref()
            .is_some_and(|tree| tree.commit == commit)
        {
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let args = ["ls-tree", "-r", "-z", "--name-only", &commit]
            .map(String::from)
            .to_vec();
        let request = channel.git(&self.entry.path, args);
        let task = cx.spawn(async move |this, cx| {
            let result = request.await.and_then(|out| out.ok());
            this.update(cx, |this, cx| {
                let Some(tree) = &mut this.history.tree else {
                    return;
                };
                tree.loading = false;
                match result {
                    Ok(out) => {
                        tree.paths = out
                            .split(|b| *b == 0)
                            .filter(|name| !name.is_empty())
                            .map(|name| String::from_utf8_lossy(name).into_owned())
                            .collect();
                    }
                    Err(err) => tree.error = Some(format!("{err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        self.history.tree = Some(CommitTree {
            commit,
            paths: Vec::new(),
            loading: true,
            error: None,
            open: HashSet::new(),
            _task: Some(task),
        });
    }

    /// 全部文件的行（目录先收着；过滤时全展开）。
    pub(super) fn render_commit_tree_rows(
        &mut self,
        needle: &str,
        hover: Hsla,
        selected: Hsla,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let muted = cx.theme().muted_foreground;
        let Some(tree) = &self.history.tree else {
            return Vec::new();
        };
        if tree.loading {
            return vec![
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(tr("正在读取文件清单…"))
                    .into_any_element(),
            ];
        }
        if let Some(error) = tree.error.clone() {
            return vec![
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .whitespace_normal()
                    .child(tf!("读取文件清单失败：{err}", err = error.to_string()))
                    .into_any_element(),
            ];
        }
        let files: Vec<(&str, usize)> = tree
            .paths
            .iter()
            .enumerate()
            .filter(|(_, path)| file_tree::matches(path, needle))
            .map(|(ix, path)| (path.as_str(), ix))
            .collect();
        let filtering = !needle.is_empty();
        let open = &tree.open;
        let rows = file_tree::rows(&files, true, &|path| !filtering && !open.contains(path));
        let total = rows.len();
        let opened = self
            .history
            .preview
            .as_ref()
            .map(|preview| preview.path.clone());
        let commit = tree.commit.clone();
        let mut out: Vec<AnyElement> = rows
            .iter()
            .take(MAX_ROWS)
            .enumerate()
            .filter_map(|(row_ix, row)| {
                let indent = |depth: usize| px(4. + depth as f32 * file_tree::INDENT);
                match row {
                    ListRow::Dir {
                        path,
                        label,
                        depth,
                        files,
                        open,
                    } => {
                        let key = path.clone();
                        let cells = file_tree::dir_cells(
                            ("tree-dir-name", row_ix),
                            label,
                            path,
                            *files,
                            *open,
                            cx,
                        );
                        Some(
                            h_flex()
                                .id(("tree-dir", row_ix))
                                .h(px(22.))
                                .pl(indent(*depth))
                                .pr_1()
                                .gap(px(5.))
                                .rounded_sm()
                                .text_xs()
                                .cursor_pointer()
                                .hover(|s| s.bg(hover))
                                .children(cells)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(tree) = &mut this.history.tree
                                        && !tree.open.remove(&key)
                                    {
                                        tree.open.insert(key.clone());
                                    }
                                    cx.notify();
                                }))
                                .into_any_element(),
                        )
                    }
                    ListRow::File { index, depth } => {
                        let path = tree.paths.get(*index)?.clone();
                        let name = file_tree::split_name(&path).1.to_string();
                        let is_open = opened.as_deref() == Some(path.as_str());
                        let at = Some((commit.clone(), false));
                        let menu = self.tree_file_menu(path.clone(), at, cx);
                        let open_path = path.clone();
                        Some(
                            h_flex()
                                .id(("tree-file", row_ix))
                                .h(px(22.))
                                .pl(indent(*depth))
                                .pr_1()
                                .gap(px(7.))
                                .rounded_sm()
                                .text_xs()
                                .cursor_pointer()
                                .when(is_open, |row| row.bg(selected))
                                .when(!is_open, |row| row.hover(|s| s.bg(hover)))
                                .child(div().flex_none().w(px(file_tree::FILE_LEAD)))
                                .child(ui::icon_small(IconName::File).flex_none().text_color(muted))
                                .child(ui::clipped(("tree-file-name", row_ix), name).flex_1())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_tree_file(open_path.clone(), window, cx)
                                }))
                                .context_menu(menu)
                                .into_any_element(),
                        )
                    }
                }
            })
            .collect();
        if total > MAX_ROWS {
            out.push(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(tf!(
                        "还有 {n} 项未列出，请用上面的过滤框查找。",
                        n = total - MAX_ROWS
                    ))
                    .into_any_element(),
            );
        }
        if out.is_empty() {
            out.push(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(tr("没有匹配的文件"))
                    .into_any_element(),
            );
        }
        out
    }

    /// 全部文件里一个文件的右键菜单：看历史、逐行追溯（原有的），加上另存。
    fn tree_file_menu(
        &self,
        path: String,
        at: Option<(String, bool)>,
        cx: &mut Context<Self>,
    ) -> super::menus::MenuFn {
        let base = self.tracked_file_menu(path.clone(), at, cx);
        let view = cx.entity().downgrade();
        Box::new(move |menu, window, cx| {
            let menu = base(menu, window, cx);
            let (view, path) = (view.clone(), path.clone());
            menu.separator().item(
                gpui_kit::component::menu::PopupMenuItem::new(tr("将此版本另存为…")).on_click(
                    move |_, window, cx| {
                        view.update(cx, |this, cx| this.save_tree_file(path.clone(), window, cx))
                            .ok();
                    },
                ),
            )
        })
    }

    /// 在右边打开看着的提交里的一个文件（只读）。
    fn open_tree_file(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(commit) = self.history.tree.as_ref().map(|tree| tree.commit.clone()) else {
            return;
        };
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let request = channel.git(
            &self.entry.path,
            vec!["show".into(), format!("{commit}:{path}")],
        );
        let shown = path.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = request.await.and_then(|out| out.ok());
            this.update_in(cx, |this, window, cx| {
                let Some(preview) = &mut this.history.preview else {
                    return;
                };
                if preview.path != shown {
                    return;
                }
                preview.loading = false;
                match result {
                    Err(err) => {
                        preview.note = Some(tf!("无法读取：{err}", err = format!("{err:#}")).into())
                    }
                    Ok(bytes) if bytes.len() > MAX_PREVIEW => {
                        preview.note = Some(
                            tf!(
                                "文件太大（{size}），请另存到本机后查看。",
                                size = crate::text::human_size(bytes.len() as u64)
                            )
                            .into(),
                        )
                    }
                    Ok(bytes) if bytes[..bytes.len().min(8000)].contains(&0) => {
                        preview.note = Some(tr("二进制文件，请另存到本机后查看。").into())
                    }
                    Ok(bytes) => {
                        let text = crate::text::decode(&bytes).replace("\r\n", "\n");
                        let (lang, _) = super::files::language(&shown);
                        let editor = cx.new(|cx| {
                            EditorState::new(window, cx)
                                .language(lang)
                                .line_number(true)
                                .searchable(true)
                        });
                        editor.update(cx, |state, cx| state.set_value(text, window, cx));
                        preview.editor = Some(editor);
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self.history.preview = Some(Preview {
            commit,
            path,
            editor: None,
            note: None,
            loading: true,
            _task: Some(task),
        });
        cx.notify();
    }

    /// 把看着的提交里的一个文件存到本机（先问存到哪）。
    pub(super) fn save_tree_file(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let commit = self
            .history
            .tree
            .as_ref()
            .map(|tree| tree.commit.clone())
            .or_else(|| self.history.details_id());
        let (Some(commit), Some(channel)) = (commit, self.channel.clone()) else {
            return;
        };
        let name = file_tree::split_name(&path).1.to_string();
        let downloads = super::files_ops::downloads_dir().unwrap_or_else(std::env::temp_dir);
        let chosen = cx.prompt_for_new_path(&downloads, Some(&name));
        let repo = self.entry.path.clone();
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(dest))) = chosen.await else {
                return;
            };
            let result = channel
                .git(&repo, vec!["show".into(), format!("{commit}:{path}")])
                .await
                .and_then(|out| out.ok())
                .and_then(|bytes| std::fs::write(&dest, bytes).map_err(anyhow::Error::from));
            cx.update(|window, cx| {
                let note = match result {
                    Ok(()) => {
                        let shown = dest.to_string_lossy().into_owned();
                        Notification::success(tf!("已保存到 {path}", path = shown.clone())).action(
                            move |_, _, _| {
                                let shown = shown.clone();
                                Button::new("tree-reveal")
                                    .small()
                                    .label(crate::shell::reveal_label())
                                    .on_click(move |_, window, cx| {
                                        let result = crate::shell::reveal_in_explorer(&shown);
                                        crate::shell::report(result, window, cx);
                                    })
                            },
                        )
                    }
                    Err(err) => {
                        Notification::error(tf!("保存失败：{err}", err = format!("{err:#}")))
                            .autohide(false)
                    }
                };
                window.push_notification(note, cx);
            })
            .ok();
        })
        .detach();
    }

    /// 右边的预览：顶上一条写着哪个文件、哪个提交，另存、关闭；下面是内容。没打开文件时没有。
    pub(super) fn render_tree_preview(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let preview = self.history.preview.as_ref()?;
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        let path = preview.path.clone();
        let title: SharedString = format!(
            "{} @ {}",
            preview.path,
            &preview.commit[..preview.commit.len().min(8)]
        )
        .into();
        let body = match (&preview.editor, &preview.note, preview.loading) {
            (_, _, true) => div()
                .p_3()
                .text_sm()
                .text_color(muted)
                .child(tr("正在读取…"))
                .into_any_element(),
            (_, Some(note), _) => div()
                .p_3()
                .text_sm()
                .text_color(muted)
                .whitespace_normal()
                .child(note.clone())
                .into_any_element(),
            (Some(editor), None, false) => Editor::new(editor)
                .readonly(true)
                .h_full()
                .into_any_element(),
            (None, None, false) => div().into_any_element(),
        };
        Some(
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(
                    h_flex()
                        .h(px(28.))
                        .flex_none()
                        .pl_3()
                        .pr_2()
                        .gap_2()
                        .border_b_1()
                        .border_color(border)
                        .text_xs()
                        .child(
                            ui::icon_small(IconName::FileText)
                                .flex_none()
                                .text_color(muted),
                        )
                        .child(ui::clipped("tree-preview-title", title).flex_1())
                        .child(
                            Button::new("tree-preview-save")
                                .xsmall()
                                .ghost()
                                .icon(ui::icon_small(IconName::Download))
                                .label(tr("另存为…"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.save_tree_file(path.clone(), window, cx)
                                })),
                        )
                        .child(
                            Button::new("tree-preview-close")
                                .xsmall()
                                .ghost()
                                .icon(ui::icon_small(IconName::X))
                                .tooltip(tr("关闭预览，回到改动"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.history.preview = None;
                                    cx.notify();
                                })),
                        ),
                )
                .child(div().flex_1().min_h_0().child(body))
                .into_any_element(),
        )
    }
}
