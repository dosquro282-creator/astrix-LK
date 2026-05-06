# Run from an elevated PowerShell on a disposable VM/test machine.
$ErrorActionPreference = "Stop"

bcdedit /set testsigning on
Write-Host "Test signing enabled. Reboot is required."
