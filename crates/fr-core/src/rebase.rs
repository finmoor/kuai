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
    /// 重放到这里停下：改内容、暂存，继续时并进这个提交
    Edit,
    /// 重放到这里停下，把这个提交的改动放回工作区，分几次重新提交（清单里也是 `edit`，
    /// 另在清单旁边的 `fr-split` 里记一笔，见 [`Paused::split`]）
    Split,
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
///
/// `moved` 是要跟着一起移动的别的分支：（它指着的提交，完整的引用名）。在那个提交（连同并进它的
/// 那几个）重放完以后写一行 `update-ref`（git 2.38 起；`--update-refs` 自己生成的那几行在我们
/// 换掉清单时就没了，所以自己写）。提交被丢掉的，分支落在它前面那个提交上。
pub fn todo(items: &[Item], moved: &[(String, String)]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut reworded = 0;
    // 等着写的 `update-ref`：后面紧跟着并入的提交时，等它们并完再写
    let mut pending: Vec<&str> = Vec::new();
    for item in items {
        if !matches!(item.action, Action::Squash | Action::Fixup) {
            lines.extend(pending.drain(..).map(|name| format!("update-ref {name}")));
        }
        match &item.action {
            Action::Pick => lines.push(format!("pick {}", item.id)),
            Action::Squash => lines.push(format!("squash {}", item.id)),
            Action::Fixup => lines.push(format!("fixup {}", item.id)),
            Action::Edit | Action::Split => lines.push(format!("edit {}", item.id)),
            Action::Drop => lines.push(format!("drop {}", item.id)),
            Action::Reword(_) => {
                reworded += 1;
                lines.push(format!("pick {}", item.id));
                lines.push(format!(
                    "exec git commit --amend --allow-empty -q -F \"$(git rev-parse --git-path rebase-merge/fr-msg-{reworded})\""
                ));
            }
        }
        pending.extend(
            moved
                .iter()
                .filter(|(id, _)| *id == item.id)
                .map(|(_, name)| name.as_str()),
        );
    }
    lines.extend(pending.drain(..).map(|name| format!("update-ref {name}")));
    lines
}

/// 清单编辑器（git 在后面接上清单文件的路径）。标准输入的第一行是各条新说明的字节数，接着是
/// 这几条说明，然后一行是要拆分的提交（空格分开，可以是空行），最后是清单：按字节数把说明一条条
/// 写到清单旁边的 `fr-msg-1`、`fr-msg-2`……，要拆分的提交写进 `fr-split`（变基结束、中止时 git
/// 连同清单一起删掉），剩下的写进清单。`read` 和 `dd bs=1` 一次读一个字节，不会多读走后面的。
/// 另放一个空的 `fr-curate`：停下时认得出这是「整理提交」起的变基（[`Paused::curating`]）。
const EDITOR: &str = r#"f(){ d=$(dirname "$1"); : > "$d/fr-curate"; read -r lens; i=0; for n in $lens; do i=$((i+1)); dd bs=1 count="$n" of="$d/fr-msg-$i" 2>/dev/null || return 1; done; read -r split; [ -z "$split" ] || printf '%s\n' $split > "$d/fr-split"; cat > "$1"; }; f"#;

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

/// 交给 [`args`] 的标准输入（格式见 [`EDITOR`]），`items` 是旧的在前；`moved` 见 [`todo`]。
pub fn input(items: &[Item], moved: &[(String, String)]) -> Vec<u8> {
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
    let split: Vec<&str> = items
        .iter()
        .filter(|item| item.action == Action::Split)
        .map(|item| item.id.as_str())
        .collect();
    out.extend_from_slice(split.join(" ").as_bytes());
    out.push(b'\n');
    for line in todo(items, moved) {
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out
}

/// 变基停在一半时的情况（[`state_args`] 读出来的）。只按 `REBASE_HEAD` 认（[`crate::state`]）认
/// 不全：清单里的 `break`、`exec` 失败停下时没有它，变基目录却在。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Paused {
    /// 停在 `edit` 上：这个提交已经放上去了（`amend` 里记的就是它），改完暂存，继续时并进去
    pub amend: Option<String>,
    /// 停在哪个提交上（清单里写的那个，重放之前的哈希）
    pub stopped: Option<String>,
    /// 第几步、共几步
    pub step: Option<(u32, u32)>,
    /// 正在变基的分支（`refs/heads/…`；分离的 HEAD 上变基时没有）
    pub branch: Option<String>,
    /// 变基开始前 HEAD 在哪
    pub orig_head: Option<String>,
    /// 这一次整理里要拆分的提交（重放之前的哈希）
    pub split: Vec<String>,
    /// 现在的 HEAD
    pub head: String,
    /// 一并移动的分支（`--update-refs`、清单里的 `update-ref`）：（完整引用名，变基开始前指着的提交）
    pub moved: Vec<(String, String)>,
    /// 是「整理提交」起的变基（变基目录里有 [`EDITOR`] 放的 `fr-curate`）：界面上叫「整理」。
    /// 终端里、别处起的变基没有它
    pub curating: bool,
}

