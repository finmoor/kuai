//! 右键菜单，以及从菜单里发起的操作：合并、变基、挑拣、还原、重置、标签、分支改名、
//! 删除远程分支、解决冲突时取哪一边。
//!
//! 菜单在点右键时才构建；构建函数里只带要用到的名字和哈希，点了菜单项再回到 [`RepoView`] 上执行。

use fr_core::i18n::{tr, tr_key};
use fr_core::state::InProgress;
use fr_core::status::{Entry, Kind};
use fr_core::tf;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::changes::Area;
use super::dialogs::{Ask, ref_name_error};
use super::history::Scope;
use super::ops::{Step, args};
use super::sidebar::SideItem;
use super::sidebar::SideSection;
use super::{RepoEvent, RepoView};
use crate::settings::RepoEntry;

/// 列出子模块路径的 git 参数：读 `.gitmodules`，没有这个文件时命令失败。
pub(super) fn submodule_args() -> Vec<String> {
    args(&[
        "config",
        "--file",
        ".gitmodules",
        "--get-regexp",
        r"^submodule\..*\.path$",
    ])
}

/// 解析 [`submodule_args`] 的输出：每行是 `submodule.<名字>.path <路径>`。
pub(super) fn parse_submodules(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .filter_map(|line| {
            line.split_once(' ')
                .map(|(_, path)| path.trim().to_string())
        })
        .filter(|path| !path.is_empty())
        .collect()
}

/// 合并的一种做法。
struct MergeMode {
    label: &'static str,
    detail: &'static str,
    flag: Option<&'static str>,
}

/// 合并对话框里可以选的做法，第一种是默认的。
const MERGE_MODES: [MergeMode; 4] = [
    MergeMode {
        label: tr_key("默认"),
        detail: tr_key("能快进就快进，不能就生成合并提交。"),
        flag: None,
    },
    MergeMode {
        label: tr_key("总是生成合并提交（--no-ff）"),
        detail: tr_key("总是生成合并提交，历史里看得出合并过。"),
        flag: Some("--no-ff"),
    },
    MergeMode {
        label: tr_key("只快进（--ff-only）"),
        detail: tr_key("不能快进就什么都不做。"),
        flag: Some("--ff-only"),
    },
    MergeMode {
        label: tr_key("压缩成一份改动（--squash）"),
        detail: tr_key("对方的改动合成一份放进暂存区，不记录合并关系，自己写说明提交。"),
        flag: Some("--squash"),
    },
];

/// `git merge` 的参数。
fn merge_args(name: &str, flag: Option<&str>, no_commit: bool) -> Vec<String> {
    let mut merge = args(&["merge", "--no-edit"]);
    merge.extend(flag.map(String::from));
    if no_commit {
        // 能快进的合并没有「提交之前」这一步，git 会直接快进，`--no-commit` 等于没说；
        // 所以默认做法下勾了「先不提交」就不让它快进，这样才停得下来
        if flag.is_none() {
            merge.push("--no-ff".into());
        }
        merge.push("--no-commit".into());
    }
    merge.push(name.to_string());
    merge
}

/// 构建菜单的函数，交给组件库的 `context_menu`。
pub(super) type MenuFn = Box<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

type View = WeakEntity<RepoView>;

/// 一个菜单项：点了以后在仓库页上执行 `f`。
fn item(
    view: &View,
    label: impl Into<SharedString>,
    f: impl Fn(&mut RepoView, &mut Window, &mut Context<RepoView>) + 'static,
) -> PopupMenuItem {
    let view = view.clone();
    PopupMenuItem::new(label).on_click(move |_, window, cx| {
        view.update(cx, |this, cx| f(this, window, cx)).ok();
    })
}

/// 把一段文字复制到剪贴板的菜单项。
fn copy_item(label: impl Into<SharedString>, text: String) -> PopupMenuItem {
    PopupMenuItem::new(label).on_click(move |_, _, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
    })
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// 侧栏里分支、远程分支、标签菜单上管提交列表的两项：只看它，和把它藏起来。
/// `hidden` 为空表示这一项不能隐藏（当前分支）。
fn scope_items(
    menu: PopupMenu,
    view: &View,
    full: String,
    name: String,
    only: bool,
    hidden: Option<bool>,
) -> PopupMenu {
    let to = if only {
        Scope::All
    } else {
        Scope::Only {
            full: full.clone(),
            name,
        }
    };
    let menu = menu.item(
        item(view, tr("提交列表里只看它"), move |this, w, cx| {
            this.set_scope(to.clone(), w, cx)
        })
        .checked(only),
    );
    match hidden {
        Some(hidden) => menu.item(
            item(view, tr("在提交列表里隐藏"), move |this, _, cx| {
                this.toggle_hidden(full.clone(), cx)
            })
            .checked(hidden),
        ),
        None => menu,
    }
}

impl RepoView {
    /// 当前分支的名字；分离的 HEAD 时是「HEAD」。
    fn current_label(&self) -> String {
        self.status
            .as_ref()
            .and_then(|s| s.branch.clone())
            .unwrap_or_else(|| "HEAD".into())
    }

    /// 推送、删除远程分支时默认用的远程：有 origin 用 origin，否则用第一个。
    pub(super) fn default_remote(&self) -> String {
        if self.remote_names.iter().any(|n| n == "origin") {
            return "origin".into();
        }
        self.remote_names
            .first()
            .cloned()
            .unwrap_or_else(|| "origin".into())
    }

    /// 解决冲突时两边各是什么。合并、挑拣时 ours 是当前分支；变基时反过来，ours 是变基的目标，
    /// theirs 才是自己分支上正在重放的那个提交。
    pub(super) fn side_labels(&self) -> (&'static str, &'static str) {
        match self.in_progress {
            Some(InProgress::Merge) => (tr("当前分支的版本"), tr("合并进来的版本")),
            Some(InProgress::Rebase) => (tr("变基目标上的版本"), tr("正在重放的那个提交的版本")),
            Some(InProgress::CherryPick) => (tr("当前分支的版本"), tr("被挑拣的提交的版本")),
            Some(InProgress::Revert) => (tr("当前分支的版本"), tr("还原之后的版本")),
            None => (tr("当前分支的版本"), tr("传入的版本")),
        }
    }

    // ---------- 菜单 ----------

