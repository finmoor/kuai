//! 仓库目录里的文件操作（文件页用）：列目录、读、写、新建、改名、删除。
//!
//! 本机仓库由客户端直接调用，服务器上的仓库由 kuaid 调用，两边是同一份代码。
//!
//! 路径都相对仓库根、用 `/` 分隔（空字符串是仓库根），不能带 `.`、`..`，也不能是绝对路径：
//! 文件页只管仓库里的东西。这不是安全边界（仓库里的符号链接可以指到外面，用户自己也有终端），
//! 只是防着拼错路径改到别处去。
//!
//! Git 的数据目录 `.git`（[`is_git_name`]）文件页不列、不写：新建、改名、上传时界面就拒绝。
//! 映射到本机的盘要看得到、改得了它（盘上的 git 命令要用），那几个操作不拦。

use std::fs::{self, File, Metadata};
use std::io::{ErrorKind, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
    // 名字里有认不出的字节（不是 UTF-8 的文件名，列目录时换成了 U+FFFD）：照这个名字找不到真的
    // 那个，碰巧和别的文件同名还会改错
    if path.contains(char::REPLACEMENT_CHARACTER) {
        bail!(
            "{}",
            tf!("文件名里有无法识别的字符（不是 UTF-8 编码）：{path}", path)
        );
    }
    let mut full = PathBuf::from(repo);
    for part in path.split('/').filter(|part| !part.is_empty()) {
        // 一段只能是一个普通的名字：`..`、盘符、（Windows 上的）反斜杠都不行
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) if name == part && !win32_special(part) => {
                full.push(part)
            }
            _ => bail!("{}", tf!("路径无效：{path}", path)),
        }
    }
    Ok(full)
}

/// Windows 上有特殊意思、不是普通文件的名字：设备名（CON、NUL、COM1，带不带扩展名都是）、
/// 带冒号的（`文件:流` 是文件的数据流）、结尾是点或空格的（系统会去掉，成了另一个文件的别名）。
fn win32_special(part: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }
    if part.contains(':') || part.ends_with(['.', ' ']) {
        return true;
    }
    let stem = part.split('.').next().unwrap_or_default().trim_end();
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (upper.len() == 4
        && (upper.starts_with("COM") || upper.starts_with("LPT"))
        && upper.as_bytes()[3].is_ascii_digit()
        && upper.as_bytes()[3] != b'0')
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

fn stat_of(meta: &Metadata, full: &Path) -> FsStat {
    FsStat {
        exists: true,
        dir: meta.is_dir(),
        size: if meta.is_dir() { 0 } else { meta.len() },
        mtime: mtime(meta),
        readonly: readonly(meta, full),
    }
}

/// 这个用户写不了它（目录：在里面建、删不了）。Linux 上按这个用户实际的权限问系统：root 的
/// 644 文件只看权限位算可写，别的用户其实写不了，映射盘上的程序打开它追加时原来不报错，写不进去
/// 只在 Kuai 里提示一句。Windows 上看只读属性。
#[cfg(unix)]
fn readonly(meta: &Metadata, full: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    match std::ffi::CString::new(full.as_os_str().as_bytes()) {
        // SAFETY: 传的是以 0 结尾的路径，只读不写
        Ok(path) => unsafe { libc::access(path.as_ptr(), libc::W_OK) != 0 },
        Err(_) => meta.permissions().readonly(),
    }
}

#[cfg(not(unix))]
fn readonly(meta: &Metadata, _full: &Path) -> bool {
    meta.permissions().readonly()
}

