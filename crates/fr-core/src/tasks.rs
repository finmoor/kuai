//! 定时任务和它们的运行记录（协议 18 起）。
//!
//! 任务存在仓库所在的主机上，由那台主机的 kuaid 保存（[`path`]）、按时运行；连上来的各台电脑看到
//! 的都是这一份。改动按动作（[`TaskOp`]）发给 kuaid，改完它把整份（连同运行记录和主机现在的时间）
//! 推给所有订阅着的客户端。和仓库列表分成两个文件：旧版的 kuaid、Kuai 只认得那一份，读写它时
//! 不会把任务弄丢。
//!
//! 时间都是主机的本地时间（[`Wall`]），见 [`crate::schedule`]。要运行的命令由客户端保存时按终端
//! 预设拼好（[`crate::task::compose`]）：预设存在各台电脑的设置里，主机上没有。

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::i18n::tr;
use crate::schedule::{Schedule, Wall};
use crate::task::{ArgStyle, Confirm, Launch};
use crate::tf;

/// 文件名，和仓库列表（`workspaces.json`）放在同一个目录里。
const FILE: &str = "tasks.json";
/// 写入前留着的上一份
const BACKUP: &str = "tasks.json.bak";
/// 每个任务最多留几条运行记录（跳过的、失败的也算）
pub const RUNS_KEPT: usize = 50;

/// 平时用的那个文件。
pub fn path() -> PathBuf {
    crate::store::dir().join(FILE)
}

/// 一个定时任务。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// 在这台主机上唯一，由新建的一方起
    pub id: String,
    pub name: String,
    /// 每次运行时发给程序的第一条消息
    pub prompt: String,
    /// 仓库（这台主机上的路径，客户端看到的写法）
    pub repo: String,
    /// 每次运行新建一个工作树；None 是直接在仓库里运行
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeBase>,
    pub preset: PresetRef,
    /// 模型；空着是按程序自己的设置
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// 思考深度；空着是默认
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
    #[serde(default)]
    pub confirm: Confirm,
    /// 拼好的命令（[`crate::task::compose`]）；`custom` 时是人写的（[`crate::task::custom_launch`]）
    pub launch: Launch,
    /// 命令是自定义的（协议 19 起）：模型、思考深度、需要确认时不加进命令，编辑时也不按预设重新拼
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub custom: bool,
    /// Windows 主机上用哪个 shell（程序和参数，本机的 Kuai 设置里选的）；空着由 kuaid 定
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shell: Vec<String>,
    pub schedule: Schedule,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub missed: Missed,
    #[serde(default)]
    pub overlap: Overlap,
    /// 保留最近几次运行的会话；0 是全部保留
    #[serde(default = "default_keep")]
    pub keep: u32,
    /// 停下时通知
    #[serde(default = "yes")]
    pub notify: bool,
}

impl Task {
    /// 运行的是哪个程序（认得出的话）：命令是自定义的看命令本身，不然看预设。
    pub fn style(&self) -> Option<ArgStyle> {
        match self.custom {
            true => ArgStyle::detect(&self.launch.command),
            false => self.preset.style,
        }
    }

    /// 运行的程序的命令（认是哪家 AI 助手用）：自定义的是写的那条，不然是预设的。
    pub fn program_command(&self) -> &str {
        match self.custom {
            true => &self.launch.command,
            false => &self.preset.command,
        }
    }
}

fn yes() -> bool {
    true
}

fn default_keep() -> u32 {
    3
}

/// 任务用的终端预设：显示用的名字、图标、颜色，和预设本来的命令（编辑时重新拼命令用）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetRef {
    /// 客户端设置里预设的 id（`claude`、`custom-1`）
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hue: Option<usize>,
    /// 预设的命令
    pub command: String,
    /// 模型、思考深度这些参数按哪个工具的写法；None 是认不出
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<ArgStyle>,
}

/// 工作树从哪个提交开出来。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeBase {
    /// 仓库当前检出的提交
    #[default]
    Head,
    /// 先获取，再用远程默认分支的最新提交
    Remote,
}

/// 错过的运行（主机关机、kuaid 没在运行）怎么办。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Missed {
    /// 恢复后补运行最近的一次
    #[default]
    CatchUp,
    Skip,
}

