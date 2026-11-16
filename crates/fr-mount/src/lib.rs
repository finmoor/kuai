//! 把服务器（或本机）上的一个目录映射成本机的盘符或文件夹。
//!
//! 盘上的读写变成 kuaid 的 `fs_*` 请求（协议 9），走客户端平时用的那条通道，不另开连接、不要
//! 服务器装别的东西。底下是 WinFsp（用户态文件系统），只在 Windows 上有：别的系统上
//! [`installed`] 总是假，[`mount`] 总是出错。
//!
//! - 盘符（`K:`）挂成网络盘的样子（`\\kuai\<名字>`）：资源管理器不在上面建回收站、不建索引。
//! - 文件夹只能挂成本地磁盘的样子：WinFsp 在那个位置建一个链接，卸载时拿掉，所以这个文件夹
//!   事先不能有（空的会先删掉）。
//!
//! 映射在调用 [`mount`] 的进程里：[`Mounted`] 放掉、进程退出，盘就没了。
//!
//! WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos
//! <https://github.com/winfsp/winfsp>

#[cfg(windows)]
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
        let mount = Box::new(Mount::new(channel, settings, &api, Box::new(on_error))?);

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

#[cfg(not(windows))]
pub use stub::{Mounted, free_drives, installed, mount};

#[cfg(not(windows))]
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

    pub fn mount(
        _channel: Channel,
        _options: Options,
        _on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Mounted> {
        bail!("{}", tr("仅 Windows 支持将目录映射到本机"))
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
    }
}

#[cfg(test)]
mod tests {
    use super::unc_share;

    #[test]
    fn 网络路径里的共享名() {
        assert_eq!(unc_share("devops-workspaces"), "devops-workspaces");
        assert_eq!(unc_share("T14P-work.v2_a"), "T14P-work.v2_a");
        // 中文、空格、别的符号都换成横线，一个字一个横线
        assert_eq!(unc_share("测试机-src"), "----src");
        assert_eq!(unc_share("my box/src"), "my-box-src");
        assert_eq!(unc_share(&"a".repeat(300)).len(), 184);
    }
}
