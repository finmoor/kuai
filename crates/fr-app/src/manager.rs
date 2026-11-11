//! 仓库管理器：本机仓库和各台服务器上的仓库；添加本机仓库、添加和编辑服务器、打开服务器上的仓库。
//!
//! 本机和每台服务器各是一张卡片：顶上一条写名字、连接状态和能做的事，下面列仓库，点一行打开。
//! 移除仓库、移除服务器都先问一下；服务器的「编辑」「移除」收在行尾的 ⋯ 菜单里，不和常用按钮挨着。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use fr_channel::Channel;
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
use crate::settings::{Auth, RepoEntry, Server, Target};
use crate::ui;

pub enum ManagerEvent {
    Open(RepoEntry),
    /// 给这个仓库改名（对话框由主窗口开）
    Rename(RepoEntry),
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

    /// 选一个本机目录；用 git 找到仓库根目录再加进列表。
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
            let result = Channel::Local
                .git(&dir, vec!["rev-parse".into(), "--show-toplevel".into()])
                .await
                .and_then(|out| out.ok());
            this.update_in(cx, |this, window, cx| match result {
                Ok(out) => {
                    let path = String::from_utf8_lossy(&out).trim().replace('/', "\\");
                    let entry = RepoEntry { server: None, path };
                    this.app
                        .update(cx, |app, cx| app.add_repo(entry.clone(), cx));
                    this.open(entry, cx);
                }
                Err(err) => window.push_notification(
                    Notification::error(tf!(
                        "{dir} 不是 git 仓库：{err}",
                        dir,
                        err = format!("{:#}", err)
                    )),
                    cx,
                ),
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
                tr("已保存，不改就留空")
            } else {
                tr("不填则连接时问")
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
            tr("主机名、IP，或 ~/.ssh/config 里的别名"),
            false,
        );
        let port = input(&target.port, "22", false);
        let user = input(&target.user, tr("不填用 ssh 的默认"), false);
        let key = input(
            old.identity_file.as_deref().unwrap_or_default(),
            tr("不填用默认密钥和 ssh-agent"),
            false,
        );
        let passphrase = input("", secret_hint(Auth::Key), true);
        let password = input("", secret_hint(Auth::Password), true);
        let name = input(&old.name, tr("默认用主机名"), false);

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
                        return fail(tr("还没有填主机"), window);
                    }
                    if !target.port.is_empty() && !target.port_ok() {
                        return fail(tr("端口要填 1～65535 的数字"), window);
                    }
                    let key = key.trim();
                    if auth == Auth::Key && !key.is_empty() {
                        if key.to_lowercase().ends_with(".pub") {
                            return fail(tr("这是公钥，要选不带 .pub 的私钥文件"), window);
                        }
                        // ~ 开头的由 ssh 自己展开，这里不查
                        if !key.starts_with('~') && !std::path::Path::new(key).is_file() {
                            return fail(tr("找不到这个私钥文件"), window);
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
                                Notification::error(tf!("密码没存上，连接时会问：{err}", err)),
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
                                    "已按新参数重连；开着的标签页关掉重开后生效。",
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
                        .label(tr("清除已保存的"))
                        .on_click(move |_, window, cx| {
                            forget.set(true);
                            input.update(cx, |state, cx| {
                                state.set_placeholder(tr("不填则连接时问"), window, cx)
                            });
                            window.refresh();
                        })
                        .into_any_element()
                });
                field_with(label, clear, Input::new(input).mask_toggle())
            };
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
                                .label(tr("转发本机的 ssh-agent（只对信任的服务器开）"))
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
                        .child(div().text_xs().text_color(muted).child(tr(
                            "密码、口令存在 Windows 凭据管理器。首次连接会往服务器的 ~/.fork-remote/ 装 agent。",
                        ))),
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
            prompt: Some(tr("选私钥文件").into()),
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
                    .tooltip(tr("改名"))
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
                    .tooltip(tr("从列表里移除"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        let (app, entry) = (this.app.clone(), remove.clone());
                        confirm(
                            tf!("从列表里移除 {name}？", name = &remove_name),
                            tf!("{path}\n只从列表里移除，仓库本身不动。", path = remove.path),
                            tr("移除"),
                            false,
                            window,
                            cx,
                            move |cx| app.update(cx, |app, cx| app.remove_repo(&entry, cx)),
                        );
                    })),
            )
            .context_menu(move |menu, _, _| {
                let (view, entry) = (view.clone(), menu_entry.clone());
                let path = entry.path.clone();
                menu.item(PopupMenuItem::new(tr("改名…")).on_click(move |_, _, cx| {
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
fn confirm(
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

fn field(label: &'static str, input: Input) -> impl IntoElement {
    field_with(label, None, input)
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
            tr("还没有本机仓库。点「添加本机仓库…」或「克隆…」。"),
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
                ConnStatus::Connecting => (Some(colors.accent), tr("正在连接…").into(), false),
                ConnStatus::Ready(conn) => (
                    Some(colors.success),
                    tf!(
                        "已连接 · git {git} · {ms} ms{installed}",
                        git = conn.hello.git.trim_start_matches("git version "),
                        ms = conn.connect_time.as_millis(),
                        installed = if conn.installed {
                            tr(" · 刚装好 agent")
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
            let (id_open, id_retry) = (server.id.clone(), server.id.clone());
            let id_clone = server.id.clone();
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
                    .tooltip(tr("编辑、移除这台服务器"))
                    .dropdown_menu(move |menu, _, _| {
                        let (view, edit) = (menu_view.clone(), menu_server.clone());
                        let (app, remove) = (menu_app.clone(), menu_server.clone());
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
                                        "它的 {n} 个仓库也从列表里移除，开着的标签页会关掉。",
                                        n = repo_count
                                    )
                                };
                                confirm(
                                    tf!("移除服务器 {name}？", name = remove.name),
                                    tf!(
                                        "{destination}\n{repos}服务器上的东西不动。",
                                        destination = remove.destination,
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
                    Some(server.destination.clone().into()),
                    status_line(&server.id, dot, status_text, colors),
                    actions,
                    colors,
                ),
                failure,
                repos,
                tr("还没有这台服务器上的仓库。点「打开仓库…」挑一个。"),
                colors,
            ));
        }

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
                                            .child(tr("点仓库在新标签页里打开。")),
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
                    .children(sections),
            )
    }
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

/// 卡片顶上的一条：图标、名字（后面跟一句小字），下面一行是状态；右边是按钮。
fn card_header(
    key: &str,
    icon: IconName,
    name: SharedString,
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
