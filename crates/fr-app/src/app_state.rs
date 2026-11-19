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

use crate::machines::{LOCAL, Machine, split_relay};
use crate::settings::{self, Auth, Layout, RepoEntry, RepoMemo, Server, Settings, WindowState};

/// 正在进行或已经结束的连接；多处等同一个连接时共用。
pub(crate) type Connecting = Shared<Task<Result<Arc<Connection>, SharedString>>>;

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
    /// 映射到本机的目录（见 `mapping`）
    pub(crate) mounts: crate::mapping::Mounts,
    /// 各台电脑保存的仓库列表和工作区，这次运行里读到的（见 `machines`）
    pub(crate) machines: HashMap<String, Machine>,
    /// 各台电脑那边已经接好的连接：改动经它交过去，改动经它推过来
    pub(crate) links: HashMap<String, Arc<Connection>>,
    /// 还没接好就要交的改动（第一次连上时一起交）
    pub(crate) pending: HashMap<String, Vec<fr_core::store::Op>>,
    /// 订阅各台电脑改动的任务；丢弃就不订阅了
    pub(crate) watches: HashMap<String, Task<()>>,
    /// 断开以后自动重连的任务
    pub(crate) reconnects: HashMap<String, Task<()>>,
}

impl AppState {
    pub fn new(settings: Settings, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            settings,
            conns: HashMap::new(),
            stages: HashMap::new(),
            save_later: None,
            mounts: crate::mapping::Mounts::new(),
            machines: HashMap::new(),
            links: HashMap::new(),
            pending: HashMap::new(),
            watches: HashMap::new(),
            reconnects: HashMap::new(),
        };
        this.start_machines(cx);
        this
    }

    pub fn server(&self, id: &str) -> Option<&Server> {
        self.settings.server(id)
    }

    /// 正在连接时显示的那句话：经跳板机的写明走到哪一台了。
    pub fn connecting_text(&self, id: &str) -> SharedString {
        // 经服务器连的：跳板机按那台服务器的配置
        if let Some((via, server)) = split_relay(id) {
            let names: Vec<String> = self
                .machines
                .get(via)
                .and_then(|m| m.store.route(server).ok())
                .map(|(_, route)| route.iter().map(|jump| jump.name.clone()).collect())
                .unwrap_or_default();
            let done = self.stages.get(id).copied().unwrap_or(0);
            return match names.get(done) {
                Some(name) => tf!("正在连接跳板机 {name}…", name).into(),
                None => tf!("正在经由 {name} 连接…", name = self.machine_name(via)).into(),
            };
        }
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

    /// 连这台电脑上的 kuaid（本机仓库的终端开在它里面）；已经连上或正在连就沿用。
    pub fn connect_local(&mut self, cx: &mut Context<Self>) -> Connecting {
        if let Some(existing) = self.conns.get(LOCAL) {
            match existing.peek() {
                None => return existing.clone(),
                Some(Ok(conn)) if conn.is_alive() => return existing.clone(),
                _ => {}
            }
        }
        let connecting = cx
            .background_executor()
            .spawn(async move {
                Connection::connect_local()
                    .map(Arc::new)
                    .map_err(|err| SharedString::from(format!("{err:#}")))
            })
            .shared();
        self.conns.insert(LOCAL.to_string(), connecting.clone());
        let done = connecting.clone();
        cx.spawn(async move |this, cx| {
            let result = done.await;
            match &result {
                Ok(conn) => log::info!(
                    "连接本机的 kuaid：{} ms，{}{}",
                    conn.connect_time.as_millis(),
                    conn.hello.version,
                    if conn.installed { "，已安装" } else { "" },
                ),
                Err(err) => log::warn!("连接本机的 kuaid 失败：{err}"),
            }
            // 本机的仓库列表和工作区经它读写；连不上就直接读写状态文件
            this.update(cx, |this, cx| match result {
                Ok(conn) => this.machine_connected(LOCAL, conn, cx),
                Err(_) => this.local_unavailable(cx),
            })
            .ok();
        })
        .detach();
        connecting
    }

    /// 连接服务器；已经连上或正在连就沿用。返回等连接结果的 future。
    /// `id` 可以是经别的电脑连的服务器（`156的id>91的id`，见 `machines`）：先连那台，再请它的 kuaid 转接。
    pub fn connect(&mut self, id: &str, cx: &mut Context<Self>) -> Connecting {
        self.connect_with(id, false, cx)
    }

    /// 同 [`Self::connect`]。`no_prompt`：要密码、口令、确认主机密钥时不弹窗，存着的答不了这一次就算
    /// 连不上（启动时自动连接用）。已经在连的沿用它原来的连法。
    pub(crate) fn connect_with(
        &mut self,
        id: &str,
        no_prompt: bool,
        cx: &mut Context<Self>,
    ) -> Connecting {
        if let Some(existing) = self.conns.get(id) {
            match existing.peek() {
                None => return existing.clone(),
                Some(Ok(conn)) if conn.is_alive() => return existing.clone(),
                _ => {}
            }
        }
        // 经跳板机的：后台线程每登录上一台报一次，这边记下来、刷新列表上的那句话
        self.stages.remove(id);
        let (stage_tx, mut stage_rx) = futures::channel::mpsc::unbounded::<usize>();
        let on_stage = Arc::new(move |done: usize| drop(stage_tx.unbounded_send(done)))
            as fr_channel::StageHandler;
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
        let connecting = match split_relay(id) {
            Some((via, server)) => {
                let via_connecting = self.connect(via, cx);
                let server = server.to_string();
                cx.background_executor()
                    .spawn(async move {
                        let via = via_connecting.await?;
                        let failed = |err: anyhow::Error| SharedString::from(format!("{err:#}"));
                        // 怎么连照那台电脑的配置：现读一份，这边只拿名字和目标来报进度、报错
                        let store = via.state_get().await.map_err(failed)?;
                        let (target, route) = store.route(&server).map_err(failed)?;
                        let options = ConnectOptions {
                            jumps: route
                                .iter()
                                .map(|jump| Jump {
                                    name: jump.name.clone(),
                                    destination: jump.destination.clone(),
                                    ..Jump::default()
                                })
                                .collect(),
                            on_stage: (!route.is_empty()).then_some(on_stage),
                            ..ConnectOptions::default()
                        };
                        Connection::connect_via(&via, &server, &target.destination, &options)
                            .map(Arc::new)
                            .map_err(failed)
                    })
                    .shared()
            }
            None => {
                let server = self.server(id).cloned();
                let route: Vec<Server> = self.settings.route(id).into_iter().cloned().collect();
                cx.background_executor()
                    .spawn(async move {
                        let server =
                            server.ok_or_else(|| SharedString::from(tr("未找到该服务器的设置")))?;
                        let mut options = connect_options(&server);
                        options.jumps = route.iter().map(jump_options).collect();
                        options.on_stage = (!route.is_empty()).then_some(on_stage);
                        options.no_prompt = no_prompt;
                        Connection::connect(&server.destination, &options)
                            .map(Arc::new)
                            .map_err(|err| SharedString::from(format!("{err:#}")))
                    })
                    .shared()
            }
        };
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
            this.update(cx, |this, cx| {
                // 连上了：这台服务器的映射换到新连接上，设了自动映射的映射上；
                // 订阅它的仓库列表和工作区
                if let Some(Ok(conn)) = this.conns.get(&id).and_then(|c| c.peek()) {
                    let conn = conn.clone();
                    if split_relay(&id).is_none() {
                        this.mappings_connected(&id, &conn, cx);
                    }
                    this.machine_connected(&id, conn, cx);
                }
                cx.notify();
            })
            .ok();
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
        // 新加的服务器没有旧设置里的仓库要并
        self.settings.merged_servers.push(id.clone());
        self.save();
        self.push_servers(cx);
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
        // 上次打开的目录不在表单里：初始目录没改就留着，改了就从新的初始目录开始
        let last_dir = match old.start_dir == server.start_dir {
            true => old.last_dir.take(),
            false => None,
        };
        // 映射不在表单里，留着
        let mappings = std::mem::take(&mut old.mappings);
        *old = server;
        old.last_dir = last_dir;
        old.mappings = mappings;
        if reconnect {
            self.conns.remove(&id);
        }
        self.save();
        self.push_servers(cx);
        cx.notify();
        reconnect
    }

    /// 从列表里去掉服务器；服务器上的东西不动。本机工作区里它上面的仓库跟着去掉，
    /// 用过的它上面的工作区不再列出。
    pub fn remove_server(&mut self, id: &str, cx: &mut Context<Self>) {
        self.unmount_server(id, cx);
        self.settings.servers.retain(|s| s.id != id);
        // 经它连接的服务器改为直接连接（已经连着的不受影响，下次连接时才按新的来）
        for server in &mut self.settings.servers {
            if server.jump.as_deref() == Some(id) {
                server.jump = None;
            }
        }
        self.settings.normalize();
        // 它的连接，和经它连的那几台的
        let relayed = format!("{id}>");
        let gone = |key: &String| key == id || key.starts_with(&relayed);
        self.conns.retain(|key, _| !gone(key));
        self.links.retain(|key, _| !gone(key));
        self.machines.retain(|key, _| !gone(key));
        self.pending.retain(|key, _| !gone(key));
        self.reconnects.retain(|key, _| !gone(key));
        // 存着的密码、口令跟着删掉
        crate::secrets::delete(id);
        self.save();
        self.push_servers(cx);
        // 现在用的就是它上面的工作区：回到本机
        if self.settings.current.machine.is_empty() && self.settings.current.id.is_empty() {
            let next = self.recent_local();
            self.set_current(next, cx);
        }
        cx.notify();
    }

    /// 记下在这台服务器上「打开仓库」时仓库所在的目录，下次从这里开始浏览。
    pub fn set_browse_dir(&mut self, server: &str, dir: String, cx: &mut Context<Self>) {
        let Some(server) = self.settings.servers.iter_mut().find(|s| s.id == server) else {
            return;
        };
        if server.last_dir.as_deref() != Some(dir.as_str()) {
            server.last_dir = Some(dir);
            self.save();
            cx.notify();
        }
    }

    /// 收起、展开仓库页「全部仓库」里的一张卡片（本机是 `settings::LOCAL_CARD`，服务器是它的 id）。
    /// 本机的记收起了的，服务器的（默认收起）记展开了的。
    pub fn toggle_card(&mut self, key: &str, cx: &mut Context<Self>) {
        let cards = match key == crate::settings::LOCAL_CARD {
            true => &mut self.settings.collapsed_cards,
            false => &mut self.settings.expanded_cards,
        };
        match cards.iter().position(|card| card == key) {
            Some(at) => {
                cards.remove(at);
            }
            None => cards.push(key.to_string()),
        }
        self.save();
        cx.notify();
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

    /// 改各块的大小（拖分隔条时一直在变），稍后存盘。
    pub fn edit_layout(&mut self, cx: &mut Context<Self>, edit: impl FnOnce(&mut Layout)) {
        let before = self.settings.layout;
        edit(&mut self.settings.layout);
        if self.settings.layout != before {
            self.save_soon(cx);
            cx.notify();
        }
    }

    /// 改终端页记的东西（固定的终端和仓库、收起的工作区），稍后存盘。
    pub fn edit_term_hub(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut crate::settings::TermHub),
    ) {
        let before = self.settings.term_hub.clone();
        edit(&mut self.settings.term_hub);
        if self.settings.term_hub != before {
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

    pub(crate) fn save(&self) {
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
pub(crate) fn jump_options(server: &Server) -> Jump {
    Jump {
        name: server.name.clone(),
        destination: server.destination.clone(),
        identity_file: server.key_file().map(str::to_string),
        password_auth: server.auth == Auth::Password,
        secret: crate::secrets::get(&server.id),
    }
}
