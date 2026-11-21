//! 工作区页、服务器页上每个仓库的子仓库，和仓库、子仓库各自在哪个分支、有几个改动（子仓库跟着
//! 仓库一起列出来，设计见 `docs/design/server-workspace/` 第二版）。
//!
//! 和仓库里的子仓库总览（`repo::subs`）是同一种数据、同一套读法：子仓库用 `fs_repos` 找（服务器上
//! 由 kuaid 找），每个仓库、子仓库读一次状态和最近一次提交（两条 git 命令），同时最多 6 个。
//! 仓库自己的工作树建在它目录里面时也会被找出来，按工作树清单去掉（和子仓库总览一样）。
//!
//! 全局一份，按仓库记着。页面显示出来、换了要看的仓库时把它们交过来（[`RepoScan::want`]）：没读过的、
//! 读过有一会儿了的（[`STALE`]）排进队里，一个仓库一个仓库地读，后交过来的排在前面（正看着的先读）。
//! 没连着的服务器上的不读，也不为了读它去连（页面上写「未连接」）。

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use fr_channel::Channel;
use futures::StreamExt as _;
use gpui_kit::*;

use crate::app_state::{AppState, ConnStatus};
use crate::repo::{PARALLEL, Summary, summarize};
use crate::settings::RepoEntry;

/// 读过的过这么久再要就重读
const STALE: Duration = Duration::from_secs(20);

/// 一个仓库读到的。
#[derive(Clone, Default)]
pub struct Scanned {
    /// 子仓库：相对仓库根的路径（正斜杠），按名字排。还没找过是 None
    pub subs: Option<Vec<String>>,
    /// 子仓库太多、目录太深，没找全（kuaid 找到上限就停）：只有前面这些
    pub truncated: bool,
    /// 仓库自己（键是空串）和各个子仓库的分支、改动数
    pub states: HashMap<String, Summary>,
    /// 最近一次开始读的时候
    at: Option<Instant>,
}

impl Scanned {
    /// 仓库自己的情况；还没读到是 None。
    pub fn own(&self) -> Option<&Summary> {
        self.states.get("")
    }
}

pub struct RepoScan {
    app: Entity<AppState>,
    repos: HashMap<RepoEntry, Scanned>,
    /// 等着读的仓库
    queue: VecDeque<RepoEntry>,
    /// 正在读的那一个
    busy: Option<(RepoEntry, Task<()>)>,
}

/// 子仓库在磁盘上的路径。本机的 Windows 路径用反斜杠，和单独打开它时的写法一样（终端、记住的东西
/// 按路径认，见 `RepoView::sub_path`）。
pub fn sub_path(root: &RepoEntry, relative: &str) -> String {
    let windows_local = root.server.is_none() && root.path.contains('\\');
    match windows_local {
        true => format!(
            "{}\\{}",
            root.path.trim_end_matches('\\'),
            relative.replace('/', "\\")
        ),
        false => format!("{}/{relative}", root.path.trim_end_matches('/')),
    }
}

/// 子仓库的标签页、终端按这个认。
pub fn sub_entry(root: &RepoEntry, relative: &str) -> RepoEntry {
    RepoEntry {
        server: root.server.clone(),
        path: sub_path(root, relative),
    }
}

/// 子仓库显示的名字：相对路径的最后一段。
pub fn sub_name(relative: &str) -> &str {
    relative.rsplit('/').next().unwrap_or(relative)
}

/// 读这个仓库用的通道：本机的直接给；服务器上的要已经连着，没连着是 None。
fn channel(app: &AppState, entry: &RepoEntry) -> Option<Channel> {
    match entry.server.as_deref() {
        None => Some(Channel::Local),
        Some(id) => match app.status(id) {
            ConnStatus::Ready(conn) => Some(Channel::Remote(conn)),
            _ => None,
        },
    }
}

impl RepoScan {
    pub fn new(app: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // 服务器连上了：页面会再交一遍要看的仓库，这里只要重画
        cx.observe(&app, |_, _, cx| cx.notify()).detach();
        Self {
            app,
            repos: HashMap::new(),
            queue: VecDeque::new(),
            busy: None,
        }
    }

    pub fn get(&self, entry: &RepoEntry) -> Option<&Scanned> {
        self.repos.get(entry)
    }

    /// 这个仓库还在读（排着队或者正在读）。
    pub fn pending(&self, entry: &RepoEntry) -> bool {
        self.queue.contains(entry) || self.busy.as_ref().is_some_and(|(at, _)| at == entry)
    }

