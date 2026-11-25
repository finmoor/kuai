//! 设置页：常规（语言、开机自启、托盘）、外观（深色浅色、配色）、终端（新建终端的预设、字体、光标、
//! 滚动和复制）、快捷键（见 `keys_page.rs`）、关于（版本、检查更新、源代码）。
//!
//! 每一项改了马上生效、马上存盘，没有「确定」「应用」。这里只改 [`AppState`] 里的设置；
//! 换主题、换语言、建托盘图标这些要动窗口的事，由主窗口（`workspace.rs`）看到设置变了再去做。
//! 开机自启不存在设置文件里，直接读写注册表（见 `autostart.rs`）。

use fr_core::i18n::{Language, tr};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonGroup};
use gpui_kit::component::group_box::GroupBoxVariant;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::setting::{
    NumberFieldOptions, SelectIndex, SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Selectable as _, Sizable as _, Size as ControlSize, WindowExt as _,
    h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::palette::PALETTES;
use crate::settings::{
    AUTO_FETCH_CHOICES, RightClick, TerminalCursor, TerminalSettings, ThemeChoice,
};
use crate::update::{self, Updater};

pub struct SettingsPage {
    app: Entity<AppState>,
    updater: Entity<Updater>,
    /// 换一个数，设置页就从头画（选中的页、滚到哪都重来），从 [`Self::start_page`] 那一页开始：
    /// 别处要打开设置的某一页时用（界面库的设置页只认第一次画时给的页）
    generation: usize,
    start_page: usize,
    /// 快捷键那一页：第一次画的时候建（里面的搜索框要窗口）
    keys: Option<Entity<crate::keys_page::KeysPage>>,
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
            generation: 0,
            start_page: 0,
            keys: None,
            _observe: observe,
        }
    }

    /// 打开时显示终端那一页（新建终端的预设在最上面）。
    pub fn show_terminal(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.start_page = 2;
        cx.notify();
    }

    /// 快捷键：上面键盘图、下面分组列出，整页由 [`crate::keys_page::KeysPage`] 画。
    fn shortcuts(&self, keys: Entity<crate::keys_page::KeysPage>) -> SettingPage {
        SettingPage::new(tr("快捷键"))
            .icon(Icon::new(IconName::Keyboard))
            .resettable(false)
            .description(tr(
                "焦点在终端中时，只有终端中可用的快捷键由 Kuai 处理，其余按键交给终端中的程序。",
            ))
            .group(
                SettingGroup::new()
                    .variant(GroupBoxVariant::Normal)
                    .item(SettingItem::render(move |_, _, _| keys.clone())),
            )
    }

    fn general(&self, cx: &App) -> SettingPage {
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
            .description(tr("立即生效，已显示的通知不受影响。"))
        };
        // 托盘：Windows 上总是有；Linux 上看桌面有没有，Wayland 上窗口藏不起来，只能最小化
        let (tray, can_hide) = (crate::tray::available(), crate::tray::can_hide());
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
        .description(match (cfg!(windows), tray, can_hide) {
            (true, ..) => tr("登录 Windows 后自动启动；开启托盘选项时直接进入托盘。"),
            (false, true, true) => tr("登录桌面后自动启动；开启托盘选项时直接进入托盘。"),
            (false, true, false) => tr("登录桌面后自动启动；开启托盘选项时启动后最小化。"),
            (false, false, _) => tr("登录桌面后自动启动。"),
        });
        let minimize = {
            let (get, set) = (app.clone(), app.clone());
            SettingItem::new(
                tr("最小化到托盘"),
                SettingField::switch(
                    move |cx| get.read(cx).settings.minimize_to_tray,
                    move |on, cx| {
                        set.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.minimize_to_tray = on)
                        })
                    },
                ),
            )
            .description(tr("最小化按钮将窗口收进托盘，点击托盘图标恢复。"))
        };
        let close = {
            let (get, set) = (app.clone(), app.clone());
            SettingItem::new(
                tr("关闭窗口时最小化到托盘"),
                SettingField::switch(
                    move |cx| get.read(cx).settings.close_to_tray,
                    move |on, cx| {
                        set.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.close_to_tray = on)
                        })
                    },
                ),
            )
            .description(match can_hide {
                true => tr("点击关闭按钮仅隐藏窗口，从托盘图标的右键菜单退出。"),
                false => tr("点击关闭按钮仅最小化窗口，从托盘图标的右键菜单退出。"),
            })
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
                "定时在后台获取当前仓库（git fetch --all），需要密码时跳过。",
            ))
        };
        let stash = {
            let (get, set) = (app.clone(), app.clone());
            SettingItem::new(
                tr("默认先贮藏未提交的改动"),
                SettingField::switch(
                    move |cx| get.read(cx).settings.stash_before_switch,
                    move |on, cx| {
                        set.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.stash_before_switch = on)
                        })
                    },
                ),
            )
            .description(tr("切换分支、检出或拉取时，对未提交改动的默认处理方式。"))
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
            // 键写现在的。键夹在句子中间，去掉了不能只删键（会剩「， 恢复」），按有没有键换整句
            .description(
                match (
                    crate::keymap::label("tab.close", cx),
                    crate::keymap::label("tab.reopen", cx),
                ) {
                    (Some(close), Some(reopen)) => tf!(
                        "关闭可避免误关；仍可用中键、{close} 或右键菜单关闭，{reopen} 恢复。",
                        close,
                        reopen
                    ),
                    (None, Some(reopen)) => {
                        tf!(
                            "关闭可避免误关；仍可用中键或右键菜单关闭，{reopen} 恢复。",
                            reopen
                        )
                    }
                    (Some(close), None) => {
                        tf!(
                            "关闭可避免误关；仍可用中键、{close} 或右键菜单关闭。",
                            close
                        )
                    }
                    (None, None) => tr("关闭可避免误关；仍可用中键或右键菜单关闭。").to_string(),
                },
            )
        };
        SettingPage::new(tr("常规"))
            .icon(Icon::new(IconName::Settings2))
            .default_open(true)
            .group(SettingGroup::new().title(tr("语言")).item(language))
            .group(SettingGroup::new().title(tr("标签页")).item(tab_close))
            .group({
                let group = SettingGroup::new().title(tr("启动和关闭")).item(autostart);
                // 没有托盘：两个托盘选项都不显示；藏不起来（Wayland）：没有「最小化到托盘」
                match (tray, can_hide) {
                    (true, true) => group.item(minimize).item(close),
                    (true, false) => group.item(close),
                    (false, _) => group,
                }
            })
            .group(
                SettingGroup::new()
                    .title("Git")
                    .item(auto_fetch)
                    .item(stash),
            )
    }

    /// 终端：最上面是预览，下面是字体、光标、滚动和复制、本机仓库用哪个 shell。
    /// 每一项改了马上作用在所有开着的终端上（见 `terminal::prefs`）。
    fn terminal(&self, cx: &App) -> SettingPage {
        let app = self.app.clone();
        let defaults = TerminalSettings::default();
        // 读终端设置里的一项、改一项
        fn get<T>(
            app: &Entity<AppState>,
            read: impl Fn(&TerminalSettings) -> T + 'static,
        ) -> impl Fn(&App) -> T + 'static {
            let app = app.clone();
            move |cx| read(&app.read(cx).settings.terminal)
        }
        fn set<T>(
            app: &Entity<AppState>,
            write: impl Fn(&mut TerminalSettings, T) + 'static,
        ) -> impl Fn(T, &mut App) + 'static {
            let app = app.clone();
            move |value, cx| {
                app.update(cx, |app, cx| {
                    app.update_settings(cx, |s| write(&mut s.terminal, value))
                })
            }
        }

        let preview = SettingItem::render(|_, _, cx| {
            let muted = cx.theme().muted_foreground;
            v_flex()
                .w_full()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(tr("预览。改动立即作用于所有已打开的终端。")),
                )
                .child(crate::terminal::preview(cx))
        });

        let font = {
            let (read, write) = (app.clone(), app.clone());
            let (dirty, reset) = (app.clone(), app.clone());
            let default = defaults.font.clone();
            let default_again = default.clone();
            SettingItem::new(
                tr("字体"),
                SettingField::render(move |options, _, cx| {
                    let current: SharedString = read.read(cx).settings.terminal.font.clone().into();
                    let mut fonts: Vec<FontOption> = crate::terminal::font_choices(cx)
                        .monospace
                        .iter()
                        .map(|name| FontOption::font(name.clone()))
                        .collect();
                    // 设置里写着的字体这台电脑上没装：照样列出来，不然下拉里没有选中的那一项
                    if !fonts.iter().any(|font| font.value == current) {
                        fonts.insert(0, FontOption::font(current.clone()));
                    }
                    let write = write.clone();
                    font_dropdown(
                        "terminal-font",
                        current,
                        fonts,
                        options.size(),
                        move |name, cx| {
                            write.update(cx, |app, cx| {
                                app.update_settings(cx, |s| s.terminal.font = name.to_string())
                            })
                        },
                    )
                })
                .on_reset(
                    move |cx| dirty.read(cx).settings.terminal.font != default,
                    move |_, cx| {
                        let font = default_again.clone();
                        reset.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.terminal.font = font)
                        })
                    },
                ),
            )
            .description(tr("仅列出等宽字体。"))
        };
        let cjk_font = {
            let (read, write) = (app.clone(), app.clone());
            let (dirty, reset) = (app.clone(), app.clone());
            SettingItem::new(
                tr("中文字体"),
                SettingField::render(move |options, _, cx| {
                    let current: SharedString =
                        read.read(cx).settings.terminal.cjk_font.clone().into();
                    let mut fonts = vec![FontOption {
                        value: SharedString::default(),
                        label: tr("跟随系统").into(),
                        family: None,
                    }];
                    fonts.extend(
                        crate::terminal::font_choices(cx)
                            .cjk
                            .iter()
                            .map(|name| FontOption::font(name.clone())),
                    );
                    if !fonts.iter().any(|font| font.value == current) {
                        fonts.insert(1, FontOption::font(current.clone()));
                    }
                    let write = write.clone();
                    font_dropdown(
                        "terminal-cjk-font",
                        current,
                        fonts,
                        options.size(),
                        move |name, cx| {
                            write.update(cx, |app, cx| {
                                app.update_settings(cx, |s| s.terminal.cjk_font = name.to_string())
                            })
                        },
                    )
                })
                .on_reset(
                    move |cx| !dirty.read(cx).settings.terminal.cjk_font.is_empty(),
                    move |_, cx| {
                        reset.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.terminal.cjk_font.clear())
                        })
                    },
                ),
            )
            .description(tr(
                "上方字体中缺少的中文用此字体显示，固定占两格，不影响对齐。",
            ))
        };
        let font_size = SettingItem::new(
            tr("字号"),
            SettingField::number_input(
                NumberFieldOptions {
                    min: f64::from(TerminalSettings::MIN_SIZE),
                    max: f64::from(TerminalSettings::MAX_SIZE),
                    step: 1.,
                },
                get(&app, |t| f64::from(t.font_size)),
                set(&app, |t, size: f64| t.font_size = size as f32),
            )
            .default_value(f64::from(defaults.font_size)),
        )
        .description(tr(
            "终端中也可按 Ctrl+加号、Ctrl+减号调整，Ctrl+0 恢复默认。",
        ));
        let line_height = {
            let key = |height: f32| SharedString::from(format!("{height}"));
            let label = |height: f32| -> SharedString {
                match height {
                    1. => tr("1.0（紧凑）").into(),
                    1.25 => tr("1.25（默认）").into(),
                    1.6 => tr("1.6（宽松）").into(),
                    h => format!("{h}").into(),
                }
            };
            SettingItem::new(
                tr("行高"),
                SettingField::dropdown(
                    TerminalSettings::LINE_HEIGHTS
                        .into_iter()
                        .map(|height| (key(height), label(height)))
                        .collect(),
                    get(&app, move |t| key(t.line_height)),
                    set(&app, |t, value: SharedString| {
                        t.line_height = value.parse().unwrap_or(1.25)
                    }),
                )
                .default_value(key(defaults.line_height)),
            )
        };

        let cursor = {
            let (read, write) = (app.clone(), app.clone());
            let (dirty, reset) = (app.clone(), app.clone());
            SettingItem::new(
                tr("光标样式"),
                SettingField::render(move |options, _, cx| {
                    let current = read.read(cx).settings.terminal.cursor;
                    ButtonGroup::new("terminal-cursor")
                        .outline()
                        .with_size(options.size())
                        .children(TerminalCursor::ALL.into_iter().map(|choice| {
                            let write = write.clone();
                            Button::new(choice.label())
                                .label(choice.label())
                                .selected(choice == current)
                                .on_click(move |_, _, cx| {
                                    write.update(cx, |app, cx| {
                                        app.update_settings(cx, |s| s.terminal.cursor = choice)
                                    })
                                })
                        }))
                })
                .on_reset(
                    move |cx| dirty.read(cx).settings.terminal.cursor != TerminalCursor::default(),
                    move |_, cx| {
                        reset.update(cx, |app, cx| {
                            app.update_settings(cx, |s| {
                                s.terminal.cursor = TerminalCursor::default()
                            })
                        })
                    },
                ),
            )
            .description(tr("终端中的程序（例如 vim）可临时改为其他样式。"))
        };
        let blink = SettingItem::new(
            tr("光标闪烁"),
            SettingField::switch(
                get(&app, |t| t.cursor_blink),
                set(&app, |t, on: bool| t.cursor_blink = on),
            )
            .default_value(defaults.cursor_blink),
        );

        let scrollback = {
            let key = |lines: usize| SharedString::from(lines.to_string());
            SettingItem::new(
                tr("回滚行数"),
                SettingField::dropdown(
                    TerminalSettings::SCROLLBACKS
                        .into_iter()
                        .map(|lines| {
                            let label = match lines {
                                10_000 => tf!("{n} 行（默认）", n = group_digits(lines)),
                                _ => tf!("{n} 行", n = group_digits(lines)),
                            };
                            (key(lines), label.into())
                        })
                        .collect(),
                    get(&app, move |t| key(t.scrollback)),
                    set(&app, |t, value: SharedString| {
                        t.scrollback = value.parse().unwrap_or(10_000)
                    }),
                )
                .default_value(key(defaults.scrollback)),
            )
            .description(tr(
                "每个终端保留的输出行数。远程主机上的终端重新连接后可回看最近 2,000 行。",
            ))
        };
        let copy_on_select = SettingItem::new(
            tr("选中即复制"),
            SettingField::switch(
                get(&app, |t| t.copy_on_select),
                set(&app, |t, on: bool| t.copy_on_select = on),
            )
            .default_value(defaults.copy_on_select),
        )
        .description(tr("选中文字后自动复制。"));
        let right_click = {
            let key = |choice: RightClick| -> SharedString {
                match choice {
                    RightClick::Paste => "paste".into(),
                    RightClick::Menu => "menu".into(),
                }
            };
            SettingItem::new(
                tr("右键"),
                SettingField::dropdown(
                    RightClick::ALL
                        .into_iter()
                        .map(|choice| (key(choice), choice.label().into()))
                        .collect(),
                    get(&app, move |t| key(t.right_click)),
                    set(&app, move |t, value: SharedString| {
                        t.right_click = RightClick::ALL
                            .into_iter()
                            .find(|choice| key(*choice) == value)
                            .unwrap_or_default()
                    }),
                )
                .default_value(key(defaults.right_click)),
            )
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
                "对新打开的终端生效。远程主机上的仓库使用该主机的登录 shell。",
            ))
        };
        let notify = {
            let (get, set) = (app.clone(), app.clone());
            SettingItem::new(
                tr("终端停下时提醒"),
                SettingField::switch(
                    move |cx| get.read(cx).settings.notify_terminal_stopped,
                    move |on, cx| {
                        set.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.notify_terminal_stopped = on)
                        })
                    },
                ),
            )
            .description(tr(
                "未显示的终端超过 10 秒没有新输出时提醒；Kuai 不在前台时任务栏按钮闪烁。",
            ))
        };
        let notify_system = {
            let (get, set) = (app.clone(), app.clone());
            SettingItem::new(
                match cfg!(windows) {
                    true => tr("发送 Windows 通知"),
                    false => tr("发送系统通知"),
                },
                SettingField::switch(
                    move |cx| get.read(cx).settings.notify_terminal_system,
                    move |on, cx| {
                        set.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.notify_terminal_system = on)
                        })
                    },
                ),
            )
            .description(match cfg!(windows) {
                true => tr(
                    "Kuai 不在前台时，终端停下同时发送 Windows 通知；点击通知前往该终端。需开启上一项。",
                ),
                false => tr(
                    "Kuai 不在前台时，终端停下同时发送系统通知；点击通知前往该终端。需开启上一项。",
                ),
            })
        };
        SettingPage::new(tr("终端"))
            .icon(Icon::new(IconName::SquareTerminal))
            .default_open(true)
            .group(crate::presets::settings_group(&self.app, cx))
            .group(SettingGroup::new().item(preview))
            .group(
                SettingGroup::new()
                    .title(tr("字体"))
                    .item(font)
                    .item(cjk_font)
                    .item(font_size)
                    .item(line_height),
            )
            .group(
                SettingGroup::new()
                    .title(tr("光标"))
                    .item(cursor)
                    .item(blink),
            )
            .group(
                SettingGroup::new()
                    .title(tr("滚动和复制"))
                    .item(scrollback)
                    .item(copy_on_select)
                    .item(right_click),
            )
            .group(SettingGroup::new().title(tr("本机仓库")).item(shell))
            .group(
                SettingGroup::new()
                    .title(tr("提醒"))
                    .item(notify)
                    .item(notify_system),
            )
    }

    fn appearance(&self) -> SettingPage {
        let app = self.app.clone();
        let mode = {
            let (get, set) = (app.clone(), app.clone());
            SettingItem::new(
                tr("颜色模式"),
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
            .description(match cfg!(windows) {
                true => tr("「跟随系统」使用 Windows 的深色或浅色设置。"),
                false => tr("「跟随系统」使用系统的深色或浅色设置。"),
            })
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
                    tr("深色模式配色")
                } else {
                    tr("浅色模式配色")
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
                    .title(tr("全部配色"))
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
            .description(tr("启动时及每 6 小时检查一次新版本。"))
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
        // GPL-3.0 要求的「适当的法律声明」：版权、不提供担保、可以按 GPL 再分发、在哪看全文。
        // 原版界面上显示了，改版也必须保留（第 5 条 d 款）
        let license = SettingItem::new(
            tr("许可证"),
            SettingField::render(|_, _, _| {
                Button::new("open-license")
                    .small()
                    .icon(Icon::new(IconName::ExternalLink))
                    .label(tr("打开"))
                    .on_click(|_, window, cx| {
                        let url = format!("{}/blob/main/LICENSE", update::repo_url());
                        let result = crate::shell::open_url(&url);
                        crate::shell::report(result, window, cx);
                    })
            }),
        )
        .description(format!(
            "Copyright © 2026 Feng\n{}",
            tr("按 GNU GPL 3.0 或更高版本发布，可自由使用、修改和再分发，不提供任何担保。")
        ));
        // 把服务器目录映射到本机用的是 WinFsp：不随 Kuai 分发，用的时候才装。照它的许可写明出处
        let winfsp = SettingItem::new(
            tr("目录映射"),
            SettingField::render(|_, _, _| {
                Button::new("open-winfsp")
                    .small()
                    .icon(Icon::new(IconName::ExternalLink))
                    .label(tr("打开"))
                    .on_click(|_, window, cx| {
                        let result = crate::shell::open_url(fr_mount::WINFSP_URL);
                        crate::shell::report(result, window, cx);
                    })
            }),
        )
        .description(format!(
            "{}\n{}",
            tr("映射主机上的目录需要 WinFsp，须单独安装。"),
            fr_mount::WINFSP_NOTICE
        ));
        // 诊断信息：版本、系统、各主机的连接情况、最近的错误，复制前可以先看
        let diagnose = {
            let app = self.app.clone();
            SettingItem::new(
                tr("诊断信息"),
                SettingField::render(move |_, _, _| {
                    let app = app.clone();
                    Button::new("copy-diagnose")
                        .small()
                        .label(tr("复制诊断信息…"))
                        .on_click(move |_, window, cx| {
                            crate::diagnose::Summary::open(app.clone(), window, cx)
                        })
                }),
            )
            .description(tr(
                "版本、系统、各主机最近一次的连接结果、最近的错误。复制前可以先看内容，密码、口令和令牌不会复制。",
            ))
        };
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
                    .item(license)
                    // WinFsp 只有 Windows 上用；Linux 上映射用系统的 fuse3，不随 Kuai 分发
                    .items(cfg!(windows).then_some(winfsp))
                    .item(diagnose)
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
                                        .label(tr("打开所在文件夹"))
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

/// 字体下拉里的一项。
struct FontOption {
    /// 存进设置的值（字体名；「跟随系统」是空的）
    value: SharedString,
    label: SharedString,
    /// 这一项用哪个字体显示：字体名就用它自己，一眼看得出长什么样
    family: Option<SharedString>,
}

impl FontOption {
    fn font(name: SharedString) -> Self {
        Self {
            value: name.clone(),
            label: name.clone(),
            family: Some(name),
        }
    }
}

/// 选字体的下拉：和别的下拉一个样子，只是每一项用它自己的字体显示。
fn font_dropdown(
    id: &'static str,
    current: SharedString,
    fonts: Vec<FontOption>,
    size: ControlSize,
    pick: impl Fn(SharedString, &mut App) + 'static,
) -> impl IntoElement {
    let in_own_face = |label: SharedString, family: Option<SharedString>| {
        div()
            .when_some(family, |text, family| text.font_family(family))
            .child(label)
    };
    let shown = fonts.iter().find(|font| font.value == current);
    let (label, family) = shown.map_or((current.clone(), None), |font| {
        (font.label.clone(), font.family.clone())
    });
    let pick = std::rc::Rc::new(pick);
    Button::new(id)
        .child(in_own_face(label, family))
        .dropdown_caret(true)
        .outline()
        .with_size(size)
        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
            let menu = fonts.iter().fold(menu, |menu, font| {
                let (label, family) = (font.label.clone(), font.family.clone());
                let (value, pick) = (font.value.clone(), pick.clone());
                menu.item(
                    PopupMenuItem::element(move |_, _| in_own_face(label.clone(), family.clone()))
                        .checked(font.value == current)
                        .on_click(move |_, _, cx| pick(value.clone(), cx)),
                )
            });
            menu.scrollable(true).max_h(px(320.))
        })
}

/// 12345 → 12,345。
fn group_digits(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
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
                .child(tr("点击即可切换。")),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg = cx.theme().background;
        let app = self.app.clone();
        let keys = self
            .keys
            .get_or_insert_with(|| cx.new(|cx| crate::keys_page::KeysPage::new(app, window, cx)))
            .clone();
        div()
            .id(("settings-page", self.generation))
            .size_full()
            .bg(bg)
            .child(
                Settings::new("settings")
                    .with_group_variant(GroupBoxVariant::Outline)
                    .sidebar_width(px(200.))
                    .default_selected_index(SelectIndex {
                        page_ix: self.start_page,
                        group_ix: None,
                    })
                    .pages([
                        self.general(cx),
                        self.appearance(),
                        self.terminal(cx),
                        self.shortcuts(keys),
                        self.about(),
                    ]),
            )
    }
}
