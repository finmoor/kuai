//! 图片文件的改动：不写「二进制文件」，把旧的和新的两张图摆出来对比。
//!
//! 三种看法：并排（左旧右新）、滑动（一条竖线，左边是旧的、右边是新的，拖下面的滑块移动它）、
//! 叠加（两张叠在一起，滑块调旧的那张有多透）。选的哪一种记在设置里。只有一边有图
//! （新增、删除）时只显示那一张。上面一行写两边的尺寸和大小。
//!
//! 图从哪来：某个提交里的、暂存区里的用 `git cat-file --filters <版本>:<路径>` 取
//! （带 `--filters` 是为了 LFS 管着的图能取到真的内容），工作区里的直接读文件。
//! 本地修改、提交详情、比较三处各有一个这样的视图，谁要显示图片谁建。SVG 是文本，照常看文字改动。

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use fr_channel::Channel;
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState, SliderValue};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::files::{Class, IMAGE_LIMIT, classify, human_size};
use crate::app_state::AppState;
use crate::ui;

/// 一边的图从哪取。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Blob {
    /// 某个提交（或者写得出来的别的版本，比如 `HEAD`、`<哈希>^`）里的
    Rev(String),
    /// 暂存区里的
    Index,
    /// 工作区里的
    Worktree,
}

/// 一边的图：从哪取、路径是什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Source {
    pub blob: Blob,
    pub path: String,
}

impl Source {
    pub(super) fn new(blob: Blob, path: impl Into<String>) -> Self {
        Self {
            blob,
            path: path.into(),
        }
    }
}

/// 这个路径是不是按图片对比的文件。SVG 不算：它是文本，看文字改动更有用。
pub(super) fn is_image(path: &str) -> bool {
    image_format(path).is_some()
}

fn image_format(path: &str) -> Option<ImageFormat> {
    match classify(path) {
        Class::Image(ImageFormat::Svg) => None,
        Class::Image(format) => Some(format),
        _ => None,
    }
}

/// 取一边的图用的 git 参数；工作区里的不用 git。
fn blob_args(source: &Source) -> Option<Vec<String>> {
    let object = match &source.blob {
        Blob::Rev(rev) => format!("{rev}:{}", source.path),
        Blob::Index => format!(":{}", source.path),
        Blob::Worktree => return None,
    };
    Some(vec!["cat-file".into(), "--filters".into(), object])
}

struct Pane {
    image: Arc<Image>,
    /// 宽、高（读得出来的话）
    size: Option<(u32, u32)>,
    bytes: u64,
}

enum Side {
    /// 这一边没有这个文件（新增的没有旧的，删除的没有新的）
    Absent,
    Loading,
    Ready(Pane),
    Failed(SharedString),
}

impl Side {
    fn pane(&self) -> Option<&Pane> {
        match self {
            Side::Ready(pane) => Some(pane),
            _ => None,
        }
    }
}

/// 两张图怎么摆。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    SideBySide,
    Swipe,
    Onion,
}

impl Layout {
    fn from_setting(value: u8) -> Self {
        match value {
            1 => Layout::Swipe,
            2 => Layout::Onion,
            _ => Layout::SideBySide,
        }
    }

    fn setting(self) -> u8 {
        match self {
            Layout::SideBySide => 0,
            Layout::Swipe => 1,
            Layout::Onion => 2,
        }
    }
}

pub(super) struct ImageDiff {
    app: Entity<AppState>,
    channel: Channel,
    repo: String,
    sources: (Option<Source>, Option<Source>),
    old: Side,
    new: Side,
    slider: Entity<SliderState>,
    /// 滑块的位置，0 到 1：滑动时是那条线在哪，叠加时是新的那张占几成
    ratio: f32,
    /// 能用来放图的地方有多大，画的时候量出来，下一帧用
    room: Rc<Cell<Size<Pixels>>>,
    task: Option<Task<()>>,
    _slider_events: Subscription,
}

