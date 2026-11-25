//! 浏览服务器上的目录，选一个仓库打开。放在「打开仓库」对话框里。
//! 克隆到服务器时也用它选放仓库的目录（只选目录，不要求是仓库）。
//!
//! 前往一个进不去的路径时，报错里写要去的路径，输入框回到列表正在显示的目录。
//! 在输入框里改了路径、没按 Enter 就点「打开此仓库」时，打开的是输入框里的路径，不是列表
//! 正在显示的目录：按钮操作的必须是用户眼前写着的那个地方。
//!
//! 从哪开始：打开仓库时是上次打开的仓库所在的目录，没有就是这台服务器的初始目录，再没有是主目录
//! （[`crate::settings::Server::browse_starts`]）；前一个进不去（目录删了）就试下一个。
//!
//! 可以一次打开几个：标有「仓库」的目录前面有勾选框，勾上的跨目录记着，
//! 一个目录里有几个仓库时可以全选；勾了以后按钮变成「打开 N 个仓库」。
//!
//! 列表下面一行按当前目录说明能做什么：是仓库；在仓库「x」里（打开它的
//! 根目录）；不是仓库（主按钮置灰，请进入含有仓库的目录或新建文件夹）。不是仓库的普通目录（主目录
//! 除外）有「初始化仓库…」，确认框里「取消」是主按钮：进错目录的人按回车只会退回，不会建出 `.git`。
//! 路径栏旁有「新建文件夹」；左下「显示隐藏目录」记在设置里（`.claude/worktrees/` 下的工作树这样才
//! 看得到）。

use fr_channel::DirListing;
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::{AppState, ConnStatus};
use crate::open_dir::{self, Found};
use crate::settings::RepoEntry;
use crate::ui;

