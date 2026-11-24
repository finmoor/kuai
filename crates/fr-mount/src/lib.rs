//! 把服务器（或本机）上的一个目录映射成本机的盘符或文件夹。
//!
//! 盘上的读写变成 kuaid 的 `fs_*` 请求（协议 9），走客户端平时用的那条通道，不另开连接、不要
//! 服务器装别的东西。底下是用户态文件系统：Windows 上是 WinFsp，Linux 上是 FUSE（经系统的
//! fusermount3，没装 fuse3 时 [`installed`] 是假）；别的系统上 [`installed`] 总是假，[`mount`]
//! 总是出错。
//!
//! - 盘符（`K:`，只有 Windows）挂成网络盘的样子（`\\kuai\<名字>`）：资源管理器不在上面建回收站、
//!   不建索引。
//! - Windows 上文件夹只能挂成本地磁盘的样子：WinFsp 在那个位置建一个链接，卸载时拿掉，所以这个
//!   文件夹事先不能有（空的会先删掉）。Linux 上挂在一个空文件夹上，没有就建，卸载后建的删掉。
//!
//! 映射在调用 [`mount`] 的进程里：[`Mounted`] 放掉、进程退出，盘就没了（Linux 上进程没正常退出时
//! 文件夹留在「连不上」的样子，下次映射到这里时先卸掉）。
//!
//! WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos
//! <https://github.com/winfsp/winfsp>

#[cfg(any(windows, target_os = "linux"))]
mod fs;
#[cfg(windows)]
mod winfsp;

/// 关于页、许可说明里要写的那一句。
pub const WINFSP_NOTICE: &str =
    "WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos";
pub const WINFSP_URL: &str = "https://github.com/winfsp/winfsp";

pub struct Options {
    /// 服务器上要映射的目录
    pub root: String,
    /// 本机的位置：盘符（`K:`），或者一个文件夹（事先不能有，空的会先删掉）
    pub place: String,
    /// 只读：盘上不让写，服务器上什么都不会变
    pub read_only: bool,
    /// 那边是 Windows：名字不分大小写，系统的错误码也是 Windows 的
    pub windows: bool,
    /// 盘的卷标
    pub label: String,
    /// 挂成盘符时网络路径 `\\kuai\<share>` 里的名字，资源管理器里盘的名字就是它；
    /// 同时挂着的几个不能一样。实际用的是 [`unc_share`] 换过的
    pub share: String,
}

/// 共享名在网络路径 `\\kuai\<共享名>` 里实际的样子：只留英文字母、数字和 `.`、`-`、`_`，
/// 别的（中文等）换成 `-`；最长 184 个字符，加上 `\kuai\` 正好是 WinFsp 前缀的上限 190。
/// 资源管理器在 `MountPoints2` 下记这个盘也用它（`##kuai#<共享名>`）。
pub fn unc_share(share: &str) -> String {
    let keep = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
    share
        .chars()
        .map(|c| if keep(c) { c } else { '-' })
        .take(184)
        .collect()
}

/// 这个位置是不是盘符（`K:`）。
pub fn is_drive(place: &str) -> bool {
    let bytes = place.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// 向服务器发了多少请求、传了多少字节。
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub calls: u64,
    pub down: u64,
    pub up: u64,
}

#[cfg(windows)]
pub use imp::{Mounted, free_drives, installed, mount};

#[cfg(windows)]
mod imp {
    use std::path::Path;
    use std::sync::atomic::Ordering::Relaxed;

    use anyhow::{Context as _, Result, bail};
    use fr_channel::Channel;
    use fr_core::tf;
    use futures::executor::block_on;

