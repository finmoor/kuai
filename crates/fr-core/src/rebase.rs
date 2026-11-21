//! 交互式变基：列出要整理的提交，按界面上排好的顺序和做法生成 git 的待办清单。
//!
//! git 的交互式变基是先把待办清单写进一个文件，再启动「清单编辑器」让人改。这里没有终端，
//! 所以把编辑器换成一小段 shell（[`EDITOR`]）：git 在后面接上清单文件的路径，它把标准输入
//! （我们排好的清单）写进那个文件。编辑器用 `-c sequence.editor=…` 给，不用环境变量，本机和
//! 服务器上的 git 都走得通（git 启动编辑器用的是 sh，Git for Windows 自带）。
//!
//! 清单和说明都不放进命令里：Git for Windows 经 sh 执行的命令（编辑器、清单里的 `exec`）超过
//! 8191 个字符就被截断（实测 git 2.53）。整理一百七十来个提交，旧的写法变基就失败；截在命令中间
//! 还可能变成另一条能执行的命令（比如只剩 `git commit --amend`，说明没改却像是成功了）。
//!
//! 改说明不用清单里的 `reword`：它要开编辑器。改成先 `pick`，再用一行 `exec` 修改刚放上去的
//! 那个提交：`git commit --amend -F` 读编辑器事先写在清单旁边的说明文件（带 `--allow-empty`：
//! 没有改动的提交变基时照样留着，改它的说明也得行）。
//!
//! 合并提交不能这样整理：清单里的 `pick` 重放不了合并提交，保留合并关系要用 `--rebase-merges`
//! 的另一套清单。范围里有合并提交时界面不打开整理（见 [`Candidate::parents`]）。

/// 对一个提交做什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 原样保留
    Pick,
    /// 保留，说明换成这个
    Reword(String),
    /// 并入更早的那个提交，两边的说明都留着
    Squash,
    /// 并入更早的那个提交，这一条的说明不要了
    Fixup,
    /// 丢掉这个提交
    Drop,
}

/// 清单里的一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// 完整哈希
    pub id: String,
    pub action: Action,
}

/// 要整理的一个提交（`base..HEAD` 里的）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    /// 父提交有几个；多于一个是合并提交
    pub parents: usize,
    pub author: String,
    /// 作者时间，Unix 秒
    pub time: i64,
    /// 完整说明（去掉了末尾的空行）
    pub message: String,
}

impl Candidate {
    /// 说明的第一行。
    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

/// 列出 `base` 之后到 HEAD 的提交，新的在前。字段用 0x1f 分隔，提交之间是 NUL。
pub fn list_args(base: &str) -> Vec<String> {
    vec![
        "log".into(),
        "-z".into(),
        "--format=%H%x1f%P%x1f%an%x1f%at%x1f%B".into(),
        format!("{base}..HEAD"),
        "--".into(),
    ]
}

/// 解析 [`list_args`] 的输出。
pub fn parse_list(out: &[u8]) -> Vec<Candidate> {
    String::from_utf8_lossy(out)
        .split('\0')
        .filter_map(|record| {
            let record = record.trim_start_matches('\n');
            let mut fields = record.splitn(5, '\x1f');
            let id = fields.next().filter(|id| !id.is_empty())?;
            let parents = fields.next()?.split_ascii_whitespace().count();
            let author = fields.next()?;
            let time = fields.next()?.parse().unwrap_or(0);
            let message = fields.next().unwrap_or("").trim_end().to_string();
            Some(Candidate {
                id: id.to_string(),
                parents,
                author: author.to_string(),
                time,
                message,
            })
        })
        .collect()
}

/// 清单有没有问题；有的话返回给用户看的原因。`items` 是旧的在前（git 清单的顺序）。
pub fn check(items: &[Item]) -> Result<(), Problem> {
    let kept = |item: &&Item| item.action != Action::Drop;
    let Some(first) = items.iter().find(kept) else {
        return Err(Problem::NothingLeft);
    };
    if matches!(first.action, Action::Squash | Action::Fixup) {
        return Err(Problem::NothingToMeldInto);
    }
    if items
        .iter()
        .any(|item| matches!(&item.action, Action::Reword(text) if text.trim().is_empty()))
    {
        return Err(Problem::EmptyMessage);
    }
    Ok(())
}

/// 清单的问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// 全都丢掉了
    NothingLeft,
    /// 留下来的第一个提交要并入更早的，可它前面没有了
    NothingToMeldInto,
    /// 改了说明却没写内容
    EmptyMessage,
}

