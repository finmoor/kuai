//! 检查更新、点一下升级。
//!
//! 版本发在 GitHub Releases 上（仓库 [`REPO`]）。启动后过一会儿、之后每 6 小时问一次
//! `releases/latest`（设置里能关，「设置 → 关于」里能手动问），比自己新就在标题栏上提示
//! （没问成的过一会儿再试，见 [`wait_before_next`]；窗口从托盘叫回来、再次启动时也问，见
//! [`Updater::window_shown`]）
//! （`update_view.rs`）。点「升级」才下载，下到 `%LOCALAPPDATA%\kuai\update\`，和 GitHub 给附件记的
//! SHA-256（API 里的 `digest`）对一遍，放进暂存目录。下哪个、怎么换看这份 Kuai 是怎么装的（[`Kind`]）：
//! - 安装版（`tools/installer/kuai.iss` 装的）：下新的安装程序。点「重启完成升级」时在后台跑它
//!   （不弹向导），它等这个进程退出后装上、打开新的 Kuai。这样「设置 → 应用」里的版本号也跟着变。
//! - 解压版：下 zip 解压。点「重启完成升级」时别的文件直接覆盖，kuai.exe 用 self-replace 换
//!   （运行中的 exe 只能改名、不能删），再拉起新的 Kuai、自己退出。
//!
//! 下好了没点重启就退出的，下次启动时先换上再开（[`apply_staged_at_startup`]）。
//!
//! 服务器上的 kuaid 不用管：新版客户端连上时，常驻进程原地换成新程序（见 docs/架构.md 第 4 节）。
//! 只有打好的包（exe 叫 kuai.exe）会换程序；开发时的 fr-app.exe 只检查、不换。
//!
//! 测试用的环境变量：`FR_UPDATE_API` 换掉 `https://api.github.com`（指到本机的假服务），
//! `FR_UPDATE_DIR` 换掉下载和暂存的目录。

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use fr_core::i18n::tr;
use fr_core::tf;
use futures::channel::oneshot;
use gpui_kit::prelude::*;
use gpui_kit::*;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::app_state::AppState;

/// 发版的 GitHub 仓库。
pub const REPO: &str = "finmoor/kuai";
/// 启动后多久问第一次：别和打开仓库、连服务器抢
const FIRST_CHECK: Duration = Duration::from_secs(3);
/// 之后多久问一次
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// 自动问没问成（开机自启时网络还没通、GitHub 限流）过多久再试：一次比一次久，之后每小时一次
const RETRY_AFTER: [Duration; 4] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
    Duration::from_secs(60 * 60),
];
/// 多久看一眼该不该问了
const TICK: Duration = Duration::from_secs(30);
/// 窗口被叫回来（再次启动、点托盘）时，离上次问成超过这么久就再问一次。
/// 常驻托盘的 Kuai 很少真的重启，对用户来说这就是「启动」
const RECHECK_ON_SHOW: Duration = Duration::from_secs(30 * 60);
/// 发布说明里最多取几条要点
const MAX_NOTES: usize = 6;
/// 暂存目录里的这个文件表示解压完了
const READY_MARK: &str = ".kuai-update-ready";
/// 换上新版本时写下原来的版本号，新的 Kuai 启动时读到就提示「已升到 …」。
/// 读到的和自己的版本一样，说明没换上（安装程序出错了）
const DONE_MARK: &str = "just-updated";
/// 安装版：下好的安装程序在暂存目录里叫这个
const SETUP_EXE: &str = "kuai-setup.exe";
/// 安装版：后台跑安装程序时它写的记录
const SETUP_LOG: &str = "setup.log";
/// 启动时换上暂存的新版本没成：记下哪个版本、为什么，界面起来以后告诉人（[`Updater::new`]）
const STARTUP_FAILED: &str = "startup-failed";
/// 每次下载自己的工作目录的前缀（见 [`fetch_and_stage`]）
const WORK_PREFIX: &str = "download-";

/// 每次下载的编号：取消以后马上重下，新的一次用新的工作目录，旧线程还在写也碰不到它。
static NEXT_DOWNLOAD: AtomicU64 = AtomicU64::new(0);
/// 「放进暂存目录」和「取消」互斥：取消了的那次不会再放进去（下次启动会把它换上）。
static PUBLISH: Mutex<()> = Mutex::new(());

/// 正在运行的版本。
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo.toml 里的版本号")
}

/// 源代码的地址。
pub fn repo_url() -> String {
    format!("https://github.com/{REPO}")
}

/// GitHub 上的一个版本。
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: Version,
    /// 发布日期 YYYY-MM-DD
    pub date: Option<String>,
    /// 发布说明里的要点
    pub notes: Vec<String>,
    pub asset: Asset,
}

impl Release {
    /// 发布页：完整的更新说明，也是手动下载的地方。按版本号自己拼，不用 API 给的网址。
    pub fn page(&self) -> String {
        format!("{}/releases/tag/v{}", repo_url(), self.version)
    }
}

/// 发布包。
#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// GitHub 给附件算的 SHA-256（小写十六进制）；2025 年 6 月以前发的附件没有
    pub sha256: Option<String>,
}

/// 这份 Kuai 是怎么装的，决定升级时下哪个文件、怎么换。
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// 用安装程序装的（文件夹里有 Inno Setup 的卸载程序 `unins000.exe`）
    Installed,
    /// 解压版
    Portable,
}

