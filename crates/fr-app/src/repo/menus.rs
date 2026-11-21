//! 右键菜单，以及从菜单里发起的操作：合并、变基、挑拣、还原、重置、标签、分支改名、
//! 删除远程分支、解决冲突时取哪一边。
//!
//! 菜单在点右键时才构建；构建函数里只带要用到的名字和哈希，点了菜单项再回到 [`RepoView`] 上执行。

use fr_channel::GitOutput;
use fr_core::i18n::{tr, tr_key};
use fr_core::state::InProgress;
use fr_core::status::{Entry, Kind};
use fr_core::tf;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::changes::Area;
use super::compare::End;
use super::dialogs::{Ask, Hint, ref_name_error};
use super::history::Scope;
use super::ops::{Step, args, failure};
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
        detail: tr_key("可以快进时快进，否则生成合并提交。"),
        flag: None,
    },
    MergeMode {
        label: tr_key("始终生成合并提交（--no-ff）"),
        detail: tr_key("始终生成合并提交，在历史中保留合并记录。"),
        flag: Some("--no-ff"),
    },
    MergeMode {
        label: tr_key("仅快进（--ff-only）"),
        detail: tr_key("无法快进时不执行任何操作。"),
        flag: Some("--ff-only"),
    },
    MergeMode {
        label: tr_key("压缩合并（--squash）"),
        detail: tr_key("将要合并的改动压缩后放入暂存区，不记录合并关系，需自行填写说明并提交。"),
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
    // 名字以 `-` 开头（取来的标签可以这样）也不会被当成选项
    merge.extend(["--".into(), name.to_string()]);
    merge
}

/// 合并时交给 git 的名字（`full` 是完整引用名）。一般用短名：合并说明里写的就是它（「Merge branch
/// 'feat'」）。git 认短名时先看 `refs/<短名>`，再看标签、本地分支、远程分支；短名会被认成别的引用时
/// 改用完整引用名，不然合并进来的可能是同名的标签。
fn merge_rev(short: &str, full: &str, refs: &fr_core::refs::Refs) -> String {
    let remote = full.starts_with("refs/remotes/");
    let first = short.split('/').next().unwrap_or_default();
    let shadowed = refs.tags.iter().any(|tag| tag.name == short)
        || (remote && refs.branches.iter().any(|branch| branch.name == short))
        // `refs/stash`、`refs/heads/…` 这些排在最前面：叫 stash、heads/x 的分支会被认成它们
        || short == "stash"
        || (short.contains('/') && ["heads", "tags", "remotes", "notes"].contains(&first))
        || short.starts_with('-');
    if shadowed { full } else { short }.to_string()
}

/// `git tag` 的参数：填了说明是带说明的标签（`-a -m`），没填是轻量标签。
fn tag_args(name: &str, message: &str, target: &str) -> Vec<String> {
    match message.is_empty() {
        true => args(&["tag", name, target]),
        false => args(&["tag", "-a", "-m", message, name, target]),
    }
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
        item(view, tr("在提交列表中仅显示此项"), move |this, w, cx| {
            this.set_scope(to.clone(), w, cx)
        })
        .checked(only),
    );
    match hidden {
        Some(hidden) => menu.item(
            item(view, tr("在提交列表中隐藏"), move |this, _, cx| {
                this.toggle_hidden(full.clone(), cx)
            })
            .checked(hidden),
        ),
        None => menu,
    }
}

