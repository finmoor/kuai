//! 做完当场能退：改写历史的操作（变基、整理提交、重置、修改提交、合并、拉取、并入之前的提交、
//! 按提交应用补丁）成功的通知里带「撤销」，回到操作前的位置；合并前预判会不会冲突；解决错了的
//! 冲突恢复成冲突状态重来。
//!
//! 撤销是 `git reset --keep <操作前的提交>`：只挪分支和暂存区，工作区里没提交的改动留着；有改动
//! 会被盖掉时 git 自己拒绝。点的时候先核对分支还停在操作后的位置（之后又提交、又切了分支就不做）。
//! 通知收掉以后去侧栏的「找回提交」，和原来一样。

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::*;

use super::RepoView;
use super::ops::{Check, Step, args};
use crate::ui;

/// 操作前的位置。
#[derive(Clone, Debug)]
pub(super) struct UndoPoint {
    /// 在哪个分支上；分离的 HEAD 是 None
    pub branch: Option<String>,
    /// 操作前 HEAD 指向的提交
    pub before: String,
    /// 撤销时改动留在暂存区（`reset --soft`）：修改上次提交、并入之前的提交这类把暂存的改动写进
    /// 了提交的，撤销以后改动要还在。别的用 `--keep`
    pub soft: bool,
    /// 跟着一起动了的别的分支（整理提交时「一并移动」的）：（完整引用名，操作前指着的提交）
    pub refs: Vec<(String, String)>,
}

impl UndoPoint {
    pub(super) fn soft(self) -> Self {
        Self { soft: true, ..self }
    }

    pub(super) fn with_refs(self, refs: Vec<(String, String)>) -> Self {
        Self { refs, ..self }
    }
}

/// 「可撤销」的那条通知的标识：同一时间只留一条，推送以后收掉（已经推出去的不提供撤销）。
pub(super) struct UndoNote;

/// 合并前预判冲突要的 git 版本（`git merge-tree --write-tree`）
const MERGE_TREE_GIT: (u32, u32, u32) = (2, 38, 0);

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

impl RepoView {
    /// 记下现在的位置（按读到的状态）：操作做完后交给 [`Self::notify_undoable`]。还没有提交时没有。
    pub(super) fn undo_point(&self) -> Option<UndoPoint> {
        let status = self.status.as_ref()?;
        Some(UndoPoint {
            branch: status.branch.clone(),
            before: status.head.clone()?,
            soft: false,
            refs: Vec::new(),
        })
    }

    /// 变基停在一半时的「操作前」：变基开始前的 HEAD 和那条分支（变基期间 HEAD 是分离的）。
    pub(super) fn rebase_undo_point(&self) -> Option<UndoPoint> {
        let paused = self.rebase.as_ref()?;
        let refs = paused
            .moved
            .iter()
            .filter(|(name, _)| Some(name) != paused.branch.as_ref())
            .cloned()
            .collect();
        Some(UndoPoint {
            branch: paused
                .branch
                .as_ref()
                .and_then(|name| name.strip_prefix("refs/heads/"))
                .map(str::to_string),
            before: paused.orig_head.clone()?,
            soft: false,
            refs,
        })
    }

