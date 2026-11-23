//! 系统通知：Windows 上是屏幕右下角弹出来、之后留在通知中心里的那种，Linux 上是桌面的通知（见
//! Linux 的 `imp`）。Kuai 不在前台时用它说「某个终端停下了」，点一下回到 Kuai、前往那个终端。
//!
//! 不用界面库带的那一套（`Notification::in_app_and_system`）：它要先给整个进程设 AppUserModelID，
//! 任务栏按这个标识把窗口和固定的图标对上号，1.14 以前固定到任务栏上的 Kuai 图标没有这个标识，
//! 设了以后运行着的窗口会和固定的图标分成两个。发通知其实只要一个登记过的标识，不必是进程自己的：
//! 在 `HKCU\Software\Classes\AppUserModelId\<标识>` 下写上显示的名字和图标，用这个标识建通知器。
//! 进程的标识不动，任务栏照旧。
//!
//! 通知只在 Kuai 运行期间点得动（没有登记后台激活）：退出时把发过的都收回，下次启动第一次发之前
//! 也先清一遍（上次是被结束掉的话会留下点不动的）。
//!
//! 环境变量 FR_TOAST_ID 可以换一个标识（测试用，免得和平时用的 Kuai 的通知混在一起）。

use futures::channel::mpsc;

/// 用户点了一条通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Click {
    /// [`Toasts::show`] 时给的标记
    pub tag: String,
    /// 通知服务给的激活令牌（Linux：`ActivationToken`，X11 上是 startup id，Wayland 上是
    /// xdg-activation 令牌），拿它才能把窗口叫到前面（见 `win::activate`）。Windows 上没有
    pub token: Option<String>,
}

/// 用户点了哪条通知。
pub type Clicks = mpsc::UnboundedReceiver<Click>;

/// 通知里的一行字：去掉控制字符（终端的最后一行可能带着），太长的截短。
fn clean(text: &str) -> String {
    const MAX: usize = 200;
    let mut out: String = text
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX + 1)
        .collect();
    if out.chars().count() > MAX {
        out = out.chars().take(MAX).collect();
        out.push('…');
    }
    out.trim().to_string()
}

