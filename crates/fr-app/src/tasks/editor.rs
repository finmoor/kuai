//! 新建、编辑定时任务的对话框（原型见 `docs/design/scheduled-tasks/design-demos/`，第二版的对话框）。
//!
//! 左边是做什么、在哪做：名称、提示词、主机和仓库、在新的工作树中运行；右边是用什么、什么时候：
//! 终端预设、模型、思考深度、时间（常用 / 高级的表达式），「更多选项」里是需要确认时、错过运行时间、
//! 上一次仍在运行、保留会话、停下时通知。底下一直显示「将运行」的命令，跟着选的变。
//!
//! 保存时按预设拼好命令（`fr_core::task::compose`）一起存进任务：预设存在各台电脑的设置里，主机上
//! 没有。认不出预设是哪个程序时模型、思考深度、需要确认时都灰着，提示词在程序启动后粘贴进去。
//!
//! 「将运行」右边是「自动 / 自定义」（第四版原型）：自定义时命令是一个输入框，起始内容是拼好的命令，
//! 存的就是写的（`fr_core::task::custom_launch`），模型、思考深度、需要确认时灰着不加；提示词仍由
//! Kuai 加在最后。切回自动按预设和选项重新拼，写过的不留；换预设不动写好的命令。

use fr_core::i18n::{tr, trc};
use fr_core::schedule::{Cron, CronError, Date, Schedule, Time};
use fr_core::task::{self, ArgStyle, Confirm, EFFORTS, PromptVia};
use fr_core::tasks::{self as model, Missed, Overlap, PresetRef, TaskOp, WorktreeBase};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::{host_now, local_now, text};
use crate::app_state::AppState;
use crate::machines::LOCAL;
use crate::presets::{self, Preset};
use crate::settings::RepoEntry;
use crate::ui;

/// 平均每天超过这么多次时提醒一句额度
const BUSY_PER_DAY: f64 = 24.;

/// 对话框从哪开始。
pub enum Start {
    New {
        /// 放在哪台主机：从仓库、终端进来的是它们所在的那台；None 是不填，要自己选（任务页的「+」）
        machine: Option<String>,
        /// 仓库（那台主机上的路径）
        repo: Option<String>,
        /// 终端预设的 id
        preset: Option<String>,
        prompt: String,
    },
    Edit {
        machine: String,
        task: model::Task,
    },
    /// 复制为新任务
    Copy {
        machine: String,
        task: model::Task,
    },
}

/// 保存好了：主机、任务的 id。
pub type OnSaved = Box<dyn Fn(String, String, &mut App)>;

/// 时间那一栏的「常用」里选的哪一种。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Once,
    Hourly,
    Daily,
    Weekdays,
    Weekly,
    Manual,
}

impl Kind {
    const ALL: [Kind; 6] = [
        Kind::Once,
        Kind::Hourly,
        Kind::Daily,
        Kind::Weekdays,
        Kind::Weekly,
        Kind::Manual,
    ];

    fn label(self) -> &'static str {
        match self {
            Kind::Once => tr("仅一次"),
            Kind::Hourly => tr("每小时"),
            Kind::Daily => tr("每天"),
            Kind::Weekdays => tr("工作日"),
            Kind::Weekly => tr("每周"),
            Kind::Manual => tr("手动"),
        }
    }
}

/// 表达式那里给的几个例子：(说法, 表达式)。
fn examples() -> [(&'static str, &'static str); 3] {
    [
        (tr("每 30 分钟"), "*/30 * * * *"),
        (tr("工作日 9 点和 18 点"), "0 9,18 * * 1-5"),
        (tr("每月 1 日 3 点"), "0 3 1 * *"),
    ]
}

/// 各程序常用的几个模型，点了填进去（也可以自己输入）。
fn models(style: Option<ArgStyle>) -> &'static [&'static str] {
    match style {
        Some(ArgStyle::Claude) => &["fable", "opus", "sonnet"],
        Some(ArgStyle::Codex) => &["gpt-6.2-sol", "gpt-6-astra"],
        Some(ArgStyle::OpenCode) => &["openai/gpt-6.2-sol"],
        _ => &[],
    }
}

/// 打开对话框。
pub fn open(
    app: Entity<AppState>,
    start: Start,
    on_saved: OnSaved,
    window: &mut Window,
    cx: &mut App,
) {
    ui::cancel_pending_tip(window, cx);
    let title: SharedString = match &start {
        Start::Edit { .. } => tr("编辑定时任务").into(),
        _ => tr("新建定时任务").into(),
    };
    let editor = cx.new(|cx| TaskEditor::new(app, start, on_saved, window, cx));
    let first = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            let focus = editor.read(cx).name.focus_handle(cx);
            focus.focus(window, cx);
        }
        let (save, try_run) = (editor.clone(), editor.clone());
        let saving = editor.read(cx).saving;
        dialog
            .title(title.clone())
            .w(px(920.))
            // 回车在提示词里是换行，不是保存
            .on_ok(|_, _, _| false)
            .child(editor.clone())
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("task-cancel")
                            .label(tr("取消"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("task-save-run")
                            .icon(ui::icon(IconName::Play))
                            .label(tr("保存并试运行"))
                            .disabled(saving)
                            .on_click(move |_, window, cx| {
                                try_run.update(cx, |editor, cx| editor.save(true, window, cx))
                            }),
                    )
                    .child(
                        Button::new("task-save")
                            .primary()
                            .label(tr("保存"))
                            .loading(saving)
                            .disabled(saving)
                            .on_click(move |_, window, cx| {
                                save.update(cx, |editor, cx| editor.save(false, window, cx))
                            }),
                    ),
            )
    });
}

pub struct TaskEditor {
    app: Entity<AppState>,
    on_saved: OnSaved,
    /// 编辑的是哪个任务（新建、复制的是 None）
    editing: Option<model::Task>,
    /// 放在哪台主机（本机是 [`LOCAL`]）；None 是还没选
    machine: Option<String>,
    repo: Option<String>,
    name: Entity<InputState>,
    prompt: Entity<TextareaState>,
    worktree: bool,
    base: WorktreeBase,
    /// 选的预设（编辑时预设已经删了的，是任务里存的那一份）
    preset: PresetRef,
    model: Entity<InputState>,
    /// 换到另一个程序的预设时清掉了哪个模型（模型下面说一句；再改模型就不说了）
    cleared_model: Option<String>,
    /// 「将运行」切到了「自定义」：运行 `command` 里写的，模型、思考深度、需要确认时不加进命令
    custom: bool,
    command: Entity<InputState>,
    effort: String,
    advanced: bool,
    kind: Kind,
    time: Entity<InputState>,
    date: Entity<InputState>,
    days: Vec<u32>,
    cron: Entity<InputState>,
    more: bool,
    confirm: Confirm,
    missed: Missed,
    overlap: Overlap,
    keep: u32,
    notify: bool,
    problem: Option<SharedString>,
    saving: bool,
    _subscriptions: Vec<Subscription>,
}

