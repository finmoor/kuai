//! 把单个文件改回某个版本：提交详情（改动的文件、全部文件）和文件历史里，文件的右键菜单有
//! 「恢复到此提交中的版本…」「恢复到此提交之前的版本…」。后一项只把这个提交对该文件的改动退回去：
//! 一个提交改了十个文件、其中一个改坏了，就用它。
//!
//! 只改写工作区里的这一个文件（`git restore --source=<提交> --worktree`），不暂存、不提交，结果在
//! 「本地修改」里看。覆盖之前存一份（和丢弃同一套，见 `discard.rs`），通知里可以撤销。

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::*;

use super::RepoView;
use super::impact::{Impact, Row, Tone};
use super::ops::{Step, args};

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// 文件菜单里的两项。`commit` 是正在看的那个提交。
pub(super) fn restore_items(
    menu: PopupMenu,
    view: &WeakEntity<RepoView>,
    path: &str,
    commit: &str,
) -> PopupMenu {
    let item = |label: &'static str, before: bool| {
        let (view, path, commit) = (view.clone(), path.to_string(), commit.to_string());
        PopupMenuItem::new(label).on_click(move |_, window, cx| {
            let (path, commit) = (path.clone(), commit.clone());
            view.update(cx, |this, cx| {
                this.restore_file_dialog(path, commit, before, window, cx)
            })
            .ok();
        })
    };
    menu.item(item(tr("恢复到此提交中的版本…"), false))
        .item(item(tr("恢复到此提交之前的版本…"), true))
}

impl RepoView {
    /// 确认后把 `path` 恢复成 `commit` 里的样子（`before` 为真是它的第一个父提交里的样子）。
    pub(super) fn restore_file_dialog(
        &mut self,
        path: String,
        commit: String,
        before: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        let rev = match before {
            true => format!("{commit}^"),
            false => commit.clone(),
        };
        // 目标版本是哪个提交（之前的版本要写出父提交）、里面有没有这个文件
        let resolve = channel.git(&repo, args(&["rev-parse", "-q", "--verify", &rev]));
        let object = format!("{rev}:{path}");
        let exists = channel.git(&repo, args(&["cat-file", "-e", &object]));
        let subject = channel.git(&repo, args(&["log", "-1", "--format=%s", &commit, "--"]));
        cx.spawn_in(window, async move |this, cx| {
            let (resolve, exists, subject) = futures::join!(resolve, exists, subject);
            let target = resolve
                .ok()
                .filter(|out| out.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
            let exists = target.is_some() && exists.is_ok_and(|out| out.success());
            let subject = subject
                .ok()
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .unwrap_or_default();
            this.update_in(cx, |this, window, cx| {
                this.confirm_restore(path, commit, subject, target, exists, before, window, cx)
            })
            .ok();
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn confirm_restore(
        &mut self,
        path: String,
        commit: String,
        subject: String,
        target: Option<String>,
        exists: bool,
        before: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = crate::settings::base_name(&path).to_string();
        // 工作区里这个文件现在有没有没提交的改动、在不在
        let entry = self
            .status
            .as_ref()
            .and_then(|status| status.entries.iter().find(|entry| entry.path == path));
        let dirty = entry.is_some();
        let version = match (before, &target) {
            (false, _) => tf!(
                "{id}「{subject}」中的版本",
                id = short(&commit),
                subject = subject.as_str()
            ),
            (true, Some(parent)) => tf!(
                "{id}「{subject}」之前的版本（{parent}）",
                id = short(&commit),
                subject = subject.as_str(),
                parent = short(parent)
            ),
            (true, None) => tf!(
                "{id}「{subject}」之前（它是最早的提交，之前没有任何文件）",
                id = short(&commit),
                subject = subject.as_str()
            ),
        };
        let mut rows = vec![
            Row::new(Tone::Plain, IconName::File, "文件", path.clone()),
            Row::new(Tone::Plain, IconName::GitCommitHorizontal, "版本", version),
        ];
        if !exists {
            rows.push(Row::new(
                Tone::Warn,
                IconName::CircleMinus,
                "结果",
                tr("此版本中没有这个文件：将从工作区删除。"),
            ));
        }
        if dirty {
            rows.push(Row::new(
                Tone::Warn,
                IconName::TriangleAlert,
                "未提交的改动",
                tr("该文件有未提交的改动，将被覆盖；完成后的通知里可撤销。"),
            ));
        }
        let impact = Impact {
            description: tr(
                "只改写工作区中的这一个文件，不暂存、不提交。结果在「本地修改」中查看。",
            )
            .into(),
            rows,
        };
        let title = tf!("恢复 {name}？", name = name.as_str());
        // 确认框开着时文件又被改了就不覆盖：照确认时的内容再核对一遍
        let guard = self.guard_content(vec![path.clone()], Vec::new(), cx);
        self.confirm_impact(
            title,
            impact,
            tr("恢复"),
            false,
            window,
            cx,
            move |this, window, cx| {
                let (path, commit, name) = (path.clone(), commit.clone(), name.clone());
                let step = match (exists, before) {
                    (true, true) => {
                        let source = format!("--source={commit}^");
                        Step::local(args(&["restore", &source, "--worktree", "--", &path]))
                    }
                    (true, false) => {
                        let source = format!("--source={commit}");
                        Step::local(args(&["restore", &source, "--worktree", "--", &path]))
                    }
                    (false, _) => Step::local(fr_core::workfile::remove_args(&path))
                        .shown(format!("rm {}", fr_core::git::sh_quote(&path))),
                };
                let done = match before {
                    false => tf!(
                        "已将 {name} 恢复为 {id} 中的版本",
                        name = name.as_str(),
                        id = short(&commit)
                    ),
                    true => tf!(
                        "已将 {name} 恢复为 {id} 之前的版本",
                        name = name.as_str(),
                        id = short(&commit)
                    ),
                };
                let run = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                    this.run_with_undo(
                        tr("恢复文件"),
                        vec![path.clone()],
                        vec![step.clone()],
                        done.clone(),
                        Some(path.clone()),
                        window,
                        cx,
                    );
                };
                match guard.clone() {
                    Some(guard) => {
                        let changed = tr("文件在确认期间又被修改，未恢复。请查看最新内容后重试。");
                        this.verify_content_then(guard, changed, window, cx, run)
                    }
                    None => run(this, window, cx),
                }
            },
        );
    }
}
