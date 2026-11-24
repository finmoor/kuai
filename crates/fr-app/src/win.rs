//! GPUI 没有包的几个窗口操作：把窗口藏起来、再叫出来、问它是不是最小化了、让系统画的边框跟着
//! 深色主题。Windows 上对着自己窗口的句柄调 user32、dwmapi；Linux 上 X11 用窗口号（x11rb），
//! Wayland 上只做得到「带着激活令牌叫到前面」（[`activate`]）。

use gpui_kit::Window;

/// 主窗口的句柄（Windows 的 HWND，X11 的窗口号）。窗口关掉以后就失效，不要存着跨窗口用。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hwnd(isize);

#[cfg(windows)]
mod imp {
    use super::Hwnd;
    use core::ffi::c_void;
    use gpui_kit::Window;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }

    /// `FLASHWINFO`
    #[repr(C)]
    struct FlashInfo {
        size: u32,
        hwnd: isize,
        flags: u32,
        count: u32,
        timeout: u32,
    }
    /// 任务栏上的按钮闪，一直到窗口回到前台
    const FLASHW_TRAY: u32 = 2;
    const FLASHW_TIMERNOFG: u32 = 12;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
        fn IsIconic(hwnd: isize) -> i32;
        fn SetForegroundWindow(hwnd: isize) -> i32;
        fn FlashWindowEx(info: *const FlashInfo) -> i32;
        fn GetCursorPos(point: *mut Point) -> i32;
        fn WindowFromPoint(point: Point) -> isize;
        fn GetAncestor(hwnd: isize, flags: u32) -> isize;
        fn GetClassNameW(hwnd: isize, name: *mut u16, len: i32) -> i32;
        fn SystemParametersInfoW(action: u32, param: u32, value: *mut c_void, ini: u32) -> i32;
    }
    const GA_ROOT: u32 = 2;
    const SPI_GETWHEELSCROLLLINES: u32 = 0x0068;
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: isize,
            attribute: u32,
            value: *const c_void,
            size: u32,
        ) -> i32;
    }
    const SW_HIDE: i32 = 0;
    const SW_SHOW: i32 = 5;
    const SW_RESTORE: i32 = 9;
    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;

    pub fn hwnd(window: &Window) -> Option<Hwnd> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        match handle.as_raw() {
            RawWindowHandle::Win32(handle) => Some(Hwnd(handle.hwnd.get())),
            _ => None,
        }
    }

    // SAFETY（这一组）：句柄来自还开着的主窗口；这几个函数对失效的句柄只是返回失败，不会出事

    pub fn hide(hwnd: Hwnd) {
        unsafe { ShowWindow(hwnd.0, SW_HIDE) };
    }

    /// 把藏起来或最小化的窗口叫出来，放到最前面。
    pub fn show(hwnd: Hwnd) {
        unsafe {
            if IsIconic(hwnd.0) != 0 {
                ShowWindow(hwnd.0, SW_RESTORE);
            } else {
                ShowWindow(hwnd.0, SW_SHOW);
            }
            SetForegroundWindow(hwnd.0);
        }
    }

    pub fn is_minimized(hwnd: Hwnd) -> bool {
        unsafe { IsIconic(hwnd.0) != 0 }
    }

    pub fn flash(hwnd: Hwnd) {
        let info = FlashInfo {
            size: size_of::<FlashInfo>() as u32,
            hwnd: hwnd.0,
            flags: FLASHW_TRAY | FLASHW_TIMERNOFG,
            count: 0,
            timeout: 0,
        };
        unsafe { FlashWindowEx(&info) };
    }

    pub fn cursor_on_taskbar() -> bool {
        let mut point = Point { x: 0, y: 0 };
        // SAFETY：只读鼠标位置和那里的窗口的类名，缓冲区长度照实给
        unsafe {
            if GetCursorPos(&mut point) == 0 {
                return false;
            }
            let root = GetAncestor(WindowFromPoint(point), GA_ROOT);
            if root == 0 {
                return false;
            }
            let mut name = [0u16; 64];
            let len = GetClassNameW(root, name.as_mut_ptr(), name.len() as i32).max(0) as usize;
            // 主屏的任务栏、别的屏上的任务栏
            matches!(
                String::from_utf16_lossy(&name[..len]).as_str(),
                "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
            )
        }
    }

    pub fn wheel_lines() -> f32 {
        let mut lines: u32 = 0;
        // SAFETY：这一项往给的地址写一个 u32
        let ok = unsafe {
            SystemParametersInfoW(SPI_GETWHEELSCROLLLINES, 0, (&raw mut lines).cast(), 0)
        };
        // 设成「一次滚一屏」时是 u32::MAX，按默认的 3 行算
        match (ok != 0, lines) {
            (true, 1..=100) => lines as f32,
            _ => 3.,
        }
    }

    pub fn set_dark_frame(hwnd: Hwnd, dark: bool) {
        let value: i32 = dark.into();
        // 值是一个 4 字节的 BOOL，长度照实给
        unsafe {
            DwmSetWindowAttribute(
                hwnd.0,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                (&raw const value).cast(),
                size_of::<i32>() as u32,
            );
        }
    }
}

