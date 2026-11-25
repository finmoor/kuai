//! 写操作：执行 git，出错时弹通知，做完刷新。还有获取、拉取、推送、切换分支、贮藏这些具体操作。
//!
//! 同一个仓库同一时间只做一个写操作，免得几个 git 进程争抢 index.lock。

use fr_core::i18n::tr;
use fr_core::tf;
use std::rc::Rc;

use fr_channel::{Channel, GitOutput, StreamItem};
use fr_core::pull;
use fr_core::state;
use futures::channel::oneshot;
use futures::{FutureExt as _, StreamExt as _};
use gpui_kit::assets::IconName;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::RepoView;
use super::impact::{Impact, Row, Tone, list_commits};

/// 写操作里的一步。
/// 怎么拉取：照配置（合并或变基）、用变基、只快进。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PullMode {
    Default,
    Rebase,
    FfOnly,
}

#[derive(Clone)]
pub(super) struct Step {
    pub args: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    /// 联网的命令：边做边显示 git 报的进度，可以取消。别的命令会改暂存区和工作区，
    /// 中途杀掉会留下锁文件，所以不给取消。
    pub net: bool,
    /// 给人看的写法（活动记录、失败的详情）：借 shell 别名的命令写成等价的命令，不露出别名那段
    /// 脚本。空着就照 `args` 写
    pub shown: Option<String>,
}

impl Step {
    pub fn local(args: Vec<String>) -> Self {
        Self {
            args,
            stdin: None,
            net: false,
            shown: None,
        }
    }

    pub fn net(args: Vec<String>) -> Self {
        Self {
            net: true,
            ..Self::local(args)
        }
    }

    /// 从标准输入喂内容的本地命令。
    pub fn input(args: Vec<String>, stdin: Vec<u8>) -> Self {
        Self {
            stdin: Some(stdin),
            ..Self::local(args)
        }
    }

    pub fn shown(self, text: String) -> Self {
        Self {
            shown: Some(text),
            ..self
        }
    }

    /// 活动记录里写的命令：给了 [`Self::shown`] 用它，没给照参数写全（`-c` 也写上）。
    fn logged(&self) -> String {
        match &self.shown {
            Some(text) => text.clone(),
            None => format!("git {}", self.args.join(" ")),
        }
    }

    /// `--` 后面的路径里有认不出的字符：文件名不是 UTF-8，读状态时换成了 U+FFFD。交给 git 的名字
    /// 和真的对不上，碰巧和别的文件同名还会改错文件。
    fn lossy_path(&self) -> bool {
        self.args
            .iter()
            .skip_while(|arg| *arg != "--")
            .any(|arg| arg.contains(char::REPLACEMENT_CHARACTER))
    }
}

type OnDone =
    Box<dyn FnOnce(&mut RepoView, Result<GitOutput, String>, &mut Window, &mut Context<RepoView>)>;

/// 动手之前要再核对一遍的一项（见 [`RepoView::verify_then`]）：git 参数，和确认时看到的输出
/// （去掉首尾空白；命令失败算 None）。
pub(super) type Check = (Vec<String>, Option<String>);

/// 要丢弃、覆盖的几个文件在确认框打开时的内容（见 [`RepoView::guard_content`]）。
#[derive(Clone)]
pub(super) struct ContentGuard {
    tracked: Vec<String>,
    untracked: Vec<String>,
    first: futures::future::Shared<Task<Result<u64, String>>>,
}

/// 几个文件内容的指纹，只用来比较前后两次是否一样。已跟踪的看工作区对暂存区的差异（`--full-index`
/// 让二进制文件的内容也体现在对象号上），这正是丢弃、覆盖会去掉的东西；未跟踪的看文件列表（目录
/// 展开成里面的文件）和每个文件的对象号。git 的退出码和报错也算进去：两次报同样的错算一样，交给后面
/// 真正执行时报。连接出错才返回 Err。
async fn content_fingerprint(
    channel: &Channel,
    repo: &str,
    tracked: &[String],
    untracked: &[String],
) -> Result<u64, String> {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut feed = |out: &GitOutput| {
        out.code.hash(&mut hasher);
        out.stdout.hash(&mut hasher);
        out.stderr.hash(&mut hasher);
    };
    if !tracked.is_empty() {
        let mut list = args(&[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--full-index",
            "--",
        ]);
        list.extend(tracked.iter().cloned());
        let out = channel
            .git(repo, list)
            .await
            .map_err(|err| format!("{err:#}"))?;
        feed(&out);
    }
    if !untracked.is_empty() {
        let mut list = args(&["ls-files", "--others", "--exclude-standard", "-z", "--"]);
        list.extend(untracked.iter().cloned());
        let listed = channel
            .git(repo, list)
            .await
            .map_err(|err| format!("{err:#}"))?;
        feed(&listed);
        // 换行分隔的路径列表：名字里带换行的文件对不上，它的增删照样体现在上面的列表里
        let files: Vec<&[u8]> = listed
            .stdout
            .split(|b| *b == 0)
            .filter(|name| !name.is_empty() && !name.contains(&b'\n'))
            .collect();
        if listed.success() && !files.is_empty() {
            let stdin = files.join(&b'\n');
            let hashed = channel
                .git_with_input(repo, args(&["hash-object", "--stdin-paths"]), Some(stdin))
                .await
                .map_err(|err| format!("{err:#}"))?;
            feed(&hashed);
        }
    }
    Ok(hasher.finish())
}

impl RepoView {
    /// HEAD 现在的样子：在哪个分支上、指向哪个提交（按读到的状态）。给 [`Self::verify_then`] 核对用。
    pub(super) fn head_checks(&self) -> Vec<Check> {
        let status = self.status.as_ref();
        let branch = status
            .and_then(|status| status.branch.as_ref())
            .map(|branch| format!("refs/heads/{branch}"));
        let head = status.and_then(|status| status.head.clone());
        vec![
            (args(&["symbolic-ref", "-q", "HEAD"]), branch),
            (args(&["rev-parse", "-q", "--verify", "HEAD"]), head),
        ]
    }