/// 清单的每一行，旧的在前。丢掉的提交写成 `drop`，这样 git 报告里看得到。改说明的第 k 个提交
/// （从 1 数），新的说明从清单旁边的 `fr-msg-k` 里读（[`input`] 交给编辑器写下的）。
pub fn todo(items: &[Item]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut reworded = 0;
    for item in items {
        match &item.action {
            Action::Pick => lines.push(format!("pick {}", item.id)),
            Action::Squash => lines.push(format!("squash {}", item.id)),
            Action::Fixup => lines.push(format!("fixup {}", item.id)),
            Action::Drop => lines.push(format!("drop {}", item.id)),
            Action::Reword(_) => {
                reworded += 1;
                lines.push(format!("pick {}", item.id));
                lines.push(format!(
                    "exec git commit --amend --allow-empty -q -F \"$(git rev-parse --git-path rebase-merge/fr-msg-{reworded})\""
                ));
            }
        }
    }
    lines
}

/// 清单编辑器（git 在后面接上清单文件的路径）。标准输入的第一行是各条新说明的字节数，接着是
/// 这几条说明，最后是清单：按字节数把说明一条条写到清单旁边的 `fr-msg-1`、`fr-msg-2`……（变基
/// 结束、中止时 git 连同清单一起删掉），剩下的写进清单。`read` 和 `dd bs=1` 一次读一个字节，
/// 不会多读走后面的。
const EDITOR: &str = r#"f(){ d=$(dirname "$1"); read -r lens; i=0; for n in $lens; do i=$((i+1)); dd bs=1 count="$n" of="$d/fr-msg-$i" 2>/dev/null || return 1; done; cat > "$1"; }; f"#;

/// `git rebase -i` 的参数：把 `base` 之后的提交重做，清单和新的说明（[`input`]）从标准输入给。
/// 总是带 `--autostash`：工作区干净时它什么都不做，有改动时先贮藏、做完放回去。
pub fn args(base: &str) -> Vec<String> {
    vec![
        "-c".into(),
        format!("sequence.editor={EDITOR}"),
        "rebase".into(),
        "-i".into(),
        "--autostash".into(),
        base.into(),
    ]
}

/// [`args`] 给人看的写法（活动记录、失败的详情）：不露出清单编辑器那段脚本。
pub fn shown(base: &str) -> String {
    format!("git rebase -i --autostash {base}")
}

