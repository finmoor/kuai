//! 把服务器上的目录映射到本机（盘符或文件夹）：哪些映射着、怎么映射和断开、服务器上的路径在
//! 本机怎么写，添加和修改映射的对话框，以及各处菜单里共用的那几项。底下是 `fr-mount`（Windows 上
//! 是 WinFsp，Linux 上是 FUSE）。Linux 上没有盘符，只能映射成文件夹；要装的是 fuse3（系统的包，
//! 由用户自己装，这里只说怎么装）。
//!
//! 一台服务器可以有多条映射（`settings::Mapping`），配置跟着服务器存在设置里。映射在 Kuai
//! 进程里：退出 Kuai 就都没了，关窗口收到托盘时还在。走的是那台服务器现有的连接，不另外登录；
//! 连接断了盘还在、读写出错，重新连上以后换到新的连接上。
//!
//! 写入是先答应、后落到服务器的（见 `fr-mount`）：落不下去时写的那个程序会收到错误，这里再弹
//! 一条通知（[`Notice`]，主窗口负责弹）。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use fr_channel::Channel;
use fr_core::i18n::tr;
use fr_core::tf;
use futures::StreamExt as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::radio::Radio;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::browser::{BrowserEvent, RemoteBrowser};
use crate::settings::{Mapping, Server, base_name};
use crate::ui;

/// WinFsp 的安装包在 winget 里叫什么；没有 winget 时打开的下载页面。
const WINFSP_PACKAGE: &str = "WinFsp.WinFsp";
const WINFSP_DOWNLOAD: &str = "https://winfsp.dev/rel/";

/// 要弹给用户看的一句话（主窗口订阅）。
pub enum Notice {
    Info(SharedString),
    Warning(SharedString),
    Error(SharedString),
    /// 用户要映射，可是没装 WinFsp：提示里带「安装 WinFsp」按钮
    NeedWinFsp,
    /// winget 没装成：提示里带「打开下载页面」按钮（网址写在文字里点不了）
    InstallFailed,
}

/// 打开 WinFsp 的下载页面（没装成、没有 winget 时手动装）。
pub fn open_winfsp_download() -> Result<(), String> {
    crate::shell::open_url(WINFSP_DOWNLOAD)
}

impl EventEmitter<Notice> for AppState {}

/// 退出时最多等映射断开这么久。
const UNMOUNT_WAIT: Duration = Duration::from_secs(30);

enum Slot {
    /// 正在映射（在等连接，或者在挂）。带着这一次的编号：期间改了配置、断开又映射，旧的那次
    /// 做完时编号对不上，结果丢掉（原来只看是不是「正在映射」，旧配置的结果会被当成新的挂上）
    Busy(u64),
    /// 映射着；`machine` 是挂上时那台电脑的标识（握手时报的），`attempt` 是挂上的那一次的编号
    /// （盘在外面被卸掉时用来认是不是还是这一个，见 [`AppState::mount_ended`]）
    Live {
        mounted: fr_mount::Mounted,
        machine: String,
        attempt: u64,
    },
    Failed(SharedString),
}

/// 一条映射现在的情况。
#[derive(Clone, PartialEq)]
pub enum MountStatus {
    /// 没有映射着
    Off,
    Busy,
    Live,
    Failed(SharedString),
}

/// 各条映射的情况（按映射的 id），和 WinFsp 装没装。
pub struct Mounts {
    slots: HashMap<String, Slot>,
    installed: bool,
    installing: bool,
    /// 上一次映射用的编号（见 [`Slot::Busy`]）
    attempts: u64,
}

impl Mounts {
    pub fn new() -> Self {
        Self {
            slots: HashMap::new(),
            installed: winfsp_present(),
            installing: false,
            attempts: 0,
        }
    }
}

/// 装没装 WinFsp。`FR_NO_WINFSP` 是测试用的：设了就当作没装（看「需要安装」的界面）；设成一个
/// 文件的路径时，那个文件在就当作没装（假的 winget 删掉它，就像装上了，见 `FR_WINGET`）。
fn winfsp_present() -> bool {
    if let Some(flag) = std::env::var_os("FR_NO_WINFSP")
        && (flag == "1" || std::path::Path::new(&flag).exists())
    {
        return false;
    }
    fr_mount::installed()
}

/// 新映射默认放在哪个文件夹：`<用户目录>\Kuai\<服务器名>-<目录名>`（Linux 上是
/// `~/Kuai/<服务器名>-<目录名>`）。
fn default_folder(server: &str, remote: &str) -> String {
    let home = match cfg!(windows) {
        true => std::env::var("USERPROFILE").unwrap_or_else(|_| "C:".into()),
        false => std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
    };
    let name: String = match base_name(remote) {
        "" => server.to_string(),
        dir => format!("{server}-{dir}"),
    };
    let name: String = name
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c => c,
        })
        .collect();
    match cfg!(windows) {
        true => format!("{home}\\Kuai\\{name}"),
        false => format!("{}/Kuai/{name}", home.trim_end_matches('/')),
    }
}

/// 没装映射要的东西时怎么说：Windows 上是 WinFsp（有安装按钮），Linux 上是 fuse3（自己装）。
fn need_install_text() -> &'static str {
    match cfg!(windows) {
        true => tr("映射需要 WinFsp，安装时需管理员权限（仅一次）。"),
        false => tr(
            "映射需要 fuse3，请先安装：sudo apt install fuse3（Fedora 为 sudo dnf install fuse3）。",
        ),
    }
}

/// 资源管理器里盘符映射显示的名称。和在资源管理器里给网络驱动器重命名一样，写在
/// `HKCU\…\Explorer\MountPoints2\##kuai#<共享名>` 的 `_LabelFromReg` 里，显示成「名称 (K:)」
/// （2026-11-15 实测）。共享名是网络路径里实际的那个（`unc`，见 [`fr_mount::unc_share`]），
/// 不是带中文的原名，不然写错了地方。`name` 为 `None` 时删掉这一项，恢复成「共享名 (\\kuai) (K:)」。
/// 盘符映射着的话，通知资源管理器刷新那个盘。只是显示，出错只记日志。
#[cfg(windows)]
fn set_explorer_label(unc: &str, name: Option<&str>, drive: &str) {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_WRITE};
    let path =
        format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\MountPoints2\##kuai#{unc}");
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let result = match name {
        Some(name) => hkcu
            .create_subkey(&path)
            .and_then(|(key, _)| key.set_value("_LabelFromReg", &name.to_string())),
        None => match hkcu.open_subkey_with_flags(&path, KEY_WRITE) {
            Ok(key) => match key.delete_value("_LabelFromReg") {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
                _ => Ok(()),
            },
            Err(_) => Ok(()),
        },
    };
    if let Err(err) = result {
        log::warn!("设置资源管理器里 {drive} 的名称失败：{err}");
        return;
    }
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHChangeNotify(
            event: i32,
            flags: u32,
            item1: *const std::ffi::c_void,
            item2: *const std::ffi::c_void,
        );
    }
    const SHCNE_UPDATEITEM: i32 = 0x0000_2000;
    const SHCNF_PATHW: u32 = 0x0005;
    let root: Vec<u16> = format!("{drive}\\").encode_utf16().chain([0]).collect();
    // SAFETY：root 是以 0 结尾的宽字符串，调用期间一直有效
    unsafe {
        SHChangeNotify(
            SHCNE_UPDATEITEM,
            SHCNF_PATHW,
            root.as_ptr().cast(),
            std::ptr::null(),
        )
    };
}

