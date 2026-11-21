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
use fr_channel::UploadSource;
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
    /// 剪贴板里的一张图：起好的文件名和原图。发之前在后台转格式（见 [`image_bytes`]）
    Image { name: String, image: Image },
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

/// 剪贴板里的图片变成要发的东西：这里只起名字，转格式放到后台（见 [`image_bytes`]）。
pub fn image_attachment(image: &Image, now: DateTime<Local>) -> Attachment {
    let ext = match image.format {
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Gif => "gif",
        ImageFormat::Webp => "webp",
        ImageFormat::Svg => "svg",
        _ => "png",
    };
    Attachment::Image {
        name: format!("paste-{}.{ext}", now.format("%m%d-%H%M%S")),
        image: image.clone(),
    }
}

/// 剪贴板里的图片要发的内容。PNG、JPEG、GIF、WebP 原样用（那些程序认这几种），别的（截图常见的
/// 是位图）转成 PNG：大截图要几十到几百毫秒，不在界面线程上调。
fn image_bytes(image: Image) -> Result<Vec<u8>, String> {
    match image.format {
        ImageFormat::Png
        | ImageFormat::Jpeg
        | ImageFormat::Gif
        | ImageFormat::Webp
        | ImageFormat::Svg => Ok(image.bytes),
        _ => to_png(&image.bytes),
    }
}

/// [`uploads`] 的结果：要传的（名字和内容），和停在哪个文件夹前面。
pub type Uploads = (Vec<(String, UploadSource)>, Option<String>);

/// 要传到服务器上的：名字和内容。在后台调：剪贴板里的图片要转格式，网络盘上看一眼是不是文件夹也
/// 可能慢。文件夹传不了，停在第一个文件夹前面：返回它前面的那些，和它的名字。
pub fn uploads(items: Vec<Attachment>) -> Result<Uploads, String> {
    let mut ready = Vec::with_capacity(items.len());
    for item in items {
        let name = item.name();
        let source = match item {
            Attachment::File(path) if path.is_dir() => return Ok((ready, Some(name))),
            Attachment::File(path) => UploadSource::File(path),
            Attachment::Image { image, .. } => UploadSource::Bytes(image_bytes(image)?),
        };
        ready.push((name, source));
    }
    Ok((ready, None))
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

/// 本机终端：每样东西在本机的路径。图片转好格式存到临时目录里（顺手清掉以前存的、已经过期的）。
/// 在后台调。
pub fn local_paths(items: Vec<Attachment>) -> Result<Vec<String>, String> {
    let dir = std::env::temp_dir().join("kuai").join("paste");
    items
        .into_iter()
        .map(|item| match item {
            Attachment::File(path) => Ok(path.to_string_lossy().into_owned()),
            Attachment::Image { name, image } => {
                let bytes = image_bytes(image)?;
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
        // PowerShell（和 Git Bash）的双引号里 `$(…)`、`$变量`、反引号会被展开执行：带这两个字符的
        // 用单引号，里面的单引号写两遍，原样不展开
        if path.contains(['$', '`']) {
            return format!("'{}'", path.replace('\'', "''"));
        }
        // 别的和 Windows 终端拖文件进去一样：有空格（或者 shell 里有特殊意思的字符）才加双引号
        let special = |c: char| c.is_whitespace() || "&()^;'`$,{}[]".contains(c);
        return match path.contains(special) {
            true => format!("\"{path}\""),
            false => path.to_string(),
        };
    }
    fr_core::git::sh_quote(path)
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
        assert_eq!(quote(r"C:\Users\me\a.png", true), r"C:\Users\me\a.png");
        assert_eq!(quote(r"C:\Users\me\图 1.png", true), "\"C:\\Users\\me\\图 1.png\"");
        assert_eq!(quote(r"D:\a&b\c.txt", true), "\"D:\\a&b\\c.txt\"");
        // PowerShell 的双引号里会展开的：用单引号，单引号写两遍
        assert_eq!(quote(r"D:\a$(calc).txt", true), r"'D:\a$(calc).txt'");
        assert_eq!(quote(r"D:\it's `x`.txt", true), r"'D:\it''s `x`.txt'");
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
        let Attachment::Image { name, image } = image_attachment(&image, now) else {
            panic!("应该是图片");
        };
        assert_eq!(name, "paste-1112-130405.png");
        let bytes = image_bytes(image).unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));
        let back = image::load_from_memory(&bytes).unwrap().to_rgb8();
        assert_eq!(back.into_raw(), [255, 0, 0, 0, 0, 255]);

        let jpeg = Image::from_bytes(ImageFormat::Jpeg, b"jpeg-bytes".to_vec());
        let Attachment::Image { name, image } = image_attachment(&jpeg, now) else {
            panic!("应该是图片");
        };
        assert_eq!(name, "paste-1112-130405.jpg");
        assert_eq!(image_bytes(image).unwrap(), b"jpeg-bytes");
        // 坏的图片：报错，不是崩
        let broken = Image::from_bytes(ImageFormat::Bmp, b"not a bitmap".to_vec());
        assert!(image_bytes(broken).is_err());
    }

    #[test]
    fn 传到服务器时停在文件夹前面() {
        let dir = std::env::temp_dir().join(format!("kuai-upload-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let png = Image::from_bytes(ImageFormat::Png, b"png-bytes".to_vec());
        let items = vec![
            Attachment::File(dir.join("a.txt")),
            image_attachment(&png, Local::now()),
            Attachment::File(dir.join("folder")),
            Attachment::File(dir.join("b.txt")),
        ];
        let (ready, folder) = uploads(items).unwrap();
        let names: Vec<&str> = ready.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "a.txt");
        assert!(matches!(&ready[1].1, UploadSource::Bytes(bytes) if bytes == b"png-bytes"));
        assert_eq!(folder.as_deref(), Some("folder"));
        std::fs::remove_dir_all(&dir).unwrap();
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
    fn 名字() {
        assert_eq!(Attachment::File(r"C:\a\图 1.png".into()).name(), "图 1.png");
        let image = Attachment::Image {
            name: "paste-1.png".into(),
            image: Image::from_bytes(ImageFormat::Png, Vec::new()),
        };
        assert_eq!(image.name(), "paste-1.png");
    }
}
