param(
    [Parameter(Mandatory)] [string]$In,
    [Parameter(Mandatory)] [string]$Out,
    [Parameter(Mandatory)] [int]$X, [Parameter(Mandatory)] [int]$Y,
    [Parameter(Mandatory)] [int]$W, [Parameter(Mandatory)] [int]$H
)
# 从截图里裁一块（原图像素坐标）
Add-Type -AssemblyName System.Drawing
$src = [System.Drawing.Bitmap]::FromFile($In)
$rect = New-Object System.Drawing.Rectangle $X, $Y, $W, $H
$dst = $src.Clone($rect, $src.PixelFormat)
$dst.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$src.Dispose(); $dst.Dispose()
"裁剪 {0}x{1} -> {2}" -f $W, $H, $Out
