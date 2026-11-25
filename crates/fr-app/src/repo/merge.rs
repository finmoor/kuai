//! 内置的合并编辑器：解决一个文件里的冲突，不用再开外部编辑器（`docs/design/
//! narrow-and-changes/` 方向 ③ A「上对照下结果」）。
//!
//! - 上半是对照：「当前｜传入」两栏，勾「显示共同祖先」变三栏；每一处冲突头上有「采纳当前」
//!   「采纳传入」「两者都要」，选过的可以「重置」；采纳以后跳到下一处没解决的。没有冲突的地方只留
//!   前后几行，中间折起来（点开看全部）。窗口窄的时候一次看一边。
//! - 下半是结果，可以直接改。每一处在结果里的位置跟着编辑挪（见 [`transform`]），所以改过别处以后
//!   按钮照样能用；在某一处里面手动改过的算「已手动修改」。
//!
//! 保存时结果里不能再有冲突标记；写回工作区后 `git add`，这个文件就算解决了。读写文件借 git 的
//! shell 别名（见 [`fr_core::workfile`]），本机和服务器上的仓库都能用。
//!
//! 共同祖先：冲突标记里本来带着（`merge.conflictStyle` 是 diff3、zdiff3）就用；没带的，按暂存区里的
//! 三份重新合并一遍（[`workfile::base_args`]），先核对工作区里的文件冲突以后没被动过，再换成带共同
//! 祖先的那份。核对不上（手动改过、git 合并时用了别的选项）就没有共同祖先可看。
//!
//! 文件按 UTF-8 处理；不是 UTF-8 的按 GB18030（GBK 的超集，国内老项目常见）试一次，解出来再编回去
//! 一字不差才接，保存时照原来的编码写回。两样都不是的（会改坏别处）不接，说明怎么在终端里解决。
//! CRLF 的文件编辑时按 LF，保存时换回 CRLF。

use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;

use fr_core::i18n::tr;
use fr_core::tf;
use fr_core::workfile;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::diff::DiffFile;
use gpui_kit::component::input::{
    Editor, EditorState, InputEditsEvent, RangeDecoration, RangeDecorationCollection,
    RangeDecorationStyle,
};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::ops::Step;
use super::{Mode, RepoView};
use crate::ui;

/// 冲突前后各留几行没冲突的
const CONTEXT: usize = 3;
/// 对照里每一行多高：跟着代码字号
fn row_height(size: f32) -> f32 {
    (size * 1.7).round().max(22.)
}
/// 内容区窄于这么多时对照一次只看一边
const ONE_SIDE_BELOW: f32 = 760.;

/// 一处冲突怎么解决的。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Choice {
    Current,
    Incoming,
    Both,
    /// 在结果里手动改过这一处
    Manual,
}

/// 窄的时候对照只看哪一边。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Current,
    Base,
    Incoming,
}

/// 一处冲突：在打开时的内容里占哪几行（从 0 起，不含末尾），各边是哪几行。
#[derive(Clone, Debug)]
struct Block {
    /// 含冲突标记
    lines: Range<usize>,
    ours: Range<usize>,
    base: Option<Range<usize>>,
    theirs: Range<usize>,
    ours_label: String,
    theirs_label: String,
}

/// 对照里的一行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    /// 两边一样的一行（打开时内容的第几行）
    Same(usize),
    /// 折起来的一段一样的行；`key` 是这一段（连着留下的几行）开头的行，点开以后记它
    Fold { key: usize, from: usize, to: usize },
    /// 一处冲突的头：按钮在这里
    Head(usize),
    /// 一处冲突里对齐的第 k 行
    Pair { block: usize, k: usize },
}

pub(super) struct MergeView {
    path: String,
    /// 关掉以后回到哪个视图
    back: Mode,
    /// 打开时的内容（换行统一成 LF；能对照共同祖先时是带 ||||||| 的那份）
    text: String,
    /// `text` 按行拆开，每行带着换行符
    lines: Vec<String>,
    /// 原来是 CRLF 的文件，保存时换回去
    crlf: bool,
    /// 原来的编码，保存时照它写回
    charset: Charset,
    /// 打开时文件的散列：保存前核对，期间被别的程序改过就不写（见 [`workfile::write_args`]）
    hash: String,
    blocks: Vec<Block>,
    /// 每一处现在怎么解决的；None 是还没解决
    choices: Vec<Option<Choice>>,
    /// 每一处在结果里的位置（字节）：编辑时跟着挪
    regions: Vec<Range<usize>>,
    /// 按钮（采纳、重置、全部采纳）改结果以后的内容，按顺序：编辑事件回来、内容对得上的，是自己改
    /// 的，各处的位置当时已经算好了，不再挪。点得快时会有几个排着
    expected: std::collections::VecDeque<String>,
    /// 结果（可以直接改）
    editor: Entity<EditorState>,
    /// 结果里没解决的几处的底色
    marks: Option<RangeDecorationCollection>,
    /// 正看着第几处
    focus: usize,
    /// 能对照共同祖先（冲突标记里带着）
    has_base: bool,
    show_base: bool,
    /// 窄的时候看哪一边
    side: Side,
    compare_scroll: UniformListScrollHandle,
    /// 点开了的折叠（开头那一行）
    unfolded: HashSet<usize>,
    loading: bool,
    error: Option<SharedString>,
    _task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl MergeView {
    /// 选过版本、改过内容（关掉会丢东西）。
    fn edited(&self, cx: &App) -> bool {
        if self.loading || self.error.is_some() {
            return false;
        }
        self.choices.iter().any(Option::is_some)
            || self.editor.read(cx).value().as_ref() != self.text
    }

    /// 这几行连起来。
    fn join(&self, range: Range<usize>) -> String {
        self.lines.get(range).unwrap_or_default().concat()
    }

    /// 选了 `choice` 以后这一处在结果里是什么；None 是原样（带冲突标记）。
    fn piece(&self, ix: usize, choice: Option<Choice>) -> String {
        let block = &self.blocks[ix];
        match choice {
            Some(Choice::Current) => self.join(block.ours.clone()),
            Some(Choice::Incoming) => self.join(block.theirs.clone()),
            Some(Choice::Both) => self.join(block.ours.clone()) + &self.join(block.theirs.clone()),
            Some(Choice::Manual) | None => self.join(block.lines.clone()),
        }
    }

    /// 这一处在结果里现在的内容。
    fn region_text(&self, ix: usize, cx: &App) -> String {
        let value = self.editor.read(cx).value();
        let range = self.regions[ix].clone();
        value.get(range).unwrap_or_default().to_string()
    }

    /// 这一处还没解决：结果里它那一段还带着冲突标记。
    fn unresolved(&self, ix: usize, cx: &App) -> bool {
        has_marker(&self.region_text(ix, cx))
    }

    /// 从 `from` 往后（绕回开头）第一处没解决的。
    fn next_unresolved(&self, from: usize, cx: &App) -> Option<usize> {
        let n = self.blocks.len();
        (1..=n)
            .map(|step| (from + step) % n)
            .find(|ix| self.unresolved(*ix, cx))
    }

    /// 对照里的行：冲突前后留 [`CONTEXT`] 行，中间多的折起来（点开过的不折）。
    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut next = 0;
        let same = |rows: &mut Vec<Row>, from: usize, to: usize, first: bool, last: bool| {
            // 开头一段只留后面几行，结尾一段只留前面几行，中间的留两头
            let keep_head = if first { 0 } else { CONTEXT };
            let keep_tail = if last { 0 } else { CONTEXT };
            if to - from > keep_head + keep_tail + 1 && !self.unfolded.contains(&from) {
                rows.extend((from..from + keep_head).map(Row::Same));
                rows.push(Row::Fold {
                    key: from,
                    from: from + keep_head,
                    to: to - keep_tail,
                });
                rows.extend((to - keep_tail..to).map(Row::Same));
            } else {
                rows.extend((from..to).map(Row::Same));
            }
        };
        for (ix, block) in self.blocks.iter().enumerate() {
            same(&mut rows, next, block.lines.start, ix == 0, false);
            rows.push(Row::Head(ix));
            let base = match (self.show_base, &block.base) {
                (true, Some(base)) => base.len(),
                _ => 0,
            };
            let height = block.ours.len().max(block.theirs.len()).max(base).max(1);
            rows.extend((0..height).map(|k| Row::Pair { block: ix, k }));
            next = block.lines.end;
        }
        same(
            &mut rows,
            next,
            self.lines.len(),
            self.blocks.is_empty(),
            true,
        );
        rows
    }
}

