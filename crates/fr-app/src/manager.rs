//! 仓库管理器：本机仓库和各台服务器上的仓库；添加本机仓库、添加和编辑服务器、打开服务器上的仓库。
//!
//! 最上面一张卡片是当前工作区的仓库（开着的、没开的分开列），手滑关掉的在这里找。
//! 下面的「全部仓库」可以收起：本机和每台服务器各是一张卡片，顶上一条写名字、连接状态和能做的事，
//! 下面列仓库，点一行打开。
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
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::{AppState, ConnStatus};
use crate::browser::{BrowserEvent, RemoteBrowser};
use crate::clone::{CloneEvent, CloneView};
use crate::settings::{Auth, RepoEntry, Server, TabGroup, Target};
use crate::ui;

pub enum ManagerEvent {
    Open(RepoEntry),
    /// 给这个仓库改名（对话框由主窗口开）
    Rename(RepoEntry),
    /// 把这个仓库从当前工作区移除（主窗口来问，开着的标签页也由它关）
    RemoveFromWorkspace(RepoEntry),
    /// 打开「快速打开」
    QuickOpen,
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

    /// 选一个本机目录；在仓库中就找到仓库根目录加进列表，不是仓库先问是否初始化。
    fn add_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(tr("添加仓库").into()),
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
                let app = this.app.clone();
                crate::open_dir::open_dir(
                    &app,
                    None,
                    dir,
                    window,
                    cx,
                    |this: &mut Self, entry, _, cx| {
                        this.app
                            .update(cx, |app, cx| app.add_repo(entry.clone(), cx));
                        this.open(entry, cx);
                    },
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
                let (app, auth, forward_agent) =
                    (app.clone(), auth.clone(), forward_agent.clone());
                let (forget, problem, old) = (forget.clone(), problem.clone(), old.clone());
                let jump = jump.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let read = |input: &Entity<InputState>| input.read(cx).value().to_string();
                    let (host, user, port) = (read(&host), read(&user), read(&port));
                    let (key, name) = (read(&key), read(&name));
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
                        .child(field(tr("名称"), Input::new(&name)))
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
                        .child(div().text_xs().text_color(muted).child(if jump_now.is_some() {
                            tr(
                                "密码和私钥口令保存在 Windows 凭据管理器中。首次连接时将在服务器的 ~/.kuai/ 中安装 kuaid，跳板机上不安装任何程序。",
                            )
                        } else {
                            tr(
                                "密码和私钥口令保存在 Windows 凭据管理器中。首次连接时将在服务器的 ~/.kuai/ 中安装 kuaid。",
                            )
                        })),
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
        let title = match server
            .as_deref()
            .and_then(|id| self.app.read(cx).server(id))
        {
            Some(s) => tf!("克隆到 {name}", name = s.name),
            None => tr("克隆到本机").into(),
        };
        let app = self.app.clone();
        let view = cx.new(|cx| CloneView::new(app, server, window, cx));
        cx.subscribe_in(&view, window, |this, _, event, window, cx| match event {
            CloneEvent::Done(entry) => {
                window.close_dialog(cx);
                this.app
                    .update(cx, |app, cx| app.add_repo(entry.clone(), cx));
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
        let title = self
            .app
            .read(cx)
            .server(&server)
            .map(|s| tf!("打开 {name} 上的仓库", name = s.name))
            .unwrap_or_default();
        let app = self.app.clone();
        let browser = cx.new(|cx| RemoteBrowser::new(app, server, window, cx));
        cx.subscribe_in(&browser, window, |this, _, event, window, cx| match event {
            BrowserEvent::Open(entry) => {
                window.close_dialog(cx);
                this.app
                    .update(cx, |app, cx| app.add_repo(entry.clone(), cx));
                this.open(entry.clone(), cx);
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
        let (open_entry, leave, menu_entry) = (entry.clone(), entry.clone(), entry.clone());
        let view = cx.entity().downgrade();
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
                    .icon(ui::icon(IconName::CircleMinus).text_color(colors.muted))
                    .tooltip(tr("从工作区移除"))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(ManagerEvent::RemoveFromWorkspace(leave.clone()));
                    })),
            )
            .context_menu(move |menu, _, _| {
                let (leave_view, rename_view) = (view.clone(), view.clone());
                let (leave, rename) = (menu_entry.clone(), menu_entry.clone());
                let path = menu_entry.path.clone();
                menu.item(
                    PopupMenuItem::new(tr("从工作区移除")).on_click(move |_, _, cx| {
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
                )
            })
            .into_any_element()
    }

    /// 最上面那张卡片：当前工作区的仓库。开着的在上（按标签页的顺序），没开的在下（刚关掉的排前）。
    fn render_workspace_card(
        &self,
        group: &TabGroup,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let closed: Vec<&RepoEntry> = group.closed.iter().rev().collect();
        let idle = group
            .repos
            .iter()
            .filter(|e| !group.tabs.contains(e) && !group.closed.contains(e));
        let away: Vec<(&RepoEntry, bool)> = closed
            .iter()
            .map(|e| (*e, true))
            .chain(idle.map(|e| (e, false)))
            .collect();
        let (open_count, away_count) = (group.tabs.len(), away.len());
        let mut rows: Vec<AnyElement> = Vec::new();
        if open_count > 0 {
            rows.push(group_label(tf!("已打开 · {n}", n = open_count), colors));
            for entry in &group.tabs {
                rows.push(self.render_member(entry, true, false, colors, cx));
            }
        }
        if away_count > 0 {
            rows.push(group_label(tf!("未打开 · {n}", n = away_count), colors));
            for (entry, recent) in away {
                rows.push(self.render_member(entry, false, recent, colors, cx));
            }
        }
        card(
            card_header(
                "workspace",
                IconName::Layers,
                group.name.clone().into(),
                Vec::new(),
                Some(tr("当前工作区").into()),
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
                colors,
            ),
            None,
            rows,
            tr("此工作区中暂无仓库。打开的仓库将归入此工作区。"),
            colors,
        )
    }

    fn render_repo(&self, entry: &RepoEntry, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
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
        let view = cx.entity().downgrade();
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
            // 「改名」「移除」一直在行尾，用次要的颜色，不抢眼
            .child(
                Button::new("rename")
                    .ghost()
                    .xsmall()
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
                    .icon(ui::icon(IconName::X).text_color(colors.muted))
                    .tooltip(tr("从列表中移除"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        let (app, entry) = (this.app.clone(), remove.clone());
                        let mut description = tf!(
                            "{path}\n仅从列表中移除，仓库本身不受影响。",
                            path = remove.path
                        );
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
            .context_menu(move |menu, _, _| {
                let (view, entry) = (view.clone(), menu_entry.clone());
                let path = entry.path.clone();
                menu.item(PopupMenuItem::new(tr("重命名…")).on_click(move |_, _, cx| {
                    view.update(cx, |_, cx| cx.emit(ManagerEvent::Rename(entry.clone())))
                        .ok();
                }))
                .item(
                    PopupMenuItem::new(tr("复制仓库路径")).on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                    }),
                )
            })
            .into_any_element()
    }
}

#[derive(Clone, Copy)]
struct Colors {
    muted: Hsla,
    hover: Hsla,
    border: Hsla,
    danger: Hsla,
    success: Hsla,
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
    let action = Rc::new(action);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let action = action.clone();
        let run = move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            window.close_dialog(cx);
            action(cx);
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

impl Render for ManagerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        let settings = self.app.read(cx).settings.clone();

        let local: Vec<AnyElement> = settings
            .repos
            .iter()
            .filter(|r| r.server.is_none())
            .map(|r| self.render_repo(r, colors, cx))
            .collect();
        let mut sections = vec![card(
            card_header(
                "local",
                IconName::Monitor,
                tr("本机").into(),
                Vec::new(),
                None,
                status_line(
                    "local",
                    None,
                    tf!("{n} 个仓库", n = local.len()).into(),
                    colors,
                ),
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
                colors,
            ),
            None,
            local,
            tr("暂无本机仓库。请点击「添加本机仓库…」或「克隆…」。"),
            colors,
        )];

        for server in &settings.servers {
            let status = self.app.read(cx).status(&server.id);
            let repos: Vec<AnyElement> = settings
                .repos
                .iter()
                .filter(|r| r.server.as_deref() == Some(&server.id))
                .map(|r| self.render_repo(r, colors, cx))
                .collect();
            let repo_count = repos.len();
            // 状态：一个小圆点加一句话；连不上的原因可能很长，单放一块，不截断
            let mut failure: Option<SharedString> = None;
            let (dot, status_text, retry): (Option<Hsla>, SharedString, bool) = match &status {
                ConnStatus::Idle => (Some(colors.muted.opacity(0.6)), tr("未连接").into(), true),
                ConnStatus::Connecting => (
                    Some(colors.accent),
                    self.app.read(cx).connecting_text(&server.id),
                    false,
                ),
                ConnStatus::Ready(conn) => (
                    Some(colors.success),
                    tf!(
                        "已连接 · git {git} · {ms} ms{installed}",
                        git = conn.hello.git.trim_start_matches("git version "),
                        ms = conn.connect_time.as_millis(),
                        installed = if conn.installed {
                            tr(" · 已安装 kuaid")
                        } else {
                            ""
                        }
                    )
                    .into(),
                    false,
                ),
                ConnStatus::Failed(err) => {
                    failure = Some(err.clone());
                    (Some(colors.danger), tr("连接失败").into(), true)
                }
            };
            // 名字后面的标签：它经哪几台连接；有没有别的服务器经它连接
            let route: Vec<String> = settings
                .route(&server.id)
                .iter()
                .map(|jump| jump.name.clone())
                .collect();
            let users: Vec<String> = settings
                .jump_users(&server.id)
                .iter()
                .map(|user| user.name.clone())
                .collect();
            let mut tags: Vec<AnyElement> = Vec::new();
            if !users.is_empty() {
                tags.push(name_tag(
                    format!("tag-jump:{}", server.id),
                    None,
                    tr("跳板机").into(),
                    tf!("经此服务器连接：{names}", names = users.join("、")).into(),
                    false,
                    colors,
                ));
            }
            if !route.is_empty() {
                let path = route.join(" → ");
                tags.push(name_tag(
                    format!("tag-via:{}", server.id),
                    Some(IconName::CornerDownRight),
                    tf!("经由 {path}", path = &path).into(),
                    tf!("连接时依次经过：{path}", path = &path).into(),
                    true,
                    colors,
                ));
            }
            let (id_open, id_retry) = (server.id.clone(), server.id.clone());
            let id_clone = server.id.clone();
            let removal_note = if users.is_empty() {
                String::new()
            } else {
                tf!(
                    "{names} 经此服务器连接，移除后将改为直接连接，需重新选择跳板机。\n",
                    names = users.join("、")
                )
            };
            let (menu_view, menu_server, menu_app) =
                (cx.entity().downgrade(), server.clone(), self.app.clone());
            let mut actions: Vec<AnyElement> = Vec::new();
            if retry {
                actions.push(
                    Button::new(SharedString::from(format!("connect-{}", server.id)))
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
                Button::new(SharedString::from(format!("clone-{}", server.id)))
                    .small()
                    .icon(ui::icon(IconName::Download))
                    .label(tr("克隆…"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.clone_dialog(Some(id_clone.clone()), window, cx)
                    }))
                    .into_any_element(),
            );
            actions.push(
                Button::new(SharedString::from(format!("open-{}", server.id)))
                    .small()
                    .icon(ui::icon(IconName::FolderSearch))
                    .label(tr("打开仓库…"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_remote_dialog(id_open.clone(), window, cx)
                    }))
                    .into_any_element(),
            );
            actions.push(
                Button::new(SharedString::from(format!("more-{}", server.id)))
                    .small()
                    .ghost()
                    .icon(ui::icon(IconName::Ellipsis))
                    .tooltip(tr("编辑或移除此服务器"))
                    .dropdown_menu(move |menu, _, _| {
                        let (view, edit) = (menu_view.clone(), menu_server.clone());
                        let (app, remove) = (menu_app.clone(), menu_server.clone());
                        let removal_note = removal_note.clone();
                        menu.item(
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
                                let repos = if repo_count == 0 {
                                    String::new()
                                } else {
                                    tf!(
                                        "该服务器的 {n} 个仓库也将从列表中移除，已打开的标签页将关闭。",
                                        n = repo_count
                                    )
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
                    .into_any_element(),
            );
            sections.push(card(
                card_header(
                    &server.id,
                    IconName::Server,
                    server.name.clone().into(),
                    tags,
                    Some(server.destination.clone().into()),
                    status_line(&server.id, dot, status_text, colors),
                    actions,
                    colors,
                ),
                failure,
                repos,
                if users.is_empty() {
                    tr("暂无此服务器上的仓库。请点击「打开仓库…」选择。")
                } else {
                    tr("仅用作跳板机，暂无此服务器上的仓库。")
                },
                colors,
            ));
        }

        // 当前工作区的仓库先列出来；别的都在「全部仓库」里，工作区里有仓库时可以收起
        let group = settings.workspace();
        let workspace_card = self.render_workspace_card(group, colors, cx);
        let collapsible = !group.repos.is_empty();
        let all_open = settings.layout.all_repos || !collapsible;
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
            .child(ui::count_pill(settings.repos.len(), false, cx))
            .when(!all_open, |row| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(colors.muted)
                        .child(tr("在此添加或克隆仓库")),
                )
            });

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
                            .child(
                                Button::new("add-server")
                                    .primary()
                                    .icon(ui::icon(IconName::Plus))
                                    .label(tr("添加服务器…"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.server_dialog(None, window, cx)
                                    })),
                            ),
                    )
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

/// 卡片顶上的一条：图标、名字（后面跟小标签和一句小字），下面一行是状态；右边是按钮。
fn card_header(
    key: &str,
    icon: IconName,
    name: SharedString,
    tags: Vec<AnyElement>,
    aside: Option<SharedString>,
    status: AnyElement,
    actions: Vec<AnyElement>,
    colors: Colors,
) -> AnyElement {
    h_flex()
        .px(px(14.))
        .py(px(10.))
        .gap(px(8.))
        .bg(colors.panel)
        // 卡片的角是圆的，这一条的底色要跟着圆，不然四个角会戳出去
        .rounded_t(px(9.))
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
        )
        .children(actions)
        .into_any_element()
}

/// 一张卡片：顶上一条，下面是仓库；`failure` 是连不上的原因，整段显示。
fn card(
    header: AnyElement,
    failure: Option<SharedString>,
    rows: Vec<AnyElement>,
    empty: &'static str,
    colors: Colors,
) -> AnyElement {
    v_flex()
        .border_1()
        .border_color(colors.border)
        .rounded(px(10.))
        .child(header)
        .child(div().h(px(1.)).bg(colors.border))
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
