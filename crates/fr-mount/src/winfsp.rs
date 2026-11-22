//! WinFsp 的薄绑定：只有用到的那几样，照 WinFsp 2.1 的 `winfsp.h`、`fsctl.h` 手写。
//! 运行时从安装目录加载 `winfsp-x64.dll`（注册表里记着装在哪），没装 WinFsp 时程序照样能启动。
//!
//! WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos
//! https://github.com/winfsp/winfsp
#![allow(
    non_snake_case,
    dead_code,
    clippy::missing_transmute_annotations,
    clippy::upper_case_acronyms
)]

use std::ffi::c_void;

use anyhow::{Context as _, Result, bail};
use fr_core::i18n::tr;
use fr_core::tf;

pub type NTSTATUS = i32;
pub type PWSTR = *mut u16;
/// `FSP_FILE_SYSTEM`：当不透明的用，只碰 `UserContext`（见 [`set_user_context`]）。
pub type FileSystem = c_void;

const fn nt(code: u32) -> NTSTATUS {
    code as i32
}

pub const STATUS_SUCCESS: NTSTATUS = 0;
pub const STATUS_BUFFER_OVERFLOW: NTSTATUS = nt(0x8000_0005);
pub const STATUS_INVALID_DEVICE_REQUEST: NTSTATUS = nt(0xC000_0010);
pub const STATUS_END_OF_FILE: NTSTATUS = nt(0xC000_0011);
pub const STATUS_ACCESS_DENIED: NTSTATUS = nt(0xC000_0022);
pub const STATUS_OBJECT_NAME_INVALID: NTSTATUS = nt(0xC000_0033);
pub const STATUS_SHARING_VIOLATION: NTSTATUS = nt(0xC000_0043);
pub const STATUS_DISK_FULL: NTSTATUS = nt(0xC000_007F);
pub const STATUS_NOT_A_DIRECTORY: NTSTATUS = nt(0xC000_0103);
pub const STATUS_CONNECTION_DISCONNECTED: NTSTATUS = nt(0xC000_020C);
pub const STATUS_MEDIA_WRITE_PROTECTED: NTSTATUS = nt(0xC000_00A2);
pub const STATUS_OBJECT_NAME_NOT_FOUND: NTSTATUS = nt(0xC000_0034);
pub const STATUS_OBJECT_NAME_COLLISION: NTSTATUS = nt(0xC000_0035);
pub const STATUS_FILE_IS_A_DIRECTORY: NTSTATUS = nt(0xC000_00BA);
pub const STATUS_UNEXPECTED_IO_ERROR: NTSTATUS = nt(0xC000_00E9);
pub const STATUS_IO_TIMEOUT: NTSTATUS = nt(0xC000_00B5);
pub const STATUS_DIRECTORY_NOT_EMPTY: NTSTATUS = nt(0xC000_0101);

pub const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
/// 打开时要的权限：写数据、追加（`GrantedAccess` 里的位）
pub const FILE_WRITE_DATA: u32 = 0x2;
pub const FILE_APPEND_DATA: u32 = 0x4;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
/// `Create` 的 `CreateOptions` 里：要建的是目录。
pub const FILE_DIRECTORY_FILE: u32 = 0x1;
/// `Cleanup` 的 `Flags` 里：这个文件要删掉。
pub const CLEANUP_DELETE: u32 = 0x01;

/// `FSP_FSCTL_VOLUME_PARAMS` 第一组标志位（C 里是位域，从最低位排起）。
pub mod flag {
    pub const CASE_SENSITIVE_SEARCH: u32 = 1 << 0;
    pub const CASE_PRESERVED_NAMES: u32 = 1 << 1;
    pub const UNICODE_ON_DISK: u32 = 1 << 2;
    pub const PERSISTENT_ACLS: u32 = 1 << 3;
    pub const READ_ONLY_VOLUME: u32 = 1 << 9;
    pub const POST_CLEANUP_WHEN_MODIFIED_ONLY: u32 = 1 << 10;
    pub const FLUSH_AND_PURGE_ON_CLEANUP: u32 = 1 << 14;
    /// 回调里的 `FileContext` 是「每次打开一份」的（UserContext2）。不设的话 WinFsp 当它是
    /// 「每个文件一份」：同一个文件打开两次，后一次的操作会带着前一次的上下文来，关两遍。
    pub const UM_FILE_CONTEXT_IS_USER_CONTEXT2: u32 = 1 << 16;
}

