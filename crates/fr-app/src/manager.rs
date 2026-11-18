//! 仓库管理器：本机仓库和各台服务器上的仓库；添加本机仓库、添加和编辑服务器、打开服务器上的仓库。
//!
//! 最上面一张卡片是当前工作区的仓库（开着的、没开的分开列），手滑关掉的在这里找。
//! 下面的「全部仓库」可以收起：本机和每台服务器各是一张卡片，顶上一条写名字、连接状态和能做的事，
//! 下面列仓库，点一行打开。服务器多了列表很长，每张卡片点顶上那一条也能单独收起（记在设置里）。
//! 移除仓库、移除服务器都先问一下；服务器的「编辑」「移除」收在行尾的 ⋯ 菜单里，不和常用按钮挨着。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use fr_core::i18n::{tr, trc};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
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
use crate::machines::{LOCAL, WorkspaceKey};
use crate::settings::{Auth, LOCAL_CARD, RepoEntry, Server, Target};
use crate::ui;

pub enum ManagerEvent {
    Open(RepoEntry),
    /// 给这个仓库改名（对话框由主窗口开）
    Rename(RepoEntry),
    /// 把这个仓库从当前工作区移除（主窗口来问，开着的标签页也由它关）
    RemoveFromWorkspace(RepoEntry),
    /// 打开「快速打开」
    QuickOpen,
    /// 切到这个工作区（服务器卡片上的「3 个工作区」）
    SwitchTo(WorkspaceKey),
}

pub struct ManagerView {
    app: Entity<AppState>,
    _observe: Subscription,
}

impl EventEmitter<ManagerEvent> for ManagerView {}

