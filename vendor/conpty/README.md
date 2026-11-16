# 新版 ConPTY（conpty.dll、OpenConsole.exe）

Windows 终端用的伪控制台，随 Windows 版 kuaid 一起发：kuaid 旁边有这两个文件就用它们开终端，
没有就用系统自带的 ConPTY。为什么要它见 `docs/架构.md` 2.28 节：系统自带的把程序要鼠标的请求
（`ESC[?1000h` 等）吞掉，Kuai 不知道程序要了鼠标，Claude Code 这类程序就收不到滚轮。

- 来源：微软的 NuGet 包 `Microsoft.Windows.Console.ConPTY` 1.25.260930003（2026-10-02 发布，MIT，
  源码在 https://github.com/microsoft/terminal ），
  https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/1.25.260930003/microsoft.windows.console.conpty.1.25.260930003.nupkg
  （1,763,391 字节，SHA-512 与 nuget.org 记录的一致）。
- 取的文件：`runtimes/win-x64/native/conpty.dll`、`build/native/runtimes/x64/OpenConsole.exe`，
  都带微软的数字签名，版本 1.25.2609.30003。

| 文件 | 字节 | SHA-256 |
|---|---|---|
| `x64/conpty.dll` | 119,608 | `feeef341d891643c62d30b6b07800bc70f0bc148f44cb8c3bee84aa557ae805a` |
| `x64/OpenConsole.exe` | 1,089,376 | `3d66b23d0a71bb8eed2b77edc8b9df9bf54ce6c8fb74c863a30e760997f80586` |

只放了 x64：Kuai 只发 x64 版，ARM64 的 Windows 版 kuaid 自己编的话用系统自带的 ConPTY。

**用在哪**：`tools/package.ps1` 打包时放进 `Kuai\kuaid\x86_64-pc-windows-msvc\`；Kuai 找 kuaid 时
（`crates/fr-channel/src/binaries.rs`）把它们和 kuaid.exe 一起装到本机的 `~/.kuai\<版本>\`、
传到 Windows 服务器上；开发时从这里取。kuaid 只加载自己目录里的 `conpty.dll`
（`crates/kuaid/src/term/conpty.rs`），`conpty.dll` 再启动它旁边的 `OpenConsole.exe`。

**更新**：从 nuget.org 下新版的包，核对它的 SHA-512 和两个文件的签名，换掉 `x64` 下的两个文件，
改这里的版本和 SHA-256、`tools/third-party-licenses.py` 里 `VENDORED` 的版本；跑 kuaid 的 Windows 测试
（`程序要鼠标的请求转给客户端` 测的就是它），再用 `fr-probe --term-keys` 看送进去的字节（做法见 2.28 节）。
