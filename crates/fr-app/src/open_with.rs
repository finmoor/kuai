//! 仓库里的文件交给本机的程序：在浏览器中打开（经 [`crate::web_serve`]），用默认程序打开（远程主机
//! 上的先下载一份到本机）。文件页、终端里的右键菜单都用。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use fr_channel::{Channel, UploadItem};
use fr_core::i18n::tr;
use fr_core::protocol::FsStat;
use fr_core::tf;
use futures::StreamExt as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use crate::settings::RepoEntry;

/// 下载超过这么大才提示「正在下载」
const SAY_DOWNLOADING: u64 = 2 << 20;

/// 小写的扩展名（没有是空的）。
fn extension(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

/// 浏览器看的文件：网页、SVG。
pub fn is_web(path: &str) -> bool {
    matches!(extension(path).as_str(), "html" | "htm" | "xhtml" | "svg")
}

/// Markdown。
pub fn is_markdown(path: &str) -> bool {
    matches!(extension(path).as_str(), "md" | "markdown")
}

/// 仓库根 `root` 加上相对路径 `path`（`/` 分隔）在那台电脑上的完整写法：主机上的、Windows 以外的本机
/// 用 `/`，Windows 本机用 `\`。
pub fn full_path(root: &str, remote: bool, path: &str) -> String {
    let root = root.trim_end_matches(['/', '\\']);
    if remote || !cfg!(windows) {
        return match path.is_empty() {
            true => root.to_string(),
            false => format!("{root}/{path}"),
        };
    }
    let root = root.replace('/', "\\");
    match path.is_empty() {
        true => root,
        false => format!("{root}\\{}", path.replace('/', "\\")),
    }
}

/// 在浏览器中打开仓库 `entry` 里的 `path`（相对仓库根）：经本机网页服务，同目录的样式、脚本、图片都
/// 能加载。`channel` 是这个仓库现在的通道，没有（主机没连上）时报错。
pub fn open_in_browser(
    entry: &RepoEntry,
    channel: Option<Channel>,
    path: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(channel) = channel else {
        let text = tr("主机未连接，无法读取此文件。");
        return crate::shell::report(Err(text.into()), window, cx);
    };
    log::info!("在浏览器中打开 {path}（仓库 {}）", entry.path);
    let opened = crate::web_serve::url_for(entry, channel, path)
        .and_then(|url| crate::shell::open_link(&url));
    crate::shell::report(opened, window, cx);
}

/// 这次运行里「用默认程序打开」下载过的副本（按仓库根和路径记）：同一个文件再打开时，主机上的没变、
/// 本机也没改过，就直接打开那一份，不再下一遍。
struct Copy {
    path: PathBuf,
    /// 下载好时的修改时间：变了就是在本机改过（这份不传回去，改过的不拿来复用）
    mtime: Option<SystemTime>,
    remote: FsStat,
}

static COPIES: Mutex<Option<HashMap<(String, String), Copy>>> = Mutex::new(None);

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// 远程主机上仓库 `repo` 里的 `path`：下载一份到本机，用默认程序打开。先问主机上那份多大：比
/// [`ASK_ABOVE`] 大的先问一下（和文件页卡片上的一样）；这次运行里下过、主机上没变、本机也没改过的
/// 直接打开那一份。下载大的文件时右下角提示一下；下载不了的报错。
pub fn open_copy(channel: Channel, repo: &str, path: &str, window: &mut Window, cx: &mut App) {
    let (repo, path) = (repo.to_string(), path.to_string());
    let stat = channel.fs_stat(&repo, &path);
    window
        .spawn(cx, async move |cx| {
            let stat = match stat.await {
                Ok(stat) if stat.exists && !stat.dir => stat,
                Ok(_) => {
                    let text = tf!("找不到 {path}", path);
                    cx.update(|window, cx| crate::shell::report(Err(text), window, cx))
                        .ok();
                    return;
                }
                Err(err) => {
                    let text = tf!("无法读取 {path}：{err}", path, err = format!("{err:#}"));
                    cx.update(|window, cx| crate::shell::report(Err(text), window, cx))
                        .ok();
                    return;
                }
            };
            let key = (repo.clone(), path.clone());
            let reuse = {
                let copies = COPIES.lock().unwrap_or_else(|err| err.into_inner());
                copies
                    .as_ref()
                    .and_then(|copies| copies.get(&key))
                    .filter(|copy| {
                        copy.path.is_file()
                            && modified(&copy.path) == copy.mtime
                            && crate::repo::same_version(&copy.remote, &stat)
                    })
                    .map(|copy| copy.path.to_string_lossy().into_owned())
            };
            cx.update(|window, cx| {
                if let Some(local) = reuse {
                    log::info!("{path} 这次运行里下载过、没变，直接打开那一份");
                    return crate::shell::open_file(&local, window, cx);
                }
                if stat.size <= ASK_ABOVE {
                    return download_and_open(channel, (repo, path), stat, window, cx);
                }
                let description = tf!(
                    "此文件大小为 {size}，需先下载到本机再打开。",
                    size = crate::text::human_size(stat.size)
                );
                crate::shell::ask(
                    (tr("下载后打开？"), description, tr("下载并打开")),
                    move |window, cx| download_and_open(channel, (repo, path), stat, window, cx),
                    window,
                    cx,
                );
            })
            .ok();
        })
        .detach();
}

/// 下载超过这么大先问一下
const ASK_ABOVE: u64 = 200 << 20;

/// [`open_copy`] 下载、打开（已经问过了）。`remote` 是下载前问到的主机上那份的属性。
fn download_and_open(
    channel: Channel,
    (repo, path): (String, String),
    remote: FsStat,
    window: &mut Window,
    cx: &mut App,
) {
    let name = path.rsplit('/').next().unwrap_or(&path).to_string();
    let dest = crate::repo::new_copy_dir().join(crate::repo::local_file_name(&name));
    log::info!("下载 {path} 到本机后用默认程序打开");
    let mut stream = channel.fs_download(&repo, &path, dest.clone());
    let key = (repo, path);
    window
        .spawn(cx, async move |cx| {
            let mut said = false;
            let mut outcome = Err(tr("传输中断").to_string());
            while let Some(item) = stream.next().await {
                match item {
                    Ok(UploadItem::Progress { total, .. }) => {
                        if !said && total > SAY_DOWNLOADING {
                            said = true;
                            let text = tf!(
                                "正在下载 {name}（{size}），完成后用默认程序打开。",
                                name = name.clone(),
                                size = crate::text::human_size(total)
                            );
                            cx.update(|window, cx| {
                                window.push_notification(Notification::info(text), cx)
                            })
                            .ok();
                        }
                    }
                    Ok(UploadItem::Done(_)) => {
                        outcome = Ok(());
                        break;
                    }
                    Err(err) => {
                        outcome = Err(format!("{err:#}"));
                        break;
                    }
                }
            }
            cx.update(|window, cx| match outcome {
                Ok(()) => {
                    let copy = Copy {
                        mtime: modified(&dest),
                        path: dest.clone(),
                        remote,
                    };
                    let mut copies = COPIES.lock().unwrap_or_else(|err| err.into_inner());
                    copies.get_or_insert_default().insert(key, copy);
                    drop(copies);
                    crate::shell::open_file(&dest.to_string_lossy(), window, cx)
                }
                Err(err) => {
                    let _ = std::fs::remove_dir_all(dest.parent().unwrap_or(&dest));
                    let text = tf!("下载 {name} 失败：{err}", name, err);
                    crate::shell::report(Err(text), window, cx);
                }
            })
            .ok();
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::{full_path, is_markdown, is_web};

    #[test]
    fn 认文件种类() {
        assert!(is_web("docs/a.HTML"));
        assert!(is_web("x.svg"));
        assert!(!is_web("x.html.bak"));
        assert!(is_markdown("README.md"));
        assert!(!is_markdown("md"));
    }

    #[test]
    fn 完整路径() {
        assert_eq!(
            full_path("/data/repo/", true, "a/b.rs"),
            "/data/repo/a/b.rs"
        );
        assert_eq!(full_path("/data/repo", true, ""), "/data/repo");
        if cfg!(windows) {
            assert_eq!(full_path(r"C:\src\r", false, "a/b.rs"), r"C:\src\r\a\b.rs");
        }
    }
}