/// 写进 XML 里的字。
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// 通知的内容：标题一行、正文一行（空着就只有标题），不出声。
#[cfg_attr(not(windows), allow(dead_code))]
fn document(title: &str, body: &str) -> String {
    let (title, body) = (clean(title), clean(body));
    let mut xml = String::from(r#"<toast><visual><binding template="ToastGeneric">"#);
    xml.push_str(&format!("<text>{}</text>", escape(&title)));
    if !body.is_empty() {
        xml.push_str(&format!("<text>{}</text>", escape(&body)));
    }
    xml.push_str(r#"</binding></visual><audio silent="true"/></toast>"#);
    xml
}

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::hash::{DefaultHasher, Hash as _, Hasher as _};

    use futures::channel::mpsc;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager, ToastNotifier};
    use windows::core::{HSTRING, IInspectable};
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    /// 通知中心里 Kuai 那一栏的标识
    const APP_ID: &str = "Finmoor.Kuai";
    /// 通知分的组：收回一条时要组名和标记一起给
    const GROUP: &str = "terminal";
    /// 图标放在设置文件旁边
    const ICON_FILE: &str = "notification-icon.png";
    const ICON: &[u8] = include_bytes!("../assets/icon.png");

    fn app_id() -> String {
        std::env::var("FR_TOAST_ID")
            .ok()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| APP_ID.to_string())
    }

    fn key_path(app_id: &str) -> String {
        format!(r"Software\Classes\AppUserModelId\{app_id}")
    }

    fn icon_path() -> Option<std::path::PathBuf> {
        Some(crate::settings::file().parent()?.join(ICON_FILE))
    }

    /// 登记标识：显示的名字和图标。每次启动后第一次发通知时写一遍（程序挪了位置、图标换了都跟着变）。
    fn register(app_id: &str) -> std::io::Result<()> {
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(key_path(app_id))?;
        key.set_value("DisplayName", &"Kuai")?;
        if let Some(path) = icon_path() {
            let stale =
                std::fs::metadata(&path).map_or(true, |meta| meta.len() != ICON.len() as u64);
            if stale {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&path, ICON)?;
            }
            key.set_value("IconUri", &path.to_string_lossy().to_string())?;
        }
        Ok(())
    }

    /// 卸载时：登记的标识和图标都删掉。
    pub fn unregister() {
        let app_id = app_id();
        let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(key_path(&app_id));
        if let Some(path) = icon_path() {
            let _ = std::fs::remove_file(path);
        }
    }

    /// 系统给通知的标记最长 64 个字符：换成定长的。
    fn system_tag(tag: &str) -> HSTRING {
        let mut hasher = DefaultHasher::new();
        tag.hash(&mut hasher);
        HSTRING::from(format!("{:016x}", hasher.finish()))
    }

    pub struct Toasts {
        app_id: HSTRING,
        /// 第一次发的时候才建
        notifier: Option<ToastNotifier>,
        /// 发出去还没收回的：留着对象，点它的时候才有回调
        shown: HashMap<String, ToastNotification>,
        clicks: mpsc::UnboundedSender<super::Click>,
    }

    impl Toasts {
        pub fn new(clicks: mpsc::UnboundedSender<super::Click>) -> Self {
            Self {
                app_id: HSTRING::from(app_id()),
                notifier: None,
                shown: HashMap::new(),
                clicks,
            }
        }

        fn notifier(&mut self) -> windows::core::Result<ToastNotifier> {
            if let Some(notifier) = &self.notifier {
                return Ok(notifier.clone());
            }
            if let Err(err) = register(&self.app_id.to_string()) {
                log::warn!("系统通知的标识没登记上，通知可能不显示名字和图标：{err}");
            }
            // 上次运行留下的（点不动了）先清掉
            if let Ok(history) = ToastNotificationManager::History() {
                let _ = history.ClearWithId(&self.app_id);
            }
            let notifier = ToastNotificationManager::CreateToastNotifierWithId(&self.app_id)?;
            self.notifier = Some(notifier.clone());
            Ok(notifier)
        }

        fn try_show(&mut self, tag: &str, title: &str, body: &str) -> windows::core::Result<()> {
            let notifier = self.notifier()?;
            let xml = XmlDocument::new()?;
            xml.LoadXml(&HSTRING::from(super::document(title, body)))?;
            let toast = ToastNotification::CreateToastNotification(&xml)?;
            toast.SetTag(&system_tag(tag))?;
            toast.SetGroup(&HSTRING::from(GROUP))?;
            let (clicks, clicked) = (self.clicks.clone(), tag.to_string());
            // 回调在系统的线程上：只往主线程递个话
            toast.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(
                move |_, _| {
                    let click = super::Click {
                        tag: clicked.clone(),
                        token: None,
                    };
                    clicks.unbounded_send(click).ok();
                    Ok(())
                },
            ))?;
            // 同一个标记的上一条：系统会换掉它，这边的对象也换掉
            notifier.Show(&toast)?;
            self.shown.insert(tag.to_string(), toast);
            Ok(())
        }

        pub fn show(&mut self, tag: &str, title: &str, body: &str) {
            if let Err(err) = self.try_show(tag, title, body) {
                log::warn!("系统通知没发出去：{err}");
            }
        }

        pub fn remove(&mut self, tag: &str) {
            let Some(toast) = self.shown.remove(tag) else {
                return;
            };
            if let Some(notifier) = &self.notifier {
                let _ = notifier.Hide(&toast);
            }
            // 已经从屏幕上收进通知中心的，`Hide` 不一定管得到：按标记再删一遍
            if let Ok(history) = ToastNotificationManager::History() {
                let _ = history.RemoveGroupedTagWithId(
                    &system_tag(tag),
                    &HSTRING::from(GROUP),
                    &self.app_id,
                );
            }
        }

        pub fn clear(&mut self) {
            if self.shown.is_empty() {
                return;
            }
            self.shown.clear();
            if let Ok(history) = ToastNotificationManager::History() {
                let _ = history.ClearWithId(&self.app_id);
            }
        }
    }
}