/// `FSP_FSCTL_VOLUME_PARAMS`。
#[repr(C)]
pub struct VolumeParams {
    pub Version: u16,
    pub SectorSize: u16,
    pub SectorsPerAllocationUnit: u16,
    pub MaxComponentLength: u16,
    pub VolumeCreationTime: u64,
    pub VolumeSerialNumber: u32,
    pub TransactTimeout: u32,
    pub IrpTimeout: u32,
    pub IrpCapacity: u32,
    pub FileInfoTimeout: u32,
    pub Flags: u32,
    pub Prefix: [u16; 192],
    pub FileSystemName: [u16; 16],
    pub Flags1: u32,
    pub VolumeInfoTimeout: u32,
    pub DirInfoTimeout: u32,
    pub SecurityTimeout: u32,
    pub StreamInfoTimeout: u32,
    pub EaTimeout: u32,
    pub FsextControlCode: u32,
    pub Reserved32: [u32; 1],
    pub Reserved64: [u64; 2],
}
const _: () = assert!(size_of::<VolumeParams>() == 504);

/// `FSP_FSCTL_VOLUME_INFO`。
#[repr(C)]
pub struct VolumeInfo {
    pub TotalSize: u64,
    pub FreeSize: u64,
    pub VolumeLabelLength: u16,
    pub VolumeLabel: [u16; 32],
}
const _: () = assert!(size_of::<VolumeInfo>() == 88);

/// `FSP_FSCTL_FILE_INFO`。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FileInfo {
    pub FileAttributes: u32,
    pub ReparseTag: u32,
    pub AllocationSize: u64,
    pub FileSize: u64,
    pub CreationTime: u64,
    pub LastAccessTime: u64,
    pub LastWriteTime: u64,
    pub ChangeTime: u64,
    pub IndexNumber: u64,
    pub HardLinks: u32,
    pub EaSize: u32,
}
const _: () = assert!(size_of::<FileInfo>() == 72);

/// `FSP_FSCTL_DIR_INFO` 的定长部分，后面紧跟文件名（UTF-16，不带结尾的 0）。
#[repr(C)]
pub struct DirInfo {
    pub Size: u16,
    pub FileInfo: FileInfo,
    pub Padding: [u8; 24],
}
const _: () = assert!(size_of::<DirInfo>() == 104);

type Ctx = *mut c_void;
type Fs = *mut FileSystem;

/// `FSP_FILE_SYSTEM_INTERFACE`：64 个函数指针，顺序和头文件里一样。用不到的留空。
#[repr(C)]
pub struct Interface {
    pub GetVolumeInfo: Option<unsafe extern "C" fn(Fs, *mut VolumeInfo) -> NTSTATUS>,
    pub SetVolumeLabel: usize,
    pub GetSecurityByName:
        Option<unsafe extern "C" fn(Fs, PWSTR, *mut u32, *mut c_void, *mut usize) -> NTSTATUS>,
    #[allow(clippy::type_complexity)]
    pub Create: Option<
        unsafe extern "C" fn(
            Fs,
            PWSTR,
            u32,
            u32,
            u32,
            *mut c_void,
            u64,
            *mut Ctx,
            *mut FileInfo,
        ) -> NTSTATUS,
    >,
    pub Open:
        Option<unsafe extern "C" fn(Fs, PWSTR, u32, u32, *mut Ctx, *mut FileInfo) -> NTSTATUS>,
    pub Overwrite: Option<unsafe extern "C" fn(Fs, Ctx, u32, u8, u64, *mut FileInfo) -> NTSTATUS>,
    pub Cleanup: Option<unsafe extern "C" fn(Fs, Ctx, PWSTR, u32)>,
    pub Close: Option<unsafe extern "C" fn(Fs, Ctx)>,
    pub Read: Option<unsafe extern "C" fn(Fs, Ctx, *mut c_void, u64, u32, *mut u32) -> NTSTATUS>,
    #[allow(clippy::type_complexity)]
    pub Write: Option<
        unsafe extern "C" fn(
            Fs,
            Ctx,
            *mut c_void,
            u64,
            u32,
            u8,
            u8,
            *mut u32,
            *mut FileInfo,
        ) -> NTSTATUS,
    >,
    pub Flush: Option<unsafe extern "C" fn(Fs, Ctx, *mut FileInfo) -> NTSTATUS>,
    pub GetFileInfo: Option<unsafe extern "C" fn(Fs, Ctx, *mut FileInfo) -> NTSTATUS>,
    pub SetBasicInfo:
        Option<unsafe extern "C" fn(Fs, Ctx, u32, u64, u64, u64, u64, *mut FileInfo) -> NTSTATUS>,
    pub SetFileSize: Option<unsafe extern "C" fn(Fs, Ctx, u64, u8, *mut FileInfo) -> NTSTATUS>,
    pub CanDelete: Option<unsafe extern "C" fn(Fs, Ctx, PWSTR) -> NTSTATUS>,
    pub Rename: Option<unsafe extern "C" fn(Fs, Ctx, PWSTR, PWSTR, u8) -> NTSTATUS>,
    pub GetSecurity: Option<unsafe extern "C" fn(Fs, Ctx, *mut c_void, *mut usize) -> NTSTATUS>,
    pub SetSecurity: usize,
    pub ReadDirectory:
        Option<unsafe extern "C" fn(Fs, Ctx, PWSTR, PWSTR, *mut c_void, u32, *mut u32) -> NTSTATUS>,
    /// 第 19 到 63 个：重解析点、命名流、扩展属性这些，都不实现
    pub Rest: [usize; 45],
}
const _: () = assert!(size_of::<Interface>() == 64 * size_of::<usize>());

