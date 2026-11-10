//! 主窗口：标签栏。第一个标签页是仓库管理器，后面每个打开的仓库一个。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::manager::{ManagerEvent, ManagerView};
use crate::prompt::{self, Prompts};
use crate::repo::RepoView;
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
