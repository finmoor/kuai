//! 找回提交（原型见 `docs/design/reliability/` 的方向 A）：侧栏「工作树」下面
//! 一项，右边按时间列出 HEAD 和各本地分支移动过的每一步（reflog）：提交、修改提交、重置、变基、
//! 切换、拉取，和在 Kuai 里删掉的分支。不在任何分支上的标出来；选中一条，上面一条横幅说清它是
//! 怎么来的，「从这里新建分支…」把它留住（不动当前分支），下面是那个提交的详情和改动（和提交历史
//! 共用一块，见 [`super::history`]）。
//!
//! - 数据：`git log -g HEAD --branches`。HEAD 和某个分支上同一时刻、同一提交、同一句话的两条合成
//!   一条：分支那条说明动的是哪个分支、动之前在哪。变基中间的每一步（start、pick）在分支上的不列。
//! - 来自哪：Kuai 自己执行的命令记在活动记录里（[`super::activity`]），时间对得上的算 Kuai，其余算
//!   终端；活动记录开始之前的不知道，空着。
//! - 删掉的分支：git 删分支会连它的 reflog 一起删，所以 Kuai 删分支时把它指向哪记在这个仓库的
//!   `RepoMemo` 里（只留 30 天）。终端里删的分支没有名字，只能在零散的提交或 HEAD 的记录里找。
//! - 零散的提交：`git fsck` 找出没有任何记录指向的提交（丢弃的贮藏、没检出过就删掉的分支）。
//!   大仓库要跑一会，点了才找。
//! - 主机上的仓库照样经通道在主机上跑这些 git 命令，不用新协议。

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::detached::branch_name_from;
use super::sidebar::{SideItem, stash_label};
use super::{Mode, RepoView};
use crate::ui;

/// 最多读多少条 reflog（git 默认留 90 天，一般远到不了）
const MAX_ENTRIES: usize = 2000;
const ROW_HEIGHT: f32 = 28.;
const TIME_WIDTH: f32 = 92.;
const OP_WIDTH: f32 = 80.;
const FROM_WIDTH: f32 = 52.;
const SHA_WIDTH: f32 = 70.;
/// 加粗的那段（分支名）在句子里的位置：翻译好的句子按它拆开
const MARK: &str = "\u{1}";

/// 这一步做了什么。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Op {
    Commit,
    Amend,
    Merge,
    CherryPick,
    Revert,
    Reset,
    Rebase,
    Checkout,
    Pull,
    Fetch,
    NewBranch,
    Rename,
    /// 在 Kuai 里删掉的分支（Kuai 自己记的）
    Deleted,
    /// 零散的提交：没有任何记录指向它
    Loose,
    /// 零散的提交里认出来的丢弃的贮藏
    Stash,
    Other,
}

impl Op {
    fn label(self) -> &'static str {
        match self {
            Op::Commit => tr("提交"),
            Op::Amend => tr("修改提交"),
            Op::Merge => tr("合并"),
            Op::CherryPick => tr("挑拣"),
            Op::Revert => tr("还原"),
            Op::Reset => tr("重置"),
            Op::Rebase => tr("变基"),
            Op::Checkout => tr("切换"),
            Op::Pull => tr("拉取"),
            Op::Fetch => tr("获取"),
            Op::NewBranch => tr("新建分支"),
            Op::Rename => tr("改名"),
            Op::Deleted => tr("删除分支"),
            Op::Loose => tr("零散的提交"),
            Op::Stash => tr("丢弃的贮藏"),
            Op::Other => tr("其他"),
        }
    }

    fn color(self, cx: &App) -> Hsla {
        let theme = cx.theme();
        match self {
            Op::Commit | Op::NewBranch => theme.success,
            Op::Amend | Op::Rebase | Op::CherryPick | Op::Revert | Op::Loose | Op::Stash => {
                theme.warning
            }
            Op::Reset | Op::Deleted => theme.danger,
            Op::Merge | Op::Pull | Op::Fetch => theme.blue,
            Op::Checkout | Op::Rename | Op::Other => theme.muted_foreground,
        }
    }
}

/// 这一步是谁做的。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Source {
    Kuai,
    Terminal,
    /// 活动记录开始之前的，不知道
    Unknown,
}

/// reflog 里的一条（还没合并）。
#[derive(Clone, Debug, PartialEq)]
struct Raw {
    /// `HEAD` 或分支名
    reference: String,
    at: i64,
    id: String,
    message: String,
    subject: String,
}

/// 列表里的一步。
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Step {
    /// Unix 秒：reflog 记下它的时间；零散的提交是提交的时间
    at: i64,
    op: Op,
    /// 动的是哪个分支；只动了 HEAD 的（分离的 HEAD、切换）是 None
    branch: Option<String>,
    id: String,
    subject: String,
    /// 动之前在哪：提交号和它的说明
    old: Option<(String, String)>,
    /// reflog 里的那句话
    message: String,
    /// 分支上那句话和 HEAD 上的不一样时（变基完成）
    branch_message: Option<String>,
    /// 要找回的那个提交不在任何分支、远程分支、标签上
    off: bool,
    /// 提交已经被 git 清理了（删掉的分支记得太久）
    gone: bool,
    from: Source,
}

impl Step {
    /// 找回时要的是动之前的位置（重置前、修改前、变基前、离开分离的 HEAD 前）。
    fn before(&self) -> Option<&(String, String)> {
        let wanted = match self.op {
            Op::Reset | Op::Amend => true,
            Op::Rebase | Op::Pull => self.message.contains("finish"),
            Op::Checkout => checkout_ends(&self.message).is_some_and(|(from, _)| is_hash(from)),
            _ => false,
        };
        self.old.as_ref().filter(|_| wanted)
    }

    /// 要找回的提交。
    fn target(&self) -> &str {
        self.before().map_or(&self.id, |(id, _)| id)
    }

    fn target_subject(&self) -> &str {
        self.before().map_or(&self.subject, |(_, subject)| subject)
    }

    /// 动的是哪个分支（给人看）：没记分支的变基完成、放弃从那句话里取
    fn branch_label(&self) -> String {
        if let Some(branch) = &self.branch {
            return branch.clone();
        }
        match self.message.split_once("returning to ") {
            Some((_, to)) => short_name(to.trim()),
            None => "HEAD".into(),
        }
    }

    /// 新建分支时默认的名字（可能和已有的重名，用的时候再挑一个没占用的）。
    fn default_name(&self) -> String {
        let base = self
            .branch
            .clone()
            .unwrap_or_else(|| "head".into())
            .replace('/', "-");
        match (self.op, self.before()) {
            (Op::Deleted, _) => self.branch.clone().unwrap_or_default(),
            (Op::Stash, _) => branch_name_from(&stash_label(&self.subject), &self.id),
            (Op::Reset, Some(_)) => format!("{base}-before-reset"),
            (Op::Amend, Some(_)) => format!("{base}-before-amend"),
            (Op::Rebase | Op::Pull, Some(_)) => format!("{base}-before-rebase"),
            _ => branch_name_from(self.target_subject(), self.target()),
        }
    }