fn kind() -> Kind {
    let installed = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("unins000.exe").is_file()))
        .unwrap_or(false);
    if installed {
        Kind::Installed
    } else {
        Kind::Portable
    }
}

/// 升级走到哪一步。
pub enum Stage {
    /// 没问过，或者问过、已是最新
    Idle,
    Available(Release),
    Downloading {
        release: Release,
        got: Arc<AtomicU64>,
        cancel: Arc<AtomicBool>,
    },
    /// 下好、对过校验值、放进了暂存目录，等重启
    Ready(Release),
    Failed {
        release: Option<Release>,
        problem: Problem,
    },
}

/// 没成的原因。
#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
    /// 问 GitHub 没问成（只有手动检查时才显示）
    Check(String),
    /// 没下完
    Download(String),
    /// 下下来的和 GitHub 上记的校验值对不上
    Digest,
    /// 程序所在的文件夹不能写
    NotWritable(PathBuf),
    /// 开发时的程序（不是打好的包），不换
    DevBuild,
    /// 解压或者换的时候出错
    Install(String),
}

pub enum UpdateEvent {
    /// 点了「重启完成升级」：工作区窗口先问一下本机终端，再调 [`Updater::install_and_relaunch`]
    Restart,
}

pub struct Updater {
    app: Entity<AppState>,
    pub stage: Stage,
    /// 正在问 GitHub
    pub checking: bool,
    /// 上次问成的时候
    pub checked_at: Option<Instant>,
    /// 上次开始问的时候（成不成都记）
    tried_at: Option<Instant>,
    /// 连着几次没问成
    failures: usize,
    /// 这次启动是从哪个版本升上来的
    pub updated_from: Option<String>,
    _work: Option<Task<()>>,
    /// 「重试」先重新问一遍 GitHub；单放一处，问完开始下载时要换掉 `_work`
    _refetch: Option<Task<()>>,
    _schedule: Task<()>,
}

impl EventEmitter<UpdateEvent> for Updater {}

