# Build client with Visual Studio environment to capture compiler errors.
# Run from project root: .\client\scripts\build-with-vs-env.ps1

$vsPath = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools"
if (-not (Test-Path $vsPath)) {
    $vsPath = "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools"
}
if (-not (Test-Path $vsPath)) {
    $vsPath = "C:\Program Files\Microsoft Visual Studio\2022\Community"
}

$vcvars = Join-Path $vsPath "VC\Auxiliary\Build\vcvars64.bat"
if (-not (Test-Path $vcvars)) {
    Write-Error "vcvars64.bat not found. Install Visual Studio Build Tools."
    exit 1
}

$clientDir = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
$env:LK_CUSTOM_WEBRTC = Join-Path $clientDir "webrtc-prebuilt\win-x64-release"

# Run cargo in cmd with vcvars to ensure proper MSVC environment
$cmd = @"
call "$vcvars" >nul 2>&1
cd /d "$clientDir"
cargo build --release 2>&1
"@

$tempBat = [System.IO.Path]::GetTempFileName() + ".bat"
$cmd | Out-File -FilePath $tempBat -Encoding ASCII
try {
    cmd /c $tempBat
} finally {
    Remove-Item $tempBat -ErrorAction SilentlyContinue
}
