//! 补丁：把改动导出成补丁文件、复制成补丁，把补丁应用到仓库。
//!
//! 仓库在主机上，补丁文件在本机：导出是在主机上生成、取回本机另存（或放进剪贴板）；应用是在本机
//! 读文件（或剪贴板），内容经标准输入交给主机上的 git，不用先把文件传上去。所以「复制为补丁，换到
//! 另一台主机上的仓库，从剪贴板应用」就能在两台主机之间搬改动，不用先推送。
//!
//! 应用之前先预检（[`fr_core::patchfile`]）：逐个文件说能不能应用。补丁不会部分应用；有文件和现在
//! 的内容对不上时，勾了三方合并才能应用，冲突留在文件里解决。带提交的补丁可以原样变回提交。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use fr_core::i18n::tr;
use fr_core::patchfile::{self, Fit, MailCommit, PatchFile};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::component::radio::Radio;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use super::discard::Overwrite;
use super::ops::{Step, args};
use crate::ui;

/// 补丁从哪来。
#[derive(Clone)]
pub(super) enum Source {
    /// 未暂存的改动；路径空着是全部。`untracked` 是其中未跟踪的文件（`git diff` 不带它们，要一个个取）
    Unstaged {
        paths: Vec<String>,
        untracked: Vec<String>,
    },
    /// 已暂存的改动；路径空着是全部
    Staged(Vec<String>),
    /// 一个或几个提交（带作者、时间、说明）；新的在前
    Commits(Vec<String>),
    /// 一条贮藏里的改动（贮藏的提交）
    Stash(String),
    /// 两端的差异（比较页）：git 的参数现成给
    Range(Vec<String>),
}

/// 未跟踪的文件最多一个个取这么多个
const UNTRACKED_MAX: usize = 200;

impl Source {
    /// 另存时默认的文件名。
    fn file_name(&self) -> String {
        let short = |id: &str| id.chars().take(8).collect::<String>();
        match self {
            Source::Unstaged { .. } => "unstaged.patch".into(),
            Source::Staged(_) => "staged.patch".into(),
            Source::Commits(ids) => match ids.as_slice() {
                [one] => format!("{}.patch", short(one)),
                _ => format!("{}-commits.patch", ids.len()),
            },
            Source::Stash(id) => format!("stash-{}.patch", short(id)),
            Source::Range(_) => "compare.patch".into(),
        }
    }

    /// 生成补丁的命令：一条或几条，输出接起来就是补丁。未跟踪的文件用 `diff --no-index`，它有
    /// 差异时退出码是 1，不算出错。
    fn commands(&self) -> Vec<(Vec<String>, bool)> {
        let diff = |extra: &[&str], paths: &[String]| {
            let mut list = args(&[
                "diff",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                "--no-color",
            ]);
            list.extend(extra.iter().map(|s| s.to_string()));
            list.push("--".into());
            list.extend(paths.iter().cloned());
            list
        };
        match self {
            Source::Unstaged { paths, untracked } => {
                let mut list = vec![(diff(&[], paths), false)];
                list.extend(untracked.iter().take(UNTRACKED_MAX).map(|path| {
                    let mut one = fr_core::patch::untracked_args(path);
                    // 二进制文件也带内容，换个地方才应用得上
                    if let Some(at) = one.iter().position(|arg| arg == "diff") {
                        one.insert(at + 1, "--binary".into());
                    }
                    (one, true)
                }));
                list
            }
            Source::Staged(paths) => vec![(diff(&["--cached"], paths), false)],
            Source::Commits(ids) => {
                // format-patch 按走的顺序倒过来输出：给新的在前，出来的补丁就是旧的在前
                let mut list =
                    args(&["format-patch", "--stdout", "--binary", "--no-walk=unsorted"]);
                // 只给一个提交时 git 当成「从它到 HEAD」（HEAD 自己就什么都不出）：加 -1 才是这一个
                if ids.len() == 1 {
                    list.push("-1".into());
                }
                list.extend(ids.iter().cloned());
                vec![(list, false)]
            }
            Source::Stash(id) => {
                let (base, tip) = (format!("{id}^1"), id.clone());
                vec![(diff(&[&base, &tip], &[]), false)]
            }
            Source::Range(list) => vec![(list.clone(), false)],
        }
    }
}