/// 一段文字里有没有冲突标记（行首的 `<<<<<<<`、`=======`、`>>>>>>>`）。
pub(super) fn has_marker(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with("<<<<<<<") || line.starts_with(">>>>>>>") || line == "=======")
}

/// 结果里改了一段（`start..old_end` 换成了长 `new_end - start` 的新内容）：每一处的位置跟着挪。
/// 在某一处前面改的整个往后挪，在后面改的不动；两头碰上的不算在里面（在一处的开头、结尾打字不会
/// 并进去），和它有交集的连新内容一起算它的。返回有交集的那几处（被手动改过）。
fn transform(
    regions: &mut [Range<usize>],
    start: usize,
    old_end: usize,
    new_end: usize,
) -> Vec<usize> {
    let delta = new_end as isize - old_end as isize;
    let shift = |at: usize| (at as isize + delta).max(0) as usize;
    let mut touched = Vec::new();
    for (ix, region) in regions.iter_mut().enumerate() {
        let (a, b) = (region.start, region.end);
        if old_end <= a {
            *region = shift(a)..shift(b);
        } else if start >= b {
            // 在后面，不动
        } else {
            let a2 = a.min(start);
            let b2 = if old_end > b { new_end } else { shift(b) };
            *region = a2..b2.max(a2);
            touched.push(ix);
        }
    }
    touched
}

/// 按钮把第 `ix` 处整个换成了长 `len` 的内容：它变成新的那一段，后面的几处跟着挪。
fn replace_region(regions: &mut [Range<usize>], ix: usize, len: usize) {
    let old = regions[ix].clone();
    let delta = len as isize - old.len() as isize;
    for (other, region) in regions.iter_mut().enumerate() {
        if other == ix {
            *region = old.start..old.start + len;
        } else if region.start >= old.end && !(region.start == old.start && other < ix) {
            let shift = |at: usize| (at as isize + delta).max(0) as usize;
            *region = shift(region.start)..shift(region.end);
        }
    }
}

/// 文件的编码。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Charset {
    #[default]
    Utf8,
    /// GB18030（GBK 的超集）
    Gb18030,
}

/// 读出来的文件变成编辑用的文字：换行统一成 LF，第二项是原来是不是 CRLF、第三项是编码（保存时
/// 照原样写回，见 [`encode`]）。不是 UTF-8 的按 GB18030 解，解出来再编回去和原来一字不差才算；
/// 都不是的返回 None：放进编辑框再写回去会坏。
fn decode(bytes: Vec<u8>) -> Option<(String, bool, Charset)> {
    let (text, charset) = match String::from_utf8(bytes) {
        Ok(text) => (text, Charset::Utf8),
        Err(err) => {
            let bytes = err.into_bytes();
            let (text, _, bad) = encoding_rs::GB18030.decode(&bytes);
            if bad {
                return None;
            }
            let (back, _, unmappable) = encoding_rs::GB18030.encode(&text);
            if unmappable || back.as_ref() != bytes.as_slice() {
                return None;
            }
            (text.into_owned(), Charset::Gb18030)
        }
    };
    let crlf = text.contains("\r\n");
    Some((text.replace("\r\n", "\n"), crlf, charset))
}

/// 编辑好的文字变回要写进文件的字节（照原来的换行和编码）。
fn encode(text: &str, crlf: bool, charset: Charset) -> Vec<u8> {
    let text = match crlf {
        true => text.replace('\n', "\r\n"),
        false => text.to_string(),
    };
    match charset {
        Charset::Utf8 => text.into_bytes(),
        Charset::Gb18030 => encoding_rs::GB18030.encode(&text).0.into_owned(),
    }
}

/// 文件里有几处冲突；冲突标记不完整（只有开头没有结尾之类）时返回 None。
fn count_conflicts(path: &str, text: &str) -> Option<usize> {
    DiffFile::parse_conflicts(path.to_string(), text)
        .ok()
        .map(|file| file.conflicts().len())
}

/// 按冲突标记把内容拆成几处。标记不完整时 None。
fn parse_blocks(path: &str, text: &str) -> Option<Vec<Block>> {
    let file = DiffFile::parse_conflicts(path.to_string(), text).ok()?;
    // 组件库的行号从 1 起
    let zero = |range: Range<usize>| range.start.saturating_sub(1)..range.end.saturating_sub(1);
    Some(
        file.conflicts()
            .iter()
            .map(|conflict| Block {
                lines: zero(conflict.lines()),
                ours: zero(conflict.current_lines()),
                base: conflict.base_lines().map(zero),
                theirs: zero(conflict.incoming_lines()),
                ours_label: conflict.current_label().to_string(),
                theirs_label: conflict.incoming_label().to_string(),
            })
            .collect(),
    )
}

/// 每一行在 `lines` 连起来的内容里从第几个字节开始；最后多一个总长度。
fn line_starts(lines: &[String]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(lines.len() + 1);
    let mut at = 0;
    for line in lines {
        starts.push(at);
        at += line.len();
    }
    starts.push(at);
    starts
}

