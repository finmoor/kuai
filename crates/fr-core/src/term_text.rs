//! 终端屏幕上的字：认输入区、认出发出去的消息、在往回翻的记录里找到这条消息。
//!
//! 终端的三样东西用它（设计见 `docs/design/terminal-scroll/`）：往回翻时输入区固定在底下（客户端认
//! 输入区）；左边的消息刻度（kuaid 在按回车的那一刻认出消息记下来，客户端到往回翻的记录里找它在哪
//! 一行、跳过去）。kuaid 和客户端用的是同一个终端仿真库，各自把屏幕读成一行行的字交给这里，规则只有
//! 这一份。
//!
//! 都是看屏幕猜的，各家程序画法不一样，认不出来时宁可不认（不固定、不记），不出错乱：
//! - Claude Code（主屏模式）：输入框是「一整行横线、`❯ ` 开头的输入行、一整行横线」，下面一两行
//!   状态。发出去的消息在往回翻的记录里是一行 `❯ 消息`。
//! - Codex 这类：输入行 `› ` 开头，下面是提示行，没有横线。
//! - shell：提示符那一行，`PS 路径> `、`user@host:~$ `、`# `、`❯ `。

use std::ops::Range;

use crate::protocol::{TERM_MARKS, TermMark};

/// 屏幕上的一行字。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Row {
    /// 这一行的字（宽字符只算一次）。行尾的空格去掉，写满折到下一行的留着：那是这一行的字（命令
    /// 正好在空格那里折开），接下一行时少了它，`1..60 | %` 就成了 `1..60| %`
    pub text: String,
    /// 这一行有字且写满了，接着写到下一行
    pub wraps: bool,
    /// 行尾这么多个字是淡色的（见 [`Shade::Faint`]），不是打的字
    pub faint: usize,
}

/// 屏幕上一格字的样子：认消息时要分出哪些不是打的字。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shade {
    Plain,
    /// 淡色字：PowerShell、zsh、fish 在光标后面用灰字给的建议（回车时不算），Claude Code 空输入框里的
    /// 占位提示
    Faint,
    /// 程序自己画的光标（反色的一格，Claude Code 的）：夹在淡色字中间不打断它
    Cursor,
}

impl Shade {
    /// 一格字算什么：反色的是程序自己画的光标；淡色的（SGR 2，PowerShell 的建议、Claude Code 的占位
    /// 提示），或者在光标后面（`after`）的灰色字（zsh、fish 的建议、OpenCode mini 的占位提示）。光标前面的灰色字
    /// 不算：PowerShell 把 `-Format` 这类参数也画成灰色。
    pub fn of(inverse: bool, dim: bool, gray: bool, after: bool) -> Shade {
        match (inverse, dim || (after && gray)) {
            (true, _) => Shade::Cursor,
            (false, true) => Shade::Faint,
            (false, false) => Shade::Plain,
        }
    }
}

impl Row {
    pub fn new(text: impl Into<String>) -> Row {
        Row::from_cells(text.into().chars().map(|c| (c, Shade::Plain)), false)
    }

    /// 一格一格拼成一行，记下行尾连着的淡色字有几个（中间的空格、程序画的光标不打断）。不换行的空格
    /// 当普通空格：Claude Code 输入框里提示符后面是它（`❯` 加 U+00A0），往回翻的记录里是普通空格。
    pub fn from_cells(cells: impl IntoIterator<Item = (char, Shade)>, wraps: bool) -> Row {
        let mut cells: Vec<(char, Shade)> = cells
            .into_iter()
            .map(|(c, shade)| (if c == '\u{a0}' { ' ' } else { c }, shade))
            .collect();
        // ConPTY 会把 OpenCode mini 用空格填满的间隔行也标成折行，输入和历史里的提示符在空行后另起。
        let wraps = wraps && cells.iter().any(|(c, _)| !c.is_whitespace());
        while !wraps && cells.last().is_some_and(|(c, _)| c.is_whitespace()) {
            cells.pop();
        }
        let tail = cells
            .iter()
            .rev()
            .take_while(|(c, shade)| *shade != Shade::Plain || c.is_whitespace())
            .collect::<Vec<_>>();
        let faint = match tail.iter().any(|(_, shade)| *shade == Shade::Faint) {
            true => tail.len(),
            false => 0,
        };
        Row {
            text: cells.iter().map(|(c, _)| c).collect(),
            wraps,
            faint,
        }
    }

    /// 去掉行尾的淡色字。
    fn typed(&self) -> &str {
        let keep = self.text.chars().count().saturating_sub(self.faint);
        let end = self
            .text
            .char_indices()
            .nth(keep)
            .map_or(self.text.len(), |(at, _)| at);
        &self.text[..end]
    }
}

/// 写满折到下一行的一行，最后一格（`last`）是不是放不下宽字留的空位、不是字：下一行开头是宽字
/// （`next_wide`）、这一格是空格。终端自己折的时候会标出这一格（alacritty 的
/// `LEADING_WIDE_CHAR_SPACER`，读的时候已经跳过），经 ConPTY 来的是它写的一个普通空格，和正好打在
/// 最后一格的空格分不出来：都当成空位。中文折行时常有这一格，正好在最后一格打空格、下一行又是中文
/// 开头的少，错了也只少一个空格。
pub fn padded(last: char, next_wide: bool) -> bool {
    next_wide && last == ' '
}

/// 一条消息最多记这么多个字
const MESSAGE_MAX: usize = 2000;
/// 到往回翻的记录里找消息时，拿消息开头的这么多个字去比
const NEEDLE: usize = 24;

