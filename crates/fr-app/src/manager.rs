//! 仓库管理器：本机仓库和各台服务器上的仓库；添加本机仓库、添加服务器、打开服务器上的仓库。

use std::cell::Cell;
use std::rc::Rc;

use fr_channel::Channel;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::{AppState, ConnStatus};
use crate::browser::{BrowserEvent, RemoteBrowser};
use crate::clone::{CloneEvent, CloneView};
use crate::settings::RepoEntry;

pub enum ManagerEvent {
    Open(RepoEntry),
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
            prompt: Some("添加仓库".into()),
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
                    Notification::error(format!("{dir} 不是 git 仓库：{err:#}")),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    fn add_server_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let destination = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("user@host、~/.ssh/config 里的主机别名，或 ssh://user@host:端口")
        });
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("可以不填，默认用主机名"));
        let app = self.app.clone();
        let first_build = Cell::new(true);
        let forward_agent = Rc::new(Cell::new(false));
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                destination.focus_handle(cx).focus(window, cx);
            }
            let muted = cx.theme().muted_foreground;
            let submit = {
                let (destination, name, app) = (destination.clone(), name.clone(), app.clone());
                let forward_agent = forward_agent.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let target = destination.read(cx).value().trim().to_string();
                    if target.is_empty() {
                        window.push_notification(Notification::error("请填写 SSH 目标"), cx);
                        return;
                    }
                    let mut label = name.read(cx).value().trim().to_string();
                    if label.is_empty() {
                        label = host_of(&target);
                    }
                    app.update(cx, |app, cx| {
                        let id = app.add_server(label, target, forward_agent.get(), cx);
                        // 添加后马上开始连接，结果显示在列表上
                        drop(app.connect(&id, cx));
                    });
                    window.close_dialog(cx);
                }
            };
            dialog
                .title("添加服务器")
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
                        .child(field("SSH 目标", Input::new(&destination)))
                        .child(field("名称", Input::new(&name)))
                        .child(
                            Checkbox::new("forward-agent")
                                .label("转发本机的 ssh-agent（服务器上的 git 推拉用本机密钥；只对信任的服务器打开）")
                                .checked(forward_agent.get())
                                .on_click({
                                    let forward_agent = forward_agent.clone();
                                    move |checked, window, _| {
                                        forward_agent.set(*checked);
                                        window.refresh();
                                    }
                                }),
                        )
                        .child(div().text_xs().text_color(muted).child(
                            "用系统自带的 ssh 连接，沿用 ~/.ssh/config 和密钥；目前只支持密钥登录。\
                             第一次连接时会在服务器的 ~/.fork-remote/ 下装一个 1 MB 左右的 agent，不要 root，不开端口。",
                        )),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("ok")
                                .primary()
                                .label("添加并连接")
                                .on_click(submit),
                        ),
                )
        });
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
            Some(s) => format!("克隆到 {}", s.name),
            None => "克隆到本机".into(),
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
            dialog
                .title(title.clone())
                .w(px(620.))
                // 回车开始克隆，不关对话框
                .on_ok(move |_, window, cx| {
                    on_enter.update(cx, |view, cx| view.start(window, cx));
                    false
                })
                .child(view.clone())
        });
    }

    /// 选主题的按钮：跟随系统、浅色、深色。
    fn render_theme_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::ThemeChoice;
        let current = self.app.read(cx).settings.theme;
        let app = self.app.clone();
        Button::new("theme")
            .ghost()
            .label(format!("主题：{}", current.label()))
            .dropdown_menu(move |mut menu, _, _| {
                for choice in [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark] {
                    let app = app.clone();
                    menu = menu.item(
                        PopupMenuItem::new(choice.label())
                            .checked(choice == current)
                            .on_click(move |_, window, cx| {
                                app.update(cx, |app, cx| app.set_theme(choice, cx));
                                crate::appearance::apply_theme(choice, window, cx);
                            }),
                    );
                }
                menu
            })
    }

    fn open_remote_dialog(&mut self, server: String, window: &mut Window, cx: &mut Context<Self>) {
        let title = self
            .app
            .read(cx)
            .server(&server)
            .map(|s| format!("打开 {} 上的仓库", s.name))
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
        h_flex()
            .id(ElementId::Name(id))
            .px_3()
            .py_1()
            .gap_3()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(colors.hover))
            .on_click(cx.listener(move |this, _, _, cx| this.open(open.clone(), cx)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().child(entry.name().to_string()))
                    .child(
                        div()
                            .text_xs()
                            .truncate()
                            .text_color(colors.muted)
                            .child(entry.path.clone()),
                    ),
            )
            .child(
                Button::new("remove")
                    .ghost()
                    .xsmall()
                    .label("移除")
                    .tooltip("从列表里去掉，不动仓库本身")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.app.update(cx, |app, cx| app.remove_repo(&remove, cx));
                    })),
            )
            .into_any_element()
    }
}

#[derive(Clone, Copy)]
struct Colors {
    muted: Hsla,
    hover: Hsla,
    border: Hsla,
    danger: Hsla,
}

fn field(label: &'static str, input: Input) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(div().text_sm().child(label))
        .child(input)
}