impl Updater {
    pub fn new(app: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let schedule = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FIRST_CHECK).await;
            loop {
                let alive = this.update(cx, |this, cx| {
                    let due = this
                        .tried_at
                        .is_none_or(|at| at.elapsed() >= wait_before_next(this.failures));
                    if due && this.app.read(cx).settings.check_updates {
                        this.check(false, cx);
                    }
                });
                if alive.is_err() {
                    return;
                }
                cx.background_executor().timer(TICK).await;
            }
        });
        let mut stage = Stage::Idle;
        let updated_from = match take_done_mark() {
            // 上次在后台跑的安装程序没装上：不再自动重试（不然每次启动都先跑一遍它），告诉人
            Some(from) if from == env!("CARGO_PKG_VERSION") => {
                discard_staged();
                let log = update_dir().join(SETUP_LOG);
                let problem =
                    Problem::Install(tf!("安装失败，日志位于 {path}", path = log.display()));
                stage = Stage::Failed {
                    release: None,
                    problem,
                };
                None
            }
            other => other,
        };
        if updated_from.is_some() {
            clean_old_downloads();
        }
        // 启动时换上暂存的新版本没成：那一份已经删了，不再自动重试；告诉人，重新下载就是重试
        if let Some((version, err)) = take_startup_failure() {
            let text = tf!("启动时安装 {version} 失败：{err}", version, err);
            stage = Stage::Failed {
                release: None,
                problem: Problem::Install(text),
            };
        }
        Self {
            app,
            stage,
            checking: false,
            checked_at: None,
            tried_at: None,
            failures: 0,
            updated_from,
            _work: None,
            _refetch: None,
            _schedule: schedule,
        }
    }

    /// 标题栏上要不要显示小牌：有新版本（没跳过）、正在下、下好了、下载或换的时候出了错。
    pub fn needs_attention(&self, cx: &App) -> bool {
        match &self.stage {
            Stage::Idle
            | Stage::Failed {
                problem: Problem::Check(_),
                ..
            } => false,
            Stage::Available(release) => !self.skipped(release, cx),
            _ => true,
        }
    }

    /// 这一版点过「跳过」。
    pub fn skipped(&self, release: &Release, cx: &App) -> bool {
        let skipped = &self.app.read(cx).settings.skipped_version;
        skipped.as_deref() == Some(release.version.to_string().as_str())
    }

    /// 窗口被叫回来了（再次启动 Kuai、点托盘图标）：离上次问成有一阵了就再问一次。
    pub fn window_shown(&mut self, cx: &mut Context<Self>) {
        let stale = self
            .checked_at
            .is_none_or(|at| at.elapsed() >= RECHECK_ON_SHOW);
        // 刚问过没成的不在这里追着问，照 `wait_before_next` 的间隔来
        let just_tried = self
            .tried_at
            .is_some_and(|at| at.elapsed() < RETRY_AFTER[0]);
        if stale && !just_tried && self.app.read(cx).settings.check_updates {
            self.check(false, cx);
        }
    }

    /// 问 GitHub 有没有新版本。`manual`：在关于页点的，问不成要显示出来。
    pub fn check(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.checking || matches!(self.stage, Stage::Downloading { .. } | Stage::Ready(_)) {
            return;
        }
        self.checking = true;
        self.tried_at = Some(Instant::now());
        cx.notify();
        let task = cx
            .background_executor()
            .spawn(async move { fetch_latest() });
        self._work = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.checking = false;
                match result {
                    Ok(release) => {
                        this.checked_at = Some(Instant::now());
                        this.failures = 0;
                        let newer = release.filter(|r| r.version > current_version());
                        match (&mut this.stage, newer) {
                            // 自动检查别盖掉上次没装上的原因，把版本补上，「重试」「去下载页」用得上
                            (
                                Stage::Failed {
                                    release: slot,
                                    problem: Problem::Install(_),
                                },
                                Some(release),
                            ) if !manual => *slot = Some(release),
                            (stage, Some(release)) => *stage = Stage::Available(release),
                            (stage, None) => *stage = Stage::Idle,
                        }
                    }
                    Err(err) => {
                        log::warn!("检查更新没成：{err:#}");
                        this.failures += 1;
                        if manual {
                            let problem = Problem::Check(format!("{err:#}"));
                            this.stage = Stage::Failed {
                                release: None,
                                problem,
                            };
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// 「跳过这一版」：标题栏上不再提示它，有更新的版本再提示；关于页里照样能升级。
    pub fn skip(&mut self, cx: &mut Context<Self>) {
        if let Stage::Available(release) = &self.stage {
            let version = release.version.to_string();
            self.app.update(cx, |app, cx| {
                app.update_settings(cx, |s| s.skipped_version = Some(version))
            });
            cx.notify();
        }
    }

    /// 下载或换的时候出了错，点「重试」：先重新问一遍 GitHub 再下载。发布后附件可能重新传过，
    /// 拿着第一次记下的校验值和地址重下会一直对不上；问不成就照原来记下的再下一次。
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        if self.checking || !matches!(self.stage, Stage::Failed { .. }) {
            return;
        }
        self.checking = true;
        cx.notify();
        let task = cx
            .background_executor()
            .spawn(async move { fetch_latest() });
        self._refetch = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.checking = false;
                match result {
                    Ok(Some(release)) if release.version > current_version() => {
                        this.checked_at = Some(Instant::now());
                        this.stage = Stage::Available(release);
                    }
                    Ok(_) => {
                        // 新版本撤掉了
                        this.checked_at = Some(Instant::now());
                        this.stage = Stage::Idle;
                        cx.notify();
                        return;
                    }
                    Err(err) => log::warn!("重试前检查更新没成：{err:#}"),
                }
                this.download(cx);
            })
            .ok();
        }));
    }

    /// 下载、核对、解压到暂存目录。下载在自己的线程里，这边每 150 毫秒看一眼进度。
    pub fn download(&mut self, cx: &mut Context<Self>) {
        let release = match &self.stage {
            Stage::Available(release)
            | Stage::Failed {
                release: Some(release),
                ..
            } => release.clone(),
            _ => return,
        };
        if let Err(problem) = install_dir() {
            self.stage = Stage::Failed {
                release: Some(release),
                problem,
            };
            cx.notify();
            return;
        }
        let got = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = oneshot::channel();
        let spawned = {
            let (release, got, cancel) = (release.clone(), got.clone(), cancel.clone());
            std::thread::Builder::new()
                .name("kuai-update".into())
                .spawn(move || {
                    let _ = tx.send(fetch_and_stage(&release, &got, &cancel));
                })
        };
        if let Err(err) = spawned {
            let problem = Problem::Download(err.to_string());
            self.stage = Stage::Failed {
                release: Some(release),
                problem,
            };
            cx.notify();
            return;
        }
        self.checking = false;
        self.stage = Stage::Downloading {
            release: release.clone(),
            got,
            cancel,
        };
        cx.notify();
        self._work = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                let finished = match rx.try_recv() {
                    Ok(None) => None,
                    Ok(Some(result)) => Some(result),
                    Err(_) => Some(Err(Problem::Download(tr("下载已中断").into()))),
                };
                let keep_going = this
                    .update(cx, |this, cx| {
                        // 点了取消：不再管它
                        if !matches!(this.stage, Stage::Downloading { .. }) {
                            return false;
                        }
                        cx.notify();
                        let Some(result) = finished else {
                            return true;
                        };
                        let release = release.clone();
                        this.stage = match result {
                            Ok(()) => Stage::Ready(release),
                            Err(problem) => Stage::Failed {
                                release: Some(release),
                                problem,
                            },
                        };
                        false
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        }));
    }

    /// 不下了。
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Stage::Downloading {
            release, cancel, ..
        } = &self.stage
        {
            // 和放进暂存目录互斥：取消以后那个线程不会再把它放进去
            let publishing = PUBLISH.lock();
            cancel.store(true, Ordering::Relaxed);
            drop(publishing);
            self.stage = Stage::Available(release.clone());
            self._work = None;
            cx.notify();
        }
    }

    /// 「重启完成升级」：交给工作区窗口去问、去退出。
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        if matches!(self.stage, Stage::Ready(_)) {
            cx.emit(UpdateEvent::Restart);
        }
    }

    /// 换上下好的新版本，拉起新的 Kuai。返回 true 时调用方马上退出程序。
    pub fn install_and_relaunch(&mut self, cx: &mut Context<Self>) -> bool {
        let Stage::Ready(release) = &self.stage else {
            return false;
        };
        let release = release.clone();
        match install_and_launch(&staging_dir(&release.version)) {
            Ok(()) => true,
            Err(err) => {
                let problem = Problem::Install(format!("{err:#}"));
                self.stage = Stage::Failed {
                    release: Some(release),
                    problem,
                };
                cx.notify();
                false
            }
        }
    }
}