/// 到点时上一次还在运行怎么办。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Overlap {
    #[default]
    Skip,
    Run,
}

/// 一次运行是怎么开始的。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// 到点
    #[default]
    Scheduled,
    /// 错过以后补的
    CatchUp,
    /// 点了「立即运行」
    Manual,
}

/// 一次运行的结果。会话还在不在、程序在不在输出，看终端列表。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// 开了终端、输入了命令
    #[default]
    Started,
    /// 没运行：原因在 [`Run::reason`]
    Skipped,
    /// 运行不了（工作树建不了、终端开不了）：原因在 [`Run::reason`]
    Failed,
}

/// 一次运行。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    /// 在这台主机上唯一
    pub id: String,
    pub task: String,
    /// 本该运行的时间（主机的本地时间）；立即运行的是点的那一刻
    pub at: Wall,
    /// 实际开始的时间（1970 年以来的秒数）
    #[serde(default)]
    pub started: u64,
    #[serde(default)]
    pub trigger: Trigger,
    #[serde(default)]
    pub outcome: Outcome,
    /// 跳过、失败的原因
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// 终端会话的编号（`term_list` 里的 `term`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
    /// 会话已经结束（1970 年以来的秒数）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<u64>,
    /// 会话里最后要紧的一行：AI 助手是它最后的回复，别的程序是它最后的输出（见 kuaid 的
    /// `Screen::summary_text`、`note_last`）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last: String,
    /// 用了多久（秒）：程序停下输出时记下，从开始到屏幕最后一次变化；停下后一分钟之内、没人在终端
    /// 里按过回车又有了输出的，先清掉，再停下时重记（见 kuaid 的 `settle`）。会话结束时还没记的，
    /// 记到结束
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<u64>,
    /// 存了会话结束时的画面（`task_screen` 读）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub screen: bool,
    /// 新建的工作树（路径）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// 工作树的分支
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// 工作树是从哪个提交开出来的（算改了几个文件用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// 会话结束时工作树里改了几个文件（含新的提交改的）。0 是没改、工作树已经删了；
    /// None 是还没结束或者没有工作树
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<u32>,
    /// 会话转成了普通会话（协议 19 起，[`TaskOp::Detach`]）：会话照常开着、列在会话页里，不再算这个
    /// 任务的。`ended` 记的是转的时候，所以「保留会话」、跟着会话记最后一行这些都不再管它
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
}

/// 一个修改动作。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TaskOp {
    /// 新建，或者按 id 整个换掉
    Set { task: Box<Task> },
    /// 删除任务和它的运行记录（运行出来的会话不动）
    Remove { id: String },
    /// 启用、暂停
    Enable { id: String, enabled: bool },
    /// 把一次运行的会话转成普通会话（协议 19 起）：会话不结束，运行记成结束（[`Run::detached`]）
    Detach { run: String },
}

/// 主机的钟：推给客户端时带上，客户端拿它算「下次运行」、显示时区。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clock {
    /// 主机现在的本地时间
    pub now: Wall,
    /// 比 UTC 快多少分钟（东八区是 480）
    pub offset: i32,
    /// 时区的名字（`Asia/Shanghai`、`中国标准时间`）；不知道是空的
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub zone: String,
}

impl Clock {
    /// `UTC+8`、`UTC-3:30`、`UTC+0`。
    pub fn offset_text(&self) -> String {
        let sign = if self.offset < 0 { '-' } else { '+' };
        let minutes = self.offset.unsigned_abs();
        match minutes % 60 {
            0 => format!("UTC{sign}{}", minutes / 60),
            rest => format!("UTC{sign}{}:{rest:02}", minutes / 60),
        }
    }
}

/// 一台主机上的全部任务和运行记录。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFile {
    /// 每改一次加一
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub tasks: Vec<Task>,
    /// 运行记录，新的在前
    #[serde(default)]
    pub runs: Vec<Run>,
    /// 调度上次看到的时间（主机的本地时间）：kuaid 没在运行的这段时间错过了什么从它算
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<Wall>,
    /// 下一个运行记录的编号
    #[serde(default)]
    pub next_run: u64,
    /// 推给客户端时带上的主机的钟，不存盘
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<Clock>,
    /// 推给客户端时带上：常驻进程以 root 运行（Claude Code 不许 root 跳过全部确认，见
    /// [`crate::task::ArgStyle::confirm_usable`]），不存盘
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub root: bool,
}

