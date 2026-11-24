//! Linux 上这份 Kuai 是怎么装的：解压版（tar.gz）、.deb（装在 `/opt/kuai`）、AppImage。升级
//! （update.rs）、应用菜单（desktop.rs）、开机自启（autostart.rs）照它来。Windows 上什么都不是
//! （安装版、解压版由 update.rs 自己认）。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// .deb 装的程序。包名是 `kuai`，`/usr/bin/kuai` 链接到它
pub const DEB_EXE: &str = "/opt/kuai/kuai";
/// dpkg 记这个包装了哪些文件的地方：有它才算是 .deb 装的（不是有人把解压版放在了 /opt/kuai）
const DEB_LIST: &str = "/var/lib/dpkg/info/kuai.list";

/// 从 AppImage 启动的：那个 .AppImage 文件。
static APPIMAGE: OnceLock<Option<PathBuf>> = OnceLock::new();
/// 是不是 .deb 装的：启动时定了就不变（apt 换掉程序文件以后再看就认不出了，见 `fr_core::exe`）
static DEB: OnceLock<bool> = OnceLock::new();

/// 启动时调一次（在开别的线程之前）。从 AppImage 启动的话，记下那个文件，再把 AppImage 的运行时
/// 设的环境变量去掉：不然会传给终端里的程序（它们不是从 AppImage 启动的）。
pub fn take_env() {
    // 程序路径先记下：.deb 升级装好以后、重启以前，现取的路径带着「(deleted)」
    fr_core::exe::remember();
    #[cfg(target_os = "linux")]
    keep_inherited_fds();
    let found = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .filter(|file| file.is_file())
        .filter(|_| {
            // 程序真在 AppImage 里（APPDIR 是它挂出来的目录），不是从别的 AppImage 里开的终端里启动的
            let dir = std::env::var_os("APPDIR").map(PathBuf::from);
            let exe = fr_core::exe::path().ok();
            matches!((dir, exe), (Some(dir), Some(exe)) if exe.starts_with(&dir))
        });
    if cfg!(target_os = "linux") && found.is_some() {
        for name in ["APPIMAGE", "APPDIR", "ARGV0", "OWD"] {
            // SAFETY：在 main 的开头调，这时还只有一个线程
            unsafe { std::env::remove_var(name) };
        }
    }
    let _ = APPIMAGE.set(found.filter(|_| cfg!(target_os = "linux")));
}

/// 启动时继承来的文件描述符（0、1、2 以外的）设成执行时关闭，不再传给拉起的 kuaid、终端里的程序和
/// 升级后的新 Kuai。AppImage 的运行时就留了这样的：挂载进程的保活管道（它等管道的读端全关了才卸载）
/// 和挂载目录。传下去的话，常驻的 kuaid 不退，AppImage 就一直挂着，每升级一次多留一个挂载进程。
#[cfg(target_os = "linux")]
fn keep_inherited_fds() {
    let Ok(dir) = std::fs::read_dir("/proc/self/fd") else {
        return;
    };
    // 先收齐再改：读目录用的那个描述符这时已经关了，下面 F_GETFD 拿不到，跳过
    let fds: Vec<i32> = dir
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .filter(|fd| *fd > 2)
        .collect();
    for fd in fds {
        // SAFETY：只改这个描述符的标志，不关、不读写
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

/// 从 AppImage 启动的：那个 .AppImage 文件。
pub fn appimage() -> Option<&'static Path> {
    APPIMAGE.get().and_then(|file| file.as_deref())
}

/// 是 .deb 装的。
pub fn deb() -> bool {
    *DEB.get_or_init(|| {
        cfg!(target_os = "linux")
            && fr_core::exe::path().is_ok_and(|exe| exe == Path::new(DEB_EXE))
            && Path::new(DEB_LIST).is_file()
    })
}

/// 应用菜单、开机自启里写的程序：AppImage 写那个 .AppImage 文件（程序本身在它运行时才挂出来的
/// 临时目录里），别的写这个程序。
pub fn launcher() -> std::io::Result<PathBuf> {
    match appimage() {
        Some(file) => Ok(file.to_path_buf()),
        None => fr_core::exe::path(),
    }
}
