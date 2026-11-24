//! WinFsp 的回调：把指针换成 [`Mount`] 那些函数的参数，结果换成 WinFsp 的结构体。

use std::ffi::c_void;
use std::sync::atomic::AtomicBool;

use super::*;

/// 1601 年到 1970 年之间的 100 纳秒数。
pub(crate) const EPOCH_GAP: u64 = 116_444_736_000_000_000;

/// 1970 年起的毫秒数换成 Windows 的文件时间（1601 年起的 100 纳秒数）。
pub(crate) fn filetime(millis: u64) -> u64 {
    millis * 10_000 + EPOCH_GAP
}

/// Windows 的文件时间换成 1970 年起的毫秒数；0 和全 1 是「不改」（None），1970 年以前的也不管。
/// 正好是 1970 年那一刻的是 `Some(0)`：原来也换成 0，和「不改」混在一起，设不成那个时间。
pub(crate) fn millis_of(filetime: u64) -> Option<u64> {
    match filetime == u64::MAX || filetime < EPOCH_GAP {
        true => None,
        false => Some((filetime - EPOCH_GAP) / 10_000),
    }
}

fn attributes(stat: &FsStat) -> u32 {
    let kind = match stat.dir {
        true => FILE_ATTRIBUTE_DIRECTORY,
        false => FILE_ATTRIBUTE_ARCHIVE,
    };
    match stat.readonly {
        true => kind | FILE_ATTRIBUTE_READONLY,
        false => kind,
    }
}

impl Mount {
    fn info(&self, stat: &FsStat) -> FileInfo {
        let size = if stat.dir { 0 } else { stat.size };
        let time = match stat.mtime {
            0 => filetime(self.started),
            millis => filetime(millis),
        };
        FileInfo {
            FileAttributes: attributes(stat),
            AllocationSize: size.div_ceil(4096) * 4096,
            FileSize: size,
            CreationTime: time,
            LastAccessTime: time,
            LastWriteTime: time,
            ChangeTime: time,
            ..FileInfo::default()
        }
    }

    /// 列目录。`marker` 是上一批的最后一个名字：只给排在它后面的。
    fn read_directory(
        &self,
        node: &Node,
        marker: Option<&[u16]>,
        buffer: *mut c_void,
        length: u32,
        transferred: *mut u32,
    ) -> Nt<()> {
        let listing = {
            let mut state = lock(&node.state);
            if marker.is_none() || state.listing.is_none() {
                let entries = self.list(&state.path, TTL)?;
                let mut rows: Vec<(Vec<u16>, FsEntry)> = entries
                    .iter()
                    .map(|entry| (entry.name.encode_utf16().collect(), entry.clone()))
                    .collect();
                if !state.path.is_empty() {
                    for name in [".", ".."] {
                        let entry = FsEntry {
                            name: name.to_string(),
                            dir: true,
                            mtime: state.stat.mtime,
                            ..FsEntry::default()
                        };
                        rows.push((name.encode_utf16().collect(), entry));
                    }
                }
                rows.sort_by(|a, b| a.0.cmp(&b.0));
                state.listing = Some(Arc::new(rows));
            }
            state.listing.clone().unwrap_or_default()
        };
        for (name, entry) in listing.iter() {
            if marker.is_some_and(|marker| name.as_slice() <= marker) {
                continue;
            }
            let bytes = size_of::<DirInfo>() + name.len() * 2;
            // 用 u64 的数组当缓冲区，对齐才够
            let mut record = vec![0u64; bytes.div_ceil(8)];
            let info = record.as_mut_ptr().cast::<DirInfo>();
            // SAFETY：record 够 bytes 这么大、按 8 字节对齐；名字紧跟在定长部分后面，没有越界。
            // buffer、transferred 是 WinFsp 给这次回调的
            let added = unsafe {
                (*info).Size = bytes as u16;
                (*info).FileInfo = self.info(&stat_of(entry));
                std::ptr::copy_nonoverlapping(
                    name.as_ptr(),
                    info.cast::<u8>().add(size_of::<DirInfo>()).cast::<u16>(),
                    name.len(),
                );
                (self.add_dir_info)(info, buffer, length, transferred)
            };
            if added == 0 {
                // 这一批装满了，系统会带着最后一个名字再来要
                return Ok(());
            }
        }
        // SAFETY：空指针是告诉 WinFsp 列完了
        unsafe { (self.add_dir_info)(std::ptr::null_mut(), buffer, length, transferred) };
        Ok(())
    }

