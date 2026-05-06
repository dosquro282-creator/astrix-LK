param(
    [Parameter(Mandatory = $true)]
    [string]$InfPath
)

# Run from an elevated PowerShell on a disposable VM/test machine.
$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath $InfPath)) {
    throw "INF not found: $InfPath"
}

pnputil /add-driver $InfPath /install
