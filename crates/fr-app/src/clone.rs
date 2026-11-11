//! 克隆仓库：填地址、放在哪个目录下、文件夹叫什么，克隆到本机或者某台服务器上。放在对话框里。
//!
//! 克隆在目标那边执行（服务器上的克隆由服务器去连远程），进度实时显示；
//! 要密码时走统一的密码弹窗。克隆期间输入框锁住，对话框只能用「中止」关：
//! 中止要再点一次确认，本机上下了一半的目录会删掉，服务器上的要用户自己删（说明里写清楚路径）。

use fr_core::i18n::tr;
use fr_core::tf;
use std::path::Path;
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
    _task: Task<()>,
}

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
                .placeholder(tr("仓库地址，比如 git@example.com:team/project.git"))
        });
        let parent = cx.new(|cx| InputState::new(window, cx).placeholder(tr("放在哪个目录下")));
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(tr("填了地址会自动带出来")));
        // 默认目录：本机用已有仓库所在的目录，没有就用用户目录；服务器用主目录
        let mut status = None;
        let mut connect = None;
        let default_parent = match &server {
            None => app
                .read(cx)
                .settings
                .repos
                .iter()
                .rev()
                .find(|r| r.server.is_none())
                .and_then(|r| {
                    r.path
                        .rsplit_once(['\\', '/'])
                        .map(|(dir, _)| dir.to_string())
                })
                .or_else(|| std::env::var("USERPROFILE").ok()),
            Some(id) => match app.read(cx).status(id) {
                ConnStatus::Ready(conn) => Some(conn.hello.home.clone()),
                _ => {
                    // 还没连上：这里就开始连，连上后把主目录填进去
                    let connecting = app.update(cx, |app, cx| app.connect(id, cx));
                    status = Some((tr("正在连接服务器…").into(), false));
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
                                        Some((tf!("连不上服务器：{err}", err).into(), true))
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

    /// 现在填的内容会克隆到哪里；还没填全时为空。
    fn target(&self, cx: &App) -> Option<String> {
        let parent = self.parent.read(cx).value().trim().to_string();
        let name = self.name.read(cx).value().trim().to_string();
        (!parent.is_empty() && !name.is_empty())
            .then(|| join(&parent, &name, self.server.is_none()))
    }

    pub fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        let url = self.url.read(cx).value().trim().to_string();
        let parent = self.parent.read(cx).value().trim().to_string();
        let name = self.name.read(cx).value().trim().to_string();
        let problem = if url.is_empty() {
            Some(tr("填写仓库地址").to_string())
        } else if parent.is_empty() {
            Some(tr("填写放在哪个目录下").to_string())
        } else {
            name_error(&name)
        };
        let local = self.server.is_none();
        let target = join(&parent, &name, local);
        // 本机可以先看一眼：目标已经有东西就不用等 git 报错了
        let problem = problem
            .or_else(|| (local && dir_not_empty(Path::new(&target))).then(|| exists_text(&target)));
        if let Some(problem) = problem {
            self.status = Some((problem.into(), true));
            cx.notify();
            return;
        }
        let created = local && !Path::new(&target).exists();
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
            let result: Result<(), String> = async {
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
                        Some(Ok(StreamItem::Done(out))) if out.success() => return Ok(()),
                        Some(Ok(StreamItem::Done(out))) => return Err(tail(&out.stderr)),
                        Some(Err(err)) => return Err(format!("{err:#}")),
                        None => return Err(tr("连接中断").into()),
                    }
                }
            }
            .await;
            this.update_in(cx, |this, _, cx| {
                this.running = None;
                this.confirm_abort = false;
                match result {
                    Ok(()) => cx.emit(CloneEvent::Done(RepoEntry {
                        server,
                        path: done_target.clone(),
                    })),
                    Err(err) => this.status = Some((explain(&err, &done_target).into(), true)),
                }
                cx.notify();
            })
            .ok();
        });
        self.running = Some(Running {
            target,
            created,
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
            _task,
        } = running;
        // 丢掉任务：本机的 git 进程被杀掉，服务器上的请求发取消
        drop(_task);
        if self.server.is_some() {
            self.status = Some((
                tf!("克隆已中止。服务器上的 {target} 里可能留着下了一半的内容：再克隆之前先把它删掉，或者换一个文件夹名。", target)
                .into(),
                true,
            ));
        } else if created {
            self.status = Some((tr("克隆已中止，正在删除下了一半的目录…").into(), false));
            let path = target.clone();
            let remove = cx.background_executor().spawn(async move {
                // git 进程刚被杀掉，文件可能还占着，多试几次
                let mut last = None;
                for _ in 0..15 {
                    match std::fs::remove_dir_all(&path) {
                        Ok(()) => return Ok(()),
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                        Err(err) => last = Some(err),
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(last.map(|err| err.to_string()).unwrap_or_default())
            });
            cx.spawn(async move |this, cx| {
                let result = remove.await;
                this.update(cx, |this, cx| {
                    // 这期间又开始了新的克隆就不动它的进度
                    if this.running.is_none() {
                        this.status = Some(match result {
                            Ok(()) => (
                                tf!("克隆已中止，下了一半的 {target} 已经删掉。", target).into(),
                                false,
                            ),
                            Err(err) => (
                                tf!("克隆已中止，但下了一半的 {target} 没删掉（{err}）：再克隆之前先手动删掉它。", target, err)
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
                    prompt: Some(tr("克隆到这个目录下").into()),
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
                        .title(tr("克隆到哪个目录下"))
                        .w(px(640.))
                        // 回车是「前往」输入的路径，不是关掉对话框
                        .on_ok(|_, _, _| false)
                        .child(browser.clone())
                });
            }
        }
    }
}

/// 目录和文件夹名拼成完整路径。本机用反斜杠，服务器上用斜杠。
fn join(parent: &str, name: &str, local: bool) -> String {
    let separator = if local { '\\' } else { '/' };
    let trimmed = parent.trim_end_matches(['/', '\\']);
    // 根目录（`/`、`C:\`）去掉尾巴后会变空或只剩盘符，补回分隔符
    format!("{trimmed}{separator}{name}")
}

/// 文件夹名不能用时的说明。
fn name_error(name: &str) -> Option<String> {
    if name.is_empty() {
        Some(tr("填写文件夹名").into())
    } else if name == "."
        || name == ".."
        || name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
    {
        Some(
            tr("文件夹名里不能有 / \\ : * ? \" < > |：这里只填一个名字，放在哪个目录下在上面填。")
                .into(),
        )
    } else {
        None
    }
}

fn dir_not_empty(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some())
}

fn exists_text(target: &str) -> String {
    tf!(
        "{target} 已经存在，而且里面有东西。换一个文件夹名，或者先把它删掉。",
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
        Some(tr("找不到这个仓库。检查地址对不对，这个账号有没有权限。").to_string())
    } else if lower.contains("no such file or directory") || lower.contains("cannot find the path")
    {
        Some(
            tr("放仓库的目录不存在。服务器上的目录要先建好，或者点「浏览…」选一个已有的。")
                .to_string(),
        )
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
        let hint = match &self.server {
            None => tr("克隆到这台电脑上。目录不存在会新建。"),
            Some(_) => tr("克隆到服务器上，由服务器去连远程仓库；放仓库的目录要已经存在。"),
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
                        .child(tr("中止克隆？已经下载的部分不会保留。")),
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
                    .child(label(tr("地址")))
                    .child(Input::new(&self.url).disabled(busy)),
            )
            .child(
                v_flex().gap_1().child(label(tr("放在这个目录下"))).child(
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
                    .child(label(tr("文件夹名")))
                    .child(Input::new(&self.name).disabled(busy)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .text_xs()
                    .text_color(muted)
                    .when_some(target, |view, target| {
                        view.child(div().child(tf!("会克隆到：{target}", target)))
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
    use super::{CloneView, explain, join, name_error};

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
        assert_eq!(join(r"C:\work\", "app", true), r"C:\work\app");
        assert_eq!(join("/home/me", "app", false), "/home/me/app");
        assert_eq!(join("/", "app", false), "/app");
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
        assert!(text.starts_with("/home/me/app 已经存在"), "{text}");
        assert!(text.contains("destination path"), "{text}");
        // 认不出来的原样显示
        assert_eq!(explain("fatal: 奇怪的错误", "/x"), "fatal: 奇怪的错误");
    }
}
