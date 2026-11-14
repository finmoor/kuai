//! 本机仓库：在进程内启动 git，每个调用一个线程收输出。

use fr_core::i18n::tr;
use fr_core::tf;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::Path;

use anyhow::{Context as _, Result, anyhow, bail};
use fr_core::exec::{self, Killer, OnStderr};
use futures::FutureExt as _;
use futures::channel::{mpsc, oneshot};
use futures::future::{self, BoxFuture};

use crate::{ChangeStream, DirListing, GitOutput, GitStream, StreamItem, UploadItem};

/// 丢弃时杀掉 git 进程；正常拿到结果后解除。
struct KillOnDrop(Option<Killer>);

impl KillOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(killer) = self.0.take() {
            killer.kill();
        }
    }
}

pub(crate) fn git(
    repo: &str,
    args: Vec<String>,
    stdin: Option<Vec<u8>>,
) -> BoxFuture<'static, Result<GitOutput>> {
    let (running, killer) = match exec::spawn(Path::new(repo), &args, stdin) {
        Ok(started) => started,
        Err(err) => return future::ready(Err(err)).boxed(),
    };
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(running.collect());
    });
    let mut guard = KillOnDrop(Some(killer));
    async move {
        let result = rx.await.map_err(|_| anyhow!(tr("git 线程意外退出")));
        guard.disarm();
        let (stdout, finished) = result??;
        Ok(GitOutput {
            code: finished.code,
            stdout,
            stderr: finished.stderr,
            elapsed: finished.elapsed,
        })
    }
    .boxed()
}

pub(crate) fn git_stream(repo: &str, args: Vec<String>, progress: bool) -> GitStream {
    let (tx, rx) = mpsc::unbounded();
    let on_stderr: Option<OnStderr> = progress.then(|| {
        let tx = tx.clone();
        Box::new(move |chunk: &[u8]| {
            let _ = tx.unbounded_send(Ok(StreamItem::Progress(chunk.to_vec())));
        }) as OnStderr
    });
    let (running, killer) = match exec::spawn_with(Path::new(repo), &args, None, on_stderr) {
        Ok(started) => started,
        Err(err) => {
            let _ = tx.unbounded_send(Err(err));
            return GitStream {
                rx,
                _cancel: Box::new(()),
            };
        }
    };
    std::thread::spawn(move || {
        let result = running.finish(|chunk| {
            let _ = tx.unbounded_send(Ok(StreamItem::Chunk(chunk.to_vec())));
        });
        let _ = tx.unbounded_send(result.map(|finished| {
            StreamItem::Done(GitOutput {
                code: finished.code,
                stdout: Vec::new(),
                stderr: finished.stderr,
                elapsed: finished.elapsed,
            })
        }));
    });
    GitStream {
        rx,
        _cancel: Box::new(KillOnDrop(Some(killer))),
    }
}

pub(crate) fn watch(repo: &str) -> ChangeStream {
    let (tx, rx) = mpsc::unbounded();
    let watcher = fr_core::watch::watch(Path::new(repo), move || {
        let _ = tx.unbounded_send(());
    });
    // 装不上监听时 tx 已经丢弃，流马上结束
    ChangeStream {
        rx,
        _cancel: match watcher {
            Ok(watcher) => Box::new(watcher),
            Err(_) => Box::new(()),
        },
    }
}

pub(crate) fn list_dir(path: &str) -> BoxFuture<'static, Result<DirListing>> {
    let path = path.to_string();
    blocking(move || fr_core::dir::list_dir(&path))
}

/// 在另一个线程里做：网络盘上读写可能很慢，不占调用方的线程。
pub(crate) fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> BoxFuture<'static, Result<T>> {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    async move { rx.await.map_err(|_| anyhow!(tr("读取目录的线程意外退出")))? }.boxed()
}

/// 复制时一块读这么多，每块报一次进度
const COPY_CHUNK: usize = 1 << 20;

