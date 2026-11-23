//! Linux 上的 FUSE（fuser，纯 Rust 实现的协议，挂载、卸载经系统的 fusermount3）：内核按 inode
//! 来问，这里换成路径交给 [`Mount`]，结果换成 FUSE 的答复。
//!
//! - inode 按路径发：内核查到一个名字（lookup）就记一次，它说忘了（forget）几次就减几次，减到 0
//!   才扔。改名时这个路径下面记着的跟着换。
//! - 权限：文件 0644、目录 0755，主机上写不了的（只读）去掉写权限，属主是当前用户；由内核按这些
//!   查权限（`default_permissions`）。主机是 Windows 时目录的只读属性不算（「文档」这些文件夹都
//!   带着它，里面照样能写）。改权限、属主当作改好了，主机上不变。
//! - 删除是马上做的：没删成（没权限）的错误直接交给删的程序。
//! - 每次请求在 fuser 的几个线程上做，等主机回音时别的请求照样进来。回调里崩了答 EIO。

use std::ffi::OsStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    KernelConfig, LockOwner, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
    TimeOrNow, WriteFlags,
};

use super::*;

/// 映射的根目录的 inode。
const ROOT: u64 = 1;
/// 列目录时还没查过的名字：不知道它的 inode（`ls -i` 显示这个数）。
const UNKNOWN_INO: u64 = 0xffff_ffff;
/// 一次挂载用几个线程接内核的请求：等主机回音时别的请求不用排队。
pub(crate) const THREADS: usize = 8;

/// 内核记着的 inode：每个对应映射目录下的一个路径。
struct Inodes {
    /// inode → 路径、内核查过它几次（还没 forget 的）
    paths: HashMap<u64, (String, u64)>,
    /// 路径（按 [`Mount::key`] 认）→ inode
    by_key: HashMap<String, u64>,
    next: u64,
}

/// 一次打开（open、create、opendir）。
struct Handle {
    node: Arc<Node>,
    /// 是文件：算在「开着的文件」里
    file: bool,
    /// 列目录：从头列（offset 0）时取一份，往后翻都用它，前后一致
    listing: Mutex<Option<Arc<Vec<FsEntry>>>>,
}

pub(crate) struct Fuse {
    mount: Arc<Mount>,
    inodes: Mutex<Inodes>,
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    next_handle: AtomicU64,
    uid: u32,
    gid: u32,
}

/// 回调里出了错、崩了，都变成一个错误码：不能让崩溃把接请求的线程带走。
fn guarded<T>(run: impl FnOnce() -> Nt<T>) -> Nt<T> {
    catch_unwind(AssertUnwindSafe(run)).unwrap_or(Err(STATUS_UNEXPECTED_IO_ERROR))
}

fn errno(code: NTSTATUS) -> Errno {
    Errno::from_i32(code)
}

/// 文件名换成字符串：主机上的路径都是 UTF-8，不是的不可能在那边。
fn utf8(name: &OsStr) -> Nt<&str> {
    name.to_str().ok_or(STATUS_OBJECT_NAME_NOT_FOUND)
}

fn join(parent: &str, name: &str) -> String {
    match parent.is_empty() {
        true => name.to_string(),
        false => format!("{parent}/{name}"),
    }
}

