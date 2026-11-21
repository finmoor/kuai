//! 文件页的画法。
//!
//! 侧栏（接在仓库名和三页的切换下面）：过滤框和「更多」，下面是目录树，最下面是正在进行的上传、下载。
//! 右边从上到下：工具栏（路径、状态、保存、改动、历史、终端、更多），打开的文件的标签，
//! 提示条（冲突、文件被删、只读的原因），内容（编辑器、图片，或者一张卡片），状态栏。

use std::ops::Range;

use fr_core::i18n::tr;
use fr_core::status::Kind;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, Input};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::files::{Body, ImageDoc, Opaque, OpaqueDoc, OpenFile, TextDoc};
use super::files_ops::{tree_menu, view_item};
use super::rail::rail_button;
use super::{Page, RepoView, TOOLBAR_HEIGHT, tool_button};
use crate::text::human_size;
use crate::ui;

/// 树里一行的高度
const ROW_HEIGHT: f32 = 26.;
/// 每深一层往右缩这么多
const INDENT: f32 = 12.;
/// 打开的文件那一排标签的高度
const TABS_HEIGHT: f32 = 34.;
/// 状态栏的高度
const STATUS_HEIGHT: f32 = 26.;

/// 文件名前面的图标。
fn file_icon(path: &str) -> IconName {
    use super::files::{Class, classify, extension, language};
    match classify(path) {
        Class::Image(_) => return IconName::FileImage,
        Class::Opaque(_) => {
            return match extension(path).as_str() {
                "pdf" | "doc" | "docx" | "rtf" | "odt" | "wps" => IconName::FileText,
                "xls" | "xlsx" | "xlsm" | "ods" | "et" => IconName::FileSpreadsheet,
                "ppt" | "pptx" | "odp" | "dps" => IconName::Presentation,
                "mp4" | "mov" | "mkv" | "avi" | "webm" | "flv" | "wmv" | "m4v" => {
                    IconName::FilePlay
                }
                "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" | "opus" | "wma" => {
                    IconName::FileHeadphone
                }
                "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "zst" => {
                    IconName::FileArchive
                }
                "ttf" | "otf" | "woff" | "woff2" | "eot" => IconName::FileType,
                "tif" | "tiff" | "psd" | "ai" | "heic" | "avif" | "raw" => IconName::FileImage,
                _ => IconName::Binary,
            };
        }
        Class::Text => {}
    }
    match language(path).0 {
        "markdown" | "text" => IconName::FileText,
        "json" | "toml" | "yaml" => IconName::Braces,
        "bash" | "make" | "cmake" => IconName::FileTerminal,
        _ => IconName::FileCode,
    }
}

/// 改动的种类在树里、标签上怎么标：字母和颜色。
fn mark_of(kind: Kind, cx: &App) -> (&'static str, Hsla) {
    let theme = cx.theme();
    match kind {
        Kind::Untracked => ("U", theme.success),
        Kind::Added => ("A", theme.success),
        Kind::Deleted => ("D", theme.danger),
        Kind::Conflicted => ("!", theme.danger),
        Kind::Renamed => ("R", theme.info),
        _ => ("M", theme.warning),
    }
}

impl RepoView {
    // ---------- 侧栏 ----------