/// 一个文件预检的结果写成字和颜色。
fn fit_label(file: &PatchFile, fit: Fit, cx: &App) -> (&'static str, Hsla) {
    let theme = cx.theme();
    match fit {
        Fit::Clean if file.created => (tr("新文件"), theme.success),
        Fit::Clean if file.deleted => (tr("可应用（删除）"), theme.success),
        Fit::Clean => (tr("可应用"), theme.success),
        Fit::Mismatch => (tr("与当前内容不一致"), theme.warning),
        Fit::Exists => (tr("文件已存在"), theme.warning),
        Fit::Missing => (tr("文件不存在"), theme.danger),
        Fit::Failed => (tr("无法应用"), theme.danger),
    }
}

/// 读进来等着应用的补丁。
#[derive(Clone)]
struct Loaded {
    /// 文件名，或者「剪贴板」
    name: SharedString,
    /// 本机上在哪（剪贴板来的没有）
    place: Option<PathBuf>,
    bytes: Arc<Vec<u8>>,
}

/// 「应用补丁」的对话框。
struct PatchDialog {
    repo: WeakEntity<RepoView>,
    patch: Loaded,
    commits: Vec<MailCommit>,
    files: Vec<PatchFile>,
    /// 直接应用时各文件行不行
    fits: HashMap<String, Fit>,
    /// 三方合并时各文件行不行
    fits3: HashMap<String, Fit>,
    loading: bool,
    error: Option<SharedString>,
    /// 应用到工作区并暂存
    index: bool,
    /// 带提交的补丁：原样变回提交
    as_commits: bool,
    three_way: bool,
    /// 应用到哪：主机、仓库、分支
    target: SharedString,
    /// 暂存区里现在有东西（`git am` 这时不肯做）
    staged: bool,
    _task: Option<Task<()>>,
}

impl PatchDialog {
    fn new(
        repo: WeakEntity<RepoView>,
        patch: Loaded,
        target: SharedString,
        staged: bool,
        channel: fr_channel::Channel,
        path: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let commits = patchfile::mail_commits(&patch.bytes);
        let as_commits = !commits.is_empty() && !staged;
        let mut dialog = Self {
            repo,
            patch,
            commits,
            files: Vec::new(),
            fits: HashMap::new(),
            fits3: HashMap::new(),
            loading: true,
            error: None,
            index: false,
            as_commits,
            three_way: false,
            target,
            staged,
            _task: None,
        };
        dialog.check(channel, path, cx);
        dialog
    }

    /// 在主机上列出补丁里的文件，预检两遍（直接应用、三方合并）。
    fn check(&mut self, channel: fr_channel::Channel, repo: String, cx: &mut Context<Self>) {
        let bytes = self.patch.bytes.clone();
        self._task = Some(cx.spawn(async move |this, cx| {
            let input = || Some(bytes.as_ref().clone());
            let list = channel.git_with_input(&repo, patchfile::list_args(), input());
            let plain = channel.git_with_input(&repo, patchfile::check_args(false), input());
            let three = channel.git_with_input(&repo, patchfile::check_args(true), input());
            let (list, plain, three) = futures::join!(list, plain, three);
            this.update(cx, |this, cx| {
                this.loading = false;
                match list {
                    Ok(out) if out.success() => this.files = patchfile::parse_list(&out.stdout),
                    Ok(out) => {
                        let said = out.stderr.trim().to_string();
                        this.error =
                            Some(tf!("这不是 git 能识别的补丁：{said}", said = said).into());
                    }
                    Err(err) => {
                        this.error =
                            Some(tf!("读取补丁失败：{err}", err = format!("{err:#}")).into())
                    }
                }
                if this.error.is_none() && this.files.is_empty() {
                    this.error = Some(tr("补丁里没有改动。").into());
                }
                let fits = |out: anyhow::Result<fr_channel::GitOutput>| -> HashMap<String, Fit> {
                    out.map(|out| patchfile::parse_check(out.stderr.as_bytes()))
                        .unwrap_or_default()
                        .into_iter()
                        .collect()
                };
                this.fits = fits(plain);
                this.fits3 = fits(three);
                cx.notify();
            })
            .ok();
        }));
    }

    /// 按现在勾的选项，这个文件行不行。预检没提到的文件按能应用算（预检本身没出错的话）。
    fn fit(&self, file: &PatchFile) -> Fit {
        let table = match self.three_way {
            true => &self.fits3,
            false => &self.fits,
        };
        table.get(&file.path).copied().unwrap_or(Fit::Clean)
    }