    /// 「内容」一栏：一句话，和其中加粗的那段（分支名）。句子里加粗的位置是 [`MARK`]。
    fn content(&self) -> (String, Option<String>) {
        let new = short(&self.id);
        let branch = self.branch_label();
        let ff = self.message.to_ascii_lowercase().ends_with("fast-forward");
        match self.op {
            Op::Reset => match &self.old {
                Some((old, _)) => (
                    tf!(
                        "{branch} 从 {old} 移到 {new}",
                        branch = MARK,
                        old = short(old),
                        new
                    ),
                    Some(branch),
                ),
                None => (tf!("{branch} 移到 {new}", branch = MARK, new), Some(branch)),
            },
            Op::Checkout => match checkout_ends(&self.message) {
                Some((from, to)) => (
                    tf!("从 {from} 切换到 {to}", from = short_name(from), to = MARK),
                    Some(short_name(to)),
                ),
                None => (self.message.clone(), None),
            },
            Op::Merge | Op::Pull | Op::Fetch if ff || self.op == Op::Fetch => (
                tf!("{branch} 快进到 {new}", branch = MARK, new),
                Some(branch),
            ),
            Op::Rebase | Op::Pull if self.message.contains("abort") => {
                (tf!("放弃变基，回到 {branch}", branch = MARK), Some(branch))
            }
            Op::Rebase | Op::Pull if self.message.contains("finish") => {
                match self.branch_message.as_deref().and_then(onto) {
                    Some(onto) => (
                        tf!("{branch} 变基到 {onto}", branch = MARK, onto = short(onto)),
                        Some(branch),
                    ),
                    None => (
                        tf!("{branch} 变基完成，位于 {new}", branch = MARK, new),
                        Some(branch),
                    ),
                }
            }
            Op::NewBranch => (
                tf!("新建 {branch}，指向 {new}", branch = MARK, new),
                Some(branch),
            ),
            Op::Rename => match rename_from(&self.message) {
                Some(from) => (
                    tf!("{old} 改名为 {branch}", old = from, branch = MARK),
                    Some(branch),
                ),
                None => (self.message.clone(), None),
            },
            Op::Deleted => (
                tf!("{branch}（删除时指向 {id}）", branch = MARK, id = new),
                Some(branch),
            ),
            Op::Stash => (stash_label(&self.subject), None),
            Op::Other => (self.message.clone(), None),
            _ => (self.subject.clone(), None),
        }
    }

    /// 选中时横幅上的说明。
    fn explain(&self, now: i64) -> String {
        if self.gone {
            return tr("这个提交已被 git 清理，无法找回。").into();
        }
        match self.op {
            Op::Loose => return tr("没有任何记录指向这个提交，git 约两周后会清理它。").into(),
            Op::Stash => {
                return tr("这是丢弃的贮藏，没有任何记录指向它，git 约两周后会清理它。").into();
            }
            _ => {}
        }
        let time = when(self.at, now);
        let state = match self.off {
            true => tr("已不在任何分支上"),
            false => tr("还在分支上"),
        };
        let branch = self.branch_label();
        let mut text = match (self.op, self.before()) {
            (Op::Reset, Some((old, _))) => tf!(
                "{time} 的重置把 {branch} 从 {old} 移到了 {new}，重置前的 {old} {state}。",
                time,
                branch,
                old = short(old),
                new = short(&self.id),
                state
            ),
            (Op::Amend, Some((old, _))) => tf!(
                "{time} 修改了提交，修改前的 {old} {state}。",
                time,
                old = short(old),
                state
            ),
            (Op::Rebase | Op::Pull, Some((old, _))) => tf!(
                "{time} 的变基把 {branch} 移到了 {new}，变基前的 {old} {state}。",
                time,
                branch,
                new = short(&self.id),
                old = short(old),
                state
            ),
            (Op::Checkout, Some((old, _))) => tf!(
                "{time} 离开了分离的 HEAD，离开前的 {old} {state}。",
                time,
                old = short(old),
                state
            ),
            (Op::Deleted, _) => tf!(
                "{time} 在 Kuai 里删除了分支 {branch}，它当时指向 {id}，{state}。",
                time,
                branch,
                id = short(&self.id),
                state
            ),
            _ => tf!("{id} {state}。", id = short(&self.id), state),
        };
        if self.off {
            text.push_str(tr("不在分支上的提交在记录里保留 30 天。"));
        }
        text
    }

    fn matches(&self, needle: &str) -> bool {
        needle.is_empty()
            || self.id.starts_with(needle)
            || self
                .old
                .as_ref()
                .is_some_and(|(id, _)| id.starts_with(needle))
            || self.subject.to_lowercase().contains(needle)
            || self.message.to_lowercase().contains(needle)
            || self
                .branch
                .as_ref()
                .is_some_and(|branch| branch.to_lowercase().contains(needle))
    }
}

/// 读的状态。
enum Load {
    Idle,
    Loading,
    Ready(Vec<Step>),
    Failed(SharedString),
}

/// 列表里的一行。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Line {
    /// `steps` 里的
    Step(usize),
    /// 「零散的提交」那一组的标题
    LooseHeader,
    /// `loose` 里的
    Loose(usize),
}

pub(super) struct Recover {
    load: Load,
    task: Option<Task<()>>,
    /// 零散的提交：点了「查找零散的提交」才读
    loose: Load,
    loose_task: Option<Task<()>>,
    /// 只看不在分支上的
    only_off: bool,
    search: Entity<InputState>,
    /// 搜索的字（小写）
    needle: String,
    /// 现在列出的行
    lines: Vec<Line>,
    selected: Option<Line>,
    /// 选中的是哪一步（提交、时间、操作）：重读以后下标会变，按它找回原来那一步
    picked: Option<(String, i64, Op)>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
}

impl Recover {
    pub(super) fn new(window: &mut Window, cx: &mut Context<RepoView>) -> (Self, Subscription) {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("搜索说明、哈希、分支…")));
        let subscription = cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.recover.needle = this.recover.search.read(cx).value().trim().to_lowercase();
                this.refilter_recover(cx);
            }
        });
        let recover = Self {
            load: Load::Idle,
            task: None,
            loose: Load::Idle,
            loose_task: None,
            only_off: false,
            search,
            needle: String::new(),
            lines: Vec::new(),
            selected: None,
            picked: None,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
        };
        (recover, subscription)
    }

    fn steps(&self) -> &[Step] {
        match &self.load {
            Load::Ready(steps) => steps,
            _ => &[],
        }
    }

    fn loose_steps(&self) -> &[Step] {
        match &self.loose {
            Load::Ready(steps) => steps,
            _ => &[],
        }
    }

    fn step(&self, line: Line) -> Option<&Step> {
        match line {
            Line::Step(ix) => self.steps().get(ix),
            Line::Loose(ix) => self.loose_steps().get(ix),
            Line::LooseHeader => None,
        }
    }

    fn key(&self, line: Option<Line>) -> Option<(String, i64, Op)> {
        let step = self.step(line?)?;
        Some((step.id.clone(), step.at, step.op))
    }

    fn off_count(&self) -> usize {
        let steps = self.steps().iter().chain(self.loose_steps());
        steps.filter(|step| step.off && !step.gone).count()
    }
}

