//! 托盘图标。设置里开了「最小化到托盘」或「关闭时收到托盘」才有。
//!
//! 左键点图标（或双击）把窗口叫出来；右键菜单里有「显示窗口」和「退出」。
//! 图标用 exe 里嵌的 1 号图标，和任务栏上的是同一个。

use fr_core::i18n::tr;
#[cfg(windows)]
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui_kit::*;

/// 这个系统上有没有托盘图标（Linux 上还没做：设置里不显示托盘的选项）。
pub const AVAILABLE: bool = cfg!(windows);

/// 用户在托盘上做了什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum TrayAction {
    /// 把窗口叫出来
    Show,
    /// 退出程序
    Quit,
}

pub struct Tray {
    #[cfg(windows)]
    _icon: tray_icon::TrayIcon,
    _task: Task<()>,
}

impl Tray {
    /// 建托盘图标；`on_action` 在界面线程上调用。建不出来（没有托盘、图标资源缺了）返回 `None`，
    /// 调用的地方就当没有托盘：最小化、关闭都按普通窗口来，不能把窗口藏起来再也叫不回来。
    #[cfg(windows)]
    pub fn new<T: 'static>(
        cx: &mut Context<T>,
        on_action: impl Fn(&mut T, TrayAction, &mut Context<T>) + 'static,
    ) -> Option<Self> {
        use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
        use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

        let menu = Menu::new();
        let show = MenuItem::with_id("show", tr("显示窗口"), true, None);
        let quit = MenuItem::with_id("quit", tr("退出 Kuai"), true, None);
        menu.append_items(&[&show, &PredefinedMenuItem::separator(), &quit])
            .map_err(|err| log::warn!("建托盘菜单失败：{err}"))
            .ok()?;
        let icon = tray_icon::Icon::from_resource(1, None)
            .map_err(|err| log::warn!("读托盘图标失败：{err}"))
            .ok()?;
        let icon = TrayIconBuilder::new()
            .with_tooltip("Kuai")
            .with_icon(icon)
            .with_menu(Box::new(menu))
            // 左键留给「显示窗口」，菜单只在右键出
            .with_menu_on_left_click(false)
            .build()
            .map_err(|err| log::warn!("建托盘图标失败：{err}"))
            .ok()?;

        // 托盘的事件在它自己的窗口过程里回调，转成消息发给界面上的任务
        let (tx, mut rx) = mpsc::unbounded::<TrayAction>();
        let from_icon = tx.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            let clicked = matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            );
            if clicked {
                let _ = from_icon.unbounded_send(TrayAction::Show);
            }
        }));
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let action = match event.id.0.as_str() {
                "show" => TrayAction::Show,
                "quit" => TrayAction::Quit,
                _ => return,
            };
            let _ = tx.unbounded_send(action);
        }));
        let task = cx.spawn(async move |this, cx| {
            while let Some(action) = rx.next().await {
                if this
                    .update(cx, |this, cx| on_action(this, action, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Some(Self {
            _icon: icon,
            _task: task,
        })
    }

    #[cfg(not(windows))]
    pub fn new<T: 'static>(
        _cx: &mut Context<T>,
        _on_action: impl Fn(&mut T, TrayAction, &mut Context<T>) + 'static,
    ) -> Option<Self> {
        let _ = (tr, mpsc::unbounded::<TrayAction>);
        None
    }
}
