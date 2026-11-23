//! 各台电脑保存的仓库列表和工作区（协议 10，格式和改法见 `fr_core::store`）。
//!
//! 本机、各台服务器的仓库列表和工作区由它自己的 kuaid 保存。Kuai 连上一台就订阅它的
//! （`state_watch`），哪一边改了，所有连着的电脑都收到整份新的。这里只放这次运行里读到的，
//! 不存到磁盘上（不做缓存，见 `docs/design/server-workspaces/spec.md`）：没连上的服务器不知道它有什么。
//! 本机的例外：启动时先直接读本机的状态文件画出来（那是本机的数据本身），再连本机的 kuaid 收改动；
//! 本机的 kuaid 用不了时直接读写那个文件。
//!
//! 这边的 [`RepoEntry`] 和各台电脑的写法（[`RepoRef`]）怎么对应：
//!
//! - 本机的：`RepoRef { server: None }` 是本机的仓库，`Some(id)` 是本机配置的服务器上的，
//!   和 `RepoEntry` 一样。
//! - 服务器 `sa` 的：`RepoRef { server: None }` 是 A 自己的仓库，这边是 `server: "sa"`；
//!   `Some("sb")` 是 A 配置的服务器上的，这边是 `server: "sa>sb"`，经 A 的 kuaid 连
//!   （见 `fr_channel::Connection::connect_via`）。服务器的 id 是 `s` 加毫秒数，不会带 `>`。
//! - Windows 服务器自己的仓库：它的文件里是反斜杠（和它上面的 Kuai 自己加的一样），这边和别的
//!   服务器上的仓库一样用正斜杠（kuaid 给出来的路径也是这样）。
//!
//! 同一台电脑只用一个键（[`Identities`]）：经 A 连的那台认出来就是本机、就是 A 自己或者就是本机
//! 直接配置的某一台时，一律换成那一台的键，不重复列、不绕路连。认电脑靠 kuaid 握手时报的标识
//! （`fr_core::store::machine_id`），本机配置的服务器连上时记下它的（`Server::machine`），随服务器
//! 列表交给本机的 kuaid，连上本机的电脑也就认得本机配置的服务器各是哪台。

use std::collections::HashMap;
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
use crate::settings::{Auth, Front, RepoEntry, Settings, Tabs};

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

/// 两份服务器列表之间，到哪几台的路变了：地址、跳板机改了或者删掉了的，和经它们连的（跳板机可以
/// 套几层）。新加的不算。
fn changed_routes(old: &[fr_core::store::Server], new: &[fr_core::store::Server]) -> Vec<String> {
    let route = |server: &fr_core::store::Server| (server.destination.clone(), server.jump.clone());
    let mut changed: Vec<String> = old
        .iter()
        .filter(|server| {
            new.iter()
                .find(|now| now.id == server.id)
                .is_none_or(|now| route(now) != route(server))
        })
        .map(|server| server.id.clone())
        .collect();
    loop {
        let more: Vec<String> = old
            .iter()
            .filter(|server| !changed.contains(&server.id))
            .filter(|server| {
                server
                    .jump
                    .as_ref()
                    .is_some_and(|jump| changed.contains(jump))
            })
            .map(|server| server.id.clone())
            .collect();
        if more.is_empty() {
            return changed;
        }
        changed.extend(more);
    }
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
/// 放进 A 的工作区）是 None。
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
    server
        .and_then(|id| match id == machine {
            true => Some(None),
            false => split_relay(id)
                .filter(|(via, _)| *via == machine)
                .map(|(_, server)| Some(server.to_string())),
        })
        .map(|server| RepoRef {
            server,
            path: entry.path.clone(),
        })
}

/// 认电脑用的：各台电脑是哪台（kuaid 握手时报的标识，见 `fr_core::store::machine_id`）。
///
/// 同一台电脑只用一个键：经别的电脑连的那台（`sa>sX`）认出来就是本机、就是它经由的那台，或者
/// 就是本机直接配置的某一台时，一律换成那一台的键（[`Identities::canonical`]）。服务器页不重复列，
/// 工作区里的仓库直接按那一台打开（不绕路），终端也只有一份。还没有谁连上过、认不出来的照常列着。
pub struct Identities<'a> {
    /// 本机的
    pub local: Option<&'a str>,
    /// 这次运行里连上的各台握手时报的
    pub live: &'a HashMap<String, String>,
    /// 本机配置的服务器（记着上次连上时报的）
    pub servers: &'a [crate::settings::Server],
    /// 各台电脑保存的东西：它们配置的服务器也记着各是哪台
    pub machines: &'a HashMap<String, Machine>,
}