    /// 提交历史里一个提交的菜单。
    pub(super) fn commit_menu(&self, ix: usize, cx: &mut Context<Self>) -> Option<MenuFn> {
        let commit = self.history.commit_at(ix)?;
        let view = cx.entity().downgrade();
        let id = commit.id.to_string();
        let subject = commit.subject.to_string();
        let is_merge = commit.parents.len() > 1;
        let cur = self.current_label();
        let is_head = self.status.as_ref().and_then(|s| s.head.as_deref()) == Some(&*id);
        let branches: Vec<String> = self
            .refs
            .branches
            .iter()
            .filter(|b| b.target == id && !b.current)
            .map(|b| b.name.clone())
            .collect();
        Some(Box::new(move |mut menu, window, cx| {
            for branch in &branches {
                let name = branch.clone();
                menu = menu.item(item(
                    &view,
                    tf!("切换到 {branch}", branch),
                    move |this, w, cx| this.checkout_branch(&name, w, cx),
                ));
            }
            let target = id.clone();
            menu = menu
                .item(item(
                    &view,
                    tr("检出这个提交（分离 HEAD）"),
                    move |this, w, cx| this.checkout_detached(target.clone(), w, cx),
                ))
                .separator()
                .item(item(&view, tr("在此新建分支…"), |this, w, cx| {
                    this.new_branch_dialog(w, cx)
                }))
                .item({
                    let (id, subject) = (id.clone(), subject.clone());
                    item(&view, tr("在此新建标签…"), move |this, w, cx| {
                        this.new_tag_dialog(id.clone(), subject.clone(), w, cx)
                    })
                })
                .separator();
            if !is_head {
                let pick = id.clone();
                menu = menu.item(item(
                    &view,
                    tf!("挑拣到 {cur}（cherry-pick）", cur),
                    move |this, w, cx| this.cherry_pick(pick.clone(), is_merge, w, cx),
                ));
            }
            let revert = id.clone();
            menu = menu.item(item(
                &view,
                tr("还原这个提交（revert）"),
                move |this, w, cx| this.revert_commit(revert.clone(), is_merge, w, cx),
            ));
            {
                let (view2, id2) = (view.clone(), id.clone());
                menu = menu.submenu(
                    tf!("把 {cur} 重置到这里", cur),
                    window,
                    cx,
                    move |sub, _, _| {
                        let mode = |label: &'static str, flag: &'static str| {
                            let id = id2.clone();
                            item(&view2, label, move |this, w, cx| {
                                this.reset_to(id.clone(), flag, w, cx)
                            })
                        };
                        // 在 HEAD 上 soft 什么都不改，不列
                        let sub = if is_head {
                            sub.item(mode(
                                tr("取消全部暂存，改动留在工作区（mixed）…"),
                                "--mixed",
                            ))
                        } else {
                            sub.item(mode(tr("保留改动，并且留在暂存区（soft）…"), "--soft"))
                                .item(mode(tr("保留改动，不暂存（mixed）…"), "--mixed"))
                        };
                        sub.separator()
                            .item(mode(tr("丢弃所有改动（hard）…"), "--hard"))
                    },
                );
            }
            if !is_head {
                let onto = id.clone();
                menu = menu.item(item(
                    &view,
                    tf!("把 {cur} 变基到这里…", cur),
                    move |this, w, cx| {
                        this.rebase_onto(onto.clone(), short(&onto).to_string(), w, cx)
                    },
                ));
                let base = id.clone();
                menu = menu.item(item(
                    &view,
                    tr("交互式变基：整理这之后的提交…"),
                    move |this, w, cx| this.interactive_rebase(base.clone(), w, cx),
                ));
            }
            menu.separator()
                .item(copy_item(tr("复制哈希"), id.clone()))
                .item(copy_item(tr("复制说明"), subject.clone()))
        }))
    }