/// 从 DLL 里拿出来的那几个函数。
#[derive(Clone, Copy)]
pub struct Api {
    pub FspFileSystemCreate: unsafe extern "C" fn(
        PWSTR,
        *const VolumeParams,
        *const Interface,
        *mut *mut FileSystem,
    ) -> NTSTATUS,
    pub FspFileSystemDelete: unsafe extern "C" fn(*mut FileSystem),
    pub FspFileSystemSetMountPoint: unsafe extern "C" fn(*mut FileSystem, PWSTR) -> NTSTATUS,
    pub FspFileSystemRemoveMountPoint: unsafe extern "C" fn(*mut FileSystem),
    pub FspFileSystemStartDispatcher: unsafe extern "C" fn(*mut FileSystem, u32) -> NTSTATUS,
    pub FspFileSystemStopDispatcher: unsafe extern "C" fn(*mut FileSystem),
    pub FspFileSystemAddDirInfo:
        unsafe extern "C" fn(*mut DirInfo, *mut c_void, u32, *mut u32) -> u8,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn LocalFree(mem: *mut c_void) -> *mut c_void;
    fn GetCurrentProcess() -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
    fn GetTokenInformation(
        token: *mut c_void,
        class: u32,
        info: *mut c_void,
        len: u32,
        returned: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *mut c_void, text: *mut *mut u16) -> i32;
    fn RegGetValueW(
        key: isize,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        kind: *mut u32,
        data: *mut c_void,
        size: *mut u32,
    ) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sddl: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
}

/// 结尾带 0 的 UTF-16。
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// 读 C 给的 UTF-16 字符串（到 0 为止）。
///
/// # Safety
/// `text` 要么是空指针，要么指向以 0 结尾的 UTF-16。
pub unsafe fn wide_slice<'a>(text: *const u16) -> &'a [u16] {
    if text.is_null() {
        return &[];
    }
    let mut len = 0;
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
        std::slice::from_raw_parts(text, len)
    }
}

/// WinFsp 装在哪（安装程序写在注册表里）。
pub fn install_dir() -> Result<String> {
    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002u32 as i32 as isize;
    const RRF_RT_REG_SZ: u32 = 0x2;
    let mut buffer = [0u16; 520];
    let mut size = size_of_val(&buffer) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            wide(r"SOFTWARE\WOW6432Node\WinFsp").as_ptr(),
            wide("InstallDir").as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != 0 {
        bail!("{}", tr("未安装 WinFsp"));
    }
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Ok(String::from_utf16_lossy(&buffer[..len]))
}

impl Api {
    /// 加载 WinFsp 的 DLL，拿到要用的函数。
    pub fn load() -> Result<Api> {
        let dir = install_dir()?;
        let dll = format!(r"{}\bin\winfsp-x64.dll", dir.trim_end_matches('\\'));
        let module = unsafe { LoadLibraryW(wide(&dll).as_ptr()) };
        if module.is_null() {
            let err = std::io::Error::last_os_error();
            bail!("{}", tf!("无法加载 {dll}：{err}", dll = &dll, err));
        }
        let find = |name: &str| -> Result<*mut c_void> {
            let symbol = format!("{name}\0");
            let found = unsafe { GetProcAddress(module, symbol.as_ptr()) };
            if found.is_null() {
                let err = format!("{name} ?");
                bail!("{}", tf!("无法加载 {dll}：{err}", dll = &dll, err));
            }
            Ok(found)
        };
        // 函数指针和 `*mut c_void` 一样大，签名照头文件写在 `Api` 的字段上
        unsafe {
            Ok(Api {
                FspFileSystemCreate: std::mem::transmute::<*mut c_void, _>(find(
                    "FspFileSystemCreate",
                )?),
                FspFileSystemDelete: std::mem::transmute::<*mut c_void, _>(find(
                    "FspFileSystemDelete",
                )?),
                FspFileSystemSetMountPoint: std::mem::transmute::<*mut c_void, _>(find(
                    "FspFileSystemSetMountPoint",
                )?),
                FspFileSystemRemoveMountPoint: std::mem::transmute::<*mut c_void, _>(find(
                    "FspFileSystemRemoveMountPoint",
                )?),
                FspFileSystemStartDispatcher: std::mem::transmute::<*mut c_void, _>(find(
                    "FspFileSystemStartDispatcher",
                )?),
                FspFileSystemStopDispatcher: std::mem::transmute::<*mut c_void, _>(find(
                    "FspFileSystemStopDispatcher",
                )?),
                FspFileSystemAddDirInfo: std::mem::transmute::<*mut c_void, _>(find(
                    "FspFileSystemAddDirInfo",
                )?),
            })
        }
    }
}