fn millis_of(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

impl Fuse {
    pub(crate) fn new(mount: Arc<Mount>) -> Fuse {
        let mut paths = HashMap::new();
        paths.insert(ROOT, (String::new(), 1));
        let mut by_key = HashMap::new();
        by_key.insert(String::new(), ROOT);
        // SAFETY：只读进程自己的用户、组
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Fuse {
            mount,
            inodes: Mutex::new(Inodes {
                paths,
                by_key,
                next: ROOT + 1,
            }),
            handles: Mutex::default(),
            next_handle: AtomicU64::new(1),
            uid,
            gid,
        }
    }

    fn path(&self, ino: INodeNo) -> Nt<String> {
        lock(&self.inodes)
            .paths
            .get(&ino.0)
            .map(|(path, _)| path.clone())
            .ok_or(STATUS_OBJECT_NAME_NOT_FOUND)
    }

    fn child(&self, parent: INodeNo, name: &OsStr) -> Nt<String> {
        let name = utf8(name)?;
        if !representable(name) {
            return Err(STATUS_OBJECT_NAME_INVALID);
        }
        Ok(join(&self.path(parent)?, name))
    }

    /// 内核查到了这个路径：给它一个 inode（已经有的接着用），记一次。
    fn remember(&self, path: &str) -> u64 {
        let key = self.mount.key(path);
        let mut inodes = lock(&self.inodes);
        if let Some(ino) = inodes.by_key.get(&key).copied()
            && let Some((_, count)) = inodes.paths.get_mut(&ino)
        {
            *count += 1;
            return ino;
        }
        let ino = inodes.next;
        inodes.next += 1;
        inodes.paths.insert(ino, (path.to_string(), 1));
        inodes.by_key.insert(key, ino);
        ino
    }

    /// 列目录时用：内核查过的给它的 inode，没查过的不新发。
    fn known(&self, path: &str) -> u64 {
        let key = self.mount.key(path);
        lock(&self.inodes)
            .by_key
            .get(&key)
            .copied()
            .unwrap_or(UNKNOWN_INO)
    }

    fn forget_ino(&self, ino: u64, count: u64) {
        if ino == ROOT {
            return;
        }
        let mut inodes = lock(&self.inodes);
        let Some((path, left)) = inodes.paths.get_mut(&ino) else {
            return;
        };
        *left = left.saturating_sub(count);
        if *left > 0 {
            return;
        }
        let key = self.mount.key(path);
        inodes.paths.remove(&ino);
        if inodes.by_key.get(&key) == Some(&ino) {
            inodes.by_key.remove(&key);
        }
    }

    /// 改了名：这个路径和它下面记着的 inode 都换到新路径上。新路径上原来的（被盖掉的）不再按
    /// 路径认，内核忘了它时再扔。
    fn moved(&self, from: &str, to: &str) {
        let (old, new) = (self.mount.key(from), self.mount.key(to));
        let inside = format!("{old}/");
        let mut inodes = lock(&self.inodes);
        // 只是改了大小写（主机是 Windows）时新旧是同一个
        if new != old {
            inodes.by_key.remove(&new);
        }
        let moving: Vec<(String, u64)> = inodes
            .by_key
            .iter()
            .filter(|(key, _)| **key == old || key.starts_with(&inside))
            .map(|(key, ino)| (key.clone(), *ino))
            .collect();
        let depth = from.split('/').count();
        for (key, ino) in moving {
            inodes.by_key.remove(&key);
            inodes
                .by_key
                .insert(format!("{new}{}", &key[old.len()..]), ino);
            if let Some((path, _)) = inodes.paths.get_mut(&ino) {
                let rest: Vec<&str> = path.split('/').skip(depth).collect();
                *path = match rest.is_empty() {
                    true => to.to_string(),
                    false => format!("{to}/{}", rest.join("/")),
                };
            }
        }
    }

    fn handle(&self, fh: FileHandle) -> Nt<Arc<Handle>> {
        lock(&self.handles)
            .get(&fh.0)
            .cloned()
            .ok_or(STATUS_UNEXPECTED_IO_ERROR)
    }

    /// 交出去一次打开：文件记在「开着的文件」里。
    fn hand_out(&self, node: Arc<Node>) -> FileHandle {
        let file = !lock(&node.state).stat.dir;
        if file {
            self.mount.open_files.fetch_add(1, Relaxed);
        }
        let fh = self.next_handle.fetch_add(1, Relaxed);
        let handle = Arc::new(Handle {
            node,
            file,
            listing: Mutex::default(),
        });
        lock(&self.handles).insert(fh, handle);
        FileHandle(fh)
    }

    fn give_back(&self, fh: FileHandle) {
        let Some(handle) = lock(&self.handles).remove(&fh.0) else {
            return;
        };
        if handle.file {
            self.mount.open_files.fetch_sub(1, Relaxed);
        }
        let node = handle.node.clone();
        drop(handle);
        self.mount.close(node);
    }

    /// 这个路径现在的情况：开着的用开着的那份（写着的大小比主机上的新），没开的查。
    fn current(&self, path: &str) -> Nt<FsStat> {
        let open = lock(&self.mount.nodes)
            .get(&self.mount.key(path))
            .and_then(Weak::upgrade);
        match open {
            Some(node) => Ok(lock(&node.state).stat),
            None => self.mount.lookup(path),
        }
    }

    fn attr(&self, ino: u64, stat: &FsStat) -> FileAttr {
        let millis = match stat.mtime {
            0 => self.mount.started,
            millis => millis,
        };
        let time = UNIX_EPOCH + Duration::from_millis(millis);
        let (kind, perm) = match stat.dir {
            true => (FileType::Directory, 0o755),
            false => (FileType::RegularFile, 0o644),
        };
        // Windows 主机上目录的只读属性不管里面能不能写
        let readonly = self.mount.read_only || (stat.readonly && !(stat.dir && self.mount.windows));
        let perm = if readonly { perm & !0o222 } else { perm };
        let size = if stat.dir { 0 } else { stat.size };
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: time,
            mtime: time,
            ctime: time,
            crtime: time,
            kind,
            perm,
            nlink: if stat.dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: UNIT as u32,
            flags: 0,
        }
    }

    /// 查到了一个路径：记下它的 inode，答给内核。
    fn entry(&self, path: &str, stat: &FsStat, reply: ReplyEntry) {
        let ino = self.remember(path);
        reply.entry(&TTL, &self.attr(ino, stat), Generation(0));
    }

    fn mtime_of(time: TimeOrNow) -> u64 {
        match time {
            TimeOrNow::SpecificTime(time) => millis_of(time),
            TimeOrNow::Now => now_millis(),
        }
    }

    /// 删文件（`dir` 为假）或目录。
    fn delete(&self, parent: INodeNo, name: &OsStr, dir: bool) -> Nt<()> {
        let path = self.child(parent, name)?;
        let node = self.mount.open(&path)?;
        let result = (|| {
            match (dir, lock(&node.state).stat.dir) {
                (false, true) => return Err(STATUS_FILE_IS_A_DIRECTORY),
                (true, false) => return Err(STATUS_NOT_A_DIRECTORY),
                _ => {}
            }
            self.mount.can_delete(&node)?;
            self.mount.remove(&node, false)
        })();
        self.mount.close(node);
        result
    }

    /// 打开着的那个；没打开的（`fh` 为空）临时打开一下，用完关上。
    fn with_node<T>(
        &self,
        ino: INodeNo,
        fh: Option<FileHandle>,
        run: impl FnOnce(&Arc<Node>) -> Nt<T>,
    ) -> Nt<T> {
        if let Some(handle) = fh.and_then(|fh| self.handle(fh).ok()) {
            return run(&handle.node);
        }
        let node = self.mount.open(&self.path(ino)?)?;
        let result = run(&node);
        self.mount.close(node);
        result
    }
}