impl Paused {
    /// 停在一个要拆分的提交上。
    pub fn splitting(&self) -> bool {
        self.amend.is_some()
            && self
                .stopped
                .as_ref()
                .is_some_and(|stopped| self.split.iter().any(|id| id == stopped))
    }

    /// 要拆分的提交刚放上去，改动还没放回工作区（HEAD 还是它）。
    pub fn split_pending(&self) -> bool {
        self.splitting() && self.amend.as_deref() == Some(self.head.as_str())
    }
}

/// 读变基目录里的几个小文件：不在变基就什么都不输出。借 git 的 shell 别名，一次读完。
/// `rebase-apply` 也是 `git am` 用的目录，里面有 `rebasing` 才是变基。
const STATE: &str = r#"alias.fr-rebase-state=!f(){ d=$(git rev-parse --git-path rebase-merge); if [ ! -d "$d" ]; then d=$(git rev-parse --git-path rebase-apply); [ -f "$d/rebasing" ] || return 0; fi; echo "dir=$d"; for n in amend stopped-sha msgnum end head-name orig-head fr-split update-refs fr-curate; do [ -f "$d/$n" ] && printf '%s=%s\n' "$n" "$(tr '\n' ' ' < "$d/$n" | cut -c1-9000)"; done; printf 'head=%s\n' "$(git rev-parse -q --verify HEAD)"; }; f"#;

/// 读变基停在哪（见 [`Paused`]），输出交给 [`parse_state`]。
pub fn state_args() -> Vec<String> {
    vec!["-c".into(), STATE.into(), "fr-rebase-state".into()]
}

