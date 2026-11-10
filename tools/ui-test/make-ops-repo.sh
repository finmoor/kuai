#!/usr/bin/env bash
# 建一个测合并、变基、挑拣、冲突用的仓库：<目录>/ops-repo，远程是同目录下的裸仓库 ops-origin.git。
#
#   main:  c1 ── c2 ── c3        c3 把 a.txt 第 2 行改成 main
#           └─── f1 ── f2  feat   f1 把 a.txt 第 2 行改成 feat（和 c3 冲突），f2 只加 c.txt
#   标签 v1 在 c1 上；main 和 feat 都推到了 origin。工作区是干净的。
#
# 在服务器上用：ssh <目标> 'bash -s -- <目录>' < make-ops-repo.sh
set -euo pipefail
root="${1:?用法：make-ops-repo.sh <目录>}"
mkdir -p "$root"
cd "$root"
rm -rf ops-repo ops-origin.git
git init -q --bare ops-origin.git
git -C ops-origin.git symbolic-ref HEAD refs/heads/main
git init -q -b main ops-repo
cd ops-repo
git config user.name 测试
git config user.email test@example.com
git config core.autocrlf false
git remote add origin ../ops-origin.git

printf '1\n2\n3\n' > a.txt
git add . && git commit -qm "c1：初始提交"
git tag v1
git checkout -q -b feat
printf '1\nfeat\n3\n' > a.txt
git commit -qam "f1：改 a.txt 第 2 行"
echo c > c.txt
git add . && git commit -qm "f2：加 c.txt"
git checkout -q main
echo b > b.txt
git add . && git commit -qm "c2：加 b.txt"
printf '1\nmain\n3\n' > a.txt
git commit -qam "c3：改 a.txt 第 2 行"
git push -q -u origin main feat
echo "建好了：$root/ops-repo"
git log --oneline --graph --all
