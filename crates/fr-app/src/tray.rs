//! 托盘图标。设置里开了「最小化到托盘」或「关闭时收到托盘」才有。
//!
//! 左键点图标（或双击）把窗口叫出来；右键菜单里有「显示窗口」和「退出」。
//! Windows 上图标用 exe 里嵌的 1 号图标，和任务栏上的是同一个。
//!
//! Linux 上是 StatusNotifierItem（经 D-Bus，用 ksni，不链接 GTK）：KDE 自带托盘，Ubuntu 的 GNOME
//! 带着 AppIndicator 扩展，别的 GNOME（Fedora 等）默认没有。启动时问一下有没有托盘（[`probe`]），
//! 没有就不显示托盘的选项；开机自启时桌面可能还没起好托盘，托盘图标照样建，等它上线（ksni 盯着），
//! 上线、下线都记下来，窗口只在托盘真在的时候才收进去（[`available`]）；下线时收在里面的窗口拿出来
//! （[`TrayAction::Lost`]）。Wayland 上窗口藏不起来
//! （GPUI 做不到），「关闭到托盘」改成最小化，「最小化到托盘」不显示（见 [`can_hide`]）。

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use fr_core::i18n::tr;
#[cfg(any(windows, target_os = "linux"))]
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui_kit::*;

/// 有没有托盘：0 还不知道，1 没有，2 有。
static HOST: AtomicU8 = AtomicU8::new(if cfg!(windows) { 2 } else { 0 });
/// 主窗口能不能藏起来（Windows、X11 能；Wayland 不能）。
static CAN_HIDE: AtomicBool = AtomicBool::new(cfg!(windows));

/// 这个桌面上现在有没有托盘（还不知道的算没有）。Linux 上启动时 [`probe`] 问，之后跟着托盘图标看到的
/// 上线、下线变。不等：原来还不知道时在界面线程上等它问完，最多卡 2 秒，问不到就记成没有托盘。
pub fn available() -> bool {
    HOST.load(Ordering::Relaxed) == 2
}

/// 还不知道有没有托盘（Linux 上启动时的 [`probe`] 还没问完）。
pub fn unknown() -> bool {
    HOST.load(Ordering::Relaxed) == 0
}

/// 主窗口能不能藏进托盘（不在任务栏上）。Wayland 上不能，只能最小化。
pub fn can_hide() -> bool {
    CAN_HIDE.load(Ordering::Relaxed)
}

/// 开主窗口时记下：拿得到窗口句柄（Windows、X11）就藏得起来。
pub fn set_can_hide(can: bool) {
    CAN_HIDE.store(can, Ordering::Relaxed);
}

/// Linux：在后台问一下会话总线上有没有托盘（`org.kde.StatusNotifierWatcher`，且登记了显示托盘的
/// 宿主）。启动时尽早调，建托盘时（[`available`]）一般早就问完了。Windows 上总是有，什么都不做。
pub fn probe() {
    #[cfg(target_os = "linux")]
    {
        let spawned = std::thread::Builder::new()
            .name("kuai-tray-probe".into())
            .spawn(|| {
                let found = linux::host_present().unwrap_or_else(|err| {
                    log::info!("问不到托盘：{err}");
                    false
                });
                log::info!("托盘：{}", if found { "有" } else { "没有" });
                HOST.store(if found { 2 } else { 1 }, Ordering::Relaxed);
            });
        if spawned.is_err() {
            HOST.store(1, Ordering::Relaxed);
        }
    }
}

/// 用户在托盘上做了什么。
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub enum TrayAction {
    /// 把窗口叫出来。`token` 是托盘给的激活令牌（Linux 上 KDE 会给，见 `win::activate`）
    Show { token: Option<String> },
    /// 退出程序
    Quit,
    /// 托盘不在了（Linux：托盘宿主下线）：收在里面的窗口要拿出来
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Lost,
}

pub struct Tray {
    #[cfg(windows)]
    _icon: tray_icon::TrayIcon,
    #[cfg(target_os = "linux")]
    handle: ksni::blocking::Handle<linux::KuaiTray>,
    _task: Task<()>,
}

