//! 克隆仓库：填地址、放在哪个目录下、文件夹叫什么，克隆到本机或者某台服务器上。放在对话框里。
//!
//! 克隆在目标那边执行（服务器上的克隆由服务器去连远程），进度实时显示；
//! 要密码时走统一的密码弹窗。克隆期间输入框锁住，对话框只能用「中止」关：
//! 中止要再点一次确认，本机上下了一半的目录会删掉，服务器上的要用户自己删（说明里写清楚路径）。

use fr_core::i18n::tr;
use fr_core::tf;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use fr_channel::StreamItem;
use futures::StreamExt as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::{AppState, ConnStatus};
use crate::browser::{BrowserEvent, RemoteBrowser};
use crate::settings::RepoEntry;

pub enum CloneEvent {
    Done(RepoEntry),
}

/// 正在进行的一次克隆。
struct Running {
    /// 克隆到的完整路径
    target: String,
    /// 开始前这个目录不存在：中止后可以放心删掉
    created: bool,
    /// 开始前这个目录在、是空的：中止后把里面的清掉，目录留着
    was_empty: bool,
    _task: Task<()>,
}

/// 中止以后正在清理的本机目录（小写）：清完之前不能再往那里克隆，不然新克隆下来的会被一起删掉。
/// 关了对话框再打开也算，所以是全局的。
static CLEANING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

