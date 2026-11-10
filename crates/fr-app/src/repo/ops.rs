//! 写操作：执行 git，出错时弹通知，做完刷新。还有获取、拉取、推送、切换分支、贮藏这些具体操作。
//!
//! 同一个仓库同一时间只做一个写操作，免得几个 git 进程争抢 index.lock。

use std::rc::Rc;

use fr_channel::{GitOutput, StreamItem};
use fr_core::state;
use futures::StreamExt as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::RepoView;

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
        let label = label.into();
        let name = label.clone();
        self.run_op_with(
            label,
            args,
            stdin,
            false,
            window,
            cx,
            move |this, result, window, cx| match result {
                Ok(out) => on_success(this, &out, window, cx),
                Err(text) => failure(window, cx, &name, text),
            },
        );
    }

    /// 执行联网的操作（获取、拉取、推送）：工具栏上显示 git 报的进度，可以取消。
    pub(super) fn run_net_op(
        &mut self,
        label: &'static str,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_success: impl FnOnce(&mut Self, &GitOutput, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.run_op_with(
            label,
            args,
            None,
            true,
            window,
            cx,
            move |this, result, window, cx| match result {
                Ok(out) => on_success(this, &out, window, cx),
                Err(text) => failure(window, cx, label, text),
            },
        );
    }

    /// 依次执行几条 git 命令，算一个写操作；有一条失败就停。成功时调用 `on_success`（给的是最后一条的输出）。
    pub(super) fn run_seq(
        &mut self,
        label: &'static str,
        steps: Vec<Vec<String>>,
        progress: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_success: impl FnOnce(&mut Self, &GitOutput, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let steps = steps.into_iter().map(|args| (args, None)).collect();
        self.run_steps(
            label,
            steps,
            progress,
            window,
            cx,
            move |this, result, window, cx| match result {
                Ok(out) => on_success(this, &out, window, cx),
                Err(text) => failure(window, cx, label, text),
            },
        );
    }

    /// 执行一个写操作，做完把结果交给 `on_done`（失败时是给用户看的报错），然后刷新状态。
    /// `progress` 为真时边做边显示标准错误里的进度（此时不能带标准输入）。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_op_with(
        &mut self,
        label: impl Into<SharedString>,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
        progress: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_done: impl FnOnce(&mut Self, Result<GitOutput, String>, &mut Window, &mut Context<Self>)
        + 'static,
    ) {
        self.run_steps(label, vec![(args, stdin)], progress, window, cx, on_done);
    }

    /// 写操作的实现：依次执行 `steps`（参数和标准输入），有一条失败就停。
    fn run_steps(
        &mut self,
        label: impl Into<SharedString>,
        steps: Vec<(Vec<String>, Option<Vec<u8>>)>,
        progress: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_done: impl FnOnce(&mut Self, Result<GitOutput, String>, &mut Window, &mut Context<Self>)
        + 'static,
    ) {
        let label = label.into();
        if let Some(running) = &self.op {
            window.push_notification(
                Notification::warning(format!("等「{running}」做完再试")),
                cx,
            );
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        for (args, _) in &steps {
            log::info!("{}：git {}", self.entry.path, args.join(" "));
        }
        let repo = self.entry.path.clone();
        let was_in_progress = self.in_progress.is_some();
        self.op = Some(label);
        self.op_progress = progress.then(SharedString::default);
        self.op_task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut result = Ok(GitOutput {
                code: Some(0),
                ..Default::default()
            });
            for (args, stdin) in steps {
                result = if progress {
                    let mut stream = channel.git_progress(&repo, args);
                    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
                    loop {
                        match stream.next().await {
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
                            None => break Err(anyhow::anyhow!("连接中断")),
                        }
                    }
                } else {
                    channel.git_with_input(&repo, args, stdin).await
                };
                if !matches!(&result, Ok(out) if out.success()) {
                    break;
                }
            }
            // 失败了：看看是不是合并、变基这类操作遇到冲突停下了。那不算出错，换一种提示
            let failed = !matches!(&result, Ok(out) if out.success());
            let stopped = if failed && !was_in_progress {
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
                this.changes.mark_stale();
                let result = match result {
                    Ok(out) if out.success() => Ok(out),
                    Ok(out) => Err(output_text(&out)),
                    Err(err) => Err(format!("{err:#}")),
                };
                match stopped {
                    Some(state) => {
                        this.in_progress = Some(state);
                        this.show_changes(cx);
                        let note = Notification::warning(
                            "在「本地修改」里解决冲突，然后点上面的「继续」；不想做了点「中止」。",
                        )
                        .title(format!("{}遇到冲突，停下了", state.name()));
                        window.push_notification(note, cx);
                    }
                    None => on_done(this, result, window, cx),
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// 中止正在进行的联网操作：丢弃任务，对应的 git 进程会被杀掉。
    pub(super) fn cancel_op(&mut self, cx: &mut Context<Self>) {
        if self.op_progress.is_none() {
            return;
        }
        self.op_task = None;
        self.op = None;
        self.op_progress = None;
        self.refresh(cx);
        cx.notify();
    }

    /// 弹出确认框，用户点了确定才执行 `action`。用于丢弃改动、删除贮藏这类不能撤销的操作。
    pub(super) fn confirm(
        &mut self,
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
        ok: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let (title, description) = (title.into(), description.into());
        let view = cx.entity().downgrade();
        let action = Rc::new(action);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (view, action) = (view.clone(), action.clone());
            alert
                .title(title.clone())
                .description(description.clone())
                .show_cancel(true)
                .cancel_text("取消")
                .ok_text(ok)
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    view.update(cx, |this, cx| action(this, window, cx)).ok();
                    true
                })
        });
    }

    pub(super) fn fetch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_net_op(
            "获取",
            args(&["fetch", "--all", "--progress"]),
            window,
            cx,
            |_, _, window, cx| {
                window.push_notification(Notification::success("获取完成"), cx);
            },
        );
    }

    pub(super) fn pull(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.status.as_ref().is_some_and(|s| s.upstream.is_none()) {
            window.push_notification(
                Notification::warning("当前分支没有上游分支，不知道从哪拉取"),
                cx,
            );
            return;
        }
        // --no-edit：要合并时直接用默认的合并说明，不弹编辑器
        self.run_net_op(
            "拉取",
            args(&["pull", "--no-edit", "--progress"]),
            window,
            cx,
            |_, out, window, cx| {
                window.push_notification(Notification::success(summary(out)).title("拉取完成"), cx);
            },
        );
    }

    pub(super) fn push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = &self.status else {
            return;
        };
        let Some(branch) = status.branch.clone() else {
            window.push_notification(
                Notification::warning("现在是分离的 HEAD，先切换到一个分支再推送"),
                cx,
            );
            return;
        };
        // 还没有上游：推到 origin 的同名分支并设为上游
        let first = status.upstream.is_none();
        let push_args = if first {
            vec![
                "push".into(),
                "--progress".into(),
                "--set-upstream".into(),
                "origin".into(),
                branch.clone(),
            ]
        } else {
            args(&["push", "--progress"])
        };
        self.run_net_op("推送", push_args, window, cx, move |_, _, window, cx| {
            let text = if first {
                format!("推送完成，{branch} 的上游设为 origin/{branch}")
            } else {
                "推送完成".into()
            };
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
            "切换分支",
            checkout,
            None,
            window,
            cx,
            move |_, _, window, cx| {
                window.push_notification(Notification::success(format!("已切换到 {name}")), cx);
            },
        );
    }

    /// 双击远程分支：有同名本地分支就切过去，没有就新建一个跟踪它的。
    pub(super) fn checkout_remote(
        &mut self,
        remote: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, local)) = remote.split_once('/') else {
            return;
        };
        if self.refs.branches.iter().any(|b| b.name == local) {
            self.checkout_branch(local, window, cx);
            return;
        }
        let (local, remote) = (local.to_string(), remote.to_string());
        let checkout = vec![
            "checkout".into(),
            "-q".into(),
            "-b".into(),
            local.clone(),
            "--track".into(),
            remote.clone(),
        ];
        self.run_op(
            "切换分支",
            checkout,
            None,
            window,
            cx,
            move |_, _, window, cx| {
                window.push_notification(
                    Notification::success(format!("已新建并切换到 {local}，跟踪 {remote}")),
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
        let description = format!("删除本地分支 {name}。远程上的同名分支不受影响。");
        self.confirm(
            "删除分支？",
            description,
            "删除",
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
            "删除分支",
            delete,
            None,
            false,
            window,
            cx,
            move |this, result, window, cx| match result {
                Ok(_) => {
                    window
                        .push_notification(Notification::success(format!("已删除分支 {name}")), cx);
                }
                // `-d` 不肯删，一般是分支还没合并。不去认 git 的报错文字（它跟着系统语言变），
                // 原样给用户看，由用户决定要不要强制删
                Err(text) if !force => {
                    let description = format!(
                        "git 没有删除 {name}：\n{text}\n\n强制删除后，只在这个分支上的提交很难再找回来。"
                    );
                    this.confirm(
                        "强制删除分支？",
                        description,
                        "强制删除",
                        window,
                        cx,
                        move |this, window, cx| {
                            this.run_delete_branch(name.clone(), true, window, cx)
                        },
                    );
                }
                Err(text) => failure(window, cx, "删除分支", text),
            },
        );
    }

    /// 弹出贮藏：`selector` 为空时弹出最近一次的。
    pub(super) fn stash_pop(
        &mut self,
        selector: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut pop = args(&["stash", "pop"]);
        pop.extend(selector);
        self.run_op("弹出贮藏", pop, None, window, cx, |_, _, _, _| {});
    }

    pub(super) fn stash_apply(
        &mut self,
        selector: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let apply = vec!["stash".into(), "apply".into(), selector];
        self.run_op("应用贮藏", apply, None, window, cx, |_, _, _, _| {});
    }

    pub(super) fn stash_drop(
        &mut self,
        selector: String,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let description = format!("{selector}（{message}）会被删除，不能撤销。");
        self.confirm(
            "删除贮藏？",
            description,
            "删除",
            window,
            cx,
            move |this, window, cx| {
                let drop = vec!["stash".into(), "drop".into(), selector.clone()];
                this.run_op("删除贮藏", drop, None, window, cx, |_, _, _, _| {});
            },
        );
    }
}

pub(super) fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn failure(window: &mut Window, cx: &mut App, label: &str, detail: String) {
    window.push_notification(
        Notification::error(detail)
            .title(format!("{label}失败"))
            .autohide(false),
        cx,
    );
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

/// 出错时给用户看的内容：标准错误加标准输出（有些错误比如「没有可提交的」打在标准输出），最多 12 行。
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
        text = format!("git 退出码 {:?}", out.code);
    }
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(12)..].join("\n")
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
        "完成".into()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::{last_progress_line, strip_progress};

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
}
