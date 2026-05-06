param(
    [string]$MsBuild = "C:\Program Files\Microsoft Visual Studio\2022\Professional\MSBuild\Current\Bin\MSBuild.exe"
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$solution = Join-Path $root "upstream\Windows-driver-samples\video\KMDOD\KMDOD.sln"

if (-not (Test-Path -LiteralPath $MsBuild)) {
    throw "MSBuild not found at '$MsBuild'. Pass -MsBuild with the Visual Studio MSBuild.exe path."
}

& $MsBuild $solution /p:Configuration=Debug /p:Platform=x64 /m
