# Fetch libyuv headers for webrtc-prebuilt (needed for yuv_helper.h).
# Run from project root: .\client\scripts\fetch-libyuv-headers.ps1

$ErrorActionPreference = "Stop"
$ClientDir = Split-Path -Parent $PSScriptRoot
$LibyuvDir = Join-Path $env:TEMP "astrix-libyuv"
$DestInclude = Join-Path $ClientDir "webrtc-prebuilt\win-x64-release\include\third_party\libyuv\include"

if (-not (Test-Path (Join-Path $ClientDir "webrtc-prebuilt\win-x64-release"))) {
    Write-Error "webrtc-prebuilt/win-x64-release not found. Run build-webrtc-via-livekit.ps1 first."
    exit 1
}

Write-Host "Fetching libyuv headers..." -ForegroundColor Cyan
if (Test-Path $LibyuvDir) {
    Remove-Item -Recurse -Force $LibyuvDir
}
# Try GitHub first (no auth); Chromium git may require depot_tools
git clone --depth 1 https://github.com/lemenkov/libyuv.git $LibyuvDir 2>$null
if ($LASTEXITCODE -ne 0) {
    Write-Host "Trying Chromium..." -ForegroundColor Yellow
    git clone --depth 1 https://chromium.googlesource.com/libyuv/libyuv $LibyuvDir 2>$null
}
if ($LASTEXITCODE -ne 0) {
    Write-Error "Failed to clone libyuv. Check network/proxy."
    exit 1
}

$SrcInclude = Join-Path $LibyuvDir "include"
New-Item -ItemType Directory -Path $DestInclude -Force | Out-Null
Copy-Item (Join-Path $SrcInclude "*") $DestInclude -Recurse -Force
if (Test-Path (Join-Path $SrcInclude "libyuv")) {
    Copy-Item (Join-Path $SrcInclude "libyuv") (Join-Path $DestInclude "libyuv") -Recurse -Force
}

Remove-Item -Recurse -Force $LibyuvDir -ErrorAction SilentlyContinue
Write-Host "Done. libyuv headers at $DestInclude" -ForegroundColor Green