    /// 不勾三方合并时有几个文件应用不上、其中几个三方合并也救不了。
    fn trouble(&self) -> (usize, usize) {
        let plain = |file: &PatchFile| self.fits.get(&file.path).copied().unwrap_or(Fit::Clean);
        let three = |file: &PatchFile| self.fits3.get(&file.path).copied().unwrap_or(Fit::Clean);
        let stuck = self.files.iter().filter(|f| plain(f) != Fit::Clean).count();
        let hard = self
            .files
            .iter()
            .filter(|f| plain(f) != Fit::Clean && three(f).hard())
            .count();
        (stuck, hard)
    }

    /// 能不能点「应用」；不能时是为什么。
    fn blocked(&self) -> Option<SharedString> {
        if self.loading {
            return Some(tr("正在检查补丁…").into());
        }
        if let Some(error) = &self.error {
            return Some(error.clone());
        }
        let (stuck, hard) = self.trouble();
        if hard > 0 {
            return Some(
                tf!(
                    "有 {hard} 个文件无法应用，三方合并也不行。补丁不会部分应用。",
                    hard
                )
                .into(),
            );
        }
        if self.as_commits && self.staged {
            return Some(
                tr("暂存区里有改动，无法作为提交应用。请先提交或取消暂存，或改为「仅应用改动」。")
                    .into(),
            );
        }
        if self.as_commits && stuck > 0 {
            return Some(
                tr("有文件与当前内容不一致，无法作为提交应用。请改为「仅应用改动」并勾选三方合并。")
                    .into(),
            );
        }
        if stuck > 0 && !self.three_way {
            return Some(
                tf!(
                    "有 {stuck} 个文件无法直接应用。补丁不会部分应用；勾选三方合并后可以应用。",
                    stuck
                )
                .into(),
            );
        }
        None
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.blocked().is_some() {
            return;
        }
        window.close_dialog(cx);
        let job = Apply {
            bytes: self.patch.bytes.clone(),
            files: self.files.clone(),
            commits: match self.as_commits {
                true => self.commits.len(),
                false => 0,
            },
            index: self.index,
            three_way: self.three_way,
        };
        self.repo
            .update(cx, |repo, cx| repo.apply_patch(job, window, cx))
            .ok();
    }
}

/// 确认以后要做的。
struct Apply {
    bytes: Arc<Vec<u8>>,
    files: Vec<PatchFile>,
    /// 作为提交应用时有几个提交；0 是只应用改动
    commits: usize,
    index: bool,
    three_way: bool,
}

