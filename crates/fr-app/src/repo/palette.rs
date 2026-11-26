//! 仓库页给命令面板（Ctrl+P，见 `command_palette.rs`）用的那一面：不带对象的 Git 命令、这个仓库
//! 的分支和标签、仓库里的文件，和选中以后做什么。
//!
//! 命令面板只是快路：这里的每一条命令在界面上另有鼠标能找到的地方（工具栏、右键菜单、各个 ⌄），
//! 调的是同一个函数。

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::*;

use super::compare::End;
use super::{Page, RepoView};

/// 一条仓库命令。
pub struct RepoCommand {
    /// 认它用的（记「最近用过」）
    pub id: &'static str,
    pub name: SharedString,
    /// 现在用不了的原因
    pub why_not: Option<SharedString>,
    pub run: fn(&mut RepoView, &mut Window, &mut Context<RepoView>),
}

/// 分支、远程分支还是标签。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RefKind {
    Branch,
    Remote,
    Tag,
}

/// 命令面板里列的一个分支或标签。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PaletteRef {
    pub kind: RefKind,
    pub name: String,
    /// 当前分支
    pub current: bool,
    target: String,
}

/// 对分支、标签能做的事（面板里按 Tab 列出来）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RefAction {
    Checkout,
    Reveal,
    Merge,
    Rebase,
    Compare,
    Pin,
    Delete,
    CopyName,
}

/// 对文件能做的事。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileAction {
    History,
    Blame,
    Open,
    CopyPath,
}

/// 最多列这么多个文件：再多筛起来慢，也翻不过来
const FILES_MAX: usize = 100_000;

type Run = fn(&mut RepoView, &mut Window, &mut Context<RepoView>);