impl ManagerView {
    pub fn new(app: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&app, |_, _, cx| cx.notify());
        Self {
            app,
            _observe: observe,
        }
    }

    fn open(&mut self, entry: RepoEntry, cx: &mut Context<Self>) {
        cx.emit(ManagerEvent::Open(entry));
    }

    /// 各开一个标签页（打开时加进当前工作区和仓库所在那台电脑的列表），按给的先后，最后一个是选中的。
    fn add_and_open(&mut self, entries: Vec<RepoEntry>, cx: &mut Context<Self>) {
        for entry in entries {
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
    fn server_dialog(&mut self, edit: Option<Server>, window: &mut Window, cx: &mut Context<Self>) {
        let editing = edit.is_some();
        let old = edit.unwrap_or_default();
        let target = Target::parse(&old.destination);
        // 这台服务器存着密码或口令（是哪一种，看它原来的登录方式）
        let had_secret = editing && crate::secrets::get(&old.id).is_some();
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
        // 点了「清除」：存着的密码、口令不要了
        let forget = Rc::new(Cell::new(false));
        // 没填对时的提示，显示在对话框里
        let problem: Rc<RefCell<Option<SharedString>>> = Rc::default();
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                host.focus_handle(cx).focus(window, cx);
            }
            let (muted, danger) = (cx.theme().muted_foreground, cx.theme().danger);
            let submit = {
                let (host, port, user, key) =
                    (host.clone(), port.clone(), user.clone(), key.clone());
                let (passphrase, password, name) =
                    (passphrase.clone(), password.clone(), name.clone());
                let start_dir = start_dir.clone();
                let (app, auth, forward_agent) =
                    (app.clone(), auth.clone(), forward_agent.clone());
                let (forget, problem, old) = (forget.clone(), problem.clone(), old.clone());
                let jump = jump.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
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
                    let fail = |message: &'static str, window: &mut Window| {
                        *problem.borrow_mut() = Some(message.into());
                        window.refresh();
                    };
                    // 主机那一格里贴进整个 user@host:端口 的也认；单独填了的用户名、端口优先
                    let mut target = Target::parse(&host);
                    if !user.trim().is_empty() {
                        target.user = user.trim().to_string();
                    }
                    if !port.trim().is_empty() {
                        target.port = port.trim().to_string();
                    }
                    if target.host.is_empty() {
                        return fail(tr("请填写主机"), window);
                    }
                    if !target.port.is_empty() && !target.port_ok() {
                        return fail(tr("端口须为 1～65535 之间的数字"), window);
                    }
                    let key = key.trim();
                    if auth == Auth::Key && !key.is_empty() {
                        if key.to_lowercase().ends_with(".pub") {
                            return fail(tr("这是公钥文件，请选择不带 .pub 后缀的私钥文件"), window);
                        }
                        // ~ 开头的由 ssh 自己展开，这里不查
                        if !key.starts_with('~') && !std::path::Path::new(key).is_file() {
                            return fail(tr("未找到私钥文件"), window);
                        }
                    }
                    let mut label = name.trim().to_string();
                    if label.is_empty() {
                        label = target.host.clone();
                    }
                    let server = Server {
                        id: old.id.clone(),
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
                        // 映射不在这张表单里，`update_server` 留着原来的
                        mappings: Vec::new(),
                    };
                    // 存着的密码、口令：填了新的就换；没填、登录方式没变、也没点清除，就留着
                    let keep = had_secret && auth == old.auth && !forget.get();
                    let secret_changed = !secret.is_empty() || (had_secret && !keep);
                    let account = server.destination.clone();
                    window.close_dialog(cx);
                    let (id, reconnect) = app.update(cx, |app, cx| {
                        if editing {
                            let id = server.id.clone();
                            (id, app.update_server(server, secret_changed, cx))
                        } else {
                            (app.add_server(server, cx), true)
                        }
                    });
                    if !secret.is_empty() {
                        if let Err(err) = crate::secrets::set(&id, &account, &secret) {
                            window.push_notification(
                                Notification::error(tf!("密码保存失败，连接时需重新输入：{err}", err)),
                                cx,
                            );
                        }
                    } else if had_secret && !keep {
                        crate::secrets::delete(&id);
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
                let saved = had_secret && auth_now == old_auth && !forget.get();
                let clear = saved.then(|| {
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
                                .font_family("Consolas")
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
                                return menu.separator().label(tr("暂无其他已配置的服务器"));
                            }
                            menu = menu.separator().label(tr("经由已配置的服务器"));
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
                                                    .font_family("Consolas")
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
                    tr("目标服务器").into()
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
                    tr("编辑服务器")
                } else {
                    tr("添加服务器")
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
                                        .child(field(tr("主机"), Input::new(&host))),
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
                                .label(tr("转发本机的 ssh-agent（仅对受信任的服务器启用）"))
                                .checked(forward_agent.get())
                                .on_click({
                                    let forward_agent = forward_agent.clone();
                                    move |checked, window, _| {
                                        forward_agent.set(*checked);
                                        window.refresh();
                                    }
                                }),
                        )
                        .when_some(problem.borrow().clone(), |body, found| {
                            body.child(div().text_sm().text_color(danger).child(found))
                        })
                        // 这句说的是上面的登录信息，紧跟着它们；映射是单独的一节，放在最后
                        .child(div().text_xs().text_color(muted).child(if jump_now.is_some() {
                            tr(
                                "密码和私钥口令保存在 Windows 凭据管理器中。首次连接时在服务器的 ~/.kuai/ 中安装 kuaid，跳板机上不安装。",
                            )
                        } else {
                            tr(
                                "密码和私钥口令保存在 Windows 凭据管理器中。首次连接时在服务器的 ~/.kuai/ 中安装 kuaid。",
                            )
                        }))
                        .children(mapping_section),
                )
                .footer(
                    DialogFooter::new()
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
                this.open(entry.clone(), cx);
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

    /// 卡片顶上那一条的收起、展开：现在收没收起，和点了要做的事。
    fn fold(&self, key: &str, collapsed: bool, cx: &mut Context<Self>) -> Fold {
        let key = key.to_string();
        let toggle = cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.app.update(cx, |app, cx| app.toggle_card(&key, cx));
        });
        (collapsed, Box::new(toggle))
    }

    /// 工作区卡片里的一行：点了打开（已经开着就切过去）。`recent` 是刚关掉的，行尾标出来。
    fn render_member(
        &self,
        entry: &RepoEntry,
        open: bool,
        recent: bool,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let app = self.app.read(cx);
        let id = SharedString::from(format!(
            "member:{}:{}",
            entry.server.as_deref().unwrap_or(""),
            entry.path
        ));
        // 本机和各台服务器的混在一起列：服务器上的在名字后面写上是哪台，和标签页、快速打开里一个写法
        let name = app.repo_title(entry);
        let place: SharedString = entry.path.clone().into();
        let glyph = if entry.server.is_some() {
            IconName::Server
        } else {
            IconName::FolderGit2
        };
        // 服务器上的仓库：有映射包含它的话，和服务器卡片里一样写出它在本机的路径
        let local = entry
            .server
            .as_deref()
            .and_then(|server| app.local_path(server, &entry.path));
        // 经服务器连的：行尾写明经由哪台
        let relay = entry.server.as_deref().and_then(|key| {
            let (via, _) = crate::machines::split_relay(key)?;
            let via = app.machine_name(via);
            let tip = tf!(
                "由 {via} 连接 {name}",
                via = &via,
                name = app.machine_name(key)
            );
            Some(relay_tag(&id, via, tip.into(), colors))
        });
        // 工作区所在的电脑断开着：只能看，不能从工作区移除
        let writable = app.writable(&app.current().machine);
        let (open_entry, leave, menu_entry) = (entry.clone(), entry.clone(), entry.clone());
        let (view, menu_app) = (cx.entity().downgrade(), self.app.clone());
        h_flex()
            .id(ElementId::Name(id))
            .h(px(46.))
            .px(px(10.))
            .gap(px(12.))
            .rounded(px(7.))
            .cursor_pointer()
            .hover(|s| s.bg(colors.hover))
            .on_click(cx.listener(move |this, _, _, cx| this.open(open_entry.clone(), cx)))
            .child(
                ui::icon(glyph)
                    .flex_none()
                    .text_color(if open { colors.accent } else { colors.muted }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        ui::clipped("name", name)
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM),
                    )
                    .child(
                        ui::clipped("path", place)
                            .text_xs()
                            .text_color(colors.muted),
                    ),
            )
            .when_some(local, |row, local| row.child(local_badge(local, colors)))
            .children(relay)
            .when(recent, |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(colors.muted)
                        .child(tr("最近关闭")),
                )
            })
            .child(
                Button::new("leave")
                    .ghost()
                    .xsmall()
                    .disabled(!writable)
                    .icon(ui::icon(IconName::CircleMinus).text_color(colors.muted))
                    .tooltip(tr("从工作区移除"))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(ManagerEvent::RemoveFromWorkspace(leave.clone()));
                    })),
            )
            .context_menu(move |menu, _, cx| {
                let (leave_view, rename_view) = (view.clone(), view.clone());
                let (leave, rename) = (menu_entry.clone(), menu_entry.clone());
                let path = menu_entry.path.clone();
                let menu = menu
                    .item(
                        PopupMenuItem::new(tr("从工作区移除"))
                            .disabled(!writable)
                            .on_click(move |_, _, cx| {
                                leave_view
                                    .update(cx, |_, cx| {
                                        cx.emit(ManagerEvent::RemoveFromWorkspace(leave.clone()))
                                    })
                                    .ok();
                            }),
                    )
                    .separator()
                    .item(PopupMenuItem::new(tr("重命名…")).on_click(move |_, _, cx| {
                        rename_view
                            .update(cx, |_, cx| cx.emit(ManagerEvent::Rename(rename.clone())))
                            .ok();
                    }))
                    .item(
                        PopupMenuItem::new(tr("复制仓库路径")).on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                        }),
                    );
                // 服务器上的仓库：和服务器卡片里的行一样，有映射相关的几项
                crate::mapping::entry_items(menu, &menu_app, &menu_entry, cx)
            })
            .into_any_element()
    }

    /// 最上面那张卡片：当前工作区的仓库。开着的在上（按标签页的顺序），没开的在下（刚关掉的排前）。
    fn render_workspace_card(&self, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let app = self.app.read(cx);
        let key = app.current().clone();
        let view = app.view();
        let members = app.members(&key);
        let name = app.workspace_name(&key);
        let subtitle: SharedString = match key.is_local() {
            true => tr("当前工作区").into(),
            false => tf!("{name} 上的工作区", name = app.machine_name(&key.machine)).into(),
        };
        // 刚关掉的：已经移出工作区的不算
        let closed: Vec<RepoEntry> = app.closed_tabs();
        let idle = members
            .iter()
            .filter(|e| !view.tabs.contains(e) && !closed.contains(e))
            .cloned();
        let away: Vec<(RepoEntry, bool)> = closed
            .iter()
            .cloned()
            .map(|e| (e, true))
            .chain(idle.map(|e| (e, false)))
            .collect();
        let (open_count, away_count) = (view.tabs.len(), away.len());
        let mut rows: Vec<AnyElement> = Vec::new();
        if open_count > 0 {
            rows.push(group_label(tf!("已打开 · {n}", n = open_count), colors));
            for entry in &view.tabs {
                rows.push(self.render_member(entry, true, false, colors, cx));
            }
        }
        if away_count > 0 {
            rows.push(group_label(tf!("未打开 · {n}", n = away_count), colors));
            for (entry, recent) in away {
                rows.push(self.render_member(&entry, false, recent, colors, cx));
            }
        }
        card(
            card_header(
                "workspace",
                IconName::Layers,
                name,
                Vec::new(),
                Some(subtitle),
                status_line(
                    "workspace",
                    None,
                    // 空的工作区只说没有仓库，不再跟一句「0 个开着」
                    if open_count + away_count == 0 {
                        tf!("{n} 个仓库", n = 0usize).into()
                    } else {
                        tf!(
                            "{n} 个仓库，{open} 个已打开",
                            n = open_count + away_count,
                            open = open_count
                        )
                        .into()
                    },
                    colors,
                ),
                vec![
                    Button::new("quick-open")
                        .small()
                        .icon(ui::icon(IconName::Search))
                        .label(tr("快速打开…"))
                        .tooltip("Ctrl+P")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(ManagerEvent::QuickOpen)))
                        .into_any_element(),
                ],
                None,
                colors,
            ),
            None,
            rows,
            tr("此工作区中暂无仓库。打开的仓库将归入此工作区。"),
            false,
            colors,
        )
    }

    /// 列表里的一行。`writable` 为假（那台电脑断开着）时不能改名、移除。
    fn render_repo(
        &self,
        entry: &RepoEntry,
        writable: bool,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = SharedString::from(format!(
            "repo:{}:{}",
            entry.server.as_deref().unwrap_or(""),
            entry.path
        ));
        let open = entry.clone();
        let remove = entry.clone();
        let (rename, menu_entry) = (entry.clone(), entry.clone());
        let name = self.app.read(cx).repo_name(entry);
        let remove_name = name.clone();
        // 服务器的列表保存在那台服务器上：移除了别的电脑上也跟着没了
        let shared_on = entry
            .server
            .as_deref()
            .map(|server| self.app.read(cx).machine_name(server));
        let view = cx.entity().downgrade();
        // 服务器上的仓库：有映射包含它的话，它在本机的路径
        let local = entry
            .server
            .as_deref()
            .and_then(|server| self.app.read(cx).local_path(server, &entry.path));
        let (menu_app, menu_server) = (self.app.clone(), entry.server.clone());
        h_flex()
            .id(ElementId::Name(id))
            .h(px(46.))
            .px(px(10.))
            .gap(px(12.))
            .rounded(px(7.))
            .cursor_pointer()
            .hover(|s| s.bg(colors.hover))
            .on_click(cx.listener(move |this, _, _, cx| this.open(open.clone(), cx)))
            .child(
                ui::icon(IconName::FolderGit2)
                    .flex_none()
                    .text_color(colors.muted),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        ui::clipped("name", name)
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM),
                    )
                    .child(
                        ui::clipped("path", entry.path.clone())
                            .text_xs()
                            .text_color(colors.muted),
                    ),
            )
            .when_some(local, |row, local| row.child(local_badge(local, colors)))
            // 「改名」「移除」一直在行尾，用次要的颜色，不抢眼
            .child(
                Button::new("rename")
                    .ghost()
                    .xsmall()
                    .disabled(!writable)
                    .icon(ui::icon(IconName::Pencil).text_color(colors.muted))
                    .tooltip(tr("重命名"))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(ManagerEvent::Rename(rename.clone()));
                    })),
            )
            .child(
                Button::new("remove")
                    .ghost()
                    .xsmall()
                    .disabled(!writable)
                    .icon(ui::icon(IconName::X).text_color(colors.muted))
                    .tooltip(tr("从列表中移除"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        let (app, entry) = (this.app.clone(), remove.clone());
                        let mut description = tf!(
                            "{path}\n仅从列表中移除，仓库本身不受影响。",
                            path = remove.path
                        );
                        if let Some(machine) = &shared_on {
                            description.push('\n');
                            description.push_str(&tf!(
                                "此列表保存在 {machine} 上，所有电脑将同步移除。",
                                machine = machine
                            ));
                        }
                        // 关掉的标签页留下的本机终端：移除以后再也打不开，一起结束
                        let terminals = crate::repo::parked_terminals_of(&remove, cx);
                        if terminals > 0 {
                            description.push('\n');
                            description.push_str(&tf!(
                                "此仓库有 {n} 个终端正在后台运行，将一并结束。",
                                n = terminals
                            ));
                        }
                        confirm(
                            tf!("从列表中移除 {name}？", name = &remove_name),
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
                    })),
            )
            .context_menu(move |menu, _, cx| {
                let (view, entry) = (view.clone(), menu_entry.clone());
                let path = entry.path.clone();
                let menu = menu
                    .item(
                        PopupMenuItem::new(tr("重命名…"))
                            .disabled(!writable)
                            .on_click(move |_, _, cx| {
                                view.update(cx, |_, cx| {
                                    cx.emit(ManagerEvent::Rename(entry.clone()))
                                })
                                .ok();
                            }),
                    )
                    .item(
                        PopupMenuItem::new(tr("复制仓库路径")).on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                        }),
                    );
                // 映射到本机只对直接连的服务器
                match &menu_server {
                    Some(server) if crate::machines::split_relay(server).is_none() => {
                        let path = menu_entry.path.clone();
                        crate::mapping::menu_items(menu, &menu_app, server, &path, true, &path, cx)
                    }
                    _ => menu,
                }
            })
            .into_any_element()
    }
}

