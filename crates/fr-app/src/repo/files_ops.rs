//! 文件页里改磁盘的那些操作：新建文件和文件夹、改名、删除、上传、下载，
//! 和「用默认程序打开」（服务器上的文件先下载一份到本机，在本机改过可以再传回去）。
//!
//! 做完都刷新一遍：树重新列，Git 页的「本地修改」跟着变。

use std::path::{Path, PathBuf};
use std::time::Duration;

use fr_channel::{FsWrite, UploadItem};
use fr_core::i18n::tr;
use fr_core::tf;
use futures::StreamExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Sizable as _, WindowExt as _};
use gpui_kit::*;

use super::dialogs::Ask;
use super::files::{
    Body, LocalCopy, OpenFile, Transfer, TreeRow, copies_dir, join, local_file_name, modified,
    remove_copy, same_version, split,
};
use super::{Page, RepoView};
use crate::text::human_size;

/// 一次最多上传这么多个文件（拖进来一个很大的目录时先拦住）
const UPLOAD_MAX: usize = 5000;
/// 比这大的文件，下载到本机打开之前先问一下
const OPEN_ASK: u64 = 200 << 20;

/// 文件名、目录名里不能有的字符（Windows 上建不出来，传到本机也会出问题）。
const BAD_CHARS: [char; 8] = ['\\', ':', '*', '?', '"', '<', '>', '|'];

/// 名字合不合规矩；不合时返回给用户看的说明。`nested` 为真时可以用 `/` 带上目录。
pub(super) fn name_error(name: &str, nested: bool) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return Some(tr("请填写名称").into());
    }
    if !nested && name.contains('/') {
        return Some(tr("名称中不能包含 /").into());
    }
    if let Some(bad) = name
        .chars()
        .find(|c| BAD_CHARS.contains(c) || c.is_control())
    {
        return Some(tf!("名称中不能包含 {bad}", bad));
    }
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Some(tr("名称无效").into());
        }
        if part.ends_with(' ') || part.ends_with('.') || part.starts_with(' ') {
            return Some(tr("名称的首尾不能是空格，末尾不能是句点").into());
        }
        if fr_core::files::is_git_name(part) {
            return Some(tr("不能使用 .git：这是 Git 存放仓库数据的目录").into());
        }
    }
    None
}

/// 把选中、拖进来的文件和目录排成要传的一个个文件：本机路径，和放到目标目录下的相对路径。
/// `.git` 目录（拖进来的是一个仓库时里面有）不传：文件页不往 Git 的数据目录里写。
fn plan_upload(paths: &[PathBuf]) -> Result<Vec<(PathBuf, String)>, String> {
    fn walk(
        local: &Path,
        relative: String,
        plan: &mut Vec<(PathBuf, String)>,
    ) -> Result<(), String> {
        if plan.len() > UPLOAD_MAX {
            return Err(tf!("文件超过 {max} 个，请分批上传。", max = UPLOAD_MAX));
        }
        if !local.is_dir() {
            plan.push((local.to_path_buf(), relative));
            return Ok(());
        }
        let entries = std::fs::read_dir(local)
            .map_err(|err| tf!("无法读取 {path}：{err}", path = local.display(), err))?;
        let mut children: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        children.sort();
        for child in children {
            let name = child.file_name().unwrap_or_default().to_string_lossy();
            if fr_core::files::is_git_name(&name) {
                continue;
            }
            walk(&child, format!("{relative}/{name}"), plan)?;
        }
        Ok(())
    }
    let mut plan = Vec::new();
    for path in paths {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.is_empty() || fr_core::files::is_git_name(&name) {
            continue;
        }
        walk(path, name.into_owned(), &mut plan)?;
    }
    if plan.is_empty() && !paths.is_empty() {
        return Err(tr("没有可传的文件（.git 目录不传）。").into());
    }
    Ok(plan)
}

/// 菜单项点了以后在仓库视图上做的事。
pub(super) type ViewAction = Box<dyn Fn(&mut RepoView, &mut Window, &mut Context<RepoView>)>;

/// 文件页菜单里的一项：点了在仓库视图上执行 `run`（视图已经关了就不做）。
pub(super) fn view_item(
    view: &WeakEntity<RepoView>,
    label: &'static str,
    run: ViewAction,
) -> PopupMenuItem {
    let view = view.clone();
    PopupMenuItem::new(label).on_click(move |_, window, cx| {
        view.update(cx, |this, cx| run(this, window, cx)).ok();
    })
}

