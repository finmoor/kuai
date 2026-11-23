//! 文件页的 Markdown 预览：界面库排好的样子（标题、列表、表格、代码块、图片），工具栏的「预览」、
//! Ctrl+Shift+V 在它和源文件之间切换。显示的是编辑器里现在的文字（没保存的也算）。
//!
//! - 链接：网址交给浏览器；指向仓库里的文件、目录的（相对这个文件所在的目录，`/` 开头的从仓库根算）
//!   在文件页打开，Markdown 打开成预览。
//! - 图片：本机仓库按路径读；远程主机上的经仓库通道读回来记着（[`MdImages`]），读回来之前空着。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui_kit::component::text::TextView;
use gpui_kit::*;

use super::RepoView;
use super::files::{Class, IMAGE_LIMIT, OpenFile, TextDoc, classify, split};

/// 远程主机上的一张图片读到哪了。
#[derive(Clone)]
enum Fetch {
    Reading,
    /// 读回来了；读不了、不是图片的是 None
    Done(Option<Arc<Image>>),
}

/// Markdown 预览里远程主机上的图片：按相对仓库根的路径记着读回来的。预览画到没读过的图片时，把
/// 路径发给读图片的任务（第一次预览时起，见 [`RepoView::start_md_images`]）。
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
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// 读不出来的图片：界面库按读不了处理（显示替代文字）。
fn missing() -> ImageSource {
    ImageSource::Resource(Resource::Path(Arc::from(Path::new(""))))
}

impl RepoView {
    /// 远程主机上的仓库：起读 Markdown 图片的任务（已经起过不再起）。
    pub(super) fn start_md_images(&mut self, cx: &mut Context<Self>) {
        if self.entry.server.is_none() || self.files.md_images.want.is_some() {
            return;
        }
        let (want, mut wanted) = unbounded::<String>();
        let task = cx.spawn(async move |this, cx| {
            while let Some(path) = wanted.next().await {
                let Ok(Some((channel, repo))) = this.update(cx, |this, _| this.files_channel())
                else {
                    // 还没连上：下次画到它时再读
                    this.update(cx, |this, _| this.files.md_images.forget(&path))
                        .ok();
                    continue;
                };
                let image = match (
                    channel.fs_open(&repo, &path, IMAGE_LIMIT).await,
                    classify(&path),
                ) {
                    (Ok((stat, bytes)), Class::Image(format))
                        if stat.exists && !bytes.is_empty() =>
                    {
                        log::info!("Markdown 预览里的图片 {path}：读回 {} 字节", bytes.len());
                        Some(Arc::new(Image::from_bytes(format, bytes)))
                    }
                    _ => {
                        log::info!("Markdown 预览里的图片 {path} 读不了");
                        None
                    }
                };
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
                if url.to_ascii_lowercase().starts_with("http") {
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

    /// 预览里的图片从哪读：网址照原样（界面库自己读）；仓库里的本机按路径、远程主机上的经仓库通道。
    fn md_image_source(
        &self,
        dir: String,
    ) -> impl Fn(&SharedUri) -> ImageSource + Send + Sync + 'static {
        let root = (self.entry.server.is_none()).then(|| self.entry.path.clone());
        let got = self.files.md_images.got.clone();
        let want = self.files.md_images.want.clone();
        move |uri: &SharedUri| {
            let Some(path) = resolve_link(&dir, uri.as_ref()) else {
                return ImageSource::Resource(Resource::Uri(uri.clone()));
            };
            if let Some(root) = &root {
                let full = crate::open_with::full_path(root, false, &path);
                return ImageSource::Resource(Resource::Path(Arc::from(Path::new(&full))));
            }
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
    use super::resolve_link;

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
        ] {
            assert_eq!(at("docs", none), None, "{none}");
        }
    }
}