    /// 侧栏里一项的菜单。
    pub(super) fn sidebar_menu(&self, side: SideItem, cx: &mut Context<Self>) -> Option<MenuFn> {
        let view = cx.entity().downgrade();
        let cur = self.current_label();
        match side {
            SideItem::Branch(i) => {
                let branch = self.refs.branches.get(i)?.clone();
                let full = format!("refs/heads/{}", branch.name);
                let (only, hidden) = (self.is_only(&full), self.hidden_refs(cx).contains(&full));
                Some(Box::new(move |mut menu, _, _| {
                    let name = branch.name.clone();
                    if !branch.current {
                        let (a, b, c) = (name.clone(), name.clone(), name.clone());
                        menu = menu
                            .item(item(
                                &view,
                                tf!("切换到 {name}", name),
                                move |this, w, cx| this.checkout_branch(&a, w, cx),
                            ))
                            .separator()
                            .item(item(
                                &view,
                                tf!("合并 {name} 到 {cur}…", name, cur),
                                move |this, w, cx| this.merge(b.clone(), w, cx),
                            ))
                            .item(item(
                                &view,
                                tf!("把 {cur} 变基到 {name}…", cur, name),
                                move |this, w, cx| this.rebase_onto(c.clone(), c.clone(), w, cx),
                            ))
                            .separator();
                    }
                    let (a, b, c, d) = (branch.clone(), branch.clone(), name.clone(), name.clone());
                    menu = menu
                        .item(item(
                            &view,
                            tf!("推送 {name}", name),
                            move |this, w, cx| this.push_branch(&a, false, w, cx),
                        ))
                        .item(item(
                            &view,
                            tr("强制推送（--force-with-lease）…"),
                            move |this, w, cx| this.push_branch(&b, true, w, cx),
                        ))
                        .separator()
                        .item(item(&view, tr("改名…"), move |this, w, cx| {
                            this.rename_branch_dialog(c.clone(), w, cx)
                        }));
                    if !branch.current {
                        menu = menu.item(item(&view, tr("删除…"), move |this, w, cx| {
                            this.delete_branch(d.clone(), w, cx)
                        }));
                    }
                    menu = scope_items(
                        menu.separator(),
                        &view,
                        format!("refs/heads/{name}"),
                        name.clone(),
                        only,
                        // 当前分支隐藏不了：HEAD 总是列出来的
                        (!branch.current).then_some(hidden),
                    );
                    menu.separator().item(copy_item(tr("复制分支名"), name))
                }))
            }
            SideItem::Remote(i) => {
                let name = self.refs.remotes.get(i)?.name.clone();
                let full = format!("refs/remotes/{name}");
                let (only, hidden) = (self.is_only(&full), self.hidden_refs(cx).contains(&full));
                Some(Box::new(move |menu, _, _| {
                    let (a, b, c, d) = (name.clone(), name.clone(), name.clone(), name.clone());
                    let e = name.clone();
                    let menu = menu
                        .item(item(
                            &view,
                            tr("检出为本地分支"),
                            move |this, w, cx| this.checkout_remote(&a, w, cx),
                        ))
                        .item(item(
                            &view,
                            tf!("设为 {cur} 的上游", cur),
                            move |this, w, cx| this.set_upstream(e.clone(), w, cx),
                        ))
                        .separator()
                        .item(item(
                            &view,
                            tf!("合并 {name} 到 {cur}…", name, cur),
                            move |this, w, cx| this.merge(b.clone(), w, cx),
                        ))
                        .item(item(
                            &view,
                            tf!("把 {cur} 变基到 {name}…", cur, name),
                            move |this, w, cx| this.rebase_onto(c.clone(), c.clone(), w, cx),
                        ))
                        .separator()
                        .item(item(
                            &view,
                            tr("删除远程分支…"),
                            move |this, w, cx| this.delete_remote_branch(d.clone(), w, cx),
                        ));
                    scope_items(
                        menu.separator(),
                        &view,
                        full.clone(),
                        name.clone(),
                        only,
                        Some(hidden),
                    )
                    .separator()
                    .item(copy_item(tr("复制分支名"), name.clone()))
                }))
            }
            SideItem::Tag(i) => {
                let name = self.refs.tags.get(i)?.name.clone();
                let remote = self.default_remote();
                let full = format!("refs/tags/{name}");
                let (only, hidden) = (self.is_only(&full), self.hidden_refs(cx).contains(&full));
                Some(Box::new(move |menu, _, _| {
                    let (a, b, c, d) = (name.clone(), name.clone(), name.clone(), name.clone());
                    let menu = menu
                        .item(item(
                            &view,
                            tr("检出这个标签（分离 HEAD）"),
                            move |this, w, cx| this.checkout_detached(a.clone(), w, cx),
                        ))
                        .separator()
                        .item(item(
                            &view,
                            tf!("推送到 {remote}", remote),
                            move |this, w, cx| this.push_tag(b.clone(), w, cx),
                        ))
                        .item(item(
                            &view,
                            tr("删除本地标签…"),
                            move |this, w, cx| this.delete_tag(c.clone(), false, w, cx),
                        ))
                        .item(item(
                            &view,
                            tf!("从 {remote} 删除…", remote),
                            move |this, w, cx| this.delete_tag(d.clone(), true, w, cx),
                        ));
                    scope_items(
                        menu.separator(),
                        &view,
                        full.clone(),
                        name.clone(),
                        only,
                        Some(hidden),
                    )
                    .separator()
                    .item(copy_item(tr("复制标签名"), name.clone()))
                }))
            }
            SideItem::Header(SideSection::Remotes) => Some(Box::new(move |menu, _, _| {
                menu.item(item(&view, tr("添加远程…"), |this, w, cx| {
                    this.add_remote_dialog(w, cx)
                }))
                .item(item(&view, tr("获取全部远程"), |this, w, cx| {
                    this.fetch(w, cx)
                }))
            })),
            // 一个远程：按配置里的远程列（刚添加、还没获取到分支的也在），对它获取、改地址、删除。
            // 配置里已经没有、只剩下远程分支的名字里还带着的，只能复制名字
            SideItem::RemoteRoot(i) => {
                let remote = self.side_remotes().get(i)?.clone();
                let configured = self.remote_names.contains(&remote);
                Some(Box::new(move |menu, _, _| {
                    let (a, b, c) = (remote.clone(), remote.clone(), remote.clone());
                    let menu = if configured {
                        menu.item(item(
                            &view,
                            tf!("从 {remote} 获取", remote),
                            move |this, w, cx| this.fetch_remote(c.clone(), w, cx),
                        ))
                        .item(item(
                            &view,
                            tf!("改 {remote} 的地址…", remote),
                            move |this, w, cx| this.set_remote_url_dialog(a.clone(), w, cx),
                        ))
                        .item(item(
                            &view,
                            tf!("删除远程 {remote}…", remote),
                            move |this, w, cx| this.remove_remote(b.clone(), w, cx),
                        ))
                        .separator()
                    } else {
                        menu
                    };
                    menu.item(copy_item(tr("复制远程的名字"), remote.clone()))
                }))
            }
            SideItem::Header(SideSection::Submodules) => Some(Box::new(move |menu, _, _| {
                menu.item(item(&view, tr("更新全部子模块"), |this, w, cx| {
                    this.update_submodules(None, w, cx)
                }))
            })),
            SideItem::Submodule(i) => {
                let path = self.submodules.get(i)?.clone();
                Some(Box::new(move |menu, _, _| {
                    let (a, b) = (path.clone(), path.clone());
                    menu.item(item(
                        &view,
                        tr("在新标签页里打开"),
                        move |this, _, cx| this.open_submodule(i, cx),
                    ))
                    .item(item(
                        &view,
                        tr("更新这个子模块"),
                        move |this, w, cx| this.update_submodules(Some(a.clone()), w, cx),
                    ))
                    .separator()
                    .item(copy_item(tr("复制路径"), b.clone()))
                }))
            }
            SideItem::Stash(i) => {
                let stash = self.refs.stashes.get(i)?.clone();
                Some(Box::new(move |menu, _, _| {
                    let (a, b) = (stash.selector.clone(), stash.selector.clone());
                    let c = (stash.selector.clone(), stash.message.clone());
                    menu.item(item(
                        &view,
                        tr("应用（贮藏留着）"),
                        move |this, w, cx| this.stash_apply(a.clone(), w, cx),
                    ))
                    .item(item(
                        &view,
                        tr("弹出（应用并删除）"),
                        move |this, w, cx| this.stash_pop(b.clone(), w, cx),
                    ))
                    .separator()
                    .item(item(&view, tr("删除…"), move |this, w, cx| {
                        this.stash_drop(c.0.clone(), c.1.clone(), w, cx)
                    }))
                }))
            }
            _ => None,
        }
    }