/// 文件交给别处看的几项：网页、SVG「在浏览器中打开」，Markdown「打开预览」，都有「用默认程序打开」。
pub(super) fn open_items(menu: PopupMenu, view: &WeakEntity<RepoView>, path: &str) -> PopupMenu {
    let item = |label, run| view_item(view, label, run);
    let mut menu = menu;
    if crate::open_with::is_web(path) {
        let path = path.to_string();
        menu = menu.item(item(
            tr("在浏览器中打开"),
            Box::new(move |this, window, cx| this.open_in_browser(path.clone(), window, cx)),
        ));
    }
    if crate::open_with::is_markdown(path) {
        let path = path.to_string();
        menu = menu.item(item(
            tr("打开预览"),
            Box::new(move |this, window, cx| this.preview_markdown(path.clone(), window, cx)),
        ));
    }
    let path = path.to_string();
    menu.item(item(
        tr("用默认程序打开"),
        Box::new(move |this, window, cx| this.open_with_default(path.clone(), window, cx)),
    ))
}

/// 树里一行（或者空白处，`row` 为 None）的右键菜单。
pub(super) fn tree_menu(
    view: WeakEntity<RepoView>,
    row: Option<TreeRow>,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, _, cx| {
        let Some(this) = view.upgrade() else {
            return menu;
        };
        let remote = this.read(cx).entry.server.is_some();
        // 新建、上传的目标：目录就是它自己，文件是它所在的目录，空白处是仓库根
        let dir = match &row {
            Some(row) if row.dir => row.path.clone(),
            Some(row) => split(&row.path).0.to_string(),
            None => String::new(),
        };
        let item = |label, run| view_item(&view, label, run);
        let mut menu = menu;
        if let Some(row) = row.as_ref().filter(|row| !row.dir) {
            let (open, changes, history, blame) = (
                row.path.clone(),
                row.path.clone(),
                row.path.clone(),
                row.path.clone(),
            );
            let changed = this.read(cx).files.mark(&row.path).is_some();
            menu = menu.item(item(
                tr("打开"),
                Box::new(move |this, window, cx| this.open_file(open.clone(), false, window, cx)),
            ));
            menu = open_items(menu, &view, &row.path);
            menu = menu.separator();
            if changed {
                menu = menu.item(item(
                    tr("查看改动"),
                    Box::new(move |this, window, cx| {
                        this.show_file_changes(changes.clone(), window, cx)
                    }),
                ));
            }
            menu = menu
                .item(item(
                    tr("文件历史"),
                    Box::new(move |this, window, cx| {
                        this.show_page(Page::Git, window, cx);
                        this.show_file_history(history.clone(), window, cx);
                    }),
                ))
                .item(item(
                    tr("逐行追溯（blame）"),
                    Box::new(move |this, window, cx| {
                        this.show_page(Page::Git, window, cx);
                        this.show_blame(blame.clone(), cx);
                    }),
                ))
                .separator();
        }
        let (new_file, new_folder, upload) = (dir.clone(), dir.clone(), dir.clone());
        menu = menu
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
            ));
        let Some(row) = row.clone() else {
            return menu;
        };
        let (full, relative, reveal, download, rename, delete) = (
            row.path.clone(),
            row.path.clone(),
            row.path.clone(),
            row.path.clone(),
            row.path.clone(),
            row.path.clone(),
        );
        menu = menu
            .separator()
            .item(item(
                tr("复制路径"),
                Box::new(move |this, _, cx| this.copy_path(&full, false, cx)),
            ))
            .item(item(
                tr("复制相对路径"),
                Box::new(move |this, _, cx| this.copy_path(&relative, true, cx)),
            ));
        if !remote {
            menu = menu.item(item(
                crate::shell::reveal_label(),
                Box::new(move |this, window, cx| {
                    let path = this.full_path(&reveal);
                    crate::shell::report(crate::shell::reveal_in_explorer(&path), window, cx);
                }),
            ));
        } else {
            // 服务器上的：映射到本机了就有本机路径，没有就可以在这里映射
            let state = this.read(cx);
            let (app, root, full) = (
                state.app.clone(),
                state.entry.path.clone(),
                state.full_path(&row.path),
            );
            if let Some(server) = state.entry.server.clone() {
                menu = crate::mapping::menu_items(menu, &app, &server, &full, row.dir, &root, cx);
            }
            if !row.dir {
                menu = menu.item(item(
                    tr("下载…"),
                    Box::new(move |this, window, cx| {
                        this.download_file(download.clone(), window, cx)
                    }),
                ));
            }
        }
        menu.separator()
            .item(item(
                tr("重命名…"),
                Box::new(move |this, window, cx| this.rename_dialog(rename.clone(), window, cx)),
            ))
            .item(item(
                tr("删除…"),
                Box::new(move |this, window, cx| this.delete_dialog(delete.clone(), window, cx)),
            ))
    }
}

impl RepoView {
    /// 报错：右下角弹一条。
    fn files_error(&self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        window.push_notification(Notification::error(text), cx);
    }