#[cfg(not(windows))]
fn set_explorer_label(_: &str, _: Option<&str>, _: &str) {}

impl AppState {
    pub fn winfsp_installed(&self) -> bool {
        self.mounts.installed
    }

    pub fn winfsp_installing(&self) -> bool {
        self.mounts.installing
    }

    /// 再看一眼 WinFsp 装没装（用户可能自己装了）；刚装上的话把该自动映射的映射上。
    pub fn recheck_winfsp(&mut self, cx: &mut Context<Self>) {
        let now = winfsp_present();
        if now != self.mounts.installed {
            self.mounts.installed = now;
            if now {
                self.mount_all_ready(cx);
            }
            cx.notify();
        }
    }

    /// 安装 WinFsp：交给 winget（它核对安装包、弹一次管理员确认）；没有 winget 就打开下载页面。
    pub fn install_winfsp(&mut self, cx: &mut Context<Self>) {
        if self.mounts.installing {
            return;
        }
        self.mounts.installing = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let started = cx
                .background_executor()
                .spawn(async { start_winget() })
                .await;
            let result = match started {
                Ok(child) => {
                    // winget 起来了，接着会弹出系统的管理员确认：先说一声是怎么回事。
                    // 没有 winget 时不说，免得让人等一个不会弹出来的窗口
                    this.update(cx, |_, cx| {
                        cx.emit(Notice::Info(
                            tr("正在安装 WinFsp，请在弹出的窗口中允许。").into(),
                        ))
                    })
                    .ok();
                    let waiting = async move { wait_winget(child) };
                    cx.background_executor().spawn(waiting).await
                }
                Err(err) => Err(err),
            };
            this.update(cx, |this, cx| {
                this.mounts.installing = false;
                this.mounts.installed = winfsp_present();
                let notice = match (this.mounts.installed, result) {
                    (true, _) => {
                        this.mount_all_ready(cx);
                        Notice::Info(tr("WinFsp 已安装。").into())
                    }
                    (false, Err(Winget::Missing)) => {
                        let _ = open_winfsp_download();
                        Notice::Info(
                            tr("未找到 winget，已打开 WinFsp 下载页。安装后即可映射。").into(),
                        )
                    }
                    (false, _) => Notice::InstallFailed,
                };
                cx.emit(notice);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 找一条映射：它在哪台服务器上。
    pub fn mapping(&self, id: &str) -> Option<(&Server, &Mapping)> {
        self.settings.servers.iter().find_map(|server| {
            let mapping = server.mappings.iter().find(|m| m.id == id)?;
            Some((server, mapping))
        })
    }

    pub fn mount_status(&self, id: &str) -> MountStatus {
        match self.mounts.slots.get(id) {
            None => MountStatus::Off,
            Some(Slot::Busy(_)) => MountStatus::Busy,
            Some(Slot::Live { .. }) => MountStatus::Live,
            Some(Slot::Failed(err)) => MountStatus::Failed(err.clone()),
        }
    }

    fn is_live(&self, id: &str) -> bool {
        matches!(self.mounts.slots.get(id), Some(Slot::Live { .. }))
    }

    /// 映射着的盘自己没了（Linux 上在文件管理器里点了卸载、`fusermount3 -u`）：还是 `attempt` 那一次
    /// 挂的就清掉，界面上回到没映射着，可以直接再映射（原来一直显示映射着，再映射也不做）。我们
    /// 自己断开的先拿掉了槽，这里不管。
    fn mount_ended(&mut self, id: &str, attempt: u64, cx: &mut Context<Self>) {
        if !matches!(self.mounts.slots.get(id), Some(Slot::Live { attempt: now, .. }) if *now == attempt)
        {
            return;
        }
        let Some(Slot::Live { mounted, .. }) = self.mounts.slots.remove(id) else {
            return;
        };
        let place = mounted.place().to_string();
        log::info!("映射到 {place} 的目录在 Kuai 以外被卸载");
        cx.background_executor()
            .spawn(async move { drop(mounted) })
            .detach();
        cx.emit(Notice::Info(
            tf!("{place} 已在 Kuai 以外卸载，映射已断开。", place = &place).into(),
        ));
        cx.notify();
    }

    /// 这台服务器上包含 `path` 的映射（配置了就算，不管现在映射着没有）；有几条时取最里面的。
    pub fn covering(&self, server: &str, path: &str) -> Option<&Mapping> {
        self.server(server)?
            .mappings
            .iter()
            .filter(|mapping| mapping.local_path(path).is_some())
            .max_by_key(|mapping| mapping.remote.len())
    }

    /// 服务器上的 `path` 在本机的路径：要有一条正映射着的映射包含它。
    pub fn local_path(&self, server: &str, path: &str) -> Option<String> {
        self.server(server)?
            .mappings
            .iter()
            .filter(|mapping| self.is_live(&mapping.id))
            .filter_map(|mapping| Some((mapping.remote.len(), mapping.local_path(path)?)))
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, local)| local)
    }

    /// 映射出来的盘上，别的程序还开着几个文件。退出前问一下用。
    pub fn mapped_open_files(&self) -> usize {
        self.mounts
            .slots
            .values()
            .map(|slot| match slot {
                Slot::Live { mounted, .. } => mounted.open_files(),
                _ => 0,
            })
            .sum()
    }

    /// 加一条映射，或者改一条（`mapping.id` 空着是加）。存下来并马上映射；返回它的 id。
    pub fn save_mapping(
        &mut self,
        server: &str,
        mut mapping: Mapping,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if mapping.id.is_empty() {
            let millis = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis());
            mapping.id = format!("m{millis}");
        }
        let id = mapping.id.clone();
        // 资源管理器里原来的名称：位置变了、名称改了、去掉了，先撤掉
        let before = self
            .mapping(&id)
            .map(|(server, old)| (self.settings.explorer_name(server, old), old.local.clone()));
        let entry = self.settings.servers.iter_mut().find(|s| s.id == server)?;
        // 只改了「自动映射」、名称：不用重新映射
        let same_place = |old: &Mapping| {
            old.remote == mapping.remote
                && old.local == mapping.local
                && old.read_only == mapping.read_only
        };
        let remount = match entry.mappings.iter_mut().find(|m| m.id == id) {
            Some(old) => {
                let changed = !same_place(old);
                *old = mapping;
                changed
            }
            None => {
                entry.mappings.push(mapping);
                true
            }
        };
        let after = self
            .mapping(&id)
            .map(|(server, now)| (self.settings.explorer_name(server, now), now.local.clone()));
        if let Some(((unc, Some(_)), local)) = &before
            && (remount || before.as_ref().map(|b| &b.0) != after.as_ref().map(|a| &a.0))
        {
            set_explorer_label(unc, None, local);
        }
        // 只改了名称、正映射着：马上换上新名称（卷标要到下次映射时才换，资源管理器看的是这一项）
        if !remount
            && self.is_live(&id)
            && let Some(((unc, Some(label)), local)) = &after
        {
            set_explorer_label(unc, Some(label), local);
        }
        self.save();
        // 没装 WinFsp：存下就好，对话框那边说「安装后生效」
        if (remount || !self.is_live(&id)) && self.mounts.installed {
            let closing = self.unmount_mapping(&id, cx);
            let again = id.clone();
            cx.spawn(async move |this, cx| {
                closing.await;
                this.update(cx, |this, cx| this.mount_mapping(&again, cx))
                    .ok();
            })
            .detach();
        }
        cx.notify();
        Some(id)
    }