    /// 本地修改里一个文件的菜单。
    pub(super) fn file_menu(&self, area: Area, entry: &Entry, cx: &mut Context<Self>) -> MenuFn {
        let view = cx.entity().downgrade();
        let entry = entry.clone();
        let local = self.entry.server.is_none();
        let full_path = self.entry.path.clone();
        let destination = self
            .entry
            .server
            .as_deref()
            .and_then(|id| self.app.read(cx).server(id))
            .map(|server| server.destination.clone());
        let conflicted = entry.unstaged == Some(Kind::Conflicted);
        let (ours, theirs) = self.side_labels();
        Box::new(move |mut menu, _, _| {
            match area {
                Area::Unstaged => {
                    if conflicted {
                        let (a, b) = (entry.path.clone(), entry.path.clone());
                        let path = entry.path.clone();
                        menu = menu
                            .item(item(
                                &view,
                                tr("在合并编辑器里解决…"),
                                move |this, w, cx| this.open_merge_editor(path.clone(), w, cx),
                            ))
                            .separator();
                        menu = menu
                            .item(item(
                                &view,
                                tf!("整个文件用{ours}（ours）…", ours),
                                move |this, w, cx| this.resolve_with(a.clone(), "--ours", w, cx),
                            ))
                            .item(item(
                                &view,
                                tf!("整个文件用{theirs}（theirs）…", theirs),
                                move |this, w, cx| this.resolve_with(b.clone(), "--theirs", w, cx),
                            ))
                            .separator();
                    }
                    let (a, b) = (entry.clone(), entry.clone());
                    menu = menu.item(item(
                        &view,
                        if conflicted {
                            tr("标记为已解决")
                        } else {
                            tr("暂存")
                        },
                        move |this, w, cx| this.stage_file(&a, w, cx),
                    ));
                    // 有冲突的文件丢弃不了（git 会报错），要么取一边，要么中止整个操作
                    if !conflicted {
                        menu = menu.item(item(&view, tr("丢弃改动…"), move |this, w, cx| {
                            this.discard_file(&b, w, cx)
                        }));
                    }
                    if entry.unstaged == Some(Kind::Untracked) {
                        let path = entry.path.clone();
                        menu =
                            menu.item(item(&view, tr("加到 .gitignore"), move |this, w, cx| {
                                this.ignore_path(path.clone(), w, cx)
                            }));
                    }
                }
                Area::Staged => {
                    let a = entry.clone();
                    menu = menu.item(item(&view, tr("取消暂存"), move |this, w, cx| {
                        this.unstage_file(&a, w, cx)
                    }));
                }
            }
            menu = menu.separator();
            // 交给编辑器：解决冲突、细改都在那边做
            let exists = entry.unstaged != Some(Kind::Deleted);
            if exists {
                match &destination {
                    Some(dest) => {
                        let (dest, file) = (dest.clone(), format!("{full_path}/{}", entry.path));
                        menu = menu.item(
                            PopupMenuItem::new(tr("在 VS Code 里打开（Remote-SSH）")).on_click(
                                move |_, window, cx| {
                                    let result = crate::shell::open_vscode_remote(&dest, &file);
                                    crate::shell::report(result, window, cx);
                                },
                            ),
                        );
                    }
                    None => {
                        let file = format!("{full_path}/{}", entry.path);
                        let file2 = file.clone();
                        menu = menu
                            .item(PopupMenuItem::new(tr("用默认程序打开")).on_click(
                                move |_, window, cx| {
                                    crate::shell::report(crate::shell::open_file(&file), window, cx)
                                },
                            ))
                            .item(PopupMenuItem::new(tr("在 VS Code 里打开")).on_click(
                                move |_, window, cx| {
                                    let result = crate::shell::open_vscode(&file2);
                                    crate::shell::report(result, window, cx);
                                },
                            ));
                    }
                }
                menu = menu.separator();
            }
            // 没进过 HEAD 的文件（未跟踪的、刚新建并暂存的）还没有历史可看
            if entry.unstaged != Some(Kind::Untracked) && entry.staged != Some(Kind::Added) {
                let path = entry.path.clone();
                let blame = entry.path.clone();
                menu = menu
                    .item(item(&view, tr("文件历史"), move |this, w, cx| {
                        this.show_file_history(path.clone(), w, cx)
                    }))
                    .item(item(
                        &view,
                        tr("逐行追溯（blame）"),
                        move |this, _, cx| this.show_blame(blame.clone(), cx),
                    ));
            }
            if local {
                let file = format!("{full_path}/{}", entry.path);
                menu = menu.item(PopupMenuItem::new(tr("在资源管理器里显示")).on_click(
                    move |_, window, cx| {
                        crate::shell::report(crate::shell::reveal_in_explorer(&file), window, cx)
                    },
                ));
            }
            menu.item(copy_item(tr("复制路径"), entry.path.clone()))
        })
    }

    /// 提交详情里一个文件的菜单：看它的历史、逐行追溯。
    pub(super) fn tracked_file_menu(&self, path: String, cx: &mut Context<Self>) -> MenuFn {
        let view = cx.entity().downgrade();
        Box::new(move |menu, _, _| {
            let (a, b) = (path.clone(), path.clone());
            menu.item(item(&view, tr("文件历史"), move |this, w, cx| {
                this.show_file_history(a.clone(), w, cx)
            }))
            .item(item(
                &view,
                tr("逐行追溯（blame）"),
                move |this, _, cx| this.show_blame(b.clone(), cx),
            ))
            .separator()
            .item(copy_item(tr("复制路径"), path.clone()))
        })
    }

    /// 多选了几个文件时的菜单：对选中的全部文件操作。
    pub(super) fn multi_file_menu(&self, area: Area, cx: &mut Context<Self>) -> MenuFn {
        let view = cx.entity().downgrade();
        let entries = self
            .changes_selected_entries()
            .map(|(_, entries)| entries)
            .unwrap_or_default();
        let count = entries.len();
        Box::new(move |menu, _, _| {
            let (a, b) = (entries.clone(), entries.clone());
            match area {
                Area::Unstaged => menu
                    .item(item(
                        &view,
                        tf!("暂存选中的 {count} 个文件", count),
                        move |this, w, cx| this.stage_files(a.clone(), w, cx),
                    ))
                    .item(item(
                        &view,
                        tf!("丢弃选中的 {count} 个文件的改动…", count),
                        move |this, w, cx| this.discard_files(b.clone(), w, cx),
                    )),
                Area::Staged => menu.item(item(
                    &view,
                    tf!("取消暂存选中的 {count} 个文件", count),
                    move |this, w, cx| this.unstage_files(a.clone(), w, cx),
                )),
            }
        })
    }

