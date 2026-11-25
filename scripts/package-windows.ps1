$ErrorActionPreference = 'Stop'

# Windows 打包脚本：构建无控制台窗口 GUI 与 CLI 发布版并生成便携 ZIP 包。
# 依赖：Visual Studio Build Tools（MSVC C++ 工具链）与 Windows 10/11 SDK（含 fxc.exe）。

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $RepoRoot

function Find-Fxc {
    if ($env:GPUI_FXC_PATH -and (Test-Path $env:GPUI_FXC_PATH)) {
        return $env:GPUI_FXC_PATH
    }
    $cmd = Get-Command fxc.exe -ErrorAction SilentlyContinue
    if ($cmd) {
        return $cmd.Source
    }
    $kits = 'C:\Program Files (x86)\Windows Kits\10\bin'
    if (Test-Path $kits) {
        $found = Get-ChildItem -Path $kits -Filter 'fxc.exe' -Recurse -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\x64\\fxc\.exe$' } |
            Sort-Object FullName -Descending |
            Select-Object -First 1
        if ($found) {
            return $found.FullName
        }
    }
    return $null
}

$fxc = Find-Fxc
if (-not $fxc) {
    Write-Error '未找到 Windows SDK 中的 fxc.exe（GPUI release 着色器编译必需）。请安装 Windows 10/11 SDK 或设置环境变量 GPUI_FXC_PATH。'
    exit 1
}
$env:GPUI_FXC_PATH = $fxc
Write-Host "Using fxc.exe: $fxc"

$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
$VersionLine = Select-String -Path 'Cargo.toml' -Pattern '^version = "([^"]+)"' | Select-Object -First 1
$Version = $VersionLine.Matches[0].Groups[1].Value

& cargo build --locked --release --bin aws-quick-bridge-gui --bin aws-quick-bridge-rs --target-dir $TargetDir
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

$ReleaseDir = Join-Path $TargetDir 'release'
$GuiExe = Join-Path $ReleaseDir 'aws-quick-bridge-gui.exe'
$CliExe = Join-Path $ReleaseDir 'aws-quick-bridge-rs.exe'
$ZipPath = Join-Path $ReleaseDir "Quick-Bridge-$Version-windows-x64.zip"
$StagingRoot = Join-Path $ReleaseDir ('.quick-bridge-win-' + [System.Guid]::NewGuid().ToString('N').Substring(0, 8))
$PackageDir = Join-Path $StagingRoot 'Quick Bridge'

try {
    New-Item -ItemType Directory -Path $PackageDir -Force | Out-Null
    Copy-Item -Path $GuiExe -Destination (Join-Path $PackageDir 'Quick Bridge.exe')
    Copy-Item -Path $CliExe -Destination (Join-Path $PackageDir 'aws-quick-bridge-rs.exe')
    if (Test-Path $ZipPath) {
        Remove-Item -Path $ZipPath -Force
    }
    Compress-Archive -Path $PackageDir -DestinationPath $ZipPath -CompressionLevel Optimal
    Write-Host $ZipPath
} finally {
    if (Test-Path $StagingRoot) {
        Remove-Item -Path $StagingRoot -Recurse -Force
    }
}
