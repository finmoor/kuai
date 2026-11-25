//! 上次获取：工具栏上分支名下面一行「2 分钟前获取」，前面一个状态点，点它立即获取。
//!
//! - 绿：获取过不久；黄：超过 [`stale_after`] 没获取，待拉取、待推送的数字可能过期，或者从没
//!   获取过；红：上次获取失败了（自动获取要密码、断网……），之后还没成功过。
//! - 时间先取 `FETCH_HEAD` 的修改时间（在终端里获取的也算，刷新时再看），这个标签页里获取、拉取、
//!   自动获取成功了就记成那一刻。

use std::time::{Duration, Instant};

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::RepoView;
use crate::ui;

#[derive(Default)]
pub(super) struct FetchState {
    /// 上次成功获取（Unix 毫秒）
    at: Option<i64>,
    /// 读过 `FETCH_HEAD` 了：读到之前不显示那一行，免得刚打开时闪一下「尚未获取」
    known: bool,
    /// 上次获取失败了，之后没有成功过
    failed: Option<Failure>,
    /// `FETCH_HEAD` 在哪：（目录，文件名）。工作树、子模块的在仓库目录外面
    head: Option<(String, String)>,
    /// 正在读 `FETCH_HEAD`
    reading: Option<Task<()>>,
}

struct Failure {
    at: i64,
    /// 原因：认得出的是中文说明，认不出是 git 说的第一句
    reason: String,
    /// 自动获取失败的（不是用户点的）
    auto: bool,
}

impl FetchState {
    /// 看到 `FETCH_HEAD` 的修改时间：比记着的新就是在别处获取过。
    fn saw(&mut self, mtime: i64) {
        if self.at.is_none_or(|at| mtime > at) {
            self.at = Some(mtime);
        }
        if self.failed.as_ref().is_some_and(|failed| mtime > failed.at) {
            self.failed = None;
        }
    }
}

/// 现在（Unix 毫秒）。
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 多久没获取算「可能过期」：一小时；自动获取的间隔长的，三个间隔。
fn stale_after(auto_fetch_minutes: u32) -> i64 {
    let hour = 60 * 60 * 1000;
    hour.max(i64::from(auto_fetch_minutes) * 3 * 60 * 1000)
}

/// 「刚刚」「2 分钟前」「3 小时前」「2 天前」。
fn ago(ms: i64) -> String {
    let minutes = ms.max(0) / 60_000;
    match minutes {
        0 => tr("刚刚").into(),
        1..60 => tf!("{n} 分钟前", n = minutes),
        60..1440 => tf!("{n} 小时前", n = minutes / 60),
        _ => tf!("{n} 天前", n = minutes / 1440),
    }
}