    /// 工具栏上「获取」「拉取」的右键菜单：不常用的选项。
    pub(super) fn fetch_menu(&self, cx: &mut Context<Self>) -> MenuFn {
        let view = cx.entity().downgrade();
        Box::new(move |menu, _, _| {
            menu.item(item(
                &view,
                tr("获取，并清理远程已删除的分支（--prune）"),
                |this, w, cx| this.fetch_prune(w, cx),
            ))
        })
    }

    pub(super) fn pull_menu(&self, cx: &mut Context<Self>) -> MenuFn {
        let view = cx.entity().downgrade();
        Box::new(move |menu, _, _| {
            menu.item(item(
                &view,
                tr("拉取，用变基代替合并（--rebase）"),
                |this, w, cx| this.pull(true, w, cx),
            ))
        })
    }

    pub(super) fn push_menu(&self, cx: &mut Context<Self>) -> MenuFn {
        let view = cx.entity().downgrade();
        Box::new(move |menu, _, _| {
            menu.item(item(
                &view,
                tr("强制推送（--force-with-lease）…"),
                |this, w, cx| {
                    let current = this.refs.branches.iter().find(|b| b.current).cloned();
                    match current {
                        Some(branch) => this.push_branch(&branch, true, w, cx),
                        None => w.push_notification(
                            Notification::warning(tr("现在是分离的 HEAD，先切换到一个分支再推送")),
                            cx,
                        ),
                    }
                },
            ))
            .item(item(
                &view,
                tr("推送所有标签（--tags）"),
                |this, w, cx| {
                    let remote = this.default_remote();
                    let push = vec!["push".into(), "--progress".into(), remote, "--tags".into()];
                    this.run_net_op(tr("推送"), push, w, cx, |_, _, w, cx| {
                        w.push_notification(Notification::success(tr("标签已推送")), cx);
                    });
                },
            ))
        })
    }

    // ---------- 操作 ----------

    pub(super) fn checkout_detached(
        &mut self,
        rev: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_local_changes(
            tf!("检出 {rev}", rev = short(&rev)),
            tr("带着改动切过去"),
            window,
            cx,
            move |this, stash, window, cx| {
                let rev = rev.clone();
                let checkout = vec![
                    "checkout".into(),
                    "-q".into(),
                    "--detach".into(),
                    rev.clone(),
                ];
                this.run_stashed(
                    tr("检出"),
                    vec![Step::local(checkout)],
                    stash,
                    window,
                    cx,
                    move |_, _, w, cx| {
                        let text = tf!("已检出 {rev}，现在是分离的 HEAD", rev = short(&rev));
                        w.push_notification(Notification::success(text), cx);
                    },
                );
            },
        );
    }

    fn cherry_pick(
        &mut self,
        id: String,
        is_merge: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut pick = args(&["cherry-pick"]);
        if is_merge {
            // 合并提交要说明相对哪个父提交：取第一个，和界面上显示的改动一致
            pick.extend(args(&["-m", "1"]));
        }
        pick.push(id.clone());
        self.run_op(tr("挑拣"), pick, None, window, cx, move |_, _, w, cx| {
            w.push_notification(
                Notification::success(tf!("已挑拣 {id}", id = short(&id))),
                cx,
            );
        });
    }

    fn revert_commit(
        &mut self,
        id: String,
        is_merge: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut revert = args(&["revert", "--no-edit"]);
        if is_merge {
            revert.extend(args(&["-m", "1"]));
        }
        revert.push(id.clone());
        self.run_op(
            tr("还原"),
            revert,
            None,
            window,
            cx,
            move |_, _, w, cx| {
                let text = tf!("已新建一个提交，还原了 {id}", id = short(&id));
                w.push_notification(Notification::success(text), cx);
            },
        );
    }

