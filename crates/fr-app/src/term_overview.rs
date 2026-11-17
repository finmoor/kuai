//! 终端总览页：不切换工作区，看所有工作区里的终端在跑、停下了还是空着。
//!
//! 左边按工作区分组列出连着的各台电脑上的终端（数据在 [`TermBoard`]），右边是选中那个的画面
//! （[`TermPeek`]，只看、不能输入）。上下键换一个，Enter、双击或者「前往」过去：切到它所在的
//! 工作区、打开那个仓库的标签页、到终端页选中它（由主窗口来做，见 `Workspace::go_to_terminal`）。
//! 设计见 `design/workspace-overview/`（方向 A）。

use std::time::Instant;

use fr_core::i18n::{tr, trc};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::machines::{WorkspaceKey, machine_of, split_relay};
use crate::term_board::{Group, Row, TermBoard, TermId, TermState};
use crate::terminal::TermPeek;
use crate::ui;

/// 左边列表的宽度
const LIST_WIDTH: f32 = 430.;
/// 列表、预览上面那一条的高度（和仓库页的工具栏一样）
const HEAD_HEIGHT: f32 = 52.;
const ROW_HEIGHT: f32 = 50.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Stopped,
    Running,
}

/// 要前往一个终端。
pub struct Go(pub Row);

pub struct TermOverview {
    app: Entity<AppState>,
    board: Entity<TermBoard>,
    selected: Option<TermId>,
    filter: Filter,
    /// 选中的那个终端的画面：哪个终端、建的时候它几列几行
    peek: Option<(TermId, Entity<TermPeek>)>,
    focus: FocusHandle,
    _observe: Subscription,
}

impl EventEmitter<Go> for TermOverview {}

impl Focusable for TermOverview {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// 多久以前，粗略地说。
fn ago(at: Instant) -> String {
    let secs = at.elapsed().as_secs();
    match secs {
        0..60 => tr("刚刚").to_string(),
        60..3600 => tf!("{n} 分钟前", n = (secs / 60).to_string()),
        3600..86400 => tf!("{n} 小时前", n = (secs / 3600).to_string()),
        _ => tf!("{n} 天前", n = (secs / 86400).to_string()),
    }
}

impl TermOverview {
    pub fn new(app: Entity<AppState>, board: Entity<TermBoard>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&board, |this, _, cx| {
            this.sync(cx);
            cx.notify();
        });
        Self {
            app,
            board,
            selected: None,
            filter: Filter::All,
            peek: None,
            focus: cx.focus_handle(),
            _observe: observe,
        }
    }

    /// 选中的是哪个终端（它显示在右边，算看着的）。
    pub fn selected(&self) -> Option<&TermId> {
        self.selected.as_ref()
    }

    /// 这一页显示出来了：先选中一个要看的（已停下的优先），焦点放到列表上。
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let still = self
            .selected
            .as_ref()
            .is_some_and(|id| rows.iter().any(|row| row.id == *id));
        if !still {
            let first = rows
                .iter()
                .find(|row| row.state == TermState::Stopped)
                .or(rows.first());
            self.selected = first.map(|row| row.id.clone());
        }
        self.sync(cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// 这一页不显示了：不用再看着那个终端。
    pub fn hidden(&mut self, cx: &mut Context<Self>) {
        self.peek = None;
        cx.notify();
    }

    fn groups(&self, cx: &App) -> Vec<Group> {
        let mut groups = self.board.read(cx).groups(cx);
        for group in &mut groups {
            group.rows.retain(|row| match self.filter {
                Filter::All => true,
                Filter::Stopped => row.state == TermState::Stopped,
                Filter::Running => row.state == TermState::Running,
            });
        }
        groups.retain(|group| !group.rows.is_empty());
        groups
    }

    /// 列表里现在有的行，从上到下。
    fn rows(&self, cx: &App) -> Vec<Row> {
        self.groups(cx)
            .into_iter()
            .flat_map(|group| group.rows)
            .collect()
    }

    /// 让右边的画面跟上选中的终端：换了一个、它改了大小就重新接。
    fn sync(&mut self, cx: &mut Context<Self>) {
        let board = self.board.read(cx);
        let row = self.selected.as_ref().and_then(|id| board.row(id, cx));
        let Some(row) = row else {
            self.peek = None;
            return;
        };
        let current = self.peek.as_ref().is_some_and(|(id, peek)| {
            *id == row.id && peek.read(cx).size() == (row.cols.max(2), row.rows.max(1))
        });
        if current {
            return;
        }
        let Some(conn) = board.conn(&row.id.machine) else {
            self.peek = None;
            return;
        };
        let peek = cx.new(|cx| TermPeek::new(&conn, &row.id.term, row.cols, row.rows, cx));
        self.peek = Some((row.id, peek));
    }

    fn select(&mut self, id: TermId, cx: &mut Context<Self>) {
        self.selected = Some(id);
        self.sync(cx);
        cx.notify();
    }

    fn go(&mut self, id: &TermId, cx: &mut Context<Self>) {
        if let Some(row) = self.board.read(cx).row(id, cx) {
            cx.emit(Go(row));
        }
    }

    fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.filter = filter;
        // 选中的被筛掉了：换成剩下的第一个
        let rows = self.rows(cx);
        let still = self
            .selected
            .as_ref()
            .is_some_and(|id| rows.iter().any(|row| row.id == *id));
        if !still {
            self.selected = rows.first().map(|row| row.id.clone());
            self.sync(cx);
        }
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        if rows.is_empty() {
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|id| rows.iter().position(|row| row.id == *id));
        match event.keystroke.key.as_str() {
            "down" => {
                let next = at.map_or(0, |at| (at + 1).min(rows.len() - 1));
                self.select(rows[next].id.clone(), cx);
            }
            "up" => {
                let next = at.map_or(0, |at| at.saturating_sub(1));
                self.select(rows[next].id.clone(), cx);
            }
            "enter" => match at {
                Some(at) => cx.emit(Go(rows[at].clone())),
                None => return,
            },
            _ => return,
        }
        cx.stop_propagation();
    }

