//! 终端页（标题栏上的「终端」，Ctrl+Shift+A）：所有工作区里各个仓库的终端合在一起管。
//!
//! 仓库标签页里的终端页是它只看一个仓库的样子：两边是同一批终端（都在各台电脑的 kuaid 里，数据
//! 在 [`TermBoard`]）。设计见 `design/terminal-hub/`（方向 C，带固定）。
//!
//! 左边一行一个仓库，按工作区分组，行尾写着这个仓库有几个终端停下了、在跑、空着；右边顶上是
//! 选中的仓库的终端，排成标签，下面是选中那个的画面。新建、重启、结束都在这一页：行尾和标签栏上的
//! +、标签上的 ×、工具栏的「重启」、右键菜单。里面还在输出的终端，结束、重启前先问一句。
//!
//! 固定（只记在这台电脑上）：
//!
//! - 终端：侧栏最上面「已固定」一组里一行，同时常驻在标签栏最前面，换到哪个仓库都在。它结束了
//!   就不再固定；重启不算结束（按「哪个仓库的第几个」认，见 [`TermSlot`]）。
//! - 仓库：排在「已固定」里固定的终端后面，原来的工作区里不再列它。
//!
//! 画面有两种接法。平常是「只看」（[`TerminalView::watch`]）：不改终端的大小，敲的字照样写得进去。
//! 这一页自己开的（新建、重启）是正经接着的（[`TerminalView::new`]），大小跟着这一页的画面，也就和
//! 仓库的终端页一样大；选到别的终端再回来，就换成只看。
//!
//! 焦点：打开这一页时在侧栏上（↑↓ 选一行，Enter 前往，→ 进到终端里）；点画面、点标签、新开一个
//! 终端时进到终端里，之后的键都归里面的程序，只留 Ctrl+Shift+↑、↓ 沿着标签栏换终端。输入时画面
//! 带一圈主色的边框：Enter 在侧栏上是前往、在终端里是回车，得一眼看得出焦点在哪。
//!
//! 侧栏和仓库页的侧栏一样宽、一样的底色和字号，右边上面一条和终端页的工具栏一样高，下面全是画面：
//! 这样画面和仓库的终端页一样大，终端是几行几列就正好放得下，字不用缩小。

use std::time::{Duration, Instant};

use fr_core::i18n::{tr, trc};
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::machines::{WorkspaceKey, machine_of, split_relay};
use crate::repo::tool_button;
use crate::settings::{RepoEntry, TermSlot};
use crate::term_board::{RepoGroup, Row, TermBoard, TermId, TermState, free_number};
use crate::terminal::{self, TerminalView};
use crate::ui;

/// 侧栏、画面上面那一条的高度（和仓库页的工具栏一样）
const HEAD_HEIGHT: f32 = 52.;
/// 侧栏里一行的高度（和仓库终端页侧栏里的一行一样）
const ROW_HEIGHT: f32 = 28.;
/// 固定的终端不在列表里了，等这么久还不在才取消固定：重启时列表里会空一下
const UNPIN_AFTER: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Stopped,
    Running,
}

/// 要前往一个终端，或者一个仓库的终端页。
pub enum Go {
    Term(Row),
    Repo(RepoEntry),
}

/// 右边的画面。
struct Peek {
    slot: TermSlot,
    /// 接着的是 kuaid 里的哪个终端；自己开的、还没出现在列表里时不知道
    id: Option<TermId>,
    view: Entity<TerminalView>,
    /// 这一页自己开的：正经接着，大小跟着这里的画面。不是就是只看
    owned: bool,
}

/// 侧栏里能选中的一行。
#[derive(Clone, PartialEq)]
enum Pick {
    /// 固定的终端
    Term(TermSlot),
    Repo(RepoEntry),
}

type Repo = (RepoEntry, Vec<Row>);

/// 侧栏里一个工作区的那一组。
struct SideGroup {
    workspace: Option<WorkspaceKey>,
    collapsed: bool,
    repos: Vec<Repo>,
}

/// 侧栏里要画的东西（筛过了）。
struct Side {
    pinned_terms: Vec<Row>,
    pinned_repos: Vec<Repo>,
    groups: Vec<SideGroup>,
    /// 没筛之前：一共几个终端，几个已停下，几个运行中
    total: usize,
    stopped: usize,
    running: usize,
}

pub struct TermOverview {
    app: Entity<AppState>,
    board: Entity<TermBoard>,
    /// 侧栏里选中的仓库：标签栏后半截是它的终端，新终端开在它里面
    repo: Option<RepoEntry>,
    /// 右边显示的终端
    shown: Option<TermSlot>,
    /// 侧栏里选的是仓库那一行（不是「已固定」里的终端）。仓库里的终端都固定了的时候，选中它右边
    /// 显示的就是固定着的终端；高亮要留在仓库上，不然用 ↑↓ 走到这里会跳回「已固定」，过不去
    by_repo: bool,
    filter: Filter,
    peek: Option<Peek>,
    /// 这一页正在开（新建、重启）的终端：等它出现在列表里
    opening: Option<TermSlot>,
    /// 这一页说了要结束的终端：列表里还有它的那一小会儿当它不在了
    ending: Vec<TermId>,
    /// 这一页显示着
    visible: bool,
    /// 等着确认固定的终端是不是真的没了
    unpin: Option<Task<()>>,
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

fn rows_in(hub: &[RepoGroup]) -> impl Iterator<Item = &Row> {
    hub.iter()
        .flat_map(|group| &group.repos)
        .flat_map(|(_, rows)| rows)
}

fn repo_in<'a>(hub: &'a [RepoGroup], entry: &RepoEntry) -> Option<&'a Repo> {
    hub.iter()
        .flat_map(|group| &group.repos)
        .find(|(other, _)| other == entry)
}

fn row_in<'a>(hub: &'a [RepoGroup], slot: &TermSlot) -> Option<&'a Row> {
    let (_, rows) = repo_in(hub, &slot.entry)?;
    rows.iter().find(|row| row.number == slot.number)
}

/// 终端的右键菜单（标签上、侧栏里固定的那一行上）。
fn term_menu(
    view: WeakEntity<TermOverview>,
    slot: TermSlot,
    pinned: bool,
    repo: SharedString,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, _, _| {
        let at = |view: &WeakEntity<TermOverview>| (view.clone(), slot.clone());
        let (go, pin, new, attach, restart, end) = (
            at(&view),
            at(&view),
            at(&view),
            at(&view),
            at(&view),
            at(&view),
        );
        menu.item(PopupMenuItem::new(tr("前往")).on_click(move |_, _, cx| {
            go.0.update(cx, |this, cx| this.go_term(&go.1, cx)).ok();
        }))
        .item(
            PopupMenuItem::new(if pinned {
                tr("取消固定")
            } else {
                tr("固定")
            })
            .on_click(move |_, _, cx| {
                pin.0
                    .update(cx, |this, cx| this.toggle_pin_term(&pin.1, cx))
                    .ok();
            }),
        )
        .separator()
        .item(
            PopupMenuItem::new(tf!("在 {repo} 中新建终端", repo = repo.clone())).on_click(
                move |_, window, cx| {
                    new.0
                        .update(cx, |this, cx| {
                            this.new_terminal(new.1.entry.clone(), window, cx)
                        })
                        .ok();
                },
            ),
        )
        .item(
            PopupMenuItem::new(tr("发送图片或文件…")).on_click(move |_, window, cx| {
                attach
                    .0
                    .update(cx, |this, cx| this.attach(&attach.1, window, cx))
                    .ok();
            }),
        )
        .item(
            PopupMenuItem::new(tr("重启")).on_click(move |_, window, cx| {
                restart
                    .0
                    .update(cx, |this, cx| this.restart(&restart.1, window, cx))
                    .ok();
            }),
        )
        .separator()
        .item(
            PopupMenuItem::new(tr("结束")).on_click(move |_, window, cx| {
                end.0
                    .update(cx, |this, cx| this.end(&end.1, window, cx))
                    .ok();
            }),
        )
    }
}

