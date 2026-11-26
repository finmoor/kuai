//! 打开一个目录：在仓库里就打开仓库的根目录；不是仓库就先问是否在此目录中初始化新仓库。
//!
//! 入口：仓库页的「添加本机仓库…」、服务器目录浏览器的「打开此仓库」、命令行 `kuai <路径>`，
//! 以及再次启动 Kuai 时带的路径（交给已经打开的那个，见 `instance`）。
//!
//! 怎么判断「不是仓库」：`rev-parse --show-toplevel` 失败后，带上 `safe.directory=*` 再执行一次
//! `rev-parse --git-dir`。这次成功说明是仓库、只是打不开（所有者不是当前用户、裸仓库、选的是
//! `.git` 目录本身），照 git 的原话报错；仍然失败，再看目录进不进得去（`git version` 不找仓库，
//! 进得去目录就成功），进得去才是不在仓库中。不按报错的文字判断：git 的报错会随系统语言翻译。

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use fr_channel::Channel;
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::notification::Notification;
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::settings::RepoEntry;

/// 查目录的结果。
pub enum Found {
    /// 在仓库中：仓库的根目录（git 给的写法）
    Repo(String),
    /// 不在任何仓库中，可以初始化
    NotRepo,
}

/// 打开仓库以后做什么（加进列表、打开标签页……），由调用方定。
type OnOpen<T> = Box<dyn FnOnce(&mut T, RepoEntry, &mut Window, &mut Context<T>)>;

/// 打开、初始化仓库失败的报错，不自动消失（读完自己关）。Windows 上找不到 git（新装的系统上没有）
/// 时带「打开下载页面」：提示里写的 git-scm.com 点不了，原来这条 5 秒就消失、也找不回来。
fn open_failed(err: String) -> Notification {
    let missing_git = cfg!(windows) && err.contains(&fr_core::git::missing());
    let note = Notification::error(err).autohide(false);
    if !missing_git {
        return note;
    }
    note.action(|_, _, cx| {
        let note = cx.entity().downgrade();
        Button::new("git-download")
            .small()
            .label(tr("打开下载页面"))
            .on_click(move |_, window, cx| {
                let result = crate::shell::open_url(GIT_DOWNLOAD);
                crate::shell::report(result, window, cx);
                note.update(cx, |note, cx| note.dismiss(window, cx)).ok();
            })
    })
}

/// Git for Windows 的下载页。
const GIT_DOWNLOAD: &str = "https://git-scm.com/download/win";

/// 打开 `server`（空着是本机）上的目录 `dir`：是仓库就交给 `on_open`，不是就问是否初始化，
/// 初始化成功后同样交给 `on_open`。出错时在右下角报。
pub fn open_dir<T: 'static>(
    app: &Entity<AppState>,
    server: Option<String>,
    dir: String,
    window: &mut Window,
    cx: &mut Context<T>,
    on_open: impl FnOnce(&mut T, RepoEntry, &mut Window, &mut Context<T>) + 'static,
) {
    // 本机的目录先看在不在：`git -C` 进不去时报的错和「不是仓库」分不开，会被当成可以初始化
    if server.is_none() && !Path::new(&dir).is_dir() {
        let text = tf!("未找到目录 {dir}", dir);
        window.push_notification(Notification::error(text).autohide(false), cx);
        return;
    }
    let on_open: OnOpen<T> = Box::new(on_open);
    let channel = app.update(cx, |app, cx| app.channel_for(server.as_deref(), cx));
    cx.spawn_in(window, async move |this, cx| {
        let result = match channel.await {
            Ok(channel) => find(&channel, &dir).await.map(|found| (channel, found)),
            Err(err) => Err(err.to_string()),
        };
        this.update_in(cx, |this, window, cx| match result {
            Ok((_, Found::Repo(root))) => on_open(this, entry(server, &root), window, cx),
            // 主目录不给初始化：多半是选错了，整个主目录变成仓库很难收拾
            Ok((channel, Found::NotRepo)) if is_home(&channel, &dir) => {
                let text = tf!("{dir} 是主目录，不是 Git 仓库。请选择其中的项目目录。", dir);
                window.push_notification(Notification::warning(text), cx);
            }
            Ok((channel, Found::NotRepo)) => ask_init(channel, server, dir, on_open, window, cx),
            Err(err) => window.push_notification(open_failed(err), cx),
        })
        .ok();
    })
    .detach();
}

