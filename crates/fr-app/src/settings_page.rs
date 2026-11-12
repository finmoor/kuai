//! 设置页：常规（语言、开机自启、托盘）、外观（深色浅色、配色）、关于（版本、检查更新、源代码）。
//!
//! 每一项改了马上生效、马上存盘，没有「确定」「应用」。这里只改 [`AppState`] 里的设置；
//! 换主题、换语言、建托盘图标这些要动窗口的事，由主窗口（`workspace.rs`）看到设置变了再去做。
//! 开机自启不存在设置文件里，直接读写注册表（见 `autostart.rs`）。

use fr_core::i18n::{Language, tr};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::group_box::GroupBoxVariant;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::setting::{
    SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::palette::PALETTES;
use crate::settings::{AUTO_FETCH_CHOICES, ThemeChoice};
use crate::update::{self, Updater};

pub struct SettingsPage {
    app: Entity<AppState>,
    updater: Entity<Updater>,
    _observe: [Subscription; 2],
}

impl SettingsPage {
    pub fn new(app: Entity<AppState>, updater: Entity<Updater>, cx: &mut Context<Self>) -> Self {
        let observe = [
            cx.observe(&app, |_, _, cx| cx.notify()),
            // 检查、下载的进度变了，关于页跟着画
            cx.observe(&updater, |_, _, cx| cx.notify()),
        ];
        Self {
            app,
            updater,
            _observe: observe,
        }
    }

    fn general(&self) -> SettingPage {
        let app = self.app.clone();
        let language = {
            let (get, set) = (app.clone(), app.clone());
            SettingItem::new(
                tr("界面语言"),
                SettingField::dropdown(
                    Language::ALL
                        .iter()
                        .map(|l| (language_key(*l).into(), l.label().into()))
                        .collect(),
                    move |cx| language_key(get.read(cx).settings.language).into(),
                    move |value: SharedString, cx| {
                        let language = Language::ALL
                            .into_iter()
                            .find(|l| language_key(*l) == value.as_ref())
                            .unwrap_or_default();
                        set.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.language = language)
                        });
                    },
                ),
            )
            .description(tr("马上生效；已弹出的通知不变。"))
        };
        let autostart = SettingItem::new(
            tr("开机时自动启动"),
            SettingField::switch(
                |_| crate::autostart::enabled(),
                |on, cx| {
                    if let Err(err) = crate::autostart::set(on) {
                        report(format!("{err:#}"), cx);
                    }
                    cx.refresh_windows();
                },
            ),
        )
        .description(tr("登录 Windows 后自动启动；开了托盘选项时直接进托盘。"));
        let minimize = {
            let (get, set) = (app.clone(), app.clone());
            SettingItem::new(
                tr("最小化时收到托盘"),
                SettingField::switch(
                    move |cx| get.read(cx).settings.minimize_to_tray,
                    move |on, cx| {
                        set.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.minimize_to_tray = on)
                        })
                    },
                ),
            )
            .description(tr("最小化后任务栏不留按钮，点托盘图标回来。"))
        };
        let close = {
            let (get, set) = (app.clone(), app.clone());
            SettingItem::new(
                tr("关闭窗口时收到托盘，不退出"),
                SettingField::switch(
                    move |cx| get.read(cx).settings.close_to_tray,
                    move |on, cx| {
                        set.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.close_to_tray = on)
                        })
                    },
                ),
            )
            .description(tr("点关闭按钮只是藏起窗口；退出在托盘图标的右键菜单里。"))
        };
        let auto_fetch = {
            let (get, set) = (app.clone(), app.clone());
            SettingItem::new(
                tr("自动获取"),
                SettingField::dropdown(
                    AUTO_FETCH_CHOICES
                        .iter()
                        .map(|minutes| (minutes.to_string().into(), auto_fetch_label(*minutes)))
                        .collect(),
                    move |cx| get.read(cx).settings.auto_fetch_minutes.to_string().into(),
                    move |value: SharedString, cx| {
                        let minutes = value.parse().unwrap_or(0);
                        set.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.auto_fetch_minutes = minutes)
                        });
                    },
                ),
            )
            .description(tr(
                "在后台定时获取正在看的仓库（git fetch --all）。要密码时跳过，不弹窗。",
            ))
        };
        let stash = {
            let (get, set) = (app.clone(), app.clone());
            SettingItem::new(
                tr("有改动时切换分支、拉取：默认先贮藏"),
                SettingField::switch(
                    move |cx| get.read(cx).settings.stash_before_switch,
                    move |on, cx| {
                        set.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.stash_before_switch = on)
                        })
                    },
                ),
            )
            .description(tr("切换分支、检出、拉取前有没提交的改动时，默认怎么处理。"))
        };
        let tab_close = {
            let (get, set) = (app.clone(), app.clone());
            SettingItem::new(
                tr("标签页上显示关闭按钮"),
                SettingField::switch(
                    move |cx| get.read(cx).settings.tab_close_button,
                    move |on, cx| {
                        set.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.tab_close_button = on)
                        })
                    },
                ),
            )
            .description(tr(
                "怕手滑就关掉：改用中键、Ctrl+W 或右键菜单关。关错了按 Ctrl+Shift+T 找回。",
            ))
        };
        let shell = {
            let (get, set) = (app.clone(), app.clone());
            // 存的是空字符串时自动挑；下拉里的项不能是空的，用 auto 代替
            let mut options: Vec<(SharedString, SharedString)> =
                vec![("auto".into(), tr("自动").into())];
            options.extend(
                crate::shell::local_shells()
                    .into_iter()
                    .map(|shell| (shell.id.into(), shell.label.into())),
            );
            SettingItem::new(
                tr("本机仓库的终端"),
                SettingField::dropdown(
                    options,
                    move |cx| match get.read(cx).settings.terminal_shell.as_str() {
                        "" => "auto".into(),
                        id => id.to_string().into(),
                    },
                    move |value: SharedString, cx| {
                        let id = match value.as_ref() {
                            "auto" => String::new(),
                            id => id.to_string(),
                        };
                        set.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.terminal_shell = id)
                        });
                    },
                ),
            )
            .description(tr(
                "新开的终端生效。服务器上的仓库用服务器上的登录 shell。",
            ))
        };
        SettingPage::new(tr("常规"))
            .icon(Icon::new(IconName::Settings2))
            .default_open(true)
            .group(SettingGroup::new().title(tr("语言")).item(language))
            .group(SettingGroup::new().title(tr("标签页")).item(tab_close))
            .group(SettingGroup::new().title(tr("终端")).item(shell))
            .group(
                SettingGroup::new()
                    .title(tr("启动和关闭"))
                    .item(autostart)
                    .item(minimize)
                    .item(close),
            )
            .group(
                SettingGroup::new()
                    .title("Git")
                    .item(auto_fetch)
                    .item(stash),
            )
    }

    fn appearance(&self) -> SettingPage {
        let app = self.app.clone();
        let mode = {
            let (get, set) = (app.clone(), app.clone());
            SettingItem::new(
                tr("深色还是浅色"),
                SettingField::dropdown(
                    ThemeChoice::ALL
                        .iter()
                        .map(|c| (theme_key(*c).into(), c.label().into()))
                        .collect(),
                    move |cx| theme_key(get.read(cx).settings.theme).into(),
                    move |value: SharedString, cx| {
                        let choice = ThemeChoice::ALL
                            .into_iter()
                            .find(|c| theme_key(*c) == value.as_ref())
                            .unwrap_or_default();
                        set.update(cx, |app, cx| app.update_settings(cx, |s| s.theme = choice));
                    },
                ),
            )
            .description(tr("「跟随系统」跟着 Windows 的深色、浅色设置走。"))
        };
        let palette_item = |dark: bool| {
            let (get, set) = (app.clone(), app.clone());
            let options = PALETTES
                .iter()
                .filter(|p| p.dark == dark)
                .map(|p| (p.id.into(), p.label().into()))
                .collect();
            SettingItem::new(
                if dark {
                    tr("深色时用的配色")
                } else {
                    tr("浅色时用的配色")
                },
                SettingField::scrollable_dropdown(
                    options,
                    move |cx| {
                        let settings = &get.read(cx).settings;
                        let id = if dark {
                            &settings.dark_theme
                        } else {
                            &settings.light_theme
                        };
                        crate::palette::find(id, dark).id.into()
                    },
                    move |value: SharedString, cx| {
                        set.update(cx, |app, cx| {
                            app.update_settings(cx, |s| {
                                if dark {
                                    s.dark_theme = value.to_string();
                                } else {
                                    s.light_theme = value.to_string();
                                }
                            })
                        });
                    },
                ),
            )
        };
        SettingPage::new(tr("外观"))
            .icon(Icon::new(IconName::Palette))
            .default_open(true)
            .group(
                SettingGroup::new()
                    .title(tr("主题"))
                    .item(mode)
                    .item(palette_item(false))
                    .item(palette_item(true)),
            )
            .group(
                SettingGroup::new()
                    .title(tr("配色一览"))
                    .item(SettingItem::render({
                        let app = app.clone();
                        move |_, window, cx| {
                            let active =
                                crate::appearance::current_palette(&app.read(cx).settings, window)
                                    .id;
                            swatches(&app, active, cx)
                        }
                    })),
            )
    }

    fn about(&self) -> SettingPage {
        let updater = self.updater.clone();
        let version =
            SettingItem::render(move |_, _, cx| crate::update_view::about_version(&updater, cx));
        let check_updates = {
            let (get, set) = (self.app.clone(), self.app.clone());
            SettingItem::new(
                tr("自动检查更新"),
                SettingField::switch(
                    move |cx| get.read(cx).settings.check_updates,
                    move |on, cx| {
                        set.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.check_updates = on)
                        })
                    },
                ),
            )
            .description(tr("启动时和之后每 6 小时问一次 GitHub，只看有没有新版本。"))
        };
        let source = SettingItem::new(
            tr("源代码"),
            SettingField::render(|_, _, _| {
                Button::new("open-repo")
                    .small()
                    .icon(Icon::new(IconName::ExternalLink))
                    .label(tr("打开"))
                    .on_click(|_, window, cx| {
                        let result = crate::shell::open_url(&update::repo_url());
                        crate::shell::report(result, window, cx);
                    })
            }),
        )
        .description(
            update::repo_url()
                .trim_start_matches("https://")
                .to_string(),
        );
        let file = crate::settings::file();
        let folder = file.parent().map(|dir| dir.to_string_lossy().to_string());
        let path: SharedString = file.to_string_lossy().to_string().into();
        SettingPage::new(tr("关于"))
            .icon(Icon::new(IconName::Info))
            .default_open(true)
            .group(
                SettingGroup::new()
                    .title("Kuai")
                    .item(version)
                    .item(check_updates)
                    .item(source)
                    .item(SettingItem::render(move |_, _, cx| {
                        let muted = cx.theme().muted_foreground;
                        let folder = folder.clone();
                        v_flex()
                            .gap_3()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(div().text_sm().child(tr("设置文件")))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted)
                                            .whitespace_normal()
                                            .child(path.clone()),
                                    ),
                            )
                            .child(
                                h_flex().child(
                                    Button::new("open-settings-folder")
                                        .small()
                                        .label(tr("打开所在的文件夹"))
                                        .on_click(move |_, window, cx| {
                                            if let Some(folder) = &folder {
                                                let result = crate::shell::open_folder(folder);
                                                crate::shell::report(result, window, cx);
                                            }
                                        }),
                                ),
                            )
                    })),
            )
    }
}