impl RepoView {
    /// 换了界面语言：搜索框里的提示文字是建的时候定的，要重新设一遍。
    pub(super) fn retranslate_recover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.recover.search.update(cx, |state, cx| {
            state.set_placeholder(tr("搜索说明、哈希、分支…"), window, cx)
        });
    }

    /// 侧栏点了「找回提交」。
    pub(super) fn show_recover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_file_views(window, cx);
        self.sidebar.selected = SideItem::Recover;
        self.mode = Mode::Recover;
        self.recover.focus.focus(window, cx);
        // 下面的详情区刚才可能显示着提交历史里选的提交
        self.show_recover_details(cx);
        self.load_recover(cx);
        cx.notify();
    }

    /// 分支、HEAD 变了（提交、重置、在终端里做了什么）：正看着这一页就重读。
    pub(super) fn recover_refs_changed(&mut self, cx: &mut Context<Self>) {
        if self.mode == Mode::Recover {
            self.load_recover(cx);
        }
    }

    /// 读 reflog、Kuai 删掉的分支，算出哪些不在分支上。旧的列表先留着，读完再换。
    pub(super) fn load_recover(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        // 还没有提交的仓库：没有记录
        if self
            .status
            .as_ref()
            .is_some_and(|status| status.head.is_none())
        {
            self.recover.load = Load::Ready(Vec::new());
            self.refilter_recover(cx);
            return;
        }
        if matches!(self.recover.load, Load::Idle | Load::Failed(_)) {
            self.recover.load = Load::Loading;
        }
        let repo = self.entry.path.clone();
        let now = chrono::Utc::now().timestamp();
        let deleted: Vec<Step> = self
            .app
            .read(cx)
            .settings
            .memo(&self.entry)
            .map(|memo| {
                memo.deleted_branches
                    .iter()
                    .filter(|branch| now - branch.at < crate::settings::RepoMemo::KEEP_DELETED)
                    .map(|branch| deleted_step(&branch.name, &branch.id, branch.at))
                    .collect()
            })
            .unwrap_or_default();
        let (since, spans) = self.activity.spans();
        let max = format!("--max-count={MAX_ENTRIES}");
        let log = channel.git(
            &repo,
            [
                "log",
                "-g",
                "--no-color",
                "--date=unix",
                &max,
                "--format=%H%x00%gD%x00%gs%x00%s",
                "HEAD",
                "--branches",
                "--",
            ]
            .map(String::from)
            .to_vec(),
        );
        self.recover.task = Some(cx.spawn(async move |this, cx| {
            let out = match log.await.and_then(|out| out.ok()) {
                Ok(out) => out,
                Err(err) => {
                    let text = err.to_string();
                    this.update(cx, |this, cx| {
                        this.recover.load = Load::Failed(text.into());
                        this.refilter_recover(cx);
                    })
                    .ok();
                    return;
                }
            };
            let mut steps = merge(parse_reflog(&String::from_utf8_lossy(&out)));
            steps.extend(deleted);
            steps.sort_by_key(|step| std::cmp::Reverse(step.at));
            // 删掉的分支指着的提交：读出说明；读不到的已经被清理了
            let deleted_ids: Vec<String> = steps
                .iter()
                .filter(|step| step.op == Op::Deleted)
                .map(|step| step.id.clone())
                .collect();
            let mut missing = HashSet::new();
            let mut subjects = HashMap::new();
            if !deleted_ids.is_empty() {
                let input = deleted_ids.join("\n") + "\n";
                let args = [
                    "log",
                    "--no-color",
                    "--no-walk=unsorted",
                    "--ignore-missing",
                    "--stdin",
                    "--format=%H%x00%s",
                ];
                let check = channel.git_with_input(
                    &repo,
                    args.map(String::from).to_vec(),
                    Some(input.into_bytes()),
                );
                if let Ok(Ok(out)) = check.await.map(|out| out.ok()) {
                    subjects = parse_subjects(&String::from_utf8_lossy(&out));
                    missing = deleted_ids
                        .into_iter()
                        .filter(|id| !subjects.contains_key(id))
                        .collect();
                }
            }
            // 哪些不在任何分支、远程分支、标签上：要找回的那个提交和动之前的位置一起问
            let mut ids: Vec<&str> = Vec::new();
            for step in &steps {
                if !missing.contains(&step.id) {
                    ids.push(&step.id);
                }
                if let Some((old, _)) = &step.old {
                    ids.push(old);
                }
            }
            ids.sort_unstable();
            ids.dedup();
            let off: HashSet<String> = if ids.is_empty() {
                HashSet::new()
            } else {
                let input = ids.join("\n") + "\n";
                let args = [
                    "rev-list",
                    "--ignore-missing",
                    "--stdin",
                    "--not",
                    "--branches",
                    "--remotes",
                    "--tags",
                ];
                let request = channel.git_with_input(
                    &repo,
                    args.map(String::from).to_vec(),
                    Some(input.into_bytes()),
                );
                match request.await.and_then(|out| out.ok()) {
                    Ok(out) => parse_ids(&String::from_utf8_lossy(&out)),
                    Err(err) => {
                        let text = err.to_string();
                        this.update(cx, |this, cx| {
                            this.recover.load = Load::Failed(text.into());
                            this.refilter_recover(cx);
                        })
                        .ok();
                        return;
                    }
                }
            };
            for step in &mut steps {
                step.gone = step.op == Op::Deleted && missing.contains(&step.id);
                if step.op == Op::Deleted
                    && let Some(subject) = subjects.get(&step.id)
                {
                    step.subject = subject.clone();
                }
                step.off = off.contains(step.target());
                step.from = source(step, since, &spans);
            }
            steps.retain(shown);
            this.update(cx, |this, cx| {
                this.recover.load = Load::Ready(steps);
                this.refilter_recover(cx);
            })
            .ok();
        }));
    }

    /// 找零散的提交：`git fsck` 列出没有任何记录指向的提交，再读它们的说明。
    fn find_loose(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        self.recover.loose = Load::Loading;
        let fsck = channel.git(
            &repo,
            ["fsck", "--connectivity-only", "--dangling", "--no-progress"]
                .map(String::from)
                .to_vec(),
        );
        self.recover.loose_task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let out = fsck.await?.ok()?;
                let ids = parse_dangling(&String::from_utf8_lossy(&out));
                if ids.is_empty() {
                    return anyhow::Ok(Vec::new());
                }
                let input = ids.join("\n") + "\n";
                let args = [
                    "log",
                    "--no-color",
                    "--no-walk=unsorted",
                    "--stdin",
                    "--format=%H%x00%ct%x00%P%x00%s",
                ];
                let out = channel
                    .git_with_input(
                        &repo,
                        args.map(String::from).to_vec(),
                        Some(input.into_bytes()),
                    )
                    .await?
                    .ok()?;
                Ok(parse_loose(&String::from_utf8_lossy(&out)))
            }
            .await;
            this.update(cx, |this, cx| {
                this.recover.loose = match result {
                    Ok(steps) => Load::Ready(steps),
                    Err(err) => Load::Failed(err.to_string().into()),
                };
                this.refilter_recover(cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// 按开关、搜索重排列出的行；选中的那条还在就留着，不在了选第一条。
    pub(super) fn refilter_recover(&mut self, cx: &mut Context<Self>) {
        let recover = &mut self.recover;
        let keep = |step: &Step| {
            (!recover.only_off || (step.off && !step.gone)) && step.matches(&recover.needle)
        };
        let mut lines: Vec<Line> = recover
            .steps()
            .iter()
            .enumerate()
            .filter(|(_, step)| keep(step))
            .map(|(ix, _)| Line::Step(ix))
            .collect();
        let loose: Vec<Line> = recover
            .loose_steps()
            .iter()
            .enumerate()
            .filter(|(_, step)| keep(step))
            .map(|(ix, _)| Line::Loose(ix))
            .collect();
        if !loose.is_empty() {
            lines.push(Line::LooseHeader);
            lines.extend(loose);
        }
        recover.lines = lines;
        let had_pick = recover.picked.is_some();
        let again = recover.picked.as_ref().and_then(|(id, at, op)| {
            recover.lines.iter().copied().find(|line| {
                recover
                    .step(*line)
                    .is_some_and(|step| step.id == *id && step.at == *at && step.op == *op)
            })
        });
        let first = recover
            .lines
            .iter()
            .copied()
            .find(|line| *line != Line::LooseHeader);
        let selected = again.or(first);
        // 选中的还是原来那一步（只是下标变了）就不重读下面的详情
        let changed = again.is_none() || !had_pick;
        recover.selected = selected;
        recover.picked = recover.key(selected);
        if changed && self.mode == Mode::Recover {
            self.show_recover_details(cx);
        }
        cx.notify();
    }

    /// 下面的详情区显示选中的那条要找回的提交。
    fn show_recover_details(&mut self, cx: &mut Context<Self>) {
        let target = self
            .recover
            .selected
            .and_then(|line| self.recover.step(line))
            .filter(|step| !step.gone)
            .map(|step| step.target().to_string());
        match target {
            Some(id) => self.show_object(id, cx),
            None => self.clear_commit_selection(cx),
        }
    }

    fn select_recover(&mut self, line: Line, cx: &mut Context<Self>) {
        if line == Line::LooseHeader || self.recover.selected == Some(line) {
            return;
        }
        self.recover.selected = Some(line);
        self.recover.picked = self.recover.key(Some(line));
        self.show_recover_details(cx);
        cx.notify();
    }

    fn recover_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lines = &self.recover.lines;
        if lines.is_empty() {
            return;
        }
        let at = self
            .recover
            .selected
            .and_then(|line| lines.iter().position(|l| *l == line));
        let last = lines.len() - 1;
        let mut next = match event.keystroke.key.as_str() {
            "down" => at.map_or(0, |at| (at + 1).min(last)),
            "up" => at.map_or(0, |at| at.saturating_sub(1)),
            "home" => 0,
            "end" => last,
            _ => return,
        };
        // 跳过组标题
        if lines[next] == Line::LooseHeader {
            next = match event.keystroke.key.as_str() {
                "up" => next.saturating_sub(1),
                _ => (next + 1).min(last),
            };
        }
        cx.stop_propagation();
        self.recover
            .scroll
            .scroll_to_item(next, ScrollStrategy::Nearest);
        self.select_recover(lines[next], cx);
    }

    /// 在要找回的提交上新建分支：名字默认取原来的分支名或 `<分支>-before-reset` 这样的，
    /// 「创建后切换」默认不勾，当前分支不动。
    fn recover_branch(&mut self, line: Line, window: &mut Window, cx: &mut Context<Self>) {
        let Some(step) = self.recover.step(line) else {
            return;
        };
        let start = (step.target().to_string(), step.target_subject().to_string());
        let name = self.free_branch_name(&step.default_name());
        self.new_branch_at(Some(start), name, false, window, cx);
    }

    /// `base` 没被占用就用它，占用了在后面加 `-2`、`-3`……
    fn free_branch_name(&self, base: &str) -> String {
        let taken = |name: &str| self.refs.branches.iter().any(|b| b.name == name);
        if !taken(base) {
            return base.to_string();
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|name| !taken(name))
            .unwrap_or_default()
    }

    /// Kuai 刚删掉了这些分支（名字、删除时指向的提交）：记下来，「找回提交」里能按原名恢复。
    pub(super) fn remember_deleted(
        &mut self,
        branches: Vec<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        if branches.is_empty() {
            return;
        }
        let entry = self.entry.clone();
        let now = chrono::Utc::now().timestamp();
        self.app.update(cx, |app, cx| {
            app.edit_memo(&entry, cx, |memo| {
                for (name, id) in &branches {
                    memo.remember_deleted(name, id, now);
                }
            })
        });
    }

    /// 「找回提交」这一页的上半部分（下半部分的详情和改动是提交历史的，见 `render_history`）。
    pub(super) fn render_recover(&mut self, collapsed: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, danger) = (theme.border, theme.muted_foreground, theme.danger);
        let off_count = self.recover.off_count();
        let only_off = self.recover.only_off;
        let toolbar = h_flex()
            .h(px(40.))
            .flex_none()
            .px(px(12.))
            .gap_2()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex_none()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("找回提交")),
            )
            .child(
                ui::clipped(
                    "recover-intro",
                    tr("HEAD 和各分支移动过的记录（reflog），包括在终端里做的"),
                )
                .flex_1()
                .text_xs()
                .text_color(muted),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(2.))
                    .child(
                        Button::new("recover-all")
                            .xsmall()
                            .ghost()
                            .selected(!only_off)
                            .label(tr("全部"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.recover.only_off = false;
                                this.refilter_recover(cx);
                            })),
                    )
                    .child(
                        Button::new("recover-off")
                            .xsmall()
                            .ghost()
                            .selected(only_off)
                            .label(tf!("不在分支上的 {n}", n = off_count))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.recover.only_off = true;
                                this.refilter_recover(cx);
                            })),
                    ),
            )
            .child(
                div().w(px(240.)).flex_shrink(1.).min_w(px(120.)).child(
                    Input::new(&self.recover.search)
                        .small()
                        .cleanable(true)
                        .prefix(ui::icon(IconName::Search).text_color(muted)),
                ),
            );
        let header = h_flex()
            .h(px(24.))
            .flex_none()
            .px(px(12.))
            .gap_2()
            .border_b_1()
            .border_color(border)
            .text_size(px(11.))
            .font_weight(FontWeight::MEDIUM)
            .text_color(muted)
            .child(div().w(px(TIME_WIDTH)).flex_none().child(tr("时间")))
            .child(div().w(px(OP_WIDTH)).flex_none().child(tr("操作")))
            .child(div().flex_1().child(tr("内容")))
            .child(div().w(px(FROM_WIDTH)).flex_none().child(tr("来自")))
            .child(div().w(px(SHA_WIDTH)).flex_none().child(tr("提交")));
        let empty = |text: SharedString, color: Hsla| {
            div()
                .flex_1()
                .p_4()
                .text_sm()
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        let list = match &self.recover.load {
            Load::Idle | Load::Loading => empty(tr("正在读取记录…").into(), muted),
            Load::Failed(err) => empty(err.clone(), danger),
            Load::Ready(_) if self.recover.lines.is_empty() => {
                let text = match self.recover.needle.is_empty() && !only_off {
                    true => tr("没有记录"),
                    false => tr("没有匹配的记录"),
                };
                empty(text.into(), muted)
            }
            Load::Ready(_) => uniform_list(
                "recover-steps",
                self.recover.lines.len(),
                cx.processor(|this, range: Range<usize>, _window, cx| {
                    this.render_recover_rows(range, cx)
                }),
            )
            .track_scroll(&self.recover.scroll)
            .size_full()
            .into_any_element(),
        };
        let banner = self.render_recover_banner(cx);
        let steps = self.recover.steps().len();
        let status: SharedString = match &self.recover.load {
            Load::Ready(_) => tf!(
                "{n} 条记录，{off} 条不在分支上",
                n = steps + self.recover.loose_steps().len(),
                off = off_count
            )
            .into(),
            _ => "".into(),
        };
        let loose_button = match &self.recover.loose {
            Load::Idle | Load::Failed(_) => Some(
                Button::new("recover-find-loose")
                    .xsmall()
                    .ghost()
                    .icon(ui::icon(IconName::Search))
                    .label(tr("查找零散的提交"))
                    .tooltip(tr(
                        "用 git fsck 找出没有任何记录指向的提交（丢弃的贮藏、在终端里删掉的分支），大仓库要等一会",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.find_loose(cx))),
            ),
            _ => None,
        };
        let loose_note: Option<(SharedString, Hsla)> = match &self.recover.loose {
            Load::Loading => Some((tr("正在查找零散的提交…").into(), muted)),
            Load::Ready(steps) if steps.is_empty() => Some((tr("没有零散的提交").into(), muted)),
            Load::Failed(err) => Some((err.clone(), danger)),
            _ => None,
        };
        let status_bar = h_flex()
            .h(px(26.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_t_1()
            .border_color(border)
            .text_xs()
            .text_color(muted)
            .child(ui::clipped("recover-status", status).flex_1())
            .children(loose_note.map(|(text, color)| {
                ui::clipped("recover-loose-note", text)
                    .flex_shrink(1.)
                    .text_color(color)
            }))
            .children(loose_button)
            .child(
                Button::new("recover-toggle-details")
                    .xsmall()
                    .ghost()
                    .icon(ui::icon(if collapsed {
                        IconName::PanelBottomOpen
                    } else {
                        IconName::PanelBottomClose
                    }))
                    .label(if collapsed {
                        tr("展开详情")
                    } else {
                        tr("收起详情")
                    })
                    .tooltip(tr("收起或展开提交详情"))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_history_details(cx))),
            );
        v_flex()
            .flex_1()
            .min_h_0()
            .child(toolbar)
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h(px(78.))
                    .overflow_hidden()
                    .track_focus(&self.recover.focus)
                    .on_key_down(cx.listener(Self::recover_key))
                    .child(list),
            )
            .children(banner)
            .child(status_bar)
            .into_any_element()
    }

    fn render_recover_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.theme();
        let (muted, fg, selected_bg, hover, warning, border) = (
            theme.muted_foreground,
            theme.foreground,
            theme.list_active,
            theme.list_hover,
            theme.warning,
            theme.border,
        );
        let now = chrono::Utc::now().timestamp();
        range
            .filter_map(|ix| {
                let line = *self.recover.lines.get(ix)?;
                if line == Line::LooseHeader {
                    return Some(
                        h_flex()
                            .id(("recover-line", ix))
                            .w_full()
                            .h(px(ROW_HEIGHT))
                            .px(px(12.))
                            .gap_2()
                            .border_t_1()
                            .border_color(border)
                            .text_xs()
                            .text_color(muted)
                            .child(
                                div()
                                    .flex_none()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(tr("零散的提交")),
                            )
                            .child(ui::clipped(
                                ("recover-loose-intro", ix),
                                tr("没有任何记录指向它们，git 约两周后会清理"),
                            ))
                            .into_any_element(),
                    );
                }
                let step = self.recover.step(line)?;
                let color = step.op.color(cx);
                let (text, bold) = step.content();
                let content = h_flex().flex_1().min_w_0().gap_2().child(
                    div()
                        .min_w_0()
                        .flex_shrink_1()
                        .truncate()
                        .text_color(fg)
                        .child(rich(&text, bold.as_deref())),
                );
                let content = content.when(step.off && !step.gone, |content| {
                    content.child(
                        div()
                            .flex_none()
                            .px(px(6.))
                            .rounded(px(4.))
                            .border_1()
                            .border_dashed()
                            .border_color(warning.opacity(0.8))
                            .text_xs()
                            .text_color(warning)
                            .child(tr("不在任何分支上")),
                    )
                });
                let content = content.when(step.gone, |content| {
                    content.child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(muted)
                            .child(tr("已被清理")),
                    )
                });
                let from = match step.from {
                    Source::Kuai => "Kuai",
                    Source::Terminal => tr("终端"),
                    Source::Unknown => "",
                };
                let is_selected = self.recover.selected == Some(line);
                let menu = (!step.gone).then(|| {
                    self.object_menu(
                        step.target().to_string(),
                        step.target_subject().to_string(),
                        cx,
                    )
                });
                let row = h_flex()
                    .id(("recover-line", ix))
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .px(px(12.))
                    .gap_2()
                    .text_sm()
                    .when(is_selected, |row| row.bg(selected_bg))
                    .when(!is_selected, |row| row.hover(|style| style.bg(hover)))
                    .on_click(cx.listener(move |this, _, _, cx| this.select_recover(line, cx)))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, _, cx| this.select_recover(line, cx)),
                    )
                    .child(
                        div()
                            .w(px(TIME_WIDTH))
                            .flex_none()
                            .text_xs()
                            .text_color(muted)
                            .child(when(step.at, now)),
                    )
                    .child(
                        div().w(px(OP_WIDTH)).flex_none().child(
                            div()
                                .w(px(OP_WIDTH - 8.))
                                .py(px(1.))
                                .rounded(px(4.))
                                .bg(color.opacity(0.16))
                                .text_xs()
                                .text_center()
                                .text_color(color)
                                .truncate()
                                .child(step.op.label()),
                        ),
                    )
                    .child(content)
                    .child(
                        div()
                            .w(px(FROM_WIDTH))
                            .flex_none()
                            .text_xs()
                            .text_color(muted)
                            .child(from),
                    )
                    .child(
                        div()
                            .w(px(SHA_WIDTH))
                            .flex_none()
                            .text_xs()
                            .font_family(crate::fonts::mono())
                            .text_color(muted)
                            .child(short(&step.id).to_string()),
                    );
                Some(match menu {
                    Some(menu) => row.context_menu(menu).into_any_element(),
                    None => row.into_any_element(),
                })
            })
            .collect()
    }

    /// 选中一条时列表下面的横幅：说清它是怎么来的，和「从这里新建分支…」「⋯」。
    fn render_recover_banner(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let line = self.recover.selected?;
        let step = self.recover.step(line)?.clone();
        let theme = cx.theme();
        let (warning, secondary, fg, muted, border) = (
            theme.warning,
            theme.secondary,
            theme.foreground,
            theme.muted_foreground,
            theme.border,
        );
        let off = step.off && !step.gone;
        let now = chrono::Utc::now().timestamp();
        let label = match (step.op, step.before().is_some()) {
            (Op::Reset, true) => tr("从重置前的位置新建分支…"),
            (Op::Amend, true) => tr("从修改前的提交新建分支…"),
            (Op::Rebase | Op::Pull, true) => tr("从变基前的位置新建分支…"),
            (Op::Deleted, _) => tr("按原名恢复分支…"),
            _ => tr("从这里新建分支…"),
        };
        let menu = self.object_menu(
            step.target().to_string(),
            step.target_subject().to_string(),
            cx,
        );
        Some(
            h_flex()
                .flex_none()
                .px(px(12.))
                .py(px(8.))
                .gap_3()
                .border_t_1()
                .border_color(if off { warning.opacity(0.5) } else { border })
                .bg(if off {
                    warning.opacity(0.12)
                } else {
                    secondary
                })
                .child(
                    ui::icon(if off {
                        IconName::TriangleAlert
                    } else {
                        IconName::Info
                    })
                    .flex_none()
                    .text_color(if off { warning } else { muted }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .text_color(fg)
                        .whitespace_normal()
                        .child(step.explain(now)),
                )
                .when(!step.gone, |banner| {
                    banner
                        .child(
                            Button::new("recover-branch")
                                .small()
                                .with_variant(if off {
                                    ButtonVariant::Primary
                                } else {
                                    ButtonVariant::Default
                                })
                                .icon(ui::icon(IconName::GitBranchPlus))
                                .label(label)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.recover_branch(line, window, cx)
                                })),
                        )
                        .child(
                            Button::new("recover-more")
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::Ellipsis))
                                .tooltip(tr("比较、检出、打标签、重置到这里、复制哈希"))
                                .dropdown_menu(menu),
                        )
                })
                .into_any_element(),
        )
    }
}

