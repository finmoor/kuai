//! 服务器页（标题栏上的「服务器」）：左边是本机和各台服务器，右边是选中那台的状态和它上面的仓库。
//!
//! 左边一行写连没连上、几个仓库；连着的服务器上配置的服务器（经由它连接）缩进挂在它下面，
//! 本机也直接配置了的不重复列。最下面「添加服务器…」。
//! 右边顶上是名字、状态（kuaid、git 的版本、延迟，「N 个工作区」点了到工作区页看那台的）、登录信息，
//! 「连接」「编辑…」和 ⋯（映射目录到本机、移除）。下面是这台上的仓库：克隆、打开仓库（本机是添加本机
//! 仓库）；每行写分支、改动数、在哪些工作区里（只显示）、已打开、终端个数，悬停出改显示名称、映射到
//! 本机、从列表中移除。子仓库默认收着（列的是整台的仓库，全展开太长）。点仓库只开标签页，不碰工作区。
//! 行的样子和工作区页一样，见 `repo_rows`。设计见 `docs/design/server-workspace/`（第二版）。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use fr_core::i18n::{tr, trc};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::radio::Radio;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::{AppState, ConnStatus};
use crate::browser::{BrowserEvent, RemoteBrowser};
use crate::clone::{CloneEvent, CloneView};
use crate::machines::LOCAL;
use crate::repo_rows::{Block, Columns, Handlers, MenuFor, render_block};
use crate::repo_scan::RepoScan;
use crate::secrets::Saved;
use crate::settings::{Auth, RepoEntry, Server, Target};
use crate::term_board::TermBoard;
use crate::ui;

/// 左边顶上那一条的高度（和工作区页、会话页一样）
const HEAD_HEIGHT: f32 = 52.;

pub enum ManagerEvent {
    /// 在标签页中打开（只开标签页，不放进哪个工作区）
    Open(RepoEntry),
    /// 给这个仓库改名（对话框由主窗口开）
    Rename(RepoEntry),
    /// 到工作区页看这台电脑上的工作区（服务器上的「3 个工作区」）
    ShowWorkspaces(String),
    /// 到会话页看这个仓库的终端（点了仓库那一行的终端个数）
    Terminals(RepoEntry),
}

pub struct ManagerView {
    app: Entity<AppState>,
    /// 子仓库、分支、改动数（和工作区页共用一份）
    scan: Entity<RepoScan>,
    board: Entity<TermBoard>,
    /// 左边选中的那台（这次运行里记着；没选过是本机）
    selected: Option<String>,
    /// 这一页显示着
    visible: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ManagerEvent> for ManagerView {}

impl ManagerView {
    pub fn new(
        app: Entity<AppState>,
        scan: Entity<RepoScan>,
        board: Entity<TermBoard>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            // 服务器连上了、仓库列表变了：重画，显示着的话把要看的仓库再交给 `scan`
            cx.observe(&app, |this, _, cx| {
                if this.visible {
                    this.want_scan(cx);
                }
                cx.notify();
            }),
            cx.observe(&scan, |_, _, cx| cx.notify()),
            cx.observe(&board, |_, _, cx| cx.notify()),
        ];
        Self {
            app,
            scan,
            board,
            selected: None,
            visible: false,
            _subscriptions: subscriptions,
        }
    }

    fn open(&mut self, entry: RepoEntry, cx: &mut Context<Self>) {
        cx.emit(ManagerEvent::Open(entry));
    }

    /// 新加的仓库：进它所在那台电脑的列表，各开一个标签页，按给的先后，最后一个是选中的。
    fn add_and_open(&mut self, entries: Vec<RepoEntry>, cx: &mut Context<Self>) {
        for entry in entries {
            self.app
                .update(cx, |app, cx| app.add_repo(entry.clone(), cx));
            self.open(entry, cx);
        }
    }

