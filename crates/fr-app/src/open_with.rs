//! 仓库里的文件交给本机的程序：在浏览器中打开（经 [`crate::web_serve`]），用默认程序打开（远程主机
//! 上的先下载一份到本机）。文件页、终端里的右键菜单都用。

use fr_channel::{Channel, UploadItem};
use fr_core::i18n::tr;
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

/// 远程主机上仓库 `repo` 里的 `path`：下载一份到本机的临时目录，用默认程序打开。下载大的文件时右下角
/// 提示一下；下载不了的报错。
pub fn open_copy(channel: Channel, repo: &str, path: &str, window: &mut Window, cx: &mut App) {
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    let dest = crate::repo::new_copy_dir().join(crate::repo::local_file_name(&name));
    log::info!("下载 {path} 到本机后用默认程序打开");
    let mut stream = channel.fs_download(repo, path, dest.clone());
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
                Ok(()) => crate::shell::open_file(&dest.to_string_lossy(), window, cx),
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