/// 仓库那一行的右键菜单。
fn repo_menu(
    view: WeakEntity<TermOverview>,
    entry: RepoEntry,
    pinned: bool,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, _, _| {
        let at = |view: &WeakEntity<TermOverview>| (view.clone(), entry.clone());
        let (pin, new, go) = (at(&view), at(&view), at(&view));
        menu.item(
            PopupMenuItem::new(if pinned {
                tr("取消固定")
            } else {
                tr("固定到顶部")
            })
            .on_click(move |_, _, cx| {
                pin.0
                    .update(cx, |this, cx| this.toggle_pin_repo(&pin.1, cx))
                    .ok();
            }),
        )
        .separator()
        .item(
            PopupMenuItem::new(tr("新建终端")).on_click(move |_, window, cx| {
                new.0
                    .update(cx, |this, cx| this.new_terminal(new.1.clone(), window, cx))
                    .ok();
            }),
        )
        .item(
            PopupMenuItem::new(tr("前往仓库")).on_click(move |_, _, cx| {
                go.0.update(cx, |_, cx| cx.emit(Go::Repo(go.1.clone())))
                    .ok();
            }),
        )
    }
}

impl TermOverview {
    pub fn new(
        app: Entity<AppState>,
        board: Entity<TermBoard>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe_in(&board, window, |this, _, window, cx| {
            this.sync(window, cx);
            cx.notify();
        });
        Self {
            app,
            board,
            repo: None,
            shown: None,
            by_repo: false,
            filter: Filter::All,
            peek: None,
            opening: None,
            ending: Vec::new(),
            visible: false,
            unpin: None,
            focus: cx.focus_handle(),
            _observe: observe,
        }
    }

    /// 全部工作区里的仓库和各自的终端。这一页说了要结束的不算。
    fn hub(&self, cx: &App) -> Vec<RepoGroup> {
        let mut groups = self.board.read(cx).repos(cx);
        if self.ending.is_empty() {
            return groups;
        }
        for group in &mut groups {
            for (_, rows) in &mut group.repos {
                rows.retain(|row| !self.ending.contains(&row.id));
            }
            // 不在任何工作区里的仓库是因为有终端才列着的
            if group.workspace.is_none() {
                group.repos.retain(|(_, rows)| !rows.is_empty());
            }
        }
        groups.retain(|group| !group.repos.is_empty());
        groups
    }

    fn find(&self, slot: &TermSlot, cx: &App) -> Option<Row> {
        row_in(&self.hub(cx), slot).cloned()
    }

    /// 右边显示的是哪个终端（它算看着的）。
    pub fn shown_id(&self, cx: &App) -> Option<TermId> {
        let slot = self.shown.as_ref()?;
        Some(self.find(slot, cx)?.id)
    }

    fn matches(&self, row: &Row) -> bool {
        match self.filter {
            Filter::All => true,
            Filter::Stopped => row.state == TermState::Stopped,
            Filter::Running => row.state == TermState::Running,
        }
    }

    fn pinned_term(&self, slot: &TermSlot, cx: &App) -> bool {
        let hub = &self.app.read(cx).settings.term_hub;
        hub.pinned_terms.contains(slot)
    }

    fn pinned_repo(&self, entry: &RepoEntry, cx: &App) -> bool {
        let hub = &self.app.read(cx).settings.term_hub;
        hub.pinned_repos.contains(entry)
    }

    /// 侧栏里要画的：最上面固定的终端、固定的仓库，下面各个工作区里的仓库。筛「已停下」「运行中」时
    /// 只留有那种终端的。
    fn side(&self, hub: &[RepoGroup], cx: &App) -> Side {
        let pins = &self.app.read(cx).settings.term_hub;
        let wanted =
            |rows: &[Row]| self.filter == Filter::All || rows.iter().any(|row| self.matches(row));
        let pinned_terms = pins
            .pinned_terms
            .iter()
            .filter_map(|slot| row_in(hub, slot))
            .filter(|row| self.matches(row))
            .cloned()
            .collect();
        let pinned_repos = pins
            .pinned_repos
            .iter()
            .filter_map(|entry| repo_in(hub, entry))
            .filter(|(_, rows)| wanted(rows))
            .cloned()
            .collect();
        let groups = hub
            .iter()
            .filter_map(|group| {
                // 固定了的仓库已经在上面，这里不再列
                let repos: Vec<Repo> = group
                    .repos
                    .iter()
                    .filter(|(entry, rows)| !pins.pinned_repos.contains(entry) && wanted(rows))
                    .cloned()
                    .collect();
                let collapsed = group
                    .workspace
                    .as_ref()
                    .is_some_and(|key| pins.collapsed.contains(key));
                (!repos.is_empty()).then(|| SideGroup {
                    workspace: group.workspace.clone(),
                    collapsed,
                    repos,
                })
            })
            .collect();
        let count = |state| rows_in(hub).filter(|row| row.state == state).count();
        Side {
            pinned_terms,
            pinned_repos,
            groups,
            total: rows_in(hub).count(),
            stopped: count(TermState::Stopped),
            running: count(TermState::Running),
        }
    }

    /// 侧栏里从上到下能选的行（收起的工作区里的不算）。
    fn picks(side: &Side) -> Vec<Pick> {
        let terms = side.pinned_terms.iter().map(|row| Pick::Term(row.slot()));
        let pinned = side.pinned_repos.iter();
        let listed = side
            .groups
            .iter()
            .filter(|group| !group.collapsed)
            .flat_map(|group| &group.repos);
        let repos = pinned
            .chain(listed)
            .map(|(entry, _)| Pick::Repo(entry.clone()));
        terms.chain(repos).collect()
    }

    /// 侧栏里高亮哪一行：显示着的是固定的终端（而且不是选仓库选出来的），就是「已固定」里它
    /// 那一行；不然是选中的仓库。
    fn highlight(&self, side: &Side) -> Option<Pick> {
        if !self.by_repo
            && let Some(slot) = &self.shown
            && side.pinned_terms.iter().any(|row| row.slot() == *slot)
        {
            return Some(Pick::Term(slot.clone()));
        }
        self.repo.clone().map(Pick::Repo)
    }

