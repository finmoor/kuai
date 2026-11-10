//! 浏览目录：打开服务器上的仓库时列出目录。agent 和客户端的本机通道共用。

use std::path::Path;

use anyhow::{Context as _, Result};

use crate::protocol::{DirEntry, DirListing};

/// 当前用户的主目录。
pub fn home() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/".into())
}

/// 列出一个目录下的子目录，标出哪些是仓库。`path` 为空或 `~` 表示主目录，也支持 `~/xxx`。
pub fn list_dir(path: &str) -> Result<DirListing> {
    let home = home();
    let path = match path.trim() {
        "" | "~" => home.clone(),
        p => match p.strip_prefix("~/") {
            Some(rest) => format!("{home}/{rest}"),
            None => p.to_string(),
        },
    };
    let dir = std::fs::canonicalize(&path).with_context(|| format!("打不开 {path}"))?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("读不了 {}", show(&dir)))? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
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
        });
    }
    entries.sort_by_cached_key(|e| e.name.to_lowercase());
    Ok(DirListing {
        path: show(&dir),
        parent: dir.parent().map(show),
        repo: dir.join(".git").exists(),
        entries,
    })
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
