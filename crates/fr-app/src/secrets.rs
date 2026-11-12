//! 服务器的登录密码、私钥口令：存在 Windows 凭据管理器里（「控制面板 › 凭据管理器 › Windows 凭据」
//! 的「普通凭据」），由系统按登录用户加密，`settings.json` 里不存。
//!
//! 每台服务器一条，名字是 `kuai:server:<服务器 id>`。环境变量 FR_SECRETS_PREFIX 可以换掉
//! 前面那段（测试用，免得测试留下的和真的混在一起）。

/// 凭据的名字。
fn target(server_id: &str) -> String {
    let prefix =
        std::env::var("FR_SECRETS_PREFIX").unwrap_or_else(|_| "kuai:server:".to_string());
    format!("{prefix}{server_id}")
}

/// 这台服务器存着的密码或口令；没存、读不了都是 None。
pub fn get(server_id: &str) -> Option<String> {
    imp::read(&target(server_id))
}

/// 存下（已有就换掉）。`account` 只是给人看的：凭据管理器里显示在「用户名」那一栏。
pub fn set(server_id: &str, account: &str, secret: &str) -> Result<(), String> {
    imp::write(&target(server_id), account, secret)
}

/// 删掉；本来就没有也算成。
pub fn delete(server_id: &str) {
    imp::delete(&target(server_id));
}

#[cfg(windows)]
mod imp {
    use core::ffi::c_void;
    use std::ptr::{null, null_mut};

    /// `CREDENTIALW`
    #[repr(C)]
    struct Credential {
        flags: u32,
        kind: u32,
        target_name: *const u16,
        comment: *const u16,
        last_written: [u32; 2],
        blob_size: u32,
        blob: *const u8,
        persist: u32,
        attribute_count: u32,
        attributes: *const c_void,
        target_alias: *const u16,
        user_name: *const u16,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn CredWriteW(credential: *const Credential, flags: u32) -> i32;
        fn CredReadW(
            target: *const u16,
            kind: u32,
            flags: u32,
            credential: *mut *mut Credential,
        ) -> i32;
        fn CredDeleteW(target: *const u16, kind: u32, flags: u32) -> i32;
        fn CredFree(buffer: *const c_void);
    }

    const CRED_TYPE_GENERIC: u32 = 1;
    /// 留在这台电脑上，不跟着漫游配置走
    const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;
    /// 系统规定一条凭据的内容最多这么多字节
    const MAX_BLOB: usize = 5 * 512;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    pub fn write(target: &str, account: &str, secret: &str) -> Result<(), String> {
        // 内容按 UTF-16 存，和系统自己、别的程序存的普通凭据一样
        let blob: Vec<u8> = secret.encode_utf16().flat_map(u16::to_le_bytes).collect();
        if blob.len() > MAX_BLOB {
            return Err(fr_core::i18n::tr("太长了，系统的凭据管理器存不下").into());
        }
        let (target, account) = (wide(target), wide(account));
        let credential = Credential {
            flags: 0,
            kind: CRED_TYPE_GENERIC,
            target_name: target.as_ptr(),
            comment: null(),
            last_written: [0; 2],
            blob_size: blob.len() as u32,
            blob: blob.as_ptr(),
            persist: CRED_PERSIST_LOCAL_MACHINE,
            attribute_count: 0,
            attributes: null(),
            target_alias: null(),
            user_name: account.as_ptr(),
        };
        // SAFETY：结构里的指针都指向上面还活着的缓冲区，系统在调用期间把内容拷走
        if unsafe { CredWriteW(&credential, 0) } != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().to_string())
        }
    }

    pub fn read(target: &str) -> Option<String> {
        let target = wide(target);
        let mut found: *mut Credential = null_mut();
        // SAFETY：成功时 found 指向系统分配的一条凭据，读完用 CredFree 还回去
        unsafe {
            if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut found) == 0 || found.is_null()
            {
                return None;
            }
            let credential = &*found;
            let bytes = if credential.blob.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(credential.blob, credential.blob_size as usize)
            };
            let units: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();
            let secret = String::from_utf16(&units).ok();
            CredFree(found.cast());
            secret
        }
    }

    pub fn delete(target: &str) {
        let target = wide(target);
        // SAFETY：只传一个以 0 结尾的名字；没有这条时返回失败，不用管
        unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn write(_: &str, _: &str, _: &str) -> Result<(), String> {
        Err(fr_core::i18n::tr("这个系统上还不能保存密码").into())
    }
    pub fn read(_: &str) -> Option<String> {
        None
    }
    pub fn delete(_: &str) {}
}

#[cfg(all(test, windows))]
mod tests {
    use super::imp;

    /// 直接给名字，不经 `target()`：测试并行跑，不去改环境变量。
    #[test]
    fn 存进去读得出来_删了就没有() {
        let name = format!("kuai-test:unit-{}", std::process::id());
        imp::delete(&name);
        assert_eq!(imp::read(&name), None);

        imp::write(&name, "tester@example", "p@ss 字 🔑").unwrap();
        assert_eq!(imp::read(&name).as_deref(), Some("p@ss 字 🔑"));
        // 再存一次是换掉
        imp::write(&name, "tester@example", "second").unwrap();
        assert_eq!(imp::read(&name).as_deref(), Some("second"));

        imp::delete(&name);
        assert_eq!(imp::read(&name), None);
        // 删两次不出错
        imp::delete(&name);
    }

    #[test]
    fn 太长的存不下() {
        let name = format!("kuai-test:long-{}", std::process::id());
        assert!(imp::write(&name, "tester", &"x".repeat(2000)).is_err());
        assert_eq!(imp::read(&name), None);
    }
}
