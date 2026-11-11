param(
    [Parameter(Mandatory)] [int]$ProcessId,
    # click | dblclick | rclick | mclick | drag | move | wheel | type | key | activate | hittest | syscommand
    [Parameter(Mandatory)] [string]$Action,
    # 截图里的坐标（窗口矩形左上角为原点的物理像素，和 capture-printwindow.ps1 的图一致）
    [int]$X, [int]$Y, [int]$X2, [int]$Y2,
    [string]$Text,
    # 按住的修饰键：ctrl、shift 或 ctrl+shift。对 click、dblclick、key 有效。
    [string]$Mods = ''
)
# 直接往测试窗口投递鼠标、字符消息，不移动真实鼠标、不抢焦点，不打扰正在用电脑的人。
# 修饰键（Shift、Ctrl）gpui 用 GetKeyState 读，读的是窗口线程自己的那份按键状态，不是真实键盘。
# 所以带修饰键时：把本线程的输入状态和窗口线程接到一起（AttachThreadInput），把那份状态里的
# Ctrl、Shift 改成按下，投递消息，等窗口处理完再改回去。真实键盘不受影响；这期间如果有人正好在
# 按键盘，那份状态会被真实按键冲掉，结果可能不对，重跑一次。
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class PostIn {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] static extern bool AttachThreadInput(uint a, uint b, bool attach);
  [DllImport("user32.dll")] static extern bool GetKeyboardState(byte[] s);
  [DllImport("user32.dll")] static extern bool SetKeyboardState(byte[] s);
  // 按住修饰键投递一串消息。整个过程在一次调用里做完，保证都在同一个线程上。
  public static bool PostWithMods(IntPtr h, bool ctrl, bool shift, uint[] msg, long[] w, long[] l) {
    uint pid; uint target = GetWindowThreadProcessId(h, out pid); uint me = GetCurrentThreadId();
    if (!AttachThreadInput(me, target, true)) return false;
    try {
      byte[] old = new byte[256]; GetKeyboardState(old);
      byte[] now = (byte[])old.Clone();
      if (ctrl) { now[0x11] = 0x80; now[0xA2] = 0x80; }
      if (shift) { now[0x10] = 0x80; now[0xA0] = 0x80; }
      SetKeyboardState(now);
      for (int i = 0; i < msg.Length; i++) { PostMessageW(h, msg[i], (IntPtr)w[i], (IntPtr)l[i]); System.Threading.Thread.Sleep(40); }
      System.Threading.Thread.Sleep(250);
      SetKeyboardState(old);
    } finally { AttachThreadInput(me, target, false); }
    return true;
  }
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

$ctrl = $Mods -match 'ctrl'; $shift = $Mods -match 'shift'
# 带修饰键时先把消息攒起来，最后一次投递
$script:batch = @()
function Post([uint32]$msg, [IntPtr]$w, [IntPtr]$l) {
    if ($ctrl -or $shift) { $script:batch += , @($msg, [long]$w, [long]$l); return }
    [PostIn]::PostMessageW($h, $msg, $w, $l) | Out-Null
    Start-Sleep -Milliseconds 40
}
# 鼠标消息的 wParam 里也带上修饰键（MK_SHIFT = 4，MK_CONTROL = 8）
$mk = 0; if ($shift) { $mk = $mk -bor 4 }; if ($ctrl) { $mk = $mk -bor 8 }
$MK_DOWN = [IntPtr](1 -bor $mk); $MK_UP = [IntPtr]$mk

