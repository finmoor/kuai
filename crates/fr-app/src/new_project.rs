//! 新建项目（`docs/design/first-run/` 第 2 节）：没有现成代码的人也有路。在主机（或本机）上
//! 新建一个文件夹，初始化成 Git 仓库，加进列表并打开。
//!
//! 填项目名称和位置（默认主目录，可以「浏览…」），总是在位置下新建一个子文件夹：不会把主目录本身
//! 初始化成仓库。名字重了、位置不存在都在对话框里说，不建半截。

use fr_channel::Channel;
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::{AppState, ConnStatus};
use crate::browser::{BrowserEvent, RemoteBrowser};
use crate::clone::{drive_root, expand_home, join};
use crate::open_dir::{self, Found};
use crate::settings::RepoEntry;

pub enum NewProjectEvent {
    /// 建好了，打开这个仓库
    Done(RepoEntry),
}

pub struct NewProjectView {
    app: Entity<AppState>,
    /// 建在哪台主机上；空是本机
    server: Option<String>,
    name: Entity<InputState>,
    parent: Entity<InputState>,
    /// 进度或错误
    status: Option<(SharedString, bool)>,
    busy: bool,
    _connect: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<NewProjectEvent> for NewProjectView {}

impl NewProjectView {
    pub fn new(
        app: Entity<AppState>,
        server: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(tr("例如 my-app")));
        let parent = cx.new(|cx| InputState::new(window, cx).placeholder(tr("位置")));
        let mut status = None;
        let mut connect = None;
        let home = match &server {
            None => Some(fr_core::dir::home()),
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
        if let Some(home) = home {
            parent.update(cx, |state, cx| state.set_value(home, window, cx));
        }
        // 改了名字、位置：预览跟着变，原来的错误不再作数
        let refresh = |this: &mut Self,
                       _: &Entity<InputState>,
                       event: &InputEvent,
                       _: &mut Window,
                       cx: &mut Context<Self>| {
            if matches!(event, InputEvent::Change) && !this.busy {
                if this.status.as_ref().is_some_and(|(_, error)| *error) {
                    this.status = None;
                }
                cx.notify();
            }
        };
        let subscriptions = vec![
            cx.subscribe_in(&name, window, refresh),
            cx.subscribe_in(&parent, window, refresh),
        ];
        Self {
            app,
            server,
            name,
            parent,
            status,
            busy: false,
            _connect: connect,
            _subscriptions: subscriptions,
        }
    }

    /// 键盘焦点放到名称上（对话框打开以后调）。
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.name.focus_handle(cx).focus(window, cx);
    }

    /// 位置换成 git、kuaid 认得的写法：本机只写了盘符的补成盘的根目录，主机上的 `~` 换成主目录。
    fn parent_dir(&self, cx: &App) -> String {
        let parent = self.parent.read(cx).value().trim().to_string();
        match &self.server {
            None => drive_root(&parent),
            Some(id) => match self.app.read(cx).status(id) {
                ConnStatus::Ready(conn) => expand_home(&parent, &conn.hello.home),
                _ => parent,
            },
        }
    }

