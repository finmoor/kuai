# 打发布包，出两个文件：
# - dist\kuai-<版本>-windows-x64-setup.exe：安装程序（tools/installer/kuai.iss），装在用户目录下；
# - dist\kuai-<版本>-windows-x64.zip：解压即用版，解压出来是一个 Kuai 文件夹。
# 同样的东西也放在 dist\kuai-<版本>-windows-x64\Kuai\ 下，本机直接试。
#
# 包里的文件夹不带版本号：解压版点「升级」是原地换文件，文件夹名留着旧版本号会误导人。
# 发版时（.github/workflows/release.yml）把两个都传到 GitHub Releases，Kuai 检查更新时
# 安装版找 kuai-*-windows-x64-setup.exe、解压版找 kuai-*-windows-x64.zip（crates/fr-app/src/update.rs）。
#
# 包里有客户端 kuai.exe，和常驻程序 kuaid（kuaid\<编译目标>\ 下）：Linux 版连服务器时上传；
# Windows 版连 Windows 服务器时上传，本机的终端也开在它里面（装到 ~/.kuai 下再启动）。
# 还有许可证和第三方库的许可证清单。
# 解压到哪都能直接运行，不用安装；机器上要有 git（要在 PATH 里）和 Windows 自带的 OpenSSH。
#
# 用法：pwsh tools/package.ps1 [-Dist <输出目录>]（默认 dist；dist 里的 Kuai 正开着、删不掉时换个目录）
# 打包的机器要有 Rust、musl 编译目标、Python 3 和 Inno Setup 6。
param([string] $Dist)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root
if (-not $Dist) { $Dist = Join-Path $root 'dist' }
$Dist = [System.IO.Path]::GetFullPath($Dist)

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$name = "kuai-$version-windows-x64"
$out = Join-Path $Dist "$name\Kuai"

cargo build --release -p fr-app
if ($LASTEXITCODE -ne 0) { throw '客户端编译失败' }
# Windows 版 kuaid：和客户端同一个编译目标，编在 target\release 下
cargo build --release -p kuaid
if ($LASTEXITCODE -ne 0) { throw 'kuaid（Windows）编译失败' }

# 装了哪个 Linux 编译目标就打哪个的 kuaid；x86_64 必须有
$installed = rustup target list --installed
$targets = @('x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl') | Where-Object { $installed -contains $_ }
if ($targets -notcontains 'x86_64-unknown-linux-musl') {
    throw '先运行 rustup target add x86_64-unknown-linux-musl'
}
foreach ($target in $targets) {
    cargo build --release -p kuaid --target $target
    if ($LASTEXITCODE -ne 0) { throw "kuaid（$target）编译失败" }
}

if (Test-Path (Split-Path $out)) { Remove-Item -Recurse -Force (Split-Path $out) }
New-Item -ItemType Directory -Force $out | Out-Null
Copy-Item target\release\fr-app.exe (Join-Path $out 'kuai.exe')
foreach ($target in $targets) {
    $dir = Join-Path $out "kuaid\$target"
    New-Item -ItemType Directory -Force $dir | Out-Null
    Copy-Item "target\$target\release\kuaid" $dir
}
$windows = Join-Path $out 'kuaid\x86_64-pc-windows-msvc'
New-Item -ItemType Directory -Force $windows | Out-Null
Copy-Item target\release\kuaid.exe $windows
Copy-Item tools\package-readme.txt (Join-Path $out '使用说明.txt')
Copy-Item LICENSE-MIT, LICENSE-APACHE $out
# 编进去的第三方库的许可证（MIT、Apache-2.0 这些要求随程序附上），要 Python 3
python tools\third-party-licenses.py (Join-Path $out 'THIRD-PARTY-LICENSES.txt')
if ($LASTEXITCODE -ne 0) { throw '第三方许可证清单生成失败' }

$zip = Join-Path $Dist "$name.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $out -DestinationPath $zip

# 安装程序：Inno Setup 6 编 tools/installer/kuai.iss，出 dist\kuai-<版本>-windows-x64-setup.exe
$iscc = @(
    $env:ISCC,
    (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'),
    (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
    (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe')
) | Where-Object { $_ -and (Test-Path -LiteralPath $_) } | Select-Object -First 1
if (-not $iscc) {
    throw '找不到 Inno Setup 6 的 ISCC.exe：winget install JRSoftware.InnoSetup --scope user，或者设环境变量 ISCC'
}
$setup = Join-Path $Dist "$name-setup.exe"
if (Test-Path $setup) { Remove-Item -Force $setup }
& $iscc /Q "/DAppVersion=$version" "/DPackageDir=$out" "/DOutputDir=$Dist" tools\installer\kuai.iss
if ($LASTEXITCODE -ne 0) { throw '安装程序编译失败' }

"打好了：$out"
Get-ChildItem -Recurse -File $out | ForEach-Object {
    '  {0,-60} {1,8:N1} MB' -f $_.FullName.Substring($out.Length + 1), ($_.Length / 1MB)
}
foreach ($file in $zip, $setup) {
    '  {0,-60} {1,8:N1} MB' -f (Split-Path $file -Leaf), ((Get-Item $file).Length / 1MB)
}
