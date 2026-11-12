# Kuai（筷子）

Windows 上界面和操作习惯接近 Fork 的 Git 客户端，最终目标是彻底替代 Fork（本机仓库也管）。

和 Fork 的区别有两处。一是能打开服务器上的仓库：「添加服务器」时在服务器上装一个类似 VS Code Server 的常驻程序 `kuaid`，经 SSH 的 stdin/stdout 通信；不要 root、不开网络端口、服务器不用联网。服务器上仓库的 git 命令都在服务器上执行，Windows 这边只收结果。二是在同一个程序里用终端写代码（1.5 起的方向）：每个仓库分 Git、终端两页（1.7 起），终端页里跑 Claude Code、Codex 这类工具，切到 Git 页看它改了什么、按行挑着收；服务器上的终端开在 `kuaid` 里，关了 Kuai、断了线它都接着跑。

> 名字 2026-11-11 定的：不用叉子（Fork）改用筷子，「筷」也谐音「快」。在这之前暂时叫 fork-remote，仓库目录、crate 前缀 `fr-`、环境变量 `FR_*` 还是那时候的，当内部代号留着。

## 技术栈

- 客户端：Rust + GPUI，经 [gpui-kit](https://github.com/longbridge/gpui-kit) 0.9 引入（gpui-pre 快照、gpui-base、gpui-component）。
- 服务器上的 `kuaid`：Rust；Linux 版用 musl 静态编译，从 Windows 交叉编译。1.5 以前叫 fr-agent，那时不常驻。
- 本机仓库在进程内直接调用核心代码；服务器上的仓库经 SSH 交给 kuaid。两边用同一套 `fr-core`。
- git 操作调用命令行，密码输入走 askpass 助手，和 Fork 的做法一样；高频只读操作以后可以换成 gitoxide。
- 选这套栈是为了性能。GPUI 还没到 1.0，所以先做了 M0 技术验证，已经通过。

## 目录

| 路径 | 内容 |
|---|---|
| `crates/fr-core` | 调用 git、解析输出、计算提交图布局、通信协议、界面语言（`i18n.rs`，英文译文在 `locales/en.json`）；不依赖界面，客户端和 kuaid 共用 |
| `crates/fr-channel` | 仓库通道：本机仓库在进程内执行 git，服务器上的仓库经 ssh 交给 kuaid（连接复用、自动安装 kuaid、终端接口）；测试工具 `fr-probe` |
| `crates/fr-app` | GPUI 客户端 |
| `crates/kuaid` | 服务器上的常驻程序 `kuaid`：执行 git、托管终端会话 |
| `tools/gen-test-repo.py` | 生成性能测试用的大仓库 |
| `tools/ui-test/` | 界面测试脚本：建测试仓库、截窗口、往测试窗口投递点击和输入（不动真实鼠标） |
| `tools/package.ps1` | 打发布包：客户端加 Linux 版 kuaid，输出到 `dist/` |
| `tools/make-icon.py` | 画应用图标，输出 `crates/fr-app/assets/icon.ico` |

## 快捷键

| 按键 | 作用 |
|---|---|
| F5 | 重读当前仓库的状态和提交历史 |
| Ctrl+Enter | 提交（焦点在提交说明框、文件列表、改动内容里都行） |
| Ctrl+1、Ctrl+2 | 切到 Git 页的本地修改、所有提交，键盘焦点跟过去 |
| Ctrl+F | 搜索提交 |
| Ctrl+P | 快速打开仓库（上下键选，回车打开） |
| Ctrl+, | 打开、关掉设置页 |
| Ctrl+W | 关掉当前标签页（有操作在做会先拦一下） |
| Ctrl+Tab、Ctrl+Shift+Tab | 下一个、上一个标签页 |
| Ctrl+Shift+T | 重新打开刚关掉的标签页 |
| Ctrl+` | 当前仓库在 Git、终端两页之间切换 |
| Ctrl+Shift+` | 当前仓库再开一个终端，切到终端页 |

列表里的按键（先点一下列表或者用 Ctrl+1、Ctrl+2 把焦点放过去）：

| 在哪 | 按键 |
|---|---|
| 本地修改的文件列表 | 上下键换文件，Shift+上下连选，Ctrl+A 全选这一栏；空格或回车暂存、取消暂存；Delete 丢弃（会先问）；右键头进到改动内容里。鼠标 Ctrl、Shift 加点击也能多选 |
| 改动内容 | 上下键在改动行之间走，Shift+上下连选；空格或回车暂存、取消暂存选中的行；Delete 丢弃选中的行（会先问）；左键头回文件列表。并排着看时改成在行号上按住拖动选行，空格、Delete 一样 |
| 提交列表 | 上下键、PageUp、PageDown、Home、End |
| 侧栏 | 上下键换选中的项，回车等于双击（切换分支）；选中文件夹时左键头收起、右键头展开 |
| 逐行追溯 | 上下键、PageUp、PageDown、Home、End 选行，回车跳到改这一行的提交，Esc 关闭 |

焦点在终端里时，F5、Ctrl+W、Ctrl+F、Ctrl+P 交给终端里的程序（shell 里它们是删一个词、往前搜、上一条命令），其余的照常。终端里：Ctrl+C 中断（选着字时是复制），Ctrl+Shift+C 复制，Ctrl+V、Shift+Insert 粘贴（剪贴板里是图片、文件时贴进去的是它们的路径），右键是「选着字就复制、没选就粘贴」，Shift+PageUp、PageDown 和滚轮翻前面的输出。

## 构建与运行

需要 Rust（stable，MSVC 工具链）和 Visual Studio 生成工具（「使用 C++ 的桌面开发」：MSVC 和 Windows SDK）。第一次编译约 5 分钟。

```bash
# Linux 版 kuaid：连服务器时客户端从 target/x86_64-unknown-linux-musl/release/ 取它上传
# （先 rustup target add x86_64-unknown-linux-musl）
cargo build-kuaid

# 客户端；给了仓库路径就加进仓库列表并打开
cargo run --release -p fr-app -- [仓库路径]

# 测通道：本机仓库，或者服务器上的仓库（需要时自动上传 kuaid）
cargo run --release -p fr-channel --bin fr-probe -- --repo .testdata/big-repo local
MSYS_NO_PATHCONV=1 cargo run --release -p fr-channel --bin fr-probe -- --repo <服务器上的仓库路径> <ssh 目标>

# 测服务器上的终端：开（或者接上）这个仓库的终端敲一条命令、列出终端会话、结束它
cargo run --release -p fr-channel --bin fr-probe -- --repo <服务器上的仓库路径> <ssh 目标> --term "echo hi"
cargo run --release -p fr-channel --bin fr-probe -- --repo <服务器上的仓库路径> <ssh 目标> --term-list

# 单元测试
cargo test --workspace

# kuaid 常驻那一套（拉起、终端断开后还在、原地升级）只能在 Linux 上测：交叉编译出测试程序，
# 连同 kuaid 拷到 Linux 机器上跑，见 crates/kuaid/tests/linux.rs 开头
cargo test --no-run --release -p kuaid --target x86_64-unknown-linux-musl

# 打发布包：dist/kuai-<版本>-windows-x64/ 和同名 zip，解压即用
pwsh tools/package.ps1
```

设置存在 `%APPDATA%\kuai\settings.json`；环境变量 `FR_SETTINGS` 可以换成别的文件，测试时不碰自己的设置（开机自启写的是注册表，测试时用 `FR_AUTOSTART_KEY` 换一个键）。

界面上新加一句话：源码里直接写中文、包上 `tr("…")`（带变量用 `tf!`），再到 `crates/fr-core/locales/en.json` 里补英文；没补的话 `cargo test -p fr-core` 会把缺的列出来。在 Git Bash 里给 Windows 程序传 `/data/...` 这类路径会被改写，所以上面测服务器那条要加 `MSYS_NO_PATHCONV=1`。