// ---------- 读 git 的输出 ----------

/// 读 `git log -g --date=unix --format=%H%x00%gD%x00%gs%x00%s` 的输出。
fn parse_reflog(text: &str) -> Vec<Raw> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split('\0');
            let id = parts.next()?.trim();
            let selector = parts.next()?;
            let message = parts.next().unwrap_or_default();
            let subject = parts.next().unwrap_or_default();
            let (reference, at) = selector.rsplit_once("@{")?;
            let at = at.strip_suffix('}')?.parse::<i64>().ok()?;
            let reference = reference.strip_prefix("refs/heads/").unwrap_or(reference);
            (id.len() >= 7).then(|| Raw {
                reference: reference.to_string(),
                at,
                id: id.to_string(),
                message: message.to_string(),
                subject: subject.to_string(),
            })
        })
        .collect()
}

/// 两句 reflog 的话说的是同一件事：一样，或者都是变基、拉取时变基（HEAD 上写「returning to」，
/// 分支上写「onto」）。
fn same_kind(a: &str, b: &str) -> bool {
    a == b
        || (a.starts_with("rebase") && b.starts_with("rebase"))
        || (a.starts_with("pull") && b.starts_with("pull"))
}

/// HEAD 和各分支的记录合成一列，新的在前。分支上的一条在 HEAD 上有同一时刻、同一提交、同一件事的
/// 那条，就并进去：记下动的是哪个分支、分支动之前在哪。
fn merge(raws: Vec<Raw>) -> Vec<Step> {
    let mut refs: Vec<(String, Vec<Raw>)> = Vec::new();
    for raw in raws {
        match refs.iter_mut().find(|(name, _)| *name == raw.reference) {
            Some((_, list)) => list.push(raw),
            None => refs.push((raw.reference.clone(), vec![raw])),
        }
    }
    let old_of = |list: &[Raw], ix: usize| {
        list.get(ix + 1)
            .map(|raw| (raw.id.clone(), raw.subject.clone()))
    };
    let new_step = |raw: &Raw, branch: Option<String>, old| Step {
        at: raw.at,
        op: classify(&raw.message),
        branch,
        id: raw.id.clone(),
        subject: raw.subject.clone(),
        old,
        message: raw.message.clone(),
        branch_message: None,
        off: false,
        gone: false,
        from: Source::Unknown,
    };
    let head = refs
        .iter()
        .position(|(name, _)| name == "HEAD")
        .map(|ix| refs.remove(ix).1)
        .unwrap_or_default();
    let mut head_steps: Vec<Step> = head
        .iter()
        .enumerate()
        .map(|(ix, raw)| new_step(raw, None, old_of(&head, ix)))
        .collect();
    let mut steps = Vec::new();
    for (name, list) in &refs {
        for (ix, raw) in list.iter().enumerate() {
            let old = old_of(list, ix);
            let twin = head_steps.iter_mut().find(|step| {
                step.branch.is_none()
                    && step.at == raw.at
                    && step.id == raw.id
                    && same_kind(&step.message, &raw.message)
            });
            match twin {
                Some(step) => {
                    step.branch = Some(name.clone());
                    if old.is_some() {
                        step.old = old;
                    }
                    if step.message != raw.message {
                        step.branch_message = Some(raw.message.clone());
                    }
                }
                None => steps.push(new_step(raw, Some(name.clone()), old)),
            }
        }
    }
    // HEAD 的在前：同一秒里先列它（排序是稳定的）
    head_steps.extend(steps);
    head_steps.sort_by_key(|step| std::cmp::Reverse(step.at));
    head_steps
}