/// 经服务器连的（工作区卡片的行尾、服务器卡片的名字后面）：「经由 156」。
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
                .font_family("Consolas")
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
    warning: Hsla,
    accent: Hsla,
    /// 卡片顶上那一条的底色
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
    /// 本机那张卡片：本机的仓库。
    fn render_local_card(&self, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let settings = &self.app.read(cx).settings;
        let local_collapsed = settings.card_collapsed(LOCAL_CARD);
        let repos = self.app.read(cx).repos_on(LOCAL).unwrap_or_default();
        let local: Vec<AnyElement> = repos
            .iter()
            .map(|r| self.render_repo(r, true, colors, cx))
            .collect();
        // 本机的终端开在本机的 kuaid 里：连上了就写上它的版本（和服务器那张卡片一样）
        let count = tf!("{n} 个仓库", n = local.len());
        let status: SharedString = match self.app.read(cx).status(LOCAL) {
            ConnStatus::Ready(conn) => format!("{count} · kuaid {}", conn.hello.version).into(),
            _ => count.into(),
        };
        card(
            card_header(
                "local",
                IconName::Monitor,
                tr("本机").into(),
                Vec::new(),
                None,
                status_line("local", None, status, colors),
                vec![
                    Button::new("clone-local")
                        .small()
                        .icon(ui::icon(IconName::Download))
                        .label(tr("克隆…"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.clone_dialog(None, window, cx)),
                        )
                        .into_any_element(),
                    Button::new("add-local")
                        .small()
                        .icon(ui::icon(IconName::FolderPlus))
                        .label(tr("添加本机仓库…"))
                        .on_click(cx.listener(|this, _, window, cx| this.add_local(window, cx)))
                        .into_any_element(),
                ],
                Some(self.fold(LOCAL_CARD, local_collapsed, cx)),
                colors,
            ),
            None,
            local,
            tr("暂无本机仓库。请点击「添加本机仓库…」或「克隆…」。"),
            local_collapsed,
            colors,
        )
    }

    /// 一台服务器的卡片。`key` 是本机配置的服务器的 id，或者经服务器连的（`156>91`）：
    /// 后者的连接设置保存在那台服务器上，这里只能连接、打开，不能映射、编辑、移除。
    /// 仓库列表保存在服务器上，连上了才有。
    fn render_server_card(&self, key: &str, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let app = self.app.read(cx);
        let relay = crate::machines::split_relay(key);
        let (name, destination): (SharedString, SharedString) = match relay {
            Some(_) => {
                let server = app.relayed_server(key);
                (
                    app.machine_name(key),
                    server.map_or_else(SharedString::default, |s| s.destination.clone().into()),
                )
            }
            None => {
                let server = app.server(key);
                (
                    app.machine_name(key),
                    server.map_or_else(SharedString::default, |s| s.destination.clone().into()),
                )
            }
        };
        let status = app.status(key);
        let loaded = app.machine(key).map(|m| m.live);
        let writable = app.writable(key);
        let repos = app.repos_on(key);
        let rows: Vec<AnyElement> = repos
            .iter()
            .flatten()
            .map(|r| self.render_repo(r, writable, colors, cx))
            .collect();
        let repo_count = repos.as_ref().map(Vec::len);
        let app = self.app.read(cx);
        // 状态：一个小圆点加一句话；连不上的原因可能很长，单放一块，不截断
        let mut failure: Option<SharedString> = None;
        let via = relay.map(|(via, _)| app.machine_name(via));
        let (dot, status_text, retry): (Option<Hsla>, SharedString, bool) = match &status {
            ConnStatus::Idle => (Some(colors.muted.opacity(0.6)), tr("未连接").into(), true),
            ConnStatus::Connecting => (Some(colors.accent), app.connecting_text(key), false),
            // 连着，但订阅断了（还没来得及重连）
            ConnStatus::Ready(_) if loaded == Some(false) => {
                (Some(colors.muted.opacity(0.6)), tr("已断开").into(), true)
            }
            ConnStatus::Ready(conn) => (
                Some(colors.success),
                match &via {
                    // kuaid 的版本是那台电脑上常驻的那个报的：和 Kuai 同一个版本号，连接时不是这一版
                    // 的会被换掉，只有别的电脑上更新的 Kuai 升过它时才比这边新
                    Some(via) => tf!(
                        "已连接 · 经由 {via} · kuaid {version} · {ms} ms",
                        via = via,
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
                }
                .into(),
                false,
            ),
            ConnStatus::Failed(_) if loaded == Some(false) => {
                (Some(colors.muted.opacity(0.6)), tr("已断开").into(), true)
            }
            ConnStatus::Failed(err) => {
                failure = Some(err.clone());
                (Some(colors.danger), tr("连接失败").into(), true)
            }
        };
        // 收起以后看不到下面的仓库：状态后面写上有几个
        let folded = app.settings.card_collapsed(key);
        let status_text: SharedString = match (folded, repo_count) {
            (true, Some(n)) => format!("{status_text} · {}", tf!("{n} 个仓库", n)).into(),
            _ => status_text,
        };
        let mut tags: Vec<AnyElement> = Vec::new();
        if let Some(via) = &via {
            let tip = tf!("由 {via} 连接，连接设置需在 {via} 上修改。", via = via);
            tags.push(relay_tag(key, via.clone(), tip.into(), colors));
        }
        // 本机视角里，有工作区的服务器：点开可以切到它的工作区
        if relay.is_none()
            && app.current().is_local()
            && let Some(workspaces) = app.workspaces_on(key).filter(|list| !list.is_empty())
        {
            tags.push(self.workspaces_tag(key, workspaces, colors, cx));
        }
        let app = self.app.read(cx);
        let mut users: Vec<String> = Vec::new();
        let mut removal_note = String::new();
        if relay.is_none() {
            // 名字后面的标签：它经哪几台连接；有没有别的服务器经它连接
            let route: Vec<String> = app
                .settings
                .route(key)
                .iter()
                .map(|jump| jump.name.clone())
                .collect();
            users = app
                .settings
                .jump_users(key)
                .iter()
                .map(|user| user.name.clone())
                .collect();
            if !users.is_empty() {
                tags.push(name_tag(
                    format!("tag-jump:{key}"),
                    None,
                    tr("跳板机").into(),
                    tf!("经此服务器连接：{names}", names = users.join("、")).into(),
                    false,
                    colors,
                ));
                removal_note = tf!(
                    "{names} 经此服务器连接，移除后改为直接连接，需重新选择跳板机。\n",
                    names = users.join("、")
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
        }
        let (id_open, id_retry, id_clone) = (key.to_string(), key.to_string(), key.to_string());
        let mut actions: Vec<AnyElement> = Vec::new();
        if retry {
            actions.push(
                Button::new(SharedString::from(format!("connect-{key}")))
                    .small()
                    .ghost()
                    .icon(ui::icon(IconName::Plug))
                    .label(tr("连接"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.app.update(cx, |app, cx| {
                            drop(app.connect(&id_retry, cx));
                        });
                    }))
                    .into_any_element(),
            );
        }
        actions.push(
            Button::new(SharedString::from(format!("clone-{key}")))
                .small()
                .disabled(loaded == Some(false))
                .icon(ui::icon(IconName::Download))
                .label(tr("克隆…"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.clone_dialog(Some(id_clone.clone()), window, cx)
                }))
                .into_any_element(),
        );
        actions.push(
            Button::new(SharedString::from(format!("open-{key}")))
                .small()
                .disabled(loaded == Some(false))
                .icon(ui::icon(IconName::FolderSearch))
                .label(tr("打开仓库…"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_remote_dialog(id_open.clone(), window, cx)
                }))
                .into_any_element(),
        );
        if relay.is_none()
            && let Some(server) = self.app.read(cx).server(key).cloned()
        {
            actions.push(self.server_menu(server, repo_count, removal_note, cx));
        }
        let empty = match (&repos, users.is_empty()) {
            (None, _) => tr("连接后显示此服务器上的仓库。"),
            (Some(_), true) => tr("暂无此服务器上的仓库。请点击「打开仓库…」选择。"),
            (Some(_), false) => tr("仅用作跳板机，暂无此服务器上的仓库。"),
        };
        card(
            card_header(
                key,
                IconName::Server,
                name,
                tags,
                Some(destination),
                status_line(key, dot, status_text, colors),
                actions,
                Some(self.fold(key, folded, cx)),
                colors,
            ),
            failure,
            rows,
            empty,
            folded,
            colors,
        )
    }

    /// 服务器名字后面的「3 个工作区」：点开列出它上面的工作区，选一个切过去。
    fn workspaces_tag(
        &self,
        key: &str,
        workspaces: Vec<(crate::machines::WorkspaceKey, String, usize)>,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = tf!("{n} 个工作区", n = workspaces.len());
        let tip = tf!(
            "切换到 {name} 上的工作区",
            name = self.app.read(cx).machine_name(key)
        );
        let view = cx.entity().downgrade();
        Button::new(SharedString::from(format!("workspaces-{key}")))
            .ghost()
            .xsmall()
            .icon(ui::icon(IconName::Layers).text_color(colors.accent))
            .label(label)
            .tooltip(tip)
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for (to, name, count) in &workspaces {
                    let (view, to) = (view.clone(), to.clone());
                    let label = tf!("{name}（{n}）", name, n = count);
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        view.update(cx, |_, cx| cx.emit(ManagerEvent::SwitchTo(to.clone())))
                            .ok();
                    }));
                }
                menu
            })
            .into_any_element()
    }

    /// 本机配置的服务器卡片上的 ⋯：映射目录到本机、编辑、移除。
    fn server_menu(
        &self,
        server: Server,
        repo_count: Option<usize>,
        removal_note: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (menu_view, menu_app) = (cx.entity().downgrade(), self.app.clone());
        Button::new(SharedString::from(format!("more-{}", server.id)))
            .small()
            .ghost()
            .icon(ui::icon(IconName::Ellipsis))
            .tooltip(tr("映射、编辑或移除此服务器"))
            .dropdown_menu(move |menu, _, _| {
                let (view, edit) = (menu_view.clone(), server.clone());
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
                .item(
                    PopupMenuItem::new(tr("编辑…")).on_click(move |_, window, cx| {
                        view.update(cx, |this, cx| {
                            this.server_dialog(Some(edit.clone()), window, cx)
                        })
                        .ok();
                    }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(tr("移除…")).on_click(move |_, window, cx| {
                        let (app, id) = (app.clone(), remove.id.clone());
                        // 仓库列表保存在服务器上，不跟着没；这里只是不再连它
                        let repos = match repo_count {
                            Some(n) if n > 0 => tf!(
                                "该服务器上的 {n} 个仓库将不再列出，已打开的标签页将关闭。",
                                n = n
                            ),
                            _ => String::new(),
                        };
                        confirm(
                            tf!("移除服务器 {name}？", name = remove.name),
                            tf!(
                                "{destination}\n{jump}{repos}服务器上的内容不受影响。",
                                destination = remove.destination,
                                jump = &removal_note,
                                repos = repos
                            ),
                            tr("移除服务器"),
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

    /// 服务器上的工作区：页面顶上一行写明保存在哪，断开时写明暂时不能改、给一个「重新连接」。
    fn render_banner(&self, colors: Colors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let app = self.app.read(cx);
        let key = app.current().clone();
        if key.is_local() {
            return None;
        }
        let machine = app.machine_name(&key.machine);
        let workspace = app.workspace_name(&key);
        let live = app.writable(&key.machine);
        let loaded = app.machine(&key.machine).is_some();
        let connecting = matches!(app.status(&key.machine), ConnStatus::Connecting);
        let warning = IconName::TriangleAlert;
        let (icon, color, text, button): (IconName, Hsla, SharedString, Option<&'static str>) =
            match (live, connecting, loaded) {
                (true, ..) => (
                    IconName::RefreshCw,
                    colors.accent,
                    tf!(
                        "工作区「{workspace}」保存在 {machine} 上，修改将同步到连接它的所有电脑。",
                        workspace = &workspace,
                        machine = &machine
                    )
                    .into(),
                    None,
                ),
                (false, true, true) => (
                    warning,
                    colors.warning,
                    tf!("正在重新连接 {name}…", name = &machine).into(),
                    None,
                ),
                (false, true, false) => (
                    warning,
                    colors.warning,
                    tf!("正在连接 {cx}…", cx = &machine).into(),
                    None,
                ),
                (false, false, true) => (
                    warning,
                    colors.warning,
                    tf!("与 {name} 的连接已断开，暂时无法修改。", name = &machine).into(),
                    Some(tr("重新连接")),
                ),
                (false, false, false) => (
                    warning,
                    colors.warning,
                    tf!("尚未连接 {name}，暂时无法修改。", name = &machine).into(),
                    Some(tr("连接")),
                ),
            };
        let id = key.machine.clone();
        Some(
            h_flex()
                .px(px(14.))
                .py(px(9.))
                .gap(px(10.))
                .rounded(px(9.))
                .border_1()
                .border_color(color.opacity(0.35))
                .bg(color.opacity(0.08))
                .child(Icon::new(icon).size_4().flex_none().text_color(color))
                .child(div().flex_1().min_w_0().text_sm().child(text))
                .when_some(button, |row, label| {
                    row.child(
                        Button::new("reconnect")
                            .small()
                            .icon(ui::icon(IconName::Plug))
                            .label(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.app.update(cx, |app, cx| drop(app.connect(&id, cx)));
                            })),
                    )
                })
                .into_any_element(),
        )
    }
}

impl Render for ManagerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let colors = Colors {
            muted: theme.muted_foreground,
            hover: theme.list_hover,
            border: theme.border,
            danger: theme.danger,
            success: theme.success,
            warning: theme.warning,
            accent: theme.primary,
            panel: theme.sidebar,
        };
        let background = theme.background;
        let app = self.app.read(cx);
        // 「全部仓库」跟着当前工作区所在的电脑：本机的工作区列本机和本机配置的服务器，
        // 156 的工作区列 156 和 156 配置的服务器（和在 156 上看到的一样）
        let machine = app.current().machine.clone();
        let local_view = machine == crate::machines::LOCAL;
        let servers: Vec<String> = match local_view {
            true => app.settings.servers.iter().map(|s| s.id.clone()).collect(),
            false => app
                .machine(&machine)
                .map(|loaded| {
                    loaded
                        .store
                        .servers
                        .iter()
                        .map(|s| crate::machines::relay_key(&machine, &s.id))
                        .collect()
                })
                .unwrap_or_default(),
        };
        let total = app.perspective_repos(&machine).len();
        let machine_name = app.machine_name(&machine);
        let workspace_has_repos = !app.members(app.current()).is_empty();
        let all_repos = app.settings.layout.all_repos;

        let mut sections = Vec::new();
        match local_view {
            true => sections.push(self.render_local_card(colors, cx)),
            false => sections.push(self.render_server_card(&machine, colors, cx)),
        }
        for key in &servers {
            sections.push(self.render_server_card(key, colors, cx));
        }

        // 当前工作区的仓库先列出来；别的都在「全部仓库」里，工作区里有仓库时可以收起
        let banner = self.render_banner(colors, cx);
        let workspace_card = self.render_workspace_card(colors, cx);
        let collapsible = workspace_has_repos;
        let all_open = all_repos || !collapsible;
        let all_header = h_flex()
            .id("all-repos")
            .h(px(30.))
            .px(px(6.))
            .gap(px(8.))
            .rounded(px(ui::RADIUS))
            .when(collapsible, |row| {
                row.cursor_pointer()
                    .hover(|s| s.bg(colors.hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app.update(cx, |app, cx| {
                            app.edit_layout(cx, |layout| layout.all_repos = !layout.all_repos)
                        });
                    }))
                    .child(
                        ui::icon_small(if all_open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .text_color(colors.muted),
                    )
            })
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("全部仓库")),
            )
            .child(ui::count_pill(total, false, cx))
            .map(|row| match (local_view, all_open) {
                (false, _) => row.child(
                    div()
                        .text_xs()
                        .text_color(colors.muted)
                        .child(tf!("{name} 及其服务器上的仓库", name = &machine_name)),
                ),
                (true, false) => row.child(
                    div()
                        .text_xs()
                        .text_color(colors.muted)
                        .child(tr("在此添加或克隆仓库")),
                ),
                (true, true) => row,
            });

        let add_server = Button::new("add-server")
            .primary()
            .icon(ui::icon(IconName::Plus))
            .label(tr("添加服务器…"))
            .disabled(!local_view)
            .on_click(cx.listener(|this, _, window, cx| this.server_dialog(None, window, cx)));
        let add_server = match local_view {
            true => add_server.into_any_element(),
            // 别的电脑的服务器在那台电脑上加
            false => add_server
                .tooltip(tf!(
                    "{name} 的服务器需在 {name} 上添加",
                    name = &machine_name
                ))
                .into_any_element(),
        };

        div()
            .id("manager")
            .size_full()
            .bg(background)
            .overflow_y_scroll()
            .child(
                // 窗口宽的时候内容居中，不然右边空一大块
                v_flex()
                    .w_full()
                    .max_w(px(920.))
                    .mx_auto()
                    .px(px(28.))
                    .py(px(24.))
                    .gap(px(18.))
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .text_xl()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(trc("页面", "仓库")),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(colors.muted)
                                            .child(tr("点击仓库即可在新标签页中打开。")),
                                    ),
                            )
                            .child(add_server),
                    )
                    .children(banner)
                    .child(workspace_card)
                    .child(all_header)
                    .when(all_open, |page| page.children(sections)),
            )
    }
}

