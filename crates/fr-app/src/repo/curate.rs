//! 整理提交：把当前分支上某个提交之后的那些提交理一遍——调顺序、改说明、几个并成一个、停下来改
//! 内容、一个拆成几个、丢掉。
//!
//! 整理占满 Git 页的主区域（和「比较」「找回提交」一样是一种视图）：左边是清单，新的在上，和提交
//! 列表一个顺序，行首选怎么处理，拖动换顺序；右边是选中的那个提交改了什么（和提交详情共用一份
//! 数据），边看边定。下面写整理后剩几个提交、要不要强制推送、别的分支跟不跟着动。
//!
//! 排好以后交给 `git rebase -i`，清单怎么送进去见 [`fr_core::rebase`]。中途停下来有三种：
//! - 冲突：和普通的变基一样，工具栏下面出横幅，解决后继续。
//! - 停下修改：git 放上那个提交就停（清单里的 `edit`），改了暂存，继续时并进去。
//! - 拆分：也是 `edit`，停下后 Kuai 再做一次 `git reset HEAD^`，那个提交的改动回到「本地修改」，
//!   分几次提交完再继续。哪些提交要拆分记在变基目录里（见 [`rebase::Paused::split`]），重开 Kuai
//!   也认得。

use std::collections::{HashMap, HashSet};

use fr_core::i18n::{tr, tr_key, trc};
use fr_core::rebase::{self, Action, Candidate, Item, Paused, Problem};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::ops::Step;
use super::{Mode, RepoView};
use crate::ui;

/// 一次最多整理这么多个提交：再多清单里也看不过来
const MAX_COMMITS: usize = 200;
/// 清单一栏最窄、最宽多少
const LIST_MIN: f32 = 380.;
const LIST_MAX: f32 = 600.;
/// 一并移动别的分支要的 git 版本（清单里的 `update-ref`）
const UPDATE_REFS_GIT: (u32, u32, u32) = (2, 38, 0);

/// 对一个提交做什么（界面上的选项；改说明的新内容在那一行自己的输入框里）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Pick,
    Reword,
    Squash,
    Fixup,
    Edit,
    Split,
    Drop,
}

impl Kind {
    const ALL: [Kind; 7] = [
        Kind::Pick,
        Kind::Reword,
        Kind::Squash,
        Kind::Fixup,
        Kind::Edit,
        Kind::Split,
        Kind::Drop,
    ];

    fn label(self) -> &'static str {
        match self {
            Kind::Pick => tr("保留"),
            Kind::Reword => tr("修改说明"),
            Kind::Squash => tr("并入下方提交"),
            Kind::Fixup => tr("并入下方，丢弃说明"),
            Kind::Edit => tr("停下修改"),
            Kind::Split => tr("拆分"),
            // 英文里别处的「丢弃」是 Discard（丢掉没提交的改动），变基的这一项叫 Drop
            Kind::Drop => trc("变基", "丢弃"),
        }
    }

    /// 菜单里的说明。
    fn detail(self) -> &'static str {
        tr(match self {
            Kind::Pick => tr_key("保留（pick）：保持原样"),
            Kind::Reword => tr_key("修改说明（reword）：改动不变，仅修改提交说明"),
            Kind::Squash => tr_key("并入下方提交（squash）：保留两者的说明"),
            Kind::Fixup => tr_key("并入下方提交并丢弃说明（fixup）：仅保留下方提交的说明"),
            Kind::Edit => tr_key("停下修改（edit）：整理到此提交时暂停，修改内容后继续"),
            Kind::Split => tr_key("拆分：整理到此提交时暂停，改动放回「本地修改」，分几次重新提交"),
            Kind::Drop => tr_key("丢弃（drop）：删除此提交及其改动"),
        })
    }

    fn melds(self) -> bool {
        matches!(self, Kind::Squash | Kind::Fixup)
    }
}

struct Row {
    commit: Candidate,
    kind: Kind,
    /// 改说明时的输入框
    message: Option<Entity<TextareaState>>,
}

/// 打开整理时先替用户设好的：多选后「压缩为一个」、在一个提交上点右键只改它。
#[derive(Default)]
pub(super) enum Preset {
    #[default]
    None,
    /// 这几个提交设成「并入下方提交」
    Squash(HashSet<String>),
    /// 这一个提交设成这种处理方式，并选中它
    One(String, Kind),
}

/// 整理提交的视图（见模块说明）。
pub(super) struct Curate {
    /// 起点：它之后的提交才动
    base: String,
    base_subject: String,
    /// 要整理的提交，新的在前
    rows: Vec<Row>,
    /// 打开时的顺序，用来看是不是什么都没改；开始前核对分支上还是这几个
    original: Vec<String>,
    /// 已推送的提交（在某个远程分支上）：标出来，整理后推送要强推
    pushed: HashSet<String>,
    /// 每个提交改了几个文件
    counts: HashMap<String, usize>,
    /// 右边正在看哪个提交
    selected: Option<String>,
    problem: Option<SharedString>,
    /// 范围里的提交上还有别的本地分支：（提交，分支名）
    others: Vec<(String, String)>,
    /// 这些分支跟着一起移动
    move_others: bool,
    focus: FocusHandle,
}

/// 拖着排序时跟着鼠标的那一块。
#[derive(Clone)]
struct DragRow {
    index: usize,
    subject: SharedString,
}

impl Render for DragRow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2()
            .py_1()
            .max_w(px(360.))
            .rounded(px(ui::RADIUS))
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_sm()
            .truncate()
            .child(self.subject.clone())
    }
}

/// 拆分停下时：要拆的那个提交改了哪些文件、原来的说明（见 [`RepoView::split_left`]）。
pub(super) struct Split {
    /// 清单里写的那个提交（重放之前的哈希）
    pub stopped: String,
    pub files: Vec<String>,
}

