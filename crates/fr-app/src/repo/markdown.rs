//! 文件页的 Markdown 预览：界面库排好的样子（标题、列表、表格、代码块、图片），工具栏的「预览」、
//! Ctrl+Shift+V 在它和源文件之间切换。显示的是编辑器里现在的文字（没保存的也算）。
//!
//! - 链接：网址交给浏览器；指向仓库里的文件、目录的（相对这个文件所在的目录，`/` 开头的从仓库根算）
//!   在文件页打开，Markdown 打开成预览。`.git` 里的不算。
//! - 图片：仓库里的经仓库通道读（只认仓库里的：跟着符号链接出了仓库的不读，见
//!   `fr_core::files::confined`）；网址上的自己下（界面库没有 HTTP 客户端），`data:` 的就地解开。
//!   读回来记着（[`MdImages`]），读回来之前空着。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui_kit::component::text::TextView;
use gpui_kit::*;

use super::RepoView;
use super::files::{Class, IMAGE_LIMIT, OpenFile, TextDoc, classify, split};

/// 一张图片读到哪了。
#[derive(Clone)]
enum Fetch {
    Reading,
    /// 读回来了；读不了、不是图片的是 None
    Done(Option<Arc<Image>>),
}

/// Markdown 预览里的图片：仓库里的按相对仓库根的路径、网址上的按地址记着读回来的。预览画到没读过的
/// 图片时，把它发给读图片的任务（第一次预览时起，见 [`RepoView::start_md_images`]）。
#[derive(Default)]
pub(crate) struct MdImages {
    got: Arc<Mutex<HashMap<String, Fetch>>>,
    want: Option<UnboundedSender<String>>,
    _task: Option<Task<()>>,
}

