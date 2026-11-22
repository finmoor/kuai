//! 服务器的登录密码、私钥口令：存在 Windows 凭据管理器里（「控制面板 › 凭据管理器 › Windows 凭据」
//! 的「普通凭据」），Linux 上存在钥匙圈（Secret Service）里，由系统按登录用户加密，`settings.json`
//! 里不存。
//!
//! 每台服务器一条，名字是 `kuai:server:<服务器 id>`。环境变量 FR_SECRETS_PREFIX 可以换掉
//! 前面那段（测试用，免得测试留下的和真的混在一起）。

/// 凭据名字前面那段。
fn prefix() -> String {
    std::env::var("FR_SECRETS_PREFIX").unwrap_or_else(|_| "kuai:server:".to_string())
}

/// 凭据的名字。
fn target(server_id: &str) -> String {
    format!("{}{server_id}", prefix())
}

/// 这台服务器存着的密码或口令；没存、读不了都是 None。
pub fn get(server_id: &str) -> Option<String> {
    imp::read(&target(server_id)).map(|(secret, _)| secret)
}

/// 连接这台服务器时用的密码或口令。密码只交给存它时的那个地址（存的时候记在「用户名」一栏）：
/// 地址改过就不用，连接时再问，不然旧密码会自动交给新地址（表单里改地址时已经删掉了，这里防的
/// 是别处改的，比如手改了设置文件）。私钥口令只用来解本机的私钥，不发给服务器，不看地址。
pub fn for_server(server: &crate::settings::Server) -> Option<String> {
    let (secret, account) = imp::read(&target(&server.id))?;
    usable(server, &account).then_some(secret)
}

/// 存着的（存的时候记下的地址是 `account`）能不能给这台服务器用（见 [`for_server`]）。
fn usable(server: &crate::settings::Server, account: &str) -> bool {
    server.auth != crate::settings::Auth::Password || account == server.destination
}

/// 存下（已有就换掉）。`account` 只是给人看的：凭据管理器里显示在「用户名」那一栏。
pub fn set(server_id: &str, account: &str, secret: &str) -> Result<(), String> {
    imp::write(&target(server_id), account, secret)
}

/// 删掉；本来就没有也算成。
pub fn delete(server_id: &str) {
    imp::delete(&target(server_id));
}

/// 凭据管理器里存着几条 Kuai 的密码、口令（包括设置里已经没有的服务器留下的）。
pub fn saved_count() -> usize {
    imp::list(&format!("{}*", prefix())).len()
}

/// 把 Kuai 存的密码、口令全删掉（卸载时问过人以后用）。
pub fn forget_all() {
    for name in imp::list(&format!("{}*", prefix())) {
        imp::delete(&name);
    }
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
        fn CredEnumerateW(
            filter: *const u16,
            flags: u32,
            count: *mut u32,
            credentials: *mut *mut *mut Credential,
        ) -> i32;
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
            return Err(fr_core::i18n::tr("内容过长，无法保存到 Windows 凭据管理器").into());
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

    /// 存着的内容和「用户名」一栏。
    pub fn read(target: &str) -> Option<(String, String)> {
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
            let account = from_wide(credential.user_name).unwrap_or_default();
            CredFree(found.cast());
            Some((secret?, account))
        }
    }

    pub fn delete(target: &str) {
        let target = wide(target);
        // SAFETY：只传一个以 0 结尾的名字；没有这条时返回失败，不用管
        unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
    }

    /// 名字和 `filter`（末尾可以带 `*`）对得上的普通凭据的名字。
    pub fn list(filter: &str) -> Vec<String> {
        let filter = wide(filter);
        let mut count = 0u32;
        let mut items: *mut *mut Credential = null_mut();
        // SAFETY：成功时 items 指向 count 个凭据指针，名字是以 0 结尾的 UTF-16；
        // 读完整块用一次 CredFree 还回去。一条都没有时返回失败
        unsafe {
            if CredEnumerateW(filter.as_ptr(), 0, &mut count, &mut items) == 0 || items.is_null() {
                return Vec::new();
            }
            let names = std::slice::from_raw_parts(items, count as usize)
                .iter()
                .filter(|item| !item.is_null() && (***item).kind == CRED_TYPE_GENERIC)
                .filter_map(|item| from_wide((**item).target_name))
                .collect();
            CredFree(items.cast());
            names
        }
    }

    /// SAFETY：`ptr` 为空，或者指向以 0 结尾的 UTF-16
    unsafe fn from_wide(ptr: *const u16) -> Option<String> {
        if ptr.is_null() {
            return None;
        }
        let mut len = 0;
        // SAFETY：见上，读到 0 为止
        while unsafe { *ptr.add(len) } != 0 {
            len += 1;
        }
        // SAFETY：前 len 个都在缓冲区里
        String::from_utf16(unsafe { std::slice::from_raw_parts(ptr, len) }).ok()
    }
}

