//! 密码弹窗：本机的 git、连接服务器的 ssh、服务器上的 git 要密码或密钥口令时，弹窗让用户填。
//!
//! 请求来自后台线程（见 `fr_channel::init_prompts`），那边一直等到这里回答；
//! 用户取消或者关掉弹窗就回答「取消」，对应的 git 或 ssh 会放弃这次认证。

use fr_core::i18n::tr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;

use futures::StreamExt as _;
use futures::channel::mpsc::{self, UnboundedReceiver};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
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

/// 有几个「不许弹窗」的操作正在做（自动获取）。
static QUIET: AtomicUsize = AtomicUsize::new(0);

/// 留着它的这段时间里，来的密码请求一律当作取消，不弹窗。
///
/// 自动获取是程序自己在后台做的：要密码的话让它这一次失败就行了，不能隔一会儿就弹个窗出来。
/// 请求里不带「是哪条 git 命令要的」，所以这段时间里别的操作要密码也会被当作取消——自动获取只在
/// 用户正看着的那个仓库上做，用户一动手做别的操作就先把它停掉，两者不会同时进行。
pub struct Quiet(());

pub fn quiet() -> Quiet {
    QUIET.fetch_add(1, Ordering::SeqCst);
    Quiet(())
}

impl Drop for Quiet {
    fn drop(&mut self) {
        QUIET.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 程序启动时调用：登记密码请求的处理函数，返回界面这头接收请求的口子。
pub fn init() -> Prompts {
    let (tx, rx) = mpsc::unbounded();
    let started = fr_channel::init_prompts(move |prompt, jump| {
        if QUIET.load(Ordering::SeqCst) > 0 {
            log::info!("自动获取要密码，这一次不问：{}", prompt.trim());
            return None;
        }
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
    let jump: Option<SharedString> = jump.map(|name| {
        fr_core::tf!(
            "此为跳板机 {name} 的登录提示。连接目标服务器前需先登录跳板机。",
            name
        )
        .into()
    });
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
                            .child(tr("此为 git 或 ssh 的原始提示，输入的内容不会被保存。")),
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
