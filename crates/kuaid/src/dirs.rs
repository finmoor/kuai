//! kuaid 自己的目录在哪、给客户端看的路径怎么写。
//!
//! Windows 上给客户端的路径一律用正斜杠（`C:/Users/x/repo`）：客户端拼路径、取文件名都按 `/` 来，
//! git 和系统的文件接口两种斜杠都认。盘符之上再加一层虚拟的 `/`，里面列着各个盘。

use std::path::PathBuf;

use anyhow::Result;
use fr_core::protocol::DirListing;

/// `~/.kuai` 放在谁的主目录下。Windows 上可以用环境变量 `FR_KUAID_HOME` 换一个（测试用，
/// 不碰平时在用的那一套）。
pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("FR_KUAID_HOME").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    PathBuf::from(fr_core::dir::home())
}

/// `~/.kuai`。
pub fn kuai_root() -> PathBuf {
    home_dir().join(".kuai")
}

/// 给客户端看的路径。
pub fn shown(path: &str) -> String {
    #[cfg(windows)]
    {
        path.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.to_string()
    }
}

/// 握手时报给客户端的主目录：浏览目录从这里开始。
pub fn home() -> String {
    shown(&fr_core::dir::home())
}

/// `list_dir`：列出子目录。
pub fn list_dir(path: &str) -> Result<DirListing> {
    #[cfg(windows)]
    {
        windows::list_dir(path)
    }
    #[cfg(not(windows))]
    {
        fr_core::dir::list_dir(path)
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use fr_core::protocol::DirEntry;

    pub fn list_dir(path: &str) -> Result<DirListing> {
        let path = path.trim();
        if path == "/" {
            return Ok(drives());
        }
        let mut listing = fr_core::dir::list_dir(&real(path))?;
        listing.path = shown(&listing.path);
        // 盘的根目录再往上是列着各个盘的那一层
        listing.parent = Some(
            listing
                .parent
                .map_or_else(|| "/".into(), |parent| shown(&parent)),
        );
        Ok(listing)
    }

    /// 客户端拼出来的路径换成系统认的：`/C:` 和 `/C:/Users` 是从虚拟的 `/` 往下点出来的，
    /// 单独一个 `C:` 指的是这个盘的根目录（系统会当成「这个盘的当前目录」）。
    pub(super) fn real(path: &str) -> String {
        let rest = path.strip_prefix('/').unwrap_or(path);
        let drive = rest.len() >= 2
            && rest.as_bytes()[0].is_ascii_alphabetic()
            && rest.as_bytes()[1] == b':';
        match (drive, rest.len()) {
            (true, 2) => format!("{rest}/"),
            (true, _) => rest.to_string(),
            (false, _) => path.to_string(),
        }
    }

    fn drives() -> DirListing {
        // SAFETY: 没有参数，返回各个盘符的位图
        let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
        let entries = (0..26u8)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| DirEntry {
                name: format!("{}:", (b'A' + bit) as char),
                repo: false,
            })
            .collect();
        DirListing {
            path: "/".into(),
            parent: None,
            repo: false,
            entries,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn 从虚拟的根点下来的路径换成系统认的() {
            assert_eq!(real("/C:"), "C:/");
            assert_eq!(real("C:"), "C:/");
            assert_eq!(real("/C:/Users"), "C:/Users");
            assert_eq!(real("C:/Users/x"), "C:/Users/x");
            assert_eq!(real("~/src"), "~/src");
            assert_eq!(real("//server/share"), "//server/share");
        }

        #[test]
        fn 盘的根目录往上是列着各个盘的那一层() {
            let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
            let root = list_dir(&format!("/{drive}")).unwrap();
            assert_eq!(root.path, format!("{drive}/"));
            assert_eq!(root.parent.as_deref(), Some("/"));
            let top = list_dir("/").unwrap();
            assert!(top.parent.is_none());
            assert!(top.entries.iter().any(|entry| entry.name == drive));
            // 主目录：正斜杠
            let home = list_dir("~").unwrap();
            assert!(!home.path.contains('\\'), "{}", home.path);
            assert!(home.parent.is_some_and(|parent| !parent.contains('\\')));
        }
    }
}