    /// 取消一条映射：断开，并从设置里删掉。服务器上的东西不动。
    pub fn remove_mapping(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some((server, mapping)) = self.mapping(id)
            && let (unc, Some(_)) = self.settings.explorer_name(server, mapping)
        {
            set_explorer_label(&unc, None, &mapping.local);
        }
        self.unmount_mapping(id, cx).detach();
        for server in &mut self.settings.servers {
            server.mappings.retain(|mapping| mapping.id != id);
        }
        self.save();
        cx.notify();
    }

    /// 断开一条映射（配置留着）。盘上在路上的写入落地以后才真的断开，返回的任务到那时结束。
    pub fn unmount_mapping(&mut self, id: &str, cx: &mut Context<Self>) -> Task<()> {
        cx.notify();
        match self.mounts.slots.remove(id) {
            Some(Slot::Live { mounted, .. }) => {
                cx.background_executor().spawn(async move { drop(mounted) })
            }
            _ => Task::ready(()),
        }
    }

    /// 退出前：全部断开，等在路上的写入落地。在另一个线程里断开，最多等 [`UNMOUNT_WAIT`]：主机上
    /// 的文件操作卡住时（SSH 照样通着，比如挂死的网络盘），盘上的请求要到超时才回来，界面不能一直
    /// 卡在这里。没等完就不等了：进程退出时盘跟着没了，还没落地的写入就丢了。
    pub fn unmount_all(&mut self) {
        let mounted: Vec<fr_mount::Mounted> = self
            .mounts
            .slots
            .drain()
            .filter_map(|(_, slot)| match slot {
                Slot::Live { mounted, .. } => Some(mounted),
                _ => None,
            })
            .collect();
        if mounted.is_empty() {
            return;
        }
        let (done, finished) = std::sync::mpsc::channel();
        let unmounting = std::thread::Builder::new()
            .name("unmount".into())
            .spawn(move || {
                drop(mounted);
                let _ = done.send(());
            });
        if unmounting.is_ok() && finished.recv_timeout(UNMOUNT_WAIT).is_err() {
            let secs = UNMOUNT_WAIT.as_secs();
            log::warn!("退出时映射在 {secs} 秒内没有断开完，还没落到主机上的写入可能丢了");
        }
    }

    /// 一台服务器被移除了：它的映射都断开。
    pub(crate) fn unmount_server(&mut self, server: &str, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .server(server)
            .map(|server| server.mappings.iter().map(|m| m.id.clone()).collect())
            .unwrap_or_default();
        for id in ids {
            self.unmount_mapping(&id, cx).detach();
        }
    }

    /// 连上了一台服务器：映射着的换到新的连接上，设了自动映射的映射上。连上的不是挂上时那台电脑
    /// （改了地址）：盘上打开着的文件、读到的内容都是原来那台的，不能接着用新连接读写，断开重新映射。
    pub(crate) fn mappings_connected(
        &mut self,
        server: &str,
        conn: &std::sync::Arc<fr_channel::Connection>,
        cx: &mut Context<Self>,
    ) {
        let mappings: Vec<Mapping> = self
            .server(server)
            .map(|server| server.mappings.clone())
            .unwrap_or_default();
        for mapping in mappings {
            match self.mounts.slots.get(&mapping.id) {
                Some(Slot::Live {
                    mounted, machine, ..
                }) if *machine == conn.hello.machine => {
                    mounted.set_channel(Some(Channel::Remote(conn.clone())));
                }
                Some(Slot::Live { .. }) => {
                    log::info!("映射 {} 连上的是另一台电脑，重新映射", mapping.remote);
                    let closing = self.unmount_mapping(&mapping.id, cx);
                    let id = mapping.id.clone();
                    cx.spawn(async move |this, cx| {
                        closing.await;
                        this.update(cx, |this, cx| this.start_mount(&id, false, cx))
                            .ok();
                    })
                    .detach();
                }
                Some(Slot::Busy(_)) => {}
                // 上次没成的，这次连上了再试
                Some(Slot::Failed(_)) | None if mapping.auto && self.mounts.installed => {
                    self.start_mount(&mapping.id, false, cx);
                }
                _ => {}
            }
        }
    }

    /// 刚装上 WinFsp：已经连着的服务器上，设了自动映射的都映射上（是用户点的安装，映射上了要说一声）。
    fn mount_all_ready(&mut self, cx: &mut Context<Self>) {
        let ready: Vec<String> = self
            .settings
            .servers
            .iter()
            .filter(|server| {
                matches!(
                    self.status(&server.id),
                    crate::app_state::ConnStatus::Ready(_)
                )
            })
            .flat_map(|server| server.mappings.iter())
            .filter(|mapping| mapping.auto)
            .map(|mapping| mapping.id.clone())
            .collect();
        for id in ready {
            self.mount_mapping(&id, cx);
        }
    }

    /// 用户点了「立即映射」、改了映射：映射上以后说一声。没装 WinFsp 时提示安装（带按钮），
    /// 不然点了什么也不发生。
    pub fn mount_mapping(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.mounts.installed {
            cx.emit(Notice::NeedWinFsp);
            return;
        }
        self.start_mount(id, true, cx);
    }