/// 自动获取的一档叫什么。
fn auto_fetch_label(minutes: u32) -> SharedString {
    match minutes {
        0 => tr("不自动获取").into(),
        60 => tr("每小时").into(),
        minutes => fr_core::tf!("每 {minutes} 分钟", minutes).into(),
    }
}

fn language_key(language: Language) -> &'static str {
    match language {
        Language::System => "system",
        Language::Chinese => "zh-CN",
        Language::English => "en",
    }
}

fn theme_key(choice: ThemeChoice) -> &'static str {
    match choice {
        ThemeChoice::System => "system",
        ThemeChoice::Light => "light",
        ThemeChoice::Dark => "dark",
    }
}

/// 所有配色的小样：底色、面板、正文、次要文字、主色各一块，下面是名字。
/// 点一个就换成它（深色的配色同时把界面切到深色，浅色的切到浅色）；正在用的那个描一圈主色。
fn swatches(app: &Entity<AppState>, active: &'static str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let (border, accent, muted) = (theme.border, theme.primary, theme.muted_foreground);
    let color = |hex: &str| gpui_kit::component::try_parse_color(hex).unwrap_or_default();
    let cards = h_flex()
        .flex_wrap()
        .gap_2()
        .children(PALETTES.iter().map(|p| {
            let app = app.clone();
            let on = p.id == active;
            v_flex()
                .id(p.id)
                .w(px(150.))
                .rounded(px(8.))
                .border_2()
                .border_color(if on { accent } else { border })
                .overflow_hidden()
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    app.update(cx, |app, cx| {
                        app.update_settings(cx, |s| {
                            if p.dark {
                                s.dark_theme = p.id.to_string();
                                s.theme = ThemeChoice::Dark;
                            } else {
                                s.light_theme = p.id.to_string();
                                s.theme = ThemeChoice::Light;
                            }
                        })
                    });
                })
                .child(
                    h_flex()
                        .h(px(34.))
                        .bg(color(p.bg))
                        .child(div().w(px(36.)).h_full().bg(color(p.panel)))
                        .child(
                            h_flex()
                                .flex_1()
                                .px_2()
                                .gap_1()
                                .child(div().w(px(26.)).h(px(4.)).rounded_full().bg(color(p.fg)))
                                .child(div().w(px(16.)).h(px(4.)).rounded_full().bg(color(p.muted)))
                                .child(div().flex_1())
                                .child(
                                    div()
                                        .w(px(18.))
                                        .h(px(12.))
                                        .rounded(px(3.))
                                        .bg(color(p.accent)),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .text_xs()
                        .child(crate::ui::clipped("palette-name", p.label()).flex_1())
                        .when(on, |row| {
                            row.child(crate::ui::icon_small(IconName::Check).text_color(accent))
                        }),
                )
        }));
    v_flex()
        .gap_2()
        .child(
            div()
                .text_xs()
                .text_color(muted)
                .child(tr("点一个就换成它。")),
        )
        .child(cards)
        .into_any_element()
}

/// 设置页里出了错（比如写注册表失败）：在主窗口上弹通知。
fn report(text: String, cx: &mut App) {
    crate::with_main_window(cx, move |window, cx| {
        window.push_notification(Notification::error(text).autohide(false), cx);
    });
}

impl Render for SettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg = cx.theme().background;
        div().size_full().bg(bg).child(
            Settings::new("settings")
                .with_group_variant(GroupBoxVariant::Outline)
                .sidebar_width(px(200.))
                .pages([self.general(), self.appearance(), self.about()]),
        )
    }
}