    /// 文件页的侧栏，接在仓库名和三页的切换下面。
    pub(super) fn render_files_side(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger, accent) = (theme.muted_foreground, theme.danger, theme.primary);
        let rows = self.files.rows();
        let count = rows.len();
        let filtering = self.files.filtering();
        let view = cx.entity().downgrade();
        let more = Button::new("files-more")
            .ghost()
            .small()
            .flex_none()
            .icon(ui::icon(IconName::Ellipsis).text_color(muted))
            .dropdown_menu(self.files_more_menu(cx));
        let note = |text: SharedString, color: Hsla| {
            div()
                .px(px(16.))
                .py(px(6.))
                .text_xs()
                .text_color(color)
                .whitespace_normal()
                .child(text)
        };
        let body = if let Some(error) = self.files.error.clone() {
            v_flex()
                .child(note(error, danger))
                .child(
                    div().px(px(16.)).child(
                        Button::new("files-retry")
                            .xsmall()
                            .label(tr("重试"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.files.error = None;
                                cx.notify();
                            })),
                    ),
                )
                .into_any_element()
        } else if !self.files.loaded {
            let text = match self.channel.is_some() {
                true => tr("正在读取目录…"),
                false => tr("正在连接…"),
            };
            note(text.into(), muted).into_any_element()
        } else if count == 0 {
            let text = match filtering {
                true => tr("没有匹配的文件"),
                false => tr("此目录为空"),
            };
            note(text.into(), muted).into_any_element()
        } else {
            uniform_list(
                "files-tree",
                count,
                cx.processor(move |this, range: Range<usize>, _window, cx| {
                    this.render_tree_rows(range, cx)
                }),
            )
            .track_scroll(&self.files.scroll)
            .size_full()
            .into_any_element()
        };
        let root_drop = self.files.drop_target.as_deref() == Some("");
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                h_flex()
                    .px(px(10.))
                    .pt(px(8.))
                    .pb(px(4.))
                    .gap(px(4.))
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.files.filter)
                                .small()
                                .cleanable(true)
                                .prefix(ui::icon(IconName::Search).text_color(muted)),
                        ),
                    )
                    .child(more),
            )
            .child(
                div()
                    .id("files-tree-area")
                    .flex_1()
                    .min_h_0()
                    .py(px(4.))
                    .track_focus(&self.files.focus)
                    .on_key_down(cx.listener(Self::tree_key))
                    // 拖本机的文件进来：停在目录上传到那个目录，停在空白处传到仓库根
                    .when(root_drop, |area| area.bg(accent.opacity(0.08)))
                    .drag_over::<ExternalPaths>(|style, _, _, _| style)
                    .on_drag_move(cx.listener(
                        |this, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                            // 行自己会标出来；在行外面（空白处）就是仓库根
                            let inside = event.bounds.contains(&event.event.position);
                            let target = inside.then(String::new);
                            let on_row = this
                                .files
                                .drop_target
                                .as_deref()
                                .is_some_and(|dir| !dir.is_empty());
                            if inside && on_row {
                                return;
                            }
                            if this.files.drop_target != target {
                                this.files.drop_target = target;
                                cx.notify();
                            }
                        },
                    ))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                        let dir = this.files.drop_target.take().unwrap_or_default();
                        this.upload_paths(dir, paths.paths().to_vec(), window, cx);
                        cx.notify();
                    }))
                    .child(body)
                    .context_menu(tree_menu(view, None)),
            )
            .children(self.render_transfers(cx))
    }

    /// 侧栏「更多」里的几项：新建、上传、显示已忽略的文件、全部折叠、刷新。
    fn files_more_menu(
        &self,
        cx: &mut Context<Self>,
    ) -> impl Fn(
        gpui_kit::component::menu::PopupMenu,
        &mut Window,
        &mut Context<gpui_kit::component::menu::PopupMenu>,
    ) -> gpui_kit::component::menu::PopupMenu
    + 'static {
        let view = cx.entity().downgrade();
        let remote = self.entry.server.is_some();
        move |menu, _, cx| {
            let Some(this) = view.upgrade() else {
                return menu;
            };
            let (dir, show_ignored) = {
                let this = this.read(cx);
                (this.target_dir(), this.files.show_ignored)
            };
            let item = |label, run| view_item(&view, label, run);
            let (new_file, new_folder, upload) = (dir.clone(), dir.clone(), dir.clone());
            let place: SharedString = match dir.is_empty() {
                true => tr("位置：仓库根目录").into(),
                false => tf!("位置：{dir}", dir).into(),
            };
            menu.label(place)
                .item(item(
                    tr("新建文件…"),
                    Box::new(move |this, window, cx| {
                        this.new_file_dialog(new_file.clone(), window, cx)
                    }),
                ))
                .item(item(
                    tr("新建文件夹…"),
                    Box::new(move |this, window, cx| {
                        this.new_folder_dialog(new_folder.clone(), window, cx)
                    }),
                ))
                .item(item(
                    if remote {
                        tr("上传文件…")
                    } else {
                        tr("复制文件到此处…")
                    },
                    Box::new(move |this, window, cx| this.pick_upload(upload.clone(), window, cx)),
                ))
                .separator()
                .item(
                    item(
                        tr("显示已忽略的文件"),
                        Box::new(|this, _, cx| this.toggle_show_ignored(cx)),
                    )
                    .checked(show_ignored),
                )
                .item(item(
                    tr("全部折叠"),
                    Box::new(|this, _, cx| this.collapse_tree(cx)),
                ))
                .item(item(
                    tr("刷新"),
                    Box::new(|this, _, cx| {
                        this.refresh(cx);
                        this.refresh_files(cx);
                    }),
                ))
        }
    }

    fn render_tree_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let rows = self.files.rows();
        let theme = cx.theme();
        let (fg, muted, active, hover, accent) = (
            theme.sidebar_foreground,
            theme.muted_foreground,
            theme.sidebar_accent,
            theme.list_hover,
            theme.primary,
        );
        let view = cx.entity().downgrade();
        range
            .filter_map(|ix| {
                let row = rows.get(ix)?.clone();
                let selected = self.files.selected.as_deref() == Some(row.path.as_str());
                let open_here = self
                    .files
                    .active_tab()
                    .is_some_and(|tab| tab.path == row.path);
                let mark = (!row.dir)
                    .then(|| self.files.mark(&row.path))
                    .flatten()
                    .map(|kind| mark_of(kind, cx));
                let dirty_dir = row.dir && self.files.dir_dirty(&row.path);
                let dropping = row.dir && self.files.drop_target.as_deref() == Some(&row.path);
                let name_color = match mark {
                    Some((_, color)) => color,
                    None => fg,
                };
                let chevron = match (row.dir, row.open) {
                    (true, true) => Some(IconName::ChevronDown),
                    (true, false) => Some(IconName::ChevronRight),
                    (false, _) => None,
                };
                let glyph = match (row.dir, row.open, row.link) {
                    (true, _, true) => IconName::FolderSymlink,
                    (true, true, _) => IconName::FolderOpen,
                    (true, false, _) => IconName::Folder,
                    (false, _, true) => IconName::FileSymlink,
                    (false, _, false) => file_icon(&row.path),
                };
                let tip = row.path.clone();
                let inner = h_flex()
                    // 要有 id：悬停样式只有带 id 的元素才会在鼠标进出时重画，不然颜色要等别的事触发重画才变
                    .id("inner")
                    .h(px(ROW_HEIGHT - 2.))
                    .flex_1()
                    .min_w_0()
                    .mx(px(6.))
                    .pl(px(4. + INDENT * row.depth as f32))
                    .pr(px(8.))
                    .gap(px(5.))
                    .rounded(px(ui::RADIUS))
                    .border_1()
                    .border_color(gpui_kit::transparent_black())
                    .text_size(px(13.))
                    .text_color(name_color)
                    .when(selected, |d| d.bg(active))
                    .when(open_here && !selected, |d| d.bg(active.opacity(0.5)))
                    .when(!selected, |d| d.hover(|s| s.bg(hover)))
                    .when(dropping, |d| {
                        d.border_color(accent).bg(accent.opacity(0.12))
                    })
                    .when(row.ignored, |d| d.opacity(0.5))
                    .child(
                        div()
                            .flex_none()
                            .w(px(12.))
                            .flex()
                            .justify_center()
                            .text_color(muted)
                            .children(chevron.map(ui::icon_small)),
                    )
                    .child(ui::icon(glyph).flex_none().text_color(if row.dir {
                        ui::folder_color(cx)
                    } else {
                        muted
                    }))
                    .child(
                        ui::clipped_with_tip(("tree-name", ix), row.name.clone(), tip)
                            .flex_1()
                            .min_w_0(),
                    )
                    .when_some(row.parent.clone(), |d, parent| {
                        d.child(
                            ui::clipped(("tree-parent", ix), parent)
                                .flex_shrink(2.)
                                .max_w(px(120.))
                                .text_ellipsis_start()
                                .text_size(px(11.))
                                .text_color(muted),
                        )
                    })
                    .when_some(mark, |d, (letter, color)| {
                        d.child(
                            div()
                                .flex_none()
                                .w(px(12.))
                                .text_center()
                                .text_size(px(11.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(color)
                                .child(letter),
                        )
                    })
                    .when(dirty_dir, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .mx(px(3.))
                                .size(px(6.))
                                .rounded_full()
                                .bg(cx.theme().warning.opacity(0.9)),
                        )
                    });
                let (click, target) = (row.clone(), row.clone());
                let drop_dir = match row.dir {
                    true => row.path.clone(),
                    false => super::files::split(&row.path).0.to_string(),
                };
                let (hover_dir, hover_is_dir) = (drop_dir.clone(), row.dir);
                Some(
                    h_flex()
                        .id(("tree-row", ix))
                        .h(px(ROW_HEIGHT))
                        .w_full()
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            this.click_tree_row(&click, event.click_count(), window, cx);
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, _, _, cx| {
                                // 右键先选中这一行：菜单是对它的
                                this.files.selected = Some(target.path.clone());
                                cx.notify();
                            }),
                        )
                        .on_drag_move(cx.listener(
                            move |this, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                                if !event.bounds.contains(&event.event.position) {
                                    return;
                                }
                                // 停在文件上：传到它所在的目录（仓库根的话整棵树亮）
                                let target = Some(hover_dir.clone());
                                if this.files.drop_target != target {
                                    this.files.drop_target = target;
                                    cx.notify();
                                }
                                let _ = hover_is_dir;
                            },
                        ))
                        .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                            cx.stop_propagation();
                            this.files.drop_target = None;
                            this.upload_paths(drop_dir.clone(), paths.paths().to_vec(), window, cx);
                            cx.notify();
                        }))
                        .child(inner)
                        .context_menu(tree_menu(view.clone(), Some(row)))
                        .into_any_element(),
                )
            })
            .collect()
    }

    /// 侧栏最下面：正在进行的上传、下载，每个一条，带进度和取消。
    fn render_transfers(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.files.transfers.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let (muted, border, accent, track) = (
            theme.muted_foreground,
            theme.sidebar_border,
            theme.primary,
            theme.secondary,
        );
        let rows = self.files.transfers.iter().map(|transfer| {
            let id = transfer.id;
            let ratio = match transfer.total {
                0 => 0.,
                total => (transfer.sent as f32 / total as f32).clamp(0., 1.),
            };
            let detail = match transfer.total {
                0 => String::new(),
                total => format!(
                    "{} / {} · {:.0}%",
                    human_size(transfer.sent),
                    human_size(total),
                    ratio * 100.
                ),
            };
            v_flex()
                .gap(px(4.))
                .child(
                    h_flex()
                        .gap(px(6.))
                        .child(
                            ui::clipped(("transfer-label", id as usize), transfer.label.clone())
                                .flex_1()
                                .text_xs(),
                        )
                        .child(
                            Button::new(("transfer-cancel", id as usize))
                                .ghost()
                                .xsmall()
                                .flex_none()
                                .icon(ui::icon_small(IconName::X))
                                .tooltip(tr("取消"))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.cancel_transfer(id, cx)),
                                ),
                        ),
                )
                .child(
                    div().h(px(3.)).w_full().rounded_full().bg(track).child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(accent)
                            .w(relative(ratio.max(0.02))),
                    ),
                )
                .child(div().text_size(px(11.)).text_color(muted).child(detail))
        });
        Some(
            v_flex()
                .flex_none()
                .px(px(12.))
                .py(px(8.))
                .gap(px(8.))
                .border_t_1()
                .border_color(border)
                .children(rows)
                .into_any_element(),
        )
    }

    /// 侧栏收成图标栏时文件页的那几项：展开侧栏看目录树、新建文件、上传。
    pub(super) fn render_files_rail(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let remote = self.entry.server.is_some();
        vec![
            rail_button("rail-tree", IconName::FolderTree, false)
                .tooltip(tr("展开侧栏以查看目录树（Ctrl+Shift+B）"))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_sidebar(window, cx)))
                .into_any_element(),
            rail_button("rail-new-file", IconName::FilePlus, false)
                .tooltip(tr("新建文件…"))
                .on_click(cx.listener(|this, _, window, cx| {
                    let dir = this.target_dir();
                    this.new_file_dialog(dir, window, cx);
                }))
                .into_any_element(),
            rail_button("rail-upload", IconName::Upload, false)
                .tooltip(if remote {
                    tr("上传文件…")
                } else {
                    tr("复制文件到此处…")
                })
                .on_click(cx.listener(|this, _, window, cx| {
                    let dir = this.target_dir();
                    this.pick_upload(dir, window, cx);
                }))
                .into_any_element(),
        ]
    }

    // ---------- 右边 ----------

    /// 文件页的右边。
    pub(super) fn render_files_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let bg = cx.theme().background;
        let active = self.files.active;
        let body = match self.files.tabs.get(active) {
            None => self.render_files_empty(cx),
            Some(tab) => match &tab.body {
                Body::Loading => self.render_file_note(tr("正在读取文件…").into(), false, cx),
                Body::Failed(error) => self.render_file_note(error.clone(), true, cx),
                Body::Text(doc) => self.render_editor(doc),
                Body::Image(doc) => self.render_image(tab, doc, cx),
                Body::Opaque(doc) => self.render_opaque(tab, doc, cx),
            },
        };
        let banner = self
            .files
            .tabs
            .get(active)
            .and_then(|tab| self.render_file_banner(tab, cx));
        let tabs = (!self.files.tabs.is_empty()).then(|| self.render_file_tabs(cx));
        let status = self
            .files
            .tabs
            .get(active)
            .map(|tab| self.render_file_status(tab, cx));
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(bg)
            .child(self.render_files_toolbar(cx))
            .children(tabs)
            .children(banner)
            .child(div().flex_1().min_h_0().child(body))
            .children(status)
            .into_any_element()
    }

    /// 工具栏：左边是路径（点一段在树里显示出来）和状态，右边是保存、改动、历史、终端、更多。
    fn render_files_toolbar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, fg, bg, hover, accent, warning) = (
            theme.border,
            theme.muted_foreground,
            theme.foreground,
            theme.background,
            theme.list_hover,
            theme.primary,
            theme.warning,
        );
        let mono = theme.mono_font_family.clone();
        let tab = self.files.active_tab();
        let path = tab.map(|tab| tab.path.clone());
        let id = tab.map(|tab| tab.id);
        let text = tab.and_then(OpenFile::text);
        let (dirty, saving, readonly) = (
            text.is_some_and(|doc| doc.dirty),
            text.is_some_and(|doc| doc.saving),
            text.is_some_and(|doc| doc.readonly.is_some()),
        );
        let deleted = tab.is_some_and(|tab| tab.deleted);
        let mark = path.as_deref().and_then(|path| self.files.mark(path));
        let can_save = text.is_some() && !readonly && !saving && (dirty || deleted);
        // 路径：一段一段，最后一段（文件名）加粗
        let mut crumbs = h_flex()
            .id("file-crumbs")
            .flex_shrink(1.)
            .min_w_0()
            .overflow_hidden()
            .gap(px(1.))
            .font_family(mono)
            .text_size(px(13.))
            .text_color(muted);
        match &path {
            None => {
                crumbs = crumbs.child(
                    div()
                        .font_family(cx.theme().font_family.clone())
                        .child(tr("未打开文件")),
                );
            }
            Some(path) => {
                let parts: Vec<&str> = path.split('/').collect();
                let mut so_far = String::new();
                for (ix, part) in parts.iter().enumerate() {
                    so_far = super::files::join(&so_far, part);
                    let last = ix + 1 == parts.len();
                    let target = so_far.clone();
                    if ix > 0 {
                        crumbs = crumbs.child(
                            ui::icon_small(IconName::ChevronRight)
                                .flex_none()
                                .text_color(muted.opacity(0.6)),
                        );
                    }
                    crumbs = crumbs.child(
                        div()
                            .id(("crumb", ix))
                            .px(px(4.))
                            .py(px(2.))
                            .rounded(px(4.))
                            .whitespace_nowrap()
                            .when(last, |d| {
                                d.text_color(fg)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .flex_shrink(1.)
                                    .min_w_0()
                                    .truncate()
                            })
                            .when(!last, |d| d.flex_shrink(4.).min_w(px(18.)).truncate())
                            .cursor_pointer()
                            .hover(|s| s.bg(hover).text_color(fg))
                            .child(ui::one_line(part.to_string()))
                            .tooltip(|window, cx| ui::tip(tr("在目录树中显示"), window, cx))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.reveal_in_tree(&target, cx)),
                            ),
                    );
                }
            }
        }
        let pill = |text: &'static str, color: Hsla| {
            div()
                .flex_none()
                .h(px(20.))
                .px(px(8.))
                .rounded_full()
                .flex()
                .items_center()
                .bg(color.opacity(0.15))
                .text_size(px(11.))
                .text_color(color)
                .child(text)
        };
        let separator = || div().flex_none().w(px(1.)).h(px(24.)).mx(px(6.)).bg(border);
        let on_file = path.is_some();
        let tracked_or_changed = on_file;
        let changed = mark.is_some();
        let (changes_path, history_path, term_path) = (path.clone(), path.clone(), path.clone());
        h_flex()
            .h(px(TOOLBAR_HEIGHT))
            .px(px(10.))
            .gap(px(2.))
            .flex_none()
            .bg(bg)
            .border_b_1()
            .border_color(border)
            .child(crumbs)
            .child(
                h_flex()
                    .flex_none()
                    .ml(px(8.))
                    .gap(px(6.))
                    .when(dirty, |d| d.child(pill(tr("未保存"), accent)))
                    .when(deleted, |d| d.child(pill(tr("已删除"), cx.theme().danger)))
                    .when(readonly, |d| d.child(pill(tr("只读"), muted)))
                    .when_some(mark.filter(|_| !deleted), |d, kind| {
                        let (text, color) = match kind {
                            Kind::Untracked => (tr("未跟踪"), cx.theme().success),
                            Kind::Added => (tr("新增"), cx.theme().success),
                            Kind::Conflicted => (tr("有冲突"), cx.theme().danger),
                            _ => (tr("已修改"), warning),
                        };
                        d.child(pill(text, color))
                    }),
            )
            .child(div().flex_1().min_w(px(4.)))
            .child(
                tool_button(
                    "file-save",
                    IconName::Save,
                    tr("保存"),
                    tr("保存（Ctrl+S）"),
                    saving,
                    can_save,
                    0,
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| this.save_active_file(window, cx))),
            )
            .child(separator())
            .child(
                tool_button(
                    "file-changes",
                    IconName::FileDiff,
                    tr("改动"),
                    if changed {
                        tr("在「本地修改」中查看此文件的改动")
                    } else {
                        tr("此文件没有未提交的改动")
                    },
                    false,
                    changed,
                    0,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(path) = changes_path.clone() {
                        this.show_file_changes(path, window, cx);
                    }
                })),
            )
            .child(
                tool_button(
                    "file-history",
                    IconName::FileClock,
                    tr("历史"),
                    tr("查看改过此文件的提交"),
                    false,
                    tracked_or_changed,
                    0,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(path) = history_path.clone() {
                        this.show_page(Page::Git, window, cx);
                        this.show_file_history(path, window, cx);
                    }
                })),
            )
            .child(
                tool_button(
                    "file-terminal",
                    IconName::SquareTerminal,
                    tr("终端"),
                    tr("在此文件所在目录新建终端"),
                    false,
                    true,
                    0,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    let dir = term_path
                        .as_deref()
                        .map(|path| super::files::split(path).0.to_string())
                        .unwrap_or_default();
                    this.new_terminal_in(dir, window, cx);
                })),
            )
            .child(self.render_file_more(id, path, cx))
            .into_any_element()
    }

    /// 工具栏最右边的「更多」。
    fn render_file_more(
        &self,
        id: Option<u64>,
        path: Option<String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let remote = self.entry.server.is_some();
        let server = self
            .entry
            .server
            .as_deref()
            .and_then(|id| self.app.read(cx).server(id))
            .map(|server| server.destination.clone());
        let (app, server_id, root) = (
            self.app.clone(),
            self.entry.server.clone(),
            self.entry.path.clone(),
        );
        super::tool_button_bare("file-more", IconName::Ellipsis, tr("更多"), false, 0, cx)
            .disabled(path.is_none())
            .dropdown_menu(move |menu, _, cx| {
                let (Some(id), Some(path)) = (id, path.clone()) else {
                    return menu;
                };
                let full_remote = format!("{}/{path}", root.trim_end_matches('/'));
                let item = |label, run| view_item(&view, label, run);
                let (blame, full, relative, download, reveal, vscode, rename, delete) = (
                    path.clone(),
                    path.clone(),
                    path.clone(),
                    path.clone(),
                    path.clone(),
                    path.clone(),
                    path.clone(),
                    path.clone(),
                );
                let server = server.clone();
                let mut menu = menu
                    .item(item(
                        tr("逐行追溯（blame）"),
                        Box::new(move |this, window, cx| {
                            this.show_page(Page::Git, window, cx);
                            this.show_blame(blame.clone(), cx);
                        }),
                    ))
                    .separator()
                    .item(item(
                        tr("复制路径"),
                        Box::new(move |this, _, cx| this.copy_path(&full, false, cx)),
                    ))
                    .item(item(
                        tr("复制相对路径"),
                        Box::new(move |this, _, cx| this.copy_path(&relative, true, cx)),
                    ))
                    .separator();
                menu = match remote {
                    true => {
                        let menu = match &server_id {
                            Some(server) => crate::mapping::menu_items(
                                menu,
                                &app,
                                server,
                                &full_remote,
                                false,
                                &root,
                                cx,
                            ),
                            None => menu,
                        };
                        menu.item(item(
                            tr("下载…"),
                            Box::new(move |this, window, cx| {
                                this.download_file(download.clone(), window, cx)
                            }),
                        ))
                    }
                    false => menu
                        .item(item(
                            tr("用默认程序打开"),
                            Box::new(move |this, window, cx| this.open_externally(id, window, cx)),
                        ))
                        .item(item(
                            tr("在资源管理器中显示"),
                            Box::new(move |this, window, cx| {
                                let path = this.full_path(&reveal);
                                let result = crate::shell::reveal_in_explorer(&path);
                                crate::shell::report(result, window, cx);
                            }),
                        )),
                };
                // 经别的服务器连的（A>B）：本机连不到它，VS Code 打不开
                let relayed = remote && server.is_none();
                menu.when(!relayed, |menu| {
                    menu.item(item(
                        tr("在 VS Code 中打开"),
                        Box::new(move |this, window, cx| {
                            let path = this.full_path(&vscode);
                            let result = match &server {
                                Some(destination) => {
                                    crate::shell::open_vscode_remote(destination, &path)
                                }
                                None => crate::shell::open_vscode(&path),
                            };
                            crate::shell::report(result, window, cx);
                        }),
                    ))
                })
                .separator()
                .item(item(
                    tr("重命名…"),
                    Box::new(move |this, window, cx| {
                        this.rename_dialog(rename.clone(), window, cx)
                    }),
                ))
                .item(item(
                    tr("删除…"),
                    Box::new(move |this, window, cx| {
                        this.delete_dialog(delete.clone(), window, cx)
                    }),
                ))
            })
    }

    /// 打开的文件的标签。预览的名字是斜体；有没保存的修改时，关闭的 × 变成一个点。
    fn render_file_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, fg, bg, panel, hover) = (
            theme.border,
            theme.muted_foreground,
            theme.foreground,
            theme.background,
            theme.sidebar,
            theme.secondary,
        );
        let active = self.files.active;
        let tabs = self.files.tabs.iter().enumerate().map(|(ix, tab)| {
            let on = ix == active;
            let id = tab.id;
            let dirty = tab.dirty();
            let color = match self.files.mark(&tab.path) {
                Some(kind) if !tab.deleted => mark_of(kind, cx).1,
                _ if on => fg,
                _ => muted,
            };
            let tip = tab.path.clone();
            let close = div()
                .id(("file-tab-close", ix))
                .flex_none()
                .size(px(18.))
                .rounded(px(4.))
                .flex()
                .items_center()
                .justify_center()
                .text_color(muted)
                .hover(|s| s.bg(hover).text_color(fg))
                .map(|close| match dirty {
                    // 没保存：一个点，鼠标停上去才是 ×
                    true => close
                        .child(
                            div()
                                .size(px(8.))
                                .rounded_full()
                                .bg(fg.opacity(0.7))
                                .group_hover("file-tab", |s| s.invisible().absolute()),
                        )
                        .child(
                            div()
                                .invisible()
                                .absolute()
                                .group_hover("file-tab", |s| s.visible().relative())
                                .child(ui::icon_small(IconName::X)),
                        ),
                    false => close.child(ui::icon_small(IconName::X)).when(!on, |close| {
                        close.invisible().group_hover("file-tab", |s| s.visible())
                    }),
                })
                .tooltip(|window, cx| ui::tip(tr("关闭（Ctrl+W）"), window, cx))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.close_file_tab(id, window, cx);
                }));
            h_flex()
                .id(("file-tab", ix))
                .group("file-tab")
                .flex_none()
                .h(px(TABS_HEIGHT - 4.))
                .max_w(px(220.))
                .pl(px(10.))
                .pr(px(6.))
                .gap(px(6.))
                .rounded_t(px(6.))
                .text_size(px(13.))
                .text_color(color)
                .cursor_pointer()
                .when(on, |tab| {
                    tab.bg(bg).border_1().border_b_0().border_color(border)
                })
                .when(!on, |tab| tab.hover(|s| s.text_color(fg)))
                .child(ui::icon(file_icon(&tab.path)).flex_none().text_color(muted))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .when(tab.preview, |d| d.italic())
                        .when(tab.deleted, |d| d.line_through())
                        .child(ui::one_line(tab.name().to_string())),
                )
                .child(close)
                .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    // 双击预览的标签：留下
                    if event.click_count() >= 2
                        && let Some(tab) = this.files.tabs.get_mut(ix)
                    {
                        tab.preview = false;
                    }
                    this.select_file_tab(ix, window, cx);
                }))
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _, window, cx| this.close_file_tab(id, window, cx)),
                )
        });
        h_flex()
            .id("file-tabs")
            .flex_none()
            .h(px(TABS_HEIGHT))
            .px(px(6.))
            .items_end()
            .bg(panel)
            .border_b_1()
            .border_color(border)
            .overflow_x_scroll()
            .children(tabs)
            .into_any_element()
    }

    /// 编辑区上面的提示条：冲突、文件被删、只读的原因。
    fn render_file_banner(&self, tab: &OpenFile, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let (warning, border, muted) = (theme.warning, theme.border, theme.muted_foreground);
        let id = tab.id;
        let doc = tab.text();
        let bar = |color: Hsla, icon: IconName, text: SharedString| {
            h_flex()
                .flex_none()
                .min_h(px(36.))
                .px(px(12.))
                .py(px(6.))
                .gap(px(10.))
                .border_b_1()
                .border_color(border)
                .bg(color.opacity(0.12))
                .text_sm()
                .child(ui::icon(icon).flex_none().text_color(color))
                .child(div().flex_1().min_w_0().whitespace_normal().child(text))
        };
        if doc.is_some_and(|doc| doc.conflict) {
            let text = match tab.deleted {
                true => tr("此文件已在磁盘上被删除，但有未保存的修改。"),
                false => tr("此文件已在磁盘上被其他程序修改，与未保存的修改冲突。"),
            };
            return Some(
                bar(warning, IconName::TriangleAlert, text.into())
                    .child(
                        Button::new("conflict-reload")
                            .small()
                            .label(tr("使用磁盘上的版本"))
                            .disabled(tab.deleted)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.reload_file(id, window, cx)
                            })),
                    )
                    .child(
                        Button::new("conflict-keep")
                            .small()
                            .label(tr("保留我的修改"))
                            .tooltip(tr("关闭此提示；保存时将覆盖磁盘上的文件"))
                            .on_click(cx.listener(move |this, _, _, cx| this.keep_mine(id, cx))),
                    )
                    .child(
                        Button::new("conflict-overwrite")
                            .small()
                            .primary()
                            .label(tr("覆盖保存"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.save_file(id, true, window, cx)
                            })),
                    )
                    .into_any_element(),
            );
        }
        if tab.deleted {
            let text = match doc.is_some() {
                true => tr("此文件已在磁盘上被删除。保存将重新创建它。"),
                false => tr("此文件已在磁盘上被删除。"),
            };
            return Some(
                bar(warning, IconName::TriangleAlert, text.into())
                    .child(
                        Button::new("deleted-close")
                            .small()
                            .label(tr("关闭"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.drop_file_tab(id, window, cx)
                            })),
                    )
                    .into_any_element(),
            );
        }
        let reason = doc.and_then(|doc| doc.readonly)?;
        Some(bar(muted, IconName::Lock, reason.into()).into_any_element())
    }

    fn render_editor(&self, doc: &TextDoc) -> AnyElement {
        Editor::new(&doc.editor)
            .bordered(false)
            .readonly(doc.readonly.is_some())
            .size_full()
            .into_any_element()
    }

    /// 图片：居中显示，垫一层棋盘格（透明的地方看得出来），下面写尺寸和大小。
    fn render_image(&self, tab: &OpenFile, doc: &ImageDoc, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, fg) = (theme.muted_foreground, theme.border, theme.foreground);
        let info = match doc.size {
            Some((width, height)) => {
                format!("{width} × {height} · {}", human_size(tab.stat.size))
            }
            None => human_size(tab.stat.size),
        };
        let checker = fg.opacity(0.06);
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(px(12.))
            .p(px(24.))
            .child(
                div()
                    .flex_shrink(1.)
                    .min_h_0()
                    .max_w_full()
                    .p(px(16.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(border)
                    .bg(checker)
                    .overflow_hidden()
                    .child(
                        img(doc.image.clone())
                            .max_w_full()
                            .max_h_full()
                            .object_fit(ObjectFit::ScaleDown),
                    ),
            )
            .child(div().flex_none().text_xs().text_color(muted).child(info))
            .into_any_element()
    }

    /// 不在这里显示的文件：一张卡片，说清楚是什么、多大，给出能做的事。
    fn render_opaque(&self, tab: &OpenFile, doc: &OpaqueDoc, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, accent, card, warning, track) = (
            theme.muted_foreground,
            theme.border,
            theme.primary,
            theme.sidebar,
            theme.warning,
            theme.secondary,
        );
        let id = tab.id;
        let remote = self.entry.server.is_some();
        let path = tab.path.clone();
        let (kind, why): (SharedString, SharedString) = match doc.reason {
            Opaque::Kind(kind) => (
                kind.into(),
                tr("Kuai 不显示此类文件，可用默认程序打开。").into(),
            ),
            Opaque::Binary => (
                tr("二进制文件").into(),
                tr("此文件的内容不是文本，无法在编辑器中显示。").into(),
            ),
            Opaque::Large => (
                tr("大文件").into(),
                tf!(
                    "超过 {limit} 的文件不在编辑器中打开。",
                    limit = human_size(match super::files::classify(&tab.path) {
                        super::files::Class::Image(_) => super::files::IMAGE_LIMIT,
                        _ => super::files::TEXT_LIMIT,
                    })
                )
                .into(),
            ),
        };
        let busy = doc.busy.clone();
        let changed = doc.local.as_ref().is_some_and(|copy| copy.changed);
        let local = doc
            .local
            .as_ref()
            .map(|copy| copy.path.to_string_lossy().into_owned());
        let actions =
            h_flex()
                .gap(px(8.))
                .child(
                    Button::new("opaque-open")
                        .primary()
                        .small()
                        .label(tr("用默认程序打开"))
                        .disabled(busy.is_some())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_externally(id, window, cx)
                        })),
                )
                .map(|row| match remote {
                    true => row.child(
                        Button::new("opaque-download")
                            .small()
                            .label(tr("下载…"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.download_file(path.clone(), window, cx)
                            })),
                    ),
                    false => row.child(
                        Button::new("opaque-reveal")
                            .small()
                            .label(tr("在资源管理器中显示"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let full = this.full_path(&path);
                                let result = crate::shell::reveal_in_explorer(&full);
                                crate::shell::report(result, window, cx);
                            })),
                    ),
                });
        let progress = busy.map(|(label, sent, total)| {
            let ratio = match total {
                0 => 0.,
                total => (sent as f32 / total as f32).clamp(0., 1.),
            };
            v_flex()
                .w_full()
                .gap(px(6.))
                .child(
                    h_flex()
                        .gap(px(8.))
                        .text_xs()
                        .text_color(muted)
                        .child(Spinner::new().with_size(px(12.)))
                        .child(div().flex_1().child(match total {
                            0 => label.to_string(),
                            total => format!(
                                "{label} {} / {} · {:.0}%",
                                human_size(sent),
                                human_size(total),
                                ratio * 100.
                            ),
                        }))
                        .child(
                            Button::new("opaque-cancel")
                                .xsmall()
                                .label(tr("取消"))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.cancel_fetch(id, cx)),
                                ),
                        ),
                )
                .child(
                    div().h(px(3.)).w_full().rounded_full().bg(track).child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(accent)
                            .w(relative(ratio.max(0.02))),
                    ),
                )
        });
        let upload_back =
            changed.then(|| {
                h_flex()
                    .w_full()
                    .px(px(10.))
                    .py(px(8.))
                    .gap(px(8.))
                    .rounded(px(ui::RADIUS))
                    .bg(warning.opacity(0.12))
                    .text_xs()
                    .child(
                        ui::icon(IconName::TriangleAlert)
                            .flex_none()
                            .text_color(warning),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal()
                            .child(tr("本机副本已修改，远程主机上的文件尚未更新。")),
                    )
                    .child(
                        Button::new("opaque-upload-back")
                            .xsmall()
                            .primary()
                            .label(tr("上传并覆盖"))
                            .disabled(doc.busy.is_some())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.upload_back(id, window, cx)
                            })),
                    )
            });
        let copy_note = local.filter(|_| remote).map(|local| {
            ui::clipped_with_tip(
                "opaque-local",
                tf!("本机副本：{local}", local = local.clone()),
                local,
            )
            .w_full()
            .text_ellipsis_start()
            .text_size(px(11.))
            .text_color(muted)
        });
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .p(px(24.))
            .child(
                v_flex()
                    .w(px(420.))
                    .max_w_full()
                    .p(px(20.))
                    .gap(px(14.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(border)
                    .bg(card)
                    .child(
                        h_flex()
                            .gap(px(12.))
                            .child(
                                div()
                                    .flex_none()
                                    .size(px(40.))
                                    .rounded(px(8.))
                                    .bg(accent.opacity(0.14))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Icon::new(file_icon(&tab.path)).size_5().text_color(accent),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(2.))
                                    .child(
                                        ui::clipped("opaque-name", tab.name().to_string())
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD),
                                    )
                                    .child(
                                        div().text_xs().text_color(muted).child(format!(
                                            "{kind} · {}",
                                            human_size(tab.stat.size)
                                        )),
                                    ),
                            ),
                    )
                    .child(div().text_xs().text_color(muted).whitespace_normal().child(
                        match remote {
                            true => SharedString::from(format!(
                                "{why}{}",
                                tr("远程主机上的文件将先下载到本机的临时目录。")
                            )),
                            false => why,
                        },
                    ))
                    .children(upload_back)
                    .children(progress)
                    .child(actions)
                    .children(copy_note),
            )
            .into_any_element()
    }

    /// 没有打开文件时右边的样子。
    fn render_files_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, fg) = (theme.muted_foreground, theme.foreground);
        let remote = self.entry.server.is_some();
        let hint = |keys: &'static str, text: &'static str| {
            h_flex()
                .gap(px(10.))
                .child(
                    div()
                        .w(px(96.))
                        .text_right()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(keys),
                )
                .child(div().text_xs().text_color(muted).child(text))
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(px(14.))
            .child(
                Icon::new(IconName::Files)
                    .size_8()
                    .text_color(muted.opacity(0.5)),
            )
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child(tr("在左侧选择文件")),
            )
            .child(
                div()
                    .max_w(px(420.))
                    .text_center()
                    .text_xs()
                    .text_color(muted)
                    .whitespace_normal()
                    .child(if remote {
                        tr("单击预览，双击保持打开。将本机文件拖到目录树上即可上传。")
                    } else {
                        tr("单击预览，双击保持打开。将文件拖到目录树上即可复制。")
                    }),
            )
            .child(
                v_flex()
                    .mt(px(6.))
                    .gap(px(6.))
                    .child(hint("Ctrl+S", tr("保存")))
                    .child(hint("Ctrl+F", tr("在文件中查找")))
                    .child(hint("Ctrl+W", tr("关闭文件")))
                    .child(hint("Ctrl+Shift+E", tr("切换到文件页面"))),
            )
            .into_any_element()
    }

    fn render_file_note(
        &self,
        text: SharedString,
        error: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let color = if error {
            theme.danger
        } else {
            theme.muted_foreground
        };
        let id = self.files.active_tab().map(|tab| tab.id);
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .max_w(px(560.))
                    .text_sm()
                    .text_color(color)
                    .whitespace_normal()
                    .child(text),
            )
            .when(error, |d| {
                d.child(
                    Button::new("file-retry")
                        .small()
                        .label(tr("重试"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(id) = id {
                                this.load_file(id, window, cx);
                            }
                        })),
                )
            })
            .into_any_element()
    }

    /// 状态栏：完整路径，右边是行列、自动换行、编码、换行符、语言（文字文件），或者大小。
    fn render_file_status(&self, tab: &OpenFile, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, panel, hover, fg) = (
            theme.border,
            theme.muted_foreground,
            theme.sidebar,
            theme.list_hover,
            theme.foreground,
        );
        let mono = theme.mono_font_family.clone();
        let id = tab.id;
        let place = self.place(cx);
        let full = self.full_path(&tab.path);
        let shown = match self.entry.server.is_some() {
            true => format!("{place}:{full}"),
            false => full.clone(),
        };
        let cell = |text: String| div().flex_none().whitespace_nowrap().child(text);
        let mut bar = h_flex()
            .flex_none()
            .h(px(STATUS_HEIGHT))
            .px(px(12.))
            .gap(px(16.))
            .border_t_1()
            .border_color(border)
            .bg(panel)
            .text_size(px(11.))
            .text_color(muted)
            .child(
                ui::clipped_with_tip("file-full-path", shown, full.clone())
                    .flex_1()
                    .min_w_0()
                    .text_ellipsis_start()
                    .font_family(mono.clone()),
            );
        // 服务器上的文件：映射到本机了就写出本机路径（点一下复制），没有就给一个去映射的入口
        if let Some(server) = self.entry.server.clone() {
            let app = self.app.clone();
            let state = app.read(cx);
            let link = |id: &'static str| {
                h_flex()
                    .id(id)
                    .flex_none()
                    .max_w(px(360.))
                    .gap(px(4.))
                    .px(px(6.))
                    .mx(px(-6.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(hover).text_color(fg))
            };
            // 三种都带盘的图标、都有提示：点了会做什么，一看就知道
            let drive = || Icon::new(IconName::HardDrive).size_3().flex_none();
            let cell = if let Some(local) = state.local_path(&server, &full) {
                let copy = local.clone();
                let tip: SharedString = format!("{local}\n{}", tr("点击复制本机路径")).into();
                link("file-local-path")
                    .child(drive())
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_ellipsis_start()
                            .font_family(mono)
                            .child(local),
                    )
                    .tooltip(move |window, cx| ui::full_text_tip(tip.clone(), window, cx))
                    .on_click(cx.listener(move |_, _, window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                        use gpui_kit::component::WindowExt as _;
                        use gpui_kit::component::notification::Notification;
                        window.push_notification(Notification::info(tr("已复制本机路径")), cx);
                    }))
            } else if let Some(mapping) = state.covering(&server, &full) {
                let id = mapping.id.clone();
                let app = app.clone();
                let tip: SharedString =
                    tf!("立即映射到 {local}", local = mapping.local_root()).into();
                link("file-map-now")
                    .child(drive())
                    .child(tr("立即映射"))
                    .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
                    .on_click(move |_, _, cx| app.update(cx, |app, cx| app.mount_mapping(&id, cx)))
            } else {
                let (app, root) = (app.clone(), self.entry.path.clone());
                link("file-map")
                    .child(drive())
                    .child(tr("映射到本机…"))
                    .tooltip(|window, cx| ui::tip(tr("映射后可用本机程序直接打开"), window, cx))
                    .on_click(move |_, window, cx| {
                        crate::mapping::map_dialog(
                            app.clone(),
                            server.clone(),
                            root.clone(),
                            None,
                            window,
                            cx,
                        )
                    })
            };
            bar = bar.child(cell);
        }
        match &tab.body {
            Body::Text(doc) => {
                let position = doc.editor.read(cx).cursor_position();
                let wrap = doc.wrap;
                bar = bar
                    .child(cell(tf!(
                        "第 {line} 行，第 {column} 列",
                        line = position.line + 1,
                        column = position.character + 1
                    )))
                    .child(
                        div()
                            .id("file-wrap")
                            .flex_none()
                            .px(px(6.))
                            .mx(px(-6.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .hover(|s| s.bg(hover).text_color(fg))
                            .child(if wrap {
                                tr("自动换行：开")
                            } else {
                                tr("自动换行：关")
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.toggle_wrap(id, window, cx)
                            })),
                    )
                    .child(cell(match doc.bom {
                        true => format!("{} BOM", doc.encoding.label()),
                        false => doc.encoding.label().to_string(),
                    }))
                    .child(cell(if doc.crlf { "CRLF" } else { "LF" }.to_string()))
                    .child(cell(doc.language.to_string()))
                    .child(cell(human_size(tab.stat.size)));
            }
            Body::Image(_) | Body::Opaque(_) => {
                bar = bar.child(cell(human_size(tab.stat.size)));
            }
            Body::Loading | Body::Failed(_) => {}
        }
        bar.into_any_element()
    }
}