/// Linux：存进 Secret Service（GNOME 钥匙圈、KWallet，经 D-Bus），和 GPUI 用的是同一个库。每条带
/// 两个属性：`application=kuai`、`kuai-target=<凭据名>`，「用户名」那一栏记在 `account` 属性里。
///
/// 只有存（用户在主机设置里点了保存）时才会弹系统的框：还没有钥匙圈时要设密码新建一个，锁着时
/// （自动登录的桌面不会自动解锁）要密码解锁；用户不给就是没存上。读、删、列出都不弹框：每次连接
/// 都要读，在界面线程上，钥匙圈没有、锁着都当作没存，连接时照常问密码。
#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;

    use oo7::dbus::{Collection, Item};

    const APPLICATION: &str = "kuai";

    type DbusResult<T> = Result<T, oo7::dbus::Error>;

    fn run<T, E>(work: impl std::future::Future<Output = Result<T, E>>) -> Result<T, E> {
        futures::executor::block_on(work)
    }

    /// 默认的钥匙圈，已经解锁的才要：不新建、不弹框解锁。
    async fn unlocked() -> DbusResult<Option<Collection>> {
        let service = oo7::dbus::Service::new().await?;
        let Some(collection) = service.with_alias("default").await? else {
            return Ok(None);
        };
        Ok((!collection.is_locked().await?).then_some(collection))
    }

    async fn find(target: Option<&str>) -> DbusResult<Vec<Item>> {
        let Some(collection) = unlocked().await? else {
            return Ok(Vec::new());
        };
        match target {
            Some(target) => {
                collection
                    .search_items(&[("application", APPLICATION), ("kuai-target", target)])
                    .await
            }
            None => {
                collection
                    .search_items(&[("application", APPLICATION)])
                    .await
            }
        }
    }

    pub fn write(target: &str, account: &str, secret: &str) -> Result<(), String> {
        let attributes = HashMap::from([
            ("application", APPLICATION),
            ("kuai-target", target),
            ("account", account),
        ]);
        run(async {
            let keyring = oo7::Keyring::new().await?;
            keyring.unlock().await?;
            // 同一条（application、kuai-target 一样）先删掉：account 变了的话 replace 换不掉它
            keyring
                .delete(&[("application", APPLICATION), ("kuai-target", target)])
                .await?;
            keyring
                .create_item(&format!("Kuai: {account}"), &attributes, secret, true)
                .await
        })
        .map_err(|err| err.to_string())
    }

    pub fn read(target: &str) -> Option<(String, String)> {
        let found = run(async {
            let Some(item) = find(Some(target)).await?.into_iter().next() else {
                return Ok(None);
            };
            if item.is_locked().await? {
                return Ok(None);
            }
            let secret = item.secret().await?;
            let account = item
                .attributes()
                .await?
                .remove("account")
                .unwrap_or_default();
            DbusResult::Ok(Some((secret, account)))
        });
        match found {
            Ok(Some((secret, account))) => {
                Some((String::from_utf8(secret.to_vec()).ok()?, account))
            }
            Ok(None) => None,
            Err(err) => {
                log::warn!("读不了钥匙圈：{err}");
                None
            }
        }
    }

    pub fn delete(target: &str) {
        let done = run(async {
            for item in find(Some(target)).await? {
                item.delete(None).await?;
            }
            DbusResult::Ok(())
        });
        if let Err(err) = done {
            log::warn!("没删掉钥匙圈里的 {target}：{err}");
        }
    }

    /// 名字匹配 `pattern` 的（只认末尾一个 `*` 当通配，和 Windows 的写法一样）。
    pub fn list(pattern: &str) -> Vec<String> {
        let prefix = pattern.strip_suffix('*');
        let found = run(async {
            let mut names = Vec::new();
            for item in find(None).await? {
                if let Some(name) = item.attributes().await?.remove("kuai-target") {
                    names.push(name);
                }
            }
            DbusResult::Ok(names)
        });
        found
            .unwrap_or_default()
            .into_iter()
            .filter(|name| match prefix {
                Some(prefix) => name.starts_with(prefix),
                None => name == pattern,
            })
            .collect()
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub fn write(_: &str, _: &str, _: &str) -> Result<(), String> {
        Err(fr_core::i18n::tr("当前系统不支持保存密码").into())
    }
    pub fn read(_: &str) -> Option<(String, String)> {
        None
    }
    pub fn delete(_: &str) {}
    pub fn list(_: &str) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::{imp, usable};
    use crate::settings::{Auth, Server};

    #[test]
    fn 密码只交给存它时的地址() {
        let mut server = Server {
            destination: "me@10.0.0.1".into(),
            auth: Auth::Password,
            ..Default::default()
        };
        assert!(usable(&server, "me@10.0.0.1"));
        assert!(!usable(&server, "me@10.0.0.2"));
        // 私钥口令不发给服务器，不看地址
        server.auth = Auth::Key;
        assert!(usable(&server, "me@10.0.0.2"));
    }

    /// 直接给名字，不经 `target()`：测试并行跑，不去改环境变量。
    #[test]
    fn 存进去读得出来_删了就没有() {
        let name = format!("kuai-test:unit-{}", std::process::id());
        imp::delete(&name);
        assert_eq!(imp::read(&name), None);

        imp::write(&name, "tester@example", "p@ss 字 🔑").unwrap();
        assert_eq!(
            imp::read(&name),
            Some(("p@ss 字 🔑".to_string(), "tester@example".to_string()))
        );
        // 再存一次是换掉
        imp::write(&name, "tester@other", "second").unwrap();
        assert_eq!(
            imp::read(&name),
            Some(("second".to_string(), "tester@other".to_string()))
        );

        imp::delete(&name);
        assert_eq!(imp::read(&name), None);
        // 删两次不出错
        imp::delete(&name);
    }

    #[test]
    fn 按前缀列出来() {
        let prefix = format!("kuai-test:list-{}:", std::process::id());
        for id in ["a", "b"] {
            imp::write(&format!("{prefix}{id}"), "tester", "x").unwrap();
        }
        let mut names = imp::list(&format!("{prefix}*"));
        names.sort();
        assert_eq!(names, [format!("{prefix}a"), format!("{prefix}b")]);
        for name in names {
            imp::delete(&name);
        }
        assert!(imp::list(&format!("{prefix}*")).is_empty());
    }

    #[test]
    fn 太长的存不下() {
        let name = format!("kuai-test:long-{}", std::process::id());
        assert!(imp::write(&name, "tester", &"x".repeat(2000)).is_err());
        assert_eq!(imp::read(&name), None);
    }
}

