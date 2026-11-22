//! 密码弹窗：本机的 git、连接服务器的 ssh、服务器上的 git 要密码或密钥口令时，弹窗让用户填。
//! ssh 第一次连一台主机、问要不要信任它的密钥时，弹的是另一个框：显示指纹，让用户确认。
//!
//! 请求来自后台线程（见 `fr_channel::init_prompts`），那边一直等到这里回答；
//! 用户取消或者关掉弹窗就回答「取消」，对应的 git 或 ssh 会放弃这次认证。

use fr_core::i18n::tr;
use std::sync::mpsc::Sender;

use futures::StreamExt as _;
use futures::channel::mpsc::{self, UnboundedReceiver};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

/// 一个密码请求：提示文字，和把回答送回去的口子（None 表示取消）。
pub struct Request {
    prompt: String,
    /// 是跳板机在问时，它的名字
    jump: Option<String>,
    reply: Sender<Option<String>>,
}

/// 等界面处理的密码请求。
pub type Prompts = UnboundedReceiver<Request>;

/// 程序启动时调用：登记密码请求的处理函数，返回界面这头接收请求的口子。
pub fn init() -> Prompts {
    let (tx, rx) = mpsc::unbounded();
    // 不许弹窗的（自动获取、自己在重连）在发问的那一头就当作取消了，到不了这里：见
    // `fr_core::askpass::NO_PROMPT` 和 `ConnectOptions::no_prompt`
    let started = fr_channel::init_prompts(move |prompt, jump| {
        let (reply, answer) = std::sync::mpsc::channel();
        tx.unbounded_send(Request {
            prompt: prompt.to_string(),
            jump: jump.map(str::to_string),
            reply,
        })
        .ok()?;
        // 弹窗没回答就被关掉时 reply 被丢弃，这里收到错误，当作取消
        answer.recv().ok().flatten()
    });
    if let Err(err) = started {
        log::error!("密码弹窗不可用，要密码的操作会直接失败：{err:#}");
    }
    rx
}

/// 在这个窗口里依次弹出收到的密码请求。返回的任务要留着，丢弃就不再弹。
pub fn show_in<T: 'static>(
    mut prompts: Prompts,
    window: &mut Window,
    cx: &mut Context<T>,
) -> Task<()> {
    cx.spawn_in(window, async move |_, cx| {
        while let Some(request) = prompts.next().await {
            let shown = cx.update(|window, cx| {
                // 窗口在后面时提到前面，不然用户不知道在等他
                window.activate_window();
                open(request, window, cx);
            });
            if shown.is_err() {
                return;
            }
        }
    })
}

fn open(request: Request, window: &mut Window, cx: &mut App) {
    let Request {
        prompt,
        jump,
        reply,
    } = request;
    // 跳板机在问：写明是哪一台，不然用户会当成是目标服务器的密码
    let jump: Option<SharedString> =
        jump.map(|name| fr_core::tf!("此为跳板机 {name} 的登录提示。", name).into());
    if fr_core::askpass::is_confirm(&prompt) {
        return open_host_key(&prompt, jump, reply, window, cx);
    }
    let secret = fr_core::askpass::is_secret(&prompt);
    let input = cx.new(|cx| InputState::new(window, cx).masked(secret));
    let prompt: SharedString = prompt.trim().to_string().into();

    let submit = {
        let (input, reply) = (input.clone(), reply.clone());
        move |window: &mut Window, cx: &mut App| {
            let _ = reply.send(Some(input.read(cx).value().to_string()));
            window.close_dialog(cx);
        }
    };
    let first_build = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_build.replace(false) {
            input.focus_handle(cx).focus(window, cx);
        }
        let muted = cx.theme().muted_foreground;
        let (submit, reply) = (submit.clone(), reply.clone());
        let (on_enter, on_dismiss) = (submit.clone(), reply.clone());
        dialog
            .title(if secret {
                tr("需要密码")
            } else {
                tr("需要输入")
            })
            .w(px(520.))
            // 回车等于点确定（submit 里会关掉弹窗）
            .on_ok(move |_, window, cx| {
                on_enter(window, cx);
                false
            })
            // 按 Esc 或点右上角的叉：明确回答「取消」
            .on_cancel(move |_, _, _| {
                let _ = on_dismiss.send(None);
                true
            })
            .child(
                v_flex()
                    .gap_3()
                    .when_some(jump.clone(), |body, jump| {
                        body.child(div().text_sm().child(jump))
                    })
                    .child(div().text_sm().child(prompt.clone()))
                    .child(Input::new(&input))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(tr("此为 git 或 ssh 的原始提示，输入的内容不保存。")),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(Button::new("cancel").label(tr("取消")).on_click(
                        move |_, window, cx| {
                            let _ = reply.send(None);
                            window.close_dialog(cx);
                        },
                    ))
                    .child(
                        Button::new("ok")
                            .primary()
                            .label(tr("确定"))
                            .on_click(move |_, window, cx| submit(window, cx)),
                    ),
            )
    });
}