    /// 选本机的目录，可以一次选几个；在仓库中就找到仓库根目录加进列表。
    /// 只选了一个、又不是仓库时问是否初始化；选了几个时不是仓库的不逐个问（见 `open_dir::open_dirs`）。
    fn add_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some(tr("添加仓库").into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let dirs: Vec<String> = paths
                .into_iter()
                .map(|dir| dir.to_string_lossy().into_owned())
                .collect();
            if dirs.is_empty() {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                let app = this.app.clone();
                crate::open_dir::open_dirs(
                    &app,
                    None,
                    dirs,
                    window,
                    cx,
                    |this: &mut Self, entries, _, cx| this.add_and_open(entries, cx),
                );
            })
            .ok();
        })
        .detach();
    }

    /// 添加服务器；`edit` 有值时是改一台已有的：id 和它名下登记的仓库都留着。
    ///
    /// 主机、端口、用户名分开填，存的时候拼成 ssh 的目标；登录方式选密钥或密码。
    /// 填了的密码、私钥口令存进系统的凭据管理器（见 `secrets`），不进设置文件。
    ///
    /// 「测试连接」拿表单里现在填的打开诊断对话框（[`crate::diagnose::Diagnose::open_form`]）：
    /// 逐步登录一次，不保存，不装、不启动 kuaid。
    fn server_dialog(&mut self, edit: Option<Server>, window: &mut Window, cx: &mut Context<Self>) {
        let editing = edit.is_some();
        let old = edit.unwrap_or_default();
        let target = Target::parse(&old.destination);
        // 这台服务器存着的密码或口令（是哪一种，看它原来的登录方式）
        let saved = match editing {
            true => crate::secrets::saved_for(&old),
            false => Saved::Absent,
        };
        // 存着、连接时在用：提示「已保存」，可以点「清除」
        let had_secret = saved == Saved::Usable;
        let old_auth = old.auth;
        let secret_hint = move |auth: Auth| {
            if had_secret && auth == old_auth {
                tr("已保存，如不修改请留空")
            } else {
                tr("留空则在连接时输入")
            }
        };
        let mut input = |value: &str, placeholder: &'static str, masked: bool| {
            let state = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .masked(masked)
            });
            if !value.is_empty() {
                state.update(cx, |state, cx| {
                    state.set_value(value.to_string(), window, cx)
                });
            }
            state
        };
        let host = input(
            &target.host,
            tr("主机名、IP 或 ~/.ssh/config 中的别名"),
            false,
        );
        let port = input(&target.port, "22", false);
        let user = input(&target.user, tr("留空则使用 ssh 默认值"), false);
        let key = input(
            old.identity_file.as_deref().unwrap_or_default(),
            tr("留空则使用默认密钥和 ssh-agent"),
            false,
        );
        let passphrase = input("", secret_hint(Auth::Key), true);
        let password = input("", secret_hint(Auth::Password), true);
        let name = input(&old.name, tr("默认使用主机名"), false);
        let start_dir = input(
            old.start_dir.as_deref().unwrap_or_default(),
            tr("留空则为主目录"),
            false,
        );
        // 能选作跳板机的：别的已配置的服务器，而且不经这台连接（不然绕圈）。
        // 每项是 id、名字、ssh 目标、从本机到它要经过的跳板机的名字
        let candidates: Rc<Vec<JumpChoice>> = {
            let settings = &self.app.read(cx).settings;
            Rc::new(
                settings
                    .servers
                    .iter()
                    .filter(|server| settings.can_jump(&old.id, &server.id))
                    .map(|server| JumpChoice {
                        id: server.id.clone(),
                        name: server.name.clone().into(),
                        destination: server.destination.clone().into(),
                        via: settings
                            .route(&server.id)
                            .iter()
                            .map(|jump| jump.name.clone().into())
                            .collect(),
                    })
                    .collect(),
            )
        };
        let jump: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(
            old.jump
                .clone()
                .filter(|id| candidates.iter().any(|choice| choice.id == *id)),
        ));
        if jump.borrow().is_some() {
            host.update(cx, |state, cx| {
                state.set_placeholder(tr(HOST_VIA_JUMP), window, cx)
            });
        }

        let app = self.app.clone();
        let view = cx.entity().downgrade();
        let first_build = Cell::new(true);
        let auth = Rc::new(Cell::new(old.auth));
        let forward_agent = Rc::new(Cell::new(old.forward_agent));
        // 新加的默认勾着
        let auto_connect = Rc::new(Cell::new(!old.manual_connect));
        // 点了「清除」：存着的密码、口令不要了
        let forget = Rc::new(Cell::new(false));
        // 没填对时的提示，显示在对话框里
        let problem: Rc<RefCell<Option<SharedString>>> = Rc::default();
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                host.focus_handle(cx).focus(window, cx);
            }
            let (muted, danger) = (cx.theme().muted_foreground, cx.theme().danger);
            // 把表单里填的读成一台服务器，连同这次填的密码或口令；没填对就是该提示的那句话
            let collect = {
                let (host, port, user, key) =
                    (host.clone(), port.clone(), user.clone(), key.clone());
                let (passphrase, password, name) =
                    (passphrase.clone(), password.clone(), name.clone());
                let start_dir = start_dir.clone();
                let (auth, forward_agent) = (auth.clone(), forward_agent.clone());
                let (auto_connect, jump, id) = (auto_connect.clone(), jump.clone(), old.id.clone());
                move |cx: &App| -> Result<(Server, String), &'static str> {
                    let read = |input: &Entity<InputState>| input.read(cx).value().to_string();
                    let (host, user, port) = (read(&host), read(&user), read(&port));
                    let (key, name) = (read(&key), read(&name));
                    let start_dir = read(&start_dir);
                    let auth = auth.get();
                    // 密码原样用，不去掉两头的空格
                    let secret = match auth {
                        Auth::Key => read(&passphrase),
                        Auth::Password => read(&password),
                    };
                    // 主机那一格里贴进整个 user@host:端口 的也认；单独填了的用户名、端口优先。
                    // 解释不了的部分不悄悄丢掉，让用户改
                    if !host.trim().is_empty() && !Target::well_formed(&host) {
                        return Err(tr("主机地址格式不正确"));
                    }
                    let mut target = Target::parse(&host);
                    if !user.trim().is_empty() {
                        target.user = user.trim().to_string();
                    }
                    if !port.trim().is_empty() {
                        target.port = port.trim().to_string();
                    }
                    if target.host.is_empty() {
                        return Err(tr("请填写主机"));
                    }
                    if !target.port.is_empty() && !target.port_ok() {
                        return Err(tr("端口须为 1～65535 之间的数字"));
                    }
                    let key = key.trim();
                    if auth == Auth::Key && !key.is_empty() {
                        if key.to_lowercase().ends_with(".pub") {
                            return Err(tr("这是公钥文件，请选择不带 .pub 后缀的私钥文件"));
                        }
                        // ~ 开头的由 ssh 自己展开，这里不查
                        if !key.starts_with('~') && !std::path::Path::new(key).is_file() {
                            return Err(tr("未找到私钥文件"));
                        }
                    }
                    let mut label = name.trim().to_string();
                    if label.is_empty() {
                        label = target.host.clone();
                    }
                    let server = Server {
                        id: id.clone(),
                        name: label,
                        destination: target.compose(),
                        forward_agent: forward_agent.get(),
                        auth,
                        // 换成密码登录时填过的私钥文件留着，换回来还在
                        identity_file: (!key.is_empty()).then(|| key.to_string()),
                        jump: jump.borrow().clone(),
                        start_dir: Some(start_dir.trim().to_string()).filter(|dir| !dir.is_empty()),
                        // 上次打开的目录留不留由 `update_server` 定
                        last_dir: None,
                        // 映射、它是哪台电脑不在这张表单里，`update_server` 留着原来的
                        mappings: Vec::new(),
                        manual_connect: !auto_connect.get(),
                        machine: None,
                    };
                    Ok((server, secret))
                }
            };
            let submit = {
                let (collect, app) = (collect.clone(), app.clone());
                let (forget, problem, old) = (forget.clone(), problem.clone(), old.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let (server, secret) = match collect(cx) {
                        Ok(read) => read,
                        Err(message) => {
                            *problem.borrow_mut() = Some(message.into());
                            window.refresh();
                            return;
                        }
                    };
                    let auth = server.auth;
                    // 存着的密码、口令：填了新的就换；没填、登录方式没变、也没点清除，就留着（钥匙圈锁着
                    // 读不出来的也留着）。密码只对原来的地址作数，口令只对原来的私钥文件作数：改了就不留，
                    // 不然旧密码会自动交给新地址；存着的对不上现在这种登录的（见 secrets::binding）也删掉
                    let bound = same_secret_binding(&old, &server);
                    let keep = matches!(saved, Saved::Usable | Saved::Unknown)
                        && auth == old.auth
                        && bound
                        && !forget.get();
                    let secret_changed = !secret.is_empty() || (had_secret && !keep);
                    let unbound = had_secret && secret.is_empty() && auth == old.auth && !bound;
                    let job = if !secret.is_empty() {
                        SecretJob::Save(secret)
                    } else if forget.get() {
                        SecretJob::Forget
                    } else if saved != Saved::Absent && !keep {
                        SecretJob::Drop
                    } else {
                        SecretJob::Keep
                    };
                    let mut stored = server.clone();
                    window.close_dialog(cx);
                    let (id, reconnect) = app.update(cx, |app, cx| {
                        if editing {
                            let id = server.id.clone();
                            (id, app.update_server(server, secret_changed, cx))
                        } else {
                            (app.add_server(server, cx), true)
                        }
                    });
                    stored.id = id.clone();
                    // 存、删可能要等（Linux 上钥匙圈锁着、还没有时弹系统的框等用户），放到后台做，不卡
                    // 界面；做完再连，连接时要读存着的
                    let done = cx.background_executor().spawn(async move { job.run(&stored) });
                    let (app, forgot) = (app.clone(), forget.get());
                    window
                        .spawn(cx, async move |cx| {
                            let done = done.await;
                            cx.update(|window, cx| {
                                match done {
                                    Err(text) => {
                                        window.push_notification(Notification::error(text), cx)
                                    }
                                    Ok(()) if unbound && !forgot => {
                                        let text = match auth {
                                            Auth::Password => tr("地址已修改，原来保存的密码已清除，连接时需重新输入。"),
                                            Auth::Key => tr("私钥文件已修改，原来保存的口令已清除，连接时需重新输入。"),
                                        };
                                        window.push_notification(Notification::info(text), cx);
                                    }
                                    Ok(()) => {}
                                }
                                if reconnect {
                                    // 马上开始连接，结果显示在列表上
                                    app.update(cx, |app, cx| drop(app.connect(&id, cx)));
                                    if editing {
                                        window.push_notification(
                                            Notification::info(tr(
                                                "已使用新设置重新连接，已打开的标签页需重新打开后生效。",
                                            )),
                                            cx,
                                        );
                                    }
                                }
                            })
                            .ok();
                        })
                        .detach();
                }
            };
            // 测试连接：拿表单里现在填的打开诊断对话框，逐步登录一次，不保存。密码、口令用这次填的；
            // 没填、存着的又还作数就用存着的；都没有就照常弹窗问。跳板机照它们自己存着的设置登录
            let run_test = {
                let (collect, app, forget) = (collect.clone(), app.clone(), forget.clone());
                let old = old.clone();
                let problem = problem.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let (server, secret) = match collect(cx) {
                        Ok(read) => read,
                        Err(message) => {
                            *problem.borrow_mut() = Some(message.into());
                            window.refresh();
                            return;
                        }
                    };
                    *problem.borrow_mut() = None;
                    let bound = same_secret_binding(&old, &server);
                    let keep = had_secret && server.auth == old_auth && bound && !forget.get();
                    let mut options = crate::app_state::connect_options(&server);
                    options.forward_agent = false;
                    options.secret = match secret.is_empty() {
                        false => Some(secret),
                        true => options.secret.filter(|_| keep),
                    };
                    // 依次经过的跳板机：连接选项，和对话框里显示的名字、地址
                    let jumps: Vec<(String, String)> = {
                        let settings = &app.read(cx).settings;
                        let last = server.jump.as_deref().and_then(|id| settings.server(id));
                        let before = last.map(|jump| settings.route(&jump.id));
                        let route: Vec<&Server> = before.into_iter().flatten().chain(last).collect();
                        options.jumps = route
                            .iter()
                            .map(|jump| crate::app_state::jump_options(jump))
                            .collect();
                        route
                            .iter()
                            .map(|jump| (jump.name.clone(), jump.destination.clone()))
                            .collect()
                    };
                    window.refresh();
                    crate::diagnose::Diagnose::open_form(
                        app.clone(),
                        &server,
                        jumps,
                        options,
                        window,
                        cx,
                    );
                }
            };
            let auth_now = auth.get();
            let pick_auth = |to: Auth| {
                let auth = auth.clone();
                move |_: &bool, window: &mut Window, _: &mut App| {
                    auth.set(to);
                    window.refresh();
                }
            };
            let browse = {
                let (view, key) = (view.clone(), key.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    view.update(cx, |this, cx| this.pick_key_file(key.clone(), window, cx))
                        .ok();
                }
            };
            // 密码、口令那一格；存着的可以点「清除」
            let secret_field = |label: &'static str, input: &Entity<InputState>| {
                let shown = had_secret && auth_now == old_auth && !forget.get();
                let clear = shown.then(|| {
                    let (forget, input) = (forget.clone(), input.clone());
                    Button::new("forget-secret")
                        .ghost()
                        .xsmall()
                        .label(tr("清除"))
                        .on_click(move |_, window, cx| {
                            forget.set(true);
                            input.update(cx, |state, cx| {
                                state.set_placeholder(tr("留空则在连接时输入"), window, cx)
                            });
                            window.refresh();
                        })
                        .into_any_element()
                });
                field_with(label, clear, Input::new(input).mask_toggle())
            };
            let jump_now = jump.borrow().clone();
            let chosen = jump_now
                .as_deref()
                .and_then(|id| candidates.iter().find(|choice| choice.id == id))
                .cloned();
            // 选一台：记下来，主机那一格的提示跟着换
            let pick_jump = {
                let (jump, host) = (jump.clone(), host.clone());
                move |to: Option<String>| {
                    let (jump, host) = (jump.clone(), host.clone());
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let hint = if to.is_some() {
                        tr(HOST_VIA_JUMP)
                    } else {
                        tr("主机名、IP 或 ~/.ssh/config 中的别名")
                    };
                        host.update(cx, |state, cx| state.set_placeholder(hint, window, cx));
                        *jump.borrow_mut() = to.clone();
                        window.refresh();
                    }
                }
            };
            let jump_field = {
                let candidates = candidates.clone();
                let pick_jump = pick_jump.clone();
                let jump_now = jump_now.clone();
                let shown = match &chosen {
                    Some(choice) => h_flex()
                        .gap_2()
                        .min_w_0()
                        .child(div().flex_none().child(choice.name.clone()))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_family(crate::fonts::mono())
                                .text_xs()
                                .text_color(muted)
                                .child(choice.destination.clone()),
                        ),
                    None => h_flex()
                        .gap_2()
                        .child(tr("不使用"))
                        .child(div().text_xs().text_color(muted).child(tr("直接连接"))),
                };
                field(
                    tr("跳板机"),
                    Button::new("jump")
                        .outline()
                        .w_full()
                        .child(shown)
                        .dropdown_caret(true)
                        .dropdown_menu(move |menu, _, cx| {
                            let muted = cx.theme().muted_foreground;
                            let mut menu = menu.min_w(px(512.)).item(
                                PopupMenuItem::new(tr("不使用（直接连接）"))
                                    .checked(jump_now.is_none())
                                    .on_click(pick_jump(None)),
                            );
                            if candidates.is_empty() {
                                return menu.separator().label(tr("暂无其他已配置的主机"));
                            }
                            menu = menu.separator().label(tr("经由已配置的主机"));
                            for choice in candidates.iter() {
                                let (name, destination) =
                                    (choice.name.clone(), choice.destination.clone());
                                menu = menu.item(
                                    PopupMenuItem::element(move |_, _| {
                                        h_flex()
                                            .gap_3()
                                            .child(name.clone())
                                            .child(
                                                div()
                                                    .font_family(crate::fonts::mono())
                                                    .text_xs()
                                                    .text_color(muted)
                                                    .child(destination.clone()),
                                            )
                                    })
                                    .checked(jump_now.as_deref() == Some(&choice.id))
                                    .on_click(pick_jump(Some(choice.id.clone()))),
                                );
                            }
                            menu
                        }),
                )
            };
            // 选了跳板机：把从本机到目标要经过的几台画出来
            let route_line = chosen.as_ref().map(|choice| {
                let typed = Target::parse(host.read(cx).value().as_ref()).host;
                let target: SharedString = if typed.is_empty() {
                    tr("目标主机").into()
                } else {
                    typed.into()
                };
                let mut stops: Vec<(SharedString, bool)> = vec![(tr("本机").into(), false)];
                stops.extend(choice.via.iter().cloned().map(|name| (name, false)));
                stops.push((choice.name.clone(), false));
                stops.push((target, true));
                let accent = cx.theme().primary;
                let chip_bg = cx.theme().secondary;
                let mut line = h_flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .child(div().text_xs().text_color(muted).child(tr("连接路径")));
                for (index, (name, last)) in stops.into_iter().enumerate() {
                    if index > 0 {
                        line = line.child(ui::icon_small(IconName::ArrowRight).text_color(muted));
                    }
                    line = line.child(
                        div()
                            .px(px(8.))
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded(px(11.))
                            .text_xs()
                            .max_w(px(200.))
                            .truncate()
                            .map(|chip| match last {
                                true => chip.bg(accent.opacity(0.16)).text_color(accent),
                                false => chip.bg(chip_bg),
                            })
                            .child(name),
                    );
                }
                line
            });
            // 映射到本机：已经添加的服务器才有（映射是挂在服务器的 id 上的）
            let mapping_section = editing.then(|| crate::mapping::section(&app, &old, cx));
            dialog
                .title(if editing {
                    tr("编辑主机")
                } else {
                    tr("添加主机")
                })
                // 回车等于点确定；关不关对话框由 submit 自己决定
                .on_ok({
                    let submit = submit.clone();
                    move |event, window, cx| {
                        submit(event, window, cx);
                        false
                    }
                })
                .w(px(560.))
                .child(
                    v_flex()
                        .gap_3()
                        .child(
                            h_flex()
                                .gap_3()
                                .items_start()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(field(tr("主机地址"), Input::new(&host))),
                                )
                                .child(
                                    div()
                                        .w(px(96.))
                                        .flex_none()
                                        .child(field(tr("端口"), Input::new(&port))),
                                ),
                        )
                        .child(v_flex().gap_2().child(jump_field).children(route_line))
                        .child(field(tr("用户名"), Input::new(&user)))
                        .child(
                            h_flex()
                                .gap_4()
                                .child(div().text_sm().child(tr("登录方式")))
                                .child(
                                    Radio::new("auth-key")
                                        .label(tr("密钥"))
                                        .checked(auth_now == Auth::Key)
                                        .on_click(pick_auth(Auth::Key)),
                                )
                                .child(
                                    Radio::new("auth-password")
                                        .label(tr("密码"))
                                        .checked(auth_now == Auth::Password)
                                        .on_click(pick_auth(Auth::Password)),
                                ),
                        )
                        .map(|body| match auth_now {
                            Auth::Key => body
                                .child(field_with(
                                    tr("私钥文件"),
                                    None,
                                    h_flex()
                                        .gap_2()
                                        .child(div().flex_1().min_w_0().child(Input::new(&key)))
                                        .child(
                                            Button::new("browse")
                                                .label(tr("浏览…"))
                                                .on_click(browse),
                                        ),
                                ))
                                .child(secret_field(tr("私钥口令"), &passphrase)),
                            Auth::Password => body.child(secret_field(tr("密码"), &password)),
                        })
                        .child(
                            h_flex()
                                .gap_3()
                                .items_start()
                                .child(
                                    div()
                                        .w(px(190.))
                                        .flex_none()
                                        .child(field(tr("名称"), Input::new(&name))),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(field(tr("初始目录"), Input::new(&start_dir))),
                                ),
                        )
                        .child(
                            Checkbox::new("forward-agent")
                                .label(tr("转发本机的 ssh-agent（仅对受信任的主机启用）"))
                                .checked(forward_agent.get())
                                .on_click({
                                    let forward_agent = forward_agent.clone();
                                    move |checked, window, _| {
                                        forward_agent.set(*checked);
                                        window.refresh();
                                    }
                                }),
                        )
                        .child(
                            Checkbox::new("auto-connect")
                                .label(tr("启动时自动连接"))
                                .checked(auto_connect.get())
                                .on_click({
                                    let auto_connect = auto_connect.clone();
                                    move |checked, window, _| {
                                        auto_connect.set(*checked);
                                        window.refresh();
                                    }
                                }),
                        )
                        .when_some(problem.borrow().clone(), |body, found| {
                            body.child(div().text_sm().text_color(danger).child(found))
                        })
                        // 这句说的是上面的登录信息，紧跟着它们；映射是单独的一节，放在最后
                        .child(div().text_xs().text_color(muted).child(
                            match (cfg!(windows), jump_now.is_some()) {
                                (true, true) => tr(
                                    "密码和私钥口令保存在 Windows 凭据管理器中。首次连接时在主机的 ~/.kuai/ 中安装 kuaid，跳板机上不安装。",
                                ),
                                (true, false) => tr(
                                    "密码和私钥口令保存在 Windows 凭据管理器中。首次连接时在主机的 ~/.kuai/ 中安装 kuaid。",
                                ),
                                (false, true) => tr(
                                    "密码和私钥口令保存在系统钥匙圈中。首次连接时在主机的 ~/.kuai/ 中安装 kuaid，跳板机上不安装。",
                                ),
                                (false, false) => tr(
                                    "密码和私钥口令保存在系统钥匙圈中。首次连接时在主机的 ~/.kuai/ 中安装 kuaid。",
                                ),
                            },
                        ))
                        .children(mapping_section),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("test")
                                .label(tr("测试连接"))
                                .on_click(run_test),
                        )
                        .child(
                            Button::new("cancel")
                                .label(tr("取消"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("ok")
                                .primary()
                                .label(if editing {
                                    tr("保存")
                                } else {
                                    tr("添加并连接")
                                })
                                .on_click(submit),
                        ),
                )
        });
    }

    /// 选一个私钥文件，填进对话框的那一格。
    fn pick_key_file(
        &mut self,
        input: Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr("选择私钥文件").into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let path = path.to_string_lossy().into_owned();
            this.update_in(cx, |_, window, cx| {
                input.update(cx, |state, cx| state.set_value(path, window, cx));
            })
            .ok();
        })
        .detach();
    }

    /// 克隆仓库到本机（`server` 为空）或者某台服务器上，克隆完打开。
    fn clone_dialog(
        &mut self,
        server: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = match server.as_deref() {
            Some(id) => tf!("克隆到 {name}", name = self.app.read(cx).machine_name(id)),
            None => tr("克隆到本机").into(),
        };
        let app = self.app.clone();
        let view = cx.new(|cx| CloneView::new(app, server, window, cx));
        cx.subscribe_in(&view, window, |this, _, event, window, cx| match event {
            CloneEvent::Done(entry) => {
                window.close_dialog(cx);
                this.add_and_open(vec![entry.clone()], cx);
            }
        })
        .detach();
        let first_build = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                view.update(cx, |view, cx| view.focus(window, cx));
            }
            let on_enter = view.clone();
            // 克隆期间不能用 Esc、点外面、点右上角关掉：那样等于不问就中止，还留下半截目录
            let busy = view.read(cx).busy();
            dialog
                .title(title.clone())
                .w(px(620.))
                .keyboard(!busy)
                .overlay_closable(!busy)
                .close_button(!busy)
                // 回车开始克隆，不关对话框
                .on_ok(move |_, window, cx| {
                    on_enter.update(cx, |view, cx| view.start(window, cx));
                    false
                })
                .child(view.clone())
        });
    }

    /// 在服务器上挑一个仓库打开。
    fn open_remote_dialog(&mut self, server: String, window: &mut Window, cx: &mut Context<Self>) {
        let title = tf!(
            "打开 {name} 上的仓库",
            name = self.app.read(cx).machine_name(&server)
        );
        let app = self.app.clone();
        let browser = cx.new(|cx| RemoteBrowser::new(app, server, window, cx));
        cx.subscribe_in(&browser, window, |this, _, event, window, cx| match event {
            BrowserEvent::Open(entries) => {
                window.close_dialog(cx);
                this.add_and_open(entries.clone(), cx);
            }
            BrowserEvent::Dir(_) => {}
        })
        .detach();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(title.clone())
                .w(px(680.))
                // 回车是「前往」输入的路径，不是关掉对话框
                .on_ok(|_, _, _| false)
                .child(browser.clone())
        });
    }
}

