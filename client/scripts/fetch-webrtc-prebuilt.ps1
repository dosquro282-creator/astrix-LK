# Fetch LiveKit webrtc prebuilt for Windows x64 (Phase 1)
# Same build as default webrtc-sys-build, used to verify LK_CUSTOM_WEBRTC pipeline.
# In Phase 2, this will be replaced by a custom build with H.264 multithreading.

$ErrorActionPreference = "Stop"
$ClientDir = Split-Path -Parent $PSScriptRoot  # client/
$OutDir = Join-Path $ClientDir "webrtc-prebuilt"
$ZipUrl = "https://github.com/livekit/rust-sdks/releases/download/webrtc-0001d84-2/webrtc-win-x64-release.zip"
$ZipPath = Join-Path $env:TEMP "webrtc-win-x64-release.zip"

Write-Host "Downloading webrtc prebuilt (~90 MB)..."
Invoke-WebRequest -Uri $ZipUrl -OutFile $ZipPath -UseBasicParsing

Write-Host "Extracting to $OutDir..."
if (Test-Path $OutDir) { Remove-Item -Recurse -Force $OutDir }
New-Item -ItemType Directory -Path $OutDir | Out-Null
Expand-Archive -Path $ZipPath -DestinationPath $OutDir -Force

Remove-Item $ZipPath -ErrorAction SilentlyContinue

# Zip contains win-x64-release/ with include/, lib/, webrtc.ninja
$WebrtcPath = Join-Path $OutDir "win-x64-release"
Write-Host "Done. LK_CUSTOM_WEBRTC = $WebrtcPath"
if (Test-Path $WebrtcPath) {
    Get-ChildItem $WebrtcPath | Format-Table Name
    if (-not (Test-Path (Join-Path $WebrtcPath "lib"))) {
        Write-Warning "lib/ folder missing - zip may be incomplete. Try re-downloading."
    }
}