/// 命令的清单：（id，名字，现在用不了的原因，做什么）。`view` 没有（当前不在仓库页）时全都用不了。
pub fn commands(view: Option<&RepoView>, cx: &App) -> Vec<RepoCommand> {
    let off_page: SharedString = tr("当前不在仓库页").into();
    // 各种「现在不行」
    let not_ready = view.is_none_or(|v| v.channel.is_none() || v.status.is_none());
    let reason = |extra: Option<&'static str>| -> Option<SharedString> {
        match view {
            None => Some(off_page.clone()),
            Some(_) if not_ready => Some(tr("仓库尚未就绪").into()),
            Some(_) => extra.map(SharedString::from),
        }
    };
    let entries = view
        .and_then(|v| v.status.as_ref())
        .map(|s| s.entries.as_slice())
        .unwrap_or(&[]);
    let has_staged = entries.iter().any(|e| e.staged.is_some());
    let has_changes = !entries.is_empty();
    let on_branch = view.is_some_and(|v| v.status.as_ref().is_some_and(|s| s.branch.is_some()));
    let has_remote = view.is_some_and(|v| !v.remote_names.is_empty());
    let has_web = view.is_some_and(|v| !v.web_remotes().is_empty());
    let unpushed = view.map_or(0, |v| v.history.unpushed_count());
    let gone = view.map_or(0, |v| v.gone_count());
    let (stashes_on, first_parent_on) = view.map_or((false, false), |v| v.graph_options(cx));
    let request = view.map_or(tr("合并请求"), |v| v.request_name(cx));
    let need = |ok: bool, why: &'static str| (!ok).then_some(why);
    let table: Vec<(&'static str, SharedString, Option<&'static str>, Run)> = vec![
        (
            "git.curate",
            tr("整理待推送的提交…").into(),
            need(unpushed > 0, tr("没有待推送的提交")),
            |this, w, cx| this.curate_unpushed(w, cx),
        ),
        (
            "git.fixup",
            tr("并入之前的提交…").into(),
            need(has_staged, tr("没有已暂存的改动")),
            |this, w, cx| this.fixup_dialog(None, w, cx),
        ),
        (
            "git.commit.no_verify",
            tr("提交，跳过钩子（--no-verify）").into(),
            need(has_staged, tr("没有已暂存的改动")),
            |this, w, cx| this.commit_then(false, true, w, cx),
        ),
        (
            "git.patch.apply",
            tr("应用补丁…").into(),
            None,
            |this, w, cx| this.apply_patch_file(w, cx),
        ),
        (
            "git.patch.clipboard",
            tr("从剪贴板应用补丁").into(),
            None,
            |this, w, cx| this.apply_patch_clipboard(w, cx),
        ),
        (
            "git.tag",
            tr("新建标签…").into(),
            None,
            |this, w, cx| {
                if let Some(head) = this.status.as_ref().and_then(|s| s.head.clone()) {
                    this.new_tag_dialog(head, String::new(), w, cx)
                }
            },
        ),
        (
            "git.stash.staged",
            tr("仅贮藏已暂存的改动…").into(),
            need(has_staged, tr("没有已暂存的改动")),
            |this, w, cx| this.stash_mode_dialog(fr_core::stash::Mode::Staged, w, cx),
        ),
        (
            "git.stash.keep",
            tr("贮藏，并保留已暂存的改动…").into(),
            need(has_changes, tr("没有可贮藏的改动")),
            |this, w, cx| this.stash_mode_dialog(fr_core::stash::Mode::KeepIndex, w, cx),
        ),
        (
            "git.fetch.prune",
            tr("获取并清理已删除的远程分支").into(),
            need(has_remote, tr("此仓库尚无远程")),
            |this, w, cx| this.fetch_prune(w, cx),
        ),
        (
            "git.pull.rebase",
            tr("拉取（变基）").into(),
            need(has_remote && on_branch, tr("当前分支无法拉取")),
            |this, w, cx| this.pull(super::ops::PullMode::Rebase, w, cx),
        ),
        (
            "git.pull.ff",
            tr("拉取（仅快进）").into(),
            need(has_remote && on_branch, tr("当前分支无法拉取")),
            |this, w, cx| this.pull(super::ops::PullMode::FfOnly, w, cx),
        ),
        (
            "git.push.upstream",
            tr("推送并设置上游…").into(),
            need(has_remote && on_branch, tr("当前分支无法推送")),
            |this, w, cx| this.push_set_upstream(w, cx),
        ),
        (
            "git.sweep",
            tr("清理上游已删除的分支…").into(),
            need(gone > 0, tr("没有上游已删除的分支")),
            |this, w, cx| this.sweep_gone_branches(w, cx),
        ),
        (
            "git.remote.add",
            tr("添加远程…").into(),
            None,
            |this, w, cx| this.add_remote_dialog(w, cx),
        ),
        (
            "git.author",
            tr("设置提交作者…").into(),
            None,
            |this, w, cx| this.author_dialog(None, w, cx),
        ),
        (
            "git.compare",
            tr("比较…").into(),
            None,
            |this, w, cx| {
                this.show_page(Page::Git, w, cx);
                this.show_compare(w, cx)
            },
        ),
        (
            "git.recover",
            tr("找回提交").into(),
            None,
            |this, w, cx| {
                this.show_page(Page::Git, w, cx);
                this.show_recover(w, cx)
            },
        ),
        (
            "git.file_history",
            tr("文件历史…").into(),
            None,
            |this, w, cx| {
                this.show_page(Page::Git, w, cx);
                this.pick_file(w, cx)
            },
        ),
        (
            "git.graph.stashes",
            match stashes_on {
                true => tr("提交图：不显示贮藏").into(),
                false => tr("提交图：显示贮藏").into(),
            },
            None,
            |this, _, cx| this.toggle_graph_option(true, cx),
        ),
        (
            "git.graph.first_parent",
            match first_parent_on {
                true => tr("提交图：显示全部提交").into(),
                false => tr("提交图：只走第一父提交").into(),
            },
            None,
            |this, _, cx| this.toggle_graph_option(false, cx),
        ),
        (
            "git.web.repo",
            tr("在浏览器中打开仓库").into(),
            need(has_web, tr("远程地址不是托管平台的地址")),
            |this, w, cx| {
                let remote = this.web_remote();
                let remote = match this.web_of(&remote) {
                    Some(_) => remote,
                    None => match this.web_remotes().into_iter().next() {
                        Some((name, _)) => name,
                        None => return,
                    },
                };
                this.open_web(remote, super::web::Page::Repo, w, cx)
            },
        ),
        (
            "git.web.request",
            tf!("创建{request}…", request).into(),
            need(has_web && on_branch, tr("远程地址不是托管平台的地址")),
            |this, w, cx| {
                let Some(branch) = this.status.as_ref().and_then(|s| s.branch.clone()) else {
                    return;
                };
                let remote = this.web_remote();
                this.open_web(remote, super::web::Page::NewRequest(branch), w, cx)
            },
        ),
        (
            "git.output",
            tr("查看输出").into(),
            None,
            |this, w, cx| {
                this.show_page(Page::Git, w, cx);
                this.open_output(None, cx)
            },
        ),
        (
            "git.activity",
            tr("活动：已执行的 git 命令").into(),
            None,
            |this, w, cx| this.show_activity(w, cx),
        ),
    ];
    table
        .into_iter()
        .map(|(id, name, why, run)| RepoCommand {
            id,
            name,
            why_not: reason(why),
            run,
        })
        .collect()
}