/// Linux：freedesktop 的通知接口（`org.freedesktop.Notifications`，经 D-Bus）。GPUI 自带的那一套
/// 同一个标记不会换掉上一条、也收不回，这里自己来：一个后台线程连着会话总线，记下每个标记的通知号，
/// 再发时带上它（换掉上一条），收回时按号关掉；用户点了通知（`ActionInvoked` 的 `default`）就把
/// 标记交出去，连同通知服务在这之前发来的激活令牌（`ActivationToken`，规范 1.2 起有）。
#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;
    use std::time::Duration;

    use futures::channel::mpsc;
    use futures::{FutureExt as _, StreamExt as _};

    enum Job {
        Show {
            tag: String,
            title: String,
            body: String,
        },
        Remove(String),
        /// 做完了往里回个话（退出前收回时要等它做完）
        Clear(std::sync::mpsc::SyncSender<()>),
    }

    pub struct Toasts {
        jobs: mpsc::UnboundedSender<Job>,
    }

    impl Toasts {
        pub fn new(clicks: mpsc::UnboundedSender<super::Click>) -> Self {
            let (jobs, rx) = mpsc::unbounded();
            let spawned = std::thread::Builder::new()
                .name("kuai-notify".into())
                .spawn(move || {
                    if let Err(err) = futures::executor::block_on(serve(rx, clicks)) {
                        log::warn!("系统通知用不了：{err}");
                    }
                });
            if let Err(err) = spawned {
                log::warn!("无法启动发系统通知的线程：{err}");
            }
            Self { jobs }
        }

        pub fn show(&mut self, tag: &str, title: &str, body: &str) {
            let _ = self.jobs.unbounded_send(Job::Show {
                tag: tag.to_string(),
                title: title.to_string(),
                body: body.to_string(),
            });
        }

        pub fn remove(&mut self, tag: &str) {
            let _ = self.jobs.unbounded_send(Job::Remove(tag.to_string()));
        }

        /// 等它做完（最多半秒）：退出时调的，不等的话进程先退了，通知还留在那里点不动。
        pub fn clear(&mut self) {
            let (done, wait) = std::sync::mpsc::sync_channel(1);
            if self.jobs.unbounded_send(Job::Clear(done)).is_ok() {
                let _ = wait.recv_timeout(Duration::from_millis(500));
            }
        }
    }

    async fn serve(
        mut jobs: mpsc::UnboundedReceiver<Job>,
        clicks: mpsc::UnboundedSender<super::Click>,
    ) -> zbus::Result<()> {
        let connection = zbus::Connection::session().await?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await?;
        let mut actions = proxy.receive_signal("ActionInvoked").await?.fuse();
        let mut tokens = proxy.receive_signal("ActivationToken").await?.fuse();
        // 标记 → 通知号
        let mut shown: HashMap<String, u32> = HashMap::new();
        // 通知号 → 点它时通知服务给的激活令牌（先于 ActionInvoked 发来）
        let mut activation: HashMap<u32, String> = HashMap::new();
        loop {
            futures::select! {
                job = jobs.next() => {
                    let Some(job) = job else { break };
                    if let Err(err) = run(&proxy, &mut shown, job).await {
                        log::warn!("系统通知没发出去：{err}");
                    }
                }
                signal = tokens.next() => {
                    let Some(signal) = signal else { break };
                    if let Ok((id, token)) = signal.body().deserialize::<(u32, String)>() {
                        activation.insert(id, token);
                    }
                }
                signal = actions.next() => {
                    let Some(signal) = signal else { break };
                    let Ok((id, action)) = signal.body().deserialize::<(u32, String)>() else {
                        continue;
                    };
                    // 两个信号是按顺序来的，可 select! 两边都有的时候随便挑一边：先把已经到了的令牌收完
                    while let Some(Some(signal)) = tokens.next().now_or_never() {
                        if let Ok((id, token)) = signal.body().deserialize::<(u32, String)>() {
                            activation.insert(id, token);
                        }
                    }
                    let token = activation.remove(&id);
                    let tag = shown.iter().find(|(_, shown)| **shown == id).map(|(tag, _)| tag);
                    if let Some(tag) = tag
                        && action == "default"
                    {
                        let click = super::Click { tag: tag.clone(), token };
                        clicks.unbounded_send(click).ok();
                    }
                }
            }
        }
        Ok(())
    }

    async fn run(
        proxy: &zbus::Proxy<'_>,
        shown: &mut HashMap<String, u32>,
        job: Job,
    ) -> zbus::Result<()> {
        match job {
            Job::Show { tag, title, body } => {
                let replaces = shown.get(&tag).copied().unwrap_or(0);
                // 桌面靠 desktop-entry 对上 kuai.desktop，显示 Kuai 的名字和图标
                let hints = HashMap::from([("desktop-entry", zbus::Variant::Str("kuai".into()))]);
                let id: u32 = proxy
                    .call(
                        "Notify",
                        &(
                            "Kuai",
                            replaces,
                            "kuai",
                            super::clean(&title),
                            // 正文可能按标记语言解析：尖括号、& 换掉
                            super::escape(&super::clean(&body)),
                            vec!["default", "Kuai"],
                            hints,
                            -1i32,
                        ),
                    )
                    .await?;
                shown.insert(tag, id);
            }
            Job::Remove(tag) => {
                if let Some(id) = shown.remove(&tag) {
                    close(proxy, id).await;
                }
            }
            Job::Clear(done) => {
                for (_, id) in shown.drain() {
                    close(proxy, id).await;
                }
                let _ = done.try_send(());
            }
        }
        Ok(())
    }

    /// 关掉一条；已经被用户关掉、过期了的不算错。
    async fn close(proxy: &zbus::Proxy<'_>, id: u32) {
        let _: zbus::Result<()> = proxy.call("CloseNotification", &(id,)).await;
    }

    pub fn unregister() {}
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use futures::channel::mpsc;

    pub struct Toasts;

    impl Toasts {
        pub fn new(_: mpsc::UnboundedSender<super::Click>) -> Self {
            Self
        }
        pub fn show(&mut self, _: &str, _: &str, _: &str) {}
        pub fn remove(&mut self, _: &str) {}
        pub fn clear(&mut self) {}
    }

    pub fn unregister() {}
}