/// Markdown 里的链接、图片地址指向仓库里的哪个路径（相对仓库根，`/` 分隔）。`dir` 是这个 Markdown
/// 所在的目录。网址、页内锚点、出了仓库的（`..` 太多）不算。
pub(super) fn resolve_link(dir: &str, link: &str) -> Option<String> {
    let link = link.trim();
    let lower = link.to_ascii_lowercase();
    if link.is_empty()
        || link.starts_with('#')
        || lower.contains("://")
        || lower.starts_with("mailto:")
        || lower.starts_with("data:")
    {
        return None;
    }
    let link = link.split(['#', '?']).next().unwrap_or_default();
    let link = crate::web_serve::decode_path(link).unwrap_or_else(|| link.to_string());
    let (base, rest) = match link.strip_prefix('/') {
        Some(rest) => ("", rest.to_string()),
        None => (dir, link),
    };
    let mut parts: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
    for part in rest.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            // Git 的数据目录不打开、不读（config 里可能有远程地址带着的口令；网页服务那边也不给）
            part if fr_core::files::is_git_name(part) => return None,
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// 读不出来的图片：界面库按读不了处理（显示替代文字）。
fn missing() -> ImageSource {
    ImageSource::Resource(Resource::Path(Arc::from(Path::new(""))))
}

/// 网址上的、`data:` 的图片：不在仓库里，地址照原样当键记着。
fn web_image(uri: &str) -> bool {
    let lower = uri.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("data:")
}

/// 网址上的、`data:` 的图片读回来（网址上的自己下，最多 [`IMAGE_LIMIT`]）。
fn fetch_web_image(uri: &str) -> Option<(ImageFormat, Vec<u8>)> {
    let uri = uri.trim();
    if uri.len() > 5 && uri[..5].eq_ignore_ascii_case("data:") {
        return data_image(&uri[5..]);
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent(format!("Kuai/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut response = agent.get(uri).call().ok()?;
    let mime = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = response
        .body_mut()
        .with_config()
        .limit(IMAGE_LIMIT)
        .read_to_vec()
        .ok()?;
    Some((image_format(&mime, &bytes)?, bytes))
}

/// `data:` 后面那段（`image/png;base64,iVBOR…` 或 `image/svg+xml,%3Csvg…`）里的图片。
fn data_image(rest: &str) -> Option<(ImageFormat, Vec<u8>)> {
    let (meta, data) = rest.split_once(',')?;
    let base64 = meta.to_ascii_lowercase().ends_with(";base64");
    let mime = meta.split(';').next().unwrap_or_default();
    let bytes = match base64 {
        true => decode_base64(data)?,
        false => crate::web_serve::decode_path(data)?.into_bytes(),
    };
    Some((image_format(mime, &bytes)?, bytes))
}

/// Base64（标准的和网址用的两种字母表都认；空白跳过，结尾的 `=` 可有可无）。
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let value = |byte: u8| match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut bits, mut count) = (0u32, 0u32);
    for byte in text.bytes() {
        if byte.is_ascii_whitespace() || byte == b'=' {
            continue;
        }
        bits = (bits << 6) | u32::from(value(byte)?);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    Some(out)
}

/// 图片的格式：先看给的类型（`image/png`），认不出再看开头几个字节。
fn image_format(mime: &str, bytes: &[u8]) -> Option<ImageFormat> {
    let mime = mime.split(';').next().unwrap_or_default().trim();
    let by_type = match mime.to_ascii_lowercase().as_str() {
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Some(ImageFormat::Jpeg),
        "image/gif" => Some(ImageFormat::Gif),
        "image/webp" => Some(ImageFormat::Webp),
        "image/svg+xml" => Some(ImageFormat::Svg),
        "image/bmp" => Some(ImageFormat::Bmp),
        "image/x-icon" | "image/vnd.microsoft.icon" => Some(ImageFormat::Ico),
        _ => None,
    };
    by_type.or_else(|| sniff_image(bytes))
}

/// 看开头几个字节认图片格式。
fn sniff_image(bytes: &[u8]) -> Option<ImageFormat> {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    let text = text.trim_start_matches('\u{feff}').trim_start();
    Some(match bytes {
        [0x89, b'P', b'N', b'G', ..] => ImageFormat::Png,
        [0xFF, 0xD8, 0xFF, ..] => ImageFormat::Jpeg,
        [b'G', b'I', b'F', b'8', ..] => ImageFormat::Gif,
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => ImageFormat::Webp,
        [b'B', b'M', ..] => ImageFormat::Bmp,
        [0, 0, 1, 0, ..] => ImageFormat::Ico,
        _ if text.starts_with("<svg") || (text.starts_with("<?xml") && text.contains("<svg")) => {
            ImageFormat::Svg
        }
        _ => return None,
    })
}

impl RepoView {
    /// 起读 Markdown 图片的任务（已经起过不再起）。
    pub(super) fn start_md_images(&mut self, cx: &mut Context<Self>) {
        if self.files.md_images.want.is_some() {
            return;
        }
        let (want, mut wanted) = unbounded::<String>();
        let task = cx.spawn(async move |this, cx| {
            while let Some(path) = wanted.next().await {
                let read = match web_image(&path) {
                    true => {
                        let uri = path.clone();
                        let got = cx
                            .background_executor()
                            .spawn(async move { fetch_web_image(&uri) })
                            .await;
                        let what = match path
                            .get(..5)
                            .is_some_and(|head| head.eq_ignore_ascii_case("data:"))
                        {
                            true => "的 data: 图片",
                            false => "网址上的图片",
                        };
                        log::info!(
                            "Markdown 预览里{what}：{}",
                            match &got {
                                Some((_, bytes)) => format!("读回 {} 字节", bytes.len()),
                                None => "读不了".to_string(),
                            }
                        );
                        got
                    }
                    false => {
                        let Ok(Some((channel, repo))) =
                            this.update(cx, |this, _| this.files_channel())
                        else {
                            // 还没连上：下次画到它时再读
                            this.update(cx, |this, _| this.files.md_images.forget(&path))
                                .ok();
                            continue;
                        };
                        match channel.fs_open_confined(&repo, &path, IMAGE_LIMIT).await {
                            Ok((stat, bytes)) if stat.exists && !bytes.is_empty() => {
                                let format = match classify(&path) {
                                    Class::Image(format) => Some(format),
                                    _ => sniff_image(&bytes),
                                };
                                log::info!(
                                    "Markdown 预览里的图片 {path}：读回 {} 字节",
                                    bytes.len()
                                );
                                format.map(|format| (format, bytes))
                            }
                            other => {
                                log::info!(
                                    "Markdown 预览里的图片 {path} 读不了：{}",
                                    match other {
                                        Err(err) => format!("{err:#}"),
                                        Ok(_) => "没有这个文件或是空的".to_string(),
                                    }
                                );
                                None
                            }
                        }
                    }
                };
                let image = read.map(|(format, bytes)| Arc::new(Image::from_bytes(format, bytes)));
                let alive = this.update(cx, |this, cx| {
                    this.files.md_images.set(path, Fetch::Done(image));
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        self.files.md_images.want = Some(want);
        self.files.md_images._task = Some(task);
    }

    /// Markdown 的预览。
    pub(super) fn render_markdown(
        &self,
        tab: &OpenFile,
        doc: &TextDoc,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let text = doc.editor.read(cx).value();
        let dir = split(&tab.path).0.to_string();
        let view = cx.entity().downgrade();
        let link_dir = dir.clone();
        let images = self.md_image_source(dir);
        TextView::markdown(("md-preview", tab.id), text)
            .selectable(true)
            .scrollable(true)
            .size_full()
            .px(px(36.))
            .py(px(20.))
            .on_link_click(move |url, _, window, cx| {
                let url = url.to_string();
                // 只认完整的网址：`http.md`、`https-setup.md` 这类是仓库里的文件
                let lower = url.trim().to_ascii_lowercase();
                if lower.starts_with("http://") || lower.starts_with("https://") {
                    return crate::shell::report(crate::shell::open_link(&url), window, cx);
                }
                let Some(path) = resolve_link(&link_dir, &url) else {
                    return;
                };
                view.update(cx, |this, cx| this.open_md_link(path, window, cx))
                    .ok();
            })
            .image_source(images)
            .into_any_element()
    }

    /// 预览里的图片从哪读：都交给读图片的任务（[`Self::start_md_images`]），读回来之前空着。
    /// 网址上的、`data:` 的按地址记，仓库里的按相对仓库根的路径记；出了仓库、进了 `.git` 的不读。
    fn md_image_source(
        &self,
        dir: String,
    ) -> impl Fn(&SharedUri) -> ImageSource + Send + Sync + 'static {
        let got = self.files.md_images.got.clone();
        let want = self.files.md_images.want.clone();
        move |uri: &SharedUri| {
            let path = match web_image(uri.as_ref()) {
                true => uri.to_string(),
                false => match resolve_link(&dir, uri.as_ref()) {
                    Some(path) => path,
                    None => return missing(),
                },
            };
            let mut got = got.lock().unwrap_or_else(|err| err.into_inner());
            match got.get(&path) {
                Some(Fetch::Done(Some(image))) => ImageSource::Image(image.clone()),
                Some(Fetch::Done(None)) => missing(),
                Some(Fetch::Reading) => ImageSource::Custom(Arc::new(|_, _| None)),
                None => match &want {
                    Some(want) if want.unbounded_send(path.clone()).is_ok() => {
                        got.insert(path, Fetch::Reading);
                        ImageSource::Custom(Arc::new(|_, _| None))
                    }
                    _ => missing(),
                },
            }
        }
    }

    /// 点了预览里指向仓库里的链接：目录在树里显示，Markdown 打开成预览，别的文件在编辑器里打开。
    fn open_md_link(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        log::info!("点了 Markdown 预览里的链接 {path}");
        let Some((channel, repo)) = self.files_channel() else {
            return;
        };
        let stat = channel.fs_stat(&repo, &path);
        cx.spawn_in(window, async move |this, cx| {
            let stat = stat.await;
            this.update_in(cx, |this, window, cx| match stat {
                Ok(stat) if stat.dir => this.show_path(path, None, true, window, cx),
                Ok(stat) if stat.exists && crate::open_with::is_markdown(&path) => {
                    this.preview_markdown(path, window, cx)
                }
                Ok(stat) if stat.exists => this.show_file(path, window, cx),
                _ => {
                    let text = fr_core::tf!("找不到 {path}", path);
                    crate::shell::report(Err(text), window, cx);
                }
            })
            .ok();
        })
        .detach();
    }
}

impl MdImages {
    fn set(&self, path: String, fetch: Fetch) {
        let mut got = self.got.lock().unwrap_or_else(|err| err.into_inner());
        got.insert(path, fetch);
    }

    fn forget(&self, path: &str) {
        let mut got = self.got.lock().unwrap_or_else(|err| err.into_inner());
        got.remove(path);
    }

    /// 文件变了（重新载入）：图片也可能换了，下次画的时候重新读。
    pub(super) fn clear(&self) {
        let mut got = self.got.lock().unwrap_or_else(|err| err.into_inner());
        got.retain(|_, fetch| matches!(fetch, Fetch::Reading));
    }
}

#[cfg(test)]
mod tests {
    use super::{ImageFormat, data_image, decode_base64, resolve_link, sniff_image, web_image};

    #[test]
    fn 网址上的图片和_data_图片() {
        assert!(web_image("https://img.shields.io/badge/a-b-c"));
        assert!(web_image(" HTTP://x/a.png"));
        assert!(web_image("data:image/png;base64,AAAA"));
        assert!(!web_image("http.png"));
        assert!(!web_image("img/https.png"));
        assert_eq!(decode_base64("aGk=").unwrap(), b"hi");
        assert_eq!(decode_base64("aG\nk").unwrap(), b"hi");
        assert_eq!(decode_base64("-_-_").unwrap(), [0xfb, 0xff, 0xbf]);
        assert!(decode_base64("a*b").is_none());
        // 1×1 的 PNG 开头
        let (format, bytes) = data_image("image/png;base64,iVBORw0KGgo=").unwrap();
        assert!(matches!(format, ImageFormat::Png));
        assert_eq!(&bytes[..4], b"\x89PNG");
        let (format, bytes) = data_image("image/svg+xml,%3Csvg%3E%3C/svg%3E").unwrap();
        assert!(matches!(format, ImageFormat::Svg));
        assert_eq!(bytes, b"<svg></svg>");
        // 类型没写对的看内容
        let (format, _) = data_image(";base64,R0lGODlhAQABAA==").unwrap();
        assert!(matches!(format, ImageFormat::Gif));
        assert!(data_image("text/plain,hello").is_none());
        assert!(matches!(
            sniff_image(b"<?xml version=\"1.0\"?>\n<svg xmlns=\"\"/>"),
            Some(ImageFormat::Svg)
        ));
        assert!(sniff_image(b"<html>").is_none());
    }

    #[test]
    fn 链接指向仓库里的哪() {
        let at = |dir: &str, link: &str| resolve_link(dir, link);
        assert_eq!(at("docs", "img/a.png").as_deref(), Some("docs/img/a.png"));
        assert_eq!(at("docs/x", "../a.md#part").as_deref(), Some("docs/a.md"));
        assert_eq!(
            at("docs", "./a%20b.png?raw=1").as_deref(),
            Some("docs/a b.png")
        );
        assert_eq!(at("docs", "/README.md").as_deref(), Some("README.md"));
        assert_eq!(at("", "设计.md").as_deref(), Some("设计.md"));
        for none in [
            "https://example.com/a.png",
            "HTTP://x",
            "#top",
            "mailto:a@b",
            "data:image/png;base64,xx",
            "../../out.md",
            "",
            ".git/config",
            "/sub/.GIT/HEAD",
            "../.git/config",
        ] {
            assert_eq!(at("docs", none), None, "{none}");
        }
    }
}