impl ImageDiff {
    /// `old`、`new` 是两边的图从哪取；哪一边没有这个文件就给 None。
    pub(super) fn new(
        app: Entity<AppState>,
        channel: Channel,
        repo: String,
        old: Option<Source>,
        new: Option<Source>,
        cx: &mut Context<Self>,
    ) -> Self {
        let slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(100.)
                .step(1.)
                .default_value(50.)
        });
        let slider_events = cx.subscribe(&slider, |this, _, event: &SliderEvent, cx| {
            let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
            if let SliderValue::Single(value) = value {
                this.ratio = (value / 100.).clamp(0., 1.);
                cx.notify();
            }
        });
        let mut view = Self {
            app,
            channel,
            repo,
            sources: (old, new),
            old: Side::Absent,
            new: Side::Absent,
            slider,
            ratio: 0.5,
            room: Rc::new(Cell::new(size(px(640.), px(420.)))),
            task: None,
            _slider_events: slider_events,
        };
        if view.sources.0.is_some() {
            view.old = Side::Loading;
        }
        if view.sources.1.is_some() {
            view.new = Side::Loading;
        }
        view.reload(cx);
        view
    }

    /// 是不是在比这两边：是的话仓库变了只用 [`Self::reload`]，不用重建。
    pub(super) fn shows(&self, old: &Option<Source>, new: &Option<Source>) -> bool {
        self.sources.0 == *old && self.sources.1 == *new
    }

    /// 重新取两边的图。取回来之前旧的先留着，不闪。
    pub(super) fn reload(&mut self, cx: &mut Context<Self>) {
        let (channel, repo) = (self.channel.clone(), self.repo.clone());
        let (old, new) = self.sources.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let (old, new) = futures::join!(
                load_side(&channel, &repo, old.as_ref()),
                load_side(&channel, &repo, new.as_ref())
            );
            this.update(cx, |this, cx| {
                this.old = old;
                this.new = new;
                cx.notify();
            })
            .ok();
        }));
    }

    fn layout(&self, cx: &App) -> Layout {
        Layout::from_setting(self.app.read(cx).settings.image_diff)
    }

    fn set_layout(&mut self, layout: Layout, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            app.update_settings(cx, |settings| settings.image_diff = layout.setting())
        });
        cx.notify();
    }

    /// 上面一行：两边的尺寸和大小，右边是三种看法的切换（两边都有图时才有）。
    fn render_bar(&self, both: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let describe = |side: &Side| -> Option<String> {
            let pane = side.pane()?;
            Some(match pane.size {
                Some((width, height)) => {
                    format!("{width} × {height} · {}", human_size(pane.bytes))
                }
                None => human_size(pane.bytes),
            })
        };
        let info: SharedString = match (describe(&self.old), describe(&self.new)) {
            (Some(old), Some(new)) if old == new => tf!("两边都是 {size}", size = old).into(),
            (Some(old), Some(new)) => format!("{old}  →  {new}").into(),
            (Some(old), None) => tf!("旧：{size}", size = old).into(),
            (None, Some(new)) => tf!("新：{size}", size = new).into(),
            (None, None) => SharedString::default(),
        };
        let layout = self.layout(cx);
        let pick = |id: &'static str, to: Layout, label: &'static str, tip: &'static str| {
            Button::new(id)
                .xsmall()
                .ghost()
                .label(label)
                .selected(layout == to)
                .tooltip(tip)
                .on_click(cx.listener(move |this, _, _, cx| this.set_layout(to, cx)))
        };
        let modes = both.then(|| {
            h_flex()
                .flex_none()
                .gap(px(2.))
                .child(pick(
                    "image-side",
                    Layout::SideBySide,
                    tr("并排"),
                    tr("并排：左侧为旧图，右侧为新图"),
                ))
                .child(pick(
                    "image-swipe",
                    Layout::Swipe,
                    tr("滑动"),
                    tr("滑动：分界线左侧为旧图，右侧为新图"),
                ))
                .child(pick(
                    "image-onion",
                    Layout::Onion,
                    tr("叠加"),
                    tr("叠加：两张图重叠，拖动滑块调整透明度"),
                ))
        });
        h_flex()
            .h(px(30.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(border)
            .text_xs()
            .text_color(muted)
            .child(ui::icon_small(IconName::Image).flex_none())
            .child(ui::clipped("image-info", info).flex_1())
            .children(modes)
            .into_any_element()
    }

    /// 一张图，垫着棋盘格的底，下面写是旧的还是新的。
    fn framed(&self, pane: &Pane, label: &'static str, cx: &App) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .justify_center()
            .gap(px(8.))
            .child(
                div()
                    .flex_shrink(1.)
                    .min_h_0()
                    .max_w_full()
                    .p(px(12.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.foreground.opacity(0.06))
                    .overflow_hidden()
                    .child(
                        img(pane.image.clone())
                            .max_w_full()
                            .max_h_full()
                            .object_fit(ObjectFit::ScaleDown),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(label),
            )
            .into_any_element()
    }

    /// 滑动、叠加：两张图叠在同一个框里，下面一个滑块。
    fn stacked(&self, old: &Pane, new: &Pane, layout: Layout, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, accent) = (theme.border, theme.muted_foreground, theme.primary);
        let backdrop = theme.foreground.opacity(0.06);
        // 框按两张里大的那张定，放不下就等比缩小；留出边距和下面滑块的地方
        let natural = |pane: &Pane| pane.size.unwrap_or((640, 420));
        let (old_size, new_size) = (natural(old), natural(new));
        let (width, height) = (
            old_size.0.max(new_size.0).max(1) as f32,
            old_size.1.max(new_size.1).max(1) as f32,
        );
        let room = self.room.get();
        let (room_w, room_h) = (
            (f32::from(room.width) - 48.).max(80.),
            (f32::from(room.height) - 96.).max(60.),
        );
        let scale = (room_w / width).min(room_h / height).min(1.);
        let (box_w, box_h) = ((width * scale).round(), (height * scale).round());
        // 两张各按自己的大小画（用同一个缩放比例），在框里居中：大小不一样时看得出来
        let picture = |pane: &Pane, size: (u32, u32)| {
            let (w, h) = (
                (size.0 as f32 * scale).round().clamp(1., box_w),
                (size.1 as f32 * scale).round().clamp(1., box_h),
            );
            div()
                .absolute()
                .left(px(((box_w - w) / 2.).floor()))
                .top(px(((box_h - h) / 2.).floor()))
                .child(img(pane.image.clone()).w(px(w)).h(px(h)))
        };
        let ratio = self.ratio;
        let frame = div()
            .relative()
            .flex_none()
            .w(px(box_w))
            .h(px(box_h))
            .rounded(px(4.))
            .border_1()
            .border_color(border)
            .bg(backdrop)
            .overflow_hidden()
            // 新的垫在下面
            .child(picture(new, new_size));
        let frame = match layout {
            // 旧的只露出线左边那一段
            Layout::Swipe => frame
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .h(px(box_h))
                        .w(px((box_w * ratio).round()))
                        .overflow_hidden()
                        .bg(theme.background)
                        .child(
                            div()
                                .absolute()
                                .left_0()
                                .top_0()
                                .w(px(box_w))
                                .h(px(box_h))
                                .bg(backdrop)
                                .child(picture(old, old_size)),
                        ),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left(px((box_w * ratio).round() - 1.))
                        .w(px(2.))
                        .h(px(box_h))
                        .bg(accent),
                ),
            // 旧的整张盖在上面，越往「新」拖越透
            _ => frame.child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .w(px(box_w))
                    .h(px(box_h))
                    .opacity(1. - ratio)
                    .child(picture(old, old_size)),
            ),
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(px(14.))
            .child(frame)
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(10.))
                    .text_xs()
                    .text_color(muted)
                    .child(tr("旧"))
                    .child(
                        div()
                            .flex_none()
                            .w(px(box_w.clamp(160., 320.)))
                            .child(Slider::new(&self.slider)),
                    )
                    .child(tr("新")),
            )
            .into_any_element()
    }
}

