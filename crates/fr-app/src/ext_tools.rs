//! 外部的比较、合并工具：Beyond Compare、WinMerge、Meld、KDiff3、VS Code、P4Merge，
//! 或者自己写的命令（`$LOCAL`、`$REMOTE`、`$BASE`、`$MERGED` 换成文件的路径，`$LTITLE`、`$RTITLE`、
//! `$BTITLE`、`$MTITLE` 换成各栏的标题）。
//!
//! 预设都传各栏的标题：不传的话各家显示的是临时文件名
//! （`app.LOCAL.txt`），WinMerge 三方合并时还按自己的习惯把左栏叫 Theirs、右栏叫 Mine，和 Kuai 的
//! 「当前在左、传入在右」正好相反。
//!
//! 工具在本机跑；服务器上的仓库先把要比的几份取到本机的临时目录（见 `repo/ext_compare.rs`）。

use std::path::{Path, PathBuf};
use std::process::Command;

use fr_core::i18n::tr;
use fr_core::tf;

/// 比较还是合并。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Diff,
    Merge,
}

/// 一个认得的工具。
pub struct Preset {
    /// 存在设置里的名字
    pub id: &'static str,
    pub name: &'static str,
    /// 在 PATH 里找的名字
    names: &'static [&'static str],
    /// Windows 上常见的安装位置（相对 Program Files、本机用户的 Programs）
    windows: &'static [&'static str],
    diff: &'static [&'static str],
    merge: &'static [&'static str],
}

/// 认得的工具。
pub const PRESETS: &[Preset] = &[
    Preset {
        id: "bc",
        name: "Beyond Compare",
        // Windows 上先找 BComp.exe：它等比较窗口关了才退出；BCompare.exe 在已经开着一个
        // Beyond Compare 时把活交过去马上就退出，合并就等不到结果
        names: &["bcomp", "bcompare", "BCompare"],
        windows: &[
            "Beyond Compare 5\\BComp.exe",
            "Beyond Compare 4\\BComp.exe",
            "Beyond Compare 5\\BCompare.exe",
            "Beyond Compare 4\\BCompare.exe",
        ],
        // 选项写成 `-` 开头：Windows、Linux 版都认（Linux 版只认 `-`）
        diff: &[
            "-lefttitle=$LTITLE",
            "-righttitle=$RTITLE",
            "$LOCAL",
            "$REMOTE",
        ],
        merge: &[
            "-lefttitle=$LTITLE",
            "-righttitle=$RTITLE",
            "-centertitle=$BTITLE",
            "$LOCAL",
            "$REMOTE",
            "$BASE",
            "$MERGED",
        ],
    },
    Preset {
        id: "winmerge",
        name: "WinMerge",
        names: &["WinMergeU"],
        windows: &["WinMerge\\WinMergeU.exe"],
        diff: &[
            "/e", "/u", "/dl", "$LTITLE", "/dr", "$RTITLE", "$LOCAL", "$REMOTE",
        ],
        merge: &[
            "/e", "/u", "/dl", "$LTITLE", "/dm", "$BTITLE", "/dr", "$RTITLE", "$LOCAL", "$BASE",
            "$REMOTE", "/o", "$MERGED",
        ],
    },
    Preset {
        id: "meld",
        name: "Meld",
        names: &["meld", "Meld"],
        windows: &["Meld\\Meld.exe"],
        // `--label` 按栏的先后各给一个；三方合并时中间那栏就是要保存的结果（从共同祖先开始）
        diff: &["--label=$LTITLE", "--label=$RTITLE", "$LOCAL", "$REMOTE"],
        merge: &[
            "--label=$LTITLE",
            "--label=$MTITLE",
            "--label=$RTITLE",
            "$LOCAL",
            "$BASE",
            "$REMOTE",
            "--output",
            "$MERGED",
        ],
    },
    Preset {
        id: "kdiff3",
        name: "KDiff3",
        names: &["kdiff3"],
        windows: &["KDiff3\\kdiff3.exe", "KDiff3\\bin\\kdiff3.exe"],
        diff: &["--L1", "$LTITLE", "--L2", "$RTITLE", "$LOCAL", "$REMOTE"],
        merge: &[
            "--L1", "$BTITLE", "--L2", "$LTITLE", "--L3", "$RTITLE", "$BASE", "$LOCAL", "$REMOTE",
            "-o", "$MERGED",
        ],
    },
    Preset {
        id: "vscode",
        name: "VS Code",
        names: &["code"],
        windows: &[],
        diff: &["--wait", "--diff", "$LOCAL", "$REMOTE"],
        merge: &["--wait", "--merge", "$LOCAL", "$REMOTE", "$BASE", "$MERGED"],
    },
    Preset {
        id: "p4merge",
        name: "P4Merge",
        names: &["p4merge"],
        windows: &["Perforce\\p4merge.exe"],
        // P4Merge 叫左、右（不叫 theirs、yours），当前放左边和 Kuai 的合并编辑器一致
        diff: &["-nl", "$LTITLE", "-nr", "$RTITLE", "$LOCAL", "$REMOTE"],
        merge: &[
            "-nb", "$BTITLE", "-nl", "$LTITLE", "-nr", "$RTITLE", "$BASE", "$LOCAL", "$REMOTE",
            "$MERGED",
        ],
    },
];

