//! 仓库目录里的文件操作（文件页用）：列目录、读、写、新建、改名、删除。
//!
//! 本机仓库由客户端直接调用，服务器上的仓库由 kuaid 调用，两边是同一份代码。
//!
//! 路径都相对仓库根、用 `/` 分隔（空字符串是仓库根），不能带 `.`、`..`，也不能是绝对路径：
//! 文件页只管仓库里的东西。这不是安全边界（仓库里的符号链接可以指到外面，用户自己也有终端），
//! 只是防着拼错路径改到别处去。

use std::fs::{self, File, Metadata};
use std::io::{ErrorKind, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context as _, Result, bail};

use crate::i18n::tr;
use crate::protocol::{FsEntry, FsSpace, FsStat, FsWrite, FsWritten, SubRepos};
use crate::tf;

/// 一次最多读这么多字节；更大的文件分几次读。
pub const READ_MAX: u64 = 4 << 20;

/// 相对仓库根的路径在磁盘上是哪。
pub fn resolve(repo: &str, path: &str) -> Result<PathBuf> {
    if repo.is_empty() {
        bail!(tr("未指定仓库"));
    }
    let mut full = PathBuf::from(repo);
    for part in path.split('/').filter(|part| !part.is_empty()) {
        // 一段只能是一个普通的名字：`..`、盘符、（Windows 上的）反斜杠都不行
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) if name == part => full.push(part),
            _ => bail!("{}", tf!("路径无效：{path}", path)),
        }
    }
    Ok(full)
}

/// 同 [`resolve`]，但不能是仓库根（改名、删除、写文件用）。
pub fn resolve_inside(repo: &str, path: &str) -> Result<PathBuf> {
    if path.split('/').all(str::is_empty) {
        bail!("{}", tf!("路径无效：{path}", path));
    }
    resolve(repo, path)
}

fn mtime(meta: &Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_millis() as u64)
}

fn stat_of(meta: &Metadata) -> FsStat {
    FsStat {
        exists: true,
        dir: meta.is_dir(),
        size: if meta.is_dir() { 0 } else { meta.len() },
        mtime: mtime(meta),
        readonly: meta.permissions().readonly(),
    }
}

fn stat_at(full: &Path) -> Result<FsStat> {
    match fs::metadata(full) {
        Ok(meta) => Ok(stat_of(&meta)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(FsStat::default()),
        Err(err) => Err(err).with_context(|| tf!("无法读取 {path}", path = full.display())),
    }
}

/// 列出一层：文件和目录都列，`.git` 不列。顺序不保证，由界面排。
pub fn list(repo: &str, dir: &str) -> Result<Vec<FsEntry>> {
    list_with(repo, dir, false)
}

/// 同 [`list`]；`all` 为真时 `.git` 也列（映射到本机的盘上要看得到它）。
pub fn list_with(repo: &str, dir: &str, all: bool) -> Result<Vec<FsEntry>> {
    let full = resolve(repo, dir)?;
    let entries =
        fs::read_dir(&full).with_context(|| tf!("无法读取 {path}", path = full.display()))?;
    let mut listed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" && !all {
            continue;
        }
        let link = entry.file_type().is_ok_and(|kind| kind.is_symlink());
        // 符号链接看它指向的；指向的东西不在了就当成一个空文件
        let meta = match link {
            true => fs::metadata(entry.path()).ok(),
            false => entry.metadata().ok(),
        };
        let stat = meta.as_ref().map(stat_of).unwrap_or_default();
        listed.push(FsEntry {
            name,
            dir: stat.dir,
            link,
            size: stat.size,
            mtime: stat.mtime,
        });
    }
    Ok(listed)
}

/// 一个路径现在的情况；什么都没有不算出错。
pub fn stat(repo: &str, path: &str) -> Result<FsStat> {
    stat_at(&resolve(repo, path)?)
}

