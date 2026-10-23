<#
    Checks, on one machine, that the Rust session agent can dismiss a live UAC prompt.

    Run from a NORMAL (non-elevated) PowerShell, after `cargo build --release --workspace` in
    src\rust:

        powershell -ExecutionPolicy Bypass -File src\rust\spike\run-uac-check.ps1

    What happens:
      1. You approve one UAC prompt. It starts `vkms-spike --uac-service` as SYSTEM in session 0
         through a scheduled task -- the same isolation the service gets -- and removes the task
         again at once. That process stands in for the service: it places the real vkms-agent on
         the Winlogon desktop and keeps queuing Escape for it, as the router queues remote input.
      2. This script raises a second UAC prompt itself. Do NOT click it. The agent should
         dismiss it within about a second. Escape cancels a prompt, so nothing is elevated.

    The SYSTEM process stops itself, and its agent, after -Seconds. Until then it will also
    dismiss any other secure-desktop screen, such as Ctrl+Alt+Del, so leave the machine alone.
#>

param(
    [switch]$Setup,
    [int]$Seconds = 40
)

$ErrorActionPreference = 'Stop'

$root     = Split-Path $PSScriptRoot -Parent
$spike    = Join-Path $root 'target\release\vkms-spike.exe'
$agent    = Join-Path $root 'target\release\vkms-agent.exe'
$dataDir  = Join-Path $env:ProgramData 'VirtualKmSwitch'
$log      = Join-Path $dataDir 'uac-check.log'
$agentLog = Join-Path $dataDir 'agent.Winlogon.log'
$taskName = 'VirtualKmSwitchUacCheck'

# --- The elevated half: start the SYSTEM process, then leave nothing registered. ---------------
if ($Setup) {
    Start-Transcript -Path (Join-Path $env:TEMP 'vkms-uac-setup.log') -Force | Out-Null
    try {
        Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue

        $action    = New-ScheduledTaskAction -Execute $spike -Argument "--uac-service --seconds $Seconds"
        $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest

        # The default refuses to start on battery and leaves the task Queued with no error at all,
        # which looks exactly like the launch itself having failed.
        $settings  = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
                         -ExecutionTimeLimit (New-TimeSpan -Seconds ($Seconds + 60))

        $null = Register-ScheduledTask -TaskName $taskName -Action $action -Principal $principal -Settings $settings -Force
        Start-ScheduledTask -TaskName $taskName

        # Removing the registration does not stop the running instance, so wait only until it has
        # visibly begun: a log written since this setup started.
        $started  = Get-Date
        $deadline = $started.AddSeconds(15)
        while ((Get-Date) -lt $deadline -and -not ((Test-Path $log) -and (Get-Item $log).LastWriteTime -ge $started)) {
            Start-Sleep -Milliseconds 200
        }
        Start-Sleep -Seconds 1
    }
    finally {
        Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
        Stop-Transcript | Out-Null
    }
    exit 0
}

# --- The unelevated half. ----------------------------------------------------------------------
function Show-Logs {
    Write-Host ''
    Write-Host '--- uac-check.log ---' -ForegroundColor Cyan
    if (Test-Path $log) { Get-Content $log } else { Write-Host '(not written)' -ForegroundColor Red }
    Write-Host ''
    Write-Host '--- agent.Winlogon.log (last 15 lines) ---' -ForegroundColor Cyan
    if (Test-Path $agentLog) { Get-Content $agentLog -Tail 15 } else { Write-Host '(not written)' -ForegroundColor Red }
}

# Only this run's log counts: one left by an earlier run would already say "agent started".
$runStarted = Get-Date

function Read-Log {
    if ((Test-Path $log) -and (Get-Item $log).LastWriteTime -ge $runStarted) {
        Get-Content $log -Raw -ErrorAction SilentlyContinue
    } else { '' }
}

Write-Host 'Virtual KM Switch UAC check' -ForegroundColor Cyan
Write-Host ('=' * 62)

$identity = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if ($identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host 'Run this from a NORMAL PowerShell. From an elevated one, the test prompt never appears.' -ForegroundColor Red
    exit 1
}

foreach ($binary in $spike, $agent) {
    if (-not (Test-Path $binary)) {
        Write-Host "Not built: $binary" -ForegroundColor Red
        Write-Host 'Run: cargo build --release --workspace, in src\rust'
        exit 1
    }
}

# Without a prompt on the secure desktop there is nothing for the Winlogon agent to do.
$policy = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
if ($policy.EnableLUA -eq 0 -or $policy.ConsentPromptBehaviorAdmin -eq 0) {
    Write-Host 'UAC is off or set to elevate without prompting; there is no prompt to dismiss.' -ForegroundColor Red
    exit 1
}
if ($policy.PromptOnSecureDesktop -eq 0) {
    Write-Host 'UAC prompts are shown on the normal desktop (PromptOnSecureDesktop=0), so this would not' -ForegroundColor Red
    Write-Host 'exercise the Winlogon agent at all.' -ForegroundColor Red
    exit 1
}