/// reflog 那句话的开头（冒号前）认出是什么操作。
fn classify(message: &str) -> Op {
    let (kind, rest) = message.split_once(':').unwrap_or((message, ""));
    let (kind, rest) = (kind.trim(), rest.trim());
    match kind {
        "commit" | "commit (initial)" | "am" => Op::Commit,
        "commit (amend)" => Op::Amend,
        "commit (merge)" => Op::Merge,
        "reset" => Op::Reset,
        "checkout" | "switch" => Op::Checkout,
        _ if kind.eq_ignore_ascii_case("branch") => {
            if rest.starts_with("Created from") {
                Op::NewBranch
            } else if rest.starts_with("renamed") {
                Op::Rename
            } else if rest.starts_with("Reset to") {
                Op::Reset
            } else {
                Op::Other
            }
        }
        _ if kind.starts_with("rebase") => Op::Rebase,
        _ if kind.starts_with("pull") => Op::Pull,
        _ if kind.starts_with("merge") => Op::Merge,
        _ if kind.starts_with("cherry-pick") => Op::CherryPick,
        _ if kind.starts_with("revert") => Op::Revert,
        _ if kind.starts_with("fetch") => Op::Fetch,
        _ => Op::Other,
    }
}

/// 变基（含拉取时变基）中间的一步：开始、挑拣、继续……不是完成、放弃。
fn intermediate(message: &str) -> bool {
    let kind = message.split_once(':').map_or(message, |(kind, _)| kind);
    let rebasing =
        kind.starts_with("rebase") || (kind.starts_with("pull") && kind.contains("rebase"));
    rebasing && !kind.contains("finish") && !kind.contains("abort")
}

