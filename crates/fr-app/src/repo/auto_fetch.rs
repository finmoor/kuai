//! 自动获取：隔一段时间在后台 `git fetch --all` 一次，工具栏上的「拉取」能及时显示落后了几个提交。
//!
//! 只对正在看的那个标签页做，没有别的操作在进行时才做；用户一动手做别的操作就把它停掉。
//! 要密码时不弹窗，这一次当作失败（见 [`crate::prompt::quiet`]）。结果记在活动记录里，不弹通知。

use std::time::{Duration, Instant};

use fr_core::i18n::tr;
use gpui_kit::*;

use super::RepoView;

/// 隔多久看一次到没到时间
const TICK: Duration = Duration::from_secs(15);

pub(super) struct AutoFetch {
    /// 上次联网（手动的获取、拉取、推送，或者自动获取）是什么时候
    pub(super) last: Instant,
    /// 正在进行的自动获取：任务，和它在活动记录里的编号
    running: Option<(Task<()>, u64)>,
    _timer: Task<()>,
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
        let args: Vec<String> = ["fetch", "--all", "--quiet"].map(String::from).to_vec();
        let id = self.activity.begin(tr("自动获取").into(), &args);
        let request = channel.git(&self.entry.path, args);
        let quiet = crate::prompt::quiet();
        self.auto_fetch.last = Instant::now();
        let task = cx.spawn(async move |this, cx| {
            let result = request.await;
            drop(quiet);
            this.update(cx, |this, cx| {
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
        self.auto_fetch.running = Some((task, id));
        cx.notify();
    }

    /// 停掉正在进行的自动获取（丢掉任务，对应的 git 会被杀掉）。
    pub(super) fn stop_auto_fetch(&mut self) {
        if let Some((task, id)) = self.auto_fetch.running.take() {
            drop(task);
            self.activity
                .end(id, &Err(anyhow::anyhow!(tr("已取消"))), true);
        }
    }

    /// 是不是正在自动获取（工具栏上「获取」按钮转圈）。
    pub(super) fn auto_fetching(&self) -> bool {
        self.auto_fetch.running.is_some()
    }
}
