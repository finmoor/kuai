//! 标题栏「任务」页：左边按主机列出任务，右边是选中的那个的详情和运行记录（原型见
//! `docs/design/scheduled-tasks/design-demos/scheduled-tasks.html`，方向 A）。
//!
//! 时间都是任务所在主机的时间：主机名后面、详情的标题下面写明是哪台的时间、哪个时区；和本机
//! 时区不同时「下次运行」另写一遍本机时间。常驻进程不在这里配置：主机名右边只写一句这台上的任务
//! 到点会不会运行，主机重启后不会运行时是黄色的，点了去主机页。

use fr_core::i18n::{tr, trc};
use fr_core::schedule::{Schedule, Wall};
use fr_core::task::PromptVia;
use fr_core::tasks::{Outcome, Run, Task as TaskDef, TaskOp, Trigger};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{host_now, local_now, text};
use crate::app_state::AppState;
use crate::machines::{LOCAL, own_entry};
use crate::settings::RepoEntry;
use crate::term_board::{TermBoard, TermId, TermState};
use crate::ui;

/// 左边顶上那一条的高度（和主机页、会话页一样）
const HEAD_HEIGHT: f32 = 52.;
/// 隔这么久重画一遍：「21 小时后」「现在 12:14」要跟着走
const TICK: std::time::Duration = std::time::Duration::from_secs(20);

pub enum TaskPageEvent {
    /// 到会话页看这个终端
    OpenTerminal(TermId),
    /// 到主机页看这台（设开机时启动）
    ShowHost(String),
    /// 在标签页中打开（工作树里有改动的那一次）
    OpenRepo(RepoEntry),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Enabled,
    Paused,
}

pub struct TaskPage {
    app: Entity<AppState>,
    board: Entity<TermBoard>,
    filter: Filter,
    /// 选中的：哪台主机上的哪个任务
    selected: Option<(String, String)>,
    tick: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<TaskPageEvent> for TaskPage {}

/// 一次运行现在怎么样（运行记录的「状态」一栏）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunState {
    Running,
    Waiting,
    /// 看过最近一次输出以后的样子了
    Seen,
    /// 还没看过，程序没有输出：普通命令不提醒，AI 助手还没到所设的时长（和会话页的「静默」一样）
    Quiet,
    /// 还没看过，回到了 shell 的提示符（程序跑完退出了）
    Idle,
    /// 主机没连着，不知道
    Unknown,
    Ended,
    Skipped,
    Failed,
}

impl RunState {
    pub fn label(self) -> &'static str {
        match self {
            RunState::Running => tr("运行中"),
            RunState::Waiting => tr("待查看"),
            RunState::Seen => tr("已查看"),
            RunState::Quiet => tr("静默"),
            RunState::Idle => tr("空闲"),
            RunState::Unknown => tr("状态未知"),
            RunState::Ended => tr("已结束"),
            RunState::Skipped => tr("已跳过"),
            RunState::Failed => tr("启动失败"),
        }
    }

    fn color(self, cx: &App) -> Hsla {
        let theme = cx.theme();
        match self {
            RunState::Running => theme.primary,
            RunState::Waiting => theme.warning,
            RunState::Seen => theme.success,
            RunState::Failed => theme.danger,
            _ => theme.muted_foreground,
        }
    }
}

/// 主机 `machine` 上的这次运行现在怎么样。`live`：那台连着。
pub fn run_state(run: &Run, machine: &str, live: bool, board: &TermBoard) -> RunState {
    match run.outcome {
        Outcome::Skipped => return RunState::Skipped,
        Outcome::Failed => return RunState::Failed,
        Outcome::Started => {}
    }
    if run.ended.is_some() {
        return RunState::Ended;
    }
    let row = run.term.as_ref().and_then(|term| {
        board.row(&TermId {
            machine: machine.to_string(),
            term: term.clone(),
        })
    });
    match row {
        Some(row) => match row.state {
            TermState::Running => RunState::Running,
            TermState::Waiting => RunState::Waiting,
            TermState::Unknown => RunState::Unknown,
            _ if row.away => RunState::Waiting,
            _ if row.seen => RunState::Seen,
            TermState::Quiet => RunState::Quiet,
            TermState::Idle => RunState::Idle,
        },
        None if !live => RunState::Unknown,
        // 会话已经没了，kuaid 下一次看的时候记上结束
        None => RunState::Ended,
    }
}

/// 下一次运行（主机的时间）；暂停了、不会再运行的是 None。
pub fn next_run(task: &TaskDef, now: &Wall) -> Option<Wall> {
    if !task.enabled {
        return None;
    }
    task.schedule.next_after(now)
}

