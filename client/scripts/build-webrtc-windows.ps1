# Build libwebrtc with H.264 multithreading patch (Phase 2)
# Requires: depot_tools (auto-cloned), Python 3, Visual Studio 2022, ninja
# Output: client/webrtc-prebuilt/win-x64-release/
# After build: uncomment LK_CUSTOM_WEBRTC in .cargo/config.toml

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $PSScriptRoot
$LibWebrtcDir = Join-Path $ScriptDir "vendor\libwebrtc"
$OutPrebuilt = Join-Path $ScriptDir "webrtc-prebuilt"

if (-not (Test-Path $LibWebrtcDir)) {
    Write-Error "vendor/libwebrtc not found. Run from client/ or ensure vendor copy exists."
}

Write-Host "Building libwebrtc with H.264 multithreading (Windows)..." -ForegroundColor Cyan
Write-Host "This may take 30-60 minutes on first run (gclient sync + ninja)." -ForegroundColor Yellow

Push-Location $LibWebrtcDir
try {
    cmd /c "build_windows.cmd --arch x64 --profile release"
    if ($LASTEXITCODE -ne 0) { throw "build_windows.cmd failed" }

    $Artifacts = Join-Path $LibWebrtcDir "win-x64-release"
    $LibPath = Join-Path $Artifacts "lib\webrtc.lib"
    if (-not (Test-Path $LibPath)) {
        # Fallback: search for webrtc.lib or libwebrtc.lib in build output
        $OutDir = Join-Path $LibWebrtcDir "src\out-x64-release"
        $FoundLib = Get-ChildItem -Path $OutDir -Recurse -Filter "*.lib" -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match "webrtc" } | Select-Object -First 1
        if ($FoundLib) {
            Write-Host "Found: $($FoundLib.FullName) -> copying as webrtc.lib" -ForegroundColor Yellow
            New-Item -ItemType Directory -Path (Split-Path $LibPath) -Force | Out-Null
            Copy-Item $FoundLib.FullName $LibPath -Force
        } else {
            Write-Host "Searched in: $OutDir" -ForegroundColor Red
            throw "Build succeeded but webrtc.lib not found. Check build_windows.cmd output for errors."
        }
    }

    Write-Host "Copying to webrtc-prebuilt/..." -ForegroundColor Green
    if (Test-Path $OutPrebuilt) { Remove-Item -Recurse -Force $OutPrebuilt }
    Copy-Item -Path $Artifacts -Destination $OutPrebuilt -Recurse

    Write-Host "Done. Next steps:" -ForegroundColor Cyan
    Write-Host "  1. Uncomment LK_CUSTOM_WEBRTC in client/.cargo/config.toml"
    Write-Host "  2. cargo build"
} finally {
    Pop-Location
}