    /// 把一条映射映射上：先等这台服务器连上。已经映射着、正在映射的不重复做。
    /// `announce`：成了以后提示一下。连上服务器后自动映射的不提示（每次连上都弹，打扰人），
    /// 映射不上的照样提示。
    fn start_mount(&mut self, id: &str, announce: bool, cx: &mut Context<Self>) {
        let Some((server, mapping)) = self.mapping(id) else {
            return;
        };
        if matches!(
            self.mounts.slots.get(id),
            Some(Slot::Busy(_)) | Some(Slot::Live { .. })
        ) {
            return;
        }
        // 调用的地方都先看过；没装时这条映射就是「没映射着」，标记上写「未安装 WinFsp」
        if !self.mounts.installed {
            return;
        }
        let (server, mapping) = (server.clone(), mapping.clone());
        let share = self.settings.share_name(&server, &mapping);
        let (unc, explorer_label) = self.settings.explorer_name(&server, &mapping);
        self.mounts.attempts += 1;
        let attempt = self.mounts.attempts;
        self.mounts
            .slots
            .insert(id.to_string(), Slot::Busy(attempt));
        cx.notify();
        let connecting = self.connect(&server.id, cx);
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let (errors_tx, mut errors) = futures::channel::mpsc::unbounded::<String>();
            let (remote, local) = (mapping.remote.clone(), mapping.local_root());
            let named = mapping.drive_name().map(str::to_string);
            let mut machine = String::new();
            let mut used = None;
            let mounting = match connecting.await {
                Ok(conn) => {
                    machine = conn.hello.machine.clone();
                    used = Some(conn.clone());
                    let options = fr_mount::Options {
                        root: mapping.remote.clone(),
                        place: mapping.local.clone(),
                        read_only: mapping.read_only,
                        windows: conn.hello.os == "windows",
                        // 卷标：起了名称用名称（命令行的 `vol`、一些程序看这个）
                        label: named.clone().unwrap_or_else(|| share.clone()),
                        share,
                    };
                    cx.background_executor()
                        .spawn(async move {
                            fr_mount::mount(Channel::Remote(conn), options, move |text| {
                                drop(errors_tx.unbounded_send(text))
                            })
                            .map_err(|err| SharedString::from(format!("{err:#}")))
                        })
                        .await
                }
                Err(err) => Err(err),
            };
            let updated = this.update(cx, |this, cx| {
                // 这期间这条映射被删了、断开了，或者又开始了新的一次：不要了
                if !matches!(this.mounts.slots.get(&id), Some(Slot::Busy(now)) if *now == attempt) {
                    if let Ok(mounted) = mounting {
                        cx.background_executor()
                            .spawn(async move { drop(mounted) })
                            .detach();
                    }
                    return;
                }
                // 挂的这一阵子又连上了一次（断网后在后台连回来）：连上时只给映射着的换连接，正在
                // 映射的跳过了，这里补上。还是那台电脑就换到现在的连接；换了一台（改了地址）的
                // 不要，用现在的连接重新映射
                if let (Ok(mounted), Some(used)) = (&mounting, &used)
                    && let crate::app_state::ConnStatus::Ready(now) = this.status(&server.id)
                    && !std::sync::Arc::ptr_eq(&now, used)
                {
                    if now.hello.machine == machine {
                        mounted.set_channel(Some(Channel::Remote(now)));
                    } else {
                        log::info!("映射 {remote} 挂上时连的已经不是现在这台电脑，重新映射");
                        if let Ok(mounted) = mounting {
                            cx.background_executor()
                                .spawn(async move { drop(mounted) })
                                .detach();
                        }
                        this.mounts.slots.remove(&id);
                        this.start_mount(&id, announce, cx);
                        return;
                    }
                }
                match mounting {
                    Ok(mut mounted) => {
                        log::info!("映射 {remote} → {local}");
                        // 盘在外面被卸掉了（Linux 上在文件管理器里点了卸载）：回到没映射着
                        let ended = mounted.ended();
                        cx.spawn({
                            let id = id.clone();
                            async move |this, cx| {
                                ended.await;
                                this.update(cx, |this, cx| this.mount_ended(&id, attempt, cx))
                                    .ok();
                            }
                        })
                        .detach();
                        let live = Slot::Live {
                            mounted,
                            machine,
                            attempt,
                        };
                        this.mounts.slots.insert(id.clone(), live);
                        if let Some(label) = &explorer_label {
                            set_explorer_label(&unc, Some(label), &mapping.local);
                        }
                        if announce {
                            let text =
                                tf!("已将 {remote} 映射到 {local}", remote = &remote, local = &local);
                            cx.emit(Notice::Info(text.into()));
                        }
                        if !this.settings.mapping_tip_shown {
                            this.settings.mapping_tip_shown = true;
                            this.save();
                            cx.emit(Notice::Info(
                                tr("映射的目录适合用本机程序查看和编辑文件。在其中运行 git 或构建较慢，请在 Kuai 或主机上进行。")
                                    .into(),
                            ));
                        }
                    }
                    Err(err) => {
                        log::warn!("映射 {remote} → {local} 失败：{err}");
                        let text = tf!(
                            "映射 {remote} 失败：{err}",
                            remote = &remote,
                            err = &err
                        );
                        this.mounts.slots.insert(id.clone(), Slot::Failed(err));
                        cx.emit(Notice::Error(text.into()));
                    }
                }
                cx.notify();
            });
            if updated.is_err() {
                return;
            }
            // 写入落不到服务器上：弹出来。一下子出很多（比如磁盘满了）时不一条条都弹
            let mut last: Option<Instant> = None;
            while let Some(text) = errors.next().await {
                if last.is_some_and(|at| at.elapsed() < Duration::from_secs(3)) {
                    continue;
                }
                last = Some(Instant::now());
                if this
                    .update(cx, |_, cx| cx.emit(Notice::Error(text.into())))
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();
    }
}

enum Winget {
    Missing,
    Failed,
}

/// 启动 winget 安装 WinFsp，不等它做完（见 `wait_winget`）。`FR_WINGET` 是测试用的：换成
/// 别的程序（假的 winget）。
fn start_winget() -> Result<std::process::Child, Winget> {
    let winget = std::env::var_os("FR_WINGET").unwrap_or_else(|| "winget".into());
    let mut cmd = std::process::Command::new(winget);
    cmd.args([
        "install",
        "--id",
        WINFSP_PACKAGE,
        "--exact",
        "--accept-package-agreements",
        "--accept-source-agreements",
        "--disable-interactivity",
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(NO_WINDOW);
    }
    match cmd.spawn() {
        Ok(child) => Ok(child),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(Winget::Missing),
        Err(err) => {
            log::warn!("无法启动 winget：{err}");
            Err(Winget::Failed)
        }
    }
}

/// 等 winget 做完。
fn wait_winget(mut child: std::process::Child) -> Result<(), Winget> {
    match child.wait() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => {
            log::warn!("winget 安装 WinFsp 没成：{status}");
            Err(Winget::Failed)
        }
        Err(err) => {
            log::warn!("等待 winget 时出错：{err}");
            Err(Winget::Failed)
        }
    }
}

/// 映射的那些地方共用的提示：没装 WinFsp 时显示，带「安装 WinFsp」按钮。装了就没有。
pub fn winfsp_banner(app: &Entity<AppState>, cx: &App) -> Option<AnyElement> {
    let state = app.read(cx);
    if state.winfsp_installed() {
        return None;
    }
    let installing = state.winfsp_installing();
    let theme = cx.theme();
    let app = app.clone();
    Some(
        h_flex()
            .gap_3()
            .px_3()
            .py_2()
            .rounded(px(ui::RADIUS))
            .border_1()
            .border_color(theme.warning.opacity(0.45))
            .bg(theme.warning.opacity(0.08))
            .child(
                ui::icon(IconName::TriangleAlert)
                    .flex_none()
                    .text_color(theme.warning),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .child(need_install_text()),
            )
            .when(cfg!(windows), |banner| {
                banner.child(
                    Button::new("install-winfsp")
                        .small()
                        .primary()
                        .flex_none()
                        .label(if installing {
                            tr("正在安装…")
                        } else {
                            tr("安装 WinFsp")
                        })
                        .loading(installing)
                        .disabled(installing)
                        .on_click(move |_, _, cx| app.update(cx, |app, cx| app.install_winfsp(cx))),
                )
            })
            .into_any_element(),
    )
}

/// 一条映射后面的两个词：「自动 · 读写」；起了名称的盘符前面加上名称。
pub fn mapping_tags(mapping: &Mapping) -> String {
    let name = mapping
        .drive_name()
        .map_or(String::new(), |name| format!("{name} · "));
    format!(
        "{name}{} · {}",
        if mapping.auto {
            tr("自动")
        } else {
            tr("手动")
        },
        if mapping.read_only {
            tr("只读")
        } else {
            tr("读写")
        }
    )
}

/// 添加、修改一条映射的对话框。`remote` 是先填上的服务器目录；`edit` 有值时是改那一条。
pub fn map_dialog(
    app: Entity<AppState>,
    server: String,
    remote: String,
    edit: Option<Mapping>,
    window: &mut Window,
    cx: &mut App,
) {
    app.update(cx, |app, cx| app.recheck_winfsp(cx));
    let Some(server_entry) = app.read(cx).server(&server).cloned() else {
        return;
    };
    let editing = edit.is_some();
    let old = edit.unwrap_or_else(|| Mapping {
        remote: remote.clone(),
        auto: true,
        ..Mapping::default()
    });
    // 能选的盘符：空着的，去掉别的映射已经定下的；正在改的这条现在用的那个也算
    let taken: Vec<String> = app
        .read(cx)
        .settings
        .servers
        .iter()
        .flat_map(|s| s.mappings.iter())
        .filter(|m| m.id != old.id)
        .map(|m| m.local.to_uppercase())
        .collect();
    let mut letters: Vec<String> = fr_mount::free_drives()
        .into_iter()
        .map(|letter| format!("{letter}:"))
        .filter(|drive| !taken.contains(drive))
        .collect();
    if old.is_drive() && !letters.contains(&old.local.to_uppercase()) {
        letters.push(old.local.to_uppercase());
        letters.sort();
    }
    // 默认从 K 往后挑：前面几个字母常被移动硬盘、读卡器占
    let first = letters
        .iter()
        .find(|drive| drive.as_str() >= "K:")
        .or(letters.first())
        .cloned()
        .unwrap_or_default();
    let letters = Rc::new(letters);
    // Linux 上没有盘符，只能选文件夹
    let drives = cfg!(windows);
    let use_drive = Rc::new(Cell::new(drives && (!editing || old.is_drive())));
    let drive = Rc::new(RefCell::new(match old.is_drive() {
        true => old.local.to_uppercase(),
        false => first,
    }));
    let auto = Rc::new(Cell::new(old.auto));
    let read_only = Rc::new(Cell::new(old.read_only));
    let problem: Rc<RefCell<Option<SharedString>>> = Rc::default();

    let mut input = |value: String, placeholder: &'static str| {
        let state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        if !value.is_empty() {
            state.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        state
    };
    let remote_input = input(old.remote.clone(), tr("主机上的目录，例如 /data/project"));
    let folder_input = input(
        match old.is_drive() || !editing {
            true => default_folder(&server_entry.name, &old.remote),
            false => old.local.clone(),
        },
        tr("本机上尚不存在的文件夹"),
    );
    let name_input = input(old.name.clone(), tr("留空则按主机名和目录名显示"));
    // 打字时预览跟着变
    let subscriptions = Rc::new([
        cx.subscribe(&remote_input, |_, _: &InputEvent, cx| cx.refresh_windows()),
        cx.subscribe(&folder_input, |_, _: &InputEvent, cx| cx.refresh_windows()),
    ]);

    let first_build = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        let _keep = &subscriptions;
        if first_build.replace(false) {
            remote_input.focus_handle(cx).focus(window, cx);
        }
        let (muted, danger, border) = (
            cx.theme().muted_foreground,
            cx.theme().danger,
            cx.theme().border,
        );
        let accent = cx.theme().primary;
        let as_drive = use_drive.get();
        let read = |input: &Entity<InputState>, cx: &App| input.read(cx).value().trim().to_string();

        let submit = {
            let (app, server, old) = (app.clone(), server.clone(), old.clone());
            let (remote_input, folder_input) = (remote_input.clone(), folder_input.clone());
            let name_input = name_input.clone();
            let (use_drive, drive) = (use_drive.clone(), drive.clone());
            let (auto, read_only, problem) = (auto.clone(), read_only.clone(), problem.clone());
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let fail = |message: SharedString, window: &mut Window| {
                    *problem.borrow_mut() = Some(message);
                    window.refresh();
                };
                let remote = read(&remote_input, cx);
                let remote = match remote.len() {
                    // 根目录就是 `/`
                    1 => remote,
                    _ => remote.trim_end_matches('/').to_string(),
                };
                let absolute =
                    remote.starts_with('/') || remote.as_bytes().get(1).is_some_and(|c| *c == b':');
                if !absolute {
                    return fail(tr("请填写主机上目录的完整路径").into(), window);
                }
                let local = match use_drive.get() {
                    true => drive.borrow().clone(),
                    false => read(&folder_input, cx)
                        .trim_end_matches(['\\', '/'])
                        .to_string(),
                };
                if local.is_empty() {
                    return fail(
                        match use_drive.get() {
                            true => tr("没有可用的盘符，请改用文件夹").into(),
                            false => tr("请填写本机文件夹").into(),
                        },
                        window,
                    );
                }
                if !use_drive.get() {
                    let folder = std::path::Path::new(&local);
                    if !folder.is_absolute() {
                        return fail(tr("请填写本机文件夹的完整路径").into(), window);
                    }
                    // 这条映射自己正映射在那里的不算
                    let own = old.local.eq_ignore_ascii_case(&local);
                    let occupied = folder.exists()
                        && std::fs::read_dir(folder)
                            .map_or(true, |mut inside| inside.next().is_some());
                    if occupied && !own {
                        return fail(
                            tr("该文件夹不为空，请选择空文件夹或新文件夹").into(),
                            window,
                        );
                    }
                }
                let clash = app.read(cx).settings.servers.iter().any(|s| {
                    s.mappings
                        .iter()
                        .any(|m| m.id != old.id && m.local.eq_ignore_ascii_case(&local))
                });
                if clash {
                    return fail(tr("另一条映射已使用此位置").into(), window);
                }
                // 名称只有盘符有：映射成文件夹的，文件夹名就是它的名称
                let name = match use_drive.get() {
                    true => read(&name_input, cx),
                    false => String::new(),
                };
                if name.chars().count() > Mapping::MAX_NAME {
                    return fail(
                        tf!("名称最多 {n} 个字符", n = Mapping::MAX_NAME).into(),
                        window,
                    );
                }
                let mapping = Mapping {
                    id: old.id.clone(),
                    remote,
                    local,
                    auto: auto.get(),
                    read_only: read_only.get(),
                    name,
                };
                window.close_dialog(cx);
                app.update(cx, |app, cx| {
                    app.save_mapping(&server, mapping, cx);
                    if !app.winfsp_installed() {
                        let text = match cfg!(windows) {
                            true => tr("映射已保存，安装 WinFsp 后生效。"),
                            false => tr("映射已保存，安装 fuse3 后生效。"),
                        };
                        cx.emit(Notice::Info(text.into()));
                    }
                });
            }
        };