impl RepoView {
    /// 命令面板里列的分支和标签：当前分支在最前，再是别的本地分支、远程分支、标签。
    pub fn palette_refs(&self) -> Vec<PaletteRef> {
        let mut refs: Vec<PaletteRef> = Vec::new();
        let branch = |kind, b: &fr_core::refs::Branch| PaletteRef {
            kind,
            name: b.name.clone(),
            current: b.current && kind == RefKind::Branch,
            target: b.target.clone(),
        };
        refs.extend(
            self.refs
                .branches
                .iter()
                .filter(|b| b.current)
                .map(|b| branch(RefKind::Branch, b)),
        );
        refs.extend(
            self.refs
                .branches
                .iter()
                .filter(|b| !b.current)
                .map(|b| branch(RefKind::Branch, b)),
        );
        refs.extend(
            self.refs
                .remotes
                .iter()
                .filter(|b| !b.name.ends_with("/HEAD"))
                .map(|b| branch(RefKind::Remote, b)),
        );
        refs.extend(self.refs.tags.iter().map(|t| PaletteRef {
            kind: RefKind::Tag,
            name: t.name.clone(),
            current: false,
            target: t.target.clone(),
        }));
        refs
    }

    /// 对这个分支、标签能做的事，第一项是回车做的那件。
    pub fn palette_ref_actions(
        &self,
        target: &PaletteRef,
        cx: &App,
    ) -> Vec<(SharedString, RefAction)> {
        let cur = self.current_label();
        let name = target.name.as_str();
        let mut list: Vec<(SharedString, RefAction)> = Vec::new();
        match target.kind {
            RefKind::Branch if target.current => {}
            RefKind::Branch => list.push((tf!("切换到 {name}", name).into(), RefAction::Checkout)),
            RefKind::Remote => list.push((tr("检出为本地分支").into(), RefAction::Checkout)),
            RefKind::Tag => {}
        }
        list.push((tr("在提交图中定位").into(), RefAction::Reveal));
        if !target.current {
            list.push((
                tf!("合并 {name} 到 {cur}…", name, cur = cur.as_str()).into(),
                RefAction::Merge,
            ));
            list.push((
                tf!("将 {cur} 变基到 {name}…", cur = cur.as_str(), name).into(),
                RefAction::Rebase,
            ));
            list.push((
                tf!("与 {cur} 比较", cur = cur.as_str()).into(),
                RefAction::Compare,
            ));
        }
        if target.kind == RefKind::Branch {
            let pinned = self.pinned_branches(cx).iter().any(|pinned| pinned == name);
            list.push((
                match pinned {
                    true => tr("取消置顶").into(),
                    false => tr("置顶").into(),
                },
                RefAction::Pin,
            ));
            if !target.current {
                list.push((tr("删除…").into(), RefAction::Delete));
            }
        }
        list.push((tr("复制名称").into(), RefAction::CopyName));
        list
    }