    use super::{Options, Stats, is_drive};
    use crate::fs::{self, Mount, Settings};
    use crate::winfsp::{self, Api, FileSystem, VolumeParams, flag, wide};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalDrives() -> u32;
    }

    /// 装没装 WinFsp。
    pub fn installed() -> bool {
        winfsp::install_dir().is_ok_and(|dir| {
            Path::new(dir.trim_end_matches('\\'))
                .join(r"bin\winfsp-x64.dll")
                .is_file()
        })
    }

    /// 还没人用的盘符，从 D 到 Z。
    pub fn free_drives() -> Vec<char> {
        // SAFETY：没有参数，只读系统状态
        let used = unsafe { GetLogicalDrives() };
        ('D'..='Z')
            .filter(|letter| used & (1 << (*letter as u32 - 'A' as u32)) == 0)
            .collect()
    }

    /// 一个映射着的目录。放掉它就卸载。
    pub struct Mounted {
        api: Api,
        file_system: *mut FileSystem,
        /// 回调里用的就是它：停了调度线程以后才能放
        mount: Box<Mount>,
        place: String,
    }

    // SAFETY：`file_system` 只在建的时候和 Drop 里碰；别的方法走的都是 `Mount` 里带锁的、原子的东西
    unsafe impl Send for Mounted {}
    unsafe impl Sync for Mounted {}

    /// 把 `options.root` 映射到 `options.place`。会等服务器回话（看那个目录在不在），别在界面线程上调。
    ///
    /// `on_error`：盘上的写入是先答应、后落到服务器的，落不下去（磁盘满、没权限）时除了让
    /// 写的那个程序出错，也从这里说一声，好让界面提示。它在别的线程上被调用。
    pub fn mount(
        channel: Channel,
        options: Options,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Mounted> {
        let api = Api::load()?;
        let Options {
            root,
            place,
            read_only,
            windows,
            label,
            share,
        } = options;
        let stat = block_on(channel.fs_stat(&root, ""))
            .with_context(|| tf!("无法读取 {path}", path = &root))?;
        if !stat.exists {
            bail!("{}", tf!("{path} 不存在", path = &root));
        }
        if !stat.dir {
            bail!("{}", tf!("{path} 不是目录", path = &root));
        }
        let drive = is_drive(&place);
        if drive {
            let letter = place.as_bytes()[0].to_ascii_uppercase() as char;
            if !free_drives().contains(&letter) {
                bail!("{}", tf!("盘符 {place} 已被占用", place = &place));
            }
        } else {
            let folder = Path::new(&place);
            if !folder.is_absolute() {
                bail!("{}", tf!("{path} 不是完整路径", path = &place));
            }
            // 空的文件夹先删掉（删不掉就是里面有东西）；上一级没有就建出来
            if folder.exists() && std::fs::remove_dir(folder).is_err() {
                bail!("{}", tf!("文件夹 {path} 已存在且不为空", path = &place));
            }
            if let Some(parent) = folder.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| tf!("无法创建 {path}", path = parent.display()))?;
            }
        }

        let settings = Settings {
            root,
            place: place.clone(),
            read_only,
            windows,
            label,
        };
        let add_dir_info = api.FspFileSystemAddDirInfo;
        let mount = Box::new(Mount::new(
            channel,
            settings,
            add_dir_info,
            Box::new(on_error),
        )?);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        // SAFETY：这个结构体全是整数和整数数组，全 0 是合法的值
        let mut params: VolumeParams = unsafe { std::mem::zeroed() };
        params.Version = size_of::<VolumeParams>() as u16;
        params.SectorSize = 4096;
        params.SectorsPerAllocationUnit = 1;
        params.MaxComponentLength = 255;
        params.VolumeCreationTime = fs::filetime(now);
        params.VolumeSerialNumber = now as u32;
        // 系统自己也把文件属性留这么久，这段时间里连回调都不来
        params.FileInfoTimeout = fs::TTL.as_millis() as u32;
        params.Flags = flag::CASE_PRESERVED_NAMES
            | flag::UNICODE_ON_DISK
            | flag::UM_FILE_CONTEXT_IS_USER_CONTEXT2;
        if !windows {
            params.Flags |= flag::CASE_SENSITIVE_SEARCH;
        }
        // 这个标志拦不住写入（回调里自己拦），但资源管理器会照它把「粘贴」「新建」变灰
        if read_only {
            params.Flags |= flag::READ_ONLY_VOLUME;
        }
        let name: Vec<u16> = "Kuai".encode_utf16().collect();
        params.FileSystemName[..name.len()].copy_from_slice(&name);
        let device = if drive {
            let share = super::unc_share(&share);
            let prefix: Vec<u16> = format!(r"\kuai\{share}").encode_utf16().take(190).collect();
            params.Prefix[..prefix.len()].copy_from_slice(&prefix);
            "WinFsp.Net"
        } else {
            "WinFsp.Disk"
        };

        let failed = |status: i32| {
            let code = format!("{status:#010x}");
            anyhow::anyhow!(tf!("映射到 {place} 失败（{code}）", place = &place, code))
        };
        let mut file_system = std::ptr::null_mut();
        // SAFETY：几个指针都指向这里活着的东西；INTERFACE 是静态的。建好以后把 `mount` 的地址
        // 交给它，回调里拿来用；`mount` 在堆上，地址不变，到 Drop 里停了调度线程才放
        unsafe {
            let status = (api.FspFileSystemCreate)(
                wide(device).as_mut_ptr(),
                &params,
                &fs::INTERFACE,
                &mut file_system,
            );
            if status != 0 {
                return Err(failed(status));
            }
            winfsp::set_user_context(file_system, (&raw const *mount).cast_mut().cast());
            let status = (api.FspFileSystemSetMountPoint)(file_system, wide(&place).as_mut_ptr());
            if status != 0 {
                (api.FspFileSystemDelete)(file_system);
                return Err(failed(status));
            }
            let status = (api.FspFileSystemStartDispatcher)(file_system, 0);
            if status != 0 {
                (api.FspFileSystemRemoveMountPoint)(file_system);
                (api.FspFileSystemDelete)(file_system);
                return Err(failed(status));
            }
        }
        Ok(Mounted {
            api,
            file_system,
            mount,
            place,
        })
    }

    impl Mounted {
        /// 本机的位置（盘符或文件夹）。
        pub fn place(&self) -> &str {
            &self.place
        }

        /// 盘上打开着的文件有几个（不算目录）。退出前拿来问「还有文件开着」。
        pub fn open_files(&self) -> usize {
            self.mount.open_files.load(Relaxed)
        }

        /// 换一条连接（重连上了），或者拿掉（断了：盘还在，读写都出错，等重连）。
        pub fn set_channel(&self, channel: Option<Channel>) {
            self.mount.set_channel(channel);
        }

        pub fn stats(&self) -> Stats {
            let counters = &self.mount.counters;
            Stats {
                calls: counters.calls.load(Relaxed),
                down: counters.down.load(Relaxed),
                up: counters.up.load(Relaxed),
            }
        }

        /// 映射没了时完成（Linux 上在外面卸掉了）。WinFsp 的盘在 Kuai 外面卸不掉，永远不完成。
        pub fn ended(&mut self) -> futures::future::BoxFuture<'static, ()> {
            Box::pin(futures::future::pending())
        }
    }

    impl Drop for Mounted {
        fn drop(&mut self) {
            // SAFETY：`file_system` 是 mount 里建的那个，只在这里放一次。先拿掉盘符、停掉调度
            // 线程，回调不会再来，之后 `mount` 才跟着这个结构体一起放
            unsafe {
                (self.api.FspFileSystemRemoveMountPoint)(self.file_system);
                (self.api.FspFileSystemStopDispatcher)(self.file_system);
                (self.api.FspFileSystemDelete)(self.file_system);
            }
            self.mount.finish();
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{Mounted, free_drives, installed, mount};

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::Ordering::Relaxed;

    use anyhow::{Context as _, Result, anyhow, bail};
    use fr_channel::Channel;
    use fr_core::tf;
    use fuser::MountOption;
    use futures::executor::block_on;
    use futures::future::BoxFuture;

    use super::{Options, Stats};
    use crate::fs::linux::{Fuse, THREADS};
    use crate::fs::{Mount, Settings};

    /// 挂载、卸载要经的 fusermount3（fuse3 包；普通用户自己挂不了 FUSE）。旧的 fusermount
    /// （fuse 2）也行。
    fn fusermount() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("FUSERMOUNT_PATH") {
            return Some(path.into());
        }
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).collect())
            .unwrap_or_default();
        dirs.extend(["/usr/bin", "/bin", "/sbin"].map(PathBuf::from));
        ["fusermount3", "fusermount"]
            .iter()
            .flat_map(|name| dirs.iter().map(move |dir| dir.join(name)))
            .find(|path| path.is_file())
    }

    /// 能不能映射：有 `/dev/fuse`，也装了 fuse3。
    pub fn installed() -> bool {
        Path::new("/dev/fuse").exists() && fusermount().is_some()
    }

    /// Linux 上没有盘符。
    pub fn free_drives() -> Vec<char> {
        Vec::new()
    }

    /// 一个映射着的目录。放掉它就卸载。
    pub struct Mounted {
        session: Option<fuser::BackgroundSession>,
        mount: Arc<Mount>,
        place: String,
        /// 文件夹是映射时建的：卸载后删掉
        created: bool,
        /// 会话结束时收到（见 [`Mounted::ended`]）
        ended: Option<futures::channel::oneshot::Receiver<()>>,
        /// 文件夹走到底的路径（`/proc/self/mountinfo` 里写的是这个），卸载时认它
        real: PathBuf,
    }

    /// 上次 Kuai 没正常退出，映射还挂在那里（读它报「Transport endpoint is not connected」）：
    /// 先卸掉。
    fn unmount_stale(folder: &Path) {
        let stale =
            std::fs::metadata(folder).is_err_and(|err| err.raw_os_error() == Some(libc::ENOTCONN));
        if let (true, Some(fusermount)) = (stale, fusermount()) {
            log::info!("卸掉上次留下的映射 {}", folder.display());
            let _ = std::process::Command::new(fusermount)
                .args(["-u", "-z"])
                .arg(folder)
                .output();
        }
    }

    /// 把 `options.root` 映射到 `options.place`（一个文件夹，没有就建，有的话要是空的）。会等服务器
    /// 回话（看那个目录在不在），别在界面线程上调。`on_error` 同 Windows 上的。
    pub fn mount(
        channel: Channel,
        options: Options,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Mounted> {
        let Options {
            root,
            place,
            read_only,
            windows,
            label,
            share: _,
        } = options;
        let stat = block_on(channel.fs_stat(&root, ""))
            .with_context(|| tf!("无法读取 {path}", path = &root))?;
        if !stat.exists {
            bail!("{}", tf!("{path} 不存在", path = &root));
        }
        if !stat.dir {
            bail!("{}", tf!("{path} 不是目录", path = &root));
        }
        let folder = Path::new(&place);
        if !folder.is_absolute() {
            bail!("{}", tf!("{path} 不是完整路径", path = &place));
        }
        unmount_stale(folder);
        let created = match std::fs::read_dir(folder) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    bail!("{}", tf!("文件夹 {path} 已存在且不为空", path = &place));
                }
                false
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(folder)
                    .with_context(|| tf!("无法创建 {path}", path = &place))?;
                true
            }
            Err(err) => {
                return Err(err).with_context(|| tf!("无法读取 {path}", path = &place));
            }
        };

        let real = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
        let settings = Settings {
            root,
            place: place.clone(),
            read_only,
            windows,
            label,
        };
        let mount = Arc::new(Mount::new(channel, settings, Box::new(on_error))?);
        let mut mount_options = vec![
            MountOption::FSName("kuai".into()),
            MountOption::Subtype("kuai".into()),
            // 内核按文件的权限查（主机上写不了的没有写权限），不来问
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::NoAtime,
        ];
        if read_only {
            mount_options.push(MountOption::RO);
        }
        let mut config = fuser::Config::default();
        config.mount_options = mount_options;
        config.n_threads = Some(THREADS);
        config.clone_fd = true;
        let (ended_tx, ended) = futures::channel::oneshot::channel();
        match fuser::spawn_mount(Fuse::new(mount.clone(), ended_tx), folder, &config) {
            Ok(session) => Ok(Mounted {
                session: Some(session),
                mount,
                place,
                created,
                ended: Some(ended),
                real,
            }),
            Err(err) => {
                if created {
                    let _ = std::fs::remove_dir(folder);
                }
                let err = anyhow!(err);
                Err(err.context(tf!("映射到 {place} 失败", place = &place)))
            }
        }
    }

    impl Mounted {
        /// 本机的位置（文件夹）。
        pub fn place(&self) -> &str {
            &self.place
        }

        /// 里面打开着的文件有几个（不算目录）。退出前拿来问「还有文件开着」。
        pub fn open_files(&self) -> usize {
            self.mount.open_files.load(Relaxed)
        }

        /// 换一条连接（重连上了），或者拿掉（断了：文件夹还在，读写都出错，等重连）。
        pub fn set_channel(&self, channel: Option<Channel>) {
            self.mount.set_channel(channel);
        }

        pub fn stats(&self) -> Stats {
            let counters = &self.mount.counters;
            Stats {
                calls: counters.calls.load(Relaxed),
                down: counters.down.load(Relaxed),
                up: counters.up.load(Relaxed),
            }
        }

        /// 映射没了时完成：在外面卸掉了（文件管理器里点了卸载、`fusermount3 -u`），或者放掉了它。
        /// 只能拿一次，再拿是永远不完成的。
        pub fn ended(&mut self) -> BoxFuture<'static, ()> {
            match self.ended.take() {
                Some(ended) => Box::pin(async move {
                    let _ = ended.await;
                }),
                None => Box::pin(futures::future::pending()),
            }
        }
    }

    impl Drop for Mounted {
        fn drop(&mut self) {
            // 先不再收新的写入、等收下的都落到主机上（见 `Mount::finish`），再卸载
            self.mount.finish();
            // 放掉会话时 fuser 卸载（不带 -z）：还有程序开着里面的文件、当前目录在里面时卸不掉，这时
            // 用 fusermount3 -u -z，先从目录树上摘下来，那些程序用完了系统再真的卸（开着的文件照样能读，
            // 写报错）。原来卸不掉就留在那里，Kuai 退出以后成了「连不上」的死目录
            drop(self.session.take());
            if mounted_at(&self.real)
                && let Some(fusermount) = fusermount()
            {
                log::info!("{} 还有程序在用，先摘下来", self.place);
                let _ = std::process::Command::new(fusermount)
                    .args(["-u", "-z"])
                    .arg(&self.real)
                    .output();
            }
            if self.created {
                let _ = std::fs::remove_dir(&self.place);
            }
        }
    }

    /// `folder` 上是不是挂着 Kuai 的映射：`/proc/self/mountinfo` 里第 5 项是挂载点，` - ` 后面第一项
    /// 是类型（`fuse.kuai`）。只认我们的，别的程序后来挂在这里的不动。
    fn mounted_at(folder: &Path) -> bool {
        let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return false;
        };
        text.lines().any(|line| {
            let point = line.split(' ').nth(4).map(unescape);
            let kind = line
                .split_once(" - ")
                .and_then(|(_, rest)| rest.split(' ').next());
            kind == Some("fuse.kuai") && point.is_some_and(|point| Path::new(&point) == folder)
        })
    }

    /// mountinfo 里的路径：空格、制表符、换行、反斜杠写成 `\ooo`（三位八进制）。
    pub(crate) fn unescape(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut at = 0;
        while at < bytes.len() {
            let code = bytes.get(at + 1..at + 4).filter(|digits| {
                bytes[at] == b'\\' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit))
            });
            match code {
                Some(digits) => {
                    let value = digits
                        .iter()
                        .fold(0u32, |value, digit| value * 8 + u32::from(digit - b'0'));
                    out.push(value as u8);
                    at += 4;
                }
                None => {
                    out.push(bytes[at]);
                    at += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub use stub::{Mounted, free_drives, installed, mount};

#[cfg(not(any(windows, target_os = "linux")))]
mod stub {
    use anyhow::{Result, bail};
    use fr_channel::Channel;
    use fr_core::i18n::tr;

    use super::{Options, Stats};

    pub fn installed() -> bool {
        false
    }

    pub fn free_drives() -> Vec<char> {
        Vec::new()
    }

    pub struct Mounted(());

    /// 和 Windows 上的一样，丢掉就是断开（这里本来就没有映射）。调用的地方在后台线程里 `drop` 它
    impl Drop for Mounted {
        fn drop(&mut self) {}
    }

    pub fn mount(
        _channel: Channel,
        _options: Options,
        _on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Mounted> {
        bail!("{}", tr("此系统不支持将目录映射到本机"))
    }

    impl Mounted {
        pub fn place(&self) -> &str {
            ""
        }

        pub fn open_files(&self) -> usize {
            0
        }

        pub fn set_channel(&self, _channel: Option<Channel>) {}

        pub fn stats(&self) -> Stats {
            Stats::default()
        }

        pub fn ended(&mut self) -> futures::future::BoxFuture<'static, ()> {
            Box::pin(futures::future::pending())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::unc_share;

    #[test]
    fn 网络路径里的共享名() {
        assert_eq!(unc_share("devops-workspaces"), "devops-workspaces");
        assert_eq!(unc_share("PC-work.v2_a"), "PC-work.v2_a");
        // 中文、空格、别的符号都换成横线，一个字一个横线
        assert_eq!(unc_share("测试机-src"), "----src");
        assert_eq!(unc_share("my box/src"), "my-box-src");
        assert_eq!(unc_share(&"a".repeat(300)).len(), 184);
    }
}

/// 真的经 FUSE 挂一次：经本机通道把一个临时目录映射到另一个临时文件夹，用 `std::fs` 经内核读写。
/// 要 `/dev/fuse` 和 fusermount3，又会真的挂载，所以只在设了 `FR_FUSE_TEST=1` 时跑（用普通用户；
/// root 不受权限限制，测不出只读）。
#[cfg(all(test, target_os = "linux"))]
mod fuse_tests {
    use std::fs;
    use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
    use std::path::Path;

    use fr_channel::Channel;

    use super::{Options, installed, mount};

    fn sorted_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// 等到 `done` 成立，最多等几秒（内核关文件时的 release 是后发的）。
    fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        done()
    }

    /// 删掉、改名盖掉还开着的文件：开着的接着读写原来那个，同名新建的是另一个，最后一个句柄关了
    /// 主机上藏起来的也删掉。目录盖空目录，不空的报 ENOTEMPTY，文件盖目录报 EISDIR。主机上的权限位
    /// 看得到，可执行的脚本能直接运行，chmod 在主机上真的改，改属主报 EPERM。
    #[test]
    fn 经_fuse_删掉开着的文件_改名盖掉_目录盖空目录_权限() {
        use std::os::unix::fs::PermissionsExt as _;
        if std::env::var_os("FR_FUSE_TEST").is_none_or(|flag| flag != "1") || !installed() {
            return;
        }
        let base = std::env::temp_dir().join(format!("fr-fuse-posix-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (host, place) = (base.join("host"), base.join("mnt"));
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("open.txt"), b"old").unwrap();
        let options = Options {
            root: host.to_string_lossy().into_owned(),
            place: place.to_string_lossy().into_owned(),
            read_only: false,
            windows: false,
            label: "test".into(),
            share: String::new(),
        };
        let mounted = mount(Channel::Local, options, |_| {}).unwrap();

        // 删掉开着的
        let mut held = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(place.join("open.txt"))
            .unwrap();
        fs::remove_file(place.join("open.txt")).unwrap();
        assert!(!place.join("open.txt").exists());
        assert!(sorted_names(&place).is_empty(), "藏起来的不该显示");
        fs::write(place.join("open.txt"), b"new").unwrap();
        let mut text = String::new();
        held.read_to_string(&mut text).unwrap();
        assert_eq!(text, "old");
        held.seek(SeekFrom::Start(0)).unwrap();
        held.write_all(b"OLD").unwrap();
        held.sync_all().unwrap();
        assert_eq!(fs::read(place.join("open.txt")).unwrap(), b"new");
        assert_eq!(fs::read(host.join("open.txt")).unwrap(), b"new");
        drop(held);
        assert!(
            wait_until(|| sorted_names(&host) == ["open.txt"]),
            "关了以后藏起来的没删：{:?}",
            sorted_names(&host)
        );

        // 改名盖掉开着的
        fs::write(place.join("b.txt"), b"bee").unwrap();
        let mut held = fs::File::open(place.join("open.txt")).unwrap();
        fs::rename(place.join("b.txt"), place.join("open.txt")).unwrap();
        assert_eq!(fs::read(place.join("open.txt")).unwrap(), b"bee");
        let mut text = String::new();
        held.read_to_string(&mut text).unwrap();
        assert_eq!(text, "new");
        drop(held);
        assert!(wait_until(|| sorted_names(&host) == ["open.txt"]));

        // 目录盖空目录；不空的、文件盖目录不行
        fs::create_dir_all(place.join("src/inner")).unwrap();
        fs::create_dir(place.join("empty")).unwrap();
        fs::rename(place.join("src"), place.join("empty")).unwrap();
        assert!(host.join("empty/inner").is_dir() && !host.join("src").exists());
        fs::create_dir(place.join("other")).unwrap();
        let err = fs::rename(place.join("other"), place.join("empty")).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOTEMPTY));
        let err = fs::rename(place.join("open.txt"), place.join("other")).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EISDIR));

        // 权限位
        fs::write(host.join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(host.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        // 刚在主机上建的：留着的目录过了留存的时间才看得到
        let mode =
            |path: &Path| fs::metadata(path).map_or(0, |meta| meta.permissions().mode() & 0o7777);
        assert!(wait_until(|| mode(&place.join("run.sh")) == 0o755));
        let ran = std::process::Command::new(place.join("run.sh"))
            .output()
            .unwrap();
        assert_eq!(ran.stdout, b"hi\n");
        fs::set_permissions(place.join("run.sh"), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(mode(&host.join("run.sh")), 0o700);
        assert_eq!(mode(&place.join("run.sh")), 0o700);
        let err = std::os::unix::fs::chown(place.join("run.sh"), Some(0), None).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EPERM));

        drop(mounted);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn 经_fuse_读写_改名_删除_主机上的文件都对() {
        if std::env::var_os("FR_FUSE_TEST").is_none_or(|flag| flag != "1") || !installed() {
            eprintln!("没设 FR_FUSE_TEST=1 或没有 FUSE，跳过");
            return;
        }
        let base = std::env::temp_dir().join(format!("fr-fuse-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (host, place) = (base.join("host"), base.join("mnt"));
        fs::create_dir_all(host.join("sub")).unwrap();
        fs::write(host.join("a.txt"), b"hello").unwrap();
        fs::write(host.join("sub/b.txt"), b"bee").unwrap();
        let options = Options {
            root: host.to_string_lossy().into_owned(),
            place: place.to_string_lossy().into_owned(),
            read_only: false,
            windows: false,
            label: "test".into(),
            share: String::new(),
        };
        let mounted = mount(Channel::Local, options, |text| {
            eprintln!("告诉用户：{text}")
        })
        .unwrap();

        // 读、列目录、查属性
        assert_eq!(fs::read_to_string(place.join("a.txt")).unwrap(), "hello");
        assert_eq!(sorted_names(&place), ["a.txt", "sub"]);
        assert_eq!(sorted_names(&place.join("sub")), ["b.txt"]);
        assert_eq!(fs::metadata(place.join("a.txt")).unwrap().len(), 5);
        assert!(fs::metadata(place.join("sub")).unwrap().is_dir());
        assert!(fs::metadata(place.join("nope")).is_err());

        // 新建、写、追加、截断
        fs::write(place.join("new.txt"), b"data").unwrap();
        assert_eq!(fs::read(host.join("new.txt")).unwrap(), b"data");
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(place.join("new.txt"))
            .unwrap();
        file.write_all(b"+more").unwrap();
        drop(file);
        assert_eq!(fs::read(host.join("new.txt")).unwrap(), b"data+more");
        fs::OpenOptions::new()
            .write(true)
            .open(place.join("new.txt"))
            .unwrap()
            .set_len(2)
            .unwrap();
        assert_eq!(fs::read(host.join("new.txt")).unwrap(), b"da");
        // 打开着的文件算在「开着的文件」里
        let held = fs::File::open(place.join("a.txt")).unwrap();
        assert_eq!(mounted.open_files(), 1);
        drop(held);

        // 大文件：随机内容写进去、在中间改一段、读回来都对
        let mut big: Vec<u8> = (0..3 << 20)
            .map(|i: u32| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        fs::write(place.join("big.bin"), &big).unwrap();
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(place.join("big.bin"))
            .unwrap();
        file.seek(SeekFrom::Start(1_000_000)).unwrap();
        file.write_all(&[7u8; 300_000]).unwrap();
        big[1_000_000..1_300_000].fill(7);
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut back = Vec::new();
        file.read_to_end(&mut back).unwrap();
        drop(file);
        assert!(back == big, "经映射读回来的不对");
        assert!(
            fs::read(host.join("big.bin")).unwrap() == big,
            "主机上的不对"
        );

        // 改名（挪到子目录、盖掉已有的）、建删目录、删文件
        fs::rename(place.join("new.txt"), place.join("sub/moved.txt")).unwrap();
        assert!(!host.join("new.txt").exists());
        assert_eq!(fs::read(host.join("sub/moved.txt")).unwrap(), b"da");
        fs::rename(place.join("sub/moved.txt"), place.join("sub/b.txt")).unwrap();
        assert_eq!(fs::read(host.join("sub/b.txt")).unwrap(), b"da");
        fs::rename(place.join("sub"), place.join("dir2")).unwrap();
        assert_eq!(fs::read_to_string(place.join("dir2/b.txt")).unwrap(), "da");
        fs::create_dir(place.join("empty")).unwrap();
        assert!(host.join("empty").is_dir());
        assert!(
            fs::remove_dir(place.join("dir2")).is_err(),
            "不空的目录删掉了"
        );
        fs::remove_dir(place.join("empty")).unwrap();
        fs::remove_file(place.join("dir2/b.txt")).unwrap();
        assert!(!host.join("dir2/b.txt").exists());
        assert!(
            fs::remove_file(place.join("dir2")).is_err(),
            "目录当文件删掉了"
        );

        // 主机上别人改了：过了留存的时间就看得到
        fs::write(host.join("a.txt"), b"changed on host").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2500));
        assert_eq!(
            fs::read_to_string(place.join("a.txt")).unwrap(),
            "changed on host"
        );

        // 卸载：映射时建的文件夹删掉
        drop(mounted);
        assert!(!place.exists(), "卸载后文件夹还在");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn 经_fuse_只读映射写不进去() {
        if std::env::var_os("FR_FUSE_TEST").is_none_or(|flag| flag != "1") || !installed() {
            return;
        }
        let base = std::env::temp_dir().join(format!("fr-fuse-ro-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (host, place) = (base.join("host"), base.join("mnt"));
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("a.txt"), b"keep").unwrap();
        // 映射到一个已有的空文件夹：卸载后留着
        fs::create_dir_all(&place).unwrap();
        let options = Options {
            root: host.to_string_lossy().into_owned(),
            place: place.to_string_lossy().into_owned(),
            read_only: true,
            windows: false,
            label: "test".into(),
            share: String::new(),
        };
        let mounted = mount(Channel::Local, options, |_| {}).unwrap();
        assert_eq!(fs::read_to_string(place.join("a.txt")).unwrap(), "keep");
        assert!(fs::write(place.join("a.txt"), b"x").is_err());
        assert!(fs::write(place.join("b.txt"), b"x").is_err());
        assert!(fs::remove_file(place.join("a.txt")).is_err());
        assert_eq!(fs::read(host.join("a.txt")).unwrap(), b"keep");
        drop(mounted);
        assert!(place.is_dir() && fs::read_dir(&place).unwrap().next().is_none());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn mountinfo_里的路径() {
        assert_eq!(
            super::linux::unescape(r"/home/me/Kuai/a\040b\011c\134d"),
            "/home/me/Kuai/a b\tc\\d"
        );
        assert_eq!(super::linux::unescape(r"/plain/\9x"), r"/plain/\9x");
        assert_eq!(super::linux::unescape("/中文/目录"), "/中文/目录");
    }

    /// 卸载时还有程序开着里面的文件、当前目录在里面：照样卸掉（-z），不留成「连不上」的死目录；
    /// 卸载以后再写报错，卸载前收下的都在主机上。
    #[test]
    fn 经_fuse_卸载时还开着文件_摘下来_收下的写入都在主机上() {
        use std::time::Duration;
        if std::env::var_os("FR_FUSE_TEST").is_none_or(|flag| flag != "1") || !installed() {
            return;
        }
        let base = std::env::temp_dir().join(format!("fr-fuse-busy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (host, place) = (base.join("host"), base.join("mnt"));
        fs::create_dir_all(&host).unwrap();
        let options = Options {
            root: host.to_string_lossy().into_owned(),
            place: place.to_string_lossy().into_owned(),
            read_only: false,
            windows: false,
            label: "test".into(),
            share: String::new(),
        };
        let mounted = mount(Channel::Local, options, |_| {}).unwrap();
        // 一直往里写，记下写成了多少
        let writer = {
            let file = place.join("log.bin");
            std::thread::spawn(move || {
                let mut out = fs::File::create(&file).unwrap();
                let mut written = 0u64;
                let chunk = [7u8; 4096];
                while out.write_all(&chunk).is_ok() {
                    written += chunk.len() as u64;
                    if written > 64 << 20 {
                        break;
                    }
                }
                drop(out);
                written
            })
        };
        std::thread::sleep(Duration::from_millis(300));
        drop(mounted);
        let written = writer.join().unwrap();
        assert!(written > 0);
        // 写成了的都在主机上
        assert_eq!(fs::metadata(host.join("log.bin")).unwrap().len(), written);
        // 卸掉了：mountinfo 里没有它
        let real = fs::canonicalize(&base).unwrap().join("mnt");
        let mountinfo = fs::read_to_string("/proc/self/mountinfo").unwrap();
        assert!(
            !mountinfo
                .lines()
                .any(|line| line.split(' ').nth(4).map(super::linux::unescape)
                    == Some(real.to_string_lossy().into_owned())),
            "还挂着"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn 经_fuse_在外面卸掉了_ended_会完成() {
        use std::time::Duration;
        if std::env::var_os("FR_FUSE_TEST").is_none_or(|flag| flag != "1") || !installed() {
            return;
        }
        let base = std::env::temp_dir().join(format!("fr-fuse-ended-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (host, place) = (base.join("host"), base.join("mnt"));
        fs::create_dir_all(&host).unwrap();
        let options = Options {
            root: host.to_string_lossy().into_owned(),
            place: place.to_string_lossy().into_owned(),
            read_only: false,
            windows: false,
            label: "test".into(),
            share: String::new(),
        };
        let mut mounted = mount(Channel::Local, options, |_| {}).unwrap();
        let ended = mounted.ended();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            futures::executor::block_on(ended);
            let _ = tx.send(());
        });
        // 挂着的时候不完成
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
        // 文件管理器里点「卸载」也是这样卸的
        let status = std::process::Command::new("fusermount3")
            .arg("-u")
            .arg(&place)
            .status()
            .unwrap();
        assert!(status.success());
        rx.recv_timeout(Duration::from_secs(10))
            .expect("在外面卸掉以后 ended 没完成");
        drop(mounted);
        let _ = fs::remove_dir_all(&base);
    }
}