/// 经服务器连的（服务器页上它的名字后面）：「经由 A」。
/// 存着的密码、口令还能不能给改过的设置用：密码只对原来的地址（用户、主机、端口）作数，私钥口令
/// 只对原来的私钥文件作数。
/// 主机表单保存时对存着的密码、口令做什么。
enum SecretJob {
    /// 不用动
    Keep,
    /// 存下这次填的
    Save(String),
    /// 用户点了「清除」：删掉，钥匙圈锁着就弹框要解锁
    Forget,
    /// 已经不作数了（登录方式、地址、私钥文件改了）：删掉，钥匙圈锁着就算了（留着也不会交错）
    Drop,
}

impl SecretJob {
    /// 做完；出错时是给用户看的那句话。可能要等（见 [`crate::secrets::set`]），在后台线程上跑。
    fn run(self, server: &Server) -> Result<(), String> {
        match self {
            Self::Keep => Ok(()),
            Self::Save(secret) => crate::secrets::set(server, &secret)
                .map_err(|err| tf!("密码保存失败，连接时需重新输入：{err}", err)),
            Self::Forget => crate::secrets::forget(&server.id)
                .map_err(|err| tf!("未能清除保存的密码：{err}", err)),
            Self::Drop => {
                crate::secrets::delete(&server.id);
                Ok(())
            }
        }
    }
}