/// 这次运行发出去的系统通知。丢弃时全部收回。
pub struct Toasts(imp::Toasts);

impl Toasts {
    /// 第二样是「点了哪条」的那一头，主线程上等着收。
    pub fn new() -> (Self, Clicks) {
        let (tx, rx) = mpsc::unbounded();
        (Self(imp::Toasts::new(tx)), rx)
    }

    /// 发一条；`tag` 相同的上一条被换掉。发不出去只记一条警告（系统里关了通知也算发出去了）。
    pub fn show(&mut self, tag: &str, title: &str, body: &str) {
        self.0.show(tag, title, body);
    }

    /// 收回一条（已经去看过那个终端了）。
    pub fn remove(&mut self, tag: &str) {
        self.0.remove(tag);
    }

    /// 全部收回（回到 Kuai 了，标题栏上有个数）。
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

impl Drop for Toasts {
    fn drop(&mut self) {
        self.0.clear();
    }
}

/// 卸载时调用：删掉登记的标识和图标。
pub fn unregister() {
    imp::unregister();
}

#[cfg(test)]
mod tests {
    use super::{clean, document};

    #[test]
    fn 通知的内容() {
        assert_eq!(
            document("repo 的终端已停下", "Continue? (y/n)"),
            concat!(
                r#"<toast><visual><binding template="ToastGeneric">"#,
                "<text>repo 的终端已停下</text><text>Continue? (y/n)</text>",
                r#"</binding></visual><audio silent="true"/></toast>"#
            )
        );
        // 正文空着就只有标题
        assert!(!document("标题", "  ").contains("<text></text>"));
        // 尖括号、引号换掉，控制字符去掉
        let xml = document("a<b> & \"c\"", "x\u{1b}[0m\ty");
        assert!(xml.contains("<text>a&lt;b&gt; &amp; &quot;c&quot;</text>"));
        assert!(xml.contains("<text>x[0my</text>"));
    }

    #[test]
    fn 太长的截短() {
        let long = "字".repeat(300);
        let cut = clean(&long);
        assert_eq!(cut.chars().count(), 201);
        assert!(cut.ends_with('…'));
        assert_eq!(clean("  短的  "), "短的");
    }
}