    /// # Safety
    /// 指针是 WinFsp 给回调的那两个。
    unsafe fn copy_security(&self, descriptor: *mut c_void, size: *mut usize) -> Nt<()> {
        if size.is_null() {
            return Ok(());
        }
        let need = self.security.len();
        unsafe {
            if *size < need {
                *size = need;
                return Err(STATUS_BUFFER_OVERFLOW);
            }
            *size = need;
            if !descriptor.is_null() {
                std::ptr::copy_nonoverlapping(self.security.as_ptr(), descriptor.cast(), need);
            }
        }
        Ok(())
    }
}

// ---- WinFsp 的回调：把指针换成上面那些函数的参数 ----
//
// 这些函数由 WinFsp 的调度线程调用：`fs` 是 `FspFileSystemCreate` 给的那个，它的 UserContext
// 指向一个活着的 [`Mount`]（卸载时先停调度线程再放掉它）；`context` 是 `open`、`create` 交出去的
// `Box<Handle>`，`close` 时收回。别的指针都是这一次回调里有效的。

type Fs = *mut FileSystem;
type Ctx = *mut c_void;

unsafe fn mount<'a>(fs: Fs) -> &'a Mount {
    unsafe { &*user_context(fs).cast::<Mount>() }
}

/// 一次打开。
struct Handle {
    node: Arc<Node>,
    /// 算在「开着的文件」里。程序关掉句柄（Cleanup）就不算了：系统的缓存还会把它留一阵才
    /// Close，那一阵不算开着
    counted: AtomicBool,
}

impl Handle {
    fn uncount(&self, mount: &Mount) {
        if self.counted.swap(false, Relaxed) {
            mount.open_files.fetch_sub(1, Relaxed);
        }
    }
}

unsafe fn handle<'a>(context: Ctx) -> &'a Handle {
    unsafe { &*context.cast::<Handle>() }
}

unsafe fn node<'a>(context: Ctx) -> &'a Arc<Node> {
    unsafe { &handle(context).node }
}

/// `\目录\文件` 换成 `目录/文件`。
unsafe fn remote_path(name: PWSTR) -> String {
    let name = String::from_utf16_lossy(unsafe { wide_slice(name) });
    name.trim_start_matches('\\').replace('\\', "/")
}

/// 回调里出了错、崩了，都变成一个状态码交回去；不能让崩溃穿过 C 的调用栈。
fn guard(run: impl FnOnce() -> Nt<()>) -> NTSTATUS {
    match catch_unwind(AssertUnwindSafe(run)) {
        Ok(Ok(())) => STATUS_SUCCESS,
        Ok(Err(status)) => status,
        Err(_) => STATUS_UNEXPECTED_IO_ERROR,
    }
}

/// 交出去一个打开的文件：记一下有几个文件开着。
unsafe fn hand_out(mount: &Mount, node: Arc<Node>, context: *mut Ctx, info: *mut FileInfo) {
    let stat = lock(&node.state).stat;
    if !stat.dir {
        mount.open_files.fetch_add(1, Relaxed);
    }
    let handle = Handle {
        node,
        counted: AtomicBool::new(!stat.dir),
    };
    unsafe {
        *info = mount.info(&stat);
        *context = Box::into_raw(Box::new(handle)).cast();
    }
}