impl Render for PatchDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list_height = (window.viewport_size().height - px(470.)).clamp(px(96.), px(260.));
        let theme = cx.theme();
        let (muted, border, danger, warning, panel) = (
            theme.muted_foreground,
            theme.border,
            theme.danger,
            theme.warning,
            theme.muted,
        );
        let (added, removed) = (theme.green, theme.red);
        let size = self.patch.bytes.len();
        let size_text = match size {
            0..1024 => format!("{size} B"),
            1024..1_048_576 => format!("{:.1} KB", size as f64 / 1024.),
            _ => format!("{:.1} MB", size as f64 / 1_048_576.),
        };
        let place = self
            .patch
            .place
            .as_ref()
            .and_then(|path| path.parent())
            .map(|dir| dir.to_string_lossy().into_owned());
        let source = h_flex()
            .gap_2()
            .child(ui::icon(IconName::FileDiff).flex_none().text_color(muted))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(ui::clipped("patch-name", self.patch.name.clone()).text_sm())
                    .child(
                        ui::clipped(
                            "patch-place",
                            match place {
                                Some(place) => format!("{place} · {size_text}"),
                                None => size_text,
                            },
                        )
                        .text_xs()
                        .text_color(muted),
                    ),
            )
            .child(
                Button::new("patch-change")
                    .small()
                    .label(tr("更换…"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        window.close_dialog(cx);
                        this.repo
                            .update(cx, |repo, cx| repo.apply_patch_file(window, cx))
                            .ok();
                    })),
            );
        let (adds, dels) = self.files.iter().fold((0, 0), |(a, d), file| {
            let (fa, fd) = file.lines.unwrap_or((0, 0));
            (a + fa, d + fd)
        });
        let kind: SharedString = match (self.loading, self.commits.len()) {
            (true, _) => tr("正在检查补丁…").into(),
            (false, 0) => tf!(
                "差异补丁：改动 {files} 个文件 +{adds} −{dels}",
                files = self.files.len(),
                adds,
                dels
            )
            .into(),
            (false, commits) => tf!(
                "提交补丁：{commits} 个提交，改动 {files} 个文件 +{adds} −{dels}",
                commits,
                files = self.files.len(),
                adds,
                dels
            )
            .into(),
        };
        const COMMITS_SHOWN: usize = 4;
        let commit_rows: Vec<AnyElement> = self
            .commits
            .iter()
            .take(COMMITS_SHOWN)
            .enumerate()
            .map(|(ix, commit)| {
                h_flex()
                    .gap_2()
                    .text_xs()
                    .child(
                        div()
                            .flex_none()
                            .font_family(crate::fonts::mono())
                            .text_color(muted)
                            .child(commit.id.chars().take(8).collect::<String>()),
                    )
                    .child(ui::clipped(("patch-commit", ix), commit.subject.clone()).flex_1())
                    .child(
                        div()
                            .flex_none()
                            .text_color(muted)
                            .child(commit.author.clone()),
                    )
                    .into_any_element()
            })
            .collect();
        let more_commits = self.commits.len().saturating_sub(COMMITS_SHOWN);
        let rows: Vec<AnyElement> = self
            .files
            .iter()
            .enumerate()
            .map(|(ix, file)| {
                let fit = self.fit(file);
                let (label, color) = fit_label(file, fit, cx);
                let (letter, letter_color) = match (file.created, file.deleted, &file.orig) {
                    (true, _, _) => ("A", added),
                    (_, true, _) => ("D", removed),
                    (_, _, Some(_)) => ("R", warning),
                    _ => ("M", warning),
                };
                let counts = match file.lines {
                    Some((a, d)) => format!("+{a} −{d}"),
                    None => tr("二进制").to_string(),
                };
                h_flex()
                    .h(px(24.))
                    .px_2()
                    .gap_2()
                    .text_xs()
                    .when(ix > 0, |d| d.border_t_1().border_color(border.opacity(0.5)))
                    .child(
                        div()
                            .w(px(10.))
                            .flex_none()
                            .font_weight(FontWeight::BOLD)
                            .text_color(letter_color)
                            .child(letter),
                    )
                    .child(
                        ui::clipped(("patch-file", ix), file.path.clone())
                            .flex_1()
                            .text_ellipsis_start(),
                    )
                    .child(div().flex_none().text_color(muted).child(counts))
                    .child(
                        div()
                            .w(px(128.))
                            .flex_none()
                            .text_right()
                            .text_color(color)
                            .child(label),
                    )
                    .into_any_element()
            })
            .collect();
        let (stuck, hard) = self.trouble();
        let blocked = self.blocked();
        let has_commits = !self.commits.is_empty();
        let (as_commits, index, three_way) = (self.as_commits, self.index, self.three_way);
        let pick = |id: &'static str, label: &'static str, on: bool| {
            Radio::new(id).label(label).checked(on)
        };
        let mut options = v_flex().gap_2();
        if has_commits {
            options = options.child(
                h_flex()
                    .gap_4()
                    .child(
                        pick(
                            "patch-as-commits",
                            tr("作为提交应用（保留作者、时间和说明）"),
                            as_commits,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.as_commits = true;
                            this.three_way = false;
                            cx.notify();
                        })),
                    )
                    .child(
                        pick("patch-as-diff", tr("仅应用改动"), !as_commits).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.as_commits = false;
                                cx.notify();
                            },
                        )),
                    ),
            );
        }
        if !as_commits {
            // 三方合并要经过暂存区，改动都会进暂存区
            let staged_on = index || three_way;
            options = options.child(
                h_flex()
                    .gap_4()
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(muted)
                            .child(tr("应用到")),
                    )
                    .child(
                        pick("patch-to-worktree", tr("工作区"), !staged_on)
                            .disabled(three_way)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.index = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        pick("patch-to-index", tr("工作区并暂存"), staged_on).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.index = true;
                                cx.notify();
                            }),
                        ),
                    ),
            );
            if stuck > hard {
                options = options.child(
                    Checkbox::new("patch-three-way")
                        .label(tr("无法直接应用的文件改用三方合并，冲突留在文件中解决"))
                        .checked(three_way)
                        .on_click(cx.listener(|this, on: &bool, _, cx| {
                            this.three_way = *on;
                            cx.notify();
                        })),
                );
            }
        }
        v_flex()
            .gap_3()
            .child(source)
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child(kind))
                    .children(commit_rows)
                    .when(more_commits > 0, |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(tf!("还有 {n} 个提交…", n = more_commits)),
                        )
                    }),
            )
            .when(!rows.is_empty(), |d| {
                d.child(
                    div()
                        .id("patch-files")
                        .max_h(list_height)
                        .overflow_y_scroll()
                        .rounded(px(ui::RADIUS))
                        .border_1()
                        .border_color(border)
                        .bg(panel.opacity(0.35))
                        .child(v_flex().children(rows)),
                )
            })
            .child(options)
            .children(blocked.clone().filter(|_| !self.loading).map(|why| {
                div()
                    .text_xs()
                    .text_color(if self.error.is_some() || hard > 0 {
                        danger
                    } else {
                        warning
                    })
                    .whitespace_normal()
                    .child(why)
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        ui::clipped("patch-target", self.target.clone())
                            .flex_1()
                            .text_xs()
                            .text_color(muted),
                    )
                    .child(
                        Button::new("patch-cancel")
                            .label(tr("取消"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("patch-apply")
                            .primary()
                            .label(tr("应用"))
                            .disabled(blocked.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

impl RepoView {
    /// 在主机上生成补丁，取回来。出错时返回给用户看的话。
    fn fetch_patch(
        &self,
        source: &Source,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<u8>, String>> {
        let Some(channel) = self.channel.clone() else {
            return Task::ready(Err(tr("尚未连接").to_string()));
        };
        let repo = self.entry.path.clone();
        let commands = source.commands();
        cx.background_spawn(async move {
            let mut patch = Vec::new();
            for (list, diff_exit) in commands {
                let out = channel
                    .git(&repo, list)
                    .await
                    .map_err(|err| format!("{err:#}"))?;
                // `diff --no-index` 有差异时退出码是 1
                let ok = out.success() || (diff_exit && out.code == Some(1));
                if !ok {
                    return Err(out.stderr.trim().to_string());
                }
                patch.extend_from_slice(&out.stdout);
            }
            Ok(patch)
        })
    }

    /// 导出为补丁文件：存到本机选的位置。
    pub(super) fn export_patch(
        &mut self,
        source: Source,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let downloads = super::files_ops::downloads_dir().unwrap_or_else(std::env::temp_dir);
        let chosen = cx.prompt_for_new_path(&downloads, Some(&source.file_name()));
        let patch = self.fetch_patch(&source, cx);
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            let result = match patch.await {
                Ok(bytes) if bytes.is_empty() => Err(tr("没有可导出的改动。").to_string()),
                Ok(bytes) => std::fs::write(&path, bytes).map_err(|err| err.to_string()),
                Err(err) => Err(err),
            };
            cx.update(|window, cx| {
                let note = match result {
                    Ok(()) => {
                        let shown = path.to_string_lossy().into_owned();
                        ui::action_note(
                            NotificationType::Success,
                            tf!("已导出补丁到 {path}", path = shown.clone()),
                            move |_, _, _| {
                                let shown = shown.clone();
                                Button::new("patch-reveal")
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
                        Notification::error(tf!("导出补丁失败：{err}", err)).autohide(false)
                    }
                };
                window.push_notification(note, cx);
            })
            .ok();
        })
        .detach();
    }

    /// 复制为补丁：放进剪贴板，不落文件。
    pub(super) fn copy_patch(
        &mut self,
        source: Source,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let patch = self.fetch_patch(&source, cx);
        cx.spawn_in(window, async move |_, cx| {
            let result = patch.await;
            cx.update(|window, cx| {
                let note = match result {
                    Ok(bytes) if bytes.is_empty() => {
                        Notification::warning(tr("没有可复制的改动。"))
                    }
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(text) => {
                            let lines = text.lines().count();
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                            Notification::success(tf!("已复制补丁（{lines} 行）", lines))
                        }
                        Err(_) => Notification::warning(tr(
                            "补丁里有非文本内容，无法放进剪贴板。请改用「导出为补丁…」。",
                        ))
                        .autohide(false),
                    },
                    Err(err) => {
                        Notification::error(tf!("生成补丁失败：{err}", err)).autohide(false)
                    }
                };
                window.push_notification(note, cx);
            })
            .ok();
        })
        .detach();
    }

    /// 「应用补丁…」：选本机的一个补丁文件。
    pub(super) fn apply_patch_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.open_patch_path(path, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// 读本机的补丁文件，打开应用补丁的对话框（选的，或者拖进来的）。
    pub(super) fn open_patch_path(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read = cx.background_spawn({
            let path = path.clone();
            async move {
                let size = std::fs::metadata(&path)
                    .map_err(|err| err.to_string())?
                    .len();
                if size > patchfile::MAX_BYTES {
                    return Err(tf!(
                        "补丁文件有 {size} MB，超过 {max} MB 的上限。",
                        size = size / 1_048_576,
                        max = patchfile::MAX_BYTES / 1_048_576
                    ));
                }
                std::fs::read(&path).map_err(|err| err.to_string())
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(bytes) => {
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let patch = Loaded {
                        name: name.into(),
                        place: Some(path),
                        bytes: Arc::new(bytes),
                    };
                    this.open_patch_dialog(patch, window, cx);
                }
                Err(err) => {
                    let text = tf!("读取补丁文件失败：{err}", err);
                    window.push_notification(Notification::error(text).autohide(false), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// 「从剪贴板应用补丁」。
    pub(super) fn apply_patch_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        if !looks_like_patch(&text) {
            let note = tr("剪贴板里不是补丁。请先在别处「复制为补丁」，或复制 git diff 的输出。");
            window.push_notification(Notification::warning(note), cx);
            return;
        }
        // 复制时可能少了末尾的换行，git 会说补丁损坏
        let mut text = text.replace("\r\n", "\n");
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let patch = Loaded {
            name: tr("剪贴板").into(),
            place: None,
            bytes: Arc::new(text.into_bytes()),
        };
        self.open_patch_dialog(patch, window, cx);
    }

    fn open_patch_dialog(&mut self, patch: Loaded, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(running) = &self.op {
            window.push_notification(
                Notification::warning(tf!("请等待「{running}」完成后重试", running)),
                cx,
            );
            return;
        }
        if let Some(state) = self.in_progress {
            let text = tf!("{name}尚未完成，请先完成或中止。", name = state.name());
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let target = tf!(
            "应用到 {place} 上的 {repo} · {branch}",
            place = self.place(cx),
            repo = self.entry.name(),
            branch = self.current_label()
        );
        let staged = self
            .status
            .as_ref()
            .is_some_and(|s| s.entries.iter().any(|e| e.staged.is_some()));
        let (view, path) = (cx.entity().downgrade(), self.entry.path.clone());
        let dialog =
            cx.new(|cx| PatchDialog::new(view, patch, target.into(), staged, channel, path, cx));
        window.open_dialog(cx, move |d, _, _| {
            d.title(tr("应用补丁")).w(px(680.)).child(dialog.clone())
        });
    }

    /// 应用补丁。只应用改动的：先存一份被改的文件，做完能撤销（和丢弃同一套）；作为提交应用的：
    /// 做完能撤销回应用前的位置，没成就中止（`git am --abort`），仓库回到原样。
    fn apply_patch(&mut self, job: Apply, window: &mut Window, cx: &mut Context<Self>) {
        let bytes = job.bytes.as_ref().clone();
        let count = job.files.len();
        if job.commits > 0 {
            let commits = job.commits;
            let point = self.undo_point();
            let step = Step::input(patchfile::am_args(false), bytes);
            self.run_steps(
                tr("应用补丁"),
                vec![step],
                window,
                cx,
                Box::new(move |this, result, window, cx| match result {
                    Ok(_) => {
                        let text = tf!("已应用补丁：新增 {commits} 个提交", commits);
                        this.notify_undoable(text, point, window, cx);
                    }
                    Err(text) => this.abort_am(text, window, cx),
                }),
            );
            return;
        }
        // 会被动到的文件：改名的连原路径一起
        let files: Vec<String> = job
            .files
            .iter()
            .flat_map(|file| std::iter::once(file.path.clone()).chain(file.orig.clone()))
            .collect();
        let step = Step::input(patchfile::apply_args(job.index, job.three_way), bytes);
        let job = Overwrite {
            label: tr("应用补丁"),
            files,
            steps: vec![step],
            done: tf!("已应用补丁：{count} 个文件有改动", count),
            look: None,
            conflicts_ok: job.three_way,
            staged: job.index || job.three_way,
        };
        self.show_changes(cx);
        self.overwrite_with_undo(job, window, cx);
    }

    /// 作为提交应用没成：停在一半的 `git am` 中止掉，再报原来的错。
    fn abort_am(&mut self, detail: String, window: &mut Window, cx: &mut Context<Self>) {
        self.run_op_with(
            tr("中止应用补丁"),
            patchfile::am_abort_args(),
            window,
            cx,
            move |this, aborted, window, cx| {
                let detail = match aborted {
                    Ok(_) => tf!(
                        "{detail}\n\n已中止，仓库回到应用前的状态。",
                        detail = detail.as_str()
                    ),
                    Err(abort) => tf!(
                        "{detail}\n\n中止也失败，请在终端中执行 git am --abort：\n{abort}",
                        detail = detail.as_str(),
                        abort
                    ),
                };
                this.failure(tr("应用补丁"), detail, window, cx);
            },
        );
    }

    /// 「本地修改」文件列表上方 ⋯ 里的菜单：导出未暂存的、已暂存的改动，应用补丁。
    pub(super) fn patch_menu(&self, cx: &mut Context<Self>) -> super::menus::MenuFn {
        let view = cx.entity().downgrade();
        let entries = self
            .status
            .as_ref()
            .map(|s| s.entries.clone())
            .unwrap_or_default();
        let untracked: Vec<String> = entries
            .iter()
            .filter(|e| e.unstaged == Some(fr_core::status::Kind::Untracked))
            .map(|e| e.path.clone())
            .collect();
        let has_unstaged = entries.iter().any(|e| e.unstaged.is_some());
        let has_staged = entries.iter().any(|e| e.staged.is_some());
        type Run = Box<dyn Fn(&mut RepoView, &mut Window, &mut Context<RepoView>)>;
        Box::new(move |menu, _, _| {
            let item = |label: &'static str, enabled: bool, run: Run| {
                let view = view.clone();
                gpui_kit::component::menu::PopupMenuItem::new(label)
                    .disabled(!enabled)
                    .on_click(move |_, window, cx| {
                        view.update(cx, |this, cx| run(this, window, cx)).ok();
                    })
            };
            let unstaged = || Source::Unstaged {
                paths: Vec::new(),
                untracked: untracked.clone(),
            };
            let (a, b, c) = (unstaged(), unstaged(), Source::Staged(Vec::new()));
            let d = Source::Staged(Vec::new());
            menu.item(item(
                tr("应用补丁…"),
                true,
                Box::new(|this, w, cx| this.apply_patch_file(w, cx)),
            ))
            .item(item(
                tr("从剪贴板应用补丁"),
                true,
                Box::new(|this, w, cx| this.apply_patch_clipboard(w, cx)),
            ))
            .separator()
            .item(item(
                tr("导出未暂存的改动为补丁…"),
                has_unstaged,
                Box::new(move |this, w, cx| this.export_patch(a.clone(), w, cx)),
            ))
            .item(item(
                tr("复制未暂存的改动为补丁"),
                has_unstaged,
                Box::new(move |this, w, cx| this.copy_patch(b.clone(), w, cx)),
            ))
            .item(item(
                tr("导出已暂存的改动为补丁…"),
                has_staged,
                Box::new(move |this, w, cx| this.export_patch(c.clone(), w, cx)),
            ))
            .item(item(
                tr("复制已暂存的改动为补丁"),
                has_staged,
                Box::new(move |this, w, cx| this.copy_patch(d.clone(), w, cx)),
            ))
        })
    }

    /// 把文件拖到「本地修改」上：只有一个 .patch、.diff 文件时打开应用补丁的对话框。
    pub(super) fn drop_patch(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let files: Vec<&PathBuf> = paths.paths().iter().collect();
        match files.as_slice() {
            [one] if is_patch_name(one) => self.open_patch_path((*one).clone(), window, cx),
            _ => {
                let text = tr("把一个 .patch 或 .diff 文件拖到这里可以应用补丁。");
                window.push_notification(Notification::info(text), cx);
            }
        }
    }
}

/// 导出、复制为补丁的两项，加到各对象的右键菜单里。
pub(super) fn patch_items(
    menu: gpui_kit::component::menu::PopupMenu,
    view: &WeakEntity<RepoView>,
    source: Source,
) -> gpui_kit::component::menu::PopupMenu {
    let (export_view, copy_view) = (view.clone(), view.clone());
    let (export, copy) = (source.clone(), source);
    menu.item(
        gpui_kit::component::menu::PopupMenuItem::new(tr("导出为补丁…")).on_click(
            move |_, window, cx| {
                let source = export.clone();
                export_view
                    .update(cx, |this, cx| this.export_patch(source, window, cx))
                    .ok();
            },
        ),
    )
    .item(
        gpui_kit::component::menu::PopupMenuItem::new(tr("复制为补丁")).on_click(
            move |_, window, cx| {
                let source = copy.clone();
                copy_view
                    .update(cx, |this, cx| this.copy_patch(source, window, cx))
                    .ok();
            },
        ),
    )
}

/// 「本地修改」里选中的文件导出成补丁时从哪取：未暂存一栏取未暂存的差异，已暂存一栏取已暂存的。
/// 改名的文件连原路径一起给，不然补丁里只有「新增」没有「删除」。
pub(super) fn entry_source(
    area: super::changes::Area,
    entries: &[fr_core::status::Entry],
) -> Source {
    let paths: Vec<String> = entries
        .iter()
        .flat_map(|entry| std::iter::once(entry.path.clone()).chain(entry.orig_path.clone()))
        .collect();
    match area {
        super::changes::Area::Staged => Source::Staged(paths),
        super::changes::Area::Unstaged => Source::Unstaged {
            paths,
            untracked: entries
                .iter()
                .filter(|entry| entry.unstaged == Some(fr_core::status::Kind::Untracked))
                .map(|entry| entry.path.clone())
                .collect(),
        },
    }
}

/// 文件名像不像补丁。
fn is_patch_name(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            let ext = ext.to_ascii_lowercase();
            ext == "patch" || ext == "diff"
        })
}

/// 一段文字像不像补丁：有 `diff --git`、提交补丁的头，或者老式的 `---`/`+++` 文件头。
fn looks_like_patch(text: &str) -> bool {
    let mut minus = false;
    for line in text.lines().take(400) {
        if line.starts_with("diff --git ") || line.starts_with("From ") && line.contains(" 2001") {
            return true;
        }
        if line.starts_with("--- ") {
            minus = true;
        } else if minus && line.starts_with("+++ ") {
            return true;
        } else {
            minus = false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{Source, is_patch_name, looks_like_patch};
    use fr_core::patchfile;

    #[test]
    fn 认出像补丁的文字() {
        assert!(looks_like_patch("diff --git a/x b/x\nindex 1..2\n"));
        assert!(looks_like_patch(
            "From 0123456789012345678901234567890123456789 Mon Sep 17 00:00:00 2001\nFrom: a\n"
        ));
        assert!(looks_like_patch("--- a/x\n+++ b/x\n@@ -1 +1 @@\n"));
        assert!(!looks_like_patch("hello\n--- not a patch\nworld\n"));
        assert!(!looks_like_patch(""));
    }

    #[test]
    fn 按扩展名认补丁文件() {
        let is = |name: &str| is_patch_name(std::path::Path::new(name));
        assert!(is("a.patch") && is("B.DIFF"));
        assert!(!is("a.txt") && !is("patch"));
    }

    #[test]
    fn 各种来源的命令() {
        let unstaged = Source::Unstaged {
            paths: vec!["a.rs".into()],
            untracked: vec!["new.txt".into()],
        };
        let commands = unstaged.commands();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].0.last().unwrap(), "a.rs");
        assert!(commands[0].0.contains(&"--binary".to_string()) && !commands[0].1);
        // 未跟踪的文件：退出码 1 不算出错
        assert!(commands[1].1 && commands[1].0.contains(&"--binary".to_string()));
        let staged = Source::Staged(Vec::new()).commands();
        assert!(staged[0].0.contains(&"--cached".to_string()));
        let commits = Source::Commits(vec!["abcdef0123".into(), "1234567890".into()]);
        assert_eq!(commits.file_name(), "2-commits.patch");
        assert_eq!(commits.commands()[0].0[0], "format-patch");
        let stash = Source::Stash("abc".into()).commands();
        assert!(stash[0].0.contains(&"abc^1".to_string()));
        assert_eq!(
            Source::Stash("abcdef0123".into()).file_name(),
            "stash-abcdef01.patch"
        );
    }

    /// 真的跑一遍：一个提交（包括 HEAD 和最早的那个）出一封，几个提交按旧的在前各出一封。
    #[test]
    fn 对着真仓库生成提交补丁() {
        let dir = std::env::temp_dir().join(format!("fr-format-patch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| fr_core::git::run(&dir, args).unwrap();
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "user.email", "t@example.com"]);
        let mut ids = Vec::new();
        for name in ["one", "two", "three"] {
            std::fs::write(dir.join("a.txt"), format!("{name}\n")).unwrap();
            git(&["add", "-A"]);
            git(&["commit", "-q", "-m", name]);
            let id = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
            ids.push(id.trim().to_string());
        }
        let subjects = |picked: &[&String]| -> Vec<String> {
            let source = Source::Commits(picked.iter().map(|id| id.to_string()).collect());
            let commands = source.commands();
            let list: Vec<&str> = commands[0].0.iter().map(String::as_str).collect();
            patchfile::mail_commits(&git(&list))
                .into_iter()
                .map(|commit| commit.subject)
                .collect()
        };
        // HEAD、中间的、最早的，各自只出自己
        assert_eq!(subjects(&[&ids[2]]), ["three"]);
        assert_eq!(subjects(&[&ids[1]]), ["two"]);
        assert_eq!(subjects(&[&ids[0]]), ["one"]);
        // 几个：给新的在前，出来旧的在前
        assert_eq!(subjects(&[&ids[2], &ids[0]]), ["one", "three"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