        let browse = {
            let (app, server, remote_input) = (app.clone(), server.clone(), remote_input.clone());
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let start = read(&remote_input, cx);
                let browser = cx.new(|cx| {
                    RemoteBrowser::new_dir_picker(app.clone(), server.clone(), start, window, cx)
                });
                let target = remote_input.clone();
                let picked = window.subscribe(&browser, cx, move |_, event, window, cx| {
                    if let BrowserEvent::Dir(path) = event {
                        window.close_dialog(cx);
                        target.update(cx, |state, cx| state.set_value(path.clone(), window, cx));
                    }
                });
                let picked = Rc::new(picked);
                window.open_dialog(cx, move |dialog, _, _| {
                    let _keep = &picked;
                    dialog
                        .title(tr("选择要映射的目录"))
                        .w(px(680.))
                        .on_ok(|_, _, _| false)
                        .child(browser.clone())
                });
            }
        };

        let pick_kind = |to: bool| {
            let use_drive = use_drive.clone();
            move |_: &bool, window: &mut Window, _: &mut App| {
                use_drive.set(to);
                window.refresh();
            }
        };
        let letter_menu = {
            let (letters, drive) = (letters.clone(), drive.clone());
            move |mut menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
                let now = drive.borrow().clone();
                if letters.is_empty() {
                    return menu.label(tr("没有可用的盘符"));
                }
                for letter in letters.iter() {
                    let (drive, value) = (drive.clone(), letter.clone());
                    menu = menu.item(
                        PopupMenuItem::new(letter.clone())
                            .checked(*letter == now)
                            .on_click(move |_, window, _| {
                                *drive.borrow_mut() = value.clone();
                                window.refresh();
                            }),
                    );
                }
                menu
            }
        };
        let toggle = |flag: &Rc<Cell<bool>>| {
            let flag = flag.clone();
            move |checked: &bool, window: &mut Window, _: &mut App| {
                flag.set(*checked);
                window.refresh();
            }
        };

        // 预览：这个目录在本机是哪里
        let remote_now = read(&remote_input, cx);
        let local_now = match as_drive {
            true => drive.borrow().clone(),
            false => read(&folder_input, cx),
        };
        let preview = Mapping {
            remote: remote_now.clone(),
            local: local_now,
            ..Mapping::default()
        };
        // 写法和服务器设置里的映射列表、标记的菜单一样：上一行服务器目录，下一行「→ 本机位置」。
        // 路径长的时候省略前面：结尾才认得出是哪个目录
        let preview_box = (!remote_now.is_empty() && !preview.local.is_empty()).then(|| {
            v_flex()
                .gap_1()
                .px_3()
                .py_2()
                .rounded(px(ui::RADIUS))
                .border_1()
                .border_color(border)
                .child(
                    ui::clipped("preview-remote", remote_now.clone())
                        .text_ellipsis_start()
                        .font_family(crate::fonts::mono())
                        .text_xs(),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .min_w_0()
                        .child(
                            ui::icon_small(IconName::ArrowRight)
                                .flex_none()
                                .text_color(muted),
                        )
                        .child(
                            ui::clipped("preview-local", preview.local_root())
                                .flex_1()
                                .text_ellipsis_start()
                                .font_family(crate::fonts::mono())
                                .text_xs()
                                .text_color(accent),
                        ),
                )
        });

        dialog
            .title(
                h_flex()
                    .gap_2()
                    .child(if editing {
                        tr("修改映射")
                    } else {
                        tr("映射到本机")
                    })
                    .child(
                        h_flex()
                            .h(px(20.))
                            .px(px(7.))
                            .gap(px(4.))
                            .rounded(px(10.))
                            .text_xs()
                            .font_weight(FontWeight::NORMAL)
                            .text_color(muted)
                            .bg(muted.opacity(0.12))
                            .child(ui::icon_small(IconName::Server))
                            .child(server_entry.name.clone()),
                    ),
            )
            .on_ok({
                let submit = submit.clone();
                move |event, window, cx| {
                    submit(event, window, cx);
                    false
                }
            })
            .w(px(520.))
            .child(
                v_flex()
                    .gap_3()
                    .children(winfsp_banner(&app, cx))
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child(tr("主机目录")))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div().flex_1().min_w_0().child(Input::new(&remote_input)),
                                    )
                                    .child(
                                        Button::new("browse").label(tr("浏览…")).on_click(browse),
                                    ),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child(tr("本机位置")))
                            .when(!drives, |row| row.child(Input::new(&folder_input)))
                            .when(drives, |row| {
                                row.child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Radio::new("kind-drive")
                                                .label(tr("盘符"))
                                                .checked(as_drive)
                                                .on_click(pick_kind(true)),
                                        )
                                        .child(
                                            Button::new("letter")
                                                .outline()
                                                .w(px(76.))
                                                .flex_none()
                                                .disabled(!as_drive)
                                                .child(
                                                    div()
                                                        .font_family(crate::fonts::mono())
                                                        .child(drive.borrow().clone()),
                                                )
                                                .dropdown_caret(true)
                                                .dropdown_menu(letter_menu),
                                        )
                                        .child(
                                            div().pl_2().child(
                                                Radio::new("kind-folder")
                                                    .label(tr("文件夹"))
                                                    .checked(!as_drive)
                                                    .on_click(pick_kind(false)),
                                            ),
                                        )
                                        .child(
                                            div().flex_1().min_w_0().child(
                                                Input::new(&folder_input).disabled(as_drive),
                                            ),
                                        ),
                                )
                            }),
                    )
                    // 资源管理器里显示的名称（「名称 (K:)」）：只有盘符有
                    .when(as_drive, |body| {
                        body.child(
                            v_flex()
                                .gap_1()
                                .child(div().text_sm().child(tr("名称")))
                                .child(Input::new(&name_input)),
                        )
                    })
                    .children(preview_box)
                    .child(
                        h_flex()
                            .gap_5()
                            .child(
                                Checkbox::new("auto")
                                    .label(tr("连接主机后自动映射"))
                                    .checked(auto.get())
                                    .on_click(toggle(&auto)),
                            )
                            .child(
                                Checkbox::new("read-only")
                                    .label(tr("只读"))
                                    .checked(read_only.get())
                                    .on_click(toggle(&read_only)),
                            ),
                    )
                    .when_some(problem.borrow().clone(), |body, found| {
                        body.child(div().text_sm().text_color(danger).child(found))
                    })
                    .child(div().text_xs().text_color(muted).child(if as_drive {
                        tr("仅在 Kuai 运行时可用。")
                    } else {
                        tr("文件夹须为空或尚不存在，仅在 Kuai 运行时可用。")
                    })),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("cancel")
                            .label(tr("取消"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("ok")
                            .primary()
                            .label(if editing { tr("保存") } else { tr("映射") })
                            .on_click(submit),
                    ),
            )
    });
}