/// 从 `offset` 起读最多 `len` 字节（不超过 [`READ_MAX`]），连同文件现在的情况一起返回。
pub fn read(repo: &str, path: &str, offset: u64, len: u64) -> Result<(FsStat, Vec<u8>)> {
    let full = resolve_inside(repo, path)?;
    let unreadable = || tf!("无法读取 {path}", path = full.display());
    let mut file = File::open(&full).with_context(unreadable)?;
    let stat = stat_of(&file.metadata().with_context(unreadable)?);
    if stat.dir {
        bail!("{}", tf!("{path} 是目录", path));
    }
    let len = len.min(READ_MAX);
    let mut bytes = Vec::with_capacity(len.min(stat.size.saturating_sub(offset)) as usize);
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))
            .with_context(unreadable)?;
    }
    file.take(len)
        .read_to_end(&mut bytes)
        .with_context(unreadable)?;
    Ok((stat, bytes))
}

/// 写一个文件（整个换掉）。见 [`FsWrite`]。
pub fn write(params: &FsWrite, bytes: &[u8]) -> Result<FsWritten> {
    let full = resolve_inside(&params.repo, &params.path)?;
    let current = stat_at(&full)?;
    if current.dir {
        bail!("{}", tf!("{path} 是目录", path = params.path));
    }
    if params.create && current.exists {
        bail!("{}", tf!("{path} 已存在", path = params.path));
    }
    if let Some(expect) = params.expect
        && (!current.exists || current.mtime != expect.mtime || current.size != expect.size)
    {
        return Ok(FsWritten {
            conflict: true,
            stat: current,
        });
    }
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)
            .with_context(|| tf!("无法创建 {path}", path = parent.display()))?;
    }
    replace(&full, bytes).with_context(|| tf!("无法写入 {path}", path = full.display()))?;
    Ok(FsWritten {
        conflict: false,
        stat: stat_at(&full)?,
    })
}

