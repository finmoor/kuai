//! 开机时启动常驻进程（协议 18 起，定时任务要它：主机重启后没有客户端来连，常驻进程不在，
//! 任务到点也不会运行）。
//!
//! - Linux 等：该用户的 crontab 里加一行 `@reboot '<~/.kuai/boot.sh>'`，不需要 root。脚本里写着
//!   现在这一版 kuaid 的路径，常驻进程每次启动时改写（升级以后旧版本的目录会被删掉）；写着的那个
//!   不在了，就用 `~/.kuai/` 下最新的一个。
//! - Windows：当前用户 Run 键里的一项，登录时运行 `kuaid.exe --boot`。常驻进程每次启动时改成
//!   自己这一版的路径。本机的 Kuai 自己有开机自启（客户端的 `autostart.rs`），Kuai 启动时就把
//!   本机的常驻进程拉起来了，用不着这一项；这一项给经 ssh 连过来的 Windows 主机用。
//!
//! 「开没开」以 crontab、注册表为准，不另存一份：用户自己改了 crontab、在任务管理器里禁用了，
//! 照实报。`kuaid --boot`（[`crate::bridge::boot`]）：常驻进程没在跑就拉起来，然后退出。

use anyhow::Result;

/// 现在开没开。设不了的（没有 crontab 命令）是 `Err`，说明原因。
pub fn enabled() -> Result<bool> {
    imp::enabled()
}

/// 打开或关掉。
pub fn set(on: bool) -> Result<()> {
    imp::set(on)
}

/// 常驻进程启动时：开着的话，把启动的程序改成自己这一版。
pub fn refresh() {
    imp::refresh();
}

/// 测试实例（设了 `FR_KUAID_HOME`）的主目录：开机时启动要带上它，不然拉起的是平时那一套。
fn isolated_home() -> Option<String> {
    std::env::var("FR_KUAID_HOME")
        .ok()
        .filter(|dir| !dir.is_empty())
}

