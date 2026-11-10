param(
    [Parameter(Mandatory)] [int]$ProcessId,
    # click | dblclick | drag | move | type
    [Parameter(Mandatory)] [string]$Action,
    # 截图里的坐标（窗口矩形左上角为原点的物理像素，和 capture-printwindow.ps1 的图一致）
    [int]$X, [int]$Y, [int]$X2, [int]$Y2,
    [string]$Text
)
# 直接往测试窗口投递鼠标、字符消息，不移动真实鼠标、不抢焦点，不打扰正在用电脑的人。
# 修饰键（Shift、Ctrl）gpui 读的是真实键盘状态，这里模拟不了。
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class PostIn {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
}
"@
[PostIn]::SetProcessDPIAware() | Out-Null
$h = (Get-Process -Id $ProcessId).MainWindowHandle
if ($h -eq 0) { throw "没有窗口" }
$r = New-Object PostIn+RECT
[PostIn]::GetWindowRect($h, [ref]$r) | Out-Null
$o = New-Object PostIn+POINT
[PostIn]::ClientToScreen($h, [ref]$o) | Out-Null
$dx = $o.X - $r.Left
$dy = $o.Y - $r.Top

function LParam([int]$x, [int]$y) {
    $cx = $x - $dx; $cy = $y - $dy
    [IntPtr](($cy -shl 16) -bor ($cx -band 0xFFFF))
}
$WM_MOUSEMOVE = 0x0200; $WM_LBUTTONDOWN = 0x0201; $WM_LBUTTONUP = 0x0202; $WM_CHAR = 0x0102
$MK_LBUTTON = [IntPtr]1

function Post([uint32]$msg, [IntPtr]$w, [IntPtr]$l) {
    [PostIn]::PostMessageW($h, $msg, $w, $l) | Out-Null
    Start-Sleep -Milliseconds 40
}

switch ($Action) {
    'move' { Post $WM_MOUSEMOVE ([IntPtr]0) (LParam $X $Y) }
    'click' {
        Post $WM_MOUSEMOVE ([IntPtr]0) (LParam $X $Y)
        Post $WM_LBUTTONDOWN $MK_LBUTTON (LParam $X $Y)
        Post $WM_LBUTTONUP ([IntPtr]0) (LParam $X $Y)
    }
    'dblclick' {
        Post $WM_MOUSEMOVE ([IntPtr]0) (LParam $X $Y)
        Post $WM_LBUTTONDOWN $MK_LBUTTON (LParam $X $Y)
        Post $WM_LBUTTONUP ([IntPtr]0) (LParam $X $Y)
        Post $WM_LBUTTONDOWN $MK_LBUTTON (LParam $X $Y)
        Post $WM_LBUTTONUP ([IntPtr]0) (LParam $X $Y)
    }
    'drag' {
        Post $WM_MOUSEMOVE ([IntPtr]0) (LParam $X $Y)
        Post $WM_LBUTTONDOWN $MK_LBUTTON (LParam $X $Y)
        $steps = 6
        for ($i = 1; $i -le $steps; $i++) {
            $mx = [int]($X + ($X2 - $X) * $i / $steps); $my = [int]($Y + ($Y2 - $Y) * $i / $steps)
            Post $WM_MOUSEMOVE $MK_LBUTTON (LParam $mx $my)
        }
        Post $WM_LBUTTONUP ([IntPtr]0) (LParam $X2 $Y2)
    }
    'activate' {
        # 模拟「切到别的程序再切回来」：先失活再激活（不真的改前台窗口）
        Post 0x0006 ([IntPtr]0) ([IntPtr]0)
        Start-Sleep -Milliseconds 200
        Post 0x0006 ([IntPtr]1) ([IntPtr]0)
    }
    'type' {
        foreach ($c in $Text.ToCharArray()) { Post $WM_CHAR ([IntPtr][int]$c) ([IntPtr]1) }
    }
    default { throw "不认识的动作 $Action" }
}
"{0} ({1},{2}) 客户区偏移 ({3},{4})" -f $Action, $X, $Y, $dx, $dy