    /// 标签栏：固定的终端在前（换到哪个仓库都在），后面是选中的仓库里没固定的。
    fn tabs(&self, hub: &[RepoGroup], cx: &App) -> (Vec<Row>, Vec<Row>) {
        let pins = &self.app.read(cx).settings.term_hub.pinned_terms;
        let pinned = pins
            .iter()
            .filter_map(|slot| row_in(hub, slot))
            .cloned()
            .collect();
        let own = self
            .repo
            .as_ref()
            .and_then(|entry| repo_in(hub, entry))
            .map(|(_, rows)| {
                rows.iter()
                    .filter(|row| !pins.contains(&row.slot()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        (pinned, own)
    }

    /// 这一页显示出来了：先选中一个要看的（已停下的优先），焦点放到侧栏上。
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = true;
        let hub = self.hub(cx);
        let still = self
            .shown
            .as_ref()
            .is_some_and(|slot| row_in(&hub, slot).is_some());
        if !still && self.opening.is_none() {
            let first = rows_in(&hub)
                .find(|row| row.state == TermState::Stopped)
                .or(rows_in(&hub).next());
            self.shown = first.map(Row::slot);
            if let Some(row) = first {
                self.repo = Some(row.entry.clone());
            }
        }
        // 选中的仓库不在了（从工作区里移走了，或者那台服务器断开了）：换成第一个
        let listed = |entry: &RepoEntry| repo_in(&hub, entry).is_some();
        if !self.repo.as_ref().is_some_and(listed) {
            let first = hub.iter().flat_map(|group| &group.repos).next();
            self.repo = first.map(|(entry, _)| entry.clone());
        }
        self.sync(window, cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// 这一页不显示了：不用再接着那个终端。
    pub fn hidden(&mut self, cx: &mut Context<Self>) {
        self.visible = false;
        self.peek = None;
        cx.notify();
    }

    /// 焦点在右边的终端里：敲的字都归它。
    pub fn typing(&self, window: &Window, cx: &App) -> bool {
        self.peek
            .as_ref()
            .is_some_and(|peek| peek.view.focus_handle(cx).is_focused(window))
    }

    /// 让右边的画面跟上：显示的终端换了一个、改了大小、别处把它重启了就重新接；它没了就换成这个
    /// 仓库里别的。终端列表有变化、选了别的、开了新的以后都来一遍。
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 说了要结束的，列表里没有了就不用再记
        if !self.ending.is_empty() {
            let listed: Vec<TermId> = {
                let groups = self.board.read(cx).repos(cx);
                rows_in(&groups).map(|row| row.id.clone()).collect()
            };
            self.ending.retain(|id| listed.contains(id));
        }
        self.check_pins(cx);
        if !self.visible {
            return;
        }
        let typing = self.typing(window, cx);
        let hub = self.hub(cx);
        // 正在开的出现在列表里了
        if let Some(slot) = &self.opening
            && let Some(row) = row_in(&hub, slot)
        {
            if let Some(peek) = &mut self.peek
                && peek.owned
                && peek.slot == *slot
            {
                peek.id = Some(row.id.clone());
            }
            self.opening = None;
        }
        // 显示着的终端没了（在别处结束了）：换成这个仓库里别的
        let gone = |slot: &TermSlot| row_in(&hub, slot).is_none();
        if self.shown.as_ref().is_some_and(gone) && self.shown != self.opening {
            self.shown = self.first_in_repo(&hub, cx);
        }
        let Some(slot) = self.shown.clone() else {
            self.peek = None;
            // 正往里输入的那个终端没了：焦点回到侧栏上，不然键盘没人接
            if typing {
                self.focus.focus(window, cx);
            }
            return;
        };
        let Some(row) = row_in(&hub, &slot) else {
            // 正在开、列表里还没有：自己开的那个画面留着（没有就建），它负责去开
            let mine = |peek: &Peek| peek.owned && peek.slot == slot;
            if !self.peek.as_ref().is_some_and(mine) {
                self.open_view(slot, window, cx);
            }
            return;
        };
        let Some(conn) = self.board.read(cx).conn(&row.id.machine) else {
            self.peek = None;
            if typing {
                self.focus.focus(window, cx);
            }
            return;
        };
        let size = Some((row.cols.max(2), row.rows.max(1)));
        let same = self.peek.as_ref().is_some_and(|peek| peek.slot == slot);
        let current = self.peek.as_ref().is_some_and(|peek| {
            peek.slot == slot
                && peek.id.as_ref() == Some(&row.id)
                // 自己开的管着它的大小，不用跟
                && (peek.owned || peek.view.read(cx).size() == size)
        });
        if current {
            return;
        }
        let view =
            cx.new(|cx| TerminalView::watch(&conn, &row.id.term, row.cols, row.rows, window, cx));
        // 正往里输入时它改了大小、被别处重启了：换上新建的那个，焦点跟过去
        if typing {
            match same {
                true => view.focus_handle(cx).focus(window, cx),
                false => self.focus.focus(window, cx),
            }
        }
        self.peek = Some(Peek {
            slot,
            id: Some(row.id.clone()),
            view,
            owned: false,
        });
    }

    /// 选中的仓库里先显示哪个终端：没固定的第一个；都固定了就是第一个。
    fn first_in_repo(&self, hub: &[RepoGroup], cx: &App) -> Option<TermSlot> {
        let (_, rows) = repo_in(hub, self.repo.as_ref()?)?;
        let free = rows.iter().find(|row| !self.pinned_term(&row.slot(), cx));
        free.or(rows.first()).map(Row::slot)
    }

    /// 在侧栏里选中一个仓库：右边换成它的终端。
    fn select_repo(&mut self, entry: RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        let hub = self.hub(cx);
        // 已经显示着这个仓库里没固定的终端：不动
        let keep = self.shown.as_ref().is_some_and(|slot| {
            slot.entry == entry && !self.pinned_term(slot, cx) && row_in(&hub, slot).is_some()
        });
        self.repo = Some(entry);
        self.by_repo = true;
        if !keep {
            self.opening = None;
            self.shown = self.first_in_repo(&hub, cx);
        }
        self.sync(window, cx);
        cx.notify();
    }

    /// 显示一个终端（点标签、点侧栏里固定的那一行）。
    fn select_term(&mut self, slot: TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        if self.opening.as_ref() != Some(&slot) {
            self.opening = None;
        }
        self.repo = Some(slot.entry.clone());
        self.by_repo = false;
        self.shown = Some(slot);
        self.sync(window, cx);
        cx.notify();
    }

    fn pick(&mut self, pick: Pick, window: &mut Window, cx: &mut Context<Self>) {
        match pick {
            Pick::Term(slot) => self.select_term(slot, window, cx),
            Pick::Repo(entry) => self.select_repo(entry, window, cx),
        }
    }

    /// 焦点放到右边的终端里，开始输入。
    fn type_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(peek) = &self.peek {
            peek.view.focus_handle(cx).focus(window, cx);
            cx.notify();
        }
    }

    fn go_term(&mut self, slot: &TermSlot, cx: &mut Context<Self>) {
        if let Some(row) = self.find(slot, cx) {
            cx.emit(Go::Term(row));
        }
    }

    /// 前往显示着的终端；没有就前往选中的仓库的终端页。
    fn go(&mut self, cx: &mut Context<Self>) {
        match (self.shown.clone(), self.repo.clone()) {
            (Some(slot), _) => self.go_term(&slot, cx),
            (None, Some(entry)) => cx.emit(Go::Repo(entry)),
            (None, None) => {}
        }
    }

    fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.filter = filter;
        cx.notify();
    }

    fn toggle_group(&mut self, key: WorkspaceKey, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            app.edit_term_hub(cx, |hub| match hub.collapsed.contains(&key) {
                true => hub.collapsed.retain(|other| *other != key),
                false => hub.collapsed.push(key),
            })
        });
        cx.notify();
    }

