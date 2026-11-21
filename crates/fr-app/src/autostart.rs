//! 开机自启：在当前用户的 Run 键（`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`）里
//! 写一项，值是 `"<这个 exe 的路径>" --minimized`。只动当前用户的，不需要管理员权限。
//!
//! 「开没开」以注册表为准，不在设置文件里另存一份：用户在任务管理器的「启动」里关掉、
//! 或者把程序挪了位置，两边就对不上了。挪了位置以后注册表里是旧路径，这里算作没开。任务管理器
//! 里关掉时 Run 键里的那一项还在，另在 `Explorer\StartupApproved\Run` 里记一笔「禁用」：
//! 那里写着禁用的也算没开；这里打开时把那一笔删掉，Windows 才会照常启动。
//!
//! 环境变量 FR_AUTOSTART_KEY 可以换一个键（测试用，免得往真的启动项里写东西），「禁用」那一笔
//! 跟着记在它后面加 `-approved` 的键里。

use anyhow::Result;

const VALUE: &str = "Kuai";
/// 开机自启带的参数：有托盘就直接收进托盘，不弹窗口。
pub const MINIMIZED_FLAG: &str = "--minimized";

/// Run 键里该写的命令。
fn command_for(exe: &str) -> String {
    format!("\"{exe}\" {MINIMIZED_FLAG}")
}

/// StartupApproved 里的值是不是「允许启动」：第一个字节是双数（02、06）为允许，单数（03 等，
/// 后面跟着禁用的时间）为禁用。没有这一项也是允许。
fn approved(value: Option<&[u8]>) -> bool {
    value
        .and_then(|bytes| bytes.first())
        .is_none_or(|first| first % 2 == 0)
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
    use super::{VALUE, approved, command_for, points_to};
    use anyhow::{Context as _, Result};
    use fr_core::i18n::tr;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    /// 启动项记在哪两个键里。
    pub struct Keys {
        /// Run 键
        pub run: String,
        /// 任务管理器记「禁用」的键
        pub approved: String,
    }

    impl Keys {
        fn current() -> Keys {
            match std::env::var("FR_AUTOSTART_KEY")
                .ok()
                .filter(|key| !key.trim().is_empty())
            {
                Some(key) => Keys {
                    approved: format!("{key}-approved"),
                    run: key,
                },
                None => Keys {
                    run: r"Software\Microsoft\Windows\CurrentVersion\Run".to_string(),
                    approved:
                        r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"
                            .to_string(),
                },
            }
        }
    }

    fn exe() -> Result<String> {
        let path = std::env::current_exe().context(tr("无法获取程序路径"))?;
        let path = path.to_string_lossy();
        Ok(path.strip_prefix(r"\\?\").unwrap_or(&path).to_string())
    }

    pub fn enabled() -> bool {
        enabled_in(&Keys::current())
    }

    pub fn enabled_in(keys: &Keys) -> bool {
        let Ok(exe) = exe() else {
            return false;
        };
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let listed = hkcu
            .open_subkey(&keys.run)
            .and_then(|key| key.get_value::<String, _>(VALUE))
            .is_ok_and(|command| points_to(&command, &exe));
        let value = hkcu
            .open_subkey(&keys.approved)
            .and_then(|key| key.get_raw_value(VALUE))
            .ok();
        listed && approved(value.as_ref().map(|value| &value.bytes[..]))
    }

    /// 删掉一个键里的这一项；本来就没有不算错。
    fn delete(path: &str) -> std::io::Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let Ok(key) = hkcu.open_subkey_with_flags(path, winreg::enums::KEY_WRITE) else {
            return Ok(());
        };
        match key.delete_value(VALUE) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            done => done,
        }
    }

    pub fn set(on: bool) -> Result<()> {
        set_in(&Keys::current(), on)
    }

    pub fn set_in(keys: &Keys, on: bool) -> Result<()> {
        if on {
            let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
                .create_subkey(&keys.run)
                .context(tr("无法打开启动项注册表键"))?;
            key.set_value(VALUE, &command_for(&exe()?))
                .context(tr("添加启动项失败"))?;
            // 任务管理器里禁用过：不删掉那一笔 Windows 不启动
            delete(&keys.approved).context(tr("添加启动项失败"))?;
        } else {
            delete(&keys.run).context(tr("删除启动项失败"))?;
            let _ = delete(&keys.approved);
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
        anyhow::bail!(tr("当前系统不支持开机自动启动"))
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
    use super::{approved, command_for, points_to};

    /// 真的写注册表（换成测试用的键）：任务管理器里禁用过的，这里打开时把那一笔删掉。
    #[cfg(windows)]
    #[test]
    fn 打开时删掉任务管理器的禁用记录() {
        use super::imp::{Keys, enabled_in, set_in};
        use winreg::RegKey;
        use winreg::enums::HKEY_CURRENT_USER;
        let root = format!(r"Software\KuaiTest-autostart-{}", std::process::id());
        let keys = Keys {
            run: format!(r"{root}\Run"),
            approved: format!(r"{root}\Approved"),
        };
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let disable = || {
            let (key, _) = hkcu.create_subkey(&keys.approved).unwrap();
            let value = winreg::RegValue {
                bytes: vec![3, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8].into(),
                vtype: winreg::enums::RegType::REG_BINARY,
            };
            key.set_raw_value(super::VALUE, &value).unwrap();
        };
        set_in(&keys, true).unwrap();
        assert!(enabled_in(&keys));
        disable();
        assert!(!enabled_in(&keys), "任务管理器里禁用了还算开着");
        set_in(&keys, true).unwrap();
        assert!(enabled_in(&keys), "打开时没删掉禁用的记录");
        set_in(&keys, false).unwrap();
        assert!(!enabled_in(&keys));
        let _ = hkcu.delete_subkey_all(&root);
    }

    #[test]
    fn 任务管理器里禁用了不算开着() {
        assert!(approved(None));
        assert!(approved(Some(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])));
        assert!(approved(Some(&[6, 0, 0, 0])));
        assert!(!approved(Some(&[
            3, 0, 0, 0, 0x10, 0x20, 0x30, 0x40, 1, 2, 3, 4
        ])));
        assert!(approved(Some(&[])));
    }

    #[test]
    fn 启动项指向这个程序才算开着() {
        let exe = r"C:\Tools\Kuai\kuai.exe";
        let command = command_for(exe);
        assert_eq!(command, r#""C:\Tools\Kuai\kuai.exe" --minimized"#);
        assert!(points_to(&command, exe));
        // 路径大小写不一样也算
        assert!(points_to(&command, &exe.to_lowercase()));
        // 程序挪了位置：注册表里还是旧路径，不算开着
        assert!(!points_to(&command, r"D:\Apps\kuai.exe"));
        // 别的东西写的、没加引号的
        assert!(!points_to(exe, exe));
        assert!(!points_to("", exe));
    }
}