/// 卡片里一组的小标题（「开着 · 3」）。
fn group_label(text: String, colors: Colors) -> AnyElement {
    div()
        .px(px(10.))
        .pt(px(8.))
        .pb(px(2.))
        .text_xs()
        .text_color(colors.muted)
        .child(text)
        .into_any_element()
}

/// 状态的那一行：一个小圆点（没有就不画）加一句话。
/// `key` 区分是哪张卡片的（本机是 "local"，服务器是它的 id）。
fn status_line(key: &str, dot: Option<Hsla>, text: SharedString, colors: Colors) -> AnyElement {
    h_flex()
        .gap(px(6.))
        .min_w_0()
        .when_some(dot, |row, color| {
            row.child(div().flex_none().size(px(7.)).rounded_full().bg(color))
        })
        .child(
            ui::clipped(ElementId::Name(format!("status:{key}").into()), text)
                .text_xs()
                .text_color(colors.muted),
        )
        .into_any_element()
}

/// 卡片能收起时：现在收没收起，和点了顶上那一条要做的事。
type Fold = (bool, Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>);

/// 卡片顶上的一条：图标、名字（后面跟小标签和一句小字），下面一行是状态；右边是按钮。
/// 给了 `fold` 的卡片能收起：最左边多一个箭头，点左边这一段（按钮除外）收起、展开。
#[allow(clippy::too_many_arguments)]
fn card_header(
    key: &str,
    icon: IconName,
    name: SharedString,
    tags: Vec<AnyElement>,
    aside: Option<SharedString>,
    status: AnyElement,
    actions: Vec<AnyElement>,
    fold: Option<Fold>,
    colors: Colors,
) -> AnyElement {
    let collapsed = fold.as_ref().is_some_and(|(collapsed, _)| *collapsed);
    let lead = h_flex()
        .id(ElementId::Name(format!("card-fold:{key}").into()))
        .flex_1()
        .min_w_0()
        .gap(px(8.))
        .when_some(fold, |lead, (collapsed, toggle)| {
            lead.cursor_pointer().on_click(toggle).child(
                ui::icon_small(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .flex_none()
                .text_color(colors.muted),
            )
        })
        .child(
            div()
                .flex_none()
                .size(px(32.))
                .mr(px(4.))
                .rounded(px(8.))
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
                .gap(px(1.))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            ui::clipped(ElementId::Name(format!("card-name:{key}").into()), name)
                                .flex_none()
                                .max_w(px(280.))
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD),
                        )
                        .children(tags)
                        .when_some(aside, |row, aside| {
                            row.child(
                                ui::clipped(
                                    ElementId::Name(format!("card-aside:{key}").into()),
                                    aside,
                                )
                                .text_xs()
                                .text_color(colors.muted),
                            )
                        }),
                )
                .child(status),
        );
    h_flex()
        .px(px(14.))
        .py(px(10.))
        .gap(px(8.))
        .bg(colors.panel)
        // 卡片的角是圆的，这一条的底色要跟着圆，不然四个角会戳出去；收起时下面两个角也是它的
        .map(|bar| match collapsed {
            true => bar.rounded(px(9.)),
            false => bar.rounded_t(px(9.)),
        })
        .child(lead)
        .children(actions)
        .into_any_element()
}

/// 一张卡片：顶上一条，下面是仓库；`failure` 是连不上的原因，整段显示。
/// `collapsed`：收起了，只画顶上那一条。
fn card(
    header: AnyElement,
    failure: Option<SharedString>,
    rows: Vec<AnyElement>,
    empty: &'static str,
    collapsed: bool,
    colors: Colors,
) -> AnyElement {
    let card = v_flex()
        .border_1()
        .border_color(colors.border)
        .rounded(px(10.))
        .child(header);
    if collapsed {
        return card.into_any_element();
    }
    card.child(div().h(px(1.)).bg(colors.border))
        .when_some(failure, |card, failure| {
            card.child(
                div()
                    .px(px(14.))
                    .py(px(8.))
                    .bg(colors.danger.opacity(0.08))
                    .text_xs()
                    .text_color(colors.danger)
                    .whitespace_normal()
                    .child(failure),
            )
        })
        .child(
            v_flex()
                .p(px(6.))
                .when(rows.is_empty(), |list| {
                    list.child(
                        div()
                            .px(px(10.))
                            .py(px(12.))
                            .text_sm()
                            .text_color(colors.muted)
                            .child(empty),
                    )
                })
                .children(rows),
        )
        .into_any_element()
}