/// 真的读写钥匙圈：要有桌面会话和 Secret Service（钥匙圈锁着时会弹框要密码），平时不跑。
/// `cargo test -p fr-app secrets -- --ignored`，在桌面的终端里运行。
#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::imp;

    #[test]
    #[ignore = "要有桌面会话和钥匙圈"]
    fn 钥匙圈存取删() {
        let name = format!("kuai-test:{}", std::process::id());
        imp::write(&name, "me@10.0.0.1", "第一个 密码").unwrap();
        assert_eq!(
            imp::read(&name),
            Some(("第一个 密码".into(), "me@10.0.0.1".into()))
        );
        // 换掉：地址也跟着换，不留两条
        imp::write(&name, "me@10.0.0.2", "second").unwrap();
        assert_eq!(
            imp::read(&name),
            Some(("second".into(), "me@10.0.0.2".into()))
        );
        assert_eq!(
            imp::list("kuai-test:*")
                .iter()
                .filter(|n| **n == name)
                .count(),
            1
        );
        imp::delete(&name);
        assert_eq!(imp::read(&name), None);
        assert!(!imp::list("kuai-test:*").contains(&name));
        // 钥匙圈锁着时读、删都不弹框，当作没存。测完钥匙圈是锁着的（下次存时会要密码），这一条
        // 删不掉，用固定的名字，重复跑也只留这一条
        let locked = "kuai-test:locked";
        imp::write(locked, "me@10.0.0.1", "x").unwrap();
        futures::executor::block_on(async {
            let service = oo7::dbus::Service::new().await.unwrap();
            let collection = service.with_alias("default").await.unwrap().unwrap();
            collection.lock(None).await.unwrap();
        });
        let started = std::time::Instant::now();
        assert_eq!(imp::read(locked), None);
        assert!(imp::list("kuai-test:*").is_empty());
        imp::delete(locked);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
