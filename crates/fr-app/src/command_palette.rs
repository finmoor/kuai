//! 命令面板（Ctrl+P）：一个框里搜仓库、命令、分支和标签、文件，按组列出。
//!
//! 不输入时是最近的仓库和常用的命令；输入几个字就在四组里一起找，每组先列几个，「另有 N 项」展开。
//! 输入 `>`、`@`、`/` 开头（或点上面的范围）只看命令、分支和标签、文件。上下键选，回车做默认的那
//! 件事：仓库是打开或切过去，命令是执行，本地分支是切换、远程分支是检出为本地分支、标签是在提交图
//! 里定位，文件是看它的历史。选中分支、标签、文件时按 Tab 列出对它能做的别的事。
//!
//! 面板只是快路：每条命令在界面上另有鼠标能找到的地方。命令从三处来：快捷键登记表（`keymap.rs`）、
//! 当前仓库页（`repo/palette.rs`）、主窗口（`workspace.rs`）；现在用不了的灰着，行尾写原因。
//! 选中以后的事由主窗口做（订阅 [`Chosen`]）。

use std::collections::HashSet;

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::AppState;
use crate::repo::{FileAction, PaletteRef, RefAction, RefKind, RepoView};
use crate::settings::RepoEntry;
use crate::ui;

/// 一组里先列几个
const GROUP_SHOWN: usize = 5;
/// 展开、或者只看这一组时最多列几个
const GROUP_MAX: usize = 60;

/// 面板里选中了什么。
#[derive(Clone)]
pub enum Chosen {
    Repo(RepoEntry),
    /// 一条命令（按 id 认，主窗口知道怎么做）
    Command(String),
    Ref(PaletteRef, RefAction),
    File(String, FileAction),
}

impl EventEmitter<Chosen> for Palette {}

/// 面板里列的一条命令。
#[derive(Clone)]
pub struct CommandRow {
    pub id: String,
    pub name: SharedString,
    /// 哪一类（「Git」「页面」「设置」……）
    pub group: SharedString,
    /// 快捷键，写成 `Ctrl+Shift+F`；没有是空的
    pub keys: SharedString,
    /// 现在用不了的原因
    pub why_not: Option<SharedString>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Group {
    Repos,
    Commands,
    Refs,
    Files,
}

impl Group {
    fn title(self) -> &'static str {
        match self {
            Group::Repos => tr("仓库"),
            Group::Commands => tr("命令"),
            Group::Refs => tr("分支和标签"),
            Group::Files => tr("文件"),
        }
    }
}

/// 只看哪一组；None 是全部。
type Scope = Option<Group>;

/// 文件清单读到哪了。
enum Files {
    /// 还没读（输入了字、或者只看文件时才读）
    Idle,
    Loading,
    Ready(Vec<SharedString>),
    Failed(SharedString),
}

/// 列表里的一行。
#[derive(Clone)]
enum Item {
    Repo {
        entry: RepoEntry,
        title: SharedString,
        mark: Option<SharedString>,
    },
    /// 命令在 [`Palette::commands`] 里的下标
    Command(usize),
    Ref(PaletteRef),
    File(SharedString),
    /// 「另有 N 项」
    More(Group, usize),
}

/// 画出来的一行：组的标题，或者能选的一项。
enum Line {
    Header(Group, usize),
    Item(Item),
}

/// 对一个分支、标签、文件能做的事（按了 Tab）。
struct Sub {
    title: SharedString,
    actions: Vec<(SharedString, Chosen)>,
    selected: usize,
}

pub struct Palette {
    app: Entity<AppState>,
    /// 当前仓库页；不在仓库页时没有（分支、文件两组就没有）
    repo: Option<Entity<RepoView>>,
    input: Entity<InputState>,
    commands: Vec<CommandRow>,
    /// 最近用过的命令，新的在前
    recent: Vec<String>,
    refs: Vec<PaletteRef>,
    files: Files,
    /// 点上面的范围选的
    scope: Scope,
    /// 点过「另有 N 项」的组
    expanded: HashSet<Group>,
    /// 高亮的是能选的第几项
    selected: usize,
    sub: Option<Sub>,
    /// 回车选了一条用不了的命令时写在下面的原因
    hint: Option<SharedString>,
    scroll: ScrollHandle,
    files_task: Option<Task<()>>,
    _subscription: Subscription,
}