/// 悬停提示里的时刻：今天的只写时分，今年的写月日，再早的写年。
fn when(ms: i64) -> String {
    use chrono::{Datelike as _, TimeZone as _};
    let Some(at) = chrono::Local.timestamp_millis_opt(ms).single() else {
        return String::new();
    };
    let now = chrono::Local::now();
    if at.date_naive() == now.date_naive() {
        at.format("%H:%M").to_string()
    } else if at.year() == now.year() {
        at.format("%m-%d %H:%M").to_string()
    } else {
        at.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// 获取失败的原因：认得出的换成中文说明，认不出就是 git 报的第一句错误。
fn reason(output: &str) -> String {
    if let Some(text) = super::ops::explain(output) {
        return text.to_string();
    }
    let lines = || {
        output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
    };
    lines()
        .find(|line| line.starts_with("fatal:") || line.starts_with("error:"))
        .or_else(|| lines().next())
        .unwrap_or_default()
        .to_string()
}

/// `git rev-parse --git-path FETCH_HEAD` 的输出换成 `fs_stat` 要的（目录，文件名）：相对的（主工作树
/// 里是 `.git/FETCH_HEAD`）以仓库目录为准；绝对的（工作树、子模块）拆成所在目录和文件名。
fn fetch_head_at(repo: &str, out: &str) -> Option<(String, String)> {
    let path = out.lines().next()?.trim();
    if path.is_empty() {
        return None;
    }
    let absolute =
        path.starts_with('/') || path.starts_with('\\') || path.as_bytes().get(1) == Some(&b':');
    if !absolute {
        // `fs_stat` 只认一段段普通的名字
        if path
            .split(['/', '\\'])
            .any(|part| part == ".." || part == ".")
        {
            return None;
        }
        return Some((repo.to_string(), path.to_string()));
    }
    let cut = path.rfind(['/', '\\'])?;
    let (dir, name) = (&path[..cut], &path[cut + 1..]);
    if name.is_empty() {
        return None;
    }
    // 在根目录下（`/FETCH_HEAD`、`C:/FETCH_HEAD`）
    let dir = match dir {
        "" => "/".to_string(),
        dir if dir.ends_with(':') => format!("{dir}/"),
        dir => dir.to_string(),
    };
    Some((dir, name.to_string()))
}

impl RepoView {
    /// 刷新时调：看看 `FETCH_HEAD` 是不是变新了（在终端里获取过）。第一次读到时自动获取从那时算起。
    pub(super) fn read_fetch_head(&mut self, cx: &mut Context<Self>) {
        if self.fetch.reading.is_some() || self.remote_names.is_empty() {
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        let head = self.fetch.head.clone();
        self.fetch.reading = Some(cx.spawn(async move |this, cx| {
            let head = match head {
                Some(head) => Some(head),
                None => {
                    let args = ["rev-parse", "--git-path", "FETCH_HEAD"].map(String::from);
                    channel
                        .git(&repo, args.to_vec())
                        .await
                        .ok()
                        .filter(|out| out.success())
                        .and_then(|out| fetch_head_at(&repo, &String::from_utf8_lossy(&out.stdout)))
                }
            };
            let stat = match &head {
                Some((dir, name)) => channel.fs_stat(dir, name).await.ok(),
                None => None,
            };
            this.update(cx, |this, cx| {
                this.fetch.reading = None;
                let first = !this.fetch.known;
                this.fetch.known = true;
                this.fetch.head = head;
                if let Some(stat) = stat.filter(|stat| stat.exists && stat.mtime > 0) {
                    this.fetch.saw(stat.mtime as i64);
                }
                if first {
                    this.sync_auto_fetch_clock();
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// 刚知道上次获取是什么时候：自动获取从那时算起。原来从打开标签页算，上次获取在三小时前也要
    /// 再等十分钟，这期间数字一直是旧的；从没获取过的，下一次检查就获取。
    fn sync_auto_fetch_clock(&mut self) {
        // 往前最多拨两小时（比最长的间隔长就够了）；刚开机不久拨不了那么多时，拨十分钟
        let ago = match self.fetch.at {
            Some(at) => Duration::from_millis((now_ms() - at).max(0) as u64),
            None => Duration::MAX,
        }
        .min(Duration::from_secs(2 * 60 * 60));
        let now = Instant::now();
        let when = now
            .checked_sub(ago)
            .or_else(|| now.checked_sub(Duration::from_secs(600)));
        if let Some(when) = when
            && when < self.auto_fetch.last
        {
            self.auto_fetch.last = when;
        }
    }

    /// 获取成功了（手动的获取、拉取的第一步、自动获取）。
    pub(super) fn fetch_succeeded(&mut self) {
        self.fetch.at = Some(now_ms());
        self.fetch.failed = None;
        self.fetch.known = true;
    }

    /// 获取失败了（取消的不算）。`output` 是 git 的输出。
    pub(super) fn fetch_failed(&mut self, output: &str, auto: bool) {
        self.fetch.failed = Some(Failure {
            at: now_ms(),
            reason: reason(output),
            auto,
        });
        self.fetch.known = true;
    }

    /// 分支名下面那一行；没有远程、还没读到上次获取的时间时没有。
    pub(super) fn render_fetch_line(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.remote_names.is_empty() || !self.fetch.known {
            return None;
        }
        let minutes = self.app.read(cx).settings.auto_fetch_minutes;
        let now = now_ms();
        let compact = super::compact_toolbar();
        let theme = cx.theme();
        let (muted, fg) = (theme.muted_foreground, theme.foreground);
        let fetching = self.op.as_deref() == Some(tr("获取")) || self.auto_fetching();
        let (dot, text, short, title, body): (Hsla, String, String, String, String) =
            match (&self.fetch.failed, self.fetch.at) {
                (Some(failed), _) => (
                    theme.danger,
                    match failed.auto {
                        true => tr("自动获取失败 · 点击重试").into(),
                        false => tr("获取失败 · 点击重试").into(),
                    },
                    tr("获取失败").into(),
                    match failed.auto {
                        true => tf!("{when} 自动获取失败", when = when(failed.at)),
                        false => tf!("{when} 获取失败", when = when(failed.at)),
                    },
                    tf!("{reason} 点击重试。", reason = failed.reason.as_str()),
                ),
                (None, Some(at)) if now - at < stale_after(minutes) => (
                    theme.success,
                    tf!("{ago}获取", ago = ago(now - at)),
                    ago(now - at),
                    tf!("上次获取：{when}", when = when(at)),
                    tr("点击立即获取（git fetch --all）。").into(),
                ),
                (None, Some(at)) => (
                    theme.warning,
                    tf!("{ago}获取 · 数字可能过期", ago = ago(now - at)),
                    ago(now - at),
                    tf!("上次获取：{when}", when = when(at)),
                    tr("已有一段时间未获取，待拉取、待推送的数字可能过期。点击立即获取。").into(),
                ),
                (None, None) => (
                    theme.warning,
                    tr("尚未获取 · 点击获取").into(),
                    tr("未获取").into(),
                    tr("尚未获取过远程").into(),
                    tr("待拉取、待推送的数字以上次获取为准。点击立即获取。").into(),
                ),
            };
        let text = match (fetching, compact) {
            (true, _) => tr("正在获取…").to_string(),
            (false, true) => short,
            (false, false) => text,
        };
        let (title, body): (SharedString, SharedString) = (title.into(), body.into());
        Some(
            h_flex()
                .id("fetch-age")
                .h(px(14.))
                .min_w_0()
                .px(px(2.))
                .gap(px(5.))
                .rounded(px(3.))
                .text_size(px(11.))
                .line_height(px(14.))
                .text_color(muted)
                .cursor_pointer()
                .hover(|s| s.text_color(fg))
                .child(div().flex_none().size(px(6.)).rounded_full().bg(dot))
                .child(div().min_w_0().truncate().child(text))
                .tooltip(move |window, cx| {
                    ui::two_line_tip(title.clone(), body.clone(), window, cx)
                })
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.op.is_none() && !this.auto_fetching() {
                        this.fetch(window, cx);
                    }
                }))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Failure, FetchState, ago, fetch_head_at, reason, stale_after};

    #[test]
    fn 找到_fetch_head() {
        let at = |out: &str| fetch_head_at("/home/u/app", out);
        assert_eq!(
            at(".git/FETCH_HEAD\n"),
            Some(("/home/u/app".into(), ".git/FETCH_HEAD".into()))
        );
        // 工作树：在主仓库的 .git/worktrees 下面
        assert_eq!(
            at("/home/u/app/.git/worktrees/w/FETCH_HEAD\n"),
            Some(("/home/u/app/.git/worktrees/w".into(), "FETCH_HEAD".into()))
        );
        assert_eq!(
            at("C:/src/app/.git/modules/sub/FETCH_HEAD"),
            Some(("C:/src/app/.git/modules/sub".into(), "FETCH_HEAD".into()))
        );
        assert_eq!(
            at("C:/FETCH_HEAD"),
            Some(("C:/".into(), "FETCH_HEAD".into()))
        );
        assert_eq!(at("/FETCH_HEAD"), Some(("/".into(), "FETCH_HEAD".into())));
        assert_eq!(at("../x/FETCH_HEAD"), None);
        assert_eq!(at(""), None);
    }

    #[test]
    fn 多久算过期() {
        let hour = 60 * 60 * 1000;
        assert_eq!(stale_after(0), hour);
        assert_eq!(stale_after(10), hour);
        assert_eq!(stale_after(60), 3 * hour);
    }

    #[test]
    fn 几分钟前() {
        assert_eq!(ago(-5), "刚刚");
        assert_eq!(ago(59_000), "刚刚");
        assert_eq!(ago(2 * 60_000), "2 分钟前");
        assert_eq!(ago(3 * 60 * 60_000 + 5), "3 小时前");
        assert_eq!(ago(50 * 60 * 60_000), "2 天前");
    }

    #[test]
    fn 失败的原因() {
        assert_eq!(
            reason("remote: x\nfatal: unable to access 'https://h/': Could not resolve host: h"),
            "无法连接远程仓库。请检查网络、代理和远程地址。"
        );
        assert_eq!(
            reason("warning: a\nerror: something odd\n"),
            "error: something odd"
        );
        assert_eq!(reason("odd\n"), "odd");
    }

    #[test]
    fn 在别处获取过() {
        let mut state = FetchState {
            at: Some(100),
            failed: Some(Failure {
                at: 200,
                reason: String::new(),
                auto: true,
            }),
            ..Default::default()
        };
        // 比失败早的不算成功过
        state.saw(150);
        assert_eq!(state.at, Some(150));
        assert!(state.failed.is_some());
        state.saw(300);
        assert_eq!(state.at, Some(300));
        assert!(state.failed.is_none());
        // 旧的不往回拨
        state.saw(50);
        assert_eq!(state.at, Some(300));
    }
}
