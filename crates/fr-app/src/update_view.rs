//! 升级在界面上的样子（原型和选定记录在 design/update/，选的是 A）：
//! 标题栏齿轮左边的小牌，点开一个小窗看说明、升级；「设置 → 关于」里的版本那一行。
//! 逻辑在 `update.rs`。

use std::sync::atomic::Ordering;
use std::time::Instant;

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::popover::{Popover, PopoverState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::ui;
use crate::update::{self, Problem, Release, Stage, Updater};

/// 标题栏上的小牌：有新版本（没跳过）、正在下、下好了、出了错时才有。
pub fn title_pill(updater: &Entity<Updater>, cx: &App) -> Option<AnyElement> {
    let this = updater.read(cx);
    if !this.needs_attention(cx) {
        return None;
    }
    let theme = cx.theme();
    let tint = |color: Hsla, fg: Hsla| {
        ButtonCustomVariant::new(cx)
            .color(color.opacity(0.14))
            .foreground(fg)
            .hover(color.opacity(0.22))
            .active(color.opacity(0.3))
    };
    let accent = tint(theme.primary, accent_text(cx));
    let button = Button::new("update-pill").small();
    let button = match &this.stage {
        Stage::Available(release) => button
            .custom(accent)
            .icon(ui::icon(IconName::CircleArrowUp))
            .label(tf!("{version} 可以升级", version = release.version)),
        Stage::Downloading { release, got, .. } => button.custom(accent).label(tf!(
            "下载 {percent}%",
            percent = percent(got.load(Ordering::Relaxed), release.asset.size)
        )),
        Stage::Ready(_) => button
            .primary()
            .icon(ui::icon(IconName::RotateCw))
            .label(tr("重启完成升级")),
        Stage::Failed { .. } => button
            .custom(tint(theme.danger, theme.danger))
            .icon(ui::icon(IconName::TriangleAlert))
            .label(tr("升级没成功")),
        Stage::Idle => return None,
    };
    let updater = updater.clone();
    Some(
        Popover::new("update-popover")
            .anchor(Anchor::TopRight)
            .trigger(button)
            .content(move |_, window, cx| {
                let popover = cx.entity();
                popover_body(&updater, &popover, window, cx)
            })
            .into_any_element(),
    )
}

/// 小牌点开的小窗。
fn popover_body(
    updater: &Entity<Updater>,
    popover: &Entity<PopoverState>,
    _: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let this = updater.read(cx);
    let theme = cx.theme();
    let (muted, primary, danger, success) = (
        theme.muted_foreground,
        theme.primary,
        theme.danger,
        theme.success,
    );
    let body = v_flex().w(px(340.)).gap_3().p_1();
    let body = match &this.stage {
        Stage::Available(release) => body
            .child(header(
                IconName::CircleArrowUp,
                primary,
                format!("Kuai {}", release.version),
                Some(release_meta(release)),
                cx,
            ))
            .child(notes(release, cx))
            .child(
                v_flex()
                    .gap_2()
                    .child(h_flex().child(page_link(release)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(tr("升级要重启 Kuai，服务器上的终端不受影响。")),
                    ),
            )
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Button::new("update-skip")
                            .ghost()
                            .small()
                            .label(tr("跳过这一版"))
                            .on_click(closing(popover, act(updater, |u, cx| u.skip(cx)))),
                    )
                    .child(download_button(updater)),
            ),
        Stage::Downloading { release, got, .. } => {
            let got = got.load(Ordering::Relaxed);
            body.child(header(
                IconName::Download,
                primary,
                tf!("正在下载 {version}", version = release.version),
                Some("github.com".into()),
                cx,
            ))
            .child(
                v_flex()
                    .gap_1()
                    .child(bar(fraction(got, release.asset.size), px(332.), cx))
                    .child(
                        h_flex()
                            .justify_between()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("{} / {} MB", mb(got), mb(release.asset.size)))
                            .child(format!("{}%", percent(got, release.asset.size))),
                    ),
            )
            .child(h_flex().justify_end().child(cancel_button(updater)))
        }
        Stage::Ready(release) => body
            .child(header(
                IconName::ShieldCheck,
                success,
                tf!("{version} 下好了", version = release.version),
                Some(tr("和 GitHub 上记的校验值对过了").into()),
                cx,
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(tr("不重启也行，下次打开 Kuai 就是新版。")),
            )
            .child(
                h_flex()
                    .justify_end()
                    .child(restart_button(updater, Some(popover))),
            ),
        Stage::Failed { release, problem } => {
            let (title, detail) = problem_text(problem);
            let retry = matches!(
                problem,
                Problem::Download(_) | Problem::Digest | Problem::Install(_)
            ) && release.is_some();
            body.child(header(
                IconName::TriangleAlert,
                danger,
                title,
                Some(detail),
                cx,
            ))
            .children(problem_hint(problem, muted))
            .child(
                h_flex()
                    .justify_between()
                    .child(downloads_link(release.as_ref()))
                    .children(retry.then(|| {
                        Button::new("update-retry")
                            .primary()
                            .small()
                            .label(tr("重试"))
                            .loading(this.checking)
                            .on_click(act(updater, |u, cx| u.retry(cx)))
                    })),
            )
        }
        Stage::Idle => body,
    };
    body.into_any_element()
}