fn same_secret_binding(old: &Server, new: &Server) -> bool {
    match new.auth {
        Auth::Password => new.destination == old.destination,
        Auth::Key => new.identity_file == old.identity_file,
    }
}

fn relay_tag(key: &str, via: SharedString, tip: SharedString, colors: Colors) -> AnyElement {
    name_tag(
        format!("relay:{key}"),
        Some(IconName::CornerDownRight),
        tf!("经由 {path}", path = &via).into(),
        tip,
        true,
        colors,
    )
}

/// 服务器上的仓库映射到了本机：行尾写出它在本机的路径。
fn local_badge(local: String, colors: Colors) -> Div {
    h_flex()
        .flex_none()
        .max_w(px(260.))
        .gap(px(4.))
        .text_xs()
        .text_color(colors.muted)
        .child(Icon::new(IconName::HardDrive).size_3().flex_none())
        .child(
            ui::clipped("local", local)
                .font_family(crate::fonts::mono())
                .text_ellipsis_start(),
        )
}

#[derive(Clone, Copy)]
struct Colors {
    muted: Hsla,
    hover: Hsla,
    border: Hsla,
    danger: Hsla,
    success: Hsla,
    accent: Hsla,
    /// 侧栏、仓库列表的底色
    panel: Hsla,
}

/// 移除之前问一下。`danger` 时按钮是红的，回车不执行，要明确点按钮。
pub(crate) fn confirm(
    title: String,
    description: String,
    ok: &'static str,
    danger: bool,
    window: &mut Window,
    cx: &mut App,
    action: impl Fn(&mut App) + 'static,
) {
    confirm_in(title, description, ok, danger, window, cx, move |_, cx| {
        action(cx)
    });
}

/// 同 [`confirm`]，要做的事还用得到窗口（比如接着要建视图、放焦点）。
pub(crate) fn confirm_in(
    title: String,
    description: String,
    ok: &'static str,
    danger: bool,
    window: &mut Window,
    cx: &mut App,
    action: impl Fn(&mut Window, &mut App) + 'static,
) {
    let action = Rc::new(action);
    ui::cancel_pending_tip(window, cx);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let action = action.clone();
        let run = move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            window.close_dialog(cx);
            action(window, cx);
        };
        let on_enter = run.clone();
        alert
            .title(title.clone())
            .description(description.clone())
            .on_ok(move |event, window, cx| {
                if !danger {
                    on_enter(event, window, cx);
                }
                false
            })
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("cancel")
                            .label(tr("取消"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("ok")
                            .label(ok)
                            .with_variant(if danger {
                                ButtonVariant::Danger
                            } else {
                                ButtonVariant::Primary
                            })
                            .on_click(run),
                    ),
            )
    });
}

fn field(label: &'static str, input: impl IntoElement) -> impl IntoElement {
    field_with(label, None, input)
}

/// 选了跳板机以后，主机那一格的提示
const HOST_VIA_JUMP: &str = "跳板机能访问到的地址，例如内网 IP";

/// 「跳板机」下拉里的一项。
#[derive(Clone)]
struct JumpChoice {
    id: String,
    name: SharedString,
    destination: SharedString,
    /// 从本机到这台要先经过的跳板机的名字（它自己也经跳板机连接时才有）
    via: Vec<SharedString>,
}