/// 设置里「自定义命令」那一项的名字。
pub const CUSTOM: &str = "custom";

/// 一次比较、合并用到的几个文件和标题。
#[derive(Default, Clone, Debug)]
pub struct Files {
    pub local: PathBuf,
    pub remote: PathBuf,
    pub base: Option<PathBuf>,
    pub merged: Option<PathBuf>,
    pub local_title: String,
    pub remote_title: String,
    /// 三方合并时共同祖先那一栏、结果那一栏的标题（比较时用不到）
    pub base_title: String,
    pub merged_title: String,
}

/// 设置里选的工具叫什么（提示、菜单里用）；没选是 None。
pub fn tool_name(id: &str) -> Option<&'static str> {
    match id {
        "" => None,
        CUSTOM => Some(tr("自定义命令")),
        id => PRESETS.iter().find(|p| p.id == id).map(|p| p.name),
    }
}

/// 拆开一行命令：空格分开，双引号里的算一段（引号去掉）。
pub fn split_command(line: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut any = false;
    for ch in line.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            ch if ch.is_whitespace() && !quoted => {
                if any || !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
                any = false;
            }
            ch => current.push(ch),
        }
    }
    if any || !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// 参数里的 `$LOCAL` 这些换成路径、标题（参数中间出现的也换）。一遍扫完：换进去的路径、标题里