pub struct CloneView {
    app: Entity<AppState>,
    /// 克隆到哪台服务器；空表示本机
    server: Option<String>,
    url: Entity<InputState>,
    parent: Entity<InputState>,
    /// 文件夹名；跟着地址自动填，用户改过就不再跟
    name: Entity<InputState>,
    /// 上一次自动填进去的文件夹名
    auto_name: String,
    /// 进度或错误
    status: Option<(SharedString, bool)>,
    running: Option<Running>,
    /// 点了「中止」，等用户确认
    confirm_abort: bool,
    _connect: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<CloneEvent> for CloneView {}

impl CloneView {
    pub fn new(
        app: Entity<AppState>,
        server: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr("仓库地址，例如 git@example.com:team/project.git"))
        });
        let parent = cx.new(|cx| InputState::new(window, cx).placeholder(tr("存放位置")));
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(tr("根据地址自动填写")));
        // 默认目录：本机用已有仓库所在的目录，没有就用用户目录；服务器用它的初始目录，没设就是主目录
        let mut status = None;
        let mut connect = None;
        let start_dir = server
            .as_deref()
            .and_then(|id| app.read(cx).server(id))
            .and_then(|server| server.start_dir.clone());
        let default_parent = match &server {
            None => app
                .read(cx)
                .repos_on(crate::machines::LOCAL)
                .unwrap_or_default()
                .iter()
                .rev()
                .find_map(|r| {
                    r.path
                        .rsplit_once(['\\', '/'])
                        .map(|(dir, _)| drive_root(dir))
                })
                .or_else(|| std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).ok()),
            Some(_) if start_dir.is_some() => start_dir,
            Some(id) => match app.read(cx).status(id) {
                ConnStatus::Ready(conn) => Some(conn.hello.home.clone()),
                _ => {
                    // 还没连上：这里就开始连，连上后把主目录填进去
                    let connecting = app.update(cx, |app, cx| app.connect(id, cx));
                    status = Some((tr("正在连接主机…").into(), false));
                    connect = Some(cx.spawn_in(window, async move |this, cx| {
                        let result = connecting.await;
                        this.update_in(cx, |this: &mut Self, window, cx| {
                            match result {
                                Ok(conn) => {
                                    this.status = None;
                                    if this.parent.read(cx).value().trim().is_empty() {
                                        let home = conn.hello.home.clone();
                                        this.parent.update(cx, |state, cx| {
                                            state.set_value(home, window, cx)
                                        });
                                    }
                                }
                                Err(err) => {
                                    this.status =
                                        Some((tf!("无法连接主机：{err}", err).into(), true))
                                }
                            }
                            cx.notify();
                        })
                        .ok();
                    }));
                    None
                }
            },
        };
        if let Some(dir) = default_parent {
            parent.update(cx, |state, cx| state.set_value(dir, window, cx));
        }
        // 地址变了，文件夹名跟着变；用户自己改过文件夹名就不动它
        let subscription = cx.subscribe_in(
            &url,
            window,
            |this: &mut Self, url, event: &InputEvent, window, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                let current = this.name.read(cx).value().trim().to_string();
                if !current.is_empty() && current != this.auto_name {
                    return;
                }
                let derived = Self::repo_name(&url.read(cx).value()).unwrap_or_default();
                if derived != current {
                    this.name
                        .update(cx, |state, cx| state.set_value(derived.clone(), window, cx));
                }
                this.auto_name = derived;
                cx.notify();
            },
        );
        Self {
            app,
            server,
            url,
            parent,
            name,
            auto_name: String::new(),
            status,
            running: None,
            confirm_abort: false,
            _connect: connect,
            _subscription: subscription,
        }
    }

    /// 把键盘焦点放到地址输入框。对话框打开后调用：打开的那一刻焦点会被对话框拿走。
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.url.focus_handle(cx).focus(window, cx);
    }

    /// 正在克隆。这时对话框不能用 Esc、点外面关掉，只能点「中止」。
    pub fn busy(&self) -> bool {
        self.running.is_some()
    }

    /// 从地址里取仓库名：最后一段，去掉 `.git`。
    fn repo_name(url: &str) -> Option<String> {
        let trimmed = url.trim().trim_end_matches(['/', '\\']);
        let last = trimmed.rsplit(['/', '\\', ':']).next()?;
        let name = last.strip_suffix(".git").unwrap_or(last);
        (!name.is_empty()).then(|| name.to_string())
    }

    /// 存放位置，换成 git -C 认得的写法：本机只写了盘符（`D:`）的补成盘的根目录，服务器上的 `~`
    /// 换成主目录。预览和执行都用它，克隆到的就是预览里写的地方。
    fn parent_dir(&self, cx: &App) -> String {
        let parent = self.parent.read(cx).value().trim().to_string();
        match &self.server {
            None => drive_root(&parent),
            Some(id) => match self.app.read(cx).status(id) {
                ConnStatus::Ready(conn) => expand_home(&parent, &conn.hello.home),
                // 还没连上：原样（git 进不去会报存放位置不存在）
                _ => parent,
            },
        }
    }

    /// 现在填的内容会克隆到哪里；还没填全时为空。
    fn target(&self, cx: &App) -> Option<String> {
        let parent = self.parent_dir(cx);
        let name = self.name.read(cx).value().trim().to_string();
        (!parent.is_empty() && !name.is_empty())
            .then(|| join(&parent, &name, self.server.is_none()))
    }

    pub fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        let url = self.url.read(cx).value().trim().to_string();
        let parent = self.parent_dir(cx);
        let name = self.name.read(cx).value().trim().to_string();
        let problem = if url.is_empty() {
            Some(tr("请填写仓库地址").to_string())
        } else if parent.is_empty() {
            Some(tr("请填写存放位置").to_string())
        } else {
            name_error(&name)
        };
        let local = self.server.is_none();
        let target = join(&parent, &name, local);
        // 上次中止的还在清：等清完（清的时候它不是空的，下面会说「已存在且不为空」，不对）
        let cleaning = local && lock_cleaning().contains(&target.to_lowercase());
        // 本机可以先看一眼：目标已经有东西就不用等 git 报错了
        let problem = problem
            .or_else(|| {
                cleaning.then(|| {
                    tf!(
                        "正在清理上次未完成的 {target}，请稍后再试。",
                        target = &target
                    )
                })
            })
            .or_else(|| (local && dir_not_empty(Path::new(&target))).then(|| exists_text(&target)));
        if let Some(problem) = problem {
            self.status = Some((problem.into(), true));
            cx.notify();
            return;
        }
        let created = local && !Path::new(&target).exists();
        let was_empty = local && Path::new(&target).is_dir() && !dir_not_empty(Path::new(&target));
        if local {
            // 本机的目录不存在就建出来；服务器上的要先有
            let _ = std::fs::create_dir_all(&parent);
        }
        let channel = self
            .app
            .update(cx, |app, cx| app.channel_for(self.server.as_deref(), cx));
        let server = self.server.clone();
        let done_target = target.clone();
        self.status = Some((tr("正在连接…").into(), false));
        self.confirm_abort = false;
        let task = cx.spawn_in(window, async move |this, cx| {
            let result: Result<RepoEntry, String> = async {
                let channel = channel.await.map_err(|err| err.to_string())?;
                let clone = vec!["clone".into(), "--progress".into(), "--".into(), url, name];
                let mut stream = channel.git_progress(&parent, clone);
                let mut stderr = Vec::new();
                loop {
                    match stream.next().await {
                        Some(Ok(StreamItem::Chunk(_))) => {}
                        Some(Ok(StreamItem::Progress(chunk))) => {
                            stderr.extend_from_slice(&chunk);
                            let line = last_line(&stderr);
                            this.update(cx, |this, cx| {
                                this.status = Some((line.into(), false));
                                cx.notify();
                            })
                            .map_err(|_| tr("已取消").to_string())?;
                        }
                        Some(Ok(StreamItem::Done(out))) if out.success() => break,
                        Some(Ok(StreamItem::Done(out))) => return Err(tail(&out.stderr)),
                        Some(Err(err)) => return Err(format!("{err:#}")),
                        None => return Err(tr("连接中断").into()),
                    }
                }
                // 按 git 认的仓库根目录登记：存放位置写的是相对路径、带 `..`、大小写不同时，
                // 和自己拼出来的不一样，列表里会多出一个打不开的
                let found = crate::open_dir::find(&channel, &done_target).await;
                Ok(match found {
                    Ok(crate::open_dir::Found::Repo(root)) => {
                        crate::open_dir::entry(server.clone(), &root)
                    }
                    _ => RepoEntry {
                        server: server.clone(),
                        path: done_target.clone(),
                    },
                })
            }
            .await;
            this.update_in(cx, |this, _, cx| {
                this.running = None;
                this.confirm_abort = false;
                match result {
                    Ok(entry) => cx.emit(CloneEvent::Done(entry)),
                    Err(err) => this.status = Some((explain(&err, &done_target).into(), true)),
                }
                cx.notify();
            })
            .ok();
        });
        self.running = Some(Running {
            target,
            created,
            was_empty,
            _task: task,
        });
        cx.notify();
    }

    /// 中止正在进行的克隆。本机上这次新建的目录删掉；服务器上的删不了，告诉用户在哪。
    fn abort(&mut self, cx: &mut Context<Self>) {
        let Some(running) = self.running.take() else {
            return;
        };
        self.confirm_abort = false;
        let Running {
            target,
            created,
            was_empty,
            _task,
        } = running;
        // 丢掉任务：本机的 git 进程被杀掉，服务器上的请求发取消
        drop(_task);
        if self.server.is_some() {
            self.status = Some((
                tf!(
                    "克隆已中止。主机上的 {target} 可能有残留内容，请删除该目录或更换名称后重试。",
                    target
                )
                .into(),
                true,
            ));
        } else if created || was_empty {
            let status = match created {
                true => tr("克隆已中止，正在删除未完成的目录…"),
                false => tr("克隆已中止，正在清理目录中未完成的内容…"),
            };
            self.status = Some((status.into(), false));
            let path = target.clone();
            // 清完之前不让再往这里克隆（见 `CLEANING`）
            lock_cleaning().insert(path.to_lowercase());
            let remove = cx.background_executor().spawn(async move {
                // git 进程刚被杀掉，文件可能还占着，多试几次
                let mut last = None;
                for _ in 0..15 {
                    // 这次新建的目录整个删掉；本来就在的空目录只清里面的
                    let removed = match created {
                        true => std::fs::remove_dir_all(&path),
                        false => empty_dir(Path::new(&path)),
                    };
                    match removed {
                        Ok(()) => {
                            last = None;
                            break;
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                            last = None;
                            break;
                        }
                        Err(err) => last = Some(err),
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                lock_cleaning().remove(&path.to_lowercase());
                match last {
                    Some(err) => Err(err.to_string()),
                    None => Ok(()),
                }
            });
            cx.spawn(async move |this, cx| {
                let result = remove.await;
                this.update(cx, |this, cx| {
                    // 这期间又开始了新的克隆就不动它的进度
                    if this.running.is_none() {
                        this.status = Some(match (result, created) {
                            (Ok(()), true) => (
                                tf!("克隆已中止，未完成的 {target} 已删除。", target).into(),
                                false,
                            ),
                            (Ok(()), false) => (
                                tf!("克隆已中止，{target} 中未完成的内容已清除。", target).into(),
                                false,
                            ),
                            (Err(err), true) => (
                                tf!("克隆已中止，但未能删除未完成的 {target}（{err}），请手动删除。", target, err)
                                .into(),
                                true,
                            ),
                            (Err(err), false) => (
                                tf!("克隆已中止，但未能清除 {target} 中未完成的内容（{err}），请手动删除。", target, err)
                                .into(),
                                true,
                            ),
                        });
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        } else {
            self.status = Some((tr("克隆已中止。").into(), false));
        }
        cx.notify();
    }

    /// 选放在哪个目录下：本机用系统的选文件夹，服务器上用目录浏览器。
    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.server.clone() {
            None => {
                let picked = cx.prompt_for_paths(PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some(tr("克隆到此目录").into()),
                });
                cx.spawn_in(window, async move |this, cx| {
                    let Ok(Ok(Some(paths))) = picked.await else {
                        return;
                    };
                    let Some(dir) = paths.into_iter().next() else {
                        return;
                    };
                    let dir = dir.to_string_lossy().into_owned();
                    this.update_in(cx, |this, window, cx| {
                        this.parent
                            .update(cx, |state, cx| state.set_value(dir, window, cx));
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            }
            Some(server) => {
                let app = self.app.clone();
                let start = self.parent.read(cx).value().trim().to_string();
                let browser =
                    cx.new(|cx| RemoteBrowser::new_dir_picker(app, server, start, window, cx));
                cx.subscribe_in(&browser, window, |this, _, event, window, cx| {
                    if let BrowserEvent::Dir(path) = event {
                        // 关掉的是叠在上面的目录浏览器，克隆对话框还在
                        window.close_dialog(cx);
                        let path = path.clone();
                        this.parent
                            .update(cx, |state, cx| state.set_value(path, window, cx));
                        cx.notify();
                    }
                })
                .detach();
                window.open_dialog(cx, move |dialog, _, _| {
                    dialog
                        .title(tr("选择存放位置"))
                        .w(px(640.))
                        // 回车是「前往」输入的路径，不是关掉对话框
                        .on_ok(|_, _, _| false)
                        .child(browser.clone())
                });
            }
        }
    }
}

fn lock_cleaning() -> std::sync::MutexGuard<'static, HashSet<String>> {
    CLEANING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 删掉目录里的东西，目录本身留着（克隆前它就在、是空的）。
fn empty_dir(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        match entry.file_type()?.is_dir() {
            true => std::fs::remove_dir_all(entry.path())?,
            false => std::fs::remove_file(entry.path())?,
        }
    }
    Ok(())
}

/// 本机只写了盘符（`D:`）时补成盘的根目录：`git -C D:` 进的是那个盘的「当前目录」，
/// 和预览里拼出来的 `D:\名字` 不是一个地方。
pub(crate) fn drive_root(path: &str) -> String {
    let bytes = path.as_bytes();
    match bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        true => format!("{path}\\"),
        false => path.to_string(),
    }
}

/// 服务器上的 `~`、`~/…` 换成主目录 `home`：git -C 不展开 `~`（目录浏览器里能用，这里也要能用）。
pub(crate) fn expand_home(path: &str, home: &str) -> String {
    let home = home.trim_end_matches(['/', '\\']);
    if path == "~" {
        return home.to_string();
    }
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => format!("{home}/{rest}"),
        None => path.to_string(),
    }
}

/// 目录和文件夹名拼成完整路径。本机用反斜杠，服务器上用斜杠。
pub(crate) fn join(parent: &str, name: &str, local: bool) -> String {
    // Windows 本机用反斜杠，别的（服务器上的、Linux 本机）用正斜杠
    let separator = if local && cfg!(windows) { '\\' } else { '/' };
    // 反斜杠只在 Windows 的路径里是分隔符（本机是 Windows，或者写着盘符的 Windows 主机）；Linux 上
    // 它是名字里的一个字，裁掉的话拼出来的和交给 git 的不是同一个目录，中止时会清理错地方
    let windows = (local && cfg!(windows)) || parent.as_bytes().get(1) == Some(&b':');
    let trimmed = match windows {
        true => parent.trim_end_matches(['/', '\\']),
        false => parent.trim_end_matches('/'),
    };
    // 根目录（`/`、`C:\`）去掉尾巴后会变空或只剩盘符，补回分隔符
    format!("{trimmed}{separator}{name}")
}

/// 文件夹名不能用时的说明。
fn name_error(name: &str) -> Option<String> {
    if name.is_empty() {
        Some(tr("请填写文件夹名称").into())
    } else if name == "."
        || name == ".."
        || name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
    {
        Some(tr("文件夹名称不能包含 / \\ : * ? \" < > |。存放位置请在上方填写。").into())
    } else {
        None
    }
}

fn dir_not_empty(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some())
}

fn exists_text(target: &str) -> String {
    tf!(
        "{target} 已存在且不为空。请更换文件夹名称，或先删除该目录。",
        target
    )
}

/// 克隆失败的中文说明，后面附上 git 的原话。
fn explain(err: &str, target: &str) -> String {
    let lower = err.to_lowercase();
    let reason = if lower.contains("already exists and is not an empty directory") {
        Some(exists_text(target))
    } else if lower.contains("repository not found")
        || lower.contains("does not appear to be a git repository")
        || lower.contains("does not exist")
    {
        Some(tr("未找到仓库。请检查地址及账号的访问权限。").to_string())
    } else if lower.contains("no such file or directory") || lower.contains("cannot find the path")
    {
        Some(tr("存放位置不存在。请先在主机上创建该目录，或点击「浏览…」选择。").to_string())
    } else {
        crate::repo::explain(err).map(str::to_string)
    };
    match reason {
        Some(reason) => format!("{reason}\n\n{err}"),
        None => err.to_string(),
    }
}

/// 进度里最新的一行（git 用回车在同一行上刷新进度）。
fn last_line(stderr: &[u8]) -> String {
    let tail = &stderr[stderr.len().saturating_sub(2000)..];
    String::from_utf8_lossy(tail)
        .split(['\r', '\n'])
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// 报错的最后几行，去掉被回车刷新掉的进度。
fn tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(|line| {
            line.trim_end_matches('\r')
                .rsplit('\r')
                .next()
                .unwrap_or(line)
        })
        .filter(|line| !line.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(6)..].join("\n")
}

impl Render for CloneView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let busy = self.running.is_some();
        let label = move |text: &'static str| div().text_xs().text_color(muted).child(text);
        // 私有仓库在哪一端认证要说清：克隆到主机时是主机去连，用的是主机上的密钥、凭据
        let hint = match &self.server {
            None => tr("克隆到本机。目录不存在时将自动创建。"),
            Some(_) => tr(
                "由主机连接远程仓库，存放位置须已存在。私有仓库在主机上认证：SSH 地址需要主机上的密钥，HTTPS 地址将提示输入用户名和访问令牌。",
            ),
        };
        // 实际会克隆到哪里：克隆期间显示的是开始时定下来的
        let target = match &self.running {
            Some(running) => Some(running.target.clone()),
            None => self.target(cx),
        };
        let buttons = if busy && self.confirm_abort {
            h_flex()
                .justify_end()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .child(tr("中止克隆？已下载的内容不会保留。")),
                )
                .child(
                    Button::new("keep")
                        .label(tr("继续克隆"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_abort = false;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("abort")
                        .label(tr("中止克隆"))
                        .with_variant(ButtonVariant::Danger)
                        .on_click(cx.listener(|this, _, _, cx| this.abort(cx))),
                )
        } else {
            h_flex()
                .justify_end()
                .gap_2()
                .child(if busy {
                    Button::new("cancel")
                        .label(tr("中止…"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_abort = true;
                            cx.notify();
                        }))
                } else {
                    Button::new("cancel")
                        .label(tr("取消"))
                        .on_click(|_, window, cx| window.close_dialog(cx))
                })
                .child(
                    Button::new("clone")
                        .primary()
                        .label(tr("克隆"))
                        .loading(busy)
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, window, cx| this.start(window, cx))),
                )
        };
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(h_flex().gap_2().child(label(tr("地址"))).child(label(tr(
                        "在托管网站仓库页的「Code」（克隆）按钮中复制，HTTPS、SSH 地址均可",
                    ))))
                    .child(Input::new(&self.url).disabled(busy)),
            )
            .child(
                v_flex().gap_1().child(label(tr("存放位置"))).child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .child(Input::new(&self.parent).disabled(busy)),
                        )
                        .child(
                            Button::new("browse")
                                .label(tr("浏览…"))
                                .disabled(busy)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.browse(window, cx)),
                                ),
                        ),
                ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(label(tr("文件夹名称")))
                    .child(Input::new(&self.name).disabled(busy)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .text_xs()
                    .text_color(muted)
                    .when_some(target, |view, target| {
                        view.child(div().child(tf!("将克隆到：{target}", target)))
                    })
                    .child(hint),
            )
            .when_some(self.status.clone(), |view, (text, error)| {
                view.child(
                    div()
                        .text_sm()
                        .whitespace_normal()
                        .text_color(if error { danger } else { muted })
                        .child(text),
                )
            })
            .child(buttons)
    }
}

