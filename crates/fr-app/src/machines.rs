//! 各台电脑保存的仓库列表和工作区（协议 10，格式和改法见 `fr_core::store`）。
//!
//! 本机、各台服务器的仓库列表和工作区由它自己的 kuaid 保存。Kuai 连上一台就订阅它的
//! （`state_watch`），哪一边改了，所有连着的电脑都收到整份新的。这里只放这次运行里读到的，
//! 不存到磁盘上（不做缓存，见 `design/server-workspaces/spec.md`）：没连上的服务器不知道它有什么。
//! 本机的例外：启动时先直接读本机的状态文件画出来（那是本机的数据本身），再连本机的 kuaid 收改动；
//! 本机的 kuaid 用不了时直接读写那个文件。
//!
//! 这边的 [`RepoEntry`] 和各台电脑的写法（[`RepoRef`]）怎么对应：
//!
//! - 本机的：`RepoRef { server: None }` 是本机的仓库，`Some(id)` 是本机配置的服务器上的，
//!   和 `RepoEntry` 一样。
//! - 服务器 `s156` 的：`RepoRef { server: None }` 是 156 自己的仓库，这边是 `server: "s156"`；
//!   `Some("s91")` 是 156 配置的服务器上的，这边是 `server: "s156>s91"`，经 156 的 kuaid 连
//!   （见 `fr_channel::Connection::connect_via`）。服务器的 id 是 `s` 加毫秒数，不会带 `>`。
//! - Windows 服务器自己的仓库：它的文件里是反斜杠（和它上面的 Kuai 自己加的一样），这边和别的
//!   服务器上的仓库一样用正斜杠（kuaid 给出来的路径也是这样）。

use std::sync::Arc;
use std::time::Duration;

use fr_channel::Connection;
use fr_core::i18n::tr;
use fr_core::store::{Op, RepoRef, Store, Workspace};
use fr_core::tf;
use futures::StreamExt as _;
use gpui_kit::*;
use serde::{Deserialize, Serialize};

use crate::app_state::{AppState, ConnStatus};
use crate::mapping::Notice;
use crate::settings::{Auth, RepoEntry, Settings, View};

/// 本机在各处当作「哪台电脑」的键：本机是空串，服务器是它的 id。
pub const LOCAL: &str = "";

/// 一个工作区：在哪台电脑上（本机为空，服务器是它在本机的 id），和它在那台电脑上的 id。
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceKey {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub machine: String,
    pub id: String,
}

impl WorkspaceKey {
    pub fn new(machine: &str, id: &str) -> WorkspaceKey {
        WorkspaceKey {
            machine: machine.to_string(),
            id: id.to_string(),
        }
    }

    pub fn is_local(&self) -> bool {
        self.machine == LOCAL
    }
}

/// 一台电脑保存的东西，这次运行里读到的。
pub struct Machine {
    pub store: Store,
    /// 在收这台电脑的改动。断开了就是假：留着最后读到的，只读
    pub live: bool,
    /// 那台电脑是 Windows（它自己的仓库路径在文件里是反斜杠）
    pub windows: bool,
}

impl Machine {
    pub fn new(store: Store, windows: bool) -> Machine {
        Machine {
            store,
            live: true,
            windows,
        }
    }
}

/// 仓库在哪台电脑的列表里：本机为空串，服务器是 `server`（经别的电脑连的是 `那台>它`）。
pub fn machine_of(entry: &RepoEntry) -> &str {
    entry.server.as_deref().unwrap_or(LOCAL)
}

/// 经别的电脑连的服务器：拆成那台电脑和它配置的服务器的 id。直接连的是 None。
pub fn split_relay(key: &str) -> Option<(&str, &str)> {
    key.split_once('>')
}

/// 经 `via` 连它配置的服务器 `server` 时用的键。
pub fn relay_key(via: &str, server: &str) -> String {
    format!("{via}>{server}")
}

/// 这边显示、比较用的路径：Windows 服务器自己的仓库换成正斜杠。本机的不换。
fn shown(key: &str, windows: bool, path: &str) -> String {
    match windows && key != LOCAL {
        true => path.replace('\\', "/"),
        false => path.to_string(),
    }
}

/// 电脑 `key` 列表里的一个仓库（它自己的）。
pub fn own_entry(key: &str, windows: bool, path: &str) -> RepoEntry {
    RepoEntry {
        server: (key != LOCAL).then(|| key.to_string()),
        path: shown(key, windows, path),
    }
}

/// 电脑 `machine` 上的工作区里的一个仓库。
pub fn member_entry(machine: &str, windows: bool, repo: &RepoRef) -> RepoEntry {
    match (&repo.server, machine == LOCAL) {
        (None, _) => own_entry(machine, windows, &repo.path),
        (Some(server), true) => RepoEntry {
            server: Some(server.clone()),
            path: repo.path.clone(),
        },
        (Some(server), false) => RepoEntry {
            server: Some(relay_key(machine, server)),
            path: repo.path.clone(),
        },
    }
}

/// 反过来：这个仓库放进电脑 `machine` 上的工作区时怎么写。那台电脑访问不到的（比如本机的仓库
/// 放进 156 的工作区）是 None。
pub fn member_ref(machine: &str, entry: &RepoEntry) -> Option<RepoRef> {
    let server = entry.server.as_deref();
    if machine == LOCAL {
        return match server {
            None => Some(RepoRef::local(entry.path.clone())),
            Some(id) if split_relay(id).is_none() => Some(RepoRef {
                server: Some(id.to_string()),
                path: entry.path.clone(),
            }),
            Some(_) => None,
        };
    }
    match server.and_then(|id| match id == machine {
        true => Some(None),
        false => split_relay(id)
            .filter(|(via, _)| *via == machine)
            .map(|(_, server)| Some(server.to_string())),
    }) {
        Some(server) => Some(RepoRef {
            server,
            path: entry.path.clone(),
        }),
        None => None,
    }
}