    /// 操作成功的通知，带「撤销」。`point` 是操作前的位置；位置没变（什么都没改）或者没记到的，
    /// 就是一条普通的成功通知。
    pub(super) fn notify_undoable(
        &mut self,
        text: impl Into<SharedString>,
        point: Option<UndoPoint>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text: SharedString = text.into();
        let (Some(point), Some(channel)) = (point, self.channel.clone()) else {
            window.push_notification(Notification::success(text), cx);
            return;
        };
        let read = channel.git(
            &self.entry.path,
            args(&["rev-parse", "-q", "--verify", "HEAD"]),
        );
        // 一并动了的分支现在在哪（撤销时核对还在这里才退回去）
        let read_refs = (!point.refs.is_empty()).then(|| {
            let mut list = args(&["for-each-ref", "--format=%(refname) %(objectname)"]);
            list.extend(point.refs.iter().map(|(name, _)| name.clone()));
            channel.git(&self.entry.path, list)
        });
        cx.spawn_in(window, async move |this, cx| {
            let after = read
                .await
                .ok()
                .filter(|out| out.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .filter(|id| !id.is_empty());
            let now = match read_refs {
                Some(read) => read
                    .await
                    .ok()
                    .filter(|out| out.success())
                    .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
                    .unwrap_or_default(),
                None => String::new(),
            };
            // （引用名，操作前，现在）：只留真的动了的
            let moved: Vec<(String, String, String)> = point
                .refs
                .iter()
                .filter_map(|(name, before)| {
                    let current = now.lines().find_map(|line| {
                        let (refname, id) = line.split_once(' ')?;
                        (refname == name).then(|| id.trim().to_string())
                    })?;
                    (current != *before).then(|| (name.clone(), before.clone(), current))
                })
                .collect();
            this.update_in(cx, |_, window, cx| {
                let Some(after) = after.filter(|after| *after != point.before) else {
                    window.push_notification(Notification::success(text), cx);
                    return;
                };
                let view = cx.entity().downgrade();
                let note = ui::action_note(NotificationType::Success, text, move |_, _, cx| {
                    let (view, me) = (view.clone(), cx.entity().downgrade());
                    let (point, after, moved) = (point.clone(), after.clone(), moved.clone());
                    Button::new("undo-op")
                        .label(tr("撤销"))
                        .tooltip(match point.soft {
                            true => tf!(
                                "回到操作前的位置 {id}；写进提交的改动放回暂存区",
                                id = short(&point.before)
                            ),
                            false => tf!(
                                "回到操作前的位置 {id}；未提交的改动保留",
                                id = short(&point.before)
                            ),
                        })
                        .on_click(move |_, window, cx| {
                            me.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                            let (point, after, moved) =
                                (point.clone(), after.clone(), moved.clone());
                            view.update(cx, |this, cx| {
                                this.undo_to(point, after, moved, window, cx)
                            })
                            .ok();
                        })
                })
                .id::<UndoNote>();
                window.push_notification(note, cx);
            })
            .ok();
        })
        .detach();
    }

    /// 撤销：分支还停在操作后的位置（`after`）才做，回到 `point`。`moved` 是跟着动了的别的分支
    /// （引用名，操作前，操作后），也还在操作后的位置才一起退回去。
    fn undo_to(
        &mut self,
        point: UndoPoint,
        after: String,
        moved: Vec<(String, String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.in_progress {
            let text = tf!("{name}尚未完成，请先完成或中止。", name = state.name());
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let branch = point.branch.as_ref().map(|b| format!("refs/heads/{b}"));
        let mut checks: Vec<Check> = vec![
            (args(&["symbolic-ref", "-q", "HEAD"]), branch),
            (args(&["rev-parse", "-q", "--verify", "HEAD"]), Some(after)),
        ];
        checks.extend(moved.iter().map(|(name, _, after)| {
            (
                args(&["rev-parse", "-q", "--verify", name]),
                Some(after.clone()),
            )
        }));
        let mode = if point.soft { "--soft" } else { "--keep" };
        let mut steps = vec![Step::local(vec![
            "reset".into(),
            mode.into(),
            point.before.clone(),
        ])];
        // 带着操作后的值：git 再核对一遍，中间被动过就不改
        steps.extend(
            moved.iter().map(|(name, before, after)| {
                Step::local(args(&["update-ref", name, before, after]))
            }),
        );
        let count = moved.len();
        self.verify_then(checks, window, cx, move |this, window, cx| {
            let point = point.clone();
            this.run_seq(
                tr("撤销"),
                steps.clone(),
                window,
                cx,
                move |_, _, window, cx| {
                    let id = short(&point.before);
                    let text = match (&point.branch, count) {
                        (Some(branch), 0) => tf!("已撤销：{branch} 回到 {id}", branch, id),
                        (Some(branch), n) => tf!(
                            "已撤销：{branch} 回到 {id}，一并移动的 {n} 个分支也已退回",
                            branch,
                            id,
                            n
                        ),
                        (None, _) => tf!("已撤销：HEAD 回到 {id}", id),
                    };
                    window.push_notification(Notification::success(text), cx);
                },
            );
        });
    }

    /// 读有哪些文件是解决过冲突的（暂存区里还记着冲突时的三份，能恢复成冲突状态）。只在合并、
    /// 变基这些停在一半时读；刷新时调。
    pub(super) fn read_resolved(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone().filter(|_| self.in_progress.is_some()) else {
            self.resolved.clear();
            self.resolved_task = None;
            return;
        };
        let read = channel.git(
            &self.entry.path,
            args(&["ls-files", "--resolve-undo", "-z"]),
        );
        self.resolved_task = Some(cx.spawn(async move |this, cx| {
            let files = read
                .await
                .ok()
                .filter(|out| out.success())
                .map(|out| parse_resolve_undo(&out.stdout))
                .unwrap_or_default();
            this.update(cx, |this, cx| {
                if this.resolved != files {
                    this.resolved = files;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// 这个文件的冲突解决过、还能恢复成冲突状态重来。
    pub(super) fn can_redo_conflict(&self, path: &str) -> bool {
        self.in_progress.is_some() && self.resolved.contains(path)
    }

    /// 「重新解决冲突…」：确认后把文件恢复成冲突状态（带冲突标记），刚才解决的结果不要了。
    pub(super) fn redo_conflict(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = crate::settings::base_name(&path).to_string();
        let description = tf!(
            "{path} 将恢复为冲突状态（带冲突标记），刚才的解决结果将被丢弃。",
            path = path.as_str()
        );
        self.confirm(
            tf!("重新解决 {name} 的冲突？", name),
            description,
            tr("恢复为冲突状态"),
            window,
            cx,
            move |this, window, cx| {
                let path = path.clone();
                let checkout = args(&["checkout", "-m", "--", &path]);
                this.run_op(
                    tr("重新解决冲突"),
                    checkout,
                    None,
                    window,
                    cx,
                    move |this, _, window, cx| {
                        this.show_changes(cx);
                        this.select_change(&path, cx);
                        let text = tf!(
                            "{path} 已恢复为冲突状态，请重新解决。",
                            path = path.as_str()
                        );
                        window.push_notification(Notification::info(text), cx);
                    },
                );
            },
        );
    }

    /// 合并前预判：把 `rev` 合进当前 HEAD 会不会冲突，冲突在哪几个文件。在对象库里试合一遍，不碰
    /// 工作区和暂存区。返回 None 是不知道（主机上的 git 低于 2.38、命令出错）。
    pub(super) fn predict_merge(
        &self,
        rev: String,
        cx: &mut Context<Self>,
    ) -> Task<Option<Vec<String>>> {
        let Some(channel) = self.channel.clone().filter(|channel| {
            channel
                .git_version()
                .is_some_and(|version| version >= MERGE_TREE_GIT)
        }) else {
            return Task::ready(None);
        };
        let read = channel.git(
            &self.entry.path,
            args(&[
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                "-z",
                "HEAD",
                &rev,
            ]),
        );
        cx.background_spawn(async move {
            let out = read.await.ok()?;
            parse_merge_tree(out.code, &out.stdout)
        })
    }
}

/// `git ls-files --resolve-undo -z` 的输出里有哪些文件：每个文件有两三条（共同祖先、两边各一条），
/// 每条是 `模式 对象 阶段\t路径`。
fn parse_resolve_undo(out: &[u8]) -> std::collections::HashSet<String> {
    out.split(|b| *b == 0)
        .filter_map(|record| {
            let text = String::from_utf8_lossy(record);
            text.split_once('\t').map(|(_, path)| path.to_string())
        })
        .filter(|path| !path.is_empty())
        .collect()
}

/// 解析 `git merge-tree --write-tree --name-only --no-messages -z` 的结果：退出码 0 是没有冲突，
/// 1 是有（输出是合出来的树，接着是冲突的文件，NUL 分隔），别的是出错。
fn parse_merge_tree(code: Option<i32>, stdout: &[u8]) -> Option<Vec<String>> {
    // 第一段是合并出来的树。认不出要合并的提交时退出码也是 1，但什么都不输出：
    // 那不是「有冲突」，更不是「没有冲突」
    let mut fields = stdout.split(|b| *b == 0);
    let tree = fields.next()?;
    if tree.len() < 40 || !tree.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    match code {
        Some(0) => Some(Vec::new()),
        Some(1) => {
            let mut files: Vec<String> = fields
                .filter(|name| !name.is_empty())
                .map(|name| String::from_utf8_lossy(name).into_owned())
                .collect();
            files.dedup();
            Some(files)
        }
        _ => None,
    }
}

/// 预判的结果写成确认框里的一句话。
pub(super) fn predict_text(conflicts: &[String]) -> String {
    const SHOWN: usize = 3;
    if conflicts.is_empty() {
        return tr("预计无冲突。").to_string();
    }
    let mut names: Vec<&str> = conflicts.iter().take(SHOWN).map(String::as_str).collect();
    let more = conflicts.len().saturating_sub(SHOWN);
    let rest = tf!("等 {n} 个", n = conflicts.len());
    if more > 0 {
        names.push(&rest);
    }
    tf!(
        "预计 {n} 个文件有冲突：{names}。合并将暂停，在「本地修改」中解决后继续。",
        n = conflicts.len(),
        names = names.join(tr("、"))
    )
}

#[cfg(test)]
mod tests {
    use super::{parse_merge_tree, parse_resolve_undo, predict_text};

    #[test]
    fn 读出解决过冲突的文件() {
        let out = b"100644 aaa 1\tsrc/a.rs\x00100644 bbb 2\tsrc/a.rs\x00100644 ccc 3\tsrc/a.rs\x00100644 ddd 2\tREADME.md\x00100644 eee 3\tREADME.md\x00";
        let files = parse_resolve_undo(out);
        assert_eq!(files.len(), 2);
        assert!(files.contains("src/a.rs") && files.contains("README.md"));
        assert!(parse_resolve_undo(b"").is_empty());
    }

    #[test]
    fn 预判合并的结果() {
        let tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        assert_eq!(
            parse_merge_tree(Some(0), format!("{tree}\0").as_bytes()),
            Some(vec![])
        );
        assert_eq!(
            parse_merge_tree(Some(1), format!("{tree}\0src/a.rs\0README.md\0").as_bytes()),
            Some(vec!["src/a.rs".to_string(), "README.md".to_string()])
        );
        // 认不出要合并的提交：退出码 1、没有输出，不能当成有结论
        assert_eq!(parse_merge_tree(Some(1), b""), None);
        assert_eq!(parse_merge_tree(Some(128), b""), None);
        assert_eq!(parse_merge_tree(None, b""), None);
    }

    #[test]
    fn 预判写成一句话() {
        assert_eq!(predict_text(&[]), "预计无冲突。");
        let two = ["a.rs".to_string(), "b.rs".to_string()];
        assert!(predict_text(&two).contains("预计 2 个文件有冲突：a.rs、b.rs。"));
        let five: Vec<String> = (1..=5).map(|n| format!("f{n}")).collect();
        assert!(predict_text(&five).contains("f1、f2、f3、等 5 个。"));
    }

    /// 真的跑一遍：两条分支改了同一行，预判说得出是哪个文件，工作区不动。
    #[test]
    fn 对着真仓库预判() {
        let dir = std::env::temp_dir().join(format!("fr-merge-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| fr_core::git::run(&dir, args).unwrap();
        let write = |name: &str, text: &str| std::fs::write(dir.join(name), text).unwrap();
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "user.email", "t@example.com"]);
        write("a.txt", "one\n");
        write("b.txt", "one\n");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "side"]);
        write("a.txt", "side\n");
        write("c.txt", "side\n");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "side"]);
        git(&["checkout", "-q", "main"]);
        write("b.txt", "main\n");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "main 改 b"]);
        let run = |rev: &str| {
            let args: Vec<String> = [
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                "-z",
                "HEAD",
                rev,
            ]
            .map(String::from)
            .to_vec();
            let (running, _) = fr_core::exec::spawn(&dir, &args, None).unwrap();
            let (out, finished) = running.collect().unwrap();
            parse_merge_tree(finished.code, &out)
        };
        // 改的是不同的文件：没有冲突
        assert_eq!(run("side"), Some(vec![]));
        write("a.txt", "main\n");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "main 改 a"]);
        assert_eq!(run("side"), Some(vec!["a.txt".to_string()]));
        assert_eq!(run("no-such-branch"), None);
        // 工作区、暂存区都没动
        assert!(git(&["status", "--porcelain"]).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
