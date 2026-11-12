# 从 CHANGELOG.md 里取出一个版本的那一节（不含标题行），发版时当发布说明。
#
# 用法：pwsh tools/release-notes.ps1 1.8.0
param([Parameter(Mandatory)] [string] $Version)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$lines = Get-Content -Encoding utf8 (Join-Path $root 'CHANGELOG.md')
$start = [array]::FindIndex([string[]]$lines, [Predicate[string]] { param($l) $l -eq "## $Version" })
if ($start -lt 0) { throw "CHANGELOG.md 里没有「## $Version」这一节" }
$section = @()
for ($i = $start + 1; $i -lt $lines.Count -and -not $lines[$i].StartsWith('## '); $i++) {
    $section += $lines[$i]
}
($section -join "`n").Trim()