#[cfg(test)]
mod tests {
    use super::{CloneView, drive_root, empty_dir, expand_home, explain, join, name_error};

    #[test]
    fn 只写盘符的补成根目录_服务器上的波浪号换成主目录() {
        assert_eq!(drive_root("D:"), r"D:\");
        assert_eq!(drive_root(r"D:\work"), r"D:\work");
        assert_eq!(drive_root("/home"), "/home");
        assert_eq!(expand_home("~", "/home/me"), "/home/me");
        assert_eq!(expand_home("~/src", "/home/me/"), "/home/me/src");
        assert_eq!(expand_home("/opt/~x", "/home/me"), "/opt/~x");
        assert_eq!(expand_home("~other/x", "/home/me"), "~other/x");
    }

    #[test]
    fn 清空目录只删里面的() {
        let dir = std::env::temp_dir().join(format!("kuai-clone-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git").join("objects")).unwrap();
        std::fs::write(dir.join(".git").join("HEAD"), b"ref").unwrap();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        empty_dir(&dir).unwrap();
        assert!(dir.is_dir());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn 从地址里取仓库名() {
        let name = |url: &str| CloneView::repo_name(url);
        assert_eq!(
            name("git@example.com:team/project.git").as_deref(),
            Some("project")
        );
        assert_eq!(name("https://example.com/a/b/").as_deref(), Some("b"));
        assert_eq!(name("git@example.com:solo.git").as_deref(), Some("solo"));
        assert_eq!(name(r"C:\repos\origin.git").as_deref(), Some("origin"));
        assert_eq!(name("  "), None);
    }

    #[test]
    fn 拼出克隆到的路径() {
        #[cfg(windows)]
        assert_eq!(join(r"C:\work\", "app", true), r"C:\work\app");
        #[cfg(not(windows))]
        assert_eq!(join("/home/me/", "app", true), "/home/me/app");
        assert_eq!(join("/home/me", "app", false), "/home/me/app");
        assert_eq!(join("/", "app", false), "/app");
        // Linux 上名字里的反斜杠不是分隔符；Windows 主机上的照样裁
        assert_eq!(join(r"/data/a\", "app", false), r"/data/a\/app");
        assert_eq!(join(r"D:\work\", "app", false), "D:\\work/app");
    }

    #[test]
    fn 文件夹名只能是一个名字() {
        assert!(name_error("my-app.v2").is_none());
        for bad in ["", "a/b", r"a\b", "..", "a:b", "what?"] {
            assert!(name_error(bad).is_some(), "{bad}");
        }
    }

    #[test]
    fn 目标已存在时说人话() {
        let text = explain(
            "fatal: destination path 'app' already exists and is not an empty directory.",
            "/home/me/app",
        );
        assert!(text.starts_with("/home/me/app 已存在"), "{text}");
        assert!(text.contains("destination path"), "{text}");
        // 认不出来的原样显示
        assert_eq!(explain("fatal: 奇怪的错误", "/x"), "fatal: 奇怪的错误");
    }
}