fn stat_at(full: &Path) -> Result<FsStat> {
    match fs::metadata(full) {
        Ok(meta) => Ok(stat_of(&meta, full)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(FsStat::default()),
        Err(err) => Err(err).with_context(|| tf!("无法读取 {path}", path = full.display())),
    }
}

/// 是不是 Git 的数据目录的名字。照 git 自己的认法（core.protectNTFS、core.protectHFS，所有平台
/// 默认都开）：不分大小写，Windows 会去掉的结尾的点和空格不算，8.3 短名 `GIT~1` 也是。
pub fn is_git_name(name: &str) -> bool {
    name.trim_end_matches(['.', ' '])
        .eq_ignore_ascii_case(".git")
        || name.eq_ignore_ascii_case("git~1")
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
        if !all && is_git_name(&name) {
            continue;
        }
        let link = entry.file_type().is_ok_and(|kind| kind.is_symlink());
        // 符号链接看它指向的；指向的东西不在了就当成一个空文件
        let meta = match link {
            true => fs::metadata(entry.path()).ok(),
            false => entry.metadata().ok(),
        };
        let stat = meta
            .as_ref()
            .map(|meta| stat_of(meta, &entry.path()))
            .unwrap_or_default();
        listed.push(FsEntry {
            name,
            dir: stat.dir,
            link,
            size: stat.size,
            mtime: stat.mtime,
            readonly: stat.readonly,
        });
    }
    Ok(listed)
}

/// 一个路径现在的情况；什么都没有不算出错。
pub fn stat(repo: &str, path: &str) -> Result<FsStat> {
    stat_at(&resolve(repo, path)?)
}

/// 从 `offset` 起读最多 `len` 字节（不超过 [`READ_MAX`]），连同文件现在的情况一起返回。
/// 给了 `whole` 而文件比它大时不读，字节是空的：调用方看大小就知道太大了。
pub fn read(
    repo: &str,
    path: &str,
    offset: u64,
    len: u64,
    whole: Option<u64>,
) -> Result<(FsStat, Vec<u8>)> {
    let full = resolve_inside(repo, path)?;
    let unreadable = || tf!("无法读取 {path}", path = full.display());
    let mut file = File::open(&full).with_context(unreadable)?;
    let stat = stat_of(&file.metadata().with_context(unreadable)?, &full);
    if stat.dir {
        bail!("{}", tf!("{path} 是目录", path));
    }
    if whole.is_some_and(|whole| stat.size > whole) {
        return Ok((stat, Vec::new()));
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

/// 把文件刷到磁盘上（fsync：内容连同大小、修改时间），返回它现在的情况。映射盘上的程序要求
/// 刷盘（`FlushFileBuffers`）时用；平常的写入不刷，和本机的文件一样交给系统。
pub fn sync(repo: &str, path: &str) -> Result<FsStat> {
    let full = resolve_inside(repo, path)?;
    let failed = || tf!("无法写入 {path}", path = full.display());
    // Windows 上刷盘要有写权限；不新建、不截断
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&full)
        .with_context(failed)?;
    file.sync_all().with_context(failed)?;
    Ok(stat_of(&file.metadata().with_context(failed)?, &full))
}

/// 写一个文件（整个换掉）。见 [`FsWrite`]。
pub fn write(params: &FsWrite, bytes: &[u8]) -> Result<FsWritten> {
    let full = resolve_inside(&params.repo, &params.path)?;
    let current = stat_at(&full)?;
    if current.dir {
        bail!("{}", tf!("{path} 是目录", path = params.path));
    }
    let exists = || tf!("{path} 已存在", path = params.path);
    if params.create && current.exists {
        bail!("{}", exists());
    }
    let changed = |now: &FsStat| {
        params.expect.is_some_and(|expect| {
            !now.exists || now.mtime != expect.mtime || now.size != expect.size
        })
    };
    if changed(&current) {
        return Ok(FsWritten {
            conflict: true,
            stat: current,
        });
    }
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)
            .with_context(|| tf!("无法创建 {path}", path = parent.display()))?;
    }
    // 写临时文件的这一会儿别人改了、删了：不换（只比得了毫秒和大小，窗口缩到改名之前那一下）
    let unchanged = || stat_at(&full).is_ok_and(|now| !changed(&now));
    match replace(&full, bytes, params.create, unchanged) {
        Ok(true) => {}
        Ok(false) => {
            return Ok(FsWritten {
                conflict: true,
                stat: stat_at(&full)?,
            });
        }
        Err(err) if params.create && err.kind() == ErrorKind::AlreadyExists => {
            bail!("{}", exists())
        }
        Err(err) => {
            return Err(err).with_context(|| tf!("无法写入 {path}", path = full.display()));
        }
    }
    Ok(FsWritten {
        conflict: false,
        stat: stat_at(&full)?,
    })
}

/// 临时文件的序号：同一个进程里两个客户端同时存同一个文件也不会撞。
static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