/// 把 `target` 的内容整个换成 `bytes`：先写到旁边的临时文件再改名，写到一半出错原来的还在。
/// 权限（和 Linux 上的属主）照原来的；是符号链接就写到它指向的文件，链接留着。
fn replace(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let is_link = fs::symlink_metadata(target).is_ok_and(|meta| meta.file_type().is_symlink());
    let target = match is_link {
        true => fs::canonicalize(target)?,
        false => target.to_path_buf(),
    };
    let existing = fs::metadata(&target).ok();
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let temp = target.with_file_name(format!(".{name}.kuai-{}.tmp", std::process::id()));
    let staged = (|| {
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(err) = staged {
        let _ = fs::remove_file(&temp);
        // 目录里不让建文件，但文件本身能写：直接写
        return match (err.kind(), &existing) {
            (ErrorKind::PermissionDenied, Some(_)) => fs::write(&target, bytes),
            _ => Err(err),
        };
    }
    if let Some(meta) = &existing {
        let _ = fs::set_permissions(&temp, meta.permissions());
        #[cfg(unix)]
        keep_owner(&temp, meta);
    }
    if let Err(err) = fs::rename(&temp, &target) {
        // 换不掉（Windows 上别的程序正开着它）：退一步直接写，再不行报改名时的错
        let direct = match existing {
            Some(_) => fs::write(&target, bytes),
            None => Err(ErrorKind::Other.into()),
        };
        let _ = fs::remove_file(&temp);
        return direct.map_err(|_| err);
    }
    Ok(())
}

/// 属主照原文件的（root 改别人的文件时，新文件不该变成 root 的）。改不了就算了。
#[cfg(unix)]
fn keep_owner(temp: &Path, meta: &Metadata) {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    let Ok(path) = std::ffi::CString::new(temp.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: path 是以 NUL 结尾的合法字符串，chown 只读它
    unsafe {
        libc::chown(path.as_ptr(), meta.uid(), meta.gid());
    }
}

/// 建目录（路径上缺的一起建）；已经有了就失败。
pub fn make_dir(repo: &str, path: &str) -> Result<()> {
    let full = resolve_inside(repo, path)?;
    if fs::symlink_metadata(&full).is_ok() {
        bail!("{}", tf!("{path} 已存在", path));
    }
    fs::create_dir_all(&full).with_context(|| tf!("无法创建 {path}", path = full.display()))
}

/// 永久删除：目录连里面的一起删；符号链接只删链接本身。
pub fn remove(repo: &str, path: &str) -> Result<()> {
    let full = resolve_inside(repo, path)?;
    let failed = || tf!("无法删除 {path}", path = full.display());
    let meta = fs::symlink_metadata(&full).with_context(failed)?;
    if meta.file_type().is_symlink() {
        // Windows 上指向目录的链接要按目录删
        return fs::remove_file(&full)
            .or_else(|_| fs::remove_dir(&full))
            .with_context(failed);
    }
    match meta.is_dir() {
        true => fs::remove_dir_all(&full),
        false => fs::remove_file(&full),
    }
    .with_context(failed)
}

/// 改名（也可以挪到别的目录，缺的目录一起建）；`to` 已经有了就失败，不盖掉别的文件。
pub fn rename(repo: &str, from: &str, to: &str) -> Result<()> {
    let source = resolve_inside(repo, from)?;
    let target = resolve_inside(repo, to)?;
    // 只改大小写（Windows 上是同一个文件）不算已存在
    let same = from.to_lowercase() == to.to_lowercase();
    if !same && fs::symlink_metadata(&target).is_ok() {
        bail!("{}", tf!("{path} 已存在", path = to));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| tf!("无法创建 {path}", path = parent.display()))?;
    }
    fs::rename(&source, &target).with_context(|| tf!("无法重命名 {path}", path = from))
}

/// 把 `bytes` 写到文件的 `offset` 那里，文件不够长就接长；别的地方不动。`create` 为真时文件不在
/// 就新建，否则文件不在是出错。返回写完以后文件的情况。
pub fn write_at(repo: &str, path: &str, offset: u64, bytes: &[u8], create: bool) -> Result<FsStat> {
    let full = resolve_inside(repo, path)?;
    let failed = || tf!("无法写入 {path}", path = full.display());
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(create)
        .open(&full)
        .with_context(failed)?;
    if !bytes.is_empty() {
        file.seek(SeekFrom::Start(offset)).with_context(failed)?;
        file.write_all(bytes).with_context(failed)?;
    }
    Ok(stat_of(&file.metadata().with_context(failed)?))
}

/// 把文件截短或接长到 `size` 字节。`new` 为真是新建一个文件：已经有了就失败。
pub fn resize(repo: &str, path: &str, size: u64, new: bool) -> Result<FsStat> {
    let full = resolve_inside(repo, path)?;
    let failed = || tf!("无法写入 {path}", path = full.display());
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(new)
        .open(&full)
        // 系统给的错误留在后面：映射到本机的盘要按错误码分辨是「已存在」还是别的
        .map_err(|err| match err.kind() {
            ErrorKind::AlreadyExists => anyhow::Error::new(err).context(tf!("{path} 已存在", path)),
            _ => anyhow::Error::new(err).context(failed()),
        })?;
    file.set_len(size).with_context(failed)?;
    Ok(stat_of(&file.metadata().with_context(failed)?))
}

/// 改修改时间（1970 年起的毫秒数）。
pub fn set_mtime(repo: &str, path: &str, mtime: u64) -> Result<FsStat> {
    let full = resolve_inside(repo, path)?;
    let failed = || tf!("无法写入 {path}", path = full.display());
    let time = UNIX_EPOCH + std::time::Duration::from_millis(mtime);
    // 目录在 Windows 上要带着「备份」标志才打得开，这里只管文件；目录的时间改不了不算出错
    if full.is_dir() {
        return stat_at(&full);
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&full)
        .with_context(failed)?;
    file.set_modified(time).with_context(failed)?;
    Ok(stat_of(&file.metadata().with_context(failed)?))
}

/// 目录所在的盘一共多大、当前用户还能用多少。
#[cfg(unix)]
pub fn space(repo: &str) -> Result<FsSpace> {
    use std::os::unix::ffi::OsStrExt as _;
    let full = resolve(repo, "")?;
    let path = std::ffi::CString::new(full.as_os_str().as_bytes())?;
    // SAFETY：path 是以 NUL 结尾的合法字符串，info 是这里的一块内存，statvfs 往里填
    let info = unsafe {
        let mut info: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &mut info) != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| tf!("无法读取 {path}", path = full.display()));
        }
        info
    };
    let block = info.f_frsize as u64;
    Ok(FsSpace {
        total: info.f_blocks as u64 * block,
        free: info.f_bavail as u64 * block,
    })
}