/// `FSP_FILE_SYSTEM::UserContext`：结构体开头是 `UINT16 Version`，然后就是它，64 位上偏移 8。
///
/// # Safety
/// `fs` 是 `FspFileSystemCreate` 给的。
pub unsafe fn set_user_context(fs: *mut FileSystem, context: *mut c_void) {
    unsafe { *fs.cast::<u8>().add(8).cast::<*mut c_void>() = context }
}

/// # Safety
/// `fs` 是 `FspFileSystemCreate` 给的。
pub unsafe fn user_context(fs: *mut FileSystem) -> *mut c_void {
    unsafe { *fs.cast::<u8>().add(8).cast::<*mut c_void>() }
}

/// 盘上文件的安全描述符写成 SDDL：系统、管理员和 `user`（SID）能读写，`user` 是 None 时给所有人。
fn sddl(user: Option<&str>) -> String {
    let user = user.unwrap_or("WD");
    format!("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{user})")
}

/// 当前用户的 SID（`S-1-5-21-…`）；取不到是 None。
fn current_user_sid() -> Option<String> {
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_USER: u32 = 1;
    // SAFETY：句柄用完关掉；缓冲区按 8 字节对齐（TOKEN_USER 开头是指针），系统写进去的 SID
    // 在缓冲区里；转出来的字符串以 0 结尾，读完用 LocalFree 还回去
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut len = 0u32;
        GetTokenInformation(token, TOKEN_USER, std::ptr::null_mut(), 0, &mut len);
        let mut buffer = vec![0u64; (len as usize).div_ceil(8).max(1)];
        let got = GetTokenInformation(token, TOKEN_USER, buffer.as_mut_ptr().cast(), len, &mut len);
        CloseHandle(token);
        if got == 0 {
            return None;
        }
        let sid = *buffer.as_ptr().cast::<*mut c_void>();
        let mut text = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut text) == 0 {
            return None;
        }
        let mut count = 0;
        while *text.add(count) != 0 {
            count += 1;
        }
        let sid = String::from_utf16(std::slice::from_raw_parts(text, count)).ok();
        LocalFree(text.cast());
        sid
    }
}

/// 盘上文件的安全描述符（自相对格式的字节）：只有映射它的用户（和系统、管理员）能读写。服务器
/// 上的权限由 kuaid 那边的账号决定，盘上不再细分。原来给 Everyone：映射到本机别的账号进得去的
/// 文件夹时，别人能用你的 SSH 身份读写远程文件。取不到当前用户时退回原来的。
pub fn open_security_descriptor() -> Result<Vec<u8>> {
    let user = current_user_sid();
    if user.is_none() {
        log::warn!("取不到当前用户，映射的盘对本机所有用户开放");
    }
    let sddl = wide(&sddl(user.as_deref()));
    let mut descriptor = std::ptr::null_mut();
    let mut size = 0u32;
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            &mut size,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context(tr("无法生成安全描述符"));
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(descriptor.cast::<u8>(), size as usize).to_vec() };
    unsafe { LocalFree(descriptor) };
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 盘上的权限只给当前用户() {
        let sid = current_user_sid().expect("取当前用户的 SID");
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        let text = sddl(Some(&sid));
        assert!(text.ends_with(&format!("(A;;FA;;;{sid})")), "{text}");
        assert!(!text.contains(";WD)"), "{text}");
        // 取不到当前用户时退回原来的
        assert!(sddl(None).ends_with("(A;;FA;;;WD)"));
        assert!(!open_security_descriptor().unwrap().is_empty());
    }
}
