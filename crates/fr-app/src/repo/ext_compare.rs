//! 用外部工具比较、合并（认得哪些工具、命令怎么拼见 `ext_tools.rs`）。
//!
//! - 本地修改里的文件：未暂存的是「暂存区 ↔ 工作区」，已暂存的是「HEAD ↔ 暂存区」。
//! - 提交里改的文件：「父提交 ↔ 这个提交」。
//! - 有冲突的文件：外部合并工具，三份是暂存区里的 `:1:` 共同祖先、`:2:` 当前、`:3:` 传入。工具关了
//!   以后看结果里还有没有冲突标记，没有了就给「标记为已解决」。
//!
//! 工具在本机跑：要比的几份取到本机的临时目录。本机仓库的工作区文件直接给它真的路径（在工具里改了、
//! 存了就是改了文件）；服务器上的仓库合并时，结果先放在临时文件里，工具关了以后核对主机上的文件没被
//! 别的程序改过，再写回去。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fr_core::i18n::tr;
use fr_core::status::{Entry, Kind as StatusKind};
use fr_core::tf;
use fr_core::workfile;
use gpui_kit::component::button::Button;
use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::component::{Sizable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use super::changes::Area;
use super::ops::Step;
use crate::ext_tools::{self, Files, Kind};

/// 一份要比的内容从哪来。
#[derive(Clone, Debug)]
enum Side {
    /// 某个提交里的（`git show 提交:路径`）
    Rev(String),
    /// 暂存区里的；冲突时是第几份（1 共同祖先、2 当前、3 传入），0 是平常的
    Stage(u8),
    /// 工作区里的
    Worktree,
    /// 没有（新加的文件的旧版本、删掉的文件的新版本）
    Empty,
}

/// 合并结果放在哪；服务器上的仓库还有打开时主机上那个文件的散列和内容（写回前核对、看变没变）
type Merged = (PathBuf, Option<(String, Vec<u8>)>);

/// 每次比较一个新的临时目录
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// 这一次用的临时目录。这次运行第一次用时先清掉以前留下的（见 [`sweep_stale`]）。
fn temp_dir() -> PathBuf {
    static SWEPT: std::sync::Once = std::sync::Once::new();
    let root = std::env::temp_dir().join("kuai-compare");
    SWEPT.call_once(|| sweep_stale(&root));
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    root.join(format!("{}-{n}", std::process::id()))
}

/// 工具跑了不到这么久就退出：多半只是把文件交给了已经开着的窗口（没带 --wait 的 VS Code 这类），
/// 文件还在用，临时目录先不删
const QUICK_EXIT: Duration = Duration::from_secs(3);

/// 工具关了：这次取出来的几份（仓库里的代码，可能来自服务器）不留在临时目录里。
fn cleanup_after(dir: &Path, ran: Duration) {
    if ran >= QUICK_EXIT {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// 清掉以前的运行留下的临时目录（一天以前的；工具一下子就退出的、程序中途退出的会留下）。
fn sweep_stale(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mine = format!("{}-", std::process::id());
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&mine) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(24 * 3600));
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// 临时文件的名字：`名字.标记.扩展名`，工具按扩展名认语言。
fn temp_name(path: &str, tag: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}.{tag}.{ext}"),
        _ => format!("{name}.{tag}"),
    }
}

/// 有没有冲突标记（行首的 `<<<<<<<`、`>>>>>>>`、单独一行 `=======`）。
fn has_marker(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(bytes)
        .lines()
        .any(|line| line.starts_with("<<<<<<<") || line.starts_with(">>>>>>>") || line == "=======")
}

impl RepoView {
    /// 设置里选的外部比较、合并工具：（工具，自定义命令，名字）。
    fn ext_tool(&self, kind: Kind, cx: &App) -> (String, String, Option<&'static str>) {
        let settings = &self.app.read(cx).settings;
        let (tool, custom) = match kind {
            Kind::Diff => (&settings.diff_tool, &settings.diff_tool_cmd),
            Kind::Merge => (&settings.merge_tool, &settings.merge_tool_cmd),
        };
        (tool.clone(), custom.clone(), ext_tools::tool_name(tool))
    }

    /// 菜单上的写法：「用外部工具比较（Beyond Compare）」，没设置时「用外部工具比较…」。
    pub(super) fn ext_label(&self, kind: Kind, cx: &App) -> SharedString {
        let (_, _, name) = self.ext_tool(kind, cx);
        match (kind, name) {
            (Kind::Diff, Some(name)) => tf!("用外部工具比较（{name}）", name).into(),
            (Kind::Diff, None) => tr("用外部工具比较…").into(),
            (Kind::Merge, Some(name)) => tf!("用外部工具合并（{name}）", name).into(),
            (Kind::Merge, None) => tr("用外部工具合并…").into(),
        }
    }

