//! 给终端起的名字（会话页、仓库终端页的「重命名…」、双击名字、F2）。
//!
//! 名字记在设置里（只记这台电脑上的），重开 Kuai 以后还在。按主机、kuaid 给的编号和开出来的时刻认
//! （编号会在 kuaid 重新启动以后重用）；在 Kuai 里重启的终端名字跟到新开的那个上（重启前记下位置，
//! 新的出现在列表里时接上，同 `presets::restarting`）。终端结束了、列表里没有它了，名字就不要了。

use fr_core::protocol::TermInfo;
use serde::{Deserialize, Serialize};

use crate::settings::TermSlot;
use crate::terminal;

/// 存下的开出来的时刻和列表里倒推的差这么多秒以内算同一个终端
const BORN_SLACK: u64 = crate::term_memory::BORN_SLACK;
/// 名字最长几个字（多了侧栏也放不下）
pub const MAX_CHARS: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermName {
    /// 主机的键（本机是空串，经别的主机连的是 `那台>它`）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub machine: String,
    /// 哪个仓库的第几个终端（重启时按它接到新开的那个上）
    pub slot: TermSlot,
    /// kuaid 给的编号；正在重启、等新的出现时是空的
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub term: String,
    /// 开出来的时刻（Unix 秒）
    #[serde(default)]
    pub born: u64,
    pub name: String,
}

/// 这个终端起过的名字。
pub fn find<'a>(names: &'a [TermName], machine: &str, term: &str, born: u64) -> Option<&'a str> {
    names
        .iter()
        .find(|name| {
            name.machine == machine
                && !name.term.is_empty()
                && name.term == term
                && name.born.abs_diff(born) <= BORN_SLACK
        })
        .map(|name| name.name.as_str())
}

/// 改名：空的是去掉起的名字，回到默认的。返回改没改。
pub fn rename(
    names: &mut Vec<TermName>,
    machine: &str,
    slot: &TermSlot,
    term: &str,
    born: u64,
    name: &str,
) -> bool {
    let name: String = name.trim().chars().take(MAX_CHARS).collect();
    let at = names.iter().position(|record| {
        record.machine == machine && record.term == term && record.born.abs_diff(born) <= BORN_SLACK
    });
    match (at, name.is_empty()) {
        (Some(at), true) => {
            names.remove(at);
            true
        }
        (Some(at), false) if names[at].name == name => false,
        (Some(at), false) => {
            names[at].name = name;
            true
        }
        (None, true) => false,
        (None, false) => {
            names.push(TermName {
                machine: machine.to_string(),
                slot: slot.clone(),
                term: term.to_string(),
                born,
                name,
            });
            true
        }
    }
}

/// 要在 Kuai 里重启 `term`：名字留着，等这个位置上新开的那个出现了接上去。
pub fn restarting(names: &mut [TermName], machine: &str, term: &str) -> bool {
    let mut changed = false;
    for record in names.iter_mut() {
        if record.machine == machine && record.term == term {
            record.term.clear();
            record.born = 0;
            changed = true;
        }
    }
    changed
}

/// 一台主机推来了完整的终端列表：在等新终端的接上去，终端已经不在了的去掉。`now` 是这一刻的
/// Unix 秒（倒推开出来的时刻）。返回改没改。
pub fn reconcile(names: &mut Vec<TermName>, machine: &str, list: &[TermInfo], now: u64) -> bool {
    let before = names.clone();
    names.retain_mut(|record| {
        if record.machine != machine {
            return true;
        }
        if record.term.is_empty() {
            // 重启以后新开的那个：同一个仓库同一个序号
            let found = list.iter().find(|info| {
                terminal::session_number(&record.slot.entry.path, &info.key)
                    == Some(record.slot.number)
            });
            if let Some(info) = found {
                record.term = info.term.clone();
                record.born = now.saturating_sub(info.age);
            }
            return true;
        }
        list.iter().any(|info| {
            info.term == record.term
                && now.saturating_sub(info.age).abs_diff(record.born) <= BORN_SLACK
        })
    });
    *names != before
}

#[cfg(test)]
mod tests {
    use super::{TermName, find, reconcile, rename, restarting};
    use crate::settings::{RepoEntry, TermSlot};
    use fr_core::protocol::TermInfo;

    fn slot(number: u32) -> TermSlot {
        TermSlot {
            entry: RepoEntry {
                server: None,
                path: "/repo".into(),
            },
            number,
        }
    }

    fn info(term: &str, number: u32, age: u64) -> TermInfo {
        TermInfo {
            term: term.into(),
            key: crate::terminal::session_key("/repo", number),
            age,
            alive: true,
            ..TermInfo::default()
        }
    }

    #[test]
    fn 改名_留空回到默认() {
        let mut names: Vec<TermName> = Vec::new();
        assert!(rename(&mut names, "", &slot(1), "t1", 1000, "  前端  "));
        assert_eq!(find(&names, "", "t1", 1000), Some("前端"));
        assert_eq!(find(&names, "s1", "t1", 1000), None, "别的主机上的 t1");
        assert!(
            !rename(&mut names, "", &slot(1), "t1", 1000, "前端"),
            "没变"
        );
        assert!(rename(&mut names, "", &slot(1), "t1", 1005, ""));
        assert!(names.is_empty());
    }

    #[test]
    fn 重启以后名字跟到新开的那个上_结束了的去掉() {
        let mut names: Vec<TermName> = Vec::new();
        rename(&mut names, "", &slot(1), "t1", 1000, "前端");
        rename(&mut names, "", &slot(2), "t2", 1000, "后端");
        assert!(restarting(&mut names, "", "t1"));
        // 新的列表：t1 已经没了，同一个位置上是 t3；t2 结束了
        let list = [info("t3", 1, 2)];
        assert!(reconcile(&mut names, "", &list, 2000));
        assert_eq!(names.len(), 1);
        assert_eq!(find(&names, "", "t3", 1998), Some("前端"));
        // kuaid 重新启动以后又一个 t3（开出来的时刻对不上）：不是它
        let list = [info("t3", 1, 5)];
        assert!(reconcile(&mut names, "", &list, 9000));
        assert!(names.is_empty());
    }
}