/// 名字去掉首尾空白；空的话叫「工作区」；和已有的重了就加上 2、3……
pub fn unique_name<'a>(name: &str, existing: impl Iterator<Item = &'a str>) -> String {
    let base = match name.trim() {
        "" => tr("工作区"),
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

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// 旧设置里的本机仓库、工作区（和工作区里服务器上的仓库）、本机仓库起的名字，交给本机时的改动；
/// 各个工作区开着的标签页挪到新的记法里，现在用的工作区跟着换过去。`local` 是本机现在的状态，
/// 新工作区的 id 不和它里面的重。
pub(crate) fn migration_ops(settings: &mut Settings, local: &Store) -> Vec<Op> {
    let mut ops: Vec<Op> = settings
        .repos
        .iter()
        .filter(|repo| repo.server.is_none())
        .map(|repo| Op::AddRepo {
            path: repo.path.clone(),
        })
        .collect();
    let mut taken = local.clone();
    let groups = settings.workspaces.clone();
    let active = settings.active_workspace;
    let mut current = None;
    for (ix, group) in groups.iter().enumerate() {
        let id = new_workspace_id(&taken);
        taken.workspaces.push(Workspace {
            id: id.clone(),
            ..Workspace::default()
        });
        ops.push(Op::AddWorkspace {
            id: id.clone(),
            name: group.name.clone(),
        });
        for entry in group.repos.iter().chain(&group.tabs) {
            if let Some(repo) = member_ref(LOCAL, entry) {
                ops.push(Op::AddToWorkspace {
                    id: id.clone(),
                    repo,
                });
            }
        }
        let key = WorkspaceKey::new(LOCAL, &id);
        let view = settings.view_mut(&key);
        view.name = group.name.clone();
        view.tabs = group.tabs.clone();
        view.active = group.active;
        view.closed = group.closed.clone();
        view.pinned = group.pinned.clone();
        if ix == active {
            view.used_at = now_millis();
            current = Some(key);
        }
    }
    for memo in &settings.repo_memos {
        if let (None, Some(name)) = (&memo.repo.server, &memo.name) {
            ops.push(Op::RenameRepo {
                path: memo.repo.path.clone(),
                name: Some(name.clone()),
            });
        }
    }
    if let Some(current) = current {
        settings.current = current;
    }
    ops
}

/// 旧设置里服务器 `key` 上的仓库和它们起的名字，第一次连上时并进服务器的列表。并过了就是空的。
pub(crate) fn merge_ops(settings: &Settings, key: &str) -> Vec<Op> {
    if key == LOCAL
        || split_relay(key).is_some()
        || settings.merged_servers.iter().any(|id| id == key)
    {
        return Vec::new();
    }
    let mut ops: Vec<Op> = settings
        .repos
        .iter()
        .filter(|repo| repo.server.as_deref() == Some(key))
        .map(|repo| Op::AddRepo {
            path: repo.path.clone(),
        })
        .collect();
    for memo in &settings.repo_memos {
        if let (Some(server), Some(name)) = (&memo.repo.server, &memo.name)
            && server == key
        {
            ops.push(Op::RenameRepo {
                path: memo.repo.path.clone(),
                name: Some(name.clone()),
            });
        }
    }
    ops
}

impl AppState {
    // ---- 启动 ----

    /// 启动时调用一次：读本机的状态文件先画出来；第一次用 1.14 时把旧设置里的仓库列表和工作区交给
    /// 本机；本机配置的服务器也交过去（别的电脑连上这台以后经它连）。然后连本机的 kuaid 收改动。
    pub(crate) fn start_machines(&mut self, cx: &mut Context<Self>) {
        let store = fr_core::store::load(&fr_core::store::path());
        self.machines
            .insert(LOCAL.to_string(), Machine::new(store, cfg!(windows)));
        let mut ops = Vec::new();
        if !self.settings.store_migrated {
            ops = migration_ops(&mut self.settings, &self.machines[LOCAL].store);
            self.settings.store_migrated = true;
        }
        ops.extend(self.servers_op());
        // 每台电脑至少有一个工作区
        let mut preview = self.machines[LOCAL].store.clone();
        let _ = preview.apply(&ops, cfg!(windows));
        if preview.workspaces.is_empty() {
            let id = new_workspace_id(&preview);
            ops.push(Op::AddWorkspace {
                id,
                name: tr("默认").to_string(),
            });
        }
        self.apply(LOCAL, ops, cx);
        self.resolve_current(cx);
        self.save();
        drop(self.connect_local(cx));
        self.auto_connect(cx);
    }

    /// 启动时连上设了「启动时自动连接」的服务器（默认都设着），它们的仓库、工作区、终端不用等人去点
    /// 「连接」才有。不弹窗：存着密码、用得了密钥的连得上；要问密码的这一次算连不上，卡片上有「重试」。
    /// 用密码登录又没存密码的肯定要问，不去连。现在用的工作区所在的那台不在这里连：它本来就会连，
    /// 要密码时照常弹窗。
    fn auto_connect(&mut self, cx: &mut Context<Self>) {
        let current = self.settings.current.machine.clone();
        let ids: Vec<String> = self
            .settings
            .servers
            .iter()
            .filter(|server| !server.manual_connect && server.id != current)
            .filter(|server| {
                server.auth != Auth::Password || crate::secrets::get(&server.id).is_some()
            })
            .map(|server| server.id.clone())
            .collect();
        for id in ids {
            log::info!("启动时自动连接服务器 {id}");
            drop(self.connect_with(&id, true, cx));
        }
    }

    /// 本机配置的服务器交给本机时的样子（不含密码）；和本机记着的一样就不用交。
    fn servers_op(&self) -> Option<Op> {
        let servers: Vec<fr_core::store::Server> = self
            .settings
            .servers
            .iter()
            .map(|server| fr_core::store::Server {
                id: server.id.clone(),
                name: server.name.clone(),
                destination: server.destination.clone(),
                forward_agent: server.forward_agent,
                password_auth: server.auth == Auth::Password,
                identity_file: server.key_file().map(str::to_string),
                jump: server.jump.clone(),
            })
            .collect();
        let same = self
            .machines
            .get(LOCAL)
            .is_some_and(|local| local.store.servers == servers);
        (!same).then_some(Op::SetServers { servers })
    }

    /// 服务器加了、改了、删了：交给本机。
    pub(crate) fn push_servers(&mut self, cx: &mut Context<Self>) {
        if let Some(op) = self.servers_op() {
            self.apply(LOCAL, vec![op], cx);
        }
    }

    // ---- 改动和订阅 ----

    /// 改电脑 `machine` 保存的东西：先改这边读到的那份（界面马上跟着变），再交给那台电脑；还没连上
    /// 就等连上了再交。断开着改不了：说一声，返回假。
    pub(crate) fn apply(&mut self, machine: &str, ops: Vec<Op>, cx: &mut Context<Self>) -> bool {
        if ops.is_empty() {
            return true;
        }
        let name = self.machine_name(machine);
        if let Some(loaded) = self.machines.get_mut(machine) {
            if !loaded.live {
                let text = tf!("与 {name} 的连接已断开，暂时无法修改。", name);
                cx.emit(Notice::Warning(text.into()));
                return false;
            }
            if let Err(err) = loaded.store.apply(&ops, loaded.windows) {
                cx.emit(Notice::Error(format!("{err:#}").into()));
                return false;
            }
            cx.notify();
        }
        match self
            .links
            .get(machine)
            .filter(|conn| conn.is_alive())
            .cloned()
        {
            Some(conn) => self.send(machine, conn, ops, cx),
            // 本机的 kuaid 用不了：直接改状态文件
            None if machine == LOCAL && matches!(self.status(LOCAL), ConnStatus::Failed(_)) => {
                self.write_local_file(ops, cx)
            }
            None => {
                self.pending
                    .entry(machine.to_string())
                    .or_default()
                    .extend(ops);
                match machine == LOCAL {
                    true => drop(self.connect_local(cx)),
                    false => drop(self.connect(machine, cx)),
                }
            }
        }
        true
    }

    /// 交给那台电脑；那边做不了（比如工作区刚被别的电脑删了）就说一声，重新读一份。
    fn send(&mut self, machine: &str, conn: Arc<Connection>, ops: Vec<Op>, cx: &mut Context<Self>) {
        let key = machine.to_string();
        cx.spawn(async move |this, cx| {
            let result = conn.state_apply(ops).await;
            this.update(cx, |this, cx| match result {
                Ok(store) => this.store_arrived(&key, store, cx),
                Err(err) => {
                    let text = tf!("保存失败：{err}", err = format!("{err:#}"));
                    cx.emit(Notice::Error(text.into()));
                    this.reload(&key, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// 重新读一份那台电脑现在的。
    fn reload(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(conn) = self.links.get(key).cloned() else {
            return;
        };
        let key = key.to_string();
        cx.spawn(async move |this, cx| {
            if let Ok(store) = conn.state_get().await {
                this.update(cx, |this, cx| this.store_arrived(&key, store, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// 本机的 kuaid 用不了时：直接读写状态文件。
    fn write_local_file(&mut self, ops: Vec<Op>, cx: &mut Context<Self>) {
        let path = fr_core::store::path();
        let mut store = fr_core::store::load(&path);
        let saved = store
            .apply(&ops, cfg!(windows))
            .and_then(|_| fr_core::store::save(&path, &store));
        match saved {
            Ok(()) => self.store_arrived(LOCAL, store, cx),
            Err(err) => cx.emit(Notice::Error(format!("{err:#}").into())),
        }
    }

    /// 本机的 kuaid 连不上：攒着的改动直接写进状态文件。
    pub(crate) fn local_unavailable(&mut self, cx: &mut Context<Self>) {
        if let Some(ops) = self.pending.remove(LOCAL) {
            self.write_local_file(ops, cx);
        }
    }

    /// 和一台电脑接上了：先交攒着的改动（和旧设置里它上面的仓库），再订阅它的改动。
    /// 订阅的第一份就是改完的。
    pub(crate) fn machine_connected(
        &mut self,
        key: &str,
        conn: Arc<Connection>,
        cx: &mut Context<Self>,
    ) {
        // 太旧的 kuaid 不会有（连接时就升级了），保险起见
        if conn.hello.protocol < 10 {
            return;
        }
        // 同一个连接被几处等着，会来几次
        if self
            .links
            .get(key)
            .is_some_and(|old| Arc::ptr_eq(old, &conn))
        {
            return;
        }
        self.links.insert(key.to_string(), conn.clone());
        self.reconnects.remove(key);
        let windows = conn.hello.os == "windows";
        let merge = merge_ops(&self.settings, key);
        let merging = !merge.is_empty();
        let mut ops = merge;
        ops.extend(self.pending.remove(key).unwrap_or_default());
        if !merging && split_relay(key).is_none() && key != LOCAL {
            self.note_merged(key);
        }
        let watch_key = key.to_string();
        let key = key.to_string();
        let task = cx.spawn(async move |this, cx| {
            if !ops.is_empty() {
                let applied = conn.state_apply(ops).await;
                this.update(cx, |this, cx| match applied {
                    Ok(_) if merging => this.note_merged(&key),
                    Ok(_) => {}
                    Err(err) => {
                        let text = tf!("保存失败：{err}", err = format!("{err:#}"));
                        cx.emit(Notice::Error(text.into()));
                    }
                })
                .ok();
            }
            let mut updates = conn.state_watch();
            while let Some(update) = updates.next().await {
                let Ok(store) = update else {
                    break;
                };
                let arrived = this.update(cx, |this, cx| {
                    this.machines
                        .entry(key.clone())
                        .or_insert_with(|| Machine::new(Store::default(), windows))
                        .windows = windows;
                    this.store_arrived(&key, store, cx)
                });
                if arrived.is_err() {
                    return;
                }
            }
            this.update(cx, |this, cx| this.machine_dropped(&key, &conn, cx))
                .ok();
        });
        self.watches.insert(watch_key, task);
    }

    fn note_merged(&mut self, key: &str) {
        if !self.settings.merged_servers.iter().any(|id| id == key) {
            self.settings.merged_servers.push(key.to_string());
            self.save();
        }
    }

    /// 收到一份那台电脑现在的。
    fn store_arrived(&mut self, key: &str, store: Store, cx: &mut Context<Self>) {
        let windows = self
            .links
            .get(key)
            .map_or(key == LOCAL && cfg!(windows), |conn| {
                conn.hello.os == "windows"
            });
        let machine = self
            .machines
            .entry(key.to_string())
            .or_insert_with(|| Machine::new(Store::default(), windows));
        machine.store = store;
        machine.live = true;
        if key == LOCAL && machine.store.workspaces.is_empty() {
            // 每台电脑至少有一个工作区（别的电脑删光了也补一个）
            let id = new_workspace_id(&machine.store);
            let name = tr("默认").to_string();
            self.apply(LOCAL, vec![Op::AddWorkspace { id, name }], cx);
        }
        if self.settings.current.machine == key {
            self.resolve_current(cx);
            // 记下工作区现在的名字：下次启动连上之前，标题栏先用它
            let current = self.settings.current.clone();
            if let Some(name) = self.workspace(&current).map(|w| w.name.clone()) {
                let view = self.settings.view_mut(&current);
                if view.name != name {
                    view.name = name;
                    self.save();
                }
            }
        }
        cx.notify();
    }

    /// 和一台电脑断开了：读到的留着、只读；现在用的就是它上面的工作区的话，过一会儿自己重连。
    /// 本机的照样能改（直接读写状态文件），也过一会儿重连。
    fn machine_dropped(&mut self, key: &str, conn: &Arc<Connection>, cx: &mut Context<Self>) {
        // 已经换上了新的连接（重连过了）：旧的断了不算
        if !self
            .links
            .get(key)
            .is_some_and(|link| Arc::ptr_eq(link, conn))
        {
            return;
        }
        self.links.remove(key);
        self.watches.remove(key);
        if key != LOCAL
            && let Some(machine) = self.machines.get_mut(key)
        {
            machine.live = false;
        }
        if key == LOCAL || self.settings.current.machine == key {
            self.schedule_reconnect(key, cx);
        }
        cx.notify();
    }

    /// 过一会儿重连，连不上隔得越来越久（最多 30 秒）。要密码的不弹窗：存着密码的照样连得上，
    /// 没存的等用户点「重新连接」。不用了（换到别的工作区、已经连上了）就不连了。
    fn schedule_reconnect(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.reconnects.contains_key(key) {
            return;
        }
        let key = key.to_string();
        let task = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let mut wait = 3;
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(wait))
                        .await;
                    let connecting = this.update(cx, |this, cx| {
                        let wanted = key == LOCAL || this.settings.current.machine == key;
                        let linked = this.links.get(&key).is_some_and(|link| link.is_alive());
                        (wanted && !linked).then(|| match key == LOCAL {
                            true => this.connect_local(cx),
                            false => this.connect(&key, cx),
                        })
                    });
                    let Ok(Some(connecting)) = connecting else {
                        break;
                    };
                    let _quiet = crate::prompt::quiet();
                    if connecting.await.is_ok() {
                        break;
                    }
                    wait = (wait * 2).min(30);
                }
            }
        });
        self.reconnects.insert(key, task);
    }

    // ---- 读 ----

    pub fn machine(&self, key: &str) -> Option<&Machine> {
        self.machines.get(key)
    }

    /// 电脑 `key` 列表里的仓库（这边的写法）。没读到过（没连上）是 None。
    pub fn repos_on(&self, key: &str) -> Option<Vec<RepoEntry>> {
        let machine = self.machines.get(key)?;
        Some(
            machine
                .store
                .repos
                .iter()
                .map(|path| own_entry(key, machine.windows, path))
                .collect(),
        )
    }

    /// 能不能改电脑 `key` 保存的东西：本机总能，服务器要连着。
    pub fn writable(&self, key: &str) -> bool {
        key == LOCAL || self.machines.get(key).is_some_and(|machine| machine.live)
    }

    /// 电脑的名字：本机、服务器名；经服务器连的是它在那台服务器上的名字。
    pub fn machine_name(&self, key: &str) -> SharedString {
        if key == LOCAL {
            return tr("本机").into();
        }
        match split_relay(key) {
            Some((via, server)) => self
                .machines
                .get(via)
                .and_then(|machine| machine.store.server(server))
                .map_or_else(|| "?".into(), |server| server.name.clone().into()),
            None => self
                .server(key)
                .map_or_else(|| "?".into(), |server| server.name.clone().into()),
        }
    }

    /// 是本机、本机配置的服务器，或者经它们连的服务器（要那台的配置里还有它）。
    pub fn knows_server(&self, key: &str) -> bool {
        match split_relay(key) {
            Some((via, server)) => self
                .machines
                .get(via)
                .is_some_and(|machine| machine.store.server(server).is_some()),
            None => key == LOCAL || self.server(key).is_some(),
        }
    }

    /// 经服务器连的服务器在那台服务器上的配置。
    pub fn relayed_server(&self, key: &str) -> Option<&fr_core::store::Server> {
        let (via, server) = split_relay(key)?;
        self.machines.get(via)?.store.server(server)
    }

    /// 仓库显示的名字：它所在的电脑上给它起的名字；没起、或者那台还没连上，是目录名。
    pub fn repo_name(&self, entry: &RepoEntry) -> SharedString {
        self.machines
            .get(machine_of(entry))
            .and_then(|machine| machine.store.name_of(&entry.path, machine.windows))
            .map_or_else(
                || entry.name().to_string().into(),
                |name| name.to_string().into(),
            )
    }

    /// 标签页上显示的名字：仓库的名字，服务器上的再加服务器名。
    pub fn repo_title(&self, entry: &RepoEntry) -> SharedString {
        let name = self.repo_name(entry);
        match &entry.server {
            None => name,
            Some(id) => format!("{name} · {}", self.machine_name(id)).into(),
        }
    }

    /// 悬停提示里的名字：经服务器连的写明经由哪台。
    pub fn repo_tip(&self, entry: &RepoEntry) -> SharedString {
        let title = self.repo_title(entry);
        match entry.server.as_deref().and_then(split_relay) {
            Some((via, _)) => {
                tf!("{title}（经由 {via}）", title, via = self.machine_name(via)).into()
            }
            None => title,
        }
    }

    pub fn current(&self) -> &WorkspaceKey {
        &self.settings.current
    }

    /// 工作区（它所在的电脑上读到的）；没读到是 None。
    pub fn workspace(&self, key: &WorkspaceKey) -> Option<&Workspace> {
        self.machines
            .get(&key.machine)?
            .store
            .workspaces
            .iter()
            .find(|workspace| workspace.id == key.id)
    }

    /// 工作区的名字：读到了用读到的，没读到用上次记下的。
    pub fn workspace_name(&self, key: &WorkspaceKey) -> SharedString {
        match self.workspace(key) {
            Some(workspace) => workspace.name.clone().into(),
            None => self
                .settings
                .view(key)
                .map(|view| view.name.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| tr("工作区").to_string())
                .into(),
        }
    }

    /// 工作区里的仓库（这边的写法），按加进来的先后；没读到是空的。
    pub fn members(&self, key: &WorkspaceKey) -> Vec<RepoEntry> {
        let Some(machine) = self.machines.get(&key.machine) else {
            return Vec::new();
        };
        self.workspace(key)
            .map(|workspace| {
                workspace
                    .repos
                    .iter()
                    .map(|repo| member_entry(&key.machine, machine.windows, repo))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 当前工作区在这台电脑上记的标签页。
    pub fn view(&self) -> View {
        let key = &self.settings.current;
        self.settings
            .view(key)
            .cloned()
            .unwrap_or_else(|| View::new(key.clone()))
    }

    /// 一台电脑上的全部工作区：键、名字、有几个仓库。没读到是 None。
    pub fn workspaces_on(&self, machine: &str) -> Option<Vec<(WorkspaceKey, String, usize)>> {
        let loaded = self.machines.get(machine)?;
        Some(
            loaded
                .store
                .workspaces
                .iter()
                .map(|w| {
                    (
                        WorkspaceKey::new(machine, &w.id),
                        w.name.clone(),
                        w.repos.len(),
                    )
                })
                .collect(),
        )
    }

    /// 站在电脑 `machine` 上看得到的仓库：它自己的，和它配置的服务器上的（读到了的才有）。
    /// 本机的工作区能放的就是这些（「仓库」页、快速打开里列的）。
    pub fn perspective_repos(&self, machine: &str) -> Vec<RepoEntry> {
        let mut repos = self.repos_on(machine).unwrap_or_default();
        let servers: Vec<String> = match machine == LOCAL {
            true => self.settings.servers.iter().map(|s| s.id.clone()).collect(),
            false => self
                .machines
                .get(machine)
                .map(|loaded| {
                    loaded
                        .store
                        .servers
                        .iter()
                        .map(|s| relay_key(machine, &s.id))
                        .collect()
                })
                .unwrap_or_default(),
        };
        for key in servers {
            repos.extend(self.repos_on(&key).unwrap_or_default());
        }
        repos
    }

    /// 工作区菜单里列哪几台服务器（本机另列），按「仓库」页上卡片的顺序：读到了而且上面有工作区的
    /// （连着的、这次连过的），用过上面的工作区的（这次还没连上的在菜单里只有「连接」），和现在用的
    /// 工作区所在的那台。
    ///
    /// 1.16 以前只列用过的：连上一台新的服务器，它的工作区不进菜单，得先到「仓库」页的卡片上点
    /// 一次（2026-11-19 Feng 当成 bug 报的）。
    pub fn menu_machines(&self) -> Vec<String> {
        let current = &self.settings.current.machine;
        let has_workspaces = |id: &String| {
            self.machines
                .get(id)
                .is_some_and(|machine| !machine.store.workspaces.is_empty())
        };
        let mut list: Vec<String> = self
            .settings
            .servers
            .iter()
            .map(|server| server.id.clone())
            .filter(|id| {
                has_workspaces(id) || self.settings.used_machines.contains(id) || id == current
            })
            .collect();
        if current != LOCAL && !list.contains(current) {
            list.push(current.clone());
        }
        list
    }

    // ---- 工作区 ----

    /// 换到一个工作区。
    pub fn set_current(&mut self, key: WorkspaceKey, cx: &mut Context<Self>) {
        let name = self.workspace(&key).map(|w| w.name.clone());
        let view = self.settings.view_mut(&key);
        if let Some(name) = name {
            view.name = name;
        }
        view.used_at = now_millis();
        if !key.is_local() && !self.settings.used_machines.contains(&key.machine) {
            self.settings.used_machines.push(key.machine.clone());
        }
        self.settings.current = key;
        self.save();
        cx.notify();
    }

    /// 现在用的工作区不在了（别的电脑删了、还没定）：换一个。那台电脑还连着就换成它上面的第一个，
    /// 不然回到本机最近用的那个。服务器还没连上的先不动，连上了再看。
    pub(crate) fn resolve_current(&mut self, cx: &mut Context<Self>) {
        let key = self.settings.current.clone();
        let next = match self.machines.get(&key.machine) {
            Some(machine) if machine.store.workspaces.iter().any(|w| w.id == key.id) => return,
            Some(machine) if machine.live && !key.is_local() => machine
                .store
                .workspaces
                .first()
                .map(|w| WorkspaceKey::new(&key.machine, &w.id)),
            Some(_) => None,
            None if !key.is_local() => return,
            None => None,
        };
        let next = next.unwrap_or_else(|| self.recent_local());
        if next != key && !next.id.is_empty() {
            self.set_current(next, cx);
        }
    }

    /// 本机最近用的工作区；都没用过就是第一个。
    pub(crate) fn recent_local(&self) -> WorkspaceKey {
        let Some(local) = self.machines.get(LOCAL) else {
            return WorkspaceKey::default();
        };
        let exists = |id: &str| local.store.workspaces.iter().any(|w| w.id == id);
        self.settings
            .views
            .iter()
            .filter(|view| view.key.is_local() && exists(&view.key.id))
            .max_by_key(|view| view.used_at)
            .map(|view| view.key.clone())
            .or_else(|| {
                local
                    .store
                    .workspaces
                    .first()
                    .map(|w| WorkspaceKey::new(LOCAL, &w.id))
            })
            .unwrap_or_default()
    }

    /// 在电脑 `machine` 上新建一个空的工作区，返回它；名字和那台上已有的重了就在后面加数字。
    /// 那台这次还没连上：先记着，连上了再交（名字这时查不了重）。断开着建不了，是 None。
    pub fn add_workspace(
        &mut self,
        machine: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Option<WorkspaceKey> {
        let (name, id) = {
            let empty = Store::default();
            let store = self
                .machines
                .get(machine)
                .map_or(&empty, |loaded| &loaded.store);
            let names = store.workspaces.iter().map(|w| w.name.as_str());
            (unique_name(name, names), new_workspace_id(store))
        };
        let op = Op::AddWorkspace {
            id: id.clone(),
            name: name.clone(),
        };
        if !self.apply(machine, vec![op], cx) {
            return None;
        }
        // 那台连上之前，标题栏和菜单先用这个名字
        let key = WorkspaceKey::new(machine, &id);
        self.settings.view_mut(&key).name = name;
        Some(key)
    }

    pub fn rename_workspace(&mut self, key: &WorkspaceKey, name: &str, cx: &mut Context<Self>) {
        let Some(loaded) = self.machines.get(&key.machine) else {
            return;
        };
        let others = loaded
            .store
            .workspaces
            .iter()
            .filter(|w| w.id != key.id)
            .map(|w| w.name.as_str());
        let name = unique_name(name, others);
        let id = key.id.clone();
        self.apply(&key.machine, vec![Op::RenameWorkspace { id, name }], cx);
    }

    /// 那台电脑上有几个工作区（没读到是 0）。
    pub fn workspace_count(&self, machine: &str) -> usize {
        self.machines
            .get(machine)
            .map_or(0, |loaded| loaded.store.workspaces.len())
    }

    /// 删掉一个工作区（里面的仓库还在列表里）。每台电脑至少留一个，调用方先看过。
    pub fn remove_workspace(&mut self, key: &WorkspaceKey, cx: &mut Context<Self>) -> bool {
        if self.workspace_count(&key.machine) <= 1 {
            return false;
        }
        let id = key.id.clone();
        let removed = self.apply(&key.machine, vec![Op::RemoveWorkspace { id }], cx);
        if removed {
            self.settings.views.retain(|view| view.key != *key);
            self.save();
        }
        removed
    }

    // ---- 仓库 ----

    /// 仓库进它所在那台电脑的列表（已经在了不动）。那台还没连上就等连上了再加。
    pub fn add_repo(&mut self, entry: RepoEntry, cx: &mut Context<Self>) {
        let machine = machine_of(&entry).to_string();
        let listed = self
            .machines
            .get(&machine)
            .is_some_and(|loaded| loaded.store.has_repo(&entry.path, loaded.windows));
        if !listed && (self.writable(&machine) || !self.machines.contains_key(&machine)) {
            self.apply(&machine, vec![Op::AddRepo { path: entry.path }], cx);
        }
    }

    /// 在当前工作区里打开了一个仓库：它成了这个工作区的仓库，也进它所在那台电脑的列表。
    pub fn adopt(&mut self, entry: &RepoEntry, cx: &mut Context<Self>) {
        let key = self.settings.current.clone();
        if let Some(repo) = member_ref(&key.machine, entry)
            && self.writable(&key.machine)
            && !self.members(&key).contains(entry)
        {
            let id = key.id.clone();
            self.apply(&key.machine, vec![Op::AddToWorkspace { id, repo }], cx);
        }
        self.add_repo(entry.clone(), cx);
    }

    /// 从它所在那台电脑的列表里去掉仓库（仓库本身不动）；各台电脑的工作区里有它的也去掉。
    pub fn remove_repo(&mut self, entry: &RepoEntry, cx: &mut Context<Self>) {
        let machine = machine_of(entry).to_string();
        let path = entry.path.clone();
        if !self.apply(&machine, vec![Op::RemoveRepo { path }], cx) {
            return;
        }
        // 别的电脑上的工作区里放着它的（比如本机的工作区里有 91 上的这个仓库）
        let mut elsewhere: Vec<(String, Vec<Op>)> = Vec::new();
        for (key, loaded) in &self.machines {
            if *key == machine || !loaded.live {
                continue;
            }
            let Some(repo) = member_ref(key, entry) else {
                continue;
            };
            let ops: Vec<Op> = loaded
                .store
                .workspaces
                .iter()
                .filter(|w| w.repos.contains(&repo))
                .map(|w| Op::RemoveFromWorkspace {
                    id: w.id.clone(),
                    repo: repo.clone(),
                })
                .collect();
            if !ops.is_empty() {
                elsewhere.push((key.clone(), ops));
            }
        }
        for (key, ops) in elsewhere {
            self.apply(&key, ops, cx);
        }
        self.settings.forget_repo(entry);
        self.save();
    }

    /// 给仓库起名字（只改显示，目录不动），存在它所在的电脑上；空的、和目录名一样就是不要了。
    pub fn rename_repo(&mut self, entry: &RepoEntry, name: &str, cx: &mut Context<Self>) {
        let name = name.trim();
        let name = (!name.is_empty() && name != entry.name()).then(|| name.to_string());
        let path = entry.path.clone();
        self.apply(machine_of(entry), vec![Op::RenameRepo { path, name }], cx);
    }

    /// 把仓库从当前工作区去掉（它还在列表里，别的工作区里的也不动）。
    pub fn remove_from_workspace(&mut self, entry: &RepoEntry, cx: &mut Context<Self>) -> bool {
        let key = self.settings.current.clone();
        let Some(repo) = member_ref(&key.machine, entry) else {
            return false;
        };
        let id = key.id.clone();
        let removed = self.apply(&key.machine, vec![Op::RemoveFromWorkspace { id, repo }], cx);
        if removed {
            self.settings.view_mut(&key).forget(entry);
            self.save();
        }
        removed
    }

    /// 把仓库放进同一台电脑上的另一个工作区，在那边开着（标签页移过去时用）。
    pub fn push_tab_to_workspace(
        &mut self,
        to: &WorkspaceKey,
        entry: RepoEntry,
        cx: &mut Context<Self>,
    ) {
        if let Some(repo) = member_ref(&to.machine, &entry) {
            let id = to.id.clone();
            self.apply(&to.machine, vec![Op::AddToWorkspace { id, repo }], cx);
        }
        let view = self.settings.view_mut(to);
        view.closed.retain(|e| *e != entry);
        if !view.tabs.contains(&entry) {
            view.tabs.push(entry);
        }
        self.save();
        cx.notify();
    }

    // ---- 当前工作区的标签页（只记在这台电脑上） ----

    /// 记下当前工作区开着的标签页和选中的那一个。
    pub fn set_open_tabs(&mut self, tabs: Vec<RepoEntry>, active: usize) {
        let key = self.settings.current.clone();
        let view = self.settings.view_mut(&key);
        if view.tabs != tabs || view.active != active {
            // 重新打开了的不算「关掉的」了
            view.closed.retain(|e| !tabs.contains(e));
            view.pinned.retain(|e| tabs.contains(e));
            view.tabs = tabs;
            view.active = active;
            self.save();
        }
    }

    /// 固定、取消固定当前工作区里的一个标签页；返回现在是不是固定的。
    pub fn toggle_pinned(&mut self, entry: &RepoEntry, cx: &mut Context<Self>) -> bool {
        let key = self.settings.current.clone();
        let view = self.settings.view_mut(&key);
        let pinned = !view.is_pinned(entry);
        view.pinned.retain(|e| e != entry);
        if pinned {
            view.pinned.push(entry.clone());
        }
        view.pinned_first();
        self.save();
        cx.notify();
        pinned
    }

    /// 记下当前工作区刚关掉的一个标签页（Ctrl+Shift+T 和快速打开里能找回来）。
    pub fn note_closed_tab(&mut self, entry: RepoEntry) {
        let key = self.settings.current.clone();
        self.settings.view_mut(&key).note_closed(entry);
    }

    /// 当前工作区最近关掉的标签页，最近的在前。已经移出工作区的不算（读到了才看得出）。
    pub fn closed_tabs(&self) -> Vec<RepoEntry> {
        let key = &self.settings.current;
        let members = self
            .machines
            .contains_key(&key.machine)
            .then(|| self.members(key));
        self.view()
            .closed
            .iter()
            .rev()
            .filter(|e| members.as_ref().is_none_or(|members| members.contains(e)))
            .cloned()
            .collect()
    }
}

/// 新工作区的 id：`w` 加毫秒数，同一毫秒里重了就往后加。
pub fn new_workspace_id(store: &Store) -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    (millis..)
        .map(|n| format!("w{n}"))
        .find(|id| !store.workspaces.iter().any(|w| w.id == *id))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：gpui_kit 里也有个叫 test 的属性宏，会盖住标准库的 #[test]
    use super::{
        LOCAL, WorkspaceKey, machine_of, member_entry, member_ref, merge_ops, migration_ops,
        new_workspace_id, split_relay, unique_name,
    };
    use crate::settings::{RepoEntry, Settings};
    use fr_core::store::{Op, RepoRef, Store};

    fn entry(server: Option<&str>, path: &str) -> RepoEntry {
        RepoEntry {
            server: server.map(str::to_string),
            path: path.into(),
        }
    }

    #[test]
    fn 各台电脑的写法和这边的仓库一一对应() {
        let on_91 = RepoRef {
            server: Some("s91".into()),
            path: "/data/deploy".into(),
        };
        // 本机的工作区：和原来一样
        assert_eq!(
            member_entry(LOCAL, true, &RepoRef::local(r"C:\src\kuai")),
            entry(None, r"C:\src\kuai")
        );
        assert_eq!(
            member_entry(LOCAL, true, &on_91),
            entry(Some("s91"), "/data/deploy")
        );
        // 156 的工作区：156 自己的仓库换成正斜杠，156 配置的服务器上的经 156 连
        let own = member_entry("s156", true, &RepoRef::local(r"C:\work\wms"));
        assert_eq!(own, entry(Some("s156"), "C:/work/wms"));
        let relayed = member_entry("s156", true, &on_91);
        assert_eq!(relayed, entry(Some("s156>s91"), "/data/deploy"));
        // 反过来
        assert_eq!(
            member_ref("s156", &own),
            Some(RepoRef::local("C:/work/wms"))
        );
        assert_eq!(member_ref("s156", &relayed), Some(on_91.clone()));
        assert_eq!(
            member_ref(LOCAL, &entry(Some("s91"), "/data/deploy")),
            Some(on_91)
        );
        // 访问不到的：本机的仓库放不进 156 的工作区，经 156 的放不进本机的、也放不进别台的
        assert_eq!(member_ref("s156", &entry(None, r"C:\src\kuai")), None);
        assert_eq!(member_ref(LOCAL, &relayed), None);
        assert_eq!(member_ref("s15", &relayed), None);
        assert_eq!(member_ref("s15", &own), None);
        // 经别的电脑连的键
        assert_eq!(split_relay("s156>s91"), Some(("s156", "s91")));
        assert_eq!(split_relay("s156"), None);
        assert_eq!(machine_of(&relayed), "s156>s91");
        assert_eq!(machine_of(&entry(None, "x")), LOCAL);
    }

    #[test]
    fn 工作区的名字不重复() {
        let existing = ["默认", "工作", "工作 2"];
        assert_eq!(unique_name(" 新的 ", existing.iter().copied()), "新的");
        assert_eq!(unique_name("工作", existing.iter().copied()), "工作 3");
        assert_eq!(unique_name("  ", existing.iter().copied()), "工作区");
    }

    #[test]
    fn 新工作区的_id_不和已有的重() {
        let mut store = Store::default();
        let first = new_workspace_id(&store);
        assert!(first.starts_with('w'));
        store
            .apply(
                &[fr_core::store::Op::AddWorkspace {
                    id: first.clone(),
                    name: "a".into(),
                }],
                false,
            )
            .unwrap();
        assert_ne!(new_workspace_id(&store), first);
    }

    #[test]
    fn 旧设置里的仓库和工作区交给各台电脑() {
        let text = r#"{
            "servers": [{"id": "s156", "name": "156", "destination": "feng@156"}],
            "repos": [{"path": "C:/a"}, {"path": "C:/b"}, {"server": "s156", "path": "/srv/x"}],
            "workspaces": [
                {"name": "日常", "tabs": [{"path": "C:/a"}, {"server": "s156", "path": "/srv/x"}],
                 "active": 2, "repos": [{"path": "C:/b"}], "pinned": [{"path": "C:/a"}]},
                {"name": "空的", "tabs": []}
            ],
            "active_workspace": 1,
            "repo_memos": [
                {"path": "C:/b", "name": "乙"},
                {"server": "s156", "path": "/srv/x", "name": "部署"}
            ]
        }"#;
        let mut settings: Settings = serde_json::from_str(text).unwrap();
        settings.normalize();
        let mut local = Store::default();
        let mut ops = migration_ops(&mut settings, &local);
        // 启动时和本机配置的服务器一起交（见 start_machines）：工作区里服务器上的仓库要它才留得住
        ops.push(Op::SetServers {
            servers: vec![fr_core::store::Server {
                id: "s156".into(),
                name: "156".into(),
                ..Default::default()
            }],
        });
        local.apply(&ops, false).unwrap();

        // 本机的仓库进本机的列表，起的名字一起；服务器上的不进
        assert_eq!(local.repos, ["C:/a", "C:/b"]);
        assert_eq!(local.name_of("C:/b", false), Some("乙"));
        assert_eq!(local.name_of("/srv/x", false), None);
        // 工作区：名字和仓库（清单里的、开着的）都在
        let names: Vec<&str> = local.workspaces.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["日常", "空的"]);
        let daily = &local.workspaces[0];
        let on_156 = RepoRef {
            server: Some("s156".into()),
            path: "/srv/x".into(),
        };
        assert_eq!(
            daily.repos,
            [RepoRef::local("C:/b"), RepoRef::local("C:/a"), on_156]
        );
        // 开着的标签页记在这台电脑上；现在用的工作区跟着换过去
        let view = settings.view(&WorkspaceKey::new(LOCAL, &daily.id)).unwrap();
        assert_eq!(view.name, "日常");
        assert_eq!(
            view.tabs,
            [entry(None, "C:/a"), entry(Some("s156"), "/srv/x")]
        );
        assert_eq!(view.active, 2);
        assert!(view.is_pinned(&entry(None, "C:/a")));
        assert_eq!(
            settings.current,
            WorkspaceKey::new(LOCAL, &local.workspaces[1].id)
        );

        // 服务器上的：第一次连上时并进它的列表，起的名字一起；并过了、本机、经别台连的都不并
        let mut remote = Store::default();
        remote.apply(&merge_ops(&settings, "s156"), false).unwrap();
        assert_eq!(remote.repos, ["/srv/x"]);
        assert_eq!(remote.name_of("/srv/x", false), Some("部署"));
        assert!(merge_ops(&settings, LOCAL).is_empty());
        assert!(merge_ops(&settings, "s156>s91").is_empty());
        settings.merged_servers.push("s156".into());
        assert!(merge_ops(&settings, "s156").is_empty());
    }
}