impl TaskPage {
    pub fn new(app: Entity<AppState>, board: Entity<TermBoard>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&app, |_, _, cx| cx.notify()),
            cx.observe(&board, |_, _, cx| cx.notify()),
        ];
        Self {
            app,
            board,
            filter: Filter::All,
            selected: None,
            tick: None,
            _subscriptions: subscriptions,
        }
    }

    /// 显示出来了：问一遍各台的常驻进程（主机名右边那句话要），隔一会儿重画一遍。
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            for key in app.task_hosts() {
                app.refresh_daemon(&key, cx);
            }
        });
        self.tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        }));
    }

    pub fn hidden(&mut self) {
        self.tick = None;
    }

    /// 选中主机 `machine` 上的任务 `id`（会话页的「查看任务」）。
    pub fn select(&mut self, machine: &str, id: &str, cx: &mut Context<Self>) {
        self.selected = Some((machine.to_string(), id.to_string()));
        self.filter = Filter::All;
        cx.notify();
    }

    /// 「新建任务」：放在选中的任务所在的那台，没有就是第一台连着的。
    pub fn new_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let machine = self.selected.as_ref().map(|(machine, _)| machine.clone());
        let page = cx.entity().downgrade();
        super::editor::open(
            self.app.clone(),
            super::editor::Start::New {
                machine,
                repo: None,
                preset: None,
                prompt: String::new(),
            },
            Box::new(move |machine, id, cx| {
                page.update(cx, |page, cx| page.select(&machine, &id, cx))
                    .ok();
            }),
            window,
            cx,
        );
    }

    fn edit(
        &mut self,
        machine: String,
        task: TaskDef,
        copy: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = cx.entity().downgrade();
        let start = match copy {
            true => super::editor::Start::Copy { machine, task },
            false => super::editor::Start::Edit { machine, task },
        };
        super::editor::open(
            self.app.clone(),
            start,
            Box::new(move |machine, id, cx| {
                page.update(cx, |page, cx| page.select(&machine, &id, cx))
                    .ok();
            }),
            window,
            cx,
        );
    }

    fn remove(
        &mut self,
        machine: String,
        task: TaskDef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app = self.app.clone();
        let page = cx.entity().downgrade();
        crate::manager::confirm(
            tf!("删除定时任务「{name}」？", name = task.name.as_str()),
            tr("运行记录一起删除，已经打开的会话不受影响。").to_string(),
            tr("删除"),
            true,
            window,
            cx,
            move |cx| {
                let ops = vec![TaskOp::Remove {
                    id: task.id.clone(),
                }];
                let done = app.update(cx, |app, cx| app.apply_tasks(&machine, ops, cx));
                let page = page.clone();
                cx.spawn(async move |cx| {
                    if done.await.is_some() {
                        page.update(cx, |page, cx| {
                            page.selected = None;
                            cx.notify();
                        })
                        .ok();
                    }
                })
                .detach();
            },
        );
    }

    fn set_enabled(&mut self, machine: &str, id: &str, enabled: bool, cx: &mut Context<Self>) {
        let ops = vec![TaskOp::Enable {
            id: id.to_string(),
            enabled,
        }];
        self.app
            .update(cx, |app, cx| app.apply_tasks(machine, ops, cx))
            .detach();
    }

    /// 查看一次运行结束时存下的画面。
    fn show_screen(
        &mut self,
        machine: String,
        run: Run,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(fetch) = self.app.read(cx).task_screen(&machine, &run.id) else {
            return;
        };
        let lines: Entity<Option<Result<Vec<String>, String>>> = cx.new(|_| None);
        let fill = lines.clone();
        cx.spawn(async move |_, cx| {
            let result = fetch.await;
            fill.update(cx, |lines, cx| {
                *lines = Some(match result {
                    Ok(bytes) => Ok(screen_lines(&bytes)),
                    Err(err) => Err(format!("{err:#}")),
                });
                cx.notify();
            });
        })
        .detach();
        ui::cancel_pending_tip(window, cx);
        window.open_dialog(cx, move |dialog, _, cx| {
            let theme = cx.theme();
            let (muted, danger, border) = (theme.muted_foreground, theme.danger, theme.border);
            let body: AnyElement =
                match lines.read(cx) {
                    None => div()
                        .py(px(24.))
                        .text_sm()
                        .text_color(muted)
                        .child(tr("正在读取…"))
                        .into_any_element(),
                    Some(Err(err)) => div()
                        .text_sm()
                        .text_color(danger)
                        .child(err.clone())
                        .into_any_element(),
                    // 和终端里一样一行就是一行，不折行：任务终端开出来是 120 列，对话框留够了宽度，
                    // 有的字（⏵、⚠）比一格宽也放得下；窗口窄、对话框跟着变窄时左右滚动看，不折成
                    // 两行，也不截掉。里面那一层按最宽的一行排宽，外面才知道能往右滚多远
                    Some(Ok(lines)) => div()
                        .id("task-screen")
                        .flex()
                        .flex_col()
                        .items_start()
                        .max_h(px(460.))
                        .overflow_scroll()
                        .p(px(10.))
                        .rounded(px(ui::RADIUS))
                        .border_1()
                        .border_color(border)
                        .font_family(crate::fonts::mono())
                        .text_xs()
                        .child(v_flex().flex_none().min_w_full().children(lines.iter().map(
                            |line| {
                                div()
                                    .flex_none()
                                    .min_h(px(16.))
                                    .whitespace_nowrap()
                                    .child(line.clone())
                            },
                        )))
                        .into_any_element(),
                };
            dialog.title(title.clone()).w(px(1000.)).child(body)
        });
    }

    // ---- 画 ----

    fn render_side(
        &self,
        width: f32,
        tasks: &[(String, TaskDef)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (panel, border, muted, accent, secondary, background, head_fg) = (
            theme.sidebar,
            theme.sidebar_border,
            theme.muted_foreground,
            theme.primary,
            theme.secondary,
            theme.background,
            theme.sidebar_foreground,
        );
        let total = tasks.len();
        let enabled = tasks.iter().filter(|(_, task)| task.enabled).count();
        let (running, can_add) = {
            let app = self.app.read(cx);
            let board = self.board.read(cx);
            let running = tasks
                .iter()
                .filter(|(machine, task)| {
                    app.task_book(machine).is_some_and(|book| {
                        book.file.runs_of(&task.id).any(|run| {
                            run_state(run, machine, book.live, board) == RunState::Running
                        })
                    })
                })
                .count();
            (running, !app.task_hosts().is_empty())
        };
        let summary = match running {
            0 => tf!("{n} 个任务", n = total),
            _ => tf!("{n} 个任务，{m} 个正在运行", n = total, m = running),
        };
        let filter_button =
            |id: &'static str, filter: Filter, label: String, cx: &Context<Self>| {
                let on = self.filter == filter;
                div()
                    .id(id)
                    .flex_1()
                    .h(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.))
                    .text_xs()
                    .cursor_pointer()
                    .when(on, |b| {
                        b.bg(background).shadow_xs().font_weight(FontWeight::MEDIUM)
                    })
                    .when(!on, |b| b.text_color(muted))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.filter = filter;
                        cx.notify();
                    }))
            };
        let filters = h_flex()
            .mx(px(12.))
            .mt(px(10.))
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(ui::RADIUS))
            .bg(secondary)
            .child(filter_button(
                "task-filter-all",
                Filter::All,
                tf!("全部 {n}", n = total),
                cx,
            ))
            .child(filter_button(
                "task-filter-enabled",
                Filter::Enabled,
                tf!("已启用 {n}", n = enabled),
                cx,
            ))
            .child(filter_button(
                "task-filter-paused",
                Filter::Paused,
                tf!("已暂停 {n}", n = total - enabled),
                cx,
            ));
        let mut list = v_flex()
            .id("task-rows")
            .flex_1()
            .min_h_0()
            .pt(px(8.))
            .pb(px(10.))
            .gap(px(2.))
            .overflow_y_scroll();
        let machines = self.app.read(cx).task_machines();
        let mut ix = 0;
        for machine in machines {
            let shown: Vec<&TaskDef> = tasks
                .iter()
                .filter(|(m, _)| *m == machine)
                .map(|(_, task)| task)
                .filter(|task| match self.filter {
                    Filter::All => true,
                    Filter::Enabled => task.enabled,
                    Filter::Paused => !task.enabled,
                })
                .collect();
            if shown.is_empty() {
                continue;
            }
            list = list.child(self.render_host_head(&machine, cx));
            for task in shown {
                list = list.child(self.render_task_row(ix, &machine, task, cx));
                ix += 1;
            }
        }
        if ix == 0 && total > 0 {
            list = list.child(
                div()
                    .px(px(16.))
                    .py(px(12.))
                    .text_xs()
                    .text_color(muted)
                    .child(match self.filter {
                        Filter::Paused => tr("没有暂停的任务"),
                        _ => tr("没有启用的任务"),
                    }),
            );
        }
        v_flex()
            .w(px(width))
            .flex_none()
            .relative()
            .h_full()
            .bg(panel)
            .border_r_1()
            .border_color(border)
            .child(
                h_flex()
                    .h(px(HEAD_HEIGHT))
                    .flex_none()
                    .px(px(12.))
                    .gap(px(10.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .flex_none()
                            .size(px(28.))
                            .rounded(px(7.))
                            .bg(accent.opacity(0.14))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(ui::icon(IconName::Clock).text_color(accent)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                ui::clipped("tasks-title", trc("页面", "任务"))
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                ui::clipped("tasks-summary", summary)
                                    .text_size(px(11.))
                                    .text_color(muted),
                            ),
                    )
                    // 和会话页一样只放一个加号：侧栏窄，带字的按钮把「几个任务」挤没了
                    .child(
                        div()
                            .id("task-new")
                            .flex_none()
                            .size(px(26.))
                            .rounded(px(6.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(muted)
                            .when(can_add, |b| {
                                b.cursor_pointer()
                                    .hover(|s| s.bg(secondary).text_color(head_fg))
                                    .on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.new_task(window, cx)
                                        }),
                                    )
                            })
                            .when(!can_add, |b| b.opacity(0.5))
                            .child(ui::icon(IconName::Plus))
                            .tooltip(move |window, cx| {
                                let tip = match can_add {
                                    true => tr("新建任务"),
                                    false => tr("连接主机后才能新建任务。"),
                                };
                                ui::tip(tip, window, cx)
                            }),
                    ),
            )
            .when(total > 0, |side| side.child(filters))
            .child(list)
            .child(crate::side_split::handle("tasks-side-split", &self.app))
            .into_any_element()
    }

    /// 主机这一组的标题：名字、时区，右边是这台上的任务到点会不会运行。
    fn render_host_head(&self, machine: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, warning, success, secondary) = (
            theme.muted_foreground,
            theme.warning,
            theme.success,
            theme.secondary,
        );
        let app = self.app.read(cx);
        let name = app.machine_name(machine);
        let book = app.task_book(machine);
        let zone = book
            .and_then(|book| book.file.clock.as_ref())
            .map(|clock| clock.offset_text());
        let live = book.is_some_and(|book| book.live);
        let scheduled = book.is_some_and(|book| book.file.has_scheduled());
        // 到点会不会运行
        let (note, color, go): (Option<&'static str>, Hsla, bool) = if !live {
            (Some(tr("未连接")), muted, false)
        } else if machine == LOCAL {
            match crate::autostart::enabled() {
                true => (Some(tr("登录后运行")), success, false),
                false => (Some(tr("Kuai 启动后才运行")), warning, false),
            }
        } else {
            match app.daemon(machine).map(|daemon| daemon.status.autostart) {
                Some(Some(true)) => (Some(tr("随时可运行")), success, false),
                Some(_) if scheduled => (Some(tr("主机重启后不运行")), warning, true),
                _ => (None, muted, false),
            }
        };
        let target = machine.to_string();
        // 两行：名字和时区；下面一行是到点会不会运行。挤在一行时窄侧栏里名字只剩一个字
        v_flex()
            .id(SharedString::from(format!("task-host-{machine}")))
            .mx(px(6.))
            .mt(px(8.))
            .mb(px(2.))
            .px(px(8.))
            .gap(px(1.))
            .text_xs()
            .child(
                h_flex()
                    .h(px(22.))
                    .gap(px(6.))
                    .child(
                        Icon::new(if machine == LOCAL {
                            IconName::Monitor
                        } else {
                            IconName::Server
                        })
                        .size(px(13.))
                        .text_color(muted),
                    )
                    .child(
                        ui::clipped(
                            SharedString::from(format!("task-host-name-{machine}")),
                            name,
                        )
                        .font_weight(FontWeight::SEMIBOLD),
                    )
                    .children(zone.map(|zone| {
                        div()
                            .flex_none()
                            .px(px(5.))
                            .rounded(px(4.))
                            .bg(secondary)
                            .text_size(px(10.5))
                            .text_color(muted)
                            .child(zone)
                    })),
            )
            .children(note.map(|note| {
                h_flex()
                    .id(SharedString::from(format!("task-host-note-{machine}")))
                    .min_w_0()
                    .pl(px(19.))
                    .gap(px(5.))
                    .text_color(color)
                    .child(div().flex_none().size(px(6.)).rounded_full().bg(color))
                    // 侧栏拖窄时截断：能点的那条提示是去哪里，别的悬停看全文
                    .map(|n| match go {
                        true => n.child(div().min_w_0().truncate().child(note)),
                        false => n.child(ui::clipped(
                            SharedString::from(format!("task-host-note-text-{machine}")),
                            note,
                        )),
                    })
                    .when(go, |n| {
                        n.cursor_pointer()
                            .hover(|s| s.underline())
                            .tooltip(|window, cx| ui::tip(tr("在主机页设置开机时启动"), window, cx))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(TaskPageEvent::ShowHost(target.clone()))
                            }))
                    })
            }))
            .into_any_element()
    }

    fn render_task_row(
        &self,
        ix: usize,
        machine: &str,
        task: &TaskDef,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (selected_bg, hover, muted, fg) = (
            theme.sidebar_accent,
            theme.list_hover,
            theme.muted_foreground,
            theme.sidebar_foreground,
        );
        let on = self
            .selected
            .as_ref()
            .is_some_and(|(m, id)| m == machine && *id == task.id);
        let app = self.app.read(cx);
        let book = app.task_book(machine);
        let live = book.is_some_and(|book| book.live);
        let windows = app.machine(machine).is_some_and(|m| m.windows);
        let repo = app.repo_name(&own_entry(machine, windows, &task.repo));
        let line = format!("{} · {repo}", text::schedule(&task.schedule));
        // 右边：在运行、待查看的写状态，暂停的写「已暂停」，别的写下次运行在多久以后
        let board = self.board.read(cx);
        let latest = book.and_then(|book| {
            book.file
                .runs_of(&task.id)
                .map(|run| run_state(run, machine, live, board))
                .find(|state| matches!(state, RunState::Running | RunState::Waiting))
        });
        let now = book.and_then(|book| book.file.clock.as_ref()).map(host_now);
        let (right, color): (Option<String>, Hsla) = match latest {
            Some(state) => (Some(state.label().to_string()), state.color(cx)),
            None if !task.enabled => (Some(tr("已暂停").to_string()), muted),
            None => (
                now.and_then(|now| next_run(task, &now).map(|next| text::until(&next, &now))),
                muted,
            ),
        };
        let icon =
            crate::presets::icon(&task.preset.icon)
                .size_4()
                .text_color(crate::presets::color(
                    &task.preset.icon,
                    task.preset.hue,
                    cx,
                ));
        let (target_machine, target_id) = (machine.to_string(), task.id.clone());
        h_flex()
            .id(("task-row", ix))
            .min_h(px(44.))
            .mx(px(6.))
            .px(px(10.))
            .gap(px(10.))
            .rounded(px(ui::RADIUS))
            .cursor_pointer()
            .text_color(fg)
            .when(on, |row| row.bg(selected_bg))
            .when(!on, |row| row.hover(|s| s.bg(hover)))
            .when(!task.enabled, |row| row.opacity(0.7))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = Some((target_machine.clone(), target_id.clone()));
                cx.notify();
            }))
            .child(div().flex_none().child(icon))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        ui::clipped(("task-name", ix), task.name.clone())
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM),
                    )
                    .child(
                        ui::clipped(("task-line", ix), line)
                            .text_xs()
                            .text_color(muted),
                    ),
            )
            .children(right.map(|right| {
                h_flex()
                    .flex_none()
                    .gap(px(4.))
                    .text_xs()
                    .text_color(color)
                    .when(latest.is_some(), |r| {
                        r.child(div().size(px(6.)).rounded_full().bg(color))
                    })
                    .child(right)
            }))
            .into_any_element()
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, fg) = (theme.muted_foreground, theme.foreground);
        let can_add = !self.app.read(cx).task_hosts().is_empty();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(px(10.))
            .px(px(32.))
            .text_color(muted)
            .child(Icon::new(IconName::Clock).size_8())
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child(tr("还没有定时任务")),
            )
            .child(
                div()
                    .max_w(px(460.))
                    .text_sm()
                    .text_center()
                    .whitespace_normal()
                    .child(tr(
                        "到点时在主机上打开终端，运行所选的程序并发出提示词，例如每天早上评审前一天的提交。由主机上的常驻进程按时运行，不需要开着 Kuai。",
                    )),
            )
            .child(
                Button::new("task-new-empty")
                    .primary()
                    .icon(ui::icon(IconName::Plus))
                    .label(tr("新建任务"))
                    .disabled(!can_add)
                    .on_click(cx.listener(|this, _, window, cx| this.new_task(window, cx))),
            )
            .when(!can_add, |col| {
                col.child(div().text_xs().child(tr("连接主机后才能新建任务。")))
            })
            .into_any_element()
    }

    fn render_detail(
        &self,
        machine: &str,
        task: &TaskDef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, accent, warning, secondary, fg) = (
            theme.muted_foreground,
            theme.border,
            theme.primary,
            theme.warning,
            theme.secondary,
            theme.foreground,
        );
        let app = self.app.read(cx);
        let book = app.task_book(machine);
        let live = book.is_some_and(|book| book.live);
        let clock = book.and_then(|book| book.file.clock.clone());
        let host = app.machine_name(machine);
        let windows = app.machine(machine).is_some_and(|m| m.windows);
        let entry = own_entry(machine, windows, &task.repo);
        let repo = app.repo_name(&entry);
        // 标题下面那一行：什么时候运行（哪台的时间），下次在什么时候
        let mut when = text::schedule(&task.schedule);
        if let Some(clock) = &clock
            && !matches!(task.schedule, Schedule::Manual)
        {
            when = match machine == LOCAL {
                true => tf!(
                    "{when}（本机时间，{zone}）",
                    when,
                    zone = clock.offset_text()
                ),
                false => tf!(
                    "{when}（{host} 时间，{zone}）",
                    when,
                    host = host.to_string(),
                    zone = clock.offset_text()
                ),
            };
        }
        let next: Option<String> = clock.as_ref().and_then(|clock| {
            let now = host_now(clock);
            let next = next_run(task, &now)?;
            let mut text = tf!(
                "下次运行 {when}（{until}",
                when = text::when(&next, &now),
                until = text::until(&next, &now)
            );
            let (local, offset) = local_now();
            if offset != clock.offset {
                let local_next = local.plus_minutes(next.minutes() - now.minutes());
                text.push_str(&tf!(
                    "，本机时间 {when}",
                    when = text::when(&local_next, &local)
                ));
            }
            text.push_str(tr("）"));
            Some(text)
        });
        let state_line = match (task.enabled, &task.schedule, &next) {
            (false, _, _) => Some(tr("已暂停").to_string()),
            (_, Schedule::Manual, _) => None,
            (_, _, None) => Some(tr("不会再运行").to_string()),
            _ => next.clone(),
        };
        let icon_name = task.preset.icon.clone();
        let (m1, m2, m3, m4, m5) = (
            machine.to_string(),
            machine.to_string(),
            machine.to_string(),
            machine.to_string(),
            machine.to_string(),
        );
        let (t1, t2, t3) = (task.clone(), task.clone(), task.clone());
        let id_run = task.id.clone();
        let id_enable = task.id.clone();
        let enabled = task.enabled;
        let head = h_flex()
            .items_start()
            .gap(px(12.))
            .child(
                div()
                    .flex_none()
                    .size(px(40.))
                    .rounded(px(9.))
                    .bg(secondary)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        crate::presets::icon(&icon_name)
                            .size_5()
                            .text_color(crate::presets::color(&icon_name, task.preset.hue, cx)),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(4.))
                    .child(
                        ui::clipped("task-title", task.name.clone())
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    // 窗口窄、放不下一行时「下次运行」换到下一行，自己一行还放不下就折行，不截断；所以
                    // 中间不用「·」（换行后会落在第二行开头），靠空隙和颜色分开
                    .child(
                        h_flex()
                            .min_w_0()
                            .flex_wrap()
                            .gap_x(px(14.))
                            .gap_y(px(2.))
                            .text_xs()
                            .text_color(muted)
                            .child(div().min_w_0().whitespace_normal().child(when))
                            .children(state_line.map(|line| {
                                div()
                                    .min_w_0()
                                    .whitespace_normal()
                                    .text_color(fg)
                                    .child(line)
                            })),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(8.))
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .text_xs()
                            .text_color(muted)
                            .child(match enabled {
                                true => tr("已启用"),
                                false => tr("已暂停"),
                            })
                            .child(
                                Switch::new("task-enabled")
                                    .accessibility_label(tr("启用任务"))
                                    .small()
                                    .checked(enabled)
                                    .disabled(!live)
                                    .on_click(cx.listener(move |this, on: &bool, _, cx| {
                                        this.set_enabled(&m1, &id_enable, *on, cx)
                                    })),
                            ),
                    )
                    .child(
                        Button::new("task-run")
                            .small()
                            .icon(ui::icon(IconName::Play))
                            .label(tr("立即运行"))
                            .disabled(!live)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.app
                                    .update(cx, |app, cx| app.run_task(&m2, &id_run, cx))
                            })),
                    )
                    .child(
                        Button::new("task-edit")
                            .small()
                            .icon(ui::icon(IconName::Pencil))
                            .label(tr("编辑"))
                            .disabled(!live)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit(m3.clone(), t1.clone(), false, window, cx)
                            })),
                    )
                    .child({
                        let page = cx.entity().downgrade();
                        Button::new("task-more")
                            .small()
                            .ghost()
                            .icon(ui::icon(IconName::Ellipsis))
                            .disabled(!live)
                            .dropdown_menu(move |menu, _, _| {
                                let (copy_page, remove_page) = (page.clone(), page.clone());
                                let (m4, m5, t2, t3) =
                                    (m4.clone(), m5.clone(), t2.clone(), t3.clone());
                                menu.item(PopupMenuItem::new(tr("复制为新任务…")).on_click(
                                    move |_, window, cx| {
                                        copy_page
                                            .update(cx, |page, cx| {
                                                page.edit(m4.clone(), t2.clone(), true, window, cx)
                                            })
                                            .ok();
                                    },
                                ))
                                .separator()
                                .item(
                                    PopupMenuItem::new(tr("删除…")).on_click(
                                        move |_, window, cx| {
                                            remove_page
                                                .update(cx, |page, cx| {
                                                    page.remove(m5.clone(), t3.clone(), window, cx)
                                                })
                                                .ok();
                                        },
                                    ),
                                )
                            })
                    }),
            );
        // 位置、终端预设、模型、思考深度、需要确认时
        // 各格按内容的宽度排、再平分剩下的宽度，一行放不下就换行：主机名、仓库名、模型名长的时候
        // 不被等分的格子截断（只有一格独占一行还放不下时才截断）
        let cell = |label: &'static str, value: AnyElement| {
            v_flex()
                .flex_auto()
                .min_w(px(120.))
                .gap(px(3.))
                .px(px(14.))
                .py(px(10.))
                .child(div().text_xs().text_color(muted).child(label))
                .child(value)
        };
        let value = |id: &'static str, text: SharedString| {
            ui::clipped(id, text).text_sm().into_any_element()
        };
        let location = h_flex()
            .min_w_0()
            .gap(px(6.))
            .child(
                ui::clipped("task-repo", format!("{host} / {repo}"))
                    .flex_shrink(1.)
                    .text_sm(),
            )
            .when(task.worktree.is_some(), |row| {
                row.child(
                    h_flex()
                        .flex_none()
                        .gap(px(3.))
                        .px(px(6.))
                        .rounded_full()
                        .border_1()
                        .border_color(border)
                        .text_xs()
                        .text_color(muted)
                        .child(Icon::new(IconName::GitBranch).size(px(11.)))
                        .child(tr("新工作树")),
                )
            })
            .into_any_element();
        let preset = h_flex()
            .min_w_0()
            .gap(px(6.))
            .child(
                crate::presets::icon(&icon_name)
                    .size(px(14.))
                    .text_color(crate::presets::color(&icon_name, task.preset.hue, cx)),
            )
            .child(ui::clipped("task-preset", task.preset.name.clone()).text_sm())
            .into_any_element();
        let model = match task.model.is_empty() {
            true => value("task-model", tr("按程序设置").into()),
            false => ui::clipped("task-model", task.model.clone())
                .text_sm()
                .font_family(crate::fonts::mono())
                .into_any_element(),
        };
        let line = task.launch.line(fr_core::task::Shell::Posix);
        let command_row = h_flex()
            .min_w_0()
            .gap(px(8.))
            .px(px(14.))
            .py(px(9.))
            .border_t_1()
            .border_color(border)
            .text_xs()
            .child(div().flex_none().text_color(muted).child(tr("将运行")))
            .child(
                ui::clipped_with_tip("task-command", task.launch.command.clone(), line)
                    .flex_shrink(1.)
                    .font_family(crate::fonts::mono()),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(6.))
                    .rounded(px(4.))
                    .bg(accent.opacity(0.12))
                    .text_color(accent)
                    .child(match task.launch.prompt {
                        PromptVia::Arg => tr("提示词"),
                        PromptVia::Paste => tr("提示词作为第一条消息粘贴"),
                    }),
            )
            .when(task.launch.exits, |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_color(muted)
                        .child(tr("运行结束即退出")),
                )
            });
        let info = v_flex()
            .rounded(px(8.))
            .border_1()
            .border_color(border)
            .bg(secondary.opacity(0.5))
            .child(
                h_flex()
                    .flex_wrap()
                    .child(cell(tr("位置"), location))
                    .child(cell(tr("终端预设"), preset))
                    .child(cell(tr("模型"), model))
                    .child(cell(
                        tr("思考深度"),
                        value("task-effort", text::effort(&task.effort).into()),
                    ))
                    .child(cell(
                        tr("需要确认时"),
                        value("task-confirm", text::confirm(task.confirm).into()),
                    )),
            )
            .child(command_row);
        let prompt = v_flex()
            .gap(px(6.))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("提示词")),
            )
            .child(
                div()
                    .px(px(14.))
                    .py(px(10.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(border)
                    .text_sm()
                    .whitespace_normal()
                    .child(task.prompt.clone()),
            );
        // 提醒：没连着；主机重启后不会运行
        let mut notes: Vec<AnyElement> = Vec::new();
        if !live {
            notes.push(note_bar(muted, super::offline_text().into(), None, cx));
        } else if task.enabled && !matches!(task.schedule, Schedule::Manual) {
            let autostart_off = match machine == LOCAL {
                true => !crate::autostart::enabled(),
                false => app
                    .daemon(machine)
                    .is_some_and(|daemon| daemon.status.autostart != Some(true)),
            };
            if autostart_off {
                let (text, link): (String, Option<(&'static str, TaskPageEvent)>) =
                    match machine == LOCAL {
                        true => (
                            tr("本机重启后，要等 Kuai 启动以后任务才会运行。可在设置中打开「开机时自动启动」。")
                                .to_string(),
                            None,
                        ),
                        false => (
                            tf!(
                                "{host} 重启后常驻进程不会自己启动，在下次连接之前任务不会运行。",
                                host = host.to_string()
                            ),
                            Some((
                                tr("设为开机时启动…"),
                                TaskPageEvent::ShowHost(machine.to_string()),
                            )),
                        ),
                    };
                notes.push(note_bar(warning, text.into(), link, cx));
            }
        }
        let runs = self.render_runs(machine, task, live, window, cx);
        v_flex()
            .w_full()
            .max_w(px(1000.))
            .mx_auto()
            .px(px(28.))
            .py(px(22.))
            .gap(px(16.))
            .child(head)
            .child(info)
            .child(prompt)
            .children(notes)
            .child(runs)
            .into_any_element()
    }

    fn render_runs(
        &self,
        machine: &str,
        task: &TaskDef,
        live: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, accent, danger, hover) = (
            theme.muted_foreground,
            theme.border,
            theme.primary,
            theme.danger,
            theme.list_hover,
        );
        let app = self.app.read(cx);
        let board = self.board.read(cx);
        let book = app.task_book(machine);
        let windows = app.machine(machine).is_some_and(|m| m.windows);
        let now = book
            .and_then(|book| book.file.clock.as_ref())
            .map(host_now)
            .unwrap_or_default();
        let runs: Vec<Run> = book
            .map(|book| book.file.runs_of(&task.id).cloned().collect())
            .unwrap_or_default();
        let explain = match (task.keep, task.worktree.is_some()) {
            (0, false) => tr("每次运行是一个会话，全部保留。").to_string(),
            (0, true) => {
                tr("每次运行是一个会话，全部保留；没有改动的工作树随会话一起删除。").to_string()
            }
            (n, false) => tf!(
                "每次运行是一个会话，保留最近 {n} 次，更早的只留最后的画面。",
                n
            ),
            (n, true) => tf!(
                "每次运行是一个会话，保留最近 {n} 次，更早的只留最后的画面；没有改动的工作树随会话一起删除。",
                n
            ),
        };
        // 说明放不下时换到标题下面一行，再放不下就折行，不截断
        let header = h_flex()
            .flex_wrap()
            .items_baseline()
            .gap_x(px(8.))
            .gap_y(px(2.))
            .child(
                div()
                    .flex_none()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("运行记录")),
            )
            .child(
                div()
                    .min_w_0()
                    .whitespace_normal()
                    .text_xs()
                    .text_color(muted)
                    .child(explain),
            );
        if runs.is_empty() {
            // AI 程序第一次在一个仓库里运行时先问信任不信任这个文件夹（Claude Code、Codex 都是，确认
            // 一次以后这个仓库和它的工作树都不再问）：没人在场的运行会停在那里，先试一次
            let empty = match task.preset.style {
                Some(_) => tr(
                    "还没有运行过。程序首次在这个仓库中运行时可能会询问是否信任该文件夹，建议先点「立即运行」试一次，并在会话中确认。",
                ),
                None => tr("还没有运行过。点「立即运行」试一次。"),
            };
            return v_flex()
                .gap(px(8.))
                .child(header)
                .child(
                    div()
                        .px(px(14.))
                        .py(px(18.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(border)
                        .text_sm()
                        .text_color(muted)
                        .whitespace_normal()
                        .child(empty),
                )
                .into_any_element();
        }
        let col = |w: f32| div().flex_none().w(px(w));
        let head_row = h_flex()
            .px(px(14.))
            .h(px(30.))
            .gap(px(10.))
            .text_xs()
            .text_color(muted)
            .border_b_1()
            .border_color(border)
            .child(col(128.).child(tr("开始时间")))
            .child(col(84.).child(tr("状态")))
            .child(col(84.).child(tr("用时")))
            .child(div().flex_1().child(tr("最后一行")))
            .child(col(64.));
        let mut table = v_flex()
            .rounded(px(8.))
            .border_1()
            .border_color(border)
            .child(head_row);
        for (ix, run) in runs.iter().enumerate() {
            let state = run_state(run, machine, live, board);
            let started = text::when(&run.at, &now);
            let tag = match run.trigger {
                Trigger::CatchUp => Some(tr("补运行")),
                Trigger::Manual => Some(tr("手动")),
                Trigger::Scheduled => None,
            };
            let took = run
                .seconds
                .map(text::duration)
                .unwrap_or_else(|| "—".into());
            // 工作树：会话还在写「工作树」，结束了有改动写改了几个文件（点了打开），没改动写已删除
            let tree: Option<AnyElement> = run.worktree.as_ref().map(|path| {
                let chip = h_flex()
                    .id(("task-run-tree", ix))
                    .flex_none()
                    .gap(px(3.))
                    .px(px(6.))
                    .rounded_full()
                    .border_1()
                    .text_xs()
                    .child(Icon::new(IconName::GitBranch).size(px(11.)));
                match run.changes {
                    Some(n) if n > 0 => {
                        let entry = own_entry(machine, windows, path);
                        chip.border_color(accent.opacity(0.5))
                            .text_color(accent)
                            .cursor_pointer()
                            .child(tf!("{n} 个文件有改动", n))
                            .tooltip(|window, cx| {
                                ui::tip(tr("在标签页中打开这个工作树"), window, cx)
                            })
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(TaskPageEvent::OpenRepo(entry.clone()))
                            }))
                            .into_any_element()
                    }
                    Some(_) => chip
                        .border_color(border)
                        .text_color(muted)
                        .child(tr("工作树已删除"))
                        .into_any_element(),
                    None => {
                        let at = SharedString::from(path.clone());
                        chip.border_color(border)
                            .text_color(muted)
                            .child(trc("运行记录", "工作树"))
                            .tooltip(move |window, cx| ui::tip(at.clone(), window, cx))
                            .into_any_element()
                    }
                }
            });
            let last: (String, Option<Hsla>) = match state {
                RunState::Skipped => (text::reason(&run.reason).to_string(), Some(muted)),
                RunState::Failed => (run.reason.clone(), Some(danger)),
                _ => (run.last.clone(), None),
            };
            let action: Option<AnyElement> = match state {
                RunState::Running
                | RunState::Waiting
                | RunState::Seen
                | RunState::Quiet
                | RunState::Idle
                | RunState::Unknown
                    if run.term.is_some() && live =>
                {
                    let id = TermId {
                        machine: machine.to_string(),
                        term: run.term.clone().unwrap_or_default(),
                    };
                    Some(
                        link_text(("task-run-open", ix), tr("打开会话"), accent)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(TaskPageEvent::OpenTerminal(id.clone()))
                            }))
                            .into_any_element(),
                    )
                }
                RunState::Ended if run.screen && live => {
                    let (machine, run, title) = (
                        machine.to_string(),
                        run.clone(),
                        SharedString::from(tf!(
                            "「{name}」{when} 的画面",
                            name = task.name.as_str(),
                            when = started.as_str()
                        )),
                    );
                    Some(
                        link_text(("task-run-screen", ix), tr("查看画面"), accent)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.show_screen(
                                    machine.clone(),
                                    run.clone(),
                                    title.clone(),
                                    window,
                                    cx,
                                )
                            }))
                            .into_any_element(),
                    )
                }
                _ => None,
            };
            let color = state.color(cx);
            let hollow = matches!(state, RunState::Ended | RunState::Skipped);
            table = table.child(
                h_flex()
                    .id(("task-run", ix))
                    .px(px(14.))
                    .min_h(px(34.))
                    .gap(px(10.))
                    .text_xs()
                    .when(ix > 0, |row| row.border_t_1().border_color(border))
                    .hover(|s| s.bg(hover))
                    .child(
                        col(128.)
                            .flex()
                            .gap(px(4.))
                            .items_center()
                            .child(started.clone())
                            .children(tag.map(|tag| {
                                div()
                                    .px(px(4.))
                                    .rounded(px(3.))
                                    .border_1()
                                    .border_color(border)
                                    .text_size(px(10.5))
                                    .text_color(muted)
                                    .child(tag)
                            })),
                    )
                    .child(
                        col(84.)
                            .flex()
                            .gap(px(5.))
                            .items_center()
                            .text_color(color)
                            .child(
                                div()
                                    .flex_none()
                                    .size(px(7.))
                                    .rounded_full()
                                    .when(hollow, |dot| dot.border_1().border_color(color))
                                    .when(!hollow, |dot| dot.bg(color)),
                            )
                            .child(state.label()),
                    )
                    .child(col(84.).text_color(muted).child(took))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(6.))
                            .children(tree)
                            .child(
                                ui::clipped(("task-run-last", ix), last.0)
                                    .flex_shrink(1.)
                                    .font_family(crate::fonts::mono())
                                    .when_some(last.1, |t, color| t.text_color(color)),
                            ),
                    )
                    .child(col(64.).flex().justify_end().children(action)),
            );
        }
        v_flex()
            .gap(px(8.))
            .child(header)
            .child(table)
            .into_any_element()
    }
}

