#Requires -RunAsAdministrator
<#
    Runs the UAC / secure-desktop spike.

    The service half must run in session 0 to reproduce the real constraint, so it is launched as
    SYSTEM via a scheduled task -- the same isolation a Windows service gets, without the SCM
    plumbing, which belongs to the real implementation rather than to a spike.

    Everything created here is removed again at the end.
#>

$ErrorActionPreference = 'Stop'

$taskName = 'VirtualKmSwitchUacSpike'
$logPath  = Join-Path $env:ProgramData 'VirtualKmSwitch\uac-spike.log'
$exePath  = Join-Path $PSScriptRoot 'VirtualKmSwitch.UacSpike\bin\Debug\net10.0-windows10.0.19041.0\VirtualKmSwitch.UacSpike.exe'

function Remove-SpikeTask {
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
}

function Stop-SpikeProcesses {
    Get-Process -Name 'VirtualKmSwitch.UacSpike' -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue
}

if (-not (Test-Path $exePath)) {
    Write-Host 'Not built. Run:' -ForegroundColor Red
    Write-Host '  dotnet build src/csharp/spike/VirtualKmSwitch.UacSpike/VirtualKmSwitch.UacSpike.csproj'
    Write-Host "Expected: $exePath"
    exit 1
}

Write-Host 'Virtual KM Switch UAC spike' -ForegroundColor Cyan
Write-Host ('=' * 62)
Write-Host "exe : $exePath"
Write-Host "log : $logPath"
Write-Host ''

# Start from a clean log so the result is unambiguous.
if (Test-Path $logPath) { Remove-Item $logPath -Force -ErrorAction SilentlyContinue }

Remove-SpikeTask
Stop-SpikeProcesses

try {
    Write-Host 'Registering SYSTEM task (runs in session 0)...' -NoNewline
    $action    = New-ScheduledTaskAction -Execute $exePath -Argument '--service'
    $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest

    # AllowStartIfOnBatteries is essential, not cosmetic: the default is to REFUSE to start on
    # battery, and the task then sits in Queued forever with no error and no process -- which
    # looks identical to the launch itself having failed.
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable

    $null = Register-ScheduledTask -TaskName $taskName -Action $action -Principal $principal -Settings $settings -Force
    Write-Host ' done'

    Write-Host 'Starting it...' -NoNewline
    Start-ScheduledTask -TaskName $taskName
    Write-Host ' done'

    Start-Sleep -Seconds 4

    $taskInfo = Get-ScheduledTaskInfo -TaskName $taskName -ErrorAction SilentlyContinue
    $taskState = (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue).State
    Remove-SpikeTask

    $agent = Get-Process -Name 'VirtualKmSwitch.UacSpike' -ErrorAction SilentlyContinue
    if (-not $agent) {
        Write-Host ''
        Write-Host 'The agent is NOT running: the service half could not place it in the session.' -ForegroundColor Red
        Write-Host ''
        if ($taskInfo) {
            Write-Host ("  task state       : {0}" -f $taskState)
            Write-Host ("  last run time    : {0}" -f $taskInfo.LastRunTime)
            Write-Host ("  last task result : 0x{0:X}" -f $taskInfo.LastTaskResult)
            if ($taskState -eq 'Queued') {
                Write-Host '  (Queued means Task Scheduler never ran it -- a conditions problem, not a code one.)' -ForegroundColor Yellow
            }
        }
        Write-Host ''
        Write-Host 'Log:' -ForegroundColor Yellow
        if (Test-Path $logPath) { Get-Content $logPath } else { Write-Host '  (no log written -- the process never started)' }
        exit 1
    }

    Write-Host ''
    Write-Host ('=' * 62) -ForegroundColor Green
    Write-Host ' AGENT IS RUNNING AS SYSTEM IN YOUR SESSION' -ForegroundColor Green
    Write-Host ('=' * 62) -ForegroundColor Green
    Write-Host ''
    Write-Host ' Now TRIGGER A UAC PROMPT. From a NORMAL (non-admin) PowerShell:' -ForegroundColor Yellow
    Write-Host ''
    Write-Host '     Start-Process notepad -Verb RunAs' -ForegroundColor White
    Write-Host ''
    Write-Host ' The agent injects ESCAPE on reaching the secure desktop, which DISMISSES the' -ForegroundColor Yellow
    Write-Host ' prompt rather than approving it.' -ForegroundColor Yellow
    Write-Host ''
    Write-Host ' WATCH FOR: does the UAC prompt close by itself?' -ForegroundColor Cyan
    Write-Host ''

    for ($i = 60; $i -gt 0; $i -= 5) {
        Write-Host "   ${i}s remaining" -ForegroundColor DarkGray
        Start-Sleep -Seconds 5
    }
}
finally {
    Remove-SpikeTask
    Stop-SpikeProcesses
}

Write-Host ''
Write-Host ('=' * 62)
Write-Host 'LOG' -ForegroundColor Cyan
Write-Host ('=' * 62)
if (Test-Path $logPath) { Get-Content $logPath } else { Write-Host '(no log written)' -ForegroundColor Red }