unsafe extern "C" fn get_volume_info(fs: Fs, info: *mut VolumeInfo) -> NTSTATUS {
    guard(|| unsafe {
        let mount = mount(fs);
        let space = mount.volume();
        let info = &mut *info;
        info.TotalSize = space.total;
        info.FreeSize = space.free;
        info.VolumeLabelLength = (mount.label.len() * 2) as u16;
        info.VolumeLabel[..mount.label.len()].copy_from_slice(&mount.label);
        Ok(())
    })
}

unsafe extern "C" fn get_security_by_name(
    fs: Fs,
    name: PWSTR,
    attributes_out: *mut u32,
    descriptor: *mut c_void,
    size: *mut usize,
) -> NTSTATUS {
    guard(|| unsafe {
        let mount = mount(fs);
        let stat = mount.lookup(&remote_path(name))?;
        if !stat.exists {
            return Err(STATUS_OBJECT_NAME_NOT_FOUND);
        }
        if !attributes_out.is_null() {
            *attributes_out = attributes(&stat);
        }
        mount.copy_security(descriptor, size)
    })
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn create(
    fs: Fs,
    name: PWSTR,
    options: u32,
    _granted: u32,
    _attributes: u32,
    _descriptor: *mut c_void,
    _allocation: u64,
    context: *mut Ctx,
    info: *mut FileInfo,
) -> NTSTATUS {
    guard(|| unsafe {
        let mount = mount(fs);
        let node = mount.create(&remote_path(name), options & FILE_DIRECTORY_FILE != 0)?;
        hand_out(mount, node, context, info);
        Ok(())
    })
}

unsafe extern "C" fn open(
    fs: Fs,
    name: PWSTR,
    _options: u32,
    granted: u32,
    context: *mut Ctx,
    info: *mut FileInfo,
) -> NTSTATUS {
    guard(|| unsafe {
        let mount = mount(fs);
        let node = mount.open(&remote_path(name))?;
        mount.check_write(&node, granted & (FILE_WRITE_DATA | FILE_APPEND_DATA) != 0)?;
        hand_out(mount, node, context, info);
        Ok(())
    })
}

unsafe extern "C" fn overwrite(
    fs: Fs,
    context: Ctx,
    _attributes: u32,
    _replace: u8,
    _allocation: u64,
    info: *mut FileInfo,
) -> NTSTATUS {
    guard(|| unsafe {
        let mount = mount(fs);
        *info = mount.info(&mount.resize(node(context), 0, false)?);
        Ok(())
    })
}

unsafe extern "C" fn cleanup(fs: Fs, context: Ctx, _name: PWSTR, flags: u32) {
    guard(|| unsafe {
        let (mount, node) = (mount(fs), node(context));
        handle(context).uncount(mount);
        if flags & CLEANUP_DELETE != 0 {
            // 系统这时已经当作删掉了（Cleanup 报不了错）：没删成的经 `on_error` 告诉用户
            let _ = mount.remove(node, true, None);
            return Ok(());
        }
        mount.commit(node).map(|_| ())
    });
}

unsafe extern "C" fn close(fs: Fs, context: Ctx) {
    guard(|| unsafe {
        let mount = mount(fs);
        let handle = Box::from_raw(context.cast::<Handle>());
        handle.uncount(mount);
        mount.close(handle.node, true);
        Ok(())
    });
}

unsafe extern "C" fn read(
    fs: Fs,
    context: Ctx,
    buffer: *mut c_void,
    offset: u64,
    length: u32,
    transferred: *mut u32,
) -> NTSTATUS {
    guard(|| unsafe {
        let buffer = std::slice::from_raw_parts_mut(buffer.cast::<u8>(), length as usize);
        *transferred = mount(fs).read(node(context), buffer, offset)? as u32;
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn write(
    fs: Fs,
    context: Ctx,
    buffer: *mut c_void,
    offset: u64,
    length: u32,
    to_end: u8,
    constrained: u8,
    transferred: *mut u32,
    info: *mut FileInfo,
) -> NTSTATUS {
    guard(|| unsafe {
        let data = std::slice::from_raw_parts(buffer.cast::<u8>(), length as usize);
        let mount = mount(fs);
        let (written, now) =
            mount.write(node(context), data, offset, to_end != 0, constrained != 0)?;
        *transferred = written as u32;
        *info = mount.info(&now);
        Ok(())
    })
}

unsafe extern "C" fn flush(fs: Fs, context: Ctx, info: *mut FileInfo) -> NTSTATUS {
    guard(|| unsafe {
        // 空指针是刷整个盘
        match context.is_null() {
            true => mount(fs).flush_all(),
            false => {
                let mount = mount(fs);
                *info = mount.info(&mount.flush_file(node(context))?);
                Ok(())
            }
        }
    })
}

unsafe extern "C" fn get_file_info(fs: Fs, context: Ctx, info: *mut FileInfo) -> NTSTATUS {
    guard(|| unsafe {
        *info = mount(fs).info(&lock(&node(context).state).stat);
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn set_basic_info(
    fs: Fs,
    context: Ctx,
    _attributes: u32,
    _creation: u64,
    _access: u64,
    write: u64,
    _change: u64,
    info: *mut FileInfo,
) -> NTSTATUS {
    guard(|| unsafe {
        let mount = mount(fs);
        *info = mount.info(&mount.set_mtime(node(context), millis_of(write))?);
        Ok(())
    })
}

unsafe extern "C" fn set_file_size(
    fs: Fs,
    context: Ctx,
    size: u64,
    allocation: u8,
    info: *mut FileInfo,
) -> NTSTATUS {
    guard(|| unsafe {
        let mount = mount(fs);
        *info = mount.info(&mount.resize(node(context), size, allocation != 0)?);
        Ok(())
    })
}

unsafe extern "C" fn can_delete(fs: Fs, context: Ctx, _name: PWSTR) -> NTSTATUS {
    guard(|| unsafe { mount(fs).can_delete(node(context)) })
}

unsafe extern "C" fn rename(
    fs: Fs,
    context: Ctx,
    _name: PWSTR,
    new_name: PWSTR,
    replace: u8,
) -> NTSTATUS {
    guard(|| unsafe { mount(fs).rename(node(context), &remote_path(new_name), replace != 0) })
}

unsafe extern "C" fn get_security(
    fs: Fs,
    _context: Ctx,
    descriptor: *mut c_void,
    size: *mut usize,
) -> NTSTATUS {
    guard(|| unsafe { mount(fs).copy_security(descriptor, size) })
}

unsafe extern "C" fn read_directory(
    fs: Fs,
    context: Ctx,
    _pattern: PWSTR,
    marker: PWSTR,
    buffer: *mut c_void,
    length: u32,
    transferred: *mut u32,
) -> NTSTATUS {
    guard(|| unsafe {
        let marker = (!marker.is_null()).then(|| wide_slice(marker));
        mount(fs).read_directory(node(context), marker, buffer, length, transferred)
    })
}

pub(crate) static INTERFACE: Interface = Interface {
    GetVolumeInfo: Some(get_volume_info),
    SetVolumeLabel: 0,
    GetSecurityByName: Some(get_security_by_name),
    Create: Some(create),
    Open: Some(open),
    Overwrite: Some(overwrite),
    Cleanup: Some(cleanup),
    Close: Some(close),
    Read: Some(read),
    Write: Some(write),
    Flush: Some(flush),
    GetFileInfo: Some(get_file_info),
    SetBasicInfo: Some(set_basic_info),
    SetFileSize: Some(set_file_size),
    CanDelete: Some(can_delete),
    Rename: Some(rename),
    GetSecurity: Some(get_security),
    SetSecurity: 0,
    ReadDirectory: Some(read_directory),
    Rest: [0; 45],
};
