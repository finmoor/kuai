//! 浏览服务器上的目录，选一个仓库打开。放在「打开仓库」对话框里。

use fr_channel::DirListing;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::{AppState, ConnStatus};
use crate::settings::RepoEntry;

pub enum BrowserEvent {
    Open(RepoEntry),
}

pub struct RemoteBrowser {
    app: Entity<AppState>,
    server: String,
    input: Entity<InputState>,
    listing: Option<DirListing>,
    /// 进行中的提示或错误
    status: Option<(SharedString, bool)>,
    busy: bool,
    task: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<BrowserEvent> for RemoteBrowser {}

impl RemoteBrowser {
    pub fn new(
        app: Entity<AppState>,
        server: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("服务器上的路径，回车前往"));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let path = input.read(cx).value().to_string();
                    this.navigate(path, window, cx);
                }
            },
        );
        let mut this = Self {
            app,
            server,
            input,
            listing: None,
            status: None,
            busy: false,
            task: None,
            _subscription: subscription,
        };
        // 从主目录开始
        this.navigate(String::new(), window, cx);
        this
    }

    fn navigate(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let connected = matches!(self.app.read(cx).status(&self.server), ConnStatus::Ready(_));
        let channel = self
            .app
            .update(cx, |app, cx| app.channel_for(Some(&self.server), cx));
        self.busy = true;
        self.status = Some((
            if connected {
                "正在读取…"
            } else {
                "正在连接服务器…"
            }
            .into(),
            false,
        ));
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match channel.await {
                Ok(channel) => channel
                    .list_dir(&path)
                    .await
                    .map_err(|err| SharedString::from(format!("{err:#}"))),
                Err(err) => Err(err),
            };
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(listing) => {
                        this.input.update(cx, |input, cx| {
                            input.set_value(listing.path.clone(), window, cx)
                        });
                        this.listing = Some(listing);
                        this.status = None;
                    }
                    Err(err) => this.status = Some((err, true)),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// 用 git 确认当前目录在仓库里，取仓库根目录，再通知打开。
    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(listing) = &self.listing else {
            return;
        };
        let path = listing.path.clone();
        let server = self.server.clone();
        let channel = self
            .app
            .update(cx, |app, cx| app.channel_for(Some(&server), cx));
        self.busy = true;
        self.status = Some(("正在检查…".into(), false));
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match channel.await {
                Ok(channel) => channel
                    .git(&path, vec!["rev-parse".into(), "--show-toplevel".into()])
                    .await
                    .and_then(|out| out.ok())
                    .map_err(|err| SharedString::from(format!("这里不是 git 仓库：{err:#}"))),
                Err(err) => Err(err),
            };
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(out) => {
                        this.status = None;
                        let path = String::from_utf8_lossy(&out).trim().to_string();
                        cx.emit(BrowserEvent::Open(RepoEntry {
                            server: Some(server),
                            path,
                        }));
                    }
                    Err(err) => this.status = Some((err, true)),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

impl Render for RemoteBrowser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted, hover, danger) = (
            theme.border,
            theme.muted_foreground,
            theme.list_hover,
            theme.danger,
        );
        let repo_color: Hsla = rgb(0x27ae60).into();

        let mut rows: Vec<AnyElement> = Vec::new();
        if let Some(listing) = &self.listing {
            if let Some(parent) = listing.parent.clone() {
                rows.push(
                    h_flex()
                        .id("parent")
                        .px_3()
                        .py_1()
                        .cursor_pointer()
                        .hover(|s| s.bg(hover))
                        .text_color(muted)
                        .child("..（上一级）")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.navigate(parent.clone(), window, cx)
                        }))
                        .into_any_element(),
                );
            }
            for (ix, entry) in listing.entries.iter().enumerate() {
                let target = join(&listing.path, &entry.name);
                rows.push(
                    h_flex()
                        .id(("dir", ix))
                        .px_3()
                        .py_1()
                        .gap_2()
                        .cursor_pointer()
                        .hover(|s| s.bg(hover))
                        .child(div().flex_1().truncate().child(format!("{}/", entry.name)))
                        .when(entry.repo, |row| {
                            row.child(div().text_xs().text_color(repo_color).child("仓库"))
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.navigate(target.clone(), window, cx)
                        }))
                        .into_any_element(),
                );
            }
            if listing.entries.is_empty() {
                rows.push(
                    div()
                        .px_3()
                        .py_2()
                        .text_color(muted)
                        .child("没有子目录")
                        .into_any_element(),
                );
            }
        }

        let is_repo = self.listing.as_ref().is_some_and(|l| l.repo);
        let hint: SharedString = match (&self.status, is_repo) {
            (Some((text, _)), _) => text.clone(),
            (None, true) => "这个目录是仓库，可以打开".into(),
            (None, false) => "点目录进去；标着「仓库」的目录可以打开".into(),
        };
        let hint_color = match &self.status {
            Some((_, true)) => danger,
            _ => muted,
        };

        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.input)))
                    .child(Button::new("go").label("前往").on_click(cx.listener(
                        |this, _, window, cx| {
                            let path = this.input.read(cx).value().to_string();
                            this.navigate(path, window, cx);
                        },
                    ))),
            )
            .child(
                div()
                    .id("dirs")
                    .h(px(340.))
                    .overflow_y_scroll()
                    .border_1()
                    .border_color(border)
                    .rounded_md()
                    .text_sm()
                    .child(v_flex().py_1().children(rows)),
            )
            .child(div().text_xs().text_color(hint_color).child(hint))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel")
                            .label("取消")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("open")
                            .primary()
                            .label("打开这个仓库")
                            .disabled(!is_repo || self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.open(window, cx))),
                    ),
            )
    }
}