impl Curate {
    /// 排好的清单，旧的在前（git 要的顺序）。
    fn items(&self, cx: &App) -> Vec<Item> {
        self.rows
            .iter()
            .rev()
            .map(|row| Item {
                id: row.commit.id.clone(),
                action: match row.kind {
                    Kind::Pick => Action::Pick,
                    Kind::Squash => Action::Squash,
                    Kind::Fixup => Action::Fixup,
                    Kind::Edit => Action::Edit,
                    Kind::Split => Action::Split,
                    Kind::Drop => Action::Drop,
                    Kind::Reword => Action::Reword(
                        row.message
                            .as_ref()
                            .map(|state| state.read(cx).value().to_string())
                            .unwrap_or_else(|| row.commit.message.clone()),
                    ),
                },
            })
            .collect()
    }

    /// 什么都没改：全是「保留」，顺序也没动。
    fn untouched(&self) -> bool {
        self.rows.iter().all(|row| row.kind == Kind::Pick)
            && self
                .rows
                .iter()
                .map(|row| &row.commit.id)
                .eq(self.original.iter())
    }

    /// 第 `ix` 行要并进哪个提交：它下面第一个没丢、自己也不是「并入」的。
    fn meld_target(&self, ix: usize) -> Option<&Candidate> {
        self.rows[ix + 1..]
            .iter()
            .find(|row| !row.kind.melds() && row.kind != Kind::Drop)
            .map(|row| &row.commit)
    }
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

impl RepoView {
    /// 提交右键「整理此后的提交…」：整理 `base` 之后到 HEAD 的提交。
    pub(super) fn interactive_rebase(
        &mut self,
        base: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_curate(base, Preset::None, window, cx);
    }

    /// 同 [`Self::interactive_rebase`]；`squash` 里的提交一打开就设成「并入下方提交」（提交列表里
    /// 多选以后「压缩为一个」用）。
    pub(super) fn interactive_rebase_with(
        &mut self,
        base: String,
        squash: HashSet<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_curate(base, Preset::Squash(squash), window, cx);
    }

    /// 「整理待推送的 N 个提交…」：起点取上游，是最常用也最安全的范围。
    pub(super) fn curate_unpushed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let upstream = self
            .status
            .as_ref()
            .and_then(|status| status.upstream.clone());
        match upstream {
            Some(upstream) => {
                self.open_curate(format!("refs/remotes/{upstream}"), Preset::None, window, cx)
            }
            None => {
                let text =
                    tr("当前分支未设置上游。请在提交列表中右键起点提交，选择「整理此后的提交…」。");
                window.push_notification(Notification::info(text), cx);
            }
        }
    }

    /// 在一个提交上点右键只改它：从它的父提交开始整理，它那一行先设成 `kind`。
    pub(super) fn curate_one(
        &mut self,
        id: String,
        kind: Kind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_curate(format!("{id}^"), Preset::One(id, kind), window, cx);
    }

