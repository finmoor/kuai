//! 自动获取：隔一段时间在后台 `git fetch --all` 一次，工具栏上的「拉取」能及时显示落后了几个提交。
//!
//! 只对正在看的那个标签页做，没有别的操作在进行时才做；用户一动手做别的操作就把它停掉。
//! 要密码时不弹窗，这一次当作失败（命令带着 [`fr_core::askpass::NO_PROMPT`]，只管这一条）。结果记在
//! 活动记录里，不弹通知。

use std::time::{Duration, Instant};

use fr_channel::GitCancel;
use fr_core::i18n::tr;
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::Shared;
use gpui_kit::*;

use super::RepoView;

/// 隔多久看一次到没到时间
const TICK: Duration = Duration::from_secs(15);

pub(super) struct AutoFetch {
    /// 上次联网（手动的获取、拉取、推送，或者自动获取）是什么时候
    pub(super) last: Instant,
    /// 正在进行的自动获取
    running: Option<Running>,
    _timer: Task<()>,
}

/// 叫停自动获取以后等它的 git 真的退出：退出时完成（见 [`RepoView::stop_auto_fetch`]）。
pub(super) type Stopped = Shared<oneshot::Receiver<()>>;

/// 一次正在进行的自动获取。
struct Running {
    task: Task<()>,
    /// 在活动记录里的编号
    id: u64,
    cancel: GitCancel,
    /// git 退出了（不论怎么退出的）就完成
    finished: Stopped,
}

impl AutoFetch {
    pub(super) fn new(cx: &mut Context<RepoView>) -> Self {
        let timer = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(tick()).await;
                if this
                    .update(cx, |this, cx| this.auto_fetch_tick(cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            last: Instant::now(),
            running: None,
            _timer: timer,
        }
    }
}

/// 测试开关：FR_AUTO_FETCH_SECS=<秒> 把间隔换成这么多秒（设置里写的分钟数不管了）。
fn test_interval() -> Option<Duration> {
    let secs: u64 = std::env::var("FR_AUTO_FETCH_SECS").ok()?.parse().ok()?;
    Some(Duration::from_secs(secs.max(1)))
}

fn tick() -> Duration {
    match test_interval() {
        Some(interval) => interval.min(TICK),
        None => TICK,
    }
}

impl RepoView {
    fn auto_fetch_tick(&mut self, cx: &mut Context<Self>) {
        let minutes = self.app.read(cx).settings.auto_fetch_minutes;
        let interval = match test_interval() {
            Some(interval) => interval,
            None if minutes == 0 => return,
            None => Duration::from_secs(u64::from(minutes) * 60),
        };
        if !self.visible
            || self.op.is_some()
            || self.in_progress.is_some()
            || self.auto_fetch.running.is_some()
            || self.remote_names.is_empty()
            || self.auto_fetch.last.elapsed() < interval
        {
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let mut args: Vec<String> = fr_core::askpass::NO_PROMPT.map(String::from).to_vec();
        args.extend(["fetch", "--all", "--quiet"].map(String::from));
        let id = self
            .activity
            .begin(tr("自动获取").into(), format!("git {}", args.join(" ")));
        let (request, cancel) = channel.git_cancellable(&self.entry.path, args);
        let (exited, finished) = oneshot::channel::<()>();
        self.auto_fetch.last = Instant::now();
        let task = cx.spawn(async move |this, cx| {
            let result = request.await;
            drop(exited);
            this.update(cx, |this, cx| {
                // 已经叫停了：活动记录里记过「已取消」，也不用重读
                let current = this.auto_fetch.running.as_ref();
                if current.is_none_or(|running| running.id != id) {
                    return;
                }
                this.activity.end(id, &result, false);
                this.auto_fetch.running = None;
                match &result {
                    // 远程分支可能动了：重读引用，变了的话提交历史也跟着重读
                    Ok(out) if out.success() => this.refresh(cx),
                    Ok(out) => log::info!("自动获取失败：{}", out.stderr.trim()),
                    Err(err) => log::info!("自动获取失败：{err:#}"),
                }
                cx.notify();
            })
            .ok();
        });
        self.auto_fetch.running = Some(Running {
            task,
            id,
            cancel,
            finished: finished.shared(),
        });
        cx.notify();
    }

    /// 叫停正在进行的自动获取：杀掉它的 git。返回的 future 在那个 git 真的退出以后完成，接着要做的
    /// 写操作先等它：原来丢掉任务就当停了，那边的 git 可能还在跑，和用户的操作抢同一个引用的锁。
    pub(super) fn stop_auto_fetch(&mut self) -> Option<Stopped> {
        let running = self.auto_fetch.running.take()?;
        running.cancel.cancel();
        // 任务留着收尾（它看到自己已经不是正在进行的那一个，什么都不做）
        running.task.detach();
        self.activity
            .end(running.id, &Err(anyhow::anyhow!(tr("已取消"))), true);
        Some(running.finished)
    }

    /// 是不是正在自动获取（工具栏上「获取」按钮转圈）。
    pub(super) fn auto_fetching(&self) -> bool {
        self.auto_fetch.running.is_some()
    }
}