impl TaskFile {
    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }

    pub fn task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|task| task.id == id)
    }

    /// 这个任务的运行记录，新的在前。
    pub fn runs_of<'a>(&'a self, task: &'a str) -> impl Iterator<Item = &'a Run> + 'a {
        self.runs.iter().filter(move |run| run.task == task)
    }

    pub fn run_mut(&mut self, id: &str) -> Option<&mut Run> {
        self.runs.iter_mut().find(|run| run.id == id)
    }

    /// 依次执行这串动作，返回有没有改动（有就把版本号加一）。做不了的报错，这时 `self` 不动。
    pub fn apply(&mut self, ops: &[TaskOp]) -> Result<bool> {
        let mut next = self.clone();
        for op in ops {
            match op {
                TaskOp::Set { task } => {
                    check(task)?;
                    match next.task_mut(&task.id) {
                        Some(old) => *old = (**task).clone(),
                        None => next.tasks.push((**task).clone()),
                    }
                }
                TaskOp::Remove { id } => {
                    if next.task(id).is_none() {
                        bail!("{}", tf!("任务 {id} 不存在", id));
                    }
                    next.tasks.retain(|task| task.id != *id);
                    next.runs.retain(|run| run.task != *id);
                }
                TaskOp::Enable { id, enabled } => {
                    next.task_mut(id)
                        .with_context(|| tf!("任务 {id} 不存在", id))?
                        .enabled = *enabled;
                }
                TaskOp::Detach { run } => {
                    let run = next.run_mut(run).context(tr("运行记录不存在"))?;
                    if run.detached {
                        continue;
                    }
                    if run.term.is_none() || run.ended.is_some() {
                        bail!(tr("这次运行没有开着的会话"));
                    }
                    let now = now_secs();
                    run.detached = true;
                    run.ended = Some(now);
                    // 还没停下过：用时记到转的时候
                    run.seconds.get_or_insert(now.saturating_sub(run.started));
                }
            }
        }
        let changed = next != *self;
        if changed {
            next.version = self.version + 1;
            *self = next;
        }
        Ok(changed)
    }

    /// 加一条运行记录（放在最前面），返回它的编号。每个任务只留最近 [`RUNS_KEPT`] 条。
    pub fn add_run(&mut self, mut run: Run) -> String {
        self.next_run += 1;
        run.id = format!("r{}", self.next_run);
        let id = run.id.clone();
        let task = run.task.clone();
        self.runs.insert(0, run);
        let mut seen = 0;
        self.runs.retain(|run| {
            if run.task != task {
                return true;
            }
            seen += 1;
            seen <= RUNS_KEPT
        });
        self.version += 1;
        id
    }

    /// 有没有启用的、会按时运行的任务：有就不该因为空闲退出。
    pub fn has_scheduled(&self) -> bool {
        self.tasks
            .iter()
            .any(|task| task.enabled && !matches!(task.schedule, Schedule::Manual))
    }
}

/// 现在是 1970 年以来的第几秒。
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// 检查一个任务能不能存：名字、提示词、仓库、命令不能空，表达式不能写错。
pub fn check(task: &Task) -> Result<()> {
    if task.id.trim().is_empty() {
        bail!(tr("任务的 id 不能为空"));
    }
    if task.name.trim().is_empty() {
        bail!(tr("请填写名称"));
    }
    if task.prompt.trim().is_empty() {
        bail!(tr("请填写提示词"));
    }
    if task.repo.trim().is_empty() {
        bail!(tr("请选择仓库"));
    }
    if task.launch.command.trim().is_empty() {
        bail!(tr("终端预设的命令为空"));
    }
    if let Err(err) = task.schedule.check() {
        bail!("{}", cron_error_text(&err));
    }
    Ok(())
}

/// 表达式写错了的说明，界面上也用它。
pub fn cron_error_text(err: &crate::schedule::CronError) -> String {
    use crate::schedule::CronError;
    const NAMES: [&str; 5] = ["分", "时", "日", "月", "周"];
    match err {
        CronError::Fields(count) => tf!(
            "表达式须为五段（分 时 日 月 周），现在是 {count} 段",
            count = count.to_string()
        ),
        CronError::Field { index, text } => tf!(
            "表达式的第 {n} 段（{name}）有误：{text}",
            n = (index + 1).to_string(),
            name = tr(NAMES[(*index).min(4)]),
            text = text.as_str()
        ),
    }
}