impl Filesystem for Fuse {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // 一次读、写多些，请求少些（内核不肯就用它的）
        let _ = config.set_max_readahead((FETCH_UNITS * UNIT) as u32);
        let _ = config.set_max_write(CHUNK as u32);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let found = guarded(|| {
            let path = self.child(parent, name)?;
            let stat = self.current(&path)?;
            match stat.exists {
                true => Ok((path, stat)),
                false => Err(STATUS_OBJECT_NAME_NOT_FOUND),
            }
        });
        match found {
            Ok((path, stat)) => self.entry(&path, &stat, reply),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.forget_ino(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        let stat = guarded(|| {
            if let Some(handle) = fh.and_then(|fh| self.handle(fh).ok()) {
                return Ok(lock(&handle.node.state).stat);
            }
            let stat = self.current(&self.path(ino)?)?;
            match stat.exists {
                true => Ok(stat),
                false => Err(STATUS_OBJECT_NAME_NOT_FOUND),
            }
        });
        match stat {
            Ok(stat) => reply.attr(&TTL, &self.attr(ino.0, &stat)),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let stat = guarded(|| {
            self.with_node(ino, fh, |node| {
                let mut stat = lock(&node.state).stat;
                if let Some(size) = size {
                    stat = self.mount.resize(node, size, false)?;
                }
                if let Some(mtime) = mtime {
                    stat = self.mount.set_mtime(node, Self::mtime_of(mtime))?;
                }
                Ok(stat)
            })
        });
        match stat {
            Ok(stat) => reply.attr(&TTL, &self.attr(ino.0, &stat)),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let made = guarded(|| {
            let path = self.child(parent, name)?;
            let node = self.mount.create(&path, true)?;
            let stat = lock(&node.state).stat;
            self.mount.close(node);
            Ok((path, stat))
        });
        match made {
            Ok((path, stat)) => self.entry(&path, &stat, reply),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match guarded(|| self.delete(parent, name, false)) {
            Ok(()) => reply.ok(),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match guarded(|| self.delete(parent, name, true)) {
            Ok(()) => reply.ok(),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let renamed = guarded(|| {
            // 两个互换、留白（overlayfs 用的）主机上做不了
            if flags.intersects(RenameFlags::RENAME_EXCHANGE | RenameFlags::RENAME_WHITEOUT) {
                return Err(STATUS_OBJECT_NAME_INVALID);
            }
            let from = self.child(parent, name)?;
            let to = self.child(newparent, newname)?;
            let node = self.mount.open(&from)?;
            let replace = !flags.contains(RenameFlags::RENAME_NOREPLACE);
            let result = self.mount.rename(&node, &to, replace);
            self.mount.close(node);
            result?;
            self.moved(&from, &to);
            Ok(())
        });
        match renamed {
            Ok(()) => reply.ok(),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let opened = guarded(|| {
            let node = self.mount.open(&self.path(ino)?)?;
            let writing = flags.acc_mode() != OpenAccMode::O_RDONLY;
            let checked = self.mount.check_write(&node, writing).and_then(|()| {
                match writing && flags.0 & libc::O_TRUNC != 0 {
                    true => self.mount.resize(&node, 0, false).map(|_| ()),
                    false => Ok(()),
                }
            });
            match checked {
                Ok(()) => Ok(self.hand_out(node)),
                Err(code) => {
                    self.mount.close(node);
                    Err(code)
                }
            }
        });
        match opened {
            Ok(fh) => reply.opened(fh, FopenFlags::empty()),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let read = guarded(|| {
            let handle = self.handle(fh)?;
            let mut buffer = vec![0; size as usize];
            match self.mount.read(&handle.node, &mut buffer, offset) {
                Ok(got) => buffer.truncate(got),
                Err(STATUS_END_OF_FILE) => buffer.clear(),
                Err(code) => return Err(code),
            }
            Ok(buffer)
        });
        match read {
            Ok(bytes) => reply.data(&bytes),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let written = guarded(|| {
            let handle = self.handle(fh)?;
            let to_end = flags.0 & libc::O_APPEND != 0;
            let (written, _) = self
                .mount
                .write(&handle.node, data, offset, to_end, false)?;
            Ok(written)
        });
        match written {
            Ok(written) => reply.written(written as u32),
            Err(code) => reply.error(errno(code)),
        }
    }

    /// 每次 close 都来：等写入落地，没写成的在这里报给程序（和本机磁盘满了时 close 报错一样）。
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        match guarded(|| self.mount.commit(&self.handle(fh)?.node)) {
            Ok(_) => reply.ok(),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let _ = guarded(|| {
            self.give_back(fh);
            Ok(())
        });
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match guarded(|| self.mount.flush_file(&self.handle(fh)?.node)) {
            Ok(_) => reply.ok(),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let opened = guarded(|| {
            let node = self.mount.open(&self.path(ino)?)?;
            if !lock(&node.state).stat.dir {
                self.mount.close(node);
                return Err(STATUS_NOT_A_DIRECTORY);
            }
            Ok(self.hand_out(node))
        });
        match opened {
            Ok(fh) => reply.opened(fh, FopenFlags::empty()),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let listed = guarded(|| {
            let handle = self.handle(fh)?;
            let path = lock(&handle.node.state).path.clone();
            let mut listing = lock(&handle.listing);
            if offset == 0 || listing.is_none() {
                *listing = Some(self.mount.list(&path, TTL)?);
            }
            Ok((path, listing.clone().unwrap_or_default()))
        });
        let (path, entries) = match listed {
            Ok(listed) => listed,
            Err(code) => return reply.error(errno(code)),
        };
        // 前两个是 `.` 和 `..`，后面是目录里的；offset 是下一个从第几个起
        let dots = [(ino.0, "."), (UNKNOWN_INO, "..")];
        let rows = dots
            .iter()
            .map(|(ino, name)| (*ino, FileType::Directory, *name))
            .chain(entries.iter().map(|entry| {
                let kind = match entry.dir {
                    true => FileType::Directory,
                    false => FileType::RegularFile,
                };
                (
                    self.known(&join(&path, &entry.name)),
                    kind,
                    entry.name.as_str(),
                )
            }));
        for (at, (ino, kind, name)) in rows.enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), at as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        let _ = guarded(|| {
            self.give_back(fh);
            Ok(())
        });
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let space = guarded(|| Ok(self.mount.volume())).unwrap_or(FsSpace {
            total: 1 << 40,
            free: 1 << 39,
        });
        let block = 4096;
        let (total, free) = (space.total / block, space.free / block);
        reply.statfs(total, free, free, 0, 0, block as u32, 255, block as u32);
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let created = guarded(|| {
            let path = self.child(parent, name)?;
            let node = match self.mount.create(&path, false) {
                Ok(node) => node,
                // 内核以为没有、主机上其实有（别处刚建的）：不是 O_EXCL 就当作打开它
                Err(STATUS_OBJECT_NAME_COLLISION) if flags & libc::O_EXCL == 0 => {
                    let node = self.mount.open(&path)?;
                    let checked = self.mount.check_write(&node, true).and_then(|()| {
                        match flags & libc::O_TRUNC != 0 {
                            true => self.mount.resize(&node, 0, false).map(|_| ()),
                            false => Ok(()),
                        }
                    });
                    if let Err(code) = checked {
                        self.mount.close(node);
                        return Err(code);
                    }
                    node
                }
                Err(code) => return Err(code),
            };
            let stat = lock(&node.state).stat;
            Ok((path, stat, self.hand_out(node)))
        });
        match created {
            Ok((path, stat, fh)) => {
                let ino = self.remember(&path);
                let attr = self.attr(ino, &stat);
                reply.created(&TTL, &attr, Generation(0), fh, FopenFlags::empty());
            }
            Err(code) => reply.error(errno(code)),
        }
    }
}