/// 碰巧有 `$REMOTE` 这样的字不会再被换一次。
fn fill(arg: &str, files: &Files) -> String {
    let path = |path: Option<&Path>| {
        path.map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let values = [
        ("$LOCAL", path(Some(&files.local))),
        ("$REMOTE", path(Some(&files.remote))),
        ("$BASE", path(files.base.as_deref())),
        ("$MERGED", path(files.merged.as_deref())),
        ("$LTITLE", files.local_title.clone()),
        ("$RTITLE", files.remote_title.clone()),
        ("$BTITLE", files.base_title.clone()),
        ("$MTITLE", files.merged_title.clone()),
    ];
    let mut out = String::with_capacity(arg.len());
    let mut rest = arg;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        match values.iter().find(|(name, _)| tail.starts_with(name)) {
            Some((name, value)) => {
                out.push_str(value);
                rest = &tail[name.len()..];
            }
            None => {
                out.push('$');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 找工具的程序：先在 PATH 里找，Windows 上再找常见的安装位置。
fn locate(preset: &Preset) -> Option<PathBuf> {
    if preset.id == "vscode" {
        return crate::shell::code_program();
    }
    if let Some(found) = preset
        .names
        .iter()
        .find_map(|name| crate::shell::find_program(name))
    {
        return Some(found);
    }
    if cfg!(windows) {
        let roots = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
            .iter()
            .filter_map(|var| std::env::var_os(var).map(PathBuf::from))
            .chain(std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("Programs")));
        for root in roots {
            for rel in preset.windows {
                let path = root.join(rel);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// 认得的工具这台电脑上有没有装。设置页每画一次都要问，找一次要翻一遍 PATH：记住结果，
/// 隔十秒再找（装好工具回到设置页就能看到）。
pub fn installed(id: &str) -> bool {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static FOUND: Mutex<Option<(Instant, Vec<&'static str>)>> = Mutex::new(None);
    let mut found = FOUND.lock().unwrap_or_else(|e| e.into_inner());
    if !found
        .as_ref()
        .is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(10))
    {
        let ids = PRESETS
            .iter()
            .filter(|preset| locate(preset).is_some())
            .map(|preset| preset.id)
            .collect();
        *found = Some((Instant::now(), ids));
    }
    found.as_ref().is_some_and(|(_, ids)| ids.contains(&id))
}

/// 照设置做出要起的命令。`tool` 是设置里选的（预设的名字或 [`CUSTOM`]），`custom` 是自定义命令。
pub fn command(kind: Kind, tool: &str, custom: &str, files: &Files) -> Result<Command, String> {
    let (program, args): (PathBuf, Vec<String>) = match tool {
        "" => {
            return Err(match kind {
                Kind::Diff => tr("尚未设置外部比较工具。请在「设置 → 外部工具」中选择。").into(),
                Kind::Merge => tr("尚未设置外部合并工具。请在「设置 → 外部工具」中选择。").into(),
            });
        }
        CUSTOM => {
            let mut parts = split_command(custom).into_iter();
            let Some(program) = parts.next().filter(|p| !p.is_empty()) else {
                return Err(tr("自定义命令是空的。请在「设置 → 外部工具」中填写。").into());
            };
            let found =
                crate::shell::find_program(&program).unwrap_or_else(|| PathBuf::from(&program));
            (found, parts.map(|arg| fill(&arg, files)).collect())
        }
        id => {
            let Some(preset) = PRESETS.iter().find(|p| p.id == id) else {
                return Err(tf!("不认识的工具：{id}", id));
            };
            let Some(program) = locate(preset) else {
                return Err(tf!(
                    "找不到 {name}。请确认已安装并在 PATH 中，或在「设置 → 外部工具」中改用自定义命令。",
                    name = preset.name
                ));
            };
            let template = match kind {
                Kind::Diff => preset.diff,
                Kind::Merge => preset.merge,
            };
            (
                program,
                template.iter().map(|arg| fill(arg, files)).collect(),
            )
        }
    };
    let mut command = Command::new(program);
    command.args(args);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::{Files, Kind, command, fill, split_command};
    use std::path::PathBuf;

    #[test]
    fn 拆命令() {
        assert_eq!(
            split_command(r#""C:\Program Files\x\x.exe" --diff "$LOCAL" $REMOTE"#),
            vec![r"C:\Program Files\x\x.exe", "--diff", "$LOCAL", "$REMOTE"]
        );
        assert_eq!(split_command("  a   b "), vec!["a", "b"]);
        assert_eq!(split_command(r#"a "" b"#), vec!["a", "", "b"]);
        assert!(split_command("").is_empty());
    }

    #[test]
    fn 换上路径() {
        let files = Files {
            local: PathBuf::from("/t/a.LOCAL.rs"),
            remote: PathBuf::from("/t/a.REMOTE.rs"),
            base: Some(PathBuf::from("/t/a.BASE.rs")),
            merged: Some(PathBuf::from("/r/a.rs")),
            local_title: "a.rs (HEAD)".into(),
            remote_title: "a.rs".into(),
            base_title: "共同祖先".into(),
            merged_title: "结果".into(),
        };
        assert_eq!(fill("-o=$MERGED", &files), "-o=/r/a.rs");
        assert_eq!(
            fill("-centertitle=$BTITLE", &files),
            "-centertitle=共同祖先"
        );
        assert_eq!(fill("--label=$MTITLE", &files), "--label=结果");
        // 不认识的 $ 原样留着；换进去的内容不再换第二次
        assert_eq!(fill("$HOME/$X", &files), "$HOME/$X");
        let tricky = Files {
            local: PathBuf::from("/t/$REMOTE.txt"),
            ..files.clone()
        };
        assert_eq!(fill("$LOCAL", &tricky), "/t/$REMOTE.txt");
        assert_eq!(fill("$BASE", &files), "/t/a.BASE.rs");
        assert_eq!(fill("$LTITLE", &files), "a.rs (HEAD)");
        // 自定义命令照填
        let cmd = command(
            Kind::Merge,
            "custom",
            "tool $LOCAL $REMOTE $BASE $MERGED",
            &files,
        )
        .unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["/t/a.LOCAL.rs", "/t/a.REMOTE.rs", "/t/a.BASE.rs", "/r/a.rs"]
        );
        assert!(command(Kind::Diff, "", "", &files).is_err());
        assert!(command(Kind::Diff, "custom", "  ", &files).is_err());
    }

    #[test]
    fn 预设都传各栏的标题() {
        let files = Files {
            local: PathBuf::from("L"),
            remote: PathBuf::from("R"),
            base: Some(PathBuf::from("B")),
            merged: Some(PathBuf::from("M")),
            local_title: "当前".into(),
            remote_title: "传入".into(),
            base_title: "共同祖先".into(),
            merged_title: "结果".into(),
        };
        let args = |id: &str, kind: Kind| -> Vec<String> {
            let preset = super::PRESETS.iter().find(|p| p.id == id).unwrap();
            let template = match kind {
                Kind::Diff => preset.diff,
                Kind::Merge => preset.merge,
            };
            template.iter().map(|arg| fill(arg, &files)).collect()
        };
        // WinMerge：左栏是当前（不传的话它叫 Theirs）
        assert_eq!(
            args("winmerge", Kind::Merge),
            [
                "/e",
                "/u",
                "/dl",
                "当前",
                "/dm",
                "共同祖先",
                "/dr",
                "传入",
                "L",
                "B",
                "R",
                "/o",
                "M"
            ]
        );
        assert_eq!(
            args("bc", Kind::Merge),
            [
                "-lefttitle=当前",
                "-righttitle=传入",
                "-centertitle=共同祖先",
                "L",
                "R",
                "B",
                "M"
            ]
        );
        assert_eq!(
            args("meld", Kind::Merge),
            [
                "--label=当前",
                "--label=结果",
                "--label=传入",
                "L",
                "B",
                "R",
                "--output",
                "M"
            ]
        );
        assert_eq!(
            args("p4merge", Kind::Merge),
            [
                "-nb",
                "共同祖先",
                "-nl",
                "当前",
                "-nr",
                "传入",
                "B",
                "L",
                "R",
                "M"
            ]
        );
        assert_eq!(
            args("kdiff3", Kind::Merge),
            [
                "--L1",
                "共同祖先",
                "--L2",
                "当前",
                "--L3",
                "传入",
                "B",
                "L",
                "R",
                "-o",
                "M"
            ]
        );
        // 每个预设的比较、合并都带上了要用的文件
        for preset in super::PRESETS {
            let diff = args(preset.id, Kind::Diff);
            assert!(diff.contains(&"L".to_string()) && diff.contains(&"R".to_string()));
            let merge = args(preset.id, Kind::Merge);
            for file in ["L", "R", "B", "M"] {
                assert!(merge.iter().any(|a| a == file), "{} {file}", preset.id);
            }
        }
    }
}