    /// 状态前面的圆点：运行中是实心的主色，已停下是带一圈的警示色，空闲是空心的。
    fn dot(state: TermState, cx: &App) -> Div {
        let theme = cx.theme();
        let dot = div().flex_none().size(px(8.)).rounded_full();
        match state {
            TermState::Running => dot.bg(theme.primary),
            TermState::Stopped => dot.bg(theme.warning),
            TermState::Idle => dot.border_1().border_color(theme.muted_foreground),
        }
    }

    fn state_text(row: &Row) -> SharedString {
        match (row.alive, row.state) {
            (false, _) => tr("已结束").into(),
            (true, TermState::Running) => tr("运行中").into(),
            (true, TermState::Stopped) => tr("已停下").into(),
            (true, TermState::Idle) => tr("空闲").into(),
        }
    }

    /// 终端叫什么：程序自己设的标题，没有就是「终端 N」。
    fn name(row: &Row) -> SharedString {
        match &row.title {
            Some(title) => title.clone().into(),
            None => tf!("终端 {n}", n = row.number.to_string()).into(),
        }
    }

    /// 它在哪台电脑上：经别的电脑连的写成「91，经由 156」。
    fn place(app: &AppState, row: &Row) -> SharedString {
        let machine = machine_of(&row.entry);
        match split_relay(machine) {
            Some((via, _)) => tf!(
                "{name}，经由 {via}",
                name = app.machine_name(machine),
                via = app.machine_name(via)
            )
            .into(),
            None => app.machine_name(machine),
        }
    }

