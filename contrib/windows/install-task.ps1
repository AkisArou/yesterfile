$ErrorActionPreference = "Stop"

$binary = (Get-Command "yesterfile.exe" -ErrorAction Stop).Source
$watchman = (Get-Command "watchman.exe" -ErrorAction Stop).Source

Write-Host "Using yesterfile: $binary"
Write-Host "Using Watchman:      $watchman"

$action = New-ScheduledTaskAction -Execute $binary -Argument "daemon"
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -RestartCount 10 -RestartInterval (New-TimeSpan -Minutes 1)
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited

Register-ScheduledTask -TaskName "Yesterfile" -Description "Event-driven Git-backed local file history" -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Force | Out-Null
Start-ScheduledTask -TaskName "Yesterfile"

Write-Host "Installed and started the Yesterfile task."

