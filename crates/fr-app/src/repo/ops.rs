//! 写操作：执行 git，出错时弹通知，做完刷新。还有获取、拉取、推送、切换分支、贮藏这些具体操作。
//!
//! 同一个仓库同一时间只做一个写操作，免得几个 git 进程争抢 index.lock。

use fr_core::i18n::tr;
use fr_core::tf;
use std::rc::Rc;

use fr_channel::{GitOutput, StreamItem};
use fr_core::state;
use futures::channel::oneshot;
use futures::{FutureExt as _, StreamExt as _};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::*;

use super::RepoView;

/// 写操作里的一步。
#[derive(Clone)]
pub(super) struct Step {
    pub args: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    /// 联网的命令：边做边显示 git 报的进度，可以取消。别的命令会改暂存区和工作区，
    /// 中途杀掉会留下锁文件，所以不给取消。
    pub net: bool,
}

impl Step {
    pub fn local(args: Vec<String>) -> Self {
        Self {
            args,
            stdin: None,
            net: false,
        }
    }

    pub fn net(args: Vec<String>) -> Self {
        Self {
            args,
            stdin: None,
            net: true,
        }
    }
}

type OnDone =
    Box<dyn FnOnce(&mut RepoView, Result<GitOutput, String>, &mut Window, &mut Context<RepoView>)>;

