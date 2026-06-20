# Install build dependencies for backseat on Windows.
#
# Run once per machine from the repo root:
#   powershell -ExecutionPolicy Bypass -File scripts\setup-ffmpeg.ps1
#
# What this does:
#   1. Downloads the BtbN FFmpeg 7.x GPL dev package (includes .lib files for static linking)
#      to vendor\ffmpeg\ and sets FFMPEG_DIR permanently in your user environment.
#   2. Installs Opus via vcpkg (static, x64).
#
# CI usage:
#   - Cache vendor\ffmpeg\ keyed on FFMPEG_VERSION.
#   - Cache the vcpkg installed tree keyed on vcpkg.json / baseline.
#   - Set FFMPEG_DIR in the CI environment rather than relying on the user-level variable.

param(
    [string]$FfmpegVersion = "7.1"
)

$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
$FfmpegDir = Join-Path $RepoRoot "vendor\ffmpeg"

# ── FFmpeg ────────────────────────────────────────────────────────────────────

if (Test-Path (Join-Path $FfmpegDir "include")) {
    Write-Host "[setup-ffmpeg] FFmpeg already present at $FfmpegDir" -ForegroundColor Green
} else {
    $ArchiveName = "ffmpeg-n${FfmpegVersion}-latest-win64-gpl-${FfmpegVersion}.zip"
    $Url = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/$ArchiveName"
    $TmpZip = Join-Path $env:TEMP $ArchiveName

    Write-Host "[setup-ffmpeg] Downloading $ArchiveName..." -ForegroundColor Cyan
    Invoke-WebRequest -Uri $Url -OutFile $TmpZip -UseBasicParsing

    Write-Host "[setup-ffmpeg] Extracting to vendor\ffmpeg..." -ForegroundColor Cyan
    $TmpExtract = Join-Path $env:TEMP "ffmpeg-extract"
    if (Test-Path $TmpExtract) { Remove-Item $TmpExtract -Recurse -Force }
    Expand-Archive -Path $TmpZip -DestinationPath $TmpExtract
    # BtbN zips contain a single top-level directory; strip it.
    $Inner = Get-ChildItem $TmpExtract | Select-Object -First 1
    New-Item -ItemType Directory -Path $FfmpegDir -Force | Out-Null
    Move-Item (Join-Path $Inner.FullName "*") $FfmpegDir
    Remove-Item $TmpExtract -Recurse -Force
    Remove-Item $TmpZip -Force

    Write-Host "[setup-ffmpeg] FFmpeg $FfmpegVersion installed at $FfmpegDir" -ForegroundColor Green
}

# Set FFMPEG_DIR for the current session and permanently for the user.
$env:FFMPEG_DIR = $FfmpegDir
[System.Environment]::SetEnvironmentVariable("FFMPEG_DIR", $FfmpegDir, "User")
Write-Host "[setup-ffmpeg] FFMPEG_DIR=$FfmpegDir (set in user environment)" -ForegroundColor Green

# ── Opus via vcpkg ───────────────────────────────────────────────────────────

$Vcpkg = Get-Command vcpkg -ErrorAction SilentlyContinue
if (-not $Vcpkg) {
    Write-Warning @"
vcpkg not found. Install it from https://github.com/microsoft/vcpkg then run:
  vcpkg install opus:x64-windows-static-md
  vcpkg integrate install
"@
} else {
    Write-Host "[setup-ffmpeg] Installing Opus via vcpkg..." -ForegroundColor Cyan
    & vcpkg install opus:x64-windows-static-md
    & vcpkg integrate install
    Write-Host "[setup-ffmpeg] Opus installed." -ForegroundColor Green
}

Write-Host ""
Write-Host "Done. Open a new terminal (or restart your IDE) so FFMPEG_DIR takes effect," -ForegroundColor White
Write-Host "then run: cargo build -p overlay" -ForegroundColor White
