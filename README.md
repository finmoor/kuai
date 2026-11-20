# Kuai（筷子）

Windows 上的 Git 客户端，界面和操作方式接近 Fork，目标是完全替代 Fork（本机仓库和服务器上的仓库都能管理）。

与 Fork 相比有两点不同。一是可以打开服务器上的仓库：添加服务器时，会在服务器上安装类似 VS Code Server 的常驻程序 `kuaid`，通过 SSH 的 stdin/stdout 通信；不需要 root 权限，不开放网络端口，服务器也无需联网。服务器上仓库的 git 命令都在服务器上执行，Windows 端只接收结果。二是可以在同一个程序中用终端编程（1.5 起）：每个仓库分为 Git 和终端两页（1.7 起），在终端页中运行 Claude Code、Codex 等工具，再切换到 Git 页查看改动、按行暂存；服务器上的终端运行在 `kuaid` 中，关闭 Kuai 或断线后仍会继续运行；本机的终端也是（1.11 起，运行在本机的 `kuaid` 中）。服务器可以是 Linux，也可以是开启了 OpenSSH 服务的 Windows 电脑（1.11 起）。服务器上的目录还可以映射成本机的盘符或文件夹，用本机的程序直接打开（1.13 起，需要安装 WinFsp）。

> 名字 2026-11-11 定的：不用叉子（Fork）改用筷子，「筷」也谐音「快」。在这之前暂时叫 fork-remote，仓库目录、crate 前缀 `fr-`、环境变量 `FR_*` 还是那时候的，当内部代号留着。

## 下载