/// 交给 [`args`] 的标准输入（格式见 [`EDITOR`]），`items` 是旧的在前。
pub fn input(items: &[Item]) -> Vec<u8> {
    let messages: Vec<Vec<u8>> = items
        .iter()
        .filter_map(|item| match &item.action {
            Action::Reword(text) => {
                let mut text = text.trim_end().replace('\r', "");
                text.push('\n');
                Some(text.into_bytes())
            }
            _ => None,
        })
        .collect();
    let lens: Vec<String> = messages.iter().map(|text| text.len().to_string()).collect();
    let mut out = lens.join(" ").into_bytes();
    out.push(b'\n');
    for text in &messages {
        out.extend_from_slice(text);
    }
    for line in todo(items) {
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, action: Action) -> Item {
        Item {
            id: id.into(),
            action,
        }
    }

    #[test]
    fn 清单按做法写() {
        let lines = todo(&[
            item("a1", Action::Pick),
            item("b2", Action::Squash),
            item("c3", Action::Fixup),
            item("d4", Action::Drop),
            item(
                "e5",
                Action::Reword("新的说明\n\n正文里有 '引号' 和 \\ 反斜杠".into()),
            ),
        ]);
        assert_eq!(
            lines,
            vec![
                "pick a1",
                "squash b2",
                "fixup c3",
                "drop d4",
                "pick e5",
                r#"exec git commit --amend --allow-empty -q -F "$(git rev-parse --git-path rebase-merge/fr-msg-1)""#,
            ]
        );
    }

    #[test]
    fn 标准输入先写说明的字节数和说明_再写清单() {
        let input = input(&[
            item("a1", Action::Reword("一\r\n二\n\n".into())),
            item("b2", Action::Pick),
            item("c3", Action::Reword("x".into())),
        ]);
        let text = String::from_utf8(input).unwrap();
        // 「一\n二\n」是 8 个字节（汉字 3 个字节）；去掉了 \r 和末尾的空行，末尾补一个换行
        assert!(text.starts_with("8 2\n一\n二\nx\npick a1\nexec "), "{text}");
        assert!(text.ends_with("fr-msg-2)\"\n"), "{text}");
    }

    #[test]
    fn 清单的问题查得出来() {
        assert_eq!(check(&[item("a", Action::Pick)]), Ok(()));
        assert_eq!(check(&[item("a", Action::Drop)]), Err(Problem::NothingLeft));
        assert_eq!(
            check(&[item("a", Action::Drop), item("b", Action::Squash)]),
            Err(Problem::NothingToMeldInto)
        );
        assert_eq!(
            check(&[
                item("a", Action::Pick),
                item("b", Action::Reword(" ".into()))
            ]),
            Err(Problem::EmptyMessage)
        );
    }

    #[test]
    fn 解析要整理的提交() {
        let out = "c3\x1fb2\x1f张伟\x1f1700000200\x1f第三个\n\n有正文\n\0\
                   b2\x1fa1 x9\x1fAlice\x1f1700000100\x1f合并\n\0";
        let list = parse_list(out.as_bytes());
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "c3");
        assert_eq!(list[0].subject(), "第三个");
        assert_eq!(list[0].message, "第三个\n\n有正文");
        assert_eq!(list[1].parents, 2);
    }

    /// 清单从标准输入给：三个提交各改成 12000 字的说明，合起来超过 Windows 一条命令的长度上限。
    #[test]
    fn 改的说明很长也不占命令行() {
        let dir = std::env::temp_dir().join(format!("fr-rebase-long-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| crate::git::run(&dir, args).unwrap();
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "user.email", "t@example.com"]);
        let mut ids = Vec::new();
        for n in 0..4 {
            git(&["commit", "-q", "--allow-empty", "-m", &format!("c{n}")]);
            let id = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
            ids.push(id.trim().to_string());
        }
        let base = ids.remove(0);
        let long = |n: usize| format!("说明{n} {}", "长".repeat(12000));
        let items: Vec<Item> = ids
            .iter()
            .enumerate()
            .map(|(n, id)| item(id, Action::Reword(long(n))))
            .collect();
        let (running, _) = crate::exec::spawn(&dir, &args(&base), Some(input(&items))).unwrap();
        let (_, finished) = running.collect().unwrap();
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        let body = String::from_utf8(git(&["log", "-1", "--format=%B"])).unwrap();
        assert_eq!(body.trim_end(), long(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真的跑一遍：调顺序、并入、丢弃、改说明（说明里有引号、反斜杠、百分号和换行）。
    #[test]
    fn 按清单变基() {
        let dir = std::env::temp_dir().join(format!("fr-rebase-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| crate::git::run(&dir, args).unwrap();
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "user.email", "t@example.com"]);
        let commit = |name: &str| {
            std::fs::write(dir.join(format!("{name}.txt")), name).unwrap();
            git(&["add", "-A"]);
            git(&["commit", "-q", "-m", name]);
            String::from_utf8(git(&["rev-parse", "HEAD"]))
                .unwrap()
                .trim()
                .to_string()
        };
        let base = commit("base");
        let (a, b, c, d) = (commit("a"), commit("b"), commit("c"), commit("d"));

        let listed = parse_list(&{
            let args = list_args(&base);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            git(&args)
        });
        let ids: Vec<&str> = listed.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [&d, &c, &b, &a], "新的在前");

        // a 改说明；c 挪到 b 前面并且并入 a；b 丢掉；d 留着
        let message = "改过的说明：100% 'ok' \\n 不是换行\n\n第二段";
        let items = [
            item(&a, Action::Reword(message.into())),
            item(&c, Action::Fixup),
            item(&b, Action::Drop),
            item(&d, Action::Pick),
        ];
        assert_eq!(check(&items), Ok(()));
        let (running, _) = crate::exec::spawn(&dir, &args(&base), Some(input(&items))).unwrap();
        let (_, finished) = running.collect().unwrap();
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);

        let log = String::from_utf8(git(&["log", "--format=%s", "main"])).unwrap();
        assert_eq!(
            log.lines().collect::<Vec<_>>(),
            ["d", "改过的说明：100% 'ok' \\n 不是换行", "base"]
        );
        let body = String::from_utf8(git(&["log", "-1", "--format=%B", "HEAD~1"])).unwrap();
        assert_eq!(body.trim_end(), message);
        // b 丢了，c 的内容并进了改过说明的那个提交
        assert!(!dir.join("b.txt").exists());
        let files =
            String::from_utf8(git(&["show", "--format=", "--name-only", "HEAD~1"])).unwrap();
        assert_eq!(files.lines().collect::<Vec<_>>(), ["a.txt", "c.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
