//! 主机页「常驻进程」一栏（原型见 `docs/design/scheduled-tasks/` 第三版：常驻进程不在任务页配置，放在主机页）。
//!
//! 第一行：正在运行、运行了多久、几个终端、几个启用的定时任务、时区和现在几点，右边「重启…」。
//! 第二行：开机时启动（远程 Linux 写 crontab，远程 Windows 写登录启动项；本机是 Kuai 的开机自启，
//! Kuai 启动时会拉起本机的常驻进程）。没开又有启用的任务时一条黄色说明。最下面一行小字说什么时候
//! 退出。重启要先确认：会结束这台上的终端。

use fr_core::i18n::{tr, trc};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{host_now, text};
use crate::app_state::{AppState, ConnStatus};
use crate::machines::LOCAL;
use crate::ui;

/// 问到的情况多久以后再问一遍（主机页显示着时）
const STALE: std::time::Duration = std::time::Duration::from_secs(30);

/// 主机页选中 `key` 时这一栏；没连着、kuaid 太旧（不认 `daemon_status`）的不画。
pub fn render(
    app: &Entity<AppState>,
    key: &str,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let (restarting, daemon, windows, host) = {
        let state = app.read(cx);
        let conn = match key == LOCAL {
            true => state
                .links
                .get(LOCAL)
                .filter(|conn| conn.is_alive())
                .cloned(),
            false => match state.status(key) {
                ConnStatus::Ready(conn) => Some(conn),
                _ => None,
            },
        };
        let restarting = state.daemon_restarting(key);
        match conn {
            Some(conn) if conn.hello.protocol < 18 => return None,
            // 重启时连接断开一会儿：这一栏留着，写「正在重启…」，不然整栏消失、下面的仓库往上跳
            None if !restarting => return None,
            _ => {}
        }
        (
            restarting,
            state
                .daemon(key)
                .map(|daemon| (daemon.status.clone(), daemon.at)),
            state.machine(key).is_some_and(|m| m.windows),
            state.machine_name(key),
        )
    };
    // 没问过、问过一阵了：再问（画完再问，不在画的时候改）。重启时连着的是要退出的那个，不问
    if !restarting && daemon.as_ref().is_none_or(|(_, at)| at.elapsed() > STALE) {
        let (app, key) = (app.clone(), key.to_string());
        window.defer(cx, move |_, cx| {
            app.update(cx, |app, cx| app.refresh_daemon(&key, cx));
        });
    }
    let status = daemon.map(|(status, _)| status);
    let theme = cx.theme();
    let (muted, border, success, warning, secondary) = (
        theme.muted_foreground,
        theme.border,
        theme.success,
        theme.warning,
        theme.secondary,
    );
    let local = key == LOCAL;
    // 第一行
    let mut facts: Vec<String> = Vec::new();
    if let Some(status) = &status {
        facts.push(tf!("已运行 {time}", time = text::uptime(status.uptime)));
        facts.push(tf!("{n} 个终端", n = status.info.terms));
        facts.push(tf!("{n} 个启用的定时任务", n = status.tasks));
        let now = host_now(&status.clock);
        facts.push(tf!(
            "时区 {zone}，现在 {now}",
            zone = status.clock.offset_text(),
            now = format!("{:02}:{:02}", now.hour, now.minute)
        ));
    }
    let (dot, state_text) = match restarting {
        true => (muted, tr("正在重启…")),
        false => (success, tr("正在运行")),
    };
    let restart = {
        let (app, key, host) = (app.clone(), key.to_string(), host.clone());
        let (terms, task_terms) = status
            .as_ref()
            .map_or((0, 0), |s| (s.info.terms, s.task_terms));
        Button::new("daemon-restart")
            .small()
            .icon(ui::icon(IconName::RotateCw))
            .label(tr("重启…"))
            .disabled(restarting)
            .on_click(move |_, window, cx| {
                let (app, key) = (app.clone(), key.clone());
                let mut description = match (terms, task_terms) {
                    (0, _) => tr("这台主机上现在没有终端。").to_string(),
                    (n, 0) => tf!("将结束这台主机上的 {n} 个终端。", n),
                    (n, m) => tf!(
                        "将结束这台主机上的 {n} 个终端，其中 {m} 个是定时任务正在运行的。",
                        n,
                        m
                    ),
                };
                description.push_str(tr("定时任务、仓库列表和工作区不受影响。"));
                description.push('\n');
                description.push_str(tr(
                    "一般不需要重启。只有这个用户加入了新的用户组（例如 docker）、改了只在登录时生效的环境，或者常驻进程无响应时才需要：新开的终端沿用常驻进程启动时的身份和环境。",
                ));
                let title = match key == LOCAL {
                    true => tr("重启本机上的常驻进程？").to_string(),
                    false => tf!("重启 {host} 上的常驻进程？", host = host.to_string()),
                };
                crate::manager::confirm(
                    title,
                    description,
                    tr("重启"),
                    true,
                    window,
                    cx,
                    move |cx| app.update(cx, |app, cx| app.restart_daemon(&key, cx)),
                );
            })
    };
    let first = h_flex()
        .gap(px(8.))
        .px(px(14.))
        .py(px(10.))
        .text_xs()
        .child(div().flex_none().size(px(7.)).rounded_full().bg(dot))
        .child(div().flex_none().text_color(dot).child(state_text))
        // 窗口窄、英文长时放不下就折行，不截掉时区和现在几点
        .child(
            div()
                .min_w_0()
                .flex_1()
                .whitespace_normal()
                .text_color(muted)
                .child(facts.join(" · ")),
        )
        .child(div().flex_none().child(restart));
    // 第二行：开机时启动
    let (title, description, on, usable, error): (&str, &str, bool, bool, Option<String>) =
        match local {
            true => (
                tr("登录时启动"),
                tr(
                    "登录后自动启动 Kuai，本机的常驻进程随它启动，本机重启后定时任务才会继续运行。与设置中的「开机时自动启动」是同一项。",
                ),
                crate::autostart::enabled(),
                true,
                None,
            ),
            false => {
                let description = match windows {
                    true => tr(
                        "登录这台主机时自动启动常驻进程，定时任务才会继续运行。写入此用户的登录启动项。",
                    ),
                    false => tr(
                        "主机重启后自动启动常驻进程，定时任务才会继续运行。写入此用户的 crontab（@reboot），不需要 root。",
                    ),
                };
                match &status {
                    Some(status) => (
                        tr("开机时启动"),
                        description,
                        status.autostart == Some(true),
                        status.autostart.is_some() && !restarting,
                        (!status.autostart_error.is_empty())
                            .then(|| status.autostart_error.clone()),
                    ),
                    None => (tr("开机时启动"), description, false, false, None),
                }
            }
        };
    let toggle = {
        let (app, key) = (app.clone(), key.to_string());
        Switch::new("daemon-autostart")
            .accessibility_label(title)
            .checked(on)
            .disabled(!usable)
            .on_click(move |on: &bool, window, cx| {
                if key == LOCAL {
                    if let Err(err) = crate::autostart::set(*on) {
                        log::warn!("设置开机自启失败：{err:#}");
                    }
                    window.refresh();
                } else {
                    app.update(cx, |app, cx| app.set_autostart(&key, *on, cx));
                }
            })
    };
    let tasks = status.as_ref().map_or(0, |status| status.tasks);
    let warn = (!on && tasks > 0 && error.is_none()).then(|| match local {
        true => tf!(
            "未开启时，本机重启后要等 Kuai 启动，这期间的 {n} 个定时任务不会运行。",
            n = tasks
        ),
        false => tf!(
            "未开启时，{host} 重启后要等下次连接，这期间的 {n} 个定时任务不会运行。",
            host = host.to_string(),
            n = tasks
        ),
    });
    let second = v_flex()
        .gap(px(8.))
        .px(px(14.))
        .py(px(10.))
        .border_t_1()
        .border_color(border)
        .child(
            h_flex()
                .gap(px(12.))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .whitespace_normal()
                                .child(description),
                        ),
                )
                .child(div().flex_none().child(toggle)),
        )
        .children(error.map(|error| {
            div()
                .text_xs()
                .text_color(muted)
                .whitespace_normal()
                .child(error)
        }))
        .children(warn.map(|warn| {
            h_flex()
                .gap(px(8.))
                .px(px(10.))
                .py(px(7.))
                .rounded(px(ui::RADIUS))
                .bg(warning.opacity(0.12))
                .text_xs()
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .size(px(13.))
                        .text_color(warning),
                )
                .child(div().min_w_0().whitespace_normal().child(warn))
        }));
    let third = div()
        .px(px(14.))
        .py(px(9.))
        .border_t_1()
        .border_color(border)
        .text_xs()
        .text_color(muted)
        .whitespace_normal()
        .child(tr(
            "退出 Kuai 不会结束常驻进程。没有已连接的 Kuai、没有终端、也没有启用的定时任务时，10 分钟后自动退出，下次连接时再启动。升级 Kuai 时自动替换，终端不中断。",
        ));
    Some(
        v_flex()
            .gap(px(8.))
            // 说明放不下时换到标题下面一行，再放不下就折行，不截断
            .child(
                h_flex()
                    .flex_wrap()
                    .items_baseline()
                    .gap_x(px(8.))
                    .gap_y(px(2.))
                    .child(
                        div()
                            .flex_none()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(trc("标题", "常驻进程")),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .whitespace_normal()
                            .text_xs()
                            .text_color(muted)
                            .child(tr("在此主机上管理终端和定时任务的后台程序（kuaid）")),
                    ),
            )
            .child(
                v_flex()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(border)
                    .bg(secondary.opacity(0.5))
                    .child(first)
                    .child(second)
                    .child(third),
            )
            .into_any_element(),
    )
}
