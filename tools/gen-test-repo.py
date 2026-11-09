#!/usr/bin/env python3
"""生成用于性能测试的大型 git 仓库，经 git fast-import 一次灌入。

提交历史模拟日常开发：主线上持续有提交，同时有多个功能分支开出、提交、
偶尔把主线合并进来，最后合并回主线；隔一段打一个标签；远程分支比本地落后一点。
提交信息中英文混排，用来检查中文显示。

指定 --big-file 时，在最新的几十个提交里加入这个大文件，再提交一个改了几百处的版本，
用来测试大 diff 的显示。

用法：
    python tools/gen-test-repo.py .testdata/big-repo --commits 200000 --big-file <某个大源文件>
"""

import argparse
import os
import random
import subprocess
import sys
import time

AUTHORS = [
    ("张伟", "zhangwei@example.com"),
    ("王芳", "wangfang@example.com"),
    ("李娜", "lina@example.com"),
    ("刘洋", "liuyang@example.com"),
    ("陈静", "chenjing@example.com"),
    ("Alice Chen", "alice@example.com"),
    ("Bob Smith", "bob@example.com"),
    ("Carol Wu", "carol@example.com"),
    ("Dave Li", "dave@example.com"),
]
TOPICS = [
    "登录页", "订单列表", "库存同步", "权限校验", "导出报表", "支付回调", "缓存层",
    "日志模块", "配置加载", "搜索", "上传组件", "消息推送",
    "parser", "renderer", "scheduler", "network layer", "settings", "sidebar",
]
TEMPLATES = [
    "修复{t}在空数据时崩溃的问题",
    "新增{t}的分页支持",
    "优化{t}的性能，减少重复查询",
    "重构{t}，拆成更小的函数",
    "补充{t}的单元测试",
    "调整{t}的样式和间距",
    "Fix race condition in {t}",
    "Add retry logic to {t}",
    "Refactor {t} for readability",
    "Update docs for {t}",
    "Handle edge case in {t} when input is empty",
]
SLUGS = [
    "login", "orders", "inventory", "auth", "export", "payment", "cache", "logging",
    "config", "search", "upload", "push", "parser", "renderer", "scheduler", "net",
]
FILE_COUNT = 300
START_TIME = 1577836800  # 2020-01-01 00:00:00 UTC


class Writer:
    """把 fast-import 命令写进管道。"""

    def __init__(self, stream):
        self.stream = stream

    def line(self, text):
        self.stream.write(text.encode("utf-8"))
        self.stream.write(b"\n")

    def data(self, text):
        raw = text.encode("utf-8")
        self.stream.write(b"data %d\n" % len(raw))
        self.stream.write(raw)
        self.stream.write(b"\n")


def modify_big_file(lines, rnd):
    """在大约 3% 的行上做改动：改写、删除或在后面插入一行。"""
    out = []
    for i, line in enumerate(lines):
        r = rnd.random()
        if r < 0.01:
            continue  # 删除
        if r < 0.02:
            out.append(line + "  // 调整 #%d" % i)
            continue
        out.append(line)
        if r < 0.03:
            indent = line[: len(line) - len(line.lstrip())]
            out.append(indent + "// 新增说明：这里的行为在 #%d 改过" % i)
    return out