/// `checkout: moving from A to B` 里的 A、B。
fn checkout_ends(message: &str) -> Option<(&str, &str)> {
    let rest = message.split_once("moving from ")?.1;
    let (from, to) = rest.rsplit_once(" to ")?;
    Some((from.trim(), to.trim()))
}

/// `rebase (finish): refs/heads/x onto <提交>` 里的提交。
fn onto(message: &str) -> Option<&str> {
    let onto = message.rsplit_once(" onto ")?.1.trim();
    (!onto.is_empty()).then_some(onto)
}

/// `Branch: renamed refs/heads/a to refs/heads/b` 里的 a。
fn rename_from(message: &str) -> Option<String> {
    let rest = message.split_once("renamed ")?.1;
    let (from, _) = rest.rsplit_once(" to ")?;
    Some(short_name(from.trim()))
}

fn is_hash(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// 分支名去掉 `refs/heads/`，完整的哈希只留前 8 位。
fn short_name(name: &str) -> String {
    if is_hash(name) {
        return short(name).to_string();
    }
    name.strip_prefix("refs/heads/").unwrap_or(name).to_string()
}

fn deleted_step(name: &str, id: &str, at: i64) -> Step {
    Step {
        at,
        op: Op::Deleted,
        branch: Some(name.to_string()),
        id: id.to_string(),
        subject: String::new(),
        old: None,
        message: String::new(),
        branch_message: None,
        off: false,
        gone: false,
        from: Source::Kuai,
    }
}

/// 读 `git log --format=%H%x00%s`：哈希 → 说明。
fn parse_subjects(out: &str) -> HashMap<String, String> {
    out.lines()
        .filter_map(|line| line.split_once('\0'))
        .map(|(id, subject)| (id.trim().to_string(), subject.to_string()))
        .collect()
}

/// 列不列这一步：
/// - 变基中间的每一步（start、pick）都在分支上的不列，结果在「变基完成」那一条里；
/// - 原地不动的重置（`git stash` 里会做一次 `reset --hard HEAD`）不列。
fn shown(step: &Step) -> bool {
    let idle_reset =
        step.op == Op::Reset && step.old.as_ref().is_some_and(|(old, _)| *old == step.id);
    !(intermediate(&step.message) && !step.off) && !idle_reset
}

fn parse_ids(out: &str) -> HashSet<String> {
    out.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// `git fsck --dangling` 里的提交（零散的树、文件不要）。
fn parse_dangling(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|line| line.trim().strip_prefix("dangling commit "))
        .map(|id| id.trim().to_string())
        .collect()
}

/// 读零散的提交：`%H%x00%ct%x00%P%x00%s`。贮藏的提交有两三个父提交、说明是 `WIP on …`、`On …`。
fn parse_loose(out: &str) -> Vec<Step> {
    let mut steps: Vec<Step> = out
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\0');
            let id = parts.next()?.trim();
            let at = parts.next()?.trim().parse::<i64>().ok()?;
            let parents = parts.next().unwrap_or_default().split_whitespace().count();
            let subject = parts.next().unwrap_or_default();
            let stash =
                parents >= 2 && (subject.starts_with("WIP on ") || subject.starts_with("On "));
            Some(Step {
                at,
                op: if stash { Op::Stash } else { Op::Loose },
                branch: None,
                id: id.to_string(),
                subject: subject.to_string(),
                old: None,
                message: String::new(),
                branch_message: None,
                off: true,
                gone: false,
                from: Source::Unknown,
            })
        })
        .collect();
    steps.sort_by_key(|step| std::cmp::Reverse(step.at));
    steps
}