    /// 本地修改里的一个文件，用外部工具比较。
    pub(super) fn external_diff_change(
        &mut self,
        area: Area,
        entry: Entry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = entry.path.clone();
        let (left, right, left_title, right_title) = match area {
            Area::Unstaged => {
                let left = match entry.unstaged {
                    Some(StatusKind::Untracked) => Side::Empty,
                    _ => Side::Stage(0),
                };
                let right = match entry.unstaged {
                    Some(StatusKind::Deleted) => Side::Empty,
                    _ => Side::Worktree,
                };
                (left, right, tr("暂存区"), tr("工作区"))
            }
            Area::Staged => {
                let left = match entry.staged {
                    Some(StatusKind::Added) => Side::Empty,
                    _ => Side::Rev("HEAD".into()),
                };
                let right = match entry.staged {
                    Some(StatusKind::Deleted) => Side::Empty,
                    _ => Side::Stage(0),
                };
                (left, right, "HEAD", tr("暂存区"))
            }
        };
        let orig = entry.orig_path.clone().unwrap_or_else(|| path.clone());
        self.launch_diff(
            (left, orig),
            (right, path.clone()),
            (
                format!("{path} ({left_title})"),
                format!("{path} ({right_title})"),
            ),
            window,
            cx,
        );
    }

