//! 结束、重启终端前要不要先问一句（终端页、仓库的终端页共用）。
//!
//! 还在输出的（运行中）直接问。没在输出的也可能有程序在跑：比如 Claude Code 的后台 agent 还在
//! 跑、屏幕不动，结束终端会把它一起关掉。所以没在输出的再问一遍那台电脑的 kuaid，里面除了 shell
//! 还有什么程序在跑（`term_programs`，协议 13），有就问，确认框里写出程序的名字。

use std::sync::Arc;

use fr_channel::Connection;
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::*;

use crate::term_board::{Row, TermState};

/// 确认框里最多写几个程序的名字
const MAX_NAMES: usize = 3;

/// 要结束、重启的终端。
pub struct Ending {
    /// 实时列表里的那一行：开在 Kuai 进程里的、列表里还没有的是 None
    pub row: Option<Row>,
    /// kuaid 给它的编号，列表里还没有它时用来问 kuaid（仓库的终端页上已经接上、或者等着接的）
    pub term: Option<String>,
    /// 画面那边知道里面的程序已经结束了，或者还什么都没开：不用问
    pub exited: bool,
    /// 连着它所在那台电脑的连接：没连着是 None
    pub conn: Option<Arc<Connection>>,
    /// 仓库名、终端名，确认框里用
    pub repo: SharedString,
    pub name: SharedString,
    pub restart: bool,
}

/// 里面的程序已经结束的直接做；还在输出的先问；别的问过 kuaid，除了 shell 还有程序在跑的先问，没有
/// 的直接做。问不了 kuaid（开在 Kuai 进程里的、还不知道编号的、没连着那台电脑的）或者问出错时，不知道
/// 里面有没有程序在跑，也先问：实时列表还没推过来时，仓库终端页上已经在跑的终端原来不问就结束了。
/// `view` 是调用的那个视图（直接做时用）。
pub fn ask_first<V: 'static>(
    view: &mut V,
    ending: Ending,
    window: &mut Window,
    cx: &mut Context<V>,
    run: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) {
    let Ending {
        row,
        term,
        exited,
        conn,
        repo,
        name,
        restart,
    } = ending;
    if exited || row.as_ref().is_some_and(|row| !row.alive) {
        return run(view, window, cx);
    }
    if row
        .as_ref()
        .is_some_and(|row| row.state == TermState::Running)
    {
        return confirm(&repo, &name, restart, None, window, cx, run);
    }
    let term = row.map(|row| row.id.term).or(term);
    let (Some(conn), Some(term)) = (conn, term) else {
        return confirm(&repo, &name, restart, None, window, cx, run);
    };
    let query = conn.term_programs(&term);
    cx.spawn_in(window, async move |this, cx| {
        let programs = query.await;
        this.update_in(cx, |view, window, cx| match programs {
            Ok(programs) if programs.is_empty() => run(view, window, cx),
            Ok(programs) => confirm(&repo, &name, restart, Some(&programs), window, cx, run),
            Err(err) => {
                log::warn!("无法查询终端里运行的程序：{err:#}");
                confirm(&repo, &name, restart, None, window, cx, run);
            }
        })
        .ok();
    })
    .detach();
}

/// 弹确认框。`programs` 是里面在跑的程序的名字，不知道是 None。
fn confirm<V: 'static>(
    repo: &str,
    name: &str,
    restart: bool,
    programs: Option<&[String]>,
    window: &mut Window,
    cx: &mut Context<V>,
    run: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) {
    let (title, text, ok) = texts(repo, name, restart, programs.and_then(names));
    let this = cx.entity().downgrade();
    crate::manager::confirm_in(
        title.to_string(),
        text,
        ok,
        true,
        window,
        cx,
        move |window, cx| {
            this.update(cx, |this, cx| run(this, window, cx)).ok();
        },
    );
}

/// 确认框的标题、说明、按钮。`programs` 是写进说明里的程序名，不知道是 None。
fn texts(
    repo: &str,
    name: &str,
    restart: bool,
    programs: Option<String>,
) -> (&'static str, String, &'static str) {
    match (restart, programs) {
        (true, None) => (
            tr("重启终端？"),
            tf!(
                "将终止 {repo} 中的终端「{name}」里正在运行的程序，并重新启动。此操作无法撤销。",
                repo,
                name
            ),
            tr("重启"),
        ),
        (true, Some(programs)) => (
            tr("重启终端？"),
            tf!(
                "将终止 {repo} 中的终端「{name}」里正在运行的 {programs}，并重新启动。此操作无法撤销。",
                repo,
                name,
                programs
            ),
            tr("重启"),
        ),
        (false, None) => (
            tr("结束终端？"),
            tf!(
                "将结束 {repo} 中的终端「{name}」，其中正在运行的程序将终止。此操作无法撤销。",
                repo,
                name
            ),
            tr("结束"),
        ),
        (false, Some(programs)) => (
            tr("结束终端？"),
            tf!(
                "将结束 {repo} 中的终端「{name}」，其中正在运行的 {programs} 将终止。此操作无法撤销。",
                repo,
                name,
                programs
            ),
            tr("结束"),
        ),
    }
}

/// 写进说明里的程序名：最多 [`MAX_NAMES`] 个，多了加「等」；一个名字都不知道是 None。
fn names(programs: &[String]) -> Option<String> {
    let known: Vec<&str> = programs
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect();
    match known.len() {
        0 => None,
        n if n <= MAX_NAMES => Some(known.join(tr("、"))),
        _ => Some(tf!("{names} 等", names = known[..MAX_NAMES].join(tr("、")))),
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：gpui 的 `test` 宏会盖住标准库的
    use super::{names, texts, tr};

    fn list(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn 程序名_空的不写_多了加等() {
        assert_eq!(names(&list(&[])), None);
        assert_eq!(names(&list(&["", " "])), None);
        assert_eq!(names(&list(&["claude"])).as_deref(), Some("claude"));
        assert_eq!(
            names(&list(&["claude", "", "node"])).as_deref(),
            Some(format!("claude{}node", tr("、")).as_str())
        );
        let many = names(&list(&["a", "b", "c", "d"])).unwrap();
        assert!(many.starts_with('a') && !many.contains('d'), "{many}");
    }

    #[test]
    fn 知道程序名时写进说明() {
        let (_, text, ok) = texts("web", "终端 1", false, Some("claude".into()));
        assert!(text.contains("claude") && text.contains("web"), "{text}");
        assert_eq!(ok, tr("结束"));
        let (_, text, ok) = texts("web", "终端 1", true, None);
        assert!(
            !text.contains("claude") && text.contains("终端 1"),
            "{text}"
        );
        assert_eq!(ok, tr("重启"));
    }
}
