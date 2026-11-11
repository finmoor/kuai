//! 开机自启：在当前用户的 Run 键（`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`）里
//! 写一项，值是 `"<这个 exe 的路径>" --minimized`。只动当前用户的，不需要管理员权限。
//!
//! 「开没开」以注册表为准，不在设置文件里另存一份：用户在任务管理器的「启动」里关掉、
//! 或者把程序挪了位置，两边就对不上了。挪了位置以后注册表里是旧路径，这里算作没开。
//!
//! 环境变量 FR_AUTOSTART_KEY 可以换一个键（测试用，免得往真的启动项里写东西）。

use anyhow::Result;

const VALUE: &str = "fork-remote";
/// 开机自启带的参数：有托盘就直接收进托盘，不弹窗口。
pub const MINIMIZED_FLAG: &str = "--minimized";

/// Run 键里该写的命令。
fn command_for(exe: &str) -> String {
    format!("\"{exe}\" {MINIMIZED_FLAG}")
}

/// 注册表里的命令是不是指向这个 exe（不分大小写：Windows 的路径不分）。
fn points_to(command: &str, exe: &str) -> bool {
    command
        .trim()
        .strip_prefix('"')
        .and_then(|rest| rest.split_once('"'))
        .is_some_and(|(path, _)| path.eq_ignore_ascii_case(exe))
}

#[cfg(windows)]
mod imp {
    use super::{VALUE, command_for, points_to};
    use anyhow::{Context as _, Result};
    use fr_core::i18n::tr;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    fn key_path() -> String {
        std::env::var("FR_AUTOSTART_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .unwrap_or_else(|| r"Software\Microsoft\Windows\CurrentVersion\Run".to_string())
    }

    fn exe() -> Result<String> {
        let path = std::env::current_exe().context(tr("找不到程序自己的路径"))?;
        let path = path.to_string_lossy();
        Ok(path.strip_prefix(r"\\?\").unwrap_or(&path).to_string())
    }

    pub fn enabled() -> bool {
        let Ok(exe) = exe() else {
            return false;
        };
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(key_path())
            .and_then(|key| key.get_value::<String, _>(VALUE))
            .is_ok_and(|command| points_to(&command, &exe))
    }

    pub fn set(on: bool) -> Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if on {
            let (key, _) = hkcu
                .create_subkey(key_path())
                .context(tr("打不开启动项的注册表键"))?;
            key.set_value(VALUE, &command_for(&exe()?))
                .context(tr("写启动项失败"))?;
        } else if let Ok(key) = hkcu.open_subkey_with_flags(key_path(), winreg::enums::KEY_WRITE) {
            match key.delete_value(VALUE) {
                Ok(()) => {}
                // 本来就没有这一项
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err).context(tr("删启动项失败")),
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use anyhow::Result;

    pub fn enabled() -> bool {
        false
    }

    pub fn set(_on: bool) -> Result<()> {
        anyhow::bail!(tr("这个系统上还不支持开机自启"))
    }
}

/// 开机自启现在开没开。
pub fn enabled() -> bool {
    imp::enabled()
}

/// 打开或关掉开机自启。
pub fn set(on: bool) -> Result<()> {
    imp::set(on)
}

#[cfg(test)]
mod tests {
    use super::{command_for, points_to};

    #[test]
    fn 启动项指向这个程序才算开着() {
        let exe = r"C:\Tools\fork-remote\fork-remote.exe";
        let command = command_for(exe);
        assert_eq!(
            command,
            r#""C:\Tools\fork-remote\fork-remote.exe" --minimized"#
        );
        assert!(points_to(&command, exe));
        // 路径大小写不一样也算
        assert!(points_to(&command, &exe.to_lowercase()));
        // 程序挪了位置：注册表里还是旧路径，不算开着
        assert!(!points_to(&command, r"D:\Apps\fork-remote.exe"));
        // 别的东西写的、没加引号的
        assert!(!points_to(exe, exe));
        assert!(!points_to("", exe));
    }
}