impl Render for ImageDiff {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let message = |text: SharedString, color: Hsla| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_sm()
                .text_color(color)
                .whitespace_normal()
                .child(text)
                .into_any_element()
        };
        let layout = self.layout(cx);
        let both = self.old.pane().is_some() && self.new.pane().is_some();
        let body: AnyElement = match (&self.old, &self.new) {
            (Side::Failed(err), _) | (_, Side::Failed(err)) => message(err.clone(), danger),
            (Side::Loading, _) | (_, Side::Loading) => message(tr("正在读取图片…").into(), muted),
            (Side::Ready(old), Side::Ready(new)) => match layout {
                Layout::SideBySide => h_flex()
                    .size_full()
                    .p(px(16.))
                    .gap(px(16.))
                    .child(self.framed(old, tr("旧"), cx))
                    .child(self.framed(new, tr("新"), cx))
                    .into_any_element(),
                _ => self.stacked(old, new, layout, cx),
            },
            (Side::Ready(old), Side::Absent) => h_flex()
                .size_full()
                .p(px(16.))
                .child(self.framed(old, tr("已删除的图片"), cx))
                .into_any_element(),
            (Side::Absent, Side::Ready(new)) => h_flex()
                .size_full()
                .p(px(16.))
                .child(self.framed(new, tr("新增的图片"), cx))
                .into_any_element(),
            (Side::Absent, Side::Absent) => message(tr("没有可显示的图片").into(), muted),
        };
        let room = self.room.clone();
        let view = cx.entity().downgrade();
        v_flex().size_full().child(self.render_bar(both, cx)).child(
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                // 量一下能放图的地方多大；变了就再画一遍
                .child(
                    canvas(
                        move |bounds, window, _| {
                            if room.replace(bounds.size) != bounds.size {
                                // 正画着的时候说「要重画」不管用，等下一帧再说
                                let view = view.clone();
                                window.on_next_frame(move |_, cx| {
                                    view.update(cx, |_, cx| cx.notify()).ok();
                                });
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .child(body),
        )
    }
}

/// 取一边的图并解出尺寸。
async fn load_side(channel: &Channel, repo: &str, source: Option<&Source>) -> Side {
    let Some(source) = source else {
        return Side::Absent;
    };
    let failed = |err: String| Side::Failed(tf!("读取图片失败：{err}", err).into());
    let too_large = || {
        Side::Failed(
            tf!(
                "图片超过 {size}，不在此处显示",
                size = human_size(IMAGE_LIMIT)
            )
            .into(),
        )
    };
    let bytes = match blob_args(source) {
        Some(args) => match channel.git(repo, args).await {
            Ok(out) if out.success() => out.stdout,
            Ok(out) => return failed(out.stderr),
            Err(err) => return failed(format!("{err:#}")),
        },
        None => match channel.fs_open(repo, &source.path, IMAGE_LIMIT).await {
            Ok((stat, _)) if stat.size > IMAGE_LIMIT => return too_large(),
            Ok((_, bytes)) => bytes,
            Err(err) => return failed(format!("{err:#}")),
        },
    };
    if bytes.len() as u64 > IMAGE_LIMIT {
        return too_large();
    }
    let Some(format) = image_format(&source.path) else {
        return failed(tr("不是图片文件").into());
    };
    let size = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok());
    Side::Ready(Pane {
        bytes: bytes.len() as u64,
        image: Arc::new(Image::from_bytes(format, bytes)),
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::{Blob, Source, blob_args, is_image};

    #[test]
    fn 哪些文件按图片对比() {
        assert!(is_image("assets/Logo.PNG"));
        assert!(is_image("a/b.jpeg"));
        assert!(is_image("x.webp"));
        // SVG 是文本，看文字改动
        assert!(!is_image("icons/x.svg"));
        assert!(!is_image("src/main.rs"));
        assert!(!is_image("png"));
    }

    #[test]
    fn 取图的参数() {
        let args = |blob| blob_args(&Source::new(blob, "a b/图.png")).map(|args| args.join(" "));
        assert_eq!(
            args(Blob::Rev("HEAD".into())).as_deref(),
            Some("cat-file --filters HEAD:a b/图.png")
        );
        assert_eq!(
            args(Blob::Index).as_deref(),
            Some("cat-file --filters :a b/图.png")
        );
        assert_eq!(args(Blob::Worktree), None);
    }
}