/// ssh 问要不要信任一台没见过的主机：显示主机和密钥指纹，让用户点「信任并连接」或「取消」。
/// 信任时回答的是 ssh 要的那个词 `yes`，ssh 随后把密钥记进 known_hosts；取消、关掉弹窗都算不信任，
/// ssh 放弃这次连接。回车不算确认：弹窗可能在用户正往别处打字时出来，点了按钮才信任。
fn open_host_key(
    prompt: &str,
    jump: Option<SharedString>,
    reply: Sender<Option<String>>,
    window: &mut Window,
    cx: &mut App,
) {
    let key = fr_core::askpass::host_key_prompt(prompt);
    let raw: SharedString = prompt.trim().to_string().into();
    window.open_dialog(cx, move |dialog, _, cx| {
        let (muted, border) = (cx.theme().muted_foreground, cx.theme().border);
        let (trust, cancel, dismiss) = (reply.clone(), reply.clone(), reply.clone());
        let body = v_flex().gap_3().when_some(jump.clone(), |body, jump| {
            body.child(div().text_sm().child(jump))
        });
        let body = match &key {
            Some(key) => {
                let host = match &key.address {
                    Some(address) => format!("{}（{address}）", key.host),
                    None => key.host.clone(),
                };
                let copy = key.fingerprint.clone();
                body.child(
                    div()
                        .text_sm()
                        .child(fr_core::tf!("首次连接 {host}，请核对密钥指纹。", host)),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .px_3()
                        .py_2()
                        .rounded(px(crate::ui::RADIUS))
                        .border_1()
                        .border_color(border)
                        .child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(key.key_type.clone()),
                                )
                                .child(
                                    Button::new("copy")
                                        .ghost()
                                        .xsmall()
                                        .label(tr("复制"))
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy.clone(),
                                            ));
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .text_sm()
                                .font_family("Consolas")
                                .child(key.fingerprint.clone()),
                        ),
                )
                // ssh 另外说的话（这把密钥还记在哪些名字下之类）原样列出
                .when(!key.notes.is_empty(), |body| {
                    body.child(
                        v_flex()
                            .text_xs()
                            .text_color(muted)
                            .children(key.notes.iter().map(|note| div().child(note.clone()))),
                    )
                })
            }
            // 拆不出主机和指纹：照 ssh 的原文显示
            None => body.child(div().text_sm().child(raw.clone())),
        };
        dialog
            .title(tr("确认主机密钥"))
            .w(px(520.))
            .on_ok(|_, _, _| false)
            // 按 Esc 或点右上角的叉：明确回答「取消」
            .on_cancel(move |_, _, _| {
                let _ = dismiss.send(None);
                true
            })
            .child(
                body.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(tr("信任后记入 known_hosts，此后密钥变化时将拒绝连接。")),
                ),
            )
            .footer(
                DialogFooter::new()
                    .child(Button::new("cancel").label(tr("取消")).on_click(
                        move |_, window, cx| {
                            let _ = cancel.send(None);
                            window.close_dialog(cx);
                        },
                    ))
                    .child(
                        Button::new("trust")
                            .primary()
                            .label(tr("信任并连接"))
                            .on_click(move |_, window, cx| {
                                let _ = trust.send(Some("yes".to_string()));
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}