/// 取消一条映射：盘上还有文件开着就先问。
pub fn remove_mapping(app: &Entity<AppState>, id: &str, window: &mut Window, cx: &mut App) {
    let Some((_, mapping)) = app.read(cx).mapping(id) else {
        return;
    };
    // 说盘符（`K:`，起了名称的是「名称 (K:)」）或文件夹：用户是按本机的位置认一条映射的
    let local = match mapping.drive_name() {
        Some(_) => mapping.local_label(),
        None => mapping.local.clone(),
    };
    let title = tf!("取消映射 {local}？", local = &local);
    let open = match app.read(cx).mount_status(id) {
        MountStatus::Live => app.read(cx).mounts_open_files(id),
        _ => 0,
    };
    let (app, id) = (app.clone(), id.to_string());
    let run = move |cx: &mut App| {
        app.update(cx, |app, cx| {
            app.remove_mapping(&id, cx);
            cx.emit(Notice::Info(
                tf!("已取消映射 {local}", local = &local).into(),
            ));
        })
    };
    if open == 0 {
        return run(cx);
    }
    let description = tf!(
        "其他程序在此映射中打开了 {n} 个文件，取消映射后将无法继续读写。",
        n = open
    );
    let run = Rc::new(run);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let run = run.clone();
        alert
            .title(title.clone())
            .description(description.clone())
            // 回车不执行，要明确点按钮
            .on_ok(|_, _, _| false)
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("cancel")
                            .label(tr("保留映射"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("ok")
                            .label(tr("取消映射"))
                            .with_variant(ButtonVariant::Danger)
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                run(cx);
                            }),
                    ),
            )
    });
}

