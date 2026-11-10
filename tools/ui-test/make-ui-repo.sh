#!/usr/bin/env bash
# 建一个界面测试用的仓库：有改动、未跟踪、删除、已暂存的文件，有分支、标签、贮藏、远程，main 领先远程 1 个提交。
# 用法：make-ui-repo.sh <目录>，会先删掉这个目录。远程是同目录下的裸仓库 origin.git，推送、拉取都在本地。
# 服务器上：ssh <目标> 'bash -s -- <目录>' < make-ui-repo.sh
set -euo pipefail
root="$1"
rm -rf "$root"
mkdir -p "$root"
git init -q --bare "$root/origin.git"
# 裸仓库的 HEAD 默认指向 master，改成 main，别处克隆时才有东西可检出
git -C "$root/origin.git" symbolic-ref HEAD refs/heads/main
repo="$root/ui-repo"
git init -q -b main "$repo"
cd "$repo"
git config user.name "测试"
git config user.email "test@example.com"
git config core.autocrlf false
git remote add origin "$root/origin.git"

mkdir -p src docs
cat > src/main.rs <<'EOF'
use std::collections::HashMap;

/// 程序入口
fn main() {
    let mut counts = HashMap::new();
    for word in ["苹果", "香蕉", "苹果"] {
        *counts.entry(word).or_insert(0) += 1;
    }
    println!("{counts:?}");
}

fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn sub(a: i32, b: i32) -> i32 {
    a - b
}

fn mul(a: i32, b: i32) -> i32 {
    a * b
}

#[cfg(test)]
mod tests {
    #[test]
    fn 加法() {
        assert_eq!(super::add(1, 2), 3);
    }
}
EOF
printf '# 界面测试仓库\n\n用来看 fork-remote 的本地修改界面。\n' > README.md
printf '第一版说明\n第二行\n第三行\n' > docs/说明.md
printf 'old\n' > docs/old.txt
git add -A
git commit -q -m "初始提交"
git tag v0.1

git checkout -q -b feature/login
printf 'fn login() {}\n' > src/login.rs
git add -A
git commit -q -m "登录：加一个空函数"
git checkout -q main
git push -q origin main feature/login 2>/dev/null
git branch -q --set-upstream-to=origin/main main

printf '\n## 用法\n\n直接运行。\n' >> README.md
git commit -q -am "README 加用法"

# 贮藏一份改动
printf '贮藏里的改动\n' >> docs/说明.md
git stash push -q -m "试验中的改动"

# 工作区改动：main.rs 改两处，新文件，删除，已暂存
sed -i 's/    a + b/    a + b + 0 \/\/ 加法/' src/main.rs
sed -i 's/fn mul(a: i32, b: i32) -> i32 {/fn mul(a: i64, b: i64) -> i64 {/' src/main.rs
printf 'fn div(a: i32, b: i32) -> i32 {\n    a / b\n}\n' >> src/main.rs
printf '新文件第一行\n第二行\n第三行\n' > 新文件.txt
rm docs/old.txt
printf '第一版说明（已改）\n第二行\n第三行\n' > docs/说明.md
git add docs/说明.md
git status --short