switch ($Action) {
    'move' { Post $WM_MOUSEMOVE ([IntPtr]0) (LParam $X $Y) }
    'click' {
        Post $WM_MOUSEMOVE $MK_UP (LParam $X $Y)
        Post $WM_LBUTTONDOWN $MK_DOWN (LParam $X $Y)
        Post $WM_LBUTTONUP $MK_UP (LParam $X $Y)
    }
    'dblclick' {
        Post $WM_MOUSEMOVE $MK_UP (LParam $X $Y)
        Post $WM_LBUTTONDOWN $MK_DOWN (LParam $X $Y)
        Post $WM_LBUTTONUP $MK_UP (LParam $X $Y)
        Post $WM_LBUTTONDOWN $MK_DOWN (LParam $X $Y)
        Post $WM_LBUTTONUP $MK_UP (LParam $X $Y)
    }
    'rclick' {
        # 右键：弹出右键菜单
        Post $WM_MOUSEMOVE ([IntPtr]0) (LParam $X $Y)
        Post 0x0204 ([IntPtr]2) (LParam $X $Y)
        Post 0x0205 ([IntPtr]0) (LParam $X $Y)
    }
    'key' {
        # 按键：-Text 给名字（enter、esc、f5、up、down、tab、delete、space、left、right、pageup、pagedown、home、end、comma），
        # 或者一个字母、数字（配 -Mods ctrl 用，比如 Ctrl+P）
        $name = $Text.ToLower()
        $vk = @{ enter = 0x0D; esc = 0x1B; f5 = 0x74; up = 0x26; down = 0x28; tab = 0x09; delete = 0x2E; space = 0x20; left = 0x25; right = 0x27; pageup = 0x21; pagedown = 0x22; end = 0x23; home = 0x24; comma = 0xBC }[$name]
        if (-not $vk -and $name -match '^[a-z0-9]$') { $vk = [int][char]$name.ToUpper() }
        if (-not $vk) { throw "不认识的按键 $Text" }
        Post 0x0100 ([IntPtr]$vk) ([IntPtr]1)
        Post 0x0101 ([IntPtr]$vk) ([IntPtr]0xC0000001)
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
    'wheel' {
        # 滚轮：-Text 给格数，负数往下滚（不给就是 -3）。滚轮消息的坐标是屏幕坐标，和别的鼠标消息不一样；
        # gpui 把滚动交给鼠标底下的那一块，所以先投一次移动
        $notches = if ($Text) { [int]$Text } else { -3 }
        Post $WM_MOUSEMOVE ([IntPtr]0) (LParam $X $Y)
        $sx = $r.Left + $X; $sy = $r.Top + $Y
        $at = [IntPtr]((($sy -band 0xFFFF) -shl 16) -bor ($sx -band 0xFFFF))
        $delta = if ($notches -lt 0) { -120 } else { 120 }
        for ($i = 0; $i -lt [Math]::Abs($notches); $i++) { Post 0x020A ([IntPtr]([int64]$delta -shl 16)) $at }
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
    'hittest' {
        # 标题栏是程序自己画的：问窗口这一点算什么（WM_NCHITTEST）。
        # 1 客户区（能点的控件），2 标题栏（能拖窗口），8 最小化，9 最大化，20 关闭，12 上边框。
        # gpui 按它记着的鼠标位置答，所以先投一次鼠标移动。真实鼠标点下去走的就是这个判断：
        # 标签页、按钮上答成 2 的话，真实鼠标点它会变成拖窗口，投递的点击测不出来。
        Post $WM_MOUSEMOVE ([IntPtr]0) (LParam $X $Y)
        Start-Sleep -Milliseconds 250
        $sx = $r.Left + $X; $sy = $r.Top + $Y
        $code = [PostIn]::SendMessageW($h, 0x0084, [IntPtr]0, [IntPtr](($sy -shl 16) -bor ($sx -band 0xFFFF)))
        "hittest ({0},{1}) = {2}" -f $X, $Y, $code
        return
    }
    'mclick' {
        # 中键：关标签页
        Post $WM_MOUSEMOVE ([IntPtr]0) (LParam $X $Y)
        Post 0x0207 ([IntPtr]0x10) (LParam $X $Y)
        Post 0x0208 ([IntPtr]0) (LParam $X $Y)
    }
    'syscommand' {
        # 系统命令：-Text minimize | restore | close（等于点标题栏上的按钮、Alt+F4）
        $sc = @{ minimize = 0xF020; restore = 0xF120; close = 0xF060; maximize = 0xF030 }[$Text.ToLower()]
        if (-not $sc) { throw "不认识的系统命令 $Text" }
        Post 0x0112 ([IntPtr]$sc) ([IntPtr]0)
    }
    default { throw "不认识的动作 $Action" }
}
if ($script:batch.Count -gt 0) {
    $ok = [PostIn]::PostWithMods($h, $ctrl, $shift,
        [uint32[]]($script:batch | ForEach-Object { $_[0] }),
        [long[]]($script:batch | ForEach-Object { $_[1] }),
        [long[]]($script:batch | ForEach-Object { $_[2] }))
    if (-not $ok) { throw "接不上窗口线程的输入状态，修饰键发不了" }
}
"{0} ({1},{2}) 客户区偏移 ({3},{4})" -f $Action, $X, $Y, $dx, $dy