    /// 做 `action`。面板关掉以后调。
    pub fn palette_ref_action(
        &mut self,
        target: &PaletteRef,
        action: RefAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = target.name.clone();
        let full = match target.kind {
            RefKind::Branch => format!("refs/heads/{name}"),
            RefKind::Remote => format!("refs/remotes/{name}"),
            RefKind::Tag => format!("refs/tags/{name}"),
        };
        if !matches!(action, RefAction::CopyName | RefAction::Pin) {
            self.show_page(Page::Git, window, cx);
        }
        match action {
            RefAction::Checkout => match target.kind {
                RefKind::Branch => self.checkout_branch(&name, window, cx),
                RefKind::Remote => self.checkout_remote(&name, window, cx),
                RefKind::Tag => self.palette_reveal(&target.target, window, cx),
            },
            RefAction::Reveal => self.palette_reveal(&target.target, window, cx),
            RefAction::Merge => self.merge(name, full, window, cx),
            RefAction::Rebase => self.rebase_onto(full, name, window, cx),
            RefAction::Compare => {
                let end = match target.kind {
                    RefKind::Branch => End::Branch(name),
                    RefKind::Remote => End::Remote(name),
                    RefKind::Tag => End::Tag(name),
                };
                let current = self.current_end();
                self.compare_ends(current, end, window, cx)
            }
            RefAction::Pin => self.toggle_pinned(name, cx),
            RefAction::Delete => self.delete_branch(name, window, cx),
            RefAction::CopyName => {
                cx.write_to_clipboard(ClipboardItem::new_string(name));
            }
        }
    }

    fn palette_reveal(&mut self, target: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.show_history_view(window, cx);
        self.reveal_commit(target, cx);
        cx.notify();
    }

    /// 读仓库里已跟踪的文件（`git ls-files`）；命令面板里输入了字、或者只看文件时才读。
    pub fn palette_files(&self, cx: &mut Context<Self>) -> Task<Result<Vec<SharedString>, String>> {
        let Some(channel) = self.channel.clone() else {
            return Task::ready(Err(tr("尚未连接").to_string()));
        };
        let read = channel.git(&self.entry.path, vec!["ls-files".into(), "-z".into()]);
        cx.background_spawn(async move {
            let out = read
                .await
                .and_then(|out| out.ok())
                .map_err(|err| format!("{err:#}"))?;
            let mut files: Vec<SharedString> = out
                .split(|b| *b == 0)
                .filter(|part| !part.is_empty())
                .take(FILES_MAX)
                .map(|part| SharedString::from(String::from_utf8_lossy(part).into_owned()))
                .collect();
            // 有冲突的文件每个版本各列一行（相邻）
            files.dedup();
            Ok(files)
        })
    }

    /// 对文件能做的事，第一项是回车做的那件。
    pub fn palette_file_actions() -> Vec<(SharedString, FileAction)> {
        vec![
            (tr("文件历史").into(), FileAction::History),
            (tr("逐行追溯（blame）").into(), FileAction::Blame),
            (tr("在文件页中打开").into(), FileAction::Open),
            (tr("复制路径").into(), FileAction::CopyPath),
        ]
    }

    pub fn palette_file_action(
        &mut self,
        path: String,
        action: FileAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            FileAction::History => {
                self.show_page(Page::Git, window, cx);
                self.show_file_history(path, window, cx)
            }
            FileAction::Blame => {
                self.show_page(Page::Git, window, cx);
                self.show_blame(path, cx)
            }
            FileAction::Open => self.show_file(path, window, cx),
            FileAction::CopyPath => cx.write_to_clipboard(ClipboardItem::new_string(path)),
        }
        cx.notify();
    }
}