/// 把面板弹出来（选中以后的事由订阅 [`Chosen`] 的主窗口做，记得关掉对话框）。
pub fn show(palette: Entity<Palette>, window: &mut Window, cx: &mut App) {
    ui::cancel_pending_tip(window, cx);
    let first_build = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        // 对话框打开的那一刻焦点会被它拿走，打开后再放到输入框
        if first_build.replace(false) {
            let input = palette.read(cx).input.clone();
            input.focus_handle(cx).focus(window, cx);
        }
        let on_enter = palette.clone();
        dialog
            .title(tr("快速打开"))
            .w(px(680.))
            // 回车选中高亮的那个
            .on_ok(move |_, window, cx| {
                on_enter.update(cx, |palette, cx| palette.pick_selected(window, cx));
                false
            })
            // Esc 先由对话框接住（不经过面板的按键处理）：列着「更多操作」时只收回去，面板不关
            .on_cancel({
                let palette = palette.clone();
                move |_, _, cx| palette.update(cx, |palette, cx| palette.cancel(cx))
            })
            .child(palette.clone())
    });
}

/// 输入的字拆成几段（小写），每段都要出现。
fn tokens(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|part| part.to_lowercase())
        .collect()
}

/// 这句话里是不是每段都有；有的话给个分，小的排前面：名字开头就对上的最前，其次是出现得早的。
fn score(text: &str, tokens: &[String]) -> Option<usize> {
    let lower = text.to_lowercase();
    let mut total = 0;
    for token in tokens {
        total += lower.find(token.as_str())?;
    }
    Some(total)
}

/// 输入框里的字拆成范围和要找的字：`>` 只看命令，`@` 只看分支和标签，`/` 只看文件。
fn parse(raw: &str, picked: Scope) -> (Scope, String) {
    let trimmed = raw.trim_start();
    let mut chars = trimmed.chars();
    let scope = match chars.next() {
        Some('>') | Some('＞') => Some(Group::Commands),
        Some('@') => Some(Group::Refs),
        Some('/') => Some(Group::Files),
        _ => return (picked, trimmed.trim().to_string()),
    };
    (scope, chars.as_str().trim().to_string())
}

