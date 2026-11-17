# 让资源管理器来启动一个程序，和用户在开始菜单、桌面上双击一样：进程的父进程是 explorer.exe，
# 不继承这个会话的东西。测「安装程序、Kuai 是用户自己打开的」这类情况用：从命令工具的会话直接启动的话，
# 会话带着什么（比如 Windows 的 Redirection Guard，见架构.md 2.30 节）进程就带着什么，测不出来。
#
#   run-from-explorer.ps1 -File <程序或 .cmd> [-Arguments '<参数>'] [-Dir <目录>] [-Show]
#
# 环境变量也不继承（用的是资源管理器的）。要带 FR_* 这些，写一个 .cmd：先 set，再启动程序，把 .cmd 交给它。
# 默认不显示窗口（.cmd 的黑框不弹出来；它再启动的带窗口的程序照常显示），-Show 照常显示。
#
# 做法：拿资源管理器桌面窗口的自动化对象调 ShellExecute，调用在资源管理器的进程里执行
# （Kuai 甩掉 Redirection Guard 用的是同一个办法，crates/fr-app/src/redirect_guard.rs）。
# 没有登录到桌面（经 ssh 来的会话）时找不到桌面窗口，报错。
#
# 查一个进程带没带着 Redirection Guard：GetProcessMitigationPolicy 的第 16 项（ProcessRedirectionTrustPolicy），
# 结果的第 0 位是 1 就是带着。
param(
    [Parameter(Mandatory)] [string] $File,
    [string] $Arguments = '',
    [string] $Dir = '',
    [switch] $Show
)
$ErrorActionPreference = 'Stop'
if (Test-Path -LiteralPath $File) { $File = (Resolve-Path -LiteralPath $File).Path }
if (-not $Dir) { $Dir = (Get-Location).Path }
$windows = (New-Object -ComObject Shell.Application).Windows()
$hwnd = 0
# 0 是 CSIDL_DESKTOP，8 是 SWC_DESKTOP，1 是 SWFO_NEEDDISPATCH
$desktop = $windows.FindWindowSW(0, $null, 8, [ref] $hwnd, 1)
if (-not $desktop) { throw '找不到资源管理器的桌面窗口' }
$desktop.Document.Application.ShellExecute($File, $Arguments, $Dir, 'open', $(if ($Show) { 1 } else { 0 }))
