//! 终端里打印的主机上的本地网址（`http://localhost:3000`）：在本机浏览器里直接打开连的是本机，
//! 不是主机。按住 Ctrl 单击（或右键「在浏览器中打开」）这样的网址时，先问一句，确认后经 SSH 把主机
//! 上的这个端口转到本机（另起一个 `ssh -N -L`，见 [`fr_channel::forward`]），再在浏览器里打开转过来
//! 的地址。
//!
//! - 本机端口尽量和主机上的一样（3000 转到本机 3000），被占了就随便挑一个空的。
//! - 同一台主机的同一个端口只转一次，之后再点直接打开；ssh 退出了（断线、主机重启）下次点重新转。
//! - 转发一直开着，退出 Kuai 时结束（[`stop_all`]）。
//! - 经由别的主机连接的主机（它的 kuaid 替这边连）没有这边的 ssh 可用，不转发，说一声、复制地址。

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fr_channel::Forwarder;
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use crate::app_state::{AppState, connect_options, jump_options};

/// 转发开好以后等本机端口接得通，最多等这么久（要密码时用户还要输入）
const READY_WAIT: Duration = Duration::from_secs(60);

/// 一个开着的转发：本机端口和那个 ssh。
type Forward = (u16, Arc<Mutex<Forwarder>>);

/// 开着的转发：（主机 id、主机上的端口）→ 转发。
#[derive(Default)]
struct Forwards(HashMap<(String, u16), Forward>);

impl Global for Forwards {}

/// 网址是不是主机自己的本地地址（localhost、127.0.0.1、0.0.0.0、[::1]）：是的话返回端口和端口后面
/// 那一截（路径、查询、锚点）。没写端口的按 http 80、https 443。
pub fn loopback(url: &str) -> Option<(u16, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    // 带用户名的（user@host）不认
    if authority.contains('@') {
        return None;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (host, after) = v6.split_once(']')?;
            (host, after.strip_prefix(':'))
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    let local = matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "0.0.0.0" | "::1"
    );
    if !local {
        return None;
    }
    let port = match port {
        Some(port) => port.parse().ok()?,
        None => default_port,
    };
    Some((port, tail.to_string()))
}

/// 主机 `host` 上的终端里点了 `url`（[`loopback`] 认出来的本地地址）：转发好了直接打开，没转发先问。
pub fn open(app: Entity<AppState>, host: String, url: String, window: &mut Window, cx: &mut App) {
    let Some((port, tail)) = loopback(&url) else {
        return crate::shell::report(crate::shell::open_link(&url), window, cx);
    };
    // 已经转发着、ssh 还在：直接打开
    let existing = cx.try_global::<Forwards>().and_then(|forwards| {
        let (local, process) = forwards.0.get(&(host.clone(), port))?;
        let alive = process.lock().ok()?.exited().is_none();
        alive.then_some(*local)
    });
    if let Some(local) = existing {
        return open_local(local, &tail, window, cx);
    }
    let Some(server) = app.read(cx).server(&host).cloned() else {
        // 经由别的主机连接的：这边没有能用的 ssh
        cx.write_to_clipboard(ClipboardItem::new_string(url));
        let text = tr("经由其他主机连接的主机暂不支持端口转发，已复制地址。");
        window.push_notification(Notification::warning(text), cx);
        return;
    };
    let name = server.name.clone();
    let description = tf!(
        "该地址在主机 {name} 上，直接在本机打开连不到。将经 SSH 把主机上的端口 {port} 转发到本机，再在浏览器中打开。转发在退出 Kuai 时结束。",
        name = &name,
        port
    );
    crate::manager::confirm_in(
        tf!("转发主机上的端口 {port}？", port),
        description,
        tr("转发并打开"),
        false,
        window,
        cx,
        move |window, cx| start(app.clone(), host.clone(), port, tail.clone(), window, cx),
    );
}