/// 读任务文件。没有就是空的；坏了读上一份，也坏了就从空的开始，坏的那份改名留着。
pub fn load(path: &Path) -> TaskFile {
    match read(path) {
        Ok(Some(file)) => return file,
        Ok(None) => {}
        Err(err) => {
            eprintln!("{err:#}");
            let _ = std::fs::rename(path, path.with_extension("json.broken"));
        }
    }
    read(&path.with_file_name(BACKUP))
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn read(path: &Path) -> Result<Option<TaskFile>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| tf!("无法读取 {path}", path = path.display()));
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| tf!("{path} 格式错误", path = path.display()))
}

/// 写任务文件：先写临时文件再改名，原来那份留作上一份。主机的钟、是不是 root 不存。
pub fn save(path: &Path, file: &TaskFile) -> Result<()> {
    let dir = path.parent().context(tr("状态文件的路径无效"))?;
    std::fs::create_dir_all(dir).with_context(|| tf!("无法创建 {path}", path = dir.display()))?;
    let mut file = file.clone();
    file.clock = None;
    file.root = false;
    let text = serde_json::to_string_pretty(&file)?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text).with_context(|| tf!("无法写入 {path}", path = temp.display()))?;
    if path.exists() {
        let _ = std::fs::copy(path, path.with_file_name(BACKUP));
    }
    std::fs::rename(&temp, path).with_context(|| tf!("无法写入 {path}", path = path.display()))
}

