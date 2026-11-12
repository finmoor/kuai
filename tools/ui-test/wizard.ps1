# 驱动安装、卸载程序的向导（Inno Setup）：列控件、点按钮、勾选框、把窗口放到最底下。
# 走 UI Automation 找控件，再给按钮窗口发消息点它，不动真实鼠标、不抢焦点。
#
#   wizard.ps1 -ProcessId <进程号> -List                 列出窗口和里面的控件（含窗口句柄，截图用）
#   wizard.ps1 -ProcessId <进程号> -Invoke '下一步(N)'    点名字完全一样的按钮
#   wizard.ps1 -ProcessId <进程号> -Toggle '桌面' -State on  勾上名字里带这几个字的勾选框
#   wizard.ps1 -ProcessId <进程号> -Bottom               窗口放到最底下
#
# 进程号给 setup.exe（或 unins000.exe）的就行，它拉起的 .tmp 子进程（真正的向导）也一起找。
# Inno 的按钮在 UI Automation 里是 Pane，没有 Invoke：向导上的按钮发 BM_CLICK；
# 系统对话框（#32770，比如「确定要卸载吗」）不在前台时 BM_CLICK 不灵，改给对话框发 WM_COMMAND。
# 截图用 capture-window.ps1 -Hwnd：向导进程的主窗口是一个看不见的 0×0 窗口。
param(
    [Parameter(Mandatory)] [int] $ProcessId,
    [switch] $List,
    [string] $Invoke,
    [string] $Toggle,
    [ValidateSet('on', 'off')] [string] $State = 'on',
    [switch] $Bottom
)
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
if (-not ('UiaWin' -as [type])) {
    Add-Type -Namespace '' -Name UiaWin -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool SetWindowPos(System.IntPtr h, System.IntPtr after, int x, int y, int cx, int cy, uint flags);
[DllImport("user32.dll")] public static extern bool PostMessage(System.IntPtr h, uint msg, System.IntPtr w, System.IntPtr l);
[DllImport("user32.dll")] public static extern int GetDlgCtrlID(System.IntPtr h);
[DllImport("user32.dll")] public static extern System.IntPtr GetParent(System.IntPtr h);
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(System.IntPtr h, System.Text.StringBuilder s, int n);
'@
}
$A = [System.Windows.Automation.AutomationElement]
$pids = @($ProcessId) + @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$ProcessId" | ForEach-Object { [int]$_.ProcessId })
$windows = @()
foreach ($p in $pids) {
    $cond = New-Object System.Windows.Automation.PropertyCondition($A::ProcessIdProperty, $p)
    $windows += @($A::RootElement.FindAll([System.Windows.Automation.TreeScope]::Children, $cond))
}
if ($windows.Count -eq 0) { "no windows for $($pids -join ',')"; exit 1 }
foreach ($w in $windows) {
    $controls = $w.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    if ($Bottom) {
        [void][UiaWin]::SetWindowPos([IntPtr]$w.Current.NativeWindowHandle, [IntPtr]1, 0, 0, 0, 0, 0x13)
        "bottom: $($w.Current.Name)"
    }
    if ($List) {
        "WINDOW [$($w.Current.ClassName)] '$($w.Current.Name)' pid=$($w.Current.ProcessId) hwnd=$($w.Current.NativeWindowHandle)"
        foreach ($c in $controls) {
            if ($c.Current.IsOffscreen) { continue }
            $type = $c.Current.ControlType.ProgrammaticName -replace 'ControlType\.', ''
            $extra = ''
            $tp = $null
            if ($c.TryGetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern, [ref]$tp)) { $extra = " toggle=$($tp.Current.ToggleState)" }
            $vp = $null
            if ($c.TryGetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern, [ref]$vp)) { $extra += " value='$($vp.Current.Value)'" }
            "  $type '$($c.Current.Name)'$extra enabled=$($c.Current.IsEnabled)"
        }
    }
    if ($Invoke) {
        foreach ($c in $controls) {
            if ($c.Current.Name -eq $Invoke -and -not $c.Current.IsOffscreen -and $c.Current.NativeWindowHandle -ne 0) {
                $btn = [IntPtr]$c.Current.NativeWindowHandle
                $sb = New-Object System.Text.StringBuilder 64
                [void][UiaWin]::GetClassName([UiaWin]::GetParent($btn), $sb, 64)
                if ($sb.ToString() -eq '#32770') {
                    # system dialogs: BM_CLICK fails when the dialog isn't active; send WM_COMMAND (0x0111) to it
                    $id = [UiaWin]::GetDlgCtrlID($btn)
                    [void][UiaWin]::PostMessage([UiaWin]::GetParent($btn), 0x0111, [IntPtr]$id, $btn)
                } else {
                    # Inno's wizard buttons show up as panes; BM_CLICK (0x00F5) to the button window works
                    [void][UiaWin]::PostMessage($btn, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero)
                }
                "clicked '$($c.Current.Name)'"
                exit 0
            }
        }
    }
    if ($Toggle) {
        foreach ($c in $controls) {
            $tp = $null
            if ($c.Current.Name.Contains($Toggle) -and -not $c.Current.IsOffscreen -and $c.TryGetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern, [ref]$tp)) {
                $want = if ($State -eq 'on') { 'On' } else { 'Off' }
                if ("$($tp.Current.ToggleState)" -ne $want) { $tp.Toggle() }
                "toggled '$($c.Current.Name)' -> $($tp.Current.ToggleState)"
                exit 0
            }
        }
    }
}
if ($Invoke -or $Toggle) { "not found: $Invoke$Toggle"; exit 2 }