/// 输入区是屏幕上的哪几行：从 [`input_start`] 起，到下面最后一行有字的（再往下空着的不算，Claude
/// Code 刚开、改了窗口大小以后屏幕没写满，下面空着好几行）。认不出来，或者比屏幕的三分之一还高（比如
/// 在输入框里写了很长一段）时是 `None`：往回翻时整屏照常滚，和没有这个功能时一样。
pub fn input_area(rows: &[Row], cursor: usize, cursor_shown: bool) -> Option<Range<usize>> {
    let top = input_start(rows, cursor, cursor_shown)?;
    let last = rows
        .iter()
        .rposition(|row| !row.text.trim().is_empty())
        .map_or(top, |last| last.max(top));
    (last + 1 - top <= rows.len() / 3).then_some(top..last + 1)
}

/// 输入区从第几行起，不管多高：
/// - 光标在、停在 PowerShell 多行语句的续行（`>> `）上的：这条语句开头的提示符那一行（见
///   [`statement_start`]）。
/// - 光标在、停在提示符那一行（上面不是横线）的：这一行。shell 里刚退出 Claude Code 时，它最后的
///   输入框还留在提示符上面，不能当成输入框。
/// - 光标往上有一整行横线、下一行是提示符开头的（Claude Code 的输入框），从这条横线起。往上最多找
///   三分之一屏有字的行，空行不算：Claude Code 藏起来的真光标停在下面的状态行上，改了窗口大小以后
///   输入框画在屏幕上面、光标停在最底下。所以不是找最近的那条横线。
/// - 不然是光标所在的那一行（折到下一行的，从这一行的头起）。
fn input_start(rows: &[Row], cursor: usize, cursor_shown: bool) -> Option<usize> {
    if cursor >= rows.len() {
        return None;
    }
    let line = line_start(rows, cursor);
    let in_box = line > 0 && is_rule(&rows[line - 1].text);
    if cursor_shown && !in_box {
        if let Some(start) = statement_start(rows, line) {
            return Some(start);
        }
        if prompt_rest(&logical(rows, line)).is_some() {
            return Some(line);
        }
    }
    let mut seen = 0;
    for at in (0..=cursor).rev() {
        if rows[at].text.trim().is_empty() {
            continue;
        }
        if is_rule(&rows[at].text) && rows.get(at + 1).is_some_and(starts_input) {
            return Some(at);
        }
        seen += 1;
        if seen > rows.len() / 3 {
            break;
        }
    }
    Some(line)
}

/// 这一行是折过来的话，往上找到它开头的那一行。
fn line_start(rows: &[Row], mut at: usize) -> usize {
    while at > 0 && rows[at - 1].wraps {
        at -= 1;
    }
    at
}

/// 从第 `start` 行起、连同写满折下去的几行连起来的字（提示符很长时，提示符自己就可能折开）。
fn logical(rows: &[Row], start: usize) -> String {
    let mut text = String::new();
    for row in &rows[start..] {
        text.push_str(&row.text);
        if !row.wraps {
            break;
        }
    }
    text
}

/// PowerShell 多行语句的续行（PSReadLine 默认的续行提示符 `>> `）后面的字。
fn continuation(text: &str) -> Option<&str> {
    let rest = unbox(text).strip_prefix(">>")?;
    match rest.is_empty() {
        true => Some(""),
        false => rest.strip_prefix(' '),
    }
}

/// PowerShell 多行语句（`if (…) {` 按回车以后一行行的 `>> `）：第 `line` 行是续行时，往上跨过别的
/// 续行，到这条语句开头 `PS …> ` 的那一行。最多往上找三分之一屏，中间夹着别的、找不到提示符那一行
/// 的是 `None`。
fn statement_start(rows: &[Row], line: usize) -> Option<usize> {
    continuation(&rows[line].text)?;
    let mut at = line;
    for _ in 0..rows.len() / 3 {
        if at == 0 {
            return None;
        }
        at = line_start(rows, at - 1);
        let text = logical(rows, at);
        if unbox(&text).trim_start().starts_with("PS ") && prompt_rest(&text).is_some() {
            return Some(at);
        }
        continuation(&text)?;
    }
    None
}

/// 光标停在 PowerShell 多行语句的续行上：按回车时读出来的消息是这条语句到现在的几行，接着上一次
/// 按回车记下的（见 [`record`]）。
pub fn continued(rows: &[Row], cursor: usize, cursor_shown: bool) -> bool {
    cursor_shown && cursor < rows.len() && statement_start(rows, line_start(rows, cursor)).is_some()
}

/// 把按回车时读出来的一条消息记进 `marks`（从早到晚，最多 [`TERM_MARKS`] 条）。光标在 PowerShell
/// 多行语句的续行上（`continued`）、上一条记的就是这条语句前面的几行时换掉它：一条语句只记一条，
/// 每按一次回车多几行，不然一条 `if` 语句能记成三四条。
pub fn record(marks: &mut Vec<TermMark>, text: String, continued: bool, at: u64) {
    let grows = |prev: &str| {
        text.strip_prefix(prev)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('\n'))
    };
    match marks.last_mut() {
        Some(last) if continued && grows(&last.text) => *last = TermMark { text, at },
        _ => {
            marks.push(TermMark { text, at });
            let over = marks.len().saturating_sub(TERM_MARKS);
            marks.drain(..over);
        }
    }
}

