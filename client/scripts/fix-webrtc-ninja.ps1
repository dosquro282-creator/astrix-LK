# Fix missing webrtc.ninja and desktop_capture.ninja in webrtc-prebuilt.
# Run when webrtc-prebuilt exists but ninja files are missing (build structure changed).
# Downloads ninja files from LiveKit prebuilt (compatible with same webrtc version).

$ErrorActionPreference = "Stop"
$ClientDir = Split-Path -Parent $PSScriptRoot
$OutDir = Join-Path $ClientDir "webrtc-prebuilt\win-x64-release"
# Use same tag as webrtc-sys-build (webrtc-0001d84-2); fallback to latest
$Tags = @("webrtc-0001d84-2", "webrtc-38b585d")

if (-not (Test-Path $OutDir)) {
    Write-Error "webrtc-prebuilt\win-x64-release not found. Run build-webrtc-via-livekit.ps1 first."
}

$need = @()
if (-not (Test-Path (Join-Path $OutDir "webrtc.ninja"))) { $need += "webrtc.ninja" }
if (-not (Test-Path (Join-Path $OutDir "desktop_capture.ninja"))) { $need += "desktop_capture.ninja" }
if ($need.Count -eq 0) {
    Write-Host "Ninja files already present." -ForegroundColor Green
    exit 0
}

Write-Host "Downloading LiveKit prebuilt to extract ninja files..." -ForegroundColor Cyan
$zipPath = Join-Path $env:TEMP "webrtc-ninja-fix.zip"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$headers = @{ "User-Agent" = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) PowerShell" }
$downloaded = $false
$lastError = $null
foreach ($tag in $Tags) {
    $url = "https://github.com/livekit/rust-sdks/releases/download/$tag/webrtc-win-x64-release.zip"
    try {
        Invoke-WebRequest -Uri $url -OutFile $zipPath -UseBasicParsing -Headers $headers -MaximumRedirection 5 -ErrorAction Stop
        if ((Get-Item $zipPath).Length -gt 1000000) {
            $downloaded = $true
            Write-Host "  Downloaded from $tag" -ForegroundColor Green
            break
        }
    } catch {
        $lastError = $_
        Write-Host "  $tag : $($_.Exception.Message)" -ForegroundColor Yellow
    }
}
if (-not $downloaded) {
    Write-Host "Download failed. Creating minimal ninja files..." -ForegroundColor Yellow
    # First line must contain -D defines (webrtc-sys extracts them). Minimal set for Windows release.
    $defines = " -D NDEBUG -D NOMINMAX -D WIN32 -D _WIN32 -D WEBRTC_WIN -D RTC_USE_H264=1"
    Set-Content (Join-Path $OutDir "webrtc.ninja") $defines -Encoding ASCII
    Set-Content (Join-Path $OutDir "desktop_capture.ninja") $defines -Encoding ASCII
    Write-Host "Created minimal ninja files. Run: cargo build --release" -ForegroundColor Green
    exit 0
}

$extractDir = Join-Path $env:TEMP "webrtc-ninja-extract"
if (Test-Path $extractDir) { Remove-Item -Recurse -Force $extractDir }
Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force

# Zip may have webrtc-win-x64-release/ or win-x64-release/ at root
$subdirs = @(
    (Join-Path $extractDir "webrtc-win-x64-release"),
    (Join-Path $extractDir "win-x64-release"),
    $extractDir
)
$found = $null
foreach ($d in $subdirs) {
    if (Test-Path (Join-Path $d "webrtc.ninja")) { $found = $d; break }
}
if (-not $found) {
    Remove-Item $zipPath -Force -ErrorAction SilentlyContinue
    Remove-Item $extractDir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Error "Could not find webrtc.ninja in downloaded zip."
}

foreach ($f in $need) {
    $src = Join-Path $found $f
    if (Test-Path $src) {
        Copy-Item $src (Join-Path $OutDir $f) -Force
        Write-Host "Copied $f" -ForegroundColor Green
    }
}

Remove-Item $zipPath -Force -ErrorAction SilentlyContinue
Remove-Item $extractDir -Recurse -Force -ErrorAction SilentlyContinue
Write-Host "Done. Run: cargo build --release" -ForegroundColor Green