fn current_exe() -> Result<String> {
    use anyhow::Context as _;
    let exe = std::env::current_exe().context("无法确定 kuaid 的安装位置")?;
    let exe = exe.to_string_lossy();
    Ok(exe.strip_prefix(r"\\?\").unwrap_or(&exe).to_string())
}

#[cfg(unix)]
mod imp {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    use anyhow::{Context as _, Result, bail};
    use fr_core::i18n::tr;
    use fr_core::tf;

    /// crontab 里那一行启动的脚本。
    fn script() -> PathBuf {
        crate::dirs::kuai_root().join("boot.sh")
    }

    /// 单引号括起来给 sh 用。
    fn quote(text: &str) -> String {
        format!("'{}'", text.replace('\'', r"'\''"))
    }

    /// crontab 里我们的那一行。
    fn line(quoted: &str) -> String {
        format!("@reboot {quoted} >/dev/null 2>&1")
    }

    /// 这一行是不是我们的：没注释掉、启动的是我们的脚本。
    fn ours(line: &str, quoted: &str) -> bool {
        let line = line.trim();
        !line.starts_with('#') && line.contains(quoted)
    }

    fn quoted_script() -> Result<String> {
        let path = script();
        let path = path.to_string_lossy();
        // crontab 里的 % 是换行，路径里有就写不对
        if path.contains(['%', '\n']) {
            bail!(tr("主目录的路径中有 % 或换行符，无法写入 crontab"));
        }
        Ok(quote(&path))
    }

    /// 脚本的内容：写着的那一版不在了就找最新的一个。
    fn script_text(exe: &str) -> String {
        let root = crate::dirs::kuai_root();
        let mut text = String::from(
            "#!/bin/sh\n\
             # Kuai：开机时启动 kuaid。由 kuaid 生成，在 Kuai 的主机页关掉「开机时启动」时删除。\n",
        );
        text.push_str(&format!("exe={}\n", quote(exe)));
        text.push_str(&format!(
            "[ -x \"$exe\" ] || exe=$(ls -td {}/*/kuaid 2>/dev/null | head -n 1)\n",
            quote(&root.to_string_lossy()),
        ));
        text.push_str("[ -n \"$exe\" ] || exit 1\n");
        if let Some(home) = super::isolated_home() {
            text.push_str(&format!(
                "FR_KUAID_HOME={}\nexport FR_KUAID_HOME\n",
                quote(&home)
            ));
        }
        text.push_str("exec \"$exe\" --boot\n");
        text
    }

    fn write_script() -> Result<()> {
        let path = script();
        let text = script_text(&super::current_exe()?);
        if std::fs::read_to_string(&path).is_ok_and(|old| old == text) {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("sh.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("无法写入 {}", tmp.display()))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("无法写入 {}", path.display()))?;
        Ok(())
    }

    /// `crontab -l`。还没有 crontab 时是空的；读不了的报错（不能当成空的，不然写回去就把别的行丢了）。
    fn read() -> Result<String> {
        let out = Command::new("crontab")
            .arg("-l")
            .stdin(Stdio::null())
            .output();
        let out = match out {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!(tr("这台主机没有 crontab 命令，无法设置开机时启动"))
            }
            Err(err) => return Err(err).context(tr("无法运行 crontab")),
            Ok(out) => out,
        };
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        let lower = err.to_lowercase();
        // cronie、Debian 的 cron：no crontab for <用户>；busybox：can't open '<用户>'
        if lower.contains("no crontab") || lower.contains("can't open") {
            return Ok(String::new());
        }
        bail!("{}", tf!("无法读取 crontab：{err}", err = err.trim()))
    }

    /// `crontab -`：整份换掉。
    fn write(text: &str) -> Result<()> {
        let mut child = Command::new("crontab")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context(tr("无法运行 crontab"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("{}", tf!("无法写入 crontab：{err}", err = err.trim()));
        }
        Ok(())
    }

    /// `crontab -r`：整份删掉。关掉以后什么都不剩时用，回到原来没有 crontab 的样子；删不了就写一份空的。
    fn remove() -> Result<()> {
        let out = Command::new("crontab")
            .arg("-r")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context(tr("无法运行 crontab"))?;
        match out.success() {
            true => Ok(()),
            false => write(""),
        }
    }

    pub fn enabled() -> Result<bool> {
        let quoted = quoted_script()?;
        Ok(read()?.lines().any(|line| ours(line, &quoted)))
    }

    pub fn set(on: bool) -> Result<()> {
        let quoted = quoted_script()?;
        let current = read()?;
        let mut lines: Vec<String> = current
            .lines()
            .filter(|line| !ours(line, &quoted))
            .map(str::to_string)
            .collect();
        if on {
            write_script()?;
            lines.push(line(&quoted));
        }
        let mut text = lines.join("\n");
        // cron 要求最后一行有换行
        if !text.is_empty() {
            text.push('\n');
        }
        if text != current {
            match text.trim().is_empty() {
                true => remove()?,
                false => write(&text)?,
            }
        }
        if !on {
            let _ = std::fs::remove_file(script());
        }
        Ok(())
    }

    pub fn refresh() {
        if script().is_file()
            && let Err(err) = write_script()
        {
            crate::daemon::log(&format!("无法更新开机时启动的脚本：{err:#}"));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{line, ours, quote};

        #[test]
        fn crontab_里认得出我们那一行() {
            let quoted = quote("/home/a b/.kuai/boot.sh");
            assert_eq!(quoted, "'/home/a b/.kuai/boot.sh'");
            assert_eq!(quote("it's"), r"'it'\''s'");
            let ours_line = line(&quoted);
            assert_eq!(
                ours_line,
                "@reboot '/home/a b/.kuai/boot.sh' >/dev/null 2>&1"
            );
            assert!(ours(&ours_line, &quoted));
            // 注释掉的、别的脚本的不算
            assert!(!ours(&format!("# {ours_line}"), &quoted));
            assert!(!ours("@reboot '/home/x/.kuai/boot.sh'", &quoted));
            assert!(!ours("0 3 * * * backup.sh", &quoted));
        }
    }
}

#[cfg(windows)]
mod imp {
    use anyhow::{Result, bail};
    use fr_core::i18n::tr;
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegDeleteKeyValueW,
        RegGetValueW, RegSetKeyValueW,
    };

    /// 平时用的两个键：Run，和任务管理器里禁用时记一笔的 StartupApproved。环境变量
    /// `FR_KUAID_AUTOSTART_KEY` 可以换一个（测试用），禁用那一笔跟着记在它后面加 `-approved` 的键里。
    fn keys() -> (String, String) {
        match std::env::var("FR_KUAID_AUTOSTART_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
        {
            Some(key) => (key.clone(), format!("{key}-approved")),
            None => (
                r"Software\Microsoft\Windows\CurrentVersion\Run".to_string(),
                r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"
                    .to_string(),
            ),
        }
    }

    /// Run 键里那一项的名字。测试实例另起一个名字，不碰平时那一项。
    fn value_name() -> String {
        match super::isolated_home() {
            Some(home) => format!("Kuai kuaid ({home})"),
            None => "Kuai kuaid".to_string(),
        }
    }

    fn command() -> Result<String> {
        let exe = super::current_exe()?;
        Ok(match super::isolated_home() {
            Some(home) => format!("\"{exe}\" --boot --home \"{home}\""),
            None => format!("\"{exe}\" --boot"),
        })
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    /// 读一项：没有是 None。
    fn get(key: &str, name: &str, kind: u32) -> Option<Vec<u8>> {
        let (key, name) = (wide(key), wide(name));
        let mut size = 0u32;
        // SAFETY: 先只问大小（缓冲区为空），两个名字都以 0 结尾
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                kind,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if status != 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: 缓冲区和它的大小如实给了
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                kind,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut size,
            )
        };
        (status == 0).then(|| {
            buf.truncate(size as usize);
            buf
        })
    }

    fn get_text(key: &str, name: &str) -> Option<String> {
        let bytes = get(key, name, RRF_RT_REG_SZ)?;
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .take_while(|unit| *unit != 0)
            .collect();
        Some(String::from_utf16_lossy(&units))
    }

    fn set_text(key: &str, name: &str, text: &str) -> Result<()> {
        let (key, name, data) = (wide(key), wide(name), wide(text));
        // SAFETY: 数据以 0 结尾，长度按字节如实给了；键不在时会建上
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if status != 0 {
            bail!(
                "{}（{}）",
                tr("添加启动项失败"),
                std::io::Error::from_raw_os_error(status as i32)
            );
        }
        Ok(())
    }

    /// 删一项；本来就没有不算错。
    fn delete(key: &str, name: &str) -> Result<()> {
        let (key, name) = (wide(key), wide(name));
        // SAFETY: 两个名字都以 0 结尾
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
        // 2：没有这一项，3：没有这个键
        if status != 0 && status != 2 && status != 3 {
            bail!(
                "{}（{}）",
                tr("删除启动项失败"),
                std::io::Error::from_raw_os_error(status as i32)
            );
        }
        Ok(())
    }

    /// StartupApproved 里的值是不是「允许启动」：第一个字节是双数（02、06）为允许，单数（03 等）
    /// 为在任务管理器里禁用了。没有这一项也是允许。
    fn approved(value: Option<&[u8]>) -> bool {
        value
            .and_then(|bytes| bytes.first())
            .is_none_or(|first| first % 2 == 0)
    }

    pub fn enabled() -> Result<bool> {
        let (run, approved_key) = keys();
        let name = value_name();
        let listed = get_text(&run, &name).is_some();
        let value = get(&approved_key, &name, RRF_RT_REG_BINARY);
        Ok(listed && approved(value.as_deref()))
    }

    pub fn set(on: bool) -> Result<()> {
        let (run, approved_key) = keys();
        let name = value_name();
        if on {
            set_text(&run, &name, &command()?)?;
            // 任务管理器里禁用过：不删掉那一笔 Windows 不启动
            delete(&approved_key, &name)?;
        } else {
            delete(&run, &name)?;
            let _ = delete(&approved_key, &name);
        }
        Ok(())
    }

    pub fn refresh() {
        let (run, _) = keys();
        let name = value_name();
        let Some(old) = get_text(&run, &name) else {
            return;
        };
        let Ok(new) = command() else {
            return;
        };
        if !old.eq_ignore_ascii_case(&new)
            && let Err(err) = set_text(&run, &name, &new)
        {
            crate::daemon::log(&format!("无法更新开机时启动的程序：{err:#}"));
        }
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn 任务管理器里禁用了算没开() {
            assert!(super::approved(None));
            assert!(super::approved(Some(&[2, 0, 0])));
            assert!(super::approved(Some(&[6])));
            assert!(!super::approved(Some(&[3, 1, 2])));
        }

        #[test]
        fn 打开关掉都落在注册表里() {
            let key = format!(r"Software\KuaiTest-kuaid-autostart-{}", std::process::id());
            // SAFETY: 测试里只有这一处设这个变量
            unsafe { std::env::set_var("FR_KUAID_AUTOSTART_KEY", &key) };
            assert!(!super::enabled().unwrap());
            super::set(true).unwrap();
            assert!(super::enabled().unwrap());
            let command = super::get_text(&key, &super::value_name()).unwrap();
            assert!(command.ends_with("\" --boot"), "{command}");
            // 在任务管理器里禁用
            let approved = super::wide(&format!("{key}-approved"));
            let name = super::wide(&super::value_name());
            let data = [3u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            // SAFETY: 数据和长度如实给了
            unsafe {
                windows_sys::Win32::System::Registry::RegSetKeyValueW(
                    super::HKEY_CURRENT_USER,
                    approved.as_ptr(),
                    name.as_ptr(),
                    windows_sys::Win32::System::Registry::REG_BINARY,
                    data.as_ptr().cast(),
                    data.len() as u32,
                );
            }
            assert!(!super::enabled().unwrap());
            // 再打开：禁用的那一笔删掉
            super::set(true).unwrap();
            assert!(super::enabled().unwrap());
            super::set(false).unwrap();
            assert!(!super::enabled().unwrap());
            // 收拾测试建的键
            for path in [key.clone(), format!("{key}-approved")] {
                let path = super::wide(&path);
                // SAFETY: 名字以 0 结尾
                unsafe {
                    windows_sys::Win32::System::Registry::RegDeleteKeyW(
                        super::HKEY_CURRENT_USER,
                        path.as_ptr(),
                    );
                }
            }
            unsafe { std::env::remove_var("FR_KUAID_AUTOSTART_KEY") };
        }
    }
}
