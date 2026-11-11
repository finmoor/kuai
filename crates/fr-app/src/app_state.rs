//! 全局状态：设置和各台服务器的连接。界面各部分共用一个 [`AppState`] 实体。

use fr_core::i18n::tr;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use fr_channel::{Channel, ConnectOptions, Connection};
use futures::FutureExt as _;
use futures::future::{LocalBoxFuture, Shared};
use gpui_kit::*;

use crate::settings::{self, RepoEntry, Server, Settings, TabGroup, WindowState};

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
            Some(Some(Ok(_))) => ConnStatus::Failed(tr("连接已断开").into()),
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
                let server =
                    server.ok_or_else(|| SharedString::from(tr("找不到这台服务器的设置")))?;
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

    /// 改一台服务器的名字、地址、转发选项；id 和它名下登记的仓库都留着。
    /// 返回连接参数变没变：变了就丢掉原来的连接，下次用的时候按新参数重连。
    pub fn update_server(
        &mut self,
        id: &str,
        name: String,
        destination: String,
        forward_agent: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(server) = self.settings.servers.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        let reconnect = server.destination != destination || server.forward_agent != forward_agent;
        server.name = name;
        server.destination = destination;
        server.forward_agent = forward_agent;
        if reconnect {
            self.conns.remove(id);
        }
        self.save();
        cx.notify();
        reconnect
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

    /// 记下当前工作区开着的标签页和选中的那一个。
    pub fn set_open_tabs(&mut self, tabs: Vec<RepoEntry>, active: usize) {
        let group = self.settings.workspace_mut();
        if group.tabs != tabs || group.active != active {
            group.tabs = tabs;
            group.active = active;
            self.save();
        }
    }

    /// 改设置里的几项（语言、主题、托盘……）并存盘。
    pub fn update_settings(&mut self, cx: &mut Context<Self>, edit: impl FnOnce(&mut Settings)) {
        edit(&mut self.settings);
        self.settings.normalize();
        self.save();
        cx.notify();
    }

    /// 新建一个空的工作区，返回它是第几个。名字和已有的重了就在后面加数字。
    pub fn add_workspace(&mut self, name: &str, cx: &mut Context<Self>) -> usize {
        let name = unique_name(
            name,
            self.settings.workspaces.iter().map(|g| g.name.as_str()),
        );
        self.settings.workspaces.push(TabGroup {
            name,
            tabs: Vec::new(),
            active: 0,
        });
        self.save();
        cx.notify();
        self.settings.workspaces.len() - 1
    }

    pub fn rename_workspace(&mut self, ix: usize, name: &str, cx: &mut Context<Self>) {
        let others = self
            .settings
            .workspaces
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != ix)
            .map(|(_, g)| g.name.as_str());
        let name = unique_name(name, others);
        if let Some(group) = self.settings.workspaces.get_mut(ix) {
            group.name = name;
            self.save();
            cx.notify();
        }
    }

    /// 删掉一个工作区（至少留一个）。里面登记的仓库还在仓库列表里，只是不再开着。
    pub fn remove_workspace(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.settings.workspaces.len() <= 1 || ix >= self.settings.workspaces.len() {
            return;
        }
        self.settings.workspaces.remove(ix);
        if self.settings.active_workspace > ix {
            self.settings.active_workspace -= 1;
        }
        self.settings.normalize();
        self.save();
        cx.notify();
    }

    pub fn set_active_workspace(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.settings.workspaces.len() && self.settings.active_workspace != ix {
            self.settings.active_workspace = ix;
            self.save();
            cx.notify();
        }
    }

    /// 往另一个工作区里加一个标签页（把标签页移过去时用）。
    pub fn push_tab_to_workspace(&mut self, ix: usize, entry: RepoEntry, cx: &mut Context<Self>) {
        if let Some(group) = self.settings.workspaces.get_mut(ix) {
            if !group.tabs.contains(&entry) {
                group.tabs.push(entry);
            }
            self.save();
            cx.notify();
        }
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

/// 名字去掉首尾空白；空的话叫「工作区」；和已有的重了就加上 2、3……
fn unique_name<'a>(name: &str, existing: impl Iterator<Item = &'a str>) -> String {
    let base = match name.trim() {
        "" => fr_core::i18n::tr("工作区"),
        name => name,
    };
    let existing: Vec<&str> = existing.collect();
    if !existing.contains(&base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|candidate| !existing.contains(&candidate.as_str()))
        .unwrap_or_else(|| base.to_string())
}

#[cfg(test)]
mod tests {
    use super::unique_name;

    #[test]
    fn 工作区的名字不重复() {
        let existing = ["默认", "工作", "工作 2"];
        assert_eq!(unique_name(" 新的 ", existing.iter().copied()), "新的");
        assert_eq!(unique_name("工作", existing.iter().copied()), "工作 3");
        assert_eq!(unique_name("  ", existing.iter().copied()), "工作区");
    }
}
