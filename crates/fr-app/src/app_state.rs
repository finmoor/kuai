//! 全局状态：设置和各台服务器的连接。界面各部分共用一个 [`AppState`] 实体。

use fr_core::i18n::tr;
use fr_core::tf;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fr_channel::{Channel, ConnectOptions, Connection, Jump};
use futures::FutureExt as _;
use futures::future::{LocalBoxFuture, Shared};
use gpui_kit::*;

use crate::machines::{LOCAL, Machine, split_relay};
use crate::settings::{self, Auth, Layout, RepoEntry, RepoMemo, Server, Settings, WindowState};

/// 正在进行或已经结束的连接；多处等同一个连接时共用。
pub(crate) type Connecting = Shared<Task<Result<Arc<Connection>, SharedString>>>;

/// 存设置失败的原因往这里送（见 [`AppState::save`]）。
static SAVE_FAILED: std::sync::OnceLock<futures::channel::mpsc::UnboundedSender<String>> =
    std::sync::OnceLock::new();

/// 收存设置失败的原因（主窗口收，弹通知）。只能要一次。
pub fn save_failures() -> futures::channel::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let _ = SAVE_FAILED.set(tx);
    rx
}

/// 一次连接在登录阶段失败的详情，见 [`AppState::last_login_failure`]。
type FailureSlot = Arc<std::sync::Mutex<Option<fr_channel::LoginFailure>>>;

/// 直接连的服务器 ssh 连不上（超时、连接被拒绝、对方断开，多半是网络抖了一下、电脑刚醒）时，同一次
/// 连接里再试几次，都不行才算连不上。登录被拒绝、主机密钥不对的不再试（见 `connect_with`）。
pub(crate) const CONNECT_RETRIES: u32 = 5;
/// 每次重试前等多久
const CONNECT_RETRY_GAP: Duration = Duration::from_secs(2);
/// 一次就等了这么久才失败的不再试：多半是登录上以后主机那头卡住了（启动脚本卡在挂死的网络盘上，
/// 等到握手期限 60 秒才结束），再试也一样，五次就是五六分钟。断网时 ssh 10 秒就超时
const CONNECT_RETRY_SLOW: Duration = Duration::from_secs(50);

/// 后台连接线程报给界面的进度
enum Progress {
    /// 经跳板机的：登录上几台了
    Stage(usize),
    /// 连不上，开始第几次重试（从 1 数）
    Retry(u32),
}

/// 一台服务器的连接状态。
pub enum ConnStatus {
    Idle,
    Connecting,
    Ready(Arc<Connection>),
    Failed(SharedString),
}

/// 一台电脑（重新）连上了，键同 `machines`（本机是 [`crate::machines::LOCAL`]）。断网、睡眠以后
/// 在后台连回来时，它上面停着的仓库标签页、终端听到这个自己接上。
pub struct HostConnected(pub String);

impl EventEmitter<HostConnected> for AppState {}

pub struct AppState {
    pub settings: Settings,
    conns: HashMap<String, Connecting>,
    /// 经跳板机连接、还在连的：已经登录上几台了（跳板机按顺序算）
    stages: HashMap<String, usize>,
    /// 还在连的：连不上、在第几次重试了（见 [`CONNECT_RETRIES`]）
    retrying: HashMap<String, u32>,
    /// 最近一次连接失败在登录阶段的哪一步（诊断用，见 `diagnose`）；登录上以后才失败的是空的。
    /// 开始连接时就放进来，后台线程在连接结果出来之前写好：等同一个结果的诊断框读到的一定是这一次的
    login_failures: HashMap<String, FailureSlot>,
    /// 每次连接一个「不要了」的标记（[`Self::drop_conns`] 时立起）：还在连、还在重试的那一次看到它就
    /// 停下，不再按旧的设置连；它报的进度也不再写到同一台的新连接上
    conn_dropped: HashMap<String, Arc<std::sync::atomic::AtomicBool>>,
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
    /// 断开、连不上以后在后台重连的任务（见 `AppState::schedule_reconnect`），连上、不用了就去掉
    pub(crate) reconnects: HashMap<String, Task<()>>,
    /// 上一次因为窗口回到前台马上重连是什么时候（`AppState::retry_now`）
    pub(crate) reconnect_kicked: Option<Instant>,
    /// 工作区页的下拉里选的是哪台电脑（没选过是选中的工作区所在的那台）。只在这次运行里记着
    pub(crate) menu_machine: Option<String>,
    /// 本机的标识（`fr_core::store::machine_id`）：启动时读文件，本机的 kuaid 握手时报的为准
    pub(crate) local_machine: Option<String>,
    /// 这次运行里连上的各台电脑握手时报的标识（直接配置的、经别的电脑连的都记）
    pub(crate) machine_ids: HashMap<String, String>,
    /// 启动时读设置文件出的问题（见 [`settings::LoadIssue`]）
    pub(crate) load_issue: Option<settings::LoadIssue>,
}