    /// 打开整理视图：读 `base`（提交、分支都行）之后到 HEAD 的提交。先确认 `base` 在当前分支的
    /// 历史上、范围里没有合并提交。
    pub(super) fn open_curate(
        &mut self,
        base: String,
        preset: Preset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(running) = &self.op {
            window.push_notification(
                Notification::warning(tf!("请等待「{running}」完成后重试", running)),
                cx,
            );
            return;
        }
        if let Some(state) = self.in_progress {
            let text = tf!("{name}尚未完成，请先完成或中止。", name = state.name());
            window.push_notification(Notification::warning(text), cx);
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let commit = format!("{base}^{{commit}}");
        let resolve = channel.git(&repo, args(&["rev-parse", "-q", "--verify", &commit]));
        let is_ancestor = channel.git(&repo, args(&["merge-base", "--is-ancestor", &base, "HEAD"]));
        let list = channel.git(&repo, rebase::list_args(&base));
        // 没推送过的：其余的已推送
        let range = format!("{base}..HEAD");
        let unpushed = channel.git(
            &repo,
            args(&["rev-list", &range, "--not", "--remotes", "--"]),
        );
        // 每个提交改了几个文件：`\x01哈希` 一行，后面是它的文件，一行一个
        let counts = channel.git(
            &repo,
            args(&["log", "--format=%x01%H", "--name-only", &range, "--"]),
        );
        let subject = channel.git(&repo, args(&["log", "-1", "--format=%s", &base, "--"]));
        let can_move = channel
            .git_version()
            .is_some_and(|version| version >= UPDATE_REFS_GIT);
        cx.spawn_in(window, async move |this, cx| {
            let (resolve, is_ancestor, list, unpushed, counts, subject) =
                futures::join!(resolve, is_ancestor, list, unpushed, counts, subject);
            let text = |out: anyhow::Result<fr_channel::GitOutput>| {
                out.and_then(|out| out.ok())
                    .map(|out| String::from_utf8_lossy(&out).into_owned())
                    .ok()
            };
            let unpushed: Option<HashSet<String>> =
                text(unpushed).map(|out| out.lines().map(|line| line.trim().to_string()).collect());
            let counts = text(counts)
                .map(|out| parse_counts(&out))
                .unwrap_or_default();
            let subject = text(subject).unwrap_or_default().trim().to_string();
            let base_id = text(resolve).map(|out| out.trim().to_string());
            this.update_in(cx, |this, window, cx| {
                let on_branch = is_ancestor.is_ok_and(|out| out.success());
                let commits = list
                    .and_then(|out| out.ok())
                    .map(|out| rebase::parse_list(&out))
                    .unwrap_or_default();
                // 清单里的 pick 重放不了合并提交（见 fr_core::rebase）
                let merges = commits.iter().filter(|c| c.parents > 1).count();
                let Some(base_id) = base_id.filter(|_| on_branch) else {
                    let text = tr("此提交不在当前分支上。请先切换到包含该提交的分支。");
                    window.push_notification(Notification::warning(text).autohide(false), cx);
                    return;
                };
                if commits.is_empty() {
                    let text = tr("此提交之后没有其他提交，无需整理。");
                    window.push_notification(Notification::info(text), cx);
                    return;
                }
                if merges > 0 {
                    this.curate_has_merges(&commits, merges, window, cx);
                    return;
                }
                if commits.len() > MAX_COMMITS {
                    let text = tf!(
                        "此提交之后有 {n} 个提交，一次最多整理 {max} 个。请选择更近的提交。",
                        n = commits.len(),
                        max = MAX_COMMITS
                    );
                    window.push_notification(Notification::warning(text).autohide(false), cx);
                    return;
                }
                let original: Vec<String> = commits.iter().map(|c| c.id.clone()).collect();
                // 读不出来就当都没推送（不标）
                let pushed: HashSet<String> = match &unpushed {
                    Some(unpushed) => original
                        .iter()
                        .filter(|id| !unpushed.contains(*id))
                        .cloned()
                        .collect(),
                    None => HashSet::new(),
                };
                // 范围里的提交上别的本地分支（当前分支自己会跟着走，不算）
                let others: Vec<(String, String)> = this
                    .refs
                    .branches
                    .iter()
                    .filter(|branch| !branch.current && original.contains(&branch.target))
                    .map(|branch| (branch.target.clone(), branch.name.clone()))
                    .collect();
                let mut selected = original.first().cloned();
                let rows = commits
                    .into_iter()
                    .map(|commit| {
                        let kind = match &preset {
                            Preset::Squash(ids) if ids.contains(&commit.id) => Kind::Squash,
                            Preset::One(id, kind) if *id == commit.id => *kind,
                            _ => Kind::Pick,
                        };
                        Row {
                            commit,
                            kind,
                            message: None,
                        }
                    })
                    .collect();
                if let Preset::One(id, _) = &preset {
                    selected = Some(id.clone());
                }
                let mut curate = Curate {
                    base: base_id,
                    base_subject: subject,
                    rows,
                    original,
                    pushed,
                    counts,
                    selected: None,
                    problem: None,
                    others,
                    move_others: can_move,
                    focus: cx.focus_handle(),
                };
                // 预设成「修改说明」的那一行要有输入框
                for row in &mut curate.rows {
                    if row.kind == Kind::Reword {
                        row.message = Some(message_input(&row.commit.message, false, window, cx));
                    }
                }
                curate.focus.focus(window, cx);
                this.curate = Some(curate);
                this.blame = None;
                this.sidebar_select_history();
                this.mode = Mode::Curate;
                if let Some(id) = selected {
                    this.curate_select(id, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 范围里有合并提交：不整理，说明原因；最后一次合并之后还有提交的，给「只整理合并之后的 N 个」。
    fn curate_has_merges(
        &mut self,
        commits: &[Candidate],
        merges: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 新的在前：第一个合并提交前面的那些，就是最后一次合并之后的
        let after = commits.iter().position(|c| c.parents > 1).unwrap_or(0);
        let text = tf!(
            "此范围内有 {merges} 个合并提交，整理时无法保留合并关系。",
            merges
        );
        if after == 0 {
            let text = format!("{text}{}", tr("请选择最后一次合并之后的提交作为起点。"));
            window.push_notification(Notification::warning(text).autohide(false), cx);
            return;
        }
        let merge = commits[after].id.clone();
        let view = cx.entity().downgrade();
        let note = ui::action_note(NotificationType::Warning, text, move |_, _, cx| {
            let (view, merge, me) = (view.clone(), merge.clone(), cx.entity().downgrade());
            Button::new("curate-after-merge")
                .small()
                .primary()
                .label(tf!("只整理合并之后的 {n} 个", n = after))
                .on_click(move |_, window, cx| {
                    me.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                    let merge = merge.clone();
                    view.update(cx, |this, cx| {
                        this.open_curate(merge, Preset::None, window, cx)
                    })
                    .ok();
                })
        });
        window.push_notification(note, cx);
    }

    /// 清单里选中一个提交：右边看它的改动。
    fn curate_select(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(curate) = &mut self.curate else {
            return;
        };
        if curate.selected.as_deref() == Some(id.as_str()) {
            return;
        }
        curate.selected = Some(id.clone());
        self.show_object(id, cx);
        cx.notify();
    }

    fn curate_set_kind(
        &mut self,
        ix: usize,
        kind: Kind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(curate) = &mut self.curate else {
            return;
        };
        let Some(row) = curate.rows.get_mut(ix) else {
            return;
        };
        row.kind = kind;
        if kind == Kind::Reword && row.message.is_none() {
            row.message = Some(message_input(&row.commit.message, true, window, cx));
        }
        curate.problem = None;
        let id = row.commit.id.clone();
        self.curate_select(id, cx);
        cx.notify();
    }

    /// 把第 `from` 行挪到第 `to` 行前面（`to` 等于行数就是挪到最下面）。
    fn curate_move(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let Some(curate) = &mut self.curate else {
            return;
        };
        if from >= curate.rows.len() || to > curate.rows.len() || from == to || from + 1 == to {
            return;
        }
        let row = curate.rows.remove(from);
        let to = if from < to { to - 1 } else { to };
        curate.rows.insert(to, row);
        curate.problem = None;
        cx.notify();
    }

    /// 全部恢复为保留，顺序也回到打开时的。
    fn curate_reset(&mut self, cx: &mut Context<Self>) {
        let Some(curate) = &mut self.curate else {
            return;
        };
        let order = curate.original.clone();
        curate
            .rows
            .sort_by_key(|row| order.iter().position(|id| *id == row.commit.id));
        for row in &mut curate.rows {
            row.kind = Kind::Pick;
        }
        curate.problem = None;
        cx.notify();
    }

    /// 离开整理视图，回到提交历史，什么都不做。
    pub(super) fn close_curate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.curate = None;
        self.clear_commit_selection(cx);
        self.go_to_head(window, cx);
        cx.notify();
    }

    /// 在别处换了视图（点了侧栏）：清单不留着，下次进来重新读。画之前调。
    pub(super) fn drop_stale_curate(&mut self) {
        if self.mode != Mode::Curate && self.curate.is_some() {
            self.curate = None;
        }
        if self.mode == Mode::Curate && self.curate.is_none() {
            self.mode = Mode::History;
        }
    }

    fn curate_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(curate) = &self.curate else {
            return;
        };
        if !curate.focus.is_focused(window) {
            return;
        }
        let count = curate.rows.len();
        let at = curate
            .selected
            .as_ref()
            .and_then(|id| curate.rows.iter().position(|row| row.commit.id == *id));
        let modifiers = event.keystroke.modifiers;
        let down = match event.keystroke.key.as_str() {
            "down" => true,
            "up" => false,
            "escape" => {
                cx.stop_propagation();
                self.close_curate(window, cx);
                return;
            }
            _ => return,
        };
        cx.stop_propagation();
        let Some(at) = at else {
            return;
        };
        let next = match down {
            true if at + 1 < count => at + 1,
            false if at > 0 => at - 1,
            _ => return,
        };
        // Alt 加上下键：挪这一行
        if modifiers.alt {
            self.curate_move(at, if down { next + 1 } else { next }, cx);
            return;
        }
        let id = curate.rows[next].commit.id.clone();
        self.curate_select(id, cx);
    }

    /// 「开始整理」：清单没问题就交给 git。
    fn start_curate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(curate) = &mut self.curate else {
            return;
        };
        // 什么都没改就点开始：变基一遍也是原样，不做了
        if curate.untouched() {
            let note = tr("顺序和处理方式均未更改，未执行。");
            window.push_notification(Notification::info(note), cx);
            self.close_curate(window, cx);
            return;
        }
        let items = curate.items(cx);
        if let Err(problem) = rebase::check(&items) {
            curate.problem = Some(
                match problem {
                    Problem::NothingLeft => tr("不能丢弃全部提交，至少保留一个。"),
                    Problem::NothingToMeldInto => {
                        tr("最早的提交无法「并入下方提交」：下方没有提交。")
                    }
                    Problem::EmptyMessage => tr("有提交选择了「修改说明」，但说明为空。"),
                }
                .into(),
            );
            cx.notify();
            return;
        }
        let moved: Vec<(String, String)> = match curate.move_others {
            true => curate
                .others
                .iter()
                .map(|(id, name)| (id.clone(), format!("refs/heads/{name}")))
                .collect(),
            false => Vec::new(),
        };
        let (base, original) = (curate.base.clone(), curate.original.clone());
        self.curate = None;
        self.clear_commit_selection(cx);
        self.sidebar_select_history();
        self.run_curate(base, items, original, moved, window, cx);
    }

    /// `original` 是打开时读到的那几个提交（新的在前，和 `git rev-list` 一样）：开始之前核对现在还是
    /// 这几个。整理视图开着时别处又提交了，清单里没有新的那个，照旧清单变基它就从分支上掉了
    /// （reflog 里还找得到）。
    fn run_curate(
        &mut self,
        base: String,
        items: Vec<Item>,
        original: Vec<String>,
        moved: Vec<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 参数里总带着 --autostash：不按打开时的状态决定，开着的这一会儿工作区可能变了
        let step = Step::input(rebase::args(&base), rebase::input(&items, &moved))
            .shown(rebase::shown(&base));
        // 没有停下来的那几种时，做完剩几个提交是算得出来的
        let kept = items
            .iter()
            .filter(|item| matches!(item.action, Action::Pick | Action::Reword(_)))
            .count();
        let listed = vec!["rev-list".into(), format!("{base}..HEAD"), "--".into()];
        let checks = vec![(listed, Some(original.join("\n")))];
        // 一并移动的分支撤销时也要退回去
        let refs = moved
            .iter()
            .map(|(id, name)| (name.clone(), id.clone()))
            .collect();
        let point = self.undo_point().map(|point| point.with_refs(refs));
        self.verify_then(checks, window, cx, move |this, window, cx| {
            this.run_seq(
                tr("整理提交"),
                vec![step],
                window,
                cx,
                move |this, _, window, cx| {
                    let text = tf!("整理完成，该范围现有 {kept} 个提交", kept);
                    this.notify_undoable(text, point, window, cx);
                },
            );
        });
    }

    // ---------- 整理中途停下 ----------

    /// 变基停下了，而且不是因为冲突（清单里的 `edit`、`break`）：记下停在哪，回到「本地修改」。要拆分
    /// 的提交这时把改动放回工作区。git 这时的退出码是 0，由 `run_steps` 看变基目录还在才知道。
    pub(super) fn rebase_paused(
        &mut self,
        paused: Paused,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.in_progress = Some(fr_core::state::InProgress::Rebase);
        self.curate = None;
        self.show_changes(cx);
        let pending = paused.split_pending();
        let stopped = paused.stopped.as_deref().map(short).unwrap_or_default();
        let amend = paused.amend.is_some();
        self.rebase = Some(paused);
        if pending {
            self.begin_split(window, cx);
            return;
        }
        let text = match amend {
            true => tf!(
                "已停在 {id}。修改并暂存后，点击上方的「继续」，已暂存的改动将并入该提交。",
                id = stopped
            ),
            false => tr("变基已按清单暂停。处理完成后，点击上方的「继续」。").to_string(),
        };
        window.push_notification(Notification::info(text), cx);
    }

    /// 读到变基停在哪（刷新时调）：横幅按它写。要拆分的提交还没把改动放回工作区的，画的时候放回去
    /// （见 [`Self::sync_split`]，那里才有窗口）。
    fn apply_rebase_state(&mut self, paused: Option<Paused>, cx: &mut Context<Self>) {
        self.rebase = paused;
        let Some(paused) = &self.rebase else {
            self.split = None;
            self.split_started = None;
            return;
        };
        // 清单里的 `break`、命令没通过停下时没有 `REBASE_HEAD`，按伪引用认不出来
        if self.in_progress.is_none() {
            self.in_progress = Some(fr_core::state::InProgress::Rebase);
        }
        if paused.splitting() && !paused.split_pending() {
            self.load_split_files(cx);
        } else if !paused.splitting() {
            self.split = None;
        }
        cx.notify();
    }

    /// 重读变基停在哪（刷新时调）。HEAD 分离着（变基期间都是）或者认出了变基才读；别的时候不多起
    /// 一个进程。
    pub(super) fn read_rebase_state(&mut self, cx: &mut Context<Self>) {
        let detached = self
            .status
            .as_ref()
            .is_some_and(|status| status.branch.is_none() && status.head.is_some());
        let rebasing = self.in_progress == Some(fr_core::state::InProgress::Rebase);
        if !detached && !rebasing {
            self.rebase = None;
            self.split = None;
            self.split_started = None;
            self.rebase_task = None;
            self.rebase_reading = false;
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let read = channel.git(&self.entry.path, rebase::state_args());
        self.rebase_reading = true;
        self.rebase_task = Some(cx.spawn(async move |this, cx| {
            let paused = read
                .await
                .ok()
                .filter(|out| out.success())
                .and_then(|out| rebase::parse_state(&out.stdout));
            this.update(cx, |this, cx| {
                this.rebase_reading = false;
                // 写操作做完会再读一遍，这中间读到的是半截的状态
                if this.op.is_none() {
                    this.apply_rebase_state(paused, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// 画之前调：停在一个要拆分的提交上、改动还没放回工作区（在别处开始的整理，或者重开了 Kuai），
    /// 这时放回去。
    pub(super) fn sync_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self.rebase.as_ref().is_some_and(Paused::split_pending);
        let started =
            self.rebase.as_ref().and_then(|p| p.stopped.as_ref()) == self.split_started.as_ref();
        if !pending || started || self.op.is_some() || !self.visible {
            return;
        }
        cx.defer_in(window, |this, window, cx| this.begin_split(window, cx));
    }

    /// 拆分：要拆的提交已经放上去了，把它的改动放回工作区（`git reset HEAD^`），提交说明框里先填上
    /// 原来的说明。同一个提交只做一次：没做成（比如这时正在做别的操作）就等下一次刷新。
    fn begin_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(stopped) = self.rebase.as_ref().and_then(|p| p.stopped.clone()) else {
            return;
        };
        if self.op.is_some() || self.split_started.as_deref() == Some(stopped.as_str()) {
            return;
        }
        self.split_started = Some(stopped.clone());
        let reset = vec!["reset".into(), "-q".into(), "HEAD^".into()];
        self.run_op(
            tr("拆分"),
            reset,
            None,
            window,
            cx,
            move |this, _, window, cx| {
                if let Some(paused) = &mut this.rebase {
                    // 改动放回去以后 HEAD 退了一步：不等刷新，先记成已经放回
                    paused.head.clear();
                }
                this.show_changes(cx);
                this.load_split_files(cx);
                this.prefill_split_message(stopped.clone(), window, cx);
                let text = tf!(
                    "{id} 的改动已放回「本地修改」。分几次提交后，点击上方的「继续」。",
                    id = short(&stopped)
                );
                window.push_notification(Notification::info(text), cx);
            },
        );
    }

    /// 读要拆分的那个提交改了哪些文件：横幅上「还有几个文件没提交」按它数。
    fn load_split_files(&mut self, cx: &mut Context<Self>) {
        let Some(stopped) = self.rebase.as_ref().and_then(|p| p.stopped.clone()) else {
            return;
        };
        if self.split.as_ref().is_some_and(|s| s.stopped == stopped) {
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let args: Vec<String> = [
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "-z",
            &stopped,
        ]
        .map(String::from)
        .to_vec();
        let read = channel.git(&self.entry.path, args);
        self.split = Some(Split {
            stopped: stopped.clone(),
            files: Vec::new(),
        });
        cx.spawn(async move |this, cx| {
            let Some(out) = read.await.ok().filter(|out| out.success()) else {
                return;
            };
            let files: Vec<String> = out
                .stdout
                .split(|b| *b == 0)
                .filter(|name| !name.is_empty())
                .map(|name| String::from_utf8_lossy(name).into_owned())
                .collect();
            this.update(cx, |this, cx| {
                if let Some(split) = &mut this.split
                    && split.stopped == stopped
                {
                    split.files = files;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// 拆分时，要拆的那个提交的文件里还有几个没提交完（工作区、暂存区里还有它的改动）。未跟踪的目录
    /// 在状态里只有一行（`dir/`），里面的文件按它算。
    pub(super) fn split_left(&self) -> usize {
        let (Some(split), Some(status)) = (&self.split, &self.status) else {
            return 0;
        };
        split
            .files
            .iter()
            .filter(|file| {
                status.entries.iter().any(|entry| {
                    entry.path == **file
                        || (entry.path.ends_with('/') && file.starts_with(entry.path.as_str()))
                })
            })
            .count()
    }

    /// 拆分时提交说明框里先填上原提交的说明，免得重写；框里已经有字就不动。
    fn prefill_split_message(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.changes.message_text(cx).is_empty() {
            return;
        }
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let args: Vec<String> = ["log", "-1", "--format=%B", &id, "--"]
            .map(String::from)
            .to_vec();
        let read = channel.git(&self.entry.path, args);
        cx.spawn_in(window, async move |this, cx| {
            let Some(out) = read.await.ok().filter(|out| out.success()) else {
                return;
            };
            let message = String::from_utf8_lossy(&out.stdout).trim().to_string();
            this.update_in(cx, |this, window, cx| {
                if !message.is_empty() && this.changes.message_text(cx).is_empty() {
                    this.set_draft(message, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    // ---------- 画 ----------

    /// 「所有提交」搜索行右边的入口：当前分支有待推送的提交时出现。
    pub(super) fn render_curate_entry(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let count = self.history.unpushed_count();
        let on_branch = self
            .status
            .as_ref()
            .is_some_and(|status| status.branch.is_some() && status.upstream.is_some());
        if count < 2 || !on_branch || self.in_progress.is_some() {
            return None;
        }
        // 窗口窄时只留图标
        let words = !super::compact_toolbar();
        Some(
            Button::new("curate-unpushed")
                .small()
                .ghost()
                .icon(ui::icon(IconName::ListOrdered))
                .when(words, |button| {
                    button.label(tf!("整理待推送的 {count} 个提交…", count))
                })
                .tooltip(tr(
                    "推送前整理提交：调整顺序、合并、修改说明、拆分（交互式变基）",
                ))
                .disabled(self.op.is_some())
                .on_click(cx.listener(|this, _, window, cx| this.curate_unpushed(window, cx)))
                .into_any_element(),
        )
    }

    /// 整理视图：上面一条（范围、取消、开始），中间左边清单、右边选中提交的改动，下面是会怎样。
    pub(super) fn render_curate(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(curate) = &self.curate else {
            return div().into_any_element();
        };
        let dirty = self.has_tracked_changes();
        let branch = self.current_label();
        let can_move = self
            .channel
            .as_ref()
            .and_then(|channel| channel.git_version())
            .is_some_and(|version| version >= UPDATE_REFS_GIT);
        let upstream = self.status.as_ref().and_then(|s| s.upstream.clone());
        let theme = cx.theme();
        let (muted, border, danger, warning, success, panel, active, hover, accent) = (
            theme.muted_foreground,
            theme.border,
            theme.danger,
            theme.warning,
            theme.success,
            theme.muted,
            theme.list_active,
            theme.list_hover,
            theme.primary,
        );
        let count = curate.rows.len();
        let content_width = f32::from(self.content_viewport().width);
        let list_width = (content_width * 0.46).clamp(LIST_MIN, LIST_MAX);
        let idle = self.op.is_none();

        let rows: Vec<AnyElement> = curate
            .rows
            .iter()
            .enumerate()
            .map(|(ix, row)| {
                let kind = row.kind;
                let id = row.commit.id.clone();
                let selected = curate.selected.as_deref() == Some(id.as_str());
                let dropped = kind == Kind::Drop;
                let view = cx.entity().downgrade();
                let drag = DragRow {
                    index: ix,
                    subject: row.commit.subject().to_string().into(),
                };
                let files = curate.counts.get(&id).copied();
                let head = h_flex()
                    .h(px(30.))
                    .gap_2()
                    .child(
                        ui::icon_small(IconName::GripVertical)
                            .flex_none()
                            .text_color(muted.opacity(0.6)),
                    )
                    // 不加悬停提示：提示出现在按钮下面，会盖住弹出的菜单
                    .child(
                        div().w(px(148.)).flex_none().child(
                            Button::new(("curate-kind", ix))
                                .xsmall()
                                .outline()
                                .w_full()
                                .label(kind.label())
                                .dropdown_caret(true)
                                .dropdown_menu(move |mut menu, _, _| {
                                    for choice in Kind::ALL {
                                        let view = view.clone();
                                        menu = menu.item(
                                            PopupMenuItem::new(choice.detail())
                                                .checked(choice == kind)
                                                .on_click(move |_, window, cx| {
                                                    view.update(cx, |this, cx| {
                                                        this.curate_set_kind(ix, choice, window, cx)
                                                    })
                                                    .ok();
                                                }),
                                        );
                                    }
                                    menu
                                }),
                        ),
                    )
                    .child(
                        div()
                            .w(px(62.))
                            .flex_none()
                            .font_family(crate::fonts::mono())
                            .text_xs()
                            .text_color(muted)
                            .child(short(&id)),
                    )
                    .when(curate.pushed.contains(&id), |line| {
                        line.child(
                            div()
                                .flex_none()
                                .px(px(5.))
                                .rounded(px(4.))
                                .border_1()
                                .border_color(warning.opacity(0.5))
                                .text_xs()
                                .text_color(warning)
                                .child(tr("已推送")),
                        )
                    })
                    .child(
                        ui::clipped(("curate-subject", ix), row.commit.subject().to_string())
                            .flex_1()
                            .text_sm()
                            .when(dropped, |d| d.line_through().text_color(muted)),
                    )
                    .when_some(files, |line, files| {
                        line.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(muted)
                                .child(tf!("{files} 个文件", files)),
                        )
                    });
                // 下面一小行：这样处理会怎样
                let note: Option<(SharedString, Hsla)> = match kind {
                    Kind::Squash | Kind::Fixup => Some(match curate.meld_target(ix) {
                        Some(target) => (
                            tf!(
                                "并入 {id}「{subject}」",
                                id = short(&target.id),
                                subject = target.subject()
                            )
                            .into(),
                            muted,
                        ),
                        None => (tr("下方没有可并入的提交").into(), danger),
                    }),
                    Kind::Edit => Some((
                        tr("整理到此提交时暂停：修改并暂存后继续，改动并入此提交").into(),
                        muted,
                    )),
                    Kind::Split => Some((
                        tr("整理到此提交时暂停：改动放回「本地修改」，分几次重新提交").into(),
                        muted,
                    )),
                    _ => None,
                };
                let note = note.map(|(text, color)| {
                    h_flex()
                        .pl(px(178.))
                        .pb(px(5.))
                        .gap(px(5.))
                        .text_xs()
                        .text_color(color)
                        .child(ui::icon_small(IconName::CornerDownRight).flex_none())
                        .child(ui::clipped(("curate-note", ix), text).flex_1())
                });
                let editor = row
                    .message
                    .as_ref()
                    .filter(|_| kind == Kind::Reword)
                    .map(|state| {
                        div()
                            .pl(px(178.))
                            .pr_1()
                            .pb_2()
                            // 在输入框里按下鼠标是要打字、选字：别让外面这一行当成点了这一行（会把
                            // 焦点抢回清单）或者拖起整行
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(Textarea::new(state).h(px(72.)))
                    });
                let select = id.clone();
                v_flex()
                    .id(("curate-row", ix))
                    .px_2()
                    .rounded(px(ui::RADIUS))
                    .border_t_2()
                    .border_color(gpui::transparent_black())
                    .when(selected, |d| d.bg(active))
                    .when(!selected, |d| d.hover(|s| s.bg(hover)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(curate) = &this.curate {
                            curate.focus.focus(window, cx);
                        }
                        this.curate_select(select.clone(), cx)
                    }))
                    .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
                    .drag_over::<DragRow>(move |style, _, _, _| style.border_color(accent))
                    .on_drop(cx.listener(move |this, drag: &DragRow, _, cx| {
                        this.curate_move(drag.index, ix, cx)
                    }))
                    .child(head)
                    .children(note)
                    .children(editor)
                    .into_any_element()
            })
            .collect();

        // 起点那一行：不动，拖到它上面就是挪到最下面
        let base_row = h_flex()
            .id("curate-base")
            .h(px(30.))
            .px_2()
            .gap_2()
            .border_t_2()
            .border_color(gpui::transparent_black())
            .text_xs()
            .text_color(muted)
            .drag_over::<DragRow>(move |style, _, _, _| style.border_color(accent))
            .on_drop(cx.listener(move |this, drag: &DragRow, _, cx| {
                this.curate_move(drag.index, count, cx)
            }))
            .child(div().w(px(170.)).flex_none().child(tr("起点，不改动")))
            .child(
                div()
                    .w(px(62.))
                    .flex_none()
                    .font_family(crate::fonts::mono())
                    .child(short(&curate.base)),
            )
            .child(ui::clipped("curate-base-subject", curate.base_subject.clone()).flex_1());

        let dropped = curate.rows.iter().filter(|r| r.kind == Kind::Drop).count();
        let melded = curate.rows.iter().filter(|r| r.kind.melds()).count();
        let splits = curate.rows.iter().filter(|r| r.kind == Kind::Split).count();
        let pauses = curate
            .rows
            .iter()
            .filter(|r| matches!(r.kind, Kind::Edit | Kind::Split))
            .count();
        let left = count - melded - dropped;
        let summary = match splits {
            0 => tf!("整理后 {left} 个提交", left),
            _ => tf!("整理后至少 {left} 个提交", left),
        };
        let summary_detail = tf!(
            "原 {count} 个，并入 {melded} 个，丢弃 {dropped} 个",
            count,
            melded,
            dropped
        );
        let pushed = curate.pushed.len();
        let line = |icon: IconName, color: Hsla, text: SharedString| {
            h_flex()
                .gap(px(6.))
                .items_start()
                .child(
                    ui::icon_small(icon)
                        .flex_none()
                        .mt(px(2.))
                        .text_color(color),
                )
                .child(div().flex_1().min_w_0().whitespace_normal().child(text))
        };
        let mut notes: Vec<AnyElement> = Vec::new();
        notes.push(match pushed {
            0 => line(
                IconName::CircleCheck,
                success,
                tf!("这 {count} 个提交均未推送，整理后照常推送。", count).into(),
            )
            .into_any_element(),
            _ => line(
                IconName::TriangleAlert,
                warning,
                tf!("其中 {pushed} 个已推送，整理后需要强制推送。", pushed).into(),
            )
            .into_any_element(),
        });
        if pauses > 0 {
            notes.push(
                line(
                    IconName::CirclePause,
                    muted,
                    tf!(
                        "整理将暂停 {pauses} 次，每次回到「本地修改」，处理完点击「继续」。",
                        pauses
                    )
                    .into(),
                )
                .into_any_element(),
            );
        }
        if !curate.others.is_empty() {
            let names: Vec<&str> = curate
                .others
                .iter()
                .map(|(_, name)| name.as_str())
                .collect();
            let names = names.join(tr("、"));
            notes.push(match can_move {
                true => h_flex()
                    .gap(px(6.))
                    .child(
                        Checkbox::new("curate-move-others")
                            .checked(curate.move_others)
                            .label(tf!(
                                "一并移动指向其中提交的分支 {names}（--update-refs）",
                                names
                            ))
                            .on_click(cx.listener(|this, on: &bool, _, cx| {
                                if let Some(curate) = &mut this.curate {
                                    curate.move_others = *on;
                                    cx.notify();
                                }
                            })),
                    )
                    .into_any_element(),
                false => line(
                    IconName::TriangleAlert,
                    warning,
                    tf!(
                        "这些分支将留在旧提交上：{names}（主机上的 git 低于 2.38，无法一并移动）",
                        names
                    )
                    .into(),
                )
                .into_any_element(),
            });
        }
        if dirty {
            notes.push(
                line(
                    IconName::Archive,
                    muted,
                    tr("未提交的改动将先贮藏，整理完成后自动恢复（--autostash）。").into(),
                )
                .into_any_element(),
            );
        }
        notes.push(
            line(
                IconName::ArchiveRestore,
                muted,
                tr("整理前的位置记在侧栏的「找回提交」中，完成后的通知里可直接撤销。").into(),
            )
            .into_any_element(),
        );

        // 更改起点：上游之后的、最近几个
        let base_menu = {
            let view = cx.entity().downgrade();
            let upstream = upstream.clone();
            move |mut menu: gpui_kit::component::menu::PopupMenu,
                  _: &mut Window,
                  _: &mut Context<gpui_kit::component::menu::PopupMenu>| {
                if let Some(upstream) = &upstream {
                    let (view, base) = (view.clone(), format!("refs/remotes/{upstream}"));
                    menu = menu.item(
                        PopupMenuItem::new(tf!("{upstream} 之后的提交（待推送）", upstream))
                            .on_click(move |_, window, cx| {
                                let base = base.clone();
                                view.update(cx, |this, cx| {
                                    this.open_curate(base, Preset::None, window, cx)
                                })
                                .ok();
                            }),
                    );
                }
                for n in [5usize, 10, 20, 50] {
                    let view = view.clone();
                    menu = menu.item(PopupMenuItem::new(tf!("最近 {n} 个提交", n)).on_click(
                        move |_, window, cx| {
                            view.update(cx, |this, cx| {
                                this.open_curate(format!("HEAD~{n}"), Preset::None, window, cx)
                            })
                            .ok();
                        },
                    ));
                }
                menu.separator()
                    .label(tr("也可在提交列表中右键任一提交，选择「整理此后的提交…」"))
            }
        };

        let range = tf!(
            "{branch} · {id}「{subject}」之后的 {count} 个提交",
            branch,
            id = short(&curate.base),
            subject = curate.base_subject.as_str(),
            count
        );
        let untouched = curate.untouched();
        let problem = curate.problem.clone();
        let focus = curate.focus.clone();
        let header = h_flex()
            .h(px(44.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex_none()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("整理提交")),
            )
            .child(
                ui::clipped("curate-range", range)
                    .flex_1()
                    .text_xs()
                    .text_color(muted),
            )
            .child(
                Button::new("curate-base-menu")
                    .small()
                    .ghost()
                    .label(tr("更改起点"))
                    .dropdown_caret(true)
                    .dropdown_menu(base_menu),
            )
            .child(
                Button::new("curate-reset")
                    .small()
                    .ghost()
                    .label(tr("全部恢复为保留"))
                    .disabled(untouched)
                    .on_click(cx.listener(|this, _, _, cx| this.curate_reset(cx))),
            )
            .child(
                Button::new("curate-cancel")
                    .small()
                    .label(tr("取消"))
                    .on_click(cx.listener(|this, _, window, cx| this.close_curate(window, cx))),
            )
            .child(
                Button::new("curate-start")
                    .small()
                    .primary()
                    .label(tr("开始整理"))
                    .disabled(!idle)
                    .on_click(cx.listener(|this, _, window, cx| this.start_curate(window, cx))),
            );

        let list = v_flex()
            .w(px(list_width))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(border)
            .child(
                div()
                    .h(px(26.))
                    .flex_none()
                    .px_3()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(border)
                    .text_xs()
                    .text_color(muted)
                    .child(ui::clipped(
                        "curate-hint",
                        tr("较新的提交在上。拖动调整顺序（Alt+↑↓），点击左侧按钮更改处理方式。"),
                    )),
            )
            .child(
                div()
                    .id("curate-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_1()
                    .py_1()
                    .bg(panel.opacity(0.25))
                    .track_focus(&focus)
                    .on_key_down(cx.listener(Self::curate_key))
                    .child(v_flex().children(rows).child(base_row)),
            );

        let footer = h_flex()
            .flex_none()
            .items_start()
            .px_3()
            .py_2()
            .gap_4()
            .border_t_1()
            .border_color(border)
            .text_xs()
            .child(
                v_flex()
                    .w(px((list_width - 28.).max(200.)))
                    .flex_none()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(summary),
                    )
                    .child(div().text_color(muted).child(summary_detail))
                    .children(problem.map(|problem| {
                        div()
                            .mt_1()
                            .text_color(danger)
                            .whitespace_normal()
                            .child(problem)
                    })),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(3.))
                    .text_color(muted)
                    .children(notes),
            );

        let preview = self.render_curate_preview(cx);
        v_flex()
            .size_full()
            .child(header)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(list)
                    .child(div().flex_1().min_w_0().h_full().child(preview)),
            )
            .child(footer)
            .into_any_element()
    }
}

/// 改说明用的输入框，先填上原来的说明；`focus` 时把键盘焦点放进去。
fn message_input(
    text: &str,
    focus: bool,
    window: &mut Window,
    cx: &mut Context<RepoView>,
) -> Entity<TextareaState> {
    let state = cx.new(|cx| TextareaState::new(window, cx));
    let text = text.to_string();
    state.update(cx, |state, cx| state.set_value(text, window, cx));
    if focus {
        state.focus_handle(cx).focus(window, cx);
    }
    state
}

/// 解析 `git log --format=%x01%H --name-only` 的输出：每个提交改了几个文件。
fn parse_counts(out: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    let mut current: Option<String> = None;
    for line in out.lines() {
        if let Some(id) = line.strip_prefix('\x01') {
            current = Some(id.trim().to_string());
            counts.insert(id.trim().to_string(), 0);
        } else if !line.trim().is_empty()
            && let Some(id) = &current
            && let Some(count) = counts.get_mut(id)
        {
            *count += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::parse_counts;

    #[test]
    fn 数每个提交改了几个文件() {
        let out = "\u{1}aaa\n\nsrc/a.rs\nsrc/b.rs\n\u{1}bbb\n\u{1}ccc\n\nREADME.md\n";
        let counts = parse_counts(out);
        assert_eq!(counts.get("aaa"), Some(&2));
        assert_eq!(counts.get("bbb"), Some(&0));
        assert_eq!(counts.get("ccc"), Some(&1));
    }
}
