//! 往终端里发图片、文件。
//!
//! 终端那条路只能过文字。Claude Code、Codex 这类程序收图片靠的是「贴进来的是一个图片文件的路径」
//! （在本地用它们时拖文件进终端就是这样），所以发过去的其实是路径：
//!
//! - 本机的终端：程序读得到本机的文件，路径直接给它；剪贴板里的图片先存成临时文件。
//! - 服务器上的终端：先把文件传到服务器（`fr_channel::Connection::upload`，kuaid 放在
//!   `~/.kuai/drop/` 下），给它服务器上的路径。
//!
//! 每个路径单独作为一次粘贴写进去，后面跟一个空格：这些程序是按「一次粘贴是不是一个图片路径」来认的。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local};
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::{Image, ImageFormat};

use super::keys;

/// 本机终端里贴的图片存成的临时文件，留这么久
const KEEP_LOCAL: Duration = Duration::from_secs(7 * 24 * 3600);

/// 要发给终端里的程序的一样东西。
pub enum Attachment {
    /// 本机的一个文件
    File(PathBuf),
    /// 内存里的一张图（剪贴板里的）：起好的文件名和编码好的内容
    Image { name: String, bytes: Vec<u8> },
}

impl Attachment {
    /// 显示用的名字，也是传到服务器上用的文件名。
    pub fn name(&self) -> String {
        match self {
            Attachment::File(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned()),
            Attachment::Image { name, .. } => name.clone(),
        }
    }
}

/// 剪贴板里的图片变成要发的东西。PNG、JPEG、GIF、WebP 原样用（那些程序认这几种），
/// 别的（截图常见的是位图）转成 PNG。
pub fn image_attachment(image: &Image, now: DateTime<Local>) -> Result<Attachment, String> {
    let (ext, bytes) = match image.format {
        ImageFormat::Png => ("png", image.bytes.clone()),
        ImageFormat::Jpeg => ("jpg", image.bytes.clone()),
        ImageFormat::Gif => ("gif", image.bytes.clone()),
        ImageFormat::Webp => ("webp", image.bytes.clone()),
        ImageFormat::Svg => ("svg", image.bytes.clone()),
        _ => ("png", to_png(&image.bytes)?),
    };
    Ok(Attachment::Image {
        name: format!("paste-{}.{ext}", now.format("%m%d-%H%M%S")),
        bytes,
    })
}

fn to_png(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let failed = |err: image::ImageError| tf!("无法读取剪贴板中的图片：{err}", err);
    let decoded = image::load_from_memory(bytes).map_err(failed)?;
    let mut png = Vec::new();
    decoded
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(failed)?;
    Ok(png)
}

/// 本机终端：每样东西在本机的路径。图片存到临时目录里（顺手清掉以前存的、已经过期的）。
pub fn local_paths(items: Vec<Attachment>) -> Result<Vec<String>, String> {
    let dir = std::env::temp_dir().join("kuai").join("paste");
    items
        .into_iter()
        .map(|item| match item {
            Attachment::File(path) => Ok(path.to_string_lossy().into_owned()),
            Attachment::Image { name, bytes } => {
                let path = save_image(&dir, &name, &bytes)
                    .map_err(|err| tf!("无法保存图片：{err}", err))?;
                Ok(path.to_string_lossy().into_owned())
            }
        })
        .collect()
}

