//! 服务器的登录密码、私钥口令：存在 Windows 凭据管理器里（「控制面板 › 凭据管理器 › Windows 凭据」
//! 的「普通凭据」），Linux 上存在钥匙圈（Secret Service）里，由系统按登录用户加密，`settings.json`
//! 里不存。
//!
//! 每台服务器一条，名字是 `kuai:server:<服务器 id>`。环境变量 FR_SECRETS_PREFIX 可以换掉
//! 前面那段（测试用，免得测试留下的和真的混在一起）。存的时候还记下它对哪种登录作数（见
//! [`binding`]），取的时候对一遍。

use sha2::{Digest as _, Sha256};

use crate::settings::{Auth, Server};

/// 凭据名字前面那段。
fn prefix() -> String {
    std::env::var("FR_SECRETS_PREFIX").unwrap_or_else(|_| "kuai:server:".to_string())
}

/// 凭据的名字。
fn target(server_id: &str) -> String {
    format!("{}{server_id}", prefix())
}

/// 存着的一条。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    secret: String,
    /// 「用户名」一栏：存的时候的地址，给人看的
    account: String,
    /// 它对哪种登录作数（[`binding`]）；1.23 以前存的没有
    binding: Option<String>,
}

/// 钥匙圈锁着、不可用，读不出来：不知道存没存。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Unavailable;

/// 这台主机存着的密码、口令是什么情况。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Saved {
    /// 存着，连接时会用
    Usable,
    /// 存着，但是给别的登录方式、地址或私钥文件存的，连接时不用
    Stale,
    /// 没存
    Absent,
    /// 读不了（Linux 的钥匙圈锁着、不可用）：不知道存没存
    Unknown,
}

/// 这台主机存着的密码、口令是什么情况（表单里要知道：提示「已保存」、改了登录方式要不要删）。
pub fn saved_for(server: &Server) -> Saved {
    match imp::read(&target(&server.id)) {
        Ok(entries) if entries.is_empty() => Saved::Absent,
        Ok(entries) if entries.iter().any(|entry| usable(server, entry)) => Saved::Usable,
        Ok(_) => Saved::Stale,
        Err(Unavailable) => Saved::Unknown,
    }
}

/// 连接这台服务器时用的密码或口令：只用对得上现在这种登录的（见 [`binding`]），没有就是 None，
/// 连接时再问。
pub fn for_server(server: &Server) -> Option<String> {
    imp::read(&target(&server.id))
        .ok()?
        .into_iter()
        .find(|entry| usable(server, entry))
        .map(|entry| entry.secret)
}