/// 名字后面的小标签：`accent` 时用强调色（「经由 …」），否则是灰的（「跳板机」）。
fn name_tag(
    key: String,
    icon: Option<IconName>,
    text: SharedString,
    tip: SharedString,
    accent: bool,
    colors: Colors,
) -> AnyElement {
    let color = if accent { colors.accent } else { colors.muted };
    h_flex()
        .id(ElementId::Name(key.into()))
        .flex_none()
        .h(px(18.))
        .px(px(6.))
        .gap(px(3.))
        .rounded(px(9.))
        .text_xs()
        .text_color(color)
        .bg(color.opacity(if accent { 0.14 } else { 0.12 }))
        .when_some(icon, |tag, icon| {
            tag.child(Icon::new(icon).size_3().text_color(color))
        })
        .child(text)
        .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
        .into_any_element()
}

/// 一格：上面是名字（右边可以带个小按钮），下面是填的地方。
fn field_with(
    label: &'static str,
    aside: Option<AnyElement>,
    body: impl IntoElement,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .h(px(20.))
                .justify_between()
                .child(div().text_sm().child(label))
                .children(aside),
        )
        .child(body)
}

impl ManagerView {
    /// 左边列的电脑：本机、本机配置的各台服务器；连着的服务器上配置的服务器（经由它连接）跟在
    /// 它下面，认出来是本机、是它自己或者本机直接配置了的那台的不重复列（见 `machines::Identities`）。
    fn machine_keys(&self, cx: &App) -> Vec<String> {
        self.app.read(cx).machine_keys()
    }

    /// 选中的那台：没选过、选的那台不在了（移除了，经由的那台断开了）是本机。
    fn selected(&self, cx: &App) -> String {
        match &self.selected {
            Some(id) if self.machine_keys(cx).contains(id) => id.clone(),
            _ => LOCAL.to_string(),
        }
    }

    /// 这一页显示出来了：读一遍选中那台上各个仓库的子仓库、分支、改动数。
    pub fn shown(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.visible = true;
        self.want_scan(cx);
        cx.notify();
    }

    /// 这一页不显示了。
    pub fn hidden(&mut self) {
        self.visible = false;
    }

    fn want_scan(&mut self, cx: &mut Context<Self>) {
        let machine = self.selected(cx);
        let repos = self.app.read(cx).repos_on(&machine).unwrap_or_default();
        self.scan.update(cx, |scan, cx| scan.want(&repos, cx));
    }

    fn select(&mut self, machine: String, cx: &mut Context<Self>) {
        self.selected = Some(machine);
        self.want_scan(cx);
        cx.notify();
    }