#[cfg(target_os = "linux")]
impl Drop for Tray {
    fn drop(&mut self) {
        // 撤掉托盘图标；不等它撤完（退出时进程一走，宿主也会收掉）
        drop(self.handle.shutdown());
    }
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
                let _ = from_icon.unbounded_send(TrayAction::Show { token: None });
            }
        }));
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let action = match event.id.0.as_str() {
                "show" => TrayAction::Show { token: None },
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

    /// Linux：托盘服务在 ksni 自己的线程上跑，点了什么经通道交给界面上的任务。现在没有托盘也建：
    /// 开机自启时桌面的托盘可能还没起来，ksni 等它上线再登记（`assume_sni_available`）；窗口收不
    /// 收进托盘看 [`available`]。连不上会话总线才返回 `None`。
    #[cfg(target_os = "linux")]
    pub fn new<T: 'static>(
        cx: &mut Context<T>,
        on_action: impl Fn(&mut T, TrayAction, &mut Context<T>) + 'static,
    ) -> Option<Self> {
        use ksni::blocking::TrayMethods as _;
        let (tx, mut rx) = mpsc::unbounded::<TrayAction>();
        let tray = linux::KuaiTray {
            actions: tx,
            show: tr("显示窗口").to_string(),
            quit: tr("退出 Kuai").to_string(),
        };
        let handle = tray
            .assume_sni_available(true)
            .spawn()
            .map_err(|err| log::warn!("建托盘图标失败：{err}"))
            .ok()?;
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
            handle,
            _task: task,
        })
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    pub fn new<T: 'static>(
        _cx: &mut Context<T>,
        _on_action: impl Fn(&mut T, TrayAction, &mut Context<T>) + 'static,
    ) -> Option<Self> {
        let _ = (tr, mpsc::unbounded::<TrayAction>);
        None
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::OnceLock;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use futures::channel::mpsc;

    use super::TrayAction;

    pub struct KuaiTray {
        pub actions: mpsc::UnboundedSender<TrayAction>,
        pub show: String,
        pub quit: String,
    }

    impl KuaiTray {
        /// 点了图标、「显示窗口」：带上托盘给的激活令牌。KDE 在点击的同时或稍后经
        /// `ProvideXdgActivationToken` 送来，等它一下（最多 100 毫秒）。
        fn show_window(&self) {
            let mut token = ksni::current_xdg_activation_token();
            for _ in 0..20 {
                if token.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
                token = ksni::current_xdg_activation_token();
            }
            let _ = self.actions.unbounded_send(TrayAction::Show { token });
        }
    }

    impl ksni::Tray for KuaiTray {
        fn id(&self) -> String {
            "kuai".into()
        }

        fn title(&self) -> String {
            "Kuai".into()
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip {
                title: "Kuai".into(),
                ..Default::default()
            }
        }

        /// 只给像素图、不给图标名：图标名要宿主到图标主题里找，.deb 装的在系统目录、解压版在用户
        /// 目录，有的宿主找不到就是空白
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            icons().clone()
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            self.show_window();
        }

        fn watcher_online(&self) {
            log::info!("托盘上线了");
            super::HOST.store(2, Ordering::Relaxed);
        }

        /// 托盘下线了（桌面重启了外壳、关了托盘扩展，或者还没起来）：窗口先不往里收，接着等它回来。
        fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
            log::info!("托盘不在：{reason:?}");
            super::HOST.store(1, Ordering::Relaxed);
            let _ = self.actions.unbounded_send(TrayAction::Lost);
            true
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::StandardItem;
            vec![
                StandardItem {
                    label: self.show.clone(),
                    activate: Box::new(|tray: &mut Self| tray.show_window()),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: self.quit.clone(),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.actions.unbounded_send(TrayAction::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    /// 托盘图标的几种大小（ARGB32，按网络字节序），从应用图标缩出来，只算一次。
    fn icons() -> &'static Vec<ksni::Icon> {
        static ICONS: OnceLock<Vec<ksni::Icon>> = OnceLock::new();
        ICONS.get_or_init(|| {
            let Ok(image) = image::load_from_memory(include_bytes!("../assets/icon.png")) else {
                return Vec::new();
            };
            [22u32, 32, 48, 64]
                .into_iter()
                .map(|size| {
                    let rgba = image
                        .resize_exact(size, size, image::imageops::FilterType::Lanczos3)
                        .to_rgba8();
                    let data = rgba
                        .pixels()
                        .flat_map(|p| [p[3], p[0], p[1], p[2]])
                        .collect();
                    ksni::Icon {
                        width: size as i32,
                        height: size as i32,
                        data,
                    }
                })
                .collect()
        })
    }

    /// 会话总线上有托盘：有 `org.kde.StatusNotifierWatcher`，且它说登记了显示托盘的宿主。
    pub fn host_present() -> zbus::Result<bool> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = tx.send(futures::executor::block_on(ask()));
        });
        rx.recv_timeout(Duration::from_secs(2)).unwrap_or(Ok(false))
    }

    async fn ask() -> zbus::Result<bool> {
        let connection = zbus::Connection::session().await?;
        let dbus = zbus::fdo::DBusProxy::new(&connection).await?;
        let name = zbus::names::BusName::try_from("org.kde.StatusNotifierWatcher")?;
        if !dbus.name_has_owner(name).await? {
            return Ok(false);
        }
        let watcher = zbus::Proxy::new(
            &connection,
            "org.kde.StatusNotifierWatcher",
            "/StatusNotifierWatcher",
            "org.kde.StatusNotifierWatcher",
        )
        .await?;
        watcher.get_property("IsStatusNotifierHostRegistered").await
    }
}