/// 目录所在的盘一共多大、当前用户还能用多少。
#[cfg(windows)]
pub fn space(repo: &str) -> Result<FsSpace> {
    use std::os::windows::ffi::OsStrExt as _;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            free: *mut u64,
            total: *mut u64,
            all_free: *mut u64,
        ) -> i32;
    }
    let full = resolve(repo, "")?;
    let wide: Vec<u16> = full.as_os_str().encode_wide().chain([0]).collect();
    let (mut free, mut total) = (0u64, 0u64);
    // SAFETY：wide 以 NUL 结尾，两个数是这里的变量，函数往里填；第四个可以不要
    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| tf!("无法读取 {path}", path = full.display()));
    }
    Ok(FsSpace { total, free })
}

/// 同 [`rename`]，`to` 已经有文件时直接盖掉（一步完成，中间没有两个都不在的时候）。目录不盖：
/// `to` 是目录就失败。
///
/// 盖掉的是普通文件时，权限和属主照被盖掉的那个（同 [`replace`]）：Windows 上的程序存盘常常是
/// 先写一个临时文件、再改名盖过去（映射到本机的盘上就走到这里），不然脚本的可执行位就没了。
pub fn rename_over(repo: &str, from: &str, to: &str) -> Result<()> {
    let source = resolve_inside(repo, from)?;
    let target = resolve_inside(repo, to)?;
    let existing = fs::symlink_metadata(&target).ok();
    if existing.as_ref().is_some_and(|meta| meta.is_dir()) && source != target {
        bail!("{}", tf!("{path} 已存在", path = to));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| tf!("无法创建 {path}", path = parent.display()))?;
    }
    #[cfg(unix)]
    if let Some(meta) = existing.filter(|meta| meta.is_file())
        && source != target
        && fs::symlink_metadata(&source).is_ok_and(|own| own.is_file())
    {
        let _ = fs::set_permissions(&source, meta.permissions());
        keep_owner(&source, &meta);
    }
    fs::rename(&source, &target).with_context(|| tf!("无法重命名 {path}", path = from))
}

/// 只删文件和空目录：目录里有东西就失败（映射到本机的盘上，删目录是系统自己一层层来的）。
pub fn remove_shallow(repo: &str, path: &str) -> Result<()> {
    let full = resolve_inside(repo, path)?;
    let failed = || tf!("无法删除 {path}", path = full.display());
    let meta = fs::symlink_metadata(&full).with_context(failed)?;
    match meta.is_dir() {
        true => fs::remove_dir(&full),
        // Windows 上指向目录的链接要按目录删
        false => fs::remove_file(&full).or_else(|_| fs::remove_dir(&full)),
    }
    .with_context(failed)
}

/// 找子仓库时往下最多走几层（仓库根下面的目录是第 1 层）。
const REPOS_MAX_DEPTH: usize = 6;
/// 找子仓库时最多看这么多个目录：再多就不找了，结果标成没找全。
const REPOS_MAX_DIRS: usize = 40_000;
/// 最多报这么多个子仓库。
const REPOS_MAX_FOUND: usize = 500;

