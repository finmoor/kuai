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

#[cfg_attr(not(windows), allow(dead_code))]
const VALUE: &str = "Kuai";
/// 开机自启带的参数：有托盘就直接收进托盘，不弹窗口。
pub const MINIMIZED_FLAG: &str = "--minimized";

/// Run 键里该写的命令。
#[cfg_attr(not(windows), allow(dead_code))]
fn command_for(exe: &str) -> String {
    format!("\"{exe}\" {MINIMIZED_FLAG}")
}

/// StartupApproved 里的值是不是「允许启动」：第一个字节是双数（02、06）为允许，单数（03 等，
/// 后面跟着禁用的时间）为禁用。没有这一项也是允许。
#[cfg_attr(not(windows), allow(dead_code))]
fn approved(value: Option<&[u8]>) -> bool {
    value
        .and_then(|bytes| bytes.first())
        .is_none_or(|first| first % 2 == 0)
}

/// 注册表里的命令是不是指向这个 exe（不分大小写：Windows 的路径不分）。
#[cfg_attr(not(windows), allow(dead_code))]
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

/// Linux 等用 freedesktop 桌面的系统：`$XDG_CONFIG_HOME/autostart/kuai.desktop`（默认在
/// `~/.config/autostart/` 下），登录桌面时由桌面环境启动。「开没开」以这个文件为准：没有、
/// 指向的不是这个程序（挪了位置）、被设成隐藏或禁用（GNOME 的「启动应用程序」那样关掉的）都算没开。
/// FR_AUTOSTART_KEY 在这里是换一个文件（测试用）。
#[cfg(not(windows))]
mod imp {
    use super::{desktop_entry, entry_enabled};
    use anyhow::{Context as _, Result};
    use fr_core::i18n::tr;
    use std::path::PathBuf;

    pub fn file() -> Option<PathBuf> {
        if let Some(file) = std::env::var_os("FR_AUTOSTART_KEY").filter(|file| !file.is_empty()) {
            return Some(PathBuf::from(file));
        }
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                let home = std::env::var_os("HOME").filter(|dir| !dir.is_empty())?;
                Some(PathBuf::from(home).join(".config"))
            })?;
        Some(config.join("autostart").join("kuai.desktop"))
    }

    fn exe() -> Result<String> {
        let path = std::env::current_exe().context(tr("无法获取程序路径"))?;
        path.to_str()
            .map(str::to_string)
            .context(tr("无法获取程序路径"))
    }

    pub fn enabled() -> bool {
        let (Some(file), Ok(exe)) = (file(), exe()) else {
            return false;
        };
        std::fs::read_to_string(file).is_ok_and(|text| entry_enabled(&text, &exe))
    }

    pub fn set(on: bool) -> Result<()> {
        let file = file().context(tr("添加启动项失败"))?;
        if on {
            let text = desktop_entry(&exe()?);
            let dir = file.parent().context(tr("添加启动项失败"))?;
            std::fs::create_dir_all(dir).context(tr("添加启动项失败"))?;
            // 先写到旁边再改名：桌面环境不会读到写了一半的
            let partial = file.with_extension("desktop.tmp");
            std::fs::write(&partial, text)
                .and_then(|_| std::fs::rename(&partial, &file))
                .context(tr("添加启动项失败"))?;
        } else {
            match std::fs::remove_file(&file) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                    return Err(err).context(tr("删除启动项失败"));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// 自启文件的内容（freedesktop 的 desktop entry）。
#[cfg_attr(windows, allow(dead_code))]
fn desktop_entry(exe: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Kuai\nExec={} {MINIMIZED_FLAG}\nIcon=kuai\n\
         Terminal=false\nX-GNOME-Autostart-enabled=true\n",
        exec_quote(exe)
    )
}

/// 按 desktop entry 的规矩把程序路径写进 `Exec=`：加双引号，里面的 `"`、`` ` ``、`$`、`\` 前面加
/// 反斜杠；整个值再按字符串的规矩把反斜杠写两遍；`%` 写成 `%%`（不然当成 `%f` 这类占位）。
pub(crate) fn exec_quote(path: &str) -> String {
    let mut quoted = String::from("\"");
    for c in path.chars() {
        match c {
            '"' | '`' | '$' => {
                quoted.push_str("\\\\");
                quoted.push(c);
            }
            '\\' => quoted.push_str("\\\\\\\\"),
            '%' => quoted.push_str("%%"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// 读出 `Exec=` 里的程序（第一个参数），按 [`exec_quote`] 的写法还原。
#[cfg_attr(windows, allow(dead_code))]
fn exec_program(exec: &str) -> Option<String> {
    let exec = exec.trim().replace("\\\\", "\\").replace("%%", "%");
    let Some(rest) = exec.strip_prefix('"') else {
        return exec.split_whitespace().next().map(str::to_string);
    };
    let mut program = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => program.push(chars.next()?),
            '"' => return Some(program),
            c => program.push(c),
        }
    }
    None
}

/// 自启文件里这一项算不算开着：`[Desktop Entry]` 里的 `Exec=` 指向这个程序，没有设成隐藏、禁用。
#[cfg_attr(windows, allow(dead_code))]
fn entry_enabled(text: &str, exe: &str) -> bool {
    let mut in_entry = false;
    let (mut points, mut off) = (false, false);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !in_entry {
            continue;
        }
        match (key.trim(), value.trim()) {
            ("Exec", exec) => points = exec_program(exec).is_some_and(|program| program == exe),
            ("Hidden", "true") | ("X-GNOME-Autostart-enabled", "false") => off = true,
            _ => {}
        }
    }
    points && !off
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
    use super::{approved, command_for, desktop_entry, entry_enabled, exec_program, points_to};

    #[test]
    fn 自启文件指向这个程序才算开着() {
        let exe = r#"/opt/Kuai "新"/$HOME`x`/a\b%f/kuai"#;
        let text = desktop_entry(exe);
        assert!(text.starts_with("[Desktop Entry]\n"));
        assert!(text.contains(" --minimized\n"), "{text}");
        let exec = text
            .lines()
            .find_map(|line| line.strip_prefix("Exec="))
            .unwrap();
        assert_eq!(exec_program(exec).as_deref(), Some(exe), "{exec}");
        assert!(entry_enabled(&text, exe));
        // 程序挪了位置
        assert!(!entry_enabled(&text, "/usr/bin/kuai"));
        // 在桌面的「启动应用程序」里关掉的
        let off = text.replace(
            "X-GNOME-Autostart-enabled=true",
            "X-GNOME-Autostart-enabled=false",
        );
        assert!(!entry_enabled(&off, exe));
        assert!(!entry_enabled(&format!("{text}Hidden=true\n"), exe));
        // 别的组里的不算
        let other = format!("[Desktop Action x]\nExec=\"{exe}\"\n");
        assert!(!entry_enabled(&other, exe));
        // 没加引号的
        assert_eq!(
            exec_program("/usr/bin/kuai --minimized").as_deref(),
            Some("/usr/bin/kuai")
        );
    }

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