    /// 展开、收起一个仓库下面的子仓库（这一页默认收着，列的是整台的仓库，全展开太长）。
    fn toggle_subs(&mut self, entry: RepoEntry, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            app.update_settings(cx, |settings| {
                match settings.unfolded_subs.contains(&entry) {
                    true => settings.unfolded_subs.retain(|other| *other != entry),
                    false => settings.unfolded_subs.push(entry),
                }
            })
        });
    }

    fn connect(&mut self, key: &str, cx: &mut Context<Self>) {
        let key = key.to_string();
        self.app.update(cx, |app, cx| drop(app.connect(&key, cx)));
    }

    /// 从列表中移除仓库（先问一下）。服务器上的列表保存在那台上，别的电脑上也跟着没了；关掉的
    /// 标签页留下的本机终端以后再也打不开，一起结束。
    fn confirm_remove(&mut self, entry: RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let (name, shared_on) = {
            let state = app.read(cx);
            let shared_on = entry
                .server
                .as_deref()
                .map(|server| state.machine_name(server));
            (state.repo_name(&entry), shared_on)
        };
        let mut description = tf!(
            "{path}\n仅从列表中移除，仓库本身不受影响。",
            path = entry.path
        );
        if let Some(machine) = &shared_on {
            description.push('\n');
            description.push_str(&tf!(
                "此列表保存在 {machine} 上，所有电脑将同步移除。",
                machine = machine
            ));
        }
        let terminals = crate::repo::parked_terminals_of(&entry, cx);
        if terminals > 0 {
            description.push('\n');
            description.push_str(&tf!(
                "此仓库有 {n} 个终端正在后台运行，将一并结束。",
                n = terminals
            ));
        }
        confirm(
            tf!("从列表中移除 {name}？", name = &name),
            description,
            tr("移除"),
            terminals > 0,
            window,
            cx,
            move |cx| {
                app.update(cx, |app, cx| app.remove_repo(&entry, cx));
                crate::repo::end_parked_terminals(&entry, cx);
            },
        );
    }

    /// 一台服务器现在连没连上：圆点的颜色、一句话、能不能点「连接」、连不上的原因。
    fn conn_state(
        &self,
        key: &str,
        colors: Colors,
        cx: &App,
    ) -> (Hsla, SharedString, bool, Option<SharedString>) {
        let app = self.app.read(cx);
        let loaded = app.machine(key).map(|m| m.live);
        let off = colors.muted.opacity(0.6);
        match app.status(key) {
            ConnStatus::Idle => (off, tr("未连接").into(), true, None),
            ConnStatus::Connecting => (colors.accent, app.connecting_text(key), false, None),
            // 连过，但订阅断了（还没来得及重连）。重连没连上的不在这里：照下面写「连接失败」和原因，
            // 不然主机密钥变了这样的原因看不到
            ConnStatus::Ready(_) | ConnStatus::Failed(_)
                if loaded == Some(false) && !app.connect_failed(key) =>
            {
                (off, tr("已断开").into(), true, None)
            }
            ConnStatus::Ready(conn) => {
                let text = match crate::machines::split_relay(key) {
                    // kuaid 的版本是那台电脑上常驻的那个报的：和 Kuai 同一个版本号，连接时不是这一版
                    // 的会被换掉，只有别的电脑上更新的 Kuai 升过它时才比这边新。经由哪台在名字
                    // 后面写着（列表里、上面的小标签），这里不重复
                    Some(_) => tf!(
                        "已连接 · kuaid {version} · {ms} ms",
                        version = conn.hello.version,
                        ms = conn.connect_time.as_millis()
                    ),
                    None => tf!(
                        "已连接 · kuaid {version} · git {git} · {ms} ms{installed}",
                        version = conn.hello.version,
                        git = conn.hello.git.trim_start_matches("git version "),
                        ms = conn.connect_time.as_millis(),
                        installed = if conn.installed {
                            tr(" · 已安装 kuaid")
                        } else {
                            ""
                        }
                    ),
                };
                (colors.success, text.into(), false, None)
            }
            ConnStatus::Failed(err) => (colors.danger, tr("连接失败").into(), true, Some(err)),
        }
    }

    /// 左边列表里的一台：图标、名字，下面一行是连没连上、几个仓库。经由别的服务器连的缩进一格，
    /// 挂在那台下面。
    fn render_machine_row(
        &self,
        ix: usize,
        key: &str,
        on: bool,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (selected_bg, secondary, fg) = (
            theme.sidebar_accent,
            theme.secondary,
            theme.sidebar_foreground,
        );
        let local = key == LOCAL;
        let relay = crate::machines::split_relay(key).map(|(via, _)| via.to_string());
        let (name, via, dot, line) = {
            let app = self.app.read(cx);
            let count = app.repos_on(key).map(|repos| repos.len());
            let name = app.machine_name(key);
            // 经由哪台：淡淡写在名字后面，下面那一行和直接连的一样（写在下面那行太长，几个仓库被截掉）
            let via: Option<SharedString> = relay
                .as_ref()
                .map(|via| tf!("经由 {path}", path = app.machine_name(via)).into());
            match local {
                true => (
                    name,
                    via,
                    None,
                    SharedString::from(tf!("{n} 个仓库", n = count.unwrap_or(0))),
                ),
                false => {
                    let (dot, text, ..) = self.conn_state(key, colors, cx);
                    // 连着时那一句很长（版本、延迟），列表里只写有几个仓库
                    let line: SharedString = match (app.writable(key), count) {
                        (true, Some(n)) => tf!("已连接 · {n} 个仓库", n = n).into(),
                        _ => text,
                    };
                    (name, via, Some(dot), line)
                }
            }
        };
        let icon = if local {
            IconName::Monitor
        } else {
            IconName::Server
        };
        let id = key.to_string();
        h_flex()
            .id(("server-row", ix))
            .min_h(px(46.))
            .mx(px(6.))
            .when(relay.is_some(), |row| row.ml(px(24.)))
            .px(px(8.))
            .gap(px(10.))
            .rounded(px(ui::RADIUS))
            .cursor_pointer()
            .text_color(fg)
            .when(on, |row| row.bg(selected_bg))
            .when(!on, |row| row.hover(|s| s.bg(colors.hover)))
            .on_click(cx.listener(move |this, _, _, cx| this.select(id.clone(), cx)))
            .child(
                div()
                    .flex_none()
                    .size(px(28.))
                    .rounded(px(7.))
                    .bg(if on {
                        colors.accent.opacity(0.22)
                    } else {
                        secondary
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(ui::icon(icon).text_color(if on {
                        colors.accent
                    } else {
                        colors.muted
                    })),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap(px(6.))
                            .child(
                                ui::clipped(("server-name", ix), name)
                                    .flex_shrink(1.)
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM),
                            )
                            .children(via.map(|via| {
                                div()
                                    .flex_shrink(2.)
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(colors.muted)
                                    .child(via)
                            })),
                    )
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap(px(5.))
                            .children(
                                dot.map(|dot| {
                                    div().flex_none().size(px(6.)).rounded_full().bg(dot)
                                }),
                            )
                            .child(
                                ui::clipped(("server-line", ix), line)
                                    .text_xs()
                                    .text_color(colors.muted),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 左边：顶上一条（几台、几台连着），本机和各台服务器，最下面「添加服务器…」。
    fn render_side(
        &self,
        selected: &str,
        width: f32,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (panel, border) = (theme.sidebar, theme.sidebar_border);
        let keys = self.machine_keys(cx);
        let summary = {
            let app = self.app.read(cx);
            let live = keys.iter().filter(|key| app.writable(key)).count();
            tf!("{n} 台，{m} 台已连接", n = keys.len(), m = live)
        };
        let rows: Vec<AnyElement> = keys
            .iter()
            .enumerate()
            .map(|(ix, key)| self.render_machine_row(ix, key, key == selected, colors, cx))
            .collect();
        v_flex()
            .w(px(width))
            .flex_none()
            .relative()
            .h_full()
            .bg(panel)
            .border_r_1()
            .border_color(border)
            .child(
                h_flex()
                    .h(px(HEAD_HEIGHT))
                    .flex_none()
                    .px(px(12.))
                    .gap(px(10.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .flex_none()
                            .size(px(28.))
                            .rounded(px(7.))
                            .bg(colors.accent.opacity(0.14))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(ui::icon(IconName::Server).text_color(colors.accent)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                ui::clipped("servers-title", trc("页面", "主机"))
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                ui::clipped("servers-summary", summary)
                                    .text_size(px(11.))
                                    .text_color(colors.muted),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .id("server-rows")
                    .flex_1()
                    .min_h_0()
                    .pt(px(6.))
                    .pb(px(10.))
                    .gap(px(2.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                div()
                    .flex_none()
                    .p(px(8.))
                    .border_t_1()
                    .border_color(border)
                    .child(
                        Button::new("add-server")
                            .small()
                            .w_full()
                            .icon(ui::icon(IconName::Plus))
                            .label(tr("添加主机…"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.server_dialog(None, window, cx)
                            })),
                    ),
            )
            .child(crate::side_split::handle("servers-side-split", &self.app))
            .into_any_element()
    }

    /// 本机配置的服务器名字后面的小标签：是不是别的服务器的跳板机、经由哪几台连接、映射到本机的
    /// 目录。另外返回移除它时要多说的一句（有别的服务器经它连接）。
    fn server_tags(
        &self,
        key: &str,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> (Vec<AnyElement>, String) {
        let app = self.app.read(cx);
        let mut tags: Vec<AnyElement> = Vec::new();
        let route: Vec<String> = app
            .settings
            .route(key)
            .iter()
            .map(|jump| jump.name.clone())
            .collect();
        let users: Vec<String> = app
            .settings
            .jump_users(key)
            .iter()
            .map(|user| user.name.clone())
            .collect();
        let mut removal_note = String::new();
        if !users.is_empty() {
            tags.push(name_tag(
                format!("tag-jump:{key}"),
                None,
                tr("跳板机").into(),
                tf!("经此主机连接：{names}", names = users.join(tr("、"))).into(),
                false,
                colors,
            ));
            removal_note = tf!(
                "{names} 经此主机连接，移除后改为直接连接，需重新选择跳板机。\n",
                names = users.join(tr("、"))
            );
        }
        if !route.is_empty() {
            let path = route.join(" → ");
            tags.push(name_tag(
                format!("tag-via:{key}"),
                Some(IconName::CornerDownRight),
                tf!("经由 {path}", path = &path).into(),
                tf!("连接时依次经过：{path}", path = &path).into(),
                true,
                colors,
            ));
        }
        if let Some(server) = app.server(key).cloned() {
            for mapping in &server.mappings {
                tags.push(crate::mapping::chip(&self.app, &server, mapping, cx));
            }
        }
        (tags, removal_note)
    }

    /// 右边顶上：图标、名字和小标签，下面是状态（点「N 个工作区」到工作区页看那台的）、登录信息；
    /// 右边「连接」「编辑…」「⋯」。另外返回连不上的原因（单放一块，整段显示）。
    fn render_head(
        &self,
        key: &str,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> (AnyElement, Option<SharedString>) {
        let local = key == LOCAL;
        let relay = crate::machines::split_relay(key).map(|(via, _)| via.to_string());
        let (dot, text, retry, failure) = match local {
            true => {
                // 本机：电脑名，本机的终端开在本机的 kuaid 里，连上了写上它的版本
                let mut parts: Vec<String> = std::env::var("COMPUTERNAME").into_iter().collect();
                if let ConnStatus::Ready(conn) = self.app.read(cx).status(LOCAL) {
                    parts.push(format!("kuaid {}", conn.hello.version));
                }
                (None, SharedString::from(parts.join(" · ")), false, None)
            }
            false => {
                let (dot, text, retry, failure) = self.conn_state(key, colors, cx);
                (Some(dot), text, retry, failure)
            }
        };
        let (name, workspaces, login, via_name) = {
            let app = self.app.read(cx);
            // 经由别的服务器连的：工作区页的下拉里没有它，不给链接
            let workspaces = match relay {
                Some(_) => None,
                None => app.workspaces_on(key).map(|list| list.len()),
            };
            let login: Option<SharedString> = match &relay {
                Some(_) => app
                    .relayed_server(key)
                    .map(|server| server.destination.clone().into()),
                None => app.server(key).map(|server| {
                    let auto = match server.manual_connect {
                        true => tr("手动连接"),
                        false => tr("启动时自动连接"),
                    };
                    format!("{} · {auto}", server.destination).into()
                }),
            };
            let via_name = relay.as_deref().map(|via| app.machine_name(via));
            (app.machine_name(key), workspaces, login, via_name)
        };
        let machine = key.to_string();
        let link = workspaces.map(|n| {
            div()
                .id("server-workspaces")
                .flex_none()
                .text_color(colors.accent)
                .cursor_pointer()
                .hover(|s| s.underline())
                .child(tf!("{n} 个工作区", n = n))
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.emit(ManagerEvent::ShowWorkspaces(machine.clone()))
                }))
        });
        let status = h_flex()
            .gap(px(6.))
            .min_w_0()
            .text_xs()
            .text_color(colors.muted)
            .children(dot.map(|dot| div().flex_none().size(px(7.)).rounded_full().bg(dot)))
            .child(ui::clipped("server-status", text).flex_shrink(1.))
            .when_some(link, |row, link| row.child("·").child(link));
        let (tags, removal_note) = match (&via_name, local) {
            (Some(via), _) => {
                let tip = tf!("由 {via} 连接，连接设置需在 {via} 上修改。", via = via);
                (
                    vec![relay_tag(key, via.clone(), tip.into(), colors)],
                    String::new(),
                )
            }
            (None, true) => (Vec::new(), String::new()),
            (None, false) => self.server_tags(key, colors, cx),
        };
        let mut actions: Vec<AnyElement> = Vec::new();
        if retry {
            let id = key.to_string();
            actions.push(
                Button::new("server-connect")
                    .small()
                    .primary()
                    .icon(ui::icon(IconName::Plug))
                    .label(tr("连接"))
                    .on_click(cx.listener(move |this, _, _, cx| this.connect(&id, cx)))
                    .into_any_element(),
            );
        }
        // 本机配置的服务器才能在这里编辑、移除；经由别的服务器连的，设置保存在那台上
        if let Some(server) = self.app.read(cx).server(key).cloned() {
            let edit = server.clone();
            actions.push(
                Button::new("server-edit")
                    .small()
                    .label(tr("编辑…"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.server_dialog(Some(edit.clone()), window, cx)
                    }))
                    .into_any_element(),
            );
            let repo_count = self.app.read(cx).repos_on(key).map(|repos| repos.len());
            actions.push(self.server_menu(server, repo_count, removal_note));
        }
        let icon = if local {
            IconName::Monitor
        } else {
            IconName::Server
        };
        let head = h_flex()
            .items_start()
            .gap(px(12.))
            .child(
                div()
                    .flex_none()
                    .size(px(40.))
                    .rounded(px(9.))
                    .bg(colors.accent.opacity(0.14))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(icon).size_4().text_color(colors.accent)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(4.))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .min_w_0()
                            .child(
                                ui::clipped("server-title", name)
                                    .flex_shrink(1.)
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .children(tags),
                    )
                    .child(status)
                    .children(login.map(|login| {
                        ui::clipped("server-login", login)
                            .text_xs()
                            .text_color(colors.muted)
                    })),
            )
            .child(h_flex().flex_none().gap(px(6.)).children(actions))
            .into_any_element();
        (head, failure)
    }

    /// 仓库那一行名字后面的小标签：它在哪些工作区里（只显示，不在这里改），映射到本机的目录。
    fn repo_chips(&self, entry: &RepoEntry, colors: Colors, cx: &App) -> Vec<AnyElement> {
        let app = self.app.read(cx);
        let secondary = cx.theme().secondary;
        let hues = app.workspace_hues();
        let mut chips: Vec<AnyElement> = Vec::new();
        let mut machines: Vec<&String> = app.machines.keys().collect();
        machines.sort_by_key(|key| (key.as_str() != LOCAL, key.as_str()));
        for machine in machines {
            for (key, name, _) in app.workspaces_on(machine).unwrap_or_default() {
                if !app.members(&key).contains(entry) {
                    continue;
                }
                let tip: SharedString = match key.is_local() {
                    true => tf!("工作区「{name}」，保存在本机", name = &name).into(),
                    false => tf!(
                        "工作区「{name}」，保存在 {machine}",
                        name = &name,
                        machine = app.machine_name(&key.machine)
                    )
                    .into(),
                };
                let id = format!("ws-chip:{}:{}:{}", key.machine, key.id, entry.path);
                let hue = hues
                    .get(&key)
                    .map_or(colors.muted, |slot| ui::hue(*slot, cx));
                chips.push(
                    h_flex()
                        .id(ElementId::Name(id.into()))
                        .flex_none()
                        .max_w(px(140.))
                        .h(px(20.))
                        .px(px(7.))
                        .gap(px(4.))
                        .rounded(px(10.))
                        .bg(secondary)
                        .text_size(px(11.))
                        .text_color(colors.muted)
                        // 工作区的色块：和工作区页、会话页同一个颜色
                        .child(ui::hue_mark(hue, 6.))
                        .child(div().min_w_0().truncate().child(name))
                        .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
                        .into_any_element(),
                );
            }
        }
        // 服务器上的仓库：有映射包含它的话，写出它在本机的路径
        if let Some(local) = entry
            .server
            .as_deref()
            .and_then(|server| app.local_path(server, &entry.path))
        {
            chips.push(local_badge(local, colors).into_any_element());
        }
        chips
    }

    /// 「仓库」那一行：个数、一句说明，右边克隆、打开（本机是添加本机仓库）。
    fn render_repos_bar(
        &self,
        key: &str,
        count: usize,
        live: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let local = key == LOCAL;
        let muted = cx.theme().muted_foreground;
        let (clone_id, open_id) = (key.to_string(), key.to_string());
        let add = match local {
            true => Button::new("server-add-local")
                .small()
                .icon(ui::icon(IconName::FolderPlus))
                .label(tr("添加本机仓库…"))
                .on_click(cx.listener(|this, _, window, cx| this.add_local(window, cx))),
            false => Button::new("server-open-repo")
                .small()
                .disabled(!live)
                .icon(ui::icon(IconName::FolderSearch))
                .label(tr("打开仓库…"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_remote_dialog(open_id.clone(), window, cx)
                })),
        };
        h_flex()
            .gap(px(8.))
            .child(
                div()
                    .flex_none()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(trc("列表", "仓库")),
            )
            // 没连上时不知道有几个仓库：不写个数和说明（写 0 像是真的没有）
            .when(live, |bar| {
                bar.child(ui::count_pill(count, false, cx)).child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(muted)
                        .child(tr("点击仓库在标签页中打开")),
                )
            })
            .child(div().flex_1())
            .child(
                Button::new("server-clone")
                    .small()
                    .disabled(!live)
                    .icon(ui::icon(IconName::Download))
                    .label(tr("克隆…"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let server = (clone_id != LOCAL).then(|| clone_id.clone());
                        this.clone_dialog(server, window, cx)
                    })),
            )
            .child(add)
            .into_any_element()
    }

    /// 这次还没连上的服务器：不知道上面有什么（不做缓存，见 2.26 节），给一个「连接」。
    fn render_offline(
        &self,
        key: &str,
        connecting: bool,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fg = cx.theme().foreground;
        let id = key.to_string();
        v_flex()
            .items_center()
            .gap(px(8.))
            .py(px(44.))
            .text_color(colors.muted)
            .child(Icon::new(IconName::Plug).size_6())
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child(tr("尚未连接此主机")),
            )
            .child(div().text_xs().child(tr("连接后显示其中的仓库和工作区。")))
            .child(
                Button::new("server-connect-blank")
                    .small()
                    .primary()
                    .icon(ui::icon(IconName::Plug))
                    .label(tr("连接"))
                    .loading(connecting)
                    .disabled(connecting)
                    .on_click(cx.listener(move |this, _, _, cx| this.connect(&id, cx))),
            )
            .into_any_element()
    }

    /// 选中那台上的仓库：上面一行是「仓库」和按钮，下面是仓库和它们的子仓库（默认收着）。
    fn render_repos(
        &self,
        key: &str,
        colors: Colors,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let local = key == LOCAL;
        let (repos, live, writable, jump_only, connecting, open) = {
            let app = self.app.read(cx);
            (
                app.repos_on(key),
                local || app.machine(key).is_some_and(|m| m.live),
                app.writable(key),
                !local && !app.settings.jump_users(key).is_empty(),
                matches!(app.status(key), ConnStatus::Connecting),
                Rc::new(app.tabs().repos.clone()),
            )
        };
        let count = repos.as_ref().map_or(0, Vec::len);
        let bar = self.render_repos_bar(key, count, live, cx);
        let card = v_flex()
            .p(px(4.))
            .border_1()
            .border_color(colors.border)
            .rounded(px(10.))
            .bg(colors.panel);
        let Some(repos) = repos else {
            let blank = self.render_offline(key, connecting, colors, cx);
            return v_flex()
                .gap(px(8.))
                .child(bar)
                .child(card.child(blank))
                .into_any_element();
        };
        let this = cx.entity().downgrade();
        let menu_app = self.app.clone();
        let menu: MenuFor = Rc::new(
            move |menu: PopupMenu, entry: RepoEntry, sub: bool, window, cx| {
                let at = |this: &WeakEntity<ManagerView>| (this.clone(), entry.clone());
                let (open, rename, remove) = (at(&this), at(&this), at(&this));
                let path = entry.path.clone();
                let menu = menu.item(PopupMenuItem::new(tr("在标签页中打开")).on_click(
                    move |_, _, cx| {
                        open.0
                            .update(cx, |_, cx| cx.emit(ManagerEvent::Open(open.1.clone())))
                            .ok();
                    },
                ));
                // 不在任何工作区里是「添加到工作区 ▸」；在的话是「打开工作区」，后面跟「添加到其他工作区 ▸」
                let add = crate::ws_links::plain_add(&menu_app);
                let menu = crate::ws_links::context_items(
                    menu, &menu_app, &entry, None, true, &add, window, cx,
                );
                let menu = menu
                    .separator()
                    .item(
                        PopupMenuItem::new(tr("重命名…"))
                            .disabled(!writable)
                            .on_click(move |_, _, cx| {
                                rename
                                    .0
                                    .update(cx, |_, cx| {
                                        cx.emit(ManagerEvent::Rename(rename.1.clone()))
                                    })
                                    .ok();
                            }),
                    )
                    .item(
                        PopupMenuItem::new(tr("复制仓库路径")).on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                        }),
                    );
                if sub {
                    return menu;
                }
                // 映射到本机只对直接连的服务器
                let menu = match &entry.server {
                    Some(server) if crate::machines::split_relay(server).is_none() => {
                        let path = entry.path.clone();
                        crate::mapping::menu_items(menu, &menu_app, server, &path, true, &path, cx)
                    }
                    _ => menu,
                };
                menu.separator().item(
                    PopupMenuItem::new(tr("从列表中移除…"))
                        .disabled(!writable)
                        .on_click(move |_, window, cx| {
                            remove
                                .0
                                .update(cx, |this, cx| {
                                    this.confirm_remove(remove.1.clone(), window, cx)
                                })
                                .ok();
                        }),
                )
            },
        );
        let handlers: Handlers<Self> = Handlers {
            open: Rc::new(|_, entry, _, cx| cx.emit(ManagerEvent::Open(entry))),
            fold: Rc::new(|this, entry, _, cx| this.toggle_subs(entry, cx)),
            terms: Some(Rc::new(|_, entry, _, cx| {
                cx.emit(ManagerEvent::Terminals(entry))
            })),
            menu: Some(menu),
        };
        let mut blocks: Vec<(Block, Vec<AnyElement>)> = Vec::new();
        for (ix, entry) in repos.iter().enumerate() {
            let block = {
                let (app, scan, board) =
                    (self.app.read(cx), self.scan.read(cx), self.board.read(cx));
                let scanned = scan.get(entry).cloned();
                Block {
                    entry: entry.clone(),
                    name: app.repo_name(entry),
                    aside: None,
                    sub_names: crate::repo_rows::sub_names(app, entry, scanned.as_ref()),
                    scanned,
                    loading: scan.pending(entry),
                    offline: !live,
                    folded: !app.settings.unfolded_subs.contains(entry),
                    open: open.clone(),
                    terms: crate::workspace_page::terminal_summary(board, entry),
                    chips: self.repo_chips(entry, colors, cx),
                    hit: None,
                    hit_at: None,
                }
            };
            let (rename, remove) = (entry.clone(), entry.clone());
            let mut actions = vec![
                Button::new(("repo-rename", ix))
                    .ghost()
                    .xsmall()
                    .disabled(!writable)
                    .icon(ui::icon(IconName::Pencil))
                    .tooltip(tr("改显示名称"))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(ManagerEvent::Rename(rename.clone()));
                    }))
                    .into_any_element(),
            ];
            if let Some(server) = entry
                .server
                .clone()
                .filter(|server| crate::machines::split_relay(server).is_none())
            {
                let (map_app, path) = (self.app.clone(), entry.path.clone());
                actions.push(
                    Button::new(("repo-map", ix))
                        .ghost()
                        .xsmall()
                        .icon(ui::icon(IconName::Link))
                        .tooltip(tr("映射到本机…"))
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            crate::mapping::map_dialog(
                                map_app.clone(),
                                server.clone(),
                                path.clone(),
                                None,
                                window,
                                cx,
                            )
                        })
                        .into_any_element(),
                );
            }
            actions.push(
                Button::new(("repo-remove", ix))
                    .ghost()
                    .xsmall()
                    .disabled(!writable)
                    .icon(ui::icon(IconName::X))
                    .tooltip(tr("从列表中移除"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.confirm_remove(remove.clone(), window, cx);
                    }))
                    .into_any_element(),
            );
            blocks.push((block, actions));
        }
        let columns = Columns::for_blocks(&blocks, compact);
        let blocks: Vec<AnyElement> = blocks
            .into_iter()
            .enumerate()
            .map(|(ix, (block, actions))| render_block(ix, block, actions, columns, &handlers, cx))
            .collect();
        let empty = blocks.is_empty().then(|| {
            div()
                .px(px(10.))
                .py(px(14.))
                .text_sm()
                .text_color(colors.muted)
                .child(match (local, jump_only) {
                    (true, _) => tr("暂无本机仓库。请点击「添加本机仓库…」或「克隆…」。"),
                    (false, true) => tr("仅用作跳板机，暂无此主机上的仓库。"),
                    (false, false) => tr("暂无此主机上的仓库。请点击「打开仓库…」选择。"),
                })
        });
        v_flex()
            .gap(px(8.))
            .child(bar)
            .child(card.children(empty).children(blocks))
            .into_any_element()
    }

    /// 本机配置的服务器右上角的 ⋯：映射目录到本机、移除。
    fn server_menu(
        &self,
        server: Server,
        repo_count: Option<usize>,
        removal_note: String,
    ) -> AnyElement {
        let menu_app = self.app.clone();
        Button::new(SharedString::from(format!("more-{}", server.id)))
            .small()
            .ghost()
            .icon(ui::icon(IconName::Ellipsis))
            .tooltip(tr("映射或移除此主机"))
            .dropdown_menu(move |menu, _, _| {
                let (app, remove) = (menu_app.clone(), server.clone());
                let removal_note = removal_note.clone();
                let (map_app, map_server) = (menu_app.clone(), server.clone());
                menu.item(PopupMenuItem::new(tr("映射目录到本机…")).on_click(
                    move |_, window, cx| {
                        crate::mapping::map_dialog(
                            map_app.clone(),
                            map_server.id.clone(),
                            map_server.start_dir.clone().unwrap_or_default(),
                            None,
                            window,
                            cx,
                        )
                    },
                ))
                .separator()
                .item(
                    PopupMenuItem::new(tr("移除…")).on_click(move |_, window, cx| {
                        let (app, id) = (app.clone(), remove.id.clone());
                        // 仓库列表保存在服务器上，不跟着没；这里只是不再连它
                        let repos = match repo_count {
                            Some(n) if n > 0 => tf!(
                                "该主机上的 {n} 个仓库将不再列出，已打开的标签页将关闭。",
                                n = n
                            ),
                            _ => String::new(),
                        };
                        confirm(
                            tf!("移除主机 {name}？", name = remove.name),
                            tf!(
                                "{destination}\n{jump}{repos}主机上的内容不受影响。",
                                destination = remove.destination,
                                jump = &removal_note,
                                repos = repos
                            ),
                            tr("移除主机"),
                            true,
                            window,
                            cx,
                            move |cx| app.update(cx, |app, cx| app.remove_server(&id, cx)),
                        );
                    }),
                )
            })
            .into_any_element()
    }
}