    /// 确认框开着的这一会儿仓库可能变了（别的窗口、终端里的 git、自动获取）：动手之前把 `checks`
    /// 再读一遍，都和确认时看到的一样才做 `then`；不一样就不做，说一声，重新读状态。原来照确认前
    /// 记下的分支名、贮藏的编号直接执行，可能作用到确认框里没写的分支、贮藏上。读和做之间还有很短
    /// 的空当，没有锁。
    pub(super) fn verify_then(
        &mut self,
        checks: Vec<Check>,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let mut same = true;
            for (args, expect) in checks {
                let out = channel.git(&repo, args).await;
                let got = out
                    .ok()
                    .filter(GitOutput::success)
                    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                    .filter(|text| !text.is_empty());
                if got != expect {
                    same = false;
                    break;
                }
            }
            this.update_in(cx, |this, window, cx| {
                if same {
                    return then(this, window, cx);
                }
                let text = tr("仓库在确认期间有了变化，没有执行。请查看最新状态后重试。");
                window.push_notification(Notification::warning(text), cx);
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// 打开丢弃、覆盖的确认框时调用：开始读这几个文件现在的内容，确认后交给
    /// [`Self::verify_content_then`] 再核对一遍。`untracked` 是未跟踪的文件和目录。
    pub(super) fn guard_content(
        &self,
        tracked: Vec<String>,
        untracked: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Option<ContentGuard> {
        use futures::FutureExt as _;
        let channel = self.channel.clone()?;
        let repo = self.entry.path.clone();
        let (t, u) = (tracked.clone(), untracked.clone());
        let first = cx
            .spawn(async move |_, _| content_fingerprint(&channel, &repo, &t, &u).await)
            .shared();
        Some(ContentGuard {
            tracked,
            untracked,
            first,
        })
    }

    /// 确认之后：在主机上再读一遍这几个文件的内容，和确认框打开时一样才做 `then`。不一样说明确认框
    /// 开着时文件又被改了（agent 还在写、在别处编辑），照旧执行会把没看过的内容一起丢掉，所以不做，
    /// 提示 `changed`，重新读状态。读和做之间仍有很短的空当，没有锁。
    pub(super) fn verify_content_then(
        &mut self,
        guard: ContentGuard,
        changed: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let first = guard.first.await;
            let now = content_fingerprint(&channel, &repo, &guard.tracked, &guard.untracked).await;
            this.update_in(cx, |this, window, cx| match (first, now) {
                (Ok(before), Ok(now)) if before == now => then(this, window, cx),
                (Ok(_), Ok(_)) => {
                    window.push_notification(Notification::warning(changed), cx);
                    this.refresh(cx);
                }
                (Err(err), _) | (_, Err(err)) => {
                    let text = tf!("无法核对文件内容，未执行：{err}", err);
                    window.push_notification(Notification::error(text), cx);
                }
            })
            .ok();
        })
        .detach();
    }

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
            stdin,
            ..Step::local(args)
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
                Err(text) => this.failure(&name, text, window, cx),
            }),
        );
    }

    /// 操作失败的通知（见 [`failure_note`]）；联过网的在「复制全部」旁边带「查看输出」，底部能看
    /// 完整的输出。
    pub(super) fn failure(
        &self,
        label: &str,
        detail: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hint = explain(&detail).unwrap_or(tr("git 返回的错误信息如下。"));
        let title = tf!("{label}失败", label);
        let note = Notification::error(hint)
            .title(title.clone())
            .autohide(false);
        let note = with_detail_and(note, &title, &detail, self.output_opener(cx));
        window.push_notification(note, cx);
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
        if steps.iter().any(Step::lossy_path) {
            let text =
                tr("文件名里有无法识别的字符（不是 UTF-8 编码），无法操作，请在终端中处理。");
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        // 同一个仓库在别的页面里正在写（子仓库页、另开的标签页、总览的批量操作）
        let key = writing_key(self.entry.server.as_deref(), &self.entry.path);
        let writing = match Writing::take(key, label.clone()) {
            Ok(writing) => writing,
            Err(running) => {
                let text = tf!(
                    "该仓库正在另一个页面中执行「{running}」，请等待完成后重试",
                    running
                );
                window.push_notification(Notification::warning(text), cx);
                return;
            }
        };
        // 后台的自动获取让路：用户要做的事优先。等它的 git 真的停了再开始，两条 git 不会抢同一个引用的锁
        let stopping = self.stop_auto_fetch();
        for step in &steps {
            let command = step.args.join(" ");
            log::info!(
                "{}：git {}",
                self.entry.path,
                fr_core::git::redact_urls(&command)
            );
        }
        let repo = self.entry.path.clone();
        let was_in_progress = self.in_progress.is_some();
        // 用了 git 自己的自动贮藏，或者停在一半的合并、变基这次可能做完（git 那时才放回改动）：做完看
        // 贮藏列表里有没有多出它放不回去时留下的那一份（见 [`kept_autostash`]）
        let autostash = was_in_progress
            || steps
                .iter()
                .any(|step| step.args.iter().any(|arg| arg == "--autostash"));
        let stashed: Vec<String> = self
            .refs
            .stashes
            .iter()
            .map(|stash| stash.target.clone())
            .collect();
        // 继续、跳过之后又停下，说明后面的提交也停住了，不算出错
        // 按标签认：和 `continue_in_progress`、`skip_in_progress` 里起的标签是同一句话
        let resuming = was_in_progress
            && self.in_progress.is_some_and(|state| {
                label.as_ref() == tf!("继续{name}", name = state.name())
                    || label.as_ref() == tr("跳过")
            });
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let net_op = steps.iter().any(|step| step.net);
        let push_op = steps
            .iter()
            .any(|step| step.net && step.args.iter().any(|arg| arg == "push"));
        self.output_start(label.clone(), net_op, push_op);
        self.op = Some(label.clone());
        self.op_writing = Some(writing);
        self.op_progress = None;
        self.op_cancel = Some(cancel_tx);
        self.op_task = Some(cx.spawn_in(window, async move |this, cx| {
            if let Some(stopping) = stopping {
                // 最多等 10 秒：连接卡住、kuaid 不回话时照样往下做
                let limit = cx
                    .background_executor()
                    .timer(std::time::Duration::from_secs(10));
                futures::future::select(stopping, Box::pin(limit)).await;
            }
            let mut cancel_rx = cancel_rx.fuse();
            let mut result = Ok(GitOutput {
                code: Some(0),
                ..Default::default()
            });
            let total = steps.len();
            let mut done: Vec<String> = Vec::new();
            let mut failed_step = String::new();
            let mut cancelled = false;
            // 推送途中连接断了：推送可能已经完成，结果不能当成失败报
            let mut push_unknown = false;
            for step in steps {
                let is_push = step.net && step.args.iter().any(|arg| arg == "push");
                let is_fetch = step.net && step.args.iter().any(|arg| arg == "fetch");
                let shown = match &step.shown {
                    Some(text) => clip(text.clone()),
                    None => command_line(&step.args),
                };
                // 工具栏上有没有「取消」，跟着这一步走
                let net = step.net;
                let marked = this.update(cx, |this, cx| {
                    this.op_progress = net.then(SharedString::default);
                    cx.notify();
                    let id = this.activity.begin(label.clone(), step.logged());
                    this.output.step(id);
                    id
                });
                let Ok(activity) = marked else {
                    return;
                };
                result = if step.net {
                    let mut stream = channel.git_progress(&repo, step.args);
                    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
                    // 底部的输出隔一会儿更新一次：大仓库的进度一秒刷几十次，每次都整理全部输出太费
                    let mut shown_at: Option<std::time::Instant> = None;
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
                                // 进度（回车刷新的）隔一会儿更新一次；换了行的（远程说的话）马上更新，
                                // 不然 hook 说完一句停下来等的时候，底部的输出里还看不到这一句
                                let live = chunk.contains(&b'\n')
                                    || shown_at.is_none_or(|at| {
                                        at.elapsed() >= std::time::Duration::from_millis(200)
                                    });
                                if live {
                                    shown_at = Some(std::time::Instant::now());
                                }
                                let text = live.then(|| String::from_utf8_lossy(&stderr).into_owned());
                                let shown = this.update(cx, |this, cx| {
                                    this.op_progress = Some(line.into());
                                    if let Some(text) = &text {
                                        this.activity.live(activity, text);
                                    }
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
                            Some(Err(err)) => {
                                push_unknown = is_push;
                                break Err(err);
                            }
                            None => {
                                push_unknown = is_push;
                                break Err(anyhow::anyhow!(tr("连接中断")));
                            }
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
                    // 获取（拉取的第一步也是）：工具栏上「上次获取」跟着变
                    match &result {
                        _ if !is_fetch || cancelled => {}
                        Ok(out) if out.success() => this.fetch_succeeded(),
                        Ok(out) => this.fetch_failed(&output_text(out), false),
                        Err(err) => this.fetch_failed(&format!("{err:#}"), false),
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
            let kept = match autostash && !cancelled {
                true => channel
                    .git(&repo, fr_core::refs::stash_args())
                    .await
                    .ok()
                    .is_some_and(|out| kept_autostash(&out.stdout, &stashed)),
                false => false,
            };
            this.update_in(cx, |this, window, cx| {
                this.op = None;
                this.op_writing = None;
                this.op_progress = None;
                this.op_cancel = None;
                // 先收进「最近的操作」：下面的通知要带「查看输出」时认的是它
                this.output.finish();
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
                    // 推送时连接断了：远程可能已经收到了。不说「失败」（照着重推、强推可能出错），
                    // 说结果未知，请重新连上以后获取、核对远程分支
                    (_, Err(text)) if push_unknown => {
                        let note = Notification::warning(tr(
                            "推送过程中连接中断，提交可能已推送到远程。请在重新连接后点击「获取」，核对远程分支后再操作。",
                        ))
                        .title(tr("推送结果未知"))
                        .autohide(false);
                        // 断开前远程说过的话（hook 的输出）在「查看输出」里
                        let note =
                            with_detail_and(note, tr("推送结果未知"), &text, this.output_opener(cx));
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
                if kept {
                    this.show_changes(cx);
                    let note = Notification::warning(tr(
                        "改动仍保存在侧栏的贮藏「autostash」中。请在「本地修改」中处理冲突，确认无遗漏后再删除该贮藏。",
                    ))
                    .title(tr("自动贮藏的改动未能完整恢复"))
                    .autohide(false);
                    window.push_notification(note, cx);
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
            |this, _, window, cx| {
                this.notify_net(Notification::success(tr("获取完成")), window, cx);
            },
        );
    }

    /// 拉取，`mode` 是合并、变基还是只快进（见 [`PullMode`]）。
    pub(super) fn pull(&mut self, mode: PullMode, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = &self.status else {
            return;
        };
        if status.upstream.is_some() {
            self.run_pull(Vec::new(), mode, window, cx);
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
                    this.run_pull(vec![set], mode, window, cx);
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
    /// 合并遇到冲突停下时，等合并提交或中止以后再放回来）；拉取菜单里勾了「拉取前自动贮藏本地
    /// 修改」的不问，直接贮藏。
    fn run_pull(
        &mut self,
        steps: Vec<Step>,
        mode: PullMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.app.read(cx).settings.pull_autostash && self.has_tracked_changes() {
            self.run_pull_now(steps, mode, true, window, cx);
            return;
        }
        self.with_local_changes(
            tr("拉取"),
            tr("不贮藏，直接拉取"),
            window,
            cx,
            move |this, stash, window, cx| {
                this.run_pull_now(steps.clone(), mode, stash, window, cx)
            },
        );
    }

    /// 拉取菜单里的「拉取前自动贮藏本地修改」：开、关。
    pub(super) fn toggle_pull_autostash(&mut self, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            app.update_settings(cx, |s| s.pull_autostash = !s.pull_autostash)
        });
    }

    /// 拉取分两步：先获取（联网，可以取消），再照配置合并或变基（改工作区，不能中途杀掉，也不再
    /// 联网；原来第二步是 `git pull`，它又获取一次，这一次取消不了）。见 [`pull`]。
    fn run_pull_now(
        &mut self,
        mut steps: Vec<Step>,
        mode: PullMode,
        autostash: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let branch = self.status.as_ref().and_then(|s| s.branch.clone());
        let read = channel.git(&self.entry.path, pull::config_args());
        cx.spawn_in(window, async move |this, cx| {
            // 什么都没配时 git config 以 1 退出：按空的算
            let config = read.await.map(|out| out.stdout).unwrap_or_default();
            let branch = branch.as_deref().unwrap_or_default();
            let finish = match mode {
                PullMode::FfOnly => pull::Finish::FfOnly,
                _ => pull::decide(&config, branch, mode == PullMode::Rebase),
            };
            this.update_in(cx, |this, window, cx| {
                steps.push(Step::net(pull::fetch_args()));
                let missing = tr("远程上找不到上游分支（可能已被删除），未合并。");
                let merge = Step::local(pull::finish_args(finish, autostash, missing));
                steps.push(merge.shown(pull::finish_shown(finish, autostash)));
                let rebased = matches!(finish, pull::Finish::Rebase { .. });
                this.run_seq(
                    tr("拉取"),
                    steps,
                    window,
                    cx,
                    move |this, out, window, cx| {
                        let title = if rebased {
                            tr("拉取完成（变基）")
                        } else {
                            tr("拉取完成")
                        };
                        this.notify_net(
                            Notification::success(summary(out)).title(title),
                            window,
                            cx,
                        );
                    },
                );
            })
            .ok();
        })
        .detach();
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
            self.run_net_op(tr("推送"), push, window, cx, |this, _, window, cx| {
                this.notify_net(Notification::success(tr("推送完成")), window, cx);
            });
            return;
        }
        // 还没有上游：先确认推到哪个远程
        self.push_new_branch(branch, window, cx);
    }

    /// 推送菜单的「推送并设置上游…」：选远程，推到那里的同名分支并设为上游（已经有上游的也能换）。
    pub(super) fn push_set_upstream(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(branch) = self.status.as_ref().and_then(|s| s.branch.clone()) else {
            window.push_notification(
                Notification::warning(tr("当前处于分离的 HEAD 状态，请先切换到分支再推送")),
                cx,
            );
            return;
        };
        self.push_to_remote(branch, false, window, cx);
    }

    /// 第一次推送一个还没有上游的分支：先说清推到哪个远程（地址）的哪个分支，可以换远程、
    /// 选要不要设为上游，确认后才推。工具栏的「推送」、提交并推送、侧栏分支菜单的「推送」共用。
    /// 原来直接推到 origin（没有就第一个远程），推完才说推到了哪里；fork 的仓库里两个远程都在时容易推错。
    pub(super) fn push_new_branch(
        &mut self,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.push_to_remote(branch, true, window, cx);
    }

    /// 选远程推送 `branch`：`fresh` 是从没推送过的（[`Self::push_new_branch`]），否则是「推送并设置
    /// 上游…」，默认选现在的上游所在的远程。
    fn push_to_remote(
        &mut self,
        branch: String,
        fresh: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
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
        let remotes = self.remote_names.clone();
        let default = match fresh {
            true => self.default_remote(),
            false => self
                .upstream_remote()
                .unwrap_or_else(|| self.default_remote()),
        };
        let read = channel.git(
            &self.entry.path,
            args(&["config", "--get-regexp", r"^remote\..*\.(push)?url$"]),
        );
        cx.spawn_in(window, async move |this, cx| {
            // 推送地址：单独配了 pushurl 用它，没有就是 url
            let config = read.await.map(|out| out.stdout).unwrap_or_default();
            let text = String::from_utf8_lossy(&config);
            let url_of = |remote: &str| {
                let find = |key: &str| {
                    let key = format!("remote.{remote}.{key} ");
                    text.lines()
                        .find_map(|line| line.strip_prefix(key.as_str()).map(str::to_string))
                };
                find("pushurl").or_else(|| find("url")).unwrap_or_default()
            };
            let options: Vec<(SharedString, SharedString)> = remotes
                .iter()
                .map(|remote| {
                    let url = url_of(remote);
                    let shown = fr_core::git::redact_urls(&url).into_owned();
                    (remote.clone().into(), shown.into())
                })
                .collect();
            let selected = remotes.iter().position(|r| *r == default).unwrap_or(0);
            this.update_in(cx, |this, window, cx| {
                let (title, description) = match fresh {
                    true => (
                        tf!("推送新分支 {branch}？", branch = branch.as_str()),
                        tf!(
                            "{branch} 尚未推送过，将在所选远程上创建同名分支。",
                            branch = branch.as_str()
                        ),
                    ),
                    false => (
                        tf!("推送 {branch} 并设置上游", branch = branch.as_str()),
                        tf!(
                            "将 {branch} 推送到所选远程的同名分支，并设为它的上游。",
                            branch = branch.as_str()
                        ),
                    ),
                };
                let choose = super::dialogs::Choose {
                    title: title.into(),
                    description: description.into(),
                    options,
                    selected,
                    checks: vec![(tr("设为上游（以后直接推送、拉取此分支）"), true)],
                    ok: tr("推送"),
                };
                this.choose(choose, window, cx, move |this, ix, checks, window, cx| {
                    let Some(remote) = remotes.get(ix).cloned() else {
                        return;
                    };
                    let upstream = checks.first().copied().unwrap_or(true);
                    let push = super::menus::push_args(&branch, &remote, &branch, upstream, None);
                    let text = match upstream {
                        true => tf!(
                            "已推送，{branch} 的上游已设为 {remote}/{branch}",
                            branch = branch.as_str(),
                            remote = remote.as_str()
                        ),
                        false => tf!(
                            "{name} 已推送到 {to}",
                            name = branch.as_str(),
                            to = format!("{remote}/{branch}")
                        ),
                    };
                    this.run_net_op(
                        tr("推送"),
                        push,
                        window,
                        cx,
                        move |this, _, window, cx| {
                            this.notify_net(Notification::success(text), window, cx);
                        },
                    );
                });
            })
            .ok();
        })
        .detach();
    }

    /// 当前分支的上游在哪个远程（远程的名字里可以有 `/`：取最长的那个前缀）。
    pub(super) fn upstream_remote(&self) -> Option<String> {
        let upstream = self.status.as_ref()?.upstream.as_deref()?;
        self.remote_names
            .iter()
            .filter(|remote| {
                upstream
                    .strip_prefix(remote.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|remote| remote.len())
            .cloned()
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
        let target = name.to_string();
        self.leave_detached_then(target.clone(), window, cx, move |this, window, cx| {
            this.checkout_branch_now(target.clone(), window, cx)
        });
    }

    fn checkout_branch_now(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
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
    /// 按提交放回（`stash apply <提交>`），不按 `stash@{N}`：别处（终端里的 git、编辑器）同时存了
    /// 贮藏、序号挪了也不会放错。放回了再从贮藏列表里删掉它，冲突时留着。
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
            let commit = list
                .await
                .ok()
                .and_then(|out| find_stash(&out.stdout, &marker));
            this.update_in(cx, |this, window, cx| {
                let Some(commit) = commit else {
                    // 刚才没有贮藏下来东西（比如只有子模块里的改动）：就当没贮藏过
                    match result {
                        Ok(out) => on_success(this, &out, window, cx),
                        Err(text) => failure(window, cx, label, text),
                    }
                    return;
                };
                // 带 --index：原来暂存的改动放回去还是暂存的
                let apply = vec![
                    "stash".into(),
                    "apply".into(),
                    "--index".into(),
                    commit.clone(),
                ];
                this.run_op_with(
                    tr("恢复改动"),
                    apply,
                    window,
                    cx,
                    move |this, applied, window, cx| {
                        // 暂存区放不回去（git 说 Conflicts in index. Try without --index.，这时它什么都
                        // 没动）：改用不带 --index 的，改动都放回工作区，不再分暂存和未暂存，说一声
                        if let Err(text) = &applied
                            && text.contains("--index")
                        {
                            let apply = vec!["stash".into(), "apply".into(), commit.clone()];
                            this.run_op_with(
                                tr("恢复改动"),
                                apply,
                                window,
                                cx,
                                move |this, applied, window, cx| {
                                    let restored = applied.is_ok();
                                    if restored {
                                        this.drop_autostash(commit, window, cx);
                                    }
                                    this.report_restore(
                                        label, result, applied, on_success, window, cx,
                                    );
                                    if restored {
                                        let text =
                                            tr("原来暂存的改动未能保持暂存，已全部放回工作区。");
                                        window.push_notification(Notification::warning(text), cx);
                                    }
                                },
                            );
                            return;
                        }
                        if applied.is_ok() {
                            this.drop_autostash(commit, window, cx);
                        }
                        this.report_restore(label, result, applied, on_success, window, cx);
                    },
                );
            })
            .ok();
        })
        .detach();
    }

    /// 改动放回以后，把自动贮藏的那一份从贮藏列表里删掉；没删成时说一声，它留在侧栏里。
    fn drop_autostash(&mut self, commit: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let path = self.entry.path.clone();
        cx.spawn_in(window, async move |this, cx| {
            if !drop_stash_commit(&channel, &path, &commit).await {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                let text = tr(
                    "改动已放回工作区，但自动贮藏未能从贮藏列表中删除，确认无误后可在侧栏中删除。",
                );
                window.push_notification(Notification::warning(text), cx);
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// [`Self::restore_stash`] 的结果：操作本身（`result`）和放回改动（`popped`）各自成没成。
    fn report_restore(
        &mut self,
        label: &'static str,
        result: Result<GitOutput, String>,
        popped: Result<GitOutput, String>,
        on_success: impl FnOnce(&mut Self, &GitOutput, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match (result, popped) {
            (Ok(out), Ok(_)) => on_success(self, &out, window, cx),
            (Ok(_), Err(text)) => {
                self.show_changes(cx);
                let title = tf!("{label}已完成，但恢复改动时发生冲突", label);
                let note = Notification::warning(tr(
                    "请在「本地修改」中解决冲突并暂存。自动贮藏仍在侧栏中，确认无遗漏后再删除。",
                ))
                .title(title.clone())
                .autohide(false);
                window.push_notification(with_detail(note, &title, &text), cx);
            }
            (Err(text), Ok(_)) => {
                let text = tf!("{text}\n\n改动已恢复到工作区。", text);
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
        }
    }

    /// 检出远程分支：有跟踪它的同名本地分支就切过去，没有就新建一个跟踪它的。
    /// 同名的本地分支跟踪的是别的远程分支时，问用户要一个新名字。
    pub(super) fn checkout_remote(
        &mut self,
        remote: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = remote.to_string();
        self.leave_detached_then(target.clone(), window, cx, move |this, window, cx| {
            this.checkout_remote_now(&target, window, cx)
        });
    }

    fn checkout_remote_now(&mut self, remote: &str, window: &mut Window, cx: &mut Context<Self>) {
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
        // 删掉以后 git 连它的 reflog 一起删：先记下它指着哪，删成了存进「找回提交」
        let tip = self
            .refs
            .branches
            .iter()
            .find(|branch| branch.name == name)
            .map(|branch| branch.target.clone());
        self.run_op_with(
            tr("删除分支"),
            delete,
            window,
            cx,
            move |this, result, window, cx| match result {
                Ok(_) => {
                    if let Some(tip) = tip.clone() {
                        this.remember_deleted(vec![(name.clone(), tip)], cx);
                    }
                    window.push_notification(
                        Notification::success(tf!("已删除分支 {name}", name)),
                        cx,
                    );
                }
                // `-d` 不肯删，一般是分支还没合并。不去认 git 的报错文字（它跟着系统语言变）：
                // 数出只在这条分支上的提交，由用户决定要不要强制删
                Err(text) if !force => this.confirm_force_delete(name, text, window, cx),
                Err(text) => failure(window, cx, tr("删除分支"), text),
            },
        );
    }

    /// `git branch -d` 没删掉：列出只在这条分支上的提交、远程上有没有同名分支，问要不要强制删。
    fn confirm_force_delete(
        &mut self,
        name: String,
        git_said: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let tip = self
            .refs
            .branches
            .iter()
            .find(|branch| branch.name == name)
            .map(|branch| branch.target.clone());
        // 同名的远程分支：`origin/name` 这样的
        let remote_same: Vec<String> = self
            .refs
            .remotes
            .iter()
            .filter(|remote| remote.name.split_once('/').is_some_and(|(_, b)| b == name))
            .map(|remote| remote.name.clone())
            .collect();
        let range = vec![
            format!("refs/heads/{name}"),
            "--not".into(),
            format!("--exclude={name}"),
            "--branches".into(),
            "--remotes".into(),
            "--tags".into(),
        ];
        let only = list_commits(channel, self.entry.path.clone(), range, false, false);
        cx.spawn_in(window, async move |this, cx| {
            let only = only.await;
            this.update_in(cx, |this, window, cx| {
                let description = match only.total {
                    0 => tf!(
                        "git 未删除 {name}：{text}",
                        name = &name,
                        text = git_said.trim()
                    ),
                    _ => tr("这条分支有没合并到其他分支的提交。").to_string(),
                };
                let mut rows = vec![match only.total {
                    0 => Row::new(
                        Tone::Plain,
                        IconName::GitCommitHorizontal,
                        "只在此分支",
                        tr("没有只在这条分支上的提交，别的分支上都有。"),
                    ),
                    _ => Row::new(
                        Tone::Warn,
                        IconName::GitCommitHorizontal,
                        "只在此分支",
                        only.count_text(),
                    )
                    .listed(&only),
                }];
                rows.push(Row::new(
                    Tone::Plain,
                    IconName::Cloud,
                    "远程",
                    match remote_same.is_empty() {
                        true => tr("远程上没有同名分支。").to_string(),
                        false => tf!(
                            "远程上有 {names}，删除本地分支不影响它。",
                            names = remote_same.join(tr("、"))
                        ),
                    },
                ));
                if let (Some(tip), true) = (&tip, only.total > 0) {
                    rows.push(Row::new(
                        Tone::Good,
                        IconName::ArchiveRestore,
                        "怎么找回",
                        tf!(
                            "Kuai 会记下它指向 {id}，在侧栏的「找回提交」里可以按原名恢复。",
                            id = &tip[..tip.len().min(8)]
                        ),
                    ));
                }
                let impact = Impact {
                    description: description.into(),
                    rows,
                };
                this.confirm_impact(
                    tf!("强制删除分支 {name}？", name = &name),
                    impact,
                    tr("强制删除"),
                    true,
                    window,
                    cx,
                    move |this, window, cx| this.run_delete_branch(name.clone(), true, window, cx),
                );
            })
            .ok();
        })
        .detach();
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
        let checks = self.stash_checks(&stash.selector);
        let selector = stash.selector.clone();
        self.confirm_plain(
            tr("弹出贮藏？"),
            description,
            tr("弹出"),
            window,
            cx,
            move |this, window, cx| {
                this.stash_pop_checked(selector.clone(), checks.clone(), window, cx)
            },
        );
    }

    /// 贮藏的编号（`stash@{n}`）是按位置排的，别处新建、删掉一条就指向别的了。记下它现在指向的那个
    /// 提交，动手之前核对编号还是不是这一条（见 [`Self::verify_then`]）。
    fn stash_checks(&self, selector: &str) -> Vec<Check> {
        let target = self
            .refs
            .stashes
            .iter()
            .find(|stash| stash.selector == selector)
            .map(|stash| stash.target.clone());
        vec![(args(&["rev-parse", "-q", "--verify", selector]), target)]
    }

    /// 弹出贮藏：应用并从列表里删除。
    pub(super) fn stash_pop(
        &mut self,
        selector: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let checks = self.stash_checks(&selector);
        self.stash_pop_checked(selector, checks, window, cx);
    }

    fn stash_pop_checked(
        &mut self,
        selector: String,
        checks: Vec<Check>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pop = vec!["stash".into(), "pop".into(), selector];
        self.verify_then(checks, window, cx, move |this, window, cx| {
            this.run_op(
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
        });
    }

    pub(super) fn stash_apply(
        &mut self,
        selector: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let checks = self.stash_checks(&selector);
        let apply = vec!["stash".into(), "apply".into(), selector];
        self.verify_then(checks, window, cx, move |this, window, cx| {
            this.run_op(
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
        });
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
        // 确认框写的是这一条：确认时编号已经指向别的贮藏了就不删
        let checks = self.stash_checks(&selector);
        self.confirm(
            tr("删除贮藏？"),
            description,
            tr("删除"),
            window,
            cx,
            move |this, window, cx| {
                let drop = vec!["stash".into(), "drop".into(), selector.clone()];
                this.verify_then(checks.clone(), window, cx, move |this, window, cx| {
                    this.run_op(
                        tr("删除贮藏"),
                        drop,
                        None,
                        window,
                        cx,
                        |_, _, window, cx| {
                            let text = tr("已删除此贮藏");
                            window.push_notification(Notification::success(text), cx);
                        },
                    );
                });
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
    window.push_notification(failure_note(label, detail), cx);
}

/// 操作失败的通知：认得出的写中文说明，下面是 git 的原文。
pub(super) fn failure_note(label: &str, detail: String) -> Notification {
    let hint = explain(&detail).unwrap_or(tr("git 返回的错误信息如下。"));
    let title = tf!("{label}失败", label);
    let note = Notification::error(hint)
        .title(title.clone())
        .autohide(false);
    with_detail(note, &title, &detail)
}

/// 给通知加上 git 的原文：放在一个最高 `DETAIL_HEIGHT` 的小块里，下面是「复制全部」
/// （复制的是标题加完整的原文）。原文不直接当通知的正文：一长就把半个窗口盖住了。
/// 「停在一半」那条提示的标识：同一时间只留一条，状态往前走了就收掉。
struct StoppedNote;

pub(super) fn with_detail(note: Notification, title: &str, detail: &str) -> Notification {
    with_detail_and(note, title, detail, None)
}

/// 点了打开底部「输出」的按钮（见 `output.rs` 的 `output_opener`）。
pub(super) type OpenOutput = Rc<dyn Fn(&mut Window, &mut App)>;

/// 同 [`with_detail`]；给了 `open` 时「复制全部」旁边多一个「查看输出」，点了收起这条通知。
pub(super) fn with_detail_and(
    note: Notification,
    title: &str,
    detail: &str,
    open: Option<OpenOutput>,
) -> Notification {
    let shown = SharedString::from(tail(detail, SHOWN_LINES));
    let full = format!("{title}\n{detail}");
    note.content(move |_, _, cx| {
        let theme = cx.theme();
        let full = full.clone();
        let me = cx.entity().downgrade();
        let open = open.clone();
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
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("copy-error")
                            .xsmall()
                            .label(tr("复制全部"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(full.clone()));
                            }),
                    )
                    .when_some(open, |row, open| {
                        row.child(
                            Button::new("view-output")
                                .xsmall()
                                .label(tr("查看输出"))
                                .on_click(move |_, window, cx| {
                                    open(window, cx);
                                    me.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                                }),
                        )
                    }),
            )
            .into_any_element()
    })
}

/// 常见失败的中文说明和下一步。认的是 git 报错里不随语言变的那部分；认不出来就只显示原文。
pub(crate) fn explain(detail: &str) -> Option<&'static str> {
    let text = detail.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| text.contains(w));
    if has(&[
        "hook declined",
        "protected branch",
        "gh006",
        "[remote rejected]",
    ]) {
        Some(tr(
            "远程拒绝了这次推送（服务器上的检查未通过，例如受保护的分支）。原因见输出中 remote: 开头的行。",
        ))
    } else if has(&["not possible to fast-forward", "can't be fast-forwarded"]) {
        Some(tr(
            "双方都有新提交，无法快进，未执行任何操作。请选择其他合并方式。",
        ))
    } else if has(&["non-fast-forward", "fetch first", "stale info"]) {
        Some(tr("推送被拒绝：远程有本地没有的提交。请先拉取。"))
    } else if has(&["unable to read askpass response"]) {
        // 要用户名、密码的框没得到回答：用户点了取消，或者在后台、弹不出框。说「认证失败、检查密码」
        // 不对，根本没输
        Some(tr("未输入登录信息，操作未执行。"))
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
        Some(tr(
            "本地改动将被覆盖，操作未执行。请先提交或贮藏改动后重试。",
        ))
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
        Some(tr(
            "上一次合并尚未提交。请先点击上方的「提交合并」或「中止」。",
        ))
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

/// git 自己的自动贮藏（`--autostash`）放回工作区时有冲突，就把它存进贮藏列表，说明是不翻译的
/// 「autostash」。`list` 是 [`fr_core::refs::stash_args`] 的输出，`before` 是操作前就有的贮藏的提交：
/// 多出了这样一份就是这次没放回去。
fn kept_autostash(list: &[u8], before: &[String]) -> bool {
    fr_core::refs::parse(&[], list)
        .stashes
        .iter()
        .any(|stash| stash.message == "autostash" && !before.contains(&stash.target))
}

/// 在 `git stash list` 的输出里找说明里带 `marker` 的那一份，返回 `stash@{n}`。
fn find_stash(list: &[u8], marker: &str) -> Option<String> {
    fr_core::refs::parse(&[], list)
        .stashes
        .into_iter()
        .find(|stash| stash.message.contains(marker))
        .map(|stash| stash.target)
}

/// 删掉贮藏列表里提交是 `commit` 的那一份，返回它是不是还留在列表里。git 只认 `stash@{N}`：
/// 删之前按提交找到它现在的序号，删完看 git 报的删掉的是不是它。正好别处同时存了贮藏、
/// 删成了别的那一份，就把那一份存回去（`stash store`），这一份留着。
async fn drop_stash_commit(channel: &Channel, path: &str, commit: &str) -> bool {
    let Ok(list) = channel.git(path, fr_core::refs::stash_args()).await else {
        return true;
    };
    let found = fr_core::refs::parse(&[], &list.stdout)
        .stashes
        .into_iter()
        .find(|stash| stash.target == commit);
    let Some(stash) = found else {
        // 已经不在列表里了
        return false;
    };
    let drop = vec!["stash".to_string(), "drop".into(), stash.selector];
    let out = match channel.git(path, drop).await {
        Ok(out) if out.success() => out,
        _ => return true,
    };
    // git 说「Dropped stash@{N} (<提交>)」，各种语言的说法里都有完整的提交号
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    match object_ids(&said).next() {
        Some(dropped) if dropped == commit => false,
        Some(other) => {
            let other = other.to_string();
            let subject = vec![
                "log".to_string(),
                "-1".into(),
                "--format=%s".into(),
                other.clone(),
            ];
            let message = match channel.git(path, subject).await {
                Ok(out) if out.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
                _ => String::new(),
            };
            let store = vec![
                "stash".to_string(),
                "store".into(),
                "-m".into(),
                message,
                other,
            ];
            channel.git(path, store).await.ok();
            true
        }
        // 认不出 git 说了什么：再列一次看它还在不在
        None => match channel.git(path, fr_core::refs::stash_args()).await {
            Ok(list) => fr_core::refs::parse(&[], &list.stdout)
                .stashes
                .iter()
                .any(|stash| stash.target == commit),
            Err(_) => true,
        },
    }
}

/// 一段文字里的完整提交号（40 位，SHA-256 的仓库是 64 位）。
fn object_ids(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .filter(|word| word.len() == 40 || word.len() == 64)
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
/// 给人看的命令行。前面的 `-c 名字=值` 不写：借 shell 别名的那几步，别名的整段脚本没法看。
fn command_line(args: &[String]) -> String {
    let mut rest = args;
    while let [flag, _, tail @ ..] = rest
        && flag == "-c"
    {
        rest = tail;
    }
    clip(format!("git {}", rest.join(" ")))
}

/// 给人看的命令行：带凭据的地址遮掉，太长就截断。
fn clip(text: String) -> String {
    let mut text = fr_core::git::redact_urls(&text).into_owned();
    if text.chars().count() > 80 {
        text = text.chars().take(80).collect::<String>() + "…";
    }
    text
}

/// 每个仓库正在做的写操作（键见 [`writing_key`]）。同一个仓库开在几个页面里时（子仓库页、「在新
/// 标签页中打开」、子仓库总览的批量操作），各页的 `op` 互相看不见，写到一起会撞 index.lock、拉取和
/// 合并交错，所以另记一份全局的。
static WRITING: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, SharedString>>,
> = std::sync::LazyLock::new(Default::default);

/// 仓库在 [`WRITING`] 里的键：哪台电脑和路径。Windows 本机不分大小写，两种斜杠都算。
pub(super) fn writing_key(server: Option<&str>, path: &str) -> String {
    match server {
        None if !cfg!(windows) => format!("\n{}", path.trim_end_matches('/')),
        None => {
            let path = path.replace('/', "\\");
            format!("\n{}", path.trim_end_matches('\\').to_lowercase())
        }
        Some(server) => format!("{server}\n{}", path.trim_end_matches('/')),
    }
}

/// 占着一个仓库的「正在写」，放掉就让出来。
pub(super) struct Writing(String);

impl Writing {
    /// 占上；这个仓库正在别处写时返回那个操作的名字。
    pub(super) fn take(key: String, label: SharedString) -> Result<Writing, SharedString> {
        let mut writing = WRITING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(running) = writing.get(&key) {
            return Err(running.clone());
        }
        writing.insert(key.clone(), label);
        Ok(Writing(key))
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        WRITING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
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
    use super::{
        Step, Writing, command_line, explain, find_stash, kept_autostash, last_progress_line,
        strip_progress, tail, writing_key,
    };

    #[test]
    fn 路径里有认不出的字符时不交给git() {
        let step = |args: &[&str]| Step::local(args.iter().map(|arg| arg.to_string()).collect());
        assert!(step(&["add", "--", "a\u{FFFD}.txt"]).lossy_path());
        assert!(!step(&["add", "--", "a.txt"]).lossy_path());
        // `--` 前面的（比如标签的说明）不算路径
        assert!(!step(&["tag", "-a", "-m", "\u{FFFD}", "v1"]).lossy_path());
    }

    #[test]
    fn 同一个仓库同时只能有一处在写() {
        // Windows 本机不分大小写、两种斜杠都算；Linux 本机分大小写
        let (path, same) = match cfg!(windows) {
            true => (r"C:\Work\写互斥测试\", "C:/WORK/写互斥测试"),
            false => ("/work/写互斥测试/", "/work/写互斥测试"),
        };
        let key = writing_key(None, path);
        assert_eq!(key, writing_key(None, same));
        #[cfg(not(windows))]
        assert_ne!(key, writing_key(None, "/WORK/写互斥测试"));
        let first = Writing::take(key.clone(), "拉取".into()).unwrap();
        let second = Writing::take(writing_key(None, same), "推送".into());
        let Err(running) = second else {
            panic!("同一个仓库应该被挡住");
        };
        assert_eq!(running.as_ref(), "拉取");
        drop(first);
        assert!(Writing::take(key, "推送".into()).is_ok());
        // 服务器上的分大小写；不同电脑上的不算一个
        assert_ne!(
            writing_key(Some("a"), "/srv/x"),
            writing_key(Some("a"), "/srv/X")
        );
        assert_ne!(
            writing_key(Some("a"), "/srv/x"),
            writing_key(Some("b"), "/srv/x")
        );
    }

    #[test]
    fn 按记号找自动贮藏的那一份() {
        let list = "stash@{0}\x1faaa\x1f1700000002\x1fOn main: 手工贮藏的\0\
                    stash@{1}\x1fbbb\x1f1700000001\x1fOn main: 切换分支之前自动贮藏的改动（kuai-123）\0";
        // 认的是提交，不是会挪动的 stash@{N}
        assert_eq!(
            find_stash(list.as_bytes(), "kuai-123").as_deref(),
            Some("bbb")
        );
        assert_eq!(find_stash(list.as_bytes(), "kuai-999"), None);
        assert_eq!(find_stash(b"", "kuai-123"), None);
    }

    #[test]
    fn 认出git放不回去时留下的自动贮藏() {
        let dir = std::env::temp_dir().join(format!("fr-autostash-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (origin, work) = (dir.join("origin"), dir.join("work"));
        std::fs::create_dir_all(&origin).unwrap();
        let git = |at: &std::path::Path, args: &[&str]| fr_core::git::run(at, args).unwrap();
        let ident = |at: &std::path::Path| {
            git(at, &["config", "user.name", "t"]);
            git(at, &["config", "user.email", "t@example.com"]);
            git(at, &["config", "core.autocrlf", "false"]);
        };
        git(&origin, &["init", "-q", "-b", "main"]);
        ident(&origin);
        std::fs::write(origin.join("a.txt"), "a\n").unwrap();
        git(&origin, &["add", "a.txt"]);
        git(&origin, &["commit", "-q", "-m", "c1"]);
        git(&dir, &["clone", "-q", "origin", "work"]);
        ident(&work);
        // 远程上改了这一行，本地也改了这一行（没提交）
        std::fs::write(origin.join("a.txt"), "remote\n").unwrap();
        git(&origin, &["commit", "-q", "-am", "c2"]);
        std::fs::write(work.join("a.txt"), "local\n").unwrap();
        let list_args = fr_core::refs::stash_args();
        let list_args: Vec<&str> = list_args.iter().map(String::as_str).collect();
        let targets = |list: &[u8]| -> Vec<String> {
            fr_core::refs::parse(&[], list)
                .stashes
                .into_iter()
                .map(|stash| stash.target)
                .collect()
        };
        let before = targets(&git(&work, &list_args));
        git(&work, &["fetch", "-q"]);
        // 合并本身成了，放回改动时有冲突：git 照样以 0 退出，改动留在贮藏列表里
        let finish = fr_core::pull::finish_args(fr_core::pull::Finish::Merge(None), true, "x");
        let finish: Vec<&str> = finish.iter().map(String::as_str).collect();
        git(&work, &finish);
        let after = git(&work, &list_args);
        assert!(kept_autostash(&after, &before));
        // 操作前就有的不算
        assert!(!kept_autostash(&after, &targets(&after)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 按提交删掉自动贮藏() {
        let dir = std::env::temp_dir().join(format!("fr-drop-stash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = fr_core::git::run(&dir, args).unwrap();
            String::from_utf8_lossy(&out).trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "user.email", "t@example.com"]);
        std::fs::write(dir.join("a.txt"), "1\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "first"]);
        std::fs::write(dir.join("a.txt"), "2\n").unwrap();
        git(&["stash", "push", "-q", "-m", "auto kuai-1"]);
        let ours = git(&["rev-parse", "stash@{0}"]);
        // 之后别处又存了一份：自动贮藏挪到了 stash@{1}
        std::fs::write(dir.join("a.txt"), "3\n").unwrap();
        git(&["stash", "push", "-q", "-m", "manual"]);
        let theirs = git(&["rev-parse", "stash@{0}"]);
        let repo = dir.to_string_lossy().to_string();
        let drop = |commit: &str| {
            futures::executor::block_on(super::drop_stash_commit(
                &fr_channel::Channel::Local,
                &repo,
                commit,
            ))
        };
        assert!(!drop(&ours));
        assert_eq!(git(&["rev-parse", "stash@{0}"]), theirs);
        assert_eq!(git(&["stash", "list"]).lines().count(), 1);
        // 已经不在列表里的：什么都不做
        assert!(!drop(&ours));
        assert_eq!(git(&["stash", "list"]).lines().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 认出文字里的提交号() {
        let id = "0123456789abcdef0123456789abcdef01234567";
        let said = format!("Dropped stash@{{1}} ({id})");
        assert_eq!(super::object_ids(&said).collect::<Vec<_>>(), [id]);
        assert_eq!(super::object_ids("丢弃了 stash@{0}（abc）").count(), 0);
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
        // 要用户名的框被取消了：不说「认证失败」
        let cancelled = "error: unable to read askpass response from '/home/u/.kuai/x/kuaid'\nfatal: could not read Username for 'http://127.0.0.1:8899': terminal prompts disabled";
        assert!(explain(cancelled).unwrap().contains("未输入登录信息"));
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
        let fetch = ["-c", "fetch.writeFetchHEAD=true", "fetch", "--progress"].map(String::from);
        assert_eq!(command_line(&fetch), "git fetch --progress");
    }
}
