//! 浏览服务器上的目录，选一个仓库打开。放在「打开仓库」对话框里。
//! 克隆到服务器时也用它选放仓库的目录（只选目录，不要求是仓库）。
//!
//! 输入框里显示的永远是列表正在显示的那个目录：前往一个进不去的路径时，
//! 报错里写要去的路径，输入框回到原来的目录，免得显示的和实际要打开的不是一个地方。
//!
//! 从哪开始：打开仓库时是上次打开的仓库所在的目录，没有就是这台服务器的初始目录，再没有是主目录
//! （[`crate::settings::Server::browse_starts`]）；前一个进不去（目录删了）就试下一个。
//!
//! 可以一次打开几个（2026-11-15 Feng 要的）：标有「仓库」的目录前面有勾选框，勾上的跨目录记着，
//! 一个目录里有几个仓库时可以全选；勾了以后按钮变成「打开 N 个仓库」。

use fr_channel::DirListing;
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::{AppState, ConnStatus};
use crate::open_dir::{self, Found};
use crate::settings::RepoEntry;

pub enum BrowserEvent {
    /// 打开这几个仓库（一个或勾选的几个），按选的先后
    Open(Vec<RepoEntry>),
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
    /// 一开始要去的目录进不去时，接着试的几个（最后一个是主目录）
    fallbacks: Vec<String>,
    /// 进行中的提示或错误
    status: Option<(SharedString, bool)>,
    /// 勾选的仓库（完整路径，按勾的先后）。换了目录还记着，可以从几个目录里各挑几个
    selected: Vec<String>,
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
        let starts = match app.read(cx).server(&server) {
            Some(server) => server.browse_starts(),
            None => vec![String::new()],
        };
        Self::build(app, server, starts, false, window, cx)
    }

    /// 选一个目录（克隆到服务器时选放仓库的目录）。`start` 为空从这台服务器的初始目录开始，
    /// 没设初始目录就是主目录。
    pub fn new_dir_picker(
        app: Entity<AppState>,
        server: String,
        start: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let start_dir = app
            .read(cx)
            .server(&server)
            .and_then(|server| server.start_dir.clone());
        let mut starts: Vec<String> = [Some(start), start_dir]
            .into_iter()
            .flatten()
            .filter(|dir| !dir.trim().is_empty())
            .collect();
        starts.dedup();
        starts.push(String::new());
        Self::build(app, server, starts, true, window, cx)
    }

    fn build(
        app: Entity<AppState>,
        server: String,
        starts: Vec<String>,
        pick_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut starts = starts.into_iter();
        let start = starts.next().unwrap_or_default();
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
            fallbacks: starts.collect(),
            status: None,
            selected: Vec::new(),
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
            // 报错后面带着：连上了、只是这个目录进不去（那样才值得换一个目录再试）
            let result = match channel.await {
                Ok(channel) => channel.list_dir(&path).await.map_err(|err| {
                    let shown = if path.is_empty() {
                        tr("主目录")
                    } else {
                        &path
                    };
                    let text = SharedString::from(tf!(
                        "无法进入 {shown}：{err}",
                        shown,
                        err = format!("{:#}", err)
                    ));
                    (text, true)
                }),
                Err(err) => Err((err, false)),
            };
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(listing) => {
                        this.input.update(cx, |input, cx| {
                            input.set_value(listing.path.clone(), window, cx)
                        });
                        this.listing = Some(listing);
                        this.fallbacks.clear();
                        this.status = None;
                    }
                    // 一开始要去的目录（上次打开的、初始目录）已经不在了：不报错，接着试下一个
                    Err((_, true)) if this.listing.is_none() && !this.fallbacks.is_empty() => {
                        let next = this.fallbacks.remove(0);
                        this.navigate(next, window, cx);
                        return;
                    }
                    Err((err, _)) => {
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

    /// 勾上、去掉一个仓库。
    fn pick(&mut self, path: &str, on: bool, cx: &mut Context<Self>) {
        let at = self.selected.iter().position(|p| p == path);
        match (on, at) {
            (true, None) => self.selected.push(path.to_string()),
            (false, Some(ix)) => {
                self.selected.remove(ix);
            }
            _ => {}
        }
        cx.notify();
    }

    /// 当前目录里标有「仓库」的子目录（完整路径，按列表的顺序）。
    fn repos_here(&self) -> Vec<String> {
        let Some(listing) = &self.listing else {
            return Vec::new();
        };
        listing
            .entries
            .iter()
            .filter(|entry| entry.repo)
            .map(|entry| join(&listing.path, &entry.name))
            .collect()
    }

    /// 用 git 确认当前目录在仓库中，取仓库根目录，再通知打开；不是仓库先问是否初始化。
    /// 勾了仓库就打开勾的那几个。
    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected.is_empty() {
            return self.open_picked(window, cx);
        }
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
                        this.remember(&root, cx);
                        cx.emit(BrowserEvent::Open(vec![RepoEntry {
                            server: Some(server),
                            path: root,
                        }]));
                    }
                    Ok((channel, Found::NotRepo)) => {
                        this.status = None;
                        open_dir::ask_init(
                            channel,
                            Some(server),
                            path,
                            |this: &mut Self, entry, _, cx| {
                                this.remember(&entry.path, cx);
                                cx.emit(BrowserEvent::Open(vec![entry]))
                            },
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

    /// 打开勾选的几个仓库。每个照样用 git 取仓库根目录：和一个一个打开时记下的路径写法一致，
    /// 不会因为符号链接之类的同一个仓库在列表里出现两次。打不开的在右下角报，其余的照常打开；
    /// 一个都打不开就留在对话框里说明。
    fn open_picked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.selected.clone();
        let server = self.server.clone();
        let channel = self
            .app
            .update(cx, |app, cx| app.channel_for(Some(&server), cx));
        self.busy = true;
        self.status = Some((tr("正在检查…").into(), false));
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut entries: Vec<RepoEntry> = Vec::new();
            let mut errors: Vec<String> = Vec::new();
            match channel.await {
                Ok(channel) => {
                    for path in &paths {
                        match open_dir::find(&channel, path).await {
                            Ok(Found::Repo(root)) => {
                                let entry = RepoEntry {
                                    server: Some(server.clone()),
                                    path: root,
                                };
                                if !entries.contains(&entry) {
                                    entries.push(entry);
                                }
                            }
                            Ok(Found::NotRepo) => {
                                errors.push(tf!("{dir} 不是 Git 仓库", dir = path))
                            }
                            Err(err) => errors.push(err),
                        }
                    }
                }
                Err(err) => errors.push(err.to_string()),
            }
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                if entries.is_empty() {
                    this.status = Some((errors.join("\n").into(), true));
                } else {
                    this.status = None;
                    for err in errors {
                        window.push_notification(Notification::error(err), cx);
                    }
                    if let Some(last) = entries.last() {
                        this.remember(&last.path, cx);
                    }
                    this.selected.clear();
                    cx.emit(BrowserEvent::Open(entries));
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

impl RemoteBrowser {
    /// 记下打开的仓库所在的目录：下次「打开仓库」从这里开始，旁边的仓库就在眼前。
    fn remember(&self, repo: &str, cx: &mut Context<Self>) {
        let dir = parent_dir(repo).unwrap_or_else(|| repo.to_string());
        self.app
            .update(cx, |app, cx| app.set_browse_dir(&self.server, dir, cx));
    }
}

/// 服务器上一个路径的上一级目录；已经是根目录（`/`、`C:/`）就没有。两种斜杠都认。
fn parent_dir(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let cut = trimmed.rfind(['/', '\\'])?;
    let parent = &trimmed[..cut];
    // `/data` 的上一级是 `/`，`C:/data` 的上一级是 `C:/`
    Some(if parent.is_empty() || parent.ends_with(':') {
        trimmed[..=cut].to_string()
    } else {
        parent.to_string()
    })
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
        // 打开仓库时，标有「仓库」的目录前面有勾选框；其他行留出同样宽的一格，目录名对齐
        let pickable = !self.pick_dir;
        let slot = || div().w(px(16.)).flex_shrink_0();

        let mut rows: Vec<AnyElement> = Vec::new();
        if let Some(listing) = &self.listing {
            if let Some(parent) = listing.parent.clone() {
                rows.push(
                    h_flex()
                        .id("parent")
                        .px_3()
                        .py_1()
                        .gap_2()
                        .cursor_pointer()
                        .hover(|s| s.bg(hover))
                        .text_color(muted)
                        .when(pickable, |row| row.child(slot()))
                        .child(tr("..（上一级）"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.navigate(parent.clone(), window, cx)
                        }))
                        .into_any_element(),
                );
            }
            for (ix, entry) in listing.entries.iter().enumerate() {
                let target = join(&listing.path, &entry.name);
                let checkbox = (pickable && entry.repo).then(|| {
                    let path = target.clone();
                    Checkbox::new(("check", ix))
                        .checked(self.selected.contains(&target))
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            this.pick(&path, *checked, cx)
                        }))
                });
                // 勾选框和点了进目录的那一块是并排的两块：点勾选框不会进目录
                rows.push(
                    h_flex()
                        .pl_3()
                        .gap_2()
                        .hover(|s| s.bg(hover))
                        .when(pickable, |row| row.child(slot().children(checkbox)))
                        .child(
                            h_flex()
                                .id(("dir", ix))
                                .flex_1()
                                .min_w_0()
                                .pr_3()
                                .py_1()
                                .gap_2()
                                .cursor_pointer()
                                .child(
                                    crate::ui::clipped("name", format!("{}/", entry.name)).flex_1(),
                                )
                                .when(entry.repo, |row| {
                                    row.child(
                                        div().text_xs().text_color(repo_color).child(tr("仓库")),
                                    )
                                })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.navigate(target.clone(), window, cx)
                                })),
                        )
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
        let picked = self.selected.len();
        let hint: SharedString = match (&self.status, is_repo) {
            (Some((text, _)), _) => text.clone(),
            (None, _) if pick_dir => tr("点击目录进入；仓库将克隆到所选目录下").into(),
            // 勾的可能在别的目录里，列出名字免得忘了
            (None, _) if picked > 0 => {
                let names = self
                    .selected
                    .iter()
                    .map(|path| crate::settings::base_name(path))
                    .collect::<Vec<_>>()
                    .join("、");
                tf!("已选 {n} 个仓库：{names}", n = picked, names).into()
            }
            (None, true) => tr("此目录是仓库，可以打开").into(),
            (None, false) => tr("点击目录进入；勾选标有「仓库」的目录可一次打开多个").into(),
        };
        // 当前目录里有两个以上仓库时可以全选
        let repos_here = if pickable {
            self.repos_here()
        } else {
            Vec::new()
        };
        let all_here = repos_here.iter().all(|path| self.selected.contains(path));
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
                // 报错可能很长（ssh 说主机密钥变了时有十几行）：限高、可滚动，不把下面的按钮挤出窗口
                div()
                    .id("hint")
                    .max_h(px(72.))
                    .overflow_y_scroll()
                    .text_xs()
                    .whitespace_normal()
                    .text_color(hint_color)
                    .child(hint),
            )
            .child(
                h_flex()
                    .gap_2()
                    .when(repos_here.len() >= 2, |bar| {
                        bar.child(
                            Checkbox::new("check-all")
                                .label(tr("全选此目录中的仓库"))
                                .checked(all_here)
                                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                    for path in &repos_here {
                                        this.pick(path, *checked, cx);
                                    }
                                })),
                        )
                    })
                    .when(picked > 0, |bar| {
                        bar.child(
                            Button::new("clear-picked")
                                .ghost()
                                .label(tr("清除选择"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.selected.clear();
                                    cx.notify();
                                })),
                        )
                    })
                    .child(div().flex_1())
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
                        // 确实不在仓库里就在下面说明。勾了仓库就打开勾的那几个
                        let label: SharedString = if picked > 0 {
                            tf!("打开 {n} 个仓库", n = picked).into()
                        } else {
                            tr("打开此仓库").into()
                        };
                        Button::new("open")
                            .primary()
                            .label(label)
                            .disabled((!has_listing && picked == 0) || self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.open(window, cx)))
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::parent_dir;

    #[test]
    fn 上一级目录() {
        let parent = |path: &str| parent_dir(path);
        assert_eq!(
            parent("/data/workspaces/platform/gateway").as_deref(),
            Some("/data/workspaces/platform")
        );
        assert_eq!(parent("/data/").as_deref(), Some("/"));
        assert_eq!(parent("/"), None);
        // Windows 服务器：两种斜杠都有
        assert_eq!(
            parent("C:/Users/me/src/wms").as_deref(),
            Some("C:/Users/me/src")
        );
        assert_eq!(parent(r"C:\Users\me").as_deref(), Some(r"C:\Users"));
        assert_eq!(parent("C:/repo").as_deref(), Some("C:/"));
        assert_eq!(parent("C:/"), None);
    }
}
