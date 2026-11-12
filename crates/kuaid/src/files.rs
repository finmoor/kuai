//! 客户端传上来的文件（往终端里发的图片、文件）：放在 `~/.kuai/drop/` 下，过期的自己清掉。
//!
//! 终端那条路只能过文字。要把图片、文件交给终端里的程序（Claude Code、Codex 这些），
//! 客户端先把文件传到这里，拿到它在服务器上的路径，再把路径贴进终端，程序自己去读。
//! 放在仓库外面：不弄脏仓库的状态。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, bail};
use fr_core::protocol::{FilePut, FilePutReply};

/// 传上来的文件留这么久
const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);
/// 没传完的留这么久（常驻进程换程序时来不及删，会剩下）
const KEEP_PARTIAL: Duration = Duration::from_secs(24 * 3600);
/// 文件名最长这么多个字
const MAX_NAME: usize = 80;
/// 没传完的文件叫 `.kuai-<进程号>-<序号>.part`
const PARTIAL_PREFIX: &str = ".kuai-";
const PARTIAL_SUFFIX: &str = ".part";

/// 默认放在哪：主目录下的 `.kuai/drop`。
pub fn default_dir() -> PathBuf {
    Path::new(&fr_core::dir::home()).join(".kuai").join("drop")
}

/// 一个客户端正在传的文件。客户端断开时丢弃，没传完的跟着删掉。
pub struct Uploads {
    dir: PathBuf,
    next: u64,
    open: HashMap<String, Upload>,
}

struct Upload {
    file: File,
    part: PathBuf,
    name: String,
    received: u64,
}

impl Drop for Upload {
    fn drop(&mut self) {
        // 传完的已经另有一个名字指着它了，这个临时的名字不管哪种情况都删
        let _ = std::fs::remove_file(&self.part);
    }
}

impl Uploads {
    pub fn new(dir: PathBuf) -> Uploads {
        Uploads {
            dir,
            next: 1,
            open: HashMap::new(),
        }
    }

    /// 处理一条 `file_put`：`bytes` 是文件的下一块。
    pub fn put(&mut self, params: FilePut, bytes: &[u8]) -> Result<FilePutReply> {
        if params.abort {
            self.open.remove(&params.upload);
            return Ok(FilePutReply {
                upload: params.upload,
                ..Default::default()
            });
        }
        let id = match params.upload.is_empty() {
            true => self.begin(&params.name)?,
            false => params.upload,
        };
        let upload = self
            .open
            .get_mut(&id)
            .context("这次上传已经不在了（连接断过，或者已经传完）")?;
        if let Err(err) = upload.file.write_all(bytes) {
            // 多半是盘满了：已经收到的不留
            self.open.remove(&id);
            return Err(err).context("文件写不下去");
        }
        upload.received += bytes.len() as u64;
        let received = upload.received;
        let path = match params.done {
            true => {
                let upload = self.open.remove(&id).expect("刚才还在");
                Some(upload.finish(&self.dir)?)
            }
            false => None,
        };
        Ok(FilePutReply {
            upload: id,
            received,
            path,
        })
    }

    /// 开一个新文件，返回这次上传的编号。
    fn begin(&mut self, name: &str) -> Result<String> {
        create_private_dir(&self.dir)?;
        let id = format!("u{}", self.next);
        self.next += 1;
        let part = self.dir.join(format!(
            "{PARTIAL_PREFIX}{}-{id}{PARTIAL_SUFFIX}",
            std::process::id()
        ));
        // 上一个同进程号的进程剩下的：换掉
        let _ = std::fs::remove_file(&part);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // 可能是截图、日志这类东西，只有本人能读
            options.mode(0o600);
        }
        let file = options
            .open(&part)
            .with_context(|| format!("建不了 {}", part.display()))?;
        self.open.insert(
            id.clone(),
            Upload {
                file,
                part,
                name: safe_name(name),
                received: 0,
            },
        );
        Ok(id)
    }
}