fn preset_ref(preset: &Preset) -> PresetRef {
    PresetRef {
        id: preset.id.clone(),
        name: preset.name.clone(),
        icon: preset.icon.clone(),
        hue: preset.hue,
        command: preset.command.clone(),
        style: ArgStyle::detect(&preset.command),
    }
}

/// 能选的预设：命令不空的。
fn choices(cx: &App) -> Vec<Preset> {
    presets::all(cx)
        .into_iter()
        .filter(|preset| !preset.command.trim().is_empty())
        .collect()
}

impl TaskEditor {
    fn new(
        app: Entity<AppState>,
        start: Start,
        on_saved: OnSaved,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = |value: &str,
                     placeholder: &'static str,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            let state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
            if !value.is_empty() {
                let value = value.to_string();
                state.update(cx, |state, cx| state.set_value(value, window, cx));
            }
            state
        };
        let hosts = app.read(cx).task_hosts();
        let (machine, existing, copy, repo, preset_id, prompt_text) = match start {
            // 不替人选位置：没带主机（任务页的「+」）、带的那台现在连不上的，主机和仓库都空着
            Start::New {
                machine,
                repo,
                preset,
                prompt,
            } => (
                machine.filter(|m| hosts.contains(m)),
                None,
                false,
                repo,
                preset,
                prompt,
            ),
            Start::Edit { machine, task } => {
                let repo = Some(task.repo.clone());
                let prompt = task.prompt.clone();
                (Some(machine), Some(task), false, repo, None, prompt)
            }
            Start::Copy { machine, task } => {
                let repo = Some(task.repo.clone());
                let prompt = task.prompt.clone();
                (Some(machine), Some(task), true, repo, None, prompt)
            }
        };
        let all = choices(cx);
        let preset = match (&existing, &preset_id) {
            // 编辑：预设还在用现在的（改过命令的跟着新的拼），删了用任务里存的
            (Some(task), _) => all
                .iter()
                .find(|preset| preset.id == task.preset.id)
                .map(preset_ref)
                .unwrap_or_else(|| task.preset.clone()),
            (None, Some(id)) => all
                .iter()
                .find(|preset| preset.id == *id)
                .map(preset_ref)
                .unwrap_or_default(),
            (None, None) => presets::default_preset(cx)
                .filter(|preset| !preset.command.trim().is_empty())
                .or_else(|| all.first().cloned())
                .map(|preset| preset_ref(&preset))
                .unwrap_or_default(),
        };
        let name_text = match (&existing, copy) {
            (Some(task), true) => tf!("{name} 副本", name = task.name.as_str()),
            (Some(task), false) => task.name.clone(),
            (None, _) => String::new(),
        };
        let name = input(&name_text, tr("例如 每日提交评审"), window, cx);
        let prompt = cx
            .new(|cx| TextareaState::new(window, cx).placeholder(tr("运行时发给程序的第一条消息")));
        if !prompt_text.is_empty() {
            prompt.update(cx, |state, cx| state.set_value(prompt_text, window, cx));
        }
        let task_model = existing
            .as_ref()
            .map(|t| t.model.clone())
            .unwrap_or_default();
        let model = input(&task_model, tr("留空按程序自己的设置"), window, cx);
        // 命令是自定义的任务：编辑、复制时接着是自定义，输入框里是存的命令
        let custom = existing.as_ref().is_some_and(|task| task.custom);
        let command_text = existing
            .as_ref()
            .filter(|_| custom)
            .map(|task| task.launch.command.clone())
            .unwrap_or_default();
        let command = input(&command_text, tr("要运行的命令"), window, cx);
        // 时间：编辑时按任务的来，新建默认每天 09:00
        let schedule = existing
            .as_ref()
            .map(|task| task.schedule.clone())
            .unwrap_or(Schedule::Daily {
                time: Time::new(9, 0),
            });
        let (kind, time_text, date_text, days, cron_text, advanced) = match &schedule {
            Schedule::Once { date, time } => (
                Kind::Once,
                time.to_string(),
                date.to_string(),
                vec![1],
                String::new(),
                false,
            ),
            Schedule::Hourly => (
                Kind::Hourly,
                "09:00".into(),
                String::new(),
                vec![1],
                String::new(),
                false,
            ),
            Schedule::Daily { time } => (
                Kind::Daily,
                time.to_string(),
                String::new(),
                vec![1],
                String::new(),
                false,
            ),
            Schedule::Weekdays { time } => (
                Kind::Weekdays,
                time.to_string(),
                String::new(),
                vec![1],
                String::new(),
                false,
            ),
            Schedule::Weekly { days, time } => (
                Kind::Weekly,
                time.to_string(),
                String::new(),
                days.clone(),
                String::new(),
                false,
            ),
            Schedule::Cron { expr } => (
                Kind::Daily,
                "09:00".into(),
                String::new(),
                vec![1],
                expr.clone(),
                true,
            ),
            Schedule::Manual => (
                Kind::Manual,
                "09:00".into(),
                String::new(),
                vec![1],
                String::new(),
                false,
            ),
        };
        // 「仅一次」默认明天
        let date_text = match date_text.is_empty() {
            false => date_text,
            true => {
                let clock = machine
                    .as_deref()
                    .and_then(|machine| app.read(cx).task_book(machine))
                    .and_then(|book| book.file.clock.clone());
                let today = clock.as_ref().map_or_else(|| local_now().0, host_now);
                let tomorrow = today.plus_minutes(1440);
                Date::new(tomorrow.year, tomorrow.month, tomorrow.day).to_string()
            }
        };
        let time = input(&time_text, "09:00", window, cx);
        let date = input(&date_text, "2026-11-28", window, cx);
        let cron = input(&cron_text, "*/30 * * * *", window, cx);
        let mut subscriptions = Vec::new();
        for state in [&name, &model, &time, &date, &cron, &command] {
            subscriptions.push(cx.subscribe_in(
                state,
                window,
                |this: &mut Self, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.problem = None;
                        cx.notify();
                    }
                },
            ));
        }
        // 自己改了模型：「已清空模型」那句不再说
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this: &mut Self, state, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) && !state.read(cx).value().is_empty() {
                    this.cleared_model = None;
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &prompt,
            window,
            |this: &mut Self, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.problem = None;
                    cx.notify();
                }
            },
        ));
        let defaults = model::Task {
            id: String::new(),
            name: String::new(),
            prompt: String::new(),
            repo: String::new(),
            worktree: None,
            preset: PresetRef::default(),
            model: String::new(),
            effort: String::new(),
            confirm: Confirm::Default,
            launch: task::compose("", None, "", "", Confirm::Default),
            custom: false,
            shell: Vec::new(),
            schedule: Schedule::Manual,
            enabled: true,
            missed: Missed::default(),
            overlap: Overlap::default(),
            keep: 3,
            notify: true,
        };
        let base = existing.clone().unwrap_or(defaults);
        let mut this = Self {
            app,
            on_saved,
            editing: existing.filter(|_| !copy),
            machine,
            repo,
            name,
            prompt,
            worktree: base.worktree.is_some(),
            base: base.worktree.unwrap_or_default(),
            preset,
            model,
            cleared_model: None,
            custom,
            command,
            effort: base.effort.clone(),
            advanced,
            kind,
            time,
            date,
            days,
            cron,
            more: false,
            confirm: base.confirm,
            missed: base.missed,
            overlap: base.overlap,
            keep: base.keep,
            notify: base.notify,
            problem: None,
            saving: false,
            _subscriptions: subscriptions,
        };
        this.fix_repo(cx);
        this
    }

    /// 选的仓库不在这台的列表里（没选主机、带进来的仓库已从列表移除）：清掉，要自己选，不替人挑一个。
    /// 编辑时不动（任务存的仓库照样显示）。
    fn fix_repo(&mut self, cx: &App) {
        if self.editing.is_some() {
            return;
        }
        let repos = self.repos(cx);
        let known = self
            .repo
            .as_ref()
            .is_some_and(|repo| repos.iter().any(|entry| entry.path == *repo));
        if !known {
            self.repo = None;
        }
    }

    /// 选的主机的仓库列表；没选主机是空的。
    fn repos(&self, cx: &App) -> Vec<RepoEntry> {
        self.machine
            .as_deref()
            .and_then(|machine| self.app.read(cx).repos_on(machine))
            .unwrap_or_default()
    }

    /// 选的主机上的任务（时钟、是不是 root）；没选主机是 None。
    fn book<'a>(&self, cx: &'a App) -> Option<&'a super::Book> {
        self.app.read(cx).task_book(self.machine.as_deref()?)
    }

    fn style(&self) -> Option<ArgStyle> {
        self.preset.style
    }

    fn model_text(&self, cx: &App) -> String {
        self.model.read(cx).value().trim().to_string()
    }

    /// 思考深度能不能选：认得出程序；OpenCode 要先选模型。
    fn effort_usable(&self, cx: &App) -> bool {
        match self.style() {
            None => false,
            Some(style) => !style.effort_needs_model() || !self.model_text(cx).is_empty(),
        }
    }

    /// 所选主机的常驻进程以 root 运行（不知道的、没选主机的算不是）。
    fn host_root(&self, cx: &App) -> bool {
        self.book(cx).is_some_and(|book| book.file.root)
    }

    /// 实际用的「需要确认时」：这个程序没有对应写法、在这台主机上用不了的（root 上 Claude Code 的
    /// 全部跳过）按程序设置。
    fn confirm_used(&self, cx: &App) -> Confirm {
        let usable = self
            .style()
            .is_some_and(|style| style.confirm_usable(self.confirm, self.host_root(cx)));
        match usable {
            true => self.confirm,
            false => Confirm::Default,
        }
    }

    /// 现在要运行的命令：自定义的是写的那条（提示词怎么给看它运行的是哪个程序），不然按预设拼。
    fn launch(&self, cx: &App) -> task::Launch {
        match self.custom {
            true => task::custom_launch(&self.command.read(cx).value()),
            false => self.composed(cx),
        }
    }

    /// 按预设和选项拼出来的命令。
    fn composed(&self, cx: &App) -> task::Launch {
        let effort = match self.effort_usable(cx) {
            true => self.effort.as_str(),
            false => "",
        };
        let model = match self.style() {
            Some(_) => self.model_text(cx),
            None => String::new(),
        };
        task::compose(
            &self.preset.command,
            self.style(),
            &model,
            effort,
            self.confirm_used(cx),
        )
    }

    /// 现在填的时间；写错了的是 Err（说明）。
    fn schedule(&self, cx: &App) -> Result<Schedule, String> {
        if self.advanced {
            let expr = self.cron.read(cx).value().trim().to_string();
            return match Cron::parse(&expr) {
                Ok(_) => Ok(Schedule::Cron { expr }),
                Err(err) => Err(cron_problem(&err)),
            };
        }
        let time = || {
            Time::parse(&self.time.read(cx).value())
                .ok_or_else(|| tr("时间的格式是 时:分，例如 09:30").to_string())
        };
        Ok(match self.kind {
            Kind::Once => {
                let date = Date::parse(&self.date.read(cx).value())
                    .ok_or_else(|| tr("日期的格式是 年-月-日，例如 2026-11-28").to_string())?;
                Schedule::Once {
                    date,
                    time: time()?,
                }
            }
            Kind::Hourly => Schedule::Hourly,
            Kind::Daily => Schedule::Daily { time: time()? },
            Kind::Weekdays => Schedule::Weekdays { time: time()? },
            Kind::Weekly => {
                if self.days.is_empty() {
                    return Err(tr("请至少选一天").to_string());
                }
                Schedule::Weekly {
                    days: text::sorted_days(&self.days),
                    time: time()?,
                }
            }
            Kind::Manual => Schedule::Manual,
        })
    }

    /// 「将运行」在「自动」和「自定义」之间切。切到自定义时输入框里先放拼好的命令（每次都重新放：
    /// 切回自动时写过的不留），焦点进去。
    fn set_custom(&mut self, custom: bool, window: &mut Window, cx: &mut Context<Self>) {
        if custom == self.custom {
            return;
        }
        if custom {
            let command = self.composed(cx).command;
            self.command.update(cx, |state, cx| {
                state.set_value(command, window, cx);
                state.focus(window, cx);
            });
        }
        self.custom = custom;
        self.problem = None;
        cx.notify();
    }

    /// 在「常用」和「高级」之间切。切到高级时把常用里选的换成表达式带过去。
    fn set_advanced(&mut self, advanced: bool, window: &mut Window, cx: &mut Context<Self>) {
        if advanced == self.advanced {
            return;
        }
        if advanced
            && let Ok(schedule) = self.schedule(cx)
            && let Some(expr) = schedule.cron_text()
        {
            self.cron
                .update(cx, |state, cx| state.set_value(expr, window, cx));
        }
        self.advanced = advanced;
        self.problem = None;
        cx.notify();
    }

    fn host_clock(&self, cx: &App) -> Option<fr_core::tasks::Clock> {
        self.book(cx).and_then(|book| book.file.clock.clone())
    }

    /// 收拢成一个任务和它放在哪台主机；缺了、写错了的是 Err（说明）。
    fn build(&self, cx: &App) -> Result<(String, model::Task), String> {
        let name = self.name.read(cx).value().trim().to_string();
        let prompt = self.prompt.read(cx).value().trim().to_string();
        if name.is_empty() {
            return Err(tr("请填写名称").into());
        }
        if prompt.is_empty() {
            return Err(tr("请填写提示词").into());
        }
        let Some(machine) = self.machine.clone() else {
            return Err(tr("请选择主机和仓库").into());
        };
        let Some(repo) = self.repo.clone() else {
            return Err(tr("请选择仓库").into());
        };
        if self.preset.command.trim().is_empty() {
            return Err(tr("请选择终端预设").into());
        }
        let launch = self.launch(cx);
        if self.custom && launch.command.is_empty() {
            return Err(tr("请填写要运行的命令").into());
        }
        let schedule = self.schedule(cx)?;
        let model = match self.style() {
            Some(_) => self.model_text(cx),
            None => String::new(),
        };
        let effort = match self.effort_usable(cx) {
            true => self.effort.clone(),
            false => String::new(),
        };
        let confirm = self.confirm_used(cx);
        // 本机是 Windows：用设置里给终端选的 shell（和手动开的终端一样）
        let shell = match machine == LOCAL && cfg!(windows) {
            true => crate::shell::local_shell_command(&self.app, cx),
            false => Vec::new(),
        };
        let id = match &self.editing {
            Some(task) => task.id.clone(),
            None => self
                .book(cx)
                .map(|book| super::new_task_id(&book.file))
                .unwrap_or_else(|| super::new_task_id(&Default::default())),
        };
        let enabled = self.editing.as_ref().is_none_or(|task| task.enabled);
        let task = model::Task {
            id,
            name,
            prompt,
            repo,
            worktree: self.worktree.then_some(self.base),
            preset: self.preset.clone(),
            model,
            effort,
            confirm,
            launch,
            custom: self.custom,
            shell,
            schedule,
            enabled,
            missed: self.missed,
            overlap: self.overlap,
            keep: self.keep,
            notify: self.notify,
        };
        model::check(&task).map_err(|err| format!("{err:#}"))?;
        Ok((machine, task))
    }

    fn save(&mut self, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let (machine, task) = match self.build(cx) {
            Ok(built) => built,
            Err(problem) => {
                self.problem = Some(problem.into());
                cx.notify();
                return;
            }
        };
        self.saving = true;
        cx.notify();
        let id = task.id.clone();
        let ops = vec![TaskOp::Set {
            task: Box::new(task),
        }];
        let done = self
            .app
            .update(cx, |app, cx| app.apply_tasks(&machine, ops, cx));
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let saved = done.await.is_some();
            let updated = this.update(cx, |this, cx| {
                this.saving = false;
                cx.notify();
                if !saved {
                    return;
                }
                if run {
                    this.app
                        .update(cx, |app, cx| app.run_task(&machine, &id, cx));
                }
                (this.on_saved)(machine.clone(), id.clone(), cx);
            });
            if saved && updated.is_ok() {
                window_handle
                    .update(cx, |_, window, cx| window.close_dialog(cx))
                    .ok();
            }
        })
        .detach();
    }

    // ---- 画 ----

    fn label(text: &'static str, cx: &App) -> Div {
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(text)
    }

    /// 一排几个选一个的小按钮（思考深度、时间的几种）。
    fn segmented<T: Clone + PartialEq + 'static>(
        id: &'static str,
        items: Vec<(T, String)>,
        current: &T,
        disabled: bool,
        cx: &mut Context<Self>,
        pick: impl Fn(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    ) -> Div {
        let theme = cx.theme();
        let (secondary, background, muted) =
            (theme.secondary, theme.background, theme.muted_foreground);
        let pick = std::rc::Rc::new(pick);
        h_flex()
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(ui::RADIUS))
            .bg(secondary)
            .when(disabled, |row| row.opacity(0.5))
            .children(items.into_iter().enumerate().map(|(ix, (value, label))| {
                let on = value == *current;
                let pick = pick.clone();
                div()
                    .id((id, ix))
                    .px(px(10.))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .rounded(px(4.))
                    .text_xs()
                    .when(on, |b| {
                        b.bg(background).shadow_xs().font_weight(FontWeight::MEDIUM)
                    })
                    .when(!on, |b| b.text_color(muted))
                    .when(!disabled, |b| {
                        b.cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                pick(this, value.clone(), window, cx);
                                this.problem = None;
                                cx.notify();
                            }))
                    })
                    .child(label)
            }))
    }

    /// 一个小牌子（模型的建议、表达式的例子）。
    fn chip(
        id: impl Into<ElementId>,
        label: String,
        on: bool,
        mono: bool,
        cx: &App,
    ) -> Stateful<Div> {
        let theme = cx.theme();
        div()
            .id(id)
            .px(px(8.))
            .h(px(22.))
            .flex()
            .items_center()
            .rounded_full()
            .border_1()
            .border_color(if on { theme.primary } else { theme.border })
            .when(on, |c| {
                c.text_color(theme.primary).bg(theme.primary.opacity(0.08))
            })
            .text_xs()
            .when(mono, |c| c.font_family(crate::fonts::mono()))
            .cursor_pointer()
            .child(label)
    }

    fn render_location(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let app = self.app.read(cx);
        let hosts = app.task_hosts();
        let host_label = |key: &str| -> String {
            let name = app.machine_name(key).to_string();
            match app.task_book(key).and_then(|book| book.file.clock.as_ref()) {
                Some(clock) => tf!("{name}（{zone}）", name, zone = clock.offset_text()),
                None => name,
            }
        };
        let host_items: Vec<(String, String)> = hosts
            .iter()
            .map(|key| (key.clone(), host_label(key)))
            .collect();
        let current_host = match &self.machine {
            Some(key) => host_label(key),
            None => tr("选择主机").to_string(),
        };
        let repos = self.repos(cx);
        let app = self.app.read(cx);
        let repo_items: Vec<(String, String)> = repos
            .iter()
            .map(|entry| (entry.path.clone(), app.repo_name(entry).to_string()))
            .collect();
        let current_repo = self
            .repo
            .as_ref()
            .map(|path| {
                repo_items
                    .iter()
                    .find(|(p, _)| p == path)
                    .map_or_else(|| path.clone(), |(_, name)| name.clone())
            })
            .unwrap_or_else(|| tr("选择仓库").to_string());
        let editing = self.editing.is_some();
        let (host_pick, repo_pick) = (this.clone(), this.clone());
        let (host_now, repo_now) = (self.machine.clone(), self.repo.clone());
        let host_button = Button::new("task-host")
            .small()
            .label(current_host)
            .dropdown_caret(true)
            .disabled(editing)
            .tooltip(match editing {
                true => tr("任务保存在所在的主机上，不能换到别的主机。可以「复制为新任务」。"),
                false => tr("任务保存在这台主机上，由它按时运行"),
            })
            .dropdown_menu(move |mut menu, _, _| {
                for (key, label) in host_items.clone() {
                    let pick = host_pick.clone();
                    let checked = host_now.as_ref() == Some(&key);
                    menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(
                        move |_, _, cx| {
                            let key = key.clone();
                            pick.update(cx, |this, cx| {
                                // 换了主机，仓库要重新选
                                if this.machine.as_ref() != Some(&key) {
                                    this.machine = Some(key);
                                    this.repo = None;
                                }
                                this.problem = None;
                                cx.notify();
                            })
                            .ok();
                        },
                    ));
                }
                menu
            });
        let chosen = self.machine.is_some();
        let no_repos = chosen && repo_items.is_empty();
        let repo_button = Button::new("task-repo")
            .small()
            .label(current_repo)
            .dropdown_caret(true)
            .disabled(!chosen || no_repos)
            .when(!chosen, |button| button.tooltip(tr("先选择主机")))
            .dropdown_menu(move |mut menu, _, _| {
                for (path, label) in repo_items.clone() {
                    let pick = repo_pick.clone();
                    let checked = repo_now.as_deref() == Some(path.as_str());
                    menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(
                        move |_, _, cx| {
                            let path = path.clone();
                            pick.update(cx, |this, cx| {
                                this.repo = Some(path);
                                this.problem = None;
                                cx.notify();
                            })
                            .ok();
                        },
                    ));
                }
                menu.max_h(px(360.)).scrollable(true)
            });
        let muted = cx.theme().muted_foreground;
        let base_pick = this.clone();
        let base_now = self.base;
        let base_label = match self.base {
            WorktreeBase::Head => tr("仓库当前检出的提交"),
            WorktreeBase::Remote => tr("远程默认分支的最新提交（先获取）"),
        };
        let repo_name = self
            .repo
            .as_ref()
            .map(|path| path.rsplit(['/', '\\']).next().unwrap_or(path).to_string())
            .unwrap_or_default();
        let worktree_note = tf!(
            "每次运行新建 {repo}.worktrees/任务名-月日-时分 和同名分支，不动仓库里未提交的改动。会话结束时，没有改动的工作树自动删除，有改动的保留。",
            repo = repo_name
        );
        v_flex()
            .gap(px(6.))
            .child(Self::label(tr("位置"), cx))
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(div().flex_none().child(host_button))
                    .child(div().min_w_0().child(repo_button)),
            )
            // 仓库从这台主机的仓库列表里选：列表是空的时说一声，不然只看到一个点不了的按钮
            .when(no_repos, |col| {
                col.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .whitespace_normal()
                        .child(tr("这台主机的仓库列表是空的，请先在主机页打开仓库。")),
                )
            })
            .child(
                Checkbox::new("task-worktree")
                    .label(tr("在新的工作树中运行"))
                    .checked(self.worktree)
                    .on_click(cx.listener(|this, on: &bool, _, cx| {
                        this.worktree = *on;
                        cx.notify();
                    })),
            )
            .when(self.worktree, |col| {
                col.child(
                    h_flex()
                        .gap(px(8.))
                        .child(Self::label(tr("基于"), cx))
                        .child(
                            Button::new("task-base")
                                .small()
                                .label(base_label)
                                .dropdown_caret(true)
                                .dropdown_menu(move |menu, _, _| {
                                    let (a, b) = (base_pick.clone(), base_pick.clone());
                                    menu.item(
                                        PopupMenuItem::new(tr("仓库当前检出的提交"))
                                            .checked(base_now == WorktreeBase::Head)
                                            .on_click(move |_, _, cx| {
                                                a.update(cx, |this, cx| {
                                                    this.base = WorktreeBase::Head;
                                                    cx.notify();
                                                })
                                                .ok();
                                            }),
                                    )
                                    .item(
                                        PopupMenuItem::new(tr("远程默认分支的最新提交（先获取）"))
                                            .checked(base_now == WorktreeBase::Remote)
                                            .on_click(move |_, _, cx| {
                                                b.update(cx, |this, cx| {
                                                    this.base = WorktreeBase::Remote;
                                                    cx.notify();
                                                })
                                                .ok();
                                            }),
                                    )
                                }),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .whitespace_normal()
                        .child(worktree_note),
                )
            })
            .into_any_element()
    }

    fn render_presets(&self, cx: &mut Context<Self>) -> AnyElement {
        let all = choices(cx);
        let mut chips: Vec<AnyElement> = all
            .iter()
            .enumerate()
            .map(|(ix, preset)| {
                let on = preset.id == self.preset.id;
                let picked = preset_ref(preset);
                let icon = presets::icon(&preset.icon)
                    .size(px(14.))
                    .text_color(presets::color(&preset.icon, preset.hue, cx));
                Self::chip(("task-preset", ix), preset.name.clone(), on, false, cx)
                    .rounded(px(ui::RADIUS))
                    .h(px(28.))
                    .gap(px(6.))
                    .child(icon)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        // 换到另一个程序：模型是各家自己的名字（Claude Code 的 sonnet 给 Antigravity CLI
                        // 不认），清掉；同一种写法的（Claude Code 和自己加的 Claude）留着
                        let model = this.model_text(cx);
                        if picked.style != this.preset.style && !model.is_empty() {
                            this.model
                                .update(cx, |state, cx| state.set_value("", window, cx));
                            this.cleared_model = Some(model);
                        }
                        this.preset = picked.clone();
                        this.problem = None;
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        // 编辑时任务用的预设已经删了：照样列出来、选中
        if !self.preset.id.is_empty() && !all.iter().any(|p| p.id == self.preset.id) {
            chips.push(
                Self::chip(
                    "task-preset-gone",
                    self.preset.name.clone(),
                    true,
                    false,
                    cx,
                )
                .rounded(px(ui::RADIUS))
                .h(px(28.))
                .tooltip(|window, cx| {
                    ui::tip(tr("这个预设已从设置中删除，任务保留原来的命令"), window, cx)
                })
                .into_any_element(),
            );
        }
        let accent = cx.theme().primary;
        v_flex()
            .gap(px(6.))
            .child(
                h_flex()
                    .child(Self::label(tr("终端预设"), cx))
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("task-manage-presets")
                            .text_xs()
                            .text_color(accent)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child(tr("管理预设…"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                                crate::with_main_workspace(cx, |workspace, window, cx| {
                                    workspace.open_terminal_settings(window, cx)
                                });
                            }),
                    ),
            )
            .child(h_flex().flex_wrap().gap(px(6.)).children(chips))
            .into_any_element()
    }

    fn render_model(&self, cx: &mut Context<Self>) -> AnyElement {
        let style = self.style();
        let muted = cx.theme().muted_foreground;
        let current = self.model_text(cx);
        let mut chips: Vec<AnyElement> = vec![
            Self::chip(
                "task-model-default",
                tr("默认").into(),
                current.is_empty(),
                false,
                cx,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.model
                    .update(cx, |state, cx| state.set_value("", window, cx));
                this.cleared_model = None;
                cx.notify();
            }))
            .into_any_element(),
        ];
        for (ix, name) in models(style).iter().enumerate() {
            let value = name.to_string();
            chips.push(
                Self::chip(
                    ("task-model", ix),
                    value.clone(),
                    current == *name,
                    true,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    let value = value.clone();
                    this.model
                        .update(cx, |state, cx| state.set_value(value, window, cx));
                    this.cleared_model = None;
                    cx.notify();
                }))
                .into_any_element(),
            );
        }
        let cleared = self
            .cleared_model
            .as_deref()
            .filter(|_| style.is_some())
            .map(|model| tf!("换了程序，已清空模型「{model}」。", model));
        let note: Option<&'static str> = match style {
            // 命令是自定义的：灰着，说明写在思考深度下面
            _ if self.custom => None,
            None => Some(tr(
                "认不出这个预设是哪个程序：模型、思考深度要写在预设的命令里，提示词在程序启动后粘贴进去。",
            )),
            Some(ArgStyle::OpenCode) if !current.is_empty() => Some(tr(
                "OpenCode 的交互界面没有模型参数：选了模型将用 opencode run 运行，结束后程序退出，输出留在终端里。",
            )),
            _ => None,
        };
        let cleared = cleared.filter(|_| !self.custom);
        v_flex()
            .gap(px(6.))
            .child(Self::label(tr("模型"), cx))
            // 自定义时只把输入框淡下去（里面可能还留着模型名），标签和思考深度那一栏一样不变
            .child(
                div().when(self.custom, |d| d.opacity(0.5)).child(
                    Input::new(&self.model)
                        .small()
                        .disabled(style.is_none() || self.custom),
                ),
            )
            .when(style.is_some() && !self.custom, |col| {
                col.child(h_flex().flex_wrap().gap(px(6.)).children(chips))
            })
            .children(cleared.map(|text| {
                div()
                    .text_xs()
                    .text_color(muted)
                    .whitespace_normal()
                    .child(text)
            }))
            .children(note.map(|note| {
                div()
                    .text_xs()
                    .text_color(muted)
                    .whitespace_normal()
                    .child(note)
            }))
            .into_any_element()
    }

    fn render_effort(&self, cx: &mut Context<Self>) -> AnyElement {
        let usable = self.effort_usable(cx) && !self.custom;
        let items: Vec<(String, String)> = std::iter::once(String::new())
            .chain(EFFORTS.iter().map(|e| e.to_string()))
            .map(|e| (e.clone(), text::effort(&e)))
            .collect();
        // 自定义时照样显示选的那一档（切回自动还是它），只是灰着
        let current = match self.effort_usable(cx) {
            true => self.effort.clone(),
            false => String::new(),
        };
        let tip = match self.style() {
            _ if self.custom => Some(tr(
                "命令已自定义：模型、思考深度、需要确认时不再加进命令，需要时写在命令里。",
            )),
            Some(ArgStyle::OpenCode) if !usable => {
                Some(tr("OpenCode 的思考深度写在模型名后面，先选模型"))
            }
            _ => None,
        };
        v_flex()
            .gap(px(6.))
            .child(Self::label(tr("思考深度"), cx))
            .child(h_flex().child(Self::segmented(
                "task-effort",
                items,
                &current,
                !usable,
                cx,
                |this, value, _, _| this.effort = value,
            )))
            .children(tip.map(|tip| {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .whitespace_normal()
                    .child(tip)
            }))
            .into_any_element()
    }

    fn render_time(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, warning, danger) = (theme.muted_foreground, theme.warning, theme.danger);
        let toggle = Self::segmented(
            "task-time-mode",
            vec![
                (false, tr("常用").to_string()),
                (true, tr("高级").to_string()),
            ],
            &self.advanced,
            false,
            cx,
            |this, advanced, window, cx| this.set_advanced(advanced, window, cx),
        );
        let clock = self.host_clock(cx);
        let host = self
            .machine
            .as_deref()
            .map(|machine| self.app.read(cx).machine_name(machine))
            .unwrap_or_default();
        let now = clock.as_ref().map(host_now);
        let schedule = self.schedule(cx);
        let mut body = v_flex().gap(px(8.));
        if self.advanced {
            let examples: Vec<AnyElement> = examples()
                .into_iter()
                .enumerate()
                .map(|(ix, (label, expr))| {
                    Self::chip(
                        ("task-cron-example", ix),
                        label.to_string(),
                        false,
                        false,
                        cx,
                    )
                    .tooltip(move |window, cx| ui::tip(expr, window, cx))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.cron
                            .update(cx, |state, cx| state.set_value(expr, window, cx));
                        cx.notify();
                    }))
                    .into_any_element()
                })
                .collect();
            body = body
                .child(
                    h_flex()
                        .gap(px(10.))
                        .child(div().w(px(220.)).child(Input::new(&self.cron).small()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .font_family(crate::fonts::mono())
                                .child(tr("分 时 日 月 周")),
                        ),
                )
                .child(h_flex().flex_wrap().gap(px(6.)).children(examples));
            match (&schedule, &now) {
                (Ok(schedule), Some(now)) => {
                    let mut at = *now;
                    let mut next: Vec<String> = Vec::new();
                    for _ in 0..3 {
                        match schedule.next_after(&at) {
                            Some(t) => {
                                next.push(text::when(&t, now));
                                at = t;
                            }
                            None => break,
                        }
                    }
                    body = body.child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(tf!("接下来：{list}", list = next.join(tr("、")))),
                    );
                    let per_day = schedule.per_day(now);
                    if per_day > BUSY_PER_DAY {
                        body = body.child(div().text_xs().text_color(warning).child(tf!(
                            "平均每天约 {n} 次。每次运行都会消耗所选程序的额度。",
                            n = format!("{per_day:.0}")
                        )));
                    }
                }
                (Err(problem), _) => {
                    body = body.child(div().text_xs().text_color(danger).child(problem.clone()));
                }
                _ => {}
            }
        } else {
            let kinds: Vec<(Kind, String)> = Kind::ALL
                .iter()
                .map(|k| (*k, k.label().to_string()))
                .collect();
            body = body.child(h_flex().child(Self::segmented(
                "task-kind",
                kinds,
                &self.kind,
                false,
                cx,
                |this, kind, _, _| this.kind = kind,
            )));
            let needs_time = !matches!(self.kind, Kind::Hourly | Kind::Manual);
            let mut row = h_flex().gap(px(8.)).flex_wrap();
            if self.kind == Kind::Once {
                row = row.child(div().w(px(120.)).child(Input::new(&self.date).small()));
            }
            if needs_time {
                row = row.child(div().w(px(80.)).child(Input::new(&self.time).small()));
            }
            if self.kind == Kind::Weekly {
                for (ix, day) in [1u32, 2, 3, 4, 5, 6, 0].into_iter().enumerate() {
                    let on = self.days.contains(&day);
                    row = row.child(
                        Self::chip(
                            ("task-day", ix),
                            text::weekday(day).to_string(),
                            on,
                            false,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            match this.days.contains(&day) {
                                true => this.days.retain(|d| *d != day),
                                false => this.days.push(day),
                            }
                            this.problem = None;
                            cx.notify();
                        })),
                    );
                }
            }
            match self.kind {
                Kind::Hourly => {
                    row = row.child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(tr("每小时整点运行")),
                    )
                }
                Kind::Manual => {
                    row = row.child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(tr("不定时，只在点「立即运行」时运行")),
                    )
                }
                _ => {}
            }
            body = body.child(row);
            if let Err(problem) = &schedule {
                body = body.child(div().text_xs().text_color(danger).child(problem.clone()));
            }
        }
        // 按哪台的时间；下次运行（和本机时区不同时另写本机时间）。还没选主机时不知道按哪台的钟
        let manual = self.kind == Kind::Manual && !self.advanced;
        if self.machine.is_none() && !manual {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .whitespace_normal()
                    .child(tr("选择主机后，按那台主机的时间显示下次运行。")),
            );
        }
        if let (Some(clock), Some(now)) = (&clock, &now)
            && !manual
        {
            let (zone, hhmm) = (
                clock.offset_text(),
                format!("{:02}:{:02}", now.hour, now.minute),
            );
            let mut line = match self.machine.as_deref() == Some(LOCAL) {
                true => tf!("按本机的时间（{zone}，现在 {now}）。", zone, now = hhmm),
                false => tf!(
                    "按 {host} 的时间（{zone}，现在 {now}）。",
                    host = host.to_string(),
                    zone,
                    now = hhmm
                ),
            };
            if let Ok(schedule) = &schedule {
                match schedule.next_after(now) {
                    Some(next) => {
                        line.push_str(&tf!(
                            "下次运行：{when}（{until}",
                            when = text::when(&next, now),
                            until = text::until(&next, now)
                        ));
                        let (local, offset) = local_now();
                        if offset != clock.offset {
                            let local_next = local.plus_minutes(next.minutes() - now.minutes());
                            line.push_str(&tf!(
                                "，本机时间 {when}",
                                when = text::when(&local_next, &local)
                            ));
                        }
                        line.push_str(tr("）"));
                    }
                    None => line.push_str(tr("这个时间已经过了，不会再运行。")),
                }
            }
            body = body.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .whitespace_normal()
                    .child(line),
            );
        }
        v_flex()
            .gap(px(6.))
            .child(
                h_flex()
                    .child(Self::label(tr("时间"), cx))
                    .child(div().flex_1())
                    .child(toggle),
            )
            .child(body)
            .into_any_element()
    }

    fn render_more(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, fg) = (theme.muted_foreground, theme.foreground);
        let this = cx.entity().downgrade();
        let header = h_flex()
            .id("task-more")
            .gap(px(4.))
            .text_xs()
            .text_color(fg)
            .cursor_pointer()
            .child(
                Icon::new(if self.more {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(px(12.)),
            )
            .child(tr("更多选项"))
            .when(!self.more, |h| {
                // 收着时把改过的写在后面，免得看不见
                let mut changed: Vec<String> = Vec::new();
                if self.custom {
                    changed.push(tr("需要确认时随命令").to_string());
                } else if self.confirm != Confirm::Default {
                    changed.push(text::confirm(self.confirm).to_string());
                }
                if self.keep != 3 {
                    changed.push(tf!("保留会话：{n}", n = text::keep(self.keep)));
                }
                if !self.notify {
                    changed.push(tr("不通知").to_string());
                }
                h.when(!changed.is_empty(), |h| {
                    h.child(
                        div()
                            .text_color(muted)
                            .child(format!("· {}", changed.join(tr("、")))),
                    )
                })
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.more = !this.more;
                cx.notify();
            }));
        if !self.more {
            return header.into_any_element();
        }
        let style = self.style();
        // 控件只占标签右边剩下的宽度：窗口窄、英文长时下拉按钮截断，勾选框的字换行，不伸出对话框
        let row = |label: &'static str, control: AnyElement, note: Option<&'static str>| {
            v_flex()
                .gap(px(4.))
                .child(
                    h_flex()
                        .gap(px(10.))
                        .child(
                            div()
                                .w(px(110.))
                                .flex_none()
                                .text_xs()
                                .text_color(muted)
                                .child(label),
                        )
                        .child(h_flex().flex_1().min_w_0().child(control)),
                )
                .children(note.map(|note| {
                    div()
                        .pl(px(120.))
                        .text_xs()
                        .text_color(muted)
                        .whitespace_normal()
                        .child(note)
                }))
        };
        // 需要确认时：这个程序没有对应写法的、这台主机上用不了的（root 上 Claude Code 的全部跳过）灰着
        let confirm_pick = this.clone();
        let confirm_now = self.confirm;
        let root = self.host_root(cx);
        let confirm_button = Button::new("task-confirm")
            .small()
            .flex_shrink(1.)
            .min_w_0()
            .label(text::confirm(self.confirm))
            .dropdown_caret(true)
            .disabled(style.is_none() || self.custom)
            .dropdown_menu(move |mut menu, _, _| {
                for choice in std::iter::once(Confirm::Default).chain(Confirm::CHOICES) {
                    let usable = style.is_some_and(|s| s.confirm_usable(choice, root));
                    let pick = confirm_pick.clone();
                    menu = menu.item(
                        PopupMenuItem::new(text::confirm(choice))
                            .checked(choice == confirm_now)
                            .disabled(!usable)
                            .on_click(move |_, _, cx| {
                                pick.update(cx, |this, cx| {
                                    this.confirm = choice;
                                    cx.notify();
                                })
                                .ok();
                            }),
                    );
                }
                menu
            })
            .into_any_element();
        let missed_pick = this.clone();
        let missed_now = self.missed;
        let missed_button = Button::new("task-missed")
            .small()
            .flex_shrink(1.)
            .min_w_0()
            .label(text::missed(self.missed))
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for choice in [Missed::CatchUp, Missed::Skip] {
                    let pick = missed_pick.clone();
                    menu = menu.item(
                        PopupMenuItem::new(text::missed(choice))
                            .checked(choice == missed_now)
                            .on_click(move |_, _, cx| {
                                pick.update(cx, |this, cx| {
                                    this.missed = choice;
                                    cx.notify();
                                })
                                .ok();
                            }),
                    );
                }
                menu
            })
            .into_any_element();
        let overlap_pick = this.clone();
        let overlap_now = self.overlap;
        let overlap_button = Button::new("task-overlap")
            .small()
            .flex_shrink(1.)
            .min_w_0()
            .label(text::overlap(self.overlap))
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for choice in [Overlap::Skip, Overlap::Run] {
                    let pick = overlap_pick.clone();
                    menu = menu.item(
                        PopupMenuItem::new(text::overlap(choice))
                            .checked(choice == overlap_now)
                            .on_click(move |_, _, cx| {
                                pick.update(cx, |this, cx| {
                                    this.overlap = choice;
                                    cx.notify();
                                })
                                .ok();
                            }),
                    );
                }
                menu
            })
            .into_any_element();
        let keep_pick = this.clone();
        let keep_now = self.keep;
        let keep_button = Button::new("task-keep")
            .small()
            .label(text::keep(self.keep))
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for choice in [1u32, 3, 10, 0] {
                    let pick = keep_pick.clone();
                    menu = menu.item(
                        PopupMenuItem::new(text::keep(choice))
                            .checked(choice == keep_now)
                            .on_click(move |_, _, cx| {
                                pick.update(cx, |this, cx| {
                                    this.keep = choice;
                                    cx.notify();
                                })
                                .ok();
                            }),
                    );
                }
                menu
            })
            .into_any_element();
        let notify = Checkbox::new("task-notify")
            .flex_1()
            .min_w_0()
            .label(tr("程序停下等待处理时通知"))
            .checked(self.notify)
            .on_click(cx.listener(|this, on: &bool, _, cx| {
                this.notify = *on;
                cx.notify();
            }))
            .into_any_element();
        v_flex()
            .gap(px(8.))
            .child(header)
            .child(row(
                tr("需要确认时"),
                confirm_button,
                Some(match self.custom {
                    true => tr("命令已自定义：需要时写在命令里。"),
                    false => text::confirm_tip(style, self.confirm, root),
                }),
            ))
            .child(row(tr("错过运行时间"), missed_button, None))
            .child(row(tr("上一次仍在运行"), overlap_button, None))
            .child(row(
                tr("保留会话"),
                keep_button,
                Some(tr("更早的会话结束，只留最后的画面。")),
            ))
            .child(row(trc("选项", "通知"), notify, None))
            .into_any_element()
    }

    fn render_command(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, accent, border, secondary) = (
            theme.muted_foreground,
            theme.primary,
            theme.border,
            theme.secondary,
        );
        let launch = self.launch(cx);
        let note = match (launch.prompt, launch.exits) {
            (PromptVia::Paste, _) => tr("程序启动后把提示词作为第一条消息粘贴进去"),
            (PromptVia::Arg, true) => tr("提示词作为参数，运行结束即退出"),
            (PromptVia::Arg, false) => tr("提示词作为第一条消息"),
        };
        let toggle = Self::segmented(
            "task-command-mode",
            vec![
                (false, tr("自动").to_string()),
                (true, tr("自定义").to_string()),
            ],
            &self.custom,
            false,
            cx,
            |this, custom, window, cx| this.set_custom(custom, window, cx),
        );
        // 自定义的是输入框，占满中间；自动的是拼好的命令，放不下截断（悬停看全文）
        let command: AnyElement = match self.custom {
            true => div()
                .flex_1()
                .min_w_0()
                .font_family(crate::fonts::mono())
                .child(Input::new(&self.command).small())
                .into_any_element(),
            false => ui::clipped("task-editor-command", launch.command.clone())
                .flex_shrink(1.)
                .font_family(crate::fonts::mono())
                .into_any_element(),
        };
        let line = h_flex()
            .gap(px(8.))
            .px(px(12.))
            .py(px(8.))
            .rounded(px(ui::RADIUS))
            .border_1()
            .border_color(border)
            .bg(secondary.opacity(0.6))
            .text_xs()
            .child(div().flex_none().text_color(muted).child(tr("将运行")))
            .child(command)
            .when(launch.prompt == PromptVia::Arg, |row| {
                row.child(
                    div()
                        .flex_none()
                        .px(px(6.))
                        .rounded(px(4.))
                        .bg(accent.opacity(0.12))
                        .text_color(accent)
                        .child(text::prompt_chip(&launch)),
                )
            })
            .when(!self.custom, |row| row.child(div().flex_1()))
            .child(div().flex_none().text_color(muted).child(note))
            .child(div().flex_none().child(toggle));
        v_flex()
            .gap(px(6.))
            .child(line)
            .when(self.custom, |col| {
                col.child(
                    div()
                        .px(px(2.))
                        .text_xs()
                        .text_color(muted)
                        .whitespace_normal()
                        .child(tr(
                            "提示词由 Kuai 加在命令后面。切回「自动」会按预设和上面的选项重新生成，改过的命令不保留。",
                        )),
                )
            })
            .into_any_element()
    }
}

