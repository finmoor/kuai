//! 全局状态：设置和各台服务器的连接。界面各部分共用一个 [`AppState`] 实体。

use fr_core::i18n::tr;
use fr_core::tf;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use fr_channel::{Channel, ConnectOptions, Connection, Jump};
use futures::FutureExt as _;
use futures::future::{LocalBoxFuture, Shared};
use gpui_kit::*;

use crate::settings::{
    self, Auth, Layout, RepoEntry, RepoMemo, Server, Settings, TabGroup, WindowState,
};

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
    /// 经跳板机连接、还在连的：已经登录上几台了（跳板机按顺序算）
    stages: HashMap<String, usize>,
    /// 过一会儿再存设置的任务（拖动窗口时位置一直在变，不每次都写文件）
    save_later: Option<Task<()>>,
}

impl AppState {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            conns: HashMap::new(),
            stages: HashMap::new(),
            save_later: None,
        }
    }

    pub fn server(&self, id: &str) -> Option<&Server> {
        self.settings.server(id)
    }

    /// 正在连接时显示的那句话：经跳板机的写明走到哪一台了。
    pub fn connecting_text(&self, id: &str) -> SharedString {
        let route = self.settings.route(id);
        let done = self.stages.get(id).copied().unwrap_or(0);
        match (route.get(done), route.last()) {
            (Some(jump), _) => tf!("正在连接跳板机 {name}…", name = jump.name).into(),
            // 跳板机都登录上了：在连目标服务器；它也登录上以后是启动 kuaid
            (None, Some(last)) if done == route.len() => {
                tf!("正在经由 {name} 连接…", name = last.name).into()
            }
            (None, Some(_)) => tr("正在启动 kuaid…").into(),
            (None, None) => tr("正在连接…").into(),
        }
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
        let route: Vec<Server> = self.settings.route(id).into_iter().cloned().collect();
        // 经跳板机的：后台线程每登录上一台报一次，这边记下来、刷新列表上的那句话
        self.stages.remove(id);
        let (stage_tx, mut stage_rx) = futures::channel::mpsc::unbounded::<usize>();
        let on_stage = (!route.is_empty()).then(|| {
            Arc::new(move |done: usize| drop(stage_tx.unbounded_send(done)))
                as fr_channel::StageHandler
        });
        let stage_id = id.to_string();
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(done) = stage_rx.next().await {
                let updated = this.update(cx, |this, cx| {
                    this.stages.insert(stage_id.clone(), done);
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
        })
        .detach();
        let connecting = cx
            .background_executor()
            .spawn(async move {
                let server =
                    server.ok_or_else(|| SharedString::from(tr("未找到该服务器的设置")))?;
                let mut options = connect_options(&server);
                options.jumps = route.iter().map(jump_options).collect();
                options.on_stage = on_stage;
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
                        "，上传了 kuaid"
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

    /// 加一台服务器，返回给它起的 id（`server.id` 传什么都不看）。
    pub fn add_server(&mut self, mut server: Server, cx: &mut Context<Self>) -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let id = format!("s{millis}");
        server.id = id.clone();
        self.settings.servers.push(server);
        self.save();
        cx.notify();
        id
    }

    /// 改一台服务器（按 `server.id` 找）；它名下登记的仓库都留着。
    /// 返回连接参数变没变（`secret_changed`：存着的密码、口令动过）：变了就丢掉原来的连接，
    /// 下次用的时候按新参数重连。
    pub fn update_server(
        &mut self,
        server: Server,
        secret_changed: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(old) = self.settings.servers.iter_mut().find(|s| s.id == server.id) else {
            return false;
        };
        let reconnect = secret_changed
            || old.destination != server.destination
            || old.forward_agent != server.forward_agent
            || old.auth != server.auth
            || old.identity_file != server.identity_file
            || old.jump != server.jump;
        let id = server.id.clone();
        *old = server;
        if reconnect {
            self.conns.remove(&id);
        }
        self.save();
        cx.notify();
        reconnect
    }

    /// 从列表里去掉服务器和它的仓库；服务器上的东西不动。
    pub fn remove_server(&mut self, id: &str, cx: &mut Context<Self>) {
        self.settings.servers.retain(|s| s.id != id);
        // 经它连接的服务器改为直接连接（已经连着的不受影响，下次连接时才按新的来）
        for server in &mut self.settings.servers {
            if server.jump.as_deref() == Some(id) {
                server.jump = None;
            }
        }
        self.settings
            .repos
            .retain(|r| r.server.as_deref() != Some(id));
        self.settings.normalize();
        self.conns.remove(id);
        // 存着的密码、口令跟着删掉
        crate::secrets::delete(id);
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
        self.settings.normalize();
        self.save();
        cx.notify();
    }

    /// 记下当前工作区开着的标签页和选中的那一个。
    pub fn set_open_tabs(&mut self, tabs: Vec<RepoEntry>, active: usize) {
        let group = self.settings.workspace_mut();
        if group.tabs != tabs || group.active != active {
            // 重新打开了的不算「关掉的」了；在这个工作区里打开的仓库就是它的仓库
            group.closed.retain(|e| !tabs.contains(e));
            for tab in &tabs {
                group.adopt(tab);
            }
            group.pinned.retain(|e| tabs.contains(e));
            group.tabs = tabs;
            group.active = active;
            self.save();
        }
    }

    /// 把仓库从当前工作区去掉（它还在仓库列表里，别的工作区里的也不动）。
    pub fn remove_from_workspace(&mut self, entry: &RepoEntry, cx: &mut Context<Self>) {
        self.settings.workspace_mut().drop_repo(entry);
        self.save();
        cx.notify();
    }

    /// 固定、取消固定当前工作区里的一个标签页；返回现在是不是固定的。
    pub fn toggle_pinned(&mut self, entry: &RepoEntry, cx: &mut Context<Self>) -> bool {
        let group = self.settings.workspace_mut();
        let pinned = !group.is_pinned(entry);
        group.pinned.retain(|e| e != entry);
        if pinned {
            group.pinned.push(entry.clone());
        }
        group.pinned_first();
        self.save();
        cx.notify();
        pinned
    }

    /// 记下当前工作区刚关掉的一个标签页（Ctrl+Shift+T 和快速打开里能找回来）。
    pub fn note_closed_tab(&mut self, entry: RepoEntry) {
        if self.settings.repos.contains(&entry) {
            self.settings.workspace_mut().note_closed(entry);
        }
    }

    /// 当前工作区最近关掉的标签页，最近的在前。
    pub fn closed_tabs(&self) -> Vec<RepoEntry> {
        self.settings.workspace().closed.iter().rev().cloned().collect()
    }

    /// 改设置里的几项（语言、主题、托盘……）并存盘。
    pub fn update_settings(&mut self, cx: &mut Context<Self>, edit: impl FnOnce(&mut Settings)) {
        edit(&mut self.settings);
        self.settings.normalize();
        // 终端的设置：开着的终端马上跟着变
        crate::terminal::set_prefs(&self.settings.terminal, cx);
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
            ..TabGroup::default()
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

    /// 往另一个工作区里加一个标签页（把标签页移过去时用）：仓库也成了那个工作区的。
    pub fn push_tab_to_workspace(&mut self, ix: usize, entry: RepoEntry, cx: &mut Context<Self>) {
        if let Some(group) = self.settings.workspaces.get_mut(ix) {
            group.closed.retain(|e| *e != entry);
            group.adopt(&entry);
            if !group.tabs.contains(&entry) {
                group.tabs.push(entry);
            }
            self.save();
            cx.notify();
        }
    }

    /// 改各块的大小（拖分隔条时一直在变），稍后存盘。
    pub fn edit_layout(&mut self, cx: &mut Context<Self>, edit: impl FnOnce(&mut Layout)) {
        let before = self.settings.layout;
        edit(&mut self.settings.layout);
        if self.settings.layout != before {
            self.save_soon(cx);
            cx.notify();
        }
    }

    /// 改一个仓库记的东西（隐藏的分支、最近的提交说明）并存盘。
    pub fn edit_memo(
        &mut self,
        repo: &RepoEntry,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut RepoMemo),
    ) {
        self.settings.edit_memo(repo, edit);
        self.save();
        cx.notify();
    }

    fn save_soon(&mut self, cx: &mut Context<Self>) {
        self.save_later = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(800))
                .await;
            this.update(cx, |this, _| this.save()).ok();
        }));
    }

    /// 记下窗口的位置和大小，稍后存盘。
    pub fn set_window_state(&mut self, state: WindowState, cx: &mut Context<Self>) {
        if self.settings.window == Some(state) {
            return;
        }
        self.settings.window = Some(state);
        self.save_soon(cx);
    }

    /// 仓库显示的名字：起过名字就用它，没有就是目录名。
    pub fn repo_name(&self, entry: &RepoEntry) -> SharedString {
        self.settings.repo_name(entry).to_string().into()
    }

    /// 给仓库改名（只改显示的名字，目录不动）；空的就恢复成目录名。
    pub fn rename_repo(&mut self, entry: &RepoEntry, name: &str, cx: &mut Context<Self>) {
        self.settings.set_repo_name(entry, name);
        self.save();
        cx.notify();
    }

    /// 标签页上显示的名字：仓库的名字，服务器上的再加服务器名。
    pub fn repo_title(&self, entry: &RepoEntry) -> SharedString {
        let name = self.settings.repo_name(entry);
        match &entry.server {
            None => name.to_string().into(),
            Some(id) => format!(
                "{name} · {}",
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

/// 登录这台服务器用的选项：私钥、密码登录、存着的密码或口令。不含跳板机（见 [`AppState::connect`]）。
pub(crate) fn connect_options(server: &Server) -> ConnectOptions {
    ConnectOptions {
        forward_agent: server.forward_agent,
        identity_file: server.key_file().map(str::to_string),
        password_auth: server.auth == Auth::Password,
        secret: crate::secrets::get(&server.id),
        ..ConnectOptions::default()
    }
}

/// 把这台服务器当跳板机时怎么登录它：沿用它自己的设置。ssh-agent 不往跳板机上转发。
fn jump_options(server: &Server) -> Jump {
    Jump {
        name: server.name.clone(),
        destination: server.destination.clone(),
        identity_file: server.key_file().map(str::to_string),
        password_auth: server.auth == Auth::Password,
        secret: crate::secrets::get(&server.id),
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
