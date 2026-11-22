//! GPUI 没有包的几个 Windows 窗口操作：把窗口藏起来、再叫出来、问它是不是最小化了、
//! 让系统画的边框跟着深色主题。都是对着自己窗口的句柄调 user32、dwmapi。

use gpui_kit::Window;

/// 主窗口的句柄。句柄是个整数，窗口关掉以后就失效，不要存着跨窗口用。
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

#[cfg(not(windows))]
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

/// Linux（X11）上把窗口叫到最前，最小化的也还原：以任务栏、窗口切换器的身份（source=2）发
/// `_NET_ACTIVE_WINDOW`。GPUI 的 `activate_window` 发的标的是应用自己（source=1）、时间戳为 0，
/// mutter 防抢焦点，只让 Dock 上的图标标出来。Wayland 上交给 GPUI；Windows 上用 [`show`]。
#[cfg(target_os = "linux")]
pub fn raise(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{ClientMessageEvent, ConnectionExt as _, EventMask};
    // GPUI 的 Window 自己也有 window_handle()，要写明是 raw-window-handle 的那个
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let id = match handle.as_raw() {
        RawWindowHandle::Xcb(handle) => handle.window.get(),
        RawWindowHandle::Xlib(handle) => handle.window as u32,
        _ => return,
    };
    let sent = (|| -> Result<(), Box<dyn std::error::Error>> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let atom = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;
        let event = ClientMessageEvent::new(32, id, atom, [2u32, 0, 0, 0, 0]);
        conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        conn.flush()?;
        Ok(())
    })();
    if let Err(err) = sent {
        log::warn!("没能把窗口叫到前面：{err}");
    }
}

#[cfg(not(target_os = "linux"))]
pub fn raise(_: &Window) {}

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