impl Render for ManagerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let colors = Colors {
            muted: theme.muted_foreground,
            hover: theme.list_hover,
            border: theme.border,
            danger: theme.danger,
            success: theme.success,
            accent: theme.primary,
            panel: theme.sidebar,
        };
        let background = theme.background;
        let viewport = window.viewport_size().width.as_f32();
        let width = crate::repo::sidebar_open(&self.app.read(cx).settings.layout, viewport);
        let selected = self.selected(cx);
        let side = self.render_side(&selected, width, colors, cx);
        let (head, failure) = self.render_head(&selected, colors, cx);
        let repos = self.render_repos(
            &selected,
            colors,
            viewport < crate::repo_rows::COMPACT_BELOW,
            cx,
        );
        h_flex()
            .size_full()
            .bg(background)
            // 拖左边那一栏的分隔条，见 side_split
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<crate::side_split::DragSide>, window, cx| {
                    crate::side_split::drag(&this.app, event, window, cx);
                    cx.notify();
                },
            ))
            .child(side)
            .child(
                div()
                    .id("server-main")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .child(
                        // 窗口宽的时候内容居中，不然右边空一大块
                        v_flex()
                            .w_full()
                            .max_w(px(960.))
                            .mx_auto()
                            .px(px(28.))
                            .py(px(24.))
                            .gap(px(16.))
                            .child(head)
                            .children(failure.map(|failure| {
                                div()
                                    .px(px(14.))
                                    .py(px(8.))
                                    .rounded(px(8.))
                                    .bg(colors.danger.opacity(0.08))
                                    .text_xs()
                                    .text_color(colors.danger)
                                    .whitespace_normal()
                                    .child(failure)
                            }))
                            .child(repos),
                    ),
            )
    }
}