    /// 名字有什么不对（空的不算：还没填）；没问题是 None。
    fn name_problem(name: &str) -> Option<&'static str> {
        if name == "." || name == ".." {
            return Some(tr("名称不能是 . 或 .."));
        }
        if name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']) {
            return Some(tr("名称中不能有斜杠、冒号、问号、引号等特殊字符"));
        }
        None
    }

    /// 会建在哪里；还没填全、名字不对时是 None。
    fn target(&self, cx: &App) -> Option<String> {
        let parent = self.parent_dir(cx);
        let name = self.name.read(cx).value().trim().to_string();
        let ok = !parent.is_empty() && !name.is_empty() && Self::name_problem(&name).is_none();
        ok.then(|| join(&parent, &name, self.server.is_none()))
    }

    /// 建文件夹、`git init`，再取仓库的根目录（和打开仓库记下的写法一致）交出去。
    pub fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(target) = self.target(cx) else {
            return;
        };
        let (parent, name) = (
            self.parent_dir(cx),
            self.name.read(cx).value().trim().to_string(),
        );
        let server = self.server.clone();
        let channel = self
            .app
            .update(cx, |app, cx| app.channel_for(server.as_deref(), cx));
        self.busy = true;
        self.status = Some((tr("正在创建…").into(), false));
        cx.spawn_in(window, async move |this, cx| {
            let result = match channel.await {
                Ok(channel) => make(&channel, &parent, &name, &target).await,
                Err(err) => Err(err.to_string()),
            };
            this.update_in(cx, |this, _, cx| {
                this.busy = false;
                match result {
                    Ok(root) => {
                        this.status = None;
                        cx.emit(NewProjectEvent::Done(open_dir::entry(server, &root)));
                    }
                    Err(err) => this.status = Some((err.into(), true)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// 选位置：本机用系统的选文件夹，主机上用目录浏览器。
    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.server.clone() {
            None => {
                let picked = cx.prompt_for_paths(PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some(tr("选择位置").into()),
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
                        // 关掉的是叠在上面的目录浏览器，新建项目的对话框还在
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
                        .title(tr("选择位置"))
                        .w(px(640.))
                        .on_ok(|_, _, _| false)
                        .child(browser.clone())
                });
            }
        }
    }
}

/// 在 `parent` 下建 `name`，初始化成仓库，返回仓库根目录（git 给的写法）。位置得先在；名字重了不建。
async fn make(channel: &Channel, parent: &str, name: &str, target: &str) -> Result<String, String> {
    channel
        .list_dir(parent)
        .await
        .map_err(|err| tf!("位置不存在或无法访问：{err}", err = format!("{err:#}")))?;
    if let Ok(listing) = channel.list_dir_with(parent, true).await
        && listing
            .entries
            .iter()
            .any(|entry| entry.name.eq_ignore_ascii_case(name))
    {
        return Err(tf!("「{name}」已存在，请换一个名称", name));
    }
    channel
        .fs_mkdir(parent, name)
        .await
        .map_err(|err| tf!("无法创建文件夹：{err}", err = format!("{err:#}")))?;
    let failed = |err: &dyn std::fmt::Display| tf!("初始化仓库失败：{err}", err);
    channel
        .git(target, vec!["init".to_string()])
        .await
        .and_then(|out| out.ok())
        .map_err(|err| failed(&format!("{err:#}")))?;
    match open_dir::find(channel, target).await? {
        Found::Repo(root) => Ok(root),
        Found::NotRepo => Err(failed(&target)),
    }
}

impl Render for NewProjectView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let label = move |text: &'static str| div().text_xs().text_color(muted).child(text);
        let name = self.name.read(cx).value().trim().to_string();
        let target = self.target(cx);
        // 下面那一行：错误，进度，名字不对，将创建在哪
        let (line, error): (SharedString, bool) = match (&self.status, Self::name_problem(&name)) {
            (Some((text, error)), _) => (text.clone(), *error),
            (None, Some(problem)) => (problem.into(), true),
            (None, None) => match &target {
                Some(target) => (
                    tf!("将创建 {target}", target = target.clone()).into(),
                    false,
                ),
                None => (tr("填写项目名称后创建。").into(), false),
            },
        };
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(label(tr("项目名称")))
                    .child(Input::new(&self.name).disabled(self.busy)),
            )
            .child(
                v_flex().gap_1().child(label(tr("位置"))).child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .child(Input::new(&self.parent).disabled(self.busy)),
                        )
                        .child(
                            Button::new("browse")
                                .label(tr("浏览…"))
                                .disabled(self.busy)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.browse(window, cx)),
                                ),
                        ),
                ),
            )
            .child(
                div()
                    .text_xs()
                    .whitespace_normal()
                    .text_color(if error { danger } else { muted })
                    .child(line),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(tr("将在位置下新建同名文件夹，并初始化为 Git 仓库。")),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel")
                            .label(tr("取消"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("create")
                            .primary()
                            .label(tr("创建并打开"))
                            .loading(self.busy)
                            .disabled(self.busy || target.is_none())
                            .on_click(cx.listener(|this, _, window, cx| this.create(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::NewProjectView;

    #[test]
    fn 项目名称里不能有路径的字符() {
        assert!(NewProjectView::name_problem("my-app").is_none());
        assert!(NewProjectView::name_problem("中文项目").is_none());
        assert!(NewProjectView::name_problem("a/b").is_some());
        assert!(NewProjectView::name_problem("a\\b").is_some());
        assert!(NewProjectView::name_problem("..").is_some());
        assert!(NewProjectView::name_problem("x:y").is_some());
    }
}