impl Palette {
    pub fn new(
        app: Entity<AppState>,
        repo: Option<Entity<RepoView>>,
        commands: Vec<CommandRow>,
        recent: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("搜索仓库、命令、分支、文件")));
        let subscription = cx.subscribe(&input, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                // 要找的字变了：回到第一个
                this.selected = 0;
                this.sub = None;
                this.hint = None;
                this.expanded.clear();
                this.load_files_if_needed(cx);
                cx.notify();
            }
        });
        let refs = repo
            .as_ref()
            .map(|repo| repo.read(cx).palette_refs())
            .unwrap_or_default();
        Self {
            app,
            repo,
            input,
            commands,
            recent,
            refs,
            files: Files::Idle,
            scope: None,
            expanded: HashSet::new(),
            selected: 0,
            sub: None,
            hint: None,
            scroll: ScrollHandle::new(),
            files_task: None,
            _subscription: subscription,
        }
    }

    fn query(&self, cx: &App) -> (Scope, String) {
        parse(self.input.read(cx).value().as_ref(), self.scope)
    }

    /// 要列文件了（输入了字，或者只看文件）还没读过清单：读。
    fn load_files_if_needed(&mut self, cx: &mut Context<Self>) {
        let (scope, query) = self.query(cx);
        let wanted = match scope {
            Some(Group::Files) => true,
            None => !query.is_empty(),
            _ => false,
        };
        let Some(repo) = self.repo.clone().filter(|_| wanted) else {
            return;
        };
        if !matches!(self.files, Files::Idle) {
            return;
        }
        self.files = Files::Loading;
        let read = repo.update(cx, |repo, cx| repo.palette_files(cx));
        self.files_task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                this.files = match result {
                    Ok(files) => Files::Ready(files),
                    Err(err) => Files::Failed(err.into()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// 匹配的仓库：刚关掉的在最前，再是开着的，最后是其余的（和原来的快速打开一样）。
    fn repo_items(&self, tokens: &[String], cx: &App) -> Vec<Item> {
        let app = self.app.read(cx);
        let open = app.tabs().repos.clone();
        let closed: Vec<RepoEntry> = app
            .closed_tabs()
            .into_iter()
            .filter(|entry| !open.contains(entry))
            .collect();
        let rest: Vec<RepoEntry> = app
            .all_repos()
            .into_iter()
            .filter(|entry| !open.contains(entry) && !closed.contains(entry))
            .collect();
        let closed_mark: SharedString = tr("最近关闭").into();
        let open_mark: SharedString = tr("已打开").into();
        closed
            .into_iter()
            .map(|entry| (entry, Some(closed_mark.clone())))
            .chain(
                open.into_iter()
                    .map(|entry| (entry, Some(open_mark.clone()))),
            )
            .chain(rest.into_iter().map(|entry| (entry, None)))
            .filter_map(|(entry, mark)| {
                let title = app.repo_title(&entry);
                let text = format!("{title} {}", entry.path);
                score(&text, tokens)?;
                Some(Item::Repo { entry, title, mark })
            })
            .collect()
    }

    /// 匹配的命令。没输入字时：最近用过的在前，再是别的能用的。输入了字：能用的在前，按对得多准排。
    fn command_items(&self, tokens: &[String]) -> Vec<Item> {
        let recent_rank = |id: &str| {
            self.recent
                .iter()
                .position(|recent| recent == id)
                .unwrap_or(usize::MAX)
        };
        let mut found: Vec<(usize, usize)> = self
            .commands
            .iter()
            .enumerate()
            .filter_map(|(ix, command)| {
                let text = format!("{} {}", command.name, command.group);
                Some((ix, score(&text, tokens)?))
            })
            .collect();
        found.sort_by_key(|(ix, rank)| {
            let command = &self.commands[*ix];
            (
                command.why_not.is_some(),
                recent_rank(&command.id),
                // 名字里对上的排在只有分组名对上的前面
                score(&command.name, tokens).is_none(),
                *rank,
                *ix,
            )
        });
        found.into_iter().map(|(ix, _)| Item::Command(ix)).collect()
    }

    fn ref_items(&self, tokens: &[String]) -> Vec<Item> {
        let mut found: Vec<(&PaletteRef, usize)> = self
            .refs
            .iter()
            .filter_map(|r| Some((r, score(&r.name, tokens)?)))
            .collect();
        // 稳定排序：原来的先后（当前分支、本地、远程、标签）在同分时留着
        found.sort_by_key(|(_, rank)| *rank);
        found
            .into_iter()
            .map(|(r, _)| Item::Ref(r.clone()))
            .collect()
    }

    /// 匹配的文件：文件名里有的排在前面，其次是路径里有的。
    fn file_items(&self, tokens: &[String]) -> Vec<Item> {
        let Files::Ready(files) = &self.files else {
            return Vec::new();
        };
        let mut found: Vec<(&SharedString, (bool, usize))> = files
            .iter()
            .filter_map(|path| {
                let rank = score(path, tokens)?;
                let name = path.rsplit('/').next().unwrap_or(path);
                Some((path, (score(name, tokens).is_none(), rank)))
            })
            .collect();
        found.sort_by_key(|(_, rank)| *rank);
        found
            .into_iter()
            .take(GROUP_MAX * 4)
            .map(|(path, _)| Item::File(path.clone()))
            .collect()
    }

    /// 现在该画的行。
    fn lines(&self, cx: &App) -> Vec<Line> {
        let (scope, query) = self.query(cx);
        let tokens = tokens(&query);
        let typed = !tokens.is_empty();
        let mut lines = Vec::new();
        let mut push = |group: Group, items: Vec<Item>| {
            if items.is_empty() {
                return;
            }
            let limit = match scope == Some(group) || self.expanded.contains(&group) {
                true => GROUP_MAX,
                false => GROUP_SHOWN,
            };
            let total = items.len();
            lines.push(Line::Header(group, total));
            lines.extend(items.into_iter().take(limit).map(Line::Item));
            if total > limit && limit < GROUP_MAX {
                lines.push(Line::Item(Item::More(group, total - limit)));
            }
        };
        let wants = |group: Group| scope.is_none_or(|only| only == group);
        if wants(Group::Repos) {
            push(Group::Repos, self.repo_items(&tokens, cx));
        }
        if wants(Group::Commands) {
            push(Group::Commands, self.command_items(&tokens));
        }
        // 分支、文件：输入了字，或者指明只看这一组时才列
        if wants(Group::Refs) && (typed || scope == Some(Group::Refs)) {
            push(Group::Refs, self.ref_items(&tokens));
        }
        if wants(Group::Files) && (typed || scope == Some(Group::Files)) {
            push(Group::Files, self.file_items(&tokens));
        }
        lines
    }

    /// 能选的那些行。
    fn items(&self, cx: &App) -> Vec<Item> {
        self.lines(cx)
            .into_iter()
            .filter_map(|line| match line {
                Line::Item(item) => Some(item),
                Line::Header(..) => None,
            })
            .collect()
    }

    /// 对这一项能做的事（分支、标签、文件才有）。
    fn sub_of(&self, item: &Item, cx: &App) -> Option<Sub> {
        let repo = self.repo.as_ref()?.read(cx);
        match item {
            Item::Ref(target) => Some(Sub {
                title: target.name.clone().into(),
                actions: repo
                    .palette_ref_actions(target, cx)
                    .into_iter()
                    .map(|(label, action)| (label, Chosen::Ref(target.clone(), action)))
                    .collect(),
                selected: 0,
            }),
            Item::File(path) => Some(Sub {
                title: path.clone(),
                actions: RepoView::palette_file_actions()
                    .into_iter()
                    .map(|(label, action)| (label, Chosen::File(path.to_string(), action)))
                    .collect(),
                selected: 0,
            }),
            _ => None,
        }
    }

    /// 选这一项：做默认的那件事。
    fn pick(&mut self, item: Item, window: &mut Window, cx: &mut Context<Self>) {
        match item {
            Item::Repo { entry, .. } => cx.emit(Chosen::Repo(entry)),
            Item::Command(ix) => {
                let Some(command) = self.commands.get(ix) else {
                    return;
                };
                match &command.why_not {
                    // 用不了：不关面板，把原因写在下面
                    Some(why) => {
                        self.hint = Some(
                            tf!("「{name}」现在无法使用：{why}", name = command.name, why).into(),
                        );
                        cx.notify();
                    }
                    None => cx.emit(Chosen::Command(command.id.clone())),
                }
            }
            Item::Ref(_) | Item::File(_) => {
                // 默认的那件事就是能做的事里的第一项
                if let Some(chosen) = self
                    .sub_of(&item, cx)
                    .and_then(|sub| sub.actions.into_iter().next())
                {
                    cx.emit(chosen.1);
                }
            }
            Item::More(group, _) => {
                self.expanded.insert(group);
                self.input.focus_handle(cx).focus(window, cx);
                cx.notify();
            }
        }
    }

    /// 回车。
    fn pick_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sub) = &self.sub {
            if let Some((_, chosen)) = sub.actions.get(sub.selected) {
                cx.emit(chosen.clone());
            }
            return;
        }
        let items = self.items(cx);
        if let Some(item) = items.get(self.selected.min(items.len().saturating_sub(1))) {
            self.pick(item.clone(), window, cx);
        }
    }

    /// Tab：列出对选中的分支、标签、文件能做的别的事；已经列着就收回去。
    fn toggle_sub(&mut self, cx: &mut Context<Self>) {
        if self.sub.take().is_some() {
            cx.notify();
            return;
        }
        let items = self.items(cx);
        let Some(item) = items.get(self.selected.min(items.len().saturating_sub(1))) else {
            return;
        };
        match self.sub_of(item, cx) {
            Some(sub) => self.sub = Some(sub),
            None => {
                self.hint =
                    Some(tr("Tab 只对分支、标签和文件有用：列出对它能做的其他操作。").into())
            }
        }
        cx.notify();
    }

    /// 按了 Esc（或点了外面）：列着「更多操作」时收回去、面板留着（返回 false），否则关掉。
    fn cancel(&mut self, cx: &mut Context<Self>) -> bool {
        if self.sub.take().is_some() {
            cx.notify();
            return false;
        }
        true
    }

    /// 上下键在能选的项里移动，到头绕回去；列着「更多操作」时 Esc 先收回去。
    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if let Some(sub) = &mut self.sub {
            let count = sub.actions.len().max(1);
            match key {
                "down" => sub.selected = (sub.selected + 1) % count,
                "up" => sub.selected = (sub.selected + count - 1) % count,
                "escape" | "left" => self.sub = None,
                _ => return,
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let count = self.items(cx).len();
        if count == 0 {
            return;
        }
        let at = self.selected.min(count - 1);
        self.selected = match key {
            "down" => (at + 1) % count,
            "up" => (at + count - 1) % count,
            _ => return,
        };
        self.hint = None;
        // 高亮的滚进看得见的地方：行高差不多，按第几行估
        self.scroll.scroll_to_item(self.selected + 1);
        cx.stop_propagation();
        cx.notify();
    }

    /// 上面那一排范围：全部、仓库、命令、分支和标签、文件。
    fn render_scopes(&self, current: Scope, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, primary, hover, active) = (
            theme.muted_foreground,
            theme.border,
            theme.primary,
            theme.list_hover,
            theme.list_active,
        );
        let has_repo = self.repo.is_some();
        let chips: [(Scope, SharedString, &'static str); 5] = [
            (None, tr("全部").into(), ""),
            (Some(Group::Repos), tr("仓库").into(), ""),
            (Some(Group::Commands), tr("命令").into(), ">"),
            (Some(Group::Refs), tr("分支和标签").into(), "@"),
            (Some(Group::Files), tr("文件").into(), "/"),
        ];
        let row = chips
            .into_iter()
            .enumerate()
            .map(|(ix, (scope, label, prefix))| {
                let on = scope == current;
                // 不在仓库页时没有分支、文件可找
                let usable = has_repo || !matches!(scope, Some(Group::Refs) | Some(Group::Files));
                h_flex()
                    .id(("palette-scope", ix))
                    .h(px(24.))
                    .px_2()
                    .gap_1()
                    .rounded(px(ui::RADIUS))
                    .border_1()
                    .border_color(if on { primary } else { border })
                    .text_xs()
                    .when(on, |chip| chip.bg(active))
                    .when(!usable, |chip| chip.opacity(0.45))
                    .when(usable && !on, |chip| {
                        chip.hover(|s| s.bg(hover)).cursor_pointer()
                    })
                    .child(label)
                    .when(!prefix.is_empty(), |chip| {
                        chip.child(
                            div()
                                .font_family(crate::fonts::mono())
                                .text_color(muted)
                                .child(prefix),
                        )
                    })
                    .when(usable, |chip| {
                        chip.on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.set_scope(scope, window, cx)
                            }),
                        )
                    })
            });
        h_flex().gap_1().children(row).into_any_element()
    }

    /// 点了上面的范围：记下来，输入框里开头的 `>`、`@`、`/` 去掉（不然它说了算）。
    fn set_scope(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.input.read(cx).value().to_string();
        let (_, query) = parse(&raw, None);
        self.scope = scope;
        self.selected = 0;
        self.sub = None;
        self.hint = None;
        if query != raw {
            self.input
                .update(cx, |input, cx| input.set_value(query, window, cx));
        }
        self.input.focus_handle(cx).focus(window, cx);
        self.load_files_if_needed(cx);
        cx.notify();
    }

    fn render_item(
        &self,
        ix: usize,
        item: &Item,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, hover, active, accent) = (
            theme.muted_foreground,
            theme.list_hover,
            theme.list_active,
            theme.primary,
        );
        let row = h_flex()
            .id(("palette-item", ix))
            .min_h(px(30.))
            .px_2()
            .py(px(3.))
            .gap_2()
            .rounded(px(ui::RADIUS))
            .cursor_pointer()
            // 高亮的是回车会选的
            .when(selected, |row| row.bg(active))
            .when(!selected, |row| row.hover(|s| s.bg(hover)))
            .on_click(cx.listener({
                let item = item.clone();
                move |this, _, window, cx| this.pick(item.clone(), window, cx)
            }));
        let tail = |text: SharedString| div().flex_none().text_xs().text_color(muted).child(text);
        match item {
            Item::Repo { entry, title, mark } => {
                let glyph = match entry.server.is_some() {
                    true => IconName::Server,
                    false => IconName::FolderGit2,
                };
                row.child(ui::icon(glyph).flex_none().text_color(muted))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(ui::clipped(("palette-title", ix), title.clone()).text_sm())
                            .child(
                                ui::clipped(("palette-path", ix), entry.path.clone())
                                    .text_xs()
                                    .text_color(muted),
                            ),
                    )
                    .children(mark.clone().map(tail))
                    .into_any_element()
            }
            Item::Command(at) => {
                let Some(command) = self.commands.get(*at) else {
                    return div().into_any_element();
                };
                let off = command.why_not.is_some();
                row.when(off, |row| row.opacity(0.55))
                    .child(
                        ui::icon(IconName::ChevronRight)
                            .flex_none()
                            .text_color(muted),
                    )
                    .child(
                        ui::clipped(("palette-command", ix), command.name.clone())
                            .flex_1()
                            .text_sm(),
                    )
                    .child(tail(match &command.why_not {
                        Some(why) => why.clone(),
                        None => command.group.clone(),
                    }))
                    .when(!command.keys.is_empty() && !off, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .px(px(5.))
                                .rounded(px(4.))
                                .border_1()
                                .border_color(muted.opacity(0.4))
                                .text_xs()
                                .text_color(muted)
                                .child(command.keys.clone()),
                        )
                    })
                    .into_any_element()
            }
            Item::Ref(target) => {
                let (glyph, kind) = match target.kind {
                    RefKind::Branch if target.current => (IconName::Check, tr("当前分支")),
                    RefKind::Branch => (IconName::GitBranch, tr("本地分支")),
                    RefKind::Remote => (IconName::Cloud, tr("远程分支")),
                    RefKind::Tag => (IconName::Tag, tr("标签")),
                };
                row.child(
                    ui::icon(glyph)
                        .flex_none()
                        .text_color(match target.current {
                            true => accent,
                            false => muted,
                        }),
                )
                .child(
                    ui::clipped(("palette-ref", ix), target.name.clone())
                        .flex_1()
                        .text_sm(),
                )
                .child(tail(kind.into()))
                .into_any_element()
            }
            Item::File(path) => {
                let (dir, name) = match path.rsplit_once('/') {
                    Some((dir, name)) => (dir.to_string(), name.to_string()),
                    None => (String::new(), path.to_string()),
                };
                row.child(ui::icon(IconName::File).flex_none().text_color(muted))
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(320.))
                            .truncate()
                            .text_sm()
                            .child(name),
                    )
                    .child(
                        ui::clipped(("palette-dir", ix), dir)
                            .flex_1()
                            .text_xs()
                            .text_color(muted),
                    )
                    .into_any_element()
            }
            Item::More(_, more) => row
                .child(div().flex_none().w(px(16.)))
                .child(
                    div()
                        .text_xs()
                        .text_color(accent)
                        .child(tf!("另有 {more} 项…", more)),
                )
                .into_any_element(),
        }
    }

    /// 「更多操作」那一小列。
    fn render_sub(&self, sub: &Sub, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, hover, active, border) = (
            theme.muted_foreground,
            theme.list_hover,
            theme.list_active,
            theme.border,
        );
        let rows = sub.actions.iter().enumerate().map(|(ix, (label, chosen))| {
            let chosen = chosen.clone();
            h_flex()
                .id(("palette-sub", ix))
                .h(px(28.))
                .px_2()
                .rounded(px(ui::RADIUS))
                .cursor_pointer()
                .text_sm()
                .when(ix == sub.selected, |row| row.bg(active))
                .when(ix != sub.selected, |row| row.hover(|s| s.bg(hover)))
                .child(label.clone())
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(chosen.clone())))
        });
        v_flex()
            .gap_1()
            .p_1()
            .rounded(px(ui::RADIUS))
            .border_1()
            .border_color(border)
            .child(
                h_flex()
                    .px_2()
                    .h(px(24.))
                    .gap_2()
                    .text_xs()
                    .text_color(muted)
                    .child(ui::clipped("palette-sub-title", sub.title.clone()).flex_1())
                    .child(div().flex_none().child(tr("Esc 返回"))),
            )
            .children(rows)
            .into_any_element()
    }
}