/// 前往输入框里的目录之后接着做的事（见 [`RemoteBrowser::press_main`]）。
#[derive(Clone, Copy)]
enum AfterNavigate {
    /// 选中这个目录（克隆到此目录）
    Pick,
    /// 打开这个目录所在的仓库
    Open,
}

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
    /// 输入框里改了路径、没按 Enter 就点了下面的主按钮：先前往那个目录，进得去再接着做
    after_navigate: Option<AfterNavigate>,
    /// 也列以 `.` 开头的目录（设置里记着）
    show_hidden: bool,
    /// 正在新建文件夹：列表顶上一行输入框
    creating: Option<(Entity<InputState>, Subscription)>,
    /// 进了目录以后下面那一行写的（「已创建「x」」）
    notice: Option<SharedString>,
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
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("主机上的路径，按 Enter 前往")));
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
        let show_hidden = app.read(cx).settings.browse_hidden;
        let mut this = Self {
            app,
            server,
            pick_dir,
            input,
            listing: None,
            fallbacks: starts.collect(),
            status: None,
            selected: Vec::new(),
            after_navigate: None,
            show_hidden,
            creating: None,
            notice: None,
            busy: false,
            task: None,
            _subscription: subscription,
        };
        this.navigate(start, window, cx);
        this
    }

    fn navigate(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.creating = None;
        let hidden = self.show_hidden;
        let connected = matches!(self.app.read(cx).status(&self.server), ConnStatus::Ready(_));
        let channel = self
            .app
            .update(cx, |app, cx| app.channel_for(Some(&self.server), cx));
        self.busy = true;
        self.status = Some((
            if connected {
                tr("正在读取…")
            } else {
                tr("正在连接主机…")
            }
            .into(),
            false,
        ));
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            // 报错后面带着：连上了、只是这个目录进不去（那样才值得换一个目录再试）
            let result = match channel.await {
                Ok(channel) => channel.list_dir_with(&path, hidden).await.map_err(|err| {
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
                        let path = listing.path.clone();
                        this.listing = Some(listing);
                        this.fallbacks.clear();
                        this.status = this.notice.take().map(|text| (text, false));
                        match this.after_navigate.take() {
                            Some(AfterNavigate::Pick) => cx.emit(BrowserEvent::Dir(path)),
                            Some(AfterNavigate::Open) => this.open(window, cx),
                            None => {}
                        }
                    }
                    // 一开始要去的目录（上次打开的、初始目录）已经不在了：不报错，接着试下一个
                    Err((_, true)) if this.listing.is_none() && !this.fallbacks.is_empty() => {
                        let next = this.fallbacks.remove(0);
                        this.navigate(next, window, cx);
                        return;
                    }
                    Err((err, _)) => {
                        this.after_navigate = None;
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

    /// 主按钮要操作的目录：输入框里改过路径（还没按 Enter）就是输入框里的，否则是列表正在显示的。
    fn target(&self, cx: &App) -> Option<String> {
        let typed = self.input.read(cx).value().trim().to_string();
        let shown = self.listing.as_ref().map(|listing| listing.path.as_str());
        match shown {
            Some(shown) if typed.is_empty() || same_dir(&typed, shown) => Some(shown.to_string()),
            _ if !typed.is_empty() => Some(typed),
            _ => None,
        }
    }

    /// 主按钮：目录就是列表正在显示的那个，直接做；输入框里改了路径，先前往（路径按主机上的写法
    /// 解析好、进不去照常报错），进得去再做。
    fn press_main(&mut self, then: AfterNavigate, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.target(cx) else {
            return;
        };
        match (&self.listing, then) {
            (Some(listing), AfterNavigate::Pick) if listing.path == target => {
                cx.emit(BrowserEvent::Dir(target))
            }
            (Some(listing), AfterNavigate::Open) if listing.path == target => self.open(window, cx),
            _ => {
                self.after_navigate = Some(then);
                self.navigate(target, window, cx);
            }
        }
    }

    /// 用 git 确认列表正在显示的目录在仓库中，取仓库根目录，再通知打开；不是仓库先问是否初始化。
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
            this.update_in(cx, |this, _, cx| {
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
                    Ok((_, Found::NotRepo)) => {
                        let text = tf!(
                            "{dir} 不是 Git 仓库。请进入含有仓库的目录；要在此新建仓库，点「初始化仓库…」。",
                            dir = path
                        );
                        this.status = Some((text.into(), true));
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
    /// 这台主机的主目录（连着才知道）。
    fn home(&self, cx: &App) -> Option<String> {
        match self.app.read(cx).status(&self.server) {
            ConnStatus::Ready(conn) => Some(conn.hello.home.clone()),
            _ => None,
        }
    }

    /// 列表正在显示的目录是什么情况。
    fn kind(&self, cx: &App) -> DirKind {
        let Some(listing) = &self.listing else {
            return DirKind::Unknown;
        };
        if listing.repo {
            return DirKind::Repo;
        }
        if let Some(root) = &listing.root {
            return DirKind::Inside(root.clone());
        }
        match self.home(cx) {
            Some(home) if same_dir(&home, &listing.path) => DirKind::Home,
            _ => DirKind::Plain,
        }
    }

    /// 「初始化仓库…」：在列表正在显示的目录里建新仓库，先问（「取消」是主按钮）。
    fn init_here(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.listing.as_ref().map(|listing| listing.path.clone()) else {
            return;
        };
        let server = self.server.clone();
        let channel = self
            .app
            .update(cx, |app, cx| app.channel_for(Some(&server), cx));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(channel) = channel.await else {
                return;
            };
            this.update_in(cx, |_, window, cx| {
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
            })
            .ok();
        })
        .detach();
    }

    /// 「新建文件夹」：列表顶上出一行输入框，Enter 建好并进去，Esc 不建。
    fn start_new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr("新文件夹的名称")));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let name = input.read(cx).value().trim().to_string();
                    this.create_folder(name, window, cx);
                }
            },
        );
        input.update(cx, |state, cx| state.focus(window, cx));
        self.creating = Some((input, subscription));
        cx.notify();
    }

    fn create_folder(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.listing.as_ref().map(|listing| listing.path.clone()) else {
            return;
        };
        if name.is_empty() {
            self.creating = None;
            cx.notify();
            return;
        }
        if name == "." || name == ".." || name.contains(['/', '\\']) {
            self.status = Some((tr("文件夹名称不能包含斜杠，也不能是 . 或 ..").into(), true));
            cx.notify();
            return;
        }
        let channel = self
            .app
            .update(cx, |app, cx| app.channel_for(Some(&self.server), cx));
        self.busy = true;
        cx.spawn_in(window, async move |this, cx| {
            let created = match channel.await {
                Ok(channel) => channel
                    .fs_mkdir(&dir, &name)
                    .await
                    .map_err(|err| format!("{err:#}")),
                Err(err) => Err(err.to_string()),
            };
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match created {
                    Ok(()) => {
                        this.notice = Some(tf!("已创建「{name}」", name).into());
                        this.navigate(join(&dir, &name), window, cx);
                    }
                    Err(err) => {
                        let text = tf!("无法创建文件夹：{err}", err);
                        this.status = Some((text.into(), true));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// 「显示隐藏目录」：记进设置，重新列一遍。
    fn set_hidden(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.show_hidden = on;
        self.app.update(cx, |app, cx| {
            if app.settings.browse_hidden != on {
                app.update_settings(cx, |settings| settings.browse_hidden = on);
            }
        });
        if let Some(path) = self.listing.as_ref().map(|listing| listing.path.clone()) {
            self.navigate(path, window, cx);
        }
    }

    /// 记下打开的仓库所在的目录：下次「打开仓库」从这里开始，旁边的仓库就在眼前。
    fn remember(&self, repo: &str, cx: &mut Context<Self>) {
        let dir = parent_dir(repo).unwrap_or_else(|| repo.to_string());
        self.app
            .update(cx, |app, cx| app.set_browse_dir(&self.server, dir, cx));
    }
}

/// 列表正在显示的目录是什么情况（下面那一行字、主按钮按它定）。
#[derive(Clone, PartialEq)]
enum DirKind {
    /// 还没列出来
    Unknown,
    /// 本身是仓库
    Repo,
    /// 在这个仓库里（仓库根的路径）
    Inside(String),
    /// 主目录：不是仓库，也不给初始化
    Home,
    /// 普通目录
    Plain,
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

/// 两个路径是不是同一个目录的写法（只差末尾的斜杠）。
fn same_dir(a: &str, b: &str) -> bool {
    let trim = |path: &str| path.trim_end_matches(['/', '\\']).to_string();
    // 根目录 `/` 去掉斜杠就空了，空对空也算同一个
    trim(a) == trim(b)
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
        let list_height = (window.viewport_size().height - px(300.)).clamp(px(140.), px(340.));
        let theme = cx.theme();
        let (border, muted, hover, danger, success) = (
            theme.border,
            theme.muted_foreground,
            theme.list_hover,
            theme.danger,
            theme.success,
        );
        // 「仓库」标签用主题的绿色：换主题跟着换
        let repo_color = success;
        // 打开仓库时，标有「仓库」的目录前面有勾选框；其他行留出同样宽的一格，目录名对齐
        let pickable = !self.pick_dir;
        let slot = || div().w(px(16.)).flex_shrink_0();

        let mut rows: Vec<AnyElement> = Vec::new();
        // 新建文件夹：列表顶上一行输入框
        if let Some((input, _)) = &self.creating {
            rows.push(
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .when(pickable, |row| row.child(slot()))
                    .child(ui::icon_small(IconName::FolderPlus).text_color(muted))
                    .child(
                        div()
                            .flex_1()
                            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                if event.keystroke.key == "escape" {
                                    cx.stop_propagation();
                                    this.creating = None;
                                    cx.notify();
                                }
                            }))
                            .child(Input::new(input).small()),
                    )
                    .into_any_element(),
            );
        }
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
                                // 隐藏目录灰一档
                                .when(entry.hidden, |row| row.text_color(muted))
                                .child(ui::clipped("name", format!("{}/", entry.name)).flex_1())
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
            if listing.entries.is_empty() && self.creating.is_none() {
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

        let kind = self.kind(cx);
        let typed_elsewhere = self.target(cx).is_some_and(|target| {
            self.listing
                .as_ref()
                .is_none_or(|listing| !same_dir(&target, &listing.path))
        });
        let has_target = self.target(cx).is_some();
        let pick_dir = self.pick_dir;
        let picked = self.selected.len();
        let repos_here = if pickable {
            self.repos_here()
        } else {
            Vec::new()
        };
        let root_name = |root: &str| crate::settings::base_name(root).to_string();
        let hint: SharedString = match (&self.status, &kind) {
            (Some((text, _)), _) => text.clone(),
            (None, _) if pick_dir => tr("点击目录进入；仓库将克隆到所选目录下").into(),
            // 勾的可能在别的目录里，列出名字免得忘了
            (None, _) if picked > 0 => {
                let names = self
                    .selected
                    .iter()
                    .map(|path| crate::settings::base_name(path))
                    .collect::<Vec<_>>()
                    .join(tr("、"));
                tf!("已选 {n} 个仓库：{names}", n = picked, names).into()
            }
            (None, DirKind::Repo) => tr("此目录是仓库，可以打开").into(),
            (None, DirKind::Inside(root)) => tf!(
                "当前目录在仓库「{name}」中，将打开其根目录",
                name = root_name(root)
            )
            .into(),
            (None, DirKind::Home | DirKind::Plain) if !repos_here.is_empty() => {
                tr("此目录不是 Git 仓库。进入标有「仓库」的目录，或勾选后一次打开多个。").into()
            }
            (None, DirKind::Home | DirKind::Plain) => {
                tr("此目录不是 Git 仓库。请进入含有仓库的目录，或新建文件夹。").into()
            }
            (None, DirKind::Unknown) => SharedString::default(),
        };
        let all_here = repos_here.iter().all(|path| self.selected.contains(path));
        let hint_color = match (&self.status, &kind) {
            (Some((_, true)), _) => danger,
            (None, DirKind::Repo | DirKind::Inside(_)) if picked == 0 && !pick_dir => success,
            _ => muted,
        };
        // 主按钮：在仓库里写打开哪个；普通目录置灰（输入框里改了路径的不知道是什么，照常能点）
        let (open_label, open_enabled): (SharedString, bool) = match (&kind, picked) {
            (_, n) if n > 0 => (tf!("打开 {n} 个仓库", n).into(), true),
            (DirKind::Inside(root), _) if !typed_elsewhere => (
                tf!("打开仓库「{name}」", name = root_name(root)).into(),
                true,
            ),
            (DirKind::Home | DirKind::Plain, _) if !typed_elsewhere => {
                (tr("打开此仓库").into(), false)
            }
            _ => (tr("打开此仓库").into(), has_target),
        };
        let can_init =
            !pick_dir && picked == 0 && !typed_elsewhere && matches!(kind, DirKind::Plain);
        let show_hidden = self.show_hidden;

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
                    )))
                    .child(
                        Button::new("new-folder")
                            .icon(ui::icon(IconName::FolderPlus))
                            .tooltip(tr("新建文件夹"))
                            .disabled(self.listing.is_none() || self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.start_new_folder(window, cx)
                                }),
                            ),
                    ),
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
                    .min_h(px(16.))
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
                    .child(
                        Checkbox::new("show-hidden")
                            .label(tr("显示隐藏目录"))
                            .checked(show_hidden)
                            .on_click(cx.listener(|this, checked: &bool, window, cx| {
                                this.set_hidden(*checked, window, cx)
                            })),
                    )
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
                    .when(can_init, |bar| {
                        bar.child(
                            Button::new("init")
                                .outline()
                                .label(tr("初始化仓库…"))
                                .tooltip(tr("在此目录中创建新的 Git 仓库"))
                                .disabled(self.busy)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.init_here(window, cx)),
                                ),
                        )
                    })
                    .child(
                        Button::new("cancel")
                            .label(tr("取消"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(if pick_dir {
                        Button::new("pick")
                            .primary()
                            .label(tr("克隆到此目录"))
                            .disabled(!has_target || self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.press_main(AfterNavigate::Pick, window, cx)
                            }))
                    } else {
                        Button::new("open")
                            .primary()
                            .label(open_label)
                            .disabled(!open_enabled || self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                // 勾了仓库就打开勾的那几个，和输入框无关
                                if this.selected.is_empty() {
                                    this.press_main(AfterNavigate::Open, window, cx)
                                } else {
                                    this.open_picked(window, cx)
                                }
                            }))
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