/// 在 `target` 旁边独占地建一个临时文件：那个名字已经有东西了就换个序号，不截断、不顺着别人放的
/// 链接写，也不会删掉不是自己建的。
fn create_temp(target: &Path) -> std::io::Result<(PathBuf, File)> {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    for _ in 0..100 {
        let seq = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temp = target.with_file_name(format!(".{name}.kuai-{}-{seq}.tmp", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(file) => return Ok((temp, file)),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(ErrorKind::AlreadyExists.into())
}

/// 写符号链接时真正要写的文件：它指向的那个。指向的东西不在了（悬空的链接）照链接里写的位置建，
/// 相对的按链接所在的目录算；不是链接就是它自己。
fn written_path(target: &Path) -> std::io::Result<PathBuf> {
    let is_link = fs::symlink_metadata(target).is_ok_and(|meta| meta.file_type().is_symlink());
    if !is_link {
        return Ok(target.to_path_buf());
    }
    match fs::canonicalize(target) {
        Ok(path) => Ok(path),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let pointed = fs::read_link(target)?;
            Ok(match target.parent() {
                Some(dir) if pointed.is_relative() => dir.join(pointed),
                _ => pointed,
            })
        }
        Err(err) => Err(err),
    }
}

/// 把 `target` 的内容整个换成 `bytes`：先写到旁边的临时文件再改名，写到一半出错原来的还在。
/// 权限（和 Linux 上的属主）照原来的；是符号链接就写到它指向的文件，链接留着。
///
/// 改名之前问一遍 `unchanged`，假的话不换，返回 false。`create`：目标得还不在，这期间别人建了
/// 同名的也不盖掉（报 AlreadyExists）。
///
/// 换不了时的退路是直接写进原文件（目录里不让建文件但文件本身能写，Windows 上别的程序开着它不让
/// 换）：会先截短原文件，写到一半失败（盘满）原来的内容就没了，所以只在换不了时才走；改名失败时
/// 新内容的临时文件留着，报错里写它在哪。
fn replace(
    target: &Path,
    bytes: &[u8],
    create: bool,
    unchanged: impl Fn() -> bool,
) -> std::io::Result<bool> {
    let target = written_path(target)?;
    let existing = fs::metadata(&target).ok();
    let staged = create_temp(&target).and_then(|(temp, mut file)| {
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        match written {
            Ok(()) => Ok(temp),
            Err(err) => {
                drop(file);
                let _ = fs::remove_file(&temp);
                Err(err)
            }
        }
    });
    let temp = match staged {
        Ok(temp) => temp,
        // 目录里不让建文件，但文件本身能写：直接写
        Err(err) if err.kind() == ErrorKind::PermissionDenied && existing.is_some() && !create => {
            if !unchanged() {
                return Ok(false);
            }
            return fs::write(&target, bytes).map(|()| true);
        }
        Err(err) => return Err(err),
    };
    if let Some(meta) = &existing {
        let _ = fs::set_permissions(&temp, meta.permissions());
        #[cfg(unix)]
        keep_owner(&temp, meta);
    }
    if !unchanged() {
        let _ = fs::remove_file(&temp);
        return Ok(false);
    }
    let placed = match create {
        true => rename_noreplace(&temp, &target),
        false => fs::rename(&temp, &target),
    };
    let Err(err) = placed else {
        return Ok(true);
    };
    if create || existing.is_none() {
        let _ = fs::remove_file(&temp);
        return Err(err);
    }
    // 换不掉（Windows 上别的程序正开着它）：退一步直接写
    match fs::write(&target, bytes) {
        Ok(()) => {
            let _ = fs::remove_file(&temp);
            Ok(true)
        }
        Err(direct) => Err(std::io::Error::new(
            direct.kind(),
            tf!(
                "{err}；新内容留在 {temp}",
                err = direct,
                temp = temp.display()
            ),
        )),
    }
}

/// 改名，`to` 已经有了就失败（AlreadyExists）。Linux 和 Windows 上是一步完成的：检查和改名之间
/// 别人建了同名的也不会被盖掉。内核太老、文件系统不支持时退回先看一眼再改，中间有很短的空当。
pub fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt as _;
        const RENAME_NOREPLACE: libc::c_uint = 1;
        let from_c = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let to_c = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: 两个都是以 NUL 结尾的合法字符串，renameat2 只读它们
        let done = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from_c.as_ptr(),
                libc::AT_FDCWD,
                to_c.as_ptr(),
                RENAME_NOREPLACE,
            )
        };
        if done == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if !matches!(err.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL)) {
            return Err(err);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
        }
        let wide =
            |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain([0]).collect() };
        let (from_w, to_w) = (wide(from), wide(to));
        // SAFETY: 两个都以 NUL 结尾；不带 MOVEFILE_REPLACE_EXISTING，目标已经有了就失败
        let ok = unsafe { MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), 0) };
        match ok {
            0 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        }
    }
    #[cfg(not(windows))]
    {
        if fs::symlink_metadata(to).is_ok() {
            return Err(ErrorKind::AlreadyExists.into());
        }
        fs::rename(from, to)
    }
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
    let exists = || tf!("{path} 已存在", path = to);
    // 只改大小写、而且真是同一个文件（Windows 上不分大小写）：不算已存在。只看名字转成小写一样
    // 不够：Linux 上 readme.md 和 README.md 是两个文件，原来会把另一个盖掉
    let same = from.to_lowercase() == to.to_lowercase()
        && from != to
        && fs::canonicalize(&source).is_ok_and(|a| fs::canonicalize(&target).is_ok_and(|b| a == b));
    if !same && fs::symlink_metadata(&target).is_ok() {
        bail!("{}", exists());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| tf!("无法创建 {path}", path = parent.display()))?;
    }
    let renamed = match same {
        true => fs::rename(&source, &target),
        false => rename_noreplace(&source, &target),
    };
    match renamed {
        Err(err) if err.kind() == ErrorKind::AlreadyExists => bail!("{}", exists()),
        renamed => renamed.with_context(|| tf!("无法重命名 {path}", path = from)),
    }
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
    Ok(stat_of(&file.metadata().with_context(failed)?, &full))
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
    Ok(stat_of(&file.metadata().with_context(failed)?, &full))
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
    Ok(stat_of(&file.metadata().with_context(failed)?, &full))
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
mod git_name_tests {
    use super::is_git_name;