/// 存着的对哪种登录作数：密码对地址，私钥口令对私钥文件（没指定的是默认密钥）。存的时候记下
/// 这个摘要，取的时候对一遍，登录方式、地址、私钥文件改过就不用：私钥口令不会被当成密码交给
/// 服务器，旧密码也不会交给新地址。表单里改的时候已经删掉了，这里防的是没删掉的：钥匙圈锁着
/// 删不了，手改了设置文件。存摘要是因为 Windows 凭据的备注一栏最多 256 个字符，放不下长路径。
fn binding(server: &Server) -> String {
    let (kind, value) = match server.auth {
        Auth::Password => ("password", server.destination.as_str()),
        Auth::Key => ("key", server.key_file().unwrap_or_default()),
    };
    let digest = Sha256::digest(format!("{kind}\n{value}").as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("v1:{hex}")
}

/// 存着的这条能不能给这台服务器用（见 [`binding`]）。
fn usable(server: &Server, entry: &Entry) -> bool {
    match &entry.binding {
        Some(saved) => *saved == binding(server),
        // 1.23 以前存的没记对哪种登录作数。Windows 上存、删都是当场做完的，留下的就是最后一次保存时
        // 那种登录方式的，按原来的规则认：密码只交给存它时的地址。Linux 上钥匙圈锁着时表单删不掉
        // 旧的，留下的可能是另一种登录方式的，不用，连接时再问
        None => {
            cfg!(windows) && (server.auth != Auth::Password || entry.account == server.destination)
        }
    }
}

/// 存下（已有就换掉），记下它对哪种登录作数。可能要等：Linux 上还没有钥匙圈、钥匙圈锁着时弹系统的
/// 框等用户，不在界面线程上调。
pub fn set(server: &Server, secret: &str) -> Result<(), String> {
    imp::write(
        &target(&server.id),
        &server.destination,
        &binding(server),
        secret,
    )
}

/// 删掉；本来就没有也算成，删不掉（Linux 上钥匙圈锁着、用户没给解锁）报错。用户点了「清除」时用：
/// 锁着就弹框要解锁，不在界面线程上调。
pub fn forget(server_id: &str) -> Result<(), String> {
    imp::delete(&target(server_id), true)
}

/// 删掉，不弹框：钥匙圈锁着就留着（它只对存它时的那种登录作数，留着也不会交错）。
pub fn delete(server_id: &str) {
    if let Err(err) = imp::delete(&target(server_id), false) {
        log::warn!("没删掉服务器 {server_id} 存着的密码、口令：{err}");
    }
}

/// 凭据管理器里存着几条 Kuai 的密码、口令（包括设置里已经没有的服务器留下的）。
pub fn saved_count() -> usize {
    imp::list(&format!("{}*", prefix())).len()
}

/// 把 Kuai 存的密码、口令全删掉（卸载时问过人以后用）。
pub fn forget_all() {
    for name in imp::list(&format!("{}*", prefix())) {
        if let Err(err) = imp::delete(&name, false) {
            log::warn!("没删掉 {name}：{err}");
        }
    }
}

#[cfg(windows)]
mod imp {
    use core::ffi::c_void;
    use std::ptr::{null, null_mut};

    use super::{Entry, Unavailable};

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

    /// 没有这条凭据
    const ERROR_NOT_FOUND: i32 = 1168;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    /// 存下（同名的整条换掉，系统一次做完）。`binding` 记在备注一栏（凭据管理器里不显示）。
    pub fn write(target: &str, account: &str, binding: &str, secret: &str) -> Result<(), String> {
        // 内容按 UTF-16 存，和系统自己、别的程序存的普通凭据一样
        let blob: Vec<u8> = secret.encode_utf16().flat_map(u16::to_le_bytes).collect();
        if blob.len() > MAX_BLOB {
            return Err(fr_core::i18n::tr("内容过长，无法保存到 Windows 凭据管理器").into());
        }
        let (target, account, binding) = (wide(target), wide(account), wide(binding));
        let credential = Credential {
            flags: 0,
            kind: CRED_TYPE_GENERIC,
            target_name: target.as_ptr(),
            comment: binding.as_ptr(),
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

    /// 存着的那条（同名的只有一条）；没有是空的，读不了（不是「没有这条」的错）是 Err。内容不是
    /// UTF-16 文本的当作没有。
    pub fn read(target: &str) -> Result<Vec<Entry>, Unavailable> {
        let target = wide(target);
        let mut found: *mut Credential = null_mut();
        // SAFETY：成功时 found 指向系统分配的一条凭据，读完用 CredFree 还回去
        unsafe {
            if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut found) == 0 || found.is_null()
            {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(ERROR_NOT_FOUND) {
                    return Ok(Vec::new());
                }
                log::warn!("读不了凭据管理器：{err}");
                return Err(Unavailable);
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
            let binding = from_wide(credential.comment).filter(|text| !text.is_empty());
            CredFree(found.cast());
            Ok(secret
                .map(|secret| Entry {
                    secret,
                    account,
                    binding,
                })
                .into_iter()
                .collect())
        }
    }

    /// 删掉；本来就没有也算成。凭据管理器不会锁，`_ask` 用不着。
    pub fn delete(target: &str, _ask: bool) -> Result<(), String> {
        let target = wide(target);
        // SAFETY：只传一个以 0 结尾的名字
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } != 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() == Some(ERROR_NOT_FOUND) {
            true => Ok(()),
            false => Err(err.to_string()),
        }
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
/// 属性 `application=kuai`、`kuai-target=<凭据名>`；「用户名」那一栏记在 `account` 属性里，对哪种
/// 登录作数记在 `kuai-binding` 里。
///
/// 只有存、用户点了「清除」时才会弹系统的框：还没有钥匙圈时要设密码新建一个（只有存时），锁着时
/// （自动登录的桌面不会自动解锁）要密码解锁；用户不给就是没存上、没删掉。这两样在后台线程上做。
/// 读、列出、别的删除不弹框：每次连接都要读，在界面线程上；还没有钥匙圈当作没存，锁着当作不知道
/// （连接时照常问密码，表单里不提示「已保存」）。
#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;

    use oo7::dbus::Collection;

    use super::{Entry, Unavailable};

    const APPLICATION: &str = "kuai";

    type DbusResult<T> = Result<T, oo7::dbus::Error>;

    fn run<T, E>(work: impl std::future::Future<Output = Result<T, E>>) -> Result<T, E> {
        futures::executor::block_on(work)
    }

    /// 一条凭据（不管存的时候的地址、登录方式）。
    fn mine(target: &str) -> [(&'static str, &str); 2] {
        [("application", APPLICATION), ("kuai-target", target)]
    }

    /// 默认的钥匙圈和它锁没锁；还没有是 None。不新建、不弹框解锁。
    async fn default_keyring() -> DbusResult<Option<(Collection, bool)>> {
        let service = oo7::dbus::Service::new().await?;
        let Some(collection) = service.with_alias("default").await? else {
            return Ok(None);
        };
        let locked = collection.is_locked().await?;
        Ok(Some((collection, locked)))
    }

    pub fn write(target: &str, account: &str, binding: &str, secret: &str) -> Result<(), String> {
        let attributes = HashMap::from([
            ("application", APPLICATION),
            ("kuai-target", target),
            ("account", account),
            ("kuai-binding", binding),
        ]);
        let wanted: HashMap<String, String> = attributes
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        run(async {
            let keyring = oo7::Keyring::new().await?;
            keyring.unlock().await?;
            // 先存新的，存上了再删旧的：存不上的话旧的还在。属性完全一样的是同一条，replace 当场
            // 换掉它；地址、登录方式变了是另一条，存好以后删掉
            let old = keyring.search_items(&mine(target)).await?;
            keyring
                .create_item(&format!("Kuai: {account}"), &attributes, secret, true)
                .await?;
            for item in old {
                // 读不出属性的不删：可能就是刚换掉的那条。删不掉也不算没存上，读的时候先用新存的。
                // 只比我们写的那几项：钥匙圈会自己加别的（GNOME 加 xdg:schema），原来整个比，刚换掉
                // 的那条也当成旧的删了
                match item.attributes().await {
                    Ok(found)
                        if wanted
                            .iter()
                            .all(|(key, value)| found.get(key) == Some(value)) => {}
                    Ok(_) => {
                        if let Err(err) = item.delete().await {
                            log::warn!("没删掉钥匙圈里 {target} 原来的那条：{err}");
                        }
                    }
                    Err(err) => log::warn!("读不了钥匙圈里 {target} 原来那条的属性：{err}"),
                }
            }
            oo7::Result::Ok(())
        })
        .map_err(|err| err.to_string())
    }

    /// 存着的，新存的在前；还没有钥匙圈是空的，锁着、读不了是 Err。
    pub fn read(target: &str) -> Result<Vec<Entry>, Unavailable> {
        let found = run(async {
            let Some((collection, locked)) = default_keyring().await? else {
                return Ok(Ok(Vec::new()));
            };
            if locked {
                return Ok(Err(Unavailable));
            }
            let mut entries = Vec::new();
            for item in collection.search_items(&mine(target)).await? {
                if item.is_locked().await? {
                    return Ok(Err(Unavailable));
                }
                let mut attributes = item.attributes().await?;
                let Ok(secret) = String::from_utf8(item.secret().await?.to_vec()) else {
                    continue;
                };
                let entry = Entry {
                    secret,
                    account: attributes.remove("account").unwrap_or_default(),
                    binding: attributes
                        .remove("kuai-binding")
                        .filter(|text| !text.is_empty()),
                };
                entries.push((item.modified().await?, entry));
            }
            entries.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
            DbusResult::Ok(Ok(entries.into_iter().map(|(_, entry)| entry).collect()))
        });
        found.unwrap_or_else(|err| {
            log::warn!("读不了钥匙圈：{err}");
            Err(Unavailable)
        })
    }

    /// 删掉这个名字的（不管存的时候的地址、登录方式）。钥匙圈锁着时 `ask` 就弹框要解锁，不然报错。
    pub fn delete(target: &str, ask: bool) -> Result<(), String> {
        let done = run(async {
            let Some((collection, locked)) = default_keyring().await? else {
                return Ok(Ok(()));
            };
            if locked {
                if !ask {
                    return Ok(Err(fr_core::i18n::tr("钥匙圈已锁定").to_string()));
                }
                collection.unlock(None).await?;
            }
            for item in collection.search_items(&mine(target)).await? {
                item.delete(None).await?;
            }
            DbusResult::Ok(Ok(()))
        });
        done.unwrap_or_else(|err| Err(err.to_string()))
    }

    /// 名字匹配 `pattern` 的（只认末尾一个 `*` 当通配，和 Windows 的写法一样）。钥匙圈锁着时是空的。
    pub fn list(pattern: &str) -> Vec<String> {
        let prefix = pattern.strip_suffix('*');
        let found = run(async {
            let mut names = Vec::new();
            let Some((collection, false)) = default_keyring().await? else {
                return Ok(names);
            };
            for item in collection
                .search_items(&[("application", APPLICATION)])
                .await?
            {
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
    use super::{Entry, Unavailable};

    pub fn write(_: &str, _: &str, _: &str, _: &str) -> Result<(), String> {
        Err(fr_core::i18n::tr("当前系统不支持保存密码").into())
    }
    pub fn read(_: &str) -> Result<Vec<Entry>, Unavailable> {
        Ok(Vec::new())
    }
    pub fn delete(_: &str, _: bool) -> Result<(), String> {
        Ok(())
    }
    pub fn list(_: &str) -> Vec<String> {
        Vec::new()
    }
}

/// 存着的对哪种登录作数，各平台一样。
#[cfg(test)]
mod binding_tests {
    use super::{Entry, binding, usable};
    use crate::settings::{Auth, Server};

    fn saved_for(server: &Server) -> Entry {
        Entry {
            secret: "s".into(),
            account: server.destination.clone(),
            binding: Some(binding(server)),
        }
    }

    #[test]
    fn 只给存它时的那种登录用() {
        let password = Server {
            destination: "me@10.0.0.1".into(),
            auth: Auth::Password,
            identity_file: Some("~/.ssh/a".into()),
            ..Default::default()
        };
        let entry = saved_for(&password);
        assert!(usable(&password, &entry));
        // 地址改了：旧密码不交给新地址
        let moved = Server {
            destination: "me@10.0.0.2".into(),
            ..password.clone()
        };
        assert!(!usable(&moved, &entry));
        // 换成私钥登录：密码不当口令用
        let key = Server {
            auth: Auth::Key,
            ..password.clone()
        };
        assert!(!usable(&key, &entry));
        // 私钥口令：换成密码登录不当密码交给服务器，同一个地址也不行
        let passphrase = saved_for(&key);
        assert!(usable(&key, &passphrase));
        assert!(!usable(&password, &passphrase));
        // 换了私钥文件不用；地址改了照用（口令只解本机的私钥，不发给服务器）
        let other_key = Server {
            identity_file: Some("~/.ssh/b".into()),
            ..key.clone()
        };
        assert!(!usable(&other_key, &passphrase));
        let key_moved = Server {
            destination: "me@10.0.0.2".into(),
            ..key.clone()
        };
        assert!(usable(&key_moved, &passphrase));
    }

    #[test]
    fn 以前存的没记登录方式() {
        let server = Server {
            destination: "me@10.0.0.1".into(),
            auth: Auth::Password,
            ..Default::default()
        };
        let old = |account: &str| Entry {
            secret: "s".into(),
            account: account.into(),
            binding: None,
        };
        // Windows 上按原来的规则认（密码只交给存它时的地址），Linux 上不用
        assert_eq!(usable(&server, &old("me@10.0.0.1")), cfg!(windows));
        assert!(!usable(&server, &old("me@10.0.0.2")));
        let key = Server {
            auth: Auth::Key,
            ..server.clone()
        };
        assert_eq!(usable(&key, &old("me@10.0.0.2")), cfg!(windows));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::{Entry, imp};

    fn entry(secret: &str, account: &str, binding: Option<&str>) -> Entry {
        Entry {
            secret: secret.into(),
            account: account.into(),
            binding: binding.map(str::to_string),
        }
    }

    /// 直接给名字，不经 `target()`：测试并行跑，不去改环境变量。
    #[test]
    fn 存进去读得出来_删了就没有() {
        let name = format!("kuai-test:unit-{}", std::process::id());
        imp::delete(&name, false).unwrap();
        assert_eq!(imp::read(&name), Ok(vec![]));

        imp::write(&name, "tester@example", "v1:aa", "p@ss 字 🔑").unwrap();
        assert_eq!(
            imp::read(&name),
            Ok(vec![entry("p@ss 字 🔑", "tester@example", Some("v1:aa"))])
        );
        // 再存一次是换掉，对哪种登录作数也跟着换
        imp::write(&name, "tester@other", "v1:bb", "second").unwrap();
        assert_eq!(
            imp::read(&name),
            Ok(vec![entry("second", "tester@other", Some("v1:bb"))])
        );
        // 以前存的备注一栏是空的
        imp::write(&name, "tester@other", "", "third").unwrap();
        assert_eq!(
            imp::read(&name),
            Ok(vec![entry("third", "tester@other", None)])
        );

        imp::delete(&name, false).unwrap();
        assert_eq!(imp::read(&name), Ok(vec![]));
        // 删两次不出错
        imp::delete(&name, true).unwrap();
    }

    #[test]
    fn 按前缀列出来() {
        let prefix = format!("kuai-test:list-{}:", std::process::id());
        for id in ["a", "b"] {
            imp::write(&format!("{prefix}{id}"), "tester", "v1:aa", "x").unwrap();
        }
        let mut names = imp::list(&format!("{prefix}*"));
        names.sort();
        assert_eq!(names, [format!("{prefix}a"), format!("{prefix}b")]);
        for name in names {
            imp::delete(&name, false).unwrap();
        }
        assert!(imp::list(&format!("{prefix}*")).is_empty());
    }

    #[test]
    fn 太长的存不下() {
        let name = format!("kuai-test:long-{}", std::process::id());
        assert!(imp::write(&name, "tester", "v1:aa", &"x".repeat(2000)).is_err());
        assert_eq!(imp::read(&name), Ok(vec![]));
    }
}

/// 真的读写钥匙圈：要有桌面会话和 Secret Service（钥匙圈锁着时会弹框要密码），平时不跑。
/// `cargo test -p fr-app secrets -- --ignored`，在桌面的终端里运行。
#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::{Entry, Unavailable, imp};

    fn entry(secret: &str, account: &str, binding: &str) -> Entry {
        Entry {
            secret: secret.into(),
            account: account.into(),
            binding: Some(binding.into()),
        }
    }

    #[test]
    #[ignore = "要有桌面会话和钥匙圈"]
    fn 钥匙圈存取删() {
        let name = format!("kuai-test:{}", std::process::id());
        imp::write(&name, "me@10.0.0.1", "v1:aa", "第一个 密码").unwrap();
        assert_eq!(
            imp::read(&name),
            Ok(vec![entry("第一个 密码", "me@10.0.0.1", "v1:aa")])
        );
        // 同样的地址、登录方式再存：同一条换掉
        imp::write(&name, "me@10.0.0.1", "v1:aa", "又一个").unwrap();
        assert_eq!(
            imp::read(&name),
            Ok(vec![entry("又一个", "me@10.0.0.1", "v1:aa")])
        );
        // 地址、登录方式变了：存上新的、删掉旧的，不留两条
        imp::write(&name, "me@10.0.0.2", "v1:bb", "second").unwrap();
        assert_eq!(
            imp::read(&name),
            Ok(vec![entry("second", "me@10.0.0.2", "v1:bb")])
        );
        assert_eq!(
            imp::list("kuai-test:*")
                .iter()
                .filter(|n| **n == name)
                .count(),
            1
        );
        imp::delete(&name, false).unwrap();
        assert_eq!(imp::read(&name), Ok(vec![]));
        assert!(!imp::list("kuai-test:*").contains(&name));
        // 钥匙圈锁着时读、列出、不问人的删除都不弹框：读出来是「不知道」，删除报错。测完钥匙圈是
        // 锁着的（下次存时会要密码），这一条删不掉，用固定的名字，重复跑也只留这一条
        let locked = "kuai-test:locked";
        imp::write(locked, "me@10.0.0.1", "v1:aa", "x").unwrap();
        futures::executor::block_on(async {
            let service = oo7::dbus::Service::new().await.unwrap();
            let collection = service.with_alias("default").await.unwrap().unwrap();
            collection.lock(None).await.unwrap();
        });
        let started = std::time::Instant::now();
        assert_eq!(imp::read(locked), Err(Unavailable));
        assert!(imp::list("kuai-test:*").is_empty());
        assert!(imp::delete(locked, false).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