impl Identities<'_> {
    /// 电脑 `key` 是哪台；不知道是 None。连着的以握手时报的为准。
    pub fn of(&self, key: &str) -> Option<&str> {
        if key == LOCAL {
            return self.local;
        }
        if let Some(id) = self.live.get(key) {
            return Some(id);
        }
        match split_relay(key) {
            Some((via, server)) => self
                .machines
                .get(via)?
                .store
                .server(server)?
                .machine
                .as_deref(),
            None => self
                .servers
                .iter()
                .find(|server| server.id == key)?
                .machine
                .as_deref(),
        }
    }

    /// 同一台电脑只用一个键：经别的电脑连的那台是本机、是它经由的那台，或者是本机直接配置的一台
    /// （配置了几次的话第一个）时，换成那一台的键。别的原样。
    pub fn canonical(&self, key: &str) -> String {
        let (Some((via, _)), Some(id)) = (split_relay(key), self.of(key)) else {
            return key.to_string();
        };
        if self.local == Some(id) {
            return LOCAL.to_string();
        }
        if self.of(via) == Some(id) {
            return via.to_string();
        }
        self.servers
            .iter()
            .find(|server| self.of(&server.id) == Some(id))
            .map_or_else(|| key.to_string(), |server| server.id.clone())
    }

    /// 经别的电脑连的这台其实是另一个键（见 [`Self::canonical`]）。
    pub fn duplicate(&self, key: &str) -> bool {
        self.canonical(key) != key
    }

    /// 仓库换到它所在的那台电脑的键下（见 [`Self::canonical`]），路径换成那一台的写法。
    pub fn entry(&self, entry: RepoEntry) -> RepoEntry {
        let key = machine_of(&entry);
        let canonical = self.canonical(key);
        if canonical == key {
            return entry;
        }
        let path = self.path_on(&canonical, &entry.path);
        RepoEntry {
            server: (canonical != LOCAL).then_some(canonical),
            path,
        }
    }

    /// 电脑 `key` 上的一个仓库这边怎么写：它的列表里有就用列表里的写法（Windows 上大小写、斜杠
    /// 不同也认作同一个）；没有的话，本机是 Windows 时换成反斜杠（本机的仓库这样写），服务器上的原样。
    fn path_on(&self, key: &str, path: &str) -> String {
        if let Some(machine) = self.machines.get(key)
            && let Some(listed) = machine.store.listed(path, machine.windows)
        {
            return own_entry(key, machine.windows, listed).path;
        }
        match key == LOCAL && cfg!(windows) {
            true => path.replace('/', "\\"),
            false => path.to_string(),
        }
    }

    /// 电脑 `machine` 的工作区里的一个仓库，这边怎么写（[`member_entry`]，再换到它所在的那台下）。
    pub fn member(&self, machine: &str, repo: &RepoRef) -> RepoEntry {
        let windows = self
            .machines
            .get(machine)
            .is_some_and(|loaded| loaded.windows);
        self.entry(member_entry(machine, windows, repo))
    }

    /// 反过来：这个仓库放进电脑 `machine` 的工作区时怎么写。那台的工作区里已经有它（哪种写法都算）
    /// 就用那一种；那台访问不到的是 None。经由那台连得到的也算访问得到：本机的仓库放进 A 的
    /// 工作区，A 配置了本机的话，写成 A 配置的那台服务器上的。
    pub fn member_ref(&self, machine: &str, entry: &RepoEntry) -> Option<RepoRef> {
        let loaded = self.machines.get(machine);
        let written = loaded.and_then(|loaded| {
            loaded
                .store
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.repos)
                .find(|repo| self.member(machine, repo) == *entry)
        });
        if let Some(repo) = written {
            return Some(repo.clone());
        }
        if let Some(repo) = member_ref(machine, entry) {
            return Some(repo);
        }
        if machine == LOCAL {
            return None;
        }
        let id = self.of(machine_of(entry))?;
        let server = loaded?
            .store
            .servers
            .iter()
            .find(|server| server.machine.as_deref() == Some(id))?;
        // 本机的仓库到了别的电脑那里，和别的服务器上的一样写正斜杠
        let path = match entry.server.is_none() && cfg!(windows) {
            true => entry.path.replace('\\', "/"),
            false => entry.path.clone(),
        };
        Some(RepoRef {
            server: Some(server.id.clone()),
            path,
        })
    }

    /// 服务器页左边列的电脑：本机、本机配置的各台服务器；连着的服务器上配置的服务器（经由它连接）
    /// 跟在它下面，认出来是本机、是它自己或者本机直接配置了的那台的不列。
    pub fn machine_keys(&self) -> Vec<String> {
        let mut keys = vec![LOCAL.to_string()];
        for server in self.servers {
            keys.push(server.id.clone());
            let Some(loaded) = self.machines.get(&server.id) else {
                continue;
            };
            for relayed in &loaded.store.servers {
                let key = relay_key(&server.id, &relayed.id);
                if !self.duplicate(&key) {
                    keys.push(key);
                }
            }
        }
        keys
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
        // 读设置时补上的那个没有名字（见 `Settings::normalize`），这时界面语言已经定了
        let name = match group.name.is_empty() {
            true => tr("默认").to_string(),
            false => group.name.clone(),
        };
        ops.push(Op::AddWorkspace {
            id: id.clone(),
            name: name.clone(),
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
        view.name = name;
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
    // 1.13 以前的设置：开着的标签页这时才挪进各个工作区的记法，再带到全局的那一份里
    if settings.tabs().repos.is_empty() {
        settings.tabs = None;
        settings.migrate_tabs();
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
        // 本机是哪台：先读文件，连上的服务器、经它们连的服务器是不是本机马上就认得出来
        match fr_core::store::machine_id(&fr_core::store::path()) {
            Ok(id) => self.local_machine = Some(id),
            Err(err) => log::warn!("读不了本机的标识：{err:#}"),
        }
        // 设置文件没读全：主机列表可能是空的、少了几台，拿它去盖本机 kuaid 记着的那份，那份也没了
        // （原来就是这样，设置文件坏了连本机的主机列表一起清空）。设置里没有的从那份补回来：不含
        // 密码，密码在凭据管理器里按主机 id 存着，还对得上
        if self
            .load_issue
            .as_ref()
            .is_some_and(crate::settings::LoadIssue::incomplete)
        {
            self.restore_servers();
        }
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
    /// 用密码登录又没存密码的肯定要问，不去连。1.16 以前现在用的工作区所在的那台不在这里连（标签页
    /// 跟着它，要密码时弹窗）；现在标签页不跟工作区走，它和别的服务器一样。
    fn auto_connect(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .settings
            .servers
            .iter()
            .filter(|server| !server.manual_connect)
            .filter(|server| {
                server.auth != Auth::Password || crate::secrets::for_server(server).is_some()
            })
            .map(|server| server.id.clone())
            .collect();
        for id in ids {
            log::info!("启动时自动连接服务器 {id}");
            drop(self.connect_with(&id, true, cx));
        }
    }

    /// 本机 kuaid 记着、设置里没有的主机补进设置（设置文件没读全时，见 `start_machines`）。初始目录、
    /// 映射这些本机 kuaid 不记的回到默认。
    fn restore_servers(&mut self) {
        let Some(local) = self.machines.get(LOCAL) else {
            return;
        };
        let missing: Vec<crate::settings::Server> = local
            .store
            .servers
            .iter()
            .filter(|kept| !self.settings.servers.iter().any(|s| s.id == kept.id))
            .map(|kept| crate::settings::Server {
                id: kept.id.clone(),
                name: kept.name.clone(),
                destination: kept.destination.clone(),
                forward_agent: kept.forward_agent,
                auth: match kept.password_auth {
                    true => Auth::Password,
                    false => Auth::Key,
                },
                identity_file: kept.identity_file.clone(),
                jump: kept.jump.clone(),
                machine: kept.machine.clone(),
                ..Default::default()
            })
            .collect();
        if !missing.is_empty() {
            log::warn!("设置文件没读全，从本机 kuaid 补回 {} 台主机", missing.len());
            self.settings.servers.extend(missing);
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
                machine: server.machine.clone(),
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

    // ---- 认电脑 ----

    /// 认电脑用的（见 [`Identities`]）。
    pub fn ids(&self) -> Identities<'_> {
        Identities {
            local: self.local_machine.as_deref(),
            live: &self.machine_ids,
            servers: &self.settings.servers,
            machines: &self.machines,
        }
    }

    /// 记下连上的电脑 `key` 是哪台（握手时报的）。本机配置的服务器记进设置、交给本机的 kuaid，
    /// 别的电脑经本机连它时也认得；经别的电脑连着的那几台里认出重复的，不再单独订阅。
    pub(crate) fn learn_machine(&mut self, key: &str, machine: &str, cx: &mut Context<Self>) {
        match key == LOCAL {
            true => self.local_machine = Some(machine.to_string()),
            false => drop(
                self.machine_ids
                    .insert(key.to_string(), machine.to_string()),
            ),
        }
        if split_relay(key).is_none()
            && let Some(server) = self.settings.servers.iter_mut().find(|s| s.id == key)
            && server.machine.as_deref() != Some(machine)
        {
            server.machine = Some(machine.to_string());
            self.save();
            self.push_servers(cx);
        }
        self.drop_duplicates(cx);
    }

    /// 经别的电脑连着的那几台里认出是重复的（就是本机或者别的某一台）：不再订阅它的仓库列表和
    /// 终端，读到的也不留（各一份就够）。连接本身留着，正用着它的标签页换过去以前照样能用。
    fn drop_duplicates(&mut self, cx: &mut Context<Self>) {
        let duplicates: Vec<String> = {
            let ids = self.ids();
            self.machines
                .keys()
                .chain(self.links.keys())
                .filter(|key| ids.duplicate(key))
                .cloned()
                .collect()
        };
        if duplicates.is_empty() {
            return;
        }
        for key in duplicates {
            let target = self.ids().canonical(&key);
            log::info!(
                "{key} 和 {} 是同一台电脑，不再单独订阅",
                self.machine_name(&target)
            );
            self.machines.remove(&key);
            self.links.remove(&key);
            self.watches.remove(&key);
        }
        cx.notify();
    }

    /// 经别的电脑连上的这台认出来就是本机或者已有的一台：说一声（它从服务器页上收起来了）。
    /// 服务器页上那一行要读到经由的那台的服务器列表才有；还没读到时（恢复标签页时自动连的）
    /// 那一行没出现过，名字也查不到，不说。
    pub(crate) fn note_duplicate(&mut self, key: &str, cx: &mut Context<Self>) {
        let target = self.ids().canonical(key);
        log::info!("{key} 和 {} 是同一台电脑", self.machine_name(&target));
        let (Some((via, _)), Some(name)) = (
            split_relay(key),
            self.relayed_server(key).map(|server| server.name.clone()),
        ) else {
            return;
        };
        let text = tf!(
            "经由 {via} 连接的「{name}」与「{target}」是同一台电脑，不再单独列出。",
            via = self.machine_name(via),
            name,
            target = self.machine_name(&target)
        );
        cx.emit(Notice::Info(text.into()));
    }

    /// 服务器页左边列的电脑（见 [`Identities::machine_keys`]）。
    pub fn machine_keys(&self) -> Vec<String> {
        self.ids().machine_keys()
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
        // 和 kuaid 用同一把跨进程的锁：它其实还活着（只是这边连不上）时，谁也不拿旧的盖掉对方的
        match fr_core::store::update(&fr_core::store::path(), &ops, cfg!(windows)) {
            Ok((store, _)) => self.store_arrived(LOCAL, store, cx),
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
        // 经它连的那几台里，它改了地址、跳板机或者删掉了的：原来握手记下的标识、接好的连接都是
        // 原来那台的，不能再用来认电脑、交改动
        if split_relay(key).is_none()
            && let Some(old) = self.machines.get(key)
        {
            for server in changed_routes(&old.store.servers, &store.servers) {
                let relayed = relay_key(key, &server);
                self.forget_target(&relayed, false, cx);
                if self.settings.current.machine == relayed {
                    self.schedule_reconnect(&relayed, cx);
                }
            }
        }
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
                        // 自己在重连：要密码、口令时不弹窗（只管这一次连接，别的操作照常问）
                        (wanted && !linked).then(|| match key == LOCAL {
                            true => this.connect_local(cx),
                            false => this.connect_with(&key, true, cx),
                        })
                    });
                    let Ok(Some(connecting)) = connecting else {
                        break;
                    };
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

    /// 这个仓库在列表里哪个仓库的目录下面（同一台电脑的列表里、不是它自己）。子仓库的标签页、
    /// 标签上同名时写的说明用；不是子仓库是 None。
    pub fn root_of(&self, entry: &RepoEntry) -> Option<RepoEntry> {
        self.repos_on(machine_of(entry))?
            .into_iter()
            .find(|root| root != entry && root.contains(entry))
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
    /// 各个工作区是第几种颜色（见 [`crate::ui::hue`]）。按名字分，撞了挑用得最少的、从名字分到的
    /// 那一种往后数；按电脑的先后（本机、各台主机）、各自工作区的先后挨个分。所以前六个工作区各是
    /// 各的颜色，加工作区不会改已有的；同一个工作区在各个页面上同色。
    pub fn workspace_hues(&self) -> HashMap<WorkspaceKey, usize> {
        let mut all: Vec<(WorkspaceKey, &str)> = Vec::new();
        for machine in self.machine_keys() {
            let Some(loaded) = self.machines.get(&machine) else {
                continue;
            };
            for workspace in &loaded.store.workspaces {
                all.push((WorkspaceKey::new(&machine, &workspace.id), &workspace.name));
            }
        }
        let slots = crate::ui::hue_slots(all.iter().map(|(_, name)| *name));
        all.into_iter().map(|(key, _)| key).zip(slots).collect()
    }

    /// 一个工作区的颜色是第几种（见 [`Self::workspace_hues`]）。不知道的按名字分。
    pub fn workspace_hue(&self, key: &WorkspaceKey) -> usize {
        self.workspace_hues()
            .get(key)
            .copied()
            .unwrap_or_else(|| crate::ui::name_slot(&self.workspace_name(key), crate::ui::HUES))
    }

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

    /// 工作区里的仓库（这边的写法，同一台电脑只用一个键），按加进来的先后；没读到是空的。
    /// 两种写法指的是同一个仓库（比如 A 自己的和经 A 配置的「A」的）只列一次。
    pub fn members(&self, key: &WorkspaceKey) -> Vec<RepoEntry> {
        let ids = self.ids();
        let mut members: Vec<RepoEntry> = Vec::new();
        for repo in self.workspace(key).map_or(&[][..], |w| &w.repos) {
            let entry = ids.member(&key.machine, repo);
            if !members.contains(&entry) {
                members.push(entry);
            }
        }
        members
    }

    /// 标题栏上开着的标签页（全局一份）。
    pub fn tabs(&self) -> &Tabs {
        self.settings.tabs()
    }

    /// 包含这个仓库的第一个工作区（读到了的电脑上，本机的在前）。子仓库算它所在的仓库的。
    /// 标签页右键的「在工作区页中显示」用。
    pub fn workspace_of(&self, entry: &RepoEntry) -> Option<WorkspaceKey> {
        let mut machines: Vec<&String> = self.machines.keys().collect();
        machines.sort_by(|a, b| (a.as_str() != LOCAL, a).cmp(&(b.as_str() != LOCAL, b)));
        let keys: Vec<WorkspaceKey> = machines
            .into_iter()
            .flat_map(|machine| self.workspaces_on(machine).unwrap_or_default())
            .map(|(key, _, _)| key)
            .collect();
        let find = |wanted: &dyn Fn(&RepoEntry) -> bool| {
            keys.iter()
                .find(|key| self.members(key).iter().any(wanted))
                .cloned()
        };
        find(&|member| member == entry).or_else(|| find(&|member| member.contains(entry)))
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
    /// 那台电脑的工作区能放的就是这些（工作区页的「添加仓库…」里列的）。它配置的服务器认出来是
    /// 本机或者本机直接配置的一台时，列那一台的（同一台电脑只用一个键）；是它自己的不重复列。
    pub fn perspective_repos(&self, machine: &str) -> Vec<RepoEntry> {
        let mut keys: Vec<String> = vec![machine.to_string()];
        match machine == LOCAL {
            true => keys.extend(self.settings.servers.iter().map(|s| s.id.clone())),
            false => {
                let ids = self.ids();
                if let Some(loaded) = self.machines.get(machine) {
                    for server in &loaded.store.servers {
                        let key = ids.canonical(&relay_key(machine, &server.id));
                        if !keys.contains(&key) {
                            keys.push(key);
                        }
                    }
                }
            }
        }
        let mut repos: Vec<RepoEntry> = Vec::new();
        for key in keys {
            for entry in self.repos_on(&key).unwrap_or_default() {
                if !repos.contains(&entry) {
                    repos.push(entry);
                }
            }
        }
        repos
    }

    // ---- 工作区 ----

    /// 工作区页上选中一个工作区（下次打开这一页还是它）。只是选中，标签页不动。
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
        self.menu_machine = Some(key.machine.clone());
        self.settings.current = key;
        self.save();
        cx.notify();
    }

    /// 工作区页列哪台电脑上的工作区：页面上的下拉里换过就是换到的那台（它还在的话），不然是选中的
    /// 工作区所在的那台。
    pub fn menu_machine(&self) -> String {
        let known = |id: &String| id == LOCAL || self.settings.servers.iter().any(|s| s.id == *id);
        let chosen = self.menu_machine.clone().filter(known);
        chosen.unwrap_or_else(|| self.settings.current.machine.clone())
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

    /// 从它所在那台电脑的列表里去掉仓库（仓库本身不动）；各台电脑的工作区里有它的也去掉。
    pub fn remove_repo(&mut self, entry: &RepoEntry, cx: &mut Context<Self>) {
        let machine = machine_of(entry).to_string();
        let path = entry.path.clone();
        if !self.apply(&machine, vec![Op::RemoveRepo { path }], cx) {
            return;
        }
        // 别的电脑上的工作区里放着它的（比如本机的工作区里有 B 上的这个仓库，A 的工作区里有
        // 经 A 配置的「本机」放进去的本机的这个仓库）
        let mut elsewhere: Vec<(String, Vec<Op>)> = Vec::new();
        let ids = self.ids();
        for (key, loaded) in &self.machines {
            if *key == machine || !loaded.live {
                continue;
            }
            let ops: Vec<Op> = loaded
                .store
                .workspaces
                .iter()
                .flat_map(|w| {
                    w.repos
                        .iter()
                        .filter(|repo| ids.member(key, repo) == *entry)
                        .map(|repo| Op::RemoveFromWorkspace {
                            id: w.id.clone(),
                            repo: repo.clone(),
                        })
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

    /// 把仓库放进工作区 `key`（已经在了不动）。那台电脑访问不到这个仓库的（本机的仓库放进没有配置
    /// 本机的服务器的工作区）放不进，返回假。
    pub fn add_to_workspace(
        &mut self,
        key: &WorkspaceKey,
        entry: &RepoEntry,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(repo) = self.ids().member_ref(&key.machine, entry) else {
            return false;
        };
        if self.members(key).contains(entry) {
            return true;
        }
        let id = key.id.clone();
        self.apply(&key.machine, vec![Op::AddToWorkspace { id, repo }], cx)
    }

    /// 把仓库从工作区 `key` 去掉（它还在列表里，别的工作区里的也不动，开着的标签页也不关）。
    /// 工作区里几种写法指的都是它的，一起去掉。
    pub fn remove_from_workspace(
        &mut self,
        key: &WorkspaceKey,
        entry: &RepoEntry,
        cx: &mut Context<Self>,
    ) -> bool {
        let ops: Vec<Op> = {
            let ids = self.ids();
            self.workspace(key)
                .map_or(&[][..], |w| &w.repos)
                .iter()
                .filter(|repo| ids.member(&key.machine, repo) == *entry)
                .map(|repo| Op::RemoveFromWorkspace {
                    id: key.id.clone(),
                    repo: repo.clone(),
                })
                .collect()
        };
        if ops.is_empty() {
            return false;
        }
        self.apply(&key.machine, ops, cx)
    }

    // ---- 标签页（全局一份，只记在这台电脑上） ----

    /// 记下开着的标签页和显示着的那一页。
    pub fn set_tabs(&mut self, repos: Vec<RepoEntry>, front: Front) {
        let tabs = self.settings.tabs_mut();
        if tabs.repos != repos || tabs.front != front {
            // 重新打开了的不算「关掉的」了
            tabs.closed.retain(|e| !repos.contains(e));
            tabs.pinned.retain(|e| repos.contains(e));
            tabs.repos = repos;
            tabs.front = front;
            self.save();
        }
    }

    /// 固定、取消固定一个仓库标签页；返回现在是不是固定的。
    pub fn toggle_pinned(&mut self, entry: &RepoEntry, cx: &mut Context<Self>) -> bool {
        let tabs = self.settings.tabs_mut();
        let pinned = !tabs.is_pinned(entry);
        tabs.pinned.retain(|e| e != entry);
        if pinned {
            tabs.pinned.push(entry.clone());
        }
        tabs.pinned_first();
        self.save();
        cx.notify();
        pinned
    }

    /// 标签页换了一种写法（经别的电脑连的那台认出来是哪一台了）：固定着的照样固定。
    pub fn repin(&mut self, from: &RepoEntry, to: &RepoEntry) {
        let tabs = self.settings.tabs_mut();
        if tabs.is_pinned(from) && !tabs.is_pinned(to) {
            tabs.pinned.push(to.clone());
        }
    }

    /// 记下刚关掉的一个仓库标签页（Ctrl+Shift+T 和快速打开里能找回来）。
    pub fn note_closed_tab(&mut self, entry: RepoEntry) {
        self.settings.tabs_mut().note_closed(entry);
    }

    /// 最近关掉的仓库标签页，最近的在前。
    pub fn closed_tabs(&self) -> Vec<RepoEntry> {
        self.tabs().closed.iter().rev().cloned().collect()
    }

    /// 各台电脑列表里的仓库（读到了的），本机的在前，再是本机配置的服务器、经服务器连的。
    /// 快速打开里列的就是这些。经服务器连的那台认出来是重复的不列（见 [`Identities`]）。
    pub fn all_repos(&self) -> Vec<RepoEntry> {
        let ids = self.ids();
        let mut machines: Vec<&String> = self
            .machines
            .keys()
            .filter(|key| !ids.duplicate(key))
            .collect();
        machines.sort_by(|a, b| {
            let rank = |key: &str| (key != LOCAL, split_relay(key).is_some());
            (rank(a), a).cmp(&(rank(b), b))
        });
        let mut repos: Vec<RepoEntry> = Vec::new();
        for machine in machines {
            for entry in self.repos_on(machine).unwrap_or_default() {
                if !repos.contains(&entry) {
                    repos.push(entry);
                }
            }
        }
        repos
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
        Identities, LOCAL, Machine, WorkspaceKey, changed_routes, machine_of, member_entry,
        member_ref, merge_ops, migration_ops, new_workspace_id, split_relay, unique_name,
    };
    use crate::settings::{Front, RepoEntry, Settings};
    use fr_core::store::{Op, RepoRef, Store, Workspace};
    use std::collections::HashMap;

    #[test]
    fn 经由的那台改了谁的路() {
        let server = |id: &str, destination: &str, jump: Option<&str>| fr_core::store::Server {
            id: id.into(),
            name: id.into(),
            destination: destination.into(),
            forward_agent: false,
            password_auth: false,
            identity_file: None,
            jump: jump.map(String::from),
            machine: Some("m".into()),
        };
        let old = vec![
            server("a", "10.0.0.1", None),
            server("b", "10.0.0.2", Some("a")),
            server("c", "10.0.0.3", Some("b")),
            server("d", "10.0.0.4", None),
            server("e", "10.0.0.5", None),
        ];
        // a 改了地址：经 a 连的 b、经 b 连的 c 也算；e 删掉了；名字、标识变了不算；新加的不算
        let mut new = vec![
            server("a", "10.0.9.1", None),
            server("b", "10.0.0.2", Some("a")),
            server("c", "10.0.0.3", Some("b")),
            server("d", "10.0.0.4", None),
            server("f", "10.0.0.6", None),
        ];
        new[3].name = "改了名字".into();
        new[3].machine = None;
        let mut changed = changed_routes(&old, &new);
        changed.sort();
        assert_eq!(changed, ["a", "b", "c", "e"]);
        assert!(changed_routes(&old, &old).is_empty());
    }

    const L: &str = "1111aaaa1111aaaa1111aaaa1111aaaa";
    const A: &str = "2222bbbb2222bbbb2222bbbb2222bbbb";
    const B: &str = "3333cccc3333cccc3333cccc3333cccc";
    const X: &str = "4444dddd4444dddd4444dddd4444dddd";

    /// 本机（Windows，标识 L）配置了 sA（A）、sB（B）、sC（还没连上过）。sA 配置了 s1 = B、s2 = 本机、
    /// s3 = sA 自己（换了个用户名）、s4（sA 也没连上过，不知道是哪台）、s5 = 一台本机没配置的 X；
    /// 它的工作区里有自己的仓库、经 s2 放进去的本机仓库（大小写和斜杠不一样）、经 s1 放进去的 sB 的。
    struct World {
        servers: Vec<crate::settings::Server>,
        machines: HashMap<String, Machine>,
        live: HashMap<String, String>,
    }

    impl World {
        fn new() -> World {
            let server = |id: &str, machine: Option<&str>| crate::settings::Server {
                id: id.into(),
                name: id.into(),
                destination: format!("u@{id}"),
                machine: machine.map(str::to_string),
                ..Default::default()
            };
            let relayed = |id: &str, machine: Option<&str>| fr_core::store::Server {
                id: id.into(),
                name: id.into(),
                destination: format!("u@{id}"),
                machine: machine.map(str::to_string),
                ..Default::default()
            };
            let on = |server: Option<&str>, path: &str| RepoRef {
                server: server.map(str::to_string),
                path: path.into(),
            };
            let local = Store {
                repos: vec![r"C:\src\erp".into(), r"C:\src\web".into()],
                ..Store::default()
            };
            let a = Store {
                repos: vec!["/srv/a".into()],
                servers: vec![
                    relayed("s1", Some(B)),
                    relayed("s2", Some(L)),
                    relayed("s3", Some(A)),
                    relayed("s4", None),
                    relayed("s5", Some(X)),
                ],
                workspaces: vec![Workspace {
                    id: "w".into(),
                    name: "交付".into(),
                    repos: vec![
                        on(None, "/srv/a"),
                        on(Some("s2"), "c:/src/ERP"),
                        on(Some("s1"), "/srv/b"),
                        on(Some("s3"), "/srv/a"),
                        on(Some("s4"), "/srv/d"),
                    ],
                }],
                ..Store::default()
            };
            let b = Store {
                repos: vec!["/srv/b".into()],
                ..Store::default()
            };
            World {
                servers: vec![
                    server("sA", Some(A)),
                    server("sB", Some(B)),
                    server("sC", None),
                ],
                machines: HashMap::from([
                    (LOCAL.to_string(), Machine::new(local, true)),
                    ("sA".to_string(), Machine::new(a, false)),
                    ("sB".to_string(), Machine::new(b, false)),
                ]),
                live: HashMap::new(),
            }
        }

        fn ids(&self) -> Identities<'_> {
            Identities {
                local: Some(L),
                live: &self.live,
                servers: &self.servers,
                machines: &self.machines,
            }
        }
    }

    #[test]
    fn 经由别的电脑连的那台_认出来是谁就换成谁的键() {
        let mut world = World::new();
        let ids = world.ids();
        assert_eq!(ids.canonical("sA>s1"), "sB", "本机直接配置了的那台");
        assert_eq!(ids.canonical("sA>s2"), LOCAL, "就是本机");
        assert_eq!(ids.canonical("sA>s3"), "sA", "就是它经由的那台");
        assert_eq!(ids.canonical("sA>s4"), "sA>s4", "认不出来的照常");
        assert_eq!(ids.canonical("sA>s5"), "sA>s5", "别的电脑，本机没配置");
        assert_eq!(ids.canonical("sB"), "sB");
        assert_eq!(ids.canonical(LOCAL), LOCAL);
        assert!(ids.duplicate("sA>s2") && !ids.duplicate("sA>s4") && !ids.duplicate("sA"));
        // 服务器页：重复的不列，认不出来的、别的电脑照常挂在 sA 下面
        assert_eq!(
            ids.machine_keys(),
            [LOCAL, "sA", "sA>s4", "sA>s5", "sB", "sC"]
        );
        // 经 sA 连上 s4 以后握手报的是本机：也认出来了
        world.live.insert("sA>s4".into(), L.into());
        assert_eq!(world.ids().canonical("sA>s4"), LOCAL);
        assert_eq!(
            world.ids().machine_keys(),
            [LOCAL, "sA", "sA>s5", "sB", "sC"]
        );
        // 本机配置了两次同一台：换成第一个
        world.servers.push(crate::settings::Server {
            id: "sB2".into(),
            machine: Some(B.into()),
            ..Default::default()
        });
        assert_eq!(world.ids().canonical("sA>s1"), "sB");
    }

    #[test]
    fn 工作区里的仓库按它所在的那台写_反过来也写得回去() {
        let world = World::new();
        let ids = world.ids();
        let entry = |server: Option<&str>, path: &str| RepoEntry {
            server: server.map(str::to_string),
            path: path.into(),
        };
        let on = |server: Option<&str>, path: &str| RepoRef {
            server: server.map(str::to_string),
            path: path.into(),
        };
        // 经 s2（本机）放进去的：就是本机的那个仓库，用本机列表里的写法
        assert_eq!(
            ids.member("sA", &on(Some("s2"), "c:/src/ERP")),
            entry(None, r"C:\src\erp")
        );
        // 不在本机列表里的（子仓库）：本机是 Windows 时换成反斜杠
        let sub = ids.member("sA", &on(Some("s2"), "C:/src/erp/libs/sdk"));
        let expected = match cfg!(windows) {
            true => r"C:\src\erp\libs\sdk",
            false => "C:/src/erp/libs/sdk",
        };
        assert_eq!(sub, entry(None, expected));
        assert_eq!(
            ids.member("sA", &on(Some("s1"), "/srv/b")),
            entry(Some("sB"), "/srv/b")
        );
        assert_eq!(
            ids.member("sA", &on(Some("s3"), "/srv/a")),
            entry(Some("sA"), "/srv/a")
        );
        assert_eq!(
            ids.member("sA", &on(None, "/srv/a")),
            entry(Some("sA"), "/srv/a")
        );
        assert_eq!(
            ids.member("sA", &on(Some("s4"), "/srv/d")),
            entry(Some("sA>s4"), "/srv/d")
        );

        // 反过来：工作区里已经有的，用它原来的写法（移出时才对得上）
        assert_eq!(
            ids.member_ref("sA", &entry(None, r"C:\src\erp")),
            Some(on(Some("s2"), "c:/src/ERP"))
        );
        // 还没放进去的本机仓库：经 sA 配置的本机，写正斜杠（本机是 Windows 时）
        let web = match cfg!(windows) {
            true => "C:/src/web",
            false => r"C:\src\web",
        };
        assert_eq!(
            ids.member_ref("sA", &entry(None, r"C:\src\web")),
            Some(on(Some("s2"), web))
        );
        assert_eq!(
            ids.member_ref("sA", &entry(Some("sB"), "/srv/b")),
            Some(on(Some("s1"), "/srv/b"))
        );
        assert_eq!(
            ids.member_ref("sA", &entry(Some("sB"), "/srv/b2")),
            Some(on(Some("s1"), "/srv/b2"))
        );
        assert_eq!(
            ids.member_ref("sA", &entry(Some("sA"), "/srv/a")),
            Some(on(None, "/srv/a"))
        );
        // 访问不到的：sA 没配置 sC；本机的工作区放不进经 sA 连的；sB 没配置本机
        assert_eq!(ids.member_ref("sA", &entry(Some("sC"), "/x")), None);
        assert_eq!(ids.member_ref(LOCAL, &entry(Some("sA>s4"), "/srv/d")), None);
        assert_eq!(ids.member_ref("sB", &entry(None, r"C:\src\erp")), None);
        // 本机的工作区里写法和以前一样
        assert_eq!(
            ids.member_ref(LOCAL, &entry(Some("sB"), "/srv/b")),
            Some(on(Some("sB"), "/srv/b"))
        );
    }

    fn entry(server: Option<&str>, path: &str) -> RepoEntry {
        RepoEntry {
            server: server.map(str::to_string),
            path: path.into(),
        }
    }

    #[test]
    fn 各台电脑的写法和这边的仓库一一对应() {
        let on_b = RepoRef {
            server: Some("sb".into()),
            path: "/data/deploy".into(),
        };
        // 本机的工作区：和原来一样
        assert_eq!(
            member_entry(LOCAL, true, &RepoRef::local(r"C:\src\kuai")),
            entry(None, r"C:\src\kuai")
        );
        assert_eq!(
            member_entry(LOCAL, true, &on_b),
            entry(Some("sb"), "/data/deploy")
        );
        // A 的工作区：A 自己的仓库换成正斜杠，A 配置的服务器上的经 A 连
        let own = member_entry("sa", true, &RepoRef::local(r"C:\work\wms"));
        assert_eq!(own, entry(Some("sa"), "C:/work/wms"));
        let relayed = member_entry("sa", true, &on_b);
        assert_eq!(relayed, entry(Some("sa>sb"), "/data/deploy"));
        // 反过来
        assert_eq!(member_ref("sa", &own), Some(RepoRef::local("C:/work/wms")));
        assert_eq!(member_ref("sa", &relayed), Some(on_b.clone()));
        assert_eq!(
            member_ref(LOCAL, &entry(Some("sb"), "/data/deploy")),
            Some(on_b)
        );
        // 访问不到的：本机的仓库放不进 A 的工作区，经 A 的放不进本机的、也放不进别台的
        assert_eq!(member_ref("sa", &entry(None, r"C:\src\kuai")), None);
        assert_eq!(member_ref(LOCAL, &relayed), None);
        assert_eq!(member_ref("s15", &relayed), None);
        assert_eq!(member_ref("s15", &own), None);
        // 经别的电脑连的键
        assert_eq!(split_relay("sa>sb"), Some(("sa", "sb")));
        assert_eq!(split_relay("sa"), None);
        assert_eq!(machine_of(&relayed), "sa>sb");
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
    fn 第一次启动补的工作区等界面语言定了再起名() {
        use fr_core::i18n::tr;
        // 读设置时（还不知道界面语言）补上的工作区没有名字
        let mut settings = Settings::default();
        assert_eq!(settings.workspaces.len(), 1);
        assert!(settings.workspaces[0].name.is_empty());
        // 交给本机的 kuaid 时（语言已经定了）才叫「默认」
        let mut local = Store::default();
        let ops = migration_ops(&mut settings, &local);
        local.apply(&ops, false).unwrap();
        assert_eq!(local.workspaces.len(), 1);
        assert_eq!(local.workspaces[0].name, tr("默认"));
        let key = WorkspaceKey::new(LOCAL, &local.workspaces[0].id);
        assert_eq!(settings.view(&key).unwrap().name, tr("默认"));
    }

    #[test]
    fn 旧设置里的仓库和工作区交给各台电脑() {
        let text = r#"{
            "servers": [{"id": "sa", "name": "A", "destination": "me@host-a"}],
            "repos": [{"path": "C:/a"}, {"path": "C:/b"}, {"server": "sa", "path": "/srv/x"}],
            "workspaces": [
                {"name": "日常", "tabs": [{"path": "C:/a"}, {"server": "sa", "path": "/srv/x"}],
                 "active": 2, "repos": [{"path": "C:/b"}], "pinned": [{"path": "C:/a"}]},
                {"name": "空的", "tabs": []}
            ],
            "active_workspace": 1,
            "repo_memos": [
                {"path": "C:/b", "name": "乙"},
                {"server": "sa", "path": "/srv/x", "name": "部署"}
            ]
        }"#;
        let mut settings: Settings = serde_json::from_str(text).unwrap();
        settings.normalize();
        let mut local = Store::default();
        let mut ops = migration_ops(&mut settings, &local);
        // 启动时和本机配置的服务器一起交（见 start_machines）：工作区里服务器上的仓库要它才留得住
        ops.push(Op::SetServers {
            servers: vec![fr_core::store::Server {
                id: "sa".into(),
                name: "A".into(),
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
        let on_a = RepoRef {
            server: Some("sa".into()),
            path: "/srv/x".into(),
        };
        assert_eq!(
            daily.repos,
            [RepoRef::local("C:/b"), RepoRef::local("C:/a"), on_a]
        );
        // 开着的标签页记在这台电脑上；现在用的工作区跟着换过去
        let view = settings.view(&WorkspaceKey::new(LOCAL, &daily.id)).unwrap();
        assert_eq!(view.name, "日常");
        assert_eq!(
            view.tabs,
            [entry(None, "C:/a"), entry(Some("sa"), "/srv/x")]
        );
        assert_eq!(view.active, 2);
        assert!(view.pinned.contains(&entry(None, "C:/a")));
        assert_eq!(
            settings.current,
            WorkspaceKey::new(LOCAL, &local.workspaces[1].id)
        );
        // 标题栏上的标签页（全局一份）只带现在用的工作区那一份：「空的」里什么都没开
        assert!(settings.tabs().repos.is_empty());
        // 现在用的是「日常」的话，它开着的、选中的、固定的都带过来
        let mut daily_first: Settings = serde_json::from_str(
            &text.replace("\"active_workspace\": 1", "\"active_workspace\": 0"),
        )
        .unwrap();
        daily_first.normalize();
        migration_ops(&mut daily_first, &Store::default());
        let tabs = daily_first.tabs();
        assert_eq!(
            tabs.repos,
            [entry(None, "C:/a"), entry(Some("sa"), "/srv/x")]
        );
        assert_eq!(tabs.front, Front::Repo(entry(Some("sa"), "/srv/x")));
        assert_eq!(tabs.pinned, [entry(None, "C:/a")]);

        // 服务器上的：第一次连上时并进它的列表，起的名字一起；并过了、本机、经别台连的都不并
        let mut remote = Store::default();
        remote.apply(&merge_ops(&settings, "sa"), false).unwrap();
        assert_eq!(remote.repos, ["/srv/x"]);
        assert_eq!(remote.name_of("/srv/x", false), Some("部署"));
        assert!(merge_ops(&settings, LOCAL).is_empty());
        assert!(merge_ops(&settings, "sa>sb").is_empty());
        settings.merged_servers.push("sa".into());
        assert!(merge_ops(&settings, "sa").is_empty());
    }
}