impl AppState {
    /// 这一条映射出来的盘上，别的程序还开着几个文件。
    pub fn mounts_open_files(&self, id: &str) -> usize {
        match self.mounts.slots.get(id) {
            Some(Slot::Live { mounted, .. }) => mounted.open_files(),
            _ => 0,
        }
    }
}

/// 一个服务器上的路径在菜单里和映射有关的几项，接在 `menu` 后面。
///
/// - 有映射着的映射包含它：「复制本机路径」「在资源管理器中显示」（「用默认程序打开」由文件页的菜单
///   自己给，映射着的开映射盘上的，见 `RepoView::open_with_default`）；
/// - 配了映射、现在没映射着：「立即映射」；
/// - 都没有：「将此目录映射到本机…」（文件的话映射的是 `root`，一般是仓库的根目录）。
pub fn menu_items(
    mut menu: PopupMenu,
    app: &Entity<AppState>,
    server: &str,
    path: &str,
    dir: bool,
    root: &str,
    cx: &App,
) -> PopupMenu {
    // 经别的服务器连的（A>B）：本机连不到它，不能映射
    if crate::machines::split_relay(server).is_some() {
        return menu;
    }
    let state = app.read(cx);
    if let Some(local) = state.local_path(server, path) {
        let (copy, reveal) = (local.clone(), local.clone());
        menu = menu
            .item(
                PopupMenuItem::new(tr("复制本机路径")).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                }),
            )
            .item(PopupMenuItem::new(crate::shell::reveal_label()).on_click(
                move |_, window, cx| {
                    let result = crate::shell::reveal_in_explorer(&reveal);
                    crate::shell::report(result, window, cx);
                },
            ));
        return menu;
    }
    menu_map_item(menu, app, server, path, dir, root, cx)
}

/// 同 [`menu_items`]，按仓库条目给：本机的仓库什么都不加。
pub fn entry_items(
    menu: PopupMenu,
    app: &Entity<AppState>,
    entry: &crate::settings::RepoEntry,
    cx: &App,
) -> PopupMenu {
    match &entry.server {
        Some(server) => menu_items(menu, app, server, &entry.path, true, &entry.path, cx),
        None => menu,
    }
}

/// 没有本机路径时的那一项：配了映射、现在没映射着是「立即映射」，否则是「映射到本机…」。
fn menu_map_item(
    menu: PopupMenu,
    app: &Entity<AppState>,
    server: &str,
    path: &str,
    dir: bool,
    root: &str,
    cx: &App,
) -> PopupMenu {
    let state = app.read(cx);
    if let Some(mapping) = state.covering(server, path) {
        let (app, id) = (app.clone(), mapping.id.clone());
        let busy = state.mount_status(&mapping.id) == MountStatus::Busy;
        return menu.item(
            PopupMenuItem::new(tf!("立即映射到 {local}", local = mapping.local_root()))
                .disabled(busy)
                .on_click(move |_, _, cx| app.update(cx, |app, cx| app.mount_mapping(&id, cx))),
        );
    }
    let (app, server) = (app.clone(), server.to_string());
    let target = match dir {
        true => path.to_string(),
        false => root.to_string(),
    };
    menu.item(
        PopupMenuItem::new(if dir {
            tr("将此目录映射到本机…")
        } else {
            tr("映射到本机…")
        })
        .on_click(move |_, window, cx| {
            map_dialog(
                app.clone(),
                server.clone(),
                target.clone(),
                None,
                window,
                cx,
            )
        }),
    )
}

/// 一条映射现在的情况，写成跟在「自动 · 读写」后面的一个词；映射着就不写。
fn status_word(status: &MountStatus, installed: bool) -> Option<&'static str> {
    match status {
        MountStatus::Live => None,
        MountStatus::Busy => Some(tr("正在映射…")),
        MountStatus::Failed(_) => Some(tr("映射失败")),
        MountStatus::Off if !installed && cfg!(windows) => Some(tr("未安装 WinFsp")),
        MountStatus::Off if !installed => Some(tr("未安装 fuse3")),
        MountStatus::Off => Some(tr("未映射")),
    }
}

/// 服务器设置里「映射到本机」那一节：这台服务器的映射一条一行，能加、能改、能取消。
/// 加、改、取消马上生效，不等服务器设置点「保存」。
pub fn section(app: &Entity<AppState>, server: &Server, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let (muted, border, accent, danger) = (
        theme.muted_foreground,
        theme.border,
        theme.primary,
        theme.danger,
    );
    let state = app.read(cx);
    let installed = state.winfsp_installed();
    let mappings = state
        .server(&server.id)
        .map(|server| server.mappings.clone())
        .unwrap_or_default();
    let add = {
        let (app, id) = (app.clone(), server.id.clone());
        let start = server.start_dir.clone().unwrap_or_default();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            map_dialog(app.clone(), id.clone(), start.clone(), None, window, cx)
        }
    };
    let mut list = v_flex()
        .rounded(px(ui::RADIUS))
        .border_1()
        .border_color(border);
    if mappings.is_empty() {
        list = list.child(
            div()
                .px_3()
                .py_3()
                .text_sm()
                .text_color(muted)
                .child(tr("尚未映射目录。")),
        );
    }
    for (index, mapping) in mappings.iter().enumerate() {
        let status = state.mount_status(&mapping.id);
        let live = status == MountStatus::Live;
        let tags = mapping_tags(mapping);
        // 没映射上的那个词用红色，和服务器卡片上红色的标记对得上
        let word = status_word(&status, installed);
        let failed = matches!(status, MountStatus::Failed(_));
        let tip: Option<SharedString> = match &status {
            MountStatus::Failed(err) => Some(err.clone()),
            _ => None,
        };
        let edit = {
            let (app, id, mapping) = (app.clone(), server.id.clone(), mapping.clone());
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let remote = mapping.remote.clone();
                map_dialog(
                    app.clone(),
                    id.clone(),
                    remote,
                    Some(mapping.clone()),
                    window,
                    cx,
                )
            }
        };
        let remove = {
            let (app, id) = (app.clone(), mapping.id.clone());
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                remove_mapping(&app, &id, window, cx)
            }
        };
        list = list.child(
            h_flex()
                .id(("mapping-row", index))
                .h(px(48.))
                .px_3()
                .gap_3()
                .when(index > 0, |row| row.border_t_1().border_color(border))
                .child(
                    ui::icon(match mapping.is_drive() {
                        true => IconName::HardDrive,
                        false => IconName::Folder,
                    })
                    .flex_none()
                    .text_color(muted),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        // 路径长的时候省略前面：结尾才认得出是哪个目录
                        .child(
                            ui::clipped("remote", mapping.remote.clone())
                                .text_ellipsis_start()
                                .font_family(crate::fonts::mono())
                                .text_xs(),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .min_w_0()
                                .child(
                                    ui::icon_small(IconName::ArrowRight)
                                        .flex_none()
                                        .text_color(muted),
                                )
                                .child(
                                    ui::clipped("local", mapping.local_root())
                                        .flex_1()
                                        .text_ellipsis_start()
                                        .font_family(crate::fonts::mono())
                                        .text_xs()
                                        .text_color(if live { accent } else { muted }),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .id("tags")
                        .flex_none()
                        .text_xs()
                        .text_color(muted)
                        .child(tags)
                        .when_some(word, |tags, word| {
                            tags.child(
                                div()
                                    .when(failed, |word| word.text_color(danger))
                                    .child(format!(" · {word}")),
                            )
                        })
                        // 原因可能很长（带着路径）：用会折行的提示
                        .when_some(tip, |tags, tip| {
                            tags.tooltip(move |window, cx| {
                                ui::full_text_tip(tip.clone(), window, cx)
                            })
                        }),
                )
                .child(
                    Button::new("edit")
                        .ghost()
                        .xsmall()
                        .icon(ui::icon(IconName::Pencil).text_color(muted))
                        .tooltip(tr("修改"))
                        .on_click(edit),
                )
                .child(
                    Button::new("remove")
                        .ghost()
                        .xsmall()
                        .icon(ui::icon(IconName::X).text_color(muted))
                        .tooltip(tr("取消映射"))
                        .on_click(remove),
                ),
        );
    }
    v_flex()
        .gap_2()
        .pt_3()
        .border_t_1()
        .border_color(border)
        .child(
            h_flex()
                .gap_2()
                .child(ui::icon(IconName::HardDrive).flex_none())
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(tr("映射到本机")),
                )
                .child(
                    Button::new("add-mapping")
                        .small()
                        .icon(ui::icon(IconName::Plus))
                        .label(tr("添加映射…"))
                        .on_click(add),
                ),
        )
        .child(div().text_xs().text_color(muted).child(match cfg!(windows) {
            true => tr(
                "将远程主机上的目录映射为本机的盘符或文件夹，供本机程序直接打开。仅在 Kuai 运行时可用。",
            ),
            false => tr("将远程主机上的目录映射为本机的文件夹，供本机程序直接打开。仅在 Kuai 运行时可用。"),
        }))
        .children(winfsp_banner(app, cx))
        .child(list)
        .into_any_element()
}