/// 起转发、等本机端口接得通、打开浏览器。
fn start(
    app: Entity<AppState>,
    host: String,
    port: u16,
    tail: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(server) = app.read(cx).server(&host).cloned() else {
        return;
    };
    let route: Vec<crate::settings::Server> = app
        .read(cx)
        .settings
        .route(&host)
        .into_iter()
        .cloned()
        .collect();
    let mut options = connect_options(&server);
    options.jumps = route.iter().map(jump_options).collect();
    let local = free_port(port);
    let destination = server.destination.clone();
    window.push_notification(
        Notification::info(tf!("正在转发主机上的端口 {port}…", port)),
        cx,
    );
    let task = cx.background_executor().spawn(async move {
        let mut process = fr_channel::forward(&destination, &options, local, port)
            .map_err(|err| format!("{err:#}"))?;
        // 等本机这一头接得通：ssh 登录上、转发开好以后才接得通；ssh 先退出了就是没成
        let started = Instant::now();
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, local));
        loop {
            if let Some(why) = process.exited() {
                return Err(why);
            }
            if TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok() {
                return Ok(process);
            }
            if started.elapsed() > READY_WAIT {
                return Err(tr("等待转发就绪超时").to_string());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let result = task.await;
        handle
            .update(cx, |_, window, cx| match result {
                Ok(process) => {
                    let forwards = cx.default_global::<Forwards>();
                    forwards
                        .0
                        .insert((host, port), (local, Arc::new(Mutex::new(process))));
                    let text = tf!(
                        "已将主机上的端口 {port} 转发到本机 localhost:{local}",
                        port,
                        local
                    );
                    window.push_notification(Notification::success(text), cx);
                    open_local(local, &tail, window, cx);
                }
                Err(err) => {
                    let text = tf!("端口转发失败：{err}", err);
                    window.push_notification(Notification::error(text).autohide(false), cx);
                }
            })
            .ok();
    })
    .detach();
}

/// 在浏览器里打开转过来的地址。写 localhost：开发服务器常常只认 localhost 这个名字。
fn open_local(local: u16, tail: &str, window: &mut Window, cx: &mut App) {
    let url = format!("http://localhost:{local}{tail}");
    crate::shell::report(crate::shell::open_link(&url), window, cx);
}

/// 本机用哪个端口：和主机上的一样空着就用它，不然让系统挑一个空的。
fn free_port(wanted: u16) -> u16 {
    let bind = |port: u16| TcpListener::bind((Ipv4Addr::LOCALHOST, port)).ok();
    if wanted >= 1024
        && let Some(listener) = bind(wanted)
    {
        drop(listener);
        return wanted;
    }
    bind(0)
        .and_then(|listener| listener.local_addr().ok())
        .map_or(wanted, |address| address.port())
}

/// 退出 Kuai 时结束所有转发。
pub fn stop_all(cx: &mut App) {
    // 丢掉就结束了（见 `Forwarder` 的 Drop）
    if cx.has_global::<Forwards>() {
        cx.set_global(Forwards::default());
    }
}

#[cfg(test)]
mod tests {
    use super::loopback;

    #[test]
    fn 认出主机自己的本地地址() {
        assert_eq!(
            loopback("http://localhost:3000/app?x=1"),
            Some((3000, "/app?x=1".into()))
        );
        assert_eq!(
            loopback("http://127.0.0.1:5173"),
            Some((5173, String::new()))
        );
        assert_eq!(loopback("http://0.0.0.0:8000/"), Some((8000, "/".into())));
        assert_eq!(
            loopback("http://[::1]:4000/#top"),
            Some((4000, "/#top".into()))
        );
        assert_eq!(loopback("https://localhost/"), Some((443, "/".into())));
        assert_eq!(loopback("http://LOCALHOST:80"), Some((80, String::new())));
        assert_eq!(loopback("https://example.com:3000/"), None);
        assert_eq!(loopback("http://192.168.1.5:3000/"), None);
        assert_eq!(loopback("http://user@localhost:3000/"), None);
        assert_eq!(loopback("ftp://localhost:21/"), None);
    }
}