/// 工作树的目录名和分支名：任务名里路径和分支名用不了的字符换成横线，后面加运行的月日和时分
/// （`夜间测试与修复-1127-0200`）。
pub fn worktree_name(task_name: &str, at: &Wall) -> String {
    let mut name = String::new();
    for c in task_name.trim().chars() {
        let bad = c.is_whitespace()
            || c.is_control()
            || matches!(
                c,
                '/' | '\\'
                    | ':'
                    | '*'
                    | '?'
                    | '"'
                    | '<'
                    | '>'
                    | '|'
                    | '~'
                    | '^'
                    | '['
                    | ']'
                    | '@'
                    | '{'
                    | '}'
                    | '.'
            );
        let c = if bad { '-' } else { c };
        if c == '-' && (name.is_empty() || name.ends_with('-')) {
            continue;
        }
        name.push(c);
    }
    let name = name.trim_end_matches('-');
    let name = match name.chars().count() {
        0 => "task".to_string(),
        n if n > 40 => name.chars().take(40).collect(),
        _ => name.to_string(),
    };
    format!(
        "{name}-{:02}{:02}-{:02}{:02}",
        at.month, at.day, at.hour, at.minute
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::Time;
    use crate::task::{PromptVia, compose};

    fn task(id: &str) -> Task {
        Task {
            id: id.into(),
            name: "每日提交评审".into(),
            prompt: "评审昨天合并到 main 的提交".into(),
            repo: "/home/me/mochi-api".into(),
            worktree: None,
            preset: PresetRef {
                id: "claude".into(),
                name: "Claude Code".into(),
                icon: "claude-code".into(),
                hue: None,
                command: "claude".into(),
                style: Some(ArgStyle::Claude),
            },
            model: "fable".into(),
            effort: "high".into(),
            confirm: Confirm::ReadOnly,
            launch: compose(
                "claude",
                Some(ArgStyle::Claude),
                "fable",
                "high",
                Confirm::ReadOnly,
            ),
            custom: false,
            shell: Vec::new(),
            schedule: Schedule::Daily {
                time: Time::new(9, 0),
            },
            enabled: true,
            missed: Missed::CatchUp,
            overlap: Overlap::Skip,
            keep: 3,
            notify: true,
        }
    }

    #[test]
    fn 新建_改_暂停_删除() {
        let mut file = TaskFile::default();
        assert!(
            file.apply(&[TaskOp::Set {
                task: Box::new(task("a"))
            }])
            .unwrap()
        );
        assert_eq!(file.version, 1);
        assert!(file.has_scheduled());
        // 一样的再存一遍不算改动
        assert!(
            !file
                .apply(&[TaskOp::Set {
                    task: Box::new(task("a"))
                }])
                .unwrap()
        );
        assert_eq!(file.version, 1);
        let mut renamed = task("a");
        renamed.name = "评审".into();
        assert!(
            file.apply(&[TaskOp::Set {
                task: Box::new(renamed)
            }])
            .unwrap()
        );
        assert_eq!(file.tasks.len(), 1);
        assert_eq!(file.tasks[0].name, "评审");
        file.apply(&[TaskOp::Enable {
            id: "a".into(),
            enabled: false,
        }])
        .unwrap();
        assert!(!file.has_scheduled());
        file.add_run(Run {
            task: "a".into(),
            ..Default::default()
        });
        assert!(file.apply(&[TaskOp::Remove { id: "a".into() }]).unwrap());
        assert!(file.tasks.is_empty());
        assert!(file.runs.is_empty(), "删任务连同运行记录");
    }

    #[test]
    fn 做不了的整串都不算() {
        let mut file = TaskFile::default();
        let mut bad = task("b");
        bad.prompt = "  ".into();
        let err = file
            .apply(&[
                TaskOp::Set {
                    task: Box::new(task("a")),
                },
                TaskOp::Set {
                    task: Box::new(bad),
                },
            ])
            .unwrap_err();
        assert_eq!(err.to_string(), "请填写提示词");
        assert_eq!(file, TaskFile::default());
        assert!(file.apply(&[TaskOp::Remove { id: "x".into() }]).is_err());
        let mut cron = task("c");
        cron.schedule = Schedule::Cron {
            expr: "0 25 * * *".into(),
        };
        let err = file
            .apply(&[TaskOp::Set {
                task: Box::new(cron),
            }])
            .unwrap_err();
        assert_eq!(err.to_string(), "表达式的第 2 段（时）有误：25");
        // 手动的不算按时运行
        let mut manual = task("m");
        manual.schedule = Schedule::Manual;
        file.apply(&[TaskOp::Set {
            task: Box::new(manual),
        }])
        .unwrap();
        assert!(!file.has_scheduled());
    }

    #[test]
    fn 转为普通会话_运行记成结束_会话不算任务的了() {
        let mut file = TaskFile::default();
        file.apply(&[TaskOp::Set {
            task: Box::new(task("a")),
        }])
        .unwrap();
        let started = now_secs() - 90;
        let open = file.add_run(Run {
            task: "a".into(),
            started,
            term: Some("t3".into()),
            ..Default::default()
        });
        let stopped = file.add_run(Run {
            task: "a".into(),
            started,
            term: Some("t4".into()),
            seconds: Some(40),
            ..Default::default()
        });
        let version = file.version;
        assert!(
            file.apply(&[
                TaskOp::Detach { run: open.clone() },
                TaskOp::Detach {
                    run: stopped.clone()
                },
            ])
            .unwrap()
        );
        assert_eq!(file.version, version + 1);
        let run = file.runs.iter().find(|run| run.id == open).unwrap();
        assert!(run.detached);
        let ended = run.ended.expect("记成结束");
        assert!(ended >= started + 90);
        assert_eq!(
            run.seconds,
            Some(ended - started),
            "没停下过的用时记到转的时候"
        );
        assert_eq!(run.term.as_deref(), Some("t3"), "会话还是那一个");
        let run = file.runs.iter().find(|run| run.id == stopped).unwrap();
        assert_eq!(run.seconds, Some(40), "停下过的用时不动");
        // 再转一遍不算改动
        assert!(!file.apply(&[TaskOp::Detach { run: open }]).unwrap());
        // 已经结束的、没开终端的、不存在的转不了
        let ended = file.add_run(Run {
            task: "a".into(),
            term: Some("t5".into()),
            ended: Some(started),
            ..Default::default()
        });
        let skipped = file.add_run(Run {
            task: "a".into(),
            outcome: Outcome::Skipped,
            ..Default::default()
        });
        for run in [ended, skipped] {
            let err = file.apply(&[TaskOp::Detach { run }]).unwrap_err();
            assert_eq!(err.to_string(), "这次运行没有开着的会话");
        }
        let err = file
            .apply(&[TaskOp::Detach { run: "r99".into() }])
            .unwrap_err();
        assert_eq!(err.to_string(), "运行记录不存在");
        // 存成的样子：没转的不写这一项
        let json = serde_json::to_string(&TaskOp::Detach { run: "r1".into() }).unwrap();
        assert_eq!(json, r#"{"op":"detach","run":"r1"}"#);
        assert!(
            !serde_json::to_string(&Run::default())
                .unwrap()
                .contains("detached")
        );
        assert!(
            !serde_json::to_string(&task("a"))
                .unwrap()
                .contains("custom")
        );
    }

    #[test]
    fn 运行记录_新的在前_每个任务留五十条() {
        let mut file = TaskFile::default();
        for minute in 0..55 {
            for id in ["a", "b"] {
                file.add_run(Run {
                    task: id.into(),
                    at: Wall::new(2026, 11, 27, 9, minute),
                    ..Default::default()
                });
            }
        }
        assert_eq!(file.runs_of("a").count(), RUNS_KEPT);
        assert_eq!(file.runs_of("b").count(), RUNS_KEPT);
        let newest = file.runs_of("a").next().unwrap();
        assert_eq!(newest.at.minute, 54);
        assert_eq!(newest.id, "r109");
        assert_eq!(file.runs_of("a").last().unwrap().at.minute, 5);
    }

    #[test]
    fn 存盘不带主机的钟_读回来一样() {
        let dir = std::env::temp_dir().join(format!("kuai-tasks-test-{}", std::process::id()));
        let path = dir.join(FILE);
        let mut file = TaskFile::default();
        file.apply(&[TaskOp::Set {
            task: Box::new(task("a")),
        }])
        .unwrap();
        file.add_run(Run {
            task: "a".into(),
            at: Wall::new(2026, 11, 27, 9, 0),
            term: Some("t3".into()),
            worktree: Some("/home/me/mochi-api.worktrees/x".into()),
            ..Default::default()
        });
        file.clock = Some(Clock {
            now: Wall::new(2026, 11, 27, 12, 14),
            offset: 0,
            zone: "Etc/UTC".into(),
        });
        save(&path, &file).unwrap();
        let back = load(&path);
        assert_eq!(back.clock, None);
        file.clock = None;
        assert_eq!(back, file);
        // 坏了读上一份
        save(&path, &file).unwrap();
        std::fs::write(&path, "{").unwrap();
        assert_eq!(load(&path), file);
        let _ = std::fs::remove_dir_all(&dir);
        // 旧文件里没有的字段取默认
        let text = r#"{"tasks":[{"id":"a","name":"n","prompt":"p","repo":"/r",
            "preset":{"name":"Claude Code","command":"claude"},
            "launch":{"command":"claude","prompt":"arg"},
            "schedule":{"kind":"hourly"}}]}"#;
        let old: TaskFile = serde_json::from_str(text).unwrap();
        let task = &old.tasks[0];
        assert!(task.enabled && task.notify);
        assert_eq!(task.keep, 3);
        assert_eq!(task.missed, Missed::CatchUp);
        assert_eq!(task.overlap, Overlap::Skip);
        assert_eq!(task.launch.prompt, PromptVia::Arg);
    }

    #[test]
    fn 时区写成_utc_加几() {
        let clock = |offset| Clock {
            offset,
            ..Default::default()
        };
        assert_eq!(clock(480).offset_text(), "UTC+8");
        assert_eq!(clock(0).offset_text(), "UTC+0");
        assert_eq!(clock(-210).offset_text(), "UTC-3:30");
        assert_eq!(clock(345).offset_text(), "UTC+5:45");
    }

    #[test]
    fn 工作树的名字() {
        let at = Wall::new(2026, 11, 27, 2, 0);
        assert_eq!(
            worktree_name("夜间测试与修复", &at),
            "夜间测试与修复-1127-0200"
        );
        assert_eq!(
            worktree_name(" fix: api/usage v2 ", &at),
            "fix-api-usage-v2-1127-0200"
        );
        assert_eq!(worktree_name("...", &at), "task-1127-0200");
        assert_eq!(
            worktree_name("a~b^c", &Wall::new(2026, 1, 5, 23, 9)),
            "a-b-c-0105-2309"
        );
        let long = "很".repeat(60);
        assert_eq!(
            worktree_name(&long, &at).chars().count(),
            40 + "-1127-0200".len()
        );
    }
}