# A running service has Winlogon agents of its own, on the same ring names.
$service = Get-Service -Name 'VirtualKmSwitch' -ErrorAction SilentlyContinue
if ($service -and $service.Status -eq 'Running') {
    Write-Host 'The Virtual KM Switch service is running. Stop it first: deploy\service.ps1 stop' -ForegroundColor Red
    exit 1
}

Write-Host ''
Write-Host 'Step 1: APPROVE the prompt that appears next. It starts the SYSTEM helper.' -ForegroundColor Yellow
try {
    $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-Setup', '-Seconds', $Seconds)
    Start-Process powershell.exe -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList $arguments
}
catch {
    Write-Host 'Setup was not approved; nothing was changed.' -ForegroundColor Red
    exit 1
}

$deadline = (Get-Date).AddSeconds(20)
while ((Get-Date) -lt $deadline -and (Read-Log) -notmatch 'agent started|FAILED') { Start-Sleep -Milliseconds 250 }

if ((Read-Log) -notmatch 'agent started') {
    Write-Host 'The SYSTEM helper did not start the agent.' -ForegroundColor Red
    Write-Host "Setup transcript: $(Join-Path $env:TEMP 'vkms-uac-setup.log')"
    Show-Logs
    exit 1
}

# The agent installs its hooks and reads pointer settings before it is ready.
Start-Sleep -Seconds 3

Write-Host ''
Write-Host 'Step 2: a second prompt is about to appear. DO NOT CLICK IT.' -ForegroundColor Yellow
Write-Host '        The agent should dismiss it by itself within about a second.' -ForegroundColor Yellow
Start-Sleep -Seconds 2

$timer = [Diagnostics.Stopwatch]::StartNew()
$outcome = 'approved'
try {
    # Something harmless if it is approved after all: a shell that exits at once.
    $null = Start-Process cmd.exe -ArgumentList '/c', 'exit' -Verb RunAs -WindowStyle Hidden -PassThru
}
catch {
    # ERROR_CANCELLED. Windows PowerShell 5.1 wraps it in an InvalidOperationException whose inner
    # exception is not always the Win32Exception, so the whole chain is searched, then the message.
    $cancelled = $false
    for ($e = $_.Exception; $e; $e = $e.InnerException) {
        if ($e -is [ComponentModel.Win32Exception] -and $e.NativeErrorCode -eq 1223) { $cancelled = $true }
    }
    if ($_.Exception.Message -match 'canceled by the user') { $cancelled = $true }
    $outcome = if ($cancelled) { 'cancelled' } else { "error: $($_.Exception.Message)" }
}
$timer.Stop()
$elapsed = [math]::Round($timer.Elapsed.TotalSeconds, 2)

# The sharper measure: how long the secure desktop was showing, from the agent's own log. The agent
# waits 750 ms after the switch before injecting, so a dismissal shows as a little over that.
$secureMs = $null
$switches = Get-Content $agentLog -ErrorAction SilentlyContinue | ForEach-Object {
    if ($_ -match "^(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\.\d{3}) .*input desktop is now '(\w+)'") {
        [pscustomobject]@{ At = [datetime]::ParseExact($Matches[1], 'yyyy-MM-dd HH:mm:ss.fff', $null); Desktop = $Matches[2] }
    }
} | Where-Object { $_.At -ge $runStarted }
for ($i = 0; $i -lt @($switches).Count - 1; $i++) {
    if ($switches[$i].Desktop -eq 'Winlogon' -and $switches[$i + 1].Desktop -ne 'Winlogon') {
        $secureMs = [int]($switches[$i + 1].At - $switches[$i].At).TotalMilliseconds
    }
}
$secureText = if ($secureMs) { "the secure desktop showed for $secureMs ms" } else { 'the agent did not log the secure desktop' }

Write-Host ''
Write-Host ('=' * 62)
switch -Wildcard ($outcome) {
    'cancelled' {
        if ($elapsed -lt 5) {
            Write-Host "PASS: the prompt was dismissed after ${elapsed}s; $secureText." -ForegroundColor Green
        } else {
            Write-Host "INCONCLUSIVE: the prompt was cancelled, but only after ${elapsed}s -- too slow for the agent;" -ForegroundColor Yellow
            Write-Host 'was it clicked? Check the agent log for the switch to Winlogon.' -ForegroundColor Yellow
        }
    }
    'approved' { Write-Host "FAIL: the prompt was approved after ${elapsed}s; the agent did not dismiss it." -ForegroundColor Red }
    default    { Write-Host "FAIL: $outcome" -ForegroundColor Red }
}
Write-Host ('=' * 62)

Write-Host ''
Write-Host "Waiting for the SYSTEM helper to stop itself (up to ${Seconds}s from its start)..."
$deadline = (Get-Date).AddSeconds($Seconds + 15)
while ((Get-Date) -lt $deadline -and (Read-Log) -notmatch 'agent stopped') { Start-Sleep -Seconds 1 }

Show-Logs