/// Linux：X11 上用窗口号做这几件事（x11rb，另开一条连接，不碰 GPUI 的）；Wayland 上没有窗口号
/// （[`hwnd`] 是 None），藏不起来，调用的地方各自退而求其次（见 workspace 的托盘）。
#[cfg(target_os = "linux")]
mod imp {
    use super::Hwnd;
    use gpui_kit::Window;
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{
        AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, PropMode, UNMAP_NOTIFY_EVENT,
        UnmapNotifyEvent,
    };
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _;

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    pub fn hwnd(window: &Window) -> Option<Hwnd> {
        x11_id(window).map(|id| Hwnd(id as isize))
    }

    /// X11 上的窗口号；Wayland 上没有。
    pub fn x11_id(window: &Window) -> Option<u32> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        // GPUI 的 Window 自己也有 window_handle()，要写明是 raw-window-handle 的那个
        let handle = HasWindowHandle::window_handle(window).ok()?;
        match handle.as_raw() {
            RawWindowHandle::Xcb(handle) => Some(handle.window.get()),
            RawWindowHandle::Xlib(handle) => Some(handle.window as u32),
            _ => None,
        }
    }

    fn connect() -> Result<(RustConnection, u32)> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        Ok((conn, root))
    }

    fn atom(conn: &RustConnection, name: &str) -> Result<u32> {
        Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
    }

    fn report(what: &str, result: Result<()>) {
        if let Err(err) = result {
            log::warn!("{what}没成：{err}");
        }
    }

    /// 发完请求、关连接之前等 X 服务器处理完：只 flush 就关的话，服务器可能先看到连接断了，
    /// 还没读的请求整个丢掉（Xorg 收到对端挂断直接关这个客户端），窗口时藏时不藏。问一个有回复的
    /// 请求，回复按顺序回来，到了就说明前面的都处理了（Xlib 的 XCloseDisplay 也是先 XSync）。
    fn finish(conn: &RustConnection) -> Result<()> {
        conn.get_input_focus()?.reply()?;
        Ok(())
    }

    /// 发给窗口管理器的请求（EWMH）：经根窗口转交。
    fn ask_wm(conn: &RustConnection, root: u32, id: u32, kind: &str, data: [u32; 5]) -> Result<()> {
        let event = ClientMessageEvent::new(32, id, atom(conn, kind)?, data);
        conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        Ok(())
    }

    /// 藏起来：照 ICCCM 撤回窗口（取消映射，再给根窗口补一条合成的 UnmapNotify），窗口管理器把它
    /// 从任务栏、Dock 上拿掉。
    pub fn hide(hwnd: Hwnd) {
        let id = hwnd.0 as u32;
        report(
            "把窗口藏起来",
            (|| {
                let (conn, root) = connect()?;
                conn.unmap_window(id)?;
                let event = UnmapNotifyEvent {
                    response_type: UNMAP_NOTIFY_EVENT,
                    sequence: 0,
                    event: root,
                    window: id,
                    from_configure: false,
                };
                conn.send_event(
                    false,
                    root,
                    EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                    event,
                )?;
                finish(&conn)?;
                Ok(())
            })(),
        );
    }

    /// 叫出来：藏起来的重新映射，再请窗口管理器激活（最小化的也还原）。
    pub fn show(hwnd: Hwnd) {
        activate_x11(hwnd.0 as u32, None);
    }

    /// 窗口管理器把它最小化了（`_NET_WM_STATE` 里有 `_NET_WM_STATE_HIDDEN`）。
    pub fn is_minimized(hwnd: Hwnd) -> bool {
        let check = || -> Result<bool> {
            let (conn, _) = connect()?;
            let (state, hidden) = (
                atom(&conn, "_NET_WM_STATE")?,
                atom(&conn, "_NET_WM_STATE_HIDDEN")?,
            );
            let reply = conn
                .get_property(false, hwnd.0 as u32, state, AtomEnum::ATOM, 0, 64)?
                .reply()?;
            Ok(reply
                .value32()
                .is_some_and(|mut atoms| atoms.any(|a| a == hidden)))
        };
        check().unwrap_or(false)
    }

    /// X11 上把窗口叫到最前，最小化的、藏起来的也还原。
    ///
    /// 带着令牌（startup id，通知服务、托盘、启动器给的）：写进窗口的 `_NET_STARTUP_ID`，窗口管理器
    /// 按这次启动、点击的时间戳激活它（mutter、KWin 都看这个属性的变化）；再以应用的身份（source=1）
    /// 带上令牌里的时间戳发 `_NET_ACTIVE_WINDOW`。没有令牌：以任务栏、窗口切换器的身份（source=2）
    /// 发。GPUI 的 `activate_window` 发的是 source=1、时间戳为 0，mutter 防抢焦点，只让 Dock 上的
    /// 图标标出来。
    pub fn activate_x11(id: u32, token: Option<&str>) {
        report(
            "把窗口叫到前面",
            (|| {
                let (conn, root) = connect()?;
                conn.map_window(id)?;
                let mut source = 2;
                let mut time = 0;
                if let Some(token) = token {
                    let utf8 = atom(&conn, "UTF8_STRING")?;
                    let startup = atom(&conn, "_NET_STARTUP_ID")?;
                    conn.change_property8(PropMode::REPLACE, id, startup, utf8, token.as_bytes())?;
                    if let Some(stamp) = super::startup_time(token) {
                        (source, time) = (1, stamp);
                    }
                }
                ask_wm(
                    &conn,
                    root,
                    id,
                    "_NET_ACTIVE_WINDOW",
                    [source, time, 0, 0, 0],
                )?;
                finish(&conn)?;
                Ok(())
            })(),
        );
    }

    /// 不闪：要提醒时由 GPUI 的 `request_attention` 设 `_NET_WM_STATE_DEMANDS_ATTENTION`（见 workspace）。
    pub fn flash(_: Hwnd) {}
    pub fn cursor_on_taskbar() -> bool {
        false
    }
    pub fn wheel_lines() -> f32 {
        3.
    }
    /// 标题栏是自己画的（客户端装饰），没有系统画的边框要跟着变。
    pub fn set_dark_frame(_: Hwnd, _: bool) {}
}

