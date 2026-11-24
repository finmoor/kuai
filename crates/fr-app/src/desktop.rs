//! Linux 的桌面集成：应用菜单里的 Kuai（`~/.local/share/applications/kuai.desktop`）和图标
//! （`~/.local/share/icons/hicolor/256x256/apps/kuai.png`）。
//!
//! 解压版、AppImage 没有安装程序，每次启动时由 Kuai 自己写一遍：程序挪了位置、升级换了图标都
//! 跟着变，内容没变就不写。AppImage 写那个 .AppImage 文件。.deb 装的用系统里的（包里带着），不写。窗口的 app_id 是 `kuai`，桌面靠它对上这个文件，任务栏、通知里才显示
//! Kuai 的名字和图标。测试用的实例（设了 FR_SETTINGS）不写，免得改掉平时用的那个。

use std::path::{Path, PathBuf};

const ICON: &[u8] = include_bytes!("../assets/icon.png");

/// 启动时调一次（在后台线程上，不耽误开窗口）。
pub fn integrate() {
    if !cfg!(target_os = "linux") || std::env::var_os("FR_SETTINGS").is_some() {
        return;
    }
    std::thread::spawn(|| {
        if let Err(err) = write_files() {
            log::warn!("没写好应用菜单里的 Kuai：{err}");
        }
    });
}

fn data_home() -> Option<PathBuf> {
    let env = |name: &str| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    env("XDG_DATA_HOME").or_else(|| Some(env("HOME")?.join(".local").join("share")))
}

fn write_files() -> std::io::Result<()> {
    let Some(data) = data_home() else {
        return Ok(());
    };
    let entry = data.join("applications").join("kuai.desktop");
    // .deb 装的：系统里有应用菜单项和图标了。以前用解压版时写的那份会盖住系统的，删掉
    if crate::packaging::deb() {
        let ours = std::fs::read_to_string(&entry)
            .is_ok_and(|text| text.contains("\nStartupWMClass=kuai\n"));
        if ours {
            std::fs::remove_file(&entry)?;
        }
        return Ok(());
    }
    // AppImage 写那个 .AppImage 文件
    let exe = crate::packaging::launcher()?;
    let Some(exe) = exe.to_str() else {
        return Ok(());
    };
    let icon = data
        .join("icons")
        .join("hicolor")
        .join("256x256")
        .join("apps")
        .join("kuai.png");
    write_if_changed(&icon, ICON)?;
    // 程序路径里有写不进桌面入口的字符（见 `autostart::exec_quote`）：不写，应用菜单里就没有这一项
    let Some(text) = desktop_entry(exe) else {
        return Ok(());
    };
    write_if_changed(&entry, text.as_bytes())?;
    Ok(())
}

/// 内容不一样才写：先写到旁边再改名，桌面不会读到写了一半的。
fn write_if_changed(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if std::fs::read(path).is_ok_and(|old| old == bytes) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&partial, bytes)?;
    std::fs::rename(&partial, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&partial);
    })
}

/// 应用菜单里那一项。程序路径的写法和开机自启的一样（`autostart::exec_quote`），写不进去是 None。
fn desktop_entry(exe: &str) -> Option<String> {
    Some(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Kuai\n\
         Comment=Remote Git client and terminal\n\
         Comment[zh_CN]=远程 Git 客户端和终端\n\
         Exec={}\n\
         Icon=kuai\n\
         Terminal=false\n\
         Categories=Development;RevisionControl;\n\
         StartupWMClass=kuai\n\
         StartupNotify=true\n",
        crate::autostart::exec_quote(exe)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 应用菜单项指向这个程序() {
        let text = desktop_entry("/opt/Kuai 1/kuai").unwrap();
        assert!(text.contains("\nExec=\"/opt/Kuai 1/kuai\"\n"), "{text}");
        assert!(text.contains("\nStartupWMClass=kuai\n"));
        assert!(text.contains("\nIcon=kuai\n"));
    }

    #[test]
    fn 内容没变不写() {
        let dir = std::env::temp_dir().join(format!("kuai-desktop-test-{}", std::process::id()));
        let file = dir.join("a").join("kuai.desktop");
        write_if_changed(&file, b"one").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"one");
        let before = file.metadata().unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_if_changed(&file, b"one").unwrap();
        assert_eq!(file.metadata().unwrap().modified().unwrap(), before);
        write_if_changed(&file, b"two").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"two");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