    pub(super) fn copy_path(&self, path: &str, relative: bool, cx: &mut Context<Self>) {
        let text = match relative {
            true => path.to_string(),
            false => self.full_path(path),
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// 跳到 Git 页「本地修改」里这个文件的改动。
    pub(super) fn show_file_changes(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_changes_view(window, cx);
        self.select_change(&path, cx);
    }

    /// 新建、上传到哪：选中的是目录就是它，是文件就是它所在的目录，没选是仓库根。
    pub(super) fn target_dir(&self) -> String {
        match &self.files.selected {
            Some(path) if self.files.is_dir(path) => path.clone(),
            Some(path) => split(path).0.to_string(),
            None => String::new(),
        }
    }

    fn where_hint(dir: &str) -> SharedString {
        match dir.is_empty() {
            true => tr("位置：仓库根目录。名称中可用 / 创建上级目录。").into(),
            false => tf!("位置：{dir}。名称中可用 / 创建上级目录。", dir).into(),
        }
    }

    pub(super) fn new_file_dialog(
        &mut self,
        dir: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (hint, within) = (Self::where_hint(&dir), dir.clone());
        let ask = Ask {
            title: tr("新建文件").into(),
            placeholder: tr("文件名，例如 notes.md").into(),
            hint: hint.into(),
            ok: tr("新建"),
            validate: Some(Box::new(move |this, answer| {
                name_error(&answer.text, true).or_else(|| this.exists_error(&within, &answer.text))
            })),
            ..Default::default()
        };
        self.ask(ask, window, cx, move |this, answer, window, cx| {
            let path = join(&dir, answer.text.trim());
            let Some((channel, repo)) = this.files_channel() else {
                return;
            };
            let params = FsWrite {
                repo,
                path: path.clone(),
                create: true,
                expect: None,
            };
            let request = channel.fs_write(params, Vec::new());
            cx.spawn_in(window, async move |this, cx| {
                let result = request.await;
                this.update_in(cx, |this, window, cx| match result {
                    Ok(_) => {
                        this.refresh(cx);
                        this.refresh_files(cx);
                        this.open_file(path.clone(), false, window, cx);
                        this.reveal_in_tree(&path, cx);
                    }
                    Err(err) => {
                        let text = tf!("新建失败：{err}", err = format!("{:#}", err));
                        this.files_error(text, window, cx);
                    }
                })
                .ok();
            })
            .detach();
        });
    }

    pub(super) fn new_folder_dialog(
        &mut self,
        dir: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (hint, within) = (Self::where_hint(&dir), dir.clone());
        let ask = Ask {
            title: tr("新建文件夹").into(),
            placeholder: tr("文件夹名").into(),
            hint: hint.into(),
            ok: tr("新建"),
            validate: Some(Box::new(move |this, answer| {
                name_error(&answer.text, true).or_else(|| this.exists_error(&within, &answer.text))
            })),
            ..Default::default()
        };
        self.ask(ask, window, cx, move |this, answer, window, cx| {
            let path = join(&dir, answer.text.trim());
            let Some((channel, repo)) = this.files_channel() else {
                return;
            };
            let request = channel.fs_mkdir(&repo, &path);
            cx.spawn_in(window, async move |this, cx| {
                let result = request.await;
                this.update_in(cx, |this, window, cx| match result {
                    Ok(()) => {
                        this.refresh_files(cx);
                        this.reveal_in_tree(&path, cx);
                    }
                    Err(err) => {
                        let text = tf!("新建失败：{err}", err = format!("{:#}", err));
                        this.files_error(text, window, cx);
                    }
                })
                .ok();
            })
            .detach();
        });
    }

    /// 这个目录里已经有同名的（只看列出来的那一层）。
    fn exists_error(&self, dir: &str, name: &str) -> Option<String> {
        let first = name.trim().split('/').next().unwrap_or_default();
        let nested = name.trim().contains('/');
        let taken = self
            .files
            .names_in(dir)
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(first));
        (taken && !nested).then(|| tf!("{name} 已存在", name = first))
    }

    pub(super) fn rename_dialog(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (parent, name) = split(&path);
        let (parent, name) = (parent.to_string(), name.to_string());
        let (within, old) = (parent.clone(), name.clone());
        let ask = Ask {
            title: tf!("重命名 {name}", name = name.clone()).into(),
            placeholder: tr("新名称").into(),
            value: name,
            ok: tr("重命名"),
            validate: Some(Box::new(move |this, answer| {
                // 只改大小写不算重名
                let same = answer.text.eq_ignore_ascii_case(&old);
                name_error(&answer.text, false).or_else(|| {
                    (!same)
                        .then(|| this.exists_error(&within, &answer.text))
                        .flatten()
                })
            })),
            ..Default::default()
        };
        self.ask(ask, window, cx, move |this, answer, window, cx| {
            let to = join(&parent, answer.text.trim());
            if to == path {
                return;
            }
            let Some((channel, repo)) = this.files_channel() else {
                return;
            };
            let request = channel.fs_rename(&repo, &path, &to);
            let from = path.clone();
            cx.spawn_in(window, async move |this, cx| {
                let result = request.await;
                this.update_in(cx, |this, window, cx| match result {
                    Ok(()) => this.files_moved(&from, Some(&to), window, cx),
                    Err(err) => {
                        let text = tf!("重命名失败：{err}", err = format!("{:#}", err));
                        this.files_error(text, window, cx);
                    }
                })
                .ok();
            })
            .detach();
        });
    }

    pub(super) fn delete_dialog(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = split(&path).1.to_string();
        let dir = self.files.is_dir(&path);
        let description = match (self.entry.server.is_some(), dir) {
            (true, true) => tr("将从主机上永久删除此目录及其中的全部内容，无法撤销。"),
            (true, false) => tr("将从主机上永久删除此文件，无法撤销。"),
            (false, true) => tr("此目录及其中的全部内容将移到回收站。"),
            (false, false) => tr("此文件将移到回收站。"),
        };
        self.confirm(
            tf!("删除 {name}？", name),
            description,
            tr("删除"),
            window,
            cx,
            move |this, window, cx| {
                let Some((channel, repo)) = this.files_channel() else {
                    return;
                };
                let request = channel.fs_remove(&repo, &path);
                let path = path.clone();
                cx.spawn_in(window, async move |this, cx| {
                    let result = request.await;
                    this.update_in(cx, |this, window, cx| match result {
                        Ok(()) => this.files_moved(&path, None, window, cx),
                        Err(err) => {
                            let text = tf!("删除失败：{err}", err = format!("{:#}", err));
                            this.files_error(text, window, cx);
                        }
                    })
                    .ok();
                })
                .detach();
            },
        );
    }

    // ---------- 上传、下载 ----------

    /// 让用户选本机的文件，传到 `dir` 里。
    pub(super) fn pick_upload(&mut self, dir: String, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.upload_paths(dir, paths, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// 把本机的文件、目录传到 `dir` 里（选的，或者拖进来的）。有同名的先问要不要覆盖。
    pub(super) fn upload_paths(
        &mut self,
        dir: String,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() || self.channel.is_none() {
            return;
        }
        let existing = self.files.names_in(&dir);
        let clash: Vec<String> = paths
            .iter()
            .filter_map(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
            .filter(|name| {
                existing
                    .iter()
                    .any(|other| other.eq_ignore_ascii_case(name))
            })
            .collect();
        if clash.is_empty() {
            self.start_upload(dir, paths, false, window, cx);
            return;
        }
        let names = match clash.len() {
            1..=3 => clash.join(tr("、")),
            count => tf!(
                "{first} 等 {count} 项",
                first = clash[..2].join(tr("、")),
                count
            ),
        };
        let place = if dir.is_empty() {
            tr("仓库根目录").to_string()
        } else {
            dir.clone()
        };
        self.confirm(
            tr("覆盖同名文件？"),
            tf!("{place} 中已有 {names}，继续将覆盖同名文件。", place, names),
            tr("覆盖"),
            window,
            cx,
            move |this, window, cx| {
                this.start_upload(dir.clone(), paths.clone(), true, window, cx);
            },
        );
    }

    fn start_upload(
        &mut self,
        dir: String,
        paths: Vec<PathBuf>,
        overwrite: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((channel, repo)) = self.files_channel() else {
            return;
        };
        let remote = self.entry.server.is_some();
        let id = self.files.next_id();
        let task = cx.spawn_in(window, async move |this, cx| {
            let planned = cx
                .background_executor()
                .spawn(async move { plan_upload(&paths) })
                .await;
            let failed = |text: String, cx: &mut AsyncWindowContext| {
                this.update_in(cx, |this, window, cx| {
                    this.files.transfers.retain(|transfer| transfer.id != id);
                    this.files_error(text, window, cx);
                    this.refresh_files(cx);
                })
                .ok();
            };
            let plan = match planned {
                Ok(plan) => plan,
                Err(text) => return failed(text, cx),
            };
            let count = plan.len();
            for (index, (local, relative)) in plan.into_iter().enumerate() {
                let name = split(&relative).1.to_string();
                let label: SharedString = match (remote, count) {
                    (true, 1) => tf!("正在上传 {name}", name),
                    (true, _) => tf!("正在上传 {name}（{n}/{count}）", name, n = index + 1, count),
                    (false, 1) => tf!("正在复制 {name}", name),
                    (false, _) => tf!("正在复制 {name}（{n}/{count}）", name, n = index + 1, count),
                }
                .into();
                let dest = join(&dir, &relative);
                let mut stream = channel.fs_upload(&repo, &dest, local, overwrite);
                let mut done = false;
                while let Some(item) = stream.next().await {
                    let (sent, total) = match item {
                        Ok(UploadItem::Progress { sent, total }) => (sent, total),
                        Ok(UploadItem::Done(_)) => {
                            done = true;
                            break;
                        }
                        Err(err) => {
                            let text = match remote {
                                true => {
                                    tf!("上传 {name} 失败：{err}", name, err = format!("{:#}", err))
                                }
                                false => {
                                    tf!("复制 {name} 失败：{err}", name, err = format!("{:#}", err))
                                }
                            };
                            return failed(text, cx);
                        }
                    };
                    let label = label.clone();
                    let alive = this.update(cx, |this, cx| {
                        if let Some(transfer) = this.files.transfers.iter_mut().find(|t| t.id == id)
                        {
                            (transfer.label, transfer.sent, transfer.total) = (label, sent, total);
                            cx.notify();
                        }
                    });
                    if alive.is_err() {
                        return;
                    }
                }
                if !done {
                    return failed(tr("传输中断").into(), cx);
                }
            }
            this.update_in(cx, |this, window, cx| {
                this.files.transfers.retain(|transfer| transfer.id != id);
                let place = if dir.is_empty() {
                    tr("仓库根目录").to_string()
                } else {
                    dir.clone()
                };
                let text = match remote {
                    true => tf!("已上传 {count} 个文件到 {place}", count, place),
                    false => tf!("已复制 {count} 个文件到 {place}", count, place),
                };
                window.push_notification(Notification::success(text), cx);
                this.refresh(cx);
                this.refresh_files(cx);
                // 传到的目录展开给用户看
                if !dir.is_empty() {
                    this.reveal_in_tree(&dir, cx);
                    this.expand_dir(dir.clone(), cx);
                }
            })
            .ok();
        });
        self.files.transfers.push(Transfer {
            id,
            label: if remote {
                tr("正在准备上传…").into()
            } else {
                tr("正在准备复制…").into()
            },
            sent: 0,
            total: 0,
            _task: task,
        });
        cx.notify();
    }

    /// 取消一批上传或者一次下载。
    pub(super) fn cancel_transfer(&mut self, id: u64, cx: &mut Context<Self>) {
        self.files.transfers.retain(|transfer| transfer.id != id);
        // 传了一半的那几个文件已经在了
        self.refresh_files(cx);
        cx.notify();
    }

    /// 把服务器上的文件存到本机：先问存到哪。
    pub(super) fn download_file(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = split(&path).1.to_string();
        let downloads = downloads_dir().unwrap_or_else(std::env::temp_dir);
        let picked = cx.prompt_for_new_path(&downloads, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(dest))) = picked.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.start_download(path, dest, window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn start_download(
        &mut self,
        path: String,
        dest: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((channel, repo)) = self.files_channel() else {
            return;
        };
        let name = split(&path).1.to_string();
        let id = self.files.next_id();
        let mut stream = channel.fs_download(&repo, &path, dest.clone());
        let label: SharedString = tf!("正在下载 {name}", name = name.clone()).into();
        let task = cx.spawn_in(window, async move |this, cx| {
            let mut outcome = Err(tr("传输中断").to_string());
            while let Some(item) = stream.next().await {
                match item {
                    Ok(UploadItem::Progress { sent, total }) => {
                        let alive = this.update(cx, |this, cx| {
                            if let Some(transfer) =
                                this.files.transfers.iter_mut().find(|t| t.id == id)
                            {
                                (transfer.sent, transfer.total) = (sent, total);
                                cx.notify();
                            }
                        });
                        if alive.is_err() {
                            return;
                        }
                    }
                    Ok(UploadItem::Done(_)) => {
                        outcome = Ok(());
                        break;
                    }
                    Err(err) => {
                        outcome = Err(format!("{err:#}"));
                        break;
                    }
                }
            }
            this.update_in(cx, |this, window, cx| {
                this.files.transfers.retain(|transfer| transfer.id != id);
                cx.notify();
                match outcome {
                    Ok(()) => {
                        let shown = dest.to_string_lossy().into_owned();
                        let text = tf!("已下载到 {path}", path = shown.clone());
                        let notification = Notification::success(text).action(move |_, _, _| {
                            let shown = shown.clone();
                            Button::new("download-reveal")
                                .small()
                                .label(crate::shell::reveal_label())
                                .on_click(move |_, window, cx| {
                                    let result = crate::shell::reveal_in_explorer(&shown);
                                    crate::shell::report(result, window, cx);
                                })
                        });
                        window.push_notification(notification, cx);
                    }
                    Err(err) => {
                        let text = tf!("下载 {name} 失败：{err}", name, err);
                        this.files_error(text, window, cx);
                    }
                }
            })
            .ok();
        });
        self.files.transfers.push(Transfer {
            id,
            label,
            sent: 0,
            total: 0,
            _task: task,
        });
        cx.notify();
    }

    // ---------- 交给浏览器、默认程序 ----------

    /// 在浏览器中打开仓库里的 `path`（网页、SVG），经本机网页服务（见 [`crate::web_serve`]）。编辑器
    /// 里有没保存的修改时提示一下：浏览器里是磁盘上的那一版。
    pub(super) fn open_in_browser(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .files
            .tabs
            .iter()
            .any(|tab| tab.path == path && tab.dirty())
        {
            let text = tr("此文件有未保存的修改，浏览器中显示的是已保存的内容。");
            window.push_notification(Notification::info(text), cx);
        }
        crate::open_with::open_in_browser(&self.entry, self.channel.clone(), &path, window, cx);
    }

    /// 用默认程序打开仓库里的 `path`。本机的直接打开；远程主机上的：映射着的开映射盘上的，文件页里
    /// 显示成卡片的用卡片那一份（本机改过可以传回去），其余的下载一份到本机再开。
    pub(super) fn open_with_default(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let full = self.full_path(&path);
        let Some(server) = self.entry.server.clone() else {
            return crate::shell::open_file(&full, window, cx);
        };
        if let Some(local) = self.app.read(cx).local_path(&server, &full) {
            return crate::shell::open_file(&local, window, cx);
        }
        let card = self
            .files
            .tabs
            .iter()
            .find(|tab| tab.path == path && matches!(tab.body, Body::Opaque(_)))
            .map(|tab| tab.id);
        if let Some(id) = card {
            return self.open_externally(id, window, cx);
        }
        if let Some((channel, repo)) = self.files_channel() {
            crate::open_with::open_copy(channel, &repo, &path, window, cx);
        }
    }

    // ---------- 用默认程序打开 ----------

    /// 用系统给这种文件设的默认程序打开。本机仓库直接打开；服务器上的先下载一份到本机的
    /// 临时目录再打开（太大的先问一下）。
    pub(super) fn open_externally(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.files.tabs.iter().find(|tab| tab.id == id) else {
            return;
        };
        if self.entry.server.is_none() {
            let path = self.full_path(&tab.path);
            crate::shell::open_file(&path, window, cx);
            return;
        }
        // 已经下载过：主机上的没变，或者在本机改过（还没传回去），直接打开那一份；本机没动过、
        // 主机上的变了，重新下载
        if let Body::Opaque(doc) = &tab.body
            && let Some(copy) = &doc.local
            && copy.path.is_file()
            && (copy.edited() || same_version(&copy.remote, &tab.stat))
        {
            let path = copy.path.to_string_lossy().into_owned();
            crate::shell::open_file(&path, window, cx);
            return;
        }
        if tab.stat.size <= OPEN_ASK {
            self.fetch_and_open(id, window, cx);
            return;
        }
        self.confirm_plain(
            tr("下载后打开？"),
            tf!(
                "此文件大小为 {size}，需先下载到本机再打开。",
                size = human_size(tab.stat.size)
            ),
            tr("下载并打开"),
            window,
            cx,
            move |this, window, cx| this.fetch_and_open(id, window, cx),
        );
    }

    fn fetch_and_open(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some((channel, repo)) = self.files_channel() else {
            return;
        };
        let Some(tab) = self.files.tab_mut(id) else {
            return;
        };
        let (path, name, remote) = (tab.path.clone(), tab.name().to_string(), tab.stat);
        let Some(doc) = tab.opaque_mut() else {
            return;
        };
        let dest = new_copy_dir().join(local_file_name(&name));
        let mut stream = channel.fs_download(&repo, &path, dest.clone());
        let label: SharedString = tr("正在下载…").into();
        doc.busy = Some((label.clone(), 0, 0));
        doc.task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut outcome = Err(tr("传输中断").to_string());
            while let Some(item) = stream.next().await {
                match item {
                    Ok(UploadItem::Progress { sent, total }) => {
                        let label = label.clone();
                        let alive = this.update(cx, |this, cx| {
                            if let Some(doc) = this.files.tab_mut(id).and_then(OpenFile::opaque_mut)
                            {
                                doc.busy = Some((label, sent, total));
                                cx.notify();
                            }
                        });
                        if alive.is_err() {
                            return;
                        }
                    }
                    Ok(UploadItem::Done(_)) => {
                        outcome = Ok(());
                        break;
                    }
                    Err(err) => {
                        outcome = Err(format!("{err:#}"));
                        break;
                    }
                }
            }
            this.update_in(cx, |this, window, cx| {
                cx.notify();
                let Some(doc) = this.files.tab_mut(id).and_then(OpenFile::opaque_mut) else {
                    return;
                };
                doc.busy = None;
                if let Err(err) = outcome {
                    let _ = std::fs::remove_dir_all(dest.parent().unwrap_or(&dest));
                    let text = tf!("下载 {name} 失败：{err}", name, err);
                    this.files_error(text, window, cx);
                    return;
                }
                let fresh = LocalCopy {
                    mtime: modified(&dest),
                    path: dest.clone(),
                    changed: false,
                    remote,
                };
                // 换下来的那份（主机上变了以后重新下载的）在本机没改过，删掉
                if let Some(old) = doc.local.replace(fresh)
                    && !old.edited()
                {
                    remove_copy(&old);
                }
                let shown = dest.to_string_lossy().into_owned();
                crate::shell::open_file(&shown, window, cx);
                this.watch_local_copy(id, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// 隔一会儿看一眼下载到本机的那份：改过了就在卡片上提示可以传回去。
    fn watch_local_copy(&mut self, id: u64, cx: &mut Context<Self>) {
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                let alive = this.update(cx, |this, cx| {
                    let Some(doc) = this.files.tab_mut(id).and_then(OpenFile::opaque_mut) else {
                        return false;
                    };
                    let Some(copy) = &mut doc.local else {
                        return false;
                    };
                    let now = modified(&copy.path);
                    if now.is_some() && now != copy.mtime && !copy.changed {
                        copy.changed = true;
                        cx.notify();
                    }
                    true
                });
                if !matches!(alive, Ok(true)) {
                    return;
                }
            }
        });
        if let Some(doc) = self.files.tab_mut(id).and_then(OpenFile::opaque_mut) {
            doc.poll = Some(poll);
        }
    }

    /// 把在本机改过的那份传回服务器，换掉原来的文件。先看一眼主机上的：下载以后在主机上被改过
    /// （或者删了），先问一声，免得盖掉别人或者服务器上的程序（比如 Claude Code）改的。
    pub(super) fn upload_back(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some((channel, repo)) = self.files_channel() else {
            return;
        };
        let Some(tab) = self.files.tab_mut(id) else {
            return;
        };
        let (path, name) = (tab.path.clone(), tab.name().to_string());
        let Some(doc) = tab.opaque_mut() else {
            return;
        };
        let Some(remote) = doc.local.as_ref().map(|copy| copy.remote) else {
            return;
        };
        if doc.busy.is_some() {
            return;
        }
        let request = channel.fs_stat(&repo, &path);
        doc.busy = Some((tr("正在上传…").into(), 0, 0));
        doc.task = Some(cx.spawn_in(window, async move |this, cx| {
            let now = request.await;
            this.update_in(cx, |this, window, cx| {
                if let Some(doc) = this.files.tab_mut(id).and_then(OpenFile::opaque_mut) {
                    doc.busy = None;
                }
                cx.notify();
                match now {
                    Ok(now) if !same_version(&now, &remote) => this.confirm(
                        tr("覆盖主机上的文件？"),
                        tf!(
                            "主机上的 {name} 在下载之后被改过或删除了。上传会用本机的这份替换主机上现在的内容。",
                            name
                        ),
                        tr("上传并覆盖"),
                        window,
                        cx,
                        move |this, window, cx| this.upload_now(id, window, cx),
                    ),
                    // 查不了（比如断线了）照常传，传不上会报错
                    _ => this.upload_now(id, window, cx),
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn upload_now(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some((channel, repo)) = self.files_channel() else {
            return;
        };
        let Some(tab) = self.files.tab_mut(id) else {
            return;
        };
        let (path, name) = (tab.path.clone(), tab.name().to_string());
        let Some(doc) = tab.opaque_mut() else {
            return;
        };
        let Some(copy) = &doc.local else {
            return;
        };
        let (local, sent_mtime) = (copy.path.clone(), modified(&copy.path));
        let mut stream = channel.fs_upload(&repo, &path, local, true);
        let stat = channel.clone();
        let label: SharedString = tr("正在上传…").into();
        doc.busy = Some((label.clone(), 0, 0));
        doc.task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut outcome = Err(tr("传输中断").to_string());
            while let Some(item) = stream.next().await {
                match item {
                    Ok(UploadItem::Progress { sent, total }) => {
                        let label = label.clone();
                        let alive = this.update(cx, |this, cx| {
                            if let Some(doc) = this.files.tab_mut(id).and_then(OpenFile::opaque_mut)
                            {
                                doc.busy = Some((label, sent, total));
                                cx.notify();
                            }
                        });
                        if alive.is_err() {
                            return;
                        }
                    }
                    Ok(UploadItem::Done(_)) => {
                        outcome = Ok(());
                        break;
                    }
                    Err(err) => {
                        outcome = Err(format!("{err:#}"));
                        break;
                    }
                }
            }
            let fresh = match outcome.is_ok() {
                true => stat.fs_stat(&repo, &path).await.ok(),
                false => None,
            };
            this.update_in(cx, |this, window, cx| {
                cx.notify();
                let Some(tab) = this.files.tab_mut(id) else {
                    return;
                };
                if let Some(fresh) = fresh {
                    tab.stat = fresh;
                }
                let Some(doc) = tab.opaque_mut() else {
                    return;
                };
                doc.busy = None;
                match outcome {
                    Ok(()) => {
                        if let Some(copy) = &mut doc.local {
                            // 传的这段时间里又改了的话，下一轮还会提示
                            copy.mtime = sent_mtime;
                            copy.changed = false;
                            // 主机上现在是传上去的这份：之后再变才是别处改的
                            if let Some(fresh) = fresh {
                                copy.remote = fresh;
                            }
                        }
                        let text = tf!("已将 {name} 上传到主机", name);
                        window.push_notification(Notification::success(text), cx);
                        this.refresh(cx);
                    }
                    Err(err) => {
                        let text = tf!("上传 {name} 失败：{err}", name, err);
                        this.files_error(text, window, cx);
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// 卡片上的「取消」：正在下载、上传的那一次不做了。
    pub(super) fn cancel_fetch(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(doc) = self.files.tab_mut(id).and_then(OpenFile::opaque_mut) {
            doc.task = None;
            doc.busy = None;
            cx.notify();
        }
    }
}

/// 「下载」文件夹：Windows 是 `<用户目录>\Downloads`；Linux 问 `xdg-user-dir`（中文桌面上叫「下载」），
/// 问不到用 `~/Downloads`。不存在时是 None。
fn downloads_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let dir = std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("Downloads"));
    #[cfg(not(windows))]
    let dir = std::process::Command::new("xdg-user-dir")
        .arg("DOWNLOAD")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|dir| PathBuf::from(dir.trim()))
        .filter(|dir| dir.is_absolute() && dir.is_dir())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads")));
    dir.filter(|dir| dir.is_dir())
}

/// 下载副本的子目录编号：整个进程一个计数。不用标签的编号，那是每个仓库页各自从头数的，两个
/// 仓库页会撞上，关一个标签就删了另一个仓库的副本。
static NEXT_COPY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// 新下载的一份副本放进哪个目录：[`copies_dir`] 下每次一个。文件名和主机上的一样（换掉本机不能用
/// 的字符，见 [`local_file_name`]），默认程序的标题栏里看得出是哪个文件。
pub(crate) fn new_copy_dir() -> PathBuf {
    sweep_old_copies();
    let number = NEXT_COPY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    copies_dir().join(format!("{}-{number}", std::process::id()))
}

/// 清掉以前留下的副本：Kuai 没关标签页就退出时，副本留在临时目录里。别的进程的、七天没动过的删掉
/// （本进程的还开着）。每个进程只清一次。
fn sweep_old_copies() {
    static SWEPT: std::sync::Once = std::sync::Once::new();
    SWEPT.call_once(|| {
        let mine = format!("{}-", std::process::id());
        let Ok(entries) = std::fs::read_dir(copies_dir()) else {
            return;
        };
        let week = Duration::from_secs(7 * 24 * 3600);
        for entry in entries.flatten() {
            let old = modified(&entry.path())
                .and_then(|time| time.elapsed().ok())
                .is_some_and(|age| age > week);
            if old && !entry.file_name().to_string_lossy().starts_with(&mine) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{name_error, plan_upload};

    #[test]
    fn 名字的规矩() {
        for good in ["a.txt", "说明 1.md", ".gitignore", "a-b_c"] {
            assert_eq!(name_error(good, false), None, "{good}");
        }
        assert_eq!(name_error("docs/图/a.md", true), None);
        assert!(name_error("docs/a.md", false).is_some());
        for bad in [
            "", "  ", "a:b", "a*b", "..", "a/../b", "a//b", "尾巴.", "a\\b", "/a",
        ] {
            assert!(name_error(bad, true).is_some(), "{bad}");
        }
        // Git 的数据目录：哪一层、什么写法都不行
        for bad in [".git", ".GIT/config", "a/.git/hooks/pre-commit", "GIT~1"] {
            assert!(name_error(bad, true).is_some(), "{bad}");
        }
        assert_eq!(name_error(".gitignore", false), None);
    }

    #[test]
    fn 上传的清单_目录展开成一个个文件() {
        let root = std::env::temp_dir().join(format!("kuai-plan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("包/子")).unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(root.join("包/b.txt"), b"b").unwrap();
        std::fs::write(root.join("包/子/c.txt"), b"c").unwrap();
        // 拖进来的是一个仓库：里面的 .git 不传
        std::fs::create_dir_all(root.join("包/.git")).unwrap();
        std::fs::write(root.join("包/.git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        let plan = plan_upload(&[root.join("a.txt"), root.join("包")]).unwrap();
        let relative: Vec<&str> = plan.iter().map(|(_, to)| to.as_str()).collect();
        assert_eq!(relative, ["a.txt", "包/b.txt", "包/子/c.txt"]);
        assert_eq!(plan[2].0, root.join("包/子/c.txt"));
        assert!(plan_upload(&[root.join("包/.git")]).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