/// 解析 [`state_args`] 的输出；不在变基时 None。
pub fn parse_state(out: &[u8]) -> Option<Paused> {
    let text = String::from_utf8_lossy(out);
    let mut paused = Paused::default();
    let mut found = false;
    let (mut done, mut total) = (None, None);
    for line in text.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let filled = || (!value.is_empty()).then(|| value.to_string());
        match name {
            "dir" => found = true,
            "amend" => paused.amend = filled(),
            "stopped-sha" => paused.stopped = filled(),
            "msgnum" => done = value.parse().ok(),
            "end" => total = value.parse().ok(),
            "head-name" => paused.branch = filled().filter(|name| name.starts_with("refs/")),
            "orig-head" => paused.orig_head = filled(),
            "fr-split" => paused.split = value.split_ascii_whitespace().map(String::from).collect(),
            "fr-curate" => paused.curating = true,
            "head" => paused.head = value.to_string(),
            // 每个分支三行：引用名、开始前的位置、移过去以后的位置（还没移时是全 0）
            "update-refs" => {
                let fields: Vec<&str> = value.split_ascii_whitespace().collect();
                paused.moved = fields
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|[name, from, _]| (name.to_string(), from.to_string()))
                    .collect();
            }
            _ => {}
        }
    }
    paused.step = done.zip(total);
    found.then_some(paused)
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
        let lines = todo(
            &[
                item("a1", Action::Pick),
                item("b2", Action::Squash),
                item("c3", Action::Fixup),
                item("d4", Action::Drop),
                item(
                    "e5",
                    Action::Reword("新的说明\n\n正文里有 '引号' 和 \\ 反斜杠".into()),
                ),
                item("f6", Action::Edit),
                item("g7", Action::Split),
            ],
            &[],
        );
        assert_eq!(
            lines,
            vec![
                "pick a1",
                "squash b2",
                "fixup c3",
                "drop d4",
                "pick e5",
                r#"exec git commit --amend --allow-empty -q -F "$(git rev-parse --git-path rebase-merge/fr-msg-1)""#,
                "edit f6",
                "edit g7",
            ]
        );
    }

    #[test]
    fn 一并移动的分支写在它那个提交并完以后() {
        let moved = [
            ("a1".to_string(), "refs/heads/one".to_string()),
            ("b2".to_string(), "refs/heads/two".to_string()),
            ("d4".to_string(), "refs/heads/four".to_string()),
        ];
        let lines = todo(
            &[
                item("a1", Action::Pick),
                item("b2", Action::Fixup),
                item("c3", Action::Pick),
                item("d4", Action::Drop),
            ],
            &moved,
        );
        // a1 后面跟着并入的 b2：两个分支都等并完再写；d4 丢掉了，分支落在它前面的提交上
        assert_eq!(
            lines,
            vec![
                "pick a1",
                "fixup b2",
                "update-ref refs/heads/one",
                "update-ref refs/heads/two",
                "pick c3",
                "drop d4",
                "update-ref refs/heads/four",
            ]
        );
    }

    #[test]
    fn 读出变基停在哪() {
        assert_eq!(parse_state(b""), None);
        let out = "dir=.git/rebase-merge\namend=aaa1 \nstopped-sha=bbb2 \nmsgnum=2 \nend=5 \n\
                   head-name=refs/heads/main \norig-head=ccc3 \nfr-split=bbb2 ddd4 \nfr-curate=\n\
                   head=aaa1\n";
        let paused = parse_state(out.as_bytes()).unwrap();
        assert!(paused.curating);
        assert_eq!(paused.amend.as_deref(), Some("aaa1"));
        assert_eq!(paused.stopped.as_deref(), Some("bbb2"));
        assert_eq!(paused.step, Some((2, 5)));
        assert_eq!(paused.branch.as_deref(), Some("refs/heads/main"));
        assert_eq!(paused.orig_head.as_deref(), Some("ccc3"));
        assert_eq!(paused.split, ["bbb2", "ddd4"]);
        assert!(paused.splitting() && paused.split_pending());
        // 改动放回工作区以后 HEAD 退了一步
        let later = Paused {
            head: "eee5".into(),
            ..paused.clone()
        };
        assert!(later.splitting() && !later.split_pending());
        // `break` 停下：没有 amend、stopped-sha；分离的 HEAD 上变基没有分支
        let out = "dir=/r/.git/worktrees/w/rebase-merge\nmsgnum=1 \nend=3 \n\
                   head-name=detached HEAD \nhead=fff6\n";
        let paused = parse_state(out.as_bytes()).unwrap();
        assert_eq!((paused.amend, paused.stopped), (None, None));
        assert_eq!(paused.branch, None);
        // 没有 fr-curate：不是「整理提交」起的
        assert!(!paused.curating);
        assert!(!Paused::default().splitting());
        // update-refs：每个分支三行（引用名、开始前、移过去以后）
        let out = "dir=d\nupdate-refs=refs/heads/x aaa 000 refs/heads/y bbb ccc \nhead=h\n";
        let paused = parse_state(out.as_bytes()).unwrap();
        assert_eq!(
            paused.moved,
            [
                ("refs/heads/x".to_string(), "aaa".to_string()),
                ("refs/heads/y".to_string(), "bbb".to_string()),
            ]
        );
    }

    #[test]
    fn 标准输入先写说明的字节数和说明_再写清单() {
        let input = input(
            &[
                item("a1", Action::Reword("一\r\n二\n\n".into())),
                item("b2", Action::Pick),
                item("c3", Action::Reword("x".into())),
            ],
            &[],
        );
        let text = String::from_utf8(input).unwrap();
        // 「一\n二\n」是 8 个字节（汉字 3 个字节）；去掉了 \r 和末尾的空行，末尾补一个换行。
        // 说明后面一行是要拆分的提交，这里没有，是空行
        assert!(
            text.starts_with("8 2\n一\n二\nx\n\npick a1\nexec "),
            "{text}"
        );
        assert!(text.ends_with("fr-msg-2)\"\n"), "{text}");
        let split = super::input(&[item("a1", Action::Split), item("b2", Action::Split)], &[]);
        assert_eq!(
            String::from_utf8(split).unwrap(),
            "\na1 b2\nedit a1\nedit b2\n"
        );
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
        let (running, _) =
            crate::exec::spawn(&dir, &args(&base), Some(input(&items, &[]))).unwrap();
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
        let (running, _) =
            crate::exec::spawn(&dir, &args(&base), Some(input(&items, &[]))).unwrap();
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

    /// 真的跑一遍：一个提交停下修改，一个拆成两个，别的分支跟着移动；每次停下都读得出停在哪。
    #[test]
    fn 停下修改_拆分_一并移动分支() {
        let dir = std::env::temp_dir().join(format!("fr-rebase-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| crate::git::run(&dir, args).unwrap();
        let text = |args: &[&str]| String::from_utf8(git(args)).unwrap().trim().to_string();
        let run = |args: Vec<String>, stdin: Option<Vec<u8>>| {
            let (running, _) = crate::exec::spawn(&dir, &args, stdin).unwrap();
            running.collect().unwrap()
        };
        let state = || parse_state(&run(state_args(), None).0);
        let own = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "core.autocrlf", "false"]);
        let commit = |files: &[(&str, &str)], message: &str| {
            for (name, content) in files {
                std::fs::write(dir.join(name), content).unwrap();
            }
            git(&["add", "-A"]);
            git(&["commit", "-q", "-m", message]);
            text(&["rev-parse", "HEAD"])
        };
        let base = commit(&[("base.txt", "base\n")], "base");
        let a = commit(&[("a.txt", "a\n")], "a");
        // b 改一个已有的文件、加一个新文件：拆成两个提交
        let b = commit(&[("base.txt", "base\nb\n"), ("b.txt", "b\n")], "b");
        let c = commit(&[("c.txt", "c\n")], "c");
        git(&["branch", "mid", &b]);
        assert_eq!(state(), None);

        let items = [
            item(&a, Action::Edit),
            item(&b, Action::Split),
            item(&c, Action::Pick),
        ];
        let moved = [(b.clone(), "refs/heads/mid".to_string())];
        let (_, finished) = run(args(&base), Some(input(&items, &moved)));
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);

        // 停在 a 上：等着修改
        let paused = state().expect("停在 a 上");
        assert!(paused.curating, "整理提交起的变基认得出来");
        assert_eq!(paused.amend.as_deref(), Some(a.as_str()));
        assert_eq!(paused.stopped.as_deref(), Some(a.as_str()));
        assert_eq!(paused.step, Some((1, 4)));
        assert_eq!(paused.branch.as_deref(), Some("refs/heads/main"));
        assert_eq!(paused.orig_head.as_deref(), Some(c.as_str()));
        assert_eq!(paused.split, [b.as_str()]);
        assert!(!paused.splitting());
        // 一并移动的分支和它原来在哪（撤销时退回去用）
        assert_eq!(paused.moved, [("refs/heads/mid".to_string(), b.clone())]);
        // 改了暂存，继续时并进 a
        std::fs::write(dir.join("a.txt"), "a\nmore\n").unwrap();
        git(&["add", "-A"]);
        let (_, finished) = run(own(&["rebase", "--continue"]), None);
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);

        // 停在 b 上：要拆分，改动还没放回工作区
        let paused = state().expect("停在 b 上");
        assert!(paused.splitting() && paused.split_pending(), "{paused:?}");
        git(&["reset", "-q", "HEAD^"]);
        let paused = state().expect("改动放回去以后");
        assert!(paused.splitting() && !paused.split_pending());
        // 分两次提交
        git(&["add", "b.txt"]);
        git(&["commit", "-q", "-m", "b：新文件"]);
        // 还有没提交的改动时继续不了
        let (_, finished) = run(own(&["rebase", "--continue"]), None);
        assert_ne!(finished.code, Some(0));
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "b：改已有的文件"]);
        let (_, finished) = run(own(&["rebase", "--continue"]), None);
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        assert_eq!(state(), None);

        let log = text(&["log", "--format=%s", "main"]);
        assert_eq!(
            log.lines().collect::<Vec<_>>(),
            ["c", "b：改已有的文件", "b：新文件", "a", "base"]
        );
        assert_eq!(text(&["show", "main~3:a.txt"]), "a\nmore");
        // mid 原来指着 b，现在落在 b 拆出来的最后一个提交上
        assert_eq!(text(&["rev-parse", "mid"]), text(&["rev-parse", "main~1"]));

        // 终端里起的 `git rebase -i`（编辑器是别的）停下：不算整理
        let edit_first = "sequence.editor=sed -i 1s/^pick/edit/";
        let (_, finished) = run(own(&["-c", edit_first, "rebase", "-i", &base]), None);
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        let paused = state().expect("停在第一个提交上");
        assert!(paused.amend.is_some() && !paused.curating, "{paused:?}");
        let (_, finished) = run(own(&["rebase", "--abort"]), None);
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
