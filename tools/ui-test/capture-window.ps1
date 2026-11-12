[CmdletBinding()]
param(
    [Parameter(Mandatory, ParameterSetName = 'process')] [int]$ProcessId,
    # 按窗口句柄截：进程的主窗口不是要截的那个时用，比如安装程序（见 wizard.ps1 -List）
    [Parameter(Mandatory, ParameterSetName = 'window')] [long]$Hwnd,
    [Parameter(Mandatory)] [string]$Out
)
# 用 PrintWindow（PW_RENDERFULLCONTENT）取窗口内容：窗口被别的窗口挡住也能截，不用把它切到前台。
# 截出来的图以窗口矩形左上角为原点、物理像素，post-input.ps1 的坐标就按这张图量。
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class PrintW {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
"@
[PrintW]::SetProcessDPIAware() | Out-Null
if ($PSCmdlet.ParameterSetName -eq 'window') {
    $window = [IntPtr]$Hwnd
} else {
    $window = (Get-Process -Id $ProcessId).MainWindowHandle
    if ($window -eq 0) { throw "没有窗口" }
}
$r = New-Object PrintW+RECT
[PrintW]::GetWindowRect($window, [ref]$r) | Out-Null
$w = $r.Right - $r.Left
$h = $r.Bottom - $r.Top
$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
$ok = [PrintW]::PrintWindow($window, $hdc, 2)
$g.ReleaseHdc($hdc)
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
"PrintWindow={0} {1}x{2} -> {3}" -f $ok, $w, $h, $Out
