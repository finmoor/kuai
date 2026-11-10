//! 主窗口：标签栏。第一个标签页是仓库管理器，后面每个打开的仓库一个。
//!
//! 快捷键：F5 刷新、Ctrl+W 关标签页、Ctrl+Tab 切标签页、Ctrl+F 搜索提交、Ctrl+P 快速打开仓库。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::manager::{ManagerEvent, ManagerView};
use crate::prompt::{self, Prompts};
use crate::repo::{RepoEvent, RepoView};
use crate::settings::RepoEntry;

struct RepoTab {
    entry: RepoEntry,
    view: Entity<RepoView>,
}

pub struct Workspace {
    app: Entity<AppState>,
    manager: Entity<ManagerView>,
    tabs: Vec<RepoTab>,
    /// 0 是仓库管理器，i 是 `tabs[i - 1]`
    active: usize,
    _subscriptions: Vec<Subscription>,
    _prompts: Task<()>,
}

impl Workspace {
    pub fn new(
        app: Entity<AppState>,
        prompts: Prompts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let manager = cx.new(|cx| ManagerView::new(app.clone(), cx));
        let subscriptions = vec![
            cx.subscribe_in(&manager, window, |this, _, event, window, cx| match event {
                ManagerEvent::Open(entry) => this.open(entry.clone(), window, cx),
            }),
            // 服务器改名之类的变化要刷新标签上的名字
            cx.observe(&app, |_, _, cx| cx.notify()),
            // 记住窗口的位置和大小
            cx.observe_window_bounds(window, |this, window, cx| {
                let state = crate::appearance::window_state(window);
                this.app
                    .update(cx, |app, cx| app.set_window_state(state, cx));
            }),
            // 主题设成「跟随系统」时，Windows 切换深浅色就跟着换
            cx.observe_window_appearance(window, |this, window, cx| {
                let choice = this.app.read(cx).settings.theme;
                if choice == crate::settings::ThemeChoice::System {
                    crate::appearance::apply_theme(choice, window, cx);
                }
            }),
        ];
        let mut this = Self {
            app: app.clone(),
            manager,
            tabs: Vec::new(),
            active: 0,
            _subscriptions: subscriptions,
            _prompts: prompt::show_in(prompts, window, cx),
        };
        // 恢复上次开着的标签页
        let (open, active) = {
            let settings = &app.read(cx).settings;
            (settings.open_tabs.clone(), settings.active_tab)
        };
        for entry in open {
            this.push_tab(entry, window, cx);
        }
        this.active = active.min(this.tabs.len());
        this.update_visibility(cx);
        this
    }

    /// 告诉每个仓库标签页它是不是当前显示的：切到哪一页，哪一页就刷新。
    fn update_visibility(&self, cx: &mut Context<Self>) {
        for (ix, tab) in self.tabs.iter().enumerate() {
            let visible = self.active == ix + 1;
            tab.view
                .update(cx, |view, cx| view.set_visible(visible, cx));
        }
    }

    /// 打开仓库：已经开着就切过去。
    pub fn open(&mut self, entry: RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        match self.tabs.iter().position(|t| t.entry == entry) {
            Some(ix) => self.active = ix + 1,
            None => {
                self.push_tab(entry, window, cx);
                self.active = self.tabs.len();
            }
        }
        self.persist(cx);
        cx.notify();
    }

    fn push_tab(&mut self, entry: RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let view = cx.new(|cx| RepoView::new(entry.clone(), app, window, cx));
        // 仓库页要求打开另一个仓库（子模块）
        cx.subscribe_in(&view, window, |this, _, event, window, cx| match event {
            RepoEvent::Open(entry) => {
                this.app
                    .update(cx, |app, cx| app.add_repo(entry.clone(), cx));
                this.open(entry.clone(), window, cx);
            }
        })
        .detach();
        self.tabs.push(RepoTab { entry, view });
    }

    fn close(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.tabs.remove(ix);
        // 关掉的是当前页或它前面的页，选中位置跟着前移
        if self.active > ix {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len());
        self.persist(cx);
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.active = ix.min(self.tabs.len());
        self.persist(cx);
        cx.notify();
    }

    /// F5：重读当前仓库的状态和提交历史。
    pub fn refresh_active(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.active.checked_sub(1).and_then(|ix| self.tabs.get(ix)) {
            tab.view.update(cx, |view, cx| view.refresh_now(cx));
        }
    }

    /// Ctrl+W：关掉当前的仓库标签页。
    pub fn close_active(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.active.checked_sub(1) {
            self.close(ix, cx);
        }
    }

    /// Ctrl+Tab、Ctrl+Shift+Tab：切到下一个、上一个标签页，到头绕回去。
    pub fn cycle_tab(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = self.tabs.len() as isize + 1;
        let next = (self.active as isize + step).rem_euclid(count);
        self.select(next as usize, cx);
    }