/// 服务器卡片上的标记：一条映射一个，写本机的位置，点开是它的菜单。没映射着的是灰的。
pub fn chip(app: &Entity<AppState>, server: &Server, mapping: &Mapping, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let state = app.read(cx);
    let status = state.mount_status(&mapping.id);
    let installed = state.winfsp_installed();
    let live = status == MountStatus::Live;
    let color = match &status {
        MountStatus::Live => theme.primary,
        MountStatus::Failed(_) => theme.danger,
        _ => theme.muted_foreground,
    };
    // 标记上写盘符（起了名称的写「名称 (K:)」，和资源管理器里一样）；映射成文件夹的写文件夹名
    let label: SharedString = mapping.local_label().into();
    let label_id = SharedString::from(format!("mapping-label-{}", mapping.id));
    let (app, server_id, mapping) = (app.clone(), server.id.clone(), mapping.clone());
    let word = status_word(&status, installed);
    let failure = match &status {
        MountStatus::Failed(err) => Some(err.clone()),
        _ => None,
    };
    Button::new(SharedString::from(format!("mapping-{}", mapping.id)))
        .ghost()
        .xsmall()
        .flex_none()
        .child(
            h_flex()
                .h(px(18.))
                .px(px(6.))
                .gap(px(4.))
                .rounded(px(9.))
                .text_xs()
                .text_color(color)
                .bg(color.opacity(0.13))
                .child(
                    Icon::new(match mapping.is_drive() {
                        true => IconName::HardDrive,
                        false => IconName::Folder,
                    })
                    .size_3()
                    .text_color(color),
                )
                // 名称最长 32 个字：太长时省略，悬停看全
                .child(ui::clipped(label_id, label).max_w(px(200.))),
        )
        .dropdown_menu(move |menu, _, cx| {
            let (muted, danger) = (cx.theme().muted_foreground, cx.theme().danger);
            let (head_remote, head_local) = (mapping.remote.clone(), mapping.local_root());
            let head_failure = failure.clone();
            let mut tags = mapping_tags(&mapping);
            if let Some(word) = word {
                tags = format!("{tags} · {word}");
            }
            // 最上面一块：哪个目录映射到了哪里。路径长的时候折行，不把菜单撑得很宽；
            // 这一块点不了、没有提示，省略了就看不到全文
            let mut menu = menu.min_w(px(240.)).item(
                PopupMenuItem::element(move |_, _| {
                    let line = |text: String| {
                        div()
                            .max_w(px(360.))
                            .font_family(crate::fonts::mono())
                            .text_xs()
                            .whitespace_normal()
                            .child(text)
                    };
                    v_flex()
                        .gap(px(2.))
                        .py(px(2.))
                        .child(line(head_remote.clone()))
                        .child(line(format!("→ {head_local}")))
                        .child(div().text_xs().text_color(muted).child(tags.clone()))
                        // 没映射上的原因：和上面一样折行，不把菜单撑宽
                        .children(head_failure.clone().map(|err| {
                            div()
                                .max_w(px(360.))
                                .text_xs()
                                .text_color(danger)
                                .whitespace_normal()
                                .child(err)
                        }))
                })
                .disabled(true),
            );
            menu = menu.separator();
            if live {
                let (open, copy) = (mapping.local_root(), mapping.local_root());
                let (app, id) = (app.clone(), mapping.id.clone());
                menu = menu
                    .item(
                        PopupMenuItem::new(crate::shell::open_folder_label()).on_click(
                            move |_, window, cx| {
                                crate::shell::report(crate::shell::open_folder(&open), window, cx)
                            },
                        ),
                    )
                    .item(
                        PopupMenuItem::new(tr("复制本机路径")).on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                        }),
                    )
                    .item(PopupMenuItem::new(tr("断开")).on_click(move |_, _, cx| {
                        app.update(cx, |app, cx| app.unmount_mapping(&id, cx).detach());
                    }));
            } else if !installed && cfg!(windows) {
                // 没装 WinFsp：给出能做的那一步，而不是一个点不了的「立即映射」。Linux 上点「立即映射」
                // 会提示怎么装 fuse3
                let app = app.clone();
                let installing = app.read(cx).winfsp_installing();
                menu = menu.item(
                    PopupMenuItem::new(if installing {
                        tr("正在安装…")
                    } else {
                        tr("安装 WinFsp")
                    })
                    .disabled(installing)
                    .on_click(move |_, _, cx| app.update(cx, |app, cx| app.install_winfsp(cx))),
                );
            } else {
                let (app, id) = (app.clone(), mapping.id.clone());
                menu = menu.item(
                    PopupMenuItem::new(tr("立即映射"))
                        .disabled(status == MountStatus::Busy)
                        .on_click(move |_, _, cx| {
                            app.update(cx, |app, cx| app.mount_mapping(&id, cx))
                        }),
                );
            }
            let (edit_app, edit_server, edit) = (app.clone(), server_id.clone(), mapping.clone());
            let (remove_app, remove_id) = (app.clone(), mapping.id.clone());
            menu.separator()
                .item(
                    PopupMenuItem::new(tr("映射设置…")).on_click(move |_, window, cx| {
                        map_dialog(
                            edit_app.clone(),
                            edit_server.clone(),
                            edit.remote.clone(),
                            Some(edit.clone()),
                            window,
                            cx,
                        )
                    }),
                )
                .item(
                    PopupMenuItem::new(tr("取消映射")).on_click(move |_, window, cx| {
                        remove_mapping(&remove_app, &remove_id, window, cx)
                    }),
                )
        })
        .into_any_element()
}