/// 启动时（画任何界面之前）：上次下好了新版本、没点重启就退出了，先换上，再拉起新版本。
/// 返回 true 表示新版本已经拉起来了，这个进程该退出了。
pub fn apply_staged_at_startup() -> bool {
    if install_dir().is_err() {
        return false;
    }
    let Some(version) = newest_staged().filter(|v| *v > current_version()) else {
        return false;
    };
    // 上次在后台跑的安装程序没装上：别再试了，等界面起来告诉人（`Updater::new`）
    if peek_done_mark().as_deref() == Some(env!("CARGO_PKG_VERSION")) {
        return false;
    }
    match install_and_launch(&staging_dir(&version)) {
        Ok(()) => true,
        Err(err) => {
            // 原来只记日志、暂存的留着，每次启动都再试一遍同一个版本，界面上什么都看不到。
            // 记下原因、删掉暂存的，界面起来以后告诉人，要重试就重新下载
            log::warn!("启动时换上新版本没成：{err:#}");
            let text = format!("{version}\n{err:#}");
            let _ = fs::write(update_dir().join(STARTUP_FAILED), text);
            discard_staged();
            false
        }
    }
}

/// 启动时换新版本没成时记下的（版本、原因），读完删掉。
fn take_startup_failure() -> Option<(String, String)> {
    let path = update_dir().join(STARTUP_FAILED);
    let text = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    let (version, err) = text.split_once('\n').unwrap_or((text.as_str(), ""));
    Some((version.trim().to_string(), err.trim().to_string()))
}

// ---------- 问 GitHub ----------

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

fn api_base() -> String {
    std::env::var("FR_UPDATE_API").unwrap_or_else(|_| "https://api.github.com".into())
}

/// 自动问过一次以后，隔多久再问：问成了等 [`CHECK_EVERY`]；连着 `failures` 次没问成的，
/// 按 [`RETRY_AFTER`] 一次比一次久。
fn wait_before_next(failures: usize) -> Duration {
    match failures {
        0 => CHECK_EVERY,
        n => RETRY_AFTER[(n - 1).min(RETRY_AFTER.len() - 1)],
    }
}

/// HTTP 客户端。代理照环境变量 HTTPS_PROXY、ALL_PROXY 走（ureq 默认就读）。
fn agent(body_timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(body_timeout))
        .user_agent(format!("Kuai/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// GitHub 上最新的正式版本。还没发过版（404）、没有 Windows 的发布包，都当作没有。
fn fetch_latest() -> Result<Option<Release>> {
    let url = format!("{}/repos/{REPO}/releases/latest", api_base());
    let response = agent(Duration::from_secs(30))
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call();
    let mut response = match response {
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        other => other.with_context(|| tf!("无法连接 {host}", host = host_of(&url)))?,
    };
    let release: GhRelease = response
        .body_mut()
        .read_json()
        .context(tr("无法解析 GitHub 返回的数据"))?;
    Ok(parse_release(release, kind()))
}

/// 认出版本号和要下的附件：安装版要安装程序（没有就退回 zip，照解压版的办法换），解压版要 zip。
fn parse_release(mut release: GhRelease, kind: Kind) -> Option<Release> {
    if release.draft || release.prerelease {
        return None;
    }
    let version = Version::parse(release.tag_name.trim_start_matches('v')).ok()?;
    let find = |want: fn(&str) -> bool| release.assets.iter().position(|a| want(&a.name));
    let index = match kind {
        Kind::Installed => find(is_setup).or_else(|| find(is_package)),
        Kind::Portable => find(is_package),
    }?;
    let asset = release.assets.swap_remove(index);
    let sha256 = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(|hex| hex.to_ascii_lowercase());
    Some(Release {
        version,
        date: release.published_at.map(|d| d.chars().take(10).collect()),
        notes: notes(release.body.as_deref().unwrap_or("")),
        asset: Asset {
            name: asset.name,
            url: asset.browser_download_url,
            size: asset.size,
            sha256,
        },
    })
}

/// 发布包的名字：`kuai-<版本>-windows-x64.zip`。
fn is_package(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("kuai-") && name.ends_with("-windows-x64.zip")
}

/// 安装程序的名字：`kuai-<版本>-windows-x64-setup.exe`。
fn is_setup(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("kuai-") && name.ends_with("-windows-x64-setup.exe")
}

/// 发布说明（Markdown）里最外层的要点：`- `、`* ` 开头的行，去掉加粗、代码、链接的记号。
fn notes(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")))
        .map(plain)
        .filter(|line| !line.is_empty())
        .take(MAX_NOTES)
        .collect()
}

/// `[文字](链接)` 只留文字，去掉 `**` 和反引号。
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        let tail = &rest[start..];
        let (Some(mid), Some(end)) = (tail.find("]("), tail.find(')')) else {
            break;
        };
        if end < mid {
            break;
        }
        out.push_str(&rest[..start]);
        out.push_str(&tail[1..mid]);
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out.replace("**", "").replace('`', "").trim().to_string()
}

fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split('/').next().unwrap_or(rest)
}

// ---------- 下载、核对、解压 ----------

/// 下载和暂存的目录。
fn update_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FR_UPDATE_DIR") {
        return dir.into();
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("kuai")
        .join("update")
}

fn staging_dir(version: &Version) -> PathBuf {
    update_dir().join(version.to_string())
}