/// 从 SSH 目标里取主机名，当默认的显示名。
fn host_of(destination: &str) -> String {
    let rest = destination.strip_prefix("ssh://").unwrap_or(destination);
    let rest = rest.rsplit('@').next().unwrap_or(rest);
    let host = match rest.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => rest,
    };
    host.trim_end_matches('/').to_string()
}

impl Render for ManagerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let colors = Colors {
            muted: theme.muted_foreground,
            hover: theme.list_hover,
            border: theme.border,
            danger: theme.danger,
        };
        let settings = self.app.read(cx).settings.clone();

        let local: Vec<AnyElement> = settings
            .repos
            .iter()
            .filter(|r| r.server.is_none())
            .map(|r| self.render_repo(r, colors, cx))
            .collect();
        let mut sections = vec![section(
            h_flex()
                .gap_2()
                .child(div().font_weight(FontWeight::SEMIBOLD).child("本机"))
                .child(div().flex_1())
                .child(Button::new("clone-local").small().label("克隆…").on_click(
                    cx.listener(|this, _, window, cx| this.clone_dialog(None, window, cx)),
                ))
                .child(
                    Button::new("add-local")
                        .small()
                        .label("添加本机仓库…")
                        .on_click(cx.listener(|this, _, window, cx| this.add_local(window, cx))),
                ),
            local,
            "还没有本机仓库",
            colors,
        )];

        for server in &settings.servers {
            let status = self.app.read(cx).status(&server.id);
            let (status_text, status_color, retry): (SharedString, Hsla, bool) = match &status {
                ConnStatus::Idle => ("未连接".into(), colors.muted, true),
                ConnStatus::Connecting => ("正在连接…".into(), colors.muted, false),
                ConnStatus::Ready(conn) => (
                    format!(
                        "已连接 · {} · {} ms{}",
                        conn.hello.git.trim_start_matches("git version "),
                        conn.connect_time.as_millis(),
                        if conn.installed {
                            " · 刚装好 agent"
                        } else {
                            ""
                        }
                    )
                    .into(),
                    colors.muted,
                    false,
                ),
                ConnStatus::Failed(err) => (err.clone(), colors.danger, true),
            };
            let repos: Vec<AnyElement> = settings
                .repos
                .iter()
                .filter(|r| r.server.as_deref() == Some(&server.id))
                .map(|r| self.render_repo(r, colors, cx))
                .collect();
            let (id_open, id_retry, id_remove) =
                (server.id.clone(), server.id.clone(), server.id.clone());
            let id_clone = server.id.clone();
            let header = h_flex()
                .gap_2()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(server.name.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.muted)
                        .child(server.destination.clone()),
                )
                .child(div().flex_1())
                .when(retry, |row| {
                    row.child(
                        Button::new(SharedString::from(format!("connect-{}", server.id)))
                            .small()
                            .ghost()
                            .label("连接")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.app.update(cx, |app, cx| {
                                    drop(app.connect(&id_retry, cx));
                                });
                            })),
                    )
                })
                .child(
                    Button::new(SharedString::from(format!("clone-{}", server.id)))
                        .small()
                        .label("克隆…")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.clone_dialog(Some(id_clone.clone()), window, cx)
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("open-{}", server.id)))
                        .small()
                        .label("打开仓库…")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_remote_dialog(id_open.clone(), window, cx)
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("remove-{}", server.id)))
                        .small()
                        .ghost()
                        .label("移除")
                        .tooltip("从列表里去掉这台服务器和它的仓库，不动服务器上的东西")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.app
                                .update(cx, |app, cx| app.remove_server(&id_remove, cx));
                        })),
                );
            sections.push(section(
                v_flex()
                    .gap_1()
                    .child(header)
                    .child(div().text_xs().text_color(status_color).child(status_text)),
                repos,
                "还没有打开过这台服务器上的仓库",
                colors,
            ));
        }

        div().id("manager").size_full().overflow_y_scroll().child(
            v_flex()
                .max_w(px(960.))
                .p_6()
                .gap_5()
                .child(
                    h_flex()
                        .gap_3()
                        .child(
                            div()
                                .text_xl()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("仓库"),
                        )
                        .child(div().flex_1())
                        .child(self.render_theme_button(cx))
                        .child(
                            Button::new("add-server")
                                .primary()
                                .label("添加服务器…")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_server_dialog(window, cx)
                                })),
                        ),
                )
                .children(sections),
        )
    }
}

fn section(
    header: impl IntoElement,
    rows: Vec<AnyElement>,
    empty: &'static str,
    colors: Colors,
) -> AnyElement {
    v_flex()
        .gap_2()
        .child(header)
        .child(
            v_flex()
                .p_1()
                .border_1()
                .border_color(colors.border)
                .rounded_lg()
                .when(rows.is_empty(), |list| {
                    list.child(
                        div()
                            .px_3()
                            .py_2()
                            .text_sm()
                            .text_color(colors.muted)
                            .child(empty),
                    )
                })
                .children(rows),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::host_of;

    #[test]
    fn 从ssh目标取主机名() {
        assert_eq!(host_of("root@10.0.0.5"), "10.0.0.5");
        assert_eq!(host_of("ssh://me@example.com:2222"), "example.com");
        assert_eq!(host_of("devbox"), "devbox");
    }
}
