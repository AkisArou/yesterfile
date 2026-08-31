$ErrorActionPreference = "Stop"

$binary = (Get-Command "local-history.exe" -ErrorAction Stop).Source
$watchman = (Get-Command "watchman.exe" -ErrorAction Stop).Source

Write-Host "Using local-history: $binary"
Write-Host "Using Watchman:      $watchman"

$action = New-ScheduledTaskAction -Execute $binary -Argument "daemon"
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -RestartCount 10 -RestartInterval (New-TimeSpan -Minutes 1)
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited

Register-ScheduledTask -TaskName "LocalHistory" -Description "Event-driven Git-backed local file history" -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Force | Out-Null
Start-ScheduledTask -TaskName "LocalHistory"

Write-Host "Installed and started the LocalHistory task."