/// 把 `source` 复制成 `target`，边复制边报进度。先写到旁边的临时文件，复制完才改成正式的名字；
/// 没人等了（取消）就停下并删掉临时文件。`target` 已经有文件时，`overwrite` 为假就失败。
pub(crate) fn copy(
    source: &Path,
    target: &Path,
    overwrite: bool,
    tx: &futures::channel::mpsc::UnboundedSender<Result<UploadItem>>,
) -> Result<()> {
    if !overwrite && std::fs::symlink_metadata(target).is_ok() {
        bail!("{}", tf!("{path} 已存在", path = target.display()));
    }
    let mut input =
        File::open(source).with_context(|| tf!("无法打开 {path}", path = source.display()))?;
    let total = input.metadata().map(|meta| meta.len()).unwrap_or(0);
    let mut part = PartFile::create(target)?;
    let mut chunk = vec![0; COPY_CHUNK];
    let mut sent = 0;
    loop {
        let read = input.read(&mut chunk).context(tr("读取文件失败"))?;
        if read == 0 {
            break;
        }
        part.write(&chunk[..read])?;
        sent += read as u64;
        let progress = UploadItem::Progress {
            sent,
            total: total.max(sent),
        };
        if tx.unbounded_send(Ok(progress)).is_err() {
            return Ok(());
        }
    }
    part.finish()?;
    let _ = tx.unbounded_send(Ok(UploadItem::Done(target.to_string_lossy().into_owned())));
    Ok(())
}

/// 正在写的文件：先用临时的名字，[`Self::finish`] 时才改成正式的；中途丢弃就删掉。
pub(crate) struct PartFile {
    file: Option<File>,
    part: std::path::PathBuf,
    target: std::path::PathBuf,
}

impl PartFile {
    pub(crate) fn create(target: &Path) -> Result<PartFile> {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| tf!("无法创建 {path}", path = parent.display()))?;
        }
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        let part = target.with_file_name(format!(".{name}.kuai-{}.part", std::process::id()));
        let file =
            File::create(&part).with_context(|| tf!("无法创建 {path}", path = part.display()))?;
        Ok(PartFile {
            file: Some(file),
            part,
            target: target.to_path_buf(),
        })
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let file = self.file.as_mut().expect("还没结束");
        file.write_all(bytes).context(tr("写入文件失败"))
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        // 先关掉再改名
        drop(self.file.take());
        std::fs::rename(&self.part, &self.target)
            .with_context(|| tf!("无法写入 {path}", path = self.target.display()))
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        // 没走到 finish：不留半个文件
        if self.file.take().is_some() {
            let _ = std::fs::remove_file(&self.part);
        }
    }
}

/// 把仓库里的 `path` 移到回收站（Windows）；别的系统上直接删除。
pub(crate) fn trash(repo: &str, path: &str) -> Result<()> {
    let full = fr_core::files::resolve_inside(repo, path)?;
    #[cfg(windows)]
    {
        if std::fs::symlink_metadata(&full).is_err() {
            bail!("{}", tf!("无法删除 {path}", path = full.display()));
        }
        recycle(&full).with_context(|| tf!("无法删除 {path}", path = full.display()))
    }
    #[cfg(not(windows))]
    {
        let _ = full;
        fr_core::files::remove(repo, path)
    }
}

/// 交给系统的回收站。不弹确认框、不弹进度、不弹报错（结果由我们自己显示）。
#[cfg(windows)]
fn recycle(path: &Path) -> Result<()> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt as _;

    #[repr(C)]
    struct FileOp {
        hwnd: *mut c_void,
        func: u32,
        from: *const u16,
        to: *const u16,
        flags: u16,
        aborted: i32,
        name_mappings: *mut c_void,
        progress_title: *const u16,
    }
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHFileOperationW(op: *mut FileOp) -> i32;
    }
    const FO_DELETE: u32 = 3;
    const FOF_SILENT: u16 = 0x0004;
    const FOF_NOCONFIRMATION: u16 = 0x0010;
    const FOF_ALLOWUNDO: u16 = 0x0040;
    const FOF_NOERRORUI: u16 = 0x0400;

    // 路径列表：每个以 NUL 结尾，最后再多一个 NUL。它不认正斜杠
    let (slash, backslash) = (u16::from(b'/'), u16::from(b'\\'));
    let mut from: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .map(|unit| if unit == slash { backslash } else { unit })
        .collect();
    from.extend([0, 0]);
    let mut op = FileOp {
        hwnd: std::ptr::null_mut(),
        func: FO_DELETE,
        from: from.as_ptr(),
        to: std::ptr::null(),
        flags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI,
        aborted: 0,
        name_mappings: std::ptr::null_mut(),
        progress_title: std::ptr::null(),
    };
    // SAFETY: op 和它指着的 from 在调用期间都活着，结构的布局和系统头文件里的一致（64 位）
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 || op.aborted != 0 {
        bail!("{}", tf!("系统返回错误 {code}", code));
    }
    Ok(())
}