impl RepoView {
    /// 执行一个写操作。成功时调用 `on_success`，失败时弹出 git 的报错；无论成败都刷新状态。
    pub(super) fn run_op(
        &mut self,
        label: impl Into<SharedString>,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_success: impl FnOnce(&mut Self, &GitOutput, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let step = Step {
            args,
            stdin,
            net: false,
        };
        self.run_seq(label, vec![step], window, cx, on_success);
    }

    /// 执行联网的操作（获取、推送）：工具栏上显示 git 报的进度，可以取消。
    pub(super) fn run_net_op(
        &mut self,
        label: &'static str,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_success: impl FnOnce(&mut Self, &GitOutput, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.run_seq(label, vec![Step::net(args)], window, cx, on_success);
    }

    /// 依次执行几步，算一个写操作；有一步失败就停。成功时调用 `on_success`（给的是最后一步的输出），
    /// 失败时弹出报错，报错里写明前面哪几步已经做完。
    pub(super) fn run_seq(
        &mut self,
        label: impl Into<SharedString>,
        steps: Vec<Step>,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_success: impl FnOnce(&mut Self, &GitOutput, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let label = label.into();
        let name = label.clone();
        self.run_steps(
            label,
            steps,
            window,
            cx,
            Box::new(move |this, result, window, cx| match result {
                Ok(out) => on_success(this, &out, window, cx),
                Err(text) => failure(window, cx, &name, text),
            }),
        );
    }

    /// 执行一个写操作，做完把结果交给 `on_done`（失败时是给用户看的报错），然后刷新状态。
    pub(super) fn run_op_with(
        &mut self,
        label: impl Into<SharedString>,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_done: impl FnOnce(&mut Self, Result<GitOutput, String>, &mut Window, &mut Context<Self>)
        + 'static,
    ) {
        self.run_steps(
            label,
            vec![Step::local(args)],
            window,
            cx,
            Box::new(on_done),
        );
    }

    /// 正在做的写操作的名字。
    pub fn busy(&self) -> Option<SharedString> {
        self.op.clone()
    }

    /// 正在做的写操作能不能取消（联网的那一步才能）。
    pub fn cancellable(&self) -> bool {
        self.op_progress.is_some() && self.op_cancel.is_some()
    }

    /// 写操作的实现：依次执行 `steps`，有一步失败就停。
    pub(super) fn run_steps(
        &mut self,
        label: impl Into<SharedString>,
        steps: Vec<Step>,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_done: OnDone,
    ) {
        let label: SharedString = label.into();
        if let Some(running) = &self.op {
            window.push_notification(
                Notification::warning(tf!("请等待「{running}」完成后重试", running)),
                cx,
            );
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        // 后台的自动获取让路：用户要做的事优先，两条 git 也不会抢同一个引用的锁
        self.stop_auto_fetch();
        for step in &steps {
            log::info!("{}：git {}", self.entry.path, step.args.join(" "));
        }
        let repo = self.entry.path.clone();
        let was_in_progress = self.in_progress.is_some();
        // 继续、跳过之后又停下，说明后面的提交也停住了，不算出错
        // 按标签认：和 `continue_in_progress`、`skip_in_progress` 里起的标签是同一句话
        let resuming = was_in_progress
            && self.in_progress.is_some_and(|state| {
                label.as_ref() == tf!("继续{name}", name = state.name())
                    || label.as_ref() == tr("跳过")
            });
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        self.op = Some(label.clone());
        self.op_progress = None;
        self.op_cancel = Some(cancel_tx);
        self.op_task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut cancel_rx = cancel_rx.fuse();
            let mut result = Ok(GitOutput {
                code: Some(0),
                ..Default::default()
            });
            let total = steps.len();
            let mut done: Vec<String> = Vec::new();
            let mut failed_step = String::new();
            let mut cancelled = false;
            for step in steps {
                let shown = command_line(&step.args);
                // 工具栏上有没有「取消」，跟着这一步走
                let net = step.net;
                let marked = this.update(cx, |this, cx| {
                    this.op_progress = net.then(SharedString::default);
                    cx.notify();
                    this.activity.begin(label.clone(), &step.args)
                });
                let Ok(activity) = marked else {
                    return;
                };
                result = if step.net {
                    let mut stream = channel.git_progress(&repo, step.args);
                    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
                    loop {
                        let next = futures::select_biased! {
                            stop = cancel_rx => Err(stop.is_ok()),
                            item = stream.next().fuse() => Ok(item),
                        };
                        let item = match next {
                            Ok(item) => item,
                            // 用户点了取消：离开这个循环时流被丢掉，对应的 git 进程会被杀掉
                            Err(true) => {
                                cancelled = true;
                                break Err(anyhow::anyhow!(tr("已取消")));
                            }
                            Err(false) => continue,
                        };
                        match item {
                            Some(Ok(StreamItem::Chunk(chunk))) => stdout.extend_from_slice(&chunk),
                            Some(Ok(StreamItem::Progress(chunk))) => {
                                stderr.extend_from_slice(&chunk);
                                let line = last_progress_line(&stderr);
                                let shown = this.update(cx, |this, cx| {
                                    this.op_progress = Some(line.into());
                                    cx.notify();
                                });
                                if shown.is_err() {
                                    return;
                                }
                            }
                            Some(Ok(StreamItem::Done(mut out))) => {
                                out.stdout = stdout;
                                break Ok(out);
                            }
                            Some(Err(err)) => break Err(err),
                            None => break Err(anyhow::anyhow!(tr("连接中断"))),
                        }
                    }
                } else {
                    channel.git_with_input(&repo, step.args, step.stdin).await
                };
                let ok = matches!(&result, Ok(out) if out.success());
                let logged = this.update(cx, |this, _| {
                    this.activity.end(activity, &result, cancelled);
                    // 刚联过网：自动获取从现在重新计时
                    if net && ok {
                        this.auto_fetch.last = std::time::Instant::now();
                    }
                });
                if logged.is_err() {
                    return;
                }
                if !ok {
                    failed_step = shown;
                    break;
                }
                done.push(shown);
            }
            // 失败了：看看是不是合并、变基这类操作遇到冲突停下了。那不算出错，换一种提示
            let failed = !matches!(&result, Ok(out) if out.success());
            let stopped = if failed && !cancelled && (!was_in_progress || resuming) {
                channel
                    .git_with_input(&repo, state::args(), Some(state::stdin()))
                    .await
                    .ok()
                    .and_then(|out| state::parse(&out.stdout))
            } else {
                None
            };
            this.update_in(cx, |this, window, cx| {
                this.op = None;
                this.op_progress = None;
                this.op_cancel = None;
                this.changes.mark_stale();
                let result = match result {
                    Ok(out) if out.success() => Ok(out),
                    Ok(out) => Err(output_text(&out)),
                    Err(err) => Err(format!("{err:#}")),
                };
                match (stopped, result) {
                    _ if cancelled => {
                        window.push_notification(Notification::info(tf!("{label}已取消", label)), cx);
                    }
                    (Some(state), Err(text)) if resuming => {
                        this.in_progress = Some(state);
                        this.show_changes(cx);
                        let title = tf!("{name}尚未完成，再次暂停", name = state.name());
                        let note = Notification::warning(
                            tr("如有新冲突，请在「本地修改」中解决后点击「继续」，或点击「跳过此提交」。"),
                        )
                        .title(title.clone())
                        .autohide(false);
                        let note = with_detail(note, &title, &text).id::<StoppedNote>();
                        window.push_notification(note, cx);
                    }
                    (Some(state), Err(_)) => {
                        this.in_progress = Some(state);
                        this.show_changes(cx);
                        // 横幅上那个按钮，合并时叫「提交合并」，别的叫「继续」
                        let go = if state == fr_core::state::InProgress::Merge {
                            tr("提交合并")
                        } else {
                            tr("继续")
                        };
                        let note = Notification::warning(tf!(
                            "请在「本地修改」中解决冲突后点击上方的「{go}」，或点击「中止」放弃。",
                            go
                        ))
                        .title(tf!("{name}遇到冲突，已暂停", name = state.name()))
                        .id::<StoppedNote>();
                        window.push_notification(note, cx);
                    }
                    (_, result) => {
                        // 停在一半时做的事（继续、跳过、中止、解决一个文件）有了结果：
                        // 之前那条「停下了」的提示说的已经是过去的事，收掉
                        if was_in_progress {
                            window.remove_notification::<StoppedNote>(cx);
                        }
                        // 多步操作：说清楚前面哪几步已经做完了，免得用户以为什么都没做
                        let result = result.map_err(|text| {
                            if total > 1 && !done.is_empty() {
                                tf!("前 {done} 步已完成：{join}\n失败的步骤：{failed_step}\n\n{text}", done = done.len(), join = done.join(tr("；")), failed_step, text)
                            } else {
                                text
                            }
                        });
                        on_done(this, result, window, cx);
                    }
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// 取消正在进行的联网操作。只是发个信号：等那一步真的停下、状态重新读过，按钮才恢复。
    pub fn cancel_op(&mut self, cx: &mut Context<Self>) {
        if self.op_progress.is_none() {
            return;
        }
        if let Some(cancel) = self.op_cancel.take() {
            cancel.send(()).ok();
            self.op_progress = Some(tr("正在取消…").into());
            cx.notify();
        }
    }

    /// 弹出确认框，用户点了那个红色按钮才执行 `action`。用于丢弃改动、删除贮藏这类不能撤销的操作：
    /// 按回车不会执行，Esc 和「取消」都是取消。
    pub(super) fn confirm(
        &mut self,
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
        ok: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.confirm_with(title, description, ok, true, window, cx, action);
    }

    /// 弹出确认框，用于能撤回、但值得先说清楚后果的操作（变基、弹出贮藏）：回车等于点确定。
    pub(super) fn confirm_plain(
        &mut self,
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
        ok: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.confirm_with(title, description, ok, false, window, cx, action);
    }

    #[allow(clippy::too_many_arguments)]
    fn confirm_with(
        &mut self,
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
        ok: &'static str,
        danger: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let (title, description) = (title.into(), description.into());
        let view = cx.entity().downgrade();
        let action = Rc::new(action);
        crate::ui::cancel_pending_tip(window, cx);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (view, action) = (view.clone(), action.clone());
            let run = move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                window.close_dialog(cx);
                view.update(cx, |this, cx| action(this, window, cx)).ok();
            };
            let on_enter = run.clone();
            let variant = if danger {
                ButtonVariant::Danger
            } else {
                ButtonVariant::Primary
            };
            alert
                .title(title.clone())
                .description(description.clone())
                .on_ok(move |event, window, cx| {
                    // 破坏性的操作按回车不执行，要明确点那个按钮
                    if !danger {
                        on_enter(event, window, cx);
                    }
                    false
                })
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .label(tr("取消"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("ok")
                                .label(ok)
                                .with_variant(variant)
                                .on_click(run),
                        ),
                )
        });
    }

    pub(super) fn fetch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_net_op(
            tr("获取"),
            args(&["fetch", "--all", "--progress"]),
            window,
            cx,
            |_, _, window, cx| {
                window.push_notification(Notification::success(tr("获取完成")), cx);
            },
        );
    }

    /// 拉取。`rebase` 为真时用变基代替合并。
    pub(super) fn pull(&mut self, rebase: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = &self.status else {
            return;
        };
        if status.upstream.is_some() {
            self.run_pull(Vec::new(), rebase, window, cx);
            return;
        }
        // 没有上游：远程上有同名分支就问要不要设成上游，没有就告诉用户该怎么办
        let Some(branch) = status.branch.clone() else {
            window.push_notification(
                Notification::warning(tr("当前处于分离的 HEAD 状态，请先切换到分支再拉取")),
                cx,
            );
            return;
        };
        let candidate = format!("{}/{branch}", self.default_remote());
        if self.refs.remotes.iter().any(|b| b.name == candidate) {
            let description = tf!(
                "{branch} 未设置上游，将设为 {candidate} 后拉取。",
                branch,
                candidate
            );
            self.confirm_plain(
                tr("设置上游并拉取？"),
                description,
                tr("设置并拉取"),
                window,
                cx,
                move |this, window, cx| {
                    let set = Step::local(vec![
                        "branch".into(),
                        format!("--set-upstream-to={candidate}"),
                    ]);
                    this.run_pull(vec![set], rebase, window, cx);
                },
            );
        } else {
            let text = tf!(
                "{branch} 未设置上游，远程也没有同名分支。请先推送，或在侧栏中右键远程分支，选择「设为 {branch} 的上游」。",
                branch
            );
            window.push_notification(Notification::warning(text).autohide(false), cx);
        }
    }

    /// 拉取。工作区有改动时先问要不要贮藏（git 自己的 `--autostash`：拉完自动放回来，
    /// 合并遇到冲突停下时，等合并提交或中止以后再放回来）。
    fn run_pull(
        &mut self,
        steps: Vec<Step>,
        rebase: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_local_changes(
            tr("拉取"),
            tr("不贮藏，直接拉取"),
            window,
            cx,
            move |this, stash, window, cx| {
                this.run_pull_now(steps.clone(), rebase, stash, window, cx)
            },
        );
    }

    /// 拉取分两步：先获取（联网，可以取消），再合并或变基（改工作区，不能中途杀掉）。
    fn run_pull_now(
        &mut self,
        mut steps: Vec<Step>,
        rebase: bool,
        autostash: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        steps.push(Step::net(args(&["fetch", "--progress"])));
        // --no-edit：要合并时直接用默认的合并说明，不弹编辑器
        let mut merge = args(&["pull", "--no-edit"]);
        if rebase {
            merge.push("--rebase".into());
        }
        if autostash {
            merge.push("--autostash".into());
        }
        steps.push(Step::local(merge));
        self.run_seq(
            tr("拉取"),
            steps,
            window,
            cx,
            move |_, out, window, cx| {
                let title = if rebase {
                    tr("拉取完成（变基）")
                } else {
                    tr("拉取完成")
                };
                window.push_notification(Notification::success(summary(out)).title(title), cx);
            },
        );
    }

    pub(super) fn push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = &self.status else {
            return;
        };
        let Some(branch) = status.branch.clone() else {
            window.push_notification(
                Notification::warning(tr("当前处于分离的 HEAD 状态，请先切换到分支再推送")),
                cx,
            );
            return;
        };
        if status.upstream.is_some() {
            let push = args(&["push", "--progress"]);
            self.run_net_op(tr("推送"), push, window, cx, |_, _, window, cx| {
                window.push_notification(Notification::success(tr("推送完成")), cx);
            });
            return;
        }
        // 还没有上游：推到默认远程的同名分支并设为上游
        if self.remote_names.is_empty() {
            window.push_notification(
                Notification::warning(tr(
                    "此仓库尚无远程。请先在侧栏的「远程」上右键选择「添加远程…」。",
                ))
                .autohide(false),
                cx,
            );
            return;
        }
        let remote = self.default_remote();
        let push = vec![
            "push".into(),
            "--progress".into(),
            "--set-upstream".into(),
            remote.clone(),
            branch.clone(),
        ];
        self.run_net_op(tr("推送"), push, window, cx, move |_, _, window, cx| {
            let text = tf!(
                "已推送，{branch} 的上游已设为 {remote}/{branch}",
                branch,
                remote
            );
            window.push_notification(Notification::success(text), cx);
        });
    }

    pub(super) fn checkout_branch(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.status.as_ref().and_then(|s| s.branch.as_deref()) == Some(name) {
            return;
        }
        // 已经在别的工作树里检出了：git 不让检出两处，改成问要不要进那个工作树
        if self.offer_tree_holding(name, window, cx) {
            return;
        }
        let name = name.to_string();
        self.with_local_changes(
            tf!("切换到 {name}", name = &name),
            tr("保留改动并切换"),
            window,
            cx,
            move |this, stash, window, cx| {
                let name = name.clone();
                let checkout = vec!["checkout".into(), "-q".into(), name.clone(), "--".into()];
                this.run_stashed(
                    tr("切换分支"),
                    vec![Step::local(checkout)],
                    stash,
                    window,
                    cx,
                    move |_, _, window, cx| {
                        window.push_notification(
                            Notification::success(tf!("已切换到 {name}", name)),
                            cx,
                        );
                    },
                );
            },
        );
    }

    /// 工作区里有没有已跟踪文件的改动（切换分支、拉取时会碍事的那种；未跟踪的文件一般不碍事）。
    pub(super) fn has_tracked_changes(&self) -> bool {
        self.status.as_ref().is_some_and(|status| {
            status.entries.iter().any(|entry| {
                entry.staged.is_some()
                    || entry
                        .unstaged
                        .is_some_and(|kind| kind != fr_core::status::Kind::Untracked)
            })
        })
    }

    /// 切换分支、检出、拉取之前：工作区有改动就先问怎么处理，再调用 `run(this, 要不要先贮藏, …)`；
    /// 没有改动直接调用。`what` 是要做的事（「切换到 main」「拉取」），`keep` 是「不贮藏」那一项的叫法。
    /// 选了哪样会记下来，下次默认选它。
    pub(super) fn with_local_changes(
        &mut self,
        what: impl Into<SharedString>,
        keep: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        run: impl Fn(&mut Self, bool, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if !self.has_tracked_changes() {
            run(self, false, window, cx);
            return;
        }
        let what: SharedString = what.into();
        let count = self.status.as_ref().map_or(0, |s| s.entries.len());
        let choose = super::dialogs::Choose {
            title: tr("工作区有未提交的改动").into(),
            description: tf!(
                "{count} 个文件有未提交的改动。执行「{what}」前如何处理？",
                count,
                what
            )
            .into(),
            options: vec![
                (
                    keep.into(),
                    tr("git 的默认方式：改动与要检出的内容冲突时拒绝执行，不做任何更改。").into(),
                ),
                (
                    tr("先贮藏，完成后恢复").into(),
                    tr("先贮藏改动（含未跟踪的文件），完成后自动恢复；恢复时如有冲突，贮藏保留在侧栏中。").into(),
                ),
            ],
            selected: usize::from(self.app.read(cx).settings.stash_before_switch),
            checks: Vec::new(),
            ok: tr("继续"),
        };
        self.choose(choose, window, cx, move |this, picked, _, window, cx| {
            let stash = picked == 1;
            this.app.update(cx, |app, cx| {
                if app.settings.stash_before_switch != stash {
                    app.update_settings(cx, |s| s.stash_before_switch = stash);
                }
            });
            run(this, stash, window, cx);
        });
    }

    /// 执行 `steps`（切换分支、检出这类不会停在一半的操作）。`stash` 为真时先把工作区的改动贮藏起来，
    /// 做完以后放回去——没做成也放回去，工作区回到原来的样子。
    pub(super) fn run_stashed(
        &mut self,
        label: &'static str,
        steps: Vec<Step>,
        stash: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_success: impl FnOnce(&mut Self, &GitOutput, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if !stash {
            self.run_seq(label, steps, window, cx, on_success);
            return;
        }
        // 贮藏的说明里带一个记号：放回去时按它找，不会弹出用户自己的贮藏
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let marker = format!("kuai-{millis}");
        let message = tf!(
            "{label}前自动贮藏的改动（{marker}）",
            label,
            marker = &marker
        );
        let push = fr_core::stash::push_args(Some(&message), true);
        self.run_op_with(label, push, window, cx, move |this, result, window, cx| {
            if let Err(text) = result {
                failure(window, cx, label, text);
                return;
            }
            this.run_steps(
                label,
                steps,
                window,
                cx,
                Box::new(move |this, result, window, cx| {
                    this.restore_stash(label, marker, result, on_success, window, cx)
                }),
            );
        });
    }

    /// [`Self::run_stashed`] 的后一半：找到刚才自动贮藏的那一份，放回工作区，然后报告结果。
    fn restore_stash(
        &mut self,
        label: &'static str,
        marker: String,
        result: Result<GitOutput, String>,
        on_success: impl FnOnce(&mut Self, &GitOutput, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let list = channel.git(&self.entry.path, fr_core::refs::stash_args());
        cx.spawn_in(window, async move |this, cx| {
            let selector = list
                .await
                .ok()
                .and_then(|out| find_stash(&out.stdout, &marker));
            this.update_in(cx, |this, window, cx| {
                let Some(selector) = selector else {
                    // 刚才没有贮藏下来东西（比如只有子模块里的改动）：就当没贮藏过
                    match result {
                        Ok(out) => on_success(this, &out, window, cx),
                        Err(text) => failure(window, cx, label, text),
                    }
                    return;
                };
                let pop = vec!["stash".into(), "pop".into(), selector];
                this.run_op_with(
                    tr("恢复改动"),
                    pop,
                    window,
                    cx,
                    move |this, popped, window, cx| match (result, popped) {
                        (Ok(out), Ok(_)) => on_success(this, &out, window, cx),
                        (Ok(_), Err(text)) => {
                            this.show_changes(cx);
                            let title = tf!("{label}已完成，但恢复改动时发生冲突", label);
                            let note = Notification::warning(tr(
                                "请在「本地修改」中解决冲突并暂存。自动贮藏仍在侧栏中，确认无遗漏后再删除。",
                            ))
                            .title(title.clone())
                            .autohide(false);
                            window.push_notification(with_detail(note, &title, &text), cx);
                        }
                        (Err(text), Ok(_)) => {
                            let text =
                                tf!("{text}\n\n改动已恢复到工作区。", text);
                            failure(window, cx, label, text);
                        }
                        (Err(text), Err(pop)) => {
                            let text = tf!(
                                "{text}\n\n恢复改动也失败，改动仍在侧栏的「贮藏」中：\n{pop}",
                                text,
                                pop
                            );
                            failure(window, cx, label, text);
                        }
                    },
                );
            })
            .ok();
        })
        .detach();
    }

    /// 检出远程分支：有跟踪它的同名本地分支就切过去，没有就新建一个跟踪它的。
    /// 同名的本地分支跟踪的是别的远程分支时，问用户要一个新名字。
    pub(super) fn checkout_remote(
        &mut self,
        remote: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((remote_name, local)) = remote.split_once('/') else {
            return;
        };
        match self.refs.branches.iter().find(|b| b.name == local) {
            None => self.track_remote(local.to_string(), remote.to_string(), window, cx),
            Some(branch) if branch.upstream.as_deref() == Some(remote) => {
                self.checkout_branch(local, window, cx);
            }
            Some(branch) => {
                let tracking = match &branch.upstream {
                    Some(upstream) => tf!("跟踪 {upstream}", upstream),
                    None => tr("未设置上游").into(),
                };
                let ask = super::dialogs::Ask {
                    title: tf!("本地分支 {local} 已存在", local).into(),
                    placeholder: tr("新本地分支名称").into(),
                    value: format!("{local}-{remote_name}"),
                    hint: tf!("本地分支 {local} {tracking}，而非 {remote}。请输入新名称新建分支，或取消后在侧栏中双击该分支。", local, tracking, remote)
                    .into(),
                    ok: tr("新建并切换"),
                    validate: Some(Box::new(|this, answer| {
                        super::dialogs::ref_name_error(&answer.text)
                            .or_else(|| this.branch_exists_error(&answer.text))
                    })),
                    ..Default::default()
                };
                let remote = remote.to_string();
                self.ask(ask, window, cx, move |this, answer, window, cx| {
                    this.track_remote(answer.text, remote.clone(), window, cx);
                });
            }
        }
    }

    fn track_remote(
        &mut self,
        local: String,
        remote: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_local_changes(
            tf!("切换到 {local}", local = &local),
            tr("保留改动并切换"),
            window,
            cx,
            move |this, stash, window, cx| {
                let (local, remote) = (local.clone(), remote.clone());
                let checkout = vec![
                    "checkout".into(),
                    "-q".into(),
                    "-b".into(),
                    local.clone(),
                    "--track".into(),
                    remote.clone(),
                ];
                this.run_stashed(
                    tr("切换分支"),
                    vec![Step::local(checkout)],
                    stash,
                    window,
                    cx,
                    move |_, _, window, cx| {
                        window.push_notification(
                            Notification::success(tf!(
                                "已新建并切换到 {local}，跟踪 {remote}",
                                local,
                                remote
                            )),
                            cx,
                        );
                    },
                );
            },
        );
    }

    /// 删除本地分支。先用 `-d`（只删已经合并的）；git 说还没合并时，再问一次要不要强制删。
    pub(super) fn delete_branch(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let description = tf!("删除本地分支 {name}，远程同名分支不受影响。", name);
        self.confirm(
            tr("删除分支？"),
            description,
            tr("删除"),
            window,
            cx,
            move |this, window, cx| this.run_delete_branch(name.clone(), false, window, cx),
        );
    }

    fn run_delete_branch(
        &mut self,
        name: String,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let flag = if force { "-D" } else { "-d" };
        let delete = vec!["branch".into(), flag.into(), "--".into(), name.clone()];
        self.run_op_with(
            tr("删除分支"),
            delete,
            window,
            cx,
            move |this, result, window, cx| match result {
                Ok(_) => {
                    window
                        .push_notification(Notification::success(tf!("已删除分支 {name}", name)), cx);
                }
                // `-d` 不肯删，一般是分支还没合并。不去认 git 的报错文字（它跟着系统语言变），
                // 原样给用户看，由用户决定要不要强制删
                Err(text) if !force => {
                    let description = tf!(
                        "git 未删除 {name}：\n{text}\n\n强制删除后，仅在此分支上的提交将难以找回。",
                        name,
                        text
                    );
                    this.confirm(
                        tr("强制删除分支？"),
                        description,
                        tr("强制删除"),
                        window,
                        cx,
                        move |this, window, cx| {
                            this.run_delete_branch(name.clone(), true, window, cx)
                        },
                    );
                }
                Err(text) => failure(window, cx, tr("删除分支"), text),
            },
        );
    }

    /// 工具栏上的「弹出贮藏」：先说清楚是哪一条、会从列表里删掉，确认了再做。
    pub(super) fn stash_pop_latest(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(stash) = self.refs.stashes.first() else {
            return;
        };
        let description = tf!(
            "恢复贮藏「{message}」（{selector}）并从列表中删除。如需保留，请在侧栏中对其使用「应用」。",
            message = stash.message,
            selector = stash.selector
        );
        let selector = stash.selector.clone();
        self.confirm_plain(
            tr("弹出贮藏？"),
            description,
            tr("弹出"),
            window,
            cx,
            move |this, window, cx| this.stash_pop(selector.clone(), window, cx),
        );
    }

    /// 弹出贮藏：应用并从列表里删除。
    pub(super) fn stash_pop(
        &mut self,
        selector: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pop = vec!["stash".into(), "pop".into(), selector];
        self.run_op(
            tr("弹出贮藏"),
            pop,
            None,
            window,
            cx,
            |this, _, window, cx| {
                this.show_changes(cx);
                window
                    .push_notification(Notification::success(tr("改动已恢复，贮藏已删除")), cx);
            },
        );
    }

    pub(super) fn stash_apply(
        &mut self,
        selector: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let apply = vec!["stash".into(), "apply".into(), selector];
        self.run_op(
            tr("应用贮藏"),
            apply,
            None,
            window,
            cx,
            |this, _, window, cx| {
                this.show_changes(cx);
                window
                    .push_notification(Notification::success(tr("改动已恢复，贮藏已保留")), cx);
            },
        );
    }

    pub(super) fn stash_drop(
        &mut self,
        selector: String,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let description = tf!(
            "{selector}（{message}）将被删除，此操作无法撤销。",
            selector,
            message
        );
        self.confirm(
            tr("删除贮藏？"),
            description,
            tr("删除"),
            window,
            cx,
            move |this, window, cx| {
                let drop = vec!["stash".into(), "drop".into(), selector.clone()];
                this.run_op(
                    tr("删除贮藏"),
                    drop,
                    None,
                    window,
                    cx,
                    |_, _, window, cx| {
                        window.push_notification(Notification::success(tr("已删除此贮藏")), cx);
                    },
                );
            },
        );
    }
}

pub(super) fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// 报错通知里 git 原文最多放几行；完整的内容点「复制全部」拿。
const SHOWN_LINES: usize = 30;
/// git 原文那一块最高这么高，再多就在块里滚动：通知不能把提交按钮和改动区都盖住。
const DETAIL_HEIGHT: f32 = 96.;

/// 操作失败的通知：常见的情况先用中文说原因和下一步，下面一个小块放 git 的原文（太长只留末尾，
/// 块里可以滚动），「复制全部」把完整的报错放进剪贴板。通知不会自己消失。
pub(super) fn failure(window: &mut Window, cx: &mut App, label: &str, detail: String) {
    let hint = explain(&detail).unwrap_or(tr("git 返回的错误信息如下。"));
    let title = tf!("{label}失败", label);
    let note = Notification::error(hint)
        .title(title.clone())
        .autohide(false);
    window.push_notification(with_detail(note, &title, &detail), cx);
}

/// 给通知加上 git 的原文：放在一个最高 `DETAIL_HEIGHT` 的小块里，下面是「复制全部」
/// （复制的是标题加完整的原文）。原文不直接当通知的正文：一长就把半个窗口盖住了。
/// 「停在一半」那条提示的标识：同一时间只留一条，状态往前走了就收掉。
struct StoppedNote;

pub(super) fn with_detail(note: Notification, title: &str, detail: &str) -> Notification {
    let shown = SharedString::from(tail(detail, SHOWN_LINES));
    let full = format!("{title}\n{detail}");
    note.content(move |_, _, cx| {
        let theme = cx.theme();
        let full = full.clone();
        v_flex()
            .mt_2()
            .gap_2()
            .child(
                div()
                    .id("error-detail")
                    .max_h(px(DETAIL_HEIGHT))
                    .overflow_y_scroll()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme.muted)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(shown.clone()),
            )
            .child(
                h_flex().child(
                    Button::new("copy-error")
                        .xsmall()
                        .label(tr("复制全部"))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(full.clone()));
                        }),
                ),
            )
            .into_any_element()
    })
}

/// 常见失败的中文说明和下一步。认的是 git 报错里不随语言变的那部分；认不出来就只显示原文。
pub(crate) fn explain(detail: &str) -> Option<&'static str> {
    let text = detail.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| text.contains(w));
    if has(&["not possible to fast-forward", "can't be fast-forwarded"]) {
        Some(tr(
            "双方都有新提交，无法快进，未执行任何操作。请选择其他合并方式。",
        ))
    } else if has(&["non-fast-forward", "fetch first", "stale info"]) {
        Some(tr("推送被拒绝：远程有本地没有的提交。请先拉取。"))
    } else if has(&[
        "authentication failed",
        "permission denied",
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
    ]) {
        Some(tr("认证失败。请检查用户名、密码或密钥。"))
    } else if has(&[
        "could not resolve host",
        "connection timed out",
        "connection refused",
        "network is unreachable",
        "unable to access",
        "connection reset",
    ]) {
        Some(tr("无法连接远程仓库。请检查网络、代理和远程地址。"))
    } else if has(&[
        "would be overwritten",
        "please commit your changes or stash them",
    ]) {
        Some(tr("本地改动将被覆盖，操作未执行。请先提交或贮藏改动后重试。"))
    } else if has(&["index.lock", "another git process"]) {
        Some(tr(
            "仓库已锁定：其他 git 进程正在使用，或上次操作未正常结束。请稍后重试；如仍锁定，请删除 .git/index.lock。",
        ))
    } else if has(&["not fully merged"]) {
        Some(tr("此分支包含尚未合并的提交。"))
    } else if has(&["no upstream", "has no upstream branch"]) {
        Some(tr("此分支未设置上游，推送后将自动设置。"))
    } else if has(&["unmerged files", "unresolved conflict", "fix conflicts"]) {
        Some(tr("存在未解决的冲突。请在「本地修改」中解决后重试。"))
    } else if has(&["not concluded your merge", "merge_head exists"]) {
        Some(tr("上一次合并尚未提交。请先点击上方的「提交合并」或「中止」。"))
    } else if has(&["nothing to commit", "no changes added to commit"]) {
        Some(tr("没有可提交的改动。"))
    } else if has(&["already exists"]) {
        Some(tr("此名称已被使用。"))
    } else if has(&[
        "not a valid",
        "invalid refspec",
        "is not a valid branch name",
    ]) {
        Some(tr(
            "名称无效：不能包含空格、~ ^ : ? * [ 或连续的两个点，也不能以 / 或 . 结尾。",
        ))
    } else {
        None
    }
}