/// 仓库目录里面还有哪些仓库（下面有 `.git` 的目录：单独克隆进来的、子模块都算），给的是相对
/// 仓库根的路径，按名字排。一个项目的各个插件各是一个仓库时（NocoBase 的业务插件），在一个
/// 标签页里看它们用。
///
/// 以 `.` 开头的目录和 `node_modules` 不进去；符号链接、目录联接不跟（会绕出去、绕回来）；
/// 找到的仓库不再往里找。层数、目录数有上限，到了上限 `truncated` 为真。
pub fn find_repos(repo: &str) -> Result<SubRepos> {
    let root = resolve(repo, "")?;
    fs::read_dir(&root).with_context(|| tf!("无法读取 {path}", path = root.display()))?;
    let mut found = SubRepos::default();
    // 一层层往下：近的先找到，到了上限丢掉的是深处的
    let mut level: Vec<(PathBuf, String)> = vec![(root, String::new())];
    let mut seen = 0;
    for _ in 0..REPOS_MAX_DEPTH {
        let mut next = Vec::new();
        for (dir, relative) in level {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name == "node_modules" {
                    continue;
                }
                seen += 1;
                if seen > REPOS_MAX_DIRS || found.repos.len() >= REPOS_MAX_FOUND {
                    found.truncated = true;
                    found.repos.sort_by_cached_key(|path| path.to_lowercase());
                    return Ok(found);
                }
                let path = entry.path();
                let relative = match relative.is_empty() {
                    true => name,
                    false => format!("{relative}/{name}"),
                };
                // 子模块、工作树的 `.git` 是一个文件
                if fs::symlink_metadata(path.join(".git")).is_ok() {
                    found.repos.push(relative);
                } else {
                    next.push((path, relative));
                }
            }
        }
        if next.is_empty() {
            break;
        }
        level = next;
    }
    found.repos.sort_by_cached_key(|path| path.to_lowercase());
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::FsExpect;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new() -> TempRepo {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "fr-files-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join(".git")).unwrap();
            TempRepo(dir)
        }

        fn repo(&self) -> &str {
            self.0.to_str().unwrap()
        }

        fn write(&self, path: &str, bytes: &[u8]) -> FsWritten {
            let params = FsWrite {
                repo: self.repo().into(),
                path: path.into(),
                ..Default::default()
            };
            write(&params, bytes).unwrap()
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn 按偏移写_截断_改时间() {
        let repo = TempRepo::new();
        let root = repo.repo();
        // 文件不在：不让新建时出错，让新建时建出来
        assert!(write_at(root, "a.bin", 0, b"x", false).is_err());
        assert_eq!(write_at(root, "a.bin", 0, b"hello", true).unwrap().size, 5);
        // 写中间：别的地方不动；写到末尾以后：中间补零
        assert_eq!(write_at(root, "a.bin", 1, b"EL", false).unwrap().size, 5);
        assert_eq!(fs::read(repo.0.join("a.bin")).unwrap(), b"hELlo");
        assert_eq!(write_at(root, "a.bin", 7, b"!", false).unwrap().size, 8);
        assert_eq!(fs::read(repo.0.join("a.bin")).unwrap(), b"hELlo\0\0!");
        // 空的一块：只是把文件建出来
        assert_eq!(write_at(root, "empty", 0, b"", true).unwrap().size, 0);
        // 截短、接长；新建时已经有了就失败
        assert_eq!(resize(root, "a.bin", 2, false).unwrap().size, 2);
        assert_eq!(fs::read(repo.0.join("a.bin")).unwrap(), b"hE");
        assert_eq!(resize(root, "a.bin", 4, false).unwrap().size, 4);
        assert_eq!(fs::read(repo.0.join("a.bin")).unwrap(), b"hE\0\0");
        assert!(resize(root, "a.bin", 0, true).is_err());
        assert_eq!(resize(root, "new.bin", 0, true).unwrap().size, 0);
        assert!(resize(root, "nope.bin", 3, false).is_err());
        // 改修改时间；目录的不改也不出错
        let then = 1_700_000_000_000;
        assert_eq!(set_mtime(root, "a.bin", then).unwrap().mtime, then);
        assert_eq!(stat(root, "a.bin").unwrap().mtime, then);
        fs::create_dir(repo.0.join("d")).unwrap();
        assert!(set_mtime(root, "d", then).unwrap().dir);
        // 不能动仓库根
        assert!(write_at(root, "", 0, b"x", true).is_err());
    }

    #[test]
    fn 盘的大小_盖掉已有的_只删空目录() {
        let repo = TempRepo::new();
        let root = repo.repo();
        let disk = space(root).unwrap();
        assert!(disk.total > 0 && disk.free <= disk.total);
        // 列目录带上 .git
        let names = |all: bool| -> Vec<String> {
            let mut names: Vec<String> = list_with(root, "", all)
                .unwrap()
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            names.sort();
            names
        };
        fs::write(repo.0.join("a.txt"), b"a").unwrap();
        fs::write(repo.0.join("b.txt"), b"b").unwrap();
        assert_eq!(names(false), ["a.txt", "b.txt"]);
        assert_eq!(names(true), [".git", "a.txt", "b.txt"]);
        // 改名盖掉已有的文件；目标是目录就不盖
        assert!(rename(root, "a.txt", "b.txt").is_err());
        rename_over(root, "a.txt", "b.txt").unwrap();
        assert_eq!(fs::read(repo.0.join("b.txt")).unwrap(), b"a");
        assert!(!repo.0.join("a.txt").exists());
        fs::create_dir_all(repo.0.join("d/sub")).unwrap();
        assert!(rename_over(root, "b.txt", "d").is_err());
        // 只删空目录和文件
        assert!(remove_shallow(root, "d").is_err());
        remove_shallow(root, "d/sub").unwrap();
        remove_shallow(root, "d").unwrap();
        remove_shallow(root, "b.txt").unwrap();
        assert_eq!(names(false), Vec::<String>::new());
        assert!(remove_shallow(root, "nope").is_err());
    }

    #[test]
    fn 路径只能在仓库里() {
        let root = if cfg!(windows) { r"C:\r" } else { "/r" };
        assert_eq!(resolve(root, "").unwrap(), PathBuf::from(root));
        assert_eq!(
            resolve(root, "a/b c/中文.txt").unwrap(),
            Path::new(root).join("a").join("b c").join("中文.txt")
        );
        // 多出来的斜杠不算错
        assert_eq!(
            resolve(root, "/a//b/").unwrap(),
            Path::new(root).join("a").join("b")
        );
        for bad in ["..", "a/../b", "./a", "a/."] {
            assert!(resolve(root, bad).is_err(), "{bad}");
        }
        #[cfg(windows)]
        for bad in [r"a\..\b", r"C:\x", "C:", r"\\server\share"] {
            assert!(resolve(root, bad).is_err(), "{bad}");
        }
        assert!(resolve("", "a").is_err());
        // 仓库根本身不能读写、删除
        assert!(read(root, "", 0, 10).is_err());
        assert!(remove(root, "/").is_err());
    }

    #[test]
    fn 找子仓库_不进点目录和_node_modules_找到的不再往里找() {
        let temp = TempRepo::new();
        let make = |path: &str| fs::create_dir_all(temp.0.join(path)).unwrap();
        make("packages/plugins/@org/plugin-b/.git");
        make("packages/plugins/@org/Plugin-a/.git");
        // 仓库里面的仓库不报：要看就进那个仓库看
        make("packages/plugins/@org/Plugin-a/vendor/inner/.git");
        make("packages/core/src");
        make("node_modules/dep/.git");
        make(".cache/x/.git");
        // 子模块：.git 是一个文件
        make("libs/sub");
        fs::write(
            temp.0.join("libs/sub/.git"),
            "gitdir: ../../.git/modules/sub",
        )
        .unwrap();
        // 太深的不找
        make("a/b/c/d/e/f/deep/.git");
        make("a/b/c/d/e/ok/.git");

        let found = find_repos(temp.repo()).unwrap();
        assert_eq!(
            found.repos,
            [
                "a/b/c/d/e/ok",
                "libs/sub",
                "packages/plugins/@org/Plugin-a",
                "packages/plugins/@org/plugin-b",
            ]
        );
        assert!(!found.truncated);
        assert!(find_repos(temp.0.join("nope").to_str().unwrap()).is_err());
    }

    #[test]
    fn 列目录_读_写_改名_删除() {
        let temp = TempRepo::new();
        let repo = temp.repo();
        // 新建：路径上缺的目录一起建
        let created = FsWrite {
            repo: repo.into(),
            path: "src/子 目录/a.txt".into(),
            create: true,
            ..Default::default()
        };
        let written = write(&created, b"hello\r\n\xd6\xd0").unwrap();
        assert!(!written.conflict && written.stat.exists && written.stat.size == 9);
        // 已经有了：新建失败
        assert!(write(&created, b"x").is_err());
        make_dir(repo, "docs").unwrap();
        assert!(make_dir(repo, "docs").is_err());

        // 列出来有文件有目录，没有 .git
        let mut root = list(repo, "").unwrap();
        root.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = root.iter().map(|e| (e.name.as_str(), e.dir)).collect();
        assert_eq!(names, [("docs", true), ("src", true)]);
        let inner = list(repo, "src/子 目录").unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(
            (inner[0].name.as_str(), inner[0].dir, inner[0].size),
            ("a.txt", false, 9)
        );
        assert!(inner[0].mtime > 0);

        // 按原样读，可以只读一段
        let (whole, bytes) = read(repo, "src/子 目录/a.txt", 0, READ_MAX).unwrap();
        assert_eq!(bytes, b"hello\r\n\xd6\xd0");
        assert_eq!(whole.size, 9);
        let (_, part) = read(repo, "src/子 目录/a.txt", 2, 3).unwrap();
        assert_eq!(part, b"llo");
        let (_, end) = read(repo, "src/子 目录/a.txt", 9, 3).unwrap();
        assert!(end.is_empty());
        assert!(read(repo, "src", 0, 10).is_err());

        // 改名：不盖掉别的文件
        temp.write("b.txt", b"b");
        assert!(rename(repo, "src/子 目录/a.txt", "b.txt").is_err());
        rename(repo, "src/子 目录/a.txt", "moved/c.txt").unwrap();
        assert!(!stat_at(&temp.0.join("src/子 目录/a.txt")).unwrap().exists);
        assert_eq!(
            fs::read(temp.0.join("moved/c.txt")).unwrap(),
            b"hello\r\n\xd6\xd0"
        );

        // 删除：目录连里面的一起
        remove(repo, "moved").unwrap();
        remove(repo, "b.txt").unwrap();
        assert!(remove(repo, "b.txt").is_err());
        assert!(!stat(repo, "moved").unwrap().exists);
    }

    #[test]
    fn 保存时别人改过就不写() {
        let temp = TempRepo::new();
        let opened = temp.write("a.txt", b"one").stat;
        let save = |expect: Option<FsExpect>, bytes: &[u8]| {
            let params = FsWrite {
                repo: temp.repo().into(),
                path: "a.txt".into(),
                create: false,
                expect,
            };
            write(&params, bytes).unwrap()
        };
        // 没人动过：写进去
        let saved = save(Some(opened.into()), b"two!");
        assert!(!saved.conflict);
        assert_eq!(saved.stat.size, 4);
        // 拿旧的情况再存：大小对不上，不写
        let stale = save(Some(opened.into()), b"three");
        assert!(stale.conflict);
        assert_eq!(stale.stat.size, 4);
        assert_eq!(fs::read(temp.0.join("a.txt")).unwrap(), b"two!");
        // 不管冲突：直接换掉
        assert!(!save(None, b"three").conflict);
        assert_eq!(fs::read(temp.0.join("a.txt")).unwrap(), b"three");
        // 文件被删了也算冲突
        fs::remove_file(temp.0.join("a.txt")).unwrap();
        let gone = save(Some(saved.stat.into()), b"x");
        assert!(gone.conflict && !gone.stat.exists);
        // 没留下临时文件
        assert!(list(temp.repo(), "").unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn 保存不改权限_符号链接写到它指向的文件() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = TempRepo::new();
        temp.write("run.sh", b"#!/bin/sh\n");
        let script = temp.0.join("run.sh");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        temp.write("run.sh", b"#!/bin/sh\necho hi\n");
        assert_eq!(
            fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o755
        );

        std::os::unix::fs::symlink("run.sh", temp.0.join("link.sh")).unwrap();
        temp.write("link.sh", b"new");
        assert!(
            fs::symlink_metadata(temp.0.join("link.sh"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&script).unwrap(), b"new");
        // 删链接不删它指向的文件
        remove(temp.repo(), "link.sh").unwrap();
        assert!(script.exists());
    }

    #[cfg(unix)]
    #[test]
    fn 改名盖掉文件时权限照被盖掉的() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = TempRepo::new();
        let path = |name: &str| temp.0.join(name);
        let set = |name: &str, mode: u32| {
            fs::set_permissions(path(name), fs::Permissions::from_mode(mode)).unwrap()
        };
        let mode = |name: &str| fs::metadata(path(name)).unwrap().permissions().mode() & 0o777;
        // Windows 上的程序存盘：写临时文件，再改名盖过去
        fs::write(path("run.sh"), "#!/bin/sh\n").unwrap();
        set("run.sh", 0o750);
        fs::write(path("run.sh.tmp"), "#!/bin/sh\necho hi\n").unwrap();
        set("run.sh.tmp", 0o644);
        rename_over(temp.repo(), "run.sh.tmp", "run.sh").unwrap();
        assert_eq!(fs::read(path("run.sh")).unwrap(), b"#!/bin/sh\necho hi\n");
        assert_eq!(mode("run.sh"), 0o750);
        // 改到一个没有的名字上：权限是它自己的
        fs::write(path("new.txt"), "x").unwrap();
        set("new.txt", 0o600);
        rename_over(temp.repo(), "new.txt", "moved.txt").unwrap();
        assert_eq!(mode("moved.txt"), 0o600);
    }
}