/// Wayland 上拿别人给的 xdg-activation 令牌激活窗口。GPUI 自己的 `activate_window` 用的是它最近
/// 一次输入换来的令牌，窗口不在前台时没有这样的输入，合成器不给焦点（GNOME 弹「已就绪」）。
/// 借 GPUI 的连接（系统的 libwayland，`display_ptr`）和窗口的 wl_surface，在自己的事件队列上绑
/// `xdg_activation_v1`，发一次 `activate`。
#[cfg(target_os = "linux")]
mod wayland {
    use gpui_kit::Window;
    use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
    use std::cell::RefCell;
    use wayland_client::backend::{Backend, ObjectId};
    use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_registry::WlRegistry, wl_surface::WlSurface};
    use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
    use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;

    struct State;

    /// 绑好的 `xdg_activation_v1`，同一条连接上接着用：wl_registry 在合成器那边要等连接断开才释放，
    /// 原来每激活一次就新建一个 registry、绑一次，一直攒着。
    struct Activator {
        /// 是哪条连接的（GPUI 的 wl_display）
        display: usize,
        conn: Connection,
        queue: EventQueue<State>,
        _globals: GlobalList,
        activation: XdgActivationV1,
    }

    thread_local! {
        static ACTIVATOR: RefCell<Option<Activator>> = const { RefCell::new(None) };
    }

    impl Dispatch<WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: <WlRegistry as Proxy>::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<XdgActivationV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &XdgActivationV1,
            _: <XdgActivationV1 as Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    /// 不是 Wayland 的窗口返回 Ok(false)。
    pub fn activate(window: &Window, token: &str) -> anyhow::Result<bool> {
        let display = match HasDisplayHandle::display_handle(window)?.as_raw() {
            RawDisplayHandle::Wayland(display) => display.display,
            _ => return Ok(false),
        };
        let surface = match HasWindowHandle::window_handle(window)?.as_raw() {
            RawWindowHandle::Wayland(window) => window.surface,
            _ => return Ok(false),
        };
        let key = display.as_ptr() as usize;
        ACTIVATOR.with_borrow_mut(|slot| {
            if slot.as_ref().is_none_or(|made| made.display != key) {
                // SAFETY：指针是 GPUI 那条还开着的连接（它和程序一样长），这里只借用、不接管：外来的
                // display 不会被关掉
                let conn = Connection::from_backend(unsafe {
                    Backend::from_foreign_display(display.as_ptr().cast())
                });
                let (globals, queue) = registry_queue_init::<State>(&conn)?;
                let activation: XdgActivationV1 = globals.bind(&queue.handle(), 1..=1, ())?;
                *slot = Some(Activator {
                    display: key,
                    conn,
                    queue,
                    _globals: globals,
                    activation,
                });
            }
            let made = slot.as_mut().expect("刚建好");
            // 攒下的事件（全局对象增减）清掉
            made.queue.dispatch_pending(&mut State)?;
            // SAFETY：surface 是这条连接上窗口的（窗口还开着），ObjectId 只用来发这一次请求
            let surface_id =
                unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.as_ptr().cast()) }?;
            let surface = WlSurface::from_id(&made.conn, surface_id)?;
            made.activation.activate(token.to_string(), &surface);
            made.conn.flush()?;
            Ok(true)
        })
    }
}