impl Upload {
    /// 传完了：给它起正式的名字，返回完整路径。重名的在后面加 -2、-3……
    fn finish(self, dir: &Path) -> Result<String> {
        self.file.sync_all().context("文件写不下去")?;
        let (stem, ext) = match self.name.rfind('.') {
            Some(dot) if dot > 0 => self.name.split_at(dot),
            _ => (self.name.as_str(), ""),
        };
        for n in 1..1000 {
            let name = match n {
                1 => self.name.clone(),
                n => format!("{stem}-{n}{ext}"),
            };
            let path = dir.join(name);
            // 再起一个名字指向同一个文件：已经有这个名字的话会失败，不会盖掉别人的
            match std::fs::hard_link(&self.part, &path) {
                Ok(()) => return Ok(path.to_string_lossy().into_owned()),
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err).with_context(|| format!("放不到 {}", path.display()));
                }
            }
        }
        bail!("叫 {} 的文件太多了", self.name)
    }
}

fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("建不了 {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// 把客户端给的文件名变成能直接贴进 shell 的：只留最后一段，字母（含中文）、数字和 `._-+@=,`
/// 以外的都换成下划线，这样路径里没有空格和引号，贴进终端不用再加引号。
pub fn safe_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut safe: String = last
        .chars()
        .map(|c| match c {
            c if c.is_alphanumeric() => c,
            '.' | '_' | '-' | '+' | '@' | '=' | ',' => c,
            _ => '_',
        })
        .collect();
    // 不要隐藏文件，也不要看起来像命令选项的
    let trimmed = safe.trim_start_matches(['.', '-']);
    if trimmed.len() != safe.len() {
        safe = trimmed.to_string();
    }
    if safe.is_empty() || safe.chars().all(|c| c == '_') {
        return "file".into();
    }
    // 太长的截掉中间，扩展名留着
    let count = safe.chars().count();
    if count > MAX_NAME {
        let ext: String = match safe.rfind('.') {
            Some(dot) if safe[dot..].chars().count() <= 12 => safe[dot..].to_string(),
            _ => String::new(),
        };
        let keep = MAX_NAME - ext.chars().count();
        let stem: String = safe.chars().take(keep).collect();
        safe = format!("{stem}{ext}");
    }
    safe
}

/// 清掉过期的：传好的文件留 [`KEEP`]，没传完的留 [`KEEP_PARTIAL`]。常驻进程启动时和之后隔一阵调用。
pub fn sweep(dir: &Path) {
    sweep_at(dir, SystemTime::now());
}