    fn toggle_pin_term(&mut self, slot: &TermSlot, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            app.edit_term_hub(cx, |hub| match hub.pinned_terms.contains(slot) {
                true => hub.pinned_terms.retain(|other| other != slot),
                false => hub.pinned_terms.push(slot.clone()),
            })
        });
        cx.notify();
    }

    fn toggle_pin_repo(&mut self, entry: &RepoEntry, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            app.edit_term_hub(cx, |hub| match hub.pinned_repos.contains(entry) {
                true => hub.pinned_repos.retain(|other| other != entry),
                false => hub.pinned_repos.push(entry.clone()),
            })
        });
        cx.notify();
    }

    /// 固定的终端里，哪些已经不在了：那台电脑的终端列表读到了，里面却没有它。没连着的电脑上的
    /// 不算（连上以后回来）。
    fn missing_pins(&self, cx: &App) -> Vec<TermSlot> {
        let board = self.board.read(cx);
        let pins = &self.app.read(cx).settings.term_hub.pinned_terms;
        pins.iter()
            .filter(|slot| self.opening.as_ref() != Some(*slot))
            .filter(|slot| {
                board
                    .terms_of(&slot.entry, cx)
                    .is_some_and(|rows| rows.iter().all(|row| row.number != slot.number))
            })
            .cloned()
            .collect()
    }

    /// 固定的终端结束了就不再固定。重启时列表里会空一下，所以等一会儿再看一遍。
    fn check_pins(&mut self, cx: &mut Context<Self>) {
        if self.unpin.is_some() || self.missing_pins(cx).is_empty() {
            return;
        }
        self.unpin = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(UNPIN_AFTER).await;
            this.update(cx, |this, cx| {
                this.unpin = None;
                let gone = this.missing_pins(cx);
                if gone.is_empty() {
                    return;
                }
                this.app.update(cx, |app, cx| {
                    app.edit_term_hub(cx, |hub| {
                        hub.pinned_terms.retain(|slot| !gone.contains(slot))
                    })
                });
                cx.notify();
            })
            .ok();
        }));
    }

    /// 在一个仓库里新开一个终端，显示它，焦点进去。序号用这个仓库里最小的空号：先问一遍那台电脑的
    /// kuaid 现在有哪几个（它可能刚被拉起来，列表还没推过来）。
    pub fn new_terminal(&mut self, entry: RepoEntry, window: &mut Window, cx: &mut Context<Self>) {
        let connecting = self.app.update(cx, |app, cx| match &entry.server {
            Some(server) if !app.knows_server(server) => None,
            Some(server) => Some(app.connect(server, cx)),
            None => Some(app.connect_local(cx)),
        });
        let Some(connecting) = connecting else {
            let text = tf!("终端启动失败：{err}", err = tr("未找到该服务器的设置"));
            window.push_notification(Notification::error(text), cx);
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let listed = match connecting.await {
                Ok(conn) => conn.term_list().await.map_err(|err| format!("{err:#}")),
                Err(err) => Err(err.to_string()),
            };
            this.update_in(cx, |this, window, cx| match listed {
                Ok(terms) => {
                    let used: Vec<u32> = terms
                        .iter()
                        .filter_map(|term| terminal::session_number(&entry.path, &term.key))
                        .collect();
                    let slot = TermSlot {
                        entry,
                        number: free_number(&used),
                    };
                    this.open(slot, window, cx);
                }
                Err(err) => {
                    let text = tf!("终端启动失败：{err}", err);
                    window.push_notification(Notification::error(text), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// 在选中的仓库里新开一个终端（Ctrl+Shift+`）。
    pub fn new_terminal_here(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.repo.clone() {
            self.new_terminal(entry, window, cx);
        }
    }

    /// 开（或者重新开）这个位置上的终端，显示它，焦点进去。
    fn open(&mut self, slot: TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        self.repo = Some(slot.entry.clone());
        self.shown = Some(slot.clone());
        self.opening = Some(slot.clone());
        if self.visible {
            self.open_view(slot, window, cx);
        }
        cx.notify();
    }

    /// 建一个正经接着的画面：kuaid 里没有这个名字的终端就开一个。
    fn open_view(&mut self, slot: TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        let (entry, app, number) = (slot.entry.clone(), self.app.clone(), slot.number);
        let view = cx.new(|cx| {
            TerminalView::new(
                move |cx| {
                    let mut launch = crate::shell::terminal_launch(&entry, &app, number, "", cx)?;
                    // 退回到开在 Kuai 进程里的终端跟着画面走，这一页换一个终端它就结束了：不退
                    launch.fallback = None;
                    Ok(launch)
                },
                window,
                cx,
            )
        });
        view.focus_handle(cx).focus(window, cx);
        self.peek = Some(Peek {
            slot,
            id: None,
            view,
            owned: true,
        });
    }

    fn term_name(row: &Row) -> SharedString {
        match &row.title {
            Some(title) => title.clone().into(),
            None => tf!("终端 {n}", n = row.number.to_string()).into(),
        }
    }

    /// 里面还在输出的终端，结束、重启前先问一句（2026-11-18 Feng：「要二次确认」）。
    fn ask_first(
        &mut self,
        slot: &TermSlot,
        restart: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.find(slot, cx) else {
            return;
        };
        let run =
            move |this: &mut Self, slot: &TermSlot, window: &mut Window, cx: &mut Context<Self>| {
                match restart {
                    true => this.restart_now(slot, window, cx),
                    false => this.end_now(slot, window, cx),
                }
            };
        if !(row.alive && row.state == TermState::Running) {
            return run(self, slot, window, cx);
        }
        let repo = self.app.read(cx).repo_name(&row.entry);
        let name = Self::term_name(&row);
        let (title, text, ok) = match restart {
            true => (
                tr("重启终端？"),
                tf!(
                    "将终止 {repo} 中的终端「{name}」里正在运行的程序，并重新启动。此操作无法撤销。",
                    repo,
                    name
                ),
                tr("重启"),
            ),
            false => (
                tr("结束终端？"),
                tf!(
                    "将结束 {repo} 中的终端「{name}」，其中正在运行的程序将终止。此操作无法撤销。",
                    repo,
                    name
                ),
                tr("结束"),
            ),
        };
        let (this, slot) = (cx.entity().downgrade(), slot.clone());
        crate::manager::confirm_in(
            title.to_string(),
            text,
            ok,
            true,
            window,
            cx,
            move |window, cx| {
                this.update(cx, |this, cx| run(this, &slot, window, cx))
                    .ok();
            },
        );
    }

    /// 结束一个终端。里面还在输出的先问。
    fn end(&mut self, slot: &TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_first(slot, false, window, cx);
    }

    /// 结束一个终端里的程序，重新开一个，并显示它。里面还在输出的先问。
    fn restart(&mut self, slot: &TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_first(slot, true, window, cx);
    }

    /// 请 kuaid 结束这个终端。列表里还有它的那一小会儿当它不在了。
    fn close(&mut self, row: &Row, cx: &mut Context<Self>) -> Option<Task<()>> {
        let conn = self.board.read(cx).conn(&row.id.machine)?;
        let closing = conn.term_close(&row.id.term);
        self.ending.push(row.id.clone());
        Some(cx.background_executor().spawn(async move {
            if let Err(err) = closing.await {
                log::warn!("结束终端失败：{err:#}");
            }
        }))
    }

    fn end_now(&mut self, slot: &TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        let hub = self.hub(cx);
        let Some(row) = row_in(&hub, slot).cloned() else {
            return;
        };
        // 显示的是它：换到同一个仓库里旁边那个
        let next = (self.shown.as_ref() == Some(slot)).then(|| {
            let (_, rows) = repo_in(&hub, &slot.entry)?;
            let at = rows.iter().position(|other| other.number == slot.number)?;
            let beside = rows
                .get(at + 1)
                .or(at.checked_sub(1).and_then(|at| rows.get(at)));
            beside.map(Row::slot)
        });
        let Some(closing) = self.close(&row, cx) else {
            return;
        };
        closing.detach();
        if let Some(next) = next {
            self.shown = next;
        }
        if self.opening.as_ref() == Some(slot) {
            self.opening = None;
        }
        if self.peek.as_ref().is_some_and(|peek| peek.slot == *slot) {
            self.peek = None;
            self.focus.focus(window, cx);
        }
        // 固定着的：不用等列表推过来再取消
        if self.pinned_term(slot, cx) {
            self.toggle_pin_term(slot, cx);
        }
        self.sync(window, cx);
        cx.notify();
    }

    fn restart_now(&mut self, slot: &TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.find(slot, cx) else {
            return;
        };
        let Some(closing) = self.close(&row, cx) else {
            return;
        };
        // 先不显示画面：原来那个正在结束，等它结束了再开，不然接上的还是它
        self.repo = Some(slot.entry.clone());
        self.shown = Some(slot.clone());
        self.opening = Some(slot.clone());
        self.peek = None;
        self.focus.focus(window, cx);
        let slot = slot.clone();
        cx.spawn_in(window, async move |this, cx| {
            closing.await;
            this.update_in(cx, |this, window, cx| {
                // 等的这一会儿没换到别的终端上去
                if this.opening.as_ref() == Some(&slot) {
                    this.open(slot, window, cx);
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// 给一个终端选图片、文件发进去，并显示它。
    fn attach(&mut self, slot: &TermSlot, window: &mut Window, cx: &mut Context<Self>) {
        self.select_term(slot.clone(), window, cx);
        if let Some(peek) = &self.peek {
            peek.view.update(cx, |view, cx| view.pick_files(cx));
        }
    }

    /// 正往终端里输入时，抢在终端前面看一眼按键：Ctrl+Shift+↑、↓ 沿着标签栏换到上一个、下一个
    /// 终端，接着输入。
    fn on_key_first(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let mods = &event.keystroke.modifiers;
        let hop = mods.control && mods.shift && !mods.alt && !mods.platform;
        let down = match event.keystroke.key.as_str() {
            "down" => true,
            "up" => false,
            _ => return,
        };
        if !hop || !self.typing(window, cx) {
            return;
        }
        cx.stop_propagation();
        let (pinned, own) = self.tabs(&self.hub(cx), cx);
        let order: Vec<TermSlot> = pinned.iter().chain(&own).map(Row::slot).collect();
        let at = self
            .shown
            .as_ref()
            .and_then(|slot| order.iter().position(|other| other == slot));
        let next = match (at, down) {
            (Some(at), true) => at + 1,
            (Some(at), false) => match at.checked_sub(1) {
                Some(next) => next,
                None => return,
            },
            (None, _) => 0,
        };
        if let Some(slot) = order.get(next) {
            self.select_term(slot.clone(), window, cx);
            self.type_in(window, cx);
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // 焦点在右边的终端里时，它没要的键也不拿来换行、前往
        if !self.focus.is_focused(window) {
            return;
        }
        let side = self.side(&self.hub(cx), cx);
        let picks = Self::picks(&side);
        let at = self
            .highlight(&side)
            .and_then(|on| picks.iter().position(|pick| *pick == on));
        let step = |next: usize| picks.get(next).cloned();
        match event.keystroke.key.as_str() {
            "down" => {
                let next = at.map_or(0, |at| (at + 1).min(picks.len().saturating_sub(1)));
                if let Some(pick) = step(next) {
                    self.pick(pick, window, cx);
                }
            }
            "up" => {
                if let Some(pick) = step(at.map_or(0, |at| at.saturating_sub(1))) {
                    self.pick(pick, window, cx);
                }
            }
            "right" => self.type_in(window, cx),
            "enter" => self.go(cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    /// 状态前面的圆点：运行中是实心的主色，已停下是带一圈的警示色，空闲是空心的。
    fn dot(state: TermState, size: f32, cx: &App) -> Div {
        let theme = cx.theme();
        let dot = div().flex_none().size(px(size)).rounded_full();
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

    /// 鼠标停在一个终端上时的提示：叫什么、哪个仓库的、状态，停下了的写停了多久。
    fn term_tip(&self, row: &Row, cx: &App) -> String {
        let repo = self.app.read(cx).repo_name(&row.entry);
        let since = row
            .last_output
            .filter(|_| row.alive && row.state != TermState::Running)
            .map(|at| format!(" · {}", ago(at)))
            .unwrap_or_default();
        format!(
            "{}（{}，{}{}）",
            Self::term_name(row),
            repo,
            Self::state_text(row),
            since
        )
    }

    /// 它在哪台电脑上：经别的电脑连的写成「91，经由 156」。
    fn place(app: &AppState, entry: &RepoEntry) -> SharedString {
        let machine = machine_of(entry);
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

    /// 几个停下了、几个在跑、几个空着：仓库那一行的行尾、收起的工作区那一行上。
    fn counts(rows: &[Row], cx: &App) -> Div {
        let muted = cx.theme().muted_foreground;
        let count = |state| rows.iter().filter(|row| row.state == state).count();
        let states = [TermState::Stopped, TermState::Running, TermState::Idle];
        h_flex()
            .flex_none()
            .gap(px(7.))
            .text_size(px(11.))
            .font_weight(FontWeight::NORMAL)
            .text_color(muted)
            .children(states.into_iter().filter_map(|state| {
                let n = count(state);
                (n > 0).then(|| {
                    h_flex()
                        .gap(px(4.))
                        .child(Self::dot(state, 7., cx))
                        .child(n.to_string())
                })
            }))
    }

    /// 行尾鼠标移上去才出现的那几个按钮：盖在行尾原来的字上面（那些字这时藏起来）。悬停的样式只在
    /// 画的时候起作用、不改排版，所以按钮一直占着位置，只是平时不画；行尾的字要留出它们的宽度。
    fn row_actions() -> Div {
        h_flex()
            .absolute()
            .top_0()
            .bottom_0()
            .right(px(4.))
            .gap(px(1.))
            .invisible()
            .group_hover("hub-row", |s| s.visible())
    }

    /// 行尾鼠标移上去才出现的小按钮。
    fn row_button(id: impl Into<ElementId>, icon: IconName, cx: &App) -> Stateful<Div> {
        let theme = cx.theme();
        let (fg, muted, hover) = (
            theme.sidebar_foreground,
            theme.muted_foreground,
            theme.secondary,
        );
        div()
            .id(id)
            .flex_none()
            .size(px(22.))
            .rounded(px(5.))
            .flex()
            .items_center()
            .justify_center()
            .text_color(muted)
            .hover(|s| s.bg(hover).text_color(fg))
            .child(ui::icon_small(icon))
    }

    /// 侧栏里一行的壳：一样高、一样的圆角和选中、悬停的底色。
    fn row_shell(id: impl Into<ElementId>, on: bool, cx: &App) -> (Stateful<Div>, Stateful<Div>) {
        let theme = cx.theme();
        let (selected_bg, hover) = (theme.sidebar_accent, theme.list_hover);
        let outer = h_flex()
            .id(id)
            .group("hub-row")
            .h(px(ROW_HEIGHT))
            .w_full()
            .cursor_pointer();
        let inner = h_flex()
            // 要有 id：悬停样式只有带 id 的元素才会在鼠标进出时重画
            .id("inner")
            .h(px(ROW_HEIGHT - 2.))
            .flex_1()
            .min_w_0()
            .relative()
            .mx(px(6.))
            .pl(px(16.))
            .pr(px(4.))
            .gap(px(8.))
            .rounded(px(ui::RADIUS))
            .text_size(px(13.))
            .when(on, |el| el.bg(selected_bg))
            .when(!on, |el| el.hover(|s| s.bg(hover)));
        (outer, inner)
    }

    /// 「已固定」里一个终端的那一行：状态、名字、是哪个仓库的；鼠标移上去行尾是取消固定。
    fn render_pinned_term(
        &self,
        index: usize,
        row: &Row,
        on: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted) = (theme.sidebar_foreground, theme.muted_foreground);
        let repo = self.app.read(cx).repo_name(&row.entry);
        let unseen = row.state == TermState::Stopped;
        let slot = row.slot();
        let (for_click, for_go, for_unpin) = (slot.clone(), slot.clone(), slot.clone());
        let tip = self.term_tip(row, cx);
        let unpin = Self::row_button(("hub-unpin", index), IconName::PinOff, cx)
            .tooltip(|window, cx| ui::tip(tr("取消固定"), window, cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_pin_term(&for_unpin, cx);
            }));
        let (outer, inner) = Self::row_shell(("hub-pinned-term", index), on, cx);
        outer
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.focus.focus(window, cx);
                match event.click_count() >= 2 {
                    true => this.go_term(&for_go, cx),
                    false => this.select_term(for_click.clone(), window, cx),
                }
            }))
            .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
            .child(
                inner
                    // 圆点占的宽度和仓库那一行的图标一样，名字才对得齐
                    .child(
                        h_flex()
                            .flex_none()
                            .w(px(12.))
                            .justify_center()
                            .child(Self::dot(row.state, 8., cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(if row.alive { fg } else { muted })
                            .when(unseen, |el| el.font_weight(FontWeight::SEMIBOLD))
                            .child(Self::term_name(row)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(22.))
                            .max_w(px(84.))
                            .pr(px(4.))
                            .truncate()
                            .text_size(px(11.))
                            .text_color(muted)
                            .group_hover("hub-row", |s| s.invisible())
                            .child(repo),
                    )
                    .child(Self::row_actions().child(unpin)),
            )
            .context_menu(term_menu(
                cx.entity().downgrade(),
                slot,
                true,
                self.app.read(cx).repo_name(&row.entry),
            ))
            .into_any_element()
    }

    /// 一个仓库的那一行：名字，行尾几个停下了、在跑、空着；鼠标移上去行尾换成固定和新建。
    fn render_repo(
        &self,
        index: usize,
        (entry, rows): &Repo,
        on: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, accent) = (
            theme.sidebar_foreground,
            theme.muted_foreground,
            theme.primary,
        );
        let app = self.app.read(cx);
        let name = app.repo_name(entry);
        let pinned = self.pinned_repo(entry, cx);
        let empty = rows.is_empty();
        let tip = match entry.server.is_some() {
            true => format!("{}（{}）", entry.path, Self::place(app, entry)),
            false => entry.path.clone(),
        };
        let (for_click, for_go, for_pin, for_new) =
            (entry.clone(), entry.clone(), entry.clone(), entry.clone());
        let pin_tip = if pinned {
            tr("取消固定")
        } else {
            tr("固定到顶部")
        };
        let new_tip = tf!("在 {repo} 中新建终端", repo = name.clone());
        let pin = Self::row_button(
            ("hub-pin-repo", index),
            if pinned {
                IconName::PinOff
            } else {
                IconName::Pin
            },
            cx,
        )
        .when(pinned, |el| el.text_color(accent))
        .tooltip(move |window, cx| ui::tip(pin_tip, window, cx))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.toggle_pin_repo(&for_pin, cx);
        }));
        let new = Self::row_button(("hub-new", index), IconName::Plus, cx)
            .tooltip(move |window, cx| ui::tip(new_tip.clone(), window, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.new_terminal(for_new.clone(), window, cx);
            }));
        let icon = match entry.server.is_some() {
            true => IconName::Server,
            false => IconName::FolderGit2,
        };
        let (outer, inner) = Self::row_shell(("hub-repo", index), on, cx);
        outer
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.focus.focus(window, cx);
                match event.click_count() >= 2 {
                    true => cx.emit(Go::Repo(for_go.clone())),
                    false => this.select_repo(for_click.clone(), window, cx),
                }
            }))
            .child(
                inner
                    .child(ui::icon_small(icon).flex_none().text_color(muted))
                    .child(
                        ui::clipped_with_tip(("hub-repo-name", index), name, tip)
                            .flex_1()
                            // 没有终端的仓库淡一档
                            .text_color(if empty { muted } else { fg }),
                    )
                    .child(
                        Self::counts(rows, cx)
                            .min_w(px(46.))
                            .justify_end()
                            .pr(px(4.))
                            .group_hover("hub-row", |s| s.invisible()),
                    )
                    .child(Self::row_actions().child(pin).child(new)),
            )
            .context_menu(repo_menu(cx.entity().downgrade(), entry.clone(), pinned))
            .into_any_element()
    }

    /// 一组上面的那一行。工作区的可以点一下收起、展开，收着时行尾写里面几个停下、在跑、空着。
    fn render_group_head(
        &self,
        index: usize,
        group: &SideGroup,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, chip_bg, hover) = (theme.muted_foreground, theme.secondary, theme.list_hover);
        let app = self.app.read(cx);
        let workspace = group.workspace.as_ref();
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
                .h(px(16.))
                .px(px(6.))
                .rounded_full()
                .bg(chip_bg)
                .text_size(px(11.))
                .line_height(px(16.))
                .font_weight(FontWeight::NORMAL)
                .text_color(muted)
                .child(text)
        };
        let all: Vec<Row> = group
            .repos
            .iter()
            .flat_map(|(_, rows)| rows.iter().cloned())
            .collect();
        let arrow = match group.collapsed {
            true => IconName::ChevronRight,
            false => IconName::ChevronDown,
        };
        let key = workspace.cloned();
        h_flex()
            .id(("hub-group", index))
            .h(px(26.))
            .mt(px(6.))
            .mx(px(6.))
            .px(px(4.))
            .gap(px(6.))
            .rounded(px(ui::RADIUS))
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(muted)
            .when_some(key, |el, key| {
                el.cursor_pointer()
                    .hover(|s| s.bg(hover))
                    .child(ui::icon_small(arrow).flex_none())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.focus.focus(window, cx);
                        this.toggle_group(key.clone(), cx);
                    }))
            })
            .child(ui::icon_small(IconName::Layers).flex_none())
            .child(ui::clipped(("hub-group-name", index), name).flex_shrink(1.))
            .children(machine.map(chip))
            .when(current, |el| el.child(chip(tr("当前").into())))
            .child(div().flex_1())
            .when(group.collapsed, |el| el.child(Self::counts(&all, cx)))
            .into_any_element()
    }

    /// 只看哪些：样子和仓库页侧栏顶上切页面的那一条一样，占满一行。
    fn render_filter(&self, side: &Side, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (track, fg, muted) = (theme.secondary, theme.foreground, theme.muted_foreground);
        let dark = theme.is_dark();
        // 选中的那一格：浅色主题是白底加一点影子，深色主题比底下亮一档
        let on_bg = if dark {
            track.blend(fg.opacity(0.12))
        } else {
            theme.background
        };
        let items = [
            (Filter::All, "overview-all", tr("全部"), side.total, ""),
            (
                Filter::Stopped,
                "overview-stopped",
                tr("已停下"),
                side.stopped,
                tr("超过 10 秒没有新输出，且停下后尚未查看"),
            ),
            (
                Filter::Running,
                "overview-running",
                tr("运行中"),
                side.running,
                tr("10 秒内有新输出"),
            ),
        ];
        h_flex()
            .flex_none()
            .mx(px(10.))
            .mt(px(8.))
            .mb(px(2.))
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(7.))
            .bg(track)
            .children(items.into_iter().map(|(filter, id, label, count, tip)| {
                let on = self.filter == filter;
                h_flex()
                    .id(id)
                    .flex_1()
                    .min_w_0()
                    .h(px(26.))
                    .gap(px(5.))
                    .justify_center()
                    .rounded(px(5.))
                    .text_size(px(12.))
                    .text_color(if on { fg } else { muted })
                    .when(on, |el| {
                        el.bg(on_bg)
                            .font_weight(FontWeight::SEMIBOLD)
                            .when(!dark, |el| el.shadow_xs())
                    })
                    .when(!on, |el| el.cursor_pointer().hover(|s| s.text_color(fg)))
                    .child(label)
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(muted)
                            .child(count.to_string()),
                    )
                    .when(!tip.is_empty(), |el| {
                        el.tooltip(move |window, cx| ui::tip(tip, window, cx))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.set_filter(filter, cx)))
            }))
    }

    /// 左边的侧栏。和仓库页的侧栏一样宽、一样的底色和字号：右边的画面才和仓库的终端页一样大。
    fn render_side(&self, width: f32, side: &Side, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (panel, border, muted, accent) = (
            theme.sidebar,
            theme.sidebar_border,
            theme.muted_foreground,
            theme.primary,
        );
        let summary = match side.stopped {
            0 => tf!("{n} 个终端", n = side.total.to_string()),
            _ => tf!(
                "{n} 个终端，{m} 个已停下",
                n = side.total.to_string(),
                m = side.stopped.to_string()
            ),
        };
        let on = self.highlight(side);
        let is_on = |pick: Pick| on.as_ref() == Some(&pick);
        let mut items: Vec<AnyElement> = Vec::new();
        let mut index = 0;
        if !side.pinned_terms.is_empty() || !side.pinned_repos.is_empty() {
            items.push(
                h_flex()
                    .h(px(26.))
                    .mt(px(6.))
                    .px(px(10.))
                    .gap(px(6.))
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(muted)
                    .child(ui::icon_small(IconName::Pin).flex_none())
                    .child(tr("已固定"))
                    .into_any_element(),
            );
            for row in &side.pinned_terms {
                let on = is_on(Pick::Term(row.slot()));
                items.push(self.render_pinned_term(index, row, on, cx));
                index += 1;
            }
            for repo in &side.pinned_repos {
                let on = is_on(Pick::Repo(repo.0.clone()));
                items.push(self.render_repo(index, repo, on, cx));
                index += 1;
            }
            items.push(
                div()
                    .h(px(1.))
                    .mx(px(10.))
                    .mt(px(8.))
                    .bg(border)
                    .into_any_element(),
            );
        }
        for (at, group) in side.groups.iter().enumerate() {
            items.push(self.render_group_head(at, group, cx));
            if group.collapsed {
                continue;
            }
            for repo in &group.repos {
                let on = is_on(Pick::Repo(repo.0.clone()));
                items.push(self.render_repo(index, repo, on, cx));
                index += 1;
            }
        }
        let empty =
            side.groups.is_empty() && side.pinned_terms.is_empty() && side.pinned_repos.is_empty();
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
                .flex_none()
                .gap(px(4.))
                .child(div().font_weight(FontWeight::MEDIUM).child(keys))
                .child(text)
        };
        v_flex()
            .w(px(width))
            .flex_none()
            .h_full()
            .bg(panel)
            .border_r_1()
            .border_color(border)
            // 顶上那一条和仓库页侧栏的一样：图标、名字、一行小字
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
                            .child(ui::icon(IconName::SquareTerminal).text_color(accent)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(trc("页面", "终端")),
                            )
                            .child(
                                ui::clipped("overview-summary", summary)
                                    .text_size(px(11.))
                                    .text_color(muted),
                            ),
                    ),
            )
            .child(self.render_filter(side, cx))
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
                                .px(px(14.))
                                .py(px(24.))
                                .text_size(px(12.))
                                .text_color(muted)
                                .text_center()
                                .child(match self.filter {
                                    Filter::All => tr("工作区中没有仓库"),
                                    Filter::Stopped => tr("没有已停下的终端"),
                                    Filter::Running => tr("没有运行中的终端"),
                                }),
                        )
                    })
                    .children(offline.map(|text| {
                        div()
                            .px(px(14.))
                            .pt(px(12.))
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(text)
                    })),
            )
            .child(
                h_flex()
                    .h(px(28.))
                    .flex_none()
                    .px(px(14.))
                    .gap(px(12.))
                    .border_t_1()
                    .border_color(border)
                    .overflow_hidden()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(hint("↑ ↓", tr("选择")))
                    .child(hint("Enter", tr("前往")))
                    .child(hint("→", tr("输入"))),
            )
    }

    /// 标签栏上的一个终端：固定的前面有图钉；显示着的、鼠标移上去的有结束的 ×。
    fn render_tab(
        &self,
        index: usize,
        row: &Row,
        pinned: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, on_bg, hover, close_hover, accent) = (
            theme.foreground,
            theme.muted_foreground,
            theme.secondary,
            theme.list_hover,
            theme.border,
            theme.primary,
        );
        let slot = row.slot();
        let on = self.shown.as_ref() == Some(&slot);
        let repo = self.app.read(cx).repo_name(&row.entry);
        let tip = self.term_tip(row, cx);
        let (for_click, for_end) = (slot.clone(), slot.clone());
        let close = div()
            .id(("hub-tab-end", index))
            .flex_none()
            .size(px(18.))
            .rounded(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .text_color(muted)
            .hover(|s| s.bg(close_hover).text_color(fg))
            .when(!on, |close| {
                close.invisible().group_hover("hub-tab", |s| s.visible())
            })
            .child(ui::icon_small(IconName::X))
            .tooltip(|window, cx| ui::tip(tr("结束此终端"), window, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.end(&for_end, window, cx);
            }));
        h_flex()
            .id(("hub-tab", index))
            .group("hub-tab")
            .h(px(30.))
            .min_w(px(60.))
            .max_w(px(if pinned { 260. } else { 220. }))
            .flex_shrink(1.)
            .pl(px(if pinned { 8. } else { 10. }))
            .pr(px(6.))
            .gap(px(7.))
            .rounded(px(ui::RADIUS))
            .text_size(px(13.))
            .text_color(if on { fg } else { muted })
            .cursor_pointer()
            .when(on, |el| el.bg(on_bg).font_weight(FontWeight::MEDIUM))
            .when(!on, |el| el.hover(|s| s.bg(hover)))
            .when(pinned, |el| {
                el.child(
                    Icon::new(IconName::Pin)
                        .size(px(11.))
                        .flex_none()
                        .text_color(if on { accent } else { muted }),
                )
            })
            .child(Self::dot(row.state, 8., cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(Self::term_name(row)),
            )
            // 固定的标签换到哪个仓库都在：写上它是哪个仓库的，不然几个「终端 1」分不出来
            .when(pinned, |el| {
                el.child(
                    div()
                        .flex_shrink(2.)
                        .min_w_0()
                        .max_w(px(96.))
                        .truncate()
                        .text_size(px(11.))
                        .font_weight(FontWeight::NORMAL)
                        .text_color(muted)
                        .child(repo.clone()),
                )
            })
            .child(close)
            .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_term(for_click.clone(), window, cx);
                this.type_in(window, cx);
            }))
            .context_menu(term_menu(cx.entity().downgrade(), slot, pinned, repo))
            .into_any_element()
    }

    /// 右边上面那一条：标签栏、+，右边是怎么输入的提示、发送文件、重启、前往。
    fn render_toolbar(
        &self,
        hub: &[RepoGroup],
        typing: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted, primary, hover, fg) = (
            theme.border,
            theme.muted_foreground,
            theme.primary,
            theme.list_hover,
            theme.foreground,
        );
        let (pinned, own) = self.tabs(hub, cx);
        let split = !pinned.is_empty() && !own.is_empty();
        let mut tabs: Vec<AnyElement> = Vec::new();
        for (index, row) in pinned.iter().enumerate() {
            tabs.push(self.render_tab(index, row, true, cx));
        }
        if split {
            tabs.push(
                div()
                    .flex_none()
                    .w(px(1.))
                    .h(px(18.))
                    .mx(px(4.))
                    .bg(border)
                    .into_any_element(),
            );
        }
        for (index, row) in own.iter().enumerate() {
            tabs.push(self.render_tab(pinned.len() + index, row, false, cx));
        }
        let app = self.app.read(cx);
        let repo = self.repo.clone();
        let repo_name = repo.as_ref().map(|entry| app.repo_name(entry));
        let shown = self
            .shown
            .as_ref()
            .and_then(|slot| row_in(hub, slot))
            .cloned();
        // 没有显示着的终端时，右边写选中的是哪个仓库、在哪
        let place = repo
            .as_ref()
            .filter(|_| shown.is_none())
            .map(|entry| format!("{} · {}", app.repo_name(entry), Self::place(app, entry)));
        let plus = repo.clone().zip(repo_name).map(|(entry, name)| {
            let tip = tf!("在 {repo} 中新建终端（Ctrl+Shift+`）", repo = name);
            div()
                .id("hub-new-tab")
                .flex_none()
                .size(px(24.))
                .rounded(px(ui::RADIUS))
                .flex()
                .items_center()
                .justify_center()
                .text_color(muted)
                .cursor_pointer()
                .hover(|s| s.bg(hover).text_color(fg))
                .child(ui::icon(IconName::Plus))
                .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.new_terminal(entry.clone(), window, cx)
                }))
        });
        // 怎么输入、正在输入时怎么换一个
        let hint = shown.as_ref().and_then(|row| match (typing, row.alive) {
            (true, _) => Some((tr("正在输入。Ctrl+Shift+↑ ↓ 切换终端"), primary)),
            (false, true) => Some((tr("点击画面或按 → 输入"), muted)),
            (false, false) => None,
        });
        let slot = shown.as_ref().map(Row::slot);
        let (for_attach, for_restart) = (slot.clone(), slot);
        h_flex()
            .h(px(HEAD_HEIGHT))
            .flex_none()
            .px(px(10.))
            .gap(px(4.))
            .border_b_1()
            .border_color(border)
            .child(
                h_flex()
                    .flex_shrink(1.)
                    .min_w_0()
                    .gap(px(2.))
                    .overflow_hidden()
                    .children(tabs),
            )
            .children(plus)
            .child(div().flex_1().min_w(px(4.)))
            .children(place.map(|place| {
                ui::clipped("hub-place", place)
                    .flex_shrink(100.)
                    .text_xs()
                    .text_color(muted)
            }))
            // 地方不够时先省略这一句，标签留着
            .children(hint.map(|(text, color)| {
                ui::clipped("hub-hint", text)
                    .flex_shrink(100.)
                    .mr(px(6.))
                    .text_xs()
                    .text_color(color)
            }))
            .when(shown.is_some(), |bar| {
                bar.child(
                    tool_button(
                        "hub-attach",
                        IconName::Paperclip,
                        tr("发送文件"),
                        tr("发送图片或文件（也可拖放，或按 Ctrl+V 粘贴）"),
                        false,
                        true,
                        0,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(slot) = &for_attach {
                            this.attach(slot, window, cx);
                        }
                    })),
                )
                .child(
                    tool_button(
                        "hub-restart",
                        IconName::RotateCw,
                        tr("重启"),
                        tr("结束当前程序并重新启动"),
                        false,
                        true,
                        0,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(slot) = &for_restart {
                            this.restart(slot, window, cx);
                        }
                    })),
                )
                .child(
                    Button::new("overview-go")
                        .primary()
                        .small()
                        .flex_none()
                        .ml(px(6.))
                        .label(tr("前往"))
                        .tooltip(tr("切换到此终端所在的工作区和仓库（Enter）"))
                        .on_click(cx.listener(|this, _, _, cx| this.go(cx))),
                )
            })
    }

    /// 右边：上面是标签栏，下面是显示着的终端的画面，占满；选中的仓库没有终端时是空的样子。
    fn render_main(
        &self,
        hub: &[RepoGroup],
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, muted, primary) = (theme.background, theme.muted_foreground, theme.primary);
        let typing = self.typing(window, cx);
        let centered = || {
            v_flex()
                .flex_1()
                .min_h_0()
                .items_center()
                .justify_center()
                .gap_3()
        };
        let body = match (&self.peek, &self.shown, &self.repo) {
            (Some(peek), _, _) => div()
                .relative()
                .flex_1()
                .min_h_0()
                // 点进终端里焦点就换了（终端自己接的）：这一页和主窗口跟着重画，边框、提示、
                // 「看没看过」才跟得上
                .on_any_mouse_down(cx.listener(|_, _, _, cx| cx.notify()))
                .child(peek.view.clone())
                // 正在输入：画面带一圈边框。盖在上面画，不占地方，不然一输入终端就小一圈
                .when(typing, |el| {
                    el.child(
                        div()
                            .absolute()
                            .inset_0()
                            .border_1()
                            .border_color(primary.opacity(0.7)),
                    )
                })
                .into_any_element(),
            // 重启：原来那个正在结束
            (None, Some(_), _) => centered()
                .text_color(muted)
                .child(div().text_size(px(13.)).child(tr("正在启动终端…")))
                .into_any_element(),
            (None, None, Some(entry)) => {
                let entry = entry.clone();
                centered()
                    .child(
                        Icon::new(IconName::SquareTerminal)
                            .size(px(28.))
                            .text_color(muted),
                    )
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr("此仓库没有打开的终端")),
                    )
                    .child(
                        Button::new("hub-first")
                            .primary()
                            .small()
                            .icon(ui::icon(IconName::Plus))
                            .label(tr("打开终端"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.new_terminal(entry.clone(), window, cx)
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(tr("或按 Ctrl+Shift+`")),
                    )
                    .into_any_element()
            }
            (None, None, None) => centered()
                .text_color(muted)
                .child(Icon::new(IconName::SquareTerminal).size(px(28.)))
                .child(div().text_size(px(13.)).child(tr("选择左侧的仓库")))
                .into_any_element(),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(bg)
            .child(self.render_toolbar(hub, typing, cx))
            .child(body)
    }
}

impl Render for TermOverview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 侧栏和仓库页的侧栏一样宽（那边拖宽了它也跟着宽）
        let viewport = f32::from(window.viewport_size().width);
        let width = crate::repo::sidebar_open(&self.app.read(cx).settings.layout, viewport);
        let hub = self.hub(cx);
        let side = self.side(&hub, cx);
        h_flex()
            .id("term-overview")
            .size_full()
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::on_key_first))
            .on_key_down(cx.listener(Self::on_key))
            .child(self.render_side(width, &side, cx))
            .child(self.render_main(&hub, window, cx))
    }
}