impl RepoView {
    /// 打开合并编辑器，解决 `path` 里的冲突。
    pub(super) fn open_merge_editor(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };
        let repo = self.entry.path.clone();
        let request = channel.git(&repo, workfile::read_args(&path));
        // 能不能对照共同祖先：zdiff3 要 git 2.35
        let zdiff3 = channel
            .git_version()
            .is_some_and(|version| version >= (2, 35, 0));
        let file = path.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = request.await.and_then(|out| out.ok()).and_then(|out| {
                let (hash, text) = workfile::split_read(&out)
                    .ok_or_else(|| anyhow::anyhow!(tr("无法识别读取结果")))?;
                Ok((hash, text.to_vec()))
            });
            // 冲突标记里没有共同祖先：按暂存区里的三份重新合并，核对工作区的文件没被动过，换成带
            // 共同祖先的那份
            let decoded = result
                .as_ref()
                .ok()
                .and_then(|(_, bytes)| decode(bytes.clone()));
            let mut with_base: Option<String> = None;
            if let Some((text, _, _)) = &decoded
                && let Some(blocks) = parse_blocks(&file, text)
                && !blocks.is_empty()
                && blocks.iter().all(|block| block.base.is_none())
            {
                let labels = [
                    blocks[0].ours_label.as_str(),
                    "base",
                    blocks[0].theirs_label.as_str(),
                ];
                let style = if zdiff3 { "zdiff3" } else { "diff3" };
                let plain = channel.git(&repo, workfile::base_args(&file, "merge", labels));
                let diff3 = channel.git(&repo, workfile::base_args(&file, style, labels));
                let (plain, diff3) = futures::join!(plain, diff3);
                let read = |out: anyhow::Result<fr_channel::GitOutput>| {
                    out.ok()
                        .filter(|out| out.success())
                        .and_then(|out| decode(out.stdout))
                        .map(|(text, _, _)| text)
                };
                if read(plain).as_deref() == Some(text.as_str()) {
                    with_base = read(diff3);
                }
            }
            this.update_in(cx, |this, window, cx| {
                let Some(view) = &mut this.merge else {
                    return;
                };
                // 上次关掉时留下了这个文件的合并编辑：文件没被改过就接着编辑，改过就作废
                if let (Ok((hash, _)), Some(at)) = (
                    &result,
                    this.merge_drafts.iter().position(|d| d.path == view.path),
                ) {
                    let mut draft = this.merge_drafts.remove(at);
                    if draft.hash == *hash {
                        draft.back = view.back;
                        this.merge = Some(draft);
                        cx.notify();
                        return;
                    }
                    let text = tf!(
                        "{path} 在此期间已被修改，之前未保存的合并编辑已作废。",
                        path = draft.path
                    );
                    window.push_notification(Notification::warning(text), cx);
                }
                let Some(view) = &mut this.merge else {
                    return;
                };
                view.loading = false;
                let hash = match &result {
                    Ok((hash, _)) => hash.clone(),
                    Err(err) => {
                        view.error =
                            Some(tf!("读取文件失败：{err}", err = format!("{:#}", err)).into());
                        cx.notify();
                        return;
                    }
                };
                let Some((text, crlf, charset)) = decoded else {
                    // 说清在哪解决：「外部编辑器」在远程主机上指的就是终端里的编辑器
                    view.error = Some(
                        tr("此文件不是 UTF-8 或 GBK 编码，无法在此编辑。请在此仓库的终端中编辑解决冲突后运行 git add，或用 git checkout --ours 或 --theirs -- 文件名 整个采用一方，再回到「本地修改」。")
                            .into(),
                    );
                    cx.notify();
                    return;
                };
                view.hash = hash;
                view.crlf = crlf;
                view.charset = charset;
                // 换成带共同祖先的那份（核对过和工作区里的一样，只是多了共同祖先）
                let text = with_base.unwrap_or(text);
                let blocks = parse_blocks(&view.path, &text).unwrap_or_default();
                view.has_base = !blocks.is_empty() && blocks.iter().all(|b| b.base.is_some());
                view.lines = text.split_inclusive('\n').map(str::to_string).collect();
                let starts = line_starts(&view.lines);
                view.regions = blocks
                    .iter()
                    .map(|block| starts[block.lines.start]..starts[block.lines.end])
                    .collect();
                view.choices = vec![None; blocks.len()];
                view.blocks = blocks;
                view.text = text.clone();
                view.editor
                    .update(cx, |state, cx| state.set_value(text, window, cx));
                let marks = view
                    .editor
                    .update(cx, |state, cx| state.create_range_decorations_collection(Vec::new(), cx));
                view.marks = Some(marks);
                this.merge_marks(cx);
                cx.notify();
            })
            .ok();
        });
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(true)
                .searchable(true)
        });
        let edits = cx.subscribe(
            &editor,
            |this: &mut RepoView, _, event: &InputEditsEvent, cx| this.merge_edited(event, cx),
        );
        let back = match self.mode {
            Mode::Merge => self.merge.as_ref().map_or(Mode::Changes, |view| view.back),
            Mode::Blame => Mode::Changes,
            other => other,
        };
        // 正开着另一个文件的合并编辑：编辑过的留着，不丢
        self.keep_merge_draft(cx);
        self.merge = Some(MergeView {
            path,
            back,
            text: String::new(),
            lines: Vec::new(),
            crlf: false,
            charset: Charset::Utf8,
            hash: String::new(),
            blocks: Vec::new(),
            choices: Vec::new(),
            regions: Vec::new(),
            expected: std::collections::VecDeque::new(),
            editor,
            marks: None,
            focus: 0,
            has_base: false,
            show_base: false,
            side: Side::Current,
            compare_scroll: UniformListScrollHandle::new(),
            unfolded: HashSet::new(),
            loading: true,
            error: None,
            _task: task,
            _subscriptions: vec![edits],
        });
        self.blame = None;
        self.mode = Mode::Merge;
        cx.notify();
    }

    /// 关掉合并编辑器。已经选了版本、改过内容的留着（见 [`Self::keep_merge_draft`]），
    /// 重新打开这个文件时接着编辑。原来直接丢掉，解决到一半的冲突要从头再来。
    pub(super) fn close_merge_editor(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = &self.merge {
            self.mode = view.back;
        }
        self.keep_merge_draft(cx);
        cx.notify();
    }

    /// 保存成功以后关掉：内容已经写回文件，不留。
    fn finish_merge_editor(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.merge.take() {
            self.mode = view.back;
            self.merge_drafts.retain(|draft| draft.path != view.path);
        }
        cx.notify();
    }

    /// 把开着的合并编辑收起来：选过版本、改过内容的放进 `merge_drafts`，没动过的直接丢掉。
    fn keep_merge_draft(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.merge.take() else {
            return;
        };
        if view.edited(cx) {
            self.merge_drafts.retain(|draft| draft.path != view.path);
            self.merge_drafts.push(view);
        }
    }

    /// 不再有冲突的文件，留着的合并编辑用不上了（别处解决了、合并中止了）。
    pub(super) fn prune_merge_drafts(&mut self, status: &fr_core::status::Status) {
        let conflicted = |path: &str| {
            status
                .entries
                .iter()
                .any(|e| e.path == path && e.unstaged == Some(fr_core::status::Kind::Conflicted))
        };
        self.merge_drafts.retain(|draft| conflicted(&draft.path));
    }

    /// 没保存的合并编辑有几个：开着的那个改过算一个，加上留着的（关标签页、退出前要问）。
    pub fn unsaved_merges(&self, cx: &App) -> usize {
        let open = self.merge.as_ref().is_some_and(|view| view.edited(cx));
        usize::from(open) + self.merge_drafts.len()
    }

    /// 结果被编辑了：每一处的位置跟着挪，在某一处里改的记成手动修改。按钮替换的那一处记成选的那样。
    fn merge_edited(&mut self, event: &InputEditsEvent, cx: &mut Context<Self>) {
        let Some(view) = &mut self.merge else {
            return;
        };
        // 按钮改的：位置当时就算好了
        let last = event.edits().last().map(|(_, text)| text.to_string());
        if let (Some(front), Some(last)) = (view.expected.front(), &last)
            && front == last
        {
            view.expected.pop_front();
            self.merge_marks(cx);
            cx.notify();
            return;
        }
        // 对不上：之前排着的那几个不会再来了（比如撤销把它们合到了一起），不再等
        view.expected.clear();
        for (edit, _) in event.edits() {
            let (start, old_end, new_end) = (edit.start_byte, edit.old_end_byte, edit.new_end_byte);
            for ix in transform(&mut view.regions, start, old_end, new_end) {
                view.choices[ix] = Some(Choice::Manual);
            }
        }
        self.merge_marks(cx);
        cx.notify();
    }

    /// 结果里没解决的几处铺上底色，正看着的那一处加个框。
    fn merge_marks(&mut self, cx: &mut Context<Self>) {
        let Some(view) = &self.merge else {
            return;
        };
        let theme = cx.theme();
        let (warning, primary) = (theme.warning, theme.primary);
        let mut marks = Vec::new();
        for ix in 0..view.blocks.len() {
            let range = view.regions[ix].clone();
            if range.is_empty() {
                continue;
            }
            if view.unresolved(ix, cx) {
                marks.push(
                    RangeDecoration::new(range.clone())
                        .with_style(RangeDecorationStyle::Fill)
                        .with_color(warning.opacity(0.13)),
                );
            }
            if ix == view.focus {
                marks.push(
                    RangeDecoration::new(range)
                        .with_style(RangeDecorationStyle::Frame)
                        .with_color(primary.opacity(0.7)),
                );
            }
        }
        if let Some(collection) = view.marks.clone() {
            collection.set(marks, cx);
        }
    }

    /// 第 `ix` 处选 `choice`（None 是重置成原样）。在结果里替换它那一段，然后跳到下一处没解决的。
    fn merge_choose(
        &mut self,
        ix: usize,
        choice: Option<Choice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = &mut self.merge else {
            return;
        };
        if ix >= view.blocks.len() {
            return;
        }
        let piece = view.piece(ix, choice);
        let range = view.regions[ix].clone();
        view.choices[ix] = choice;
        let value = view.editor.read(cx).value().to_string();
        if value.get(range.clone()) != Some(piece.as_str()) {
            // 位置现在就算好（跳到下一处没解决的要用），编辑事件回来时认出是自己改的
            let mut after = value;
            after.replace_range(range.clone(), &piece);
            replace_region(&mut view.regions, ix, piece.len());
            view.expected.push_back(after);
            let editor = view.editor.clone();
            editor.update(cx, |state, cx| {
                // 替换前后不动滚动的位置：结果不会因为点了上面的按钮跳走
                let scroll = state.scroll_offset();
                state.set_selected_range(range, cx);
                state.replace(piece, window, cx);
                state.set_scroll_offset(scroll, cx);
            });
        }
        // 选了以后跳到下一处没解决的；重置的留在原地
        if choice.is_some()
            && let Some(view) = &self.merge
            && let Some(next) = view.next_unresolved(ix, cx)
        {
            self.merge_focus(next, cx);
        }
        self.merge_marks(cx);
        cx.notify();
    }

    /// 每一处都取同一边（手动改过的不动）。一次换好整个结果，能撤销。
    fn merge_pick_all(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &mut self.merge else {
            return;
        };
        let mut text = view.editor.read(cx).value().to_string();
        // 从后往前换：前面的位置不受影响
        for ix in (0..view.blocks.len()).rev() {
            if view.choices[ix] == Some(Choice::Manual) {
                continue;
            }
            let piece = view.piece(ix, Some(choice));
            let range = view.regions[ix].clone();
            if text.get(range.clone()).is_none() {
                continue;
            }
            text.replace_range(range, &piece);
            replace_region(&mut view.regions, ix, piece.len());
            view.choices[ix] = Some(choice);
        }
        if text == view.editor.read(cx).value().as_ref() {
            self.merge_marks(cx);
            cx.notify();
            return;
        }
        view.expected.push_back(text.clone());
        let editor = view.editor.clone();
        editor.update(cx, |state, cx| {
            let scroll = state.scroll_offset();
            state.replace_all(text, window, cx);
            state.set_scroll_offset(scroll, cx);
        });
        self.merge_marks(cx);
        cx.notify();
    }

    /// 看第 `ix` 处：对照滚到它，结果也滚到它。
    fn merge_focus(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(view) = &mut self.merge else {
            return;
        };
        if ix >= view.blocks.len() {
            return;
        }
        view.focus = ix;
        let rows = view.rows();
        if let Some(head) = rows.iter().position(|row| *row == Row::Head(ix)) {
            // 这一处整段都看得见就不动；露出一截（比如只看得见表头）也要滚，表头放到顶上、上面留一行
            let last = rows
                .iter()
                .rposition(|row| matches!(row, Row::Pair { block, .. } if *block == ix))
                .unwrap_or(head);
            let row_h = row_height(crate::zoom::code_size(cx));
            let (top, height) = {
                let state = view.compare_scroll.0.borrow();
                (
                    -f32::from(state.base_handle.offset().y),
                    f32::from(state.base_handle.bounds().size.height),
                )
            };
            let shown = height > 0.
                && head as f32 * row_h >= top
                && (last + 1) as f32 * row_h <= top + height;
            if !shown {
                view.compare_scroll
                    .scroll_to_item_strict_with_offset(head, ScrollStrategy::Top, 1);
            }
        }
        let start = view.regions[ix].start;
        let line = view
            .editor
            .read(cx)
            .value()
            .get(..start)
            .map_or(0, |head| head.matches('\n').count());
        view.editor.update(cx, |state, cx| {
            if let Some(height) = state.line_height() {
                let y = (line.saturating_sub(2)) as f32 * f32::from(height);
                state.set_scroll_offset(point(px(0.), px(-y)), cx);
            }
        });
        self.merge_marks(cx);
        cx.notify();
    }

    /// 上一处、下一处（绕回）。
    fn merge_step(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(view) = &self.merge else {
            return;
        };
        let n = view.blocks.len();
        if n == 0 {
            return;
        }
        let next = match down {
            true => (view.focus + 1) % n,
            false => (view.focus + n - 1) % n,
        };
        self.merge_focus(next, cx);
    }

    /// Ctrl+F 在合并编辑器里：在结果里查找。
    pub(super) fn find_in_merge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.merge else {
            return;
        };
        if !view.loading && view.error.is_none() {
            super::files::open_editor_search(&view.editor, window, cx);
        }
    }

    /// 保存并标记为已解决：结果里不能再有冲突标记。
    fn merge_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.merge else {
            return;
        };
        let text = view.editor.read(cx).value().to_string();
        match count_conflicts(&view.path, &text) {
            Some(0) if !has_marker(&text) => {}
            Some(0) | None => {
                let note =
                    tr("结果里仍有不完整的冲突标记（<<<<<<<、=======、>>>>>>>）。请删除后保存。");
                window.push_notification(Notification::warning(note), cx);
                return;
            }
            Some(left) => {
                let note = tf!(
                    "还有 {left} 处冲突未解决：请在上方选择版本，或直接在结果中修改后保存。",
                    left
                );
                window.push_notification(Notification::warning(note), cx);
                return;
            }
        }
        let path = view.path.clone();
        let bytes = encode(&text, view.crlf, view.charset);
        // 打开以后文件被别的程序改过（外部编辑器、终端里的 git）：不写，编辑的内容留在编辑器里
        let changed =
            tr("文件在编辑期间已被其他程序修改，未保存。可先复制编辑内容，再重新打开此文件。");
        let steps = vec![
            Step::input(workfile::write_args(&path, &view.hash, changed), bytes)
                .shown(workfile::write_shown(&path)),
            Step::local(vec!["add".into(), "--".into(), path.clone()]),
        ];
        let label = tr("解决冲突");
        self.run_steps(
            label,
            steps,
            window,
            cx,
            Box::new(move |this, result, w, cx| match result {
                Ok(_) => {
                    this.finish_merge_editor(cx);
                    this.show_changes(cx);
                    let text = tf!("{path} 已保存，并标记为已解决", path);
                    w.push_notification(Notification::success(text), cx);
                }
                Err(text) if text.contains(changed) => {
                    w.push_notification(Notification::warning(changed).autohide(false), cx);
                }
                Err(text) => super::ops::failure(w, cx, label, text),
            }),
        );
    }

    pub(super) fn render_merge(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, danger, accent, warning, success) = (
            theme.border,
            theme.muted_foreground,
            theme.danger,
            theme.primary,
            theme.warning,
            theme.success,
        );
        let (ours_long, theirs_long) = self.side_labels();
        let busy = self.op.is_some();
        let ext_tip = self.ext_label(crate::ext_tools::Kind::Merge, cx);
        // 内容区多宽：窄了对照一次只看一边
        let width = f32::from(self.viewport.width) - self.sizes(cx).sidebar;
        let one_side = width < ONE_SIDE_BELOW;
        let Some(view) = &self.merge else {
            return div().into_any_element();
        };
        let path: SharedString = view.path.clone().into();
        let ready = !view.loading && view.error.is_none();
        let total = view.blocks.len();
        let left = (0..total).filter(|ix| view.unresolved(*ix, cx)).count();
        let text_now = view.editor.read(cx).value();
        let stray = total == 0 && has_marker(&text_now);
        let status: SharedString = if view.loading {
            tr("正在读取文件…").into()
        } else if total == 0 {
            match stray {
                true => {
                    tr("冲突标记不完整，无法逐处对照。请直接在下方修改，删除所有冲突标记后保存。")
                        .into()
                }
                false => tr("文件里没有冲突标记。确认内容后点击「保存并标记为已解决」。").into(),
            }
        } else {
            tf!(
                "共 {total} 处冲突，第 {at} 处 · 还有 {left} 处未解决",
                total,
                at = view.focus + 1,
                left
            )
            .into()
        };
        let base_tip: SharedString = match view.has_base {
            true => tr("对照共同祖先：双方都改之前的样子").into(),
            false => tr("此文件已在冲突后被修改，或合并时用了特殊选项，无法对照共同祖先").into(),
        };
        let compare = if let Some(error) = view.error.clone() {
            div()
                .p_4()
                .text_sm()
                .text_color(danger)
                .whitespace_normal()
                .child(error)
                .into_any_element()
        } else if view.loading || total == 0 {
            div().into_any_element()
        } else {
            let rows = Rc::new(view.rows());
            let scroll = view.compare_scroll.clone();
            div()
                .relative()
                .size_full()
                .child(
                    uniform_list(
                        "merge-compare",
                        rows.len(),
                        cx.processor(move |this, range: Range<usize>, _window, cx| {
                            this.render_merge_rows(&rows, range, one_side, cx)
                        }),
                    )
                    .track_scroll(&scroll)
                    .size_full(),
                )
                // 按住 Ctrl 滚滚轮调代码字号
                .child(crate::zoom::wheel_zoom(self.app.clone()))
                .into_any_element()
        };
        let result_head: SharedString = match (total, left) {
            (0, _) => tr("结果 · 可直接编辑").into(),
            (_, 0) => tr("结果 · 已全部解决，可直接编辑").into(),
            (_, left) => tf!("结果 · 还有 {left} 处未解决，可直接编辑", left).into(),
        };
        let tip = path.clone();
        let side = view.side;
        let show_base = view.show_base;
        let has_base = view.has_base;
        let has_blocks = ready && total > 0;
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h(px(38.))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .child(ui::icon(IconName::GitMerge).flex_none().text_color(accent))
                    .child(
                        div()
                            .flex_none()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr("解决冲突")),
                    )
                    .child(
                        div()
                            .id("merge-path")
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_start()
                            .text_sm()
                            .text_color(muted)
                            .child(ui::one_line(path))
                            .tooltip(move |window, cx| ui::tip(tip.clone(), window, cx)),
                    )
                    .child(
                        Button::new("merge-external")
                            .small()
                            .ghost()
                            .label(tr("外部工具…"))
                            .tooltip(ext_tip)
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let Some(path) = this.merge.as_ref().map(|view| view.path.clone()) else {
                                    return;
                                };
                                // 交给外部工具：这里没保存的选择留着（关掉合并编辑器时照样保留）
                                this.close_merge_editor(cx);
                                this.external_merge(path, window, cx);
                            })),
                    )
                    .child(
                        Button::new("merge-save")
                            .small()
                            .primary()
                            .label(tr("保存并标记为已解决"))
                            .disabled(!ready || busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.merge_save(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("merge-close")
                            .small()
                            .ghost()
                            .label(tr("关闭"))
                            .tooltip(tr("返回「本地修改」。未保存的选择和编辑将保留，重新打开此文件可继续。"))
                            .on_click(cx.listener(|this, _, _, cx| this.close_merge_editor(cx))),
                    ),
            )
            .child(
                h_flex()
                    .min_h(px(32.))
                    .flex_none()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .flex_wrap()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.))
                            .text_xs()
                            .text_color(if left > 0 && total > 0 { warning } else { muted })
                            .whitespace_normal()
                            .child(status),
                    )
                    .when(has_blocks, |bar| {
                        bar.child(
                            Button::new("merge-prev")
                                .xsmall()
                                .ghost()
                                .icon(ui::icon(IconName::ChevronUp))
                                .tooltip(tr("上一处冲突"))
                                .on_click(cx.listener(|this, _, _, cx| this.merge_step(false, cx))),
                        )
                        .child(
                            Button::new("merge-next")
                                .xsmall()
                                .ghost()
                                .icon(ui::icon(IconName::ChevronDown))
                                .tooltip(tr("下一处冲突"))
                                .on_click(cx.listener(|this, _, _, cx| this.merge_step(true, cx))),
                        )
                        .child(
                            Button::new("merge-all-ours")
                                .xsmall()
                                .label(tr("全部采纳当前"))
                                .tooltip(ours_long)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.merge_pick_all(Choice::Current, window, cx)
                                })),
                        )
                        .child(
                            Button::new("merge-all-theirs")
                                .xsmall()
                                .label(tr("全部采纳传入"))
                                .tooltip(theirs_long)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.merge_pick_all(Choice::Incoming, window, cx)
                                })),
                        )
                        .child(
                            div().id("merge-base-wrap").child(
                                Checkbox::new("merge-base")
                                    .label(tr("显示共同祖先"))
                                    .checked(show_base && has_base)
                                    .tooltip(base_tip)
                                    .disabled(!has_base)
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        if let Some(view) = &mut this.merge {
                                            view.show_base = *checked;
                                            if !*checked && view.side == Side::Base {
                                                view.side = Side::Current;
                                            }
                                        }
                                        cx.notify();
                                    })),
                            ),
                        )
                    }),
            )
            // 窄的时候对照一次只看一边
            .when(has_blocks && one_side, |page| {
                page.child(
                    h_flex()
                        .flex_none()
                        .h(px(32.))
                        .px_3()
                        .gap_2()
                        .border_b_1()
                        .border_color(border)
                        .text_xs()
                        .text_color(muted)
                        .child(tr("对照："))
                        .child(
                            ButtonGroup::new("merge-side")
                                .outline()
                                .xsmall()
                                .child(
                                    Button::new("merge-side-ours")
                                        .label(tr("当前"))
                                        .selected(side == Side::Current)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.merge_side(Side::Current, cx)
                                        })),
                                )
                                .child(
                                    Button::new("merge-side-theirs")
                                        .label(tr("传入"))
                                        .selected(side == Side::Incoming)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.merge_side(Side::Incoming, cx)
                                        })),
                                )
                                .when(has_base, |group| {
                                    group.child(
                                        Button::new("merge-side-base")
                                            .label(tr("共同祖先"))
                                            .selected(side == Side::Base)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.merge_side(Side::Base, cx)
                                            })),
                                    )
                                }),
                        ),
                )
            })
            .when(has_blocks || view.error.is_some(), |page| {
                page.child(
                    div()
                        .flex_none()
                        .h(relative(if view.error.is_some() { 1. } else { 0.52 }))
                        .min_h(px(120.))
                        .border_b_1()
                        .border_color(border)
                        .child(compare),
                )
            })
            .when(ready, |page| {
                page.child(
                    h_flex()
                        .flex_none()
                        .h(px(28.))
                        .px_3()
                        .gap_2()
                        .border_b_1()
                        .border_color(border)
                        .text_xs()
                        .child(
                            div()
                                .size(px(7.))
                                .rounded_full()
                                .bg(if left > 0 && total > 0 { warning } else { success }),
                        )
                        .child(div().text_color(muted).child(result_head)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(Editor::new(&view.editor).h_full()),
                )
            })
            .into_any_element()
    }

    /// 窄的时候对照看哪一边。
    fn merge_side(&mut self, side: Side, cx: &mut Context<Self>) {
        if let Some(view) = &mut self.merge {
            view.side = side;
            if side == Side::Base {
                view.show_base = true;
            }
        }
        cx.notify();
    }

    fn render_merge_rows(
        &mut self,
        rows: &[Row],
        range: Range<usize>,
        one_side: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.theme();
        let (border, muted, fg, primary, warning, success, hover, panel) = (
            theme.border,
            theme.muted_foreground,
            theme.foreground,
            theme.primary,
            theme.warning,
            theme.success,
            theme.list_hover,
            theme.muted,
        );
        // 当前是蓝，传入是紫，共同祖先是灰
        let ours_bg = hsla(212. / 360., 0.75, 0.55, 0.12);
        let theirs_bg = hsla(275. / 360., 0.6, 0.6, 0.13);
        let base_bg = panel.opacity(0.6);
        let Some(view) = &self.merge else {
            return Vec::new();
        };
        let columns: Vec<Side> = match (one_side, view.show_base && view.has_base) {
            (true, _) => vec![view.side],
            (false, true) => vec![Side::Current, Side::Base, Side::Incoming],
            (false, false) => vec![Side::Current, Side::Incoming],
        };
        let mono = crate::fonts::mono();
        let size = crate::zoom::code_size(cx);
        let row_h = row_height(size);
        let cell = |text: Option<String>, bg: Option<Hsla>, chosen: bool, faded: bool| {
            div()
                .flex_1()
                .min_w_0()
                .h_full()
                .px(px(8.))
                .border_l_2()
                .border_color(if chosen {
                    primary
                } else {
                    gpui_kit::transparent_black()
                })
                .when_some(bg, |d, bg| d.bg(bg))
                .when(faded, |d| d.opacity(0.5))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(match text {
                    Some(text) => text.trim_end_matches('\n').to_string(),
                    None => String::new(),
                })
        };
        range
            .filter_map(|ix| {
                let row = *rows.get(ix)?;
                let element = match row {
                    Row::Same(line) => {
                        let text = view.lines.get(line).cloned();
                        h_flex()
                            .h(px(row_h))
                            .w_full()
                            .text_color(muted)
                            .children(
                                columns
                                    .iter()
                                    .map(|_| cell(text.clone(), None, false, false)),
                            )
                            .into_any_element()
                    }
                    Row::Fold { key, from, to } => h_flex()
                        .id(("merge-fold", from))
                        .h(px(row_h))
                        .w_full()
                        .px_3()
                        .gap_2()
                        .bg(panel.opacity(0.35))
                        .text_xs()
                        .text_color(muted)
                        .cursor_pointer()
                        .hover(|s| s.bg(hover))
                        .child(ui::icon_small(IconName::ChevronsUpDown))
                        .child(tf!("{n} 行相同，点击展开", n = to - from))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(view) = &mut this.merge {
                                view.unfolded.insert(key);
                            }
                            cx.notify();
                        }))
                        .into_any_element(),
                    Row::Head(block) => {
                        let choice = view.choices.get(block).copied().flatten();
                        let unresolved = view.unresolved(block, cx);
                        let focused = view.focus == block;
                        let b = &view.blocks[block];
                        let state: (SharedString, Hsla) = match (unresolved, choice) {
                            (true, Some(Choice::Manual)) => {
                                (tr("已手动修改，仍有冲突标记").into(), warning)
                            }
                            (true, _) => (tr("未解决").into(), warning),
                            (false, Some(Choice::Current)) => (tr("已采纳当前").into(), success),
                            (false, Some(Choice::Incoming)) => (tr("已采纳传入").into(), success),
                            (false, Some(Choice::Both)) => (tr("两者都要").into(), success),
                            (false, _) => (tr("已手动修改").into(), success),
                        };
                        let resettable = choice.is_some();
                        let head_cell = |side: Side| -> AnyElement {
                            let base = h_flex().flex_1().min_w_0().h_full().px(px(6.)).gap(px(6.));
                            match side {
                                Side::Current => base
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .when(focused, |d| d.text_color(primary))
                                            .child(tf!("第 {n} 处", n = block + 1)),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_xs()
                                            .text_color(state.1)
                                            .child(state.0.clone()),
                                    )
                                    .child(
                                        ui::clipped(
                                            ("merge-ours-label", block),
                                            tf!("当前 · {label}", label = b.ours_label.as_str()),
                                        )
                                        .flex_shrink(1.)
                                        .text_xs()
                                        .text_color(muted),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        Button::new(("merge-take-ours", block))
                                            .xsmall()
                                            .label(tr("采纳当前"))
                                            .selected(
                                                choice == Some(Choice::Current) && !unresolved,
                                            )
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.merge_choose(
                                                    block,
                                                    Some(Choice::Current),
                                                    window,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .into_any_element(),
                                Side::Base => base
                                    .child(div().text_xs().text_color(muted).child(tr("共同祖先")))
                                    .into_any_element(),
                                Side::Incoming => base
                                    .child(
                                        ui::clipped(
                                            ("merge-theirs-label", block),
                                            tf!("传入 · {label}", label = b.theirs_label.as_str()),
                                        )
                                        .flex_shrink(1.)
                                        .text_xs()
                                        .text_color(muted),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        Button::new(("merge-take-theirs", block))
                                            .xsmall()
                                            .label(tr("采纳传入"))
                                            .selected(
                                                choice == Some(Choice::Incoming) && !unresolved,
                                            )
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.merge_choose(
                                                    block,
                                                    Some(Choice::Incoming),
                                                    window,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .child(
                                        Button::new(("merge-take-both", block))
                                            .xsmall()
                                            .label(tr("两者都要"))
                                            .tooltip(tr("先当前、后传入"))
                                            .selected(choice == Some(Choice::Both) && !unresolved)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.merge_choose(
                                                    block,
                                                    Some(Choice::Both),
                                                    window,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .when(resettable, |row| {
                                        row.child(
                                            Button::new(("merge-reset", block))
                                                .xsmall()
                                                .ghost()
                                                .icon(ui::icon_small(IconName::RotateCcw))
                                                .tooltip(tr("重置：恢复成带冲突标记的原样"))
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        cx.stop_propagation();
                                                        this.merge_choose(block, None, window, cx)
                                                    },
                                                )),
                                        )
                                    })
                                    .into_any_element(),
                            }
                        };
                        // 只看一边时，按钮都放在这一边上
                        let cells: Vec<AnyElement> = match columns.as_slice() {
                            [_] => vec![
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .px(px(6.))
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .when(focused, |d| d.text_color(primary))
                                            .child(tf!("第 {n} 处", n = block + 1)),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_xs()
                                            .text_color(state.1)
                                            .child(state.0.clone()),
                                    )
                                    .child(
                                        Button::new(("merge-take-ours", block))
                                            .xsmall()
                                            .label(tr("当前"))
                                            .tooltip(tr("采纳当前"))
                                            .selected(
                                                choice == Some(Choice::Current) && !unresolved,
                                            )
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.merge_choose(
                                                    block,
                                                    Some(Choice::Current),
                                                    window,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .child(
                                        Button::new(("merge-take-theirs", block))
                                            .xsmall()
                                            .label(tr("传入"))
                                            .tooltip(tr("采纳传入"))
                                            .selected(
                                                choice == Some(Choice::Incoming) && !unresolved,
                                            )
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.merge_choose(
                                                    block,
                                                    Some(Choice::Incoming),
                                                    window,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .child(
                                        Button::new(("merge-take-both", block))
                                            .xsmall()
                                            .label(tr("两者"))
                                            .tooltip(tr("两者都要：先当前、后传入"))
                                            .selected(choice == Some(Choice::Both) && !unresolved)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.merge_choose(
                                                    block,
                                                    Some(Choice::Both),
                                                    window,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .when(resettable, |row| {
                                        row.child(
                                            Button::new(("merge-reset", block))
                                                .xsmall()
                                                .ghost()
                                                .icon(ui::icon_small(IconName::RotateCcw))
                                                .tooltip(tr("重置：恢复成带冲突标记的原样"))
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        cx.stop_propagation();
                                                        this.merge_choose(block, None, window, cx)
                                                    },
                                                )),
                                        )
                                    })
                                    .into_any_element(),
                            ],
                            _ => columns.iter().map(|side| head_cell(*side)).collect(),
                        };
                        h_flex()
                            .id(("merge-head", block))
                            .h(px(row_h))
                            .w_full()
                            // 当前这一处要一眼看得出：顶边和底色都用主题色
                            .border_t_1()
                            .border_color(if focused { primary } else { border })
                            .bg(if focused {
                                primary.opacity(0.2)
                            } else {
                                panel.opacity(0.5)
                            })
                            .text_color(fg)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.merge_focus(block, cx)),
                            )
                            .children(cells)
                            .into_any_element()
                    }
                    Row::Pair { block, k } => {
                        let b = &view.blocks[block];
                        let choice = view.choices.get(block).copied().flatten();
                        let resolved = !view.unresolved(block, cx);
                        let line = |range: &Range<usize>| {
                            (k < range.len()).then(|| view.lines[range.start + k].clone())
                        };
                        let cells = columns.iter().map(|side| match side {
                            Side::Current => {
                                let chosen = resolved
                                    && matches!(choice, Some(Choice::Current) | Some(Choice::Both));
                                let text = line(&b.ours);
                                let faded = resolved && !chosen && choice != Some(Choice::Manual);
                                cell(text.clone(), text.map(|_| ours_bg), chosen, faded)
                            }
                            Side::Incoming => {
                                let chosen = resolved
                                    && matches!(
                                        choice,
                                        Some(Choice::Incoming) | Some(Choice::Both)
                                    );
                                let text = line(&b.theirs);
                                let faded = resolved && !chosen && choice != Some(Choice::Manual);
                                cell(text.clone(), text.map(|_| theirs_bg), chosen, faded)
                            }
                            Side::Base => {
                                let text = b.base.as_ref().and_then(line);
                                cell(text.clone(), text.map(|_| base_bg), false, resolved)
                            }
                        });
                        h_flex()
                            .h(px(row_h))
                            .w_full()
                            .children(cells)
                            .into_any_element()
                    }
                };
                Some(
                    div()
                        .w_full()
                        .font_family(mono)
                        .text_size(px(size - 0.5))
                        .line_height(px(row_h))
                        .child(element)
                        .into_any_element(),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Charset, count_conflicts, decode, encode, has_marker, line_starts, parse_blocks,
        replace_region, transform,
    };
    use fr_core::workfile;
    use gpui_kit::component::diff::{DiffConflict, DiffConflictResolution, DiffFile};
    use std::io::Write as _;
    use std::ops::Range;
    use std::path::{Path, PathBuf};
    use std::process::{Output, Stdio};

    /// 按每一处的选择拼出文件的内容。没选的那几处原样留着（带冲突标记）。
    /// `text` 是解析出 `conflicts` 的那份内容。
    fn assemble(
        text: &str,
        conflicts: &[DiffConflict],
        resolution: impl Fn(usize) -> Option<DiffConflictResolution>,
    ) -> String {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        // 组件库给的行号从 1 起、不含末尾
        let take = |out: &mut String, range: Range<usize>| {
            let (start, end) = (range.start.saturating_sub(1), range.end.saturating_sub(1));
            for line in lines.get(start..end.min(lines.len())).unwrap_or_default() {
                out.push_str(line);
            }
        };
        let mut out = String::with_capacity(text.len());
        let mut next = 1;
        for (ix, conflict) in conflicts.iter().enumerate() {
            let all = conflict.lines();
            take(&mut out, next..all.start);
            match resolution(ix) {
                Some(DiffConflictResolution::Current) => take(&mut out, conflict.current_lines()),
                Some(DiffConflictResolution::Incoming) => take(&mut out, conflict.incoming_lines()),
                Some(DiffConflictResolution::Both) => {
                    take(&mut out, conflict.current_lines());
                    take(&mut out, conflict.incoming_lines());
                }
                _ => take(&mut out, all.clone()),
            }
            next = all.end;
        }
        take(&mut out, next..lines.len() + 1);
        out
    }

    const TEXT: &str = "head
<<<<<<< HEAD
ours 1
ours 2
=======
theirs 1
>>>>>>> feature
middle
<<<<<<< HEAD
=======
only theirs
>>>>>>> feature
tail
";

    fn resolved(choices: [Option<DiffConflictResolution>; 2]) -> String {
        let file = DiffFile::parse_conflicts("a.txt", TEXT).unwrap();
        assert_eq!(file.conflicts().len(), 2);
        assemble(TEXT, file.conflicts(), |ix| choices[ix])
    }

    #[test]
    fn 按每一处的选择拼出内容() {
        use DiffConflictResolution::{Both, Current, Incoming};
        // 都没选：原样
        assert_eq!(resolved([None, None]), TEXT);
        assert_eq!(
            resolved([Some(Current), Some(Incoming)]),
            "head\nours 1\nours 2\nmiddle\nonly theirs\ntail\n"
        );
        assert_eq!(
            resolved([Some(Incoming), Some(Current)]),
            "head\ntheirs 1\nmiddle\ntail\n"
        );
        assert_eq!(
            resolved([Some(Both), Some(Both)]),
            "head\nours 1\nours 2\ntheirs 1\nmiddle\nonly theirs\ntail\n"
        );
        // 只选了一处：另一处还带着标记
        let half = resolved([Some(Current), None]);
        assert!(half.starts_with("head\nours 1\nours 2\nmiddle\n<<<<<<< HEAD\n"));
        assert_eq!(count_conflicts("a.txt", &half), Some(1));
        assert_eq!(
            count_conflicts("a.txt", &resolved([Some(Current), Some(Current)])),
            Some(0)
        );
    }

    #[test]
    fn 拆成几处_位置对得上() {
        let blocks = parse_blocks("a.txt", TEXT).unwrap();
        assert_eq!(blocks.len(), 2);
        let lines: Vec<String> = TEXT.split_inclusive('\n').map(str::to_string).collect();
        let starts = line_starts(&lines);
        let block = &blocks[0];
        assert_eq!(block.lines, 1..7);
        assert_eq!(lines[block.ours.clone()].concat(), "ours 1\nours 2\n");
        assert_eq!(lines[block.theirs.clone()].concat(), "theirs 1\n");
        assert_eq!(block.ours_label, "HEAD");
        assert_eq!(block.theirs_label, "feature");
        // 第一处在全文里的字节范围就是它的全部行
        let region = starts[block.lines.start]..starts[block.lines.end];
        assert!(TEXT[region].starts_with("<<<<<<< HEAD\n"));
        assert!(blocks[1].ours.is_empty());
    }

    #[test]
    fn 编辑时各处的位置跟着挪() {
        // 三处：0..10、20..30、40..40（空的）
        let mut regions = vec![0..10, 20..30, 40..40];
        // 在两处之间插 5 个字：后面两处往后挪，都没碰到
        assert!(transform(&mut regions, 15, 15, 20).is_empty());
        assert_eq!(regions, vec![0..10, 25..35, 45..45]);
        // 在第二处的开头打字：不并进去
        assert!(transform(&mut regions, 25, 25, 26).is_empty());
        assert_eq!(regions, vec![0..10, 26..36, 46..46]);
        // 在第二处的结尾打字：也不并进去
        assert!(transform(&mut regions, 36, 36, 37).is_empty());
        assert_eq!(regions, vec![0..10, 26..36, 47..47]);
        // 在第一处里删掉 4 个字：算手动改过，后面跟着往前挪
        assert_eq!(transform(&mut regions, 2, 6, 2), vec![0]);
        assert_eq!(regions, vec![0..6, 22..32, 43..43]);
        // 从第一处中间删到第二处中间：两处都碰到了
        assert_eq!(transform(&mut regions, 3, 25, 4), vec![0, 1]);
        assert_eq!(regions, vec![0..4, 3..11, 22..22]);
    }

    #[test]
    fn 按钮替换一处() {
        let mut regions = vec![0..10, 20..30, 40..40];
        // 第二处换成 2 个字：后面的往前挪 8
        replace_region(&mut regions, 1, 2);
        assert_eq!(regions, vec![0..10, 20..22, 32..32]);
        // 空的那处换成 5 个字
        replace_region(&mut regions, 2, 5);
        assert_eq!(regions, vec![0..10, 20..22, 32..37]);
        // 第一处换成空的
        replace_region(&mut regions, 0, 0);
        assert_eq!(regions, vec![0..0, 10..12, 22..27]);
    }

    #[test]
    fn 认得冲突标记() {
        assert!(has_marker("a\n<<<<<<< HEAD\nb\n"));
        assert!(has_marker("=======\n"));
        assert!(!has_marker("a ======= b\n"));
        assert!(!has_marker("plain\n"));
    }

    #[test]
    fn 带共同祖先的冲突也认得() {
        let text = "a\n<<<<<<< HEAD\nours\n||||||| base\norig\n=======\ntheirs\n>>>>>>> x\nz";
        let file = DiffFile::parse_conflicts("a.txt", text).unwrap();
        let out = assemble(text, file.conflicts(), |_| {
            Some(DiffConflictResolution::Incoming)
        });
        // 最后一行没有换行符，照原样
        assert_eq!(out, "a\ntheirs\nz");
        let out = assemble(text, file.conflicts(), |_| {
            Some(DiffConflictResolution::Both)
        });
        assert_eq!(out, "a\nours\ntheirs\nz");
        let blocks = parse_blocks("a.txt", text).unwrap();
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let base = blocks[0].base.clone().unwrap();
        assert_eq!(lines[base].concat(), "orig\n");
    }

    /// 一个临时仓库，用真的 git。
    struct Repo(PathBuf);

    impl Repo {
        fn new(name: &str) -> Repo {
            let dir = std::env::temp_dir().join(format!("fr-merge-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let repo = Repo(dir);
            repo.ok(&["init", "-q", "-b", "main"]);
            repo.ok(&["config", "user.name", "测试"]);
            repo.ok(&["config", "user.email", "test@example.com"]);
            repo.ok(&["config", "core.autocrlf", "false"]);
            repo
        }

        fn run(&self, args: &[String], stdin: Option<&[u8]>) -> Output {
            let mut child = fr_core::git::command(&self.0, args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut pipe = child.stdin.take().unwrap();
            if let Some(bytes) = stdin {
                let _ = pipe.write_all(bytes);
            }
            drop(pipe);
            child.wait_with_output().unwrap()
        }

        fn ok(&self, args: &[&str]) -> Vec<u8> {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let out = self.run(&args, None);
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "git {args:?}：{err}");
            out.stdout
        }

        fn commit(&self, name: &str, bytes: &[u8], message: &str) {
            std::fs::write(self.0.join(name), bytes).unwrap();
            self.ok(&["add", "--", name]);
            self.ok(&["commit", "-q", "-m", message]);
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 在两个分支上改同两处，合并时冲突：返回仓库，冲突的文件是 `name`。两处隔开四行，
    /// 挨得太近 git 会把它们并成一处。
    fn conflicted(tag: &str, name: &str, eol: &str) -> Repo {
        let repo = Repo::new(tag);
        let text = |a: &str, b: &str| {
            ["head", a, "m1", "m2", "m3", "m4", b, "tail", ""]
                .join(eol)
                .into_bytes()
        };
        repo.commit(name, &text("a", "b"), "base");
        repo.ok(&["checkout", "-q", "-b", "feature"]);
        let theirs = text("a theirs", "b theirs");
        repo.commit(name, &theirs, "theirs");
        repo.ok(&["checkout", "-q", "main"]);
        let ours = text("a ours", "b ours");
        repo.commit(name, &ours, "ours");
        let out = repo.run(&["merge".into(), "feature".into()], None);
        assert!(!out.status.success(), "应该冲突");
        repo
    }

    /// 像合并编辑器那样走一遍：读出来、逐处选、写回去、`git add`。返回写进文件的字节。
    fn resolve(repo: &Repo, name: &str, pick: [DiffConflictResolution; 2]) -> Vec<u8> {
        let out = repo.run(&workfile::read_args(name), None);
        assert!(out.status.success());
        let (hash, bytes) = workfile::split_read(&out.stdout).unwrap();
        let (text, crlf, charset) = decode(bytes.to_vec()).unwrap();
        let file = DiffFile::parse_conflicts(name, &text).unwrap();
        assert_eq!(file.conflicts().len(), 2, "{text}");
        let merged = assemble(&text, file.conflicts(), |ix| Some(pick[ix]));
        assert_eq!(count_conflicts(name, &merged), Some(0));
        let bytes = encode(&merged, crlf, charset);
        let write = repo.run(&workfile::write_args(name, &hash, "changed"), Some(&bytes));
        let err = String::from_utf8_lossy(&write.stderr);
        assert!(write.status.success(), "{err}");
        repo.ok(&["add", "--", name]);
        bytes
    }

    #[test]
    fn 真的冲突_逐处选完写回_标记为已解决后能提交() {
        use DiffConflictResolution::{Both, Current};
        let repo = conflicted("lf", "a.txt", "\n");
        let bytes = resolve(&repo, "a.txt", [Current, Both]);
        assert_eq!(
            bytes,
            b"head\na ours\nm1\nm2\nm3\nm4\nb ours\nb theirs\ntail\n"
        );
        assert_eq!(std::fs::read(repo.path().join("a.txt")).unwrap(), bytes);
        // 没有没解决的文件了，合并能提交，提交有两个父提交
        assert!(
            repo.ok(&["diff", "--name-only", "--diff-filter=U"])
                .is_empty()
        );
        repo.ok(&["commit", "-q", "--no-edit"]);
        let parents = repo.ok(&["rev-list", "--parents", "-n", "1", "HEAD"]);
        let parents = String::from_utf8_lossy(&parents).split_whitespace().count();
        assert_eq!(parents, 3);
    }

    #[test]
    fn 真的冲突_带共同祖先的那份和工作区对得上() {
        let repo = conflicted("base", "a.txt", "\n");
        let worktree = std::fs::read(repo.path().join("a.txt")).unwrap();
        let (text, _, _) = decode(worktree).unwrap();
        let plain = repo.run(
            &workfile::base_args("a.txt", "merge", ["HEAD", "base", "feature"]),
            None,
        );
        let (plain, _, _) = decode(plain.stdout).unwrap();
        // 工作区里的文件冲突以后没动过：和重新合并的一样
        assert_eq!(plain, text);
        let diff3 = repo.run(
            &workfile::base_args("a.txt", "diff3", ["HEAD", "base", "feature"]),
            None,
        );
        let (diff3, _, _) = decode(diff3.stdout).unwrap();
        let blocks = parse_blocks("a.txt", &diff3).unwrap();
        assert_eq!(blocks.len(), 2);
        let lines: Vec<&str> = diff3.split_inclusive('\n').collect();
        let base = blocks[0].base.clone().unwrap();
        assert_eq!(lines[base].concat(), "a\n");
    }

    #[test]
    fn 真的冲突_crlf_的文件写回去还是_crlf() {
        use DiffConflictResolution::{Current, Incoming};
        let repo = conflicted("crlf", "w.txt", "\r\n");
        let bytes = resolve(&repo, "w.txt", [Incoming, Current]);
        assert_eq!(
            bytes,
            b"head\r\na theirs\r\nm1\r\nm2\r\nm3\r\nm4\r\nb ours\r\ntail\r\n"
        );
        assert!(
            repo.ok(&["diff", "--name-only", "--diff-filter=U"])
                .is_empty()
        );
        repo.ok(&["commit", "-q", "--no-edit"]);
        // 提交进去的也是 CRLF，没有多出、少掉换行
        assert_eq!(repo.ok(&["show", "HEAD:w.txt"]), bytes);
    }

    #[test]
    fn gbk_的文件照原编码写回_认不出的不接_crlf_来回不变() {
        // 「你好」的 GBK：解成文字，原样编回去
        let (text, crlf, charset) = decode(b"\xc4\xe3\xba\xc3\n".to_vec()).unwrap();
        assert_eq!(
            (text.as_str(), crlf, charset),
            ("你好\n", false, Charset::Gb18030)
        );
        assert_eq!(encode(&text, crlf, charset), b"\xc4\xe3\xba\xc3\n");
        // UTF-8 和 GB18030 都不是（单独一个前导字节）：不接
        assert_eq!(decode(b"\x81\n".to_vec()), None);
        assert_eq!(
            decode(b"a\r\nb\r\n".to_vec()),
            Some(("a\nb\n".to_string(), true, Charset::Utf8))
        );
        assert_eq!(encode("a\nb\n", true, Charset::Utf8), b"a\r\nb\r\n");
        assert_eq!(encode("a\nb\n", false, Charset::Utf8), b"a\nb\n");
    }
}