/// 「设置 → 关于」里的版本那一行：版本号、检查和升级的状态、按钮；有新版本时下面列出要点。
pub fn about_version(updater: &Entity<Updater>, cx: &App) -> AnyElement {
    let this = updater.read(cx);
    let theme = cx.theme();
    let (muted, danger) = (theme.muted_foreground, theme.danger);
    let status = |text: SharedString| div().text_sm().text_color(muted).child(text);
    let check_button = || {
        Button::new("update-check")
            .small()
            .label(tr("检查更新"))
            .on_click(act(updater, |u, cx| u.check(true, cx)))
    };
    let right = h_flex().flex_none().gap_2().items_center();
    let right = match &this.stage {
        _ if this.checking => right.child(status(tr("正在问 GitHub…").into())),
        Stage::Idle => {
            let text = match (this.checked_at, &this.updated_from) {
                (Some(at), _) => tf!("已是最新 · {when}检查过", when = ago(at)),
                (None, Some(from)) => tf!("刚从 {from} 升上来", from = from.as_str()),
                (None, None) => String::new(),
            };
            right.child(status(text.into())).child(check_button())
        }
        Stage::Available(release) => {
            let text = if this.skipped(release, cx) {
                tf!("有新版本 {version}（跳过了）", version = release.version)
            } else {
                tf!("有新版本 {version}", version = release.version)
            };
            right
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(accent_text(cx))
                        .child(text),
                )
                .child(download_button(updater))
        }
        Stage::Downloading { release, got, .. } => {
            let got = got.load(Ordering::Relaxed);
            right
                .child(bar(fraction(got, release.asset.size), px(200.), cx))
                .child(status(
                    format!("{}%", percent(got, release.asset.size)).into(),
                ))
                .child(cancel_button(updater))
        }
        Stage::Ready(release) => right
            .child(status(
                tf!("{version} 下好了", version = release.version).into(),
            ))
            .child(restart_button(updater, None)),
        Stage::Failed { release, problem } => {
            let (title, detail) = problem_text(problem);
            let text = div()
                .max_w(px(360.))
                .text_sm()
                .text_color(danger)
                .child(ui::clipped("update-problem", format!("{title}：{detail}")));
            match problem {
                Problem::Check(_) => right.child(text).child(check_button()),
                Problem::NotWritable(_) | Problem::DevBuild => {
                    right.child(text).child(downloads_link(release.as_ref()))
                }
                _ => right.child(text).child(
                    Button::new("update-retry")
                        .small()
                        .label(tr("重试"))
                        .on_click(act(updater, |u, cx| u.retry(cx))),
                ),
            }
        }
    };
    let release = match &this.stage {
        Stage::Available(r) | Stage::Ready(r) | Stage::Downloading { release: r, .. } => Some(r),
        _ => None,
    };
    let notes_box = release.map(|release| {
        v_flex()
            .gap_1()
            .p_3()
            .rounded(px(8.))
            .border_1()
            .border_color(theme.primary.opacity(0.25))
            .bg(theme.primary.opacity(0.05))
            .child(
                h_flex()
                    .gap_3()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("{} · {}", release.version, release_meta(release)))
                    .child(page_link(release)),
            )
            .child(notes(release, cx))
    });
    v_flex()
        .w_full()
        .gap_3()
        .child(
            h_flex()
                .w_full()
                .items_start()
                .justify_between()
                .gap_4()
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_base()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(tf!("版本 {version}", version = env!("CARGO_PKG_VERSION"))),
                        )
                        .child(div().text_xs().text_color(muted).child(tr(
                            "能打开服务器上仓库的 Git 客户端：本机和服务器上的仓库用同一套界面。",
                        ))),
                )
                .child(right),
        )
        .children(notes_box)
        .into_any_element()
}