/// 这一步是不是 Kuai 做的：Kuai 执行的命令（开始、结束的 Unix 秒）里有时间对得上的。主机的钟可能
/// 差一点，前后各放宽几秒。活动记录开始（`since`）之前的不知道。
fn source(step: &Step, since: i64, spans: &[(i64, i64)]) -> Source {
    const SLACK: i64 = 3;
    if step.op == Op::Deleted {
        return Source::Kuai;
    }
    if step.at < since - SLACK {
        return Source::Unknown;
    }
    match spans
        .iter()
        .any(|(start, end)| step.at >= start - SLACK && step.at <= end + SLACK)
    {
        true => Source::Kuai,
        false => Source::Terminal,
    }
}

/// 时间一栏：今天、昨天写几点，今年写月日，更早的写年月日。
fn when(at: i64, now: i64) -> String {
    use chrono::{Datelike as _, Local, TimeZone as _};
    let (Some(time), Some(today)) = (
        Local.timestamp_opt(at, 0).single(),
        Local.timestamp_opt(now, 0).single(),
    ) else {
        return String::new();
    };
    let hm = time.format("%H:%M").to_string();
    let days = (today.date_naive() - time.date_naive()).num_days();
    match days {
        0 => tf!("今天 {hm}", hm),
        1 => tf!("昨天 {hm}", hm),
        _ if time.year() == today.year() => time.format("%m-%d %H:%M").to_string(),
        _ => time.format("%Y-%m-%d").to_string(),
    }
}

/// 一句话里 [`MARK`] 的地方换成加粗的 `bold`：一段文字，放不下时整句一起截断。
fn rich(text: &str, bold: Option<&str>) -> StyledText {
    let Some(bold) = bold else {
        return StyledText::new(text.to_string());
    };
    let mut joined = String::new();
    let mut runs = Vec::new();
    for (ix, piece) in text.split(MARK).enumerate() {
        if ix > 0 {
            let start = joined.len();
            joined.push_str(bold);
            let style = HighlightStyle {
                font_weight: Some(FontWeight::SEMIBOLD),
                ..Default::default()
            };
            runs.push((start..joined.len(), style));
        }
        joined.push_str(piece);
    }
    StyledText::new(joined).with_highlights(runs)
}

#[cfg(test)]
mod tests {
    // 不用 `super::*`：那会把 gpui 的 `test` 宏也带进来，盖掉标准的 `#[test]`
    use super::{
        MARK, Op, Source, classify, deleted_step, intermediate, merge, parse_dangling, parse_loose,
        parse_reflog, parse_subjects, rename_from, shown, source,
    };

