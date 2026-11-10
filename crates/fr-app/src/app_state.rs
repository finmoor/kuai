//! 全局状态：设置和各台服务器的连接。界面各部分共用一个 [`AppState`] 实体。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use fr_channel::{Channel, ConnectOptions, Connection};
use futures::FutureExt as _;
use futures::future::{LocalBoxFuture, Shared};
use gpui_kit::*;

use crate::settings::{self, RepoEntry, Server, Settings, ThemeChoice, WindowState};

/// 正在进行或已经结束的连接；多处等同一个连接时共用。
type Connecting = Shared<Task<Result<Arc<Connection>, SharedString>>>;

/// 一台服务器的连接状态。
pub enum ConnStatus {
    Idle,
    Connecting,
    Ready(Arc<Connection>),
    Failed(SharedString),
}

pub struct AppState {
    pub settings: Settings,
    conns: HashMap<String, Connecting>,
    /// 过一会儿再存设置的任务（拖动窗口时位置一直在变，不每次都写文件）
    save_later: Option<Task<()>>,
}

impl AppState {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            conns: HashMap::new(),
            save_later: None,
        }
    }

    pub fn server(&self, id: &str) -> Option<&Server> {
        self.settings.servers.iter().find(|s| s.id == id)
    }

    pub fn status(&self, id: &str) -> ConnStatus {
        match self.conns.get(id).map(|c| c.peek()) {
            None => ConnStatus::Idle,
            Some(None) => ConnStatus::Connecting,
            Some(Some(Ok(conn))) if conn.is_alive() => ConnStatus::Ready(conn.clone()),
            Some(Some(Ok(_))) => ConnStatus::Failed("连接已断开".into()),
            Some(Some(Err(err))) => ConnStatus::Failed(err.clone()),
        }
    }

    /// 连接服务器；已经连上或正在连就沿用。返回等连接结果的 future。
    pub fn connect(&mut self, id: &str, cx: &mut Context<Self>) -> Connecting {
        if let Some(existing) = self.conns.get(id) {
            match existing.peek() {
                None => return existing.clone(),
                Some(Ok(conn)) if conn.is_alive() => return existing.clone(),
                _ => {}
            }
        }
        let server = self.server(id).cloned();
        let connecting = cx
            .background_executor()
            .spawn(async move {
                let server = server.ok_or_else(|| SharedString::from("找不到这台服务器的设置"))?;
                let options = ConnectOptions {
                    forward_agent: server.forward_agent,
                };
                Connection::connect(&server.destination, &options)
                    .map(Arc::new)
                    .map_err(|err| SharedString::from(format!("{err:#}")))
            })
            .shared();
        self.conns.insert(id.to_string(), connecting.clone());
        // 连完刷新界面上的连接状态
        let done = connecting.clone();
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            match done.await {
                Ok(conn) => log::info!(
                    "连接 {}：{} ms，{}{}",
                    conn.destination,
                    conn.connect_time.as_millis(),
                    conn.hello.git,
                    if conn.installed {
                        "，上传了 agent"
                    } else {
                        ""
                    }
                ),
                Err(err) => log::warn!("连接服务器 {id} 失败：{err}"),
            }
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
        cx.notify();
        connecting
    }

    /// 仓库所在的通道：本机仓库直接给；服务器上的仓库等连接好。
    pub fn channel_for(
        &mut self,
        server: Option<&str>,
        cx: &mut Context<Self>,
    ) -> LocalBoxFuture<'static, Result<Channel, SharedString>> {
        match server {
            None => futures::future::ready(Ok(Channel::Local)).boxed_local(),
            Some(id) => {
                let connecting = self.connect(id, cx);
                async move { connecting.await.map(Channel::Remote) }.boxed_local()
            }
        }
    }

    pub fn add_server(
        &mut self,
        name: String,
        destination: String,
        forward_agent: bool,
        cx: &mut Context<Self>,
    ) -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let id = format!("s{millis}");
        self.settings.servers.push(Server {
            id: id.clone(),
            name,
            destination,
            forward_agent,
        });
        self.save();
        cx.notify();
        id
    }

    /// 从列表里去掉服务器和它的仓库；服务器上的东西不动。
    pub fn remove_server(&mut self, id: &str, cx: &mut Context<Self>) {
        self.settings.servers.retain(|s| s.id != id);
        self.settings
            .repos
            .retain(|r| r.server.as_deref() != Some(id));
        self.conns.remove(id);
        self.save();
        cx.notify();
    }

    pub fn add_repo(&mut self, entry: RepoEntry, cx: &mut Context<Self>) {
        if !self.settings.repos.contains(&entry) {
            self.settings.repos.push(entry);
            self.save();
            cx.notify();
        }
    }

    /// 从列表里去掉仓库；仓库本身不动。
    pub fn remove_repo(&mut self, entry: &RepoEntry, cx: &mut Context<Self>) {
        self.settings.repos.retain(|r| r != entry);
        self.save();
        cx.notify();
    }

    pub fn set_open_tabs(&mut self, tabs: Vec<RepoEntry>, active: usize) {
        if self.settings.open_tabs != tabs || self.settings.active_tab != active {
            self.settings.open_tabs = tabs;
            self.settings.active_tab = active;
            self.save();
        }
    }

    pub fn set_theme(&mut self, theme: ThemeChoice, cx: &mut Context<Self>) {
        self.settings.theme = theme;
        self.save();
        cx.notify();
    }

    /// 记下窗口的位置和大小，稍后存盘。
    pub fn set_window_state(&mut self, state: WindowState, cx: &mut Context<Self>) {
        if self.settings.window == Some(state) {
            return;
        }
        self.settings.window = Some(state);
        self.save_later = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(800))
                .await;
            this.update(cx, |this, _| this.save()).ok();
        }));
    }

    /// 标签页上显示的名字：本机仓库是目录名，服务器上的再加服务器名。
    pub fn repo_title(&self, entry: &RepoEntry) -> SharedString {
        match &entry.server {
            None => entry.name().to_string().into(),
            Some(id) => format!(
                "{} · {}",
                entry.name(),
                self.server(id).map_or("?", |s| s.name.as_str())
            )
            .into(),
        }
    }

    fn save(&self) {
        if let Err(err) = settings::save(&self.settings) {
            log::error!("{err:#}");
        }
    }
}