/// 按回车的那一刻，屏幕上的输入区里写着的消息（去掉提示符和末尾的淡色字）。空的、认出是在选菜单
/// （Claude Code 问「要不要执行」时的「❯ 1. Yes」、问「信任这个文件夹吗」时的「❯ No, exit」）的
/// 不算。`cursor_shown`：程序没把光标藏起来。
pub fn message(rows: &[Row], cursor: usize, cursor_shown: bool) -> Option<String> {
    let top = input_start(rows, cursor, cursor_shown)?;
    let boxed = is_rule(&rows[top].text);
    let mut lines: Vec<&Row> = match boxed {
        // 输入框：两条横线之间
        true => rows[top + 1..]
            .iter()
            .take_while(|row| !is_rule(&row.text))
            .collect(),
        // 提示符那一行，连同折到下面的
        false => {
            let mut end = cursor;
            while end + 1 < rows.len() && rows[end].wraps {
                end += 1;
            }
            if !cursor_shown && is_pointer_menu(rows, top, end) {
                return None;
            }
            rows[top..=end].iter().collect()
        }
    };
    // 末尾整行都是淡色字的不算，剩下的最后一行去掉行尾的淡色字
    while lines
        .last()
        .is_some_and(|row| row.faint >= row.text.chars().count())
    {
        lines.pop();
    }
    let last = lines.len().checked_sub(1)?;
    let texts: Vec<&str> = lines
        .iter()
        .enumerate()
        .map(|(at, row)| match at == last {
            true => row.typed(),
            false => row.text.as_str(),
        })
        .collect();
    if is_menu(rows, top, &lines[0].text) {
        return None;
    }
    // 认不出提示符、以冒号结尾的一行：程序在问密码、用户名（打的字不显示），不是发出去的消息
    let first = unbox(texts[0]).trim();
    if texts.len() == 1 && prompt_rest(first).is_none() && first.ends_with([':', '：']) {
        return None;
    }
    // 写满折下去的几行连成一行。行尾的空格是字（见 [`Row::text`]）：去掉方框时跟着去掉了，补回去
    let mut logical: Vec<String> = Vec::new();
    let mut joined = false;
    for (row, line) in lines.iter().zip(&texts) {
        let trailing = match row.wraps {
            true => &line[line.trim_end().len()..],
            false => "",
        };
        let line = unbox(line);
        match logical.last_mut() {
            Some(last) if joined => last.push_str(line),
            _ => logical.push(line.to_string()),
        }
        if let Some(last) = logical.last_mut() {
            last.push_str(trailing);
        }
        joined = row.wraps;
    }
    // 去掉开头的提示符（提示符很长、自己折开了也认得）、shell 多行语句的续行提示符；程序换的行去掉
    // 行首的缩进
    let text = logical
        .iter()
        .enumerate()
        .map(|(at, line)| {
            let line = line.as_str();
            let line = match at {
                0 => prompt_rest(line).unwrap_or(line),
                _ if !boxed => continuation(line).unwrap_or(line),
                _ => line,
            };
            line.trim()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.chars().take(MESSAGE_MAX).collect())
}

/// 是不是在选菜单：这一行去掉提示符是「1. 」这样的编号，紧下面一行也是。
fn is_menu(rows: &[Row], top: usize, first: &str) -> bool {
    let numbered = |text: &str| {
        let text = text
            .trim_start()
            .trim_start_matches(['❯', '>', '›'])
            .trim_start();
        let digits = text.chars().take_while(char::is_ascii_digit).count();
        digits > 0 && text[digits..].starts_with(". ")
    };
    if !numbered(unbox(first)) {
        return false;
    }
    rows.iter()
        .skip(top)
        .skip_while(|row| unbox(&row.text) != unbox(first))
        .nth(1)
        .is_some_and(|row| numbered(unbox(&row.text)))
}

/// 是不是在不带编号的菜单里选（Claude Code 问「信任这个文件夹吗」时的「❯ No, exit」）：`top..=end`
/// 这一行是指针 `❯ ` 开头，紧挨着的上一行或者下一行缩进到指针后面的字那里、写着另一个选项。只在光标
/// 藏起来时看：shell 的 `❯ ` 提示符、Codex 的 `› ` 输入行（折下去的行也缩进两格）光标都在。
fn is_pointer_menu(rows: &[Row], top: usize, end: usize) -> bool {
    let text = &rows[top].text;
    let indent = text.len() - text.trim_start_matches(' ').len();
    let rest = &text[indent..];
    let pointer = rest.chars().next().filter(|c| matches!(c, '❯' | '›' | '>'));
    if !pointer.is_some_and(|pointer| rest[pointer.len_utf8()..].starts_with(' ')) {
        return false;
    }
    let option = |row: &Row| {
        let spaces = row.text.len() - row.text.trim_start_matches(' ').len();
        spaces == indent + 2 && row.text.len() > spaces
    };
    (top > 0 && option(&rows[top - 1])) || rows.get(end + 1).is_some_and(option)
}

/// 去掉输入框的边（老版 Claude Code 的 `│ > 字 │`、OpenCode mini 的 `┃ 字`）。
fn unbox(text: &str) -> &str {
    let text = text.trim_end();
    let text = text.strip_prefix(['│', '┃']).unwrap_or(text);
    text.strip_suffix('│').unwrap_or(text).trim_end()
}

/// 一整行横线（可以带方框的角），至少 8 个字。
fn is_rule(text: &str) -> bool {
    let text = text.trim();
    let mut count = 0;
    for c in text.chars() {
        match c {
            '─' | '━' | '═' | '╌' | '┄' => count += 1,
            '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘' | '├' | '┤' => {}
            _ => return false,
        }
    }
    count >= 8
}

/// 这一行是不是提示符开头的输入行（Claude Code、Codex 的输入框里那一行）。
fn starts_input(row: &Row) -> bool {
    let text = unbox(&row.text).trim_start();
    matches!(text.chars().next(), Some('❯' | '>' | '›'))
        && text[text.chars().next().map_or(0, char::len_utf8)..]
            .chars()
            .next()
            .is_none_or(|c| c == ' ')
}

/// 去掉行首的提示符，给后面的字；认不出提示符时是 `None`。
pub fn prompt_rest(text: &str) -> Option<&str> {
    let text = unbox(text);
    let trimmed = text.trim_start();
    // Claude Code、Codex、Python、starship 这类；`>> ` 是 PowerShell 多行语句的续行
    for prefix in ["❯ ", "› ", "> ", ">>> ", "... ", ">> "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    if matches!(trimmed, "❯" | "›" | ">" | ">>") {
        return Some("");
    }
    // PowerShell：`PS C:\path> `（后面没打字时行尾的空格去掉了）
    if let Some(rest) = trimmed.strip_prefix("PS ") {
        if let Some(at) = rest.find("> ") {
            return Some(&rest[at + 2..]);
        }
        if rest.ends_with('>') {
            return Some("");
        }
    }
    // sh、bash、zsh：`user@host:~/dir$ `、`# `、`% `，提示符最多 80 个字
    let head: String = trimmed.chars().take(80).collect();
    for mark in ["$ ", "# ", "% "] {
        if let Some(at) = head.find(mark)
            && (at == 0 || !head[..at].ends_with(' '))
        {
            return Some(&trimmed[at + mark.len()..]);
        }
    }
    None
}

/// 提示符里写着的当前目录：这一行（折开的连起来）是带目录的 shell 提示符时，返回提示符里写的目录，
/// 原样返回（`C:\…`、`/…`、`~/…`），不是提示符、提示符里没写目录的是 `None`。认这几种默认的写法：
/// - PowerShell：`PS C:\path> `（`Microsoft.PowerShell.Core\FileSystem::\\主机\共享` 这种去掉前缀；
///   `HKLM:\` 这类不是文件系统的不算）。
/// - bash、sh（Debian、Ubuntu 默认）：`user@host:/path$ `、`root@host:~/dir# `，前面可以有 `(环境名) `。
/// - fish：`user@host ~/dir> `。
///
/// 终端里点相对路径时，往上找最近的一行，按它写的目录算（命令打出来的路径是相对它当时所在的目录的）。
pub fn prompt_dir(text: &str) -> Option<String> {
    let text = unbox(text).trim_start();
    if let Some(rest) = text.strip_prefix("PS ") {
        let dir = match rest.find("> ") {
            Some(at) => &rest[..at],
            None => rest.strip_suffix('>')?,
        };
        let dir = dir
            .strip_prefix("Microsoft.PowerShell.Core\\FileSystem::")
            .unwrap_or(dir);
        let b = dir.as_bytes();
        let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
        return (drive || dir.starts_with("\\\\") || dir.starts_with('/')).then(|| dir.to_string());
    }
    // `(base) `、`(chroot) ` 这类前缀
    let text = match text.strip_prefix('(') {
        Some(rest) => rest.split_once(") ")?.1,
        None => text,
    };
    let name = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    let (user, rest) = text.split_once('@')?;
    if !name(user) {
        return None;
    }
    let host_end = rest.find([':', ' '])?;
    let (host, rest) = rest.split_at(host_end);
    if !name(host) {
        return None;
    }
    let (rest, ends) = match rest.split_at(1) {
        (":", rest) => (rest, ["$ ", "# "]),
        (" ", rest) => (rest, ["> ", "> "]),
        _ => return None,
    };
    if !rest.starts_with(['/', '~']) {
        return None;
    }
    let at = ends.iter().filter_map(|end| rest.find(end)).min();
    let dir = match at {
        Some(at) => &rest[..at],
        None => rest
            .strip_suffix(ends[0].trim_end())
            .or_else(|| rest.strip_suffix(ends[1].trim_end()))?,
    };
    Some(dir.to_string())
}

/// 空白连成一个空格、去掉两头的空白：比较消息和屏幕上的字时用（屏幕上换行、多出来的空格不算）。
pub fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 一串发出去的消息（从早到晚）各在往回翻的记录（`rows`，从上到下）里的哪一行。
///
/// 从最后一条往前找，每条只在后一条的上面找：先找提示符后面就是这条消息的行（Claude Code 记录里的
/// `❯ 消息`、shell 的 `PS …> 命令`），找不到再找含有它的行（回复里提到同样几个字的不会抢在前面）。
/// 拿消息第一行开头的 [`NEEDLE`] 个字去比：长的消息在屏幕上折成了几行。写满折到下一行的几行连起来
/// 当一行比：提示符很长时命令从哪里折开都有可能，第一行可能只剩一两个字。找不到的是 `None`（往回翻
/// 的记录只留那么多行、程序清过屏，早的已经没了；最新的一条还排着队、没显示出来），跳过它接着往上找；
/// 有一条找不到以后，更早的只认提示符后面的：早的记录丢了，回复里碰巧有同样几个字的行不能冒充。
/// 给的是消息开头那一行。
pub fn locate_all(rows: &[Row], messages: &[&str]) -> Vec<Option<usize>> {
    // 连起来的每一行从第几行开始、连起来的字
    let mut starts: Vec<usize> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for (at, row) in rows.iter().enumerate() {
        match lines.last_mut() {
            Some(line) if rows[at - 1].wraps => line.push_str(&row.text),
            _ => {
                starts.push(at);
                lines.push(row.text.clone());
            }
        }
    }
    let norms: Vec<String> = lines.iter().map(|line| normalize(line)).collect();
    let prompts: Vec<Option<String>> = lines
        .iter()
        .map(|line| prompt_rest(line).map(normalize))
        .collect();
    let mut found = vec![None; messages.len()];
    let mut end = lines.len();
    let mut missed = false;
    for (at, message) in messages.iter().enumerate().rev() {
        let first = message.lines().next().unwrap_or_default();
        let needle: String = normalize(first).chars().take(NEEDLE).collect();
        if needle.is_empty() {
            continue;
        }
        let after_prompt = (0..end).rev().find(|&line| {
            prompts[line]
                .as_ref()
                .is_some_and(|rest| rest.starts_with(&needle))
        });
        let hit = match missed {
            true => after_prompt,
            false => {
                after_prompt.or_else(|| (0..end).rev().find(|&line| norms[line].contains(&needle)))
            }
        };
        match hit {
            Some(line) => {
                found[at] = Some(starts[line]);
                end = line;
            }
            None => missed = true,
        }
    }
    found
}

/// 消息后面的回复开头（悬停时的卡片上写）：消息那一行（连同折下去的）之后、`end` 之前，前 `count` 行
/// 有字的，去掉两头的空白。
pub fn reply_after(rows: &[Row], at: usize, end: usize, count: usize) -> Vec<String> {
    let mut start = at;
    while start < rows.len() && rows[start].wraps {
        start += 1;
    }
    rows.iter()
        .take(end.min(rows.len()))
        .skip(start + 1)
        .map(|row| row.text.trim())
        .filter(|text| !text.is_empty() && !is_rule(text))
        .take(count)
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lines: &[&str]) -> Vec<Row> {
        lines.iter().map(|line| Row::new(*line)).collect()
    }

    const RULE: &str = "────────────────────────────────────────────────";

    /// Claude Code 2.1.330 的屏幕最下面几行（从 kuaid 还原的屏幕里读的）。
    fn claude(prompt: &str) -> Vec<Row> {
        let mut screen = rows(&[
            "● 上一条回复的最后一行",
            "",
            "✻ Churned for 21m 10s · done 11:48 AM",
            "",
        ]);
        // 补满到 18 行，输入框连状态行一共 5 行，不超过三分之一
        while screen.len() < 13 {
            screen.insert(0, Row::new("  前面的输出"));
        }
        screen.extend(rows(&[
            RULE,
            prompt,
            RULE,
            "  ⏵⏵ auto mode on (shift+tab to cycle) · esc to interrupt    ✔ Update installed",
            "                                                            /rc failed",
        ]));
        screen
    }

    #[test]
    fn claude_code_输入框里提示符后面是不换行空格_下面列着命令() {
        // 测试终端里 /status 原来记成了「❯\u{a0}/status」，/exit 没记上
        let screen = claude("❯\u{a0}/status");
        assert_eq!(input_area(&screen, 17, false), Some(13..18));
        assert_eq!(message(&screen, 17, false).as_deref(), Some("/status"));
        // 打着斜杠命令时输入框下面列着命令，藏着的光标停在选中的那一项上
        let mut screen = claude("❯\u{a0}/exit");
        screen.truncate(16);
        screen.extend(rows(&[
            "  ⚠ Transcript saving is off",
            "  ❯ /exit                               Exit the CLI",
            "    /context                            Show what fills the context",
            "    /usage-credits                      Configure usage credits",
        ]));
        assert_eq!(message(&screen, 17, false).as_deref(), Some("/exit"));
    }

    #[test]
    fn claude_code_的输入框_从上面那条横线起() {
        let screen = claude("❯ 帮我看看");
        // 闲着时真光标在输入行上；忙着时停在状态行上：都从上面那条横线起
        assert_eq!(input_area(&screen, 14, false), Some(13..18));
        assert_eq!(input_area(&screen, 16, false), Some(13..18));
        assert_eq!(message(&screen, 14, true).as_deref(), Some("帮我看看"));
        assert_eq!(message(&screen, 16, true).as_deref(), Some("帮我看看"));
        // 空的输入框按回车不算
        assert_eq!(message(&claude("❯"), 14, true), None);
        assert_eq!(message(&claude("❯ "), 14, true), None);
    }

    #[test]
    fn 多行的消息_折过来的接上_换行的分开() {
        let mut screen = claude("❯ 第一行很长很长");
        screen[14].wraps = true;
        screen.insert(15, Row::new("接着写"));
        screen.insert(16, Row::new("  第二行"));
        screen.drain(..2);
        assert_eq!(
            message(&screen, 13, true).as_deref(),
            Some("第一行很长很长接着写\n第二行")
        );
    }

    #[test]
    fn 老版的方框输入框() {
        let mut screen = rows(&["输出"; 12]);
        screen.extend(rows(&[
            "╭──────────────────────────╮",
            "│ > fix the build          │",
            "╰──────────────────────────╯",
            "  ? for shortcuts",
        ]));
        assert_eq!(input_area(&screen, 13, true), Some(12..16));
        assert_eq!(message(&screen, 13, true).as_deref(), Some("fix the build"));
    }

    #[test]
    fn shell_的提示符那一行() {
        let mut screen = rows(&["输出"; 11]);
        screen.push(Row::new(r"PS C:\Users\me\repo> git status"));
        assert_eq!(input_area(&screen, 11, true), Some(11..12));
        assert_eq!(message(&screen, 11, true).as_deref(), Some("git status"));
        screen[11] = Row::new("me@box:~/code$ cargo build --release");
        assert_eq!(
            message(&screen, 11, true).as_deref(),
            Some("cargo build --release")
        );
        screen[11] = Row::new("root@box:/# ls");
        assert_eq!(message(&screen, 11, true).as_deref(), Some("ls"));
        // 认不出提示符：整行
        screen[11] = Row::new("mysql> select 1;");
        assert_eq!(
            message(&screen, 11, true).as_deref(),
            Some("mysql> select 1;")
        );
        // 问密码、用户名的（打的字不显示）
        screen[11] = Row::new("Enter passphrase for key '/home/me/.ssh/id_ed25519':");
        assert_eq!(message(&screen, 11, true), None);
        screen[11] = Row::new("Username for 'https://github.com':");
        assert_eq!(message(&screen, 11, true), None);
        // 空着按回车
        screen[11] = Row::new(r"PS C:\Users\me\repo>");
        assert_eq!(message(&screen, 11, true), None);
    }

    #[test]
    fn opencode_mini_输入与横幅和状态行分开() {
        let mut screen = rows(&[""; 24]);
        screen[1] = wrapped("▪ oc mini v2.3.2 · repo   ");
        screen[2] = wrapped("                          ");
        screen[3] = wrapped("┃ Render probe references ");
        screen[4] = wrapped("                          ");
        screen[5] = Row::new("Build · Fixture · ctrl+p menu");

        assert_eq!(input_area(&screen, 3, true), Some(3..6));
        let mark = message(&screen, 3, true).unwrap();
        assert_eq!(mark, "Render probe references");

        screen[3] = wrapped("› Render probe references ");
        assert_eq!(locate_all(&screen, &[&mark]), vec![Some(3)]);
    }

    #[test]
    fn 命令折成两行() {
        let mut screen = rows(&["输出"; 10]);
        screen.push(Row {
            text: r"PS C:\a\very\long\path> echo aaaa".into(),
            wraps: true,
            ..Row::default()
        });
        screen.push(Row::new("bbbb"));
        // 光标在折下来的那一行
        assert_eq!(input_area(&screen, 11, true), Some(10..12));
        assert_eq!(message(&screen, 11, true).as_deref(), Some("echo aaaabbbb"));
    }

    /// 一格一格都是打的字的一行，写满了折到下一行（行尾的空格留着）。
    fn wrapped(text: &str) -> Row {
        Row::from_cells(text.chars().map(|c| (c, Shade::Plain)), true)
    }

    #[test]
    fn 命令正好在空格那里折开_空格不丢() {
        // 测试终端里：提示符很长，`1..60 ` 正好写满第一行，原来记成了 `1..60| % …`
        let mut screen = rows(&["输出"; 10]);
        screen.push(wrapped(r"PS C:\Users\me\code\demo> 1..60 "));
        screen.push(Row::new(r#"| % { "after $_" }"#));
        assert_eq!(input_area(&screen, 11, true), Some(10..12));
        assert_eq!(
            message(&screen, 11, true).as_deref(),
            Some(r#"1..60 | % { "after $_" }"#)
        );
        // 程序换的行照样去掉行尾的空格
        assert_eq!(Row::new("abc  ").text, "abc");
    }

    #[test]
    fn powershell_多行语句记成一条() {
        let mut screen = rows(&["输出"; 9]);
        screen.push(Row::new(r"PS C:\repo> if ($true) {"));
        screen.push(Row::new(">> echo 2"));
        screen.push(Row::new(">> }"));
        // 光标在续行上：输入区从提示符那一行起，消息是整条语句、不带续行提示符
        assert_eq!(input_area(&screen, 11, true), Some(9..12));
        assert_eq!(
            message(&screen, 11, true).as_deref(),
            Some("if ($true) {\necho 2\n}")
        );
        assert!(continued(&screen, 11, true));
        assert!(!continued(&screen, 9, true), "提示符那一行不是续行");
        // 每按一次回车记一次：同一条语句换掉上一条
        let mut marks = Vec::new();
        record(&mut marks, "if ($true) {".into(), false, 1);
        record(&mut marks, "if ($true) {\necho 2".into(), true, 2);
        record(&mut marks, "if ($true) {\necho 2\n}".into(), true, 3);
        assert_eq!(marks.len(), 1);
        assert_eq!(
            (marks[0].text.as_str(), marks[0].at),
            ("if ($true) {\necho 2\n}", 3)
        );
        // 空着的续行上按回车（结束语句）：还是那一条
        record(&mut marks, "if ($true) {\necho 2\n}".into(), true, 4);
        assert_eq!(marks.len(), 1);
        // 不在续行上：一样的命令敲两次是两条
        record(&mut marks, "git status".into(), false, 5);
        record(&mut marks, "git status".into(), false, 6);
        assert_eq!(marks.len(), 3);
        // 在续行上、但上一条是别的（一次写好几行才按回车）：另记一条
        record(&mut marks, "echo 1\necho 2".into(), true, 7);
        assert_eq!(marks.len(), 4);
        // 提示符改过、认不出语句开头：只算光标这一行，去掉续行提示符
        let mut screen = rows(&["输出"; 10]);
        screen.push(Row::new("❯ if ($true) {"));
        screen.push(Row::new(">> echo 2"));
        assert_eq!(message(&screen, 11, true).as_deref(), Some("echo 2"));
        assert!(!continued(&screen, 11, true));
    }

    #[test]
    fn 提示符自己折开了也认得() {
        let mut screen = rows(&["输出"; 10]);
        screen.push(wrapped(r"PS C:\Users\me\a\very\long\pa"));
        screen.push(Row::new(r"th\to\repo> git status"));
        assert_eq!(input_area(&screen, 11, true), Some(10..12));
        assert_eq!(message(&screen, 11, true).as_deref(), Some("git status"));
    }

    #[test]
    fn 找消息时折开的几行连起来比() {
        // 第一行提示符后面只剩 5 个字：原来只看第一行认不出来，它和更早的刻度都跳不过去
        let history = vec![
            Row::new(r"PS C:\repo> echo alpha"),
            Row::new("alpha"),
            wrapped(r"PS C:\Users\me\code\demo> 1..60 "),
            Row::new(r#"| % { "after $_" }"#),
            Row::new("after 1"),
        ];
        assert_eq!(
            locate_all(&history, &["echo alpha", r#"1..60 | % { "after $_" }"#]),
            [Some(0), Some(2)]
        );
        // 提示符正好写满一行、命令整个在下一行的也认得
        let history = vec![
            wrapped(r"PS C:\Users\me\code\demo> "),
            Row::new("git status"),
        ];
        assert_eq!(locate_all(&history, &["git status"]), [Some(0)]);
    }

    #[test]
    fn 输入区下面空着的行不算_太高的不固定() {
        // 屏幕没满，提示符在第 1 行，下面全空着：只算提示符那一行
        let mut screen = rows(&["PS C:\\>"]);
        screen.extend(rows(&[""; 11]));
        assert_eq!(input_area(&screen, 0, true), Some(0..1));
        // Claude Code 刚开：输入框连状态行下面还空着几行
        let mut screen = claude("❯");
        screen.drain(..3);
        screen.extend(rows(&["", "", ""]));
        assert_eq!(input_area(&screen, 15, false), Some(10..15));
        // 改了窗口大小：输入框画在屏幕上面，藏起来的真光标停在最底下（49 行的屏幕上
        // 输入框在第 2～6 行、光标在第 48 行）。中间的空行不算进往上找的行数
        let mut screen = rows(&["", ""]);
        screen.extend(claude("❯").drain(13..));
        screen.extend(rows(&[""; 42]));
        assert_eq!(screen.len(), 49);
        assert_eq!(input_area(&screen, 48, false), Some(2..7));
        screen[3] = Row::new("❯ 改完了吗");
        assert_eq!(message(&screen, 48, false).as_deref(), Some("改完了吗"));
        // 输入框里写了很长一段，比三分之一高
        let mut screen = claude("❯ 第一行");
        for _ in 0..6 {
            screen.insert(15, Row::new("  又一行"));
        }
        assert_eq!(input_area(&screen, 14, false), None);
        assert_eq!(
            message(&screen, 14, false).as_deref(),
            Some("第一行\n又一行\n又一行\n又一行\n又一行\n又一行\n又一行")
        );
    }

    #[test]
    fn 退出_claude_code_以后_提示符上面留着的输入框不算() {
        let mut screen = claude("❯ 最后一条");
        screen.drain(..4);
        screen.push(Row::new(r"PS C:\repo> git log"));
        let prompt = screen.len() - 1;
        assert_eq!(input_area(&screen, prompt, true), Some(prompt..prompt + 1));
        assert_eq!(message(&screen, prompt, true).as_deref(), Some("git log"));
    }

    /// 按格子拼一行：`plain` 是打的字，`faint` 是跟在后面的淡色字。
    fn shaded(plain: &str, faint: &str) -> Row {
        let cells = plain
            .chars()
            .map(|c| (c, Shade::Plain))
            .chain(faint.chars().map(|c| (c, Shade::Faint)))
            .chain("   ".chars().map(|c| (c, Shade::Plain)));
        Row::from_cells(cells, false)
    }

    #[test]
    fn 光标后面的灰字建议_占位提示不算() {
        // PowerShell 的灰字建议（测试终端里只打了 claude，原来记成了 claude update）
        let mut screen = rows(&["输出"; 11]);
        screen.push(shaded(r"PS C:\repo> claude", " update"));
        assert_eq!(screen[11].faint, 7);
        assert_eq!(message(&screen, 11, true).as_deref(), Some("claude"));
        // 建议长到折到下一行：下一行整行都是灰字
        screen[11] = shaded(r"PS C:\repo> git", " log --oneline --graph");
        screen[11].wraps = true;
        screen.push(shaded("", "--all"));
        assert_eq!(message(&screen, 11, true).as_deref(), Some("git"));
        // Claude Code 空输入框里的占位提示，第一格是它画的反色光标
        let mut placeholder = Row::from_cells(
            [
                ('❯', Shade::Plain),
                (' ', Shade::Plain),
                ('T', Shade::Cursor),
            ]
            .into_iter()
            .chain("ry \"fix lint errors\"".chars().map(|c| (c, Shade::Faint))),
            false,
        );
        let mut screen = claude("");
        assert_eq!(placeholder.faint, 22);
        std::mem::swap(&mut screen[14], &mut placeholder);
        assert_eq!(message(&screen, 16, false), None);
        // 淡色字夹在中间的照样算；只有反色光标、没有淡色字的不去掉
        let row = Row::from_cells(
            "❯ a"
                .chars()
                .map(|c| (c, Shade::Faint))
                .chain([('b', Shade::Plain), ('c', Shade::Cursor)]),
            false,
        );
        assert_eq!(row.faint, 0);
    }

    /// Claude Code 2.1.330 问信任文件夹的样子（指针挪到了第二项上）。
    fn trust(pointer_on_yes: bool) -> Vec<Row> {
        let (no, yes) = match pointer_on_yes {
            true => ("   No, exit", " ❯ Yes, I trust this folder"),
            false => (" ❯ No, exit", "   Yes, I trust this folder"),
        };
        let mut screen = rows(&["输出"; 4]);
        screen.extend(rows(&[
            RULE,
            " Accessing workspace:",
            "",
            r" C:\repos\demo",
            "",
            " Quick safety check: Is this a project you created or one you trust?",
            "",
            " Security guide",
            "",
            no,
            yes,
            "",
            " Enter to confirm · Esc to cancel",
        ]));
        screen
    }

    #[test]
    fn 不带编号的菜单按的回车不算() {
        assert_eq!(message(&trust(true), 14, false), None);
        assert_eq!(message(&trust(false), 13, false), None);
        // 光标在的（shell 的 ❯ 提示符，上面一行碰巧缩进两格）照样算
        let screen = rows(&["  上一条命令的输出", "❯ git status"]);
        assert_eq!(message(&screen, 1, true).as_deref(), Some("git status"));
        assert_eq!(message(&screen, 1, false), None);
    }

    #[test]
    fn 选菜单时按的回车不算() {
        let mut screen = rows(&["输出"; 8]);
        screen.extend(rows(&[
            RULE,
            " Bash command",
            "   git push",
            " Do you want to proceed?",
            " ❯ 1. Yes",
            "   2. Yes, and don't ask again for git push commands",
            "   3. No, and tell Claude what to do differently (esc)",
        ]));
        assert_eq!(message(&screen, 12, true), None);
        // 只是以数字开头的消息照样算
        assert_eq!(
            message(&claude("❯ 1. 先改这个"), 14, true).as_deref(),
            Some("1. 先改这个")
        );
    }

    #[test]
    fn 提示符() {
        assert_eq!(prompt_rest("❯ hi"), Some("hi"));
        assert_eq!(prompt_rest("› hi"), Some("hi"));
        assert_eq!(prompt_rest(">>> 1+1"), Some("1+1"));
        assert_eq!(prompt_rest(r"PS C:\x y> ls"), Some("ls"));
        assert_eq!(prompt_rest("a@b:~/x y$ make"), Some("make"));
        // PowerShell 多行语句的续行
        assert_eq!(prompt_rest(">> echo 2"), Some("echo 2"));
        assert_eq!(prompt_rest(">>"), Some(""));
        assert_eq!(prompt_rest("普通的一行字"), None);
        // 「$ 」前面是空格的不是提示符（说明里的价格之类）
        assert_eq!(prompt_rest("costs about $ 5"), None);
    }

    #[test]
    fn 在记录里找消息_先找提示符后面的() {
        let history = rows(&[
            "❯ 发版",
            "● 好的，发版前先看清单。",
            "  发版的步骤：",
            "❯ 发版",
            "● 已发版。",
            r"PS C:\repo> git status",
        ]);
        // 回复里提到「发版」的行不算；两条一样的按先后对上
        assert_eq!(locate_all(&history, &["发版", "发版"]), [Some(0), Some(3)]);
        assert_eq!(
            locate_all(&history, &["发版", "git status"]),
            [Some(3), Some(5)]
        );
        // 只有正文里有的：退回找含有它的行
        assert_eq!(locate_all(&history, &["发版的步骤"]), [Some(2)]);
        // 多行的消息按第一行找；空白不算
        assert_eq!(locate_all(&history, &["git   status\n第二行"]), [Some(5)]);
        // 找不到的一条跳过，更早的接着找（最新的一条还排着队、没显示出来时，前面的照样找得到）
        assert_eq!(
            locate_all(&history, &["发版", "没有的", "git status"]),
            [Some(3), None, Some(5)]
        );
        assert_eq!(
            locate_all(&history, &["发版", "git status", "还在排队的"]),
            [Some(3), Some(5), None]
        );
        // 有一条找不到以后，更早的只认提示符后面的：只在回复里有的不算
        assert_eq!(
            locate_all(&history, &["发版的步骤", "git status", "还在排队的"]),
            [None, Some(5), None]
        );
        // 空的消息（只按了回车）跳过
        assert_eq!(
            locate_all(&history, &["发版", "", "git status"]),
            [Some(3), None, Some(5)]
        );
    }

    #[test]
    fn 长消息的开头折到了下一行() {
        let history = vec![
            Row {
                text: r"PS C:\Users\me\Documents\workspace\repo> echo 一二".into(),
                wraps: true,
                ..Row::default()
            },
            Row::new("三四五六七八九十一二三四五六七八九十"),
        ];
        assert_eq!(
            locate_all(&history, &["echo 一二三四五六七八九十一二三四五六七八九十"]),
            [Some(0)]
        );
    }

    #[test]
    fn 回复的开头() {
        let history = rows(&[
            "❯ 问一句",
            "",
            "● 第一行回复",
            RULE,
            "  第二行",
            "  第三行",
            "❯ 下一句",
        ]);
        assert_eq!(reply_after(&history, 0, 6, 2), ["● 第一行回复", "第二行"]);
        assert_eq!(reply_after(&history, 5, 6, 2), Vec::<String>::new());
    }

    #[test]
    fn 提示符里写着的目录() {
        use super::prompt_dir;
        let cases = [
            ("PS C:\\Users\\me\\repo> ", Some("C:\\Users\\me\\repo")),
            ("PS C:\\Users\\me\\repo>", Some("C:\\Users\\me\\repo")),
            ("PS C:\\> Get-Content a.txt", Some("C:\\")),
            (
                "PS Microsoft.PowerShell.Core\\FileSystem::\\\\nas\\share\\x> ls",
                Some("\\\\nas\\share\\x"),
            ),
            ("PS /home/me/repo> ls", Some("/home/me/repo")),
            ("PS HKLM:\\Software> ", None),
            (">> echo 2", None),
            ("root@dev:/data/repo# ls -la", Some("/data/repo")),
            ("me@box:~/code/x$ ", Some("~/code/x")),
            ("me@box:~$", Some("~")),
            ("(base) me@box:/opt/app$ python x.py", Some("/opt/app")),
            ("me@box ~/code> ", Some("~/code")),
            ("me@box:~/a dir$ ls", Some("~/a dir")),
            ("❯ ls", None),
            ("> fix the bug", None),
            ("mail me@example.com: /etc/hosts is fine", None),
            ("src/main.rs:3:5", None),
        ];
        for (line, expected) in cases {
            assert_eq!(prompt_dir(line).as_deref(), expected, "{line}");
        }
    }
}
