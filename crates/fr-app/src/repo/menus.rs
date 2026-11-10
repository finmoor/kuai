//! 右键菜单，以及从菜单里发起的操作：合并、变基、挑拣、还原、重置、标签、分支改名、
//! 删除远程分支、解决冲突时取哪一边。
//!
//! 菜单在点右键时才构建；构建函数里只带要用到的名字和哈希，点了菜单项再回到 [`RepoView`] 上执行。

use fr_core::state::InProgress;
use fr_core::status::{Entry, Kind};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::changes::Area;
use super::dialogs::Ask;
use super::ops::args;
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

impl RepoView {
    /// 当前分支的名字；分离的 HEAD 时是「HEAD」。
    fn current_label(&self) -> String {
        self.status
            .as_ref()
            .and_then(|s| s.branch.clone())
            .unwrap_or_else(|| "HEAD".into())
    }

    /// 推送、删除远程分支时默认用的远程：有 origin 用 origin，否则用第一个。
    fn default_remote(&self) -> String {
        let mut names = self
            .refs
            .remotes
            .iter()
            .filter_map(|b| b.name.split_once('/').map(|(remote, _)| remote));
        let first = names.next().unwrap_or("origin").to_string();
        if first == "origin" || names.any(|n| n == "origin") {
            "origin".into()
        } else {
            first
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
                    format!("切换到 {branch}"),
                    move |this, w, cx| this.checkout_branch(&name, w, cx),
                ));
            }
            let target = id.clone();
            menu = menu
                .item(item(
                    &view,
                    "检出这个提交（分离 HEAD）",
                    move |this, w, cx| this.checkout_detached(target.clone(), w, cx),
                ))
                .separator()
                .item(item(&view, "在此新建分支…", |this, w, cx| {
                    this.new_branch_dialog(w, cx)
                }))
                .item({
                    let (id, subject) = (id.clone(), subject.clone());
                    item(&view, "在此新建标签…", move |this, w, cx| {
                        this.new_tag_dialog(id.clone(), subject.clone(), w, cx)
                    })
                })
                .separator();
            if !is_head {
                let pick = id.clone();
                menu = menu.item(item(
                    &view,
                    format!("挑拣到 {cur}（cherry-pick）"),
                    move |this, w, cx| this.cherry_pick(pick.clone(), is_merge, w, cx),
                ));
            }
            let revert = id.clone();
            menu = menu.item(item(
                &view,
                "还原这个提交（revert）",
                move |this, w, cx| this.revert_commit(revert.clone(), is_merge, w, cx),
            ));
            if !is_head {
                let (view2, id2) = (view.clone(), id.clone());
                menu = menu.submenu(
                    format!("把 {cur} 重置到这里"),
                    window,
                    cx,
                    move |sub, _, _| {
                        let mode = |label: &'static str, flag: &'static str| {
                            let id = id2.clone();
                            item(&view2, label, move |this, w, cx| {
                                this.reset_to(id.clone(), flag, w, cx)
                            })
                        };
                        sub.item(mode("保留改动，并且留在暂存区（soft）", "--soft"))
                            .item(mode("保留改动，不暂存（mixed）", "--mixed"))
                            .item(mode("丢弃所有改动（hard）…", "--hard"))
                    },
                );
                let onto = id.clone();
                menu = menu.item(item(
                    &view,
                    format!("把 {cur} 变基到这里…"),
                    move |this, w, cx| {
                        this.rebase_onto(onto.clone(), short(&onto).to_string(), w, cx)
                    },
                ));
            }
            menu.separator()
                .item(copy_item("复制哈希", id.clone()))
                .item(copy_item("复制说明", subject.clone()))
        }))
    }

    /// 侧栏里一项的菜单。
    pub(super) fn sidebar_menu(&self, side: SideItem, cx: &mut Context<Self>) -> Option<MenuFn> {
        let view = cx.entity().downgrade();
        let cur = self.current_label();
        match side {
            SideItem::Branch(i) => {
                let branch = self.refs.branches.get(i)?.clone();
                Some(Box::new(move |mut menu, _, _| {
                    let name = branch.name.clone();
                    if !branch.current {
                        let (a, b, c) = (name.clone(), name.clone(), name.clone());
                        menu = menu
                            .item(item(
                                &view,
                                format!("切换到 {name}"),
                                move |this, w, cx| this.checkout_branch(&a, w, cx),
                            ))
                            .separator()
                            .item(item(
                                &view,
                                format!("合并 {name} 到 {cur}"),
                                move |this, w, cx| this.merge(b.clone(), w, cx),
                            ))
                            .item(item(
                                &view,
                                format!("把 {cur} 变基到 {name}…"),
                                move |this, w, cx| this.rebase_onto(c.clone(), c.clone(), w, cx),
                            ))
                            .separator();
                    }
                    let (a, b, c, d) = (branch.clone(), branch.clone(), name.clone(), name.clone());
                    menu = menu
                        .item(item(&view, format!("推送 {name}"), move |this, w, cx| {
                            this.push_branch(&a, false, w, cx)
                        }))
                        .item(item(
                            &view,
                            "强制推送（--force-with-lease）…",
                            move |this, w, cx| this.push_branch(&b, true, w, cx),
                        ))
                        .separator()
                        .item(item(&view, "改名…", move |this, w, cx| {
                            this.rename_branch_dialog(c.clone(), w, cx)
                        }));
                    if !branch.current {
                        menu = menu.item(item(&view, "删除…", move |this, w, cx| {
                            this.delete_branch(d.clone(), w, cx)
                        }));
                    }
                    menu.separator().item(copy_item("复制分支名", name))
                }))
            }
            SideItem::Remote(i) => {
                let name = self.refs.remotes.get(i)?.name.clone();
                Some(Box::new(move |menu, _, _| {
                    let (a, b, c, d) = (name.clone(), name.clone(), name.clone(), name.clone());
                    menu.item(item(&view, "检出为本地分支", move |this, w, cx| {
                        this.checkout_remote(&a, w, cx)
                    }))
                    .separator()
                    .item(item(
                        &view,
                        format!("合并 {name} 到 {cur}"),
                        move |this, w, cx| this.merge(b.clone(), w, cx),
                    ))
                    .item(item(
                        &view,
                        format!("把 {cur} 变基到 {name}…"),
                        move |this, w, cx| this.rebase_onto(c.clone(), c.clone(), w, cx),
                    ))
                    .separator()
                    .item(item(&view, "删除远程分支…", move |this, w, cx| {
                        this.delete_remote_branch(d.clone(), w, cx)
                    }))
                    .separator()
                    .item(copy_item("复制分支名", name.clone()))
                }))
            }
            SideItem::Tag(i) => {
                let name = self.refs.tags.get(i)?.name.clone();
                let remote = self.default_remote();
                Some(Box::new(move |menu, _, _| {
                    let (a, b, c, d) = (name.clone(), name.clone(), name.clone(), name.clone());
                    menu.item(item(
                        &view,
                        "检出这个标签（分离 HEAD）",
                        move |this, w, cx| this.checkout_detached(a.clone(), w, cx),
                    ))
                    .separator()
                    .item(item(
                        &view,
                        format!("推送到 {remote}"),
                        move |this, w, cx| this.push_tag(b.clone(), w, cx),
                    ))
                    .item(item(&view, "删除本地标签…", move |this, w, cx| {
                        this.delete_tag(c.clone(), false, w, cx)
                    }))
                    .item(item(
                        &view,
                        format!("从 {remote} 删除…"),
                        move |this, w, cx| this.delete_tag(d.clone(), true, w, cx),
                    ))
                    .separator()
                    .item(copy_item("复制标签名", name.clone()))
                }))
            }
            SideItem::Header(SideSection::Remotes) => {
                let mut remotes: Vec<String> = self
                    .refs
                    .remotes
                    .iter()
                    .filter_map(|b| b.name.split_once('/').map(|(remote, _)| remote.to_string()))
                    .collect();
                remotes.dedup();
                Some(Box::new(move |mut menu, _, _| {
                    menu = menu.item(item(&view, "添加远程…", |this, w, cx| {
                        this.add_remote_dialog(w, cx)
                    }));
                    for remote in &remotes {
                        let (a, b) = (remote.clone(), remote.clone());
                        menu = menu
                            .separator()
                            .item(item(
                                &view,
                                format!("改 {remote} 的地址…"),
                                move |this, w, cx| this.set_remote_url_dialog(a.clone(), w, cx),
                            ))
                            .item(item(
                                &view,
                                format!("删除远程 {remote}…"),
                                move |this, w, cx| this.remove_remote(b.clone(), w, cx),
                            ));
                    }
                    menu
                }))
            }
            SideItem::Header(SideSection::Submodules) => Some(Box::new(move |menu, _, _| {
                menu.item(item(&view, "更新全部子模块", |this, w, cx| {
                    this.update_submodules(None, w, cx)
                }))
            })),
            SideItem::Submodule(i) => {
                let path = self.submodules.get(i)?.clone();
                Some(Box::new(move |menu, _, _| {
                    let (a, b) = (path.clone(), path.clone());
                    menu.item(item(
                        &view,
                        "在新标签页里打开",
                        move |this, _, cx| this.open_submodule(i, cx),
                    ))
                    .item(item(&view, "更新这个子模块", move |this, w, cx| {
                        this.update_submodules(Some(a.clone()), w, cx)
                    }))
                    .separator()
                    .item(copy_item("复制路径", b.clone()))
                }))
            }
            SideItem::Stash(i) => {
                let stash = self.refs.stashes.get(i)?.clone();
                Some(Box::new(move |menu, _, _| {
                    let (a, b) = (stash.selector.clone(), stash.selector.clone());
                    let c = (stash.selector.clone(), stash.message.clone());
                    menu.item(item(
                        &view,
                        "应用（贮藏留着）",
                        move |this, w, cx| this.stash_apply(a.clone(), w, cx),
                    ))
                    .item(item(
                        &view,
                        "弹出（应用并删除）",
                        move |this, w, cx| this.stash_pop(Some(b.clone()), w, cx),
                    ))
                    .separator()
                    .item(item(&view, "删除…", move |this, w, cx| {
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
        let conflicted = entry.unstaged == Some(Kind::Conflicted);
        Box::new(move |mut menu, _, _| {
            match area {
                Area::Unstaged => {
                    if conflicted {
                        let (a, b) = (entry.path.clone(), entry.path.clone());
                        menu = menu
                            .item(item(
                                &view,
                                "用当前分支的（ours）",
                                move |this, w, cx| this.resolve_with(a.clone(), "--ours", w, cx),
                            ))
                            .item(item(
                                &view,
                                "用传入的（theirs）",
                                move |this, w, cx| this.resolve_with(b.clone(), "--theirs", w, cx),
                            ));
                    }
                    let (a, b) = (entry.clone(), entry.clone());
                    menu = menu
                        .item(item(
                            &view,
                            if conflicted {
                                "标记为已解决"
                            } else {
                                "暂存"
                            },
                            move |this, w, cx| this.stage_file(&a, w, cx),
                        ))
                        .item(item(&view, "丢弃改动…", move |this, w, cx| {
                            this.discard_file(&b, w, cx)
                        }));
                    if entry.unstaged == Some(Kind::Untracked) {
                        let path = entry.path.clone();
                        menu = menu.item(item(&view, "加到 .gitignore", move |this, w, cx| {
                            this.ignore_path(path.clone(), w, cx)
                        }));
                    }
                }
                Area::Staged => {
                    let a = entry.clone();
                    menu = menu.item(item(&view, "取消暂存", move |this, w, cx| {
                        this.unstage_file(&a, w, cx)
                    }));
                }
            }
            menu = menu.separator();
            if entry.unstaged != Some(Kind::Untracked) {
                let path = entry.path.clone();
                let blame = entry.path.clone();
                menu = menu
                    .item(item(&view, "文件历史", move |this, _, cx| {
                        this.show_file_history(path.clone(), cx)
                    }))
                    .item(item(
                        &view,
                        "逐行追溯（blame）",
                        move |this, _, cx| this.show_blame(blame.clone(), cx),
                    ));
            }
            if local {
                let file = format!("{full_path}/{}", entry.path);
                menu = menu.item(
                    PopupMenuItem::new("在资源管理器里显示")
                        .on_click(move |_, _, _| crate::shell::reveal_in_explorer(&file)),
                );
            }
            menu.item(copy_item("复制路径", entry.path.clone()))
        })
    }

    /// 工具栏上「获取」「拉取」的右键菜单：不常用的选项。
    pub(super) fn fetch_menu(&self, cx: &mut Context<Self>) -> MenuFn {
        let view = cx.entity().downgrade();
        Box::new(move |menu, _, _| {
            menu.item(item(
                &view,
                "获取，并清理远程已删除的分支（--prune）",
                |this, w, cx| this.fetch_prune(w, cx),
            ))
        })
    }

    pub(super) fn pull_menu(&self, cx: &mut Context<Self>) -> MenuFn {
        let view = cx.entity().downgrade();
        Box::new(move |menu, _, _| {
            menu.item(item(
                &view,
                "拉取，用变基代替合并（--rebase）",
                |this, w, cx| this.pull_rebase(w, cx),
            ))
        })
    }

    pub(super) fn push_menu(&self, cx: &mut Context<Self>) -> MenuFn {
        let view = cx.entity().downgrade();
        Box::new(move |menu, _, _| {
            menu.item(item(
                &view,
                "强制推送（--force-with-lease）…",
                |this, w, cx| {
                    let current = this.refs.branches.iter().find(|b| b.current).cloned();
                    match current {
                        Some(branch) => this.push_branch(&branch, true, w, cx),
                        None => w.push_notification(
                            Notification::warning("现在是分离的 HEAD，先切换到一个分支再推送"),
                            cx,
                        ),
                    }
                },
            ))
            .item(item(
                &view,
                "推送所有标签（--tags）",
                |this, w, cx| {
                    let remote = this.default_remote();
                    let push = vec!["push".into(), "--progress".into(), remote, "--tags".into()];
                    this.run_net_op("推送", push, w, cx, |_, _, w, cx| {
                        w.push_notification(Notification::success("标签已推送"), cx);
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
        let checkout = vec![
            "checkout".into(),
            "-q".into(),
            "--detach".into(),
            rev.clone(),
        ];
        self.run_op("检出", checkout, None, window, cx, move |_, _, w, cx| {
            let text = format!("已检出 {}，现在是分离的 HEAD", short(&rev));
            w.push_notification(Notification::success(text), cx);
        });
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
        self.run_op("挑拣", pick, None, window, cx, move |_, _, w, cx| {
            w.push_notification(Notification::success(format!("已挑拣 {}", short(&id))), cx);
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
        self.run_op("还原", revert, None, window, cx, move |_, _, w, cx| {
            let text = format!("已新建一个提交，还原了 {}", short(&id));
            w.push_notification(Notification::success(text), cx);
        });
    }

    fn reset_to(
        &mut self,
        id: String,
        flag: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cur = self.current_label();
        let reset = vec!["reset".into(), flag.into(), id.clone()];
        let run = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            let (id, cur) = (id.clone(), cur.clone());
            this.run_op(
                "重置",
                reset.clone(),
                None,
                window,
                cx,
                move |_, _, w, cx| {
                    let text = format!("{cur} 已重置到 {}", short(&id));
                    w.push_notification(Notification::success(text), cx);
                },
            );
        };
        if flag == "--hard" {
            let description =
                "工作区和暂存区里的改动全部丢弃，不能撤销。被跳过的提交可以从 reflog 里找回。";
            self.confirm("重置并丢弃所有改动？", description, "重置", window, cx, run);
        } else {
            run(self, window, cx);
        }
    }

    fn merge(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let merge = vec!["merge".into(), "--no-edit".into(), name.clone()];
        self.run_op("合并", merge, None, window, cx, move |_, out, w, cx| {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let first = stdout.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            let note = Notification::success(first.to_string()).title(format!("已合并 {name}"));
            w.push_notification(note, cx);
        });
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
        let description = format!(
            "{cur} 上独有的提交会在 {shown} 之后重新生成，哈希会变。已经推送过的分支变基后要强制推送。"
        );
        self.confirm(
            format!("把 {cur} 变基到 {shown}？"),
            description,
            "变基",
            window,
            cx,
            move |this, window, cx| {
                let shown = shown.clone();
                let rebase = vec!["rebase".into(), target.clone()];
                this.run_op("变基", rebase, None, window, cx, move |_, _, w, cx| {
                    w.push_notification(Notification::success(format!("已变基到 {shown}")), cx);
                });
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
        let run = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            let text = format!("{name} 已推送到 {to}");
            this.run_net_op("推送", push.clone(), window, cx, move |_, _, w, cx| {
                w.push_notification(Notification::success(text), cx);
            });
        };
        if force {
            let description = format!(
                "远程上 {} 的提交会被本地的覆盖。只有远程分支还停在你上次获取时的位置才会成功（--force-with-lease）。",
                branch.name
            );
            self.confirm("强制推送？", description, "强制推送", window, cx, run);
        } else {
            run(self, window, cx);
        }
    }

    fn rename_branch_dialog(&mut self, old: String, window: &mut Window, cx: &mut Context<Self>) {
        let ask = Ask {
            title: format!("给 {old} 改名").into(),
            placeholder: "新的分支名".into(),
            value: old.clone(),
            hint: "只改本地分支的名字，远程上的分支不变。".into(),
            ok: "改名",
            checks: Vec::new(),
        };
        self.ask(ask, window, cx, move |this, new, _, window, cx| {
            if new == old {
                return;
            }
            let rename = vec!["branch".into(), "-m".into(), old.clone(), new.clone()];
            this.run_op("改名", rename, None, window, cx, move |_, _, w, cx| {
                w.push_notification(Notification::success(format!("已改名为 {new}")), cx);
            });
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
        let description = format!(
            "远程 {remote} 上的分支 {branch} 会被删除，其他人也会看不到它。本地分支不受影响。"
        );
        self.confirm(
            "删除远程分支？",
            description,
            "删除",
            window,
            cx,
            move |this, window, cx| {
                let full = full.clone();
                this.run_net_op(
                    "删除远程分支",
                    delete.clone(),
                    window,
                    cx,
                    move |_, _, w, cx| {
                        w.push_notification(Notification::success(format!("已删除 {full}")), cx);
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
            title: "新建标签".into(),
            placeholder: "标签名，比如 v1.0.0".into(),
            value: String::new(),
            hint: format!("打在提交 {} {subject} 上", short(&id)).into(),
            ok: "创建",
            checks: vec![("创建后推送到远程", false)],
        };
        self.ask(ask, window, cx, move |this, name, checks, window, cx| {
            let push = checks.first().copied().unwrap_or(false);
            let mut steps = vec![vec!["tag".into(), name.clone(), id.clone()]];
            if push {
                steps.push(vec![
                    "push".into(),
                    "--progress".into(),
                    remote.clone(),
                    format!("refs/tags/{name}"),
                ]);
            }
            this.run_seq(
                "新建标签",
                steps,
                push,
                window,
                cx,
                move |_, _, w, cx| {
                    w.push_notification(Notification::success(format!("已创建标签 {name}")), cx);
                },
            );
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
        self.run_net_op("推送标签", push, window, cx, move |_, _, w, cx| {
            let text = format!("标签 {name} 已推送到 {remote}");
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
                "从远程删除标签？",
                format!("{remote} 上的标签 {name} 会被删除，本地的留着。"),
            )
        } else {
            (
                "删除本地标签？",
                format!("本地的标签 {name} 会被删除，远程上的不受影响。"),
            )
        };
        self.confirm(
            title,
            description,
            "删除",
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
                    this.run_net_op("删除标签", delete, window, cx, move |_, _, w, cx| {
                        w.push_notification(
                            Notification::success(format!("已从远程删除 {name}")),
                            cx,
                        );
                    });
                } else {
                    let delete = vec!["tag".into(), "-d".into(), name.clone()];
                    this.run_op(
                        "删除标签",
                        delete,
                        None,
                        window,
                        cx,
                        move |_, _, w, cx| {
                            w.push_notification(
                                Notification::success(format!("已删除标签 {name}")),
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
        self.run_net_op("获取", fetch, window, cx, |_, _, w, cx| {
            w.push_notification(Notification::success("获取完成，已清理远程删除的分支"), cx);
        });
    }

    fn pull_rebase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pull = args(&["pull", "--rebase", "--progress"]);
        self.run_net_op("拉取", pull, window, cx, |_, _, w, cx| {
            w.push_notification(Notification::success("拉取完成（变基）"), cx);
        });
    }

    /// 有冲突的文件整个取一边，然后标记为已解决。
    fn resolve_with(
        &mut self,
        path: String,
        side: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let steps = vec![
            vec!["checkout".into(), side.into(), "--".into(), path.clone()],
            vec!["add".into(), "--".into(), path],
        ];
        self.run_seq("解决冲突", steps, false, window, cx, |_, _, _, _| {});
    }

    fn ignore_path(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let ignore = fr_core::ignore::args(&path);
        self.run_op("忽略", ignore, None, window, cx, move |_, _, w, cx| {
            w.push_notification(
                Notification::success(format!("{path} 已加到 .gitignore")),
                cx,
            );
        });
    }

    // ---------- 远程、子模块 ----------

    fn add_remote_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ask = Ask {
            title: "添加远程".into(),
            placeholder: "名字 地址，比如 upstream https://example.com/team/project.git".into(),
            value: String::new(),
            hint: "名字和地址之间用空格隔开。添加后会获取一次。".into(),
            ok: "添加",
            checks: Vec::new(),
        };
        self.ask(ask, window, cx, |this, text, _, window, cx| {
            let Some((name, url)) = text.split_once(char::is_whitespace) else {
                window
                    .push_notification(Notification::warning("要填名字和地址，中间用空格隔开"), cx);
                return;
            };
            let (name, url) = (name.trim().to_string(), url.trim().to_string());
            let steps = vec![
                vec!["remote".into(), "add".into(), name.clone(), url],
                vec!["fetch".into(), "--progress".into(), name.clone()],
            ];
            this.run_seq(
                "添加远程",
                steps,
                true,
                window,
                cx,
                move |_, _, w, cx| {
                    w.push_notification(Notification::success(format!("已添加远程 {name}")), cx);
                },
            );
        });
    }

    fn set_remote_url_dialog(
        &mut self,
        remote: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ask = Ask {
            title: format!("改 {remote} 的地址").into(),
            placeholder: "新的地址".into(),
            value: String::new(),
            hint: "获取和推送都改用这个地址。".into(),
            ok: "修改",
            checks: Vec::new(),
        };
        self.ask(ask, window, cx, move |this, url, _, window, cx| {
            let remote = remote.clone();
            let set = vec!["remote".into(), "set-url".into(), remote.clone(), url];
            this.run_op("修改远程", set, None, window, cx, move |_, _, w, cx| {
                w.push_notification(Notification::success(format!("{remote} 的地址已修改")), cx);
            });
        });
    }

    fn remove_remote(&mut self, remote: String, window: &mut Window, cx: &mut Context<Self>) {
        let description = format!(
            "本地记录的 {remote} 和它的远程分支会被删掉。远程仓库本身不受影响，以后可以再添加。"
        );
        self.confirm(
            "删除远程？",
            description,
            "删除",
            window,
            cx,
            move |this, window, cx| {
                let remote = remote.clone();
                let remove = vec!["remote".into(), "remove".into(), remote.clone()];
                this.run_op(
                    "删除远程",
                    remove,
                    None,
                    window,
                    cx,
                    move |_, _, w, cx| {
                        w.push_notification(
                            Notification::success(format!("已删除远程 {remote}")),
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
        self.run_net_op("更新子模块", update, window, cx, |_, _, w, cx| {
            w.push_notification(Notification::success("子模块已更新"), cx);
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
        let go = match state {
            // 合并没有 --continue 以外的事要做：冲突解决完，提交就是了
            InProgress::Merge => args(&["commit", "--no-edit"]),
            other => args(&[other.command(), "--continue"]),
        };
        self.run_op(
            format!("继续{}", state.name()),
            go,
            None,
            window,
            cx,
            |_, _, _, _| {},
        );
    }

    /// 跳过当前这个提交（变基、挑拣、还原）。
    pub(super) fn skip_in_progress(
        &mut self,
        state: InProgress,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let skip = args(&[state.command(), "--skip"]);
        self.run_op("跳过", skip, None, window, cx, |_, _, _, _| {});
    }

    pub(super) fn abort_in_progress(
        &mut self,
        state: InProgress,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let description = format!(
            "回到{}开始之前的状态，这期间解决的冲突都不保留。",
            state.name()
        );
        self.confirm(
            format!("中止{}？", state.name()),
            description,
            "中止",
            window,
            cx,
            move |this, window, cx| {
                let abort = args(&[state.command(), "--abort"]);
                this.run_op(
                    format!("中止{}", state.name()),
                    abort,
                    None,
                    window,
                    cx,
                    |_, _, _, _| {},
                );
            },
        );
    }
}