    fn render_row(&self, index: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, selected_bg, hover, primary, warning) = (
            theme.foreground,
            theme.muted_foreground,
            theme.list_active,
            theme.list_hover,
            theme.primary,
            theme.warning,
        );
        let app = self.app.read(cx);
        let selected = self.selected.as_ref() == Some(&row.id);
        let unseen = row.state == TermState::Stopped;
        let repo = app.repo_name(&row.entry);
        let remote = row.entry.server.is_some();
        let state_color = match row.state {
            _ if !row.alive => muted,
            TermState::Running => primary,
            TermState::Stopped => warning,
            TermState::Idle => muted,
        };
        // 已停下的写停了多久；运行中的不写
        let since = row
            .last_output
            .filter(|_| row.state != TermState::Running)
            .map(ago);
        let (for_click, for_go) = (row.id.clone(), row.id.clone());
        h_flex()
            .id(("overview-row", index))
            .h(px(ROW_HEIGHT))
            .mx(px(8.))
            .px(px(10.))
            .gap(px(11.))
            .rounded(px(7.))
            .border_1()
            .cursor_pointer()
            .when(selected, |el| {
                el.bg(selected_bg).border_color(primary.opacity(0.5))
            })
            .when(!selected, |el| {
                el.border_color(gpui_kit::transparent_black())
                    .hover(|s| s.bg(hover))
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.focus.focus(window, cx);
                match event.click_count() >= 2 {
                    true => this.go(&for_go, cx),
                    false => this.select(for_click.clone(), cx),
                }
            }))
            .child(Self::dot(row.state, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        h_flex()
                            .gap(px(7.))
                            .text_size(px(13.))
                            .child(
                                ui::clipped(("overview-name", index), Self::name(row))
                                    .flex_shrink(1.)
                                    .text_color(fg)
                                    .when(unseen, |el| el.font_weight(FontWeight::SEMIBOLD)),
                            )
                            .child(
                                ui::clipped(("overview-repo", index), repo)
                                    .flex_shrink(1.)
                                    .text_color(muted),
                            )
                            .when(remote, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(12.))
                                        .text_color(muted)
                                        .child(Self::place(app, row)),
                                )
                            }),
                    )
                    .child(
                        ui::clipped(("overview-last", index), row.last.clone())
                            .font_family(theme.mono_font_family.clone())
                            .text_size(px(12.))
                            .text_color(muted),
                    ),
            )
            .child(
                v_flex()
                    .flex_none()
                    .items_end()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(state_color)
                            .when(unseen, |el| el.font_weight(FontWeight::SEMIBOLD))
                            .child(Self::state_text(row)),
                    )
                    .children(
                        since.map(|since| div().text_size(px(11.)).text_color(muted).child(since)),
                    ),
            )
            .into_any_element()
    }

    fn render_group_head(&self, workspace: Option<&WorkspaceKey>, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let (muted, chip_bg) = (theme.muted_foreground, theme.secondary);
        let app = self.app.read(cx);
        let name: SharedString = match workspace {
            Some(key) => app.workspace_name(key),
            None => tr("其他").into(),
        };
        let current = workspace.is_some_and(|key| key == app.current());
        // 服务器上的工作区标出是哪台
        let machine = workspace
            .filter(|key| !key.is_local())
            .map(|key| app.machine_name(&key.machine));
        let chip = |text: SharedString| {
            div()
                .flex_none()
                .h(px(18.))
                .px(px(6.))
                .rounded_full()
                .bg(chip_bg)
                .text_size(px(11.))
                .line_height(px(18.))
                .text_color(muted)
                .child(text)
        };
        h_flex()
            .h(px(30.))
            .mt(px(6.))
            .px(px(16.))
            .gap(px(7.))
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(muted)
            .child(ui::icon_small(IconName::Layers).flex_none())
            .child(name)
            .children(machine.map(chip))
            .when(current, |el| el.child(chip(tr("当前").into())))
            .into_any_element()
    }

    fn render_filter(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, on_bg, fg, muted) = (
            theme.secondary,
            theme.background,
            theme.foreground,
            theme.muted_foreground,
        );
        let all = self.board.read(cx).groups(cx);
        let count = |state: Option<TermState>| {
            all.iter()
                .flat_map(|group| &group.rows)
                .filter(|row| state.is_none_or(|state| row.state == state))
                .count()
        };
        let items = [
            (Filter::All, "overview-all", tr("全部"), count(None), ""),
            (
                Filter::Stopped,
                "overview-stopped",
                tr("已停下"),
                count(Some(TermState::Stopped)),
                tr("超过 10 秒没有新输出，且停下后尚未查看"),
            ),
            (
                Filter::Running,
                "overview-running",
                tr("运行中"),
                count(Some(TermState::Running)),
                tr("10 秒内有新输出"),
            ),
        ];
        h_flex()
            .flex_none()
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(7.))
            .bg(bg)
            .children(items.into_iter().map(|(filter, id, label, count, tip)| {
                let on = self.filter == filter;
                h_flex()
                    .id(id)
                    .h(px(22.))
                    .px(px(9.))
                    .gap(px(4.))
                    .rounded(px(5.))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .text_color(if on { fg } else { muted })
                    .when(on, |el| el.bg(on_bg))
                    .child(label)
                    .child(div().text_color(muted).child(count.to_string()))
                    .when(!tip.is_empty(), |el| {
                        el.tooltip(move |window, cx| ui::tip(tip, window, cx))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.set_filter(filter, cx)))
            }))
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (panel, border, muted) = (theme.sidebar, theme.border, theme.muted_foreground);
        let groups = self.groups(cx);
        let empty = groups.is_empty();
        let mut items: Vec<AnyElement> = Vec::new();
        let mut index = 0;
        for group in &groups {
            items.push(self.render_group_head(group.workspace.as_ref(), cx));
            for row in &group.rows {
                items.push(self.render_row(index, row, cx));
                index += 1;
            }
        }
        // 没连着的服务器上的终端不知道，也不为了看它去连：说一声
        let offline: Vec<String> = {
            let (app, board) = (self.app.read(cx), self.board.read(cx));
            let servers = app.settings.servers.iter();
            servers
                .filter(|server| !board.watching(&server.id))
                .map(|server| server.name.clone())
                .collect()
        };
        let offline = (!offline.is_empty()).then(|| {
            tf!(
                "未连接的服务器：{names}。连接后显示其终端。",
                names = offline.join(tr("、"))
            )
        });
        let hint = |keys: &'static str, text: &'static str| {
            h_flex()
                .gap(px(5.))
                .child(div().font_weight(FontWeight::MEDIUM).child(keys))
                .child(text)
        };
        v_flex()
            .w(px(LIST_WIDTH))
            .flex_none()
            .h_full()
            .bg(panel)
            .border_r_1()
            .border_color(border)
            .child(
                h_flex()
                    .h(px(HEAD_HEIGHT))
                    .flex_none()
                    .pl(px(16.))
                    .pr(px(12.))
                    .gap(px(10.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(trc("页面", "终端")),
                    )
                    .child(div().flex_1())
                    .child(self.render_filter(cx)),
            )
            .child(
                v_flex()
                    .id("overview-rows")
                    .flex_1()
                    .min_h_0()
                    .pb(px(10.))
                    .overflow_y_scroll()
                    .children(items)
                    .when(empty, |list| {
                        list.child(
                            div()
                                .px(px(16.))
                                .py(px(28.))
                                .text_size(px(13.))
                                .text_color(muted)
                                .text_center()
                                .child(match self.filter {
                                    Filter::All => tr("没有打开的终端"),
                                    Filter::Stopped => tr("没有已停下的终端"),
                                    Filter::Running => tr("没有运行中的终端"),
                                }),
                        )
                    })
                    .children(offline.map(|text| {
                        div()
                            .px(px(18.))
                            .pt(px(14.))
                            .text_size(px(12.))
                            .text_color(muted)
                            .child(text)
                    })),
            )
            .child(
                h_flex()
                    .h(px(32.))
                    .flex_none()
                    .px(px(16.))
                    .gap(px(14.))
                    .border_t_1()
                    .border_color(border)
                    .text_size(px(12.))
                    .text_color(muted)
                    .child(hint("↑ ↓", tr("选择")))
                    .child(hint("Enter", tr("前往")))
                    .child(div().flex_1())
                    .child(tr("已停下：超过 10 秒没有新输出")),
            )
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, border, muted, fg) = (
            theme.background,
            theme.border,
            theme.muted_foreground,
            theme.foreground,
        );
        let row = self
            .selected
            .as_ref()
            .and_then(|id| self.board.read(cx).row(id, cx));
        let body = match (&row, &self.peek) {
            (Some(_), Some((_, peek))) => div()
                .flex_1()
                .min_h_0()
                .child(peek.clone())
                .into_any_element(),
            _ => v_flex()
                .flex_1()
                .min_h_0()
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(muted)
                .child(Icon::new(IconName::SquareTerminal).size(px(28.)))
                .child(div().text_size(px(13.)).child(tr("选择左侧的终端查看画面")))
                .into_any_element(),
        };
        let head = row.as_ref().map(|row| {
            let app = self.app.read(cx);
            let workspace = self
                .board
                .read(cx)
                .groups(cx)
                .into_iter()
                .find(|group| group.rows.iter().any(|other| other.id == row.id))
                .and_then(|group| group.workspace);
            let mut place = format!("{} · {}", app.repo_name(&row.entry), Self::place(app, row));
            if let Some(key) = &workspace {
                place.push_str(" · ");
                place.push_str(&tf!("工作区「{name}」", name = app.workspace_name(key)));
            }
            let since = row
                .last_output
                .filter(|_| row.state == TermState::Stopped)
                .map(|at| format!(" · {}", ago(at)))
                .unwrap_or_default();
            let id = row.id.clone();
            h_flex()
                .h(px(HEAD_HEIGHT))
                .flex_none()
                .pl(px(16.))
                .pr(px(12.))
                .gap(px(10.))
                .border_b_1()
                .border_color(border)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap(px(8.))
                                .child(
                                    ui::clipped("overview-title", Self::name(row))
                                        .flex_shrink(1.)
                                        .text_size(px(14.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(fg),
                                )
                                .child(Self::dot(row.state, cx))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(px(12.))
                                        .text_color(muted)
                                        .child(format!("{}{since}", Self::state_text(row))),
                                ),
                        )
                        .child(
                            ui::clipped("overview-place", place)
                                .text_size(px(12.))
                                .text_color(muted),
                        ),
                )
                .child(
                    Button::new("overview-go")
                        .primary()
                        .small()
                        .label(tr("前往"))
                        .tooltip(tr("切换到此终端所在的工作区和仓库（Enter）"))
                        .on_click(cx.listener(move |this, _, _, cx| this.go(&id, cx))),
                )
        });
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(bg)
            .children(head)
            .child(body)
            .when(row.is_some(), |el| {
                el.child(
                    h_flex()
                        .h(px(32.))
                        .flex_none()
                        .px(px(16.))
                        .gap(px(8.))
                        .border_t_1()
                        .border_color(border)
                        .text_size(px(12.))
                        .text_color(muted)
                        .child(ui::icon_small(IconName::Eye).flex_none())
                        .child(tr("只读预览，画面实时更新。前往后可输入。")),
                )
            })
    }
}

impl Render for TermOverview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("term-overview")
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .child(self.render_list(cx))
            .child(self.render_preview(cx))
    }
}