/// 能升级的话返回程序所在的文件夹：得是打好的包（kuai.exe），文件夹得能写。
fn install_dir() -> Result<PathBuf, Problem> {
    let exe = std::env::current_exe().map_err(|err| Problem::Install(err.to_string()))?;
    let packaged = exe
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("kuai.exe"));
    if !packaged {
        return Err(Problem::DevBuild);
    }
    let dir = exe
        .parent()
        .ok_or_else(|| Problem::Install(tr("未找到程序所在的文件夹").into()))?
        .to_path_buf();
    let probe = dir.join(".kuai-write-test");
    match File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            Ok(dir)
        }
        Err(_) => Err(Problem::NotWritable(dir)),
    }
}

/// 在下载线程里跑：下载、核对校验值、放进暂存目录（安装程序直接放，zip 解压）。
fn fetch_and_stage(release: &Release, got: &AtomicU64, cancel: &AtomicBool) -> Result<(), Problem> {
    let Some(expected) = &release.asset.sha256 else {
        return Err(Problem::Download(
            tr("GitHub 未提供此文件的校验值").into(),
        ));
    };
    // 这一次自己的工作目录：下载、核对、解压都在里面，做完整个目录一次改名放进暂存目录。原来
    // 几次下载共用同一个 .part、zip 和暂存目录，取消以后马上重下，旧线程还在写、删、解压同一份
    let n = NEXT_DOWNLOAD.fetch_add(1, Ordering::Relaxed);
    let work = update_dir().join(format!("{WORK_PREFIX}{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).map_err(|err| Problem::Install(err.to_string()))?;
    let staged = work.join("staged");
    let result = fetch_into(release, expected, &work, &staged, got, cancel).and_then(|()| {
        let _publishing = PUBLISH.lock();
        if cancel.load(Ordering::Relaxed) {
            return Err(Problem::Download(tr("已取消").into()));
        }
        let staging = staging_dir(&release.version);
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(|err| Problem::Install(err.to_string()))?;
        }
        fs::rename(&staged, &staging).map_err(|err| Problem::Install(err.to_string()))
    });
    let _ = fs::remove_dir_all(&work);
    result
}

/// 下到 `work` 里、核对校验值，在 `staged` 里摆好要换上的东西（安装程序直接放，zip 解压）。
/// 核对的就是要放进去的那一份文件。
fn fetch_into(
    release: &Release,
    expected: &str,
    work: &Path,
    staged: &Path,
    got: &AtomicU64,
    cancel: &AtomicBool,
) -> Result<(), Problem> {
    let zip = work.join(&release.asset.name);
    let part = work.join(format!("{}.part", release.asset.name));
    let actual = download(&release.asset.url, &part, got, cancel)
        .map_err(|err| Problem::Download(format!("{err:#}")))?;
    if actual != expected {
        return Err(Problem::Digest);
    }
    if is_setup(&release.asset.name) {
        return stage_setup(&part, staged).map_err(|err| Problem::Install(format!("{err:#}")));
    }
    fs::rename(&part, &zip).map_err(|err| Problem::Install(err.to_string()))?;
    extract(&zip, staged).map_err(|err| Problem::Install(format!("{err:#}")))
}

/// 下好的安装程序挪进暂存目录。
fn stage_setup(downloaded: &Path, out: &Path) -> Result<()> {
    if out.exists() {
        fs::remove_dir_all(out)?;
    }
    fs::create_dir_all(out)?;
    fs::rename(downloaded, out.join(SETUP_EXE))?;
    write_ready(out)
}

/// 写暂存目录的就绪记号：里面每个文件一行「SHA-256 相对路径」（路径用 `/`），按路径排。
/// 换上之前照它再核对一遍（[`verify_staged`]）：暂存目录被改过、多了别的文件、没摆全都不换。
fn write_ready(out: &Path) -> Result<()> {
    let lines = staged_files(out)?;
    fs::write(out.join(READY_MARK), lines.join("\n"))?;
    Ok(())
}

/// 暂存目录里的文件（就绪记号除外）：「SHA-256 相对路径」，按路径排。
fn staged_files(root: &Path) -> Result<Vec<String>> {
    fn walk(root: &Path, dir: &Path, lines: &mut Vec<String>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                walk(root, &path, lines)?;
                continue;
            }
            let rel = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            if rel == READY_MARK {
                continue;
            }
            let mut hasher = Sha256::new();
            io::copy(&mut File::open(&path)?, &mut hasher)?;
            let digest: String = hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            lines.push(format!("{digest} {rel}"));
        }
        Ok(())
    }
    let mut lines = Vec::new();
    walk(root, root, &mut lines)?;
    lines.sort_by(|a, b| {
        a.split_once(' ')
            .map(|x| x.1)
            .cmp(&b.split_once(' ').map(|x| x.1))
    });
    Ok(lines)
}

/// 换上之前核对暂存目录：和就绪记号里记的一模一样（同样的文件、同样的内容）才换。
fn verify_staged(staging: &Path) -> Result<()> {
    let mark = fs::read_to_string(staging.join(READY_MARK)).context(tr("新版本未下载完成"))?;
    let recorded: Vec<&str> = mark.lines().filter(|line| !line.is_empty()).collect();
    let actual = staged_files(staging)?;
    if recorded.is_empty() || recorded != actual {
        bail!(tr("暂存的新版本与下载时核对的不一致，请重新下载"));
    }
    Ok(())
}