    /// Ctrl+F：在当前仓库里搜索提交。
    pub fn find_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active.checked_sub(1).and_then(|ix| self.tabs.get(ix)) {
            tab.view.update(cx, |view, cx| view.find_commit(window, cx));
        }
    }

    /// Ctrl+P：快速打开仓库。输入几个字筛选，回车打开第一个，也可以点。
    pub fn quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let picker = cx.new(|cx| QuickOpen::new(app, window, cx));
        cx.subscribe_in(&picker, window, |this, _, event: &Picked, window, cx| {
            window.close_dialog(cx);
            this.open(event.0.clone(), window, cx);
        })
        .detach();
        let first_build = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            // 对话框打开的那一刻焦点会被它拿走，打开后再放到输入框
            if first_build.replace(false) {
                let input = picker.read(cx).input.clone();
                input.focus_handle(cx).focus(window, cx);
            }
            let on_enter = picker.clone();
            dialog
                .title("快速打开仓库")
                .w(px(620.))
                // 回车打开第一个匹配的
                .on_ok(move |_, _, cx| {
                    on_enter.update(cx, |picker, cx| picker.pick_first(cx));
                    false
                })
                .child(picker.clone())
        });
    }

    fn persist(&self, cx: &mut Context<Self>) {
        self.update_visibility(cx);
        let tabs = self.tabs.iter().map(|t| t.entry.clone()).collect();
        let active = self.active;
        self.app
            .update(cx, |app, _| app.set_open_tabs(tabs, active));
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, fg) = (theme.background, theme.foreground);
        let app = self.app.read(cx);

        let mut tabs = vec![Tab::new().label("仓库")];
        for (ix, tab) in self.tabs.iter().enumerate() {
            let close = Button::new(("close-tab", ix))
                .ghost()
                .xsmall()
                .label("×")
                .tooltip("关闭")
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.close(ix, cx);
                }));
            tabs.push(Tab::new().label(app.repo_title(&tab.entry)).suffix(close));
        }

        let content = match self.active {
            0 => self.manager.clone().into_any_element(),
            ix => self.tabs[ix - 1].view.clone().into_any_element(),
        };

        v_flex()
            .size_full()
            .bg(bg)
            .text_color(fg)
            .child(
                TabBar::new("tabs")
                    .children(tabs)
                    .selected_index(self.active)
                    .on_click(cx.listener(|this, ix: &usize, _, cx| this.select(*ix, cx))),
            )
            .child(div().flex_1().min_h_0().child(content))
    }
}

/// 快速打开里选中了一个仓库。
struct Picked(RepoEntry);

/// 快速打开：一个输入框加仓库列表，输入的字按名字和路径筛选。
struct QuickOpen {
    app: Entity<AppState>,
    input: Entity<InputState>,
    _subscription: Subscription,
}

impl EventEmitter<Picked> for QuickOpen {}

impl QuickOpen {
    fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("仓库名或路径里的几个字"));
        let subscription = cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        Self {
            app,
            input,
            _subscription: subscription,
        }
    }

    /// 匹配输入内容的仓库：名字、路径、服务器名里有这几个字（不分大小写）。
    fn matches(&self, cx: &App) -> Vec<(RepoEntry, SharedString)> {
        let needle = self.input.read(cx).value().trim().to_lowercase();
        let app = self.app.read(cx);
        app.settings
            .repos
            .iter()
            .map(|entry| (entry.clone(), app.repo_title(entry)))
            .filter(|(entry, title)| {
                needle.is_empty()
                    || title.to_lowercase().contains(&needle)
                    || entry.path.to_lowercase().contains(&needle)
            })
            .collect()
    }

    fn pick_first(&mut self, cx: &mut Context<Self>) {
        if let Some((entry, _)) = self.matches(cx).into_iter().next() {
            cx.emit(Picked(entry));
        }
    }
}

impl Render for QuickOpen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, hover, active) = (theme.muted_foreground, theme.list_hover, theme.list_active);
        let rows: Vec<AnyElement> = self
            .matches(cx)
            .into_iter()
            .enumerate()
            .map(|(ix, (entry, title))| {
                let path = entry.path.clone();
                div()
                    .id(ix)
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    // 第一个是回车会打开的
                    .when(ix == 0, |row| row.bg(active))
                    .hover(|s| s.bg(hover))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(Picked(entry.clone()))))
                    .child(div().text_sm().child(title))
                    .child(div().text_xs().text_color(muted).truncate().child(path))
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        v_flex().gap_2().child(Input::new(&self.input)).child(
            div()
                .id("quick-open-list")
                .max_h(px(360.))
                .overflow_y_scroll()
                .child(v_flex().gap_1().children(rows))
                .when(empty, |list| {
                    list.child(
                        div()
                            .p_2()
                            .text_sm()
                            .text_color(muted)
                            .child("没有匹配的仓库"),
                    )
                }),
        )
    }
}