/// 一次打开几个目录（「添加本机仓库…」里多选了）。只选了一个照 [`open_dir`]，不是仓库会问是否初始化；
/// 几个时把在仓库中的一起交给 `on_open`（同一个仓库里的几个目录只算一个），不是仓库的不逐个问，
/// 在右下角说一声，要初始化就单独选那个目录。
pub fn open_dirs<T: 'static>(
    app: &Entity<AppState>,
    server: Option<String>,
    mut dirs: Vec<String>,
    window: &mut Window,
    cx: &mut Context<T>,
    on_open: impl FnOnce(&mut T, Vec<RepoEntry>, &mut Window, &mut Context<T>) + 'static,
) {
    if dirs.len() == 1 {
        let dir = dirs.remove(0);
        return open_dir(
            app,
            server,
            dir,
            window,
            cx,
            move |this, entry, window, cx| on_open(this, vec![entry], window, cx),
        );
    }
    let channel = app.update(cx, |app, cx| app.channel_for(server.as_deref(), cx));
    cx.spawn_in(window, async move |this, cx| {
        let mut repos: Vec<RepoEntry> = Vec::new();
        let mut not_repos: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        match channel.await {
            Ok(channel) => {
                for dir in dirs {
                    // 同 `open_dir`：本机的目录先看在不在
                    if server.is_none() && !Path::new(&dir).is_dir() {
                        errors.push(tf!("未找到目录 {dir}", dir));
                        continue;
                    }
                    match find(&channel, &dir).await {
                        Ok(Found::Repo(root)) => {
                            let found = entry(server.clone(), &root);
                            if !repos.contains(&found) {
                                repos.push(found);
                            }
                        }
                        Ok(Found::NotRepo) => not_repos.push(dir),
                        Err(err) => errors.push(err),
                    }
                }
            }
            Err(err) => errors.push(err.to_string()),
        }
        this.update_in(cx, |this, window, cx| {
            for err in errors {
                window.push_notification(open_failed(err), cx);
            }
            if !not_repos.is_empty() {
                let dirs = not_repos.join(tr("、"));
                let text = tf!(
                    "{dirs} 不是 Git 仓库，未添加。如需初始化，请单独选择该目录。",
                    dirs
                );
                window.push_notification(Notification::warning(text), cx);
            }
            if !repos.is_empty() {
                on_open(this, repos, window, cx);
            }
        })
        .ok();
    })
    .detach();
}

/// 这个目录在不在仓库中。返回的错误是给人看的。
pub async fn find(channel: &Channel, dir: &str) -> Result<Found, String> {
    let cannot_open = |err: &dyn std::fmt::Display| tf!("无法打开 {dir}：{err}", dir, err);
    let out = channel
        .git(dir, args(&["rev-parse", "--show-toplevel"]))
        .await
        .map_err(|err| cannot_open(&format!("{err:#}")))?;
    if out.success() {
        return Ok(Found::Repo(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ));
    }
    let probe = channel
        .git(
            dir,
            args(&["-c", "safe.directory=*", "rev-parse", "--git-dir"]),
        )
        .await;
    let not_repo = matches!(&probe, Ok(probe) if probe.code.is_some() && !probe.success());
    // 两次都失败也可能是目录进不去（服务器上的目录刚被删、没有权限）：`git -C` 进不去时也是 128，
    // 当成不是仓库会问要不要初始化，而 `git init` 会把不在的目录建出来
    let reachable = not_repo
        && channel
            .git(dir, args(&["version"]))
            .await
            .is_ok_and(|out| out.success());
    match reachable {
        true => Ok(Found::NotRepo),
        // 是仓库只是打不开、目录进不去，或者第二次没执行成：都照第一次 git 的原话报
        false => Err(cannot_open(&out.stderr.trim())),
    }
}

/// `dir` 是不是这台主机的主目录（只比写法，两种斜杠、末尾的斜杠、Windows 上的大小写都不算）。
pub fn is_home(channel: &Channel, dir: &str) -> bool {
    let home = match channel {
        Channel::Local => fr_core::dir::home(),
        Channel::Remote(conn) => conn.hello.home.clone(),
    };
    let norm = |path: &str| {
        let path = path.replace('\\', "/");
        let path = path.trim_end_matches('/').to_string();
        match cfg!(windows) || path.contains(':') {
            true => path.to_lowercase(),
            false => path,
        }
    };
    !home.is_empty() && norm(&home) == norm(dir)
}

/// 问是否在 `dir` 中初始化新仓库；点「初始化仓库」后执行 `git init`，成功了交给 `on_open`。
/// 「取消」是主按钮、回车也是取消：进错目录的人按回车只会退回，不会建出 `.git`。
pub fn ask_init<T: 'static>(
    channel: Channel,
    server: Option<String>,
    dir: String,
    on_open: impl FnOnce(&mut T, RepoEntry, &mut Window, &mut Context<T>) + 'static,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let on_open: OnOpen<T> = Box::new(on_open);
    let target = cx.entity().downgrade();
    let description = tf!(
        "{dir} 不是 Git 仓库。将在此目录中创建新仓库，现有文件不受影响。如只是选错了目录，请取消后继续浏览。",
        dir
    );
    // 对话框每次重画都会调一遍下面的闭包，要做的事只能取走一次
    let job = Rc::new(Cell::new(Some((channel, server, dir, on_open))));
    window.open_alert_dialog(cx, move |alert, _, _| {
        let start = {
            let (target, job) = (target.clone(), job.clone());
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                window.close_dialog(cx);
                if let Some((channel, server, dir, on_open)) = job.take() {
                    target
                        .update(cx, |_, cx| {
                            run_init(channel, server, dir, on_open, window, cx)
                        })
                        .ok();
                }
            }
        };
        alert
            .title(tr("在此初始化仓库？"))
            .description(description.clone())
            // 回车是取消：选错了目录的人按回车只会退回
            .on_ok(|_, window, cx| {
                window.close_dialog(cx);
                false
            })
            .footer(
                DialogFooter::new()
                    .child(Button::new("ok").label(tr("初始化仓库")).on_click(start))
                    .child(
                        Button::new("cancel")
                            .label(tr("取消"))
                            .with_variant(ButtonVariant::Primary)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
    });
}