/// 边下边算 SHA-256，返回小写十六进制。
fn download(url: &str, dest: &Path, got: &AtomicU64, cancel: &AtomicBool) -> Result<String> {
    let mut response = agent(Duration::from_secs(30 * 60))
        .get(url)
        .call()
        .with_context(|| tf!("无法连接 {host}", host = host_of(url)))?;
    let mut reader = response.body_mut().as_reader();
    let mut file = File::create(dest)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = fs::remove_file(dest);
            bail!(tr("已取消"));
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        got.fetch_add(n as u64, Ordering::Relaxed);
    }
    file.flush()?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// 解压发布包。包里最外层的文件夹（`Kuai\`）去掉，解出来的文件直接放在 `out` 下。
fn extract(zip: &Path, out: &Path) -> Result<()> {
    if out.exists() {
        fs::remove_dir_all(out)?;
    }
    fs::create_dir_all(out)?;
    let mut archive = zip::ZipArchive::new(File::open(zip)?)?;
    let mut entries = Vec::new();
    for i in 0..archive.len() {
        let file = archive.by_index(i)?;
        let name = file
            .enclosed_name()
            .ok_or_else(|| anyhow!(tr("发布包中包含不安全的路径")))?;
        entries.push((name, file.is_dir()));
    }
    let top = common_top(&entries);
    for (i, (name, is_dir)) in entries.iter().enumerate() {
        let rel = match &top {
            Some(top) => name.strip_prefix(top).unwrap_or(name),
            None => name.as_path(),
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let path = out.join(rel);
        if *is_dir {
            fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = archive.by_index(i)?;
        io::copy(&mut file, &mut File::create(&path)?)?;
    }
    if !out.join("kuai.exe").is_file() {
        bail!(tr("发布包中缺少 kuai.exe"));
    }
    write_ready(out)
}

/// 所有条目都在同一个最外层文件夹里：返回它。
fn common_top(entries: &[(PathBuf, bool)]) -> Option<PathBuf> {
    let first = entries.first()?.0.components().next()?;
    let top = PathBuf::from(first.as_os_str());
    entries
        .iter()
        .all(|(name, is_dir)| name.starts_with(&top) && (name != &top || *is_dir))
        .then_some(top)
}

// ---------- 换上 ----------

/// 换上暂存目录里的新版本，打开新的 Kuai。成功后调用方马上退出。
fn install_and_launch(staging: &Path) -> Result<()> {
    verify_staged(staging)?;
    let setup = staging.join(SETUP_EXE);
    if setup.is_file() {
        return run_setup(&setup);
    }
    let exe = install(staging)?;
    relaunch(&exe)
}

/// 安装版：在后台跑新的安装程序，不弹向导。它先等这个进程退出（`/waitpid`），装到这份 Kuai
/// 所在的文件夹，装好后打开新的 Kuai（`/relaunch`），见 `tools/installer/kuai.iss`。
/// 没装上的话新的 Kuai 不会打开；下次启动时读到记号里还是自己的版本号，就知道没装上。
fn run_setup(setup: &Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().context(tr("未找到程序所在的文件夹"))?;
    let dir = dir.to_string_lossy();
    let dir = dir.strip_prefix(r"\\?\").unwrap_or(&dir);
    let _ = fs::write(update_dir().join(DONE_MARK), env!("CARGO_PKG_VERSION"));
    let mut command = std::process::Command::new(setup);
    command.args([
        "/VERYSILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/relaunch=1",
    ]);
    // 带空格的路径照 Inno Setup 文档写成 /DIR="…"（交给 arg 的话整个参数外面加引号，成了
    // "/DIR=…"；Inno 解析时也会去掉，但不是文档写的格式）。Windows 路径里不会有引号
    let log = update_dir().join(SETUP_LOG);
    raw_switch(&mut command, "DIR", dir);
    raw_switch(&mut command, "LOG", &log.to_string_lossy());
    command
        .arg(format!("/waitpid={}", std::process::id()))
        .spawn()
        .context(tr("无法启动安装程序"))?;
    Ok(())
}

/// 给安装程序加一个 `/名字="值"` 的开关。
fn raw_switch(command: &mut std::process::Command, name: &str, value: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.raw_arg(format!("/{name}=\"{value}\""));
    }
    #[cfg(not(windows))]
    command.arg(format!("/{name}={value}"));
}

/// 解压版：把暂存目录里的新版本换到程序所在的文件夹：别的文件直接覆盖，kuai.exe 最后换。
/// 返回 exe 的路径（已经是新版本了）。调用方先核对过暂存目录（[`verify_staged`]）。
fn install(staging: &Path) -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().context(tr("未找到程序所在的文件夹"))?;
    copy_tree(staging, staging, dir)?;
    self_replace::self_replace(staging.join("kuai.exe")).context(tr("无法替换 kuai.exe"))?;
    let _ = fs::write(update_dir().join(DONE_MARK), env!("CARGO_PKG_VERSION"));
    // 换上了就删掉：万一新程序报的版本号不比现在新（发版时标签打错），启动时也不会一遍遍地换
    let _ = fs::remove_dir_all(staging);
    Ok(exe)
}

fn copy_tree(root: &Path, from: &Path, to: &Path) -> Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(root)?;
        if rel == Path::new("kuai.exe") || rel == Path::new(READY_MARK) {
            continue;
        }
        let target = to.join(rel);
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&target)?;
            copy_tree(root, &path, to)?;
        } else {
            fs::copy(&path, &target)
                .with_context(|| tf!("无法替换 {path}", path = target.display()))?;
        }
    }
    Ok(())
}