    fn reset_to(
        &mut self,
        id: String,
        flag: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cur = self.current_label();
        let is_head = self.status.as_ref().and_then(|s| s.head.as_deref()) == Some(&*id);
        let moved = if is_head {
            String::new()
        } else {
            tf!(
                "{cur} 会指向 {id}，它后面的提交从这个分支上消失（还能从 reflog 找回）。",
                cur,
                id = short(&id)
            )
        };
        let reset = vec!["reset".into(), flag.into(), id.clone()];
        let run = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            let (id, cur) = (id.clone(), cur.clone());
            this.run_op(
                tr("重置"),
                reset.clone(),
                None,
                window,
                cx,
                move |_, _, w, cx| {
                    let text = tf!("{cur} 已重置到 {id}", cur, id = short(&id));
                    w.push_notification(Notification::success(text), cx);
                },
            );
        };
        match flag {
            "--hard" => {
                let description = tf!("{moved}工作区和暂存区里的改动全部丢弃，不能撤销。", moved);
                self.confirm(
                    tr("重置并丢弃所有改动？"),
                    description,
                    tr("重置"),
                    window,
                    cx,
                    run,
                );
            }
            "--soft" => {
                let description = tf!("{moved}这些提交的改动留在暂存区，工作区不动。", moved);
                self.confirm_plain(
                    tr("重置（soft）？"),
                    description,
                    tr("重置"),
                    window,
                    cx,
                    run,
                );
            }
            _ => {
                let description = tf!("{moved}改动留在工作区，暂存区清空。", moved);
                self.confirm_plain(
                    tr("重置（mixed）？"),
                    description,
                    tr("重置"),
                    window,
                    cx,
                    run,
                );
            }
        }
    }

    /// 合并：先问怎么合（默认、总是生成合并提交、只快进、压缩成一份改动），要不要先不提交。
    fn merge(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let cur = self.current_label();
        let choose = super::dialogs::Choose {
            title: tf!("合并 {name} 到 {cur}", name = &name, cur).into(),
            description: SharedString::default(),
            options: MERGE_MODES
                .iter()
                .map(|mode| (tr(mode.label).into(), tr(mode.detail).into()))
                .collect(),
            selected: 0,
            checks: vec![(tr("先不提交，合并好停下来检查（--no-commit）"), false)],
            ok: tr("合并"),
        };
        self.choose(
            choose,
            window,
            cx,
            move |this, picked, checks, window, cx| {
                let mode = MERGE_MODES.get(picked).unwrap_or(&MERGE_MODES[0]);
                let no_commit = checks.first().copied().unwrap_or(false);
                let merge = merge_args(&name, mode.flag, no_commit);
                let (name, squash) = (name.clone(), mode.flag == Some("--squash"));
                this.run_op(
                    tr("合并"),
                    merge,
                    None,
                    window,
                    cx,
                    move |this, out, w, cx| {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        let first = stdout.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                        let note = if squash {
                            this.show_changes(cx);
                            Notification::success(tr(
                                "对方的改动已合成一份放进暂存区。在「本地修改」里写说明后提交。",
                            ))
                            .title(tf!("已把 {name} 的改动压缩进来", name))
                        } else if no_commit && !stdout.contains("Fast-forward") {
                            this.show_changes(cx);
                            Notification::success(tr(
                                "还没提交：检查后点上面的「提交合并」，不要了点「中止」。",
                            ))
                            .title(tf!("{name} 合并好了，等你检查", name))
                        } else {
                            Notification::success(first.to_string())
                                .title(tf!("已合并 {name}", name))
                        };
                        w.push_notification(note, cx);
                    },
                );
            },
        );
    }

    /// 把当前分支变基到 `target`；`shown` 是给用户看的名字。
    fn rebase_onto(
        &mut self,
        target: String,
        shown: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cur = self.current_label();
        let description = tf!(
            "{cur} 独有的提交会接到 {shown} 后面重新生成（哈希会变）。已推送的分支变基后要强制推送。",
            cur,
            shown
        );
        self.confirm_plain(
            tf!("把 {cur} 变基到 {shown}？", cur, shown),
            description,
            tr("变基"),
            window,
            cx,
            move |this, window, cx| {
                let shown = shown.clone();
                let rebase = vec!["rebase".into(), target.clone()];
                this.run_op(
                    tr("变基"),
                    rebase,
                    None,
                    window,
                    cx,
                    move |_, _, w, cx| {
                        w.push_notification(
                            Notification::success(tf!("已变基到 {shown}", shown)),
                            cx,
                        );
                    },
                );
            },
        );
    }

    /// 推送一个本地分支：有上游就推到上游，没有就推到默认远程的同名分支并设为上游。
    fn push_branch(
        &mut self,
        branch: &fr_core::refs::Branch,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = branch.name.clone();
        let mut push = args(&["push", "--progress"]);
        if force {
            push.push("--force-with-lease".into());
        }
        let to = match branch.upstream.as_deref().and_then(|u| u.split_once('/')) {
            Some((remote, remote_branch)) => {
                push.push(remote.into());
                push.push(format!("{name}:{remote_branch}"));
                format!("{remote}/{remote_branch}")
            }
            None => {
                let remote = self.default_remote();
                push.extend(["--set-upstream".into(), remote.clone(), name.clone()]);
                format!("{remote}/{name}")
            }
        };
        let description = tf!(
            "{to} 上的提交会被本地 {name} 覆盖。{to} 在上次获取后有新提交时不会推送（--force-with-lease）。",
            to,
            name
        );
        let run = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            let text = tf!("{name} 已推送到 {to}", name, to);
            this.run_net_op(
                tr("推送"),
                push.clone(),
                window,
                cx,
                move |_, _, w, cx| {
                    w.push_notification(Notification::success(text), cx);
                },
            );
        };
        if force {
            self.confirm(
                tr("强制推送？"),
                description,
                tr("强制推送"),
                window,
                cx,
                run,
            );
        } else {
            run(self, window, cx);
        }
    }

    fn rename_branch_dialog(&mut self, old: String, window: &mut Window, cx: &mut Context<Self>) {
        let ask = Ask {
            title: tf!("给 {old} 改名", old).into(),
            placeholder: tr("新的分支名").into(),
            value: old.clone(),
            hint: tr("只改本地分支的名字，远程上的分支不变。").into(),
            ok: tr("改名"),
            validate: Some(Box::new({
                let old = old.clone();
                move |this, answer| {
                    if answer.text == old {
                        return None;
                    }
                    ref_name_error(&answer.text).or_else(|| this.branch_exists_error(&answer.text))
                }
            })),
            ..Default::default()
        };
        self.ask(ask, window, cx, move |this, answer, window, cx| {
            let new = answer.text;
            if new == old {
                return;
            }
            let rename = vec!["branch".into(), "-m".into(), old.clone(), new.clone()];
            this.run_op(
                tr("改名"),
                rename,
                None,
                window,
                cx,
                move |_, _, w, cx| {
                    w.push_notification(Notification::success(tf!("已改名为 {new}", new)), cx);
                },
            );
        });
    }

    fn delete_remote_branch(&mut self, full: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some((remote, branch)) = full.split_once('/') else {
            return;
        };
        let delete = vec![
            "push".into(),
            "--progress".into(),
            remote.into(),
            "--delete".into(),
            branch.into(),
        ];
        let description = tf!(
            "{remote} 上的分支 {branch} 会被删除，其他人也看不到了。本地分支不受影响。",
            remote,
            branch
        );
        self.confirm(
            tr("删除远程分支？"),
            description,
            tr("删除"),
            window,
            cx,
            move |this, window, cx| {
                let full = full.clone();
                this.run_net_op(
                    tr("删除远程分支"),
                    delete.clone(),
                    window,
                    cx,
                    move |_, _, w, cx| {
                        w.push_notification(Notification::success(tf!("已删除 {full}", full)), cx);
                    },
                );
            },
        );
    }

    fn new_tag_dialog(
        &mut self,
        id: String,
        subject: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let remote = self.default_remote();
        let ask = Ask {
            title: tr("新建标签").into(),
            placeholder: tr("标签名，比如 v1.0.0").into(),
            value: String::new(),
            hint: tf!("打在提交 {id} {subject} 上", id = short(&id), subject).into(),
            ok: tr("创建"),
            checks: vec![(tr("创建后推送到远程"), false)],
            validate: Some(Box::new(|this, answer| {
                ref_name_error(&answer.text).or_else(|| {
                    this.refs
                        .tags
                        .iter()
                        .any(|t| t.name == answer.text)
                        .then(|| tf!("已经有叫 {text} 的标签了", text = answer.text))
                })
            })),
            ..Default::default()
        };
        self.ask(ask, window, cx, move |this, answer, window, cx| {
            let name = answer.text;
            let push = answer.checks.first().copied().unwrap_or(false);
            let mut steps = vec![Step::local(vec!["tag".into(), name.clone(), id.clone()])];
            if push {
                steps.push(Step::net(vec![
                    "push".into(),
                    "--progress".into(),
                    remote.clone(),
                    format!("refs/tags/{name}"),
                ]));
            }
            this.run_seq(tr("新建标签"), steps, window, cx, move |_, _, w, cx| {
                let text = if push {
                    tf!("已创建标签 {name}，并推送到远程", name)
                } else {
                    tf!("已创建标签 {name}", name)
                };
                w.push_notification(Notification::success(text), cx);
            });
        });
    }

    fn push_tag(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let remote = self.default_remote();
        let push = vec![
            "push".into(),
            "--progress".into(),
            remote.clone(),
            format!("refs/tags/{name}"),
        ];
        self.run_net_op(tr("推送标签"), push, window, cx, move |_, _, w, cx| {
            let text = tf!("标签 {name} 已推送到 {remote}", name, remote);
            w.push_notification(Notification::success(text), cx);
        });
    }

    /// 删除标签：`on_remote` 为真时删的是远程上的，本地的留着。
    fn delete_tag(
        &mut self,
        name: String,
        on_remote: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let remote = self.default_remote();
        let (title, description) = if on_remote {
            (
                tr("从远程删除标签？"),
                tf!(
                    "{remote} 上的标签 {name} 会被删除，本地的留着。",
                    remote,
                    name
                ),
            )
        } else {
            (
                tr("删除本地标签？"),
                tf!("本地的标签 {name} 会被删除，远程上的不受影响。", name),
            )
        };
        self.confirm(
            title,
            description,
            tr("删除"),
            window,
            cx,
            move |this, window, cx| {
                let name = name.clone();
                if on_remote {
                    let delete = vec![
                        "push".into(),
                        "--progress".into(),
                        remote.clone(),
                        "--delete".into(),
                        format!("refs/tags/{name}"),
                    ];
                    this.run_net_op(
                        tr("删除标签"),
                        delete,
                        window,
                        cx,
                        move |_, _, w, cx| {
                            w.push_notification(
                                Notification::success(tf!("已从远程删除 {name}", name)),
                                cx,
                            );
                        },
                    );
                } else {
                    let delete = vec!["tag".into(), "-d".into(), name.clone()];
                    this.run_op(
                        tr("删除标签"),
                        delete,
                        None,
                        window,
                        cx,
                        move |_, _, w, cx| {
                            w.push_notification(
                                Notification::success(tf!("已删除标签 {name}", name)),
                                cx,
                            );
                        },
                    );
                }
            },
        );
    }

    fn fetch_prune(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fetch = args(&["fetch", "--all", "--prune", "--progress"]);
        self.run_net_op(tr("获取"), fetch, window, cx, |_, _, w, cx| {
            w.push_notification(
                Notification::success(tr("获取完成，已清理远程删除的分支")),
                cx,
            );
        });
    }

    fn fetch_remote(&mut self, remote: String, window: &mut Window, cx: &mut Context<Self>) {
        let fetch = vec!["fetch".into(), "--progress".into(), remote.clone()];
        self.run_net_op(tr("获取"), fetch, window, cx, move |_, _, w, cx| {
            w.push_notification(Notification::success(tf!("已从 {remote} 获取", remote)), cx);
        });
    }

    /// 把当前分支的上游设成这个远程分支。
    fn set_upstream(&mut self, remote: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(branch) = self.status.as_ref().and_then(|s| s.branch.clone()) else {
            window.push_notification(
                Notification::warning(tr("现在是分离的 HEAD，先切换到一个分支")),
                cx,
            );
            return;
        };
        let set = vec!["branch".into(), format!("--set-upstream-to={remote}")];
        self.run_op(
            tr("设置上游"),
            set,
            None,
            window,
            cx,
            move |_, _, w, cx| {
                let text = tf!("{branch} 的上游已设为 {remote}", branch, remote);
                w.push_notification(Notification::success(text), cx);
            },
        );
    }

    /// 有冲突的文件整个取一边，然后标记为已解决。会盖掉另一边和手工改过的内容，所以先确认。
    fn resolve_with(
        &mut self,
        path: String,
        side: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (ours, theirs) = self.side_labels();
        let which = if side == "--ours" { ours } else { theirs };
        let description = tf!(
            "{path} 的每处冲突都取这一边，文件标记为已解决。另一边的改动和你手工改过的内容会被覆盖。",
            path
        );
        self.confirm(
            tf!("整个文件用{which}？", which),
            description,
            tr("覆盖并标记为已解决"),
            window,
            cx,
            move |this, window, cx| {
                let path = path.clone();
                let steps = vec![
                    Step::local(vec![
                        "checkout".into(),
                        side.into(),
                        "--".into(),
                        path.clone(),
                    ]),
                    Step::local(vec!["add".into(), "--".into(), path.clone()]),
                ];
                this.run_seq(tr("解决冲突"), steps, window, cx, move |_, _, w, cx| {
                    let text = tf!("{path} 已用{which}，并标记为已解决", path, which);
                    w.push_notification(Notification::success(text), cx);
                });
            },
        );
    }

    fn ignore_path(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let ignore = fr_core::ignore::args(&path);
        self.run_op(
            tr("忽略"),
            ignore,
            None,
            window,
            cx,
            move |_, _, w, cx| {
                w.push_notification(
                    Notification::success(tf!("{path} 已加到 .gitignore", path)),
                    cx,
                );
            },
        );
    }

    // ---------- 远程、子模块 ----------

    fn add_remote_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ask = Ask {
            title: tr("添加远程").into(),
            placeholder: tr("名字，比如 upstream").into(),
            second: Some(tr("地址，比如 https://example.com/team/project.git").into()),
            hint: tr("添加后会获取一次。获取失败时远程已经加上，可在右键菜单里改地址或删除。")
                .into(),
            ok: tr("添加"),
            validate: Some(Box::new(|this, answer| {
                if answer.second.is_empty() {
                    return Some(tr("还没有填地址").into());
                }
                if answer.text.contains(char::is_whitespace) || answer.text.contains('/') {
                    return Some(tr("远程的名字里不能有空格和斜杠").into());
                }
                this.remote_names
                    .contains(&answer.text)
                    .then(|| tf!("已经有叫 {text} 的远程了", text = answer.text))
            })),
            ..Default::default()
        };
        self.ask(ask, window, cx, |this, answer, window, cx| {
            let (name, url) = (answer.text, answer.second);
            let steps = vec![
                Step::local(vec!["remote".into(), "add".into(), name.clone(), url]),
                Step::net(vec!["fetch".into(), "--progress".into(), name.clone()]),
            ];
            this.run_seq(tr("添加远程"), steps, window, cx, move |_, _, w, cx| {
                w.push_notification(Notification::success(tf!("已添加远程 {name}", name)), cx);
            });
        });
    }

    fn set_remote_url_dialog(
        &mut self,
        remote: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ask = Ask {
            title: tf!("改 {remote} 的地址", remote).into(),
            placeholder: tr("新的地址").into(),
            value: String::new(),
            hint: tr("获取和推送都改用这个地址。").into(),
            ok: tr("修改"),
            ..Default::default()
        };
        self.ask(ask, window, cx, move |this, answer, window, cx| {
            let url = answer.text;
            let remote = remote.clone();
            let set = vec!["remote".into(), "set-url".into(), remote.clone(), url];
            this.run_op(
                tr("修改远程"),
                set,
                None,
                window,
                cx,
                move |_, _, w, cx| {
                    w.push_notification(
                        Notification::success(tf!("{remote} 的地址已修改", remote)),
                        cx,
                    );
                },
            );
        });
    }

    fn remove_remote(&mut self, remote: String, window: &mut Window, cx: &mut Context<Self>) {
        let description = tf!(
            "只删本地记录的 {remote} 和它的远程分支，远程仓库本身不受影响。",
            remote
        );
        self.confirm(
            tr("删除远程？"),
            description,
            tr("删除"),
            window,
            cx,
            move |this, window, cx| {
                let remote = remote.clone();
                let remove = vec!["remote".into(), "remove".into(), remote.clone()];
                this.run_op(
                    tr("删除远程"),
                    remove,
                    None,
                    window,
                    cx,
                    move |_, _, w, cx| {
                        w.push_notification(
                            Notification::success(tf!("已删除远程 {remote}", remote)),
                            cx,
                        );
                    },
                );
            },
        );
    }

    /// 更新子模块（没检出的会检出）；`path` 为空时更新全部。
    fn update_submodules(
        &mut self,
        path: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut update = args(&["submodule", "update", "--init", "--recursive", "--progress"]);
        if let Some(path) = path {
            update.extend(["--".into(), path]);
        }
        self.run_net_op(tr("更新子模块"), update, window, cx, |_, _, w, cx| {
            w.push_notification(Notification::success(tr("子模块已更新")), cx);
        });
    }

    /// 在新标签页里打开子模块：它就是仓库目录下的另一个仓库。
    pub(super) fn open_submodule(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(path) = self.submodules.get(ix) else {
            return;
        };
        let full = match &self.entry.server {
            Some(_) => format!("{}/{path}", self.entry.path),
            None => format!("{}\\{}", self.entry.path, path.replace('/', "\\")),
        };
        cx.emit(RepoEvent::Open(RepoEntry {
            server: self.entry.server.clone(),
            path: full,
        }));
    }

    // ---------- 进行到一半的操作 ----------

    /// 继续合并、变基、挑拣、还原。
    pub(super) fn continue_in_progress(
        &mut self,
        state: InProgress,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 合并没有 --continue 以外的事要做：冲突解决完，提交就是了。
        // 提交说明框里写了内容就用它，没写用 git 准备好的合并说明
        let message = self.changes.message_text(cx);
        let (go, stdin) = match state {
            InProgress::Merge if message.is_empty() => (args(&["commit", "--no-edit"]), None),
            InProgress::Merge => (args(&["commit", "-F", "-"]), Some(message.into_bytes())),
            other => (args(&[other.command(), "--continue"]), None),
        };
        let used_message = stdin.is_some();
        self.run_op(
            tf!("继续{name}", name = state.name()),
            go,
            stdin,
            window,
            cx,
            move |this, _, window, cx| {
                if used_message {
                    this.changes.clear_message(window, cx);
                }
                let text = tf!("{name}做完了", name = state.name());
                window.push_notification(Notification::success(text), cx);
            },
        );
    }

    /// 跳过当前这个提交（变基、挑拣、还原）。
    pub(super) fn skip_in_progress(
        &mut self,
        state: InProgress,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let description = match state {
            InProgress::Rebase => tr(
                "跳过正在重放的这个提交：它的改动（包括已解决的冲突）会被丢掉，变基接着处理后面的。",
            ),
            _ => tr("放弃正在处理的这个提交，已解决的冲突不保留。"),
        };
        self.confirm(
            tr("跳过这个提交？"),
            description,
            tr("跳过"),
            window,
            cx,
            move |this, window, cx| {
                let skip = args(&[state.command(), "--skip"]);
                this.run_op(tr("跳过"), skip, None, window, cx, |_, _, window, cx| {
                    window.push_notification(Notification::success(tr("已跳过这个提交")), cx);
                });
            },
        );
    }

    pub(super) fn abort_in_progress(
        &mut self,
        state: InProgress,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let description = tf!(
            "回到{name}之前的状态，已解决的冲突不保留。",
            name = state.name()
        );
        self.confirm(
            tf!("中止{name}？", name = state.name()),
            description,
            tr("中止"),
            window,
            cx,
            move |this, window, cx| {
                let abort = args(&[state.command(), "--abort"]);
                this.run_op(
                    tf!("中止{name}", name = state.name()),
                    abort,
                    None,
                    window,
                    cx,
                    move |_, _, window, cx| {
                        let text = tf!("已中止{name}，回到了开始之前的状态", name = state.name());
                        window.push_notification(Notification::success(text), cx);
                    },
                );
            },
        );
    }
}