fn sweep_at(dir: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let partial = name.starts_with(PARTIAL_PREFIX) && name.ends_with(PARTIAL_SUFFIX);
        let keep = if partial { KEEP_PARTIAL } else { KEEP };
        let age = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok());
        if age.is_some_and(|age| age > keep) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 测试用的临时目录，丢弃时删掉。
    pub(crate) struct TempDir(pub PathBuf);

    impl TempDir {
        pub(crate) fn new() -> TempDir {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "kuaid-files-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            TempDir(dir)
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn first(name: &str) -> FilePut {
        FilePut {
            name: name.into(),
            ..Default::default()
        }
    }

    fn more(upload: &str, done: bool) -> FilePut {
        FilePut {
            upload: upload.into(),
            done,
            ..Default::default()
        }
    }

    #[test]
    fn 文件名变成能直接贴进_shell_的() {
        assert_eq!(safe_name("截图 2026-11-12 (1).png"), "截图_2026-11-12__1_.png");
        assert_eq!(safe_name(r"C:\Users\Feng\a b.txt"), "a_b.txt");
        assert_eq!(safe_name("/etc/../x'y\"z;$(rm).sh"), "x_y_z___rm_.sh");
        assert_eq!(safe_name("..hidden"), "hidden");
        assert_eq!(safe_name("-rf"), "rf");
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name("???"), "file");
        assert_eq!(safe_name("a/"), "file");
        let long = format!("{}.mp4", "长".repeat(200));
        let safe = safe_name(&long);
        assert_eq!(safe.chars().count(), MAX_NAME);
        assert!(safe.ends_with(".mp4"));
    }

    #[test]
    fn 一块块传上来_传完才有正式的名字() {
        let dir = TempDir::new();
        let mut uploads = Uploads::new(dir.0.clone());
        let reply = uploads.put(first("a b.txt"), b"hello ").unwrap();
        assert_eq!((reply.received, reply.path.clone()), (6, None));
        assert_eq!(dir.names().len(), 1, "这时只有没传完的那个");
        assert!(dir.names()[0].ends_with(PARTIAL_SUFFIX));
        let reply = uploads.put(more(&reply.upload, false), b"wor").unwrap();
        assert_eq!(reply.received, 9);
        let reply = uploads.put(more(&reply.upload, true), b"ld").unwrap();
        assert_eq!(reply.received, 11);
        let path = PathBuf::from(reply.path.unwrap());
        assert_eq!(path, dir.0.join("a_b.txt"));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        assert_eq!(dir.names(), ["a_b.txt"]);
        // 传完以后这个编号就不在了
        assert!(uploads.put(more(&reply.upload, true), b"x").is_err());
    }

    #[test]
    fn 一块就传完的_和空文件() {
        let dir = TempDir::new();
        let mut uploads = Uploads::new(dir.0.clone());
        let mut whole = first("one.png");
        whole.done = true;
        let reply = uploads.put(whole, b"png").unwrap();
        assert_eq!(std::fs::read(reply.path.unwrap()).unwrap(), b"png");
        let mut empty = first("empty");
        empty.done = true;
        let reply = uploads.put(empty, b"").unwrap();
        assert_eq!(std::fs::read(reply.path.unwrap()).unwrap(), b"");
    }

    #[test]
    fn 重名的换个名字_不盖掉原来的() {
        let dir = TempDir::new();
        let mut uploads = Uploads::new(dir.0.clone());
        let mut paths = Vec::new();
        for content in [&b"1"[..], b"2", b"3"] {
            let mut put = first("shot.png");
            put.done = true;
            paths.push(uploads.put(put, content).unwrap().path.unwrap());
        }
        assert_eq!(dir.names(), ["shot-2.png", "shot-3.png", "shot.png"]);
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"1");
        assert_eq!(std::fs::read(&paths[2]).unwrap(), b"3");
        // 没有扩展名的
        for _ in 0..2 {
            let mut put = first("LICENSE");
            put.done = true;
            uploads.put(put, b"x").unwrap();
        }
        assert!(dir.names().contains(&"LICENSE-2".to_string()));
    }

    #[test]
    fn 不传了_或者客户端断开_没传完的删掉() {
        let dir = TempDir::new();
        let mut uploads = Uploads::new(dir.0.clone());
        let a = uploads.put(first("a.bin"), b"123").unwrap().upload;
        let b = uploads.put(first("b.bin"), b"456").unwrap().upload;
        assert_ne!(a, b);
        assert_eq!(dir.names().len(), 2);
        let abort = FilePut {
            upload: a.clone(),
            abort: true,
            ..Default::default()
        };
        uploads.put(abort, b"").unwrap();
        assert_eq!(dir.names().len(), 1);
        assert!(uploads.put(more(&a, true), b"").is_err());
        // 客户端断开
        drop(uploads);
        assert!(dir.names().is_empty());
    }

    #[test]
    fn 过期的清掉() {
        let dir = TempDir::new();
        std::fs::create_dir_all(&dir.0).unwrap();
        let now = SystemTime::now();
        let aged = |name: &str, age: Duration| {
            let path = dir.0.join(name);
            let file = File::create(&path).unwrap();
            file.set_modified(now - age).unwrap();
        };
        let day = Duration::from_secs(24 * 3600);
        aged("新的.png", day);
        aged("旧的.png", 8 * day);
        aged(".kuai-1-u1.part", day / 2);
        aged(".kuai-1-u2.part", 2 * day);
        std::fs::create_dir(dir.0.join("子目录")).unwrap();
        sweep_at(&dir.0, now);
        assert_eq!(dir.names(), [".kuai-1-u1.part", "子目录", "新的.png"]);
        // 目录不在也没事
        sweep(&dir.0.join("没有"));
    }
}
