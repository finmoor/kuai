# fork-remote（暂名）

Windows 上界面和操作习惯接近 Fork 的 Git 客户端，最终目标是彻底替代 Fork（本机仓库也管）。

和 Fork 的区别是能打开服务器上的仓库：「添加服务器」时在服务器上装一个类似 VS Code Server 的 agent，经 SSH 的 stdin/stdout 通信；不要 root、不开端口、服务器不用联网。服务器上仓库的 git 命令都在服务器上执行，Windows 这边只收结果。

> 名字是暂用的。

## 技术栈

- 客户端：Rust + GPUI，经 [gpui-kit](https://github.com/longbridge/gpui-kit) 0.9 引入（gpui-pre 快照、gpui-base、gpui-component）。
- 服务端 agent：Rust；Linux 版用 musl 静态编译，从 Windows 交叉编译。
- 本机仓库在进程内直接调用核心代码；服务器上的仓库经 SSH 交给 agent。两边用同一套 `fr-core`。
- git 操作调用命令行，密码输入走 askpass 助手，和 Fork 的做法一样；高频只读操作以后可以换成 gitoxide。
- 选这套栈是为了性能。GPUI 还没到 1.0，所以先做 M0 技术验证；验证不通过就退回 Fork 同款的 WPF。

## 目录

| 路径 | 内容 |
|---|---|
| `crates/fr-core` | 调用 git、解析 log、计算提交图布局、agent 通信协议；不依赖界面 |
| `crates/fr-app` | GPUI 客户端 |
| `crates/fr-agent` | 服务端程序 `fr-agent`，以及测试客户端 `fr-probe` |
| `tools/gen-test-repo.py` | 生成性能测试用的大仓库 |

## 构建与运行

需要 Rust（stable，MSVC 工具链）和 Visual Studio 生成工具（「使用 C++ 的桌面开发」：MSVC 和 Windows SDK）。第一次编译约 5 分钟。

```bash
# 客户端；不给路径就打开 .testdata/big-repo
cargo run --release -p fr-app -- <仓库路径>

# Linux 版 agent（先 rustup target add x86_64-unknown-linux-musl）
cargo build --release -p fr-agent --bin fr-agent --target x86_64-unknown-linux-musl

# 核心逻辑的单元测试
cargo test -p fr-core
```
