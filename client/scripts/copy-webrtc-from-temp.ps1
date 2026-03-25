# Copy webrtc build artifacts from temp to webrtc-prebuilt/win-x64-release
# Run this if build-webrtc-via-livekit.ps1 succeeded but didn't create the right folder structure.

$ErrorActionPreference = "Stop"
$ClientDir = Split-Path -Parent $PSScriptRoot
$Temp = Join-Path $env:TEMP "astrix-webrtc-build\webrtc-sys\libwebrtc"
$Source = Join-Path $Temp "win-x64-release"
$OutDir = Join-Path $ClientDir "webrtc-prebuilt"
$Dest = Join-Path $OutDir "win-x64-release"

if (-not (Test-Path $Source)) {
    Write-Host "Temp folder not found: $Source" -ForegroundColor Red
    Write-Host "Run build-webrtc-via-livekit.ps1 again." -ForegroundColor Yellow
    exit 1
}

if (Test-Path $OutDir) {
    Remove-Item -Recurse -Force $OutDir
}
New-Item -ItemType Directory -Path (Split-Path $Dest) -Force | Out-Null
Copy-Item $Source $Dest -Recurse

Write-Host "Done. Copied to $Dest" -ForegroundColor Green
Get-ChildItem $Dest | Format-Table Name