/// 在 `git stash list` 的输出里找说明里带 `marker` 的那一份，返回 `stash@{n}`。
fn find_stash(list: &[u8], marker: &str) -> Option<String> {
    fr_core::refs::parse(&[], list)
        .stashes
        .into_iter()
        .find(|stash| stash.message.contains(marker))
        .map(|stash| stash.selector)
}

/// 最后 `n` 行；前面有省略时在开头注明省了多少行。
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= n {
        return lines.join("\n");
    }
    tf!(
        "…（已省略前 {n} 行，「复制全部」可获取完整内容）\n{n2}",
        n = lines.len() - n,
        n2 = lines[lines.len() - n..].join("\n")
    )
}

/// 给用户看的命令行：`git` 加参数，太长就截断。
fn command_line(args: &[String]) -> String {
    let mut text = format!("git {}", args.join(" "));
    if text.chars().count() > 80 {
        text = text.chars().take(80).collect::<String>() + "…";
    }
    text
}

/// 进度里最新的一行。git 用回车在同一行上刷新进度，所以回车和换行都算分隔。
fn last_progress_line(stderr: &[u8]) -> String {
    let tail = &stderr[stderr.len().saturating_sub(2000)..];
    let text = String::from_utf8_lossy(tail);
    text.split(['\r', '\n'])
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// 去掉用回车刷新掉的进度：每行只留最后一个回车后面的内容。
fn strip_progress(text: &str) -> String {
    text.lines()
        .map(|line| {
            line.trim_end_matches('\r')
                .rsplit('\r')
                .next()
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 出错时给用户看的内容：标准错误加标准输出（有些错误比如「没有可提交的」打在标准输出）。
fn output_text(out: &GitOutput) -> String {
    let mut text = strip_progress(out.stderr.trim());
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(stdout.trim());
    }
    if text.is_empty() {
        text = tf!("git 退出码 {code}", code = format!("{:?}", out.code));
    }
    text
}

/// 成功时的简短结果：标准输出的前几行，没有就用标准错误的。
fn summary(out: &GitOutput) -> String {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = strip_progress(&out.stderr);
    let text = if stdout.trim().is_empty() {
        stderr.as_str()
    } else {
        stdout.as_ref()
    };
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(4)
        .collect();
    if lines.is_empty() {
        tr("完成").into()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::{command_line, explain, find_stash, last_progress_line, strip_progress, tail};

    #[test]
    fn 按记号找自动贮藏的那一份() {
        let list = "stash@{0}\x1faaa\x1f1700000002\x1fOn main: 手工贮藏的\0\
                    stash@{1}\x1fbbb\x1f1700000001\x1fOn main: 切换分支之前自动贮藏的改动（kuai-123）\0";
        assert_eq!(
            find_stash(list.as_bytes(), "kuai-123").as_deref(),
            Some("stash@{1}")
        );
        assert_eq!(find_stash(list.as_bytes(), "kuai-999"), None);
        assert_eq!(find_stash(b"", "kuai-123"), None);
    }

    #[test]
    fn 进度取最新的一行() {
        let stderr = b"remote: Counting objects: 100% (5/5), done.\nReceiving objects:  12% (1/8)\rReceiving objects:  50% (4/8)\r";
        assert_eq!(last_progress_line(stderr), "Receiving objects:  50% (4/8)");
        assert_eq!(last_progress_line(b""), "");
    }

    #[test]
    fn 报错里去掉刷新掉的进度() {
        let text = "Writing objects:  50% (1/2)\rWriting objects: 100% (2/2), done.\nfatal: 出错了";
        assert_eq!(
            strip_progress(text),
            "Writing objects: 100% (2/2), done.\nfatal: 出错了"
        );
    }

    #[test]
    fn 常见的失败有中文说明() {
        let rejected =
            " ! [rejected]        feat -> feat (non-fast-forward)\nerror: failed to push some refs";
        assert!(explain(rejected).unwrap().contains("先拉取"));
        assert!(
            explain("fatal: Authentication failed for 'https://example.com/a.git/'")
                .unwrap()
                .contains("认证")
        );
        assert!(
            explain("fatal: unable to access 'https://example.com/': Could not resolve host")
                .unwrap()
                .contains("无法连接")
        );
        assert!(
            explain("error: You have not concluded your merge (MERGE_HEAD exists).")
                .unwrap()
                .contains("提交合并")
        );
        assert!(explain("error: 别的什么错").is_none());
    }

    #[test]
    fn 报错太长时只留末尾并注明() {
        let text = (1..=20)
            .map(|i| format!("第 {i} 行"))
            .collect::<Vec<_>>()
            .join("\n");
        let shown = tail(&text, 12);
        assert!(shown.starts_with("…（已省略前 8 行"));
        assert!(shown.ends_with("第 20 行"));
        assert_eq!(tail("一行", 12), "一行");
    }

    #[test]
    fn 命令行太长会截断() {
        let args: Vec<String> = vec!["push".into(), "x".repeat(200)];
        assert!(command_line(&args).ends_with('…'));
        assert_eq!(command_line(&["status".to_string()]), "git status");
    }
}