从 [GitHub Releases](https://github.com/finmoor/kuai/releases) 下载，以下两种任选其一：

- **安装版** `kuai-<版本>-windows-x64-setup.exe`（推荐）：安装在用户目录下（`%LOCALAPPDATA%\Programs\Kuai`），无需管理员权限；可从开始菜单启动，可在「设置 → 应用」中卸载。
- **解压版** `kuai-<版本>-windows-x64.zip`：解压得到的 Kuai 文件夹可放在任意位置，双击 `kuai.exe` 运行。

运行要求：64 位 Windows 10 或 11，PATH 中有 git，使用 Windows 自带的 OpenSSH；服务器为 Linux（x86_64）或开启了 OpenSSH 服务的 Windows，并已安装 git，无需 root 权限。将服务器目录映射到本机时还需安装 [WinFsp](https://winfsp.dev/)，未安装时 Kuai 可经 winget 代为安装。程序未进行代码签名，首次运行时 Windows 可能提示「Windows 已保护你的电脑」，请点击「更多信息」，再点击「仍要运行」。

有新版本时，标题栏齿轮左侧会显示更新提示，点击可查看更新内容并一键更新（安装版在后台运行新的安装程序，解压版直接替换文件）；也可在「设置 → 关于」中检查更新。服务器上的 kuaid 无需单独更新，新版 Kuai 连接时会自动替换。

## 技术栈

- 客户端：Rust + GPUI，经 [gpui-kit](https://github.com/longbridge/gpui-kit) 0.9 引入（gpui-pre 快照、gpui-base、gpui-component）。
- 服务器上的 `kuaid`：Rust；Linux 版用 musl 静态编译，从 Windows 交叉编译；Windows 版（1.11 起）和客户端一起编。1.5 以前叫 fr-agent，那时不常驻。
- 本机仓库在进程内直接调用核心代码；服务器上的仓库经 SSH 交给 kuaid。两边用同一套 `fr-core`。本机仓库的终端（1.11 起）开在这台电脑自己的 kuaid 里。
- git 操作调用命令行，密码输入走 askpass 助手，和 Fork 的做法一样；高频只读操作以后可以换成 gitoxide。
- 选这套栈是为了性能。GPUI 还没到 1.0，所以先做了 M0 技术验证，已经通过。

## 目录

| 路径 | 内容 |
|---|---|
| `crates/fr-core` | 调用 git、解析输出、计算提交图布局、通信协议、界面语言（`i18n.rs`，英文译文在 `locales/en.json`）；不依赖界面，客户端和 kuaid 共用 |
| `crates/fr-channel` | 仓库通道：本机仓库在进程内执行 git，服务器上的仓库经 ssh 交给 kuaid（连接复用、自动安装 kuaid、终端接口）；测试工具 `fr-probe` |
| `crates/fr-app` | GPUI 客户端 |
| `crates/kuaid` | 常驻程序 `kuaid`（Linux、Windows）：执行 git、托管终端会话；装在服务器上，Windows 电脑自己也用它开终端 |
| `crates/fr-mount` | 把服务器上的目录映射成本机的盘符或文件夹：WinFsp 的薄绑定（运行时加载它的 DLL）和文件系统，读写变成 kuaid 的 `fs_*` 请求 |
| `tools/gen-test-repo.py` | 生成性能测试用的大仓库 |
| `tools/ui-test/` | 界面测试脚本：建测试仓库、截窗口、往测试窗口投递点击和输入（不动真实鼠标） |
| `tools/package.ps1` | 打发布包：客户端加 Linux 版、Windows 版 kuaid，出安装程序和解压版的 zip，输出到 `dist/` |
| `tools/installer/` | 安装程序的脚本（Inno Setup 6，`kuai.iss`）和简体中文的界面文字 |
| `tools/release-notes.ps1`、`CHANGELOG.md` | 发版时的发布说明：取 `CHANGELOG.md` 里这个版本的那一节 |
| `.github/workflows/` | 推代码时跑测试（`ci.yml`，kuaid 常驻那一套在 Windows、Linux 上各跑各的）；推 `v*` 标签时打包发版（`release.yml`） |
| `tools/make-icon.py` | 画应用图标，输出 `crates/fr-app/assets/icon.ico` |

## 快捷键

| 按键 | 作用 |
|---|---|
| F5 | 刷新当前仓库的状态和提交历史 |
| Ctrl+Enter | 提交（焦点位于提交说明框、文件列表或改动内容中时均可） |
| Ctrl+1、Ctrl+2 | 切换到 Git 页的本地修改、所有提交，并将键盘焦点移到该处 |
| Ctrl+F | 在当前页中查找：Git 页中搜索提交（正在看改动时在改动中搜索），文件页中查找打开的文件，合并编辑器中查找文本 |
| Ctrl+P | 快速打开仓库（上下方向键选择，Enter 打开） |
| Ctrl+, | 打开或关闭设置页 |
| Ctrl+W | 关闭当前标签页（有操作正在执行时会先提示）；在文件页中先关闭当前打开的文件 |
| Ctrl+Tab、Ctrl+Shift+Tab | 切换到下一个、上一个标签页 |
| Ctrl+Shift+T | 重新打开最近关闭的标签页 |
| Ctrl+` | 切换到当前仓库的终端页；已在终端页时回到之前的那一页（Git 页或文件页） |
| Ctrl+Shift+` | 为当前仓库新建终端，并切换到终端页 |
| Ctrl+Shift+B | 收起或展开左侧栏（收起后显示为一列图标，所有标签页共用此状态） |
| Ctrl+Shift+E | 切换到当前仓库的文件页 |
| Ctrl+Shift+A | 切换到「终端」页（所有工作区中各仓库的终端） |
| Alt+← | 在子仓库或工作树中时，返回根仓库的子仓库总览或工作树总览（焦点在终端、输入框中时不生效） |
| Ctrl+S | 保存文件页中当前打开的文件 |

列表中的按键（先点击列表，或用 Ctrl+1、Ctrl+2 将焦点移到列表）：

| 位置 | 按键 |
|---|---|
| 本地修改的文件列表 | 上下方向键切换文件，Shift+上下方向键连续选择，Ctrl+A 全选该栏；空格键或 Enter 暂存、取消暂存；Delete 丢弃（会先确认）；右方向键进入改动内容。按住 Ctrl 或 Shift 点击也可多选 |
| 改动内容 | 上下方向键在改动行之间移动，Shift+上下方向键连续选择；空格键或 Enter 暂存、取消暂存所选行；Delete 丢弃所选行（会先确认）；左方向键返回文件列表。并排显示时改为在行号上拖动以选择行，空格键和 Delete 的作用不变 |
| 提交列表 | 上下键、PageUp、PageDown、Home、End |
| 侧栏 | 上下方向键切换选中项，Enter 相当于双击（切换分支）；选中文件夹时左方向键折叠、右方向键展开 |
| 逐行追溯 | 上下方向键、PageUp、PageDown、Home、End 选择行，Enter 跳转到修改该行的提交，Esc 关闭 |
| 文件页的目录树 | 上下方向键切换，左右方向键折叠、展开目录，Enter 打开文件并保留，空格键预览；F2 重命名，Delete 删除（会先确认） |

焦点位于终端中时，只有以下快捷键仍由 Kuai 处理：Ctrl+Tab、Ctrl+Shift+Tab、Ctrl+Shift+T、Ctrl+1、Ctrl+2、Ctrl+Shift+E、Ctrl+\`、Ctrl+Shift+\`、Ctrl+,、Ctrl+Shift+B、Ctrl+Shift+A。其余按键（Tab、Shift+Tab、Esc、F5、Ctrl+W、Ctrl+F、Ctrl+P、Ctrl+S、功能键、Alt 组合键等）全部交给终端中的程序。终端中：Ctrl+C 中断（选中文字时为复制），Ctrl+Shift+C 复制，Ctrl+V、Shift+Insert 粘贴（剪贴板中为图片或文件时，粘贴的是其路径）；右键在有选中文字时复制，否则粘贴；Shift+PageUp、Shift+PageDown 和鼠标滚轮可查看之前的输出；终端中的程序使用鼠标时（例如 Claude Code），滚轮交给程序，按住 Shift 滚动则查看终端的记录。

## 构建与运行

需要 Rust（stable，MSVC 工具链）和 Visual Studio 生成工具（「使用 C++ 的桌面开发」：MSVC 和 Windows SDK）。第一次编译约 5 分钟。

```bash
# Linux 版 kuaid：连服务器时客户端从 target/x86_64-unknown-linux-musl/release/ 取它上传
# （先 rustup target add x86_64-unknown-linux-musl）
cargo build-kuaid

# 客户端；给了目录就打开（不是仓库先问是否初始化）。同一个设置文件只运行一个 Kuai，
# 已经在运行时交给运行着的那个，测试时用 FR_SETTINGS 换一个设置文件
cargo run --release -p fr-app -- [目录]

# 测通道：本机仓库，或者服务器上的仓库（需要时自动上传 kuaid）
cargo run --release -p fr-channel --bin fr-probe -- --repo .testdata/big-repo local
MSYS_NO_PATHCONV=1 cargo run --release -p fr-channel --bin fr-probe -- --repo <服务器上的仓库路径> <ssh 目标>

# 测服务器上的终端：开（或者接上）这个仓库的终端敲一条命令、列出终端会话、结束它
cargo run --release -p fr-channel --bin fr-probe -- --repo <服务器上的仓库路径> <ssh 目标> --term "echo hi"
cargo run --release -p fr-channel --bin fr-probe -- --repo <服务器上的仓库路径> <ssh 目标> --term-list

# 单元测试
cargo test --workspace

# kuaid 常驻那一套（拉起、终端断开后还在、升级不断终端）两个系统各有一份实现、各测各的。
# Windows 版：上面的 cargo test --workspace 里就有（crates/kuaid/tests/windows.rs，拿真的 kuaid.exe 测）。
# Linux 版：交叉编译出测试程序，连同 kuaid 拷到 Linux 机器上跑，见 crates/kuaid/tests/linux.rs 开头；
# GitHub Actions 的 ci.yml 在 ubuntu 上直接跑
cargo test --no-run --release -p kuaid --target x86_64-unknown-linux-musl

# Windows 版 kuaid：连 Windows 服务器、开本机终端时，客户端从自己旁边（target/release/kuaid.exe）取它
cargo build --release -p kuaid

# 打发布包：dist/kuai-<版本>-windows-x64-setup.exe（安装程序）和 dist/kuai-<版本>-windows-x64.zip
# （解压出来是一个 Kuai 文件夹）。要 Inno Setup 6（winget install JRSoftware.InnoSetup --scope user）；
# 包里的第三方库许可证清单由 tools/third-party-licenses.py 生成，要 Python 3
pwsh tools/package.ps1
```

发版：改 `Cargo.toml` 的版本号，在 `CHANGELOG.md` 加上这个版本的一节（每条一句话，会显示在 Kuai 的更新提示中），提交后打标签 `v<版本>` 推上去，`release.yml` 测试、打包、建 GitHub Release。Kuai 检查更新时只认 `kuai-*-windows-x64-setup.exe`（安装版用）和 `kuai-*-windows-x64.zip`（解压版用）这两个附件。

设置存在 `%APPDATA%\kuai\settings.json`；环境变量 `FR_SETTINGS` 可以换成别的文件，测试时不碰自己的设置（开机自启写的是注册表，测试时用 `FR_AUTOSTART_KEY` 换一个键）。

界面上新加一句话：源码里直接写中文、包上 `tr("…")`（带变量用 `tf!`），再到 `crates/fr-core/locales/en.json` 里补英文；没补的话 `cargo test -p fr-core` 会把缺的列出来。在 Git Bash 里给 Windows 程序传 `/data/...` 这类路径会被改写，所以上面测服务器那条要加 `MSYS_NO_PATHCONV=1`。

## 许可证

MIT 或 Apache-2.0，任选其一，见 [LICENSE-MIT](LICENSE-MIT)、[LICENSE-APACHE](LICENSE-APACHE)。界面图标是 [Lucide](https://lucide.dev)（ISC 许可），经 gpui-kit 带进来。用到的第三方库和它们的许可证在发布包的 `THIRD-PARTY-LICENSES.txt` 里。