def main():
    # Windows 控制台默认编码不是 UTF-8，固定输出编码，免得中文乱码
    sys.stdout.reconfigure(encoding="utf-8")
    ap = argparse.ArgumentParser(description="生成用于性能测试的大型 git 仓库")
    ap.add_argument("dest", help="目标目录，必须不存在或为空")
    ap.add_argument("--commits", type=int, default=200_000)
    ap.add_argument("--max-branches", type=int, default=12, help="同时活跃的功能分支数上限")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--big-file", help="大文件素材；不给就不生成大 diff 的提交")
    args = ap.parse_args()

    if os.path.exists(args.dest) and os.listdir(args.dest):
        sys.exit(f"目标目录不是空的：{args.dest}")
    os.makedirs(args.dest, exist_ok=True)
    subprocess.run(["git", "init", "-q", "-b", "main", args.dest], check=True)

    big_lines = None
    if args.big_file:
        with open(args.big_file, encoding="utf-8") as f:
            big_lines = f.read().splitlines()

    rnd = random.Random(args.seed)
    started = time.time()
    proc = subprocess.Popen(
        ["git", "-C", args.dest, "fast-import", "--quiet", "--force", "--done"],
        stdin=subprocess.PIPE,
    )
    w = Writer(proc.stdin)

    total = args.commits
    big_add_at = total - 40 if big_lines else -1
    big_modify_at = total - 15 if big_lines else -1
    mark = 0
    ts = START_TIME
    main_tip = None
    branches = []  # 每项：{"name", "tip", "left"}
    branch_seq = 0
    tags = []
    main_history = []  # 主线提交的 mark，用来给远程分支挑一个落后的位置

    def commit(ref, parents, message, files):
        nonlocal mark, ts
        mark += 1
        ts += rnd.randint(30, 900)
        name, email = rnd.choice(AUTHORS)
        w.line(f"commit {ref}")
        w.line(f"mark :{mark}")
        w.line(f"author {name} <{email}> {ts} +0800")
        w.line(f"committer {name} <{email}> {ts} +0800")
        w.data(message)
        if parents:
            w.line(f"from :{parents[0]}")
            for p in parents[1:]:
                w.line(f"merge :{p}")
        for path, content in files:
            w.line(f"M 100644 inline {path}")
            w.data(content)
        w.line("")
        return mark

    def small_change():
        n = rnd.randrange(FILE_COUNT)
        path = f"src/{SLUGS[n % len(SLUGS)]}/file{n:03d}.txt"
        body = "".join(f"第 {i} 行 line {i}: {rnd.randrange(1_000_000)}\n" for i in range(rnd.randint(3, 12)))
        return [(path, body)]

    def message():
        return rnd.choice(TEMPLATES).format(t=rnd.choice(TOPICS))

    made = 0
    while made < total:
        if main_tip is None:
            main_tip = commit("refs/heads/main", [], "初始提交", small_change())
            main_history.append(main_tip)
        elif made == big_add_at:
            main_tip = commit("refs/heads/main", [main_tip], "加入大文件 big/window.rs",
                              [("big/window.rs", "\n".join(big_lines) + "\n")])
            main_history.append(main_tip)
        elif made == big_modify_at:
            changed = modify_big_file(big_lines, rnd)
            main_tip = commit("refs/heads/main", [main_tip], "大文件改动：big/window.rs 多处调整",
                              [("big/window.rs", "\n".join(changed) + "\n")])
            main_history.append(main_tip)
        else:
            r = rnd.random()
            if len(branches) < args.max_branches and r < 0.06:
                # 从主线开一个新的功能分支
                branch_seq += 1
                b = {"name": f"feature/{rnd.choice(SLUGS)}-{branch_seq}", "tip": main_tip,
                     "left": rnd.randint(2, 60), "files": {}}
                branches.append(b)
                change = small_change()
                b["files"].update(change)
                b["tip"] = commit("refs/tmp/work", [b["tip"]], message(), change)
                b["left"] -= 1
            elif branches and r < 0.70:
                b = rnd.choice(branches)
                if b["left"] <= 0:
                    # 功能分支合并回主线。fast-import 不会自己合并文件树，
                    # 把分支改过的文件写进合并提交，合并提交的 diff 才像真实仓库
                    main_tip = commit("refs/heads/main", [main_tip, b["tip"]],
                                      f"Merge branch '{b['name']}'", sorted(b["files"].items()))
                    main_history.append(main_tip)
                    branches.remove(b)
                elif rnd.random() < 0.03:
                    # 把主线合并进功能分支，同步最新代码
                    b["tip"] = commit("refs/tmp/work", [b["tip"], main_tip],
                                      f"Merge branch 'main' into {b['name']}", [])
                else:
                    change = small_change()
                    b["files"].update(change)
                    b["tip"] = commit("refs/tmp/work", [b["tip"]], message(), change)
                    b["left"] -= 1
            else:
                main_tip = commit("refs/heads/main", [main_tip], message(), small_change())
                main_history.append(main_tip)
                if len(main_history) % 3000 == 0:
                    tags.append((f"v{len(tags) // 10 + 1}.{len(tags) % 10}.0", main_tip))
        made += 1

    # 收尾：给还没合并的功能分支、标签、远程分支建引用
    for b in branches:
        w.line(f"reset refs/heads/{b['name']}")
        w.line(f"from :{b['tip']}")
        w.line("")
    for name, m in tags:
        w.line(f"reset refs/tags/{name}")
        w.line(f"from :{m}")
        w.line("")
    behind = main_history[max(0, len(main_history) - 30)]
    w.line("reset refs/remotes/origin/main")
    w.line(f"from :{behind}")
    w.line("")
    for b in branches[:3]:
        w.line(f"reset refs/remotes/origin/{b['name']}")
        w.line(f"from :{b['tip']}")
        w.line("")
    w.line("done")
    proc.stdin.close()
    if proc.wait() != 0:
        sys.exit("git fast-import 失败")

    git = ["git", "-C", args.dest]
    subprocess.run(git + ["update-ref", "-d", "refs/tmp/work"], check=True)
    subprocess.run(git + ["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"], check=True)
    subprocess.run(git + ["reset", "-q", "--hard", "main"], check=True)
    # gc 过的真实仓库一般都有 commit-graph 文件，git log 会快很多
    subprocess.run(git + ["commit-graph", "write", "--reachable"], check=True)

    print(f"生成完成：{made} 个提交，{len(tags)} 个标签，{len(branches)} 个未合并分支，"
          f"用时 {time.time() - started:.1f} 秒")


if __name__ == "__main__":
    main()
