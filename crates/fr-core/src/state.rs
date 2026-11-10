//! 仓库是不是停在一个进行到一半的操作上：合并、变基、挑拣、还原。
//!
//! 这些操作遇到冲突停下来时，git 目录里会留下 `MERGE_HEAD` 之类的伪引用。
//! 用一条 `git cat-file --batch-check` 把它们都问一遍，本机和服务器上的仓库都能用。

/// 进行到一半的操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProgress {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl InProgress {
    pub fn name(self) -> &'static str {
        match self {
            InProgress::Merge => "合并",
            InProgress::Rebase => "变基",
            InProgress::CherryPick => "挑拣",
            InProgress::Revert => "还原",
        }
    }

    /// git 的子命令，后面接 `--continue`、`--abort`。
    pub fn command(self) -> &'static str {
        match self {
            InProgress::Merge => "merge",
            InProgress::Rebase => "rebase",
            InProgress::CherryPick => "cherry-pick",
            InProgress::Revert => "revert",
        }
    }
}

/// 变基放最前：变基中途停下时 `CHERRY_PICK_HEAD` 也可能在。
const MARKERS: [(&str, InProgress); 4] = [
    ("REBASE_HEAD", InProgress::Rebase),
    ("MERGE_HEAD", InProgress::Merge),
    ("CHERRY_PICK_HEAD", InProgress::CherryPick),
    ("REVERT_HEAD", InProgress::Revert),
];

pub fn args() -> Vec<String> {
    vec!["cat-file".into(), "--batch-check".into()]
}

/// 配合 [`args`] 写进标准输入的内容。
pub fn stdin() -> Vec<u8> {
    MARKERS
        .iter()
        .flat_map(|(name, _)| format!("{name}\n").into_bytes())
        .collect()
}

/// 解析输出：每个伪引用一行，存在时是「哈希 类型 大小」，不存在时是「名字 missing」。
pub fn parse(out: &[u8]) -> Option<InProgress> {
    let text = String::from_utf8_lossy(out);
    text.lines()
        .zip(MARKERS)
        .find(|(line, _)| !line.trim_end().ends_with("missing") && !line.trim().is_empty())
        .map(|(_, (_, state))| state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 按伪引用判断进行中的操作() {
        let none = b"REBASE_HEAD missing\nMERGE_HEAD missing\nCHERRY_PICK_HEAD missing\nREVERT_HEAD missing\n";
        assert_eq!(parse(none), None);
        let merge = b"REBASE_HEAD missing\n0123456789abcdef0123456789abcdef01234567 commit 250\nCHERRY_PICK_HEAD missing\nREVERT_HEAD missing\n";
        assert_eq!(parse(merge), Some(InProgress::Merge));
        // 变基中途 CHERRY_PICK_HEAD 也在：算变基
        let rebase = b"0123456789abcdef0123456789abcdef01234567 commit 250\nMERGE_HEAD missing\n0123456789abcdef0123456789abcdef01234567 commit 250\nREVERT_HEAD missing\n";
        assert_eq!(parse(rebase), Some(InProgress::Rebase));
        assert_eq!(parse(b""), None);
    }

    #[test]
    fn 真仓库里没有进行中的操作() {
        let dir = std::env::temp_dir().join(format!("fr-state-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::git::run(&dir, &["init", "-q"]).unwrap();
        let (running, _) = crate::exec::spawn(&dir, &args(), Some(stdin())).unwrap();
        let (out, finished) = running.collect().unwrap();
        assert_eq!(finished.code, Some(0));
        assert_eq!(parse(&out), None);
        assert_eq!(String::from_utf8_lossy(&out).lines().count(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