    /// 页面要看这些仓库：没读过、读过有一会儿了的排到队伍最前面（按给的先后）。正在读的、
    /// 没连着的服务器上的不排。
    pub fn want(&mut self, entries: &[RepoEntry], cx: &mut Context<Self>) {
        let now = Instant::now();
        let app = self.app.read(cx);
        let mut ahead: Vec<RepoEntry> = Vec::new();
        for entry in entries {
            let fresh = self
                .repos
                .get(entry)
                .and_then(|scanned| scanned.at)
                .is_some_and(|at| now.saturating_duration_since(at) < STALE);
            let running = self.busy.as_ref().is_some_and(|(at, _)| at == entry);
            if fresh || running || ahead.contains(entry) || channel(app, entry).is_none() {
                continue;
            }
            ahead.push(entry.clone());
        }
        if ahead.is_empty() {
            return;
        }
        self.queue.retain(|entry| !ahead.contains(entry));
        for entry in ahead.into_iter().rev() {
            self.queue.push_front(entry);
        }
        self.pump(cx);
    }

    /// 没在读就从队里拿一个读。
    fn pump(&mut self, cx: &mut Context<Self>) {
        while self.busy.is_none() {
            let Some(entry) = self.queue.pop_front() else {
                return;
            };
            // 排队的时候连接断了：跳过，下次页面再要时再读
            let Some(channel) = channel(self.app.read(cx), &entry) else {
                continue;
            };
            self.repos.entry(entry.clone()).or_default().at = Some(Instant::now());
            let task = cx.spawn({
                let entry = entry.clone();
                async move |this, cx| {
                    let found = find_subs(&channel, &entry).await;
                    let alive = this.update(cx, |this, cx| {
                        let scanned = this.repos.entry(entry.clone()).or_default();
                        // 这次没找成（目录一时读不了）：留着上次找到的，不当成没有
                        let Some((subs, truncated)) = &found else {
                            return scanned.subs.clone().unwrap_or_default();
                        };
                        scanned
                            .states
                            .retain(|key, _| key.is_empty() || subs.contains(key));
                        scanned.subs = Some(subs.clone());
                        scanned.truncated = *truncated;
                        cx.notify();
                        subs.clone()
                    });
                    let Ok(subs) = alive else {
                        return;
                    };
                    // 仓库自己和每个子仓库
                    let jobs = std::iter::once(String::new()).chain(subs).map(|relative| {
                        let channel = channel.clone();
                        let path = match relative.is_empty() {
                            true => entry.path.clone(),
                            false => sub_path(&entry, &relative),
                        };
                        async move { (relative, summarize(channel, path).await) }
                    });
                    let mut results = futures::stream::iter(jobs).buffer_unordered(PARALLEL);
                    while let Some((relative, summary)) = results.next().await {
                        let alive = this.update(cx, |this, cx| {
                            let scanned = this.repos.entry(entry.clone()).or_default();
                            scanned.states.insert(relative, summary);
                            cx.notify();
                        });
                        if alive.is_err() {
                            return;
                        }
                    }
                    this.update(cx, |this, cx| {
                        this.busy = None;
                        this.pump(cx);
                        cx.notify();
                    })
                    .ok();
                }
            });
            self.busy = Some((entry, task));
        }
    }
}

/// 找仓库目录里的子仓库，去掉建在里面的工作树；第二项是没找全（到了上限）。找不了（旧的 kuaid
/// 不认识这个请求、目录一时读不了）是 None。
async fn find_subs(channel: &Channel, entry: &RepoEntry) -> Option<(Vec<String>, bool)> {
    let found = channel.fs_repos(&entry.path);
    let nul = fr_core::worktree::has_nul(channel.git_version());
    let trees = channel.git(&entry.path, fr_core::worktree::list_args(nul));
    let (found, trees) = futures::join!(found, trees);
    let trees: Vec<String> = trees
        .and_then(|out| out.ok())
        .map(|out| {
            fr_core::worktree::parse(&out, nul)
                .into_iter()
                .map(|tree| tree.path)
                .collect()
        })
        .unwrap_or_default();
    match found {
        Ok(found) => {
            let subs = found
                .repos
                .into_iter()
                .filter(|relative| {
                    let full = sub_path(entry, relative);
                    !trees
                        .iter()
                        .any(|tree| fr_core::worktree::same_path(tree, &full))
                })
                .collect();
            Some((subs, found.truncated))
        }
        Err(err) => {
            log::info!("{}：找子仓库失败：{err:#}", entry.path);
            None
        }
    }
}