/// 侧栏里分支、远程分支、标签菜单上的比较两项：和当前分支比（`current` 为空表示这一项就是
/// 当前分支，不列），和本地修改比。点的这一项当基准。
fn compare_items(menu: PopupMenu, view: &View, end: End, current: Option<End>) -> PopupMenu {
    let menu = match current {
        Some(current) => {
            let label = match current {
                End::Head => tr("与 HEAD 比较"),
                _ => tr("与当前分支比较"),
            };
            let base = end.clone();
            menu.item(item(view, label, move |this, w, cx| {
                this.compare_ends(base.clone(), current.clone(), w, cx)
            }))
        }
        None => menu,
    };
    menu.item(item(
        view,
        tr("与本地修改比较"),
        move |this, w, cx| this.compare_ends(end.clone(), End::Worktree, w, cx),
    ))
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
            Some(InProgress::Merge) => (tr("当前分支的版本"), tr("被合并分支的版本")),
            Some(InProgress::Rebase) => (tr("变基目标的版本"), tr("正在重放的提交的版本")),
            Some(InProgress::CherryPick) => (tr("当前分支的版本"), tr("被挑拣的提交的版本")),
            Some(InProgress::Revert) => (tr("当前分支的版本"), tr("还原后的版本")),
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
        // 「选中的提交」：点右键之前选中的那个（点右键会先选中这一行）
        let marked = self.history.marked_for(ix).filter(|marked| *marked != id);
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
                    tr("检出此提交（分离 HEAD）"),
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
                tr("还原此提交（revert）"),
                move |this, w, cx| this.revert_commit(revert.clone(), is_merge, w, cx),
            ));
            {
                let (view2, id2) = (view.clone(), id.clone());
                menu = menu.submenu(
                    tf!("将 {cur} 重置到此提交", cur),
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
                                tr("取消全部暂存，改动保留在工作区（mixed）…"),
                                "--mixed",
                            ))
                        } else {
                            sub.item(mode(tr("保留改动并暂存（soft）…"), "--soft"))
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
                    tf!("将 {cur} 变基到此提交…", cur),
                    move |this, w, cx| {
                        this.rebase_onto(onto.clone(), short(&onto).to_string(), w, cx)
                    },
                ));
                let base = id.clone();
                menu = menu.item(item(
                    &view,
                    tr("交互式变基：整理此后的提交…"),
                    move |this, w, cx| this.interactive_rebase(base.clone(), w, cx),
                ));
            }
            menu = menu.separator();
            if !is_head {
                let base = id.clone();
                menu = menu.item(item(&view, tr("与 HEAD 比较"), move |this, w, cx| {
                    this.compare_ends(End::Commit(base.clone()), End::Head, w, cx)
                }));
            }
            let base = id.clone();
            menu = menu.item(item(
                &view,
                tr("与本地修改比较"),
                move |this, w, cx| {
                    this.compare_ends(End::Commit(base.clone()), End::Worktree, w, cx)
                },
            ));
            if let Some(marked) = marked.clone() {
                let this_one = id.clone();
                menu = menu.item(item(
                    &view,
                    tf!("与选中的提交（{id}）比较", id = short(&marked)),
                    move |this, w, cx| {
                        this.compare_commits(marked.clone(), this_one.clone(), w, cx)
                    },
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
                // 不是当前分支时从哪拉取：它的上游；没设上游就看默认远程上有没有同名的分支
                let pull_from = branch
                    .upstream
                    .clone()
                    .filter(|_| !branch.gone)
                    .or_else(|| {
                        let candidate = format!("{}/{}", self.default_remote(), branch.name);
                        self.refs
                            .remotes
                            .iter()
                            .any(|remote| remote.name == candidate)
                            .then_some(candidate)
                    });
                // 没在哪个工作树里检出的分支，可以另开一个工作树检出它
                let spare = self.has_worktrees_item() && self.tree_holding(&branch.name).is_none();
                let current = (!branch.current).then(|| self.current_end());
                Some(Box::new(move |mut menu, _, _| {
                    let name = branch.name.clone();
                    if !branch.current {
                        let (a, b, c) = (name.clone(), name.clone(), name.clone());
                        let (b_full, c_full) = (full.clone(), full.clone());
                        menu = menu.item(item(
                            &view,
                            tf!("切换到 {name}", name),
                            move |this, w, cx| this.checkout_branch(&a, w, cx),
                        ));
                        if spare {
                            let d = name.clone();
                            menu = menu.item(item(
                                &view,
                                tr("在新工作树中检出…"),
                                move |this, w, cx| this.new_worktree_dialog(Some(d.clone()), w, cx),
                            ));
                        }
                        menu = menu
                            .separator()
                            .item(item(
                                &view,
                                tf!("合并 {name} 到 {cur}…", name, cur),
                                move |this, w, cx| this.merge(b.clone(), b_full.clone(), w, cx),
                            ))
                            .item(item(
                                &view,
                                tf!("将 {cur} 变基到 {name}…", cur, name),
                                move |this, w, cx| {
                                    this.rebase_onto(c_full.clone(), c.clone(), w, cx)
                                },
                            ))
                            .separator();
                    }
                    // 拉取：当前分支和工具栏上的「拉取」一样；别的分支不用切过去，只在能快进时更新
                    if branch.current {
                        menu = menu.item(item(
                            &view,
                            tf!("拉取 {name}", name),
                            |this, w, cx| this.pull(false, w, cx),
                        ));
                    } else if let Some(upstream) = pull_from.clone() {
                        let branch = name.clone();
                        menu = menu.item(item(
                            &view,
                            tf!("拉取 {name}（仅快进，无需切换）", name),
                            move |this, w, cx| {
                                this.fast_forward_branch(branch.clone(), upstream.clone(), w, cx)
                            },
                        ));
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
                        .item(item(&view, tr("重命名…"), move |this, w, cx| {
                            this.rename_branch_dialog(c.clone(), w, cx)
                        }));
                    if !branch.current {
                        menu = menu.item(item(&view, tr("删除…"), move |this, w, cx| {
                            this.delete_branch(d.clone(), w, cx)
                        }));
                    }
                    menu = compare_items(
                        menu.separator(),
                        &view,
                        End::Branch(name.clone()),
                        current.clone(),
                    );
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
                let current = self.current_end();
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
                        .item(item(&view, tf!("合并 {name} 到 {cur}…", name, cur), {
                            let full = full.clone();
                            move |this, w, cx| this.merge(b.clone(), full.clone(), w, cx)
                        }))
                        .item(item(
                            &view,
                            tf!("将 {cur} 变基到 {name}…", cur, name),
                            {
                                let full = full.clone();
                                move |this, w, cx| this.rebase_onto(full.clone(), c.clone(), w, cx)
                            },
                        ))
                        .separator()
                        .item(item(
                            &view,
                            tr("删除远程分支…"),
                            move |this, w, cx| this.delete_remote_branch(d.clone(), w, cx),
                        ));
                    let menu = compare_items(
                        menu.separator(),
                        &view,
                        End::Remote(name.clone()),
                        Some(current.clone()),
                    );
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
                let current = self.current_end();
                Some(Box::new(move |menu, _, _| {
                    let (a, b, c, d) = (name.clone(), name.clone(), name.clone(), name.clone());
                    let menu = menu
                        .item(item(
                            &view,
                            tr("检出此标签（分离 HEAD）"),
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
                    let menu = compare_items(
                        menu.separator(),
                        &view,
                        End::Tag(name.clone()),
                        Some(current.clone()),
                    );
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
            SideItem::Header(SideSection::Branches) => {
                let gone = self.gone_count();
                Some(Box::new(move |menu, _, _| {
                    menu.item(item(&view, tr("新建分支…"), |this, w, cx| {
                        this.new_branch_dialog(w, cx)
                    }))
                    .item(
                        item(
                            &view,
                            tr("清理上游已删除的分支…"),
                            |this, w, cx| this.sweep_gone_branches(w, cx),
                        )
                        .disabled(gone == 0),
                    )
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
                            tf!("修改 {remote} 的地址…", remote),
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
                    menu.item(copy_item(tr("复制远程名称"), remote.clone()))
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
                    let c = path.clone();
                    menu.item(item(
                        &view,
                        tr("在新标签页中打开"),
                        move |this, w, cx| this.open_submodule(c.clone(), w, cx),
                    ))
                    .item(item(
                        &view,
                        tr("更新此子模块"),
                        move |this, w, cx| this.update_submodules(Some(a.clone()), w, cx),
                    ))
                    .separator()
                    .item(copy_item(tr("复制路径"), b.clone()))
                }))
            }
            SideItem::Stash(i) => {
                let target = self.refs.stashes.get(i)?.target.clone();
                Some(Box::new(move |menu, _, _| {
                    // 按提交认，点的时候再换成 stash@{n}：菜单开着时又贮藏了一个（或者删了一个），
                    // 原来的 stash@{n} 就对着别的了
                    let (a, b, c) = (target.clone(), target.clone(), target.clone());
                    menu.item(item(
                        &view,
                        tr("应用（保留贮藏）"),
                        move |this, w, cx| {
                            if let Some(stash) = this.stash_now(&a) {
                                this.stash_apply(stash.selector, w, cx)
                            }
                        },
                    ))
                    .item(item(
                        &view,
                        tr("弹出（应用并删除）"),
                        move |this, w, cx| {
                            if let Some(stash) = this.stash_now(&b) {
                                this.stash_pop(stash.selector, w, cx)
                            }
                        },
                    ))
                    .separator()
                    .item(item(&view, tr("删除…"), move |this, w, cx| {
                        if let Some(stash) = this.stash_now(&c) {
                            this.stash_drop(stash.selector, stash.message, w, cx)
                        }
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
                                tr("在合并编辑器中解决…"),
                                move |this, w, cx| this.open_merge_editor(path.clone(), w, cx),
                            ))
                            .separator();
                        menu = menu
                            .item(item(
                                &view,
                                tf!("整个文件采用{ours}（ours）…", ours),
                                move |this, w, cx| this.resolve_with(a.clone(), "--ours", w, cx),
                            ))
                            .item(item(
                                &view,
                                tf!("整个文件采用{theirs}（theirs）…", theirs),
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
                            menu.item(item(&view, tr("添加到 .gitignore"), move |this, w, cx| {
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
                let path = entry.path.clone();
                menu = menu.item(item(
                    &view,
                    tr("在文件页中打开"),
                    move |this, w, cx| this.show_file(path.clone(), w, cx),
                ));
                match &destination {
                    Some(dest) => {
                        let (dest, file) = (dest.clone(), format!("{full_path}/{}", entry.path));
                        menu = menu.item(
                            PopupMenuItem::new(tr("在 VS Code 中打开（Remote-SSH）")).on_click(
                                move |_, window, cx| {
                                    let result = crate::shell::open_vscode_remote(&dest, &file);
                                    crate::shell::report(result, window, cx);
                                },
                            ),
                        );
                    }
                    // 经别的服务器连的（A>B）：本机连不到它，交不出去
                    None if !local => {}
                    None => {
                        let file = format!("{full_path}/{}", entry.path);
                        let file2 = file.clone();
                        menu = menu
                            .item(PopupMenuItem::new(tr("用默认程序打开")).on_click(
                                move |_, window, cx| {
                                    crate::shell::report(crate::shell::open_file(&file), window, cx)
                                },
                            ))
                            .item(PopupMenuItem::new(tr("在 VS Code 中打开")).on_click(
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
                menu = menu.item(PopupMenuItem::new(tr("在资源管理器中显示")).on_click(
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
            let (a, b, open) = (path.clone(), path.clone(), path.clone());
            menu.item(item(
                &view,
                tr("在文件页中打开"),
                move |this, w, cx| this.show_file(open.clone(), w, cx),
            ))
            .item(item(&view, tr("文件历史"), move |this, w, cx| {
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
            let (a, b, c) = (entries.clone(), entries.clone(), entries.clone());
            let menu = match area {
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
            };
            menu.separator().item(item(
                &view,
                tf!("贮藏选中的 {count} 个文件…", count),
                move |this, w, cx| this.stash_entries_dialog(c.clone(), w, cx),
            ))
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
                tr("拉取，以变基代替合并（--rebase）"),
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
                            Notification::warning(tr("当前处于分离的 HEAD 状态，请先切换到分支再推送")),
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
                    let mut push = args(&["push", "--progress", "--tags", "--"]);
                    push.push(remote);
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
            tr("保留改动并切换"),
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
                        let text = tf!("已检出 {rev}，当前处于分离的 HEAD 状态", rev = short(&rev));
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
                let text = tf!("已新建提交以还原 {id}", id = short(&id));
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
                "{cur} 将指向 {id}，之后的提交将从此分支移除（可通过 reflog 找回）。",
                cur,
                id = short(&id)
            )
        };
        let reset = vec!["reset".into(), flag.into(), id.clone()];
        // 确认框写的是现在这个分支：确认时 HEAD 已经换了（切了分支、别处提交了）就不重置，不然动的
        // 是确认框里没写的那个分支，--hard 还丢掉它的改动
        let checks = self.head_checks();
        let run = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            let (id, cur, reset) = (id.clone(), cur.clone(), reset.clone());
            this.verify_then(checks.clone(), window, cx, move |this, window, cx| {
                this.run_op(tr("重置"), reset, None, window, cx, move |_, _, w, cx| {
                    let text = tf!("{cur} 已重置到 {id}", cur, id = short(&id));
                    w.push_notification(Notification::success(text), cx);
                });
            });
        };
        match flag {
            "--hard" => {
                let description = tf!("{moved}工作区和暂存区中的所有改动将被丢弃，此操作无法撤销。", moved);
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
                let description = tf!("{moved}这些提交的改动将保留在暂存区，工作区不变。", moved);
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
                let description = tf!("{moved}改动将保留在工作区，暂存区将被清空。", moved);
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
    /// 把 `name`（完整引用名是 `full`）合并到当前分支。
    fn merge(&mut self, name: String, full: String, window: &mut Window, cx: &mut Context<Self>) {
        let cur = self.current_label();
        let rev = merge_rev(&name, &full, &self.refs);
        let choose = super::dialogs::Choose {
            title: tf!("合并 {name} 到 {cur}", name = &name, cur).into(),
            description: SharedString::default(),
            options: MERGE_MODES
                .iter()
                .map(|mode| (tr(mode.label).into(), tr(mode.detail).into()))
                .collect(),
            selected: 0,
            checks: vec![(tr("合并后暂不提交，以便检查（--no-commit）"), false)],
            ok: tr("合并"),
        };
        self.choose(
            choose,
            window,
            cx,
            move |this, picked, checks, window, cx| {
                let mode = MERGE_MODES.get(picked).unwrap_or(&MERGE_MODES[0]);
                let no_commit = checks.first().copied().unwrap_or(false);
                let merge = merge_args(&rev, mode.flag, no_commit);
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
                                "改动已压缩并放入暂存区。请在「本地修改」中填写说明后提交。",
                            ))
                            .title(tf!("已压缩合并 {name} 的改动", name))
                        } else if no_commit && !stdout.contains("Fast-forward") {
                            this.show_changes(cx);
                            Notification::success(tr(
                                "尚未提交：请检查后点击上方的「提交合并」，或点击「中止」放弃。",
                            ))
                            .title(tf!("{name} 已合并，尚未提交", name))
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

    /// 把当前分支变基到 `target`（完整引用名或提交，短名可能认成同名的标签）；`shown` 是给用户看的名字。
    fn rebase_onto(
        &mut self,
        target: String,
        shown: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cur = self.current_label();
        let description = tf!(
            "{cur} 独有的提交将在 {shown} 之后重新生成。已推送的分支变基后需强制推送。",
            cur,
            shown
        );
        self.confirm_plain(
            tf!("将 {cur} 变基到 {shown}？", cur, shown),
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

    /// 拉取一个不是当前的本地分支：从 `upstream`（`远程/分支`）获取，能快进就把分支挪过去，
    /// 工作区不动。本地有上游没有的提交时 git 会拒绝（那得切过去合并或变基）。
    fn fast_forward_branch(
        &mut self,
        name: String,
        upstream: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((remote, remote_branch)) = upstream.split_once('/') else {
            return;
        };
        let fetch = vec![
            "fetch".into(),
            "--progress".into(),
            remote.into(),
            format!("{remote_branch}:{name}"),
        ];
        self.run_steps(
            tr("拉取"),
            vec![Step::net(fetch)],
            window,
            cx,
            Box::new(move |_, result, window, cx| match result {
                Ok(_) => {
                    let text = tf!("{name} 已更新到 {upstream}", name, upstream);
                    window.push_notification(Notification::success(text), cx);
                }
                Err(text) if text.contains("non-fast-forward") => {
                    let text = tf!(
                        "{name} 有 {upstream} 中没有的提交，无法快进。请切换到 {name} 后拉取。",
                        name,
                        upstream
                    );
                    window.push_notification(Notification::warning(text).autohide(false), cx);
                }
                Err(text) => failure(window, cx, tr("拉取"), text),
            }),
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
        let upstream = branch.upstream.as_deref();
        let (remote, remote_branch, set_upstream) =
            match upstream.and_then(|upstream| upstream.split_once('/')) {
                Some((remote, remote_branch)) => (remote.into(), remote_branch.into(), false),
                None => (self.default_remote(), name.clone(), true),
            };
        let to = format!("{remote}/{remote_branch}");
        // 强制推送时远程上的那个分支应当是什么：确认框打开时看到的远程跟踪分支，没有就是「远程上还
        // 没有」。不写期望值时 git 拿推送那一刻的远程跟踪分支比，自动获取可能刚把它更新过，没看过的
        // 新提交照样被盖掉
        let tracking = upstream.unwrap_or(&to);
        let expect = self
            .refs
            .remotes
            .iter()
            .find(|remote| remote.name == tracking)
            .map(|remote| remote.target.clone());
        let mut push = args(&["push", "--progress"]);
        if force {
            let id = expect.as_deref().unwrap_or_default();
            push.push(format!(
                "--force-with-lease=refs/heads/{remote_branch}:{id}"
            ));
        }
        if set_upstream {
            push.push("--set-upstream".into());
        }
        // 写完整引用名：本地有同名的标签时短名会认错；`--` 之后名字以 `-` 开头也不会被当成选项
        push.extend([
            "--".into(),
            remote.clone(),
            format!("refs/heads/{name}:refs/heads/{remote_branch}"),
        ]);
        let description = match &expect {
            Some(id) => tf!(
                "{to} 上的提交将被本地的 {name} 覆盖。若远程上的 {to} 已不是 {id}，则不推送（--force-with-lease）。",
                to,
                name,
                id = short(id)
            ),
            None => tf!(
                "将本地的 {name} 推送到 {to}。若远程上已有 {to}，则不推送（--force-with-lease）。",
                to,
                name
            ),
        };
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
            title: tf!("重命名 {old}", old).into(),
            placeholder: tr("新分支名称").into(),
            value: old.clone(),
            hint: tr("仅重命名本地分支，远程分支不变。").into(),
            ok: tr("重命名"),
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
                tr("重命名"),
                rename,
                None,
                window,
                cx,
                move |_, _, w, cx| {
                    w.push_notification(Notification::success(tf!("已重命名为 {new}", new)), cx);
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
            "--delete".into(),
            "--".into(),
            remote.into(),
            format!("refs/heads/{branch}"),
        ];
        let description = tf!(
            "将删除 {remote} 上的分支 {branch}，其他协作者也将无法访问。本地分支不受影响。",
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
            placeholder: tr("标签名称，例如 v1.0.0").into(),
            value: String::new(),
            // 填了说明就是带说明的标签（annotated），留空是轻量标签
            second: Some(tr("说明（可选）：填写后创建带说明的标签").into()),
            hint: tf!("目标提交：{id} {subject}", id = short(&id), subject).into(),
            ok: tr("创建"),
            checks: vec![(tr("创建后推送到远程").into(), false)],
            validate: Some(Box::new(|this, answer| {
                ref_name_error(&answer.text).or_else(|| {
                    this.refs
                        .tags
                        .iter()
                        .any(|t| t.name == answer.text)
                        .then(|| tf!("标签 {text} 已存在", text = answer.text))
                })
            })),
            ..Default::default()
        };
        self.ask(ask, window, cx, move |this, answer, window, cx| {
            let name = answer.text;
            let push = answer.checks.first().copied().unwrap_or(false);
            let mut steps = vec![Step::local(tag_args(&name, &answer.second, &id))];
            if push {
                steps.push(Step::net(vec![
                    "push".into(),
                    "--progress".into(),
                    "--".into(),
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
            "--".into(),
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
                    "将删除 {remote} 上的标签 {name}，本地标签保留。",
                    remote,
                    name
                ),
            )
        } else {
            (
                tr("删除本地标签？"),
                tf!("将删除本地标签 {name}，远程标签不受影响。", name),
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
                        "--delete".into(),
                        "--".into(),
                        remote.clone(),
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
                Notification::success(tr("获取完成，已清理远程已删除的分支")),
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
                Notification::warning(tr("当前处于分离的 HEAD 状态，请先切换到分支")),
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
            "{path} 中的所有冲突将采用此版本并标记为已解决，另一方的改动和手动编辑的内容将被覆盖。",
            path
        );
        self.confirm(
            tf!("整个文件采用{which}？", which),
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
                    let text = tf!("{path} 已采用{which}，并标记为已解决", path, which);
                    w.push_notification(Notification::success(text), cx);
                });
            },
        );
    }

    fn ignore_path(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let ignore = Step::local(fr_core::ignore::args(&path)).shown(fr_core::ignore::shown(&path));
        self.run_seq(
            tr("忽略"),
            vec![ignore],
            window,
            cx,
            move |_, _, w, cx| {
                w.push_notification(
                    Notification::success(tf!("{path} 已添加到 .gitignore", path)),
                    cx,
                );
            },
        );
    }

    // ---------- 远程、子模块 ----------

    fn add_remote_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ask = Ask {
            title: tr("添加远程").into(),
            placeholder: tr("名称，例如 upstream").into(),
            second: Some(tr("地址，例如 https://example.com/team/project.git").into()),
            hint: tr("添加后立即获取；获取失败时远程仍保留，可在右键菜单中修改或删除。").into(),
            ok: tr("添加"),
            validate: Some(Box::new(|this, answer| {
                if answer.second.is_empty() {
                    return Some(tr("请填写地址").into());
                }
                if answer.text.contains(char::is_whitespace) || answer.text.contains('/') {
                    return Some(tr("远程名称不能包含空格和斜杠").into());
                }
                this.remote_names
                    .contains(&answer.text)
                    .then(|| tf!("远程 {text} 已存在", text = answer.text))
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

    /// 修改远程的地址：先读出现在的地址填进去。推送单独配了地址（`pushurl`）时 `remote set-url`
    /// 不动它，对话框里写明，并给一个勾选项把它删掉、推送也改用新地址。
    fn set_remote_url_dialog(
        &mut self,
        remote: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        let read = |key: &str| {
            let key = format!("remote.{remote}.{key}");
            channel.git(&repo, args(&["config", "--get-all", &key]))
        };
        let (url, push) = (read("url"), read("pushurl"));
        cx.spawn_in(window, async move |this, cx| {
            let (url, push) = futures::join!(url, push);
            let text = |out: anyhow::Result<GitOutput>| {
                out.ok()
                    .filter(GitOutput::success)
                    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                    .unwrap_or_default()
            };
            // 获取地址配了好几个时 git 用第一个
            let url = text(url).lines().next().unwrap_or_default().to_string();
            let push = text(push);
            this.update_in(cx, |this, window, cx| {
                this.set_remote_url_ask(remote, url, push, window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn set_remote_url_ask(
        &mut self,
        remote: String,
        url: String,
        push: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 推送地址可以配好几个，一行一个
        let push = push.lines().collect::<Vec<_>>().join(tr("、"));
        let (hint, checks) = if push.is_empty() {
            (tr("获取和推送均使用此地址。").into(), Vec::new())
        } else {
            // 勾上「推送也改用此地址」时说明跟着变
            let hint = Hint::ByChecks(Box::new(move |checks| {
                match checks.first().copied().unwrap_or(false) {
                    true => tf!(
                        "将删除另行配置的推送地址 {push}，获取和推送均使用此地址。",
                        push
                    ),
                    false => tf!("推送另行配置了地址 {push}，不随此处修改。", push),
                }
                .into()
            }));
            (hint, vec![(tr("推送也改用此地址").into(), false)])
        };
        let ask = Ask {
            title: tf!("修改 {remote} 的地址", remote = &remote).into(),
            placeholder: tr("新地址").into(),
            value: url,
            hint,
            ok: tr("修改"),
            checks,
            validate: Some(Box::new(|_, answer| {
                // 以 - 开头的地址会被 ssh 当成选项（git 也拒绝这样的地址）
                answer
                    .text
                    .starts_with('-')
                    .then(|| tr("地址不能以 - 开头。").to_string())
            })),
            ..Default::default()
        };
        self.ask(ask, window, cx, move |this, answer, window, cx| {
            let remote = remote.clone();
            let set = vec![
                "remote".into(),
                "set-url".into(),
                "--".into(),
                remote.clone(),
                answer.text,
            ];
            let mut steps = vec![Step::local(set)];
            if answer.checks.first().copied().unwrap_or(false) {
                let key = format!("remote.{remote}.pushurl");
                steps.push(Step::local(args(&["config", "--unset-all", &key])));
            }
            this.run_seq(tr("修改远程"), steps, window, cx, move |_, _, w, cx| {
                let text = tf!("{remote} 的地址已修改", remote);
                w.push_notification(Notification::success(text), cx);
            });
        });
    }

    fn remove_remote(&mut self, remote: String, window: &mut Window, cx: &mut Context<Self>) {
        let description = tf!(
            "仅删除本地记录的 {remote} 及其远程分支，远程仓库本身不受影响。",
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

    /// 在新标签页里打开子模块：它就是仓库目录下的另一个仓库。先核对它有自己的仓库：没初始化的
    /// 子模块目录里没有，git 会往上找到这个仓库，打开的标签页叫子模块、动的却是这个仓库。没初始化
    /// 就问要不要初始化。
    /// 现在列表里提交是 `target` 的那个贮藏（菜单按提交认，见 `side_menu`）。没了是 None。
    pub(super) fn stash_now(&self, target: &str) -> Option<fr_core::refs::Stash> {
        self.refs
            .stashes
            .iter()
            .find(|stash| stash.target == target)
            .cloned()
    }

    /// 在新标签页中打开子模块 `path`（相对仓库根目录）。按路径不按下标：菜单开着时子模块列表可能
    /// 重读了。
    pub(super) fn open_submodule(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.submodules.contains(&path) {
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let full = self.sub_path(&path);
        let outer = channel.git(&self.entry.path, super::subs::toplevel_args());
        cx.spawn_in(window, async move |this, cx| {
            let outer = super::subs::toplevel(outer.await);
            let ready = super::subs::own_repo(&channel, &full, outer.as_deref()).await;
            this.update_in(cx, |this, window, cx| {
                if ready {
                    cx.emit(RepoEvent::Open(RepoEntry {
                        server: this.entry.server.clone(),
                        path: full,
                    }));
                    return;
                }
                let description = tf!(
                    "子模块 {path} 尚未初始化。初始化将从远程获取其内容（git submodule update --init），完成后可再打开。",
                    path = &path
                );
                this.confirm_plain(
                    tr("初始化子模块？"),
                    description,
                    tr("初始化"),
                    window,
                    cx,
                    move |this, window, cx| this.update_submodules(Some(path.clone()), window, cx),
                );
            })
            .ok();
        })
        .detach();
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
                let text = tf!("{name}已完成", name = state.name());
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
            InProgress::Rebase => {
                tr("跳过正在重放的提交：其改动（含已解决的冲突）将被丢弃，变基继续处理后续提交。")
            }
            _ => tr("放弃当前正在处理的提交，已解决的冲突将丢失。"),
        };
        self.confirm(
            tr("跳过此提交？"),
            description,
            tr("跳过"),
            window,
            cx,
            move |this, window, cx| {
                let skip = args(&[state.command(), "--skip"]);
                this.run_op(tr("跳过"), skip, None, window, cx, |_, _, window, cx| {
                    window.push_notification(Notification::success(tr("已跳过此提交")), cx);
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
            "恢复到{name}之前的状态，已解决的冲突将丢失。",
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
                        let text = tf!("已中止{name}，已恢复到开始前的状态", name = state.name());
                        window.push_notification(Notification::success(text), cx);
                    },
                );
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{merge_args, merge_rev};
    use fr_core::refs::{Branch, Refs, Tag};

    fn branch(name: &str) -> Branch {
        Branch {
            name: name.into(),
            target: String::new(),
            current: false,
            upstream: None,
            ahead: 0,
            behind: 0,
            gone: false,
        }
    }

    #[test]
    fn 合并时短名会认错就用完整引用名() {
        let refs = Refs {
            branches: ["feat", "v1", "origin/main", "stash", "heads/x"]
                .map(branch)
                .to_vec(),
            remotes: ["origin/main", "origin/dev"].map(branch).to_vec(),
            tags: vec![Tag {
                name: "v1".into(),
                target: String::new(),
            }],
            stashes: Vec::new(),
        };
        let rev = |short: &str, full: &str| merge_rev(short, full, &refs);
        assert_eq!(rev("feat", "refs/heads/feat"), "feat");
        // 有同名的标签：git 先认标签
        assert_eq!(rev("v1", "refs/heads/v1"), "refs/heads/v1");
        // 远程分支和本地分支同名：git 先认本地分支
        let remote = "refs/remotes/origin/main";
        assert_eq!(rev("origin/main", remote), remote);
        assert_eq!(rev("origin/main", "refs/heads/origin/main"), "origin/main");
        assert_eq!(rev("origin/dev", "refs/remotes/origin/dev"), "origin/dev");
        // 排在最前面的 refs/<名字>
        assert_eq!(rev("stash", "refs/heads/stash"), "refs/heads/stash");
        assert_eq!(rev("heads/x", "refs/heads/heads/x"), "refs/heads/heads/x");
        // 名字放在 `--` 后面
        assert_eq!(
            merge_args("-x", None, false),
            ["merge", "--no-edit", "--", "-x"]
        );
    }
}