/// 拉起新的 Kuai，参数照旧（开机自启带的「收进托盘」不带：人正看着呢）。
/// 只运行一个 Kuai（`instance.rs`）：新的先等这个进程退出再启动，不然会把自己交给正在退出的这个。
fn relaunch(exe: &Path) -> Result<()> {
    let args = std::env::args_os().skip(1).filter(|arg| {
        let arg = arg.to_string_lossy();
        arg != crate::autostart::MINIMIZED_FLAG && !arg.starts_with(crate::instance::WAIT_PID_FLAG)
    });
    std::process::Command::new(exe)
        .args(args)
        .arg(format!(
            "{}{}",
            crate::instance::WAIT_PID_FLAG,
            std::process::id()
        ))
        .spawn()
        .context(tr("无法启动新版本"))?;
    Ok(())
}

/// 暂存好了的版本里最新的那个。
fn newest_staged() -> Option<Version> {
    fs::read_dir(update_dir())
        .ok()?
        .flatten()
        .filter(|entry| entry.path().join(READY_MARK).is_file())
        .filter_map(|entry| Version::parse(&entry.file_name().to_string_lossy()).ok())
        .max()
}

/// 记号里的版本号：换之前的版本。
fn peek_done_mark() -> Option<String> {
    let from = fs::read_to_string(update_dir().join(DONE_MARK)).ok()?;
    Some(from.trim().to_string()).filter(|v| !v.is_empty())
}

/// 刚升级完：读出原来的版本号，删掉记号。
fn take_done_mark() -> Option<String> {
    let from = peek_done_mark();
    let _ = fs::remove_file(update_dir().join(DONE_MARK));
    from
}