    #[test]
    fn 认得出各种写法的git目录() {
        for name in [
            ".git", ".GIT", ".Git", ".git.", ".git ", ".git. .", "GIT~1", "git~1",
        ] {
            assert!(is_git_name(name), "{name:?}");
        }
        for name in [
            "git",
            ".gitignore",
            ".github",
            "a.git",
            ".git~1",
            "GIT~2",
            "..git",
        ] {
            assert!(!is_git_name(name), "{name:?}");
        }
    }
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
        // 列目录时换成了 U+FFFD 的名字：对不上真的文件，不操作
        assert!(resolve(root, "a/\u{FFFD}.txt").is_err());
        // 仓库根本身不能读写、删除
        assert!(read(root, "", 0, 10, None).is_err());
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
        let (whole, bytes) = read(repo, "src/子 目录/a.txt", 0, READ_MAX, None).unwrap();
        assert_eq!(bytes, b"hello\r\n\xd6\xd0");
        assert_eq!(whole.size, 9);
        let (_, part) = read(repo, "src/子 目录/a.txt", 2, 3, None).unwrap();
        assert_eq!(part, b"llo");
        let (_, end) = read(repo, "src/子 目录/a.txt", 9, 3, None).unwrap();
        assert!(end.is_empty());
        assert!(read(repo, "src", 0, 10, None).is_err());
        // 比 whole 大的不读，只回大小
        let (info, none) = read(repo, "src/子 目录/a.txt", 0, READ_MAX, Some(8)).unwrap();
        assert_eq!((info.size, none.len()), (9, 0));
        let (_, all) = read(repo, "src/子 目录/a.txt", 0, READ_MAX, Some(9)).unwrap();
        assert_eq!(all.len(), 9);

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
    fn 刷盘返回现在的情况() {
        let temp = TempRepo::new();
        temp.write("a.txt", b"hello");
        let stat = sync(temp.repo(), "a.txt").unwrap();
        assert_eq!((stat.exists, stat.size), (true, 5));
        // 不在的文件、仓库根都不行，也不会新建
        assert!(sync(temp.repo(), "missing.txt").is_err());
        assert!(!temp.0.join("missing.txt").exists());
        assert!(sync(temp.repo(), "").is_err());
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

    #[test]
    fn 改名不盖掉别的文件_只改大小写照样能改() {
        let temp = TempRepo::new();
        let root = temp.repo();
        let path = |name: &str| temp.0.join(name);
        // 一步完成的不覆盖改名
        fs::write(path("a"), "a").unwrap();
        fs::write(path("b"), "b").unwrap();
        let err = rename_noreplace(&path("a"), &path("b")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read(path("b")).unwrap(), b"b");
        rename_noreplace(&path("a"), &path("c")).unwrap();
        assert_eq!(fs::read(path("c")).unwrap(), b"a");
        // 只改大小写
        temp.write("readme.md", b"x");
        rename(root, "readme.md", "README.md").unwrap();
        let names: Vec<String> = list(root, "")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.contains(&"README.md".to_string()), "{names:?}");
        // Linux 上大小写不同是两个文件：不能把另一个盖掉
        #[cfg(unix)]
        {
            temp.write("x.md", b"lower");
            temp.write("X.md", b"upper");
            assert!(rename(root, "x.md", "X.md").is_err());
            assert_eq!(fs::read(path("X.md")).unwrap(), b"upper");
            assert_eq!(fs::read(path("x.md")).unwrap(), b"lower");
        }
    }