// ---------- 小零件 ----------

/// 按钮点了以后对 Updater 做的事。
/// 点了先收起小窗再做事：重启要弹确认框，跳过以后小牌就没了。
fn closing(
    popover: &Entity<PopoverState>,
    f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let popover = popover.downgrade();
    move |event, window, cx| {
        popover.update(cx, |p, cx| p.dismiss(window, cx)).ok();
        f(event, window, cx)
    }
}

fn act(
    updater: &Entity<Updater>,
    f: impl Fn(&mut Updater, &mut Context<Updater>) + 'static,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let updater = updater.clone();
    move |_, _, cx| updater.update(cx, |u, cx| f(u, cx))
}

fn download_button(updater: &Entity<Updater>) -> Button {
    Button::new("update-download")
        .primary()
        .small()
        .icon(ui::icon(IconName::Download))
        .label(tr("升级"))
        .on_click(act(updater, |u, cx| u.download(cx)))
}

fn cancel_button(updater: &Entity<Updater>) -> Button {
    Button::new("update-cancel")
        .small()
        .label(tr("取消"))
        .on_click(act(updater, |u, cx| u.cancel(cx)))
}

/// `popover`：在小窗里的，点了先收起小窗，不然它压在接着弹的确认框上面。
fn restart_button(updater: &Entity<Updater>, popover: Option<&Entity<PopoverState>>) -> Button {
    let restart = act(updater, |u, cx| u.restart(cx));
    let button = Button::new("update-restart")
        .primary()
        .small()
        .icon(ui::icon(IconName::RotateCw))
        .label(tr("重启完成升级"));
    match popover {
        Some(popover) => button.on_click(closing(popover, restart)),
        None => button.on_click(restart),
    }
}

/// 「完整的更新说明」：打开发布页。
fn page_link(release: &Release) -> Button {
    let page = release.page();
    Button::new("update-page")
        .link()
        .small()
        .label(tr("完整的更新说明"))
        .on_click(move |_, window, cx| {
            crate::shell::report(crate::shell::open_url(&page), window, cx)
        })
}

/// 「去下载页」：知道是哪一版就开那一版的发布页，不知道就开发布列表。
fn downloads_link(release: Option<&Release>) -> Button {
    let page = release.map_or_else(|| format!("{}/releases", update::repo_url()), Release::page);
    Button::new("update-downloads")
        .link()
        .small()
        .label(tr("去下载页"))
        .on_click(move |_, window, cx| {
            crate::shell::report(crate::shell::open_url(&page), window, cx)
        })
}

/// 左边一块带底色的图标，右边标题和一行小字。
fn header(
    icon: IconName,
    color: Hsla,
    title: impl Into<SharedString>,
    meta: Option<SharedString>,
    cx: &App,
) -> Div {
    let muted = cx.theme().muted_foreground;
    h_flex()
        .gap_2p5()
        .items_start()
        .child(
            div()
                .flex_none()
                .size(px(30.))
                .rounded(px(8.))
                .bg(color.opacity(0.14))
                .flex()
                .items_center()
                .justify_center()
                .child(ui::icon(icon).text_color(color)),
        )
        .child(
            v_flex()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .text_base()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.into()),
                )
                .children(meta.map(|meta| {
                    div()
                        .text_xs()
                        .text_color(muted)
                        .whitespace_normal()
                        .child(meta)
                })),
        )
}