/// 删掉所有暂存的版本（上次没装上，不再自动重试）。
fn discard_staged() {
    let Ok(entries) = fs::read_dir(update_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        if Version::parse(&entry.file_name().to_string_lossy()).is_ok() {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// 升级完了：删掉不比现在新的暂存目录和下了一半的文件。
fn clean_old_downloads() {
    let Ok(entries) = fs::read_dir(update_dir()) else {
        return;
    };
    let current = current_version();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let old = Version::parse(&name).is_ok_and(|v| v <= current);
        if old || name.starts_with(WORK_PREFIX) {
            let _ = fs::remove_dir_all(entry.path());
        } else if name.ends_with(".part") || name.ends_with(".zip") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：gpui_kit 里也有个叫 test 的属性宏，会盖住标准库的 #[test]
    use super::{CHECK_EVERY, wait_before_next};
    use super::{
        GhAsset, GhRelease, Kind, READY_MARK, Version, common_top, copy_tree, extract, stage_setup,
        verify_staged,
    };
    use super::{parse_release, plain};
    use std::fs::{self, File};
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::time::Duration;

    fn gh(tag: &str, assets: &[(&str, Option<&str>)]) -> GhRelease {
        GhRelease {
            tag_name: tag.into(),
            body: Some("新版本\n\n- **点一下升级**：见 [说明](https://x)\n* 修了 `kuaid` 的问题\n  - 缩进的不算\n正文".into()),
            published_at: Some("2026-11-20T08:00:00Z".into()),
            draft: false,
            prerelease: false,
            assets: assets
                .iter()
                .map(|(name, digest)| GhAsset {
                    name: (*name).into(),
                    browser_download_url: format!("https://github.com/x/{name}"),
                    size: 10,
                    digest: digest.map(Into::into),
                })
                .collect(),
        }
    }

    #[test]
    fn 没问成的过一会儿再试() {
        assert_eq!(wait_before_next(0), CHECK_EVERY);
        assert_eq!(wait_before_next(1), Duration::from_secs(60));
        assert_eq!(wait_before_next(3), Duration::from_secs(15 * 60));
        // 再多也是每小时一次，不会比正常的间隔还久
        assert_eq!(wait_before_next(4), Duration::from_secs(60 * 60));
        assert_eq!(wait_before_next(99), Duration::from_secs(60 * 60));
        assert!(wait_before_next(99) < CHECK_EVERY);
    }

    #[test]
    fn 认出发布包和校验值() {
        let release = parse_release(
            gh(
                "v1.8.0",
                &[
                    ("notes.txt", None),
                    ("kuai-1.8.0-windows-x64.zip", Some("sha256:ABCD")),
                ],
            ),
            Kind::Portable,
        )
        .unwrap();
        assert_eq!(release.version, Version::new(1, 8, 0));
        assert_eq!(release.asset.name, "kuai-1.8.0-windows-x64.zip");
        assert_eq!(release.asset.sha256.as_deref(), Some("abcd"));
        assert_eq!(release.date.as_deref(), Some("2026-11-20"));
        assert_eq!(release.notes, ["点一下升级：见 说明", "修了 kuaid 的问题"]);
        assert_eq!(
            release.page(),
            "https://github.com/finmoor/kuai/releases/tag/v1.8.0"
        );
    }

    #[test]
    fn 安装版要安装程序_解压版要zip() {
        let both = || {
            gh(
                "v1.8.0",
                &[
                    ("kuai-1.8.0-windows-x64.zip", Some("sha256:aa")),
                    ("kuai-1.8.0-windows-x64-setup.exe", Some("sha256:bb")),
                ],
            )
        };
        let installed = parse_release(both(), Kind::Installed).unwrap();
        assert_eq!(installed.asset.name, "kuai-1.8.0-windows-x64-setup.exe");
        assert_eq!(installed.asset.sha256.as_deref(), Some("bb"));
        let portable = parse_release(both(), Kind::Portable).unwrap();
        assert_eq!(portable.asset.name, "kuai-1.8.0-windows-x64.zip");
        // 安装版遇到只有 zip 的版本：退回 zip
        let zip_only = gh("v1.8.0", &[("kuai-1.8.0-windows-x64.zip", None)]);
        let fallback = parse_release(zip_only, Kind::Installed).unwrap();
        assert_eq!(fallback.asset.name, "kuai-1.8.0-windows-x64.zip");
        // 解压版遇到只有安装程序的版本：当作没有
        let setup_only = gh("v1.8.0", &[("kuai-1.8.0-windows-x64-setup.exe", None)]);
        assert!(parse_release(setup_only, Kind::Portable).is_none());
    }

    #[test]
    fn 没有发布包或者版本号不对就当没有() {
        let portable = |release| parse_release(release, Kind::Portable);
        assert!(portable(gh("v1.8.0", &[("source.zip", None)])).is_none());
        assert!(portable(gh("nightly", &[("kuai-x-windows-x64.zip", None)])).is_none());
        let mut pre = gh("v1.9.0-beta.1", &[("kuai-1.9.0-windows-x64.zip", None)]);
        pre.prerelease = true;
        assert!(portable(pre).is_none());
    }

    #[test]
    fn 去掉_markdown_记号() {
        assert_eq!(
            plain("看 [这里](http://a) 和 [那里](http://b)。"),
            "看 这里 和 那里。"
        );
        assert_eq!(plain("[没闭合的方括号"), "[没闭合的方括号");
        assert_eq!(plain("a) [b](c"), "a) [b](c");
    }

    #[test]
    fn 最外层的文件夹() {
        let e = |p: &str, d: bool| (PathBuf::from(p), d);
        let entries = [
            e("Kuai/", true),
            e("Kuai/kuai.exe", false),
            e("Kuai/kuaid/x/kuaid", false),
        ];
        assert_eq!(common_top(&entries), Some(PathBuf::from("Kuai")));
        let flat = [e("kuai.exe", false), e("kuaid/x/kuaid", false)];
        assert_eq!(common_top(&flat), None);
    }

    #[test]
    fn 解压和换文件() {
        let root = std::env::temp_dir().join(format!("kuai-update-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // 和 tools/package.ps1 打的一样：最外层一个 Kuai 文件夹
        let zip_path = root.join("kuai-9.9.9-windows-x64.zip");
        {
            let mut zip = zip::ZipWriter::new(File::create(&zip_path).unwrap());
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("Kuai/kuai.exe", options).unwrap();
            zip.write_all(b"new exe").unwrap();
            zip.start_file("Kuai/kuaid/x86_64-unknown-linux-musl/kuaid", options)
                .unwrap();
            zip.write_all(b"new kuaid").unwrap();
            zip.finish().unwrap();
        }
        let staging = root.join("9.9.9");
        extract(&zip_path, &staging).unwrap();
        assert_eq!(fs::read(staging.join("kuai.exe")).unwrap(), b"new exe");
        assert!(staging.join(READY_MARK).is_file());
        verify_staged(&staging).unwrap();

        // 别的文件覆盖到安装目录，kuai.exe 和记号不动（exe 由 self-replace 换）
        let installed = root.join("installed");
        fs::create_dir_all(installed.join("kuaid/x86_64-unknown-linux-musl")).unwrap();
        fs::write(installed.join("kuai.exe"), b"old exe").unwrap();
        fs::write(
            installed.join("kuaid/x86_64-unknown-linux-musl/kuaid"),
            b"old",
        )
        .unwrap();
        copy_tree(&staging, &staging, &installed).unwrap();
        let kuaid = installed.join("kuaid/x86_64-unknown-linux-musl/kuaid");
        assert_eq!(fs::read(kuaid).unwrap(), b"new kuaid");
        assert_eq!(fs::read(installed.join("kuai.exe")).unwrap(), b"old exe");
        assert!(!installed.join(READY_MARK).exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn 换上之前核对暂存的文件() {
        let root = std::env::temp_dir().join(format!("kuai-update-verify-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let downloaded = root.join("setup.part");
        fs::write(&downloaded, b"installer").unwrap();
        let staging = root.join("9.9.9");
        stage_setup(&downloaded, &staging).unwrap();
        verify_staged(&staging).unwrap();
        // 改了内容、多了文件、少了文件、记号是空的（旧版本留下的）都不换
        let setup = staging.join(super::SETUP_EXE);
        fs::write(&setup, b"tampered").unwrap();
        assert!(verify_staged(&staging).is_err());
        fs::write(&setup, b"installer").unwrap();
        fs::write(staging.join("extra.dll"), b"x").unwrap();
        assert!(verify_staged(&staging).is_err());
        fs::remove_file(staging.join("extra.dll")).unwrap();
        verify_staged(&staging).unwrap();
        fs::remove_file(&setup).unwrap();
        assert!(verify_staged(&staging).is_err());
        fs::write(staging.join(READY_MARK), b"").unwrap();
        assert!(verify_staged(&staging).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn 不安全的路径不解() {
        let root = std::env::temp_dir().join(format!("kuai-update-slip-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let zip_path = root.join("bad.zip");
        {
            let mut zip = zip::ZipWriter::new(File::create(&zip_path).unwrap());
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("../evil.txt", options).unwrap();
            zip.write_all(b"x").unwrap();
            zip.finish().unwrap();
        }
        assert!(extract(&zip_path, &root.join("out")).is_err());
        assert!(!root.join("evil.txt").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