    #[test]
    fn 新建时已经有了不盖掉_不留临时文件() {
        let temp = TempRepo::new();
        let params = FsWrite {
            repo: temp.repo().into(),
            path: "n.txt".into(),
            create: true,
            ..Default::default()
        };
        write(&params, b"first").unwrap();
        assert!(write(&params, b"second").is_err());
        assert_eq!(fs::read(temp.0.join("n.txt")).unwrap(), b"first");
        // 连着存好几次：每次的临时文件都收拾掉了
        for n in 0..5 {
            temp.write("n.txt", format!("v{n}").as_bytes());
        }
        let names: Vec<String> = list(temp.repo(), "")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["n.txt"]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_的设备名_数据流_结尾的点不算普通名字() {
        let root = r"C:\r";
        for bad in [
            "con", "CON.txt", "a/nul", "Com1.log", "lpt9", "a:b", "a.", "a ",
        ] {
            assert!(resolve(root, bad).is_err(), "{bad}");
        }
        for good in [
            "console.log",
            "com0",
            "com10",
            ".gitignore",
            "a.b.c",
            "auxiliary",
        ] {
            assert!(resolve(root, good).is_ok(), "{good}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn 按这个用户实际能不能写算只读() {
        use std::os::unix::fs::PermissionsExt as _;
        // root 什么都写得了，这个测试要用普通用户跑
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("用 root 跑的，跳过");
            return;
        }
        let repo = TempRepo::new();
        let root = repo.repo();
        repo.write("mine.txt", b"x");
        repo.write("ro.txt", b"x");
        fs::set_permissions(repo.0.join("ro.txt"), fs::Permissions::from_mode(0o444)).unwrap();
        fs::create_dir(repo.0.join("locked")).unwrap();
        fs::set_permissions(repo.0.join("locked"), fs::Permissions::from_mode(0o555)).unwrap();
        assert!(!stat(root, "mine.txt").unwrap().readonly);
        assert!(stat(root, "ro.txt").unwrap().readonly);
        assert!(stat(root, "locked").unwrap().readonly);
        let listed = list(root, "").unwrap();
        let flag = |name: &str| listed.iter().find(|e| e.name == name).unwrap().readonly;
        assert!((flag("ro.txt"), flag("locked"), flag("mine.txt")) == (true, true, false));
        // 让清理删得掉
        fs::set_permissions(repo.0.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn 悬空的符号链接照样写到它指向的位置() {
        let temp = TempRepo::new();
        std::os::unix::fs::symlink("target.txt", temp.0.join("link.txt")).unwrap();
        temp.write("link.txt", b"hello");
        assert_eq!(fs::read(temp.0.join("target.txt")).unwrap(), b"hello");
        assert!(
            fs::symlink_metadata(temp.0.join("link.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
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
