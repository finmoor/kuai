# 打发布包：dist\kuai-<版本>-windows-x64\ 和同名的 zip。
#
# 包里有客户端 kuai.exe，和连服务器时要上传的 Linux 版常驻程序 kuaid（kuaid\<编译目标>\kuaid）。
# 解压到哪都能直接运行，不用安装；机器上要有 git（要在 PATH 里）和 Windows 自带的 OpenSSH。
#
# 用法：pwsh tools/package.ps1
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$name = "kuai-$version-windows-x64"
$out = Join-Path $root "dist\$name"

cargo build --release -p fr-app
if ($LASTEXITCODE -ne 0) { throw '客户端编译失败' }

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

if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force $out | Out-Null
Copy-Item target\release\fr-app.exe (Join-Path $out 'kuai.exe')
foreach ($target in $targets) {
    $dir = Join-Path $out "kuaid\$target"
    New-Item -ItemType Directory -Force $dir | Out-Null
    Copy-Item "target\$target\release\kuaid" $dir
}
Copy-Item tools\package-readme.txt (Join-Path $out '使用说明.txt')

$zip = "$out.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $out -DestinationPath $zip

"打好了：$out"
Get-ChildItem -Recurse -File $out | ForEach-Object {
    '  {0,-60} {1,8:N1} MB' -f $_.FullName.Substring($out.Length + 1), ($_.Length / 1MB)
}
'  {0,-60} {1,8:N1} MB' -f (Split-Path $zip -Leaf), ((Get-Item $zip).Length / 1MB)