/// 表达式写错了怎么说。
fn cron_problem(err: &CronError) -> String {
    match err {
        CronError::Fields(n) => tf!("表达式要有五段（分 时 日 月 周），现在是 {n} 段", n = *n),
        // 写明这一段能填多少：不是数字、超出范围、范围写反了，看了就知道怎么改
        CronError::Field { index, text } => {
            let (field, range) = match index {
                0 => (tr("分"), "0～59"),
                1 => (tr("时"), "0～23"),
                2 => (tr("日"), "1～31"),
                3 => (tr("月"), "1～12"),
                _ => (tr("周"), tr("0～7（0 和 7 都是周日）")),
            };
            tf!(
                "第 {n} 段（{field}）的「{text}」有误，可填 {range}",
                n = index + 1,
                field,
                text = text.as_str(),
                range
            )
        }
    }
}

impl Render for TaskEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let left = v_flex()
            .flex_1()
            .min_w_0()
            .gap(px(14.))
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(Self::label(tr("名称"), cx))
                    .child(Input::new(&self.name).small()),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(Self::label(tr("提示词"), cx))
                    .child(Textarea::new(&self.prompt).h(px(132.)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .whitespace_normal()
                            .child(tr(
                                "运行时没有人在场。请写清范围和不要做的事，例如「只读，不提交」。",
                            )),
                    ),
            )
            .child(self.render_location(cx));
        let right = v_flex()
            .flex_1()
            .min_w_0()
            .gap(px(14.))
            .child(self.render_presets(cx))
            .child(self.render_model(cx))
            .child(self.render_effort(cx))
            .child(self.render_time(cx))
            .child(self.render_more(cx));
        v_flex()
            .gap(px(14.))
            .child(h_flex().items_start().gap(px(24.)).child(left).child(right))
            .child(self.render_command(cx))
            .children(
                self.problem
                    .clone()
                    .map(|problem| div().text_sm().text_color(danger).child(problem)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::cron_problem;
    use fr_core::schedule::Cron;

    #[test]
    fn 表达式写错了说是哪一段() {
        let err = Cron::parse("*/30 * *").unwrap_err();
        assert_eq!(
            cron_problem(&err),
            "表达式要有五段（分 时 日 月 周），现在是 3 段"
        );
        let err = Cron::parse("0 25 * * *").unwrap_err();
        assert_eq!(cron_problem(&err), "第 2 段（时）的「25」有误，可填 0～23");
        let err = Cron::parse("0 9 * * 8").unwrap_err();
        assert_eq!(
            cron_problem(&err),
            "第 5 段（周）的「8」有误，可填 0～7（0 和 7 都是周日）"
        );
    }
}