/// 其他系统（macOS 等）：还没做，什么都不干。
#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::Hwnd;
    use gpui_kit::Window;

    pub fn hwnd(_: &Window) -> Option<Hwnd> {
        None
    }
    pub fn hide(_: Hwnd) {}
    pub fn show(_: Hwnd) {}
    pub fn is_minimized(_: Hwnd) -> bool {
        false
    }
    pub fn flash(_: Hwnd) {}
    pub fn cursor_on_taskbar() -> bool {
        false
    }
    pub fn wheel_lines() -> f32 {
        3.
    }
    pub fn set_dark_frame(_: Hwnd, _: bool) {}
}

pub fn hwnd(window: &Window) -> Option<Hwnd> {
    imp::hwnd(window)
}

/// startup id 末尾的 `_TIME<时间戳>`（X11 的启动通知规范）：点击、启动时的 X 服务器时间。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn startup_time(token: &str) -> Option<u32> {
    let at = token.rfind("_TIME")?;
    token[at + 5..].parse().ok().filter(|time| *time != 0)
}

/// Linux 上把窗口叫到最前，最小化的（X11 上藏起来的）也还原。`token` 是别人给的激活令牌：点系统
/// 通知时通知服务给的、点托盘图标时托盘给的、再次启动时启动器给的（X11 上是 startup id，Wayland
/// 上是 xdg-activation 令牌）。窗口不在前台时，带着它窗口管理器才肯把焦点交过来。
/// Windows 上用 [`show`]。
#[cfg(target_os = "linux")]
pub fn activate(window: &mut Window, token: Option<&str>) {
    if let Some(id) = imp::x11_id(window) {
        imp::activate_x11(id, token);
        return;
    }
    let done = match token {
        Some(token) => wayland::activate(window, token).unwrap_or_else(|err| {
            log::warn!("没能用激活令牌把窗口叫到前面：{err:#}");
            false
        }),
        None => false,
    };
    if !done {
        window.activate_window();
    }
}

#[cfg(not(target_os = "linux"))]
pub fn activate(_: &mut Window, _: Option<&str>) {}

/// 把窗口藏起来（任务栏上的按钮也没了），只能从托盘叫回来。
pub fn hide(hwnd: Hwnd) {
    imp::hide(hwnd)
}

pub fn show(hwnd: Hwnd) {
    imp::show(hwnd)
}

pub fn is_minimized(hwnd: Hwnd) -> bool {
    imp::is_minimized(hwnd)
}

/// 让任务栏上的按钮闪，到窗口回到前台为止（窗口不在前台时有事要说）。
pub fn flash(hwnd: Hwnd) {
    imp::flash(hwnd)
}

/// 鼠标在任务栏上：刚才的最小化是点了任务栏上的按钮（系统把在前面的窗口最小化了）。
pub fn cursor_on_taskbar() -> bool {
    imp::cursor_on_taskbar()
}

/// 滚轮滚一格走几行（系统设置里的，默认 3）。GPUI 报的滚动量已经乘过它。
pub fn wheel_lines() -> f32 {
    imp::wheel_lines()
}

/// 系统画的窗口边框、系统菜单跟着主题变深、变浅。
pub fn set_dark_frame(window: &Window, dark: bool) {
    if let Some(hwnd) = hwnd(window) {
        imp::set_dark_frame(hwnd, dark);
    }
}