impl Render for Palette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list_height = (window.viewport_size().height - px(300.)).clamp(px(140.), px(420.));
        let (scope, query) = self.query(cx);
        let lines = self.lines(cx);
        let count = lines
            .iter()
            .filter(|line| matches!(line, Line::Item(_)))
            .count();
        self.selected = self.selected.min(count.saturating_sub(1));
        let selected = self.selected;
        let muted = cx.theme().muted_foreground;
        let mut at = 0;
        let rows: Vec<AnyElement> = lines
            .iter()
            .map(|line| match line {
                Line::Header(group, total) => h_flex()
                    .h(px(22.))
                    .px_2()
                    .mt(px(2.))
                    .gap_2()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(muted)
                    .child(group.title())
                    .child(
                        div()
                            .font_weight(FontWeight::NORMAL)
                            .child(total.to_string()),
                    )
                    .into_any_element(),
                Line::Item(item) => {
                    let ix = at;
                    at += 1;
                    self.render_item(ix, item, ix == selected && self.sub.is_none(), cx)
                }
            })
            .collect();
        // 一行都没有时说为什么
        let empty: Option<SharedString> = match (rows.is_empty(), &self.files, scope) {
            (false, ..) => None,
            (true, Files::Loading, Some(Group::Files)) => Some(tr("正在读取文件列表…").into()),
            (true, Files::Failed(err), Some(Group::Files)) => Some(err.clone()),
            (true, _, Some(Group::Refs) | Some(Group::Files)) if self.repo.is_none() => {
                Some(tr("当前不在仓库页，没有分支和文件可找。").into())
            }
            (true, ..) if query.is_empty() => Some(tr("没有可列出的项目").into()),
            (true, ..) => Some(tr("没有匹配的结果").into()),
        };
        let sub = self.sub.as_ref().map(|sub| self.render_sub(sub, cx));
        let hint = self.hint.clone();
        v_flex()
            .gap_2()
            // Tab 在这个上下文里绑成了「换一个」（见 `main.rs`）：这里是列出更多操作
            .key_context(crate::workspace::QUICK_OPEN_CONTEXT)
            .capture_key_down(cx.listener(Self::on_key))
            .on_action(cx.listener(|this, _: &crate::NextProgram, _, cx| this.toggle_sub(cx)))
            .on_action(cx.listener(|this, _: &crate::PrevProgram, _, cx| this.toggle_sub(cx)))
            .child(self.render_scopes(scope, cx))
            .child(Input::new(&self.input))
            .child(match sub {
                Some(sub) => sub,
                None => div()
                    .id("palette-list")
                    .max_h(list_height)
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(v_flex().children(rows))
                    .children(empty.map(|text| {
                        div().p_2().text_sm().text_color(muted).child(text)
                    }))
                    .into_any_element(),
            })
            .children(hint.map(|hint| {
                div()
                    .px_2()
                    .text_xs()
                    .text_color(cx.theme().warning)
                    .whitespace_normal()
                    .child(hint)
            }))
            .child(
                ui::clipped(
                    "palette-keys",
                    tr("↑↓ 选择 · Enter 确认 · Tab 更多操作 · 开头输入 > 只看命令，@ 分支和标签，/ 文件"),
                )
                .px_2()
                .text_xs()
                .text_color(muted),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{Group, parse, score, tokens};

    #[test]
    fn 开头的符号定范围() {
        assert_eq!(
            parse(">  fetch", None),
            (Some(Group::Commands), "fetch".into())
        );
        assert_eq!(parse("@main", None), (Some(Group::Refs), "main".into()));
        assert_eq!(
            parse("/src/a.rs", None),
            (Some(Group::Files), "src/a.rs".into())
        );
        assert_eq!(parse(" kuai ", None), (None, "kuai".into()));
        // 点了范围、输入框里没有符号：照点的；有符号的以符号为准
        assert_eq!(
            parse("x", Some(Group::Repos)),
            (Some(Group::Repos), "x".into())
        );
        assert_eq!(
            parse("@x", Some(Group::Repos)),
            (Some(Group::Refs), "x".into())
        );
        assert_eq!(parse("", None), (None, String::new()));
    }

    #[test]
    fn 每一段都要出现() {
        let t = tokens("Login  UI");
        assert_eq!(t, ["login", "ui"]);
        assert!(score("feature/login-ui", &t).is_some());
        assert!(score("feature/login", &t).is_none());
        // 没输入字：都算匹配
        assert_eq!(score("anything", &[]), Some(0));
        // 出现得早的分小
        let one = tokens("main");
        assert!(score("main", &one) < score("origin/main", &one));
    }
}
