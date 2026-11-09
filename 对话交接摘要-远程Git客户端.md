# 对话交接摘要：Windows 上操作 Linux 项目的 Git

## 用户背景与需求
- 主力工作机是 **Windows**，想把项目放在 **Linux** 上开发（命令和构建都在 Linux 上执行）。
- 希望在 Windows 上用图形界面查看和操作 Linux 上项目的 Git：分支、提交历史、diff，**包括未提交的修改**。
- 偏好 **Fork** 这类原生 Git 图形客户端的体验。

## 已讨论的结论

### 1. 现成方案对比
| 方案 | 能否看到未提交的修改 | 评价 |
|---|---|---|
| VS Code Remote-SSH + Git Graph / GitLens | 能（直接操作 Linux 上的工作区） | **最推荐**，体验最好 |
| JetBrains Gateway | 能 | 同上，适合 JetBrains 用户 |
| clone 一份到 Windows，用 Fork、Sourcetree 等 | **不能**，只能看到已 commit 并 push 或 fetch 过来的内容；stash 也看不到 | 只适合看历史和做代码审查 |
| Samba 或 SSHFS-Win 挂成网络盘，再用 Fork 打开 | 能 | 大仓库很慢，会遇到 `safe.directory`、权限位、换行符、`index.lock` 等问题，只建议用来查看 |
| X11 转发或 xrdp，在 Linux 上运行 GitKraken、Sublime Merge、SmartGit 等 | 能 | 画面有延迟；Fork 没有 Linux 版 |
| SSH 登录后用 lazygit 或 tig | 能 | 终端界面，轻量、响应快 |

关键事实：**Fork、Sourcetree、GitKraken、TortoiseGit 这类 Windows 客户端都不支持通过 SSH 直接操作远程工作区**，只能打开本地磁盘（或网络盘）上的仓库。

### 2. 自研“远程 Git 客户端”的可行性分析（用户最后关心的问题）
目标：做一个类似 VS Code Remote-SSH 的工具，Linux 上运行 agent，Windows 上是原生图形客户端，所有 git 命令都在 Linux 上执行。

**结论：技术上完全可行**（VS Code Remote-SSH、JetBrains Gateway、Zed 都是同样的架构），主要是工程量问题。

**架构**
- Windows 图形客户端通过 SSH 连接，在 stdin/stdout 上用 JSON-RPC 或 msgpack 和 Linux 上的 agent 通信。
- 启动流程：SSH 登录 → 用 `uname -m` 检测架构 → 检查 `~/.yourtool/agent` 的版本，缺失就上传 → 执行 `agent --stdio`。
- Linux 上不用开新端口，认证和加密都复用 SSH。
- 比网络盘快的原因：git 在 Linux 本地执行，网络上只传结果。

**技术选型建议**
- agent：**Go**（静态单文件，交叉编译方便，没有 glibc 兼容问题），也可以用 Rust 加 musl。
- 执行 git：**调用 git 命令行**，用 `--porcelain=v2 -z` 等机器可读格式，不用 libgit2 或 go-git（它们功能不全，对 hooks、凭据、LFS 的支持差）。
- 客户端：Tauri（体积小）或 Electron（生态全）。分支图可以参考 Git Graph 插件的布局算法。
- 协议分三类消息：请求/响应、流式进度（fetch、push）、事件推送（inotify 检测到文件变化后推送，防抖 200–500ms）。
- 性能：log 分页懒加载，diff 只在点开时请求；大仓库开 `core.untrackedCache` 和 fsmonitor。

**难点**
1. 交互式认证：push 时需要输入 SSH key 密码、HTTPS 凭据或 GPG 密码。要用 `GIT_ASKPASS` 和 `SSH_ASKPASS` 把请求转到客户端弹窗，或者用 SSH agent forwarding。
2. 需要编辑器的操作（`rebase -i`、写 commit message、解决冲突）：要接管 `GIT_EDITOR` 和 `GIT_SEQUENCE_EDITOR`。
3. 断线重连：可以每次连接各起一个 agent，断开后重新查询状态；也可以让 agent 常驻并保留会话。
4. 和用户在终端里同时执行 git：要处理 `index.lock`，重试并提示。
5. 兼容不同发行版、CPU 架构（x86_64/arm64）和 git 版本，用静态编译。
6. 非 UTF-8 路径、二进制文件、超大文件。
7. 防止范围膨胀：**只做 Git，不做远程编辑文件**。

**推荐的实施路径**
- **第一阶段：先不用 agent。** 客户端用 SSH 库（russh、x/crypto/ssh 或 ssh2）保持一条长连接，每个操作开一个 channel 执行 `cd repo && git ... --porcelain`，再解析输出；刷新靠手动或轮询。大约 2–4 周能做出原型。
- **第二阶段：加 agent。** 加入文件监听、fetch/push 进度、认证弹窗、断线重连，再需要 1–2 个月。
- **第三阶段：接近 Fork 的水平。** 交互式 rebase、冲突解决、按行暂存、stash、子模块等，需要半年以上。

**取舍判断**
- 只是自己用：不值得自己做，VS Code Remote-SSH 加 Git Graph 或 GitLens 再加 lazygit 已经覆盖大约九成需求。
- 想做成产品或开源项目：有一定空白，目前缺一个“原生、轻量、支持 SSH 远程仓库、体验像 Fork”的 Git 客户端。

## 未完成或可继续的方向
- 用户还没回答：Linux 是本机虚拟机、局域网服务器还是云服务器；项目有多大。
- 我主动提议过，用户还没确认：写一个不用 agent 的最小原型（Go 或 Tauri），通过 SSH 拉取远程仓库的 status 和 log，并显示分支图。
- 也可以继续：协议的详细设计（消息格式、方法列表）、Git Graph 分支图布局算法、ASKPASS 转发的具体实现。
