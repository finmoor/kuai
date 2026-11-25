//! 浏览目录：打开服务器上的仓库时列出目录。kuaid 和客户端的本机通道共用。

use crate::tf;
use std::path::Path;

use anyhow::{Context as _, Result};

use crate::protocol::{DirEntry, DirListing};

/// 当前用户的主目录。Windows 上先看 `USERPROFILE`：`HOME` 不一定有，有也可能是别的写法。
pub fn home() -> String {
    let order = match cfg!(windows) {
        true => ["USERPROFILE", "HOME"],
        false => ["HOME", "USERPROFILE"],
    };
    order
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|dir| !dir.is_empty()))
        .unwrap_or_else(|| "/".into())
}

/// Windows 上终端会话的名字怎么比：名字是仓库路径（后面可能还有序号），同一个目录有几种写法
/// （两种斜杠、盘符和名字的大小写），统一成正斜杠、小写再比。本机的 Kuai 和从别的电脑连上来的
/// Kuai 这样才认得出是同一个终端。kuaid 和客户端共用。
pub fn windows_key(key: &str) -> String {
    key.replace('\\', "/").to_lowercase()
}

/// 列出一个目录下的子目录，标出哪些是仓库。`path` 为空或 `~` 表示主目录，也支持 `~/xxx`。
pub fn list_dir(path: &str) -> Result<DirListing> {
    list_dir_with(path, false)
}

/// 同 [`list_dir`]；`hidden` 时以 `.` 开头的目录（`.git` 除外）也列，标上 [`DirEntry::hidden`]。
/// 顺带往上找这个目录在哪个仓库里（[`DirListing::root`]）。
pub fn list_dir_with(path: &str, hidden: bool) -> Result<DirListing> {
    let home = home();
    let path = match path.trim() {
        "" | "~" => home.clone(),
        p => match p.strip_prefix("~/") {
            Some(rest) => format!("{home}/{rest}"),
            None => p.to_string(),
        },
    };
    let dir = std::fs::canonicalize(&path).with_context(|| tf!("无法打开 {path}", path))?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| tf!("无法读取 {dir}", dir = show(&dir)))?
    {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let dotted = name.starts_with('.');
        if dotted && (!hidden || name == ".git") {
            continue;
        }
        // 跟随符号链接判断是不是目录
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        entries.push(DirEntry {
            repo: path.join(".git").exists(),
            name,
            hidden: dotted,
        });
    }
    // 隐藏的排在后面
    entries.sort_by_cached_key(|e| (e.hidden, e.name.to_lowercase()));
    Ok(DirListing {
        path: show(&dir),
        parent: dir.parent().map(show),
        repo: dir.join(".git").exists(),
        root: enclosing_repo(&dir).map(|root| show(&root)),
        entries,
    })
}

/// 往上找第一个有 `.git`（目录，或者子模块、工作树的 `.git` 文件）的目录。
fn enclosing_repo(dir: &Path) -> Option<std::path::PathBuf> {
    dir.ancestors()
        .take(64)
        .find(|at| std::fs::symlink_metadata(at.join(".git")).is_ok())
        .map(Path::to_path_buf)
}

/// 路径转成显示用的字符串；去掉 Windows 上 canonicalize 加的 `\\?\` 前缀。
fn show(path: &Path) -> String {
    let s = path.to_string_lossy().into_owned();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => rest.to_string(),
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 同一个目录的几种写法是同一个名字() {
        assert_eq!(windows_key(r"C:\Users\me\Repo"), "c:/users/me/repo");
        assert_eq!(windows_key("c:/Users/me/repo"), "c:/users/me/repo");
        // 第 2 个终端起名字后面带着的序号不受影响
        assert_eq!(windows_key("C:\\Repo\u{1f}2"), "c:/repo\u{1f}2");
    }

    #[test]
    fn 只列子目录并标出仓库() {
        let root = std::env::temp_dir().join(format!("fr-dir-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("b-repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("A-plain")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("file.txt"), "x").unwrap();

        let listing = list_dir(&root.to_string_lossy()).unwrap();
        let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["A-plain", "b-repo"]);
        assert!(!listing.entries[0].repo);
        assert!(listing.entries[1].repo);
        assert!(!listing.repo);
        assert!(listing.parent.is_some());

        let inner = list_dir(&root.join("b-repo").to_string_lossy()).unwrap();
        assert!(inner.repo);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