fn save_image(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    sweep(dir, SystemTime::now());
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, "png"));
    // 同一秒里贴了两张：后面加序号
    for n in 1..1000 {
        let path = match n {
            1 => dir.join(name),
            n => dir.join(format!("{stem}-{n}.{ext}")),
        };
        let created = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        match created {
            Ok(mut file) => {
                std::io::Write::write_all(&mut file, bytes)?;
                return Ok(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::other(tr("同名文件过多")))
}

fn sweep(dir: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > KEEP_LOCAL);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 把几个路径变成写给程序的字节：每个路径是一次粘贴（程序开了「括号粘贴」就包上标记），
/// 后面跟一个空格。`windows`：路径是本机 Windows 的（引号的加法不一样）。
pub fn paste_paths(paths: &[String], bracketed: bool, windows: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    for path in paths {
        bytes.extend(keys::paste(&quote(path, windows), bracketed));
        bytes.push(b' ');
    }
    bytes
}

/// 路径里有空格、引号这些就加引号，没有就原样。
fn quote(path: &str, windows: bool) -> String {
    if windows {
        // 和 Windows 终端拖文件进去一样：有空格（或者 shell 里有特殊意思的字符）才加双引号
        let special = |c: char| c.is_whitespace() || "&()^;'`$,{}[]".contains(c);
        return match path.contains(special) {
            true => format!("\"{path}\""),
            false => path.to_string(),
        };
    }
    let plain = |c: char| c.is_alphanumeric() || "_./+@=,:-%".contains(c);
    match path.chars().all(plain) && !path.is_empty() {
        true => path.to_string(),
        false => format!("'{}'", path.replace('\'', "'\\''")),
    }
}

/// 把字节数写成人看的大小。
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.;
    let size = bytes as f64;
    if size < KB {
        format!("{bytes} B")
    } else if size < KB * KB {
        format!("{:.0} KB", size / KB)
    } else if size < KB * KB * KB {
        format!("{:.1} MB", size / KB / KB)
    } else {
        format!("{:.2} GB", size / KB / KB / KB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    #[test]
    fn 每个路径是一次粘贴_后面跟空格() {
        let paths = ["/root/.kuai/drop/a.png".to_string(), "/tmp/b c.txt".to_string()];
        assert_eq!(
            paste_paths(&paths, true, false),
            b"\x1b[200~/root/.kuai/drop/a.png\x1b[201~ \x1b[200~'/tmp/b c.txt'\x1b[201~ "
        );
        assert_eq!(
            paste_paths(&paths, false, false),
            b"/root/.kuai/drop/a.png '/tmp/b c.txt' "
        );
        assert!(paste_paths(&[], true, false).is_empty());
    }

    #[test]
    fn 路径里有空格和引号才加引号() {
        // 服务器上（POSIX shell）
        assert_eq!(quote("/root/.kuai/drop/截图-2.png", false), "/root/.kuai/drop/截图-2.png");
        assert_eq!(quote("/a/it's here.png", false), r"'/a/it'\''s here.png'");
        assert_eq!(quote("/a/$(x).png", false), "'/a/$(x).png'");
        // 本机（Windows）
        assert_eq!(quote(r"C:\Users\Feng\a.png", true), r"C:\Users\Feng\a.png");
        assert_eq!(quote(r"C:\Users\Feng\图 1.png", true), "\"C:\\Users\\Feng\\图 1.png\"");
        assert_eq!(quote(r"D:\a&b\c.txt", true), "\"D:\\a&b\\c.txt\"");
    }

    #[test]
    fn 剪贴板里的位图转成_png_别的原样() {
        let now = Local.with_ymd_and_hms(2026, 11, 12, 13, 4, 5).unwrap();
        // 一个 2×1 的位图
        let mut bmp = Vec::new();
        image::RgbImage::from_raw(2, 1, vec![255, 0, 0, 0, 0, 255])
            .unwrap()
            .write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
            .unwrap();
        let image = Image::from_bytes(ImageFormat::Bmp, bmp);
        let Attachment::Image { name, bytes } = image_attachment(&image, now).unwrap() else {
            panic!("应该是图片");
        };
        assert_eq!(name, "paste-1112-130405.png");
        assert!(bytes.starts_with(b"\x89PNG"));
        let back = image::load_from_memory(&bytes).unwrap().to_rgb8();
        assert_eq!(back.into_raw(), [255, 0, 0, 0, 0, 255]);

        let jpeg = Image::from_bytes(ImageFormat::Jpeg, b"jpeg-bytes".to_vec());
        let Attachment::Image { name, bytes } = image_attachment(&jpeg, now).unwrap() else {
            panic!("应该是图片");
        };
        assert_eq!((name.as_str(), &bytes[..]), ("paste-1112-130405.jpg", &b"jpeg-bytes"[..]));
        // 坏的图片：报错，不是崩
        let broken = Image::from_bytes(ImageFormat::Bmp, b"not a bitmap".to_vec());
        assert!(image_attachment(&broken, now).is_err());
    }

    #[test]
    fn 本机贴的图片存成临时文件_同名的加序号() {
        let dir = std::env::temp_dir().join(format!("kuai-attach-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = save_image(&dir, "paste-1.png", b"one").unwrap();
        let second = save_image(&dir, "paste-1.png", b"two").unwrap();
        assert_eq!(first, dir.join("paste-1.png"));
        assert_eq!(second, dir.join("paste-1-2.png"));
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        // 过期的清掉，新的留着
        let old = std::fs::File::options().write(true).open(&first).unwrap();
        old.set_modified(SystemTime::now() - KEEP_LOCAL - Duration::from_secs(60))
            .unwrap();
        drop(old);
        save_image(&dir, "paste-2.png", b"three").unwrap();
        assert!(!first.exists());
        assert!(second.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn 大小写成人看的() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(340 * 1024), "340 KB");
        assert_eq!(human_size(5 * 1024 * 1024 + 300 * 1024), "5.3 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    #[test]
    fn 名字() {
        assert_eq!(Attachment::File(r"C:\a\图 1.png".into()).name(), "图 1.png");
        let image = Attachment::Image {
            name: "paste-1.png".into(),
            bytes: Vec::new(),
        };
        assert_eq!(image.name(), "paste-1.png");
    }
}