/// 发布说明里的要点，一条一行。
fn notes(release: &Release, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    v_flex().gap_1().children(release.notes.iter().map(|note| {
        h_flex()
            .gap_2()
            .items_start()
            .text_sm()
            .child(div().flex_none().text_color(muted).child("•"))
            .child(div().min_w_0().whitespace_normal().child(note.clone()))
    }))
}

/// 「11 月 20 日发布 · 18.4 MB」那一行。
fn release_meta(release: &Release) -> SharedString {
    let size = format!("{} MB", mb(release.asset.size));
    match &release.date {
        Some(date) => tf!("{date} 发布 · {size}", date = date.as_str(), size).into(),
        None => size.into(),
    }
}

fn problem_text(problem: &Problem) -> (SharedString, SharedString) {
    let (title, detail): (&str, String) = match problem {
        Problem::Check(err) => (tr("没问成"), err.clone()),
        Problem::Download(err) => (tr("没下完"), err.clone()),
        Problem::Digest => (
            tr("没装"),
            tr("下载的文件和 GitHub 上记的校验值对不上").into(),
        ),
        Problem::NotWritable(_) => (tr("装不上"), tr("Kuai 所在的文件夹不能写").into()),
        Problem::DevBuild => (
            tr("开发版不自己升级"),
            tr("打好的包（kuai.exe）才会换程序").into(),
        ),
        Problem::Install(err) => (tr("没换成"), err.clone()),
    };
    (title.into(), detail.into())
}

/// 文件夹不能写时多说一句怎么办。
fn problem_hint(problem: &Problem, muted: Hsla) -> Option<Div> {
    let Problem::NotWritable(dir) = problem else {
        return None;
    };
    Some(
        v_flex()
            .gap_1()
            .text_xs()
            .text_color(muted)
            .child(
                div()
                    .font_family("Consolas")
                    .child(dir.to_string_lossy().to_string()),
            )
            .child(
                div()
                    .whitespace_normal()
                    .child(tr("到下载页手动下载，或者把 Kuai 挪到自己的文件夹再升级。")),
            ),
    )
}

/// 进度条。
fn bar(fraction: f32, width: Pixels, cx: &App) -> Div {
    let primary = cx.theme().primary;
    div()
        .flex_none()
        .w(width)
        .h(px(5.))
        .rounded(px(3.))
        .bg(primary.opacity(0.2))
        .child(
            div()
                .h_full()
                .rounded(px(3.))
                .w(width * fraction)
                .bg(primary),
        )
}

/// 写字用的主题色：比按钮的主题色深（浅色主题）或亮（深色主题）一档，在淡底上才看得清。
fn accent_text(cx: &App) -> Hsla {
    let theme = cx.theme();
    let primary = theme.primary;
    let l = if theme.is_dark() {
        (primary.l + 0.12).min(0.85)
    } else {
        (primary.l - 0.08).max(0.25)
    };
    Hsla { l, ..primary }
}

fn fraction(got: u64, total: u64) -> f32 {
    if total == 0 {
        0.
    } else {
        (got as f32 / total as f32).clamp(0., 1.)
    }
}

fn percent(got: u64, total: u64) -> u64 {
    (fraction(got, total) * 100.).round() as u64
}

fn mb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1024. / 1024.)
}

/// 「刚才」「5 分钟前」「3 小时前」。
fn ago(at: Instant) -> String {
    let minutes = at.elapsed().as_secs() / 60;
    match minutes {
        0 => tr("刚才").into(),
        1..60 => tf!("{n} 分钟前", n = minutes),
        _ => tf!("{n} 小时前", n = minutes / 60),
    }
}