    /// 提交里改的一个文件，用外部工具比较：第一个父提交 ↔ 这个提交。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn external_diff_commit(
        &mut self,
        commit: String,
        parent: Option<String>,
        path: String,
        orig: Option<String>,
        letter: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let short = |id: &str| id.chars().take(8).collect::<String>();
        let left = match (&parent, letter) {
            (Some(parent), l) if l != "A" => Side::Rev(parent.clone()),
            _ => Side::Empty,
        };
        let right = match letter {
            "D" => Side::Empty,
            _ => Side::Rev(commit.clone()),
        };
        let titles = (
            format!(
                "{path} ({})",
                parent.as_deref().map(short).unwrap_or_default()
            ),
            format!("{path} ({})", short(&commit)),
        );
        let old = orig.unwrap_or_else(|| path.clone());
        self.launch_diff((left, old), (right, path), titles, window, cx);
    }

    /// 取出两份，起比较工具（不等它关）。
    fn launch_diff(
        &mut self,
        (left, left_path): (Side, String),
        (right, right_path): (Side, String),
        (left_title, right_title): (String, String),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (tool, custom, _) = self.ext_tool(Kind::Diff, cx);
        if tool.is_empty() {
            self.ext_not_set(Kind::Diff, window, cx);
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        let local = self.entry.server.is_none();
        cx.spawn_in(window, async move |_, cx| {
            let dir = temp_dir();
            let local_file =
                materialize(&channel, &repo, local, &left, &left_path, &dir, "LEFT").await;
            let remote_file =
                materialize(&channel, &repo, local, &right, &right_path, &dir, "RIGHT").await;
            cx.update(|window, cx| {
                let files = match (local_file, remote_file) {
                    (Ok(local), Ok(remote)) => Files {
                        local,
                        remote,
                        local_title: left_title,
                        remote_title: right_title,
                        ..Default::default()
                    },
                    (Err(err), _) | (_, Err(err)) => {
                        let text = tf!("无法取出要比较的文件：{err}", err = format!("{err:#}"));
                        window.push_notification(Notification::error(text).autohide(false), cx);
                        return;
                    }
                };
                let started = ext_tools::command(Kind::Diff, &tool, &custom, &files)
                    .and_then(|mut command| spawn(&mut command));
                match started {
                    // 在别的线程上等工具关，关了删掉取出来的文件
                    Ok(mut child) => {
                        let dir = dir.clone();
                        std::thread::spawn(move || {
                            let begun = Instant::now();
                            let _ = child.wait();
                            cleanup_after(&dir, begun.elapsed());
                        });
                    }
                    Err(err) => {
                        let _ = std::fs::remove_dir_all(&dir);
                        window.push_notification(Notification::error(err).autohide(false), cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// 有冲突的文件，用外部合并工具解决。工具关了以后看结果。
    pub(super) fn external_merge(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (tool, custom, _) = self.ext_tool(Kind::Merge, cx);
        if tool.is_empty() {
            self.ext_not_set(Kind::Merge, window, cx);
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        let local = self.entry.server.is_none();
        cx.spawn_in(window, async move |this, cx| {
            let dir = temp_dir();
            let base =
                materialize(&channel, &repo, local, &Side::Stage(1), &path, &dir, "BASE").await;
            let ours = materialize(
                &channel,
                &repo,
                local,
                &Side::Stage(2),
                &path,
                &dir,
                "LOCAL",
            )
            .await;
            let theirs = materialize(
                &channel,
                &repo,
                local,
                &Side::Stage(3),
                &path,
                &dir,
                "REMOTE",
            )
            .await;
            // 结果：本机仓库是工作区里的文件；服务器上的先放临时文件，记下主机上的散列（写回前核对）
            let merged: anyhow::Result<Merged> = match local {
                true => Ok((Path::new(&repo).join(&path), None)),
                false => {
                    async {
                        let out = channel.git(&repo, workfile::read_args(&path)).await?.ok()?;
                        let (hash, bytes) = workfile::split_read(&out)
                            .ok_or_else(|| anyhow::anyhow!(tr("无法识别读取结果")))?;
                        let file = dir.join(temp_name(&path, "MERGED"));
                        std::fs::write(&file, bytes)?;
                        Ok((file, Some((hash, bytes.to_vec()))))
                    }
                    .await
                }
            };
            let (files, merged_file, remote) = match (base, ours, theirs, merged) {
                (Ok(base), Ok(ours), Ok(theirs), Ok((merged, remote))) => (
                    Files {
                        local: ours,
                        remote: theirs,
                        base: Some(base),
                        merged: Some(merged.clone()),
                        local_title: tr("当前").into(),
                        remote_title: tr("传入").into(),
                        base_title: tr("共同祖先").into(),
                        merged_title: tf!(
                            "合并结果：{name}",
                            name = path.rsplit('/').next().unwrap_or(&path)
                        ),
                    },
                    merged,
                    remote,
                ),
                (Err(err), ..) | (_, Err(err), ..) | (_, _, Err(err), _) | (.., Err(err)) => {
                    cx.update(|window, cx| {
                        let text = tf!("无法取出要合并的文件：{err}", err = format!("{err:#}"));
                        window.push_notification(Notification::error(text).autohide(false), cx);
                    })
                    .ok();
                    return;
                }
            };
            // 起工具，在别的线程上等它关
            let child = cx.update(|window, cx| {
                match ext_tools::command(Kind::Merge, &tool, &custom, &files)
                    .and_then(|mut c| spawn(&mut c))
                {
                    Ok(child) => Some(child),
                    Err(err) => {
                        window.push_notification(Notification::error(err).autohide(false), cx);
                        None
                    }
                }
            });
            let Ok(Some(mut child)) = child else {
                return;
            };
            let (done, wait) = futures::channel::oneshot::channel();
            let begun = Instant::now();
            std::thread::spawn(move || {
                let _ = child.wait();
                let _ = done.send(());
            });
            let _ = wait.await;
            let ran = begun.elapsed();
            let result = std::fs::read(&merged_file).unwrap_or_default();
            // 服务器上的：内容变了才写回，写之前核对主机上的文件没被别的程序改过
            let written: Result<bool, String> = match &remote {
                Some((hash, before)) if *before != result => {
                    let changed = tr("文件在合并期间已被其他程序修改，未写回。");
                    let args = workfile::write_args(&path, hash, changed);
                    match channel
                        .git_with_input(&repo, args, Some(result.clone()))
                        .await
                    {
                        Ok(out) if out.success() => Ok(true),
                        Ok(out) => Err(out.stderr.trim().to_string()),
                        Err(err) => Err(format!("{err:#}")),
                    }
                }
                Some(_) => Ok(false),
                None => Ok(true),
            };
            // 结果已经读出来、写回了：取出来的几份不留（本机仓库的结果是工作区里的文件，不在这里）。
            // 没写回去的留着：临时目录里那份合并结果是用户做的活，只剩这一份
            if written.is_ok() {
                cleanup_after(&dir, ran);
            }
            this.update_in(cx, |this, window, cx| {
                this.refresh(cx);
                match written {
                    Err(err) => {
                        // 合并结果只剩临时目录里那一份：写明在哪
                        let text = tf!(
                            "合并结果未写回：{err}
合并结果保存在 {file}",
                            err,
                            file = merged_file.display()
                        );
                        window.push_notification(Notification::error(text).autohide(false), cx);
                    }
                    Ok(_) if has_marker(&result) => {
                        let text = tf!(
                            "{path} 仍有冲突标记。可再次用外部工具或在合并编辑器中解决。",
                            path = path.as_str()
                        );
                        // 用户是从工具的窗口切回来的：不自动消失，免得没看到
                        window.push_notification(Notification::warning(text).autohide(false), cx);
                    }
                    Ok(_) => {
                        let view = cx.entity().downgrade();
                        let file = path.clone();
                        // 带着「标记为已解决」：不自动消失，切回来时按钮还在
                        let note = crate::ui::action_note(
                            NotificationType::Success,
                            tf!("{path} 已在外部工具中合并", path = path.as_str()),
                            move |_, _, cx| {
                                let (view, file, me) =
                                    (view.clone(), file.clone(), cx.entity().downgrade());
                                Button::new("ext-merge-resolved")
                                    .small()
                                    .label(tr("标记为已解决"))
                                    .on_click(move |_, window, cx| {
                                        let file = file.clone();
                                        view.update(cx, |this, cx| {
                                            let add = vec![Step::local(vec![
                                                "add".into(),
                                                "--".into(),
                                                file.clone(),
                                            ])];
                                            this.run_seq(
                                                tr("标记为已解决"),
                                                add,
                                                window,
                                                cx,
                                                move |_, _, w, cx| {
                                                    let text = tf!(
                                                        "{path} 已标记为已解决",
                                                        path = file.as_str()
                                                    );
                                                    w.push_notification(
                                                        Notification::success(text),
                                                        cx,
                                                    );
                                                },
                                            );
                                        })
                                        .ok();
                                        me.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                                    })
                            },
                        );
                        window.push_notification(note, cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// 没设置外部工具：说一声，给个按钮去设置。
    fn ext_not_set(&mut self, kind: Kind, window: &mut Window, cx: &mut Context<Self>) {
        let text = match kind {
            Kind::Diff => tr("尚未设置外部比较工具。请在「设置 → 外部工具」中选择。"),
            Kind::Merge => tr("尚未设置外部合并工具。请在「设置 → 外部工具」中选择。"),
        };
        let note = crate::ui::action_note(NotificationType::Info, text, |_, _, cx| {
            let me = cx.entity().downgrade();
            Button::new("ext-open-settings")
                .small()
                .label(tr("打开设置"))
                .on_click(move |_, window, cx| {
                    window.dispatch_action(Box::new(crate::OpenSettings), cx);
                    me.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                })
        });
        window.push_notification(note, cx);
    }
}

/// 起工具；Windows 上不弹黑框（`code.cmd` 这种批处理会弹）。
fn spawn(command: &mut std::process::Command) -> Result<std::process::Child, String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    command
        .spawn()
        .map_err(|err| tf!("无法启动外部工具：{err}", err))
}

/// 把一份内容放到本机：本机仓库的工作区文件直接用真的路径，别的取出来写进临时目录。
async fn materialize(
    channel: &fr_channel::Channel,
    repo: &str,
    local: bool,
    side: &Side,
    path: &str,
    dir: &Path,
    tag: &str,
) -> anyhow::Result<PathBuf> {
    if let (Side::Worktree, true) = (side, local) {
        return Ok(Path::new(repo).join(path));
    }
    let bytes: Vec<u8> = match side {
        Side::Empty => Vec::new(),
        Side::Rev(rev) => show(channel, repo, &format!("{rev}:{path}")).await,
        Side::Stage(0) => show(channel, repo, &format!(":{path}")).await,
        Side::Stage(n) => show(channel, repo, &format!(":{n}:{path}")).await,
        Side::Worktree => {
            let out = channel.git(repo, workfile::read_args(path)).await?.ok()?;
            workfile::split_read(&out)
                .map(|(_, bytes)| bytes.to_vec())
                .ok_or_else(|| anyhow::anyhow!(tr("无法识别读取结果")))?
        }
    };
    std::fs::create_dir_all(dir)?;
    let file = dir.join(temp_name(path, tag));
    std::fs::write(&file, bytes)?;
    Ok(file)
}

/// `git show 对象`；没有这一份（两边都新加的文件没有共同祖先）算空的。
async fn show(channel: &fr_channel::Channel, repo: &str, object: &str) -> Vec<u8> {
    channel
        .git(repo, vec!["show".into(), object.into()])
        .await
        .ok()
        .and_then(|out| out.ok().ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{has_marker, temp_name};

    #[test]
    fn 临时文件的名字() {
        assert_eq!(temp_name("src/main.rs", "LOCAL"), "main.LOCAL.rs");
        assert_eq!(temp_name("Makefile", "BASE"), "Makefile.BASE");
        assert_eq!(temp_name(".gitignore", "REMOTE"), ".gitignore.REMOTE");
        assert_eq!(temp_name("a/b.tar.gz", "LEFT"), "b.tar.LEFT.gz");
    }

    #[test]
    fn 认得冲突标记() {
        assert!(has_marker(b"a\n<<<<<<< HEAD\nb\n"));
        assert!(!has_marker(b"a\nb\n"));
    }
}
