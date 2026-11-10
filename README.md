# fork-remote（暂名）

Windows 上界面和操作习惯接近 Fork 的 Git 客户端，最终目标是彻底替代 Fork（本机仓库也管）。

和 Fork 的区别是能打开服务器上的仓库：「添加服务器」时在服务器上装一个类似 VS Code Server 的 agent，经 SSH 的 stdin/stdout 通信；不要 root、不开端口、服务器不用联网。服务器上仓库的 git 命令都在服务器上执行，Windows 这边只收结果。

> 名字是暂用的。

## 技术栈

- 客户端：Rust + GPUI，经 [gpui-kit](https://github.com/longbridge/gpui-kit) 0.9 引入（gpui-pre 快照、gpui-base、gpui-component）。
- 服务端 agent：Rust；Linux 版用 musl 静态编译，从 Windows 交叉编译。
- 本机仓库在进程内直接调用核心代码；服务器上的仓库经 SSH 交给 agent。两边用同一套 `fr-core`。
- git 操作调用命令行，密码输入走 askpass 助手，和 Fork 的做法一样；高频只读操作以后可以换成 gitoxide。
- 选这套栈是为了性能。GPUI 还没到 1.0，所以先做了 M0 技术验证，已经通过。

## 目录

| 路径 | 内容 |
|---|---|
| `crates/fr-core` | 调用 git、解析输出、计算提交图布局、agent 通信协议；不依赖界面，客户端和 agent 共用 |
| `crates/fr-channel` | 仓库通道：本机仓库在进程内执行 git，服务器上的仓库经 ssh 交给 agent（连接复用、自动安装 agent）；测试工具 `fr-probe` |
| `crates/fr-app` | GPUI 客户端 |
| `crates/fr-agent` | 服务端程序 `fr-agent` |
| `tools/gen-test-repo.py` | 生成性能测试用的大仓库 |
| `tools/ui-test/` | 界面测试脚本：建测试仓库、截窗口、往测试窗口投递点击和输入（不动真实鼠标） |
| `tools/package.ps1` | 打发布包：客户端加 Linux 版 agent，输出到 `dist/` |

## 构建与运行

需要 Rust（stable，MSVC 工具链）和 Visual Studio 生成工具（「使用 C++ 的桌面开发」：MSVC 和 Windows SDK）。第一次编译约 5 分钟。

```bash
# Linux 版 agent：连服务器时客户端从 target/x86_64-unknown-linux-musl/release/ 取它上传
# （先 rustup target add x86_64-unknown-linux-musl）
cargo build-agent

# 客户端；给了仓库路径就加进仓库列表并打开
cargo run --release -p fr-app -- [仓库路径]

# 测通道：本机仓库，或者服务器上的仓库（需要时自动上传 agent）
cargo run --release -p fr-channel --bin fr-probe -- --repo .testdata/big-repo local
MSYS_NO_PATHCONV=1 cargo run --release -p fr-channel --bin fr-probe -- --repo <服务器上的仓库路径> <ssh 目标>

# 单元测试
cargo test --workspace

# 打发布包：dist/fork-remote-<版本>-windows-x64/ 和同名 zip，解压即用
pwsh tools/package.ps1
```

设置存在 `%APPDATA%\fork-remote\settings.json`；环境变量 `FR_SETTINGS` 可以换成别的文件，测试时不碰自己的设置。在 Git Bash 里给 Windows 程序传 `/data/...` 这类路径会被改写，所以上面测服务器那条要加 `MSYS_NO_PATHCONV=1`。