impl AppState {
    pub fn new(
        settings: Settings,
        load_issue: Option<settings::LoadIssue>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            load_issue,
            settings,
            conns: HashMap::new(),
            stages: HashMap::new(),
            retrying: HashMap::new(),
            login_failures: HashMap::new(),
            conn_dropped: HashMap::new(),
            save_later: None,
            mounts: crate::mapping::Mounts::new(),
            machines: HashMap::new(),
            links: HashMap::new(),
            pending: HashMap::new(),
            watches: HashMap::new(),
            reconnects: HashMap::new(),
            reconnect_kicked: None,
            menu_machine: None,
            local_machine: None,
            machine_ids: HashMap::new(),
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
        // 重试的这一次还没登录上第一台
        if let Some(retry) = self.retrying(id)
            && done == 0
        {
            return tf!(
                "正在重试（{retry}/{total}）…",
                retry,
                total = CONNECT_RETRIES
            )
            .into();
        }
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

    /// 正在连的这一次连不上、在第几次重试了（见 [`CONNECT_RETRIES`]）；没在连、没重试的是 None。
    pub fn retrying(&self, id: &str) -> Option<u32> {
        let connecting = matches!(self.conns.get(id).map(|c| c.peek()), Some(None));
        connecting.then(|| self.retrying.get(id).copied()).flatten()
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

    /// 最近一次连这台失败在登录阶段的哪一步；没失败、登录上以后才失败的是 None。
    pub fn last_login_failure(&self, id: &str) -> Option<fr_channel::LoginFailure> {
        self.login_failures.get(id)?.lock().unwrap().clone()
    }

    /// 最近一次连这台是不是没连上（连上以后又断开的不算）。
    pub fn connect_failed(&self, id: &str) -> bool {
        matches!(self.conns.get(id).map(|c| c.peek()), Some(Some(Err(_))))
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
                Ok(conn) => {
                    this.learn_machine(LOCAL, &conn.hello.machine, cx);
                    this.machine_connected(LOCAL, conn, cx)
                }
                Err(_) => this.local_unavailable(cx),
            })
            .ok();
        })
        .detach();
        connecting
    }

    /// 连接服务器；已经连上或正在连就沿用。返回等连接结果的 future。
    /// `id` 可以是经别的电脑连的服务器（`A 的 id>B 的 id`，见 `machines`）：先连那台，再请它的 kuaid 转接。
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
        // 经跳板机的：后台线程每登录上一台报一次；连不上要重试时也报。这边记下来、刷新列表上的那句话
        self.stages.remove(id);
        self.retrying.remove(id);
        let failure_slot = FailureSlot::default();
        self.login_failures
            .insert(id.to_string(), failure_slot.clone());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if let Some(old) = self.conn_dropped.insert(id.to_string(), dropped.clone()) {
            old.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let is_dropped = {
            let dropped = dropped.clone();
            move || dropped.load(std::sync::atomic::Ordering::Relaxed)
        };
        let (progress_tx, mut progress_rx) = futures::channel::mpsc::unbounded::<Progress>();
        let retry_tx = progress_tx.clone();
        let on_stage =
            Arc::new(move |done: usize| drop(progress_tx.unbounded_send(Progress::Stage(done))))
                as fr_channel::StageHandler;
        let stage_id = id.to_string();
        let progress_dropped = is_dropped.clone();
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(progress) = progress_rx.next().await {
                // 这次连接不要了（改了、删了主机）：它的进度不写，免得盖掉同一台的新连接
                if progress_dropped() {
                    return;
                }
                let updated = this.update(cx, |this, cx| {
                    match progress {
                        Progress::Stage(done) => {
                            this.stages.insert(stage_id.clone(), done);
                        }
                        // 重试从第一台跳板机连起
                        Progress::Retry(retry) => {
                            this.stages.remove(&stage_id);
                            this.retrying.insert(stage_id.clone(), retry);
                        }
                    }
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
                // 自己在重连的（不许问人）：转接的那台也照这样连，那边的 ssh 也不许问（见
                // `ConnectOptions::no_prompt`）
                let via_connecting = self.connect_with(via, no_prompt, cx);
                let server = server.to_string();
                let slot = failure_slot.clone();
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
                            no_prompt,
                            ..ConnectOptions::default()
                        };
                        Connection::connect_via(&via, &server, &target.destination, &options)
                            .map(Arc::new)
                            .map_err(|err| {
                                // 登录阶段在哪一步失败的、要不要人来处理（后台重连看它停不停）
                                *slot.lock().unwrap() = fr_channel::login_failure(&err).cloned();
                                failed(err)
                            })
                    })
                    .shared()
            }
            None => {
                let server = self.server(id).cloned();
                let route: Vec<Server> = self.settings.route(id).into_iter().cloned().collect();
                let slot = failure_slot.clone();
                let executor = cx.background_executor().clone();
                cx.background_executor()
                    .spawn(async move {
                        let server =
                            server.ok_or_else(|| SharedString::from(tr("未找到该主机的设置")))?;
                        let mut options = connect_options(&server);
                        options.jumps = route.iter().map(jump_options).collect();
                        options.on_stage = (!route.is_empty()).then_some(on_stage);
                        options.no_prompt = no_prompt;
                        let mut retry = 0;
                        loop {
                            // 等的时候主机被改了、删了：不再按旧的设置连
                            if is_dropped() {
                                return Err(SharedString::from(tr("主机设置已更改，已断开")));
                            }
                            let started = Instant::now();
                            let err = match Connection::connect(&server.destination, &options) {
                                Ok(conn) => return Ok(Arc::new(conn)),
                                Err(err) => err,
                            };
                            let failure = fr_channel::login_failure(&err).cloned();
                            // 登录阶段连不上（超时、连接被拒绝、对方断开）的等一下再试。登录被拒绝、
                            // 主机密钥不对、取消了输入的再试也一样，还会再弹窗；登录上以后才出的错
                            // （kuaid 起不来、git 太旧）、一次就卡了很久的也不试
                            let again = retry < CONNECT_RETRIES
                                && started.elapsed() < CONNECT_RETRY_SLOW
                                && failure.as_ref().is_some_and(|f| !f.needs_user());
                            if !again {
                                // 登录阶段在哪一步失败的，留给诊断
                                *slot.lock().unwrap() = failure;
                                return Err(SharedString::from(format!("{err:#}")));
                            }
                            retry += 1;
                            log::info!(
                                "连接 {} 失败，第 {retry} 次重试：{err:#}",
                                server.destination
                            );
                            drop(retry_tx.unbounded_send(Progress::Retry(retry)));
                            executor.timer(CONNECT_RETRY_GAP).await;
                        }
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
                // 最近的错误里记一条（诊断用）
                if let Some(Err(err)) = this.conns.get(&id).and_then(|c| c.peek()) {
                    let name = this.machine_name(&id).to_string();
                    crate::diagnose::record_error(&name, err);
                    // 连不上（断网、电脑刚醒、启动时还没网）：过一会儿自己再连，连上以后打开着的仓库、
                    // 终端跟着接上。登录被拒绝这类要人来处理的不再试
                    if this.worth_retrying(&id) {
                        this.schedule_reconnect(&id, cx);
                    }
                }
                // 连上了：记下它是哪台；这台服务器的映射换到新连接上，设了自动映射的映射上；
                // 订阅它的仓库列表和工作区
                if let Some(Ok(conn)) = this.conns.get(&id).and_then(|c| c.peek()) {
                    let conn = conn.clone();
                    this.learn_machine(&id, &conn.hello.machine, cx);
                    // 浏览器里开着的这台主机上的网页：之后的请求经新连接读
                    crate::web_serve::connected(&id, &Channel::Remote(conn.clone()));
                    // 经别的电脑连的这台认出来就是本机或者已有的一台：不单独列，也不订阅
                    if this.ids().duplicate(&id) {
                        this.note_duplicate(&id, cx);
                    } else {
                        if split_relay(&id).is_none() {
                            this.mappings_connected(&id, &conn, cx);
                        }
                        this.machine_connected(&id, conn, cx);
                    }
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
        // 它是哪台电脑也不在表单里：地址、跳板机没改就还是那台，改了等下次连上再认
        let same_target = old.destination == server.destination && old.jump == server.jump;
        let machine = old.machine.take().filter(|_| same_target);
        *old = server;
        old.last_dir = last_dir;
        old.mappings = mappings;
        old.machine = machine;
        if !same_target {
            // 可能换了一台电脑：原来那台的东西都不能接着用（经它连的那几台也是），映射着的断开，
            // 盘上打开的文件不会再读写到新地址那台的同名文件上
            self.forget_target(&id, true, cx);
            self.unmount_server(&id, cx);
        }
        if reconnect {
            self.drop_conns(|key| *key == id, cx);
        }
        self.save();
        self.push_servers(cx);
        cx.notify();
        reconnect
    }

    /// 键是 `key` 的电脑换了（改了地址、跳板机，或者删掉了）：原来那台握手时报的标识、读到的仓库
    /// 列表和工作区、接好的连接和订阅都是原来那台的，丢掉，等连上新的那台再来。`and_relayed` 为真时
    /// 经它连的那几台（`key>…`）也一样。原来只清标识，界面照旧显示原来那台的仓库，改动也还交给它。
    pub(crate) fn forget_target(&mut self, key: &str, and_relayed: bool, cx: &mut Context<Self>) {
        let relayed = format!("{key}>");
        let gone = |k: &String| k == key || (and_relayed && k.starts_with(&relayed));
        self.machine_ids.retain(|k, _| !gone(k));
        self.machines.retain(|k, _| !gone(k));
        self.links.retain(|k, _| !gone(k));
        self.watches.retain(|k, _| !gone(k));
        // 浏览器里打开过的那几台上的网页：旧地址作废，不然读到的是新那台上同一路径的仓库
        crate::web_serve::forget_servers(|server| {
            server == key || (and_relayed && server.starts_with(&relayed))
        });
        self.drop_conns(gone, cx);
    }

    /// 丢掉键满足 `gone` 的连接，并且断开（[`Connection::close`]）：打开着的仓库、终端手里还拿着
    /// 它的话，原来一直连着原来那台，现在看到断开，按新的设置重连。还在连的，连上以后断开。
    fn drop_conns(&mut self, gone: impl Fn(&String) -> bool, cx: &mut Context<Self>) {
        // 还在连、还在重试的也停下（见 `conn_dropped`）
        for (_, dropped) in self.conn_dropped.extract_if(|key, _| gone(key)) {
            dropped.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let keys: Vec<String> = self.conns.keys().filter(|key| gone(key)).cloned().collect();
        for key in keys {
            let Some(connecting) = self.conns.remove(&key) else {
                continue;
            };
            let reason = || tr("主机设置已更改，已断开").to_string();
            match connecting.peek() {
                Some(Ok(conn)) => conn.close(reason()),
                Some(Err(_)) => {}
                None => cx
                    .background_executor()
                    .spawn(async move {
                        if let Ok(conn) = connecting.await {
                            conn.close(reason());
                        }
                    })
                    .detach(),
            }
        }
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
        crate::web_serve::forget_servers(|server| server == id || server.starts_with(&relayed));
        self.drop_conns(gone, cx);
        self.links.retain(|key, _| !gone(key));
        self.machines.retain(|key, _| !gone(key));
        self.pending.retain(|key, _| !gone(key));
        self.reconnects.retain(|key, _| !gone(key));
        self.machine_ids.retain(|key, _| !gone(key));
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

    /// 改设置里的几项（语言、主题、托盘……）并存盘。
    pub fn update_settings(&mut self, cx: &mut Context<Self>, edit: impl FnOnce(&mut Settings)) {
        edit(&mut self.settings);
        self.settings.normalize();
        // 终端的设置：开着的终端马上跟着变
        crate::terminal::set_prefs(&self.settings.terminal, cx);
        crate::presets::sync(&self.settings, cx);
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

    /// 改会话页记的东西（固定的终端和仓库、收起的工作区），稍后存盘。
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

    /// 存设置。存不了时记日志，在界面上说一声（主窗口收着 [`save_failures`] 弹通知）：原来只写
    /// 日志，设置页上改了照样生效，重启以后全没了，用户不知道。同一个原因连着出只说一次；读设置时
    /// 就出了问题、这次不写的，启动时已经说过了，不再说。
    pub(crate) fn save(&self) {
        let Err(err) = settings::save(&self.settings) else {
            return;
        };
        let text = format!("{err:#}");
        log::error!("{text}");
        if settings::read_only() {
            return;
        }
        thread_local! {
            static TOLD: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
        }
        if TOLD.with(|told| told.replace(Some(text.clone())).as_deref() == Some(text.as_str())) {
            return;
        }
        if let Some(tx) = SAVE_FAILED.get() {
            let _ = tx.unbounded_send(text);
        }
    }
}

/// 登录这台服务器用的选项：私钥、密码登录、存着的密码或口令。不含跳板机（见 [`AppState::connect`]）。
pub(crate) fn connect_options(server: &Server) -> ConnectOptions {
    ConnectOptions {
        forward_agent: server.forward_agent,
        identity_file: server.key_file().map(str::to_string),
        password_auth: server.auth == Auth::Password,
        secret: crate::secrets::for_server(server),
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
        secret: crate::secrets::for_server(server),
    }
}
