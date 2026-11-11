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
    fn run_steps(
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
                Notification::warning(tf!("等「{running}」做完再试", running)),
                cx,
            );
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
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
                });
                if marked.is_err() {
                    return;
                }
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
                if !matches!(&result, Ok(out) if out.success()) {
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
                        let title = tf!("{name}还没做完，又停下了", name = state.name());
                        let note = Notification::warning(
                            tr("到「本地修改」里看看：有新的冲突就解决后再点「继续」；这个提交不要了点「跳过这个提交」。"),
                        )
                        .title(title.clone())
                        .autohide(false);
                        window.push_notification(with_detail(note, &title, &text), cx);
                    }
                    (Some(state), Err(_)) => {
                        this.in_progress = Some(state);
                        this.show_changes(cx);
                        let note = Notification::warning(
                            tr("在「本地修改」里解决冲突，然后点上面的「继续」；不想做了点「中止」。"),
                        )
                        .title(tf!("{name}遇到冲突，停下了", name = state.name()));
                        window.push_notification(note, cx);
                    }
                    (_, result) => {
                        // 多步操作：说清楚前面哪几步已经做完了，免得用户以为什么都没做
                        let result = result.map_err(|text| {
                            if total > 1 && !done.is_empty() {
                                tf!("前面 {done} 步已经做完：{join}\n失败的是：{failed_step}\n\n{text}", done = done.len(), join = done.join(tr("；")), failed_step, text)
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
                Notification::warning(tr("现在是分离的 HEAD，先切换到一个分支再拉取")),
                cx,
            );
            return;
        };
        let candidate = format!("{}/{branch}", self.default_remote());
        if self.refs.remotes.iter().any(|b| b.name == candidate) {
            let description = tf!(
                "{branch} 还没有上游分支。把它的上游设为 {candidate}，然后拉取。",
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
                "{branch} 还没有上游分支，远程上也没有同名的分支。先推送一次（会自动设好上游）；要跟踪别的远程分支，在侧栏的远程分支上点右键，选「设为当前分支的上游」。",
                branch
            );
            window.push_notification(Notification::warning(text).autohide(false), cx);
        }
    }

    /// 拉取分两步：先获取（联网，可以取消），再合并或变基（改工作区，不能中途杀掉）。
    fn run_pull(
        &mut self,
        mut steps: Vec<Step>,
        rebase: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        steps.push(Step::net(args(&["fetch", "--progress"])));
        // --no-edit：要合并时直接用默认的合并说明，不弹编辑器
        let mut merge = args(&["pull", "--no-edit"]);
        if rebase {
            merge.push("--rebase".into());
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
                Notification::warning(tr("现在是分离的 HEAD，先切换到一个分支再推送")),
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
                    "这个仓库还没有远程。在侧栏「远程」上点右键，选「添加远程…」，再推送。",
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
                "推送完成，{branch} 的上游设为 {remote}/{branch}",
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
        let name = name.to_string();
        let checkout = vec!["checkout".into(), "-q".into(), name.clone(), "--".into()];
        self.run_op(
            tr("切换分支"),
            checkout,
            None,
            window,
            cx,
            move |_, _, window, cx| {
                window.push_notification(Notification::success(tf!("已切换到 {name}", name)), cx);
            },
        );
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
                    Some(upstream) => tf!("跟踪的是 {upstream}", upstream),
                    None => tr("没有上游").into(),
                };
                let ask = super::dialogs::Ask {
                    title: tf!("本地已经有分支 {local}", local).into(),
                    placeholder: tr("新的本地分支名").into(),
                    value: format!("{local}-{remote_name}"),
                    hint: tf!("本地的 {local} {tracking}，不是 {remote}。填一个名字，新建跟踪 {remote} 的分支；想用原来的 {local}，取消后在侧栏双击它。", local, tracking, remote)
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
        let checkout = vec![
            "checkout".into(),
            "-q".into(),
            "-b".into(),
            local.clone(),
            "--track".into(),
            remote.clone(),
        ];
        self.run_op(
            tr("切换分支"),
            checkout,
            None,
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
    }

    /// 删除本地分支。先用 `-d`（只删已经合并的）；git 说还没合并时，再问一次要不要强制删。
    pub(super) fn delete_branch(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let description = tf!("删除本地分支 {name}。远程上的同名分支不受影响。", name);
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
                    let description = tf!("git 没有删除 {name}：\n{text}\n\n强制删除后，只在这个分支上的提交很难再找回来。", name, text);
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
            "恢复最近一次贮藏「{message}」（{selector}）里的改动，并把它从贮藏列表里删除。只想恢复、不删除，在侧栏的贮藏上点「应用」。",
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
                window.push_notification(
                    Notification::success(tr(
                        "贮藏里的改动已经恢复到工作区，这条贮藏已从列表里删除",
                    )),
                    cx,
                );
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
                window.push_notification(
                    Notification::success(tr("贮藏里的改动已经恢复到工作区，这条贮藏还留在列表里")),
                    cx,
                );
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
            "{selector}（{message}）会被删除，不能撤销。",
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
                        window.push_notification(Notification::success(tr("已删除这条贮藏")), cx);
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
    let hint = explain(&detail).unwrap_or(tr("git 报的错在下面。"));
    let title = tf!("{label}失败", label);
    let note = Notification::error(hint)
        .title(title.clone())
        .autohide(false);
    window.push_notification(with_detail(note, &title, &detail), cx);
}

/// 给通知加上 git 的原文：放在一个最高 `DETAIL_HEIGHT` 的小块里，下面是「复制全部」
/// （复制的是标题加完整的原文）。原文不直接当通知的正文：一长就把半个窗口盖住了。
fn with_detail(note: Notification, title: &str, detail: &str) -> Notification {
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
    if has(&["non-fast-forward", "fetch first", "stale info"]) {
        Some(tr(
            "远程分支上有本地没有的提交，所以被拒绝了。先拉取（或获取后变基），再推送。",
        ))
    } else if has(&[
        "authentication failed",
        "permission denied",
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
    ]) {
        Some(tr(
            "认证没通过。检查用户名、密码或密钥有没有这个仓库的权限，然后再试。",
        ))
    } else if has(&[
        "could not resolve host",
        "connection timed out",
        "connection refused",
        "network is unreachable",
        "unable to access",
        "connection reset",
    ]) {
        Some(tr("连不上远程。检查网络、代理和远程的地址，然后再试。"))
    } else if has(&[
        "would be overwritten",
        "please commit your changes or stash them",
    ]) {
        Some(tr(
            "本地有改动会被覆盖，所以没有做。先提交或贮藏这些改动，然后再试。",
        ))
    } else if has(&["index.lock", "another git process"]) {
        Some(tr(
            "有别的 git 进程正在用这个仓库，或者上次的操作没正常结束留下了锁文件。等一下再试；一直不行就删掉 .git 目录里的 index.lock。",
        ))
    } else if has(&["not fully merged"]) {
        Some(tr("这个分支还有没合并进来的提交。"))
    } else if has(&["no upstream", "has no upstream branch"]) {
        Some(tr("这个分支还没有上游分支。推送一次会自动设好。"))
    } else if has(&["unmerged files", "unresolved conflict", "fix conflicts"]) {
        Some(tr("还有没解决的冲突。到「本地修改」里解决后再试。"))
    } else if has(&["not concluded your merge", "merge_head exists"]) {
        Some(tr(
            "上一次合并还没提交。先点上面横幅里的「提交合并」（或者「中止」），然后再试。",
        ))
    } else if has(&["nothing to commit", "no changes added to commit"]) {
        Some(tr("没有可以提交的改动。"))
    } else if has(&["already exists"]) {
        Some(tr("这个名字已经有了，换一个。"))
    } else if has(&[
        "not a valid",
        "invalid refspec",
        "is not a valid branch name",
    ]) {
        Some(tr(
            "名字不合规则：不能有空格、~ ^ : ? * [ 和连着的两个点，也不能以 / 或 . 结尾。",
        ))
    } else {
        None
    }
}

/// 最后 `n` 行；前面有省略时在开头注明省了多少行。
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= n {
        return lines.join("\n");
    }
    tf!(
        "…（前面还有 {n} 行，点「复制全部」看完整的）\n{n2}",
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
    use super::{command_line, explain, last_progress_line, strip_progress, tail};

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
                .contains("连不上")
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
        assert!(shown.starts_with("…（前面还有 8 行"));
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