/// 一行提醒：灰色（没连着）或黄色（主机重启后不运行），后面可以跟一个链接。
fn note_bar(
    color: Hsla,
    text: SharedString,
    link: Option<(&'static str, TaskPageEvent)>,
    cx: &Context<TaskPage>,
) -> AnyElement {
    let accent = cx.theme().primary;
    let mut link = link;
    h_flex()
        .gap(px(8.))
        .px(px(14.))
        .py(px(9.))
        .rounded(px(8.))
        .bg(color.opacity(0.12))
        .text_xs()
        .child(
            Icon::new(IconName::TriangleAlert)
                .size(px(13.))
                .text_color(color),
        )
        .child(div().min_w_0().whitespace_normal().child(text))
        .children(link.take().map(|(label, event)| {
            let event = std::cell::RefCell::new(Some(event));
            div()
                .id("task-note-link")
                .flex_none()
                .text_color(accent)
                .cursor_pointer()
                .hover(|s| s.underline())
                .child(label)
                .on_click(cx.listener(move |_, _, _, cx| {
                    if let Some(event) = event.borrow_mut().take() {
                        cx.emit(event);
                    }
                }))
        }))
        .into_any_element()
}

fn link_text(id: impl Into<ElementId>, label: &'static str, color: Hsla) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .text_color(color)
        .cursor_pointer()
        .hover(|s| s.underline())
        .child(label)
}

/// 存下的画面（还原屏幕用的输出）换成一行一行的文字：往回翻的记录在前，屏幕在后，去掉末尾的空行。
pub fn screen_lines(bytes: &[u8]) -> Vec<String> {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    struct Size {
        cols: usize,
        rows: usize,
    }
    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.rows
        }
        fn screen_lines(&self) -> usize {
            self.rows
        }
        fn columns(&self) -> usize {
            self.cols
        }
    }
    let config = Config {
        scrolling_history: 10_000,
        ..Config::default()
    };
    // 比原来的终端宽：画面是照原来的宽度写的，窄了会把长行折乱；宽了多出来的是空白，去掉
    let mut term = Term::new(
        config,
        &Size {
            cols: 400,
            rows: 60,
        },
        VoidListener,
    );
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, bytes);
    let grid = term.grid();
    let history = grid.history_size() as i32;
    let screen = grid.screen_lines() as i32;
    let cols = grid.columns();
    let mut lines: Vec<String> = (-history..screen)
        .map(|line| {
            let row = &grid[Line(line)];
            // 中文这样的宽字符占两格，后一格是占位的，不算字
            let mut text: String = (0..cols)
                .map(|col| &row[Column(col)])
                .filter(|cell| {
                    !cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                })
                .map(|cell| cell.c)
                .collect();
            text.truncate(text.trim_end().len());
            text
        })
        .collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

