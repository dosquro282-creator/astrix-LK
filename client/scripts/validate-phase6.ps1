# Phase 6: Quick validation of MFT GPU path.
# Run from client directory: .\scripts\validate-phase6.ps1
#
# Full checklist: client/docs/phase6_validation.md

$ErrorActionPreference = "Stop"
Push-Location $PSScriptRoot\..

Write-Host "=== Phase 6 Quick Validation ===" -ForegroundColor Cyan
cargo run --example validate_mft_path
Write-Host ""
Write-Host "Full checklist: client/docs/phase6_validation.md" -ForegroundColor Green
Pop-Location