fn run_init<T: 'static>(
    channel: Channel,
    server: Option<String>,
    dir: String,
    on_open: OnOpen<T>,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    cx.spawn_in(window, async move |this, cx| {
        let result = init(&channel, &dir).await;
        this.update_in(cx, |this, window, cx| match result {
            Ok(root) => on_open(this, entry(server, &root), window, cx),
            Err(err) => window.push_notification(open_failed(err), cx),
        })
        .ok();
    })
    .detach();
}

/// `git init`，再取仓库的根目录。
async fn init(channel: &Channel, dir: &str) -> Result<String, String> {
    let failed = |err: &dyn std::fmt::Display| tf!("初始化仓库失败：{err}", err);
    channel
        .git(dir, args(&["init"]))
        .await
        .and_then(|out| out.ok())
        .map_err(|err| failed(&format!("{err:#}")))?;
    match find(channel, dir).await? {
        Found::Repo(root) => Ok(root),
        Found::NotRepo => Err(failed(&dir)),
    }
}

/// 仓库列表里的一项。本机是 Windows 时路径换成 Windows 的写法，和以前加进来的一致。
/// git 给的仓库根目录（`rev-parse --show-toplevel`）在列表里的写法：Windows 本机换成反斜杠。
pub(crate) fn entry(server: Option<String>, root: &str) -> RepoEntry {
    let path = match server {
        None if cfg!(windows) => root.replace('/', "\\"),
        _ => root.to_string(),
    };
    RepoEntry { server, path }
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| arg.to_string()).collect()
}

#[cfg(test)]
mod tests {
    // 不用 `super::*`：那样会把 gpui 的 `test` 宏也引进来，盖掉标准的 `#[test]`
    use super::{Found, args, find, init};
    use fr_channel::Channel;
    use futures::executor::block_on;

    /// 测试用的临时目录，测完删掉。
    struct Temp(std::path::PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("kuai-open-dir-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, rel: &str) -> String {
            self.0.join(rel).to_string_lossy().into_owned()
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(dir: &str, list: &[&str]) {
        let out = block_on(Channel::Local.git(dir, args(list))).unwrap();
        assert!(out.success(), "git {list:?}：{}", out.stderr);
    }

    #[test]
    fn 不在仓库中的目录可以初始化() {
        let temp = Temp::new("plain");
        let dir = temp.path("");
        assert!(matches!(
            block_on(find(&Channel::Local, &dir)),
            Ok(Found::NotRepo)
        ));
        let root = block_on(init(&Channel::Local, &dir)).unwrap();
        assert!(temp.0.join(".git").is_dir());
        assert_eq!(
            std::fs::canonicalize(&root).unwrap(),
            std::fs::canonicalize(&temp.0).unwrap()
        );
    }

    #[test]
    fn 仓库的子目录打开的是仓库根目录() {
        let temp = Temp::new("sub");
        git(&temp.path(""), &["init", "-q"]);
        std::fs::create_dir_all(temp.0.join("a").join("b")).unwrap();
        let Ok(Found::Repo(root)) = block_on(find(&Channel::Local, &temp.path("a/b"))) else {
            panic!("子目录应该认出仓库");
        };
        assert_eq!(
            std::fs::canonicalize(&root).unwrap(),
            std::fs::canonicalize(&temp.0).unwrap()
        );
    }

    #[test]
    fn 是仓库但打不开的照原话报错_不问初始化() {
        let temp = Temp::new("bare");
        let bare = temp.path("bare.git");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "-q", "--bare"]);
        assert!(block_on(find(&Channel::Local, &bare)).is_err());

        // 选的是 .git 目录本身
        let work = temp.path("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q"]);
        let dot_git = temp.path("work/.git");
        assert!(block_on(find(&Channel::Local, &dot_git)).is_err());
    }

    #[test]
    fn 目录不在了_报错_不问初始化() {
        let temp = Temp::new("gone");
        let gone = temp.path("gone");
        let Err(err) = block_on(find(&Channel::Local, &gone)) else {
            panic!("不在的目录不该当成可以初始化");
        };
        assert!(err.contains(&gone), "{err}");
        assert!(!temp.0.join("gone").exists());
    }
}