impl Render for TaskPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().background;
        let viewport = window.viewport_size().width.as_f32();
        let width = crate::repo::sidebar_open(&self.app.read(cx).settings.layout, viewport);
        // 全部任务：(主机, 任务)
        let tasks: Vec<(String, TaskDef)> = {
            let app = self.app.read(cx);
            app.task_machines()
                .into_iter()
                .flat_map(|machine| {
                    let list: Vec<TaskDef> = app
                        .task_book(&machine)
                        .map(|book| book.file.tasks.clone())
                        .unwrap_or_default();
                    list.into_iter().map(move |task| (machine.clone(), task))
                })
                .collect()
        };
        // 选中的不在了（删了、主机移除了）：选第一个看得到的
        let visible = |(_, task): &&(String, TaskDef)| match self.filter {
            Filter::All => true,
            Filter::Enabled => task.enabled,
            Filter::Paused => !task.enabled,
        };
        let found = self.selected.as_ref().and_then(|(machine, id)| {
            tasks
                .iter()
                .find(|(m, task)| m == machine && task.id == *id)
                .cloned()
        });
        let current = found.or_else(|| tasks.iter().find(visible).cloned());
        if let Some((machine, task)) = &current {
            self.selected = Some((machine.clone(), task.id.clone()));
        }
        let side = self.render_side(width, &tasks, cx);
        let main = match &current {
            Some((machine, task)) => self.render_detail(machine, task, window, cx),
            None => self.render_empty(cx),
        };
        h_flex()
            .size_full()
            .bg(background)
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<crate::side_split::DragSide>, window, cx| {
                    crate::side_split::drag(&this.app, event, window, cx);
                    cx.notify();
                },
            ))
            .child(side)
            .child(
                div()
                    .id("task-main")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .child(main),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::screen_lines;

    #[test]
    fn 存下的画面换成文字() {
        let lines = screen_lines(b"\x1b[1mhello\x1b[0m\r\nworld   \r\n\r\n");
        assert_eq!(lines, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn 中文之间不多出空格_长行不被折断() {
        let lines = screen_lines("只读，不提交。\r\n".as_bytes());
        assert_eq!(lines, vec!["只读，不提交。".to_string()]);
        // 原来的终端比 160 列宽时写出来的一行
        let long = "x".repeat(230);
        let lines = screen_lines(format!("{long}\r\nend\r\n").as_bytes());
        assert_eq!(lines, vec![long, "end".to_string()]);
    }
}
