# 从 tools/installer/kuai.iss 编一个测试用的安装程序：单独的 AppId、开始菜单名和「应用」里的名字，
# 不带 [UninstallDelete]。平时装着 Kuai 的电脑上测安装、卸载要用它，不能直接跑真的安装程序：
# 真的安装脚本 AppId 是固定的，换个目录装（/DIR=）也会把「应用」里 Kuai 那一项和开始菜单的图标
# 改指到测试目录，卸载时连它们和 %LOCALAPPDATA%\kuai\update 一起删掉。
#
#   make-test-setup.ps1 -PackageDir <打好的 Kuai 文件夹> -OutDir <输出目录> [-Name a] [-Drop '^RedirectionGuard=']
#
# -PackageDir：tools/package.ps1 打出来的 Kuai 文件夹（dist\kuai-<版本>-windows-x64\Kuai）。
# -Name：区分几个测试安装程序，各有各的 AppId；出来的是 kuai-test-<Name>.exe，开始菜单里叫「Kuai Test <Name>」。
# -Drop：去掉安装脚本里匹配这些正则的行，对比某一行起不起作用（比如 RedirectionGuard=no，见架构.md 2.30 节）。
#
# 装的时候照旧设好 FR_* 环境变量、加 /DIR=<测试目录>，见 README 的「安装程序」一节。
param(
    [Parameter(Mandatory)] [string] $PackageDir,
    [Parameter(Mandatory)] [string] $OutDir,
    [ValidatePattern('^[A-Za-z0-9]+$')] [string] $Name = 'a',
    [string[]] $Drop = @()
)
$ErrorActionPreference = 'Stop'
$root = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
$iscc = @(
    $env:ISCC,
    (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'),
    (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
    (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe')
) | Where-Object { $_ -and (Test-Path -LiteralPath $_) } | Select-Object -First 1
if (-not $iscc) { throw '找不到 Inno Setup 6 的 ISCC.exe' }
$PackageDir = (Resolve-Path -LiteralPath $PackageDir).Path
New-Item -ItemType Directory -Force $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path
$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value

# AppId 由名字算出来：同一个名字每次一样（重装是覆盖），不同名字互不相干，也不会和真的撞上
$md5 = [System.Security.Cryptography.MD5]::Create()
$guid = [guid]::new($md5.ComputeHash([System.Text.Encoding]::UTF8.GetBytes("kuai-test-setup-$Name")))
$appId = '{{' + $guid.ToString().ToUpperInvariant() + '}'

$source = Join-Path $root 'tools\installer\kuai.iss'
$bytes = [System.IO.File]::ReadAllBytes($source)
$bom = $bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF
$lines = [System.IO.File]::ReadAllText($source, [System.Text.Encoding]::UTF8) -split "`r?`n"
$out = [System.Collections.Generic.List[string]]::new()
:line foreach ($line in $lines) {
    foreach ($pattern in $Drop) { if ($line -match $pattern) { continue line } }
    # 卸载时不删真的更新目录
    if ($line -match '^Type: \w+; Name: "\{localappdata\}\\kuai') { continue }
    if ($line -match '^AppId=') { $line = "AppId=$appId" }
    elseif ($line -match '^UninstallDisplayName=') { $line = "UninstallDisplayName=Kuai Test $Name" }
    elseif ($line -match '^OutputBaseFilename=') { $line = "OutputBaseFilename=kuai-test-$Name" }
    $line = $line.Replace('"{autoprograms}\Kuai"', "`"{autoprograms}\Kuai Test $Name`"")
    $line = $line.Replace('"{autodesktop}\Kuai"', "`"{autodesktop}\Kuai Test $Name`"")
    $out.Add($line)
}
# 放在真的脚本旁边编，里面的相对路径（图、语言文件）才对得上；编完就删
$script = Join-Path $root "tools\installer\kuai-test-$Name.tmp.iss"
[System.IO.File]::WriteAllText($script, ($out -join "`r`n"), [System.Text.UTF8Encoding]::new($bom))
try {
    & $iscc /Q "/DAppVersion=$version" "/DPackageDir=$PackageDir" "/DOutputDir=$OutDir" $script
    if ($LASTEXITCODE -ne 0) { throw "安装程序编译失败（$LASTEXITCODE）" }
} finally {
    Remove-Item -LiteralPath $script -Force -ErrorAction SilentlyContinue
}
$setup = Join-Path $OutDir "kuai-test-$Name.exe"
'{0}  {1:N1} MB  AppId={2}_is1' -f $setup, ((Get-Item -LiteralPath $setup).Length / 1MB), $appId.Substring(1)
