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
    /// 同一次登录里又问了同一种：上次输入的不对
    retry: bool,
    reply: Sender<Option<String>>,
}

/// 等界面处理的密码请求。
pub type Prompts = UnboundedReceiver<Request>;

/// 程序启动时调用：登记密码请求的处理函数，返回界面这头接收请求的口子。
pub fn init() -> Prompts {
    let (tx, rx) = mpsc::unbounded();
    // 不许弹窗的（自动获取、自己在重连）在发问的那一头就当作取消了，到不了这里：见
    // `fr_core::askpass::NO_PROMPT` 和 `ConnectOptions::no_prompt`
    let started = fr_channel::init_prompts(move |prompt, jump, retry| {
        let (reply, answer) = std::sync::mpsc::channel();
        tx.unbounded_send(Request {
            prompt: prompt.to_string(),
            jump: jump.map(str::to_string),
            retry,
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
        retry,
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
    // 认出是哪一种：SSH 登录密码、私钥口令、仓库（HTTPS）的用户名或密码，标题和说明写清楚；
    // 原文照样列在下面
    let (title, explain) = classify(&prompt, secret);
    // ssh 答错了会再问：说一声上次不对（ssh 自己的那句 Permission denied 只在它的输出里）
    let wrong: Option<SharedString> = retry.then(|| {
        match fr_core::askpass::prompt_kind(&prompt) {
            fr_core::askpass::PromptKind::Passphrase => tr("口令不正确，请重新输入。"),
            _ => tr("密码不正确，请重新输入。"),
        }
        .into()
    });
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
        let (muted, danger) = (cx.theme().muted_foreground, cx.theme().danger);
        let (submit, reply) = (submit.clone(), reply.clone());
        let (on_enter, on_dismiss) = (submit.clone(), reply.clone());
        dialog
            .title(title)
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
                    .when_some(explain.clone(), |body, explain| {
                        body.child(div().text_sm().whitespace_normal().child(explain))
                    })
                    .when_some(wrong.clone(), |body, wrong| {
                        body.child(div().text_sm().text_color(danger).child(wrong))
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .font_family(crate::fonts::mono())
                            .whitespace_normal()
                            .child(prompt.clone()),
                    )
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

/// 主机密钥确认框里的「怎么核对」：在主机上运行的命令（可复制），核对不了时怎么办。
fn verify_hint(key_type: &str, muted: Hsla, border: Hsla) -> impl IntoElement {
    let kind = key_type.to_ascii_lowercase();
    let file = if kind.contains("ecdsa") {
        "ssh_host_ecdsa_key.pub"
    } else if kind.contains("rsa") {
        "ssh_host_rsa_key.pub"
    } else {
        "ssh_host_ed25519_key.pub"
    };
    let command = format!("ssh-keygen -lf /etc/ssh/{file}");
    let copy = command.clone();
    v_flex()
        .gap_1()
        .text_xs()
        .text_color(muted)
        .child(tr(
            "核对方法：在主机上运行以下命令，或查看云服务商控制台中显示的主机密钥指纹。",
        ))
        .child(
            h_flex()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .px_2()
                        .py_1()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(border)
                        .font_family(crate::fonts::mono())
                        .child(command),
                )
                .child(
                    Button::new("copy-verify")
                        .ghost()
                        .xsmall()
                        .label(tr("复制"))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                        }),
                ),
        )
        .child(tr("无法核对时，请确认主机地址填写正确后再信任。"))
}

/// 认出提示问的是什么：标题，和一句说明（认不出时没有，只列原文）。看 ssh、git 固定的英文说法；
/// 主机上的 git 翻译了的提示认不出，照旧按「需要密码」「需要输入」。
fn classify(prompt: &str, secret: bool) -> (&'static str, Option<SharedString>) {
    let lower = prompt.to_lowercase();
    // 引号里的东西：私钥的路径、仓库的网址
    let quoted = || {
        let start = prompt.find('\'')? + 1;
        let end = start + prompt[start..].find('\'')?;
        Some(prompt[start..end].to_string())
    };
    if lower.contains("passphrase") {
        let key = quoted().unwrap_or_default();
        return (
            tr("解锁私钥"),
            Some(fr_core::tf!("请输入私钥 {key} 的口令，不是主机的登录密码。", key).into()),
        );
    }
    if lower.starts_with("username for") || lower.starts_with("password for") {
        let url = quoted().unwrap_or_default();
        // 放在句子中间：英文要小写
        let what = match lower.starts_with("username") {
            true => fr_core::i18n::trc("句中", "用户名"),
            false => tr("密码或访问令牌"),
        };
        return (
            tr("远程仓库要求登录"),
            Some(
                fr_core::tf!(
                    "远程仓库 {url} 要求登录，请输入代码托管平台的{what}。",
                    url,
                    what
                )
                .into(),
            ),
        );
    }
    // ssh 的登录密码：`user@host's password:`，或 `(user@host) Password:`
    if secret && lower.contains("password") {
        return (
            tr("主机登录密码"),
            Some(tr("请输入 SSH 登录主机用的密码。").into()),
        );
    }
    match secret {
        true => (tr("需要密码"), None),
        false => (tr("需要输入"), None),
    }
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
                    // 英文界面用半角括号
                    Some(address) => {
                        fr_core::tf!("{host}（{address}）", host = key.host.as_str(), address)
                    }
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
                                .font_family(crate::fonts::mono())
                                .child(key.fingerprint.clone()),
                        ),
                )
                // 怎么核对：在主机上看它自己的指纹（按密钥的类型找文件），或看云服务商控制台
                .child(verify_hint(&key.key_type, muted, border))
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

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn 认出是哪一种密码() {
        let (title, explain) = classify(
            "Enter passphrase for key '/home/me/.ssh/id_ed25519': ",
            true,
        );
        assert_eq!(title, "解锁私钥");
        assert!(explain.unwrap().contains("/home/me/.ssh/id_ed25519"));
        let (title, explain) = classify("Password for 'https://me@github.com': ", true);
        assert_eq!(title, "远程仓库要求登录");
        assert!(explain.unwrap().contains("https://me@github.com"));
        let (title, _) = classify("Username for 'https://github.com': ", false);
        assert_eq!(title, "远程仓库要求登录");
        let (title, _) = classify("root@203.0.113.8's password: ", true);
        assert_eq!(title, "主机登录密码");
        let (title, explain) = classify("Verification code: ", false);
        assert_eq!((title, explain), ("需要输入", None));
    }
}
