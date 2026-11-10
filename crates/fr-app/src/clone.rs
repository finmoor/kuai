//! 克隆仓库：填地址和放在哪个目录下，克隆到本机或者某台服务器上。放在对话框里。
//!
//! 克隆在目标那边执行（服务器上的克隆由服务器去连远程），进度实时显示；
//! 要密码时走统一的密码弹窗。关掉对话框就中止。

use fr_channel::StreamItem;
use futures::StreamExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::{AppState, ConnStatus};
use crate::settings::RepoEntry;

pub enum CloneEvent {
    Done(RepoEntry),
}

pub struct CloneView {
    app: Entity<AppState>,
    /// 克隆到哪台服务器；空表示本机
    server: Option<String>,
    url: Entity<InputState>,
    parent: Entity<InputState>,
    /// 进度或错误
    status: Option<(SharedString, bool)>,
    task: Option<Task<()>>,
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
                .placeholder("仓库地址，比如 git@example.com:team/project.git")
        });
        let parent = cx.new(|cx| InputState::new(window, cx).placeholder("放在哪个目录下"));
        // 默认目录：本机用已有仓库所在的目录，没有就用用户目录；服务器用主目录
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
                _ => None,
            },
        };
        if let Some(dir) = default_parent {
            parent.update(cx, |state, cx| state.set_value(dir, window, cx));
        }
        Self {
            app,
            server,
            url,
            parent,
            status: None,
            task: None,
        }
    }

    /// 把键盘焦点放到地址输入框。对话框打开后调用：打开的那一刻焦点会被对话框拿走。
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.url.focus_handle(cx).focus(window, cx);
    }

    /// 从地址里取仓库名：最后一段，去掉 `.git`。
    fn repo_name(url: &str) -> Option<String> {
        let trimmed = url.trim().trim_end_matches(['/', '\\']);
        let last = trimmed.rsplit(['/', '\\', ':']).next()?;
        let name = last.strip_suffix(".git").unwrap_or(last);
        (!name.is_empty()).then(|| name.to_string())
    }

    pub fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.task.is_some() {
            return;
        }
        let url = self.url.read(cx).value().trim().to_string();
        let parent = self.parent.read(cx).value().trim().to_string();
        let Some(name) = Self::repo_name(&url) else {
            self.status = Some(("填写仓库地址".into(), true));
            cx.notify();
            return;
        };
        if parent.is_empty() {
            self.status = Some(("填写放在哪个目录下".into(), true));
            cx.notify();
            return;
        }
        let local = self.server.is_none();
        let separator = if local { '\\' } else { '/' };
        let target = format!("{}{separator}{name}", parent.trim_end_matches(['/', '\\']));
        if local {
            // 本机的目录不存在就建出来；服务器上的要先有
            let _ = std::fs::create_dir_all(&parent);
        }
        let channel = self
            .app
            .update(cx, |app, cx| app.channel_for(self.server.as_deref(), cx));
        let server = self.server.clone();
        self.status = Some(("正在连接…".into(), false));
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
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
                            .map_err(|_| "已取消".to_string())?;
                        }
                        Some(Ok(StreamItem::Done(out))) if out.success() => return Ok(()),
                        Some(Ok(StreamItem::Done(out))) => return Err(tail(&out.stderr)),
                        Some(Err(err)) => return Err(format!("{err:#}")),
                        None => return Err("连接中断".into()),
                    }
                }
            }
            .await;
            this.update_in(cx, |this, _, cx| {
                this.task = None;
                match result {
                    Ok(()) => cx.emit(CloneEvent::Done(RepoEntry {
                        server,
                        path: target,
                    })),
                    Err(err) => this.status = Some((err.into(), true)),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
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
        let busy = self.task.is_some();
        let field = |label: &'static str, input: Input| {
            v_flex()
                .gap_1()
                .child(div().text_xs().text_color(muted).child(label))
                .child(input)
        };
        let hint = match &self.server {
            None => "克隆到这台电脑上。目录不存在会新建。",
            Some(_) => "克隆到服务器上，由服务器去连远程仓库；目录要已经存在。",
        };
        v_flex()
            .gap_3()
            .child(field("地址", Input::new(&self.url)))
            .child(field("放在这个目录下", Input::new(&self.parent)))
            .child(div().text_xs().text_color(muted).child(hint))
            .when_some(self.status.clone(), |view, (text, error)| {
                view.child(
                    div()
                        .text_sm()
                        .text_color(if error { danger } else { muted })
                        .child(text),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel")
                            .label(if busy { "中止" } else { "取消" })
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("clone")
                            .primary()
                            .label("克隆")
                            .loading(busy)
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| this.start(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::CloneView;

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
}
