# 定时任务管理器打包:release 构建 + 发布目录 + 压缩包。
# 目标运行时:PowerShell 7 (pwsh);不保证 Windows PowerShell 5.1。
# 打包规则:
#   - 版本号取自 workspace Cargo.toml 的 version 字段(不使用 git 计数)
#   - exe 文件名保持 scheduled-task.exe 不变
#   - 版本号落在发布目录与压缩包名上:
#       dist/scheduled-task-v<版本>-dist/scheduled-task.exe
#       dist/scheduled-task-v<版本>-dist.7z(未安装 7z 时为 .zip)
#
# 用法:
#   pwsh -File scripts\build.ps1,或直接运行 scripts\build.bat(自动拉起 pwsh)。
#   -SkipBuild  跳过构建,仅用已有 target/release 产物重新打包
#   -NoZip      只产出发布目录,不压缩

[CmdletBinding()]
param(
    # 跳过构建步骤,仅用已有 target/release 产物重新打包。
    [switch]$SkipBuild,

    # 只产出发布目录,不生成压缩包。
    [switch]$NoZip
)

$ErrorActionPreference = 'Stop'

# 定位项目根(脚本位于 scripts/ 下)。
$ProjectRoot = Split-Path $PSScriptRoot -Parent
Set-Location $ProjectRoot

# 从 Cargo.toml 解析版本号(仅匹配行首的 version,避免命中依赖里的 version)。
$cargoToml = Get-Content 'Cargo.toml' -Raw
if ($cargoToml -notmatch '(?m)^version\s*=\s*"([^"]+)"') {
    throw '无法从 Cargo.toml 解析 version'
}
$Version = $Matches[1]
if ($Version -notmatch '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$') {
    throw "版本号格式不正确: $Version"
}
Write-Host "版本: $Version" -ForegroundColor Cyan

# 构建步骤。
if (-not $SkipBuild) {
    Write-Host '==> cargo build --release -p scheduled-task' -ForegroundColor Green
    & cargo build --release -p scheduled-task
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build 失败 (exit $LASTEXITCODE)"
    }
}

$exePath = Join-Path $ProjectRoot 'target/release/scheduled-task.exe'
if (-not (Test-Path $exePath)) {
    throw "未找到构建产物: $exePath(请去掉 -SkipBuild 重新构建)"
}

# 压缩产物(7z 优先极限压缩,未安装 7z 时回退 Compress-Archive zip)。
function Compress-Artifact {
    param([string]$SourceSpec, [string]$ArchivePath)
    if (Test-Path $ArchivePath) { Remove-Item $ArchivePath -Force }
    if (Get-Command 7z -ErrorAction SilentlyContinue) {
        Write-Host "==> 压缩 (7z -mx=9): $ArchivePath" -ForegroundColor Green
        & 7z a -t7z -mx=9 $ArchivePath $SourceSpec
        if ($LASTEXITCODE -ne 0) {
            throw "7z 压缩失败 (exit $LASTEXITCODE)"
        }
    } else {
        Write-Host '未检测到 7z,回退至 zip 压缩。' -ForegroundColor Yellow
        Write-Host "==> 压缩 (zip): $ArchivePath" -ForegroundColor Green
        Compress-Archive -Path $SourceSpec -DestinationPath $ArchivePath -CompressionLevel Optimal
    }
}

# 组装发布目录(exe 文件名不变,版本号落在目录名上)。
$DistName = "scheduled-task-v${Version}-dist"
$distRoot = Join-Path $ProjectRoot 'dist' | Join-Path -ChildPath $DistName
$distRoot = [IO.Path]::GetFullPath($distRoot)
$expectedParent = [IO.Path]::GetFullPath((Join-Path $ProjectRoot 'dist'))
if ((Split-Path $distRoot -Parent) -ne $expectedParent) {
    throw "发布目录超出预期位置: $distRoot"
}
Write-Host "==> 组装发布目录: $DistName" -ForegroundColor Green

if (Test-Path -LiteralPath $distRoot) { Remove-Item -LiteralPath $distRoot -Recurse -Force }
New-Item -ItemType Directory -Path $distRoot -Force | Out-Null

Copy-Item $exePath $distRoot
Copy-Item (Join-Path $ProjectRoot 'README.md') $distRoot
Copy-Item (Join-Path $ProjectRoot 'docs') $distRoot -Recurse
$exampleDir = Join-Path $distRoot 'crates/cli/testdata'
New-Item -ItemType Directory -Path $exampleDir -Force | Out-Null
Copy-Item (Join-Path $ProjectRoot 'crates/cli/testdata/concurrency.ron') $exampleDir

# 打印发布目录结构。
Write-Host '发布目录结构:' -ForegroundColor DarkGray
Get-ChildItem $distRoot -Recurse -File | ForEach-Object {
    $rel = $_.FullName.Substring($distRoot.Length + 1)
    Write-Host "  $rel" -ForegroundColor DarkGray
}

# 压缩发布目录(压缩包内不含目录层级)。
$distArchivePath = $null
if (-not $NoZip) {
    $distArchivePath = if (Get-Command 7z -ErrorAction SilentlyContinue) { "$distRoot.7z" } else { "$distRoot.zip" }
    Compress-Artifact -SourceSpec "$distRoot/*" -ArchivePath $distArchivePath
}

# 完成。
Write-Host ''
Write-Host '打包完成。' -ForegroundColor Green
Write-Host "  目录: $distRoot"
if ($distArchivePath) { Write-Host "  压缩包: $distArchivePath" }
