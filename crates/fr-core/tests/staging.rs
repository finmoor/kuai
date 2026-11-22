//! 按行暂存、取消暂存、丢弃的集成测试：在临时仓库里用真的 git 生成 diff、应用部分补丁，再核对结果。

use std::collections::HashSet;
use std::path::PathBuf;

use fr_core::patch::{self, FileDiff, LineKind};
use fr_core::{exec, status};

struct Repo(PathBuf);

impl Repo {
    fn new(name: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!("fr-staging-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repo(dir);
        repo.git(&["init", "-q"]);
        repo.git(&["config", "user.name", "测试"]);
        repo.git(&["config", "user.email", "test@example.com"]);
        repo.git(&["config", "core.autocrlf", "false"]);
        repo
    }

    fn run(&self, args: Vec<String>, stdin: Option<Vec<u8>>, ok: &[i32]) -> Vec<u8> {
        let (running, _) = exec::spawn(&self.0, &args, stdin).unwrap();
        let (out, finished) = running.collect().unwrap();
        assert!(
            finished.code.is_some_and(|c| ok.contains(&c)),
            "git {args:?} 失败（{:?}）：{}",
            finished.code,
            finished.stderr
        );
        out
    }

    fn git(&self, args: &[&str]) -> Vec<u8> {
        self.run(args.iter().map(|s| s.to_string()).collect(), None, &[0])
    }

    fn write(&self, name: &str, content: &[u8]) {
        std::fs::write(self.0.join(name), content).unwrap();
    }

    fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.0.join(name)).unwrap()
    }

    fn commit(&self, name: &str, content: &[u8]) {
        self.write(name, content);
        self.git(&["add", "--", name]);
        self.git(&["commit", "-q", "-m", name]);
    }

    /// 暂存区里的文件内容
    fn index(&self, name: &str) -> Vec<u8> {
        self.git(&["show", &format!(":{name}")])
    }

    fn diff(&self, args: Vec<String>) -> FileDiff {
        let out = self.run(args, None, &[0, 1]);
        let mut files = patch::parse(&out);
        assert_eq!(files.len(), 1, "{}", String::from_utf8_lossy(&out));
        files.remove(0)
    }

    fn apply(&self, args: Vec<String>, patch: Vec<u8>) {
        self.run(args, Some(patch), &[0]);
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 选出内容满足条件的改动行。
fn pick(file: &FileDiff, want: impl Fn(LineKind, &[u8]) -> bool) -> HashSet<(usize, usize)> {
    let mut set = HashSet::new();
    for (hi, hunk) in file.hunks.iter().enumerate() {
        for (li, line) in hunk.lines.iter().enumerate() {
            if line.kind != LineKind::Context && want(line.kind, &line.content) {
                set.insert((hi, li));
            }
        }
    }
    assert!(!set.is_empty(), "没有选中任何行");
    set
}

fn partial(file: &FileDiff, reverse: bool, set: &HashSet<(usize, usize)>) -> Vec<u8> {
    file.partial(reverse, |h, l| set.contains(&(h, l))).unwrap()
}

#[test]
fn 按行暂存再按行取消暂存() {
    let repo = Repo::new("modify");
    repo.commit("a.txt", b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n");
    repo.write("a.txt", b"1\ntwo\n3\n4\n5\nnew\n6\n7\n8\n10\n");

    // 只暂存「2 改成 two」
    let file = repo.diff(patch::unstaged_args("a.txt"));
    let set = pick(&file, |_, c| c == b"2" || c == b"two");
    repo.apply(patch::apply_cached_args(false), partial(&file, false, &set));
    assert_eq!(repo.index("a.txt"), b"1\ntwo\n3\n4\n5\n6\n7\n8\n9\n10\n");

    // 工作区剩下的改动：+new 和 -9
    let st = status::parse(&repo.run(status::args(), None, &[0]));
    let entry = &st.entries[0];
    assert_eq!(
        (entry.staged, entry.unstaged),
        (Some(status::Kind::Modified), Some(status::Kind::Modified))
    );
    let rest = repo.diff(patch::unstaged_args("a.txt"));
    assert_eq!(rest.changed_lines(), 2);

    // 已暂存的改动里只取消「+two」：暂存区里 two 去掉，2 仍然算删掉
    let staged = repo.diff(patch::staged_args("a.txt", None));
    let set = pick(&staged, |k, c| k == LineKind::Added && c == b"two");
    repo.apply(patch::apply_cached_args(true), partial(&staged, true, &set));
    assert_eq!(repo.index("a.txt"), b"1\n3\n4\n5\n6\n7\n8\n9\n10\n");
}

#[test]
fn 按行丢弃工作区的改动() {
    let repo = Repo::new("discard");
    repo.commit("a.txt", b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n");
    repo.write("a.txt", b"1\n2\n3\n4\n5\nnew\n6\n7\n8\n10\n");
    let file = repo.diff(patch::unstaged_args("a.txt"));
    let set = pick(&file, |_, c| c == b"new");
    repo.apply(patch::discard_args(), partial(&file, true, &set));
    assert_eq!(repo.read("a.txt"), b"1\n2\n3\n4\n5\n6\n7\n8\n10\n");
    assert_eq!(repo.index("a.txt"), b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n");
}

#[test]
fn 未跟踪的文件按行暂存() {
    let repo = Repo::new("untracked");
    repo.commit("base.txt", b"base\n");
    repo.write("b.txt", b"x\ny\nz\n");
    let st = status::parse(&repo.run(status::args(), None, &[0]));
    assert_eq!(st.entries[0].unstaged, Some(status::Kind::Untracked));

    let file = repo.diff(patch::untracked_args("b.txt"));
    assert!(file.is_new());
    let set = pick(&file, |_, c| c == b"x" || c == b"y");
    repo.apply(patch::apply_cached_args(false), partial(&file, false, &set));
    assert_eq!(repo.index("b.txt"), b"x\ny\n");
}

#[test]
fn 删除的文件按行暂存() {
    let repo = Repo::new("deleted");
    repo.commit("c.txt", b"p\nq\nr\n");
    std::fs::remove_file(repo.0.join("c.txt")).unwrap();
    let file = repo.diff(patch::unstaged_args("c.txt"));
    assert!(file.is_deleted());
    let set = pick(&file, |_, c| c == b"q");
    repo.apply(patch::apply_cached_args(false), partial(&file, false, &set));
    assert_eq!(repo.index("c.txt"), b"p\nr\n");
}

#[test]
fn 已暂存删除的文件按行取消暂存() {
    let repo = Repo::new("deleted-staged");
    repo.commit("c.txt", b"p\nq\nr\n");
    repo.git(&["rm", "-q", "--cached", "--", "c.txt"]);
    let file = repo.diff(patch::staged_args("c.txt", None));
    assert!(file.is_deleted());
    // 取消「删掉 q」：暂存区里又有了这个文件，只剩 q（删掉 p、r 仍是暂存的）
    let set = pick(&file, |_, c| c == b"q");
    repo.apply(patch::apply_cached_args(true), partial(&file, true, &set));
    assert_eq!(repo.index("c.txt"), b"q\n");
}

#[test]
fn 新加的文件按行取消暂存() {
    let repo = Repo::new("new-file");
    repo.commit("base.txt", b"base\n");
    repo.write("g.txt", b"1\n2\n3\n");
    repo.git(&["add", "--", "g.txt"]);
    let file = repo.diff(patch::staged_args("g.txt", None));
    assert!(file.is_new());
    let set = pick(&file, |_, c| c == b"2");
    repo.apply(patch::apply_cached_args(true), partial(&file, true, &set));
    assert_eq!(repo.index("g.txt"), b"1\n3\n");
}

#[test]
fn 文件末尾没有换行() {
    let repo = Repo::new("no-newline");
    repo.commit("d.txt", b"a\nb");
    repo.write("d.txt", b"a\nb\nc\nd");
    let file = repo.diff(patch::unstaged_args("d.txt"));
    // b 那一行要连同「补上换行」一起选
    let set = pick(&file, |_, c| c == b"b" || c == b"c");
    repo.apply(patch::apply_cached_args(false), partial(&file, false, &set));
    assert_eq!(repo.index("d.txt"), b"a\nb\nc\n");
}

#[test]
fn 文件末尾没有换行_只暂存后面加的行() {
    let repo = Repo::new("no-newline-append");
    repo.commit("d.txt", b"a\nb");
    repo.write("d.txt", b"a\nb\nc\n");
    let file = repo.diff(patch::unstaged_args("d.txt"));
    // 只选 +c：b 要补上换行才能在后面加行，补丁自己带上
    let set = pick(&file, |k, c| k == LineKind::Added && c == b"c");
    repo.apply(patch::apply_cached_args(false), partial(&file, false, &set));
    assert_eq!(repo.index("d.txt"), b"a\nb\nc\n");
}

#[test]
fn 文件末尾没有换行_只丢弃一部分() {
    let repo = Repo::new("no-newline-discard");
    repo.commit("d.txt", b"a\nb");
    repo.write("d.txt", b"a\nc");
    let file = repo.diff(patch::unstaged_args("d.txt"));
    // 只丢弃「删掉 b」：b 回来，c 留着还是没有换行的最后一行
    let set = pick(&file, |k, c| k == LineKind::Removed && c == b"b");
    repo.apply(patch::discard_args(), partial(&file, true, &set));
    assert_eq!(repo.read("d.txt"), b"a\nb\nc");
}

#[test]
fn 按行取消暂存不撤销改名和权限() {
    let repo = Repo::new("rename-mode");
    repo.commit("old.txt", b"1\n2\n3\n4\n5\n6\n");
    repo.git(&["mv", "old.txt", "new.txt"]);
    repo.write("new.txt", b"1\n2\n3\n4\n5\n6\nseven\neight\n");
    repo.git(&["add", "--", "new.txt"]);
    repo.git(&["update-index", "--chmod=+x", "--", "new.txt"]);
    let file = repo.diff(patch::staged_args("new.txt", Some("old.txt")));
    assert!(file.is_moved(), "{:?}", file.header);
    let set = pick(&file, |k, c| k == LineKind::Added && c == b"eight");
    repo.apply(patch::apply_cached_args(true), partial(&file, true, &set));
    assert_eq!(repo.index("new.txt"), b"1\n2\n3\n4\n5\n6\nseven\n");
    let files = String::from_utf8(repo.git(&["ls-files", "-s"])).unwrap();
    assert!(
        files.lines().all(|line| !line.ends_with("old.txt")),
        "{files}"
    );
    assert!(
        files
            .lines()
            .any(|line| line.starts_with("100755 ") && line.ends_with("new.txt")),
        "{files}"
    );
}

#[test]
fn 非utf8和crlf原样保留() {
    let repo = Repo::new("bytes");
    // GBK 编码的「你好」，CRLF 换行
    repo.commit("e.txt", b"\xc4\xe3\xba\xc3\r\nline2\r\n");
    repo.write(
        "e.txt",
        b"\xc4\xe3\xba\xc3\r\n\xca\xc0\xbd\xe7\r\nline2\r\nmore\r\n",
    );
    let file = repo.diff(patch::unstaged_args("e.txt"));
    let set = pick(&file, |_, c| c == b"\xca\xc0\xbd\xe7\r");
    repo.apply(patch::apply_cached_args(false), partial(&file, false, &set));
    assert_eq!(
        repo.index("e.txt"),
        b"\xc4\xe3\xba\xc3\r\n\xca\xc0\xbd\xe7\r\nline2\r\n"
    );
}

#[test]
fn 文件名里有空格和通配符() {
    let repo = Repo::new("names");
    repo.commit("base.txt", b"base\n");
    // Windows 的文件名不能有 * 和 ?，用 [1] 试：当通配符时它会匹配「a 1.txt」
    repo.write("a [1].txt", b"x\n");
    repo.write("a 1.txt", b"y\n");
    repo.git(&["add", "--", "a [1].txt"]);
    let st = status::parse(&repo.run(status::args(), None, &[0]));
    let staged: Vec<_> = st.staged().map(|e| e.path.as_str()).collect();
    assert_eq!(staged, ["a [1].txt"]);
    let file = repo.diff(patch::staged_args("a [1].txt", None));
    assert_eq!(file.changed_lines(), 1);
}

/// 一串看着随机、每次都一样的数（xorshift）。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// 按行拆开，每行带着自己的换行符。
fn lines_of(text: &[u8]) -> Vec<Vec<u8>> {
    text.split_inclusive(|&byte| byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect()
}

/// 不经过补丁、直接按选中的行算出来的结果：`base` 是补丁要打到的那一边（暂存时是暂存区，
/// 反向时是 diff 的新的一边），`keep_unpicked` 是没选中的行留哪一种。
///
/// - 正向（暂存）：删除的行选中了才删，增加的行选中了才加；
/// - 反向（取消暂存、丢弃）：增加的行选中了才去掉，删除的行选中了才加回来。
fn expected(base: &[u8], file: &FileDiff, reverse: bool, set: &HashSet<(usize, usize)>) -> Vec<u8> {
    let base = lines_of(base);
    let mut out = Vec::new();
    let mut at = 0;
    for (hi, hunk) in file.hunks.iter().enumerate() {
        let (start, count) = if reverse {
            (hunk.new_start, hunk.new_count)
        } else {
            (hunk.old_start, hunk.old_count)
        };
        // 块前面没动的行
        let first = (start as usize).saturating_sub(1).max(at);
        out.extend(base[at..first].iter().flatten());
        for (li, line) in hunk.lines.iter().enumerate() {
            let picked = set.contains(&(hi, li));
            let mut bytes = line.content.clone();
            bytes.push(b'\n');
            match (line.kind, reverse) {
                (LineKind::Context, _) => out.extend(bytes),
                (LineKind::Removed, false) | (LineKind::Added, true) if !picked => {
                    out.extend(bytes)
                }
                (LineKind::Added, false) | (LineKind::Removed, true) if picked => out.extend(bytes),
                _ => {}
            }
        }
        at = first + count as usize;
    }
    out.extend(base[at..].iter().flatten());
    out
}

/// 随机挑一部分改动行（至少一行）。
fn random_pick(file: &FileDiff, rng: &mut Rng) -> HashSet<(usize, usize)> {
    let mut all = Vec::new();
    for (hi, hunk) in file.hunks.iter().enumerate() {
        for (li, line) in hunk.lines.iter().enumerate() {
            if line.kind != LineKind::Context {
                all.push((hi, li));
            }
        }
    }
    let mut set: HashSet<_> = all.iter().copied().filter(|_| rng.below(2) == 0).collect();
    if set.is_empty() {
        set.insert(all[rng.below(all.len() as u64) as usize]);
    }
    set
}

/// 一个随机的文件：行尾 LF、CRLF 混着，有的行重复（diff 对齐时容易错位）。
fn random_file(rng: &mut Rng, lines: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..lines {
        let word = match rng.below(10) {
            0 => "}".to_string(),
            1 => String::new(),
            _ => format!("line {}", rng.below(1000)),
        };
        out.extend(word.as_bytes());
        out.extend(if rng.below(4) == 0 {
            &b"\r\n"[..]
        } else {
            b"\n"
        });
    }
    out
}

/// 在 `base` 上随机删、改、插几行。
fn random_edit(rng: &mut Rng, base: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines_of(base) {
        match rng.below(12) {
            0 => {}
            1 => out.extend(format!("changed {}\n", rng.below(1000)).as_bytes()),
            2 => {
                out.extend(&line);
                out.extend(format!("inserted {}\r\n", rng.below(1000)).as_bytes());
            }
            _ => out.extend(&line),
        }
    }
    if out == base {
        out.extend(b"tail\n");
    }
    out
}

#[test]
fn 随机改动随机选行_暂存取消暂存丢弃的结果都和选中的行一致() {
    let repo = Repo::new("random");
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for round in 0..40 {
        let name = format!("r{round}.txt");
        let lines = 5 + rng.below(60) as usize;
        let head = random_file(&mut rng, lines);
        repo.commit(&name, &head);
        let work = random_edit(&mut rng, &head);
        repo.write(&name, &work);

        // 暂存一部分：暂存区按选中的行变，工作区不动
        let file = repo.diff(patch::unstaged_args(&name));
        let set = random_pick(&file, &mut rng);
        let want = expected(&repo.index(&name), &file, false, &set);
        repo.apply(patch::apply_cached_args(false), partial(&file, false, &set));
        assert_eq!(repo.index(&name), want, "第 {round} 轮暂存");
        assert_eq!(repo.read(&name), work, "第 {round} 轮暂存动了工作区");

        // 已暂存的再取消一部分
        let staged = repo.diff(patch::staged_args(&name, None));
        if staged.changed_lines() > 0 {
            let set = random_pick(&staged, &mut rng);
            let want = expected(&repo.index(&name), &staged, true, &set);
            repo.apply(patch::apply_cached_args(true), partial(&staged, true, &set));
            assert_eq!(repo.index(&name), want, "第 {round} 轮取消暂存");
            assert_eq!(repo.read(&name), work, "第 {round} 轮取消暂存动了工作区");
        }

        // 工作区里剩下的丢弃一部分：工作区按选中的行变，暂存区不动
        let index = repo.index(&name);
        let rest = repo.diff(patch::unstaged_args(&name));
        if rest.changed_lines() > 0 {
            let set = random_pick(&rest, &mut rng);
            let want = expected(&work, &rest, true, &set);
            repo.apply(patch::discard_args(), partial(&rest, true, &set));
            assert_eq!(repo.read(&name), want, "第 {round} 轮丢弃");
            assert_eq!(repo.index(&name), index, "第 {round} 轮丢弃动了暂存区");
        }
    }
}
