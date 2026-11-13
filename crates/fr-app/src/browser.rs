//! 浏览服务器上的目录，选一个仓库打开。放在「打开仓库」对话框里。
//! 克隆到服务器时也用它选放仓库的目录（只选目录，不要求是仓库）。
//!
//! 输入框里显示的永远是列表正在显示的那个目录：前往一个进不去的路径时，
//! 报错里写要去的路径，输入框回到原来的目录，免得显示的和实际要打开的不是一个地方。

use fr_channel::DirListing;
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::{AppState, ConnStatus};
use crate::open_dir::{self, Found};
use crate::settings::RepoEntry;

pub enum BrowserEvent {
    Open(RepoEntry),
    /// 选目录的用法：选中了这个目录
    Dir(String),
}

pub struct RemoteBrowser {
    app: Entity<AppState>,
    server: String,
    /// 只选目录，不打开仓库
    pick_dir: bool,
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
        // 从主目录开始
        Self::build(app, server, String::new(), false, window, cx)
    }

    /// 选一个目录（克隆到服务器时选放仓库的目录）。`start` 为空从主目录开始。
    pub fn new_dir_picker(
        app: Entity<AppState>,
        server: String,
        start: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(app, server, start, true, window, cx)
    }

    fn build(
        app: Entity<AppState>,
        server: String,
        start: String,
        pick_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("服务器上的路径，按 Enter 前往")));
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
            pick_dir,
            input,
            listing: None,
            status: None,
            busy: false,
            task: None,
            _subscription: subscription,
        };
        this.navigate(start, window, cx);
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
                tr("正在读取…")
            } else {
                tr("正在连接服务器…")
            }
            .into(),
            false,
        ));
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match channel.await {
                Ok(channel) => channel.list_dir(&path).await.map_err(|err| {
                    let shown = if path.is_empty() {
                        tr("主目录")
                    } else {
                        &path
                    };
                    SharedString::from(tf!(
                        "无法进入 {shown}：{err}",
                        shown,
                        err = format!("{:#}", err)
                    ))
                }),
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
                    Err(err) => {
                        // 输入框回到列表正在显示的目录：下面的按钮操作的是它，不是没进去的那个
                        if let Some(current) = this.listing.as_ref().map(|l| l.path.clone()) {
                            this.input
                                .update(cx, |input, cx| input.set_value(current, window, cx));
                        }
                        this.status = Some((err, true));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// 用 git 确认当前目录在仓库中，取仓库根目录，再通知打开；不是仓库先问是否初始化。
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
        self.status = Some((tr("正在检查…").into(), false));
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match channel.await {
                Ok(channel) => open_dir::find(&channel, &path)
                    .await
                    .map(|found| (channel, found))
                    .map_err(SharedString::from),
                Err(err) => Err(err),
            };
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok((_, Found::Repo(root))) => {
                        this.status = None;
                        cx.emit(BrowserEvent::Open(RepoEntry {
                            server: Some(server),
                            path: root,
                        }));
                    }
                    Ok((channel, Found::NotRepo)) => {
                        this.status = None;
                        open_dir::ask_init(
                            channel,
                            Some(server),
                            path,
                            |_: &mut Self, entry, _, cx| cx.emit(BrowserEvent::Open(entry)),
                            window,
                            cx,
                        );
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 目录列表的高度跟着窗口走：窗口矮的时候对话框不能比窗口还高
        let list_height = (window.viewport_size().height - px(290.)).clamp(px(140.), px(340.));
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
                        .child(tr("..（上一级）"))
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
                        .child(crate::ui::clipped("name", format!("{}/", entry.name)).flex_1())
                        .when(entry.repo, |row| {
                            row.child(div().text_xs().text_color(repo_color).child(tr("仓库")))
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
                        .child(tr("没有子目录"))
                        .into_any_element(),
                );
            }
        }

        let is_repo = self.listing.as_ref().is_some_and(|l| l.repo);
        let has_listing = self.listing.is_some();
        let pick_dir = self.pick_dir;
        let hint: SharedString = match (&self.status, is_repo) {
            (Some((text, _)), _) => text.clone(),
            (None, _) if pick_dir => tr("点击目录进入；仓库将克隆到所选目录下").into(),
            (None, true) => tr("此目录是仓库，可以打开").into(),
            (None, false) => tr("点击目录进入；标有「仓库」的目录可直接打开").into(),
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
                    .child(Button::new("go").label(tr("前往")).on_click(cx.listener(
                        |this, _, window, cx| {
                            let path = this.input.read(cx).value().to_string();
                            this.navigate(path, window, cx);
                        },
                    ))),
            )
            .child(
                div()
                    .id("dirs")
                    .h(list_height)
                    .overflow_y_scroll()
                    .border_1()
                    .border_color(border)
                    .rounded_md()
                    .text_sm()
                    .child(v_flex().py_1().children(rows)),
            )
            .child(
                div()
                    .text_xs()
                    .whitespace_normal()
                    .text_color(hint_color)
                    .child(hint),
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
                    .child(if pick_dir {
                        Button::new("pick")
                            .primary()
                            .label(tr("克隆到此目录"))
                            .disabled(!has_listing || self.busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(listing) = &this.listing {
                                    cx.emit(BrowserEvent::Dir(listing.path.clone()));
                                }
                            }))
                    } else {
                        // 不是仓库的目录也能点：仓库里面的子目录同样能认出仓库根，
                        // 确实不在仓库里就在下面说明
                        Button::new("open")
                            .primary()
                            .label(tr("打开此仓库"))
                            .disabled(!has_listing || self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.open(window, cx)))
                    }),
            )
    }
}