    const LOG: &str = concat!(
        "c3\0HEAD@{300}\0reset: moving to c1\0step 1\n",
        "c3\0feature@{300}\0reset: moving to c1\0step 1\n",
        "c2c2c2c\0HEAD@{200}\0commit: step 2\0step 2\n",
        "c2c2c2c\0feature@{200}\0commit: step 2\0step 2\n",
        "c1c1c1c\0HEAD@{100}\0checkout: moving from main to feature\0step 1\n",
        "c1c1c1c\0feature@{90}\0branch: Created from HEAD\0step 1\n",
    );

    #[test]
    fn 读记录_合并_head_和分支上的同一步() {
        // 前两行的哈希太短，不算
        let raws = parse_reflog(LOG);
        assert_eq!(raws.len(), 4);
        assert_eq!(raws[0].reference, "HEAD");
        assert_eq!(raws[1].reference, "feature");
        assert_eq!(raws[1].at, 200);
        let log = LOG.replace("c3\0", "c3c3c3c\0");
        let steps = merge(parse_reflog(&log));
        let ops: Vec<Op> = steps.iter().map(|s| s.op).collect();
        assert_eq!(ops, [Op::Reset, Op::Commit, Op::Checkout, Op::NewBranch]);
        // 重置：动的是 feature，动之前在 c2（分支自己的上一条）
        assert_eq!(steps[0].branch.as_deref(), Some("feature"));
        assert_eq!(steps[0].old.as_ref().map(|o| o.0.as_str()), Some("c2c2c2c"));
        assert_eq!(steps[0].target(), "c2c2c2c");
        assert_eq!(steps[0].default_name(), "feature-before-reset");
        // 切换只在 HEAD 上
        assert_eq!(steps[2].branch, None);
        assert_eq!(steps[3].branch.as_deref(), Some("feature"));
        let (text, bold) = steps[0].content();
        assert_eq!(text, format!("{MARK} 从 c2c2c2c 移到 c3c3c3c"));
        assert_eq!(bold.as_deref(), Some("feature"));
        let (text, bold) = steps[2].content();
        assert_eq!(text, format!("从 main 切换到 {MARK}"));
        assert_eq!(bold.as_deref(), Some("feature"));
    }

    #[test]
    fn 变基_head_和分支的话不一样也合并_中间步骤认得出() {
        let log = concat!(
            "n2n2n2n\0HEAD@{50}\0rebase (finish): returning to refs/heads/topic\0b\n",
            "n2n2n2n\0topic@{50}\0rebase (finish): refs/heads/topic onto 1111111111111111111111111111111111111111\0b\n",
            "n2n2n2n\0HEAD@{49}\0rebase (pick): b\0b\n",
            "1111111\0HEAD@{48}\0rebase (start): checkout main\0m\n",
            "o2o2o2o\0HEAD@{40}\0commit: b\0b\n",
            "o2o2o2o\0topic@{40}\0commit: b\0b\n",
        );
        let steps = merge(parse_reflog(log));
        assert_eq!(steps[0].op, Op::Rebase);
        assert_eq!(steps[0].branch.as_deref(), Some("topic"));
        assert_eq!(steps[0].target(), "o2o2o2o");
        assert_eq!(steps[0].default_name(), "topic-before-rebase");
        assert_eq!(steps[0].content().0, format!("{MARK} 变基到 11111111"));
        assert!(intermediate(&steps[1].message));
        assert!(intermediate(&steps[2].message));
        assert!(!intermediate(&steps[0].message));
        assert!(!intermediate("pull: Fast-forward"));
        assert!(intermediate("pull --rebase (pick): x"));
    }

    #[test]
    fn 认操作() {
        assert_eq!(classify("commit (amend): x"), Op::Amend);
        assert_eq!(classify("commit (initial): x"), Op::Commit);
        assert_eq!(classify("merge origin/main: Fast-forward"), Op::Merge);
        assert_eq!(classify("pull: Fast-forward"), Op::Pull);
        assert_eq!(
            classify("Branch: renamed refs/heads/a to refs/heads/b"),
            Op::Rename
        );
        assert_eq!(classify("branch: Reset to HEAD~1"), Op::Reset);
        assert_eq!(classify("cherry-pick: x"), Op::CherryPick);
        assert_eq!(classify("whatever"), Op::Other);
        assert_eq!(
            rename_from("Branch: renamed refs/heads/a to refs/heads/b").as_deref(),
            Some("a")
        );
    }

    #[test]
    fn 离开分离的_head_要找回的是离开前的提交() {
        let id = "4".repeat(40);
        let log = format!(
            "mmmmmmm\0HEAD@{{20}}\0checkout: moving from {id} to main\0main tip\n\
             {id}\0HEAD@{{10}}\0commit: keep me\0keep me\n"
        );
        let steps = merge(parse_reflog(&log));
        assert_eq!(steps[0].target(), id);
        assert_eq!(steps[0].default_name(), "keep-me");
        assert_eq!(steps[0].content().0, format!("从 44444444 切换到 {MARK}"));
    }

    #[test]
    fn 零散的提交_认出贮藏() {
        let out = "aaaaaaa\u{0}100\u{0}p1 p2 p3\u{0}On feature: wip\nbbbbbbb\u{0}200\u{0}p1\u{0}lost work\n";
        let steps = parse_loose(out);
        assert_eq!(steps[0].op, Op::Loose);
        assert_eq!(steps[1].op, Op::Stash);
        assert_eq!(steps[1].content().0, "wip");
        assert_eq!(
            parse_dangling("dangling blob x\ndangling commit abc\n"),
            ["abc".to_string()]
        );
        assert_eq!(parse_subjects("abc\0fix it\n")["abc"], "fix it");
        assert_eq!(steps[1].default_name(), "wip");
    }

    #[test]
    fn 原地不动的重置和变基中间的步骤不列() {
        let log = concat!(
            "aaaaaaa\0HEAD@{30}\0reset: moving to HEAD\0x\n",
            "aaaaaaa\0HEAD@{20}\0rebase (pick): x\0x\n",
            "bbbbbbb\0HEAD@{10}\0commit: y\0y\n",
        );
        let steps = merge(parse_reflog(log));
        assert!(!shown(&steps[0]));
        assert!(!shown(&steps[1]));
        assert!(shown(&steps[2]));
        let mut off = steps[1].clone();
        off.off = true;
        assert!(shown(&off));
    }

    #[test]
    fn 来自_kuai_还是终端() {
        let mut step = deleted_step("x", "abc", 500);
        assert_eq!(source(&step, 0, &[]), Source::Kuai);
        step.op = Op::Commit;
        assert_eq!(source(&step, 600, &[]), Source::Unknown);
        assert_eq!(source(&step, 400, &[(498, 499)]), Source::Kuai);
        assert_eq!(source(&step, 400, &[(100, 200)]), Source::Terminal);
    }
}
